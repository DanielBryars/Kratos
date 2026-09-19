use std::{env, fmt, sync::Arc, time::Duration};

use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};

const LOOKUP_URL: &str = "https://identitytoolkit.googleapis.com/v1/accounts:lookup";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HumanIdentity {
    pub(crate) subject: String,
    pub(crate) email: String,
    pub(crate) display_name: String,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum VerifyError {
    #[error("identity token was rejected")]
    Rejected,
    #[error("identity provider is unavailable")]
    Unavailable,
}

#[async_trait]
pub(crate) trait IdentityVerifier: Send + Sync {
    async fn verify(&self, id_token: &str) -> Result<HumanIdentity, VerifyError>;
}

#[derive(Clone)]
pub struct HumanAuth {
    verifier: Arc<dyn IdentityVerifier>,
    bootstrap_operator_email: Arc<str>,
}

impl fmt::Debug for HumanAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HumanAuth")
            .field("verifier", &"[REDACTED]")
            .field("bootstrap_operator_email", &"[REDACTED]")
            .finish()
    }
}

impl HumanAuth {
    pub(crate) fn new(verifier: Arc<dyn IdentityVerifier>, bootstrap_operator_email: &str) -> Self {
        Self {
            verifier,
            bootstrap_operator_email: Arc::from(normalize_email(bootstrap_operator_email)),
        }
    }

    pub(crate) async fn verify(&self, id_token: &str) -> Result<HumanIdentity, VerifyError> {
        self.verifier.verify(id_token).await
    }

    pub(crate) fn is_bootstrap_operator(&self, identity: &HumanIdentity) -> bool {
        normalize_email(&identity.email).as_str() == self.bootstrap_operator_email.as_ref()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HumanAuthConfigError {
    #[error("human authentication configuration is incomplete; missing {0}")]
    Missing(&'static str),
    #[error("KRATOS_BOOTSTRAP_OPERATOR_EMAIL is invalid")]
    InvalidEmail,
    #[error("identity provider HTTP client could not be created")]
    Client,
}

/// Reads the complete human-authentication configuration, or returns `None` when wholly absent.
///
/// # Errors
///
/// Returns an error when configuration is incomplete, the bootstrap email is malformed, or the
/// bounded HTTP client cannot be created.
pub fn human_auth_from_environment() -> Result<Option<HumanAuth>, HumanAuthConfigError> {
    let api_key = env::var("KRATOS_IDENTITY_PLATFORM_API_KEY").ok();
    let operator_email = env::var("KRATOS_BOOTSTRAP_OPERATOR_EMAIL").ok();
    if api_key.is_none() && operator_email.is_none() {
        return Ok(None);
    }
    let api_key =
        api_key
            .filter(|value| !value.trim().is_empty())
            .ok_or(HumanAuthConfigError::Missing(
                "KRATOS_IDENTITY_PLATFORM_API_KEY",
            ))?;
    let operator_email = operator_email
        .filter(|value| !value.trim().is_empty())
        .ok_or(HumanAuthConfigError::Missing(
            "KRATOS_BOOTSTRAP_OPERATOR_EMAIL",
        ))?;
    if !operator_email.contains('@') {
        return Err(HumanAuthConfigError::InvalidEmail);
    }
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(8))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| HumanAuthConfigError::Client)?;
    Ok(Some(HumanAuth::new(
        Arc::new(IdentityPlatformClient { client, api_key }),
        &operator_email,
    )))
}

struct IdentityPlatformClient {
    client: Client,
    api_key: String,
}

impl fmt::Debug for IdentityPlatformClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IdentityPlatformClient")
            .field("api_key", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LookupRequest<'a> {
    id_token: &'a str,
}

#[derive(Deserialize)]
struct LookupResponse {
    #[serde(default)]
    users: Vec<LookupUser>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LookupUser {
    local_id: String,
    email: Option<String>,
    email_verified: Option<bool>,
    display_name: Option<String>,
    disabled: Option<bool>,
}

#[async_trait]
impl IdentityVerifier for IdentityPlatformClient {
    async fn verify(&self, id_token: &str) -> Result<HumanIdentity, VerifyError> {
        let response = self
            .client
            .post(LOOKUP_URL)
            .query(&[("key", &self.api_key)])
            .json(&LookupRequest { id_token })
            .send()
            .await
            .map_err(|_| VerifyError::Unavailable)?;
        if matches!(
            response.status(),
            StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED
        ) {
            return Err(VerifyError::Rejected);
        }
        let response = response
            .error_for_status()
            .map_err(|_| VerifyError::Unavailable)?
            .json::<LookupResponse>()
            .await
            .map_err(|_| VerifyError::Unavailable)?;
        let [user] = response.users.as_slice() else {
            return Err(VerifyError::Rejected);
        };
        let email = user.email.as_deref().filter(|value| !value.is_empty());
        if user.local_id.is_empty()
            || email.is_none()
            || user.email_verified != Some(true)
            || user.disabled == Some(true)
        {
            return Err(VerifyError::Rejected);
        }
        Ok(HumanIdentity {
            subject: user.local_id.clone(),
            email: email.unwrap_or_default().to_owned(),
            display_name: user
                .display_name
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(email.unwrap_or_default())
                .to_owned(),
        })
    }
}

fn normalize_email(email: &str) -> String {
    email.trim().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::normalize_email;

    #[test]
    fn bootstrap_email_comparison_is_case_and_whitespace_insensitive() {
        assert_eq!(
            normalize_email(" Operator@Example.COM "),
            "operator@example.com"
        );
    }
}
