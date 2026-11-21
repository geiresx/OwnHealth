// Vendor scores (migration 1220, GET /profiles/{p}/vendor-scores): readiness, Body Battery and the like
// are the vendor's own numbers. The Wearables page shows them in their own card, one row per provider
// and category, and never next to the measurements.

export interface VendorScore {
	provider: string;
	category: string;
	t: string;
	value: number;
	qualifier: string | null;
	components: Record<string, unknown> | null;
	computed_by: 'provider';
	/** The connection that brought the score; null once that connection is removed. */
	source: { id: string; label: string } | null;
}

export interface VendorScores {
	days: number;
	scores: VendorScore[];
}

/** The server's categories (ownhealth-devices scores::CATEGORIES), in the order the card lists them. */
export const CATEGORIES = ['readiness', 'recovery', 'sleep', 'activity', 'stress', 'strain', 'body_battery'] as const;

export interface ScoreRow {
	provider: string;
	category: string;
	source: VendorScore['source'];
	latest: VendorScore;
	/** The window's values, oldest first, for the sparkline. */
	values: number[];
}

/** One row per provider, category and connection: categories in CATEGORIES order (unknown ones last),
 * then provider, then connection. Two connections that bring the same vendor's score keep a row each. */
export function scoreRows(scores: VendorScore[]): ScoreRow[] {
	const groups = new Map<string, VendorScore[]>();
	for (const s of scores) {
		const key = `${s.category}\u0000${s.provider}\u0000${s.source?.id ?? ''}`;
		groups.set(key, [...(groups.get(key) ?? []), s]);
	}
	const rank = (c: string) => {
		const i = (CATEGORIES as readonly string[]).indexOf(c);
		return i < 0 ? CATEGORIES.length : i;
	};
	return [...groups.values()]
		.map((g) => {
			const sorted = [...g].sort((a, b) => Date.parse(a.t) - Date.parse(b.t));
			return { provider: g[0].provider, category: g[0].category, source: g[0].source, latest: sorted[sorted.length - 1], values: sorted.map((s) => s.value) };
		})
		.sort(
			(a, b) =>
				rank(a.category) - rank(b.category) ||
				a.category.localeCompare(b.category) ||
				a.provider.localeCompare(b.provider) ||
				(a.source?.label ?? '\uffff').localeCompare(b.source?.label ?? '\uffff')
		);
}

// Brand names are never translated.
const BRANDS: Record<string, string> = {
	oura: 'Oura',
	garmin: 'Garmin',
	whoop: 'WHOOP',
	polar: 'Polar',
	withings: 'Withings',
	fitbit: 'Fitbit',
	samsung: 'Samsung',
	coros: 'COROS',
	suunto: 'Suunto'
};

/** The vendor's name as it writes it; an unknown key ('ultrahuman') gets a capital letter. */
export function providerLabel(p: string): string {
	return BRANDS[p] ?? p.split('_').map((w) => w.charAt(0).toUpperCase() + w.slice(1)).join(' ');
}

/** Scores are shown as the vendor sends them: whole numbers as whole, others with one decimal. */
export function scoreDecimals(v: number): number {
	return Number.isInteger(v) ? 0 : 1;
}

/** A score's day, as the card prints it next to the vendor: in UTC, like the measurement charts of the
 * same page, so a score keyed at midnight UTC keeps its day in a browser west of UTC. */
export function fmtScoreDay(iso: string, lang: string): string {
	return new Intl.DateTimeFormat(lang === 'es' ? 'es-ES' : 'en-GB', { day: 'numeric', month: 'short', timeZone: 'UTC' }).format(new Date(iso));
}
