# LOINC notice

`loinc_subset.tsv` contains content from LOINC® (https://loinc.org), version 2.83: 205 codes with
their long common name, component, property, timing, system, scale, example UCUM units and status.
The rows were looked up on the HL7 FHIR terminology server (tx.fhir.org) by
`build_subset.py` from the curated list in `codes.tsv` (both in this folder). The
`alias_group` and `method` columns are ours, not LOINC content. `data/summary/ips_sections.tsv` holds
the 7 document and section codes of the International Patient Summary with their long common names,
looked up the same way; its `section` column is ours, and this notice covers it.

This material contains content from LOINC (https://loinc.org). LOINC is copyright © Regenstrief
Institute, Inc. and the Logical Observation Identifiers Names and Codes (LOINC) Committee and is
available at no cost under the license at https://loinc.org/license. LOINC® is a registered United
States trademark of Regenstrief Institute, Inc.

We ship the LOINC content unmodified; no field was translated or changed. The multilingual names in
`../aliases/` are our own reviewed aliases, not LOINC linguistic variants.
