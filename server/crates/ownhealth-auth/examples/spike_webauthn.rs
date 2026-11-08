//! Spike (c): verify a login assertion with webauthn-rs for a passkey that py_webauthn registered.
//! Line protocol on stdin/stdout, driven by `scripts/spike_webauthn.py`:
//!   in:  {"cred_id": b64url, "public_key_hex": the stored passkey.public_key, "sign_count": n}
//!   out: the request options for navigator.credentials.get()
//!   in:  the assertion (PublicKeyCredential JSON)
//!   out: {"ok": bool, "counter": n, "user_handle": uuid, "error": ...}

use std::io::BufRead;

use base64::Engine;
use serde_json::{Value, json};
use webauthn_rs::prelude::*;
use webauthn_rs_core::proto::{
    Credential, ParsedAttestation, ParsedAttestationData, RegisteredExtensions, UserVerificationPolicy,
};

fn passkey_from_row(cred_id: Vec<u8>, public_key: &[u8], sign_count: u32) -> anyhow::Result<Passkey> {
    let cbor: serde_cbor_2::Value = serde_cbor_2::from_slice(public_key)?;
    let cred = COSEKey::try_from(&cbor).map_err(|e| anyhow::anyhow!("not a COSE key: {e:?}"))?;
    Ok(Passkey::from(Credential {
        cred_id,
        cred,
        counter: sign_count,
        transports: None,
        user_verified: true,
        backup_eligible: false,
        backup_state: false,
        registration_policy: UserVerificationPolicy::Required,
        extensions: RegisteredExtensions::none(),
        attestation: ParsedAttestation {
            data: ParsedAttestationData::None,
            metadata: AttestationMetadata::None,
        },
        attestation_format: AttestationFormat::None,
    }))
}

fn main() -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let row: Value = serde_json::from_str(&lines.next().ok_or_else(|| anyhow::anyhow!("no row"))??)?;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let cred_id = b64.decode(row["cred_id"].as_str().unwrap_or_default())?;
    let pk = hex::decode(row["public_key_hex"].as_str().unwrap_or_default())?;
    let passkey = passkey_from_row(cred_id, &pk, row["sign_count"].as_u64().unwrap_or(0) as u32)?;

    let origin = Url::parse("http://localhost:8080")?;
    let webauthn = WebauthnBuilder::new("localhost", &origin)?
        .rp_name("OwnHealth")
        .build()?;
    // Username-less login as in auth/passkeys.py: a discoverable credential, the browser picks it.
    let (options, state) = webauthn.start_discoverable_authentication()?;
    println!("{}", serde_json::to_string(&options)?);

    let assertion: PublicKeyCredential =
        serde_json::from_str(&lines.next().ok_or_else(|| anyhow::anyhow!("no assertion"))??)?;
    let out = match webauthn.identify_discoverable_authentication(&assertion) {
        Err(e) => json!({"ok": false, "error": format!("{e:?}")}),
        Ok((user, _cred)) => match webauthn.finish_discoverable_authentication_v2(&assertion, state, &passkey) {
            Ok(r) => {
                json!({"ok": true, "counter": r.counter(), "user_handle": user.to_string(), "uv": r.user_verified()})
            }
            Err(e) => json!({"ok": false, "error": format!("{e:?}")}),
        },
    };
    println!("{out}");
    Ok(())
}
