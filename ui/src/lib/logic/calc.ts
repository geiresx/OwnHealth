// Calculators on the biomarker page: which results a page lists, and the newest one of each.
import type { CalcItem } from '../api/types';
import { unitLabel } from './format.ts';

export interface CalcSummary {
	id: string;
	/** the newest result: the latest draw, or the one item that says why there is none */
	latest: CalcItem;
	/** the newest result with a value, which may be older than `latest` when the newest draw was refused */
	latestOk?: CalcItem;
	/** how many draws gave a value */
	computed: number;
}

/** One summary per calculator whose item matches, in the server's order. Items of one calculator come oldest first. */
export function summarize(items: CalcItem[], match: (i: CalcItem) => boolean): CalcSummary[] {
	const out: CalcSummary[] = [];
	for (const it of items) {
		if (!match(it)) continue;
		const s = out.find((x) => x.id === it.id);
		const ok = it.status === 'ok';
		if (!s) out.push({ id: it.id, latest: it, latestOk: ok ? it : undefined, computed: ok ? 1 : 0 });
		else {
			s.latest = it;
			if (ok) {
				s.latestOk = it;
				s.computed++;
			}
		}
	}
	return out;
}

/** The calculators listed on one marker's page. */
export const forGroup = (items: CalcItem[], group: string) => summarize(items, (i) => i.shows_on.includes(group));

/** The computed rows a dashboard card shows beside its markers (display only, no status word): each
 *  calculator's newest value, even when a later draw was refused. */
export const rowsForCard = (items: CalcItem[], card: string) =>
	summarize(items, (i) => i.system === card).filter((s): s is CalcSummary & { latestOk: CalcItem } => !!s.latestOk);

/** A unit for people: 'mL/min/{1.73_m2}' reads 'mL/min/1.73 m²', 'u[IU]/mL' reads 'µU/mL' (Matthews' unit), 'mm[Hg]' reads 'mmHg',
 *  and UCUM's micro prefix 'u' reads 'µ' ('umol/L' reads 'µmol/L'). */
export const calcUnit = (u: string | null | undefined) =>
	unitLabel(u)
		.replace('{1.73_m²}', '1.73 m²')
		.replace('{1.73_m2}', '1.73 m²')
		.replace('u[IU]/mL', 'µU/mL')
		.replace('m[IU]/L', 'mU/L')
		.replace('mm[Hg]', 'mmHg')
		.replace(/^u(?=mol\/|g\/)/, 'µ');
