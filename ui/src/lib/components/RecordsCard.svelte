<script lang="ts">
	// The profile's records by kind, each with Correct (a new version with a reason) and Retract (with a
	// reason), and a form to add one by hand. Writing needs a role that may write (not viewer).
	import '#lib/components/pages2/pages2.css';
	import { listRecords, retractRecord } from '#lib/api';
	import type { RecordOut } from '#lib/api/types';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { errText, Loader } from '#lib/components/pages1/util.svelte';
	import { focusOnOpen } from '#lib/actions/focusOnOpen';
	import ManualEntry, { type EntryKind } from './ManualEntry.svelte';
	import { t } from '#lib/i18n/index.svelte';

	let { profile, canWrite, onchange }: { profile: string; canWrite: boolean; onchange?: () => void } = $props();
	type RK = Exclude<EntryKind, 'observation'>;
	const KINDS: RK[] = ['medication', 'condition', 'allergy', 'immunization', 'visit'];
	const kindName = (k: RK) => (k === 'visit' ? t('h1_ty_visit') : t('me_k_' + k));
	let kind = $state<RK>('medication');
	const recs = new Loader<RecordOut[]>();
	let adding = $state(false);
	let correcting = $state<string | null>(null);
	let retracting = $state<string | null>(null);
	let reason = $state('');
	let error = $state<string | null>(null);
	let busy = $state(false);

	function load() {
		const p = profile;
		const k = kind;
		recs.run(async () => (await listRecords(p, k)).items, `${p}/${k}`);
	}
	$effect(load);
	function changed() {
		adding = false;
		correcting = null;
		retracting = null;
		load();
		onchange?.();
	}
	async function retract(e: SubmitEvent, r: RecordOut) {
		e.preventDefault();
		if (busy) return;
		busy = true;
		error = null;
		try {
			await retractRecord(profile, r.kind, r.id, reason.trim());
			reason = '';
			changed();
		} catch (err) {
			error = errText(err);
		} finally {
			busy = false;
		}
	}
	function openRetract(id: string) {
		retracting = retracting === id ? null : id;
		correcting = null;
		reason = '';
		error = null;
	}
	// The second line of a record: words, and dates that never break in the middle.
	const sub = (r: RecordOut): { w?: string; d?: string }[] => {
		const d = r.detail as Record<string, string | null | undefined>;
		const parts: { w?: string; d?: string }[] = [];
		if (d.dose_text) parts.push({ w: d.dose_text });
		if (d.started) parts.push({ w: t('me_f_start'), d: d.started });
		if (d.stopped) parts.push({ w: t('me_f_stop'), d: d.stopped });
		if (d.onset) parts.push({ d: d.onset });
		if (d.occurred) parts.push({ d: d.occurred });
		if (d.reaction_text) parts.push({ w: d.reaction_text });
		if (d.period_start) parts.push({ d: d.period_start.slice(0, 10) });
		if (d.provider_name && d.provider_name !== r.title) parts.push({ w: d.provider_name });
		return parts;
	};
</script>

<div class="card">
	<h3>{t('me_records')}{#if canWrite}<span class="r"><button class="btn sm" onclick={() => ((adding = !adding), (correcting = null))} aria-expanded={adding}>{t('me_add')}</button></span>{/if}</h3>
	<div class="seg fill" role="group" aria-label={t('me_kind')} style="margin-bottom:6px">
		{#each KINDS as k (k)}<button class:on={k === kind} aria-pressed={k === kind} onclick={() => ((kind = k), (adding = false))}>{kindName(k)}</button>{/each}
	</div>
	{#if adding}
		{#key kind}<ManualEntry {profile} kinds={[kind]} onsaved={changed} oncancel={() => (adding = false)} />{/key}
	{/if}
	{#if recs.error}
		<NotReady error={recs.error} onretry={load} />
	{:else}
		{#each recs.data ?? [] as r (r.id)}
			<div style="padding:5px 0;border-top:1px solid #20252f">
				<div class="p2row">
					<div style="flex:1;min-width:0">
						<b>{r.title || kindName(kind)}</b>
						{#if sub(r).length}
							<div class="dim" style="font-size:11px">
								{#each sub(r) as x, i (i)}{i ? ' · ' : ''}{x.w ?? ''}{x.w && x.d ? ' ' : ''}{#if x.d}<span class="num" style="white-space:nowrap">{x.d}</span>{/if}{/each}
							</div>
						{/if}
					</div>
					{#if canWrite}
						<button class="btn sm" onclick={() => ((correcting = correcting === r.id ? null : r.id), (retracting = null))} aria-expanded={correcting === r.id}>{t('me_correct')}</button>
						<button class="btn sm no" onclick={() => openRetract(r.id)} aria-expanded={retracting === r.id}>{t('me_retract')}</button>
					{/if}
				</div>
				{#if correcting === r.id}
					<ManualEntry {profile} correcting={r} onsaved={changed} oncancel={() => (correcting = null)} />
				{:else if retracting === r.id}
					<form class="p2row" style="margin-top:4px" onsubmit={(e) => retract(e, r)} use:focusOnOpen={{ oncancel: () => (retracting = null) }}>
						<span class="muted" style="font-size:12px;flex-basis:100%">{t('me_retract_d')}</span>
						<input class="p2in" style="flex:1;min-width:140px" bind:value={reason} required maxlength="500" placeholder={t('me_reason')} aria-label={t('me_reason')} />
						<button class="btn sm" type="button" onclick={() => (retracting = null)}>{t('i2_cancel')}</button>
						<button class="btn sm no" type="submit" disabled={busy}>{t('me_retract')}</button>
					</form>
					{#if error}<div class="p2err" role="alert">{error}</div>{/if}
				{/if}
			</div>
		{:else}
			<div class="dim" style="font-size:12px">{recs.loading ? t('loading') : t('me_none')}</div>
		{/each}
	{/if}
</div>
