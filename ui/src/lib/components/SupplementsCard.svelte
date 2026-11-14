<script lang="ts">
	// Mockup screen #drug-check, the Supplements card: every supplement with a photo of its label and the
	// decision the person recorded on it (keep, stop, do not buy again) with their reason. The decision is
	// the person's own choice, stored as a new version of the record; the app never suggests one.
	import { addMedicationLabel, correctRecord, documentFileUrl } from '#lib/api';
	import type { RecordOut } from '#lib/api/types';
	import SourceChip from '#lib/components/SourceChip.svelte';
	import { openSource } from '#lib/components/pages1/source.svelte';
	import { errText, tf } from '#lib/components/pages1/util.svelte';
	import { t } from '#lib/i18n/index.svelte';
	import { DECISIONS, DECISION_RECORDED, decisionOf, decisionReasonOf, isCurrent, supplementRows, supplementState, type Decision } from '#lib/logic/meds';
	import { todayIso } from '#lib/logic/people';
	import { tick } from 'svelte';

	let {
		profile,
		records,
		canWrite,
		today,
		onsaved
	}: {
		profile: string;
		/** Every medication record of the profile, with history. */
		records: RecordOut[];
		canWrite: boolean;
		/** The page's own date, so this card and the medication list agree on what is taken today. */
		today: string;
		onsaved: () => void;
	} = $props();

	const rows = $derived(supplementRows(records, today));
	const s = (r: RecordOut, k: string) => (typeof r.detail[k] === 'string' && (r.detail[k] as string).trim() ? (r.detail[k] as string) : null);

	// One open decision form and one upload at a time; errors stay with their row.
	let deciding = $state<{ id: string; decision: Decision } | null>(null);
	let reason = $state('');
	let busy = $state<string | null>(null);
	let uploading = $state<string | null>(null);
	let errors = $state<Record<string, string>>({});
	const inputs: Record<string, HTMLInputElement> = {};
	let reasonInput = $state<HTMLInputElement | null>(null);

	async function pick(r: RecordOut, d: Decision) {
		deciding = { id: r.id, decision: d };
		reason = decisionOf(r.detail) === d ? (decisionReasonOf(r.detail) ?? '') : '';
		errors = { ...errors, [r.id]: '' };
		// the reason is the next thing to write
		await tick();
		reasonInput?.focus();
	}

	async function saveDecision(e: SubmitEvent, r: RecordOut) {
		e.preventDefault();
		const open = deciding;
		if (!open) return;
		busy = r.id;
		try {
			await correctRecord(profile, 'medication', r.id, {
				...r.detail,
				decision: open.decision,
				decision_reason: reason.trim() || null,
				// the day the person decided, on their own calendar
				decision_on: todayIso(),
				reason: DECISION_RECORDED
			});
			// a form opened on another row while this one saved stays open
			if (deciding === open) deciding = null;
			onsaved();
		} catch (err) {
			errors = { ...errors, [r.id]: errText(err) };
		} finally {
			busy = null;
		}
	}

	const IMAGE = ['image/png', 'image/jpeg', 'image/webp'];
	async function upload(r: RecordOut, input: HTMLInputElement) {
		const file = input.files?.[0];
		input.value = '';
		if (!file) return;
		if (file.type && !IMAGE.includes(file.type)) {
			errors = { ...errors, [r.id]: t('med_sup_photo_type') };
			return;
		}
		busy = uploading = r.id;
		errors = { ...errors, [r.id]: '' };
		try {
			// the Documents list names the photo after the supplement, not the camera's IMG_1234.jpg
			const ext = file.name.match(/\.[A-Za-z0-9]{1,5}$/)?.[0] ?? '';
			await addMedicationLabel(profile, r.id, file, `${s(r, 'name') ?? r.title} – ${t('med_sup_photo_file')}${ext}`);
			onsaved();
		} catch (err) {
			errors = { ...errors, [r.id]: errText(err) };
		} finally {
			busy = uploading = null;
		}
	}
</script>

<section class="card sup" aria-labelledby="med-supp">
	<h3 id="med-supp">{t('med_sup_title')}</h3>
	<div class="dim hint">{t('med_sup_d')}</div>
	{#each rows as r (r.id)}
		{@const label = r.labels?.[0]}
		{@const decision = decisionOf(r.detail)}
		{@const name = s(r, 'name') ?? r.title}
		<div class="row">
			<div class="thumb">
				{#if label}
					<a href={documentFileUrl(label)} target="_blank" rel="noopener" title={t('med_sup_photo_open')}
						><img src={documentFileUrl(label)} alt={tf('med_sup_photo_alt', { name })} loading="lazy" width="64" height="64" /></a
					>
				{:else}
					<div class="ph-img" aria-hidden="true">
						<svg width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"
							><rect x="3" y="6" width="18" height="14" rx="2" /><circle cx="12" cy="13" r="3.5" /><path d="M8 6l1.5-2h5L16 6" /></svg
						>
					</div>
				{/if}
			</div>
			<div class="body">
				<div>
					<b>{name}</b>{#if s(r, 'strength')}{' '}{s(r, 'strength')}{/if}
					<span class="pill {isCurrent(r, today) ? 'p-ok' : 'p-stale'}" style="margin-left:4px"
						><i aria-hidden="true"></i>{t('med_st_' + supplementState(r, today))}</span
					>
				</div>
				<div class="line">
					{#if decision}
						<span>{t('med_sup_decision')}: <b>{t('med_sup_dec_' + decision)}</b></span>
						{#if s(r, 'decision_on')}<span class="muted"> · {tf('med_sup_on', { date: s(r, 'decision_on') ?? '' })}</span>{/if}
					{:else}
						<span class="muted">{t('med_sup_no_decision')}</span>
					{/if}
					<SourceChip label="" name={r.title} src={r.src} onopen={openSource} />
				</div>
				{#if decision && decisionReasonOf(r.detail)}<div class="dim line">{t('med_reason')}: {decisionReasonOf(r.detail)}</div>{/if}
				{#if canWrite}
					<div class="p2row acts">
						<div class="seg" role="group" aria-label={t('med_sup_seg')}>
							{#each DECISIONS as d (d)}
								{@const on = deciding?.id === r.id ? deciding.decision === d : decision === d}
								<button type="button" class:on aria-pressed={on} disabled={busy === r.id} onclick={() => pick(r, d)}>{t('med_sup_dec_' + d)}</button>
							{/each}
						</div>
						<input
							bind:this={inputs[r.id]}
							type="file"
							accept={IMAGE.join(',')}
							hidden
							onchange={(e) => upload(r, e.currentTarget)}
						/>
						<button type="button" class="btn sm" disabled={busy === r.id} onclick={() => inputs[r.id]?.click()}>
							{uploading === r.id ? t('med_sup_uploading') : label ? t('med_sup_photo_change') : t('med_sup_photo_add')}
						</button>
					</div>
					{#if deciding?.id === r.id}
						<form class="p2form decide" onsubmit={(e) => saveDecision(e, r)}>
							<label>{t('med_sup_reason')}<input class="p2in" bind:this={reasonInput} bind:value={reason} maxlength="500" onkeydown={(e) => e.key === 'Escape' && (deciding = null)} /></label>
							<div class="p2row">
								<button class="btn pri sm" type="submit" disabled={busy === r.id}>{t('med_sup_save')}</button>
								<button class="btn sm" type="button" onclick={() => (deciding = null)}>{t('i2_cancel')}</button>
							</div>
						</form>
					{/if}
				{/if}
				{#if errors[r.id]}<div class="p2err" role="alert">{errors[r.id]}</div>{/if}
			</div>
		</div>
	{:else}
		<div class="muted">{t('med_sup_none')}</div>
	{/each}
</section>

<style>
	.sup {
		margin-bottom: 14px;
	}
	.hint {
		font-size: 11.5px;
		margin: -4px 0 6px;
	}
	.row {
		display: flex;
		gap: 12px;
		align-items: flex-start;
		padding: 9px 0;
		border-top: 1px solid #20252f;
	}
	.row:first-of-type {
		border-top: 0;
	}
	.thumb {
		flex: none;
		width: 64px;
		height: 64px;
	}
	.thumb img,
	.ph-img {
		width: 64px;
		height: 64px;
		border-radius: 8px;
		border: 1px solid var(--line2);
		object-fit: cover;
		display: block;
	}
	.ph-img {
		display: grid;
		place-items: center;
		color: var(--muted);
		background: var(--surface);
	}
	.body {
		flex: 1;
		min-width: 0;
	}
	.line {
		font-size: 12px;
		margin-top: 3px;
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 4px;
	}
	.acts {
		margin-top: 7px;
	}
	.decide {
		margin-top: 7px;
		max-width: 420px;
	}
</style>
