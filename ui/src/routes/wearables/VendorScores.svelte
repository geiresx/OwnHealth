<script lang="ts">
	// The vendor scores card of the Wearables page (GET /profiles/{p}/vendor-scores): the vendor's own
	// numbers, each labelled with who computed it, apart from the measurement charts. Hidden while
	// there is none; a failed read shows its error instead of hiding.
	import { get, p } from '#lib/api/core';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader, tf } from '#lib/components/pages1/util.svelte';
	import Sparkline from '#lib/components/Sparkline.svelte';
	import { fmtNum, i18n, t } from '#lib/i18n/index.svelte';
	import { fmtScoreDay, providerLabel, scoreDecimals, scoreRows, type VendorScores } from './scores';

	let { slug, days }: { slug: string | undefined; days: number } = $props();

	const s = new Loader<VendorScores>();
	function load() {
		const d = days;
		if (slug) s.run(() => get<VendorScores>(`${p(slug)}/vendor-scores`, { days: d }), `${slug}/${d}`);
	}
	$effect(load);

	const rows = $derived(scoreRows(s.data?.scores ?? []));
	const fmtDay = (iso: string) => fmtScoreDay(iso, i18n.lang);
</script>

{#if s.error || rows.length}
	<div class="wg" style="margin-top:12px">
		<div class="card">
			<h3>{t('vs_title')}<span class="r">{t('vs_by_vendor')}</span></h3>
			{#if s.error}
				<NotReady error={s.error} onretry={load} />
			{:else}
				<ul class="vs">
					{#each rows as r (r.category + '/' + r.provider + '/' + (r.source?.id ?? ''))}
						{@const who = providerLabel(r.provider)}
						<li>
							<div class="vs-name">
								<div style="font-weight:600">{t('vs_c_' + r.category) === 'vs_c_' + r.category ? r.category : t('vs_c_' + r.category)}</div>
								<div class="dim" style="font-size:11px">
									{tf('vs_by', { p: who })} · {r.source ? tf('vs_via', { s: r.source.label }) : t('vs_no_source')} · <span class="nw">{fmtDay(r.latest.t)}</span>
								</div>
							</div>
							{#if r.values.length > 1}<Sparkline values={r.values} status="accent" />{/if}
							<div class="vs-val">
								<b class="num">{fmtNum(r.latest.value, scoreDecimals(r.latest.value))}</b>
								{#if r.latest.qualifier}
									<div class="dim" style="font-size:11px">“{r.latest.qualifier}”</div>
									<div class="dim" style="font-size:10px">{tf('vs_word', { p: who })}</div>
								{/if}
							</div>
						</li>
					{/each}
				</ul>
				<div class="note">{t('vs_note')}</div>
			{/if}
		</div>
	</div>
{/if}

<style>
	.vs {
		list-style: none;
		margin: 6px 0 8px;
		padding: 0;
	}
	.vs li {
		display: flex;
		align-items: center;
		gap: 10px;
		padding: 6px 0;
		border-top: 1px solid var(--line);
	}
	.vs li:first-child {
		border-top: 0;
	}
	.vs-name {
		flex: 1;
		min-width: 0;
	}
	.vs-val {
		min-width: 56px;
		text-align: right;
	}
	.nw {
		white-space: nowrap;
	}
	.vs-val b {
		font-size: 18px;
	}
</style>
