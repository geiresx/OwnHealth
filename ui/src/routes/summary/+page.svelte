<script lang="ts">
	// Mockup screen #doctor-summary: the printed doctor summary. The options sit on the left (specialty,
	// language of the sheet, sensitive findings for this print); the server renders the PDF, shown on
	// the right with a download. Printing needs a fresh step-up, so the PDF is made on request only.
	import '#lib/components/pages2/pages2.css';
	import { ApiError, printSummary, SUMMARY_SPECIALTIES, type SummarySpecialty } from '#lib/api';
	import StepUpDialog from '#lib/components/pages2/StepUpDialog.svelte';
	import { withStepUp } from '#lib/components/pages2/stepup.svelte';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { i18n, t } from '#lib/i18n/index.svelte';
	import { currentProfile } from '#lib/state/app.svelte';

	const prof = $derived(currentProfile());
	let specialty = $state<SummarySpecialty>('gp');
	let lang = $state<'es' | 'en'>(i18n.lang === 'es' ? 'es' : 'en');
	let sensitive = $state(false);
	let busy = $state(false);
	let error = $state<unknown>(null);
	let pdf = $state<{ url: string; filename: string; key: string } | null>(null);

	const canGenome = $derived(!!prof?.scopes.includes('read:genome'));
	const sensitiveWhy = $derived(prof?.is_minor ? t('sum_sens_minor') : prof && !prof.genome_sensitive_ui ? t('sum_sens_off') : null);
	const partial = $derived(!!prof && !['read:labs', 'read:records', 'read:genome'].every((s) => (prof.scopes as string[]).includes(s)));
	// What the shown PDF was made from; a change of profile or option makes it out of date.
	const key = $derived(`${prof?.slug}|${specialty}|${lang}|${sensitive && !sensitiveWhy}`);
	const outdated = $derived(!!pdf && pdf.key !== key);

	function drop() {
		if (pdf) URL.revokeObjectURL(pdf.url);
		pdf = null;
	}
	// Another profile: its sheet is another person's, so the old one goes, and the sensitive tick
	// was a choice for that person's print, so it is cleared too.
	$effect(() => {
		void prof?.slug;
		sensitive = false;
		return drop;
	});

	async function show() {
		const slug = prof?.slug;
		if (!slug || busy) return;
		busy = true;
		error = null;
		const made = key;
		try {
			// the options as they were when asked: a retry after the step-up sends these, not later ones
			const body = { lang, specialty, include_sensitive: sensitive && !sensitiveWhy };
			const { blob, filename } = await withStepUp(() => printSummary(slug, body));
			// the profile changed while it was made: this sheet is another person's
			if (prof?.slug !== slug) return;
			drop();
			pdf = { url: URL.createObjectURL(new Blob([blob], { type: 'application/pdf' })), filename, key: made };
		} catch (e) {
			// a cancelled "confirm it is you" is not a failure: the page stays as it was
			if (!(e instanceof ApiError && e.code === 'step_up_required')) error = e;
		} finally {
			busy = false;
		}
	}

	function download() {
		if (!pdf) return;
		const a = document.createElement('a');
		a.href = pdf.url;
		a.download = pdf.filename;
		document.body.appendChild(a);
		a.click();
		a.remove();
	}
</script>

<div class="ph">
	<div>
		<h1>{t('sum_nav')}{prof ? ' · ' + prof.display_name : ''}</h1>
		<div class="sub">{t('sum_sub')}</div>
	</div>
</div>

<div class="sumgrid">
	<section class="card" aria-labelledby="sum-opts">
		<h3 id="sum-opts">{t('sum_opts')}</h3>
		<fieldset>
			<legend>{t('sum_spec')}</legend>
			{#each SUMMARY_SPECIALTIES as s (s)}
				<label class="opt"><input type="radio" name="specialty" value={s} bind:group={specialty} />{t('sum_spec_' + s)}</label>
			{/each}
			<div class="dim hint">{t('sum_spec_d')}</div>
		</fieldset>
		<fieldset>
			<legend>{t('sum_lang')}</legend>
			<label class="opt"><input type="radio" name="lang" value="es" bind:group={lang} />Español</label>
			<label class="opt"><input type="radio" name="lang" value="en" bind:group={lang} />English</label>
		</fieldset>
		{#if canGenome}
			<fieldset>
				<label class="opt"><input type="checkbox" bind:checked={sensitive} disabled={!!sensitiveWhy} aria-describedby="sum-sens-d" />{t('sum_sens')}</label>
				<div class="dim hint" id="sum-sens-d">{sensitiveWhy ?? t('sum_sens_d')}</div>
			</fieldset>
		{/if}
		{#if partial}<div class="dim hint">🔒 {t('sum_scopes')}</div>{/if}
		<div class="acts">
			<button class="btn pri" onclick={show} disabled={!prof || busy} aria-busy={busy}>{busy ? t('sum_making') : t('sum_show')}</button>
			{#if pdf}
				<button class="btn" onclick={download} disabled={outdated}>{t('sum_download')}</button>
				<!-- out of date: no href and out of the tab order, so neither a click nor Enter opens the old sheet -->
				<a class="btn" href={outdated ? undefined : pdf.url} target="_blank" rel="noopener" aria-disabled={outdated} tabindex={outdated ? -1 : undefined}>{t('sum_open')}</a>
			{/if}
		</div>
	</section>

	<section class="card sheet" aria-label={t('sum_frame')} aria-busy={busy}>
		{#if error}
			<NotReady {error} onretry={show} />
		{:else if pdf}
			{#if outdated}<div class="banner">{t('sum_changed')}</div>{/if}
			<iframe src={pdf.url} title={t('sum_frame')}></iframe>
			<!-- phone browsers often show no PDF inside a page: there the sheet opens or downloads -->
			<div class="empty ready">{t('sum_ready')}</div>
		{:else}
			<div class="empty">{busy ? t('sum_making') : t('sum_empty')}</div>
		{/if}
	</section>
</div>

<StepUpDialog />

<style>
	.sumgrid {
		display: grid;
		grid-template-columns: minmax(240px, 300px) 1fr;
		gap: 14px;
		align-items: start;
	}
	fieldset {
		border: 0;
		margin: 0 0 12px;
		padding: 0;
	}
	legend {
		font-size: 12px;
		color: var(--muted);
		margin-bottom: 4px;
	}
	.opt {
		display: flex;
		gap: 8px;
		align-items: center;
		padding: 3px 0;
		font-size: 13px;
	}
	.opt input {
		accent-color: var(--accent);
	}
	.hint {
		font-size: 11.5px;
		margin-top: 4px;
	}
	.acts {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		margin-top: 6px;
	}
	.acts a[aria-disabled='true'] {
		pointer-events: none;
		opacity: 0.5;
	}
	.sheet {
		min-height: 420px;
	}
	.sheet iframe {
		display: block;
		width: 100%;
		height: calc(100vh - 190px);
		min-height: 520px;
		border: 0;
		border-radius: 6px;
		background: #f4f2ee;
	}
	.banner {
		margin-bottom: 8px;
	}
	.ready {
		display: none;
	}
	@media (max-width: 860px) {
		.sumgrid {
			grid-template-columns: 1fr;
		}
		.sheet {
			min-height: 0;
		}
		.sheet iframe {
			display: none;
		}
		.ready {
			display: block;
		}
	}
</style>
