// The Medications screen: which medicine rows are current, the history of each medicine, and its code chips.

/** The parts of a medication RecordOut these helpers read. */
export interface MedRecord {
	id: string;
	title: string;
	detail: Record<string, unknown>;
	status: string;
	status_reason: string | null;
	supersedes: string | null;
	recorded_at: string;
}

const str = (v: unknown): string | null => (typeof v === 'string' && v.trim() ? v : null);

/**
 * Taken on `today` (YYYY-MM-DD), as the server's active_medications counts it: med_status active within
 * its dates, or a stopped course whose dates hold today. Only the record's current version counts.
 */
export function isCurrent(r: MedRecord, today: string): boolean {
	if (r.status !== 'current' && r.status !== 'provisional') return false;
	const started = str(r.detail.started);
	const stopped = str(r.detail.stopped);
	const status = str(r.detail.med_status) ?? 'active';
	const startedOk = !started || started <= today;
	const stoppedOk = !stopped || stopped > today;
	return (status === 'active' && startedOk && stoppedOk) || (status !== 'on_hold' && !!started && !!stopped && startedOk && stoppedOk);
}

export interface MedGroup<T extends MedRecord = MedRecord> {
	key: string;
	title: string;
	current: boolean;
	/**
	 * Why a medicine not taken today is not current, from its newest live version: stopped, on hold, starts
	 * later, or retracted when no version is live. Null when current, or when every version was corrected
	 * into another medicine.
	 */
	state: 'stopped' | 'on_hold' | 'later' | 'retracted' | null;
	/** Newest course first; corrected and retracted versions sit with their medicine. */
	entries: T[];
}

/** Every version of every medicine, one group per ingredient (else name): current medicines first. */
export function historyGroups<T extends MedRecord>(records: T[], today: string): MedGroup<T>[] {
	const byKey = new Map<string, T[]>();
	for (const r of records) {
		const key = (str(r.detail.ingredient) ?? str(r.detail.name) ?? r.title).trim().toLowerCase();
		byKey.set(key, [...(byKey.get(key) ?? []), r]);
	}
	const since = (r: MedRecord) => str(r.detail.started) ?? '';
	const groups: MedGroup<T>[] = [...byKey].map(([key, rows]) => {
		const entries = [...rows].sort((a, b) => since(b).localeCompare(since(a)) || b.recorded_at.localeCompare(a.recorded_at));
		const current = entries.some((e) => isCurrent(e, today));
		return { key, title: str(entries[0].detail.name) ?? entries[0].title, current, state: current ? null : notTaken(entries, today), entries };
	});
	return groups.sort((a, b) => Number(b.current) - Number(a.current) || since(b.entries[0]).localeCompare(since(a.entries[0])) || a.title.localeCompare(b.title));
}

function notTaken(entries: MedRecord[], today: string): MedGroup['state'] {
	const live = entries.find((e) => e.status === 'current' || e.status === 'provisional');
	if (!live) return entries.some((e) => e.status === 'retracted') ? 'retracted' : null;
	if (str(live.detail.med_status) === 'on_hold') return 'on_hold';
	const started = str(live.detail.started);
	return started && started > today ? 'later' : 'stopped';
}

const SYSTEMS: Record<string, string> = {
	rxnorm: 'RxNorm',
	'http://www.nlm.nih.gov/research/umls/rxnorm': 'RxNorm',
	atc: 'ATC',
	'http://www.whocc.no/atc': 'ATC',
	snomed: 'SNOMED',
	'http://snomed.info/sct': 'SNOMED'
};
const FULL: Record<string, string> = { rxnorm: 'http://www.nlm.nih.gov/research/umls/rxnorm', atc: 'http://www.whocc.no/atc', snomed: 'http://snomed.info/sct' };

/** A short name for a code system: ours, else the host of its URL, else the system as sent. */
function systemLabel(system: string | null): string {
	if (!system) return '';
	const known = SYSTEMS[system.toLowerCase()];
	if (known) return known;
	try {
		return new URL(system).hostname.replace(/^www\./, '');
	} catch {
		return system;
	}
}

export interface CodeChip {
	label: string;
	code: string;
	display: string | null;
	system: string | null;
}

/** The primary code first, then every other code the source sent, each once. */
export function codeChips(detail: Record<string, unknown>): CodeChip[] {
	const out: CodeChip[] = [];
	const seen = new Set<string>();
	const add = (system: string | null, code: string | null, display: string | null) => {
		if (!code) return;
		const full = system ? (FULL[system.toLowerCase()] ?? system) : '';
		const k = `${full}|${code}`;
		if (seen.has(k)) return;
		seen.add(k);
		out.push({ label: systemLabel(system), code, display, system });
	};
	add(str(detail.code_system), str(detail.code), null);
	if (Array.isArray(detail.codes)) {
		for (const c of detail.codes as Record<string, unknown>[]) add(str(c?.system), str(c?.code), str(c?.display));
	}
	return out;
}

/**
 * The reason a version was replaced, split for translation. The server writes an agent's accepted
 * proposal as `agent proposal <start|stop|change>` with an optional `: <reason>`; any other text is
 * the person's own correction note and is shown as written.
 */
export function replacedReason(reason: string | null): { action: 'start' | 'stop' | 'change' | null; text: string | null } {
	const m = /^agent proposal (start|stop|change)(?:: ([\s\S]*))?$/.exec(reason ?? '');
	if (m) return { action: m[1] as 'start' | 'stop' | 'change', text: str(m[2]) };
	return { action: null, text: str(reason) };
}

/** The drug-check answers asked for one profile, keyed by the drug name the check resolves. */
export interface PgxViews<V> {
	slug: string;
	views: Record<string, V | null>;
}

/**
 * The drug-check answer behind a current row's PGx badges: only from answers asked for this profile, and
 * never for a supplement (supplements are not in CPIC). Answers left from another profile show nothing.
 */
export function pgxFor<V>(cache: PgxViews<V> | null, slug: string | undefined, drug: string, kind: unknown): V | null {
	if (!cache || cache.slug !== slug || kind === 'supplement') return null;
	return cache.views[drug] ?? null;
}

/** The decisions a person can record on a supplement: their own choice, never advice. */
export const DECISIONS = ['keep', 'stop', 'do_not_repurchase'] as const;
export type Decision = (typeof DECISIONS)[number];

/**
 * The `status_reason` of a version replaced by a new decision. It is a code, never shown as written:
 * the screen says it in the reader's language.
 */
export const DECISION_RECORDED = 'decision recorded';

/** The decision a version carries, or null when none was recorded (or the value is not one we know). */
export function decisionOf(detail: Record<string, unknown>): Decision | null {
	const d = detail.decision;
	return DECISIONS.includes(d as Decision) ? (d as Decision) : null;
}

/** The reason the person gave for their decision. Reads mark it as untrusted text; an older plain string still reads. */
export function decisionReasonOf(detail: Record<string, unknown>): string | null {
	const r = detail.decision_reason;
	const text = typeof r === 'string' ? r : r && typeof r === 'object' && typeof (r as { text?: unknown }).text === 'string' ? (r as { text: string }).text : null;
	return text?.trim() ? text : null;
}

/** How a supplement row reads today: taken, starting later, on hold or stopped. */
export function supplementState(r: MedRecord, today: string): 'active' | 'later' | 'on_hold' | 'stopped' {
	if (isCurrent(r, today)) return 'active';
	// the same reading as the History card's group label
	const state = notTaken([r], today);
	return state === 'on_hold' || state === 'later' ? state : 'stopped';
}

/** Every supplement's live version, taken or not: the ones taken today first, then by name. */
export function supplementRows<T extends MedRecord>(records: T[], today: string): T[] {
	return records
		.filter((r) => (r.status === 'current' || r.status === 'provisional') && r.detail.kind === 'supplement')
		.sort((a, b) => Number(isCurrent(b, today)) - Number(isCurrent(a, today)) || a.title.localeCompare(b.title));
}
