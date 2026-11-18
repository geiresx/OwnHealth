<script lang="ts">
	// Mockup screen #due, tab "Next draw": every test the record lists for the next blood draw, from
	// GET /profiles/{p}/next-draw (read:labs). The server joins the overdue retests, the tests never
	// measured and the last values outside the lab range, lists each once with every reason, and groups
	// them by the preparation a cited source describes; this page shows them. The same list prints as a
	// PDF request for the doctor (GET .../next-draw.pdf), in either language, with or without the name.
	import '#lib/components/pages2/pages2.css';
	import { getNextDraw, nextDrawPdf } from '#lib/api';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import SourcePanel from '#lib/components/pages1/SourcePanel.svelte';
	import { openSource } from '#lib/components/pages1/source.svelte';
	import { Loader, shortHash, tp } from '#lib/components/pages1/util.svelte';
	import PhaseChip from '#lib/components/PhaseChip.svelte';
	import SourceChip from '#lib/components/SourceChip.svelte';
	import { i18n, t } from '#lib/i18n/index.svelte';
	import type { NextDraw } from '#lib/api/types';
	import { todayIso } from '#lib/logic/people';
	import { currentProfile } from '#lib/state/app.svelte';
	import Tabs from './Tabs.svelte';

	const prof = $derived(currentProfile());
	const plan = new Loader<NextDraw>();
	function load() {
		const slug = prof?.slug;
		const lang = i18n.lang;
		if (slug) plan.run(() => getNextDraw(slug, lang, todayIso()), `${slug}/${lang}`);
	}
	$effect(load);

	// The printed request: its own language (the doctor's, maybe not the reader's) and the name choice.
	let lang = $state<'es' | 'en'>(i18n.lang === 'es' ? 'es' : 'en');
	let hide = $state(false);
	let pdf = $state<{ url: string; filename: string } | null>(null);
	let pdfErr = $state<unknown>(null);
	let busy = $state(false);
	let seq = 0;
	function drop() {
		if (pdf) URL.revokeObjectURL(pdf.url);
		pdf = null;
	}
	// another profile or option makes another sheet; only the latest request fills the preview
	async function makePdf(slug: string, l: 'es' | 'en', h: boolean) {
		const mine = ++seq;
		busy = true;
		pdfErr = null;
		try {
			const { blob, filename } = await nextDrawPdf(slug, { lang: l, hide_name: h, as_of: todayIso() });
			if (mine !== seq) return;
			drop();
			pdf = { url: URL.createObjectURL(new Blob([blob], { type: 'application/pdf' })), filename };
		} catch (e) {
			if (mine === seq) pdfErr = e;
		} finally {
			if (mine === seq) busy = false;
		}
	}
	// without read:labs the list says the view is not shared; no sheet is asked for either
	$effect(() => {
		const slug = prof?.slug;
		if (slug && (prof.scopes as string[]).includes('read:labs')) makePdf(slug, lang, hide);
	});
	// a sheet is one person's: it goes with the page or the profile
	$effect(() => {
		void prof?.slug;
		return drop;
	});
	const retryPdf = () => prof && makePdf(prof.slug, lang, hide);

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
		<h1>{t('due_nav')}</h1>
		<div class="sub">{prof?.display_name ?? ''} · {t('due_draw_sub')}</div>
	</div>
	<div class="right"><PhaseChip phase="v0.3" /></div>
</div>
<Tabs />

{#if !prof}
	<div class="card"><div class="empty">{t('h1_noprofile')} <a href="/family">{t('h1_noprofile_link')}</a></div></div>
{:else if plan.error}
	<div class="card"><NotReady error={plan.error} onretry={load} /></div>
{:else if plan.loading && !plan.data}
	<div class="card"><div class="empty">{t('loading')}</div></div>
{:else if plan.data}
	{@const d = plan.data}
	{#if !d.count}
		<div class="card" style="margin-bottom:12px"><div class="empty">{d.labels.empty}</div></div>
	{/if}
	{#each d.sections as sec, k (sec.prep.join('+') || 'none')}
		<section class="card" style="margin-bottom:12px" aria-labelledby="nd-{k}">
			<h3 id="nd-{k}">{sec.title}<span class="r num">{tp('due_draw_count', sec.items.length)}</span></h3>
			{#each sec.notes as n (n.id)}
				<p class="prep">
					{#if sec.notes.length > 1}<b>{n.name}:</b>{/if}
					{n.text}
					<a class="chip" href={n.cite} target="_blank" rel="noopener" title={t('due_cite_t')}>{n.source}</a>
				</p>
			{/each}
			{#each sec.items as it (it.group)}
				<div class="due ndrow">
					<div class="w">
						<div><b>{it.name}</b>{#if it.loinc}<span class="mono dim loinc">LOINC {it.loinc}</span>{/if}</div>
						{#each it.reasons as r (r.kind + r.rule)}
							<div class="reason">
								<span>{r.text}</span>
								<span class="meta">
									{#each r.src as s (s)}<SourceChip label={s} name={it.name} src={s} onopen={openSource} />{/each}
									{#if r.cite}<a class="chip" href={r.cite} target="_blank" rel="noopener" title={t('due_cite_t')}>{r.rule}</a>{:else}<span class="mono">{r.rule}</span>{/if}
								</span>
							</div>
						{/each}
					</div>
					<a class="bio" href="/biomarkers?code={encodeURIComponent(it.group)}">{t('due_lab_link')}</a>
				</div>
			{/each}
		</section>
	{/each}

	<div class="ndgrid">
		<section class="card" aria-labelledby="nd-opts">
			<h3 id="nd-opts">{t('due_draw_opts')}</h3>
			<fieldset>
				<legend>{t('due_draw_lang')}</legend>
				<label class="opt"><input type="radio" name="nd-lang" value="es" bind:group={lang} />Español</label>
				<label class="opt"><input type="radio" name="nd-lang" value="en" bind:group={lang} />English</label>
			</fieldset>
			<fieldset>
				<label class="opt"><input type="checkbox" bind:checked={hide} aria-describedby="nd-hide-d" />{t('due_draw_hide')}</label>
				<div class="dim hint" id="nd-hide-d">{t('due_draw_hide_d')}</div>
			</fieldset>
			<div class="acts">
				<button class="btn pri" onclick={download} disabled={!pdf || busy}>{t('sum_download')}</button>
				<a class="btn" href={pdf && !busy ? pdf.url : undefined} target="_blank" rel="noopener" aria-disabled={!pdf || busy} tabindex={pdf && !busy ? undefined : -1}>{t('sum_open')}</a>
			</div>
		</section>
		<section class="card sheet" aria-label={t('due_draw_pdf')} aria-busy={busy}>
			{#if pdfErr}
				<NotReady error={pdfErr} onretry={retryPdf} />
			{:else if pdf}
				<iframe src={pdf.url} title={t('due_draw_pdf')}></iframe>
				<!-- phone browsers often show no PDF inside a page: there the sheet opens or downloads -->
				<div class="empty ready">{t('sum_ready')}</div>
			{:else}
				<div class="empty">{t('sum_making')}</div>
			{/if}
		</section>
	</div>

	<div class="note" style="margin-top:12px">{t('due_draw_info')}</div>
	<div class="foot-hash">
		{#if d.state_hash}<span>state_hash <span class="mono" title={d.state_hash}>{shortHash(d.state_hash)}</span></span>{/if}
		<span>{t('due_rules')} <span class="mono">{d.rules.join(' · ')}</span></span>
		<span>{t('due_asof')} <span class="num">{d.as_of}</span></span>
	</div>
{/if}

<SourcePanel />

<style>
	.prep {
		margin: 0 0 8px;
		font-size: 12px;
		color: var(--muted);
		line-height: 1.5;
	}
	.prep a.chip,
	.meta a.chip {
		text-decoration: none;
		margin-left: 4px;
	}
	.ndrow {
		align-items: flex-start;
		padding: 9px 0;
	}
	.ndrow .w > div:first-child {
		font-size: 13px;
		color: var(--text);
	}
	.loinc {
		margin-left: 8px;
		font-size: 11px;
	}
	.reason {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 10px;
		align-items: center;
		margin-top: 3px;
		font-size: 12px;
		color: var(--muted);
	}
	.due .w div.reason:last-child {
		font-size: 12px;
		color: var(--muted);
	}
	.meta {
		display: inline-flex;
		gap: 8px;
		flex-wrap: wrap;
		align-items: center;
		font-size: 11px;
		color: var(--dim);
	}
	.bio {
		font-size: 11.5px;
		white-space: nowrap;
	}
	.ndgrid {
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
	.sheet iframe {
		display: block;
		width: 100%;
		height: 640px;
		border: 0;
		border-radius: 6px;
		background: #f4f2ee;
	}
	.ready {
		display: none;
	}
	@media (max-width: 860px) {
		.ndgrid {
			grid-template-columns: 1fr;
		}
		.sheet iframe {
			display: none;
		}
		.ready {
			display: block;
		}
	}
</style>
