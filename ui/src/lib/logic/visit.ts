// The period a visit form sends (slice 6). The form has only a day. A new visit, or a correction that
// moves the day, is placed at noon UTC, so it stays on that day in every time zone. A correction that
// keeps the day keeps the stored start, and an end that would fall before a moved start is cleared.

export type Period = { period_start?: string; period_end?: string | null };

export function visitPeriod(day: string | null | undefined, stored?: Record<string, unknown>): Period {
	if (!day) return {};
	const start = stored?.period_start;
	if (typeof start === 'string' && start.slice(0, 10) === day) return { period_start: start };
	const out: Period = { period_start: `${day}T12:00:00Z` };
	const end = stored?.period_end;
	if (typeof end === 'string' && Date.parse(end) < Date.parse(out.period_start!)) out.period_end = null;
	return out;
}
