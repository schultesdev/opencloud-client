//! Local loopback listener that catches the authorization response
//! (`http://127.0.0.1:<port>/?code=...&state=...`).

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use url::form_urlencoded;

use crate::error::AuthError;

/// Maximum size of the request head we are willing to read.
const MAX_HEAD_BYTES: usize = 16 * 1024;
/// Maximum time to read a single request head.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Parsed query parameters of the authorization response.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Callback {
    /// Authorization code (on success).
    pub code: Option<String>,
    /// Echoed `state` of the authorization request.
    pub state: Option<String>,
    /// OAuth error code (e.g. `access_denied`), on failure.
    pub error: Option<String>,
    /// Human-readable error description, on failure.
    pub error_description: Option<String>,
}

impl Callback {
    /// Parses the relevant query parameters.
    fn from_query(query: &str) -> Self {
        let mut callback = Callback::default();
        for (key, value) in form_urlencoded::parse(query.as_bytes()) {
            match &*key {
                "code" => callback.code = Some(value.into_owned()),
                "state" => callback.state = Some(value.into_owned()),
                "error" => callback.error = Some(value.into_owned()),
                "error_description" => callback.error_description = Some(value.into_owned()),
                _ => {}
            }
        }
        callback
    }
}

/// A TCP listener on `127.0.0.1:<free port>` that receives exactly one
/// authorization response.
pub struct CallbackListener {
    listener: TcpListener,
    port: u16,
}

impl CallbackListener {
    /// Binds to `127.0.0.1:0` and lets the OS pick a free port.
    pub async fn bind() -> Result<Self, AuthError> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        Ok(Self { listener, port })
    }

    /// Redirect URI to use in the authorization request.
    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }

    /// Waits for a single authorization response until `timeout` elapses.
    /// Stray requests (e.g. a browser prefetching `/favicon.ico`) are
    /// answered and ignored; the listener keeps waiting.
    pub async fn wait(self, timeout: Duration) -> Result<Callback, AuthError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(AuthError::Timeout {
                    timeout_secs: timeout.as_secs(),
                });
            }
            let (stream, _) = match tokio::time::timeout(remaining, self.listener.accept()).await {
                Ok(accept) => accept?,
                Err(_) => {
                    return Err(AuthError::Timeout {
                        timeout_secs: timeout.as_secs(),
                    });
                }
            };
            if let Some(callback) = Self::handle(stream).await? {
                return Ok(callback);
            }
        }
    }

    /// Reads one HTTP request, answers it, and returns `Some` when it is an
    /// authorization response.
    async fn handle(mut stream: TcpStream) -> Result<Option<Callback>, AuthError> {
        let head = match read_request_head(&mut stream).await {
            Ok(head) => head,
            Err(_) => {
                return Ok(None);
            }
        };
        let mut parts = head.split(' ');
        let method = parts.next().unwrap_or_default();
        let target = parts.next().unwrap_or_default();
        if method != "GET" {
            respond(&mut stream, 405, "Method Not Allowed", "").await;
            return Ok(None);
        }
        let query = target
            .split_once('?')
            .map_or(String::new(), |(_, q)| q.to_string());
        let callback = Callback::from_query(&query);
        if callback == Callback::default() {
            respond(&mut stream, 404, "Not Found", "").await;
            return Ok(None);
        }
        respond(&mut stream, 200, "OK", SUCCESS_PAGE).await;
        Ok(Some(callback))
    }
}

/// Success page shown in the browser after the redirect.
const SUCCESS_PAGE: &str = "<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\"><title>OpenCloud Sync</title></head><body><h1>Anmeldung erfolgreich</h1><p>Dieses Fenster kann geschlossen werden; die Synchronisation wird fortgesetzt.</p></body></html>";

/// Reads the request head (request line + headers) up to `\r\n\r\n`.
async fn read_request_head(stream: &mut TcpStream) -> Result<String, AuthError> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let read = tokio::time::timeout(READ_TIMEOUT, stream.read(&mut chunk)).await;
        let n = match read {
            Ok(Ok(n)) => n,
            Ok(Err(err)) => return Err(err.into()),
            Err(_) => {
                return Err(AuthError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "timed out reading the request",
                )));
            }
        };
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() >= MAX_HEAD_BYTES {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Writes a minimal HTTP response; errors are ignored (best effort).
async fn respond(stream: &mut TcpStream, status: u16, reason: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}
