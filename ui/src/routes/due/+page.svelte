<script lang="ts">
	// Mockup screen #due: the screenings and vaccines the profile country's guideline lists for this
	// person's age and sex, and the rule pack's lab gaps, from GET /profiles/{p}/due
	// (ownhealth.prevention/v1). The server computes every status and date; this page groups and shows.
	// A person with write access acts on an item: snooze it to a date, mark it done by pointing at the
	// record or document that shows it, or dismiss it with a reason (POST /profiles/{p}/due/{key}/decision).
	import '#lib/components/pages2/pages2.css';
	import { createRecord, decideDue, getDue, listDocuments, listRecords, undoDue } from '#lib/api';
	import { ApiError } from '#lib/api/core';
	import DueAnswers from '#lib/components/DueAnswers.svelte';
	import { focusOnOpen } from '#lib/actions/focusOnOpen';
	import { C } from '#lib/components/colors';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import SourcePanel from '#lib/components/pages1/SourcePanel.svelte';
	import { openSource } from '#lib/components/pages1/source.svelte';
	import { errCode, errText, Loader, shortHash, tf } from '#lib/components/pages1/util.svelte';
	import PhaseChip from '#lib/components/PhaseChip.svelte';
	import SourceChip from '#lib/components/SourceChip.svelte';
	import { i18n, t } from '#lib/i18n/index.svelte';
	import { addDays, docLabel, dueColor, dueGroups, evidenceDay, evidenceLimit, isOpen, type DueDecisionIn, type DueDoc, type DueItem } from '#lib/logic/due';
	import { todayIso } from '#lib/logic/people';
	import { currentProfile } from '#lib/state/app.svelte';

	const prof = $derived(currentProfile());
	const canWrite = $derived(!!prof && prof.role !== 'viewer');
	const doc = new Loader<DueDoc>();
	// the local day, read again on each load and each form open, so a page left open past midnight moves on
	let today = $state(todayIso());

	function load() {
		const slug = prof?.slug;
		const lang = i18n.lang;
		if (!slug) return;
		// the local day, as the form's date: a record saved today counts on the reload
		const day = todayIso();
		today = day;
		doc.run(() => getDue(slug, lang, day), `${slug}/${lang}`);
	}
	$effect(load);

	const items = $derived(doc.data?.items ?? []);
	const hidden = $derived(doc.data?.hidden ?? []);
	const TITLES = { again: 'due_g_again', open: 'due_g_open', done: 'due_g_done', na: 'due_g_na' } as const;
	const groups = $derived(dueGroups(items).map((g) => ({ ...g, title: t(TITLES[g.id]) })));
	const ICON: Record<string, string> = {
		lab: '<path d="M9 3h6M10 3v6l-5 9a2 2 0 0 0 2 3h10a2 2 0 0 0 2-3l-5-9V3"/>',
		vaccine: '<path d="m18 2 4 4M17 7l-9 9M14 4l6 6M5 19l-3 3M7 13l4 4"/>',
		screening: '<circle cx="11" cy="11" r="7"/><path d="m20 20-3.5-3.5"/>'
	};
	// The date beside the status: when it opens, when it is next listed, or since when it is open
	// again after a record (a row never recorded says so instead of a date from decades ago).
	const when = (i: DueItem) =>
		!i.due || (!i.last && isOpen(i) && i.status !== 'upcoming') ? '' : i.status === 'up_to_date' ? tf('due_next', { d: i.due }) : i.status === 'upcoming' ? tf('due_from', { d: i.due }) : tf('due_since', { d: i.due });

	// How many items the scopes leave out, and which scopes they need.
	const withheldText = (d: DueDoc) => tf('due_withheld', { n: d.withheld, scopes: (d.withheld_scopes ?? []).join(', ') });

	// One small form at a time: record a vaccine or a procedure with the name that counts for the row
	// (panel 'record'), or act on the item (panel 'act').
	let open = $state<string | null>(null);
	let panel = $state<'record' | 'act'>('record');
	let name = $state('');
	let date = $state('');
	let busy = $state(false);
	let error = $state<string | null>(null);
	// each form opening gets a new number: a save that ends after the form was closed or opened again
	// leaves the newer form and its draft alone
	let formSeq = 0;
	// An open form belongs to the profile it was opened for: another profile closes it, so text typed
	// for one person is never saved to another.
	const profSlug = $derived(prof?.slug);
	$effect(() => {
		void profSlug;
		formSeq++;
		open = null;
		error = null;
	});
	function start(i: DueItem) {
		formSeq++;
		open = open === i.key && panel === 'record' ? null : i.key;
		panel = 'record';
		name = i.record?.text ?? i.title ?? '';
		today = todayIso();
		date = today;
		error = null;
	}
	async function save(e: SubmitEvent, i: DueItem) {
		e.preventDefault();
		if (!prof || !i.record || busy) return;
		const form = formSeq;
		busy = true;
		error = null;
		try {
			const body = i.record.kind === 'immunization' ? { vaccine: name.trim(), occurred: date || null } : { text: name.trim(), performed: date || null };
			await createRecord(prof.slug, i.record.kind, body);
			if (form === formSeq) open = null;
			load();
		} catch (err) {
			if (form === formSeq) error = errText(err);
		} finally {
			busy = false;
		}
	}

	// Acting on an item: one kind at a time, with its own field.
	type Act = 'snooze' | 'done' | 'dismiss';
	const ACTS: Act[] = ['snooze', 'done', 'dismiss'];
	let act = $state<Act>('snooze');
	let until = $state('');
	let reason = $state('');
	let evidence = $state('');
	type Ev = { value: string; label: string; group: string };
	let evidenceList = $state<Ev[] | null>(null);
	// a source that failed for another reason than the scopes: the list says so and offers a retry
	let evidenceErr = $state<unknown>(null);
	// only the latest load fills the list: a slower one for another item or profile is dropped
	let evidenceSeq = 0;
	async function loadEvidence(slug: string) {
		const seq = ++evidenceSeq;
		evidenceList = null;
		evidenceErr = null;
		const failed: unknown[] = [];
		const empty = (e: unknown) => {
			if (!(e instanceof ApiError && e.status === 403)) failed.push(e);
			return { items: [] };
		};
		const limit = evidenceLimit();
		const day = (v: unknown) => evidenceDay(v, limit);
		const [proc, imm, docs] = await Promise.all([
			listRecords(slug, 'procedure').catch(empty),
			listRecords(slug, 'immunization').catch(empty),
			listDocuments(slug).catch(empty)
		]);
		type Rec = { id: string; title: string; detail: Record<string, unknown> };
		const recDay = (r: Rec) => day(r.detail.performed ?? r.detail.occurred ?? r.detail.effective_at);
		const rec = (kind: string, group: string) => (r: Rec) => ({ value: `${kind}:${r.id}`, label: [r.title, recDay(r)].join(' · '), group });
		if (seq !== evidenceSeq) return;
		evidenceErr = failed[0] ?? null;
		// a record with no day, or one after today, proves nothing yet, so the server refuses it as evidence
		evidenceList = [
			...imm.items.filter(recDay).map(rec('immunization', t('due_f_vaccine'))),
			...proc.items.filter(recDay).map(rec('procedure', t('due_f_procedure'))),
			...docs.items
				// a document needs a clinical date to count, and this list knows only its collection day
				.filter((d) => d.status === 'active' && day(d.collected_at))
				.map((d) => ({ value: `document:${d.id}`, label: docLabel(d), group: t('due_ev_document') }))
		];
	}
	function startAct(i: DueItem) {
		formSeq++;
		open = open === i.key && panel === 'act' ? null : i.key;
		panel = 'act';
		today = todayIso();
		act = 'snooze';
		until = addDays(today, 90);
		reason = '';
		evidence = '';
		error = null;
		if (open && prof) loadEvidence(prof.slug);
	}
	async function saveAct(e: SubmitEvent, i: DueItem) {
		e.preventDefault();
		if (!prof || busy) return;
		const note = reason.trim() || undefined;
		let body: DueDecisionIn;
		if (act === 'snooze') body = { kind: 'snooze', until, reason: note };
		else if (act === 'dismiss') body = { kind: 'dismiss', reason: reason.trim() };
		else {
			const [table, id] = evidence.split(':') as ['immunization' | 'procedure' | 'document', string];
			body = { kind: 'done', evidence: { table, id }, reason: note };
		}
		const form = formSeq;
		busy = true;
		error = null;
		try {
			await decideDue(prof.slug, i.key, body);
			// a form opened meanwhile, for this item or another, stays open with its draft
			if (form === formSeq) open = null;
			load();
		} catch (err) {
			if (form === formSeq) error = errText(err);
		} finally {
			busy = false;
		}
	}
	const actReady = $derived(act === 'snooze' ? !!until : act === 'dismiss' ? !!reason.trim() : !!evidence);
	// Undo: the item is as if nothing had been decided.
	let undoErr = $state<{ key: string; text: string } | null>(null);
	async function undo(i: DueItem) {
		if (!prof || busy) return;
		busy = true;
		undoErr = null;
		try {
			await undoDue(prof.slug, i.key);
			load();
		} catch (err) {
			undoErr = { key: i.key, text: errText(err) };
		} finally {
			busy = false;
		}
	}
	// The decision in words, beside the item.
	const decided = (i: DueItem, isHidden: boolean) => {
		const d = i.decision;
		if (!d) return '';
		if (d.kind === 'snooze') return tf(isHidden ? 'due_d_snooze' : 'due_d_was', { d: d.until ?? '' });
		return t(d.kind === 'done' ? 'due_d_done' : 'due_d_dismiss');
	};
</script>

<div class="ph">
	<div>
		<h1>{t('due_nav')}</h1>
		<div class="sub">{prof?.display_name ?? ''} · {t('due_sub')}</div>
	</div>
	<div class="right"><PhaseChip phase="v0.3" /></div>
</div>

{#if !prof}
	<div class="card"><div class="empty">{t('h1_noprofile')} <a href="/family">{t('h1_noprofile_link')}</a></div></div>
{:else if doc.error}
	<div class="card"><NotReady error={doc.error} onretry={load} /></div>
{:else if doc.loading && !doc.data}
	<div class="card"><div class="empty">{t('loading')}</div></div>
{:else if doc.data}
	{@const d = doc.data}
	{#if d.reason}<div class="banner" role="note">{d.reason}</div>{/if}
	{#if d.disclaimer}<div class="note" style="margin-bottom:10px">{d.disclaimer}</div>{/if}
	{#if !items.length}
		<div class="card" style="margin-bottom:10px"><div class="empty">{d.withheld ? withheldText(d) : t('due_none')}</div></div>
	{/if}
	{#each groups.filter((g) => g.rows.length) as g (g.id)}
		<section class="card" style="margin-bottom:12px" aria-labelledby="due-{g.id}">
			<h3 id="due-{g.id}">{g.title}<span class="r num">{g.rows.length}</span></h3>
			{#each g.rows as i (i.key)}{@render row(i, false)}{/each}
		</section>
	{/each}
	{#if hidden.length}
		<details class="card hid" style="margin-bottom:12px">
			<summary><h3>{t('due_hidden')}<span class="r num">{hidden.length}</span></h3></summary>
			{#each hidden as i (i.key)}{@render row(i, true)}{/each}
		</details>
	{/if}
	{#if items.length && d.withheld}<div class="note" style="margin-bottom:10px">{withheldText(d)}</div>{/if}
	{#if d.unmatched?.length}
		<div class="note" style="margin-bottom:10px">
			{t('due_unmatched')}
			{#each d.unmatched as u, k (u.src + k)}{k ? ', ' : ' '}<span>{u.name}</span> <SourceChip label="" name={u.name} src={u.src} onopen={openSource} />{/each}
		</div>
	{/if}
	{#if prof}<div style="margin-bottom:12px"><DueAnswers profile={prof.slug} {canWrite} onchange={load} /></div>{/if}
	<div class="note">{t('due_info')}</div>
	<div class="foot-hash">
		{#if d.prevention_hash}<span>prevention_hash <span class="mono" title={d.prevention_hash}>{shortHash(d.prevention_hash)}</span></span>{/if}
		{#if d.rules}<span>{t('due_rules')} <span class="mono">{d.rules}</span></span>{/if}
		<span>{t('due_asof')} <span class="num">{d.as_of}</span></span>
	</div>
{/if}

{#snippet row(i: DueItem, isHidden: boolean)}
	<div class="due duerow">
		<div class="ic"><svg viewBox="0 0 24 24" aria-hidden="true">{@html ICON[i.area] ?? ICON.lab}</svg></div>
		<div class="w">
			<div><b>{i.title ?? i.key}</b></div>
			<div>{i.reason ?? i.why}</div>
			<div class="meta">
				{#if i.last}
					<span>{tf('due_last', { d: i.last.date })}</span>
					{#if i.last.src}<SourceChip label="" name={i.title ?? i.key} src={i.last.src} onopen={openSource} />{/if}
				{:else if i.status !== 'not_assessed'}
					<span>{t('due_never')}</span>
				{/if}
				{#if i.cite}<a class="chip" href={i.cite} target="_blank" rel="noopener" title={t('due_cite_t')}>{i.source ?? t('due_pack')}</a>{/if}
				{#if i.area === 'lab' && i.group}<a href="/biomarkers?code={encodeURIComponent(i.group)}">{t('due_lab_link')}</a>{/if}
				{#if i.answer && prof}<a href="#due-answers-{prof.slug}">{t('due_ans_link')}</a>{/if}
				{#if i.area !== 'lab'}<span class="mono dim" style="font-size:10px">{i.rule}</span>{/if}
			</div>
			{#if i.decision}
				<div class="meta dec">
					<span class="decw">{decided(i, isHidden)}</span>
					{#if i.decision.reason}<span class="why2">“{i.decision.reason}”</span>{/if}
					{#if canWrite}<button class="btn sm" type="button" disabled={busy} onclick={() => undo(i)}>{t(i.decision.kind === 'snooze' && !isHidden ? 'due_keep' : 'due_undo')}</button>{/if}
				</div>
				{#if undoErr?.key === i.key}<div class="p2err" role="alert">{undoErr.text}</div>{/if}
			{/if}
		</div>
		<div class="st">
			<span class="stw" style="color:{C[dueColor(i.status)]}"
				><i style="background:{C[dueColor(i.status)]}" aria-hidden="true"></i>{t('due_st_' + i.status)}</span
			>
			{#if when(i)}<span class="num dim" style="white-space:nowrap">{when(i)}</span>{/if}
			{#if canWrite && !isHidden}
				<span class="acts">
					{#if i.record}
						<button class="btn sm" type="button" aria-expanded={open === i.key && panel === 'record'} onclick={() => start(i)}>{t('due_record')}</button>
					{/if}
					<button class="btn sm" type="button" title={t('due_act_t')} aria-expanded={open === i.key && panel === 'act'} onclick={() => startAct(i)}>{t('due_manage')}</button>
				</span>
			{/if}
		</div>
	</div>
	{#if open === i.key && panel === 'record' && i.record && !isHidden}
		<form class="p2form recform" onsubmit={(e) => save(e, i)} use:focusOnOpen={{ oncancel: () => (open = null) }} aria-label={t('due_record')}>
			<div class="p2row">
				<label style="flex:1;min-width:160px">{t(i.record.kind === 'immunization' ? 'due_f_vaccine' : 'due_f_procedure')}<input class="p2in" bind:value={name} required maxlength="200" /></label>
				<label>{t('me_f_date')}<input class="p2in" type="date" bind:value={date} max={today} required /></label>
			</div>
			{#if error}<div class="p2err" role="alert">{error}</div>{/if}
			<div class="p2row">
				<button class="btn pri sm" type="submit" disabled={busy || !name.trim()}>{t('me_save')}</button>
				<button class="btn sm" type="button" onclick={() => (open = null)}>{t('i2_cancel')}</button>
			</div>
			<div class="note">{t('due_record_note')}</div>
		</form>
	{/if}
	{#if open === i.key && panel === 'act' && !isHidden}
		<form class="p2form recform" onsubmit={(e) => saveAct(e, i)} use:focusOnOpen={{ oncancel: () => (open = null) }} aria-label="{t('due_act_t')} · {i.title ?? i.key}">
			<div class="seg" role="group" aria-label={t('due_act_t')}>
				{#each ACTS as a (a)}<button type="button" class:on={act === a} aria-pressed={act === a} onclick={() => ((act = a), (error = null))}>{t('due_' + a)}</button>{/each}
			</div>
			{#if act === 'snooze'}
				<label style="max-width:200px">{t('due_until')}<input class="p2in" type="date" bind:value={until} min={today} required /></label>
			{:else if act === 'done'}
				<label
					>{t('due_evidence')}
					{#if evidenceList === null}
						<span class="dim">{t('loading')}</span>
					{:else if !evidenceList.length}
						<span class="dim">{t('due_ev_none')}</span>
					{:else}
						<select class="p2in" bind:value={evidence} required>
							<option value="">{t('due_ev_pick')}</option>
							{#each [...new Set(evidenceList.map((e) => e.group))] as grp (grp)}
								<optgroup label={grp}>
									{#each evidenceList.filter((e) => e.group === grp) as e (e.value)}<option value={e.value}>{e.label}</option>{/each}
								</optgroup>
							{/each}
						</select>
					{/if}
				</label>
				{#if evidenceErr}
					<div class="p2err" role="alert">
						{tf('h1_na_d', { code: errCode(evidenceErr) })}
						<button class="btn sm" type="button" onclick={() => prof && loadEvidence(prof.slug)}>{t('retry')}</button>
					</div>
				{/if}
			{/if}
			<label>{t(act === 'dismiss' ? 'due_reason' : 'due_note_opt')}<input class="p2in" bind:value={reason} maxlength="500" required={act === 'dismiss'} /></label>
			{#if error}<div class="p2err" role="alert">{error}</div>{/if}
			<div class="p2row">
				<button class="btn pri sm" type="submit" disabled={busy || !actReady}>{t('me_save')}</button>
				<button class="btn sm" type="button" onclick={() => (open = null)}>{t('i2_cancel')}</button>
			</div>
		</form>
	{/if}
{/snippet}

<SourcePanel />

<style>
	.duerow {
		align-items: flex-start;
		padding: 9px 0;
	}
	.duerow .w > div:first-child {
		font-size: 13px;
		color: var(--text);
	}
	.meta {
		display: flex;
		gap: 8px;
		flex-wrap: wrap;
		align-items: center;
		margin-top: 4px;
		font-size: 11px;
		color: var(--dim);
	}
	.meta a.chip {
		text-decoration: none;
	}
	.st {
		display: grid;
		justify-items: end;
		gap: 4px;
		font-size: 11.5px;
		flex: none;
		text-align: right;
	}
	.stw {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		font-weight: 600;
	}
	.stw i {
		width: 8px;
		height: 8px;
		border-radius: 50%;
	}
	.acts {
		display: inline-flex;
		gap: 6px;
		flex-wrap: wrap;
		justify-content: flex-end;
	}
	.dec {
		margin-top: 6px;
	}
	.decw {
		color: var(--muted);
		font-weight: 600;
	}
	.why2 {
		color: var(--muted);
		word-break: break-word;
	}
	.hid summary {
		cursor: pointer;
		list-style: none;
	}
	.hid summary::-webkit-details-marker {
		display: none;
	}
	.hid summary h3::before {
		content: '▸ ';
		color: var(--dim);
	}
	.hid[open] summary h3::before {
		content: '▾ ';
	}
	.recform {
		margin: 0 0 10px 36px;
		padding: 10px;
		border: 1px solid var(--line2);
		border-radius: 10px;
	}
	@media (max-width: 640px) {
		.duerow {
			flex-wrap: wrap;
		}
		.st {
			justify-items: start;
			text-align: left;
			width: 100%;
			padding-left: 36px;
			grid-auto-flow: column;
			justify-content: start;
			align-items: center;
		}
		.recform {
			margin-left: 0;
		}
	}
</style>
