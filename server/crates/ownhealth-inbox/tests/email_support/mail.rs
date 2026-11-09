//! Lane A test support: build synthetic RFC 5322 messages (LabNorte, the Rivera family). Shared by
//! the inbox tests and the API tests (`#[path = ...]`), so it uses std only.
#![allow(dead_code)]

pub const LAB: &str = "resultados@labnorte.example";
pub const TRUSTED: &str = "mx.correo-salud.example";

/// `Authentication-Results` from the trusted receiving server with DKIM and SPF passing for `domain`.
pub fn auth_pass(domain: &str) -> String {
    format!("{TRUSTED}; dkim=pass header.d={domain} header.s=s1; spf=pass smtp.mailfrom=resultados@{domain}")
}

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len() * 4 / 3 + 4);
    for (i, c) in data.chunks(3).enumerate() {
        if i > 0 && i % 19 == 0 {
            s.push_str("\r\n");
        }
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for k in 0..4 {
            if k <= c.len() {
                s.push(T[(n >> (18 - 6 * k) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

pub struct Mail<'a> {
    pub message_id: &'a str,
    pub from: &'a str,
    pub auth: Vec<String>,
    pub subject: &'a str,
    pub body: &'a str,
    /// (filename, mime, bytes)
    pub attachments: Vec<(&'a str, &'a str, Vec<u8>)>,
}

impl<'a> Mail<'a> {
    pub fn new(message_id: &'a str, from: &'a str) -> Mail<'a> {
        Mail {
            message_id,
            from,
            auth: Vec::new(),
            subject: "Resultados de laboratorio",
            body: "Adjuntamos su informe.",
            attachments: Vec::new(),
        }
    }

    pub fn auth(mut self, header: String) -> Self {
        self.auth.push(header);
        self
    }

    pub fn attach(mut self, filename: &'a str, mime: &'a str, bytes: Vec<u8>) -> Self {
        self.attachments.push((filename, mime, bytes));
        self
    }

    pub fn build(&self) -> Vec<u8> {
        let mut m = String::new();
        for a in &self.auth {
            m.push_str(&format!("Authentication-Results: {a}\r\n"));
        }
        m.push_str(&format!(
            "Message-ID: <{}>\r\nFrom: {}\r\nTo: salud.rivera@correo-salud.example\r\nSubject: {}\r\n\
             Date: Mon, 02 Nov 2026 09:15:00 +0100\r\nMIME-Version: 1.0\r\n\
             Content-Type: multipart/mixed; boundary=\"b1\"\r\n\r\n\
             --b1\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n{}\r\n",
            self.message_id, self.from, self.subject, self.body
        ));
        for (name, mime, bytes) in &self.attachments {
            m.push_str(&format!(
                "--b1\r\nContent-Type: {mime}; name=\"{name}\"\r\nContent-Disposition: attachment; filename=\"{name}\"\r\n\
                 Content-Transfer-Encoding: base64\r\n\r\n{}\r\n",
                base64(bytes)
            ));
        }
        m.push_str("--b1--\r\n");
        m.into_bytes()
    }
}
