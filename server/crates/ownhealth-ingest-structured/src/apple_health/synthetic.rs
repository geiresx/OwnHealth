//! Synthetic Apple Health export.zip for tests, the demo seed and the memory measurement
//! (`ingest/apple_health/synthetic.py`), with the same numbers: the noise comes from Python's
//! `random.Random(seed)` (`ownhealth_core::pyrandom`).
//!
//! The layout follows a real export (`apple_health_export/export.xml` with its DTD header, `<Me>`,
//! `Record` elements with `sourceName` and an escaped `device` string, `clinical-records/*.json`,
//! `electrocardiograms/*.csv`). Values are made up; the people are the synthetic Rivera family. The
//! XML is written as a stream, so a file of any size costs no memory to make.

use std::io::{self, Read, Write};
use std::path::Path;

use chrono::{Duration, NaiveDate};
use serde_json::{Value, json};
use zip::write::SimpleFileOptions;

use crate::py::float_repr;
use ownhealth_core::pyrandom::PyRandom;

pub const HEADER: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE HealthData [
<!ELEMENT HealthData (ExportDate,Me,(Record|Correlation|Workout|ActivitySummary|ClinicalRecord)*)>
<!ATTLIST HealthData locale CDATA #REQUIRED>
<!ELEMENT ExportDate EMPTY>
<!ATTLIST ExportDate value CDATA #REQUIRED>
<!ELEMENT Me EMPTY>
<!ELEMENT Record ((MetadataEntry|HeartRateVariabilityMetadataList)*)>
<!ELEMENT MetadataEntry EMPTY>
]>
<HealthData locale="es_ES">
"#;

pub const WATCH: &str =
    "<<HKDevice: 0x6000>, name:Apple Watch, manufacturer:Apple Inc., model:Watch, hardware:Watch7,2, software:11.1>";
pub const IPHONE: &str =
    "<<HKDevice: 0x6001>, name:iPhone, manufacturer:Apple Inc., model:iPhone, hardware:iPhone16,1, software:26.1>";

#[derive(Debug, Clone)]
pub struct ExportSpec {
    pub first_name: String,
    pub birth_date: NaiveDate,
    pub end: NaiveDate,
    pub days: i64,
    pub tz: String,
    /// steps, resting HR, HRV, sleep, VO2max
    pub watch: bool,
    /// steps too: the overlap the priority rule must not double count
    pub iphone: bool,
    /// an InBed record around each night's sleep stages
    pub in_bed: bool,
    /// nightly HRV (Apple measures SDNN)
    pub hrv: bool,
    /// > 0 adds Watch heart rate samples this often (the large-file knob)
    pub hr_every_min: i64,
    pub seed: u64,
    /// FHIR resources for clinical-records/
    pub clinical: Vec<Value>,
    pub ecg_count: usize,
}

impl Default for ExportSpec {
    fn default() -> Self {
        ExportSpec {
            first_name: "Alex".into(),
            birth_date: NaiveDate::from_ymd_opt(1979, 5, 14).unwrap_or_default(),
            end: NaiveDate::from_ymd_opt(2026, 11, 7).unwrap_or_default(),
            days: 90,
            tz: "+0100".into(),
            watch: true,
            iphone: true,
            in_bed: true,
            hrv: true,
            hr_every_min: 0,
            seed: 7,
            clinical: Vec::new(),
            ecg_count: 0,
        }
    }
}

fn ts(d: NaiveDate, h: i64, m: i64, tz: &str) -> String {
    format!("{} {h:02}:{m:02}:00 {tz}", d.format("%Y-%m-%d"))
}

/// `xml.sax.saxutils.quoteattr`.
pub fn quoteattr(s: &str) -> String {
    let mut data = s.replace('&', "&amp;").replace('>', "&gt;").replace('<', "&lt;");
    data = data.replace('\n', "&#10;").replace('\r', "&#13;").replace('\t', "&#9;");
    if data.contains('"') {
        if data.contains('\'') {
            format!("\"{}\"", data.replace('"', "&quot;"))
        } else {
            format!("'{data}'")
        }
    } else {
        format!("\"{data}\"")
    }
}

fn rec(typ: &str, src: &str, device: &str, unit: Option<&str>, start: &str, end: &str, value: &str) -> String {
    let unit_attr = unit.map_or(String::new(), |u| format!(" unit={}", quoteattr(u)));
    format!(
        "<Record type={} sourceName={} sourceVersion=\"26.1\" device={}{unit_attr} creationDate={} startDate={} \
         endDate={} value={}/>\n",
        quoteattr(typ),
        quoteattr(src),
        quoteattr(device),
        quoteattr(end),
        quoteattr(start),
        quoteattr(end),
        quoteattr(value)
    )
}

/// export.xml as a stream of text chunks: the header, one chunk per day, the clinical records, the end.
pub struct XmlChunks {
    spec: ExportSpec,
    rng: PyRandom,
    step: i64,
}

pub fn iter_xml(spec: &ExportSpec) -> XmlChunks {
    XmlChunks {
        rng: PyRandom::new(spec.seed),
        spec: spec.clone(),
        step: -1,
    }
}

impl XmlChunks {
    fn head(&self) -> String {
        let s = &self.spec;
        format!(
            "{HEADER} <ExportDate value=\"{}\"/>\n <Me HKCharacteristicTypeIdentifierDateOfBirth=\"{}\" \
             HKCharacteristicTypeIdentifierBiologicalSex=\"HKBiologicalSexNotSet\" \
             HKCharacteristicTypeIdentifierBloodType=\"HKBloodTypeNotSet\"/>\n",
            ts(s.end, 8, 0, &s.tz),
            s.birth_date.format("%Y-%m-%d")
        )
    }

    fn day(&mut self, i: i64) -> String {
        let spec = &self.spec;
        let rng = &mut self.rng;
        let tz = spec.tz.as_str();
        let watch_src = format!("Apple Watch de {}", spec.first_name);
        let phone_src = format!("iPhone de {}", spec.first_name);
        let start = spec.end - Duration::days(spec.days - 1);
        let d = start + Duration::days(i);
        let prev = d - Duration::days(1);
        let mut chunk = String::new();
        if spec.watch {
            // Sleep that started the evening before, counted on the morning it ends.
            if spec.in_bed {
                chunk.push_str(&rec(
                    "HKCategoryTypeIdentifierSleepAnalysis",
                    &watch_src,
                    WATCH,
                    None,
                    &ts(prev, 23, 10, tz),
                    &ts(d, 7, 0, tz),
                    "HKCategoryValueSleepAnalysisInBed",
                ));
            }
            let asleep_end = 6 * 60 + 30 + rng.randint(-25, 25);
            let stages = [
                (23 * 60 + 30, 24 * 60 + 90, "Core"),
                (24 * 60 + 90, 24 * 60 + 150, "Deep"),
                (24 * 60 + 150, 24 * 60 + 155, "Awake"),
                (24 * 60 + 155, 24 * 60 + asleep_end, "REM"),
            ];
            for (a, b, stage) in stages {
                let da = if a < 24 * 60 { prev } else { d };
                let db = if b < 24 * 60 { prev } else { d };
                let value = if stage == "Awake" {
                    "HKCategoryValueSleepAnalysisAwake".to_string()
                } else {
                    format!("HKCategoryValueSleepAnalysisAsleep{stage}")
                };
                chunk.push_str(&rec(
                    "HKCategoryTypeIdentifierSleepAnalysis",
                    &watch_src,
                    WATCH,
                    None,
                    &ts(da, (a / 60) % 24, a % 60, tz),
                    &ts(db, (b / 60) % 24, b % 60, tz),
                    &value,
                ));
            }
            let rhr = 56 + rng.randint(-3, 3);
            chunk.push_str(&rec(
                "HKQuantityTypeIdentifierRestingHeartRate",
                &watch_src,
                WATCH,
                Some("count/min"),
                &ts(d, 7, 5, tz),
                &ts(d, 7, 5, tz),
                &rhr.to_string(),
            ));
            if spec.hrv {
                let v = crate::py::round(46.0 + rng.uniform(-8.0, 8.0), 1);
                chunk.push_str(&rec(
                    "HKQuantityTypeIdentifierHeartRateVariabilitySDNN",
                    &watch_src,
                    WATCH,
                    Some("ms"),
                    &ts(d, 3, 15, tz),
                    &ts(d, 3, 16, tz),
                    &float_repr(v),
                ));
            }
            if i % 7 == 0 {
                let v = crate::py::round(42.0 + i as f64 / 60.0 + rng.uniform(-0.3, 0.3), 2);
                chunk.push_str(&rec(
                    "HKQuantityTypeIdentifierVO2Max",
                    &watch_src,
                    WATCH,
                    Some("mL/min·kg"),
                    &ts(d, 18, 30, tz),
                    &ts(d, 18, 30, tz),
                    &float_repr(v),
                ));
            }
        }
        for h in 7..23 {
            let steps = rng.randint(150, 900);
            if spec.watch {
                chunk.push_str(&rec(
                    "HKQuantityTypeIdentifierStepCount",
                    &watch_src,
                    WATCH,
                    Some("count"),
                    &ts(d, h, 0, tz),
                    &ts(d, h, 59, tz),
                    &steps.to_string(),
                ));
            }
            if spec.iphone && (8..=21).contains(&h) {
                chunk.push_str(&rec(
                    "HKQuantityTypeIdentifierStepCount",
                    &phone_src,
                    IPHONE,
                    Some("count"),
                    &ts(d, h, 0, tz),
                    &ts(d, h, 59, tz),
                    &((steps as f64 * 0.9) as i64).to_string(),
                ));
            }
        }
        if spec.watch && spec.hr_every_min > 0 {
            let mut m = 0;
            while m < 24 * 60 {
                let t = ts(d, m / 60, m % 60, tz);
                let v = 60 + rng.randint(-8, 30);
                chunk.push_str(&rec(
                    "HKQuantityTypeIdentifierHeartRate",
                    &watch_src,
                    WATCH,
                    Some("count/min"),
                    &t,
                    &t,
                    &v.to_string(),
                ));
                m += spec.hr_every_min;
            }
        }
        chunk
    }

    fn tail(&self) -> String {
        let mut out = String::new();
        for r in &self.spec.clinical {
            let (rt, id) = (
                r["resourceType"].as_str().unwrap_or_default(),
                r["id"].as_str().unwrap_or_default(),
            );
            out.push_str(&format!(
                " <ClinicalRecord type=\"HKClinicalTypeIdentifierLabResultRecord\" identifier=\"{id}\" \
                 sourceName=\"Hospital LabNorte\" fhirVersion=\"4.0.1\" receivedDate=\"{}\" \
                 resourceFilePath=\"/clinical-records/{rt}-{id}.json\"/>\n",
                ts(self.spec.end, 8, 0, &self.spec.tz)
            ));
        }
        out.push_str("</HealthData>\n");
        out
    }
}

impl Iterator for XmlChunks {
    type Item = String;

    fn next(&mut self) -> Option<String> {
        let step = self.step;
        self.step += 1;
        match step {
            -1 => Some(self.head()),
            i if i < self.spec.days => Some(self.day(i)),
            i if i == self.spec.days => Some(self.tail()),
            _ => None,
        }
    }
}

/// The generated XML as a `Read`, chunk by chunk (never the whole file in memory).
pub struct XmlStream {
    chunks: XmlChunks,
    buf: Vec<u8>,
    pos: usize,
}

impl XmlStream {
    pub fn new(spec: &ExportSpec) -> XmlStream {
        XmlStream {
            chunks: iter_xml(spec),
            buf: Vec::new(),
            pos: 0,
        }
    }
}

impl Read for XmlStream {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.pos >= self.buf.len() {
            match self.chunks.next() {
                Some(c) => {
                    self.buf = c.into_bytes();
                    self.pos = 0;
                }
                None => return Ok(0),
            }
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

pub fn ecg_csv(spec: &ExportSpec, n: usize) -> String {
    let d = spec.end - Duration::days(10 * (n as i64 + 1));
    let mut lines = vec![
        format!("Name,{} Rivera", spec.first_name),
        format!("Date of Birth,{}", spec.birth_date.format("%d %b %Y")),
        format!("Recorded Date,{}", ts(d, 9, 30, &spec.tz)),
        "Classification,Sinus Rhythm".into(),
        "Symptoms,".into(),
        "Software Version,2.0".into(),
        "Device,Watch7.2".into(),
        "Sample Rate,512 hertz".into(),
        String::new(),
        "Lead,Lead I".into(),
        "Unit,µV".into(),
        String::new(),
    ];
    let mut rng = PyRandom::new(n as u64);
    lines.extend((0..512).map(|_| format!("{:.3}", rng.uniform(-80.0, 300.0))));
    lines.join("\n") + "\n"
}

/// Write the zip to `path`.
pub fn write_export(path: &Path, spec: &ExportSpec) -> io::Result<()> {
    let mut file = io::BufWriter::new(std::fs::File::create(path)?);
    write_export_to(&mut file, spec)?;
    file.flush()
}

/// Write the zip to any seekable writer.
pub fn write_export_to<W: Write + io::Seek>(out: W, spec: &ExportSpec) -> io::Result<()> {
    let mut z = zip::ZipWriter::new(out);
    let opts = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .compression_level(Some(1));
    let zerr = |e: zip::result::ZipError| io::Error::other(e.to_string());
    z.start_file("apple_health_export/export.xml", opts.large_file(true))
        .map_err(zerr)?;
    for chunk in iter_xml(spec) {
        z.write_all(chunk.as_bytes())?;
    }
    for r in &spec.clinical {
        let (rt, id) = (
            r["resourceType"].as_str().unwrap_or_default(),
            r["id"].as_str().unwrap_or_default(),
        );
        z.start_file(format!("apple_health_export/clinical-records/{rt}-{id}.json"), opts)
            .map_err(zerr)?;
        z.write_all(crate::py::dumps(r).as_bytes())?;
    }
    for n in 0..spec.ecg_count {
        z.start_file(
            format!("apple_health_export/electrocardiograms/ecg_{}.csv", n + 1),
            opts,
        )
        .map_err(zerr)?;
        z.write_all(ecg_csv(spec, n).as_bytes())?;
    }
    z.finish().map_err(zerr)?.flush()
}

/// Two clinical records as Apple stores them: one lab result, one prescription.
pub fn demo_clinical(first_name: &str) -> Vec<Value> {
    let who = first_name.to_lowercase();
    vec![
        json!({
            "resourceType": "Observation",
            "id": format!("ah-{who}-ferritin"),
            "status": "final",
            "category": [{
                "coding": [{"system": "http://terminology.hl7.org/CodeSystem/observation-category", "code": "laboratory"}]
            }],
            "code": {
                "coding": [{"system": "http://loinc.org", "code": "2276-4", "display": "Ferritin"}],
                "text": "Ferritin",
            },
            "effectiveDateTime": "2026-06-02T08:15:00+02:00",
            "valueQuantity": {"value": 68, "unit": "ng/mL", "system": "http://unitsofmeasure.org", "code": "ng/mL"},
            "referenceRange": [{"low": {"value": 30}, "high": {"value": 300}}],
        }),
        json!({
            "resourceType": "MedicationRequest",
            "id": format!("ah-{who}-cetirizine"),
            "status": "active",
            "intent": "order",
            "medicationCodeableConcept": {
                "coding": [{"system": "http://www.nlm.nih.gov/research/umls/rxnorm", "code": "1014678"}],
                "text": "Cetirizine 10 mg tablet",
            },
            "authoredOn": "2026-04-03",
            "dosageInstruction": [{"text": "1 tablet daily in spring"}],
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_xml_matches_the_python_generator() {
        // python -c "from ownhealth.ingest.apple_health.synthetic import *; import hashlib;
        //   print(hashlib.sha256(''.join(iter_xml(ExportSpec(days=2, hr_every_min=60))).encode()).hexdigest())"
        let spec = ExportSpec {
            days: 2,
            hr_every_min: 60,
            ..ExportSpec::default()
        };
        let xml: String = iter_xml(&spec).collect();
        let got = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(xml.as_bytes()));
        assert_eq!(got, PYTHON_XML_SHA256);
        let ecg = ecg_csv(&spec, 1);
        let got = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(ecg.as_bytes()));
        assert_eq!(got, PYTHON_ECG_SHA256);
    }

    const PYTHON_XML_SHA256: &str = "93b4950776de0cdcf36c875c17b292ffb0bde2ea4c5812198b78fda5268e5a7b";
    const PYTHON_ECG_SHA256: &str = "c0a80a1f84c8ed41c2092d324cf9f953f985736252affba9d17e7bba43cea094";
}
