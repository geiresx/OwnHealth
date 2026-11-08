//! A small PDF writer for the synthetic lab slips: the content reportlab's canvas wrote for
//! 0.1.1's `fixtures/labslips/generate.py` (one `BT .. Tm (..) Tj ET` per string, Helvetica as a standard
//! font, Bitstream Vera as an embedded TrueType font with a ToUnicode map and reportlab's code
//! assignment), uncompressed. It measures strings with the same widths, so the truth boxes match.

use std::io::Write as _;

use ownhealth_ingest_pdf::afm;

pub const PAGE_W: f64 = 595.275_590_551_181_2;
pub const PAGE_H: f64 = 841.889_763_779_527_7;

const VERA: &[u8] = include_bytes!("../../../../../fixtures/labslips/fonts/Vera.ttf");
const VERA_BOLD: &[u8] = include_bytes!("../../../../../fixtures/labslips/fonts/VeraBd.ttf");

/// reportlab's `fp_str`: at most 4 decimals, trailing zeros dropped.
pub fn num(x: f64) -> String {
    let s = format!("{x:.4}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".into() } else { s.to_string() }
}

fn be16(d: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([d[o], d[o + 1]])
}

fn be32(d: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}

/// The two tables a width needs: the Unicode cmap (3,1 format 4) and hmtx.
struct Ttf {
    data: &'static [u8],
    upem: f64,
    cmap4: usize,
    hmtx: usize,
    n_hmetrics: usize,
}

impl Ttf {
    fn parse(data: &'static [u8]) -> Ttf {
        let n = be16(data, 4) as usize;
        let table = |tag: &[u8; 4]| {
            (0..n)
                .map(|i| 12 + 16 * i)
                .find(|o| &data[*o..*o + 4] == tag)
                .map(|o| be32(data, o + 8) as usize)
                .unwrap_or_else(|| panic!("no {} table", String::from_utf8_lossy(tag)))
        };
        let (head, hhea, cmap) = (table(b"head"), table(b"hhea"), table(b"cmap"));
        let subtables = be16(data, cmap + 2) as usize;
        let cmap4 = (0..subtables)
            .map(|i| cmap + 4 + 8 * i)
            .find(|o| be16(data, *o) == 3 && be16(data, o + 2) == 1)
            .map(|o| cmap + be32(data, o + 4) as usize)
            .expect("a (3,1) cmap");
        Ttf {
            data,
            upem: be16(data, head + 18) as f64,
            cmap4,
            hmtx: table(b"hmtx"),
            n_hmetrics: be16(data, hhea + 34) as usize,
        }
    }

    fn glyph(&self, c: char) -> u16 {
        let (d, t) = (self.data, self.cmap4);
        let u = c as u32;
        if u > 0xFFFF {
            return 0;
        }
        let u = u as u16;
        let segs = be16(d, t + 6) as usize / 2;
        let (ends, starts, deltas, offs) = (t + 14, t + 16 + 2 * segs, t + 16 + 4 * segs, t + 16 + 6 * segs);
        for i in 0..segs {
            if u > be16(d, ends + 2 * i) {
                continue;
            }
            let start = be16(d, starts + 2 * i);
            if u < start {
                return 0;
            }
            let delta = be16(d, deltas + 2 * i);
            let ro = be16(d, offs + 2 * i) as usize;
            if ro == 0 {
                return u.wrapping_add(delta);
            }
            let g = be16(d, offs + 2 * i + ro + 2 * (u - start) as usize);
            return if g == 0 { 0 } else { g.wrapping_add(delta) };
        }
        0
    }

    /// Advance width per 1000 em, as reportlab's TTFontFace computed it.
    fn width(&self, c: char) -> f64 {
        let g = (self.glyph(c) as usize).min(self.n_hmetrics - 1);
        be16(self.data, self.hmtx + 4 * g) as f64 * 1000.0 / self.upem
    }
}

/// One embedded TrueType font: its code assignment as reportlab's subsetting made it.
struct Embedded {
    ttf: Ttf,
    base: &'static str,
    flags: u32,
    bbox: &'static str,
    stem_v: u32,
    /// code -> char; ASCII keeps its code, others get 1, 2, ... in order of first use
    codes: Vec<Option<char>>,
    used: bool,
}

impl Embedded {
    fn new(data: &'static [u8], base: &'static str, flags: u32, bbox: &'static str, stem_v: u32) -> Embedded {
        let mut codes = vec![None; 128];
        for (c, slot) in codes.iter_mut().enumerate().take(127).skip(32) {
            *slot = char::from_u32(c as u32);
        }
        Embedded {
            ttf: Ttf::parse(data),
            base,
            flags,
            bbox,
            stem_v,
            codes,
            used: false,
        }
    }

    fn code(&mut self, c: char) -> u8 {
        if (c as u32) >= 32 && (c as u32) < 127 {
            return c as u8;
        }
        if let Some(i) = self.codes.iter().position(|x| *x == Some(c)) {
            return i as u8;
        }
        let free = (1..32)
            .chain([127])
            .find(|i| self.codes[*i].is_none())
            .expect("a free code");
        self.codes[free] = Some(c);
        free as u8
    }
}

/// The fonts a slip can use: F1 Helvetica, F2 Helvetica-Bold, F3 Vera, F4 Vera-Bold.
pub struct Fonts {
    vera: [Embedded; 2],
}

impl Fonts {
    pub fn new() -> Fonts {
        Fonts {
            vera: [
                Embedded::new(
                    VERA,
                    "AAAAAA+BitstreamVeraSans-Roman",
                    4,
                    "-183.1055 -235.8398 1287.109 928.2227",
                    87,
                ),
                Embedded::new(
                    VERA_BOLD,
                    "AAAAAA+BitstreamVeraSans-Bold",
                    262_148,
                    "-199.2188 -235.8398 1416.992 928.2227",
                    165,
                ),
            ],
        }
    }

    /// reportlab's `stringWidth(s, font, size)`.
    pub fn width(&self, font: &str, s: &str, size: f64) -> f64 {
        let w: f64 = match font {
            "Vera" => s.chars().map(|c| self.vera[0].ttf.width(c)).sum(),
            "Vera-Bold" => s.chars().map(|c| self.vera[1].ttf.width(c)).sum(),
            _ => s.chars().map(|c| afm::width(font, c).unwrap_or(0.0)).sum(),
        };
        w * size / 1000.0
    }

    fn resource(font: &str) -> &'static str {
        match font {
            "Helvetica-Bold" => "F2",
            "Vera" => "F3",
            "Vera-Bold" => "F4",
            _ => "F1",
        }
    }

    fn encode(&mut self, font: &str, s: &str) -> Vec<u8> {
        match font {
            "Vera" | "Vera-Bold" => {
                let f = &mut self.vera[usize::from(font == "Vera-Bold")];
                f.used = true;
                s.chars().map(|c| f.code(c)).collect()
            }
            _ => s.chars().map(|c| afm::cp1252(c).unwrap_or(b'?')).collect(),
        }
    }
}

fn pdf_string(bytes: &[u8]) -> Vec<u8> {
    let mut out = vec![b'('];
    for &b in bytes {
        match b {
            b'(' | b')' | b'\\' => out.extend([b'\\', b]),
            32..=126 => out.push(b),
            _ => out.extend(format!("\\{b:03o}").bytes()),
        }
    }
    out.push(b')');
    out
}

/// One RGB image drawn over a whole page (a scan).
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

pub struct Pdf {
    pub fonts: Fonts,
    pages: Vec<(Vec<u8>, Option<usize>)>,
    cur: Vec<u8>,
    cur_image: Option<usize>,
    images: Vec<Image>,
    pub info: Vec<(String, String)>,
}

impl Pdf {
    pub fn new() -> Pdf {
        Pdf {
            fonts: Fonts::new(),
            pages: Vec::new(),
            cur: Vec::new(),
            cur_image: None,
            images: Vec::new(),
            info: Vec::new(),
        }
    }

    /// Draw `s` with its baseline at `y` (PDF coordinates).
    pub fn draw_string(&mut self, font: &str, size: f64, x: f64, y: f64, s: &str, rgb: Option<(f64, f64, f64)>) {
        if let Some((r, g, b)) = rgb {
            self.cur
                .extend(format!("{} {} {} rg\n", num(r), num(g), num(b)).bytes());
        }
        let enc = self.fonts.encode(font, s);
        self.cur.extend(
            format!(
                "BT /{} {} Tf 1 0 0 1 {} {} Tm ",
                Fonts::resource(font),
                num(size),
                num(x),
                num(y)
            )
            .bytes(),
        );
        self.cur.extend(pdf_string(&enc));
        self.cur.extend(b" Tj ET\n");
        if rgb.is_some() {
            self.cur.extend(b"0 0 0 rg\n");
        }
    }

    pub fn line(&mut self, width: f64, x0: f64, y0: f64, x1: f64, y1: f64) {
        self.cur.extend(
            format!(
                "{} w\nn {} {} m {} {} l S\n",
                num(width),
                num(x0),
                num(y0),
                num(x1),
                num(y1)
            )
            .bytes(),
        );
    }

    pub fn rect_fill(&mut self, x: f64, y: f64, w: f64, h: f64, rgb: (f64, f64, f64)) {
        self.cur.extend(
            format!(
                "{} {} {} rg\nn {} {} {} {} re f\n0 0 0 rg\n",
                num(rgb.0),
                num(rgb.1),
                num(rgb.2),
                num(x),
                num(y),
                num(w),
                num(h)
            )
            .bytes(),
        );
    }

    pub fn image_page(&mut self, img: Image) {
        self.images.push(img);
        self.cur_image = Some(self.images.len() - 1);
        self.cur
            .extend(format!("q {} 0 0 {} 0 0 cm /Im0 Do Q\n", num(PAGE_W), num(PAGE_H)).bytes());
    }

    pub fn show_page(&mut self) {
        let content = std::mem::take(&mut self.cur);
        self.pages.push((content, self.cur_image.take()));
    }

    /// Assemble the file. Object numbers: 1 catalog, 2 pages, 3 info, then fonts, pages, images.
    pub fn finish(mut self) -> Vec<u8> {
        if !self.cur.is_empty() {
            self.show_page();
        }
        let mut objs: Vec<Vec<u8>> = Vec::new();
        let mut add = |o: Vec<u8>| -> usize {
            objs.push(o);
            objs.len() + 3
        };
        let mut font_refs = vec![
            format!(
                "/F1 {} 0 R",
                add(
                    b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Name /F1 /Encoding /WinAnsiEncoding >>"
                        .to_vec()
                )
            ),
            format!(
                "/F2 {} 0 R",
                add(
                    b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Name /F2 /Encoding /WinAnsiEncoding >>"
                        .to_vec()
                )
            ),
        ];
        for (i, f) in self.fonts.vera.iter().enumerate() {
            if !f.used {
                continue;
            }
            let mut file = format!(
                "<< /Length {} /Length1 {} >>\nstream\n",
                f.ttf.data.len(),
                f.ttf.data.len()
            )
            .into_bytes();
            file.extend(f.ttf.data);
            file.extend(b"\nendstream");
            let file_ref = add(file);
            let missing = f.ttf.width('\u{FFFF}');
            let desc = add(format!(
                "<< /Type /FontDescriptor /FontName /{} /Flags {} /FontBBox [ {} ] /ItalicAngle 0 /Ascent 759.7656 \
                     /Descent -240.2344 /CapHeight 759.7656 /StemV {} /MissingWidth {} /FontFile2 {file_ref} 0 R >>",
                f.base,
                f.flags,
                f.bbox,
                f.stem_v,
                num(missing)
            )
            .into_bytes());
            let mut cmap = String::from(
                "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n/CIDSystemInfo\n<< /Registry (Adobe) \
                 /Ordering (UCS) /Supplement 0 >> def\n/CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n1 \
                 begincodespacerange\n<00> <7F>\nendcodespacerange\n",
            );
            let mapped: Vec<(usize, char)> = f
                .codes
                .iter()
                .enumerate()
                .filter_map(|(i, c)| c.map(|c| (i, c)))
                .collect();
            cmap.push_str(&format!("{} beginbfchar\n", mapped.len()));
            for (code, c) in &mapped {
                cmap.push_str(&format!("<{code:02X}> <{:04X}>\n", *c as u32));
            }
            cmap.push_str("endbfchar\nendcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");
            let tounicode = add(format!("<< /Length {} >>\nstream\n{cmap}endstream", cmap.len()).into_bytes());
            let widths: Vec<String> = f
                .codes
                .iter()
                .map(|c| num(c.map_or(missing, |c| f.ttf.width(c))))
                .collect();
            let name = if i == 0 { "F3" } else { "F4" };
            let font = add(format!(
                "<< /Type /Font /Subtype /TrueType /BaseFont /{} /Name /{name} /FirstChar 0 /LastChar 127 \
                     /Widths [ {} ] /FontDescriptor {desc} 0 R /ToUnicode {tounicode} 0 R >>",
                f.base,
                widths.join(" ")
            )
            .into_bytes());
            font_refs.push(format!("/{name} {font} 0 R"));
        }
        let mut image_refs = Vec::new();
        for img in &self.images {
            let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(6));
            z.write_all(&img.rgb).expect("in-memory zlib");
            let data = z.finish().expect("in-memory zlib");
            let mut o = format!(
                "<< /Type /XObject /Subtype /Image /Width {} /Height {} /ColorSpace /DeviceRGB /BitsPerComponent 8 \
                 /Filter /FlateDecode /Length {} >>\nstream\n",
                img.width,
                img.height,
                data.len()
            )
            .into_bytes();
            o.extend(data);
            o.extend(b"\nendstream");
            image_refs.push(add(o));
        }
        let fonts = font_refs.join(" ");
        let mut page_refs = Vec::new();
        for (content, image) in &self.pages {
            let mut c = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
            c.extend(content);
            c.extend(b"endstream");
            let content_ref = add(c);
            let xobj = image.map_or(String::new(), |i| format!(" /XObject << /Im0 {} 0 R >>", image_refs[i]));
            page_refs.push(add(format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [ 0 0 {} {} ] /Contents {content_ref} 0 R /Resources << \
                     /Font << {fonts} >>{xobj} >> /Rotate 0 >>",
                num(PAGE_W),
                num(PAGE_H)
            )
            .into_bytes()));
        }
        let kids: Vec<String> = page_refs.iter().map(|r| format!("{r} 0 R")).collect();
        let info: Vec<String> = self
            .info
            .iter()
            .map(|(k, v)| {
                let s = pdf_string(&v.chars().map(|c| afm::cp1252(c).unwrap_or(b'?')).collect::<Vec<_>>());
                format!("/{k} {}", String::from_utf8_lossy(&s))
            })
            .collect();
        let head: [Vec<u8>; 3] = [
            b"<< /Type /Catalog /Pages 2 0 R /PageMode /UseNone >>".to_vec(),
            format!(
                "<< /Type /Pages /Count {} /Kids [ {} ] >>",
                page_refs.len(),
                kids.join(" ")
            )
            .into_bytes(),
            format!("<< {} /Trapped /False >>", info.join(" ")).into_bytes(),
        ];
        let mut out = b"%PDF-1.3\n%\x93\x8c\x8b\x9e synthetic lab slip\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in head.iter().chain(objs.iter()).enumerate() {
            offsets.push(out.len());
            out.extend(format!("{} 0 obj\n", i + 1).bytes());
            out.extend(o);
            out.extend(b"\nendobj\n");
        }
        let xref = out.len();
        out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).bytes());
        for o in &offsets {
            out.extend(format!("{o:010} 00000 n \n").bytes());
        }
        out.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R /Info 3 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                offsets.len() + 1
            )
            .bytes(),
        );
        out
    }
}
