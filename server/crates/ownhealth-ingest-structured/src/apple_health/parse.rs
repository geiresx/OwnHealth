//! Streaming reader for Apple Health `export.xml` (`ingest/apple_health/parse.py`, connectors.md 2.2).
//!
//! quick-xml pulls one event at a time into a reused buffer and only the attributes of the current
//! top-level element are kept, so memory stays flat whatever the file size: a decade of Apple Watch
//! data is several GB and never fits a DOM.

use std::collections::HashMap;
use std::io::BufRead;

use chrono::{DateTime, Duration, NaiveDate, Utc};
use quick_xml::XmlVersion;
use quick_xml::events::{BytesStart, Event};

/// (Apple type, metric, [(unit as exported, factor to our unit)]).
pub type QuantitySpec = (&'static str, &'static str, &'static [(&'static str, f64)]);

/// Apple type -> (metric, [(unit as exported, factor to our unit)]).
pub const QUANTITY: [QuantitySpec; 12] = [
    ("HKQuantityTypeIdentifierStepCount", "steps", &[("count", 1.0)]),
    ("HKQuantityTypeIdentifierHeartRate", "hr", &[("count/min", 1.0)]),
    (
        "HKQuantityTypeIdentifierRestingHeartRate",
        "hr_resting",
        &[("count/min", 1.0)],
    ),
    (
        "HKQuantityTypeIdentifierHeartRateVariabilitySDNN",
        "hrv_sdnn",
        &[("ms", 1.0)],
    ),
    (
        "HKQuantityTypeIdentifierVO2Max",
        "vo2max",
        &[("mL/min·kg", 1.0), ("mL/(kg·min)", 1.0), ("ml/(kg*min)", 1.0)],
    ),
    ("HKQuantityTypeIdentifierOxygenSaturation", "spo2", &[("%", 100.0)]), // exported as a fraction
    (
        "HKQuantityTypeIdentifierRespiratoryRate",
        "resp_rate",
        &[("count/min", 1.0)],
    ),
    (
        "HKQuantityTypeIdentifierBodyMass",
        "weight",
        &[("kg", 1.0), ("lb", 0.45359237), ("g", 0.001)],
    ),
    (
        "HKQuantityTypeIdentifierBloodPressureSystolic",
        "bp_systolic",
        &[("mmHg", 1.0)],
    ),
    (
        "HKQuantityTypeIdentifierBloodPressureDiastolic",
        "bp_diastolic",
        &[("mmHg", 1.0)],
    ),
    (
        "HKQuantityTypeIdentifierActiveEnergyBurned",
        "active_energy",
        &[("kcal", 1.0), ("Cal", 1.0), ("kJ", 1.0 / 4.184)],
    ),
    (
        "HKQuantityTypeIdentifierDistanceWalkingRunning",
        "distance",
        &[("km", 1.0), ("mi", 1.609344), ("m", 0.001)],
    ),
];

pub const SLEEP_TYPE: &str = "HKCategoryTypeIdentifierSleepAnalysis";

fn sleep_metric(value: &str) -> Option<&'static str> {
    Some(match value {
        "HKCategoryValueSleepAnalysisInBed" => "sleep_in_bed",
        "HKCategoryValueSleepAnalysisAsleep"
        | "HKCategoryValueSleepAnalysisAsleepUnspecified"
        | "HKCategoryValueSleepAnalysisAsleepCore"
        | "HKCategoryValueSleepAnalysisAsleepDeep"
        | "HKCategoryValueSleepAnalysisAsleepREM" => "sleep_asleep",
        _ => return None,
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub metric: &'static str,
    /// UTC.
    pub t: DateTime<Utc>,
    pub t_end: Option<DateTime<Utc>>,
    pub v: f64,
    pub tz_offset_min: i16,
    pub device: String,
}

/// `'2026-10-01 08:00:00 +0200'` -> (UTC datetime, offset minutes). Hand-sliced, as the Python one.
pub fn parse_time(s: &str) -> Option<(DateTime<Utc>, i16)> {
    let int = |a: usize, b: usize| -> Option<i64> { s.get(a..b)?.trim().parse().ok() };
    let sign = if s.as_bytes().get(20)? == &b'-' { -1 } else { 1 };
    let off = sign * (int(21, 23)? * 60 + int(23, 25)?);
    let date = NaiveDate::from_ymd_opt(int(0, 4)? as i32, int(5, 7)? as u32, int(8, 10)? as u32)?;
    if chrono::Datelike::year(&date) < 1 {
        return None;
    }
    let local = date.and_hms_opt(int(11, 13)? as u32, int(14, 16)? as u32, int(17, 19)? as u32)?;
    let utc = local.checked_sub_signed(Duration::minutes(off))?.and_utc();
    Some((utc, i16::try_from(off).ok()?))
}

/// The device name in Apple's escaped device string (regex `name:([^,>]+)`), else the source name.
pub fn device_name(device: Option<&str>, source_name: Option<&str>) -> String {
    let d = device.unwrap_or_default();
    for (i, _) in d.match_indices("name:") {
        let rest = &d[i + 5..];
        let end = rest.find([',', '>']).unwrap_or(rest.len());
        if end > 0 {
            return rest[..end].trim().to_string();
        }
    }
    source_name.unwrap_or("unknown").trim().to_string()
}

/// A failure that stops the import: not well-formed XML.
#[derive(Debug)]
pub struct XmlError(pub String);

impl std::fmt::Display for XmlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Read `samples` once with `next_sample`; `me`, `export_date`, `clinical_paths` and `counts` fill in
/// as the file is read.
pub struct ExportReader<R: BufRead> {
    reader: quick_xml::Reader<R>,
    buf: Vec<u8>,
    depth: usize,
    done: bool,
    seen_root: bool,
    pub me: HashMap<String, String>,
    pub export_date: Option<String>,
    pub clinical_paths: Vec<String>,
    /// What was skipped, by kind, in first-seen order.
    pub counts: Vec<(String, i64)>,
    /// Where each kind sits in `counts`, so a bump is O(1) like Python's `Counter`.
    count_index: HashMap<String, usize>,
}

impl<R: BufRead> ExportReader<R> {
    pub fn new(stream: R) -> ExportReader<R> {
        let mut reader = quick_xml::Reader::from_reader(stream);
        reader.config_mut().check_end_names = true;
        ExportReader {
            reader,
            buf: Vec::with_capacity(4096),
            depth: 0,
            done: false,
            seen_root: false,
            me: HashMap::new(),
            export_date: None,
            clinical_paths: Vec::new(),
            counts: Vec::new(),
            count_index: HashMap::new(),
        }
    }

    /// The capacity of the reused event buffer (what the reader holds besides the current element).
    pub fn buffer_capacity(&self) -> usize {
        self.buf.capacity()
    }

    pub fn birth_date(&self) -> Option<NaiveDate> {
        let dob = self.me.get("HKCharacteristicTypeIdentifierDateOfBirth")?;
        crate::py::fromisoformat_date(dob)
    }

    fn bump(&mut self, key: String) {
        match self.count_index.get(&key) {
            Some(&i) => self.counts[i].1 += 1,
            None => {
                self.count_index.insert(key.clone(), self.counts.len());
                self.counts.push((key, 1));
            }
        }
    }

    /// `Counter.most_common(n)`: by count, ties in first-seen order.
    pub fn most_common(&self, n: usize) -> Vec<(String, i64)> {
        let mut all = self.counts.clone();
        all.sort_by_key(|a| std::cmp::Reverse(a.1));
        all.truncate(n);
        all
    }

    /// The next sample, or None at the end of the file.
    pub fn next_sample(&mut self) -> Result<Option<Sample>, XmlError> {
        while !self.done {
            self.buf.clear();
            let event = self
                .reader
                .read_event_into(&mut self.buf)
                .map_err(|e| XmlError(format!("{e} (at byte {})", self.reader.error_position())))?;
            let element = match event {
                Event::Start(e) => {
                    self.seen_root = true;
                    self.depth += 1;
                    if self.depth != 2 {
                        continue;
                    }
                    Some(attrs(&e)?)
                }
                Event::Empty(e) => {
                    self.seen_root = true;
                    if self.depth != 1 {
                        continue;
                    }
                    Some(attrs(&e)?)
                }
                Event::End(_) => {
                    self.depth = self.depth.saturating_sub(1);
                    None
                }
                Event::Eof => {
                    if self.depth != 0 || !self.seen_root {
                        return Err(XmlError("no element found".into()));
                    }
                    self.done = true;
                    None
                }
                _ => None,
            };
            if let Some((tag, a)) = element
                && let Some(s) = self.element(&tag, a)
            {
                return Ok(Some(s));
            }
        }
        Ok(None)
    }

    fn element(&mut self, tag: &str, a: HashMap<String, String>) -> Option<Sample> {
        match tag {
            "Record" => return self.record(&a),
            "Me" => self.me = a,
            "ExportDate" => self.export_date = a.get("value").cloned(),
            "ClinicalRecord" => {
                if let Some(p) = a.get("resourceFilePath").filter(|p| !p.is_empty()) {
                    self.clinical_paths.push(p.trim_start_matches('/').to_string());
                }
            }
            other => self.bump(format!("skipped:{other}")),
        }
        None
    }

    fn record(&mut self, a: &HashMap<String, String>) -> Option<Sample> {
        let typ = a.get("type").map(String::as_str).unwrap_or_default();
        match self.read_record(typ, a) {
            Ok(s) => s,
            Err(()) => {
                self.bump("date_invalid".into());
                None
            }
        }
    }

    /// Ok(None) for a record we skip (counted); Err for a missing or bad date or value.
    fn read_record(&mut self, typ: &str, a: &HashMap<String, String>) -> Result<Option<Sample>, ()> {
        let (t, off) = parse_time(a.get("startDate").ok_or(())?).ok_or(())?;
        let t_end = match a.get("endDate").filter(|e| !e.is_empty()) {
            Some(e) => Some(parse_time(e).ok_or(())?.0),
            None => None,
        };
        let device = || {
            device_name(
                a.get("device").map(String::as_str),
                a.get("sourceName").map(String::as_str),
            )
        };
        if typ == SLEEP_TYPE {
            let metric = sleep_metric(a.get("value").map(String::as_str).unwrap_or_default());
            let (Some(metric), Some(end)) = (metric, t_end) else {
                self.bump("skipped:sleep_awake".into());
                return Ok(None);
            };
            let minutes = (end - t).num_microseconds().map_or(f64::NAN, |us| us as f64 / 1e6) / 60.0;
            let device = device();
            return Ok(Some(Sample {
                metric,
                t,
                t_end,
                v: minutes,
                tz_offset_min: off,
                device,
            }));
        }
        let Some((_, metric, units)) = QUANTITY.iter().find(|q| q.0 == typ) else {
            self.bump(format!("skipped:{typ}"));
            return Ok(None);
        };
        let unit = a.get("unit").map(String::as_str).unwrap_or_default();
        let Some((_, factor)) = units.iter().find(|u| u.0 == unit) else {
            self.bump(format!("unit_unconvertible:{metric}"));
            return Ok(None);
        };
        let value: f64 = py_float(a.get("value").ok_or(())?).ok_or(())?;
        let device = device();
        Ok(Some(Sample {
            metric,
            t,
            t_end,
            v: value * factor,
            tz_offset_min: off,
            device,
        }))
    }
}

/// Python's `float(text)`: surrounding whitespace, underscores between digits, inf and nan.
fn py_float(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.contains('_') {
        let ok = t
            .split('_')
            .all(|p| !p.is_empty() && p.bytes().last().is_some_and(|b| b.is_ascii_digit()));
        if !ok {
            return None;
        }
        return t.replace('_', "").parse().ok();
    }
    t.parse().ok()
}

fn attrs(e: &BytesStart<'_>) -> Result<(String, HashMap<String, String>), XmlError> {
    let tag = e.name().as_ref().to_string();
    let mut out = HashMap::new();
    for a in e.attributes() {
        let a = a.map_err(|err| XmlError(err.to_string()))?;
        let key = a.key.as_ref().to_string();
        let value = a
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|err| XmlError(err.to_string()))?
            .into_owned();
        out.insert(key, value);
    }
    Ok((tag, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_are_utc_with_the_offset_kept() {
        let (t, off) = parse_time("2026-10-25 07:05:00 +0100").expect("a time");
        assert_eq!(t.to_rfc3339(), "2026-10-25T06:05:00+00:00");
        assert_eq!(off, 60);
        assert_eq!(parse_time("2026-10-25 07:05:00 -0330").map(|x| x.1), Some(-210));
        assert!(parse_time("2026-10-25").is_none());
        assert_eq!(py_float(" 1_000 "), Some(1000.0));
        assert_eq!(py_float("1__0"), None);
    }

    #[test]
    fn reads_top_level_elements_only() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE HealthData [
<!ELEMENT HealthData (ExportDate,Me)>
]>
<HealthData locale="es_ES">
 <ExportDate value="2026-11-07 08:00:00 +0100"/>
 <Me HKCharacteristicTypeIdentifierDateOfBirth="1979-05-14"/>
 <Record type="HKQuantityTypeIdentifierStepCount" sourceName="iPhone de Alex" unit="count" device="&lt;&lt;HKDevice: 0x1&gt;, name:iPhone, model:iPhone&gt;" startDate="2026-11-06 10:00:00 +0100" endDate="2026-11-06 10:59:00 +0100" value="321">
  <MetadataEntry key="HKWasUserEntered" value="1"/>
 </Record>
 <Record type="HKQuantityTypeIdentifierHeartRate" sourceName="Watch" unit="count/s" startDate="2026-11-06 10:00:00 +0100" value="1"/>
 <Record type="HKQuantityTypeIdentifierStepCount" sourceName="Watch" unit="count" startDate="bad" value="1"/>
 <Workout workoutActivityType="Run"/>
</HealthData>
"#;
        let mut r = ExportReader::new(xml.as_bytes());
        let mut got = Vec::new();
        while let Some(s) = r.next_sample().expect("parses") {
            got.push(s);
        }
        assert_eq!(got.len(), 1);
        assert_eq!(
            (got[0].metric, got[0].v, got[0].device.as_str()),
            ("steps", 321.0, "iPhone")
        );
        assert_eq!(r.birth_date(), NaiveDate::from_ymd_opt(1979, 5, 14));
        assert_eq!(r.export_date.as_deref(), Some("2026-11-07 08:00:00 +0100"));
        let counts: Vec<(String, i64)> = r.most_common(30);
        assert_eq!(
            counts,
            vec![
                ("unit_unconvertible:hr".to_string(), 1),
                ("date_invalid".to_string(), 1),
                ("skipped:Workout".to_string(), 1)
            ]
        );
        let mut broken = ExportReader::new(&b"<HealthData><Record type=\"x\"></HealthData>"[..]);
        assert!(broken.next_sample().is_err());
    }

    #[test]
    fn many_distinct_skipped_names_count_in_linear_time() {
        // Element names come from the uploaded file; counting them must stay O(1) per element.
        let n = 200_000;
        let mut xml = String::from("<HealthData>");
        for i in 0..n {
            xml.push_str(&format!("<E{i}/>"));
        }
        xml.push_str("<E7/><E7/><E3/></HealthData>");
        let started = std::time::Instant::now();
        let mut r = ExportReader::new(xml.as_bytes());
        while r.next_sample().expect("parses").is_some() {}
        let top = r.most_common(3);
        let elapsed = started.elapsed();
        assert_eq!(
            top,
            vec![
                ("skipped:E7".to_string(), 3),
                ("skipped:E3".to_string(), 2),
                ("skipped:E0".to_string(), 1)
            ]
        );
        assert!(elapsed < std::time::Duration::from_secs(5), "took {elapsed:?}");
    }
}
