// v0.2 Inboxes (lane F-ui): the endpoints the Inbox screen, the share screen, the OAuth consent screen
// and the connected-apps list read. Backends: ownhealth-api routes inboxes.rs (registry, health),
// inbox.rs (quarantine, sender rules, PDF passwords), ingest.rs (share target), oauth.rs (consent).
import { del, get, post, request } from './core';
import type { IsoDateTime, Scope, UntrustedText, Uuid } from './types';

/** A list answer, whether the route sends `[...]` or `{items: [...]}`. */
type List<T> = T[] | { items: T[] };
const items = <T>(r: List<T>): { items: T[] } => ({ items: Array.isArray(r) ? r : (r?.items ?? []) });
const list = <T>(path: string, query?: Record<string, string | number>) => get<List<T>>(path, query).then(items);

export type SourceKind = 'email' | 'folder' | 'paperless' | 'share' | 'shortcut' | 'health_auto_export' | 'gadgetbridge' | 'nightscout';

/** A configured inbox with its health. Secrets are write-only: `has_secret` says one is stored. */
export interface InboxSource {
	id: Uuid;
	kind: SourceKind;
	label: string;
	profile_id: Uuid | null;
	config: Record<string, unknown>;
	enabled: boolean;
	poll_seconds: number | null;
	token_prefix?: string | null;
	has_secret?: boolean;
	last_run_at: IsoDateTime | null;
	last_success_at: IsoDateTime | null;
	last_error_at: IsoDateTime | null;
	last_error: string | null;
	consecutive_failures: number;
	stale: boolean;
	/** The newest finished run (ok, partial, error), its problem count and first problem (`code: message`). */
	last_run_status?: 'ok' | 'partial' | 'error' | null;
	last_run_problems?: number;
	last_run_problem?: string | null;
	/** Messages of this source waiting in quarantine (email only). */
	quarantined?: number;
	created_at?: IsoDateTime;
}

export interface SourceRun {
	id: number;
	trigger: 'poll' | 'push' | 'manual';
	started_at: IsoDateTime;
	finished_at: IsoDateTime | null;
	status: 'running' | 'ok' | 'partial' | 'error';
	items_seen: number;
	items_new: number;
	items_duplicate: number;
	items_quarantined: number;
	errors: { code: string; message: string }[];
	error: string | null;
}

export interface SourceIn {
	kind: SourceKind;
	label: string;
	profile_id?: Uuid | null;
	config: Record<string, unknown>;
	/** IMAP password, Paperless API token or Nightscout token; sealed on the server, never returned. */
	secret?: string;
	poll_seconds?: number | null;
}

export type QuarantineReason = 'sender_not_allowed' | 'auth_failed' | 'too_large' | 'unsupported_type' | 'no_profile';

export interface QuarantineOut {
	id: Uuid;
	source_id: Uuid | null;
	sender: UntrustedText | string | null;
	subject: UntrustedText | string | null;
	attachments: { filename: UntrustedText | string; mime: string; size: number }[];
	reason: QuarantineReason;
	status: 'held' | 'released' | 'deleted';
	received_at: IsoDateTime;
}

export interface PdfPasswordOut {
	id: Uuid;
	label: string | null;
	created_at?: IsoDateTime;
}

export interface SenderRuleOut {
	id: Uuid;
	pattern: string;
	profile_id: Uuid;
	doc_type_hint: string | null;
	trust_unauthenticated: boolean;
	pdf_passwords: PdfPasswordOut[];
	created_at?: IsoDateTime;
}

export const listInboxes = () => list<InboxSource>('/inboxes');
export const createInbox = (body: SourceIn) => post<InboxSource>('/inboxes', body);
export const updateInbox = (id: Uuid, body: Partial<Omit<SourceIn, 'kind'>> & { enabled?: boolean }) =>
	request<InboxSource>('PATCH', `/inboxes/${id}`, { body });
export const deleteInbox = (id: Uuid) => del<{ ok: true }>(`/inboxes/${id}`);
export const listRuns = (id: Uuid, limit = 10) => list<SourceRun>(`/inboxes/${id}/runs`, { limit });
/** A new push token, shown once. Needs step-up. */
export const issueInboxToken = (id: Uuid) => post<{ token: string; prefix: string; url?: string }>(`/inboxes/${id}/token`);
export const syncInbox = (id: Uuid) => post<{ job_id: number | null }>(`/inboxes/${id}/sync`);

export const listQuarantine = (status: 'held' | 'released' | 'deleted' = 'held') => list<QuarantineOut>('/inboxes/quarantine', { status });
/** Release a held message into a profile; `allow_sender` also adds the sender to the allowlist. Step-up. */
export const releaseQuarantined = (id: Uuid, body: { profile_id: Uuid; allow_sender: boolean }) =>
	post<{ document_ids: Uuid[]; problems?: { code: string; message: string }[] }>(`/inboxes/quarantine/${id}/release`, body);
export const deleteQuarantined = (id: Uuid) => del<{ ok: true }>(`/inboxes/quarantine/${id}`);

export const listSenders = (sourceId: Uuid) => list<SenderRuleOut>(`/inboxes/${sourceId}/senders`);
/** Adding a sender widens what flows in: step-up. */
export const addSender = (sourceId: Uuid, body: { pattern: string; profile_id: Uuid; doc_type_hint?: string | null; trust_unauthenticated?: boolean }) =>
	post<SenderRuleOut>(`/inboxes/${sourceId}/senders`, body);
export const deleteSender = (id: Uuid) => del<{ ok: true }>(`/senders/${id}`);
/** The password is sealed on the server and never comes back. */
export const addPdfPassword = (senderId: Uuid, body: { password: string; label?: string | null }) =>
	post<PdfPasswordOut>(`/senders/${senderId}/pdf-passwords`, body);
export const deletePdfPassword = (id: Uuid) => del<{ ok: true }>(`/pdf-passwords/${id}`);

/** The PWA share target: the files Android shared, into one profile. */
export function shareFiles(profile: string, files: File[]): Promise<{ items: { filename: string; outcome?: string; document_id?: Uuid; duplicate?: boolean }[] }> {
	const form = new FormData();
	form.set('profile', profile);
	for (const f of files) form.append('files', f, f.name);
	return request('POST', '/ingest/share', { body: form });
}

// ---- OAuth 2.1: the consent screen and the connected apps (lane E backend) ----

export type IdentityMode = 'full' | 'pseudonymous' | 'minimal';

export interface OAuthRequestOut {
	id: string;
	client_id: string; // the CIMD URL
	client_name: UntrustedText | string | null;
	client_uri?: string | null;
	logo_uri?: string | null;
	redirect_uri: string;
	/** Requested scopes; labels in es and en when the server sends them. */
	scopes: (Scope | { scope: Scope; en?: string; es?: string })[];
	profiles: { id: Uuid; slug: string; display_name: string }[];
	default_days?: number;
	max_days?: number;
}

export interface OAuthConsentOut {
	id: Uuid;
	client_id: string;
	client_name: UntrustedText | string | null;
	scopes: Scope[];
	profile_ids: Uuid[];
	identity_mode: IdentityMode;
	created_at: IsoDateTime;
	expires_at: IsoDateTime;
	revoked_at: IsoDateTime | null;
	last_used_at?: IsoDateTime | null;
	tokens?: { id: Uuid; prefix?: string; created_at: IsoDateTime; expires_at: IsoDateTime; last_used_at: IsoDateTime | null; revoked_at: IsoDateTime | null }[];
}

export const getOAuthRequest = (id: string) => get<OAuthRequestOut>(`/oauth/requests/${encodeURIComponent(id)}`);
/** Approving a client needs step-up; the answer is where to send the browser back. */
export const approveOAuth = (id: string, body: { scopes: Scope[]; profile_ids: Uuid[]; default_profile?: Uuid; identity_mode: IdentityMode; days: number }) =>
	post<{ redirect: string }>(`/oauth/requests/${encodeURIComponent(id)}/approve`, body);
export const denyOAuth = (id: string) => post<{ redirect: string }>(`/oauth/requests/${encodeURIComponent(id)}/deny`);
export const listConsents = () => list<OAuthConsentOut>('/oauth/consents');
export const revokeConsent = (id: Uuid) => del<{ ok: true }>(`/oauth/consents/${id}`);

// ---- CGM (lane D backend): GET /profiles/{p}/cgm/summary ----

/** Times in ranges over a window, per the international consensus; `valid` needs >= 70 % sensor wear over 14 days. */
export interface CgmSummary {
	start: IsoDateTime;
	end: IsoDateTime;
	readings: number;
	wear_fraction: number;
	valid: boolean;
	invalid_reason: 'no_readings' | 'short_window' | 'insufficient_wear' | null;
	mean_mg_dl: number | null;
	gmi_percent: number | null;
	very_low_pct: number | null;
	low_pct: number | null;
	in_range_pct: number | null;
	high_pct: number | null;
	very_high_pct: number | null;
}

export const getCgmSummary = (profile: string, days = 14) => get<CgmSummary>(`/profiles/${encodeURIComponent(profile)}/cgm/summary`, { days });
