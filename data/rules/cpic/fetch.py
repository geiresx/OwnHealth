# /// script
# requires-python = ">=3.13"
# dependencies = ["httpx"]
# ///
"""Refresh the bundled CPIC snapshot from the CPIC API (maintainer tool, not used at runtime).

    uv run --script data/rules/cpic/fetch.py

Writes `snapshot.json` beside this file: the curated CPIC level A drugs below, their gene pairs and
every CPIC recommendation for them, with the recommendation text exactly as CPIC publishes it.
CPIC content fetched from CPIC's own API is released under CC0 1.0 (https://github.com/cpicpgx/cpic-data/blob/main/LICENSE.md).
Nothing from ClinPGx or PharmGKB (CC BY-SA 4.0) is bundled.
"""

from __future__ import annotations

import json
from datetime import UTC, datetime
from pathlib import Path

import httpx

API = "https://api.cpicpgx.org/v1"
OUT = Path(__file__).with_name("snapshot.json")

# Curated: CPIC level A pairs used for recommendations, the drugs a family is most likely to meet.
DRUGS = [
    "abacavir",
    "allopurinol",
    "amitriptyline",
    "atomoxetine",
    "atorvastatin",
    "azathioprine",
    "capecitabine",
    "carbamazepine",
    "celecoxib",
    "citalopram",
    "clopidogrel",
    "codeine",
    "efavirenz",
    "escitalopram",
    "fluorouracil",
    "ibuprofen",
    "lansoprazole",
    "mercaptopurine",
    "nortriptyline",
    "omeprazole",
    "ondansetron",
    "pantoprazole",
    "paroxetine",
    "phenytoin",
    "pravastatin",
    "rosuvastatin",
    "sertraline",
    "simvastatin",
    "tacrolimus",
    "tamoxifen",
    "thioguanine",
    "tramadol",
    "voriconazole",
    "warfarin",
]
# Drugs we only need the CPIC id for (CYP inhibitors in pgx_phenoconversion.yaml, common names).
IDS_ONLY = [
    "bupropion",
    "duloxetine",
    "esomeprazole",
    "fluoxetine",
    "fluvastatin",
    "fluvoxamine",
    "hydrocodone",
    "lovastatin",
    "metformin",
    "mirabegron",
    "morphine",
    "oxycodone",
    "pitavastatin",
    "quinidine",
    "terbinafine",
]
REC_FIELDS = (
    "id,guidelineid,drugid,phenotypes,activityscore,allelestatus,lookupkey,drugrecommendation,"
    "classification,population,comments"
)


def _get(client: httpx.Client, path: str, **params) -> list[dict]:
    r = client.get(f"{API}/{path}", params=params, timeout=60)
    r.raise_for_status()
    return r.json()


def _dump(snapshot: dict) -> str:
    """One record per line, keys sorted: small diffs when CPIC changes a row."""
    lines = ["{"]
    keys = sorted(snapshot)
    for i, key in enumerate(keys):
        value = snapshot[key]
        end = "," if i < len(keys) - 1 else ""
        if isinstance(value, list):
            rows = [json.dumps(r, ensure_ascii=False, sort_keys=True) for r in value]
            lines.append(f" {json.dumps(key)}: [")
            lines.extend(f"  {r}," for r in rows[:-1])
            lines.extend(f"  {r}" for r in rows[-1:])
            lines.append(f" ]{end}")
        else:
            lines.append(f" {json.dumps(key)}: {json.dumps(value, ensure_ascii=False)}{end}")
    lines.append("}")
    return "\n".join(lines) + "\n"


def main() -> None:
    names = ",".join(DRUGS)
    with httpx.Client() as c:
        drugs = _get(c, "drug", name=f"in.({names})", select="drugid,name,rxnormid,guidelineid")
        ids = ",".join(d["drugid"] for d in drugs)
        extra = _get(c, "drug", name=f"in.({','.join(IDS_ONLY)})", select="drugid,name,rxnormid")
        pairs = _get(
            c,
            "pair_view",
            drugid=f"in.({ids})",
            cpiclevel="eq.A",
            usedforrecommendation="eq.Yes",
            select="drugid,drugname,genesymbol,guidelinename,guidelineurl,cpiclevel,pmids",
        )
        gids = ",".join(sorted({str(d["guidelineid"]) for d in drugs if d["guidelineid"]}))
        guidelines = _get(c, "guideline", id=f"in.({gids})", select="id,name,url,genes")
        recs = _get(c, "recommendation", drugid=f"in.({ids})", select=REC_FIELDS, order="id")
    missing = sorted(set(DRUGS + IDS_ONLY) - {d["name"] for d in drugs + extra})
    if missing:
        raise SystemExit(f"CPIC does not know: {missing}")
    snapshot = {
        "source": "CPIC API https://api.cpicpgx.org/v1",
        "license": "CC0-1.0 (https://github.com/cpicpgx/cpic-data/blob/main/LICENSE.md)",
        "retrieved": datetime.now(UTC).date().isoformat(),
        "drugs": sorted(drugs, key=lambda d: d["name"]),
        "drugs_ids_only": sorted(extra, key=lambda d: d["name"]),
        "pairs": sorted(pairs, key=lambda p: (p["drugname"], p["genesymbol"])),
        "guidelines": sorted(guidelines, key=lambda g: g["id"]),
        "recommendations": recs,
    }
    OUT.write_text(_dump(snapshot))
    print(f"{OUT}: {len(drugs)} drugs, {len(pairs)} pairs, {len(recs)} recommendations")


if __name__ == "__main__":
    main()
