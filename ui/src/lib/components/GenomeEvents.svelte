<script lang="ts">
	// The ClinVar history of the Genome screen (genomics.md section 9): every change the weekly ClinVar
	// check found for a variant this genome carries, newest first, with the release in use and whether
	// the check is on. GET /profiles/{p}/genome/events gates each row for the reader: a withheld row
	// names nothing. OwnHealth only compares; OwnGenome makes a reclassified call again.
	import { getGenomeEvents } from '#lib/api';
	import type { ClinvarClass, GenomeEvent, GenomeEvents } from '#lib/api/types';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader, tf } from '#lib/components/pages1/util.svelte';
	import { day } from '#lib/components/pages2/fmt';
	import { i18n, t } from '#lib/i18n/index.svelte';

	// `rev` changes when the page reloads (a sensitive-switch change renames or hides rows).
	// `release` hands the release in use back to the page subtitle.
	let { profile, rev, release = $bindable(null) }: { profile: string; rev?: unknown; release?: string | null } = $props();

	const view = new Loader<GenomeEvents>();
	// A new `rev` or a retry is a new request key, so rows the reader may no longer see clear at once.
	let attempt = $state(0);
	let revN = 0;
	let lastRev: unknown;
	$effect(() => {
		const p = profile;
		if (rev !== lastRev) {
			lastRev = rev;
			revN++;
		}
		const n = attempt;
		view.run(() => getGenomeEvents(p), `${p}/${revN}/${n}`);
	});
	$effect(() => {
		release = view.data?.release ?? null;
	});

	const v = $derived(view.data);
	let open = $state<string | null>(null);
	const lang = $derived(i18n.lang === 'es' ? 'es' : 'en');
	// The OwnGenome command, kept on one line so it reads as one command.
	const RECHECK = 'bin/og recheck --sample <name> --rerun';
	const rerun = $derived((v?.rerun?.[lang] ?? '').split(RECHECK));
	const stars = (n: number | null | undefined) => (n == null ? '' : '★'.repeat(n) + '☆'.repeat(Math.max(0, 4 - n)));
	function before(e: GenomeEvent): string {
		const o = e.old as (ClinvarClass & { in_previous_release?: boolean }) | undefined;
		if (o?.class) return t('gen_cv_c_' + o.class);
		return o?.in_previous_release === false ? t('gen_cv_not_in_prev') : t('gen_cv_not_reported');
	}
	const toggle = (id: string) => (open = open === id ? null : id);
</script>

<section class="card cvh" aria-labelledby="cvh-h">
	<h3 id="cvh-h">
		{t('gen_cv_t')}
		<span class="r">
			{#if v}
				<!-- Before the first comparison the body says so, and the page subtitle names the genome file's release. -->
				{#if v.release}<span class="dim num">{tf('gen_cv_release', { date: day(v.release) })}</span>{/if}
				<a class="pill {v.weekly ? 'p-ok' : 'p-stale'}" href="/settings"><i aria-hidden="true"></i>{t(v.weekly ? 'gen_cv_on' : 'gen_cv_off')}</a>
			{/if}
		</span>
	</h3>
	<div class="muted intro">{t('gen_cv_d')}</div>
	{#if view.error}
		<NotReady error={view.error} onretry={() => attempt++} />
	{:else if view.loading && !v}
		<div class="empty">{t('loading')}</div>
	{:else if v}
		{#if !v.events.length}
			<div class="empty">{v.release ? tf('gen_cv_none', { date: day(v.release) }) : t('gen_cv_none_yet')}</div>
		{:else}
			<ul class="rows">
				{#each v.events as e (e.id)}
					<li>
						<div class="head">
							<span class="num dim when">{day(e.release_to)}</span>
							{#if e.gene}<b>{e.gene}</b>{/if}
							{#if e.alerted}<span class="pill p-watch"><i aria-hidden="true"></i>{t('gen_cv_alert')}</span>{/if}
							<button class="btn sm see" onclick={() => toggle(e.id)} aria-expanded={open === e.id} aria-controls="cv-{e.id}">{t('gen_cv_see')}</button>
						</div>
						<p class="txt">{#if e.withheld}<span aria-hidden="true">{'🔒 '}</span>{/if}{e.text[lang]}</p>
						{#if open === e.id}
							<div class="diff" id="cv-{e.id}">
								{#if e.withheld}
									<div class="dim">{t('gen_cv_withheld_d')}</div>
								{:else}
									<dl>
										<dt>{t('gen_cv_before')}</dt>
										<dd>
											{before(e)}
											{#if e.old?.stars != null}<span class="st" role="img" aria-label={tf('gen_cv_stars', { n: e.old.stars })} title={tf('gen_cv_stars', { n: e.old.stars })}>{stars(e.old.stars)}</span>{/if}
											{#if e.old?.significance}<span class="mono dim">{e.old.significance}</span>{/if}
											{#if e.release_from}<span class="dim num">· {e.release_from}</span>{/if}
										</dd>
										<dt>{t('gen_cv_now')}</dt>
										<dd>
											{t('gen_cv_c_' + (e.new?.class ?? 'other'))}
											{#if e.new?.stars != null}<span class="st" role="img" aria-label={tf('gen_cv_stars', { n: e.new.stars })} title={tf('gen_cv_stars', { n: e.new.stars })}>{stars(e.new.stars)}</span>{/if}
											{#if e.new?.significance}<span class="mono dim">{e.new.significance}</span>{/if}
											<span class="dim num">· {e.release_to}</span>
										</dd>
									</dl>
									{#if e.variation_id}<a class="dim" href="https://www.ncbi.nlm.nih.gov/clinvar/variation/{e.variation_id}/" target="_blank" rel="noopener">{tf('gen_cv_in_clinvar', { id: e.variation_id })} ↗</a>{/if}
								{/if}
								{#if rerun[0]}<div class="rerun muted">{rerun[0]}{#if rerun.length > 1}<code class="mono">{RECHECK}</code>{rerun.slice(1).join(RECHECK)}{/if}</div>{/if}
							</div>
						{/if}
					</li>
				{/each}
			</ul>
		{/if}
	{/if}
</section>

<style>
	.cvh {
		margin-top: 12px;
	}
	.cvh h3 .r {
		display: inline-flex;
		flex-wrap: wrap;
		align-items: center;
		justify-content: flex-end;
		gap: 6px 10px;
		font-weight: 400;
		font-size: 12px;
	}
	.cvh h3 a.pill {
		text-decoration: none;
	}
	.intro {
		font-size: 12px;
		margin-bottom: 8px;
	}
	.rows {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.rows li {
		padding: 9px 0;
		border-top: 1px solid var(--line);
	}
	.head {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 6px 8px;
	}
	.when {
		font-size: 12px;
		white-space: nowrap;
	}
	.see {
		margin-left: auto;
	}
	.txt {
		margin: 5px 0 0;
		font-size: 12.5px;
	}
	.diff {
		margin-top: 8px;
		padding: 8px 10px;
		border-radius: 8px;
		background: var(--raised);
		font-size: 12px;
		display: grid;
		gap: 6px;
	}
	.diff dl {
		display: grid;
		grid-template-columns: auto minmax(0, 1fr);
		gap: 3px 12px;
		margin: 0;
	}
	.diff dt {
		color: var(--dim);
	}
	.diff dd {
		margin: 0;
		display: flex;
		flex-wrap: wrap;
		gap: 2px 8px;
		overflow-wrap: anywhere;
	}
	.st {
		color: var(--watch);
		letter-spacing: 1px;
	}
	.rerun code {
		white-space: nowrap;
	}
	@media (max-width: 760px) {
		.rerun code {
			white-space: normal;
			overflow-wrap: anywhere;
		}
	}
</style>
