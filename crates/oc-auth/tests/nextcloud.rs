//! Integration tests for the Nextcloud login (Login Flow v2) and the app
//! password lifecycle against mock servers.

// Test helpers may use unwrap/expect just like the test functions.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::time::Duration;

use oc_auth::{AppPasswordManager, AuthError, InMemoryTokenStore, TokenStore};
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ACCOUNT: &str = "nc-konto";

fn manager(
    server: &MockServer,
    store: InMemoryTokenStore,
) -> AppPasswordManager<InMemoryTokenStore> {
    AppPasswordManager::new(&server.uri(), store, ACCOUNT)
        .expect("manager")
        .with_poll_interval(Duration::from_millis(10))
}

async fn mount_flow_start(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/index.php/login/v2"))
        .and(header("User-Agent", oc_auth::NEXTCLOUD_DEVICE_NAME))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "poll": {
                "token": "poll-token",
                "endpoint": format!("{}/login/v2/poll", server.uri()),
            },
            "login": format!("{}/login/v2/flow/abc", server.uri()),
        })))
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn login_flow_stores_the_app_password() {
    let server = MockServer::start().await;
    mount_flow_start(&server).await;
    // Pending twice, then granted.
    Mock::given(method("POST"))
        .and(path("/login/v2/poll"))
        .and(body_string_contains("token=poll-token"))
        .respond_with(ResponseTemplate::new(404))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/login/v2/poll"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "server": server.uri(),
            "loginName": "alice",
            "appPassword": "app-secret",
        })))
        .mount(&server)
        .await;

    let store = InMemoryTokenStore::default();
    let first = manager(&server, store.clone());
    assert!(!first.has_stored_login());
    let credential = first
        .login(false, Duration::from_secs(5))
        .await
        .expect("login");
    assert_eq!(credential.login_name, "alice");
    assert_eq!(credential.basic_credential(), "alice:app-secret");
    assert!(first.has_stored_login());
    let stored = store
        .load_refresh_token(ACCOUNT)
        .expect("load")
        .expect("stored");
    assert!(stored.contains("app-secret"));

    // A second manager (e.g. after a restart) reads it from the store.
    let restarted = manager(&server, store);
    assert_eq!(
        restarted
            .credential()
            .await
            .expect("credential")
            .app_password,
        "app-secret"
    );
}

#[tokio::test]
async fn login_flow_times_out_while_access_is_not_granted() {
    let server = MockServer::start().await;
    mount_flow_start(&server).await;
    Mock::given(method("POST"))
        .and(path("/login/v2/poll"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let manager = manager(&server, InMemoryTokenStore::default());
    let err = manager
        .login(false, Duration::from_millis(60))
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::Timeout { .. }), "{err:?}");
    assert!(!manager.has_stored_login());
}

#[tokio::test]
async fn failed_flow_start_is_reported() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/index.php/login/v2"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let err = manager(&server, InMemoryTokenStore::default())
        .login(false, Duration::from_secs(1))
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::LoginFlow(_)), "{err:?}");
}

#[tokio::test]
async fn rejected_password_is_not_handed_out_again() {
    let server = MockServer::start().await;
    let store = InMemoryTokenStore::default();
    store
        .save_refresh_token(ACCOUNT, r#"{"login_name":"alice","app_password":"old"}"#)
        .expect("seed");
    let manager = manager(&server, store);
    let credential = manager.credential().await.expect("credential");
    manager.mark_rejected(&credential.basic_credential());
    assert!(matches!(
        manager.credential().await,
        Err(AuthError::AppPasswordRejected)
    ));
}

#[tokio::test]
async fn missing_or_foreign_credentials_require_a_login() {
    let server = MockServer::start().await;
    let store = InMemoryTokenStore::default();
    let manager = manager(&server, store.clone());
    assert!(matches!(
        manager.credential().await,
        Err(AuthError::NoAppPassword)
    ));
    // A leftover OpenCloud refresh token is not a Nextcloud login.
    store
        .save_refresh_token(ACCOUNT, "opaque-refresh-token")
        .expect("seed");
    assert!(!manager.has_stored_login());
    assert!(matches!(
        manager.credential().await,
        Err(AuthError::NoAppPassword)
    ));
}

#[tokio::test]
async fn logout_deletes_the_app_password_remotely_and_locally() {
    let server = MockServer::start().await;
    // base64("alice:app-secret")
    Mock::given(method("DELETE"))
        .and(path("/ocs/v2.php/core/apppassword"))
        .and(header("OCS-APIRequest", "true"))
        .and(header("Authorization", "Basic YWxpY2U6YXBwLXNlY3JldA=="))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let store = InMemoryTokenStore::default();
    store
        .save_refresh_token(
            ACCOUNT,
            r#"{"login_name":"alice","app_password":"app-secret"}"#,
        )
        .expect("seed");
    let manager = manager(&server, store.clone());
    manager.logout().await.expect("logout");
    assert!(!manager.has_stored_login());
    assert!(store.load_refresh_token(ACCOUNT).expect("load").is_none());
}

#[tokio::test]
async fn logout_succeeds_while_the_server_is_unreachable() {
    let store = InMemoryTokenStore::default();
    store
        .save_refresh_token(ACCOUNT, r#"{"login_name":"a","app_password":"b"}"#)
        .expect("seed");
    // Nothing listens on port 9 (discard) on loopback.
    let manager =
        AppPasswordManager::new("http://127.0.0.1:9", store.clone(), ACCOUNT).expect("manager");
    manager.logout().await.expect("logout");
    assert!(store.load_refresh_token(ACCOUNT).expect("load").is_none());
}
