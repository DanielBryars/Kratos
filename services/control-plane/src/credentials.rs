use std::fmt;

use argon2::{
    Argon2, PasswordHash, PasswordHasher, PasswordVerifier,
    password_hash::{SaltString, rand_core::OsRng as SaltOsRng},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{RngCore, rngs::OsRng};
use uuid::Uuid;

const SECRET_BYTES: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialKind {
    Enrolment,
    Worker,
}

impl CredentialKind {
    const fn prefix(self) -> &'static str {
        match self {
            Self::Enrolment => "ken",
            Self::Worker => "kwc",
        }
    }
}

pub struct PlaintextCredential(String);

impl PlaintextCredential {
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PlaintextCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PlaintextCredential([REDACTED])")
    }
}

pub struct IssuedCredential {
    pub id: Uuid,
    pub plaintext: PlaintextCredential,
    pub verifier: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error("credential format is invalid")]
    InvalidFormat,
    #[error("credential verifier could not be created")]
    Hash,
}

/// Creates a new high-entropy credential and its Argon2id verifier.
///
/// # Errors
///
/// Returns [`CredentialError::Hash`] if the verifier cannot be produced.
pub fn issue(kind: CredentialKind) -> Result<IssuedCredential, CredentialError> {
    let id = Uuid::new_v4();
    let mut secret = [0_u8; SECRET_BYTES];
    OsRng.fill_bytes(&mut secret);
    let encoded = URL_SAFE_NO_PAD.encode(secret);
    let plaintext = PlaintextCredential(format!("{}_{}_{}", kind.prefix(), id.simple(), encoded));
    let salt = SaltString::generate(&mut SaltOsRng);
    let verifier = Argon2::default()
        .hash_password(plaintext.expose().as_bytes(), &salt)
        .map_err(|_| CredentialError::Hash)?
        .to_string();

    Ok(IssuedCredential {
        id,
        plaintext,
        verifier,
    })
}

/// Extracts the non-secret lookup identifier after validating the credential envelope.
///
/// # Errors
///
/// Returns [`CredentialError::InvalidFormat`] for a malformed credential or the wrong credential
/// kind.
pub fn identifier(kind: CredentialKind, credential: &str) -> Result<Uuid, CredentialError> {
    let mut parts = credential.splitn(3, '_');
    let prefix = parts.next().ok_or(CredentialError::InvalidFormat)?;
    let id = parts.next().ok_or(CredentialError::InvalidFormat)?;
    let secret = parts.next().ok_or(CredentialError::InvalidFormat)?;
    if prefix != kind.prefix()
        || secret.len() != 43
        || !secret
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(CredentialError::InvalidFormat);
    }
    Uuid::parse_str(id).map_err(|_| CredentialError::InvalidFormat)
}

#[must_use]
pub fn verify(credential: &str, verifier: &str) -> bool {
    let Ok(hash) = PasswordHash::new(verifier) else {
        return false;
    };
    Argon2::default()
        .verify_password(credential.as_bytes(), &hash)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::{CredentialKind, identifier, issue, verify};

    #[test]
    fn issued_credentials_are_identifiable_and_verifiable() {
        let issued = issue(CredentialKind::Worker).unwrap();
        assert_eq!(
            identifier(CredentialKind::Worker, issued.plaintext.expose()).unwrap(),
            issued.id
        );
        assert!(verify(issued.plaintext.expose(), &issued.verifier));
        assert!(!verify("kwc_invalid_wrong", &issued.verifier));
    }

    #[test]
    fn credential_debug_output_is_redacted() {
        let issued = issue(CredentialKind::Enrolment).unwrap();
        let debug = format!("{:?}", issued.plaintext);
        assert_eq!(debug, "PlaintextCredential([REDACTED])");
        assert!(!debug.contains(issued.plaintext.expose()));
    }

    #[test]
    fn credential_kinds_cannot_be_confused() {
        let issued = issue(CredentialKind::Enrolment).unwrap();
        assert!(identifier(CredentialKind::Worker, issued.plaintext.expose()).is_err());
    }

    #[test]
    fn url_safe_underscore_in_secret_is_valid() {
        let credential =
            "kwc_00112233445566778899aabbccddeeff_AAAAAAAAAAAAAAAAAAAAA_AAAAAAAAAAAAAAAAAAAAA";

        assert!(identifier(CredentialKind::Worker, credential).is_ok());
    }
}
