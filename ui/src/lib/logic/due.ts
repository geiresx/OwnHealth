// The Due screen and the dashboard's Due card: screenings, vaccines and lab gaps from the prevention
// document (GET /profiles/{p}/due, and the dashboard's `prevention_due`). The server computes every
// status and date; these helpers only merge, count and pick a colour.

export type DueStatus = 'overdue' | 'due' | 'upcoming' | 'up_to_date' | 'not_assessed';

/** One item of ownhealth.prevention/v1 as the server renders it in one language. */
export interface DueItem {
	key: string;
	rule: string;
	area: 'screening' | 'vaccine' | 'lab';
	kind: string;
	status: DueStatus;
	due?: string;
	last?: { date: string; src?: string | null };
	source?: string;
	cite?: string | null;
	src: string[];
	group?: string;
	labs?: boolean;
	title: string | null;
	why: string | null;
	reason?: string | null;
	reason_code?: string;
	/** A guideline row: the record kind and the name that make a new record count for it. */
	record?: { kind: 'immunization' | 'procedure'; text: string | null };
	/** not_assessed `no_answer`: the answer key the row depends on. */
	answer?: string;
	/** What the person decided about this item (POST /profiles/{p}/due/{key}/decision). */
	decision?: DueDecision;
}

/** A person's decision about one item; `reason` is their own words (shown, never hashed). */
export interface DueDecision {
	kind: 'snooze' | 'dismiss' | 'done';
	until?: string;
	reason?: string;
	evidence?: { table: 'immunization' | 'procedure' | 'encounter' | 'document'; id: string };
}

/** A decision as the screen sends it. */
export type DueDecisionIn =
	| { kind: 'snooze'; until: string; reason?: string }
	| { kind: 'dismiss'; reason: string }
	| { kind: 'done'; evidence: { table: 'immunization' | 'procedure' | 'encounter' | 'document'; id: string }; reason?: string };

/** One question some guideline rows need (GET /profiles/{p}/prevention-answers). */
export interface DueAnswer {
	key: string;
	question: string | null;
	value: 'yes' | 'no' | null;
	src: string | null;
	recorded_at: string | null;
}

export interface DueDoc {
	schema: string;
	profile: string;
	as_of: string;
	rules?: string;
	country?: string;
	reason_code?: string;
	reason?: string | null;
	disclaimer?: string | null;
	items: DueItem[];
	/** Items the person's decisions hide (snoozed, dismissed, done); absent when none. */
	hidden?: DueItem[];
	unmatched?: { name: string; src: string }[];
	withheld: number;
	/** The scopes the withheld items need. */
	withheld_scopes?: string[];
	/** Absent for a caller without read:records. */
	prevention_hash?: string;
	lang: string;
}

/**
 * The scopes an empty Due card lacks, as one string ("read:labs, read:records"), or null when the
 * card has rows or nothing is withheld: an empty card that withholds items must not read as nothing due.
 */
export function lockedScopes(rows: CardRow[], withheld: string[] | undefined): string | null {
	return !rows.length && withheld?.length ? withheld.join(', ') : null;
}

/** The scopes a Due card with rows still leaves items out for, or null: a partial card must say so. */
export function partialScopes(rows: CardRow[], withheld: string[] | undefined): string | null {
	return rows.length && withheld?.length ? withheld.join(', ') : null;
}

/** The Due screen lists prevention items only, which need read:records: without it the card links nowhere. */
export function dueScreenOpen(withheld: string[] | undefined): boolean {
	return !withheld?.includes('read:records');
}

/**
 * What the dashboard's Due card lists: the first `max` rows, and for the rest a link to the Due
 * screen when it is open to the caller, or else a control that reveals them on the card itself.
 */
export function dueCardView<R>(rows: R[], max: number, linked: boolean, expanded: boolean): { rows: R[]; more: number; action: 'link' | 'expand' | 'collapse' | null } {
	if (rows.length <= max) return { rows, more: 0, action: null };
	if (linked) return { rows: rows.slice(0, max), more: rows.length - max, action: 'link' };
	return expanded ? { rows, more: 0, action: 'collapse' } : { rows: rows.slice(0, max), more: rows.length - max, action: 'expand' };
}

/** A row of the dashboard's older `due` list (lab gaps of the rule pack, grouped by rule). */
export interface GlanceDue {
	title: string;
	why: string;
	status: string;
	kind: string;
	phase: string | null;
	rule?: string;
}

const OPEN: DueStatus[] = ['overdue', 'due', 'upcoming'];
export const isOpen = (i: { status: string }) => (OPEN as string[]).includes(i.status);

/** An open item whose snooze has run out: the screen lists it under "Shown again" until the person acts. */
export const shownAgain = (i: DueItem) => i.decision?.kind === 'snooze' && isOpen(i);

/** The Due screen's sections, in order; an item is in exactly one. */
export function dueGroups(items: DueItem[]): { id: 'again' | 'open' | 'done' | 'na'; rows: DueItem[] }[] {
	return [
		{ id: 'again', rows: items.filter(shownAgain) },
		{ id: 'open', rows: items.filter((i) => isOpen(i) && !shownAgain(i)) },
		{ id: 'done', rows: items.filter((i) => i.status === 'up_to_date') },
		{ id: 'na', rows: items.filter((i) => i.status === 'not_assessed') }
	];
}

/**
 * The label of a document in the Done evidence list: its file name (untrusted text from the
 * document, shown as plain text), else its type, else the start of its id, and its day.
 */
export function docLabel(d: { id: string; filename?: { text: string } | string | null; doc_type?: string | null; collected_at?: string | null }): string {
	const name = typeof d.filename === 'string' ? d.filename : d.filename?.text;
	return [name || d.doc_type || d.id.slice(0, 8), d.collected_at?.slice(0, 10)].filter(Boolean).join(' · ');
}

/**
 * The day (YYYY-MM-DD) a record or document proves something was done, or null when it has none or
 * it is after `today`: the server refuses both as `done` evidence.
 */
export function evidenceDay(v: unknown, today: string): string | null {
	const day = typeof v === 'string' ? v.slice(0, 10) : '';
	return day && day <= today ? day : null;
}

/**
 * The last day the server takes as `done` evidence: its own today, on the UTC calendar. East of UTC
 * the browser's local day can already be tomorrow there, and a record of that day would be refused.
 */
export function evidenceLimit(now = new Date()): string {
	return now.toISOString().slice(0, 10);
}

/** The day after `iso` (YYYY-MM-DD), the earliest a new snooze may end on the local calendar. */
export function addDays(iso: string, n: number): string {
	const d = new Date(`${iso}T12:00:00Z`);
	d.setUTCDate(d.getUTCDate() + n);
	return d.toISOString().slice(0, 10);
}

/** The status colour key (colors.ts C) of a due status. */
export function dueColor(s: string): 'out' | 'watch' | 'ok' | 'nodata' | 'stale' {
	switch (s) {
		case 'overdue':
			return 'out';
		case 'due':
			return 'watch';
		case 'upcoming':
			return 'stale';
		case 'up_to_date':
			return 'ok';
		default:
			return 'nodata';
	}
}

/** A row of the Due card; `color` is a key of colors.ts C and `word` the dict key of its status. */
export type CardRow = { key: string; title: string; why: string; color: string; word: string; area: string; phase: string | null };

/**
 * The Due card: the open prevention items, then each older `due` row whose rule no prevention item
 * has (a token without read:records gets no prevention items, so it keeps the older list). Nothing
 * shows twice: a lab gap the prevention list holds is shown from there only.
 */
export function dueCard(glance: GlanceDue[], prevention: DueItem[] | undefined): CardRow[] {
	const items = prevention ?? [];
	const rules = new Set(items.map((i) => i.rule));
	const rows: CardRow[] = items.filter(isOpen).map((i) => ({
		key: i.key,
		title: i.title ?? i.key,
		why: i.why ?? '',
		color: dueColor(i.status),
		word: `due_st_${i.status}`,
		area: i.area,
		phase: null
	}));
	for (const g of glance) {
		if (g.rule && rules.has(g.rule)) continue;
		rows.push({ key: `glance:${g.rule ?? g.title}`, title: g.title, why: g.why, color: g.status, word: g.status, area: g.kind, phase: g.phase });
	}
	return rows;
}
