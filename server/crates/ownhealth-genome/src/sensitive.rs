//! OwnHealth's own sensitive gene list (genomics.md 7), applied on top of the producer's tag
//! (`genome/sensitive.py`). APOE, the ACMG SF cancer and cardiovascular genes, HTT and other
//! predictive repeat loci, as padded GRCh38 spans so a raw lookup (rs429358 for APOE) cannot bypass
//! the opt-in. The carrier-panel spans sit beside them for the minor rule. Both lists are fixed.

use std::collections::BTreeSet;
use std::sync::LazyLock;

pub const SPAN_PAD_BP: i64 = 2_000;

const SENSITIVE_TSV: &str = include_str!("../../../../data/genome/sensitive_genes.tsv");
const CARRIER_TSV: &str = include_str!("../../../../data/genome/carrier_genes.tsv");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneSpan {
    pub gene: String,
    pub chrom: String,
    /// 1-based, closed, padded.
    pub start: i64,
    pub end: i64,
    pub group: String,
}

/// gene, chrom, start, end[, group, ...] rows of a data file; `group` overrides the fifth column.
fn load(text: &str, group: Option<&str>) -> Vec<GeneSpan> {
    let mut rows = Vec::new();
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') || line.starts_with("gene\t") {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        let (Some(gene), Some(chrom), Some(start), Some(end)) = (cols.first(), cols.get(1), cols.get(2), cols.get(3))
        else {
            continue;
        };
        let (Ok(start), Ok(end)) = (start.trim().parse::<i64>(), end.trim().parse::<i64>()) else {
            continue;
        };
        let span_group = group.or(cols.get(4).copied()).unwrap_or_default();
        rows.push(GeneSpan {
            gene: (*gene).to_string(),
            chrom: (*chrom).to_string(),
            start: (start - SPAN_PAD_BP).max(1),
            end: end + SPAN_PAD_BP,
            group: span_group.to_string(),
        });
    }
    rows
}

static SPANS: LazyLock<Vec<GeneSpan>> = LazyLock::new(|| load(SENSITIVE_TSV, None));
static CARRIER_SPANS: LazyLock<Vec<GeneSpan>> = LazyLock::new(|| load(CARRIER_TSV, Some("carrier")));
static SENSITIVE_GENES: LazyLock<BTreeSet<String>> =
    LazyLock::new(|| SPANS.iter().map(|s| s.gene.to_uppercase()).collect());

pub fn sensitive_genes() -> &'static BTreeSet<String> {
    &SENSITIVE_GENES
}

pub fn is_sensitive_gene(gene: Option<&str>) -> bool {
    gene.is_some_and(|g| !g.is_empty() && SENSITIVE_GENES.contains(&g.trim().to_uppercase()))
}

/// `chr` + the upper-case contig, with 23/25 -> X, 24 -> Y, 26 and MT -> M.
pub fn norm_chrom(chrom: &str) -> String {
    let mut c = chrom.trim();
    if let (Some(head), Some(rest)) = (c.get(..3), c.get(3..))
        && head.eq_ignore_ascii_case("chr")
    {
        c = rest;
    }
    let up = c.to_uppercase();
    let mapped = match up.as_str() {
        "23" | "25" => "X",
        "24" => "Y",
        "26" | "MT" => "M",
        other => other,
    };
    format!("chr{mapped}")
}

fn overlapping(spans: &'static [GeneSpan], chrom: &str, start: i64, end: i64) -> Vec<&'static GeneSpan> {
    let c = norm_chrom(chrom);
    spans
        .iter()
        .filter(|s| s.chrom == c && s.start <= end && s.end >= start)
        .collect()
}

pub fn spans_overlapping(chrom: &str, start: i64, end: i64) -> Vec<&'static GeneSpan> {
    overlapping(&SPANS, chrom, start, end)
}

/// The carrier-panel genes: carrier facts sit inside them (importer rule 9), and a minor's raw
/// lookups inside them are withheld (genomics.md 7).
pub fn carrier_spans_overlapping(chrom: &str, start: i64, end: i64) -> Vec<&'static GeneSpan> {
    overlapping(&CARRIER_SPANS, chrom, start, end)
}

pub fn sensitive_gene_at(chrom: &str, pos: i64) -> Option<&'static str> {
    spans_overlapping(chrom, pos, pos).first().map(|s| s.gene.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_load_and_pad() {
        assert!(is_sensitive_gene(Some("apoe ")) && is_sensitive_gene(Some("BRCA1")));
        assert!(!is_sensitive_gene(Some("CYP2C19")) && !is_sensitive_gene(None) && !is_sensitive_gene(Some("")));
        assert_eq!(sensitive_gene_at("19", 44908684), Some("APOE"));
        assert!(!carrier_spans_overlapping("chr7", 117559590, 117559590).is_empty());
        assert!(CARRIER_SPANS.len() > 3000);
    }

    #[test]
    fn chrom_names() {
        assert_eq!(norm_chrom("chr7"), "chr7");
        assert_eq!(norm_chrom("x"), "chrX");
        assert_eq!(norm_chrom("23"), "chrX");
        assert_eq!(norm_chrom("MT"), "chrM");
        assert_eq!(norm_chrom("ChrM"), "chrM");
    }
}
