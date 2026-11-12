// Endpoints the "health" pages read (lane H owns this file): dashboard, biomarkers, timeline (genome and drug check: manage.ts).
// Backends: lane E (status.py, drug_check.py), lane A (observations.py), lane H (timeline.py), lane D (genome.py).
import { get, p } from './core';
import type { DueDoc } from '../logic/due';
import type { Calculators, Dashboard, ObservationOut, Status, Timeline, Trend, TrendPoint } from './types';

export type Units = 'si' | 'conventional' | 'as_reported';

export const getDashboard = (profile: string, lang: string, asOf?: string) =>
	get<Dashboard>(`${p(profile)}/dashboard`, { lang, as_of: asOf });
/** The Due screen: ownhealth.prevention/v1 in one language, trimmed to the caller's scopes. */
export const getDue = (profile: string, lang: string, asOf?: string) => get<DueDoc>(`${p(profile)}/due`, { lang, as_of: asOf });
export const getStatus = (profile: string, asOf?: string) => get<Record<string, unknown>>(`${p(profile)}/status`, { as_of: asOf });

export const searchObservations = (
	profile: string,
	q: { q?: string; loinc?: string; alias_group?: string; category?: string; from?: string; to?: string; units?: Units; lang?: string } = {}
) => get<{ items: ObservationOut[] }>(`${p(profile)}/observations`, q);
export const getTrend = (profile: string, code: string, window = 'P10Y', units: Units = 'as_reported', lang?: string) =>
	get<Trend>(`${p(profile)}/trend`, { code, window, units, lang });

export const getTimeline = (profile: string, q: { types?: string; from?: string; to?: string; lang?: string } = {}) =>
	get<Timeline>(`${p(profile)}/timeline`, q);

/** A trend point may carry the status word lane E computed for it (requested in .lanes/H-ui-1-requests.md). */
export type TrendPointStatus = TrendPoint & { status?: Status };

// v0.3 calculators: eGFR (CKD-EPI 2021), FIB-4, HOMA-IR, computed on read (read:labs).
export const getCalculators = (profile: string, lang: string, asOf?: string) =>
	get<Calculators>(`${p(profile)}/calculators`, { lang, as_of: asOf });
