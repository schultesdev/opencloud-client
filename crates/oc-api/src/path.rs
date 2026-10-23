//! WebDAV path handling: percent-encoding of request path segments,
//! decoding of server hrefs back to relative paths, and Unicode NFC
//! normalization for comparing local and remote names.

use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use unicode_normalization::UnicodeNormalization;
use url::Url;

use crate::error::ApiError;

/// Characters that must not appear unencoded in a single path segment:
/// everything outside the RFC 3986 `pchar` set (unreserved, sub-delims,
/// `:`, `@`) plus `%` itself.
const SEGMENT_ENCODE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'&')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// NFC-normalizes text (file and folder names from local and remote
/// sources, so both sides compare equal regardless of decomposition).
pub fn normalize_nfc(input: &str) -> String {
    input.nfc().collect()
}

/// Percent-encodes a single path segment.
pub(crate) fn encode_segment(segment: &str) -> String {
    utf8_percent_encode(segment, SEGMENT_ENCODE).to_string()
}

/// Builds a request URL from `base` (which must end in `/`) and a relative
/// path with `/`-separated segments. Empty, leading, and trailing slashes
/// are tolerated; `.`/`..` segments and NUL bytes are rejected.
pub(crate) fn build_url(base: &Url, path: &str) -> Result<Url, ApiError> {
    if path.contains('\0') {
        return Err(ApiError::invalid_path(path, "contains NUL bytes"));
    }
    let mut relative = String::new();
    for segment in path.split('/') {
        if segment.is_empty() {
            continue;
        }
        if segment == "." || segment == ".." {
            return Err(ApiError::invalid_path(
                path,
                "dot/dot-dot segments are not allowed",
            ));
        }
        if !relative.is_empty() {
            relative.push('/');
        }
        relative.push_str(&encode_segment(segment));
    }
    base.join(&relative)
        .map_err(|err| ApiError::invalid_url(base.as_str(), err.to_string()))
}

/// Decodes a server `href` (absolute URL or absolute path, percent-encoded)
/// into a path relative to the space root. Returns `""` for the root,
/// otherwise NFC-normalized without trailing slash.
#[cfg(test)]
pub(crate) fn href_to_path(base: &Url, href: &str) -> Result<String, ApiError> {
    href_to_server_path(base, href).map(|path| normalize_nfc(&path))
}

/// Like [`href_to_path`], but keeps the server's spelling (no Unicode
/// normalization) — the form that addresses the resource in requests.
/// The href must lie below the space root on a segment boundary
/// (`/dav/spaces/sid2/...` is not below `/dav/spaces/sid`).
pub(crate) fn href_to_server_path(base: &Url, href: &str) -> Result<String, ApiError> {
    let url = if href.contains("://") {
        Url::parse(href).map_err(|err| ApiError::invalid_url(href, err.to_string()))?
    } else {
        base.join(href)
            .map_err(|err| ApiError::invalid_url(href, err.to_string()))?
    };
    let base_prefix = base.path().trim_end_matches('/');
    let rest = url.path().strip_prefix(base_prefix).ok_or_else(|| {
        ApiError::protocol(format!(
            "href '{href}' does not lie below '{}'",
            base.as_str()
        ))
    })?;
    if !(rest.is_empty() || rest.starts_with('/')) {
        return Err(ApiError::protocol(format!(
            "href '{href}' does not lie below '{}'",
            base.as_str()
        )));
    }
    let rest = rest.trim_matches('/');
    let decoded = percent_encoding::percent_decode_str(rest)
        .decode_utf8()
        .map_err(|_| ApiError::protocol(format!("href '{href}' is not valid UTF-8")))?;
    Ok(decoded.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://cloud.example.com/dav/spaces/sid/").expect("base url")
    }

    #[test]
    fn segments_are_percent_encoded() {
        assert_eq!(encode_segment("a b.txt"), "a%20b.txt");
        assert_eq!(encode_segment("a&b#c"), "a%26b%23c");
        assert_eq!(encode_segment("Café.txt"), "Caf%C3%A9.txt");
        // Sub-delims, colon, and at stay readable.
        assert_eq!(encode_segment("a$b+c:d"), "a$b+c:d");
    }

    #[test]
    fn build_url_encodes_each_segment() {
        let url = build_url(&base(), "Dateien & mehr/Bericht Q1.pdf").expect("url");
        assert_eq!(
            url.as_str(),
            "https://cloud.example.com/dav/spaces/sid/Dateien%20%26%20mehr/Bericht%20Q1.pdf"
        );
    }

    #[test]
    fn build_url_tolerates_slashes_but_rejects_dot_segments() {
        let url = build_url(&base(), "/Ordner//Datei.txt/").expect("url");
        assert_eq!(
            url.as_str(),
            "https://cloud.example.com/dav/spaces/sid/Ordner/Datei.txt"
        );
        assert_eq!(
            build_url(&base(), "a/../b").unwrap_err().to_string(),
            "invalid WebDAV path 'a/../b': dot/dot-dot segments are not allowed"
        );
        assert!(matches!(
            build_url(&base(), "a\0b"),
            Err(ApiError::InvalidPath { .. })
        ));
    }

    #[test]
    fn href_is_decoded_and_normalized() {
        let url = base();
        assert_eq!(href_to_path(&url, "/dav/spaces/sid/").expect("root"), "");
        assert_eq!(
            href_to_path(&url, "/dav/spaces/sid/Ordner/Bericht%20Q1.pdf").expect("file"),
            "Ordner/Bericht Q1.pdf"
        );
        assert_eq!(
            href_to_path(&url, "https://cloud.example.com/dav/spaces/sid/Ordner/")
                .expect("absolute"),
            "Ordner"
        );
        // NFD (e + combining acute) becomes NFC.
        assert_eq!(
            href_to_path(&url, "/dav/spaces/sid/Cafe%CC%81.txt").expect("nfc"),
            "Caf\u{e9}.txt"
        );
    }

    #[test]
    fn href_outside_the_space_is_rejected() {
        let err = href_to_path(&base(), "/dav/spaces/other/file.txt").unwrap_err();
        assert!(matches!(err, ApiError::Protocol(_)));
        // A sibling space sharing the id as prefix is outside as well.
        let err = href_to_path(&base(), "/dav/spaces/sid2/file.txt").unwrap_err();
        assert!(matches!(err, ApiError::Protocol(_)));
    }

    #[test]
    fn server_path_keeps_the_servers_spelling() {
        assert_eq!(
            href_to_server_path(&base(), "/dav/spaces/sid/Cafe%CC%81/a.txt").expect("nfd"),
            "Cafe\u{301}/a.txt"
        );
    }

    #[test]
    fn nfc_normalization() {
        assert_eq!(normalize_nfc("Cafe\u{301}"), "Caf\u{e9}");
        assert_eq!(normalize_nfc("already nfc"), "already nfc");
    }
}
