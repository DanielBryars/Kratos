use std::{collections::BTreeMap, env, sync::Arc, time::Duration};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::{DateTime, TimeDelta, Utc};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use utoipa::ToSchema;

const AUTHORIZATION_LIFETIME_SECONDS: i64 = 600;
const SESSION_LIFETIME_SECONDS: i64 = 7 * 24 * 60 * 60;
const METADATA_TOKEN_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";

#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct ResumableUploadSession {
    /// Secret GCS session URI created once by the control plane.
    pub uri: String,
    pub method: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredObjectMetadata {
    pub bucket: String,
    pub object_key: String,
    pub generation: i64,
    pub byte_length: i64,
    pub crc32c: String,
    pub sha256: String,
}

#[derive(Debug, Error)]
pub enum ArtifactStorageError {
    #[error("artifact storage is temporarily unavailable")]
    Unavailable,
    #[error("artifact object does not exist")]
    NotFound,
    #[error("artifact storage returned an invalid response")]
    InvalidResponse,
}

#[async_trait]
pub trait ArtifactStorage: Send + Sync {
    async fn initiate_resumable_upload(
        &self,
        object_key: &str,
        media_type: &str,
        byte_length: u64,
        sha256: &str,
        issued_at: DateTime<Utc>,
    ) -> Result<ResumableUploadSession, ArtifactStorageError>;

    async fn object_metadata(
        &self,
        bucket: &str,
        object_key: &str,
        generation: i64,
    ) -> Result<StoredObjectMetadata, ArtifactStorageError>;

    async fn protect_verified_object(
        &self,
        bucket: &str,
        object_key: &str,
        generation: i64,
    ) -> Result<(), ArtifactStorageError>;

    async fn delete_object(
        &self,
        bucket: &str,
        object_key: &str,
        generation: i64,
    ) -> Result<(), ArtifactStorageError>;

    async fn cancel_resumable_upload(&self, session_uri: &str) -> Result<(), ArtifactStorageError>;

    fn bucket(&self) -> &str;
}

#[derive(Clone)]
pub struct ArtifactStorageClient(Arc<dyn ArtifactStorage>);

impl ArtifactStorageClient {
    #[must_use]
    pub fn new(storage: impl ArtifactStorage + 'static) -> Self {
        Self(Arc::new(storage))
    }

    /// Creates exactly one resumable session for an object without exposing initiation authority.
    ///
    /// # Errors
    /// Returns an error when the backing signer cannot create the authorization.
    pub async fn initiate_resumable_upload(
        &self,
        object_key: &str,
        media_type: &str,
        byte_length: u64,
        sha256: &str,
        issued_at: DateTime<Utc>,
    ) -> Result<ResumableUploadSession, ArtifactStorageError> {
        self.0
            .initiate_resumable_upload(object_key, media_type, byte_length, sha256, issued_at)
            .await
    }

    /// Reads one immutable object generation from the authoritative store.
    ///
    /// # Errors
    /// Returns an error when metadata is unavailable, missing, or malformed.
    pub async fn object_metadata(
        &self,
        bucket: &str,
        object_key: &str,
        generation: i64,
    ) -> Result<StoredObjectMetadata, ArtifactStorageError> {
        self.0.object_metadata(bucket, object_key, generation).await
    }

    /// Protects a verified object from the unverified-upload lifecycle rule.
    ///
    /// # Errors
    /// Returns an error when the exact object generation cannot be updated.
    pub async fn protect_verified_object(
        &self,
        bucket: &str,
        object_key: &str,
        generation: i64,
    ) -> Result<(), ArtifactStorageError> {
        self.0
            .protect_verified_object(bucket, object_key, generation)
            .await
    }

    /// Deletes a rejected object at its exact generation.
    ///
    /// # Errors
    /// Returns an error when the exact object generation cannot be deleted.
    pub async fn delete_object(
        &self,
        bucket: &str,
        object_key: &str,
        generation: i64,
    ) -> Result<(), ArtifactStorageError> {
        self.0.delete_object(bucket, object_key, generation).await
    }

    /// Cancels a resumable session URI that is no longer authorised.
    ///
    /// # Errors
    /// Returns an error when the session cannot be reached or refuses cancellation.
    pub async fn cancel_resumable_upload(
        &self,
        session_uri: &str,
    ) -> Result<(), ArtifactStorageError> {
        self.0.cancel_resumable_upload(session_uri).await
    }

    #[must_use]
    pub fn bucket(&self) -> &str {
        self.0.bucket()
    }
}

#[derive(Debug, Error)]
pub enum ArtifactStorageConfigError {
    #[error(
        "KRATOS_ARTIFACT_BUCKET and KRATOS_ARTIFACT_SIGNER_SERVICE_ACCOUNT must be set together"
    )]
    Incomplete,
    #[error("artifact storage HTTP client could not be created")]
    HttpClient,
}

/// Builds the production storage adapter when both required variables are present.
///
/// # Errors
/// Returns an error for partial configuration or an unusable HTTP client.
pub fn artifact_storage_from_environment()
-> Result<Option<ArtifactStorageClient>, ArtifactStorageConfigError> {
    let bucket = env::var("KRATOS_ARTIFACT_BUCKET").ok();
    let signer = env::var("KRATOS_ARTIFACT_SIGNER_SERVICE_ACCOUNT").ok();
    match (bucket, signer) {
        (None, None) => Ok(None),
        (Some(bucket), Some(signer)) if !bucket.trim().is_empty() && !signer.trim().is_empty() => {
            let client = Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .map_err(|_| ArtifactStorageConfigError::HttpClient)?;
            Ok(Some(ArtifactStorageClient::new(GoogleArtifactStorage {
                client,
                bucket,
                signer_service_account: signer,
            })))
        }
        _ => Err(ArtifactStorageConfigError::Incomplete),
    }
}

struct GoogleArtifactStorage {
    client: Client,
    bucket: String,
    signer_service_account: String,
}

#[derive(Deserialize)]
struct MetadataTokenResponse {
    access_token: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SignBlobResponse {
    signed_blob: String,
}

#[derive(Deserialize)]
struct GcsObjectResponse {
    bucket: String,
    name: String,
    generation: String,
    size: String,
    crc32c: String,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
}

struct UploadSigningMaterial {
    canonical_uri: String,
    canonical_query: String,
    canonical_headers: String,
    signed_headers: &'static str,
    timestamp: String,
    scope: String,
    headers: BTreeMap<String, String>,
}

fn upload_signing_material(
    bucket: &str,
    signer_service_account: &str,
    object_key: &str,
    media_type: &str,
    byte_length: u64,
    sha256: &str,
    issued_at: DateTime<Utc>,
) -> UploadSigningMaterial {
    let date = issued_at.format("%Y%m%d").to_string();
    let timestamp = issued_at.format("%Y%m%dT%H%M%SZ").to_string();
    let scope = format!("{date}/auto/storage/goog4_request");
    let credential = format!("{signer_service_account}/{scope}");
    let signed_headers = "content-type;host;x-goog-content-sha256;x-goog-if-generation-match;x-goog-meta-kratos-sha256;x-goog-resumable;x-upload-content-length";
    let canonical_uri = format!(
        "/{}/{}",
        percent_encode(bucket, false),
        percent_encode(object_key, true)
    );
    let mut query = [
        ("X-Goog-Algorithm", "GOOG4-RSA-SHA256".to_owned()),
        ("X-Goog-Credential", credential),
        ("X-Goog-Date", timestamp.clone()),
        ("X-Goog-Expires", AUTHORIZATION_LIFETIME_SECONDS.to_string()),
        ("X-Goog-SignedHeaders", signed_headers.to_owned()),
    ];
    query.sort_by(|left, right| left.0.cmp(right.0));
    let canonical_query = query
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                percent_encode(key, false),
                percent_encode(value, false)
            )
        })
        .collect::<Vec<_>>()
        .join("&");
    let canonical_headers = format!(
        "content-type:{media_type}\nhost:storage.googleapis.com\nx-goog-content-sha256:UNSIGNED-PAYLOAD\nx-goog-if-generation-match:0\nx-goog-meta-kratos-sha256:{sha256}\nx-goog-resumable:start\nx-upload-content-length:{byte_length}\n"
    );
    let headers = BTreeMap::from([
        ("content-type".to_owned(), media_type.to_owned()),
        (
            "x-goog-content-sha256".to_owned(),
            "UNSIGNED-PAYLOAD".to_owned(),
        ),
        ("x-goog-if-generation-match".to_owned(), "0".to_owned()),
        ("x-goog-meta-kratos-sha256".to_owned(), sha256.to_owned()),
        ("x-goog-resumable".to_owned(), "start".to_owned()),
        (
            "x-upload-content-length".to_owned(),
            byte_length.to_string(),
        ),
    ]);
    UploadSigningMaterial {
        canonical_uri,
        canonical_query,
        canonical_headers,
        signed_headers,
        timestamp,
        scope,
        headers,
    }
}

impl GoogleArtifactStorage {
    async fn access_token(&self) -> Result<String, ArtifactStorageError> {
        let response = self
            .client
            .get(METADATA_TOKEN_URL)
            .header("Metadata-Flavor", "Google")
            .send()
            .await
            .map_err(|_| ArtifactStorageError::Unavailable)?;
        if !response.status().is_success() {
            return Err(ArtifactStorageError::Unavailable);
        }
        response
            .json::<MetadataTokenResponse>()
            .await
            .map(|response| response.access_token)
            .map_err(|_| ArtifactStorageError::InvalidResponse)
    }

    async fn sign_blob(&self, payload: &[u8]) -> Result<Vec<u8>, ArtifactStorageError> {
        let token = self.access_token().await?;
        let signer = percent_encode(&self.signer_service_account, false);
        let url = format!(
            "https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/{signer}:signBlob"
        );
        let response = self
            .client
            .post(url)
            .bearer_auth(token)
            .json(&serde_json::json!({ "payload": STANDARD.encode(payload) }))
            .send()
            .await
            .map_err(|_| ArtifactStorageError::Unavailable)?;
        if !response.status().is_success() {
            return Err(ArtifactStorageError::Unavailable);
        }
        let response = response
            .json::<SignBlobResponse>()
            .await
            .map_err(|_| ArtifactStorageError::InvalidResponse)?;
        STANDARD
            .decode(response.signed_blob)
            .map_err(|_| ArtifactStorageError::InvalidResponse)
    }
}

#[async_trait]
impl ArtifactStorage for GoogleArtifactStorage {
    async fn initiate_resumable_upload(
        &self,
        object_key: &str,
        media_type: &str,
        byte_length: u64,
        sha256: &str,
        issued_at: DateTime<Utc>,
    ) -> Result<ResumableUploadSession, ArtifactStorageError> {
        let material = upload_signing_material(
            &self.bucket,
            &self.signer_service_account,
            object_key,
            media_type,
            byte_length,
            sha256,
            issued_at,
        );
        let canonical_request = format!(
            "POST\n{}\n{}\n{}\n{}\nUNSIGNED-PAYLOAD",
            material.canonical_uri,
            material.canonical_query,
            material.canonical_headers,
            material.signed_headers
        );
        let canonical_hash = hex_lower(&Sha256::digest(canonical_request.as_bytes()));
        let string_to_sign = format!(
            "GOOG4-RSA-SHA256\n{}\n{}\n{canonical_hash}",
            material.timestamp, material.scope
        );
        let signature = hex_lower(&self.sign_blob(string_to_sign.as_bytes()).await?);
        let signed_url = format!(
            "https://storage.googleapis.com{}?{}&X-Goog-Signature={signature}",
            material.canonical_uri, material.canonical_query
        );
        let response = self
            .client
            .post(signed_url)
            .headers(
                material
                    .headers
                    .iter()
                    .map(|(name, value)| {
                        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                            .map_err(|_| ArtifactStorageError::InvalidResponse)?;
                        let value = reqwest::header::HeaderValue::from_str(value)
                            .map_err(|_| ArtifactStorageError::InvalidResponse)?;
                        Ok((name, value))
                    })
                    .collect::<Result<reqwest::header::HeaderMap, ArtifactStorageError>>()?,
            )
            .send()
            .await
            .map_err(|_| ArtifactStorageError::Unavailable)?;
        if !response.status().is_success() {
            return Err(ArtifactStorageError::Unavailable);
        }
        let uri = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .filter(|value| value.starts_with("https://storage.googleapis.com/"))
            .ok_or(ArtifactStorageError::InvalidResponse)?
            .to_owned();
        Ok(ResumableUploadSession {
            uri,
            method: "PUT".to_owned(),
            expires_at: issued_at + TimeDelta::seconds(SESSION_LIFETIME_SECONDS),
        })
    }

    async fn object_metadata(
        &self,
        bucket: &str,
        object_key: &str,
        generation: i64,
    ) -> Result<StoredObjectMetadata, ArtifactStorageError> {
        if bucket != self.bucket {
            return Err(ArtifactStorageError::InvalidResponse);
        }
        let token = self.access_token().await?;
        let url = format!(
            "https://storage.googleapis.com/storage/v1/b/{}/o/{}",
            percent_encode(bucket, false),
            percent_encode(object_key, false)
        );
        let response = self
            .client
            .get(url)
            .query(&[("generation", generation.to_string())])
            .bearer_auth(token)
            .send()
            .await
            .map_err(|_| ArtifactStorageError::Unavailable)?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(ArtifactStorageError::NotFound);
        }
        if !response.status().is_success() {
            return Err(ArtifactStorageError::Unavailable);
        }
        let response = response
            .json::<GcsObjectResponse>()
            .await
            .map_err(|_| ArtifactStorageError::InvalidResponse)?;
        Ok(StoredObjectMetadata {
            bucket: response.bucket,
            object_key: response.name,
            generation: response
                .generation
                .parse()
                .map_err(|_| ArtifactStorageError::InvalidResponse)?,
            byte_length: response
                .size
                .parse()
                .map_err(|_| ArtifactStorageError::InvalidResponse)?,
            crc32c: response.crc32c,
            sha256: response
                .metadata
                .get("kratos-sha256")
                .cloned()
                .unwrap_or_default(),
        })
    }

    fn bucket(&self) -> &str {
        &self.bucket
    }

    async fn protect_verified_object(
        &self,
        bucket: &str,
        object_key: &str,
        generation: i64,
    ) -> Result<(), ArtifactStorageError> {
        if bucket != self.bucket {
            return Err(ArtifactStorageError::InvalidResponse);
        }
        let token = self.access_token().await?;
        let url = format!(
            "https://storage.googleapis.com/storage/v1/b/{}/o/{}",
            percent_encode(bucket, false),
            percent_encode(object_key, false)
        );
        let response = self
            .client
            .patch(url)
            .query(&[("ifGenerationMatch", generation.to_string())])
            .bearer_auth(token)
            .json(&serde_json::json!({ "temporaryHold": true }))
            .send()
            .await
            .map_err(|_| ArtifactStorageError::Unavailable)?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(ArtifactStorageError::NotFound);
        }
        if response.status().is_success() {
            Ok(())
        } else {
            Err(ArtifactStorageError::Unavailable)
        }
    }

    async fn delete_object(
        &self,
        bucket: &str,
        object_key: &str,
        generation: i64,
    ) -> Result<(), ArtifactStorageError> {
        if bucket != self.bucket {
            return Err(ArtifactStorageError::InvalidResponse);
        }
        let token = self.access_token().await?;
        let url = format!(
            "https://storage.googleapis.com/storage/v1/b/{}/o/{}",
            percent_encode(bucket, false),
            percent_encode(object_key, false)
        );
        let response = self
            .client
            .delete(url)
            .query(&[("ifGenerationMatch", generation.to_string())])
            .bearer_auth(token)
            .send()
            .await
            .map_err(|_| ArtifactStorageError::Unavailable)?;
        if response.status().is_success() {
            Ok(())
        } else if response.status() == StatusCode::NOT_FOUND {
            Err(ArtifactStorageError::NotFound)
        } else {
            Err(ArtifactStorageError::Unavailable)
        }
    }

    async fn cancel_resumable_upload(&self, session_uri: &str) -> Result<(), ArtifactStorageError> {
        if !session_uri.starts_with("https://storage.googleapis.com/") {
            return Err(ArtifactStorageError::InvalidResponse);
        }
        let response = self
            .client
            .delete(session_uri)
            .header(reqwest::header::CONTENT_LENGTH, "0")
            .send()
            .await
            .map_err(|_| ArtifactStorageError::Unavailable)?;
        if response.status().is_success() || matches!(response.status().as_u16(), 404 | 410 | 499) {
            Ok(())
        } else {
            Err(ArtifactStorageError::Unavailable)
        }
    }
}

fn percent_encode(value: &str, preserve_slash: bool) -> String {
    use std::fmt::Write as _;

    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'~')
            || (preserve_slash && byte == b'/')
        {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            let _ = write!(encoded, "{byte:02X}");
        }
    }
    encoded
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};

    use super::{percent_encode, upload_signing_material};

    #[test]
    fn encoding_preserves_only_canonical_path_separators() {
        assert_eq!(
            percent_encode("v1/owners/a b/+", true),
            "v1/owners/a%20b/%2B"
        );
        assert_eq!(percent_encode("a/b", false), "a%2Fb");
    }

    #[test]
    fn xml_resumable_fixture_signs_generation_and_size_as_headers() {
        let issued_at = DateTime::parse_from_rfc3339("2026-09-20T09:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let material = upload_signing_material(
            "kratos-artifacts",
            "upload@example.iam.gserviceaccount.com",
            "v1/owners/owner/artifacts/object",
            "application/octet-stream",
            512,
            &"b".repeat(64),
            issued_at,
        );

        assert!(!material.canonical_query.contains("ifGenerationMatch"));
        assert_eq!(
            material.headers.get("x-goog-if-generation-match"),
            Some(&"0".to_owned())
        );
        assert_eq!(
            material.headers.get("x-upload-content-length"),
            Some(&"512".to_owned())
        );
        assert_eq!(
            material.canonical_headers,
            format!(
                "content-type:application/octet-stream\nhost:storage.googleapis.com\nx-goog-content-sha256:UNSIGNED-PAYLOAD\nx-goog-if-generation-match:0\nx-goog-meta-kratos-sha256:{}\nx-goog-resumable:start\nx-upload-content-length:512\n",
                "b".repeat(64)
            )
        );
        assert_eq!(
            material.signed_headers,
            "content-type;host;x-goog-content-sha256;x-goog-if-generation-match;x-goog-meta-kratos-sha256;x-goog-resumable;x-upload-content-length"
        );
    }
}
