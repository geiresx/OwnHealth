// The timeline's bars lane (slice 6). Its axis ends today, so a course that starts later has no bar:
// drawn, it would sit at today's edge as if it ran now. The phone list still shows it with its date.
import { buildBars, type Annotation, type Bar } from './axis.ts';

export function laneBars<G extends Annotation>(segments: G[], x: (d: string) => number, today: string): { b: Bar; g: G }[] {
	const started = segments.filter((g) => g.date <= today);
	return buildBars(started, x, today).map((b, i) => ({ b, g: started[i] }));
}

/** The whole lane: x in percent of the time from Jan 1 of the first year drawn to today, the year ticks and
 * one row per span with a course started by today. The axis is built from the drawn courses only, so a
 * profile whose courses all start later gets no lane (null), as one with no spans. */
export function buildLane<S extends { segments: Annotation[] }>(spans: S[], today: string) {
	const dates = spans.flatMap((s) => s.segments.map((g) => g.date)).filter((d) => d <= today);
	if (!dates.length) return null;
	const y0 = Number(dates.reduce((a, b) => (a < b ? a : b)).slice(0, 4));
	const t0 = Date.parse(`${y0}-01-01`);
	const t1 = Date.parse(today);
	const x = (d: string) => Math.min(100, Math.max(0, ((Date.parse(d) - t0) / Math.max(1, t1 - t0)) * 100));
	const y1 = Number(today.slice(0, 4));
	const step = y1 - y0 > 12 ? 5 : y1 - y0 > 6 ? 2 : 1;
	const ticks: { year: number; x: number }[] = [];
	for (let y = y0; y <= y1; y += step) ticks.push({ year: y, x: x(`${y}-01-01`) });
	const rows = spans.map((s) => ({ span: s, bars: laneBars(s.segments as S['segments'][number][], x, today) })).filter((r) => r.bars.length);
	return { ticks, rows };
}
