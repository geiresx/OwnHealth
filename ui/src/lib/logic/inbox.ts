// Pure helpers for the v0.2 screens (Inbox, review queue additions, OAuth consent). No Svelte, no
// fetch: tests/inbox.test.ts runs them under node.

type Kind = 'email' | 'folder' | 'paperless' | 'share' | 'shortcut' | 'health_auto_export' | 'gadgetbridge' | 'nightscout';

/** Kinds a person can add from the Inbox screen, in the order the form offers them. */
export const ADDABLE: Kind[] = ['email', 'folder', 'paperless', 'health_auto_export', 'shortcut', 'nightscout'];
/** Kinds that receive pushes with an `oh_ing_` token (Paperless webhook, Shortcut, Health Auto Export). */
export const PUSH_KINDS: Kind[] = ['paperless', 'shortcut', 'health_auto_export'];
/** Kinds the scheduler polls. */
export const POLL_KINDS: Kind[] = ['email', 'folder', 'paperless', 'nightscout'];
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
export function sourceStatus(s: HealthLike): SourceStatus {
	if (!s.enabled) return 'off';
	const failing = s.consecutive_failures > 0 && (!s.last_success_at || (s.last_error_at != null && s.last_error_at > s.last_success_at));
	if (failing) return 'out';
	if (s.last_run_status === 'partial') return 'partial';
	if (s.stale) return 'stale';
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
 * secret requirement (an empty secret field keeps the stored one).
 */
export function sourceBody(f: SourceForm, editing = false): { body: SourceBody } | { error: string } {
	const label = f.label.trim();
	if (!label) return { error: 'fu_err_label' };
	if (PROFILE_KINDS.includes(f.kind) && !f.profile_id) return { error: 'fu_err_profile' };
	let poll_seconds: number | null = null;
	if (POLL_KINDS.includes(f.kind)) {
		const m = Number(f.poll_minutes);
		if (!Number.isFinite(m) || m < 1 || m > 1440) return { error: 'fu_err_poll' };
		poll_seconds = Math.round(m * 60);
	}
	const config: Record<string, unknown> = {};
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
			break;
		}
		case 'paperless':
		case 'nightscout': {
			const url = httpUrl(f.url);
			if (!url) return { error: 'fu_err_url' };
			config.base_url = url;
			config.allow_private = f.allow_private;
			if (f.kind === 'paperless') config.health_tag = f.tag.trim() || 'health';
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
