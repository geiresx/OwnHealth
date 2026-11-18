<script lang="ts">
	// Biomarkers (mockup screen #biomarker). Without ?code= it lists the profile's analytes; with it, the
	// detail: trend over the lab's printed reference band, a source chip per point, the printed string on
	// hover and a unit toggle. Values come from lane A (observations, trend); status words from lane E.
	import { page } from '$app/state';
	import { getCalculators, getDashboard, getTrend, searchObservations, type TrendPointStatus, type Units } from '#lib/api';
	import type { Calculators, Dashboard, MarkerRow, ObservationOut, Trend } from '#lib/api/types';
	import { C } from '#lib/components/colors';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import SourcePanel from '#lib/components/pages1/SourcePanel.svelte';
	import { openSource } from '#lib/components/pages1/source.svelte';
	import Tip from '#lib/components/pages1/Tip.svelte';
	import { Loader, tf, tp, unitLabel, yfrac } from '#lib/components/pages1/util.svelte';
	import { ApiError } from '#lib/api/core';
	import PhaseChip from '#lib/components/PhaseChip.svelte';
	import SourceChip from '#lib/components/SourceChip.svelte';
	import ManualEntry from '#lib/components/ManualEntry.svelte';
	import Sparkline from '#lib/components/Sparkline.svelte';
	import StatusChip from '#lib/components/StatusChip.svelte';
	import { fmtNum, i18n, t } from '#lib/i18n/index.svelte';
	import { buildBars, niceStep } from '#lib/logic/axis';
	import { calcUnit, forGroup, rowsForCard } from '#lib/logic/calc';
	import { refRange, sourceLabel } from '#lib/logic/format';
	import { currentProfile } from '#lib/state/app.svelte';

	const profile = $derived(currentProfile());
	const code = $derived(page.url.searchParams.get('code'));
	// ?q= from the top-bar search filters the list.
	const query = $derived(page.url.searchParams.get('q') ?? '');
	// 'direct' / 'calculated' and other method words, translated when the dict knows them.
	const methodLabel = (m: string) => {
		const k = 'h1_method_' + m.toLowerCase();
		return t(k) === k ? m : t(k);
	};

	// ---- list ----
	const obs = new Loader<{ items: ObservationOut[] }>();
	// ---- detail: one trend per unit system, so the toggle shows only units that really differ ----
	const trends = new Loader<Trend[]>();
	const dash = new Loader<Dashboard>();
	// v0.3 calculators: published formulas on this profile's labs (read:labs), computed on read.
	const calcs = new Loader<Calculators>();
	let unitIdx = $state(0);
	let adding = $state(false);
	// the calculators card's smoking status form, opened from a risk result that waits for it
	let smoking = $state(false);

	function load() {
		const slug = profile?.slug;
		const lang = i18n.lang;
		if (!slug) return;
		if (!code) {
			const q = query || undefined;
			obs.run(() => searchObservations(slug, { category: 'laboratory', lang, q }), slug);
			dash.run(() => getDashboard(slug, lang), slug);
			return;
		}
		unitIdx = 0;
		const c = code;
		trends.run(async () => {
			const all = await Promise.allSettled((['as_reported', 'si', 'conventional'] as Units[]).map((u) => getTrend(slug, c, 'all', u, lang)));
			const ok = all.flatMap((r) => (r.status === 'fulfilled' ? [r.value] : []));
			if (!ok.length) throw (all[0] as PromiseRejectedResult).reason;
			return ok.filter((tr, i) => ok.findIndex((o) => o.unit === tr.unit) === i);
		}, `${slug}/${c}`);
		dash.run(() => getDashboard(slug, lang), slug);
		calcs.run(() => getCalculators(slug, lang), slug);
	}
	$effect(load);

	// List rows: latest value per alias group, newest first.
	const groups = $derived.by(() => {
		const by = new Map<string, { latest: ObservationOut; n: number }>();
		for (const o of obs.data?.items ?? []) {
			const g = by.get(o.alias_group);
			if (!g) by.set(o.alias_group, { latest: o, n: 1 });
			else {
				g.n++;
				if (o.date > g.latest.date) g.latest = o;
			}
		}
		return [...by.values()].sort((a, b) => a.latest.name.localeCompare(b.latest.name));
	});

	// Each marker's status word from the dashboard (lane E). A marker the dashboard does not show has none.
	const statusOf = $derived(new Map((dash.data?.systems ?? []).flatMap((s) => (s.withheld ? [] : s.markers.map((m) => [m.alias_group, m.status] as const)))));
	// An unknown code is not an outage: there is nothing to retry.
	const unknown = $derived(trends.error instanceof ApiError && trends.error.status === 404 && trends.error.code === 'unknown');

	const tr = $derived(trends.data?.[Math.min(unitIdx, (trends.data?.length ?? 1) - 1)] ?? null);
	const pts = $derived((tr?.points ?? []) as TrendPointStatus[]);
	const last = $derived(pts.length ? pts[pts.length - 1] : null);
	const labs = $derived([...new Set(pts.map((p) => p.lab ?? '—'))]);
	// The dashboard card holding this marker gives its status and the related markers.
	const card = $derived(dash.data?.systems.find((s) => s.markers.some((m) => m.alias_group === tr?.alias_group)) ?? null);
	const marker = $derived(card?.markers.find((m) => m.alias_group === tr?.alias_group) ?? null);
	const related = $derived((card?.markers ?? []).filter((m) => m.alias_group !== tr?.alias_group) as MarkerRow[]);
	// The calculators that read this marker, and the computed rows of its dashboard card.
	const calcShown = $derived(forGroup(calcs.data?.items ?? [], tr?.alias_group ?? code ?? ''));
	const calcRows = $derived(card ? rowsForCard(calcs.data?.items ?? [], card.key) : []);

	// A point's colour: its status from lane E when present, else the lab's own flag (abnormal or not).
	const ABN = new Set(['L', 'H', 'LL', 'HH', 'A']);
	function ptColor(p: TrendPointStatus): string {
		if (p.status) return C[p.status];
		if (p.flag && ABN.has(p.flag)) return C.out;
		return p.flag === 'N' ? C.ok : C.accent;
	}
	const SHAPES = ['circle', 'square', 'diamond', 'triangle'] as const;
	const GLYPH = { circle: '●', square: '■', diamond: '◆', triangle: '▲' };
	const shapeOf = (lab: string | null) => SHAPES[Math.max(0, labs.indexOf(lab ?? '—')) % SHAPES.length];

	// ---- chart geometry (port of the mockup's ldlChart) ----
	// 860 wide as in the mockup. In a narrow card the drawing gets the card's own width (a little less,
	// so 10-unit text lands at 11 px), or a phone would shrink every label to about 4 px.
	let chartW = $state(0);
	const W = $derived(chartW > 0 && chartW < 760 ? Math.max(300, Math.round(chartW / 1.1)) : 860);
	const H = 330;
	const pl = 60;
	const pr = 20;
	const pt = 14;
	const pb = 74;
	const iw = $derived(W - pl - pr);
	const ih = H - pt - pb;
	const geo = $derived.by(() => {
		if (!pts.length || !tr) return null;
		const asOf = new Date().toISOString().slice(0, 10);
		const x0 = Math.floor(yfrac(pts[0].date));
		const x1 = Math.max(Math.ceil(yfrac(pts[pts.length - 1].date) + 0.01), Math.ceil(yfrac(asOf)));
		const bl = tr.stats.baseline ?? null;
		const vals = [...pts.flatMap((p) => [p.value, p.ref_low, p.ref_high]), bl?.low, bl?.high].filter((v): v is number => v != null);
		const lo = Math.min(...vals);
		const hi = Math.max(...vals);
		const step = niceStep((hi - lo || Math.abs(hi) || 1) / 4);
		const y0 = Math.max(0, Math.floor((lo - step * 0.5) / step) * step);
		const y1 = Math.ceil((hi + step * 0.3) / step) * step;
		const sx = (y: number) => pl + ((y - x0) / (x1 - x0)) * iw;
		const sy = (v: number) => pt + ih - ((v - y0) / (y1 - y0 || 1)) * ih;
		const ticks: number[] = [];
		for (let v = y0; v <= y1 + 1e-9; v += step) ticks.push(+v.toFixed(6));
		const years: number[] = [];
		for (let y = x0; y <= x1; y++) years.push(y);
		const xy = pts.map((p) => [sx(yfrac(p.date)), sy(p.value)] as [number, number]);
		// Reference band as printed, one step per point: from halfway to the previous point to halfway to the next.
		const band = pts.map((p, i) => {
			const a = i === 0 ? sx(x0) : (xy[i - 1][0] + xy[i][0]) / 2;
			const b = i === pts.length - 1 ? sx(x1) : (xy[i][0] + xy[i + 1][0]) / 2;
			if (p.ref_low == null && p.ref_high == null) return null;
			const top = sy(Math.min(p.ref_high ?? y1, y1));
			const bot = sy(Math.max(p.ref_low ?? y0, y0));
			return { x: a, w: b - a, y: top, h: Math.max(0, bot - top) };
		});
		// Dashed markers where the lab or the code changes between two points.
		const marks: { x: number; label: string; row: number }[] = [];
		for (let i = 1; i < pts.length; i++) {
			const a = pts[i - 1];
			const b = pts[i];
			// the server compares method and lab without case or spacing; trust it when it answers
			if (b.method_change === false) continue;
			const parts = [];
			if ((a.lab ?? '') !== (b.lab ?? '')) parts.push(t('h1_lab_change'));
			if ((a.loinc ?? '') !== (b.loinc ?? '') || (a.method ?? '') !== (b.method ?? '')) parts.push(b.method ? methodLabel(b.method) : t('h1_code_change'));
			if (parts.length) marks.push({ x: (xy[i - 1][0] + xy[i][0]) / 2, label: parts.join(' · '), row: 0 });
		}
		// Each label takes the first row where it does not print over an earlier one (about 5.6 px per character).
		const rowEnd: number[] = [];
		for (const m of marks) {
			let r = rowEnd.findIndex((end) => m.x + 4 > end + 6);
			if (r < 0) r = rowEnd.length;
			rowEnd[r] = m.x + 4 + m.label.length * 5.6;
			m.row = r;
		}
		const clampX = (d: string) => Math.min(sx(x1), Math.max(sx(x0), sx(yfrac(d))));
		// Annotations with an end draw as a bar on the Meds row; one-day ones as a dot on the Events row.
		// A course with no end is still running and draws to today.
		const bars = buildBars(tr.annotations, clampX, asOf, `${x0}-01-01`);
		// The person's usual level before the last point (rule pack 0.2.0), drawn across the chart.
		const usual = bl ? { y: sy(Math.min(bl.high, y1)), h: Math.max(0, sy(Math.max(bl.low, y0)) - sy(Math.min(bl.high, y1))), yl: sy(bl.low), yh: sy(bl.high), n: bl.n } : null;
		return { x0, x1, y0, y1, sx, sy, ticks, years, xy, band, marks, bars, usual };
	});

	// Courses too short to hold their name, near the right edge: their names go under the chart.
	const unlabelled = $derived(
		geo
			? [...new Set(geo.bars.filter((b) => b.span && !(b.x2 - b.x > b.label.length * 5.6 + 12) && !(b.x2 + 6 + b.label.length * 5.6 < W - pr)).map((b) => b.label))]
			: []
	);
	// Extra Meds rows when courses overlap: each adds 14 px below the chart.
	const extra = $derived(geo ? Math.max(0, ...geo.bars.filter((b) => b.span).map((b) => b.row)) * 14 : 0);
	// Hover: the printed string and its provenance.
	let tip = $state<{ i: number; x: number; y: number } | null>(null);
	const fmt = (v: number) => fmtNum(v);
	const pctFmt = (v: number) => `${v > 0 ? '+' : v < 0 ? '−' : ''}${fmtNum(Math.abs(v), 1)}%`;
	const change = $derived(pts.length > 1 ? pts[pts.length - 1].value - pts[pts.length - 2].value : null);
	// The printed range is in the lab's unit. In another unit the range comes from the converted numbers.
	const converted = $derived(unitIdx > 0);
	const rangeOf = (p: TrendPointStatus) => (!converted && p.range) || refRange(p.ref_low, p.ref_high, fmt);
	const years = (a: string, b: string) => (a === b ? a : `${a}–${b}`);
	// A number from the dashboard ('5.6') in the reader's language; words such as '7:14' stay as they are.
	// a number in the reader's language; a censored value ('<5', '>=0.3') keeps its comparator
	const numText = (v: string | number) => {
		if (typeof v === 'number') return fmtNum(v);
		const m = /^([<>]=?)?(-?\d+(\.\d+)?)$/.exec(v);
		return m ? (m[1] ?? '') + fmtNum(Number(m[2])) : v;
	};
</script>

{#if !code}
	<div class="ph">
		<div>
			<h1>{t('nav_bio')}</h1>
			{#if obs.data}<div class="sub">{tp('h1_bio_sub', groups.length, { name: profile?.display_name ?? '' })}</div>{/if}
		</div>
		<div class="right">
			{#if profile && profile.role !== 'viewer'}<button class="btn" onclick={() => (adding = !adding)} aria-expanded={adding}>{t('me_add_value')}</button>{/if}
			<PhaseChip phase="v0.1" />
		</div>
	</div>
	{#if adding && profile}
		<div class="card" style="margin-bottom:12px">
			<h3>{t('me_add_value')}</h3>
			<!-- a form belongs to one profile: another profile gets a fresh one -->
			{#key profile.slug}<ManualEntry profile={profile.slug} kinds={['observation', 'smoking']} onsaved={load} oncancel={() => (adding = false)} />{/key}
		</div>
	{/if}
	{#if query}<div class="note" style="margin-bottom:6px">{t('me_filtered')} <b>{query}</b> · <a href="/biomarkers">{t('h1_clear')}</a></div>{/if}
	<div class="card" style="padding:6px 6px 2px">
		{#if obs.loading && !obs.data}
			<div class="empty">{t('loading')}</div>
		{:else if obs.error}
			<NotReady error={obs.error} onretry={load} />
		{:else if !groups.length}
			<div class="empty">{t('h1_bio_none')}</div>
		{:else}
			<div class="tscroll">
			<table class="t nw bml">
				<thead><tr><th>{t('h1_th_name')}</th><th>{t('h1_th_latest')}</th><th>{t('h1_th_status')}</th><th class="ph-hide">{t('h1_th_printed')}</th><th>{t('h1_th_date')}</th><th>{t('h1_th_lab')}</th><th style="text-align:right">{t('h1_th_n')}</th><th>{t('h1_th_source')}</th></tr></thead>
				<tbody>
					{#each groups as g (g.latest.alias_group)}
						<tr>
							<td><a href="/biomarkers?code={encodeURIComponent(g.latest.alias_group)}">{g.latest.name}</a></td>
							<td class="num" style="font-weight:600" title={g.latest.printed.value + (g.latest.printed.unit ? ' ' + g.latest.printed.unit : '')}>
								{#if g.latest.value != null}{fmtNum(g.latest.value)} <span class="muted" style="font-weight:400">{unitLabel(g.latest.unit ?? g.latest.printed.unit)}</span>
								{:else}{g.latest.printed.value} <span class="muted" style="font-weight:400">{unitLabel(g.latest.printed.unit)}</span>{/if}
							</td>
							<td>
								{#if statusOf.get(g.latest.alias_group)}<StatusChip status={statusOf.get(g.latest.alias_group)!} />
								{:else}<span class="dim">—</span>{/if}
							</td>
							<td class="mono muted ph-hide">{g.latest.printed.name}</td>
							<td class="num">{g.latest.date}</td>
							<td>{g.latest.lab ?? '—'}</td>
							<td class="num" style="text-align:right">{g.n}</td>
							<td><SourceChip label={g.latest.src_label ?? g.latest.src} src={g.latest.src} onopen={openSource} /></td>
						</tr>
					{/each}
				</tbody>
			</table>
			</div>
		{/if}
	</div>
{:else}
	<div class="ph">
		<div>
			<h1>
				{tr?.name ?? code}
				{#each tr?.codes ?? [] as c (c)}
					<span class="chip mono" style="vertical-align:3px">{c.startsWith('LOINC:') ? 'LOINC ' + c.slice(6) : c}</span>{' '}
				{/each}
			</h1>
			<div class="sub">
				{#if pts.length}
					{profile?.display_name} · {tp('h1_meas', pts.length, { labs: tp('h1_labs', labs.length), years: years(pts[0].date.slice(0, 4), pts[pts.length - 1].date.slice(0, 4)) })}
				{/if}
			</div>
		</div>
		<div class="right"><a class="btn sm" href="/biomarkers">{t('h1_back')}</a><PhaseChip phase="v0.1" /><PhaseChip phase={t('h1_ph_calc')} /></div>
	</div>

	{#if trends.loading && !trends.data}
		<div class="card"><div class="empty">{t('loading')}</div></div>
	{:else if unknown}
		<div class="card"><div class="empty">{tf('h1_code_unknown', { code: code ?? '' })}</div></div>
	{:else if trends.error}
		<div class="card"><NotReady error={trends.error} onretry={load} /></div>
	{:else if tr && !pts.length}
		<div class="card"><div class="empty">{t('h1_trend_none')}</div></div>
	{:else if tr && geo && last}
		<div class="bm">
			<div>
				<div class="card" style="padding:12px 14px 6px">
					<div style="display:flex;align-items:center;gap:6px 16px;margin-bottom:4px;flex-wrap:wrap">
						<div class="kv"><span class="big num">{fmt(last.value)}</span><span class="muted">{unitLabel(last.unit)}</span></div>
						{#if marker}<StatusChip status={marker.status} />{:else if last.status}<StatusChip status={last.status} />{/if}
						<span class="muted" style="font-size:12px"><span class="num" style="white-space:nowrap">{last.date}</span> · <SourceChip label={last.src_label ?? last.src} src={last.src} onopen={openSource} /></span>
						{#if (trends.data?.length ?? 0) > 1}
							<div class="seg" role="group" aria-label={t('h1_th_value')} style="margin-left:auto">
								{#each trends.data ?? [] as u, i (u.unit)}
									<button class:on={i === unitIdx} aria-pressed={i === unitIdx} onclick={() => (unitIdx = i)}>{unitLabel(u.unit)}</button>
								{/each}
							</div>
						{/if}
					</div>
					<div class="legend" style="margin:2px 0 2px">
						<span><i style="width:9px;height:9px;border-radius:50%;background:{C.ok}"></i>{t('h1_lg_ok')}</span>
						<span><i style="width:9px;height:9px;border-radius:50%;background:{C.watch}"></i>{t('calc_lg_watch')}</span>
						<span><i style="width:9px;height:9px;border-radius:50%;background:{C.out}"></i>{t('h1_lg_out')}</span>
						<span class="dim">{t('h1_lg_shape')} {#each labs as l (l)}{GLYPH[shapeOf(l)]} {l}{' '}{/each}</span>
						<span class="dim">{t('h1_lg_flags')}</span>
						{#if tr.stats.baseline}<span><i style="width:14px;height:0;border-top:1.5px dashed {C.accent}"></i>{tf('calc_bl_band', { n: tr.stats.baseline.n })}</span>{/if}
					</div>
					<!-- role=group, not img: its points are buttons a keyboard and a screen reader reach one by one. -->
					<div bind:clientWidth={chartW}>
					<svg width="100%" viewBox="0 0 {W} {H + extra}" role="group" aria-label={tp('h1_chart_label', pts.length, { name: tr.name, v: fmt(last.value), u: unitLabel(last.unit) })}>
						{#each geo.ticks as v (v)}
							<line x1={pl} x2={W - pr} y1={geo.sy(v)} y2={geo.sy(v)} stroke={C.grid} />
							<text x={pl - 7} y={geo.sy(v) + 4} fill={C.axis} font-size="11" text-anchor="end">{fmt(v)}</text>
						{/each}
						{#each geo.band as b, i (i)}
							{#if b}<rect x={b.x} y={b.y} width={b.w} height={b.h} fill="#3fb68b" opacity=".08" />{/if}
						{/each}
						<!-- Only when it fits: on a phone the legend line above says what the band is. -->
						{#if t('h1_band').length * 5.8 < iw - 12}<text x={pl + 6} y={geo.sy(geo.y0) - 6} fill="#3fb68b" font-size="10.5" opacity=".9">{t('h1_band')}</text>{/if}
						{#if geo.usual}
							{@const u = geo.usual}
							<rect x={pl} y={u.y} width={iw} height={u.h} fill={C.accent} opacity=".07" />
							<line x1={pl} x2={W - pr} y1={u.yh} y2={u.yh} stroke={C.accent} stroke-dasharray="4 3" opacity=".7" />
							<line x1={pl} x2={W - pr} y1={u.yl} y2={u.yl} stroke={C.accent} stroke-dasharray="4 3" opacity=".7" />
						{/if}
						{#each geo.years as y (y)}
							<line x1={geo.sx(y)} x2={geo.sx(y)} y1={pt} y2={pt + ih} stroke={C.grid} opacity=".6" />
							<text x={geo.sx(y)} y={pt + ih + 15} fill={C.axis} font-size="11" text-anchor="middle">{y}</text>
						{/each}
						{#each geo.marks as m, i (i)}
							<line x1={m.x} x2={m.x} y1={pt} y2={pt + ih} stroke="#56607a" stroke-dasharray="2 3" />
							<text x={m.x + 4} y={pt + 10 + m.row * 12} fill="#8a93a5" font-size="10">{m.label}</text>
						{/each}
						<polyline points={geo.xy.map((p) => p.join(',')).join(' ')} fill="none" stroke="#8792a6" stroke-width="1.6" />
						{#each pts as p, i (p.src + "|" + i)}
							{@const [x, y] = geo.xy[i]}
							{@const shape = shapeOf(p.lab)}
							<g
								fill={ptColor(p)}
								stroke="#0f1115"
								stroke-width="1.5"
								style="cursor:pointer"
								role="button"
								tabindex="0"
								aria-label={[p.date, p.printed, p.lab, p.method_change && t('calc_pt_change')].filter(Boolean).join(' · ')}
								onmousemove={(e) => (tip = { i, x: e.clientX, y: e.clientY })}
								onmouseleave={() => (tip = null)}
								onfocus={(e) => {
									const r = (e.currentTarget as SVGGElement).getBoundingClientRect();
									tip = { i, x: r.right, y: r.top };
								}}
								onblur={() => (tip = null)}
								onclick={() => openSource(p.src)}
								onkeydown={(e) => {
									if (e.key !== 'Enter' && e.key !== ' ') return;
									e.preventDefault();
									openSource(p.src);
								}}
							>
								{#if shape === 'circle'}<circle cx={x} cy={y} r="5.5" />
								{:else if shape === 'square'}<rect x={x - 5} y={y - 5} width="10" height="10" rx="1.5" />
								{:else if shape === 'diamond'}<path d="M{x} {y - 7}L{x + 7} {y}L{x} {y + 7}L{x - 7} {y}Z" />
								{:else}<path d="M{x} {y - 6.5}L{x + 6.5} {y + 5}L{x - 6.5} {y + 5}Z" />{/if}
							</g>
							{#if i >= pts.length - 2}
								<text {x} y={y - 11} fill="#e6e8ee" font-size="11" font-weight="600" text-anchor="middle">{fmt(p.value)}</text>
							{/if}
						{/each}
						{#if geo.bars.length}
							{@const my = pt + ih + 30}
							{#if geo.bars.some((b) => b.span)}<text x={pl - 7} y={my + 9} fill={C.axis} font-size="10.5" text-anchor="end">{t('h1_meds_row')}</text>{/if}
							{#each geo.bars.filter((b) => b.span) as b, i (i)}
								<!-- A wide bar holds its label; a narrow one prints it after its end, or in the list under the chart. -->
								{@const wide = b.x2 - b.x > b.label.length * 5.6 + 12}
								{@const after = b.x2 + 6 + b.label.length * 5.6 < W - pr}
								<rect x={b.x} y={my + b.row * 14} width={Math.max(4, b.x2 - b.x)} height="12" rx="3" fill={C.accent} opacity=".55"><title>{b.label}{b.ongoing ? ' · ' + t('h1_ongoing') : ''}</title></rect>
								{#if b.ongoing}<path d="M{b.x2 - 1} {my + b.row * 14 + 2}l5 4-5 4z" fill={C.accent} opacity=".8" />{/if}
								{#if wide}<text x={b.x + 6} y={my + b.row * 14 + 9.5} fill="#0c1222" font-size="10" font-weight="600">{b.label}</text>
								{:else if after}<text x={b.x2 + (b.ongoing ? 9 : 4)} y={my + b.row * 14 + 9.5} fill="#8a93a5" font-size="10">{b.label}</text>{/if}
							{/each}
							{#if geo.bars.some((b) => !b.span)}<text x={pl - 7} y={my + 37 + extra} fill={C.axis} font-size="10.5" text-anchor="end">{t('h1_ev_row')}</text>{/if}
							{#each geo.bars.filter((b) => !b.span) as b, i (i)}
								<circle cx={b.x} cy={my + 33 + extra} r="4" fill="#c39bd3"><title>{b.label}</title></circle>
								<!-- left of the dot, unless that would print over the row's own label -->
								{@const left = b.x - pl > b.label.length * 5.6 + 14}
								<text x={left ? b.x - 7 : b.x + 7} y={my + 37 + extra} fill="#8a93a5" font-size="10" text-anchor={left ? 'end' : 'start'}>{b.label}</text>
							{/each}
						{/if}
					</svg>
					</div>
					{#if unlabelled.length}
						<div class="dim" style="font-size:11px;margin:0 0 6px {(pl / W) * 100}%">{t('h1_meds_row')}: {unlabelled.join(' · ')}</div>
					{/if}
				</div>

				<div class="card" style="margin-top:12px;padding:6px 6px 2px;overflow-x:auto">
					<table class="t nw">
						<thead>
							<tr><th>{t('h1_th_date')}</th><th>{t('h1_th_value')}</th><th>{t('h1_th_printed')}</th><th>{t('h1_th_range')}</th><th>{t('h1_th_loinc')}</th><th>{t('h1_th_lab')}</th><th>{t('h1_th_status')}</th><th>{t('h1_th_source')}</th></tr>
						</thead>
						<tbody>
							{#each pts.slice().reverse() as p, i (p.src + "|" + i)}
								<tr>
									<td class="num">{p.date}</td>
									<td class="num" style="font-weight:600;color:{p.status && p.status !== 'ok' ? C[p.status] : 'inherit'}">{fmt(p.value)} <span class="muted" style="font-weight:400">{unitLabel(p.unit)}</span></td>
									<td class="mono muted">{p.printed}</td>
									<td class="num">{rangeOf(p)}</td>
									<td class="mono">{p.loinc ?? '—'}</td>
									<td>{p.lab ?? '—'}</td>
									<td>
										{#if p.status}<StatusChip status={p.status} />
										{:else if p.flag}<span class="flag" title={t('h1_lg_flags')}>{p.flag}</span>
										{:else}<span class="dim">—</span>{/if}
									</td>
									<td><SourceChip label={p.src_label ?? p.src} src={p.src} onopen={openSource} /></td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			</div>

			<div class="grid" style="align-content:start">
				<div class="card">
					<h3>{t('h1_rcv_t')}</h3>
					{#if change !== null}
						<div style="display:flex;gap:16px;align-items:baseline">
							<div>
								<div class="dim" style="font-size:11px">{t('h1_change')}</div>
								<div style="font-size:18px;font-weight:650;white-space:nowrap">{change > 0 ? '+' : change < 0 ? '−' : ''}{fmt(Math.abs(+change.toFixed(4)))} {unitLabel(last.unit)}</div>
								{#if tr.stats.last_vs_previous_pct != null}<div class="muted" style="font-size:11.5px">{pctFmt(tr.stats.last_vs_previous_pct)}</div>{/if}
							</div>
							{#if tr.stats.rcv_pct != null}
								<div>
									<div class="dim" style="font-size:11px">{t('h1_rcv')}</div>
									<div style="font-size:18px;font-weight:650">±{fmtNum(tr.stats.rcv_pct, 1)}%</div>
								</div>
							{/if}
						</div>
					{/if}
					<div class="note" style="margin-top:6px">
						{tr.stats.exceeds_rcv === true ? t('h1_rcv_yes') : tr.stats.exceeds_rcv === false ? t('h1_rcv_no') : tr.stats.method_change && tr.stats.rcv_pct != null ? t('calc_method_change') : tr.stats.rcv_pct == null && tr.stats.n >= 2 ? t('calc_rcv_none') : t('h1_rcv_na')}
					</div>
					{#if tr.stats.cv_i != null && tr.stats.cv_a != null}
						<div class="dim" style="font-size:11.5px;margin-top:6px">
							{tf('calc_cv', { cvi: fmtNum(Number(tr.stats.cv_i)), cva: fmtNum(Number(tr.stats.cv_a)) })}
							{#if tr.stats.rcv_cite}<a href={tr.stats.rcv_cite} target="_blank" rel="noopener noreferrer">{t('calc_cv_src')}</a>{/if}
						</div>
					{/if}
					{#if tr.stats.cv_i != null}
						{@const bl = tr.stats.baseline}
						<div style="margin-top:10px">
							<div class="dim" style="font-size:11px">{t('calc_bl_t')}</div>
							{#if bl}
								<div style="font-size:15px;font-weight:650;white-space:nowrap">{fmt(bl.low)}–{fmt(bl.high)} {unitLabel(last.unit)}</div>
								<div class="muted" style="font-size:11.5px">{tf('calc_bl_range', { n: bl.n })}</div>
								<div class="note" style="margin-top:4px">{bl.outside ? t('calc_bl_out') : t('calc_bl_in')}</div>
							{:else}
								<div class="muted" style="font-size:11.5px">{t('calc_bl_na')}</div>
							{/if}
						</div>
					{/if}
				</div>
				<div class="card">
					<h3>{t('h1_calc')} <span class="r">{t('nodx')}</span></h3>
					{#if calcs.loading && !calcs.data}
						<div class="empty" style="padding:8px 0">{t('loading')}</div>
					{:else if calcs.error}
						<NotReady error={calcs.error} onretry={load} />
					{:else if !calcShown.length}
						<div class="empty" style="padding:8px 0">{t('calc_none')}</div>
					{:else}
						{#each calcShown as c (c.id)}
							{@const it = c.latest}
							<div class="calc">
								<div class="row">
									<b>{it.name}</b>
									{#if it.status === 'ok'}
										<span class="val num" style="white-space:nowrap">{numText(it.value ?? '')}{#if it.unit}{' '}<small class="muted" style="font-size:11px;font-weight:400">{calcUnit(it.unit)}</small>{/if}</span>
									{:else}
										<span class="val dim" style="font-size:12px;font-weight:500;white-space:nowrap">{t('calc_' + it.status)}</span>
									{/if}
								</div>
								{#if it.status === 'ok' && it.date}
									<div class="muted" style="font-size:11.5px"><span class="num">{it.date}</span>{#if c.computed > 1}{' · ' + tp('calc_computed', c.computed)}{/if}</div>
								{/if}
								{#if it.status !== 'ok' && c.latestOk}
									{@const ok = c.latestOk}
									<div class="muted" style="font-size:11.5px">{tf('calc_last_ok', { v: numText(ok.value ?? '') + (ok.unit ? ' ' + calcUnit(ok.unit) : ''), date: ok.date ?? '' })}{#if c.computed > 1}{' · ' + tp('calc_computed', c.computed)}{/if}</div>
								{/if}
								{#if it.text}<div class="note" style="margin-top:2px">{it.text}</div>{/if}
								{#each it.notes as n (n)}<div class="dim" style="font-size:11px">{n}</div>{/each}
								{#if it.inputs.length || it.person}
									<div class="dim calc-in" style="font-size:11px">
										{t('calc_inputs')}:
										{#each it.inputs as i (i.group)}
											{#if i.text}
												<!-- a line that is no lab value: what the record holds, with its source when it holds something -->
												<span class="calc-fact"><span>{i.text}</span>{#if i.src}{' '}<SourceChip label={i.date} name={i.name} src={i.src} onopen={openSource} />{/if}</span>
											{:else}
												<span><span class="calc-nw">{i.name} <span class="num">{numText(i.value)}</span> {calcUnit(i.unit)}</span>{#if i.converted}{' '}<span class="calc-nw">({tf('calc_read_as', { v: numText(i.converted.value) + ' ' + calcUnit(i.converted.unit) })})</span>{/if}{' '}<SourceChip label={i.date} name={i.name} src={i.src} onopen={openSource} /></span>
											{/if}
										{/each}
										{#if it.person?.age != null}<span class="calc-nw">{tf('calc_age', { age: it.person.age })}</span>{/if}
										{#if it.person?.sex}<span class="calc-nw">{t('calc_' + it.person.sex)}</span>{/if}
									</div>
								{/if}
								<div class="dim" style="font-size:11px;overflow-wrap:anywhere">{it.version} · <a href={it.cite} target="_blank" rel="noopener noreferrer">{it.cite}</a></div>
							</div>
						{/each}
						{#if profile && profile.role !== 'viewer' && calcShown.some((c) => c.latest.reason?.groups?.includes('smoking_status') || c.latest.reason_code === 'smoking_status_not_coded')}
							{#if smoking}
								{#key profile.slug}<ManualEntry profile={profile.slug} kinds={['smoking']} onsaved={() => ((smoking = false), load())} oncancel={() => (smoking = false)} />{/key}
							{:else}
								<button class="btn sm" style="margin-top:6px" onclick={() => (smoking = true)}>{t('calc_smk_add')}</button>
							{/if}
						{/if}
						<div class="note" style="margin-top:4px">{t('calc_note')}</div>
					{/if}
				</div>
				{#if related.length || calcRows.length}
					<div class="card">
						<h3>{t('h1_related')}</h3>
						{#each related as m (m.alias_group)}
							<div style="display:flex;align-items:center;gap:8px;padding:4px 0;border-top:1px solid #20252f">
								<a style="flex:1;color:inherit" href="/biomarkers?code={encodeURIComponent(m.alias_group)}">{m.name}</a>
								<span class="num" style="color:{m.status === 'ok' ? 'inherit' : C[m.status]};font-weight:600">{numText(m.value)} {unitLabel(m.unit)}<span class="sr-only"> · {t(m.status)}</span></span>
								{#if m.spark.length}<Sparkline values={m.spark} status={m.status} w={60} h={20} />{/if}
							</div>
						{/each}
						{#each calcRows as c (c.id)}
							<div style="display:flex;align-items:center;gap:8px;padding:4px 0;border-top:1px solid #20252f">
								<span style="flex:1" title={c.latestOk.name}>{c.latestOk.short}</span>
								<span class="num" style="font-weight:600;white-space:nowrap">{numText(c.latestOk.value ?? '')} {calcUnit(c.latestOk.unit)}</span>
								<span class="dim" style="font-size:11px;white-space:nowrap">calc · {c.latestOk.date}</span>
							</div>
						{/each}
					</div>
				{/if}
				{#if tr.limitations.length}
					<div class="card">
						<h3>{t('h1_limits')}</h3>
						{#each tr.limitations as l (l)}<div class="note" style="margin-bottom:4px">{l}</div>{/each}
					</div>
				{/if}
			</div>
		</div>
	{/if}
{/if}

{#if tip && pts[tip.i]}
	{@const p = pts[tip.i]}
	<Tip x={tip.x} y={tip.y}>
		<b>{fmt(p.value)} {unitLabel(p.unit)}</b>{#if p.status}&nbsp;·&nbsp;<StatusChip status={p.status} />{/if}<br />
		<span class="mono">{p.printed}</span><br />
		<span class="num" style="white-space:nowrap">{p.date}</span> · {p.lab ?? '—'}{p.method ? ' · ' + methodLabel(p.method) : ''}<br />
		<span class="dim">{t('h1_printed_range')} {rangeOf(p)}{p.loinc ? ' · LOINC ' + p.loinc : ''}</span><br />
		<span style="color:#6ea8fe;white-space:nowrap">{tf('h1_open_page', { p: sourceLabel(p.src_label ?? p.src, t('src_manual')) })}</span>
	</Tip>
{/if}

<SourcePanel />

<style>
	/* The printed name repeats the name on a phone: the list keeps what fits. */
	/* On a phone the status must show: the name wraps onto two lines instead of taking the width. */
	.calc-in {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 2px 6px;
	}
	.calc-nw {
		white-space: nowrap;
	}
	/* fact lines read as separate items, not one sentence */
	.calc-fact::before {
		content: '·';
		margin-right: 4px;
		opacity: 0.6;
	}
	@media (max-width: 760px) {
		.ph-hide {
			display: none;
		}
		.bml td:first-child {
			white-space: normal;
			min-width: 7em;
		}
		.bml td:nth-child(2) {
			white-space: normal;
		}
	}
</style>
