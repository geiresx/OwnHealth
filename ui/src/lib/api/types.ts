// REST API types. This file mirrors CONTRACTS.md "REST API": change both together.
// Ids are UUID strings, dates ISO 8601 strings, decimals arrive as JSON numbers or strings.

export type Uuid = string;
export type IsoDate = string; // 2026-11-07
export type IsoDateTime = string; // 2026-11-07T07:12:00Z
export type Role = 'owner' | 'guardian' | 'caregiver' | 'viewer';
export type Status = 'ok' | 'watch' | 'out' | 'stale' | 'nodata';
export type Lifecycle = 'provisional' | 'current' | 'superseded' | 'retracted';
export type Scope =
	| 'read:summary'
	| 'read:labs'
	| 'read:vitals'
	| 'read:wearables'
	| 'read:records'
	| 'read:genome'
	| 'read:genome_raw'
	| 'read:sensitive'
	| 'read:documents_text'
	| 'write:propose';

/** Free text that came from a document, a person or an agent. Render as text, never as HTML. */
export interface UntrustedText {
	trust: 'untrusted_text';
	origin: string;
	text: string;
}

export interface ApiErrorBody {
	detail: { code: string; message: string; allowed?: string[] } | string;
}

// ---------------------------------------------------------------- foundation ----

export interface ProfileSummary {
	id: Uuid;
	slug: string;
	display_name: string;
	initials: string;
	role: Role;
	relationship: string | null;
	age: number | null;
	sex_at_birth: 'female' | 'male' | 'unknown' | null;
	country: string | null;
	is_minor: boolean;
	is_self: boolean;
	unit_pref: 'si' | 'conventional' | 'as_reported';
	genome_sensitive_ui: boolean;
	scopes: Scope[];
	flags?: number; // added by lane E when the status snapshot exists
}

export interface Me {
	user: { id: Uuid; display_name: string; locale: 'es' | 'en' };
	household: { id: Uuid; name: string };
	auth: { kind: 'session' | 'token' | 'dev' };
	profiles: ProfileSummary[];
	review_open: number;
	demo: boolean;
}

export interface SourceDetail {
	src_id: string;
	kind: string;
	label: string | null;
	status: Lifecycle | 'gone';
	facts: { table: string; id: Uuid; status: Lifecycle; status_reason: string | null }[];
	document: {
		id: Uuid;
		profile_id: Uuid | null;
		doc_type: string | null;
		mime: string;
		pages: number | null;
		provider_name: string | null;
		report_status: string | null;
		collected_at: IsoDateTime | null;
		reported_at: IsoDateTime | null;
		sha256: string;
		status: string;
	} | null;
	location: { page: number; bbox: [number, number, number, number] | null; units: 'pt' } | null;
	import_ref: string | null;
	created_at: IsoDateTime;
	text: UntrustedText | null;
}

export interface Check {
	name: string;
	ok: boolean;
	reason: string | null;
	detail: string | null;
}

export interface ReviewItem {
	id: Uuid;
	kind:
		| 'candidate_row'
		| 'agent_proposal'
		| 'fhir_unmapped'
		| 'document_identity'
		| 'amended_report'
		| 'rederive_diff'
		| 'genome_reinterpretation'
		| 'job_failed'
		| 'pending_document'
		| 'restore_check_failed'
		| 'near_duplicate'
		| 'encrypted_document';
	profile_id: Uuid | null;
	status: 'open' | 'accepted' | 'rejected' | 'dismissed';
	reasons: string[];
	document_id: Uuid | null;
	candidate_row_id: Uuid | null;
	observation_id: Uuid | null;
	/** candidate_row: {observation, src, checks: Check[], cells, page_no, bbox, row_conf, why_not, allowance} */
	payload: Record<string, unknown>;
	row_conf: number | null;
	proposed_by_token: Uuid | null;
	context_src_ids: string[] | null;
	/** v0.2: documents the proposing token read in the hour before (when the server sends them). */
	context_documents?: Uuid[] | null;
	decided_by: Uuid | null;
	decided_at: IsoDateTime | null;
	decision_reason: string | null;
	result_ids: Uuid[] | null;
	created_at: IsoDateTime;
}

export interface ImportItem {
	filename: string;
	kind: 'pdf' | 'image' | 'apple_health_zip' | 'fhir_json' | 'fhir_ndjson' | 'genome_facts' | 'vcf' | 'tbi';
	sha256: string;
	document_id?: Uuid;
	duplicate?: boolean;
	job_id?: number | null;
	attached_to?: string;
}

export interface JobInfo {
	id: number;
	kind: string;
	state: 'queued' | 'running' | 'done' | 'failed' | 'dead';
	attempts: number;
	max_attempts: number;
	last_error: string | null;
	result: Record<string, unknown> | null;
	created_at: IsoDateTime;
	finished_at: IsoDateTime | null;
	profile_id: Uuid | null;
	filename: string | null;
	document_id: Uuid | null;
}

// ------------------------------------------------- lane E: status, dashboard ----

export interface MarkerRow {
	alias_group: string;
	name: string; // localized display name
	value: string; // as printed, or canonical in the chosen units
	unit: string;
	status: Status;
	stale: boolean;
	date: IsoDate;
	src: string; // src id
	src_label: string | null; // 'LabNorte · 2026-09-14 · p2'; null until the importer labels the source
	spark: number[];
	band: [number, number] | null;
	rule?: string | null; // rule id that set the status; src is null for wearable rows (no single source)
}

export interface SystemCard {
	key: string; // cardiometabolic | liver | kidney | thyroid | blood | inflammation | vitamins | sleep | fitness
	name: string;
	status: Status;
	markers: MarkerRow[];
	newest: IsoDate | null;
	rule: string; // the id of the rule that set the status; the pack file when the card is withheld
	rule_file?: string; // the pack file of the system, e.g. 'cardiometabolic.yaml'
	withheld?: boolean; // true when the access does not cover this card (status 'nodata' is a placeholder, no markers)
	needs?: string; // the scope a withheld card needs, e.g. 'read:labs' (requested from lane E)
}

export interface MedChip {
	name: string;
	strength: string | null;
	since: IsoDate | null;
	src: string;
	pgx: { gene: string; label: string; phenotype: string; level: string | null; text: string } | null;
}

export interface Dashboard {
	profile: string;
	as_of: IsoDate;
	subtitle: string;
	systems: SystemCard[];
	medications: MedChip[];
	changes: { name: string; from: string; to: string; unit: string; direction: 'up' | 'down'; status: Status }[];
	changes_vs: IsoDate | null;
	not_measured_this_time: string[];
	due: { title: string; why: string; status: Status; kind: 'lab' | 'vaccine' | 'screening'; phase: string | null }[];
	genome: { summary: string; chips: string[]; hidden: number; source: string | null } | null;
	review_open: number;
	state_hash: string;
	rules_version: string;
	generated_at: IsoDateTime;
}

// ------------------------------------------------ lane A: observations, trend ----

export interface ObservationOut {
	id: Uuid;
	src: string;
	src_label: string | null;
	name: string;
	printed: { name: string; value: string; unit: string | null; range: string | null; flag: string | null };
	value: number | null;
	unit: string | null;
	loinc: string | null;
	alias_group: string;
	interpretation: 'L' | 'N' | 'H' | 'LL' | 'HH' | 'A' | null;
	method: string | null;
	date: IsoDate;
	lab: string | null;
	accepted_by: 'auto' | 'person' | 'import';
	status: Lifecycle;
}

export interface TrendPoint {
	date: IsoDate;
	value: number;
	unit: string;
	printed: string; // raw printed string, shown on hover
	range: string | null;
	ref_low: number | null;
	ref_high: number | null;
	flag: string | null;
	method: string | null;
	loinc: string | null;
	lab: string | null;
	src: string;
	src_label: string | null;
	status?: Status; // lane E's status word as of the point's date
}

export interface Trend {
	name: string;
	alias_group: string;
	codes: string[];
	unit: string;
	unit_options: string[];
	points: TrendPoint[];
	annotations: { date: IsoDate; end: IsoDate | null; label: string; src: string }[];
	stats: { n: number; last_vs_previous_pct: number | null; rcv_pct: number | null; exceeds_rcv: boolean | null };
	limitations: string[];
}

// ------------------------------------------------------- lane D: genome ----

export type CallState = 'ref' | 'alt' | 'no_call' | 'not_on_array' | 'not_covered';

export interface GenomeFinding {
	id: string; // f_ + 16 hex
	category: string;
	gene: string | null;
	title: string;
	diplotype: string | null;
	phenotype: string | null;
	rsid: string | null;
	zygosity: string | null;
	evidence: 'E1' | 'E2' | 'E3' | 'E4';
	confidence: 'high' | 'medium' | 'low' | 'unconfirmed';
	call_state: CallState | null;
	verification: 'provisional' | 'confirmed' | 'refuted';
	summary: UntrustedText | null;
	drugs: string[];
	src: string;
}

// GET /profiles/{p}/genome/variant?rsid=|hgvs=|region= (read:genome_raw)
export interface VariantCall {
	source: string;
	assay: string;
	call_state: CallState;
	genotype?: string | null;
	gt?: string | null;
	zygosity?: string | null;
	dp?: number | null;
	gq?: number | null;
	basis: string;
	reason?: string;
	src: string;
}

export interface VariantLookup {
	query: string;
	variant?: { gene: string | null; chrom: string; pos: number; ref: string | null; alt: string | null; rsid: string | null };
	calls?: VariantCall[];
	withheld?: 'withheld_sensitive' | 'withheld_minor' | null;
	cited_by_findings?: string[];
	region?: { chrom: string; start: number; end: number };
	records?: (VariantCall & { chrom: string; pos: number; ref: string; alts: string[] })[];
	clipped?: { gene: string; start: number; end: number; reason: string }[];
	next_cursor?: string | null;
	limitations: string[];
	notice: string;
}

// ---------------------------------------------------- lane C: wearables, records ----

export interface WearableMetric {
	metric: string;
	label: string;
	unit: string;
	/** 'sum': value is the day total (steps, sleep minutes); 'mean': the day average */
	agg: 'sum' | 'mean';
	latest: number | null;
	latest_date: IsoDate | null;
	avg_30d: number | null;
	series: { date: IsoDate; value: number | null; min: number | null; max: number | null }[];
	coverage_pct: number;
	/** device of the newest point; each day comes from the highest-priority source that has data */
	source: string;
}

/** GET /profiles/{p}/wearables/{metric}?window=&resolution= */
export interface WearableSeries {
	metric: string;
	label: string;
	unit: string;
	agg: 'sum' | 'mean';
	resolution: 'day' | 'hour';
	window: string;
	priority: number[];
	points: {
		t: IsoDateTime;
		value: number | null;
		min: number | null;
		max: number | null;
		mean: number | null;
		median: number | null;
		p10: number | null;
		p90: number | null;
		n: number;
		coverage: number;
		source_id: number;
		device: string | null;
	}[];
}

export interface WearablesSummary {
	metrics: WearableMetric[];
	devices: { source_id: number; provider: string; device: string | null; last_sample: IsoDateTime | null; coverage_pct: number }[];
}

export interface RecordOut {
	id: Uuid;
	kind: 'condition' | 'medication' | 'allergy' | 'immunization' | 'procedure';
	title: string;
	detail: Record<string, unknown>;
	status: Lifecycle;
	status_reason: string | null;
	/** id of the version this one corrected */
	supersedes: Uuid | null;
	src: string;
	recorded_at: IsoDateTime;
}

// ---------------------------------------------------------- lane H: timeline ----

export interface TimelineEvent {
	date: IsoDate;
	type: 'lab' | 'visit' | 'imaging' | 'diagnosis' | 'medication' | 'vaccine' | 'genome';
	title: string;
	detail: string | null;
	country: string | null;
	flags: string[];
	src: string | null;
	src_label: string | null; // 'LabNorte · 2026-09-14 · p1'
	provider: string | null; // lab or clinic name when the source records one
}

// ------------------------------------------------ lane B: documents ----

export interface DocumentSummary {
	id: Uuid;
	profile_id: Uuid | null;
	filename: UntrustedText | null;
	doc_type: string | null;
	provider_name: string | null;
	collected_at: IsoDateTime | null;
	pages: number | null;
	status: 'active' | 'retracted';
	flags: Record<string, unknown>;
	open_review: number;
	snippet: UntrustedText | null; // full-text search hit, UI only
}

export interface DocumentDetail extends DocumentSummary {
	mime: string;
	size_bytes: number;
	pages_info: { page_no: number; width_pt: number; height_pt: number; text_layer_ok: boolean; flags: Record<string, unknown> }[];
}

// ---------------------------------------- lane G: tokens, audit, family ----

export interface TokenOut {
	id: Uuid;
	label: string;
	prefix: string;
	scopes: Scope[];
	profiles: string[]; // slugs
	default_profile: string | null;
	identity_mode: 'full' | 'pseudonymous' | 'minimal';
	lang: 'es' | 'en';
	units: 'si' | 'conventional' | 'as_reported';
	limits: Record<string, number>;
	created_at: IsoDateTime;
	expires_at: IsoDateTime;
	last_used_at: IsoDateTime | null;
	revoked_at: IsoDateTime | null;
}

export interface AuditOut {
	id: number;
	ts: IsoDateTime;
	actor: string; // 'Alex Rivera' or 'Claude Code · laptop (token)'
	via: string;
	action: string;
	tool: string | null;
	profile: string | null;
	rows_out: number | null;
	bytes_out: number | null;
	result_kind: string | null;
	actor_kind?: 'user' | 'token' | 'share' | 'system';
	resource_type?: string | null;
	resource_ids?: string[];
	src_ids?: string[];
	args_summary?: string | null; // codes and ids only; free text is hashed
	withheld?: Record<string, unknown> | null;
}

export interface TokenUsageDay {
	calls: number;
	facts: number;
	bytes: number;
	lookups: number;
	proposals: number;
}

export interface TokenUsage {
	token_id: Uuid;
	day: IsoDate;
	today: TokenUsageDay;
	limits: Record<string, number>;
	history: (TokenUsageDay & { day: IsoDate })[];
}

export interface Household {
	members: {
		user_id: Uuid;
		display_name: string;
		locale?: 'es' | 'en';
		logins?: { passkeys: number; oidc: boolean; password: boolean };
		enrolled?: boolean; // false: invited, has not logged in yet
		invite_pending?: boolean;
		own_profile?: string | null; // slug of the profile this login claimed
		is_me?: boolean;
	}[];
	// profiles carry, beyond ProfileSummary: can_manage, claimed, locks, genome_sensitive_agents
	profiles: (ProfileSummary & { can_manage?: boolean; claimed?: boolean; locks?: Scope[]; genome_sensitive_agents?: boolean })[];
	grants: { user_id: Uuid; profile: string; role: Role; categories: Scope[]; expires_at: IsoDateTime | null; granted_at?: IsoDateTime }[];
}

/** GET /auth/state: what the login, setup and enrol pages need. */
export interface AuthState {
	setup_needed: boolean;
	passkeys_possible: boolean; // false on plain HTTP at a LAN address: WebAuthn cannot run there
	password_allowed: boolean;
	oidc: { enabled: boolean; label: string | null };
	user: {
		id: Uuid;
		display_name: string;
		locale: 'es' | 'en';
		passkeys: number;
		recovery_codes_left: number;
		oidc_linked: boolean;
	} | null;
	method: 'passkey' | 'recovery' | 'oidc' | 'password' | null;
	auth_kind: 'session' | 'token' | 'dev' | null;
	step_up_fresh: boolean;
	step_up_seconds: number;
}

/** POST /auth/step-up/begin. Then POST /auth/step-up/finish {credential} or {password}, or go to `url`. */
export type StepUpBegin =
	| { method: 'passkey'; options: Record<string, unknown> }
	| { method: 'password' }
	| { method: 'oidc'; url: string };

/** Setup, enrol: shown once. */
export interface EnrolResult {
	user_id: Uuid;
	profile?: string;
	recovery_codes: string[];
}

export interface InviteOut {
	user_id: Uuid;
	code: string;
	url: string; // `${base}/enroll?code=...`
	expires_in_days: number;
}

// ------------------------------------------- lane I-ui-2: shapes as the backend sends them ----
// The genome and drug-check responses as the backend sends them: GenomeView, DrugCheckView.

export interface GenomeFindingFull extends Omit<GenomeFinding, 'call_state'> {
	call_state: CallState | null;
	hgvs: string | null;
	activity_score: number | null;
	value: Record<string, unknown>;
	clinvar: { variation_id: number; stars: number; native: string } | null;
	condition: { name: string; inheritance: string | null } | null;
	evidence_native: string | null;
	confidence_basis: string | null;
	status: Lifecycle;
	sensitive: boolean;
	confirm_clinically: boolean;
	limitations: UntrustedText[];
	variants: { rsid: string | null; call_state: CallState; gt: string | null; zygosity: string | null }[];
	source: { assay: string; tool: string; sample_id: string | null } | null;
}

export interface GenomeView {
	dataset: {
		assay: string;
		platform: string | null;
		mean_depth: number | null;
		build: string;
		source_tool: string;
		generated_utc: IsoDateTime | null;
		imported_at: IsoDateTime;
		db_versions: Record<string, string>;
		raw_files: Record<string, boolean>;
		src: string;
	} | null;
	tiles: { key: string; value: string; detail: string; actionable?: number; genes?: string; hidden?: number }[];
	findings: GenomeFindingFull[];
	coverage: { target: string; panel: string | null; state: 'assessed' | 'partial' | 'not_assessed'; reason: string | null }[];
	not_assessed: { gene?: string; panel: string; reason: string }[];
	withheld: { sensitive: number; minor_rule: number };
	locked: { category: string; gene: string | null }[];
	consent: { ui: boolean; agents: boolean; can_change: boolean };
	is_minor: boolean;
	limitations: string[];
	notice: string;
}

export interface DrugCheckView {
	drug: { name: string; query: string; cpic_drug_id: string | null; currently_taking: boolean; genes: string[] };
	pgx: {
		gene: string;
		diplotype: string | null;
		phenotype: string | null;
		activity_score: number | null;
		evidence: string;
		confidence: string;
		call_state: CallState | null;
		cpic_level: string | null;
		src: string;
	}[];
	phenoconversion: { gene: string; by: string; inhibitor_strength: string; computed: boolean; functional_phenotype: string | null; rule: string | null; src: string; text: string }[];
	current_medications_relevant: { name: string; since: IsoDate | null; relation: string; relation_text: string; src: string }[];
	relevant_labs: { name: string; alias_group: string; value: string | null; unit: string | null; date: IsoDate | null; status: Status; stale: boolean; reason: string; src: string | null }[];
	guidance: { source: string; level: string | null; strength: string; applies_to: string; population: string | null; quote: string; url: string }[];
	not_assessed: string[];
	limitations: string[];
	withheld: { by_scope: string[] };
	db_versions: Record<string, string>;
	notice: string;
}

export interface PasskeyOut {
	id: string;
	label: string | null;
	created_at: IsoDateTime;
	last_used_at: IsoDateTime | null;
}
