//! Update notification (Phase 9): checks the running version against the
//! project's releases feed and reports a newer release to the GUI. It is a
//! notification only — there is no auto-update (see `PLAN.md`, Phase 9).
//!
//! The feed is the GitHub releases API of the repository configured in
//! `[workspace.package] repository`. While that is unset, the check
//! reports [`UpdateCheck::NotConfigured`] without touching the network,
//! mirroring the placeholder links in the Info tab; setting `repository`
//! activates both.

use std::time::Duration;

use serde::Deserialize;

/// The running version, from the workspace manifest.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The source repository.
const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

/// How long the update check may take before giving up.
const CHECK_TIMEOUT: Duration = Duration::from_secs(15);

/// The outcome of an update check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateCheck {
    /// No releases feed: the manifest has no `repository` yet.
    NotConfigured,
    /// The running version is the newest release.
    UpToDate,
    /// A newer release exists: its version tag and the release page.
    Available {
        /// The release's version tag as published (e.g. `v0.2.0`).
        version: String,
        /// The release page, for the GUI's link.
        url: String,
    },
}

/// The fields of GitHub's `releases/latest` reply that are used.
#[derive(Debug, Deserialize)]
struct ReleaseReply {
    /// The release's git tag, e.g. `v0.2.0`.
    tag_name: String,
    /// The human-facing release page.
    html_url: String,
}

/// The releases feed derived from the manifest's `repository` — `None`
/// while it is unset or points somewhere other than GitHub.
pub fn release_feed_url() -> Option<String> {
    let slug = REPOSITORY
        .trim_end_matches('/')
        .strip_prefix("https://github.com/")?;
    let (owner, repo) = slug.split_once('/')?;
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    Some(format!(
        "https://api.github.com/repos/{owner}/{repo}/releases/latest"
    ))
}

/// Whether the release tag `tag` is newer than the running version;
/// `None` when the tag is not a version (`nightly`, `release-2024`, ...).
fn tag_is_newer(tag: &str) -> Option<bool> {
    let latest = semver::Version::parse(tag.trim().trim_start_matches(['v', 'V'])).ok()?;
    let current = semver::Version::parse(CURRENT_VERSION).ok()?;
    Some(latest > current)
}

/// Checks `feed` (from [`release_feed_url`]) for a newer release.
/// `None` short-circuits to [`UpdateCheck::NotConfigured`].
pub async fn check(feed: Option<&str>) -> Result<UpdateCheck, String> {
    let Some(feed) = feed else {
        return Ok(UpdateCheck::NotConfigured);
    };
    let client = reqwest::Client::builder()
        .timeout(CHECK_TIMEOUT)
        .build()
        .map_err(|error| error.to_string())?;
    let reply: ReleaseReply = client
        .get(feed)
        .header(
            "User-Agent",
            concat!("opencloud-sync/", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map_err(|error| error.to_string())?
        .json()
        .await
        .map_err(|error| error.to_string())?;
    // A latest release with a non-version tag cannot be compared; it
    // counts as current rather than failing the check.
    if tag_is_newer(&reply.tag_name).unwrap_or(false) {
        Ok(UpdateCheck::Available {
            version: reply.tag_name,
            url: reply.html_url,
        })
    } else {
        Ok(UpdateCheck::UpToDate)
    }
}

/// Checks the releases feed derived from the manifest.
pub async fn check_latest() -> Result<UpdateCheck, String> {
    check(release_feed_url().as_deref()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A release tag far above any released version.
    const NEWER_TAG: &str = "v999.0.0";

    #[test]
    fn feed_url_follows_the_manifest() {
        match REPOSITORY {
            "" => assert_eq!(release_feed_url(), None),
            _ => assert_eq!(
                release_feed_url().as_deref(),
                Some(
                    format!(
                        "https://api.github.com/repos/{}/releases/latest",
                        REPOSITORY
                            .trim_end_matches('/')
                            .strip_prefix("https://github.com/")
                            .unwrap()
                    )
                    .as_str()
                )
            ),
        }
    }

    #[test]
    fn newer_older_and_invalid_tags() {
        assert_eq!(tag_is_newer(NEWER_TAG), Some(true));
        assert_eq!(tag_is_newer("v0.0.1"), Some(false));
        assert_eq!(tag_is_newer(CURRENT_VERSION), Some(false));
        // Prereleases of a higher version count as newer; non-version tags
        // cannot be compared.
        assert_eq!(tag_is_newer("999.0.0-rc.1"), Some(true));
        assert_eq!(tag_is_newer("nightly"), None);
    }

    #[tokio::test]
    async fn check_reports_newer_release() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "tag_name": NEWER_TAG,
                    "html_url": "https://example.com/release",
                })),
            )
            .mount(&server)
            .await;
        assert_eq!(
            check(Some(&server.uri())).await,
            Ok(UpdateCheck::Available {
                version: NEWER_TAG.into(),
                url: "https://example.com/release".into(),
            })
        );
    }

    #[tokio::test]
    async fn check_reports_up_to_date_on_older_tag() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "tag_name": "v0.0.1",
                    "html_url": "https://example.com/release",
                })),
            )
            .mount(&server)
            .await;
        assert_eq!(check(Some(&server.uri())).await, Ok(UpdateCheck::UpToDate));
    }

    #[tokio::test]
    async fn check_reports_feed_errors() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&server)
            .await;
        assert!(check(Some(&server.uri())).await.is_err());
    }

    #[tokio::test]
    async fn without_feed_nothing_is_checked() {
        assert_eq!(check(None).await, Ok(UpdateCheck::NotConfigured));
    }
}
