// The Genome screen's PGx gene grid: the words of one gene box, in the reader's language.

/** The parts of a pgx_grid row these helpers read. */
export interface GridRow {
	status: 'found' | 'no_finding' | 'partial' | 'not_assessed' | 'withheld';
	diplotype: string | null;
	phenotype: string | null;
	markers: { marker: string; present: boolean | null }[];
	coverage: { state: string; reason: string | null } | null;
	sv_coverage?: { state: string; reason: string | null } | null;
}

type T = (key: string) => string;

/** The coverage reasons OwnGenome writes (genomics.md, genome-facts coverage[].reason). */
export const COVERAGE_REASONS = [
	'not_on_array',
	'low_depth',
	'paralog',
	'sv_not_callable',
	'no_bam',
	'required_positions_missing',
	'not_run',
	'assay_cannot_detect',
	'other'
] as const;

/** A coverage reason in the reader's words; null for a code we have no words for (never the raw code). */
export function reasonText(code: string | null | undefined, t: T): string | null {
	if (!code) return null;
	const k = 'gen_grid_reason_' + code;
	const s = t(k);
	return s === k ? null : s;
}

/** A coverage state and its reason: "partial coverage · a similar gene nearby (paralog)". */
function coverageText(state: string, reason: string | null | undefined, t: T): string {
	const word = state === 'partial' ? t('gen_grid_partial') : t('gen_grid_not_assessed');
	return [word, reasonText(reason, t)].filter(Boolean).join(' · ');
}

/** An HLA marker's presence: a value the file does not state as true or false is unknown, never absent. */
function markerWord(p: unknown, t: T): string {
	return p === true ? t('i2_present') : p === false ? t('i2_absent') : t('gen_grid_marker_unknown');
}

/** The call of one gene box, or why there is none. */
export function gridCall(r: GridRow, t: T): string {
	if (r.status === 'withheld') return t('gen_grid_withheld');
	if (r.status === 'no_finding') return t('gen_grid_no_finding');
	if (r.status === 'partial') return coverageText('partial', r.coverage?.reason, t);
	if (r.status === 'not_assessed') {
		const why = r.coverage?.reason;
		return why === 'not_in_coverage_map' ? t('gen_grid_not_in_map') : coverageText('not_assessed', why, t);
	}
	if (r.markers.length && !r.phenotype) {
		return r.markers.map((m) => `${m.marker.replace(/^HLA-/, '')} ${markerWord(m.present, t)}`).join(' · ');
	}
	return [r.diplotype, r.phenotype].filter(Boolean).join(' · ') || '—';
}

/** The gene's structural variants when they were not fully assessed (CYP2D6 from a chip or without a BAM). */
export function svNote(r: GridRow, t: T): string | null {
	const sv = r.sv_coverage;
	if (!sv || sv.state === 'assessed') return null;
	return `${t('gen_grid_sv')}: ${coverageText(sv.state, sv.reason, t)}`;
}

/** The parts of a finding the result cell reads. */
export interface ResultRow {
	category: string;
	diplotype?: string | null;
	phenotype?: string | null;
	zygosity?: string | null;
	condition?: { name?: string | null } | null;
	value?: Record<string, unknown> | null;
	prs?: { status: string; reason_code: string | null } | null;
}

type TF = (key: string, vars: Record<string, string>) => string;

/** The result cell of one finding. A refused PRS says so before anything its fact carries. */
export function resultCell(f: ResultRow, t: T, tf: TF): string {
	const v = (k: string) => f.value?.[k] as string | number | boolean | string[] | undefined;
	if (f.category === 'prs' && f.prs?.status === 'not_interpretable') return t('gen_prs_ni');
	if (f.diplotype || f.phenotype) return [f.diplotype, f.phenotype].filter(Boolean).join(' · ');
	if (f.category === 'prs') return tf('i2_prs_pct', { p: String(v('percentile') ?? '—'), pop: String(v('most_similar_pop') ?? '') });
	if (f.category === 'hla') return `${(v('alleles') as string[] | undefined)?.join(' · ') ?? ''} · ${v('marker')} ${markerWord(v('present'), t)}`;
	if (f.category === 'trait') return `${v('trait') ?? ''} ${v('call') ?? ''}`.trim();
	return [f.zygosity ? t('i2_zyg_' + f.zygosity) : null, f.condition?.name].filter(Boolean).join(' · ');
}

/** The parts of the overview the empty state reads. */
export interface OverviewLike {
	findings: unknown[];
	locked: unknown[];
	withheld: { minor_rule?: number | null };
	pgx_grid?: { status: string }[] | null;
}

/** No genome data to show: no dataset, or a dataset with nothing in the table, nothing locked, nothing
 *  withheld for a minor and no call in the gene grid (which also reads calls below the table level). */
export function genomeEmpty(g: OverviewLike, hasDataset: boolean): boolean {
	if (!hasDataset) return true;
	return !g.findings.length && !g.locked.length && !g.withheld.minor_rule && !(g.pgx_grid ?? []).some((r) => r.status === 'found');
}

/** Limitation codes the Genome screen words itself; any other line shows the server's text. */
const LIMIT_KEYS: Record<string, string> = {
	no_genome: 'i2_lim_no_genome',
	no_vcf: 'i2_lim_no_vcf',
	variant_only_vcf: 'i2_lim_variant_only',
	chip_unconfirmed: 'gen_lim_chip',
	no_cram: 'i2_lim_no_cram'
};

/** The limitation lines under the coverage note, in the reader's language. Two datasets can give the same line; it shows once. */
export function limitLines(
	texts: string[],
	codes: ({ code: string; assay?: string | null } | null | undefined)[] | null | undefined,
	tf: TF
): string[] {
	const lines = texts.map((text, i) => {
		const c = codes?.[i];
		const key = c && LIMIT_KEYS[c.code];
		return key ? tf(key, { kind: (c.assay ?? '').toUpperCase() }) : text;
	});
	return [...new Set(lines)];
}

/**
 * The release part of the Genome subtitle, in the genome file's order. ClinVar is the release in use: the newest
 * the weekly check compared, else the genome file's own. A genome file that names no ClinVar release still shows
 * the compared one.
 */
export function releaseParts(dbVersions: Record<string, string>, clinvarNow: string | null): string[] {
	const parts = Object.entries(dbVersions)
		.filter(([k]) => k === 'clinvar' || k === 'cpic')
		.map(([k, v]) => (k === 'clinvar' ? `ClinVar ${clinvarNow ?? v}` : `CPIC ${v}`));
	return clinvarNow && !('clinvar' in dbVersions) ? [...parts, `ClinVar ${clinvarNow}`] : parts;
}
