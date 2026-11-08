//! Synthetic LabNorte lab reports for upload tests: the layout of 0.1.1 ownhealth/demo/pdf.py
//! (A4, Helvetica, text layer only), so a test can print the name of the profile it made.

const COLS: [f64; 5] = [56.0, 300.0, 352.0, 430.0, 515.0];

/// One printed result row: name, value, unit, reference range, flag.
pub struct Row<'a>(
    pub &'a str,
    pub &'a str,
    pub &'a str,
    pub &'a str,
    pub &'a str,
);

fn esc(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for ch in s.chars() {
        let b = if (ch as u32) < 0x100 && !(0x80..0xA0).contains(&(ch as u32)) {
            ch as u32 as u8
        } else {
            b'?'
        };
        if matches!(b, b'\\' | b'(' | b')') {
            out.push(b'\\');
        }
        out.push(b);
    }
    out
}

fn text(out: &mut Vec<u8>, x: f64, y: f64, s: &str, font: &str, size: f64) {
    out.extend(format!("BT /{font} {size:.1} Tf {x:.2} {y:.2} Td (").as_bytes());
    out.extend(esc(s));
    out.extend(b") Tj ET\n");
}

/// A one-page LabNorte report for `patient` ("RIVERA, ALEX"), collected on `collected` (dd/mm/yyyy).
pub fn lab_report(
    patient: &str,
    birth: &str,
    collected: &str,
    report_id: &str,
    rows: &[Row],
) -> Vec<u8> {
    let mut s = Vec::new();
    text(&mut s, 56.0, 790.0, "LabNorte", "F2", 14.0);
    text(&mut s, 56.0, 772.0, "Informe de laboratorio", "F1", 10.0);
    text(
        &mut s,
        56.0,
        748.0,
        &format!("Paciente: {patient}    F. nacimiento: {birth}"),
        "F1",
        9.0,
    );
    text(
        &mut s,
        56.0,
        734.0,
        &format!("Fecha de toma: {collected}    Informe: {report_id}    Pagina 1/1"),
        "F1",
        9.0,
    );
    for (x, h) in COLS.iter().zip([
        "Prueba",
        "Resultado",
        "Unidades",
        "Valores de referencia",
        "",
    ]) {
        if !h.is_empty() {
            text(&mut s, *x, 706.0, h, "F2", 8.5);
        }
    }
    s.extend(b"0.6 w 56 700 m 539 700 l S\n");
    let mut y = 684.0;
    for r in rows {
        for (i, (x, cell)) in COLS.iter().zip([r.0, r.1, r.2, r.3, r.4]).enumerate() {
            if !cell.is_empty() {
                text(&mut s, *x, y, cell, if i == 4 { "F2" } else { "F1" }, 9.0);
            }
        }
        y -= 16.0;
    }
    text(
        &mut s,
        56.0,
        60.0,
        "Documento sintetico de prueba. No corresponde a ningun paciente real.",
        "F1",
        7.0,
    );

    let mut objs: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [5 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Encoding /WinAnsiEncoding >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Resources << /Font << /F1 3 0 R /F2 4 0 R >> >> /Contents 6 0 R >>"
            .to_vec(),
    ];
    let mut content = format!("<< /Length {} >>\nstream\n", s.len()).into_bytes();
    content.extend(&s);
    content.extend(b"endstream");
    objs.push(content);
    let mut out: Vec<u8> = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend(body);
        out.extend(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for off in offsets {
        out.extend(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objs.len() + 1
        )
        .as_bytes(),
    );
    out
}
