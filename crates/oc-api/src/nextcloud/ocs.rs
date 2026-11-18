//! The OCS endpoint for the logged-in user: id, display name, quota.

use serde::Deserialize;
use serde_json::Value;
use url::Url;

use crate::error::ApiError;
use crate::http::{AuthedHttp, Replay};
use crate::token::TokenProvider;

/// The logged-in Nextcloud user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NextcloudUser {
    /// The user id — the path segment of the user's WebDAV tree
    /// (`/remote.php/dav/files/{id}/`); may differ from the login name.
    pub id: String,
    /// Display name.
    pub display_name: String,
    /// Used bytes, if reported.
    pub quota_used: Option<u64>,
    /// Total bytes, if the user has a limited quota.
    pub quota_total: Option<u64>,
}

/// OCS envelope `{"ocs": {"data": ...}}`.
#[derive(Debug, Deserialize)]
struct Envelope {
    ocs: Ocs,
}

#[derive(Debug, Deserialize)]
struct Ocs {
    data: UserData,
}

#[derive(Debug, Deserialize)]
struct UserData {
    id: String,
    /// Real servers send both spellings; separate fields (instead of a
    /// serde alias) so both may be present at once.
    #[serde(rename = "display-name", default)]
    display_name: Option<String>,
    #[serde(default)]
    displayname: Option<String>,
    #[serde(default)]
    quota: Option<Value>,
}

/// A non-negative byte count; Nextcloud reports unlimited or unknown
/// quotas as negative numbers and sometimes as floats.
fn bytes(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    if let Some(number) = value.as_u64() {
        return Some(number);
    }
    value
        .as_f64()
        .filter(|number| number.is_finite() && *number >= 0.0)
        .map(|number| number as u64)
}

/// `GET <server>/ocs/v2.php/cloud/user?format=json`.
pub(crate) async fn current_user<P: TokenProvider>(
    http: &AuthedHttp<P>,
    server: &Url,
) -> Result<NextcloudUser, ApiError> {
    let url = below(server, "ocs/v2.php/cloud/user?format=json")?;
    let response = http
        .send(Some(http.request_timeout()), Replay::Safe, || {
            http.http()
                .get(url.clone())
                .header("OCS-APIRequest", "true")
                .header(reqwest::header::ACCEPT, "application/json")
        })
        .await?;
    let body = response.text().await?;
    parse_user(&body)
}

/// Parses the OCS user document.
fn parse_user(body: &str) -> Result<NextcloudUser, ApiError> {
    let envelope: Envelope = serde_json::from_str(body)
        .map_err(|err| ApiError::protocol(format!("invalid OCS user response: {err}")))?;
    let data = envelope.ocs.data;
    if data.id.trim().is_empty() {
        return Err(ApiError::protocol("OCS user response without user id"));
    }
    let quota = data.quota.as_ref();
    let total = bytes(quota.and_then(|quota| quota.get("total")));
    // `quota` (the configured limit) is negative when unlimited; `total`
    // is then free + used and not a limit.
    let limited = quota
        .and_then(|quota| quota.get("quota"))
        .and_then(Value::as_f64)
        .is_none_or(|limit| limit >= 0.0);
    Ok(NextcloudUser {
        display_name: data
            .display_name
            .filter(|name| !name.trim().is_empty())
            .or(data.displayname.filter(|name| !name.trim().is_empty()))
            .unwrap_or_else(|| data.id.clone()),
        id: data.id,
        quota_used: bytes(quota.and_then(|quota| quota.get("used"))),
        quota_total: total.filter(|_| limited),
    })
}

/// `relative` below the server base URL (a base path counts as folder).
pub(crate) fn below(server: &Url, relative: &str) -> Result<Url, ApiError> {
    let mut base = server.clone();
    if !base.path().ends_with('/') {
        let path = format!("{}/", base.path());
        base.set_path(&path);
    }
    base.join(relative)
        .map_err(|err| ApiError::invalid_url(base.as_str(), err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_with_limited_quota() {
        let user = parse_user(
            r#"{"ocs":{"meta":{"status":"ok"},"data":{"id":"alice","display-name":"Alice A.",
            "quota":{"free":900,"used":100,"total":1000,"relative":10,"quota":1000}}}}"#,
        )
        .expect("user");
        assert_eq!(user.id, "alice");
        assert_eq!(user.display_name, "Alice A.");
        assert_eq!(user.quota_used, Some(100));
        assert_eq!(user.quota_total, Some(1000));
    }

    #[test]
    fn unlimited_quota_has_no_total() {
        let user = parse_user(
            r#"{"ocs":{"data":{"id":"bob","quota":{"free":-3,"used":5.0,"total":-3,"quota":-3}}}}"#,
        )
        .expect("user");
        assert_eq!(user.display_name, "bob");
        assert_eq!(user.quota_used, Some(5));
        assert_eq!(user.quota_total, None);
    }

    #[test]
    fn both_display_name_spellings_are_accepted() {
        // Real servers send `display-name` and `displayname` side by side.
        let user = parse_user(
            r#"{"ocs":{"data":{"id":"carol","display-name":"Carol C.","displayname":"Carol C."}}}"#,
        )
        .expect("user");
        assert_eq!(user.display_name, "Carol C.");
        let user =
            parse_user(r#"{"ocs":{"data":{"id":"dave","displayname":"Dave D."}}}"#).expect("user");
        assert_eq!(user.display_name, "Dave D.");
    }

    #[test]
    fn base_paths_are_kept() {
        let server = Url::parse("https://host/nextcloud").expect("url");
        assert_eq!(
            below(&server, "ocs/v2.php/cloud/user")
                .expect("url")
                .as_str(),
            "https://host/nextcloud/ocs/v2.php/cloud/user"
        );
    }
}
