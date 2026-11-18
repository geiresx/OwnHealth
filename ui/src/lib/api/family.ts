// Family history and the pedigree (v0.3 Insight): server/crates/ownhealth-api/src/routes/family_history.rs.
import { del, get, p, post, put } from './core';
import type { Uuid } from './types';

export type Sex = 'female' | 'male' | 'unknown';

export interface FamilyCondition {
	kind: string;
	text?: string | null;
	code_system?: string | null;
	code?: string | null;
	onset_age?: number | null;
	onset_approx?: boolean;
}

export interface FamilyMember {
	id: Uuid;
	relationship: string;
	display_name: string | null;
	/** The name to show: the one recorded, else the linked profile's when the caller can read it. */
	name: string | null;
	sex_at_birth: Sex | null;
	born_year: number | null;
	deceased: boolean;
	death_age: number | null;
	conditions: FamilyCondition[];
	sensitivity: string[];
	linked: boolean;
	linked_profile: { slug: string; display_name: string } | null;
	/** Linked to a profile the caller cannot read: no name, years or conditions. */
	masked: boolean;
	src: string;
}

export interface FamilyCatalog {
	version: string;
	relationships: { code: string; en: string; es: string; generation: number; side: string; sex: Sex | null }[];
	conditions: { kind: string; en: string; es: string; short_en: string; short_es: string; group: string }[];
}

export interface PedigreeCondition {
	kind: string;
	text: string | null;
	en: string | null;
	es: string | null;
	short_en: string | null;
	short_es: string | null;
	group: string | null;
	onset_age: number | null;
	onset_approx: boolean;
}

export interface PedigreeNode {
	id: Uuid;
	relationship: string;
	label: { en: string; es: string };
	generation: number;
	side: string;
	degree: number;
	slot: number;
	union: string | null;
	child_of: string | null;
	sex_at_birth: Sex | null;
	name: string | null;
	age: number | null;
	deceased: boolean;
	death_age: number | null;
	conditions: PedigreeCondition[];
	linked: boolean;
	masked: boolean;
	src: string;
}

export interface Pedigree {
	profile: {
		slug: string;
		name: string;
		sex_at_birth: Sex | null;
		age: number | null;
		deceased: boolean;
		slot: number;
		union: string;
		child_of: string;
	};
	as_of: string;
	rules: string;
	nodes: PedigreeNode[];
	/** ownhealth.family/v1: the guideline patterns these same relatives show, worded in es and en. */
	patterns: FamilyPatterns;
}

export interface FamilyPattern {
	rule: string;
	status: 'present' | 'not_assessed';
	reason_code?: string;
	basis: 'guideline' | 'pack_default';
	source: string;
	cite: string;
	/** The relatives (node ids) the pattern rests on, in drawing order. */
	members: Uuid[];
	rel: string[];
	kinds: string[];
	src: string[];
	title: { en: string; es: string };
	why: { en: string; es: string };
	reason?: { en: string; es: string };
}

export interface FamilyPatterns {
	schema: 'ownhealth.family/v1';
	profile: string;
	as_of: string;
	rules: string;
	items: FamilyPattern[];
	/** Cross-profile hints (data/family/hints.yaml), present only when one fires. */
	hints?: FamilyHint[];
	family_hash: string;
}

/** A relative who shares their results with this profile meets a cited test. No name, value or src. */
export interface FamilyHint {
	rule: string;
	category: ShareCategory;
	/** The RoleCode of this profile's relative row linked to the person who shared. */
	rel: string;
	analyte?: string;
	gene?: string;
	/** The gene is withheld from this caller: the hint reads as a shared genetic finding. */
	withheld?: boolean;
	basis: 'guideline' | 'pack_default';
	source: string;
	cite: string;
	title: { en: string; es: string };
	why: { en: string; es: string };
}

export type ShareCategory = 'labs' | 'cascade';

/** GET /profiles/{p}/family-shares: what this profile lets feed hints in each other profile. */
export interface FamilyShares {
	profile: string;
	/** A minor's results never feed hints. */
	minor: boolean;
	categories: ShareCategory[];
	items: {
		/** The slug, or the id of a masked profile. */
		profile: string;
		/** Null when masked: the caller cannot read that profile. */
		display_name: string | null;
		masked: boolean;
		minor: boolean | null;
		/** The caller reads that profile's family history; without it the degree is unknown, not absent. */
		history: boolean;
		/** The blood degree of that profile's relative row linked to this one, or null. */
		degree: number | null;
		categories: ShareCategory[];
		granted_at: string | null;
	}[];
}

/** A new version: the whole relative. `linked_profile` absent keeps the link, null removes it. */
export interface MemberIn {
	relationship: string;
	display_name?: string | null;
	sex_at_birth?: Sex | null;
	born_year?: number | null;
	deceased?: boolean;
	death_age?: number | null;
	conditions?: FamilyCondition[];
	/** Sent back unchanged on an edit, so a new version keeps the row's sensitivity. */
	sensitivity?: string[];
	linked_profile?: string | null;
}

export const listFamily = (profile: string) => get<{ items: FamilyMember[]; catalog: FamilyCatalog }>(`${p(profile)}/family`);
export const getPedigree = (profile: string) => get<Pedigree>(`${p(profile)}/pedigree`);
export const addFamilyMember = (profile: string, body: MemberIn) => post<FamilyMember>(`${p(profile)}/family`, body);
export const updateFamilyMember = (profile: string, id: Uuid, body: MemberIn) => put<FamilyMember>(`${p(profile)}/family/${id}`, body);
export const removeFamilyMember = (profile: string, id: Uuid) => del<{ id: Uuid; status: 'retracted' }>(`${p(profile)}/family/${id}`);
export const listFamilyShares = (profile: string) => get<FamilyShares>(`${p(profile)}/family-shares`);
/** The whole set shared with `target`; an empty set revokes. Adding a category needs step-up. */
export const putFamilyShare = (profile: string, target: string, categories: ShareCategory[]) =>
	put<{ profile: string; target: string; categories: ShareCategory[] }>(`${p(profile)}/family-shares`, { target, categories });
