<script lang="ts">
	// Timeline (mockup screen #timeline): dated events from GET /profiles/{p}/timeline (lane H), newest first,
	// grouped by year, filterable by type and country, each with its source chip. Above the list, the bars lane
	// (slice 6): one row per condition and per medicine, a segment per course; phones get it as a list.
	import { documentFileUrl, getTimeline } from '#lib/api';
	import type { Timeline, TimelineEvent, TimelineSegment, TimelineSpan } from '#lib/api/types';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import SourcePanel from '#lib/components/pages1/SourcePanel.svelte';
	import { openSource } from '#lib/components/pages1/source.svelte';
	import { Loader, tf } from '#lib/components/pages1/util.svelte';
	import PhaseChip from '#lib/components/PhaseChip.svelte';
	import SourceChip from '#lib/components/SourceChip.svelte';
	import RecordsCard from '#lib/components/RecordsCard.svelte';
	import { i18n, t } from '#lib/i18n/index.svelte';
	import { currentProfile } from '#lib/state/app.svelte';
	import { sourceLabel } from '#lib/logic/format';
	import { buildBars, type Bar } from '#lib/logic/axis';
	import { todayIso } from '#lib/logic/people';

	type Ty = TimelineEvent['type'];
	// Colours and glyphs from the mockup's TYPES table (procedure and document added in slice 6).
	const TYPES: [Ty, string, string][] = [
		['visit', '#6ea8fe', '<path d="M12 4v16M4 12h16"/>'],
		['lab', '#3fb68b', '<path d="M9 3v7l-4 9h14l-4-9V3"/>'],
		['imaging', '#c39bd3', '<circle cx="12" cy="12" r="6"/>'],
		['diagnosis', '#ef6b73', '<path d="M12 6v7M12 17v1"/>'],
		['medication', '#e0a84a', '<path d="M6 12h12"/>'],
		['procedure', '#e88fb4', '<path d="m7 7 10 10M7 17 17 7"/>'],
		['vaccine', '#7fd1d9', '<path d="m6 18 12-12"/>'],
		['document', '#9aa3b2', '<path d="M7 3h7l4 4v14H7z"/>'],
		['genome', '#b7c38c', '<path d="M8 4c0 8 8 8 8 16M16 4c0 8-8 8-8 16"/>']
	];
	const TY = Object.fromEntries(TYPES.map(([k, c, g]) => [k, { c, g }])) as Record<Ty, { c: string; g: string }>;
	const tyName = (k: string) => (k === 'procedure' || k === 'document' ? t('tl_ty_' + k) : t('h1_ty_' + k));

	const profile = $derived(currentProfile());
	const tl = new Loader<Timeline>();
	let types = $state(new Set<Ty>());
	let countries = $state(new Set<string>());

	function load() {
		const slug = profile?.slug;
		const lang = i18n.lang;
		if (slug) tl.run(() => getTimeline(slug, { lang }), slug);
	}
	$effect(load);

	const events = $derived(tl.data?.events ?? []);
	const countryList = $derived([...new Set(events.map((e) => e.country).filter((c): c is string => !!c))].sort());
	const shown = $derived(events.filter((e) => (!types.size || types.has(e.type)) && (!countries.size || (e.country && countries.has(e.country)))));
	const count = (k: Ty) => events.filter((e) => e.type === k).length;
	// Sources card: one row per provider (or source label when no provider is recorded).
	const sources = $derived.by(() => {
		const m = new Map<string, { country: string | null; n: number }>();
		for (const e of events) {
			const key = e.provider ? sourceLabel(e.provider, t('src_manual')) : e.src_label ? sourceLabel(e.src_label.split(' · ')[0], t('src_manual')) : t('h1_unknown_src');
			const r = m.get(key) ?? { country: e.country, n: 0 };
			r.n++;
			m.set(key, r);
		}
		return [...m.entries()].sort((a, b) => b[1].n - a[1].n);
	});
	const years = $derived.by(() => {
		const out: { year: string; items: TimelineEvent[] }[] = [];
		for (const e of shown) {
			const y = e.date.slice(0, 4);
			if (out.at(-1)?.year !== y) out.push({ year: y, items: [] });
			out.at(-1)!.items.push(e);
		}
		return out;
	});
	function toggle<T>(set: Set<T>, k: T): Set<T> {
		const s = new Set(set);
		if (s.has(k)) s.delete(k);
		else s.add(k);
		return s;
	}

	// ---- the bars lane: x in percent of the time from Jan 1 of the first year to today ----
	const today = todayIso();
	const spans = $derived((tl.data?.spans ?? []).filter((s) => !types.size || types.has(s.type)));
	const lane = $derived.by(() => {
		const dates = spans.flatMap((s) => s.segments.map((g) => g.date));
		if (!dates.length) return null;
		const y0 = Number(dates.reduce((a, b) => (a < b ? a : b)).slice(0, 4));
		const t0 = Date.parse(`${y0}-01-01`);
		const t1 = Date.parse(today);
		const x = (d: string) => Math.min(100, Math.max(0, ((Date.parse(d) - t0) / Math.max(1, t1 - t0)) * 100));
		const y1 = Number(today.slice(0, 4));
		const step = y1 - y0 > 12 ? 5 : y1 - y0 > 6 ? 2 : 1;
		const ticks: { year: number; x: number }[] = [];
		for (let y = y0; y <= y1; y += step) ticks.push({ year: y, x: x(`${y}-01-01`) });
		// one row per span; buildBars draws its open-ended courses and one-day dots
		const rows = spans.map((s) => ({ span: s, bars: buildBars(s.segments, x, today).map((b, i) => ({ b, g: s.segments[i] })) }));
		return { ticks, rows };
	});
	let pick = $state<{ span: TimelineSpan; g: TimelineSegment } | null>(null);
	// the detail follows the bars: another profile or a type filter that hides its row clears it, so removing
	// the filter later does not bring the old detail back
	const tip = $derived(pick && spans.includes(pick.span) ? pick : null);
	$effect(() => {
		if (pick && !tip) pick = null;
	});
	const range = (g: TimelineSegment) => (g.end === g.date ? g.date : `${g.date} – ${g.end ?? t('tl_now')}`);
	// two courses of one medicine at once (two strengths) stack inside its row
	const segStyle = (b: Bar, c: string) =>
		(b.span ? `left:${b.x}%;width:${Math.max(b.x2 - b.x, 0.8)}%;` : `left:calc(${b.x}% - 5px);`) + `top:${6 + b.row * 12}px;--c:${c}`;
	const trackHeight = (bars: { b: Bar }[]) => 22 + Math.max(0, ...bars.map(({ b }) => b.row)) * 12;
</script>

<div class="ph">
	<div>
		<h1>{t('title_tl')}</h1>
		{#if tl.data}<div class="sub">{tf('h1_tl_sub', { name: profile?.display_name ?? '', n: events.length, s: sources.length })}</div>{/if}
	</div>
</div>

{#if tl.loading && !tl.data}
	<div class="card"><div class="empty">{t('loading')}</div></div>
{:else if tl.error}
	<div class="card"><NotReady error={tl.error} onretry={load} /></div>
{:else if tl.data}
	<div class="filters">
		<span class="lab">{t('h1_type')}</span>
		{#each TYPES as [k, c] (k)}
			<button class="chip" class:on={types.has(k)} aria-pressed={types.has(k)} onclick={() => (types = toggle(types, k))}>
				<i style="width:8px;height:8px;border-radius:50%;background:{c};display:inline-block"></i>{tyName(k)} <span class="dim">{count(k)}</span>
			</button>
		{/each}
		{#if countryList.length}
			<span class="lab" style="margin-left:14px">{t('h1_country')}</span>
			{#each countryList as c (c)}
				<button class="chip" class:on={countries.has(c)} aria-pressed={countries.has(c)} onclick={() => (countries = toggle(countries, c))}>{c}</button>
			{/each}
		{/if}
		{#if types.size || countries.size}
			<button class="chip" onclick={() => ((types = new Set()), (countries = new Set()))}>{t('h1_clear')}</button>
		{/if}
	</div>
	{#if lane && countries.size}
		<div class="note" style="margin-bottom:12px">{t('tl_bars_country')}</div>
	{:else if lane}
		<div class="card lane">
			<h3>{t('tl_bars')}</h3>
			<div class="lgrid" role="group" aria-label={t('tl_bars')}>
				<div class="lrow axis" aria-hidden="true">
					<div></div>
					<div class="track">{#each lane.ticks as k (k.year)}<span class="tick num" style="left:{k.x}%">{k.year}</span>{/each}</div>
				</div>
				{#each lane.rows as r, ri (r.span.type + r.span.label + ri)}
					<div class="lrow">
						<div class="lname" title={r.span.label}><i style="background:{TY[r.span.type].c}"></i>{r.span.label}</div>
						<div class="track" style="height:{trackHeight(r.bars)}px">
							{#each lane.ticks as k (k.year)}<span class="gl" style="left:{k.x}%"></span>{/each}
							{#each r.bars as { b, g }, i (i)}
								<button
									type="button"
									class="seg"
									class:dot={!b.span}
									class:open={b.ongoing}
									class:sel={tip?.g === g}
									style={segStyle(b, TY[r.span.type].c)}
									title={`${g.label} · ${range(g)}`}
									aria-label={`${g.label}: ${range(g)}`}
									onmouseenter={() => (pick = { span: r.span, g })}
									onfocus={() => (pick = { span: r.span, g })}
									onclick={() => (pick = { span: r.span, g })}
								></button>
							{/each}
						</div>
					</div>
				{/each}
			</div>
			<div class="ltip" aria-live="polite">
				{#if tip}
					<i style="background:{TY[tip.span.type].c}"></i><b>{tip.g.label}</b>
					<span class="num muted">{range(tip.g)}</span>
					{#if tip.g.end == null}<span class="chip">{t('tl_ongoing')}</span>{/if}
					{#if tip.g.src}<SourceChip label={tip.g.src_label ?? tip.g.src} name={tip.g.label} src={tip.g.src} onopen={openSource} />{/if}
				{:else}
					<span class="dim">{t('tl_bars_hint')}</span>
				{/if}
			</div>
			<ul class="llist">
				{#each spans as s, si (s.type + s.label + si)}
					<li>
						<div><i style="background:{TY[s.type].c}"></i><b>{s.label}</b> <span class="dim">{tyName(s.type)}</span></div>
						{#each s.segments as g, gi (gi)}
							<div class="lseg">
								<span class="num">{range(g)}</span>
								{#if g.src}<SourceChip label={g.src_label ?? g.src} name={g.label} src={g.src} onopen={openSource} />{/if}
							</div>
						{/each}
					</li>
				{/each}
			</ul>
		</div>
	{/if}
	<div class="tl">
		<div>
			{#each years as y (y.year)}
				<div class="yr">{y.year}</div>
				{#each y.items as e, i (e.type + e.date + e.title + i)}
					<div class="ev">
						<div class="dt num">{e.date.slice(5)}</div>
						<div class="rail">
							<div class="dot" style="background:{TY[e.type]?.c ?? '#9aa3b2'}"><svg viewBox="0 0 24 24" aria-hidden="true">{@html TY[e.type]?.g ?? ''}</svg></div>
						</div>
						<div class="body">
							<div>
								<div class="t1">{e.title}</div>
								{#if e.detail}<div class="t2">{e.detail}</div>{/if}
								{#if e.document_id}<a class="t2 fname" href={documentFileUrl(e.document_id)} target="_blank" rel="noopener" title={e.filename?.text}>{e.filename?.text ?? t('i2_open_doc')}</a>
								{:else if e.filename}<div class="t2 fname" title={e.filename.text}>{e.filename.text}</div>{/if}
							</div>
							<div class="rt">
								<span class="chip">{tyName(e.type)}</span>
								{#each e.flags as f (f)}<span class="flag">{f}</span>{/each}
								{#if e.country}<span class="flag">{e.country}</span>{/if}
								{#if e.src}<SourceChip label={e.src_label ?? e.src} src={e.src} onopen={openSource} />{/if}
							</div>
						</div>
					</div>
				{/each}
			{:else}
				<div class="muted" style="padding:30px">{events.length ? t('h1_nomatch') : t('h1_tl_none')}</div>
			{/each}
			{#if tl.data.withheld.by_scope.length}
				<div class="note" style="margin:10px 0 0 74px">{tf('h1_withheld', { list: tl.data.withheld.by_scope.map(tyName).join(', ') })}</div>
			{/if}
		</div>
		<div class="grid" style="align-content:start">
			<div class="card">
				<h3>{t('h1_sources')}</h3>
				{#if sources.length}
					<table class="t">
						<tbody>
							{#each sources as [name, s] (name)}
								<tr><td>{name}</td><td>{#if s.country}<span class="flag">{s.country}</span>{/if}</td><td class="num" style="text-align:right">{s.n}</td></tr>
							{/each}
						</tbody>
					</table>
				{:else}
					<div class="dim" style="font-size:12px">{t('h1_tl_none')}</div>
				{/if}
			</div>
			{#if profile}
				{#key profile.slug}<RecordsCard profile={profile.slug} canWrite={profile.role !== 'viewer'} onchange={load} />{/key}
			{/if}
			<div class="card">
				<h3>{t('h1_gaps')} <span class="r"><PhaseChip phase="v0.3" /></span></h3>
				<div class="dim" style="font-size:12px">{t('h1_gaps_d')}</div>
			</div>
		</div>
	</div>
{/if}

<SourcePanel />

<style>
	.lane {
		margin-bottom: 12px;
	}
	.lrow {
		display: grid;
		grid-template-columns: 170px minmax(0, 1fr);
		gap: 10px;
		align-items: center;
		min-height: 22px;
	}
	.lrow.axis {
		min-height: 18px;
	}
	.lname {
		font-size: 12px;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.lname i,
	.ltip i,
	.llist i {
		display: inline-block;
		width: 8px;
		height: 8px;
		border-radius: 50%;
		margin-right: 6px;
	}
	.track {
		position: relative;
		height: 22px;
	}
	.axis .track {
		height: 18px;
		border-bottom: 1px solid var(--line);
	}
	.tick {
		position: absolute;
		top: 0;
		font-size: 10.5px;
		color: var(--dim);
		transform: translateX(-50%);
	}
	.tick:first-child {
		transform: none;
	}
	/* the year lines; not `.grid`, which this page also uses for the side column */
	.gl {
		position: absolute;
		top: 0;
		bottom: 0;
		width: 1px;
		background: var(--line);
		opacity: 0.5;
	}
	.seg {
		position: absolute;
		top: 6px;
		height: 10px;
		padding: 0;
		border: 0;
		border-radius: 5px;
		background: var(--c);
		cursor: pointer;
		opacity: 0.85;
	}
	/* still running: the bar fades out at today instead of ending */
	.seg.open {
		border-radius: 5px 0 0 5px;
		background: linear-gradient(90deg, var(--c) 80%, transparent);
	}
	.seg.dot {
		width: 10px;
		border-radius: 50%;
	}
	.seg:hover,
	.seg.sel {
		opacity: 1;
		outline: 2px solid var(--text);
		outline-offset: 1px;
	}
	.seg:focus-visible {
		opacity: 1;
		outline: 2px solid var(--accent);
		outline-offset: 2px;
	}
	/* not `.tip`: app.css uses it for the floating chart tooltip */
	.ltip {
		display: flex;
		gap: 8px;
		align-items: center;
		flex-wrap: wrap;
		min-height: 26px;
		margin-top: 8px;
		padding-top: 6px;
		border-top: 1px solid var(--line);
		font-size: 12px;
	}
	.llist {
		display: none;
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.llist li {
		padding: 6px 0;
		border-top: 1px solid var(--line);
		font-size: 12.5px;
	}
	.llist li:first-child {
		border-top: 0;
	}
	.lseg {
		display: flex;
		gap: 8px;
		align-items: center;
		margin: 3px 0 0 14px;
		font-size: 12px;
		color: var(--muted);
	}
	.fname {
		overflow-wrap: anywhere;
	}
	@media (max-width: 760px) {
		.lgrid,
		.ltip {
			display: none;
		}
		.llist {
			display: block;
		}
	}
</style>
