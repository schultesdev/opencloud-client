//! PROPFIND request body and Multistatus XML parser.
//!
//! The parser matches local tag names (namespace-prefix agnostic, so `DAV:`,
//! `d:`, and unprefixed documents all work) and only accepts properties
//! from a `<propstat>` with status 200.

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::error::ApiError;

/// PROPFIND body requesting the properties the sync engine needs:
/// resource type, ETag, size, mtime, and free space for collections.
pub(crate) const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns">
 <d:prop>
  <d:resourcetype/>
  <d:getetag/>
  <d:getcontentlength/>
  <d:getlastmodified/>
  <d:quota-available-bytes/>
  <oc:checksums/>
 </d:prop>
</d:propfind>"#;

/// One parsed `<response>` — raw, the href is not yet resolved to a path.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct RawResource {
    /// The `href` exactly as the server sent it (percent-encoded).
    pub href: String,
    /// `<resourcetype><collection/>` present, or the href ends in `/`.
    pub is_dir: bool,
    /// `getcontentlength`.
    pub size: Option<u64>,
    /// `getetag` in quoted form, as far as the server provides one.
    pub etag: Option<String>,
    /// `getlastmodified` as HTTP-date string.
    pub last_modified: Option<String>,
    /// `quota-available-bytes` for collections.
    pub quota_available_bytes: Option<u64>,
    /// A `<status>` directly inside `<response>` (RFC 4918 allows one in
    /// place of propstats, e.g. `404` for a resource that is gone).
    pub status: Option<String>,
    /// `oc:checksums` content, e.g. `SHA1:… MD5:… ADLER32:…`.
    pub checksums: Option<String>,
}

/// Whether a `HTTP/1.1 200 OK`-style status line carries a 2xx code.
fn is_success_status(line: &str) -> bool {
    line.split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .is_some_and(|code| (200..300).contains(&code))
}

/// Properties of one `<propstat>`, committed on its 200 status.
#[derive(Debug, Default)]
struct PropStat {
    status: String,
    etag: Option<String>,
    size: Option<u64>,
    last_modified: Option<String>,
    quota_available_bytes: Option<u64>,
    checksums: Option<String>,
}

impl PropStat {
    fn is_ok(&self) -> bool {
        is_success_status(&self.status)
    }
}

/// The tag whose text is currently being collected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    Href,
    Status,
    Etag,
    Size,
    LastModified,
    QuotaAvailable,
    Checksum,
}

/// Parses a `207 Multi-Status` PROPFIND response body.
pub(crate) fn parse_multistatus(xml: &str) -> Result<Vec<RawResource>, ApiError> {
    let mut reader = Reader::from_str(xml);
    let mut resources = Vec::new();
    let mut current: Option<RawResource> = None;
    let mut propstat: Option<PropStat> = None;
    let mut pending: Option<Pending> = None;
    let mut text = String::new();

    loop {
        let event = reader
            .read_event()
            .map_err(|err| ApiError::protocol(format!("XML parse error: {err}")))?;
        match event {
            Event::Start(start) => {
                let name = local_name(start.name());
                handle_open(&name, &mut current, &mut propstat, &mut pending, &mut text);
            }
            Event::Empty(empty) => {
                // Only structurally relevant empty tags: a self-closing
                // `<collection/>` marks a directory.
                if local_name(empty.name()) == "collection" {
                    if let Some(resource) = current.as_mut() {
                        resource.is_dir = true;
                    }
                }
            }
            Event::Text(bytes) => {
                if pending.is_some() {
                    let chunk = bytes
                        .unescape()
                        .map_err(|err| ApiError::protocol(format!("bad text: {err}")))?;
                    text.push_str(&chunk);
                }
            }
            Event::CData(bytes) => {
                if pending.is_some() {
                    text.push_str(&String::from_utf8_lossy(bytes.as_ref()));
                }
            }
            Event::End(end) => {
                let name = local_name(end.name());
                handle_close(
                    &name,
                    current.as_mut(),
                    propstat.as_mut(),
                    pending.take(),
                    &mut text,
                )?;
                if name == "response" {
                    if let Some(mut resource) = current.take() {
                        let gone = resource
                            .status
                            .as_deref()
                            .is_some_and(|status| !is_success_status(status));
                        if !resource.href.is_empty() && !gone {
                            resource.is_dir |= resource.href.ends_with('/');
                            resources.push(resource);
                        }
                    }
                } else if name == "propstat" {
                    if let (Some(stage), Some(resource)) = (propstat.take(), current.as_mut()) {
                        if stage.is_ok() {
                            resource.etag = resource.etag.take().or(stage.etag);
                            resource.size = resource.size.or(stage.size);
                            resource.last_modified =
                                resource.last_modified.take().or(stage.last_modified);
                            resource.quota_available_bytes = resource
                                .quota_available_bytes
                                .or(stage.quota_available_bytes);
                            resource.checksums = resource.checksums.take().or(stage.checksums);
                        }
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(resources)
}

/// Opens a tag: tracks the current `<response>`/`<propstat>`, flags
/// collections, and starts text collection for the property tags.
fn handle_open(
    name: &str,
    current: &mut Option<RawResource>,
    propstat: &mut Option<PropStat>,
    pending: &mut Option<Pending>,
    text: &mut String,
) {
    match name {
        "response" => {
            *current = Some(RawResource::default());
            *propstat = None;
            *pending = None;
        }
        "propstat" => {
            *propstat = Some(PropStat::default());
        }
        "collection" => {
            if let Some(resource) = current.as_mut() {
                resource.is_dir = true;
            }
        }
        "href" => {
            *pending = Some(Pending::Href);
            text.clear();
        }
        "status" => {
            *pending = Some(Pending::Status);
            text.clear();
        }
        "getetag" => {
            *pending = Some(Pending::Etag);
            text.clear();
        }
        "getcontentlength" => {
            *pending = Some(Pending::Size);
            text.clear();
        }
        "getlastmodified" => {
            *pending = Some(Pending::LastModified);
            text.clear();
        }
        "quota-available-bytes" => {
            *pending = Some(Pending::QuotaAvailable);
            text.clear();
        }
        "checksum" => {
            *pending = Some(Pending::Checksum);
            text.clear();
        }
        _ => {}
    }
}

/// Closes a text tag and moves the collected value into the current
/// response or propstat.
fn handle_close(
    name: &str,
    current: Option<&mut RawResource>,
    propstat: Option<&mut PropStat>,
    pending: Option<Pending>,
    text: &mut String,
) -> Result<(), ApiError> {
    let is_text_tag = matches!(
        name,
        "href"
            | "status"
            | "getetag"
            | "getcontentlength"
            | "getlastmodified"
            | "quota-available-bytes"
            | "checksum"
    );
    let Some(kind) = pending else {
        return Ok(());
    };
    if !is_text_tag {
        return Ok(());
    }
    let value = text.trim().to_string();
    text.clear();
    match kind {
        Pending::Href => {
            if let Some(resource) = current {
                resource.href = value;
            }
        }
        Pending::Status => match (propstat, current) {
            (Some(stage), _) => stage.status = value,
            (None, Some(resource)) => resource.status = Some(value),
            (None, None) => {}
        },
        Pending::Etag => {
            if let Some(stage) = propstat {
                stage.etag = (!value.is_empty()).then_some(value);
            }
        }
        Pending::Size => {
            if let Some(stage) = propstat {
                stage.size = parse_u64(&value)?;
            }
        }
        Pending::LastModified => {
            if let Some(stage) = propstat {
                stage.last_modified = (!value.is_empty()).then_some(value);
            }
        }
        Pending::QuotaAvailable => {
            if let Some(stage) = propstat {
                stage.quota_available_bytes = parse_quota_bytes(&value)?;
            }
        }
        Pending::Checksum => {
            if let Some(stage) = propstat {
                if !value.is_empty() {
                    // Several <oc:checksum> elements are joined.
                    let joined = match stage.checksums.take() {
                        Some(previous) => format!("{previous} {value}"),
                        None => value,
                    };
                    stage.checksums = Some(joined);
                }
            }
        }
    }
    Ok(())
}

/// Parses a trimmed decimal number; an empty value means "not present",
/// a malformed number is a protocol error.
fn parse_u64(value: &str) -> Result<Option<u64>, ApiError> {
    if value.is_empty() {
        return Ok(None);
    }
    value
        .parse::<u64>()
        .map(Some)
        .map_err(|_| ApiError::protocol(format!("invalid number '{value}' in WebDAV response")))
}

/// Like [`parse_u64`], but for `quota-available-bytes`: RFC 4331 allows
/// negative sentinel values (e.g. -1 unlimited, -2 not computed, -3
/// infinite). Servers like OpenCloud/reva send them for spaces without
/// quota; they map to "unknown" instead of failing the whole response.
fn parse_quota_bytes(value: &str) -> Result<Option<u64>, ApiError> {
    if let Ok(signed) = value.parse::<i64>() {
        if signed < 0 {
            return Ok(None);
        }
    }
    parse_u64(value)
}

/// The local name of a tag (namespace prefix stripped).
fn local_name(name: quick_xml::name::QName<'_>) -> String {
    String::from_utf8_lossy(name.local_name().as_ref()).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owncloud_checksums_are_collected() {
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns">
 <d:response>
  <d:href>/dav/spaces/sid/a.txt</d:href>
  <d:propstat>
   <d:prop>
    <oc:checksums><oc:checksum>SHA1:abc MD5:def ADLER32:01</oc:checksum></oc:checksums>
   </d:prop>
   <d:status>HTTP/1.1 200 OK</d:status>
  </d:propstat>
 </d:response>
</d:multistatus>"#;
        let resources = parse_multistatus(xml).expect("parse");
        assert_eq!(
            resources[0].checksums.as_deref(),
            Some("SHA1:abc MD5:def ADLER32:01")
        );
    }

    #[test]
    fn responses_with_an_error_status_are_dropped() {
        let xml = r#"<?xml version="1.0"?>
<D:multistatus xmlns:D="DAV:">
 <D:response>
  <D:href>/dav/spaces/sid/gone.txt</D:href>
  <D:status>HTTP/1.1 404 Not Found</D:status>
 </D:response>
 <D:response>
  <D:href>/dav/spaces/sid/folder/</D:href>
  <D:propstat>
   <D:prop><D:getetag>"f"</D:getetag></D:prop>
   <D:status>HTTP/1.1 200 OK</D:status>
  </D:propstat>
 </D:response>
</D:multistatus>"#;
        let resources = parse_multistatus(xml).expect("parse");
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].href, "/dav/spaces/sid/folder/");
        // A trailing slash marks a collection even without resourcetype.
        assert!(resources[0].is_dir);
        assert!(is_success_status("HTTP/1.1 207 Multi-Status"));
        assert!(!is_success_status("HTTP/1.1 404 Not Found 200"));
    }

    const FIXTURE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:s="http://sabredav.org/ns">
 <D:response>
  <D:href>/dav/spaces/sid/</D:href>
  <D:propstat>
   <D:prop>
    <D:resourcetype><D:collection/></D:resourcetype>
    <D:getetag>&quot;root-etag&quot;</D:getetag>
    <D:quota-available-bytes>1073741824</D:quota-available-bytes>
   </D:prop>
   <D:status>HTTP/1.1 200 OK</D:status>
  </D:propstat>
 </D:response>
 <D:response>
  <D:href>/dav/spaces/sid/Ordner/</D:href>
  <D:propstat>
   <D:prop>
    <D:resourcetype><D:collection/></D:resourcetype>
   </D:prop>
   <D:status>HTTP/1.1 200 OK</D:status>
  </D:propstat>
 </D:response>
 <D:response>
  <D:href>/dav/spaces/sid/Ordner/Bericht%20Q1.pdf</D:href>
  <D:propstat>
   <D:prop>
    <D:resourcetype/>
    <D:getetag>&quot;etag-1&quot;</D:getetag>
    <D:getcontentlength>1024</D:getcontentlength>
    <D:getlastmodified>Wed, 21 Oct 2026 10:00:00 GMT</D:getlastmodified>
    <D:quota-available-bytes>999</D:quota-available-bytes>
   </D:prop>
   <D:status>HTTP/1.1 200 OK</D:status>
  </D:propstat>
 </D:response>
 <D:response>
  <D:href>/dav/spaces/sid/missing.txt</D:href>
  <D:propstat>
   <D:prop>
    <D:getetag/>
   </D:prop>
   <D:status>HTTP/1.1 404 Not Found</D:status>
  </D:propstat>
 </D:response>
</D:multistatus>"#;

    #[test]
    fn parses_properties_of_ok_propstats() {
        let resources = parse_multistatus(FIXTURE).expect("fixture parses");
        assert_eq!(resources.len(), 4);

        let root = &resources[0];
        assert_eq!(root.href, "/dav/spaces/sid/");
        assert!(root.is_dir);
        assert_eq!(root.etag.as_deref(), Some("\"root-etag\""));
        assert_eq!(root.quota_available_bytes, Some(1_073_741_824));
        assert_eq!(root.size, None);

        let file = &resources[2];
        assert_eq!(file.href, "/dav/spaces/sid/Ordner/Bericht%20Q1.pdf");
        assert!(!file.is_dir);
        assert_eq!(file.etag.as_deref(), Some("\"etag-1\""));
        assert_eq!(file.size, Some(1024));
        assert_eq!(
            file.last_modified.as_deref(),
            Some("Wed, 21 Oct 2026 10:00:00 GMT")
        );
    }

    #[test]
    fn propstats_with_error_status_are_skipped() {
        let resources = parse_multistatus(FIXTURE).expect("fixture parses");
        let missing = &resources[3];
        assert_eq!(missing.etag, None);
    }

    #[test]
    fn prefixed_and_unprefixed_namespaces_parse_equally() {
        let unprefixed = r#"<multistatus xmlns="DAV:">
 <response>
  <href>/dav/spaces/sid/</href>
  <propstat>
   <prop><resourcetype><collection/></resourcetype></prop>
   <status>HTTP/1.1 200 OK</status>
  </propstat>
 </response>
</multistatus>"#;
        let resources = parse_multistatus(unprefixed).expect("parses");
        assert_eq!(resources.len(), 1);
        assert!(resources[0].is_dir);
    }

    #[test]
    fn malformed_xml_is_a_protocol_error() {
        let bad_entity = r#"<D:multistatus xmlns:D="DAV:">
 <D:response>
  <D:href>/dav/spaces/sid/&bogus;</D:href>
 </D:response>
</D:multistatus>"#;
        assert!(matches!(
            parse_multistatus(bad_entity),
            Err(ApiError::Protocol(_))
        ));
    }

    #[test]
    fn invalid_numbers_are_protocol_errors() {
        let bad = r#"<D:multistatus xmlns:D="DAV:">
 <D:response>
  <D:href>/dav/spaces/sid/x</D:href>
  <D:propstat>
   <D:prop><D:getcontentlength>1o24</D:getcontentlength></D:prop>
   <D:status>HTTP/1.1 200 OK</D:status>
  </D:propstat>
 </D:response>
</D:multistatus>"#;
        assert!(matches!(parse_multistatus(bad), Err(ApiError::Protocol(_))));
    }

    #[test]
    fn quota_sentinels_mean_unknown() {
        // RFC 4331: negative quota-available-bytes are sentinels, not
        // numbers — e.g. OpenCloud/reva sends -2 for spaces without quota.
        let sentinel = r#"<D:multistatus xmlns:D="DAV:">
 <D:response>
  <D:href>/dav/spaces/sid/</D:href>
  <D:propstat>
   <D:prop>
    <D:resourcetype><D:collection/></D:resourcetype>
    <D:quota-available-bytes>-2</D:quota-available-bytes>
   </D:prop>
   <D:status>HTTP/1.1 200 OK</D:status>
  </D:propstat>
 </D:response>
</D:multistatus>"#;
        let resources = parse_multistatus(sentinel).expect("sentinel quota parses");
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].quota_available_bytes, None);
    }

    #[test]
    fn self_closing_collection_marks_a_directory() {
        let empty_collection = r#"<D:multistatus xmlns:D="DAV:">
 <D:response>
  <D:href>/dav/spaces/sid/sub/</D:href>
  <D:propstat>
   <D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop>
   <D:status>HTTP/1.1 200 OK</D:status>
  </D:propstat>
 </D:response>
</D:multistatus>"#;
        let resources = parse_multistatus(empty_collection).expect("parses");
        assert!(resources[0].is_dir);
    }
}
