//! Test helpers (feature `testing`): a software WebAuthn authenticator (P-256, `none` attestation,
//! user verification on), the same one 0.1.1's `tests/security/softkey.py` was, and an ES256 signer
//! for a fake OIDC provider's ID tokens.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub fn b64u(b: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(b)
}

pub fn unb64u(s: &str) -> Vec<u8> {
    URL_SAFE_NO_PAD.decode(s.trim_end_matches('=')).unwrap_or_default()
}

/// A random P-256 key (or the one with this scalar).
pub fn signing_key(scalar: Option<&[u8]>) -> SigningKey {
    if let Some(s) = scalar
        && let Ok(k) = SigningKey::from_slice(s)
    {
        return k;
    }
    loop {
        let mut raw = [0u8; 32];
        rand::fill(&mut raw[..]);
        if let Ok(k) = SigningKey::from_slice(&raw) {
            return k;
        }
    }
}

/// The public point's x and y.
pub fn public_xy(key: &SigningKey) -> (Vec<u8>, Vec<u8>) {
    let point = key.verifying_key().to_sec1_point(false);
    let b = point.as_bytes();
    (b[1..33].to_vec(), b[33..65].to_vec())
}

/// A compact ES256 JWT over `claims`.
pub fn es256_jwt(key: &SigningKey, kid: &str, claims: &Value) -> String {
    let header = json!({"alg": "ES256", "typ": "JWT", "kid": kid});
    let input = format!(
        "{}.{}",
        b64u(header.to_string().as_bytes()),
        b64u(claims.to_string().as_bytes())
    );
    let sig: Signature = key.sign(input.as_bytes());
    format!("{input}.{}", b64u(&sig.to_bytes()))
}

/// A JWK of the key, for a JWKS document.
pub fn jwk(key: &SigningKey, kid: &str) -> Value {
    let (x, y) = public_xy(key);
    json!({"kty": "EC", "crv": "P-256", "x": b64u(&x), "y": b64u(&y), "kid": kid, "use": "sig", "alg": "ES256"})
}

// ---- a tiny CBOR writer: enough for a COSE key and an attestation object ----

pub enum Cbor {
    Int(i64),
    Bytes(Vec<u8>),
    Text(String),
    Map(Vec<(Cbor, Cbor)>),
}

fn head(out: &mut Vec<u8>, major: u8, n: u64) {
    let m = major << 5;
    match n {
        0..=23 => out.push(m | n as u8),
        24..=0xff => out.extend([m | 24, n as u8]),
        0x100..=0xffff => {
            out.push(m | 25);
            out.extend((n as u16).to_be_bytes());
        }
        _ => {
            out.push(m | 26);
            out.extend((n as u32).to_be_bytes());
        }
    }
}

pub fn cbor(v: &Cbor) -> Vec<u8> {
    let mut out = Vec::new();
    write_cbor(&mut out, v);
    out
}

fn write_cbor(out: &mut Vec<u8>, v: &Cbor) {
    match v {
        Cbor::Int(i) if *i >= 0 => head(out, 0, *i as u64),
        Cbor::Int(i) => head(out, 1, (-1 - *i) as u64),
        Cbor::Bytes(b) => {
            head(out, 2, b.len() as u64);
            out.extend(b);
        }
        Cbor::Text(t) => {
            head(out, 3, t.len() as u64);
            out.extend(t.as_bytes());
        }
        Cbor::Map(m) => {
            head(out, 5, m.len() as u64);
            for (k, v) in m {
                write_cbor(out, k);
                write_cbor(out, v);
            }
        }
    }
}

/// A software passkey.
pub struct SoftKey {
    pub rp_id: String,
    pub origin: String,
    pub key: SigningKey,
    pub cred_id: Vec<u8>,
    pub count: u32,
    pub user_handle: Option<Vec<u8>>,
}

impl Default for SoftKey {
    fn default() -> Self {
        SoftKey::new("testserver", "https://testserver")
    }
}

impl SoftKey {
    pub fn new(rp_id: &str, origin: &str) -> SoftKey {
        let mut cred_id = vec![0u8; 32];
        rand::fill(&mut cred_id[..]);
        SoftKey {
            rp_id: rp_id.into(),
            origin: origin.into(),
            key: signing_key(None),
            cred_id,
            count: 0,
            user_handle: None,
        }
    }

    fn client(&self, kind: &str, challenge: &str, origin: Option<&str>) -> Vec<u8> {
        // json.dumps key order and separators, as softkey.py
        format!(
            "{{\"type\": \"{kind}\", \"challenge\": \"{challenge}\", \"origin\": \"{}\", \"crossOrigin\": false}}",
            origin.unwrap_or(&self.origin)
        )
        .into_bytes()
    }

    fn auth_data(&mut self, flags: u8, extra: &[u8], rp_id: Option<&str>) -> Vec<u8> {
        self.count += 1;
        let mut out = Sha256::digest(rp_id.unwrap_or(&self.rp_id).as_bytes()).to_vec();
        out.push(flags);
        out.extend(self.count.to_be_bytes());
        out.extend(extra);
        out
    }

    /// The COSE public key, as stored in `passkey.public_key`.
    pub fn cose(&self) -> Vec<u8> {
        let (x, y) = public_xy(&self.key);
        cbor(&Cbor::Map(vec![
            (Cbor::Int(1), Cbor::Int(2)),
            (Cbor::Int(3), Cbor::Int(-7)),
            (Cbor::Int(-1), Cbor::Int(1)),
            (Cbor::Int(-2), Cbor::Bytes(x)),
            (Cbor::Int(-3), Cbor::Bytes(y)),
        ]))
    }

    /// `navigator.credentials.create()` answering these options.
    pub fn create(&mut self, options: &Value, origin: Option<&str>) -> Value {
        self.user_handle = Some(unb64u(options["user"]["id"].as_str().unwrap_or_default()));
        let mut attested = vec![0u8; 16];
        attested.extend((self.cred_id.len() as u16).to_be_bytes());
        attested.extend(&self.cred_id);
        attested.extend(self.cose());
        let auth = self.auth_data(0x01 | 0x04 | 0x40, &attested, None);
        let att = cbor(&Cbor::Map(vec![
            (Cbor::Text("fmt".into()), Cbor::Text("none".into())),
            (Cbor::Text("attStmt".into()), Cbor::Map(vec![])),
            (Cbor::Text("authData".into()), Cbor::Bytes(auth)),
        ]));
        let client = self.client(
            "webauthn.create",
            options["challenge"].as_str().unwrap_or_default(),
            origin,
        );
        json!({
            "id": b64u(&self.cred_id),
            "rawId": b64u(&self.cred_id),
            "type": "public-key",
            "response": {
                "clientDataJSON": b64u(&client),
                "attestationObject": b64u(&att),
                "transports": ["internal"],
            },
            "clientExtensionResults": {},
        })
    }

    /// `navigator.credentials.get()` answering these options.
    pub fn get(&mut self, options: &Value, origin: Option<&str>, rp_id: Option<&str>) -> Value {
        let client = self.client(
            "webauthn.get",
            options["challenge"].as_str().unwrap_or_default(),
            origin,
        );
        let auth = self.auth_data(0x01 | 0x04, &[], rp_id);
        let mut msg = auth.clone();
        msg.extend(Sha256::digest(&client));
        let sig: Signature = self.key.sign(&msg);
        json!({
            "id": b64u(&self.cred_id),
            "rawId": b64u(&self.cred_id),
            "type": "public-key",
            "response": {
                "clientDataJSON": b64u(&client),
                "authenticatorData": b64u(&auth),
                "signature": b64u(sig.to_der().as_bytes()),
                "userHandle": b64u(self.user_handle.as_deref().unwrap_or_default()),
            },
            "clientExtensionResults": {},
        })
    }
}

/// Every entry of a zip, by name.
pub fn unzip(bytes: &[u8]) -> anyhow::Result<std::collections::BTreeMap<String, Vec<u8>>> {
    use std::io::Read;
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    let mut out = std::collections::BTreeMap::new();
    for i in 0..z.len() {
        let mut f = z.by_index(i)?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        out.insert(f.name()?.to_string(), buf);
    }
    Ok(out)
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

pub use p256::ecdsa::SigningKey as EcKey;

/// The PKCE `S256` challenge of a verifier.
pub fn s256(verifier: &str) -> String {
    b64u(&Sha256::digest(verifier.as_bytes()))
}
