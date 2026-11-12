// Endpoints the "manage" pages read (lane I owns this file): wearables, import & review, family, MCP & access.
// Backends: lane C (wearables.py, records.py), lane B (documents.py), lane G (tokens.py, audit.py, family.py),
// lane F (mcp_usage.py). Review and uploads are in core.ts.
import { del, get, p, post, put, request, requestBlob } from './core';
import type {
	AuditOut,
	AuthState,
	DocumentDetail,
	DocumentSummary,
	DrugCheckView,
	GenomeEvents,
	GenomeView,
	Household,
	PasskeyOut,
	RecordOut,
	Role,
	Scope,
	TokenOut,
	TokenUsage,
	Uuid,
	WearablesSummary
} from './types';

export const getWearables = (profile: string, lang?: string) => get<WearablesSummary>(`${p(profile)}/wearables/summary`, { lang });

export const listDocuments = (profile: string, q: { q?: string; type?: string } = {}) =>
	get<{ items: DocumentSummary[] }>(`${p(profile)}/documents`, q);
export const getDocument = (id: Uuid) => get<DocumentDetail>(`/documents/${id}`);
export const pageImageUrl = (id: Uuid, page: number) => `/api/documents/${id}/pages/${page}.png`;
export const documentFileUrl = (id: Uuid) => `/api/documents/${id}/file`;

/** `history` adds the superseded and retracted versions of every current record. */
export const listRecords = (profile: string, kind: RecordOut['kind'], history = false) =>
	get<{ items: RecordOut[] }>(`${p(profile)}/records/${kind}`, history ? { history: true } : undefined);

// Manual entry and corrections (backend api/records.py). A correction is a new version; the old one stays.
export type ManualObservationIn = { name: string; value: string; unit?: string | null; effective_at: string; note?: string | null; loinc?: string | null; category?: string | null };
export const addManualObservation = (profile: string, body: ManualObservationIn) =>
	post<{ id: Uuid; src: string; status: string }>(`${p(profile)}/manual-observations`, body);
export const createRecord = (profile: string, kind: RecordOut['kind'], body: Record<string, unknown>) => post<RecordOut>(`${p(profile)}/records/${kind}`, body);
export const correctRecord = (profile: string, kind: RecordOut['kind'], id: Uuid, body: Record<string, unknown> & { reason: string }) =>
	post<RecordOut>(`${p(profile)}/records/${kind}/${id}/correct`, body);
export const retractRecord = (profile: string, kind: RecordOut['kind'], id: Uuid, reason: string) =>
	post<RecordOut>(`${p(profile)}/records/${kind}/${id}/retract`, { reason });

export const listTokens = () => get<{ items: TokenOut[] }>('/tokens');
export const createToken = (body: {
	label: string;
	scopes: Scope[];
	profiles: string[];
	default_profile?: string;
	days?: number;
	identity_mode?: TokenOut['identity_mode'];
	lang?: 'es' | 'en';
	units?: TokenOut['units'];
}) => post<{ token: TokenOut; plaintext: string }>('/tokens', body);
export const revokeToken = (id: Uuid) => del<{ id: Uuid; revoked: true }>(`/tokens/${id}`);
export const getTokenUsage = (id: Uuid) => get<TokenUsage>(`/tokens/${id}/usage`);

export const listAudit = (q: { profile?: string; limit?: number; before?: number; via?: string } = {}) => get<{ items: AuditOut[] }>('/audit', q);

export const getHousehold = () => get<Household>('/household');
// People (backend api/family.py): a profile without a login, an invite for a new login, deleting a profile.
export type RelativeIn = {
	display_name: string;
	role: 'guardian';
	relationship?: 'partner' | 'child' | 'parent' | 'other' | null;
	birth_date?: string | null;
	sex_at_birth?: 'female' | 'male' | 'unknown' | null;
	country?: string | null;
	slug?: string | null;
};
export const addRelative = (body: RelativeIn) => post<{ id: Uuid; slug: string }>('/household/profiles', body);
export const inviteMember = (body: { display_name: string; locale: 'es' | 'en'; claim_profile?: string | null }) =>
	post<{ user_id: Uuid; code: string; url: string; expires_in_days: number }>('/household/invites', body);
/** Needs a fresh step-up; `confirm` repeats the slug. */
export const deleteProfile = (slug: string) =>
	request<{ deleted: string; rows: number; blobs: number }>('DELETE', p(slug), { query: { confirm: slug } });
/** Full export: FHIR R4 NDJSON plus the original files in one zip. Needs a fresh step-up. */
export const exportProfile = (slug: string) => requestBlob('POST', `${p(slug)}/export`, { fmt: 'zip' }, `ownhealth-${slug}.zip`);
/** The four views of the printed doctor summary (data/summary/specialties.yaml), in their order. */
export const SUMMARY_SPECIALTIES = ['gp', 'cardiology', 'endocrinology', 'hematology'] as const;
export type SummarySpecialty = (typeof SUMMARY_SPECIALTIES)[number];
/** The printed doctor summary (two A4 pages, a PDF). Needs a fresh step-up. */
export const printSummary = (slug: string, body: { lang: 'es' | 'en'; specialty: SummarySpecialty; include_sensitive: boolean }) =>
	requestBlob('POST', `${p(slug)}/summary.pdf`, body, `ownhealth-summary-${slug}.pdf`);
export const setLocks = (profile: string, locks: Scope[]) => put<{ ok: true }>(`${p(profile)}/locks`, { locks });

// ---- lane I-ui-2: genome, drug check, documents, review helpers, auth, settings ----

export const getGenomeView = (profile: string, lang?: string) => get<GenomeView>(`${p(profile)}/genome`, { lang });
export const getGenomeEvents = (profile: string) => get<GenomeEvents>(`${p(profile)}/genome/events`);
/** Needs a fresh step-up (withStepUp). */
export const setGenomeConsent = (profile: string, body: { ui?: boolean; agents?: boolean }) =>
	post<{ ui: boolean; agents: boolean }>(`${p(profile)}/genome/consent`, body);
export const lookupVariant = (profile: string, rsid: string) => get<Record<string, unknown>>(`${p(profile)}/genome/variant`, { rsid });

export const getDrugCheckView = (profile: string, drug: string, lang?: string) =>
	get<DrugCheckView>(`${p(profile)}/drug-check`, { drug, lang });

export const retractDocument = (id: Uuid, reason: string) => post<{ document_id: Uuid; status: 'retracted' }>(`/documents/${id}/retract`, { reason });
export const addAlias = (body: { profile: string; alias: string; alias_group: string; lang?: string; loinc?: string | null; unit?: string | null }) =>
	post<{ id: Uuid; alias_group: string; rederived: number }>('/aliases', body);

export const deleteGrant = (userId: Uuid, profile: string) =>
	request<{ ok: true }>('DELETE', '/grants', { query: { user_id: userId, profile } });
export const putGrant = (body: { user_id: Uuid; profile: string; role: Role; categories?: Scope[] | null; expires_at?: string | null }) => put<{ ok: true }>('/grants', body);
export const putLocks = (profile: string, locks: Scope[]) => put<{ ok: true; locks: Scope[] }>(`${p(profile)}/locks`, { locks });
export const putSensitive = (profile: string, body: { ui?: boolean; agents?: boolean }) =>
	put<{ ok: true; ui: boolean; agents: boolean }>(`${p(profile)}/sensitive`, body);

// auth (lane G, api/auth.py). WebAuthn options and credentials travel as JSON (base64url).
type Json = Record<string, unknown>;
type Setup = { household_name?: string; display_name: string; locale: 'es' | 'en'; birth_date?: string | null; sex_at_birth?: string | null; country?: string | null };
export const getAuthState = () => get<AuthState>('/auth/state');
// Enrolment from an invite link (/enroll?code=).
export type InviteInfo = { display_name: string; household: string; claims_profile: boolean; passkeys_possible: boolean; password_allowed: boolean; oidc: boolean };
export const getInvite = (code: string) => get<InviteInfo>(`/auth/invite/${encodeURIComponent(code)}`);
export const enrollBegin = (code: string) => post<Json>('/auth/enroll/begin', { code });
export const enrollFinish = (code: string, credential: Json, label?: string) =>
	post<{ user_id: Uuid; recovery_codes: string[] }>('/auth/enroll/finish', { code, credential, label });
export const enrollPassword = (code: string, password: string) =>
	post<{ user_id: Uuid; recovery_codes: string[] }>('/auth/enroll/password', { code, password });
export const setupBegin = (body: Setup) => post<Json>('/auth/setup/begin', body);
export const setupFinish = (credential: Json, label?: string) =>
	post<{ user_id: Uuid; profile: string; recovery_codes: string[] }>('/auth/setup/finish', { credential, label });
export const setupPassword = (body: Setup & { password: string }) =>
	post<{ user_id: Uuid; profile: string; recovery_codes: string[] }>('/auth/setup/password', body);
export const loginBegin = () => post<Json>('/auth/login/begin');
export const loginFinish = (credential: Json) => post<{ user_id: Uuid }>('/auth/login/finish', { credential });
export const recoveryLogin = (code: string) => post<{ user_id: Uuid; recovery_codes_left: number }>('/auth/recovery', { code });
export const passwordLogin = (name: string, password: string) => post<{ user_id: Uuid }>('/auth/password/login', { name, password });
export const logout = () => post<{ ok: true }>('/auth/logout');
export const stepUpBegin = () => post<{ method: 'passkey' | 'password' | 'oidc'; options?: Json; url?: string }>('/auth/step-up/begin');
export const stepUpFinish = (body: { credential?: Json; password?: string }) => post<{ ok: true; valid_seconds: number }>('/auth/step-up/finish', body);
export const listPasskeys = () => get<{ items: PasskeyOut[] }>('/auth/passkeys');
export const addPasskeyBegin = () => post<Json>('/auth/passkeys/begin');
export const addPasskeyFinish = (credential: Json, label?: string) => post<{ items: PasskeyOut[] }>('/auth/passkeys/finish', { credential, label });
export const removePasskey = (id: string) => del<{ ok: true }>(`/auth/passkeys/${encodeURIComponent(id)}`);
export const newRecoveryCodes = () => post<{ recovery_codes: string[] }>('/auth/recovery-codes');
