<script module lang="ts">
	export type EntryKind = 'observation' | 'medication' | 'condition' | 'allergy' | 'immunization' | 'visit' | 'smoking';
</script>

<script lang="ts">
	// Enter a value or a record by hand (observation, current medication, condition, allergy, vaccine, visit),
	// or correct a record: a correction is stored as a new version with a reason, the old one stays.
	import '#lib/components/pages2/pages2.css';
	import { untrack } from 'svelte';
	import { addManualObservation, correctRecord, createRecord } from '#lib/api';
	import type { RecordOut } from '#lib/api/types';
	import { focusOnOpen } from '#lib/actions/focusOnOpen';
	import { errText } from '#lib/components/pages1/util.svelte';
	import { t } from '#lib/i18n/index.svelte';
	import { decimalText } from '#lib/logic/format';
	import { visitPeriod } from '#lib/logic/visit';
	import { todayIso } from '#lib/logic/people';

	type Fields = Record<string, string>;
	let {
		profile,
		kinds = ['observation', 'medication', 'condition', 'allergy', 'immunization'],
		correcting = null,
		note = '',
		date = null,
		onsaved,
		oncancel
	}: {
		profile: string;
		kinds?: EntryKind[];
		correcting?: RecordOut | null;
		note?: string;
		/** The day an observation starts on; '' leaves it empty (required), null means today. */
		date?: string | null;
		onsaved: (kind: EntryKind, id?: string) => void;
		oncancel?: () => void;
	} = $props();

	// The fields each kind asks for, in order: [field, dict key, input type, required].
	const FIELDS: Record<EntryKind, [string, string, string, boolean][]> = {
		observation: [['name', 'me_f_name', 'text', true], ['value', 'me_f_value', 'text', true], ['unit', 'me_f_unit', 'text', false], ['date', 'me_f_date', 'date', true]],
		// medication (slice 3): what, how much, when it is taken, since when, who prescribed it and why
		medication: [
			['name', 'med_f_name', 'text', true],
			['strength', 'me_f_strength', 'text', false],
			['dose_text', 'me_f_dose', 'text', false],
			['schedule_text', 'med_f_schedule', 'text', false],
			['started', 'me_f_start', 'date', false],
			['stopped', 'me_f_stop', 'date', false],
			['prescriber', 'med_f_prescriber', 'text', false],
			['reason_text', 'med_f_reason', 'text', false]
		],
		condition: [['text', 'me_f_cond', 'text', true], ['onset', 'me_f_onset', 'date', false], ['abatement', 'me_f_abate', 'date', false]],
		allergy: [['substance', 'me_f_subst', 'text', true], ['reaction_text', 'me_f_reaction', 'text', false]],
		immunization: [['vaccine', 'me_f_vaccine', 'text', true], ['occurred', 'me_f_occurred', 'date', false], ['dose_number', 'me_f_dosen', 'number', false]],
		// visit (slice 6): the day it happened, why, and where; it goes on the timeline on that day
		visit: [['period_start', 'tl_f_day', 'date', true], ['reason_text', 'tl_f_reason', 'text', false], ['provider_name', 'tl_f_provider', 'text', false]],
		// smoking status (slice 20): one answer and the day it was true; the risk calculators read it
		smoking: [['date', 'me_f_date', 'date', true]]
	};
	// The smoking answers as SNOMED CT codes (LOINC 72166-2), in the order the block shows them.
	const SMOKING = [['266919005', 'calc_smk_never'], ['8517006', 'calc_smk_former'], ['77176002', 'calc_smk_current']] as const;
	// The smoking block puts focus on its first answer when it opens or is chosen, after the form's own
	// focus on open (one frame), so Tab goes on to the day and Save.
	function firstAnswer(node: HTMLElement) {
		let inner = 0;
		const outer = requestAnimationFrame(() => (inner = requestAnimationFrame(() => node.querySelector<HTMLElement>('button')?.focus({ preventScroll: true }))));
		return {
			destroy() {
				cancelAnimationFrame(outer);
				cancelAnimationFrame(inner);
			}
		};
	}
	const kindName = (k: EntryKind) => (k === 'visit' ? t('h1_ty_visit') : k === 'smoking' ? t('calc_smoking') : t('me_k_' + k));
	const today = todayIso();
	const fresh = (k: EntryKind): Fields =>
		k === 'observation' ? { date: date ?? today, note } : k === 'medication' ? { kind: 'medication' } : k === 'smoking' ? { date: today, code: '' } : {};
	const fromRecord = (r: RecordOut): Fields => ({
		// a visit's day is the date part of its start
		...Object.fromEntries(FIELDS[r.kind as EntryKind].map(([f]) => [f, r.detail[f] == null ? '' : f === 'period_start' ? String(r.detail[f]).slice(0, 10) : String(r.detail[f])])),
		...(r.kind === 'medication' ? { kind: String(r.detail.kind ?? 'medication') } : {})
	});

	// The form starts from the props once; a parent that needs a fresh form remounts it ({#key}).
	const start = untrack(() => (correcting ? { kind: correcting.kind as EntryKind, f: fromRecord(correcting) } : { kind: kinds[0], f: fresh(kinds[0]) }));
	let kind = $state<EntryKind>(start.kind);
	let f = $state<Fields>(start.f);
	let reason = $state('');
	let busy = $state(false);
	let error = $state<string | null>(null);
	let saved = $state(0);

	function pick(k: EntryKind) {
		kind = k;
		f = fresh(k);
		error = null;
	}
	const val = (k: string) => (f[k] ?? '').trim() || null;
	function body(): Record<string, unknown> {
		const out: Record<string, unknown> = {};
		for (const [k, , type] of FIELDS[kind]) out[k] = type === 'number' ? (val(k) ? Number(val(k)) : null) : val(k);
		if (kind === 'medication') {
			out.med_status = val('stopped') && val('stopped')! <= today ? 'stopped' : 'active';
			out.kind = f.kind === 'supplement' ? 'supplement' : 'medication';
		}
		if (kind === 'condition') out.clinical_status = val('abatement') ? 'resolved' : 'active';
		// the visit's day at noon UTC; a correction on the same day keeps the stored start (visitPeriod)
		if (kind === 'visit') Object.assign(out, visitPeriod(val('period_start'), correcting?.detail));
		return out;
	}
	// The timeline places a new medication on its start day, so a new one needs it. A correction keeps
	// whatever the record already had.
	const needsStart = (k: string) => kind === 'medication' && k === 'started' && !correcting;
	// The value of an observation is a number: say so in the reader's language before anything is sent.
	const isValue = (k: string) => kind === 'observation' && k === 'value';
	function checkValue(e: Event) {
		const el = e.currentTarget as HTMLInputElement;
		el.setCustomValidity(el.value.trim() && decimalText(el.value) == null ? t('err_decimal') : '');
	}
	async function save(e: SubmitEvent) {
		e.preventDefault();
		error = null;
		// no answer is chosen for the person: the block waits for one
		if (kind === 'smoking' && !SMOKING.some(([c]) => c === f.code)) {
			error = t('calc_smk_pick');
			return;
		}
		if (kind === 'observation' && !correcting && decimalText(f.value ?? '') == null) {
			error = t('err_decimal');
			return;
		}
		busy = true;
		let id: string | undefined;
		try {
			if (correcting) {
				await correctRecord(profile, correcting.kind, correcting.id, { ...correcting.detail, ...body(), reason: reason.trim() });
			} else if (kind === 'smoking') {
				// the code is what counts; the words are the answer as the person saw it
				const answer = SMOKING.find(([c]) => c === f.code)!;
				id = (await addManualObservation(profile, {
					name: t('calc_smoking'),
					value: t(answer[1]),
					loinc: '72166-2',
					value_code: answer[0],
					category: 'survey',
					effective_at: `${val('date') ?? today}T12:00:00Z`
				})).id;
			} else if (kind === 'observation') {
				id = (await addManualObservation(profile, {
					name: val('name')!,
					value: decimalText(f.value ?? '')!,
					unit: val('unit'),
					effective_at: `${val('date') ?? today}T12:00:00Z`,
					note: val('note')
				})).id;
			} else {
				id = (await createRecord(profile, kind, body())).id;
			}
			saved++;
			const k = kind;
			if (!correcting) f = { ...fresh(k), ...(k === 'observation' || k === 'smoking' ? { date: f.date } : {}) };
			onsaved(k, id);
		} catch (err) {
			error = errText(err);
		} finally {
			busy = false;
		}
	}
</script>

<form class="p2form" onsubmit={save} use:focusOnOpen={{ oncancel }} aria-label={correcting ? t('me_correct') : t('me_add')}>
	{#if !correcting && kinds.length > 1}
		<div class="seg" role="group" aria-label={t('me_kind')}>
			{#each kinds as k (k)}<button type="button" class:on={k === kind} aria-pressed={k === kind} onclick={() => pick(k)}>{kindName(k)}</button>{/each}
		</div>
	{/if}
	{#if kind === 'medication'}
		<div class="seg" role="group" aria-label={t('med_f_kind')}>
			{#each ['medication', 'supplement'] as mk (mk)}<button type="button" class:on={(f.kind ?? 'medication') === mk} aria-pressed={(f.kind ?? 'medication') === mk} onclick={() => (f.kind = mk)}>{t('med_k_' + mk)}</button>{/each}
		</div>
	{/if}
	{#if kind === 'smoking'}
		<div style="display:grid;gap:3px;font-size:11.5px;color:var(--muted)">
			<span aria-hidden="true">{t('calc_smoking')}</span>
			<div class="seg" role="group" aria-label={t('calc_smoking')} use:firstAnswer>
				{#each SMOKING as [code, key] (code)}<button type="button" class:on={f.code === code} aria-pressed={f.code === code} onclick={() => (f.code = code)}>{t(key)}</button>{/each}
			</div>
		</div>
	{/if}
	<div class="p2row">
		{#each FIELDS[kind] as [k, key, type, req] (k)}
			<label style={type === 'text' ? 'flex:1;min-width:120px' : ''}
				>{t(key)}{#if isValue(k)}<input
						class="p2in"
						type="text"
						inputmode="decimal"
						pattern={'-?(\\d+([.,]\\d+)?|[.,]\\d+)'}
						bind:value={f[k]}
						required={req}
						oninput={checkValue}
					/>{:else}<input class="p2in" {type} bind:value={f[k]} required={req || needsStart(k)} max={((kind === 'observation' || kind === 'smoking') && k === 'date') || (kind === 'visit' && k === 'period_start') ? today : undefined} />{/if}</label
			>
		{/each}
	</div>
	{#if kind === 'medication' && !correcting}<div class="dim" style="font-size:11.5px">{t('me_start_why')}</div>{/if}
	{#if kind === 'observation' && !correcting}
		<label>{t('me_f_note')}<input class="p2in" bind:value={f.note} maxlength="4000" /></label>
	{/if}
	{#if correcting}
		<label>{t('me_reason')}<input class="p2in" bind:value={reason} required maxlength="500" /></label>
	{/if}
	{#if error}<div class="p2err" role="alert">{error}</div>{/if}
	<div class="p2row">
		<button class="btn pri sm" type="submit" disabled={busy}>{correcting ? t('me_save_corr') : t('me_save')}</button>
		{#if oncancel}<button class="btn sm" type="button" onclick={oncancel}>{t('i2_cancel')}</button>{/if}
		{#if saved && !correcting}<span class="dim" style="font-size:11.5px" aria-live="polite">{t('me_saved')}</span>{/if}
	</div>
	<div class="note">{t('me_note')}</div>
</form>
