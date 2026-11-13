<script lang="ts">
	// Mockup screen #genome: findings imported from OwnGenome, with evidence levels, call_state, and
	// sensitive findings behind the profile's consent switch. OwnHealth runs no genome engine.
	import '#lib/components/pages2/pages2.css';
	import { dismissReview, getGenomeView, listReview, lookupVariant, setGenomeConsent } from '#lib/api';
	import type { GenomeFindingFull, GenomeView, PgxGridRow, PgxStale, ReviewItem } from '#lib/api/types';
	import PhaseChip from '#lib/components/PhaseChip.svelte';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import SourcePanel from '#lib/components/pages1/SourcePanel.svelte';
	import { openSource } from '#lib/components/pages1/source.svelte';
	import { errText, Loader, tf, tp } from '#lib/components/pages1/util.svelte';
	import ConfirmRow from '#lib/components/ConfirmRow.svelte';
	import CoupleCarrierCard from '#lib/components/CoupleCarrierCard.svelte';
	import { ApiError } from '#lib/api/core';
	import { focusOnOpen } from '#lib/actions/focusOnOpen';
	import StepUpDialog from '#lib/components/pages2/StepUpDialog.svelte';
	import { day, plain } from '#lib/components/pages2/fmt';
	import { withStepUp } from '#lib/components/pages2/stepup.svelte';
	import { i18n, t, type Key } from '#lib/i18n/index.svelte';
	import { reinterpText } from '#lib/logic/inbox';
	import { currentProfile } from '#lib/state/app.svelte';

	const OWNGENOME = 'https://github.com/GeiserX/OwnGenome';

	function tileDetail(tile: GenomeView['tiles'][number]): string {
		if (tile.genes === undefined) return tile.detail;
		const parts = [tile.actionable ? tp('i2_actionable', tile.actionable) : '', tile.genes, tile.hidden ? tp('h1_hidden', tile.hidden) : ''];
		return parts.filter(Boolean).join(' · ');
	}
	const prof = $derived(currentProfile());
	const gen = new Loader<GenomeView>();
	const alerts = new Loader<ReviewItem[]>();
	let actionError = $state<string | null>(null);
	let lookupOpen = $state(false);
	let rsid = $state('');
	// The answer of GET /genome/variant, as far as this page shows it; the whole answer stays under 'Raw'.
	type Call = { source?: string; assay?: string; call_state?: string | null; genotype?: string | null; zygosity?: string | null; dp?: number | null; gq?: number | null };
	type Lookup = {
		variant?: { chrom?: string; pos?: number; ref?: string; alt?: string; rsid?: string; gene?: string | null };
		calls?: Call[];
		cited_by_findings?: string[];
		limitations?: string[];
		limitation_codes?: string[];
	};
	const lookup = new Loader<Lookup>();
	// A lookup's limitation in the reader's language by its code; the server's sentence when it has none.
	const limText = (L: Lookup, i: number, text: string) => {
		const k = 'g_lim_' + (L.limitation_codes?.[i] || '-');
		return t(k) === k ? text : t(k);
	};
	// The fix of a refused lookup in the reader's language when we have one for its code.
	const fixText = (e: ApiError) => {
		const k = 'err_' + (e.code ?? '') + '_fix';
		return t(k) === k ? e.fix : t(k);
	};
	// Revealing sensitive findings widens what is on screen: it asks once, inline.
	let revealing = $state(false);

	function load() {
		const slug = prof?.slug;
		if (!slug) return;
		gen.run(() => getGenomeView(slug, i18n.lang), slug);
		alerts.run(async () => (await listReview({ status: 'open', profile: slug, kind: 'genome_reinterpretation' })).items, slug);
	}
	$effect(() => {
		void prof?.slug;
		void i18n.lang;
		load();
	});

	async function consent(ui: boolean) {
		const slug = prof?.slug;
		if (!slug) return;
		actionError = null;
		try {
			await withStepUp(() => setGenomeConsent(slug, { ui }));
			revealing = false;
			load();
		} catch (e) {
			actionError = errText(e);
		}
	}

	async function markRead(id: string) {
		actionError = null;
		try {
			await dismissReview(id, 'read');
			load();
		} catch (e) {
			actionError = errText(e);
		}
	}

	const g = $derived(gen.data);
	const ds = $derived(g?.dataset ?? null);
	const v = (f: GenomeFindingFull, k: string) => f.value?.[k] as string | number | boolean | string[] | undefined;
	function variantCell(f: GenomeFindingFull): string {
		if (f.category === 'prs') return String(v(f, 'pgs_id') ?? '');
		if (f.category === 'hla') return String(v(f, 'marker') ?? '');
		const ids = f.variants?.length ? f.variants.map((x) => x.rsid).filter(Boolean) : [f.rsid];
		return [ids.join(', '), f.hgvs].filter(Boolean).join(' · ');
	}
	function resultCell(f: GenomeFindingFull): string {
		if (f.diplotype || f.phenotype) return [f.diplotype, f.phenotype].filter(Boolean).join(' · ');
		if (f.category === 'prs' && f.prs?.status === 'not_interpretable') return t('gen_prs_ni');
		if (f.category === 'prs') return tf('i2_prs_pct', { p: String(v(f, 'percentile') ?? '—'), pop: String(v(f, 'most_similar_pop') ?? '') });
		if (f.category === 'hla') return `${(v(f, 'alleles') as string[] | undefined)?.join(' · ') ?? ''} · ${v(f, 'marker')} ${v(f, 'present') ? t('i2_present') : t('i2_absent')}`;
		if (f.category === 'trait') return `${v(f, 'trait') ?? ''} ${v(f, 'call') ?? ''}`.trim();
		return [f.zygosity ? t('i2_zyg_' + f.zygosity) : null, f.condition?.name].filter(Boolean).join(' · ');
	}
	function noteCell(f: GenomeFindingFull): string {
		const parts = [f.evidence_native, f.clinvar ? `VariationID ${f.clinvar.variation_id}` : null, f.drugs?.length ? f.drugs.join(', ') : null];
		return parts.filter(Boolean).join(' · ');
	}
	const conf = (c: string) => (c === 'high' ? 'p-ok' : c === 'medium' ? 'p-watch' : c === 'low' ? 'p-stale' : 'p-out');
	const sub = $derived.by(() => {
		if (!ds || !prof) return prof?.display_name ?? '';
		return [prof.display_name, [ds.assay.toUpperCase(), ds.mean_depth ? `${Math.round(ds.mean_depth)}×` : null].filter(Boolean).join(' '), ds.build].join(' · ');
	});
	const subTail = $derived(ds ? [...Object.entries(ds.db_versions).filter(([k]) => k === 'clinvar' || k === 'cpic').map(([k, val]) => `${k === 'clinvar' ? 'ClinVar' : 'CPIC'} ${val}`), `${t('i2_ingested')} ${day(ds.imported_at)}`].join(' · ') : '');
	const sensitiveShown = $derived((g?.findings ?? []).filter((f) => f.sensitive));
	// The server's limitation codes in the reader's language; an unknown code keeps the server's text.
	const LIM: Record<string, Key> = {
		no_genome: 'i2_lim_no_genome',
		no_vcf: 'i2_lim_no_vcf',
		variant_only_vcf: 'i2_lim_variant_only',
		chip_unconfirmed: 'gen_lim_chip',
		no_cram: 'i2_lim_no_cram'
	};
	const limits = $derived(
		(g?.limitations ?? []).map((text, i) => {
			const c = g?.limitation_codes?.[i];
			const key = c && LIM[c.code];
			return key ? tf(key, { kind: (c.assay ?? '').toUpperCase() }) : text;
		})
	);
	// A dataset whose findings were all retracted (or never had any) reads as no genome data at all.
	const empty = $derived(!!g && (!ds || (!g.findings.length && !g.locked.length && !g.withheld.minor_rule)));
	const denied = $derived(gen.error instanceof ApiError && gen.error.status === 403);
	const canRaw = $derived(prof?.scopes.includes('read:genome_raw') ?? false);
	const catLabel = (c: string) => (t('i2_cat_' + c) === 'i2_cat_' + c ? c : t('i2_cat_' + c));
	const findingById = (id: string) => g?.findings.find((f) => f.id === id) ?? null;
	// The PGx gene grid: one box per CPIC gene, with its call or why there is none.
	const list = (xs: (string | number)[]) => new Intl.ListFormat(i18n.lang, { type: 'conjunction' }).format(xs.map(String));
	const grid = $derived(g?.pgx_grid ?? []);
	const staleNote = $derived.by(() => {
		const rows = grid.filter((r) => r.stale);
		if (!rows.length) return '';
		const all = rows.map((r) => r.stale as PgxStale);
		return tf('gen_stale_note', {
			genes: list(rows.map((r) => r.gene)),
			by: list([...new Set(all.flatMap((s) => s.by))]),
			since: day(all.map((s) => s.since).sort()[0]),
			steps: list(all[0].rerun_steps)
		});
	});
	const staleTitle = (s: PgxStale) => tf('gen_stale_title', { by: list(s.by), steps: list(s.rerun_steps) });
	function gridCall(r: PgxGridRow): string {
		if (r.status === 'withheld') return t('gen_grid_withheld');
		if (r.status === 'no_finding') return t('gen_grid_no_finding');
		if (r.status === 'not_assessed') {
			const why = r.coverage?.reason;
			return why === 'not_in_coverage_map' ? t('gen_grid_not_in_map') : [t('gen_grid_not_assessed'), why?.replace(/_/g, ' ')].filter(Boolean).join(' · ');
		}
		if (r.markers.length && !r.phenotype) return r.markers.map((m) => `${m.marker.replace(/^HLA-/, '')} ${m.present ? t('i2_present') : t('i2_absent')}`).join(' · ');
		return [r.diplotype, r.phenotype].filter(Boolean).join(' · ') || '—';
	}
	const coverage = $derived.by(() => {
		const c = g?.coverage ?? [];
		return { assessed: c.filter((x) => x.state === 'assessed').length, partial: c.filter((x) => x.state === 'partial').map((x) => x.target), total: c.length };
	});
</script>

<div class="ph">
	<div>
		<h1>{t('nav_gen')}</h1>
		<div class="sub">
			{#if ds}
				{sub} · <span class="mono">genome-facts.json</span> {t('i2_from')} <a href={OWNGENOME} target="_blank" rel="noopener">OwnGenome</a> {ds.source_tool.replace(/^OwnGenome\s*/, 'v')} · {subTail}
			{:else}
				{prof?.display_name ?? ''}
			{/if}
		</div>
	</div>
	<div class="right">
		<PhaseChip phase="v0.1" /><PhaseChip phase={t('i2_gen_ph')} />
		{#if !canRaw}<span class="dim" style="font-size:11.5px">{t('i2_lookup_noraw')}</span>{/if}
		<button class="btn" onclick={() => (lookupOpen = !lookupOpen)} aria-expanded={lookupOpen} disabled={!canRaw}>{t('i2_lookup')}</button>
	</div>
</div>

{#if lookupOpen && prof}
	<form class="card p2row" style="margin-bottom:12px" onsubmit={(e) => { e.preventDefault(); lookup.run(() => lookupVariant(prof.slug, rsid.trim()) as Promise<Lookup>); }} use:focusOnOpen={{ oncancel: () => (lookupOpen = false) }}>
		<label for="rsid" class="dim">rsID</label>
		<input id="rsid" class="p2in mono" bind:value={rsid} placeholder="rs4244285" required pattern="rs\d+" />
		<button class="btn sm pri" type="submit" disabled={lookup.loading}>{t('i2_lookup_go')}</button>
		<span class="dim" style="font-size:11.5px">{t('i2_lookup_d')}</span>
		{#if lookup.error}
			<div class="p2err" role="alert" style="flex-basis:100%">
				{errText(lookup.error)}{#if lookup.error instanceof ApiError && lookup.error.fix}<div class="dim">{fixText(lookup.error)}</div>{/if}
			</div>
		{/if}
		{#if lookup.data}
			{@const L = lookup.data}
			<dl class="lk">
				{#if L.variant}
					<dt>{t('i2_g_gene')}</dt><dd><b>{L.variant.gene ?? '—'}</b> <span class="mono muted">{L.variant.rsid ?? ''}</span></dd>
					<dt>{t('i2_lk_pos')}</dt><dd class="mono">{L.variant.chrom}:{L.variant.pos} {L.variant.ref}&gt;{L.variant.alt}</dd>
				{/if}
				{#each L.calls ?? [] as c, i (i)}
					<dt>{t('i2_lk_call')}{(L.calls?.length ?? 0) > 1 ? ' ' + (i + 1) : ''}</dt>
					<dd>
						<b class="mono">{c.genotype ?? '—'}</b>{#if c.zygosity}{' · '}{t('i2_zyg_' + c.zygosity)}{/if}{' · '}<span class="mono">{c.call_state ?? 'n/a'}</span>{#if c.dp != null || c.gq != null}{' · '}<span class="num">{tf('i2_lk_depth', { dp: c.dp ?? '—', gq: c.gq ?? '—' })}</span>{/if}{#if c.assay}{' · '}<span class="dim">{c.assay.toUpperCase()}</span>{/if}
					</dd>
				{:else}
					<dt>{t('i2_lk_call')}</dt><dd class="dim">{t('i2_lk_nocall')}</dd>
				{/each}
				{#if L.cited_by_findings?.length}
					<dt>{t('i2_lk_cited')}</dt>
					<dd>
						{#each L.cited_by_findings as id, i (id)}{@const f = findingById(id)}{i ? ', ' : ''}<a href="#f-{id}">{f ? [f.gene, resultCell(f)].filter(Boolean).join(' · ') : id}</a>{/each}
					</dd>
				{/if}
				{#each L.limitations ?? [] as l, i (l)}<dt></dt><dd class="dim">{limText(L, i, l)}</dd>{/each}
			</dl>
			<details style="flex-basis:100%"><summary class="dim">{t('i2_lk_raw')}</summary><pre class="code mono" style="max-height:240px;overflow:auto">{JSON.stringify(L, null, 2)}</pre></details>
		{/if}
	</form>
{/if}

{#each alerts.data ?? [] as a (a.id)}
	<div class="banner" role="status">
		<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="#e0a84a" stroke-width="2" aria-hidden="true"><path d="M12 3 2 20h20zM12 10v4M12 17v.5" /></svg>
		<div><b>{t('i2_reinterp')}</b><div class="muted" style="font-size:12px">{reinterpText(a.payload, i18n.lang)}</div></div>
		<button class="btn sm" style="margin-left:auto" onclick={() => markRead(a.id)}>{t('i2_mark_read')}</button>
	</div>
{/each}

{#if actionError}<div class="p2err" role="alert" style="margin-bottom:8px">{actionError}</div>{/if}

{#if gen.error}
	<div class="card"><NotReady error={gen.error} onretry={load} /></div>
{:else if gen.loading && !g}
	<div class="card"><div class="empty">{t('loading')}</div></div>
{:else if g && empty}
	<div class="card promo">
		<h3>{t('i2_gen_empty_t')}</h3>
		<p class="muted" style="margin:0 0 10px">{t('i2_gen_empty_d')} {t('i2_gen_from_none')}</p>
		<div class="p2row">
			<a class="btn pri" href={OWNGENOME} target="_blank" rel="noopener">{t('i2_gen_get')}</a>
			<a class="btn" href="/import-review">{t('i2_gen_import')}</a>
			<span class="dim mono" style="font-size:11.5px">{OWNGENOME}</span>
		</div>
	</div>
{:else if g}
	<div class="tiles">
		{#each g.tiles as tile (tile.key)}
			<div class="tile">
				<div class="k">{t('i2_cat_' + tile.key)}</div>
				<div class="n num">{tile.value}</div>
				<div class="d">{tileDetail(tile)}</div>
			</div>
		{/each}
	</div>
	{#if grid.length}
		<section class="card pgxgrid" aria-labelledby="pgx-grid-h">
			<div class="gh"><h2 id="pgx-grid-h">{t('gen_grid_t')}</h2><span class="dim">{t('gen_grid_d')}</span></div>
			{#if staleNote}<div class="stalenote" role="note"><span class="pill p-stale"><i aria-hidden="true"></i>{t('gen_stale')}</span><span class="muted">{staleNote}</span></div>{/if}
			<ul class="ggrid">
				{#each grid as r (r.gene)}
					<li class="gcell" class:act={r.actionable} class:none={r.status !== 'found'}>
						<div class="gname">
							{#if r.fact_ids.length}<a href="#f-{r.fact_ids[0]}"><b>{r.gene}</b></a>{:else}<b>{r.gene}</b>{/if}
							{#if r.stale}<span class="pill p-stale" title={staleTitle(r.stale)}><i aria-hidden="true"></i>{t('gen_stale')}</span>{/if}
						</div>
						<div class="gcall">{gridCall(r)}{#if r.actionable}<span class="sr-only"> · {t('gen_grid_act')}</span>{/if}</div>
						{#if r.meds?.length}<div class="gmeds">{tf('gen_grid_on', { meds: r.meds.map((m) => m.name).join(', ') })}</div>{/if}
					</li>
				{/each}
			</ul>
			{#if grid.some((r) => r.meds === null)}<div class="dim" style="font-size:11.5px;margin-top:6px">{t('gen_grid_meds_hidden')}</div>{/if}
		</section>
	{/if}
	<div class="card" style="padding:4px 6px">
		<div class="tscroll">
		<table class="t gtab">
			<thead>
				<tr><th>{t('i2_g_cat')}</th><th>{t('i2_g_gene')}</th><th>{t('i2_g_var')}</th><th>{t('i2_g_res')}</th><th>{t('i2_g_ev')}</th><th>{t('i2_g_conf')}</th><th>{t('i2_g_cs')}</th><th>{t('i2_g_note')}</th></tr>
			</thead>
			<tbody>
				{#each g.findings as f (f.id)}
					<tr id="f-{f.id}" class:sens={f.sensitive}>
						<td><span class="chip" title={f.category}>{catLabel(f.category)}</span></td>
						<td><b>{f.gene ?? '—'}</b></td>
						<td class="mono muted">{variantCell(f)}</td>
						<td>
							<button class="lnk" onclick={() => openSource(f.src)} title={t('open_src')}>{resultCell(f)}</button>
							{#if f.stale}<span class="pill p-stale" style="margin-left:6px" title={staleTitle(f.stale)}><i aria-hidden="true"></i>{t('gen_stale')}</span>{/if}
							{#if f.prs?.reason_code}<div class="dim" style="font-size:11px">{t('gen_prs_' + f.prs.reason_code)}</div>{/if}
							{#if f.confirm_clinically}<div class="dim" style="font-size:11px">{t('i2_confirm_clin')}</div>{/if}
							{#if f.chip_note}<div class="dim" style="font-size:11px">{tf('gen_chip_note', { pct: f.chip_note.ppv_pct })} <a href={f.chip_note.cite} target="_blank" rel="noopener">PubMed</a></div>{/if}
						</td>
						<td><span class="pgx ev-{f.evidence}" title={t('i2_ev_' + f.evidence)}>{f.evidence}</span></td>
						<td>{#if f.confidence}<span class="pill {conf(f.confidence)}"><i aria-hidden="true"></i>{t('i2_conf_' + f.confidence)}</span>{:else}<span class="dim">—</span>{/if}</td>
						<td class="mono" style="color:{f.call_state === 'alt' ? 'var(--text)' : 'var(--muted)'}">{f.call_state ?? 'n/a'}</td>
						<td class="muted" style="font-size:12px">
							{noteCell(f) || '—'}
							{#if f.sensitive}
								<div class="p2row" style="margin-top:4px">
									<span class="pill p-watch"><i aria-hidden="true"></i>{t('i2_sens_chip')}</span>
									<span class="dim" style="font-size:11px">{t('i2_sens_by_owner')}</span>
									<button class="btn sm" onclick={() => consent(false)}>{t('i2_hide_btn')}</button>
								</div>
							{/if}
						</td>
					</tr>
				{/each}
			</tbody>
		</table>
		</div>
		{#if g.locked.length || g.withheld.minor_rule}
			<!-- Outside the scrolling table, so the reveal button and its question stay on screen at any width. -->
			<div class="locks">
				{#each g.locked as l, i (i)}
					<div class="lockline"><span class="chip" title={l.category}>{catLabel(l.category)}</span><span class="muted"><span aria-hidden="true">{'🔒 '}</span>{tf('i2_locked', { gene: l.gene ?? '—' })}</span></div>
				{/each}
				{#if g.locked.length}
					<div class="p2row">
						{#if g.consent.can_change}<button class="btn sm" onclick={() => (revealing = true)} aria-expanded={revealing}>{t('i2_reveal')}</button>{:else}<span class="dim" style="font-size:11px">{t('i2_owner_only')}</span>{/if}
					</div>
					{#if revealing}
						<ConfirmRow text={tf('i2_reveal_confirm', { name: prof?.display_name ?? '' })} confirmLabel={t('i2_reveal_yes')} onconfirm={() => consent(true)} oncancel={() => (revealing = false)} />
					{/if}
				{/if}
				{#if g.withheld.minor_rule}
					<div class="lockline"><span class="muted"><span aria-hidden="true">{'🔒 '}</span>{tf('i2_minor_rule', { n: g.withheld.minor_rule })}</span></div>
				{/if}
			</div>
		{/if}
	</div>
	{#if sensitiveShown.length}<div class="sr-only" role="status">{t('i2_sens_shown')}</div>{/if}
	<!-- Slot (slice 13): the ClinVar history card mounts here, one line. -->
{/if}

{#if !denied}
	<div class="note" style="margin-top:10px">
		<span>{t('i2_gen_from1')} <a href={OWNGENOME} target="_blank" rel="noopener">OwnGenome</a> {t('i2_gen_from2')}</span>
	</div>
{/if}
{#if g && ds && !empty}
	<div style="display:flex;gap:18px;margin-top:6px" class="note cov">
		<span>
			<b style="color:var(--text)">{t('i2_coverage')}</b>{' · '}{tf('i2_cov_line', { a: coverage.assessed, n: coverage.total })}{#if coverage.partial.length}{' · '}{t('i2_cov_partial')}: {coverage.partial.join(', ')}{/if}
			{' · '}call_state: ref / alt / no_call / not_on_array / not_covered; n/a {t('i2_cs_na')}
			{#if g.not_assessed.length}{' · '}{t('i2_not_assessed')}: {g.not_assessed.map((x) => x.gene ?? (t('i2_cat_' + x.panel) === 'i2_cat_' + x.panel ? x.panel.replace(/_/g, ' ') : t('i2_cat_' + x.panel))).join(', ')}{/if}
			{#each limits as l (l)}{' · '}{l}{/each}
		</span>
		<span style="margin-left:auto">{t('i2_ev_legend')}</span>
	</div>
	<div class="note" style="margin-top:6px">{g.notice_code ? t('i2_gen_notice') : plain(g.notice)}</div>
{/if}

{#if g && prof}<CoupleCarrierCard profile={prof} rev={gen.data} />{/if}
<SourcePanel />
<StepUpDialog />

<style>
	/* On a phone the result and its confidence must show: the category chip, the variant ids and the raw
	   call state go (the result says the same in words). Desktop and tablet keep every column. */
	@media (max-width: 760px) {
		.gtab th:nth-child(1),
		.gtab td:nth-child(1),
		.gtab th:nth-child(3),
		.gtab td:nth-child(3),
		.gtab th:nth-child(7),
		.gtab td:nth-child(7) {
			display: none;
		}
	}
	.locks {
		display: flex;
		flex-direction: column;
		gap: 8px;
		padding: 10px 8px 8px;
		border-top: 1px solid var(--line);
		font-size: 12.5px;
	}
	.lockline {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 6px 8px;
	}
	.lnk {
		background: none;
		border: 0;
		padding: 0;
		color: inherit;
		text-align: left;
		font: inherit;
	}
	button.lnk:hover {
		color: var(--accent);
	}
	tr.sens td {
		background: rgba(224, 168, 74, 0.06);
	}
	tr.sens td:first-child {
		box-shadow: inset 3px 0 0 var(--watch);
	}
	.lk {
		flex-basis: 100%;
		display: grid;
		grid-template-columns: max-content minmax(0, 1fr);
		gap: 4px 12px;
		margin: 4px 0 0;
		font-size: 12.5px;
	}
	.lk dt {
		color: var(--dim);
	}
	.lk dd {
		margin: 0;
	}
	.pgxgrid {
		margin-bottom: 12px;
	}
	.gh {
		display: flex;
		flex-wrap: wrap;
		align-items: baseline;
		gap: 4px 10px;
		margin-bottom: 8px;
	}
	.gh h2 {
		font-size: 14px;
		margin: 0;
	}
	.gh .dim {
		font-size: 11.5px;
	}
	.stalenote {
		display: flex;
		align-items: baseline;
		gap: 8px;
		font-size: 12px;
		margin-bottom: 8px;
	}
	.stalenote .pill {
		flex: none;
	}
	.ggrid {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(150px, 1fr));
		gap: 8px;
	}
	.gcell {
		border: 1px solid var(--line);
		border-radius: 8px;
		padding: 8px 10px;
		font-size: 12.5px;
		min-width: 0;
	}
	.gcell.act {
		border-color: rgba(224, 168, 74, 0.55);
		box-shadow: inset 3px 0 0 var(--watch);
	}
	.gcell.none .gcall {
		color: var(--dim);
	}
	.gname {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 4px 6px;
	}
	.gname a {
		color: inherit;
	}
	.gname .pill {
		font-size: 10.5px;
	}
	.gcall {
		margin-top: 3px;
		overflow-wrap: anywhere;
	}
	.gmeds {
		margin-top: 3px;
		font-size: 11.5px;
		color: var(--watch);
	}
	.promo {
		border-color: rgba(110, 168, 254, 0.35);
	}
	.cov > span:first-child {
		flex: 1;
	}
	.cov > span:last-child {
		flex: none;
		max-width: 40%;
	}
</style>
