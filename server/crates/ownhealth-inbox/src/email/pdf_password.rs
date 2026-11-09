//! Lane A: per-sender PDF passwords (ingestion.md 3.2, SPIKES-v0.2.md c). pdfium-render opens a
//! locked PDF but cannot save it unlocked (it saves with flags 0, so `/Encrypt` stays); lopdf opens it
//! with the password (`LoadOptions::with_password`), and saving without the `/Encrypt` trailer entry
//! gives an unlocked copy that PDFium reads with no password. CPU work: call from `spawn_blocking`.

use lopdf::{Document, LoadOptions};
use ownhealth_core::error::{Error, Result};
use ownhealth_core::secrets::SecretString;

/// What unlocking a PDF gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unlock {
    /// Needs no password (no `/Encrypt`, or an empty user password): import the bytes as they are.
    NotEncrypted,
    /// The password at `index` opened it; `pdf` is the unlocked copy.
    Unlocked { pdf: Vec<u8>, index: usize },
    /// Encrypted and none of the sender's passwords opened it: review as `encrypted_unknown_password`.
    NoPasswordWorked,
}

/// True when the PDF cannot be read without a password: it has an `/Encrypt` dictionary and the
/// empty user password does not open it. A file lopdf cannot parse is not called encrypted; the
/// importer reports it as it does any broken PDF.
pub fn is_encrypted(pdf: &[u8]) -> bool {
    // Without a password lopdf decrypts what the empty password opens and removes `/Encrypt` from
    // the trailer; a file that needs a real password keeps it.
    Document::load_mem(pdf).is_ok_and(|d| d.is_encrypted())
}

/// Try the sender's passwords in order. Never logs a password or says which one failed.
pub fn unlock(pdf: &[u8], passwords: &[SecretString]) -> Result<Unlock> {
    if !is_encrypted(pdf) {
        return Ok(Unlock::NotEncrypted);
    }
    for (index, password) in passwords.iter().enumerate() {
        let Ok(mut doc) = Document::load_mem_with_options(pdf, LoadOptions::with_password(password.expose())) else {
            continue;
        };
        doc.trailer.remove(b"Encrypt");
        doc.encryption_state = None;
        let mut out = Vec::with_capacity(pdf.len());
        doc.save_to(&mut out)
            .map_err(|e| Error::Invalid(format!("the unlocked copy could not be written: {e}")))?;
        return Ok(Unlock::Unlocked { pdf: out, index });
    }
    Ok(Unlock::NoPasswordWorked)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use lopdf::encryption::crypt_filters::{Aes256CryptFilter, CryptFilter};
    use lopdf::{EncryptionState, EncryptionVersion, Permissions};
    use ownhealth_core::demo_pdf::{LabReport, LabRow, lab_report_pdf};

    use super::*;

    pub(crate) fn slip() -> Vec<u8> {
        lab_report_pdf(&LabReport {
            lab: "LabNorte",
            title: "Informe de resultados de laboratorio",
            patient: "RIVERA, ALEX",
            birth: "14/05/1979",
            collected: "02/11/2026",
            report_id: "LN-20261102-AL1",
            pages: vec![vec![
                LabRow::new("Glucosa", "92", "mg/dL", "70 - 110"),
                LabRow::new("Colesterol total", "182", "mg/dL", "< 200"),
            ]],
        })
        .0
    }

    /// AES-256 (V5/R6) with a synthetic DNI-shaped user password, as a lab portal sends it.
    pub(crate) fn lock(pdf: &[u8], user_password: &str) -> Vec<u8> {
        let mut doc = Document::load_mem(pdf).expect("slip parses");
        let filter: Arc<dyn CryptFilter> = Arc::new(Aes256CryptFilter);
        let state = EncryptionState::try_from(EncryptionVersion::V5 {
            encrypt_metadata: true,
            crypt_filters: BTreeMap::from([(b"StdCF".to_vec(), filter)]),
            file_encryption_key: &[7u8; 32],
            stream_filter: b"StdCF".to_vec(),
            string_filter: b"StdCF".to_vec(),
            owner_password: "owner-labnorte",
            user_password,
            permissions: Permissions::all(),
        })
        .expect("encryption state");
        doc.encrypt(&state).expect("encrypt");
        let mut out = Vec::new();
        doc.save_to(&mut out).expect("save");
        out
    }

    fn pw(s: &str) -> SecretString {
        SecretString::new(s)
    }

    #[test]
    fn a_plain_pdf_needs_nothing() {
        let pdf = slip();
        assert!(!is_encrypted(&pdf));
        assert_eq!(unlock(&pdf, &[pw("12345678Z")]).ok(), Some(Unlock::NotEncrypted));
        assert!(!is_encrypted(b"not a pdf at all"));
    }

    #[test]
    fn the_right_password_gives_an_unlocked_copy() {
        let locked = lock(&slip(), "12345678Z");
        assert!(is_encrypted(&locked));
        let got = unlock(&locked, &[pw("00000000T"), pw("12345678Z")]).expect("unlock");
        let Unlock::Unlocked { pdf, index } = got else {
            panic!("not unlocked: {got:?}");
        };
        assert_eq!(index, 1);
        assert!(!is_encrypted(&pdf));
        assert!(!pdf.windows(8).any(|w| w == b"/Encrypt"));
        let doc = Document::load_mem(&pdf).expect("copy parses");
        let text = doc.extract_text(&[1]).expect("text");
        assert!(text.contains("Glucosa"), "{text}");
    }

    #[test]
    fn wrong_or_no_passwords_say_so() {
        let locked = lock(&slip(), "12345678Z");
        assert_eq!(unlock(&locked, &[]).ok(), Some(Unlock::NoPasswordWorked));
        assert_eq!(
            unlock(&locked, &[pw("12345678"), pw("")]).ok(),
            Some(Unlock::NoPasswordWorked)
        );
    }

    #[test]
    fn an_empty_user_password_is_no_lock() {
        let open = lock(&slip(), "");
        assert!(!is_encrypted(&open));
        assert_eq!(unlock(&open, &[]).ok(), Some(Unlock::NotEncrypted));
    }
}
