<script lang="ts">
	// The check-a-drug box of the Medications screen (mockup #drug-check): gene × drug from OwnGenome
	// findings, phenoconversion by current medicines, relevant labs and allergies, with CPIC guidance
	// quoted verbatim, and DPWG text from the genome file beside it. Quotes, never advice. The drug
	// comes from ?drug= so a check is a link.
	import '#lib/components/pages2/pages2.css';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { getDrugCheckView, listRecords } from '#lib/api';
	import { ApiError } from '#lib/api/core';
	import type { DrugCheckView, RecordOut } from '#lib/api/types';
	import SourceChip from '#lib/components/SourceChip.svelte';
	import StatusChip from '#lib/components/StatusChip.svelte';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { openSource } from '#lib/components/pages1/source.svelte';
	import { Loader, tf, tp, unitLabel } from '#lib/components/pages1/util.svelte';
	import { fmtNum, i18n, t } from '#lib/i18n/index.svelte';
	import { isActionable, withheldBy } from '#lib/logic/health';
	import { currentProfile } from '#lib/state/app.svelte';

	const prof = $derived(currentProfile());
	const drug = $derived(page.url.searchParams.get('drug') ?? '');
	let query = $state('');
	$effect(() => {
		query = drug;
	});
	const check = new Loader<DrugCheckView>();
	const allergies = new Loader<RecordOut[]>();

	$effect(() => {
		const slug = prof?.slug;
		const d = drug.trim();
		void i18n.lang;
		if (!slug || !d) return;
		check.run(() => getDrugCheckView(slug, d, i18n.lang), `${slug}/${d}`);
	});
	$effect(() => {
		const slug = prof?.slug;
		if (!slug) return;
		allergies.run(async () => (await listRecords(slug, 'allergy')).items, slug);
	});

	function submit(e: SubmitEvent) {
		e.preventDefault();
		const q = query.trim();
		if (q) goto(`?drug=${encodeURIComponent(q)}`);
	}
	const c = $derived(drug ? check.data : null);
	const unknown = $derived(check.error instanceof ApiError && check.error.code === 'drug_unknown');
	const relTone = (r: string): 'watch' | 'info' | 'stale' => (r.startsWith('inhibitor') || r.startsWith('inducer') ? 'watch' : r === 'same_drug' ? 'info' : 'stale');
	const EXAMPLES = ['clopidogrel', 'codeine', 'warfarin', 'simvastatin'];
	// The answer names a lab value by its date; the chip opens the source, which shows the lab and page.
	const labSrc = (l: DrugCheckView['relevant_labs'][number]) => l.date ?? '—';
	// The API names a withheld scope with or without 'read:'.
	const withheld = (s: string) => withheldBy(c?.withheld.by_scope, s);
	// A dict word for a code, with the code itself when the dict has none.
	const word = (prefix: string, code: string) => {
		const k = prefix + code.toLowerCase().replace(/\W+/g, '_');
		return t(k) === k ? code : t(k);
	};
	// CPIC quotes are plain text; DPWG quotes come from the genome file wrapped as untrusted text.
	const quoteText = (q: DrugCheckView['guidance'][number]['quote']) => (q == null ? '' : typeof q === 'string' ? q : q.text);
	const labValue = (v: string) => (/^-?\d+(\.\d+)?$/.test(v) ? fmtNum(Number(v)) : v);
	const allergyItems = $derived((allergies.data ?? []).filter((r) => r.status === 'current' || r.status === 'provisional'));
</script>

<h2 class="medh" id="check">{t('med_check')}</h2>
<form class="search big" role="search" onsubmit={submit}>
	<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="#9aa3b2" stroke-width="2" aria-hidden="true"><circle cx="11" cy="11" r="7" /><path d="m20 20-3.5-3.5" /></svg>
	<input bind:value={query} placeholder={t('i2_drug_ph_in')} aria-label={t('i2_drug_ph_in')} autocomplete="off" />
	{#if c}
		{#if c.drug.cpic_drug_id}<span class="chip mono">{c.drug.cpic_drug_id}</span>{/if}
		{#each c.drug.genes as gname (gname)}<span class="chip">{gname}</span>{/each}
	{/if}
</form>

{#if !drug}
	<div class="card">
		<div class="empty">
			{t('i2_drug_empty')}
			<div class="p2row" style="justify-content:center;margin-top:8px">
				{#each EXAMPLES as ex (ex)}<a class="chip" href="?drug={ex}">{ex}</a>{/each}
			</div>
		</div>
	</div>
{:else if unknown}
	<div class="card"><div class="empty">{tf('i2_drug_unknown', { drug })}</div></div>
{:else if check.error}
	<div class="card"><NotReady error={check.error} /></div>
{:else if check.loading && !c}
	<div class="card"><div class="empty">{t('loading')}</div></div>
{:else if c}
	<div class="drug">
		<div class="grid" style="align-content:start">
			{#each c.pgx as x (x.gene)}
				{@const recs = c.guidance.filter((gd) => gd.source !== 'DPWG' && gd.applies_to.startsWith(x.gene))}
				{@const dpwg = c.guidance.filter((gd) => gd.source === 'DPWG' && gd.applies_to.startsWith(x.gene) && quoteText(gd.quote))}
				{@const missing = x.evidence == null && x.diplotype == null}
				<!-- Amber only when the phenotype changes what CPIC says: standard dosing is not a warning. -->
				{@const act = recs.length > 0 && isActionable(x.phenotype)}
				<section class="card" style={act ? 'border-color:rgba(224,168,74,.45)' : ''}>
					<h3>
						{t('i2_pgx')}
						{#if missing}<span class="pill p-stale"><i aria-hidden="true"></i>{t('i2_not_assessed')}</span>
						{:else if recs.length}<span class="pill {act ? 'p-watch' : 'p-info'}"><i aria-hidden="true"></i>{tp('i2_recs', recs.length)}</span>
						{:else}<span class="pill p-stale"><i aria-hidden="true"></i>{t('i2_no_recs')}</span>{/if}
						{#if x.cpic_level}<span class="r">{t('i2_cpic_level')} {x.cpic_level}</span>{/if}
					</h3>
					{#if missing}
						<div class="muted">{tf('i2_gene_missing', { gene: x.gene })}</div>
					{:else}
					<div style="display:flex;gap:22px;align-items:center;margin:4px 0 8px;flex-wrap:wrap">
						<div><div class="dim" style="font-size:11px">{t('i2_g_gene')}</div><div style="font-size:18px;font-weight:650">{x.gene}</div></div>
						<div><div class="dim" style="font-size:11px">{t('i2_diplotype')}</div><div style="font-size:18px;font-weight:650" class="mono">{x.diplotype ?? '—'}</div></div>
						<div><div class="dim" style="font-size:11px">{t('i2_phenotype')}</div><div style="font-size:15px;font-weight:600;color:{act ? 'var(--watch)' : 'var(--text)'}">{x.phenotype ?? '—'}</div></div>
						<div style="margin-left:auto;text-align:right">
							{#if x.evidence}<span class="pgx ev-{x.evidence}">{x.evidence}</span>{/if}
							{#if x.confidence}<span class="pill {x.confidence === 'high' ? 'p-ok' : x.confidence === 'medium' ? 'p-watch' : 'p-stale'}"><i aria-hidden="true"></i>{t('i2_conf_' + x.confidence)}</span>{/if}
							{#if x.src}<div style="margin-top:4px"><!-- The call state is a genome-page code; the chip names the source and its panel says the rest. -->
								<SourceChip label="OwnGenome" src={x.src} onopen={openSource} /></div>{/if}
						</div>
					</div>
					{/if}
					{#each recs as gd (quoteText(gd.quote))}
						<div class="dim" style="font-size:11px;text-transform:uppercase;letter-spacing:.05em">{gd.source} · {gd.applies_to}{gd.population ? ' · ' + word('i2_pop_', gd.population) : ''}</div>
						<blockquote>“{quoteText(gd.quote)}” {#if gd.strength}<span class="dim" style="white-space:nowrap">{t('i2_classification')}: {word('i2_str_', gd.strength)}</span>{/if}</blockquote>
					{/each}
					{#if recs.length}
						<div class="dim" style="font-size:11px">{[...new Set(recs.map((r) => r.url))].join(' · ')}</div>
						{#if recs.length > 1}<div class="note" style="margin-top:8px">{t('i2_indication')}</div>{/if}
					{/if}
					{#each dpwg as gd, i (i)}
						<div class="dpwg">
							<div class="dim" style="font-size:11px;text-transform:uppercase;letter-spacing:.05em">DPWG{gd.level ? ' ' + gd.level : ''} · {gd.applies_to}</div>
							<blockquote>“{quoteText(gd.quote)}” <span class="chip untrusted" title={t('med_dpwg_from_file')}>{t('fu_untrusted_chip')}</span></blockquote>
							<div class="dim" style="font-size:11px">{t('med_dpwg_from_file')}{#if gd.url}{' · '}<span style="overflow-wrap:anywhere">{gd.url}</span>{/if}</div>
						</div>
					{/each}
				</section>
			{:else}
				<section class="card">
					<h3>{t('i2_pgx')}</h3>
					<div class="muted">{#if withheld('genome')}<span aria-hidden="true">{'🔒 '}</span>{t('i2_withheld_genome')}{:else}{t('i2_no_pgx')}{/if}</div>
				</section>
			{/each}

			{#if c.phenoconversion.length}
				<section class="card">
					<h3>{t('i2_phenoconv')}{#if c.phenoconversion[0].rule}<span class="r">rule {c.phenoconversion[0].rule}</span>{/if}</h3>
					{#each c.phenoconversion as pc (pc.gene + pc.by)}
						<div style="display:flex;gap:10px;align-items:center">
							{#if pc.computed}<span class="pill p-info"><i aria-hidden="true"></i>{t('i2_computed')}</span>{:else}<span class="pill p-stale"><i aria-hidden="true"></i>{t('i2_not_computed')}</span>{/if}
							<span>{pc.text}</span>
							<SourceChip label="" name={pc.text} src={pc.src} onopen={openSource} />
						</div>
					{/each}
				</section>
			{/if}

			<section class="card">
				<h3>{t('i2_rel_labs')}<span class="r">{t('i2_latest')}</span></h3>
				{#each c.relevant_labs as l (l.alias_group)}
					<div class="lab">
						<span style="flex:1">{l.name} <span class="dim" style="font-size:11px">· {l.reason}</span></span>
						{#if l.src}<SourceChip label={labSrc(l)} src={l.src} onopen={openSource} />{/if}
						<span class="num" style="font-weight:600;min-width:90px;text-align:right;white-space:nowrap">{#if l.value != null}{labValue(l.value)} <span class="muted" style="font-weight:400">{unitLabel(l.unit)}</span>{:else}<span class="dim">—</span>{/if}</span>
						<StatusChip status={l.status} />
					</div>
				{:else}
					<div class="muted">{#if withheld('labs')}<span aria-hidden="true">{'🔒 '}</span>{t('i2_withheld_labs')}{:else}{t('i2_no_labs')}{/if}</div>
				{/each}
			</section>
		</div>

		<div class="grid" style="align-content:start">
			<section class="card">
				<h3>{t('med_current')}</h3>
				{#if c.current_medications_relevant.length}
					<div class="tscroll">
						<table class="t">
							<tbody>
								{#each c.current_medications_relevant as m (m.src + m.name)}
									<tr>
										<td style="white-space:nowrap"><b>{m.name}</b>{#if m.kind === 'supplement'}<div style="margin-top:3px"><span class="chip">{t('med_supplement')}</span></div>{/if}</td>
										<td><StatusChip status={relTone(m.relation)} label={m.relation_text} /></td>
										<td class="muted" style="font-size:12px">
											{#if m.since}<span style="white-space:nowrap">{t('i2_since')} <span class="num">{m.since}</span></span>{/if}
											<span style="white-space:nowrap;margin-left:{m.since ? '6px' : '0'}"><SourceChip label={m.kind === 'supplement' ? '' : 'Rx'} name={m.name} src={m.src} onopen={openSource} /></span>
										</td>
									</tr>
								{/each}
							</tbody>
						</table>
					</div>
				{:else}
					<div class="muted">{#if withheld('records')}<span aria-hidden="true">{'🔒 '}</span>{t('i2_withheld_records')}{:else}{t('i2_no_meds')}{/if}</div>
				{/if}
				{#each c.not_assessed as na (na)}<div class="note" style="margin-top:6px">{t('i2_not_assessed_cap')}: {na}</div>{/each}
			</section>
			<section class="card">
				<h3>{t('i2_allergies')}</h3>
				{#if allergies.error}
					<NotReady error={allergies.error} />
				{:else if allergyItems.length}
					{#each allergyItems as a (a.id)}<div><b>{a.title}</b> <SourceChip label="" name={a.title} src={a.src} onopen={openSource} /></div>{/each}
				{:else if allergies.data}
					<div class="muted">{t('i2_no_allergy')}</div>
				{/if}
			</section>
			{#if c.limitations.length}
				<section class="card">
					<h3>{t('i2_limits')}<span class="r">CPIC {c.db_versions.cpic ?? ''}</span></h3>
					{#each c.limitations as l (l)}<div class="note" style="margin-bottom:4px">{l}</div>{/each}
				</section>
			{/if}
		</div>
	</div>
{/if}

<div class="disc">
	<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="#6ea8fe" stroke-width="2" aria-hidden="true"><circle cx="12" cy="12" r="9" /><path d="M12 8v.5M12 11v5" /></svg>
	<div style="flex:1 1 300px"><b>{t('i2_disc_b')}</b> {t('i2_disc')}</div>
	{#if drug && prof}<span class="mono mcp-snip" style="margin-left:auto;font-size:11px;color:var(--muted)">MCP: check_drug(profile="{prof.slug}", drug="{drug}")</span>{/if}
</div>

<style>
	.medh {
		font-size: 15px;
		font-weight: 620;
		margin: 4px 0 10px;
		scroll-margin-top: 12px;
	}
	.search.big {
		max-width: none;
		height: 42px;
		margin-bottom: 12px;
		background: var(--raised);
	}
	.search.big input {
		font-size: 15px;
	}
	.lab {
		display: flex;
		gap: 10px;
		align-items: center;
		padding: 5px 0;
		border-top: 1px solid #20252f;
	}
	.lab:first-of-type {
		border-top: 0;
	}
	.mcp-snip {
		white-space: nowrap;
	}
	.dpwg {
		margin-top: 10px;
		padding-top: 8px;
		border-top: 1px solid #20252f;
	}
	.chip.untrusted {
		font-size: 10px;
		color: var(--watch);
		border-color: rgba(224, 168, 74, 0.45);
	}
	/* On a phone the MCP call may wrap, on its own line under the disclaimer. */
	/* Below desktop the MCP call takes its own line under the disclaimer, so the text keeps its width. */
	@media (max-width: 1100px) {
		.disc {
			flex-wrap: wrap;
		}
		.mcp-snip {
			flex-basis: 100%;
			margin-left: 0 !important;
		}
	}
	@media (max-width: 760px) {
		/* The drug's code and genes drop under the box instead of pushing the page sideways. */
		.search.big {
			flex-wrap: wrap;
			height: auto;
			min-height: 42px;
			padding-top: 6px;
			padding-bottom: 6px;
		}
		.search.big input {
			flex: 1 1 60%;
			min-width: 0;
		}
		.mcp-snip {
			white-space: normal;
			overflow-wrap: anywhere;
		}
	}
</style>
