//! A tiny PDF writer for synthetic lab reports (demo seed and tests), byte for byte the output of
//! `demo/pdf.py`. Text layer only, Helvetica. It knows where it drew each row, so the bboxes it
//! returns match the page (PDF points, origin top-left).

pub const PAGE_W: f64 = 595.0;
pub const PAGE_H: f64 = 842.0;
/// name, value, unit, range, flag
pub const COLS: [f64; 5] = [56.0, 300.0, 352.0, 430.0, 515.0];
pub const ROW_H: f64 = 16.0;
pub const FONT_SIZE: f64 = 9.0;

pub type BBox = [f64; 4];

#[derive(Debug, Clone, PartialEq, Default)]
pub struct LabRow {
    pub name: String,
    pub value: String,
    pub unit: String,
    pub r#ref: String,
    pub flag: String,
}

impl LabRow {
    pub fn new(name: &str, value: &str, unit: &str, r#ref: &str) -> LabRow {
        LabRow {
            name: name.into(),
            value: value.into(),
            unit: unit.into(),
            r#ref: r#ref.into(),
            flag: String::new(),
        }
    }

    pub fn flagged(mut self, flag: &str) -> LabRow {
        self.flag = flag.into();
        self
    }

    fn cells(&self) -> [&str; 5] {
        [&self.name, &self.value, &self.unit, &self.r#ref, &self.flag]
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct LabReport<'a> {
    pub lab: &'a str,
    pub title: &'a str,
    pub patient: &'a str,
    pub birth: &'a str,
    pub collected: &'a str,
    pub report_id: &'a str,
    pub pages: Vec<Vec<LabRow>>,
}

/// Python `s.encode("cp1252", errors="replace")`.
fn cp1252(s: &str) -> Vec<u8> {
    const HIGH: [(char, u8); 27] = [
        ('€', 0x80),
        ('‚', 0x82),
        ('ƒ', 0x83),
        ('„', 0x84),
        ('…', 0x85),
        ('†', 0x86),
        ('‡', 0x87),
        ('ˆ', 0x88),
        ('‰', 0x89),
        ('Š', 0x8A),
        ('‹', 0x8B),
        ('Œ', 0x8C),
        ('Ž', 0x8E),
        ('‘', 0x91),
        ('’', 0x92),
        ('“', 0x93),
        ('”', 0x94),
        ('•', 0x95),
        ('–', 0x96),
        ('—', 0x97),
        ('˜', 0x98),
        ('™', 0x99),
        ('š', 0x9A),
        ('›', 0x9B),
        ('œ', 0x9C),
        ('ž', 0x9E),
        ('Ÿ', 0x9F),
    ];
    s.chars()
        .map(|c| match c as u32 {
            0..=0x7F | 0xA0..=0xFF => c as u8,
            _ => HIGH.iter().find(|(h, _)| *h == c).map_or(b'?', |(_, b)| *b),
        })
        .collect()
}

fn esc(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for b in cp1252(s) {
        if matches!(b, b'\\' | b'(' | b')') {
            out.push(b'\\');
        }
        out.push(b);
    }
    out
}

fn text(out: &mut Vec<u8>, x: f64, y: f64, s: &str, font: &str, size: f64) {
    out.extend_from_slice(format!("BT /{font} {size:.1} Tf {x:.2} {y:.2} Td (").as_bytes());
    out.extend_from_slice(&esc(s));
    out.extend_from_slice(b") Tj ET\n");
}

fn width(s: &str) -> f64 {
    s.chars().count() as f64 * FONT_SIZE * 0.52
}

/// Python `round(x, 2)`.
fn round2(x: f64) -> f64 {
    format!("{x:.2}").parse().unwrap_or(x)
}

/// Render a report. Returns (pdf bytes, bboxes per page per row, top-left origin).
pub fn lab_report_pdf(r: &LabReport<'_>) -> (Vec<u8>, Vec<Vec<BBox>>) {
    let n = r.pages.len();
    let mut streams = Vec::new();
    let mut boxes = Vec::new();
    for (i, rows) in r.pages.iter().enumerate() {
        let page_no = i + 1;
        let mut out = Vec::new();
        text(&mut out, 56.0, 790.0, r.lab, "F2", 14.0);
        text(&mut out, 56.0, 772.0, r.title, "F1", 10.0);
        text(
            &mut out,
            56.0,
            748.0,
            &format!("Paciente: {}    F. nacimiento: {}", r.patient, r.birth),
            "F1",
            9.0,
        );
        text(
            &mut out,
            56.0,
            734.0,
            &format!(
                "Fecha de toma: {}    Informe: {}    Pagina {page_no}/{n}",
                r.collected, r.report_id
            ),
            "F1",
            9.0,
        );
        let headers = ["Prueba", "Resultado", "Unidades", "Valores de referencia", ""];
        for (x, h) in COLS.iter().zip(headers) {
            if !h.is_empty() {
                text(&mut out, *x, 706.0, h, "F2", 8.5);
            }
        }
        out.extend_from_slice(b"0.6 w 56 700 m 539 700 l S\n");
        let mut page_boxes = Vec::new();
        let mut y = 684.0;
        for row in rows {
            let cells = row.cells();
            for (j, (x, cell)) in COLS.iter().zip(cells).enumerate() {
                if !cell.is_empty() {
                    text(&mut out, *x, y, cell, if j == 4 { "F2" } else { "F1" }, FONT_SIZE);
                }
            }
            let right = COLS
                .iter()
                .zip(cells)
                .filter(|(_, c)| !c.is_empty())
                .map(|(x, c)| x + width(c))
                .fold(f64::NEG_INFINITY, f64::max);
            let top = PAGE_H - (y + FONT_SIZE * 0.78);
            let bottom = PAGE_H - (y - FONT_SIZE * 0.22);
            page_boxes.push([COLS[0], round2(top), round2(right), round2(bottom)]);
            y -= ROW_H;
        }
        text(
            &mut out,
            56.0,
            60.0,
            "Documento sintetico de prueba. No corresponde a ningun paciente real.",
            "F1",
            7.0,
        );
        streams.push(out);
        boxes.push(page_boxes);
    }
    (assemble(&streams), boxes)
}

fn assemble(streams: &[Vec<u8>]) -> Vec<u8> {
    let n = streams.len();
    // objects: 1 catalog, 2 pages, 3 font F1, 4 font F2, then per page: page obj, content obj
    let kids = (0..n)
        .map(|i| format!("{} 0 R", 5 + 2 * i))
        .collect::<Vec<_>>()
        .join(" ");
    let mut objs: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        format!("<< /Type /Pages /Kids [{kids}] /Count {n} >>").into_bytes(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Encoding /WinAnsiEncoding >>".to_vec(),
    ];
    for (i, stream) in streams.iter().enumerate() {
        let content_ref = 6 + 2 * i;
        objs.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {PAGE_W:.0} {PAGE_H:.0}] \
                 /Resources << /Font << /F1 3 0 R /F2 4 0 R >> >> /Contents {content_ref} 0 R >>"
            )
            .into_bytes(),
        );
        let mut body = format!("<< /Length {} >>\nstream\n", stream.len()).into_bytes();
        body.extend_from_slice(stream);
        body.extend_from_slice(b"endstream");
        objs.push(body);
    }
    let mut out: Vec<u8> = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::new();
    for (num, body) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", num + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for off in offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objs.len() + 1
        )
        .as_bytes(),
    );
    out
}
