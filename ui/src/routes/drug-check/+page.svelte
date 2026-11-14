<script lang="ts">
	// Mockup screen #drug-check, the Medications screen: the medicines and supplements taken now (dose,
	// schedule, since when, why, who prescribed them, the codes the source sent, PGx badges), the history
	// of each medicine (a pause and a restart, corrected versions), and the check-a-drug box.
	import '#lib/components/pages2/pages2.css';
	import { getDrugCheckView, listRecords } from '#lib/api';
	import type { DrugCheckView, RecordOut } from '#lib/api/types';
	import DrugCheckCard from '#lib/components/DrugCheckCard.svelte';
	import ManualEntry from '#lib/components/ManualEntry.svelte';
	import PhaseChip from '#lib/components/PhaseChip.svelte';
	import SourceChip from '#lib/components/SourceChip.svelte';
	import SupplementsCard from '#lib/components/SupplementsCard.svelte';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import SourcePanel from '#lib/components/pages1/SourcePanel.svelte';
	import { openSource } from '#lib/components/pages1/source.svelte';
	import { Loader, tf } from '#lib/components/pages1/util.svelte';
	import { i18n, t } from '#lib/i18n/index.svelte';
	import { isActionable } from '#lib/logic/health';
	import { DECISION_RECORDED, codeChips, decisionOf, decisionReasonOf, historyGroups, isCurrent, pgxFor, replacedReason, type PgxViews } from '#lib/logic/meds';
	import { currentProfile } from '#lib/state/app.svelte';

	const prof = $derived(currentProfile());
	const canWrite = $derived(!!prof && prof.role !== 'viewer');
	const today = new Date().toISOString().slice(0, 10);
	const meds = new Loader<RecordOut[]>();
	const pgx = new Loader<PgxViews<DrugCheckView>>();
	let adding = $state(false);
	let correcting = $state<string | null>(null);

	function load() {
		const slug = prof?.slug;
		if (!slug) return;
		meds.run(async () => (await listRecords(slug, 'medication', true)).items, slug);
	}
	$effect(load);
	function saved() {
		adding = false;
		correcting = null;
		load();
	}

	// Medicines first, then supplements, each by name.
	const current = $derived(
		(meds.data ?? [])
			.filter((r) => isCurrent(r, today))
			.sort((a, b) => Number(a.detail.kind === 'supplement') - Number(b.detail.kind === 'supplement') || a.title.localeCompare(b.title))
	);
	const groups = $derived(historyGroups(meds.data ?? [], today));
	const s = (r: RecordOut, k: string) => (typeof r.detail[k] === 'string' && (r.detail[k] as string).trim() ? (r.detail[k] as string) : null);
	// The name the drug check resolves: the ingredient when the record has one.
	const drugOf = (r: RecordOut) => s(r, 'ingredient') ?? s(r, 'name') ?? r.title;

	// One drug check per current medicine, for its PGx badges. A drug the check does not know has none;
	// supplements are not in CPIC, so they are not asked about. Answers carry the profile they were asked
	// for, so another profile's answers never show here.
	$effect(() => {
		const slug = prof?.slug;
		const lang = i18n.lang;
		const names = [...new Set(current.filter((r) => s(r, 'kind') !== 'supplement').map(drugOf))].sort();
		if (!slug || !names.length) return;
		pgx.run(async () => {
			const answers = await Promise.all(names.map((n) => getDrugCheckView(slug, n, lang).catch(() => null)));
			return { slug, views: Object.fromEntries(names.map((n, i) => [n, answers[i]])) };
		}, `${slug}/${lang}/${names.join('|')}`);
	});
	const view = (r: RecordOut) => pgxFor(pgx.data, prof?.slug, drugOf(r), r.detail.kind);
	const badges = (r: RecordOut) => view(r)?.pgx ?? [];
	const guided = (r: RecordOut, gene: string) => (view(r)?.guidance ?? []).some((g) => g.source !== 'DPWG' && g.applies_to.startsWith(gene));
	// What a version's dates do not already say: corrected, retracted, stopped or on hold.
	const stateWord = (r: RecordOut) =>
		r.status === 'superseded'
			? t('med_st_superseded')
			: r.status === 'retracted'
				? t('med_st_retracted')
				: (s(r, 'med_status') ?? 'active') === 'active'
					? null
					: t('med_st_' + s(r, 'med_status'));
	// Why a version was replaced, in the reader's language when an agent proposal replaced it.
	const replaced = (r: RecordOut) => {
		if (r.status_reason === DECISION_RECORDED) return t('med_sup_corr_reason');
		const { action, text } = replacedReason(r.status_reason);
		return [action ? t('med_by_agent_' + action) : null, text].filter(Boolean).join(': ') || null;
	};
</script>

<div class="ph">
	<div>
		<h1>{t('med_nav')}</h1>
		<div class="sub">{prof?.display_name ?? ''} · {t('med_sub')}</div>
	</div>
	<div class="right"><PhaseChip phase="v0.1" /><PhaseChip phase="v0.3" /></div>
</div>

<div class="drug" style="margin-bottom:14px">
	<section class="card" aria-labelledby="med-current">
		<h3 id="med-current">
			{t('med_current')}
			{#if canWrite}<span class="r"><button class="btn sm" onclick={() => ((adding = !adding), (correcting = null))} aria-expanded={adding}>{t('me_add')}</button></span>{/if}
		</h3>
		{#if adding && prof}
			<ManualEntry profile={prof.slug} kinds={['medication']} onsaved={saved} oncancel={() => (adding = false)} />
		{/if}
		{#if meds.error}
			<NotReady error={meds.error} onretry={load} />
		{:else if meds.loading && !meds.data}
			<div class="dim">{t('loading')}</div>
		{:else}
			{#each current as r (r.id)}
				{@const chips = codeChips(r.detail)}
				<div class="medrow">
					<div class="p2row" style="align-items:flex-start;flex-wrap:nowrap">
						<div style="flex:1;min-width:0">
							<div>
								<b>{s(r, 'name') ?? r.title}</b>{#if s(r, 'strength')}{' '}{s(r, 'strength')}{/if}
								{#if s(r, 'kind') === 'supplement'}<span class="chip" style="margin-left:4px">{t('med_supplement')}</span>{/if}
							</div>
							<div class="muted line">
								{#each [s(r, 'dose_text'), s(r, 'schedule_text')].filter(Boolean) as x, i (i)}{i ? ' · ' : ''}{x}{/each}
								{#if s(r, 'started')}{s(r, 'dose_text') || s(r, 'schedule_text') ? ' · ' : ''}<span style="white-space:nowrap">{t('med_since')} <span class="num">{s(r, 'started')}</span></span>{/if}
								{#if !badges(r).length && !chips.length}<SourceChip label="" name={r.title} src={r.src} onopen={openSource} />{/if}
							</div>
							{#if s(r, 'reason_text') || s(r, 'prescriber')}
								<div class="dim line">
									{#if s(r, 'reason_text')}{t('med_reason')}: {s(r, 'reason_text')}{/if}
									{#if s(r, 'prescriber')}{s(r, 'reason_text') ? ' · ' : ''}{t('med_prescriber')}: {s(r, 'prescriber')}{/if}
								</div>
							{/if}
							{#if badges(r).length || chips.length}<div class="chips">
								{#each badges(r) as x (x.gene)}
									{@const na = x.evidence == null && x.diplotype == null}
									<a
										class="pill {na ? 'p-stale' : guided(r, x.gene) && isActionable(x.phenotype) ? 'p-watch' : 'p-info'}"
										href="?drug={encodeURIComponent(drugOf(r))}#check"
										title={t('med_pgx_open')}
										><i aria-hidden="true"></i>{na ? tf('med_pgx_na', { gene: x.gene }) : `${x.gene} · ${x.phenotype ?? x.diplotype}`}</a
									>
								{/each}
								{#each chips as c (c.label + c.code)}<span class="chip mono" title={[t('med_codes'), c.display, c.system].filter(Boolean).join(' · ')}>{c.label} {c.code}</span>{/each}
								<SourceChip label="" name={r.title} src={r.src} onopen={openSource} />
							</div>{/if}
						</div>
						{#if canWrite}<button class="btn sm" onclick={() => ((correcting = correcting === r.id ? null : r.id), (adding = false))} aria-expanded={correcting === r.id}>{t('me_correct')}</button>{/if}
					</div>
					{#if correcting === r.id && prof}
						<ManualEntry profile={prof.slug} correcting={r} onsaved={saved} oncancel={() => (correcting = null)} />
					{/if}
				</div>
			{:else}
				<div class="muted">{t('med_none')}</div>
			{/each}
		{/if}
	</section>

	<section class="card" aria-labelledby="med-history">
		<h3 id="med-history">{t('med_history')}</h3>
		<div class="dim" style="font-size:11.5px;margin:-4px 0 6px">{t('med_history_d')}</div>
		{#if meds.error}
			<NotReady error={meds.error} onretry={load} />
		{:else if meds.loading && !meds.data}
			<div class="dim">{t('loading')}</div>
		{:else}
			{#each groups as g (g.key)}
				<div class="medrow">
					<div><b>{g.title}</b>{#if g.state}<span class="pill p-stale" style="margin-left:6px"><i aria-hidden="true"></i>{t('med_st_' + g.state)}</span>{/if}</div>
					<ol class="hist">
						{#each g.entries as e (e.id)}
							<li class:old={e.status !== 'current'}>
								<!-- a replaced version's end is the version that replaced it, so it shows only its start -->
								<span class="num" style="white-space:nowrap">{s(e, 'started') ?? '—'}{#if e.status !== 'superseded' && e.status !== 'retracted'}{' '}→ {s(e, 'stopped') ?? t('med_ongoing')}{/if}</span>
								{#if stateWord(e)}<span class="muted">· {stateWord(e)}</span>{/if}
								{#if s(e, 'strength') || s(e, 'dose_text') || s(e, 'schedule_text')}<span class="muted">· {[s(e, 'strength'), s(e, 'dose_text'), s(e, 'schedule_text')].filter(Boolean).join(' · ')}</span>{/if}
								{#if s(e, 'reason_text')}<span class="muted">· {t('med_reason')}: {s(e, 'reason_text')}</span>{/if}
								{#if decisionOf(e.detail)}<span class="muted"
										>· {t('med_sup_decision')}: {t('med_sup_dec_' + decisionOf(e.detail))}{#if s(e, 'decision_on')}{' '}{tf('med_sup_on', { date: s(e, 'decision_on') ?? '' })}{/if}{#if decisionReasonOf(e.detail)}{' '}· {t('med_reason')}: {decisionReasonOf(e.detail)}{/if}</span
									>{/if}
								{#if replaced(e)}<span class="dim">· {replaced(e)}</span>{/if}
								<SourceChip label="" name={e.title} src={e.src} onopen={openSource} />
							</li>
						{/each}
					</ol>
				</div>
			{:else}
				<div class="muted">{t('med_none_hist')}</div>
			{/each}
		{/if}
	</section>
</div>

{#if prof && meds.data}<SupplementsCard profile={prof.slug} records={meds.data} {canWrite} {today} onsaved={load} />{/if}

<DrugCheckCard />

<SourcePanel />

<style>
	.medrow {
		padding: 7px 0;
		border-top: 1px solid #20252f;
	}
	.medrow:first-of-type {
		border-top: 0;
	}
	.line {
		font-size: 12px;
		margin-top: 2px;
	}
	.chips {
		display: flex;
		flex-wrap: wrap;
		gap: 5px;
		align-items: center;
		margin-top: 5px;
	}
	.chips a.pill {
		text-decoration: none;
	}
	.hist {
		list-style: none;
		margin: 4px 0 0;
		padding: 0 0 0 10px;
		border-left: 2px solid var(--line2);
		font-size: 12px;
		display: grid;
		gap: 3px;
	}
	.hist .old {
		opacity: 0.7;
	}
</style>
