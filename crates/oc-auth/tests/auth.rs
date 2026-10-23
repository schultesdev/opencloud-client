//! Integration tests for OIDC authentication against mock servers
//! (discovery, token exchange, refresh, single-flight, logout).

use std::time::Duration;

use oc_auth::{
    AuthClient, AuthError, CallbackListener, InMemoryTokenStore, Pkce, TokenManager, TokenSet,
    TokenStore, s256,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Builds a token response body.
fn token_json(access: &str, refresh: Option<&str>, expires_in: u64) -> serde_json::Value {
    let mut body = serde_json::json!({
        "access_token": access,
        "token_type": "Bearer",
        "expires_in": expires_in,
    });
    if let Some(refresh) = refresh {
        body["refresh_token"] = serde_json::json!(refresh);
    }
    body
}

fn tokens(access: &str, expires_in: u64, refresh: Option<&str>) -> TokenSet {
    TokenSet {
        access_token: access.into(),
        token_type: Some("Bearer".into()),
        expires_in,
        refresh_token: refresh.map(String::from),
        scope: None,
        id_token: None,
    }
}

/// A long-lived token (never refreshed within the default 30 s skew).
const LONG_LIVED: u64 = 3600;
/// A token that is instantly stale given the default 30 s skew.
const INSTANTLY_STALE: u64 = 1;

// --- authorize URL ---------------------------------------------------------

#[test]
fn authorize_url_contains_pkce_state_and_scopes() {
    let client = AuthClient::new("https://cloud.example.com").expect("valid server");
    let pkce = Pkce::new();
    let url = client
        .authorize_url("http://127.0.0.1:8080/", &pkce, "the-state")
        .expect("authorize url");

    assert_eq!(url.path(), "/signin/v1/identifier/_/authorize");
    let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(
        query.get("client_id").map(String::as_str),
        Some("OpenCloudDesktop")
    );
    assert_eq!(query.get("response_type").map(String::as_str), Some("code"));
    assert_eq!(
        query.get("scope").map(String::as_str),
        Some("openid profile email offline_access")
    );
    assert_eq!(
        query.get("redirect_uri").map(String::as_str),
        Some("http://127.0.0.1:8080/")
    );
    assert_eq!(query.get("state").map(String::as_str), Some("the-state"));
    assert_eq!(
        query.get("code_challenge").map(String::as_str),
        Some(pkce.challenge.as_str())
    );
    assert_eq!(
        query.get("code_challenge_method").map(String::as_str),
        Some("S256")
    );
}

// --- discovery -------------------------------------------------------------

#[tokio::test]
async fn discovery_overrides_endpoints() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "issuer": server.uri(),
            "authorization_endpoint": format!("{}/custom/authorize", server.uri()),
            "token_endpoint": format!("{}/custom/token", server.uri()),
            "revocation_endpoint": format!("{}/custom/revoke", server.uri()),
        })))
        .expect(1)
        .mount(&server)
        .await;

    let mut client = AuthClient::new(&server.uri()).expect("valid server");
    let endpoints = client.discover_endpoints().await;

    assert_eq!(
        endpoints.token_endpoint.as_deref(),
        Some(format!("{}/custom/token", server.uri()).as_str())
    );
    assert_eq!(
        endpoints.authorization_endpoint.as_deref(),
        Some(format!("{}/custom/authorize", server.uri()).as_str())
    );
    assert_eq!(
        endpoints.revocation_endpoint.as_deref(),
        Some(format!("{}/custom/revoke", server.uri()).as_str())
    );
}

#[tokio::test]
async fn discovery_failure_falls_back_to_opencloud_defaults() {
    let server = MockServer::start().await; // nothing mounted → 404

    let mut client = AuthClient::new(&server.uri()).expect("valid server");
    let endpoints = client.discover_endpoints().await;

    assert_eq!(
        endpoints.authorization_endpoint.as_deref(),
        Some(format!("{}/signin/v1/identifier/_/authorize", server.uri()).as_str())
    );
    assert_eq!(
        endpoints.token_endpoint.as_deref(),
        Some(format!("{}/konnect/v1/token", server.uri()).as_str())
    );
    assert_eq!(endpoints.revocation_endpoint, None);
}

// --- callback listener -----------------------------------------------------

#[tokio::test]
async fn callback_listener_catches_code_and_state() {
    let listener = CallbackListener::bind().await.expect("bind");
    let redirect_uri = listener.redirect_uri();
    let handle = tokio::spawn(async move { listener.wait(Duration::from_secs(10)).await });

    // Act as the provider: send the browser to the redirect URI.
    let response = reqwest::get(format!("{redirect_uri}?code=abc123&state=s1"))
        .await
        .expect("redirect");
    assert!(response.status().is_success());

    let callback = handle.await.expect("join").expect("callback");
    assert_eq!(callback.code.as_deref(), Some("abc123"));
    assert_eq!(callback.state.as_deref(), Some("s1"));
}

#[tokio::test]
async fn callback_listener_times_out() {
    let listener = CallbackListener::bind().await.expect("bind");
    let result = listener.wait(Duration::from_millis(100)).await;
    assert!(matches!(result, Err(AuthError::Timeout { .. })));
}

#[tokio::test]
async fn callback_listener_survives_stray_requests() {
    let listener = CallbackListener::bind().await.expect("bind");
    let redirect_uri = listener.redirect_uri();
    let handle = tokio::spawn(async move { listener.wait(Duration::from_secs(10)).await });

    // A stray browser request (e.g. favicon) must not consume the wait...
    reqwest::get(format!("{redirect_uri}favicon.ico"))
        .await
        .expect("stray");
    // ...the real response still arrives.
    reqwest::get(format!("{redirect_uri}?code=late&state=s2"))
        .await
        .expect("redirect");

    let callback = handle.await.expect("join").expect("callback");
    assert_eq!(callback.code.as_deref(), Some("late"));
}

// --- login flow pieces + token exchange ------------------------------------

#[tokio::test]
async fn full_login_flow_against_mock_server() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/konnect/v1/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(token_json(
            "access-1",
            Some("refresh-1"),
            LONG_LIVED,
        )))
        .expect(1)
        .mount(&server)
        .await;

    let client = AuthClient::new(&server.uri()).expect("valid server");
    let store = InMemoryTokenStore::default();
    let manager = TokenManager::new(client.clone(), store, "konto-1");

    // Steps of AuthClient::login, driven manually (no browser in tests).
    let pkce = Pkce::new();
    let state = "test-state".to_string();
    let listener = CallbackListener::bind().await.expect("bind");
    let redirect_uri = listener.redirect_uri();
    let authorize_url = client
        .authorize_url(&redirect_uri, &pkce, &state)
        .expect("authorize url");
    // The authorize URL must carry the S256 challenge for our verifier.
    let challenge: std::collections::HashMap<_, _> =
        authorize_url.query_pairs().into_owned().collect();
    assert_eq!(
        challenge.get("code_challenge").map(String::as_str),
        Some(s256(&pkce.verifier).as_str())
    );

    let wait = tokio::spawn(async move { listener.wait(Duration::from_secs(10)).await });
    reqwest::get(format!("{redirect_uri}?code=the-code&state={state}"))
        .await
        .expect("redirect");
    let callback = wait.await.expect("join").expect("callback");

    let received = manager
        .client()
        .exchange_code(
            callback.code.as_deref().expect("code"),
            &redirect_uri,
            &pkce.verifier,
        )
        .await
        .expect("exchange");
    manager.apply_login(&received).await.expect("apply login");

    assert_eq!(received.refresh_token.as_deref(), Some("refresh-1"));
    assert_eq!(
        manager
            .store()
            .load_refresh_token("konto-1")
            .expect("store")
            .as_deref(),
        Some("refresh-1")
    );
    assert_eq!(
        manager.access_token().await.expect("access token"),
        "access-1"
    );
    server.verify().await;
}

// --- refresh / single-flight / expiry --------------------------------------

#[tokio::test]
async fn valid_token_is_not_refreshed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/konnect/v1/token"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(token_json("fresh", None, LONG_LIVED)),
        )
        // The mock server must not be hit at all.
        .expect(0)
        .mount(&server)
        .await;

    let client = AuthClient::new(&server.uri()).expect("valid server");
    let manager = TokenManager::new(client, InMemoryTokenStore::default(), "konto-1");
    manager
        .apply_login(&tokens("old", LONG_LIVED, Some("r1")))
        .await
        .expect("apply login");

    assert_eq!(manager.access_token().await.expect("access"), "old");
    assert_eq!(manager.access_token().await.expect("access"), "old");
    server.verify().await;
}

#[tokio::test]
async fn expired_token_triggers_refresh_with_rotation() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/konnect/v1/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(token_json(
            "fresh",
            Some("r2"),
            LONG_LIVED,
        )))
        .expect(1)
        .mount(&server)
        .await;

    let client = AuthClient::new(&server.uri()).expect("valid server");
    let store = InMemoryTokenStore::default();
    let manager = TokenManager::new(client, store.clone(), "konto-1");
    // expires_in=1 → immediately stale given the 30 s refresh skew.
    manager
        .apply_login(&tokens("old", INSTANTLY_STALE, Some("r1")))
        .await
        .expect("apply login");

    assert_eq!(manager.access_token().await.expect("access"), "fresh");
    // The rotated refresh token replaced r1 in the store.
    assert_eq!(
        store
            .load_refresh_token("konto-1")
            .expect("store")
            .as_deref(),
        Some("r2")
    );
    server.verify().await;
}

#[tokio::test]
async fn concurrent_refreshes_share_one_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/konnect/v1/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(token_json(
            "fresh",
            Some("r2"),
            LONG_LIVED,
        )))
        // Single-flight: two parallel callers, exactly one refresh request.
        .expect(1)
        .mount(&server)
        .await;

    let client = AuthClient::new(&server.uri()).expect("valid server");
    let manager = std::sync::Arc::new(TokenManager::new(
        client,
        InMemoryTokenStore::default(),
        "konto-1",
    ));
    manager
        .apply_login(&tokens("old", INSTANTLY_STALE, Some("r1")))
        .await
        .expect("apply login");

    let first = {
        let manager = manager.clone();
        tokio::spawn(async move { manager.access_token().await })
    };
    let second = {
        let manager = manager.clone();
        tokio::spawn(async move { manager.access_token().await })
    };
    assert_eq!(first.await.expect("join").expect("access"), "fresh");
    assert_eq!(second.await.expect("join").expect("access"), "fresh");

    server.verify().await;
}

#[tokio::test]
async fn force_refresh_overrides_valid_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/konnect/v1/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(token_json("new", None, LONG_LIVED)))
        .expect(1)
        .mount(&server)
        .await;

    let client = AuthClient::new(&server.uri()).expect("valid server");
    let manager = TokenManager::new(client, InMemoryTokenStore::default(), "konto-1");
    manager
        .apply_login(&tokens("old", LONG_LIVED, Some("r1")))
        .await
        .expect("apply login");

    // Simulates the 401 path: the token looks fine but the server disagrees.
    assert_eq!(manager.force_refresh().await.expect("refresh"), "new");
    assert_eq!(manager.access_token().await.expect("access"), "new");
    server.verify().await;
}

#[tokio::test]
async fn invalid_refresh_token_clears_the_login() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/konnect/v1/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": "invalid_grant",
            "error_description": "refresh token expired",
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = AuthClient::new(&server.uri()).expect("valid server");
    let store = InMemoryTokenStore::default();
    let manager = TokenManager::new(client, store.clone(), "konto-1");
    manager
        .apply_login(&tokens("old", INSTANTLY_STALE, Some("r1")))
        .await
        .expect("apply login");

    let error = manager.access_token().await.expect_err("refresh must fail");
    assert!(matches!(error, AuthError::RefreshTokenInvalid { .. }));
    // The stored login is gone; a retry reports NoRefreshToken instead of
    // hammering the server.
    assert!(!manager.has_stored_login());
    assert_eq!(store.load_refresh_token("konto-1").expect("store"), None);
    let error = manager.access_token().await.expect_err("must fail");
    assert!(matches!(error, AuthError::NoRefreshToken));
    server.verify().await;
}

// --- logout ----------------------------------------------------------------

#[tokio::test]
async fn logout_revokes_and_clears_the_store() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "revocation_endpoint": format!("{}/konnect/v1/revoke", server.uri()),
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/konnect/v1/revoke"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let mut client = AuthClient::new(&server.uri()).expect("valid server");
    client.discover_endpoints().await;

    let store = InMemoryTokenStore::default();
    store.save_refresh_token("konto-1", "r1").expect("save");
    let manager = TokenManager::new(client, store, "konto-1");

    manager.logout().await.expect("logout");
    assert!(!manager.has_stored_login());
    server.verify().await;
}

#[tokio::test]
async fn logout_without_revocation_endpoint_still_clears_the_store() {
    let client = AuthClient::new("https://cloud.example.com").expect("valid server");
    let store = InMemoryTokenStore::default();
    store.save_refresh_token("konto-1", "r1").expect("save");
    let manager = TokenManager::new(client, store, "konto-1");

    manager.logout().await.expect("logout");
    assert!(!manager.has_stored_login());
}

#[tokio::test]
async fn stale_refresh_token_after_concurrent_rotation_keeps_the_login() {
    let server = MockServer::start().await;
    let store = InMemoryTokenStore::default();

    // Another manager rotates r1 → r2 while our request with r1 is in
    // flight; the server then rejects r1 as already used.
    let rotating = store.clone();
    Mock::given(method("POST"))
        .and(path("/konnect/v1/token"))
        .and(wiremock::matchers::body_string_contains("refresh_token=r1"))
        .respond_with(move |_: &wiremock::Request| {
            rotating
                .save_refresh_token("konto-1", "r2")
                .expect("rotate");
            ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "invalid_grant",
                "error_description": "refresh token already used",
            }))
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/konnect/v1/token"))
        .and(wiremock::matchers::body_string_contains("refresh_token=r2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(token_json(
            "fresh",
            Some("r3"),
            LONG_LIVED,
        )))
        .expect(1)
        .mount(&server)
        .await;

    let client = AuthClient::new(&server.uri()).expect("valid server");
    let manager = TokenManager::new(client, store.clone(), "konto-1");
    manager
        .apply_login(&tokens("old", INSTANTLY_STALE, Some("r1")))
        .await
        .expect("apply login");

    assert_eq!(manager.access_token().await.expect("access"), "fresh");
    assert_eq!(
        store
            .load_refresh_token("konto-1")
            .expect("store")
            .as_deref(),
        Some("r3")
    );
    server.verify().await;
}
