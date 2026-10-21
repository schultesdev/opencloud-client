//! OpenCloud API client (Phase 3): Graph API for spaces and a WebDAV client
//! (PROPFIND, GET, PUT, MKCOL, MOVE, COPY, DELETE) on top of `reqwest`.
//!
//! Building blocks:
//!
//! - [`GraphClient`]: `GET /graph/v1.0/me/drives` — the spaces of the
//!   logged-in user, with `id`, `name`, `driveType`, `driveAlias`,
//!   `root.webDavUrl`, and `quota`.
//! - [`WebDavClient`]: per-space client at `https://<server>/dav/spaces/<id>/`
//!   or built from `root.webDavUrl`. Paths are percent-encoded per segment
//!   and hrefs are decoded and NFC-normalized, so local and remote names
//!   compare equal. Writes support `If-Match`/`If-None-Match` conditions
//!   ([`WriteCondition`]).
//! - [`ApiError`]: error model differentiating network, 401, 403, 404,
//!   412 (precondition/conflict), 423 (locked), 429, and 5xx — the basis
//!   for retries and conflict detection.
//! - [`RetryConfig`]/[`with_retry`]: exponential backoff with jitter; the
//!   clients honor `Retry-After` on 429 and force a token refresh after
//!   401 (single-flight via [`TokenProvider`]).
//!
//! All operations are covered by tests against `wiremock` mock servers and
//! XML fixtures; only the token machinery is shared with `oc-auth`.

mod dav;
mod error;
mod graph;
mod http;
mod path;
mod propfind;
mod retry;
mod token;

pub use dav::{DavResource, Depth, WebDavClient, WriteCondition};
pub use error::ApiError;
pub use graph::{GRAPH_API_PREFIX, GraphClient, Quota, Space, SpaceRoot};
pub use http::{DEFAULT_TIMEOUT, DEFAULT_TRANSFER_TIMEOUT};
pub use path::normalize_nfc;
pub use retry::{RetryConfig, with_retry};
pub use token::{StaticToken, TokenProvider};
