<script lang="ts">
	// Wearables (mockup screen #wearables): daily rollups from GET /profiles/{p}/wearables/summary (lane C),
	// one chart per metric with its average, its coverage and the days it is missing.
	import { getWearables } from '#lib/api';
	import { getCgmSummary, type CgmSummary } from '#lib/api/inbox';
	import PhaseChip from '#lib/components/PhaseChip.svelte';
	import type { WearableMetric, WearablesSummary } from '#lib/api/types';
	import { C } from '#lib/components/colors';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader, tf, tp } from '#lib/components/pages1/util.svelte';
	import TrendChart from '#lib/components/TrendChart.svelte';
	import { axisTicks } from '#lib/logic/axis';
	import { cgmBands } from '#lib/logic/inbox';
	import { fmtNum, i18n, t } from '#lib/i18n/index.svelte';
	import { hoursMinutes } from '#lib/logic/duration';
	import { sliceStats } from '#lib/logic/health';
	import { ago } from '#lib/components/pages2/fmt';
	import { currentProfile } from '#lib/state/app.svelte';
	import VendorScores from './VendorScores.svelte';

	// How each known metric draws, as in the mockup. Unknown metrics get a plain line.
	// `max` and `minSpan` keep an axis honest: SpO2 never passes 100 %, and a 1 bpm change is not a cliff.
	type Look = { bars?: boolean; color: string; scale?: number; unit?: string; sw?: number; max?: number; minSpan?: number };
	const LOOK: Record<string, Look> = {
		sleep_asleep: { bars: true, color: '#5b84c9', scale: 1 / 60, unit: 'h' },
		sleep_in_bed: { bars: true, color: '#3f5f99', scale: 1 / 60, unit: 'h' },
		hrv_rmssd: { color: '#8fb4f6' },
		hr_resting: { color: '#e58f96', minSpan: 10 },
		hr: { color: C.accent, minSpan: 10 },
		steps: { bars: true, color: '#3fb68b' },
		vo2max: { color: '#3fb68b', sw: 2 },
		spo2: { color: C.accent, max: 100, minSpan: 10 },
		respiratory_rate: { color: C.accent, minSpan: 6 },
		weight: { color: C.accent, minSpan: 4 },
		glucose: { color: C.accent, minSpan: 40 }
	};
	const ORDER = ['sleep_asleep', 'sleep_in_bed', 'hrv_rmssd', 'hr_resting', 'steps', 'vo2max'];
	// CGM glucose has its own card below; a second line chart of it would repeat it.
	const OWN_CARD = new Set(['glucose_cgm']);

	const profile = $derived(currentProfile());
	const w = new Loader<WearablesSummary>();
	const cgm = new Loader<CgmSummary>();
	let days = $state(30);

	function load() {
		const slug = profile?.slug;
		const lang = i18n.lang;
		if (slug) w.run(() => getWearables(slug, lang), slug);
		if (slug) cgm.run(() => getCgmSummary(slug), slug);
	}
	// The five consensus ranges, very low to very high, as one stacked bar.
	type Band = 'very_low_pct' | 'low_pct' | 'in_range_pct' | 'high_pct' | 'very_high_pct';
	const CGM_BANDS: [Band, string, string][] = [
		['very_low_pct', 'h1_cgm_vlow', '#c0392b'],
		['low_pct', 'h1_cgm_low', '#e58f96'],
		['in_range_pct', 'h1_cgm_in', '#3fb68b'],
		['high_pct', 'h1_cgm_high', '#e0b252'],
		['very_high_pct', 'h1_cgm_vhigh', '#d9822b']
	];
	// The consensus thresholds of each band, in mg/dL.
	const CGM_MGDL: Record<Band, string> = { very_low_pct: '<54', low_pct: '54–69', in_range_pct: '70–180', high_pct: '181–250', very_high_pct: '>250' };
	$effect(load);

	const rank = (m: WearableMetric) => (ORDER.includes(m.metric) ? ORDER.indexOf(m.metric) : ORDER.length);
	const metrics = $derived([...(w.data?.metrics ?? [])].filter((m) => !OWN_CARD.has(m.metric)).sort((a, b) => rank(a) - rank(b)));
	// A device whose samples were all retracted feeds nothing: it is not listed.
	const devices = $derived((w.data?.devices ?? []).filter((d) => d.last_sample != null));
	const providerName = (p: string) => (t('fu_k_' + p) === 'fu_k_' + p ? p : t('fu_k_' + p));
	const localTime = (iso: string) => new Intl.DateTimeFormat(i18n.lang === 'es' ? 'es-ES' : 'en-GB', { dateStyle: 'medium', timeStyle: 'short' }).format(new Date(iso));
	const windowWord = (d: number) => t(d === 7 ? 'h1_w7' : d === 30 ? 'h1_w30' : 'h1_w1y');
	// The longest series decides which windows make sense; never offer a window the data cannot fill.
	const span = $derived(Math.max(0, ...metrics.map((m) => m.series.length)));
	const windows = $derived([7, 30, 365].filter((d) => d === 7 || span >= d));
	const fmtDay = (d: string) => new Intl.DateTimeFormat(i18n.lang === 'es' ? 'es-ES' : 'en-GB', { day: 'numeric', month: 'short', timeZone: 'UTC' }).format(new Date(d));

	function chart(m: WearableMetric) {
		const look = LOOK[m.metric] ?? { color: C.accent };
		const k = look.scale ?? 1;
		const ser = m.series.slice(-days);
		const vals = ser.map((p) => (p.value == null ? null : +(p.value * k).toFixed(2)));
		const nums = vals.filter((v): v is number => v != null);
		if (!nums.length) return null;
		// Bars start at 0 (sleep a little below its shortest night); the step comes from that span.
		const floor = !look.bars ? undefined : m.metric === 'sleep_asleep' ? Math.max(0, Math.floor(Math.min(...nums)) - 2) : 0;
		const { lo, hi, ticks } = axisTicks(nums, floor, { max: look.max, minSpan: look.minSpan });
		const n = ser.length;
		const idx = [...new Set([0, Math.round((n - 1) / 3), Math.round((2 * (n - 1)) / 3), n - 1])];
		const fmt = (v: number) => (m.metric === 'steps' ? `${fmtNum(v / 1000)}k` : look.unit ? `${fmtNum(v)}${look.unit}` : fmtNum(v));
		// The average and the coverage of the days on screen; the server's 30-day average only for 30 days.
		const st = sliceStats(ser.map((p) => p.value));
		return {
			avg: days === 30 && m.avg_30d != null ? m.avg_30d : st.avg,
			coverage: st.coverage,
			series: [{ vals, color: look.color, dots: !look.bars, sw: look.sw }],
			yMin: lo,
			yMax: hi,
			ticks,
			xlabels: idx.map((i) => [i, fmtDay(ser[i].date)] as [number, string]),
			bars: !!look.bars,
			fmt,
			missing: vals.filter((v) => v == null).length,
			unit: look.unit ?? m.unit,
			k
		};
	}
	const fmtVal = (m: WearableMetric, v: number | null, k: number, unit: string) => {
		if (v == null) return '—';
		const x = v * k;
		if (unit === 'h') return hoursMinutes(x);
		const whole = m.metric === 'steps' || unit === 'count' || unit === 'bpm' || unit === 'ms';
		return `${whole ? fmtNum(Math.round(x), 0) : fmtNum(x, 1)}${unit === 'count' ? '' : ' ' + unit}`;
	};
</script>

<div class="ph">
	<div>
		<h1>{t('title_wear')}</h1>
		<div class="sub">{tf('h1_w_sub', { name: profile?.display_name ?? '', d: days })}</div>
	</div>
	<div class="right">
		<PhaseChip phase="Apple Health v0.1" /><PhaseChip phase="Nightscout v0.2" /><PhaseChip phase="Oura, Withings v1.0" />
		{#if windows.length > 1}
			<div class="seg" role="group" aria-label={t('title_wear')}>
				{#each windows as d (d)}
					<button class:on={days === d} aria-pressed={days === d} onclick={() => (days = d)}>{t(d === 7 ? 'h1_w7' : d === 30 ? 'h1_w30' : 'h1_w1y')}</button>
				{/each}
			</div>
		{/if}
	</div>
</div>

{#if w.loading && !w.data}
	<div class="card"><div class="empty">{t('loading')}</div></div>
{:else if w.error}
	<div class="card"><NotReady error={w.error} onretry={load} /></div>
{:else if w.data && !w.data.metrics.length}
	<div class="card"><div class="empty">{t('h1_w_none')}</div></div>
{:else if w.data}
	<div class="devs">
		{#each devices as d (d.source_id)}
			<div class="dev">
				<div style="flex:1;min-width:0">
					<div style="font-weight:600">{d.device ?? providerName(d.provider)}</div>
					<div class="dim" style="font-size:11px" title={d.last_sample ? localTime(d.last_sample) : undefined}>
						{#if d.device}{providerName(d.provider)}{' · '}{/if}<span style="white-space:nowrap">{tf('h1_last_sample', { d: ago(d.last_sample) })}</span>
					</div>
				</div>
				<div style="width:76px;text-align:right">
					<div class="num" style="font-weight:650">{fmtNum(d.coverage_pct, 0)}%</div>
					<div class="bar"><i style="width:{d.coverage_pct}%;background:{d.coverage_pct > 90 ? C.ok : C.watch}"></i></div>
					<div class="dim" style="font-size:10px">{t('h1_coverage')}</div>
				</div>
			</div>
		{/each}
	</div>
	<div class="wg">
		{#each metrics as m (m.metric)}
			{@const c = chart(m)}
			<div class="card">
				<h3>
					{m.label}
					<span class="r" title={c ? tf('h1_latest', { v: fmtVal(m, m.latest, c.k, c.unit) }) : undefined}>{#if c}{tf('h1_avg_w', { v: fmtVal(m, c.avg, c.k, c.unit), w: windowWord(days) })}{/if}</span>
				</h3>
				{#if c}
					<TrendChart
						series={c.series}
						yMin={c.yMin}
						yMax={c.yMax}
						ticks={c.ticks}
						xlabels={c.xlabels}
						bars={c.bars}
						fmt={c.fmt}
						label={tf('h1_chart_w', { label: m.label, avg: fmtVal(m, c.avg, c.k, c.unit), n: days })}
					/>
				{:else}
					<div class="empty">{t('h1_nomark')}</div>
				{/if}
				<div style="display:flex;justify-content:space-between;align-items:flex-start;gap:8px;margin-top:2px">
					{#if m.source}
						<span class="src" style="cursor:default"><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M6 3h8l4 4v14H6z" /><path d="M14 3v4h4" /></svg>{m.source}</span>
					{:else}
						<span class="dim" style="font-size:11px">{t('h1_several_src')}</span>
					{/if}
					<span class="dim" style="font-size:11px;text-align:right">
						{#if m.metric === 'vo2max'}{t('h1_estimate')}{:else if c?.missing}{tp('h1_missing', c.missing)}{:else if c}{tf('h1_cov_pct', { n: fmtNum(c.coverage, 0) })}{/if}
					</span>
				</div>
			</div>
		{/each}
	</div>
{/if}

<!-- Outside the wearables branches: a failed or empty wearables summary never hides the CGM card. -->
<div class="wg" style="margin-top:12px">
	<div class="card cgm-card">
		<h3>{t('h1_cgm')}<span class="r">{t('h1_cgm_window')}</span></h3>
		{#if cgm.loading && !cgm.data}
			<div class="empty">{t('loading')}</div>
		{:else if cgm.error}
			<NotReady error={cgm.error} onretry={load} />
		{:else if cgm.data && cgm.data.readings > 0}
			{@const g = cgm.data}
			{@const bands = cgmBands(g, CGM_BANDS.map(([k]) => k))}
			<div class="cgm">
				<div><div class="dim">{t('h1_cgm_tir')}</div><b class="num">{g.in_range_pct == null ? '—' : fmtNum(g.in_range_pct, 1) + ' %'}</b></div>
				<div><div class="dim">{t('h1_cgm_mean')}</div><b class="num" style="white-space:nowrap">{g.mean_mg_dl == null ? '—' : fmtNum(g.mean_mg_dl, 0) + ' mg/dL'}</b></div>
				<div><div class="dim">GMI</div><b class="num">{g.gmi_percent == null ? '—' : fmtNum(g.gmi_percent, 1) + ' %'}</b></div>
				<div><div class="dim">{t('h1_cgm_wear')}</div><b class="num">{fmtNum(g.wear_fraction * 100, 0)} %</b></div>
			</div>
			<!-- Not valid: the server gives no range figures, so no bar and no legend (a missing band is not 0 %). -->
			{#if bands}
				<div class="tir" role="img" aria-label={t('h1_cgm_tir')}>
					{#each CGM_BANDS as [k, label, color] (k)}
						{#if bands[k] > 0}<span style="width:{bands[k]}%;background:{color}" title="{t(label)}: {fmtNum(bands[k], 1)} %"></span>{/if}
					{/each}
				</div>
				<div class="dim" style="font-size:11px;display:flex;flex-wrap:wrap;gap:8px">
					{#each CGM_BANDS as [k, label, color] (k)}<span style="white-space:nowrap"><i style="background:{color}"></i>{t(label)} <span class="num">{CGM_MGDL[k]}</span> · {fmtNum(bands[k], 1)} %</span>{/each}
				</div>
			{/if}
			{#if !g.valid}<div class="note" style="margin-top:6px">{t('h1_cgm_bad_' + (g.invalid_reason ?? 'no_readings'))}</div>{/if}
		{:else}
			<div class="empty">{t('h1_cgm_d')}</div>
		{/if}
	</div>
</div>

<!-- The vendor's own scores, apart from the measurements above (slice 27). -->
<VendorScores slug={profile?.slug} {days} />

<style>
	.cgm {
		display: grid;
		grid-template-columns: repeat(4, 1fr);
		gap: 8px;
		font-size: 12px;
		margin: 6px 0 10px;
	}
	.cgm b {
		font-size: 18px;
	}
	/* The CGM card takes two of the three desktop columns, as in the mockup; one column under 760 px. */
	.cgm-card {
		grid-column: span 2;
	}
	@media (max-width: 760px) {
		.cgm-card {
			grid-column: auto;
		}
		.cgm {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
	}
	.tir {
		display: flex;
		height: 12px;
		border-radius: 6px;
		overflow: hidden;
		background: var(--raised);
		margin-bottom: 6px;
	}
	.dim i {
		display: inline-block;
		width: 8px;
		height: 8px;
		border-radius: 2px;
		margin-right: 4px;
	}
</style>
