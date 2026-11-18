// Pure helpers for the v0.2 screens (Inbox, review queue additions, OAuth consent). No Svelte, no
// fetch: tests/inbox.test.ts runs them under node.

type Kind = 'email' | 'folder' | 'paperless' | 'share' | 'shortcut' | 'health_auto_export' | 'gadgetbridge' | 'nightscout' | 'clinvar';

/** Kinds a person can add from the Inbox screen, in the order the form offers them. */
export const ADDABLE: Kind[] = ['email', 'folder', 'paperless', 'health_auto_export', 'shortcut', 'nightscout'];
/** Kinds that receive pushes with an `oh_ing_` token (Paperless webhook, Shortcut, Health Auto Export). */
export const PUSH_KINDS: Kind[] = ['paperless', 'shortcut', 'health_auto_export'];
/** Kinds the scheduler polls. */
export const POLL_KINDS: Kind[] = ['email', 'folder', 'paperless', 'nightscout', 'clinvar'];
/** Kinds whose data always belongs to one profile (email picks it per sender rule, a folder per subfolder). */
export const PROFILE_KINDS: Kind[] = ['paperless', 'shortcut', 'health_auto_export', 'nightscout'];
/** The secret each kind keeps sealed, if any. */
export const SECRET_OF: Partial<Record<Kind, 'imap_password' | 'paperless_token' | 'nightscout_token'>> = {
	email: 'imap_password',
	paperless: 'paperless_token',
	nightscout: 'nightscout_token'
};
/** The setup recipe of each kind, in the repository's docs/recipes. */
export const RECIPE: Partial<Record<Kind, string>> = {
	folder: 'scanner-to-folder.md',
	paperless: 'paperless-ngx.md',
	health_auto_export: 'health-auto-export.md',
	shortcut: 'ios-shortcut.md',
	nightscout: 'nightscout.md'
};
export const RECIPES_URL = 'https://github.com/GeiserX/OwnHealth/blob/main/docs/recipes/';
/** Default poll interval in minutes: mail every minute (the 2-minute target), Paperless fallback 15. */
export const DEFAULT_POLL_MIN: Partial<Record<Kind, number>> = { email: 1, folder: 1, paperless: 15, nightscout: 5 };
/** How the mailbox connection is secured; `none` only reaches a loopback or private address. */
export const TLS_MODES = ['implicit', 'starttls', 'none'] as const;
export type TlsMode = (typeof TLS_MODES)[number];

export type SourceStatus = 'off' | 'out' | 'partial' | 'stale' | 'ok' | 'nodata';

interface HealthLike {
	enabled: boolean;
	last_run_at?: string | null;
	poll_seconds?: number | null;
	created_at?: string | null;
	last_success_at: string | null;
	last_error_at: string | null;
	consecutive_failures: number;
	stale: boolean;
	/** The newest finished run: ok, partial or error. */
	last_run_status?: string | null;
}

/**
 * The pill a source shows. Failing beats stale: a source whose last run failed is Out even if the
 * failure is recent. A last run that brought some items in but not others is Partial (Watch), so a
 * stuck message or file never hides behind OK. A source that never ran is No data, never OK.
 */
export function sourceStatus(s: HealthLike, now = new Date()): SourceStatus {
	if (!s.enabled) return 'off';
	const failing = s.consecutive_failures > 0 && (!s.last_success_at || (s.last_error_at != null && s.last_error_at > s.last_success_at));
	if (failing) return 'out';
	if (s.last_run_status === 'partial') return 'partial';
	// A source added a moment ago has not had its first run yet: that is No data, not Stale.
	const fresh = !s.last_run_at && !!s.created_at && !!s.poll_seconds && now.getTime() - new Date(s.created_at).getTime() < s.poll_seconds * 1000;
	if (s.stale && !fresh) return 'stale';
	if (!s.last_success_at) return 'nodata';
	return 'ok';
}

/** The fields of the "add source" form, all strings or booleans as the inputs hold them. */
export interface SourceForm {
	kind: Kind;
	label: string;
	profile_id: string; // '' = chosen per item by a rule
	host: string;
	port: string;
	tls: TlsMode;
	username: string;
	secret: string;
	folder: string;
	done_folder: string;
	trusted_authserv_id: string;
	path: string;
	url: string;
	tag: string;
	allow_private: boolean;
	poll_minutes: string;
}

const TLS_PORT: Record<TlsMode, string> = { implicit: '993', starttls: '143', none: '143' };

/** The port after the TLS mode changes: the new mode's default when the port still holds the old one's. */
export function portForTls(from: TlsMode, to: TlsMode, port: string): string {
	return port.trim() === '' || port.trim() === TLS_PORT[from] ? TLS_PORT[to] : port;
}

/** Held messages across mailboxes, from each source's server-side count (the list itself is paged). */
export function heldTotal(sources: { kind: string; quarantined?: number }[] | null | undefined): number | null {
	if (!sources) return null;
	return sources.reduce((n, s) => n + (s.kind === 'email' ? (s.quarantined ?? 0) : 0), 0);
}

export function emptyForm(kind: Kind): SourceForm {
	return {
		kind,
		label: '',
		profile_id: '',
		host: '',
		port: '993',
		tls: 'implicit',
		username: '',
		secret: '',
		folder: 'INBOX',
		done_folder: 'OwnHealth/Done',
		trusted_authserv_id: '',
		path: '',
		url: '',
		tag: 'health',
		allow_private: false,
		poll_minutes: String(DEFAULT_POLL_MIN[kind] ?? '')
	};
}

export interface SourceBody {
	kind: Kind;
	label: string;
	profile_id: string | null;
	config: Record<string, unknown>;
	secret?: string;
	poll_seconds: number | null;
}

/** An http(s) URL with a host, or null. */
function httpUrl(v: string): string | null {
	try {
		const u = new URL(v.trim());
		return (u.protocol === 'https:' || u.protocol === 'http:') && u.hostname ? u.toString().replace(/\/$/, '') : null;
	} catch {
		return null;
	}
}

/**
 * The POST /inboxes body for a form, or the i18n key of the first problem. `editing` skips the
 * secret requirement (an empty secret field keeps the stored one). `stored` is the config the source
 * has now: the server replaces config as a whole, so an edit starts from it and overwrites only the
 * keys the form owns. Keys set only through the API (Paperless `profile_by_tag`, Nightscout
 * `backfill_days`, a mailbox `ca_file`) survive an edit in the form.
 */
export function sourceBody(f: SourceForm, editing = false, stored?: Record<string, unknown> | null): { body: SourceBody } | { error: string } {
	const label = f.label.trim();
	if (!label) return { error: 'fu_err_label' };
	if (PROFILE_KINDS.includes(f.kind) && !f.profile_id) return { error: 'fu_err_profile' };
	let poll_seconds: number | null = null;
	if (POLL_KINDS.includes(f.kind)) {
		const m = Number(f.poll_minutes);
		if (!Number.isFinite(m) || m < 1 || m > 1440) return { error: 'fu_err_poll' };
		poll_seconds = Math.round(m * 60);
	}
	const config: Record<string, unknown> = { ...(stored ?? {}) };
	const needSecret = SECRET_OF[f.kind] && !editing;
	switch (f.kind) {
		case 'email': {
			const port = Number(f.port);
			if (!f.host.trim() || !f.username.trim()) return { error: 'fu_err_imap' };
			if (!Number.isInteger(port) || port < 1 || port > 65535) return { error: 'fu_err_port' };
			Object.assign(config, {
				host: f.host.trim(),
				port,
				tls: f.tls,
				username: f.username.trim(),
				folder: f.folder.trim() || 'INBOX',
				done_folder: f.done_folder.trim() || 'OwnHealth/Done',
				trusted_authserv_id: f.trusted_authserv_id.trim() || null
			});
			break;
		}
		case 'folder': {
			const path = f.path.trim().replace(/^\/+|\/+$/g, '');
			if (path.split('/').includes('..')) return { error: 'fu_err_path' };
			config.subdir = path;
			delete config.path;
			break;
		}
		case 'paperless':
		case 'nightscout': {
			const url = httpUrl(f.url);
			if (!url) return { error: 'fu_err_url' };
			// The server only takes plain http for a service on the person's own network.
			if (url.startsWith('http:') && !f.allow_private) return { error: 'fu_err_http' };
			config.base_url = url;
			// `url` is an older name of base_url; both at once would be refused by the server.
			delete config.url;
			config.allow_private = f.allow_private;
			if (f.kind === 'paperless') {
				config.health_tag = f.tag.trim() || 'health';
				delete config.tag;
			}
			break;
		}
	}
	if (needSecret && !f.secret) return { error: 'fu_err_secret' };
	const body: SourceBody = { kind: f.kind, label, profile_id: f.profile_id || null, config, poll_seconds };
	if (SECRET_OF[f.kind] && f.secret) body.secret = f.secret;
	return { body };
}

/** The form for an existing source (the secret field stays empty: secrets never come back). */
export function formOf(s: { kind: Kind; label: string; profile_id: string | null; config: Record<string, unknown>; poll_seconds: number | null }): SourceForm {
	const f = emptyForm(s.kind);
	const c = s.config ?? {};
	const str = (k: string, d: string) => (c[k] == null ? d : String(c[k]));
	return {
		...f,
		label: s.label,
		profile_id: s.profile_id ?? '',
		host: str('host', ''),
		port: str('port', f.port),
		tls: (TLS_MODES as readonly string[]).includes(String(c.tls)) ? (c.tls as TlsMode) : f.tls,
		username: str('username', ''),
		folder: str('folder', f.folder),
		done_folder: str('done_folder', f.done_folder),
		trusted_authserv_id: str('trusted_authserv_id', ''),
		path: str('subdir', ''),
		url: str('base_url', str('url', '')),
		tag: str('health_tag', f.tag),
		allow_private: c.allow_private === true,
		poll_minutes: s.poll_seconds ? String(Math.round(s.poll_seconds / 60)) : f.poll_minutes
	};
}

/** What a source card shows of its config: the names the form writes, or the older ones. */
export function sourceWhere(config: Record<string, unknown> | null | undefined): { subdir: string; url: string; tag: string } {
	const c = config ?? {};
	const s = (...keys: string[]) => {
		for (const k of keys) if (c[k] != null && c[k] !== '') return String(c[k]);
		return '';
	};
	return { subdir: s('subdir', 'path'), url: s('base_url', 'url'), tag: s('health_tag', 'tag') };
}

/**
 * The pill of a held message. `auth_failed` from a mailbox with no trusted authserv-id means nothing
 * was checked (the sender rule does not accept unauthenticated mail), so it is not a forgery: amber
 * `auth_unchecked`, which points at the authserv-id field. A real DKIM/SPF failure stays red.
 */
export function quarantineReason(reason: string, mailbox: Record<string, unknown> | null | undefined): { key: string; tone: 'p-out' | 'p-watch' } {
	if (reason !== 'auth_failed') return { key: 'fu_qr_' + reason, tone: 'p-watch' };
	return hasAuthserv(mailbox) ? { key: 'fu_qr_auth_failed', tone: 'p-out' } : { key: 'fu_qr_auth_unchecked', tone: 'p-watch' };
}

/** Whether a mailbox config names the authserv-id whose Authentication-Results count. */
export function hasAuthserv(mailbox: Record<string, unknown> | null | undefined): boolean {
	const v = mailbox?.trusted_authserv_id;
	return typeof v === 'string' && v.trim() !== '';
}

/** The label of a source's token button: every kind can have one, a polled kind for its monitor. */
export function tokenButtonKey(kind: Kind, hasToken: boolean): string {
	if (PUSH_KINDS.includes(kind)) return hasToken ? 'fu_token_new' : 'fu_token';
	return hasToken ? 'fu_mon_token_new' : 'fu_mon_token';
}

/**
 * The one copy path of the app. Copy a value; true only when the clipboard took it (there is no clipboard
 * on a plain-http LAN origin). On false, the caller shows fu_copy_hand; when it passes the box that shows
 * the value (a .p2secret or pre.code, both user-select:all), its text is selected for Ctrl/Cmd+C.
 */
export async function copyText(
	clip: { writeText(t: string): Promise<void> } | null | undefined,
	text: string,
	box?: { ownerDocument: Document } | null
): Promise<boolean> {
	try {
		if (!clip) throw new Error('no clipboard');
		await clip.writeText(text);
		return true;
	} catch {
		if (box) {
			const sel = box.ownerDocument.getSelection();
			const range = box.ownerDocument.createRange();
			range.selectNodeContents(box as unknown as Node);
			sel?.removeAllRanges();
			sel?.addRange(range);
		}
		return false;
	}
}

/** The pill of a shared file: already there, will never import, or queued. */
export function shareTone(r: { outcome?: string; duplicate?: boolean }): { key: string; tone: 'p-stale' | 'p-watch' | 'p-ok' } {
	if (r.duplicate || r.outcome === 'duplicate' || r.outcome === 'already_seen') return { key: 'fu_share_dup', tone: 'p-stale' };
	if (r.outcome === 'unsupported') return { key: 'fu_share_unsupported', tone: 'p-watch' };
	return { key: 'fu_share_queued', tone: 'p-ok' };
}

/**
 * The five time-in-range bands of a CGM summary, or null when the summary is not valid: the server
 * then gives no figures, and none may be drawn (a missing band is not 0 %).
 */
export function cgmBands<K extends string>(g: { valid: boolean } & Partial<Record<K, number | null>>, keys: readonly K[]): Record<K, number> | null {
	if (!g.valid) return null;
	const out = {} as Record<K, number>;
	for (const k of keys) {
		const v = g[k];
		if (typeof v !== 'number') return null;
		out[k] = v;
	}
	return out;
}

const PATTERN = /^([^@\s]+@)?[^@\s]+\.[^@\s]+$/;

/**
 * The allowlist pattern for a typed sender: the bare address from 'Name <a@b.example>', or a domain,
 * lower-cased. Null when it is neither (the database check in 1000_inboxes.sql would refuse it).
 */
export function senderPattern(v: string): string | null {
	const m = v.match(/<([^>]+)>/);
	const p = (m ? m[1] : v).trim().toLowerCase().replace(/^@/, '');
	return PATTERN.test(p) ? p : null;
}

// ---------------------------------------------------------------- review ----

/**
 * The Paperless page a document came from (`flags.source_link`, built by the server from the source's
 * base_url), or null. Only http(s) links are shown.
 */
export function paperlessLink(flags: Record<string, unknown> | null | undefined): string | null {
	const l = flags?.source_link as { kind?: unknown; url?: unknown } | undefined;
	if (!l || l.kind !== 'paperless' || typeof l.url !== 'string') return null;
	try {
		const u = new URL(l.url);
		return u.protocol === 'https:' || u.protocol === 'http:' ? u.toString() : null;
	} catch {
		return null;
	}
}

export type Grounding = 'grounded' | 'ungrounded' | 'no_word_layer' | null;

/**
 * Whether a review row's value was found in the page's word layer. Reads `payload.grounding`
 * (candidate_row.grounding) and falls back to the reason codes, so either form flags the row.
 */
export function rowGrounding(item: { reasons: string[]; payload: Record<string, unknown> }): Grounding {
	const g = item.payload?.grounding;
	if (g === 'ungrounded' || item.reasons.includes('not_grounded')) return 'ungrounded';
	if (g === 'ungrounded_no_word_layer' || item.reasons.includes('ungrounded_no_word_layer')) return 'no_word_layer';
	if (g === 'grounded') return 'grounded';
	return null;
}

/**
 * The text of a genome_reinterpretation review item in the screen language: the ClinVar diff writes
 * `summary` (en) and `summary_es`. Older or hand-made items fall back to what they carry.
 */
export function reinterpText(p: Record<string, unknown>, lang: string): string {
	const s = (k: string) => (typeof p[k] === 'string' ? (p[k] as string) : undefined);
	return (lang === 'es' ? s('summary_es') : undefined) ?? s('summary') ?? s('text') ?? [s('gene'), s('from'), '→', s('to')].filter(Boolean).join(' ');
}

/** One row of an agent's observations proposal, as the server checked it (propose.rs `stored.rows`). */
export interface ProposalRow {
	index: number;
	name: string;
	value: string;
	unit: string;
	page: number;
	bbox: number[] | null;
	grounding: Grounding;
	/** Failed checks other than grounding, which has its own flag. */
	problems: { reason: string | null; detail: string | null }[];
	/** False when the server could not normalize the row: accepting cannot write it. */
	usable: boolean;
}

/** The rows of an observations proposal, or null for any other item. */
export function proposalRows(item: { kind: string; payload: Record<string, unknown> }): ProposalRow[] | null {
	const p = item.payload ?? {};
	if (item.kind !== 'agent_proposal' || p.kind !== 'observations' || !Array.isArray(p.rows)) return null;
	return (p.rows as Record<string, unknown>[]).map((r, index) => {
		const input = (r?.input ?? {}) as Record<string, unknown>;
		const g = r?.grounding;
		const checks = (Array.isArray(r?.checks) ? r.checks : []) as { ok?: unknown; name?: unknown; reason?: unknown; detail?: unknown }[];
		const bbox = Array.isArray(r?.bbox) && r.bbox.length === 4 ? (r.bbox as number[]) : null;
		return {
			index,
			name: String(input.name ?? ''),
			value: input.value == null ? '' : String(input.value),
			unit: input.unit == null ? '' : String(input.unit),
			page: Number(input.page ?? 1) || 1,
			bbox,
			// An unknown or missing grounding counts as not found: never shown as grounded.
			grounding: g === 'grounded' && bbox ? 'grounded' : g === 'ungrounded_no_word_layer' ? 'no_word_layer' : 'ungrounded',
			problems: checks
				.filter((c) => c.ok !== true && c.name !== 'grounding')
				.map((c) => ({ reason: typeof c.reason === 'string' ? c.reason : null, detail: typeof c.detail === 'string' ? c.detail : null })),
			usable: r?.observation != null && typeof r.observation === 'object'
		};
	});
}

/**
 * The rows ticked at first: only usable rows the server found on the page and that are not already in the
 * record (a duplicate) or against a value already there (a conflict). The rest wait for a person.
 */
export function proposalDefaultKeep(rows: ProposalRow[]): number[] {
	const held = (r: ProposalRow) => r.problems.some((p) => p.reason === 'duplicate' || p.reason === 'conflict');
	return rows.filter((r) => r.usable && r.grounding === 'grounded' && !held(r)).map((r) => r.index);
}

/** The value a conflict check found in another document ('another document holds 231 for this date'), or null. */
export function conflictValue(detail: string | null | undefined): string | null {
	const m = /another document holds (.+) for this date/.exec(detail ?? '');
	return m && m[1] !== 'None' ? m[1].trim() : null;
}

/**
 * The failed checks of a review row, one per reason (the server can file the same reason twice), skipping
 * the reasons that have their own line. A conflict keeps the other value when any of its checks names it.
 */
export function reviewChecks(
	checks: { ok: boolean; reason: string | null; detail: string | null }[],
	skip: string[] = []
): { reason: string | null; detail: string | null; value: string | null }[] {
	const out = new Map<string, { reason: string | null; detail: string | null; value: string | null }>();
	for (const c of checks) {
		if (c.ok || skip.includes(c.reason ?? '')) continue;
		const key = c.reason ?? `?${c.detail ?? ''}`;
		const value = c.reason === 'conflict' ? conflictValue(c.detail) : null;
		const had = out.get(key);
		if (!had) out.set(key, { reason: c.reason, detail: c.detail, value });
		else if (value && !had.value) out.set(key, { ...had, value, detail: c.detail });
	}
	return [...out.values()];
}

/** Whether a check's detail only says again what its translated label says. */
export function repeatsLabel(detail: string | null | undefined, label: string | null | undefined): boolean {
	const n = (s: string) => s.toLowerCase().replace(/[^\p{L}\p{N}]+/gu, ' ').trim();
	return !!detail && !!label && n(detail) === n(label);
}

/** One line of 'Other items': a document's extracted rows together, or one item on its own. */
export type OtherLine<T> = { document_id: string; rows: T[]; item: T } | { document_id: null; rows: null; item: T };

/** The 'Other items' list with the extracted rows of each document folded into one line, in first-seen order. */
export function othersGrouped<T extends { kind: string; document_id: string | null }>(items: T[]): OtherLine<T>[] {
	const out: OtherLine<T>[] = [];
	const byDoc = new Map<string, T[]>();
	for (const i of items) {
		if (i.kind === 'candidate_row' && i.document_id) {
			const rows = byDoc.get(i.document_id);
			if (rows) rows.push(i);
			else {
				const fresh = [i];
				byDoc.set(i.document_id, fresh);
				out.push({ document_id: i.document_id, rows: fresh, item: i });
			}
		} else out.push({ document_id: null, rows: null, item: i });
	}
	return out;
}

/** Profiles whose sources a person may set up: the ones they own or are guardian of. */
export function manageable<T extends { role: string }>(profiles: T[]): T[] {
	return profiles.filter((p) => p.role === 'owner' || p.role === 'guardian');
}

/** The one Inbox tile of Import & review: how many sources, how many failing, held mail. Null until loaded. */
export function inboxSummary(
	sources: { kind: string; enabled: boolean; consecutive_failures: number; quarantined?: number }[] | null | undefined
): { sources: number; failing: number; held: number } | null {
	if (!sources) return null;
	return {
		sources: sources.length,
		failing: sources.filter((s) => s.enabled && s.consecutive_failures > 0).length,
		held: heldTotal(sources) ?? 0
	};
}

/** Whether the share screen can send a file: a PDF or an image, as the share endpoint takes. */
export function shareAccepts(f: { name: string; type: string }): boolean {
	if (f.type === 'application/pdf' || f.type.startsWith('image/')) return true;
	return /\.(pdf|jpe?g|png|heic|heif|webp|tiff?|gif)$/i.test(f.name);
}

/**
 * The accept edits for the ticked rows, or null when none is ticked. Never an empty list: the server
 * reads an empty or absent `rows` as "every row".
 */
export function proposalAcceptEdits(rows: ProposalRow[], kept: number[]): { rows: number[] } | null {
	const ok = new Set(rows.filter((r) => r.usable).map((r) => r.index));
	const pick = [...new Set(kept)].filter((i) => ok.has(i)).sort((a, b) => a - b);
	return pick.length ? { rows: pick } : null;
}

/** What the proposing token read in the hour before a proposal (security-privacy.md 4.2 #4). */
export function agentContext(item: { payload: Record<string, unknown>; context_src_ids?: string[] | null } & Record<string, unknown>): {
	documents: string[];
	src_ids: string[];
} {
	const p = item.payload ?? {};
	const arr = (v: unknown): string[] => (Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string') : []);
	const documents = [...new Set([...arr(item.context_documents), ...arr(p.context_documents)])];
	const src_ids = [...new Set([...arr(item.context_src_ids), ...arr(p.context_src_ids), ...arr(p.based_on)])];
	return { documents, src_ids };
}

/** The open near_duplicate item filed for a document, if any. */
export function nearDuplicateOf<T extends { kind: string; document_id: string | null; status?: string }>(items: T[], documentId: string | null): T | null {
	if (!documentId) return null;
	return items.find((i) => i.kind === 'near_duplicate' && i.document_id === documentId && (i.status ?? 'open') === 'open') ?? null;
}

// ---------------------------------------------------------------- OAuth ----

/** Nothing is pre-checked beyond read:summary (mcp.md 2.2), and no profile is pre-checked. */
export function consentDefaults(requested: string[], defaultDays = 90): { scopes: string[]; profiles: string[]; days: number } {
	return { scopes: requested.includes('read:summary') ? ['read:summary'] : [], profiles: [], days: Math.min(365, Math.max(1, defaultDays)) };
}

/** The approve body is valid only with at least one profile and one scope, and a 1-365 day lifetime. */
export function consentProblem(scopes: string[], profiles: string[], days: number): string | null {
	if (!profiles.length) return 'fu_oa_need_profile';
	if (!scopes.length) return 'fu_oa_need_scope';
	if (!Number.isInteger(days) || days < 1 || days > 365) return 'fu_oa_need_days';
	return null;
}

/**
 * The Settings ClinVar switch. With no source yet the scheduler turns the check on with the first
 * genome (never in the demo), so the switch shows that default and a click records the opposite.
 */
export function clinvarOn(src: { enabled: boolean } | null, demo: boolean): boolean {
	return src ? src.enabled : !demo;
}

/** The config a Settings save sends: a typed path, else the mirror set through the API, else NCBI. */
export function clinvarConfig(path: string, current: Record<string, unknown> | undefined): Record<string, unknown> {
	const p = path.trim();
	if (p) return { path: p };
	if (typeof current?.url === 'string' && current.url) return { url: current.url, ...(current.allow_private ? { allow_private: true } : {}) };
	return {};
}
