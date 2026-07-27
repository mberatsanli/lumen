//! Resource loading behind a swappable [`ResourceLoader`] trait.
//!
//! The engine crates never fetch anything; orchestration code passes a
//! loader in. `file://` and plain paths are served by [`FileLoader`],
//! `http(s)://` by [`HttpLoader`] (via `ureq` — HTTP transport and TLS are
//! deliberately not hand-written, unlike the parsing/layout pipeline).

pub use url::Url;

use std::io::Read as _;

/// Most bytes a single response body (or local file) may occupy in
/// memory — unbounded reads are a memory-DoS vector, so every load
/// funnels through [`read_body_capped`].
const MAX_BODY_BYTES: u64 = 256 * 1024 * 1024;

/// Reads at most `limit` bytes from `reader`, silently truncating the
/// rest.
fn read_body_capped(reader: impl std::io::Read, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// A request for one resource.
#[derive(Debug, Clone)]
pub struct ResourceRequest {
    pub url: Url,
    /// `Cookie` header to send, when the caller's jar has matches.
    pub cookie: Option<String>,
    /// POST body as (content type, bytes); the request is a GET when
    /// `None`.
    pub body: Option<(String, Vec<u8>)>,
}

impl ResourceRequest {
    /// A plain GET with no cookies.
    #[must_use]
    pub fn get(url: Url) -> Self {
        Self {
            url,
            cookie: None,
            body: None,
        }
    }
}

/// A loaded resource.
#[derive(Debug, Clone)]
pub struct ResourceResponse {
    /// Final URL after redirects — the base for relative references.
    pub final_url: Url,
    /// `Content-Type` header value, or a guess from the file extension.
    pub content_type: Option<String>,
    pub body: Vec<u8>,
    /// Raw `Set-Cookie` header values, in response order.
    pub set_cookies: Vec<String>,
    /// Raw `Access-Control-Allow-Origin` header value, when present —
    /// the CORS grant a session checks before letting a page's script
    /// read a cross-origin response.
    pub access_control_allow_origin: Option<String>,
}

impl ResourceResponse {
    /// Body decoded to text, sniffing the encoding from the BOM, the
    /// Content-Type header and `<meta>` declarations (falling back to
    /// lossy UTF-8). See [`crate::encoding`].
    #[must_use]
    pub fn text(&self) -> String {
        crate::encoding::decode_text(&self.body, self.content_type.as_deref())
    }
}

#[derive(Debug)]
pub enum LoadError {
    InvalidUrl(String),
    UnsupportedScheme(String),
    Io(std::io::Error),
    Http(String),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUrl(url) => write!(formatter, "invalid URL: {url}"),
            Self::UnsupportedScheme(scheme) => {
                write!(formatter, "unsupported URL scheme: {scheme}")
            }
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Http(message) => write!(formatter, "HTTP error: {message}"),
        }
    }
}

impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for LoadError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

/// Loads resources by URL.
pub trait ResourceLoader: Send + Sync {
    fn load(&self, request: &ResourceRequest) -> Result<ResourceResponse, LoadError>;

    /// Answers a CORS preflight probe (HTTP OPTIONS). The default
    /// refuses: a loader without HTTP semantics cannot grant a
    /// non-simple cross-origin read, and a refused preflight fails the
    /// request like a network error.
    fn preflight(&self, _probe: &CorsPreflight) -> Result<CorsGrant, LoadError> {
        Err(LoadError::Http(
            "CORS preflight not supported by this loader".to_string(),
        ))
    }
}

/// A CORS preflight probe: may a non-simple cross-origin script read
/// (a method beyond GET/HEAD/POST, or custom request headers) go out?
/// HTTP loaders answer it with an OPTIONS request.
#[derive(Debug, Clone)]
pub struct CorsPreflight {
    /// URL of the actual request.
    pub url: Url,
    /// Method of the actual request (`Access-Control-Request-Method`).
    pub method: String,
    /// Custom (non-safelisted) header names of the actual request
    /// (`Access-Control-Request-Headers`), lowercase.
    pub headers: Vec<String>,
}

/// What an answered preflight grants, parsed from its response headers.
#[derive(Debug, Clone, Default)]
pub struct CorsGrant {
    /// Raw `Access-Control-Allow-Origin` value.
    pub allow_origin: Option<String>,
    /// `Access-Control-Allow-Methods` entries, lowercase.
    pub allow_methods: Vec<String>,
    /// `Access-Control-Allow-Headers` entries, lowercase.
    pub allow_headers: Vec<String>,
}

impl CorsGrant {
    /// Whether the grant covers `method` and every one of `headers`.
    /// A `*` entry covers anything only for non-credentialed reads —
    /// with credentials the spec reads it as a literal name.
    #[must_use]
    pub fn covers(&self, method: &str, headers: &[String], credentialed: bool) -> bool {
        let listed = |list: &[String], name: &str| {
            list.iter().any(|entry| entry == name)
                || (!credentialed && list.iter().any(|entry| entry == "*"))
        };
        listed(&self.allow_methods, &method.to_ascii_lowercase())
            && headers
                .iter()
                .all(|header| listed(&self.allow_headers, header))
    }
}

/// Serves `file://` URLs from the local filesystem.
#[derive(Debug, Clone, Copy, Default)]
pub struct FileLoader;

impl ResourceLoader for FileLoader {
    fn load(&self, request: &ResourceRequest) -> Result<ResourceResponse, LoadError> {
        let path = request
            .url
            .to_file_path()
            .map_err(|()| LoadError::InvalidUrl(request.url.to_string()))?;
        let body = read_body_capped(std::fs::File::open(&path)?, MAX_BODY_BYTES)?;
        Ok(ResourceResponse {
            final_url: request.url.clone(),
            content_type: guess_content_type(&path).map(str::to_string),
            body,
            set_cookies: Vec::new(),
            access_control_allow_origin: None,
        })
    }
}

/// Serves `http://` and `https://` URLs. Redirects are followed by hand
/// (ureq's auto-follow is disabled) so every hop's `Set-Cookie` is
/// captured — a login flow's session cookie lives on the 3xx response,
/// not the final page.
#[derive(Debug, Clone, Copy, Default)]
pub struct HttpLoader;

/// Most redirect hops any one request will follow before giving up.
const MAX_REDIRECTS: usize = 10;

/// Shared HTTP agent: 5s connect / 20s total per request, so a stalled
/// server can never hang a caller indefinitely. Auto-redirects are off;
/// [`HttpLoader`] follows them itself to keep intermediate cookies.
fn agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_connect(Some(std::time::Duration::from_secs(5)))
            .timeout_global(Some(std::time::Duration::from_secs(20)))
            .max_redirects(0)
            .build()
            .into()
    })
}

/// The browser's identity on the wire, shared with
/// `navigator.userAgent`. Mozilla-compatible prefix: many servers
/// 403 unknown or empty agents.
pub const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Lumen/0.1";

/// Headers every HTTP request carries, browser-style. No
/// `Accept-Encoding`: this build does not decompress.
const DEFAULT_HEADERS: [(&str, &str); 3] = [
    ("User-Agent", USER_AGENT),
    (
        "Accept",
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
    ),
    ("Accept-Language", "en-US,en;q=0.9"),
];

/// Merges the caller's `Cookie` header with cookies collected along the
/// redirect chain (later hops override earlier same-name values). The
/// caller keeps the chain on one origin: a cross-origin hop stops
/// sending the base header and drops the pairs collected so far.
fn chain_cookie_header(base: Option<&str>, chain: &[(String, String)]) -> Option<String> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    if let Some(base) = base {
        for part in base.split(';') {
            if let Some((name, value)) = part.trim().split_once('=') {
                pairs.push((name.to_string(), value.to_string()));
            }
        }
    }
    for (name, value) in chain {
        pairs.retain(|(existing, _)| existing != name);
        pairs.push((name.clone(), value.clone()));
    }
    (!pairs.is_empty()).then(|| {
        pairs
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; ")
    })
}

impl ResourceLoader for HttpLoader {
    fn load(&self, request: &ResourceRequest) -> Result<ResourceResponse, LoadError> {
        use ureq::ResponseExt as _;
        let mut url = request.url.clone();
        // POST body only rides the first hop; a 301/302/303 turns the
        // follow-up into a GET.
        let mut body = request.body.clone();
        // Every `Set-Cookie` across the whole chain, for the caller's jar.
        let mut all_set_cookies: Vec<String> = Vec::new();
        // Name=value pairs to replay as `Cookie` on the next hop.
        let mut chain: Vec<(String, String)> = Vec::new();
        // The caller's `Cookie` header belongs to the original URL's
        // origin; a cross-origin hop must never see it.
        let mut send_base = true;

        for _ in 0..=MAX_REDIRECTS {
            let base = if send_base {
                request.cookie.as_deref()
            } else {
                None
            };
            let cookie = chain_cookie_header(base, &chain);
            let mut response = match &body {
                Some((content_type, bytes)) => {
                    let mut builder = agent()
                        .post(url.as_str())
                        .header("Content-Type", content_type);
                    for (name, value) in DEFAULT_HEADERS {
                        builder = builder.header(name, value);
                    }
                    if let Some(cookie) = &cookie {
                        builder = builder.header("Cookie", cookie);
                    }
                    builder
                        .send(&bytes[..])
                        .map_err(|error| LoadError::Http(error.to_string()))?
                }
                None => {
                    let mut builder = agent().get(url.as_str());
                    for (name, value) in DEFAULT_HEADERS {
                        builder = builder.header(name, value);
                    }
                    if let Some(cookie) = &cookie {
                        builder = builder.header("Cookie", cookie);
                    }
                    builder
                        .call()
                        .map_err(|error| LoadError::Http(error.to_string()))?
                }
            };

            for value in response.headers().get_all("set-cookie") {
                let Ok(raw) = value.to_str() else { continue };
                if let Some((name, val)) = raw.split(';').next().and_then(|p| p.split_once('=')) {
                    let (name, val) = (name.trim().to_string(), val.trim().to_string());
                    chain.retain(|(existing, _)| *existing != name);
                    chain.push((name, val));
                }
                all_set_cookies.push(raw.to_string());
            }

            let status = response.status().as_u16();
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            if let (301..=303 | 307 | 308, Some(location)) = (status, location.as_deref()) {
                let next = resolve(&url, location)?;
                if next.origin() != url.origin() {
                    // Cross-origin hop: neither the caller's cookies nor
                    // the pairs collected so far may leak to the new host.
                    send_base = false;
                    chain.clear();
                }
                url = next;
                // 301/302/303 downgrade the method to GET; 307/308 keep it.
                if matches!(status, 301..=303) {
                    body = None;
                }
                continue;
            }

            let final_url = response
                .get_uri()
                .to_string()
                .parse()
                .unwrap_or_else(|_| url.clone());
            let content_type = response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            let bytes = read_body_capped(response.body_mut().as_reader(), MAX_BODY_BYTES)
                .map_err(|error| LoadError::Http(error.to_string()))?;
            let access_control_allow_origin = response
                .headers()
                .get("access-control-allow-origin")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            return Ok(ResourceResponse {
                final_url,
                content_type,
                body: bytes,
                set_cookies: all_set_cookies,
                access_control_allow_origin,
            });
        }
        Err(LoadError::Http(format!(
            "too many redirects (>{MAX_REDIRECTS})"
        )))
    }

    fn preflight(&self, probe: &CorsPreflight) -> Result<CorsGrant, LoadError> {
        // The preflight itself is never credentialed and never
        // redirected: it asks a question, it does not carry state.
        let mut builder = agent()
            .options(probe.url.as_str())
            .header("Access-Control-Request-Method", &probe.method);
        for (name, value) in DEFAULT_HEADERS {
            builder = builder.header(name, value);
        }
        if !probe.headers.is_empty() {
            builder = builder.header("Access-Control-Request-Headers", &probe.headers.join(", "));
        }
        let response = builder
            .call()
            .map_err(|error| LoadError::Http(error.to_string()))?;
        if !response.status().is_success() {
            return Err(LoadError::Http(format!(
                "CORS preflight answered {}",
                response.status()
            )));
        }
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        };
        let list = |name: &str| {
            header(name)
                .map(|value| {
                    value
                        .split(',')
                        .map(|entry| entry.trim().to_ascii_lowercase())
                        .filter(|entry| !entry.is_empty())
                        .collect()
                })
                .unwrap_or_default()
        };
        Ok(CorsGrant {
            allow_origin: header("access-control-allow-origin"),
            allow_methods: list("access-control-allow-methods"),
            allow_headers: list("access-control-allow-headers"),
        })
    }
}

/// Dispatches to [`FileLoader`] or [`HttpLoader`] by scheme.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultLoader;

impl ResourceLoader for DefaultLoader {
    fn load(&self, request: &ResourceRequest) -> Result<ResourceResponse, LoadError> {
        match request.url.scheme() {
            "file" => FileLoader.load(request),
            "http" | "https" => HttpLoader.load(request),
            scheme => Err(LoadError::UnsupportedScheme(scheme.to_string())),
        }
    }

    fn preflight(&self, probe: &CorsPreflight) -> Result<CorsGrant, LoadError> {
        match probe.url.scheme() {
            "http" | "https" => HttpLoader.preflight(probe),
            scheme => Err(LoadError::UnsupportedScheme(scheme.to_string())),
        }
    }
}

/// Parses `input` as an absolute URL, or as a filesystem path (made
/// absolute against the current directory) when it has no scheme.
pub fn url_from_user_input(input: &str) -> Result<Url, LoadError> {
    if let Ok(url) = Url::parse(input) {
        return Ok(url);
    }
    let path = std::path::absolute(input)?;
    Url::from_file_path(&path).map_err(|()| LoadError::InvalidUrl(input.to_string()))
}

/// Resolves a (possibly relative) reference against a base URL.
pub fn resolve(base: &Url, reference: &str) -> Result<Url, LoadError> {
    base.join(reference)
        .map_err(|error| LoadError::InvalidUrl(format!("{reference}: {error}")))
}

fn guess_content_type(path: &std::path::Path) -> Option<&'static str> {
    match path.extension()?.to_str()? {
        "html" | "htm" => Some("text/html"),
        "css" => Some("text/css"),
        "svg" => Some("image/svg+xml"),
        "txt" => Some("text/plain"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_loader_reads_local_files() {
        let path = std::path::absolute("../../examples/hello.html").unwrap();
        let url = Url::from_file_path(&path).unwrap();
        let response = FileLoader.load(&ResourceRequest::get(url)).unwrap();
        assert!(response.text().contains("Hello from Lumen"));
        assert_eq!(response.content_type.as_deref(), Some("text/html"));
    }

    #[test]
    fn requests_carry_browser_headers() {
        let (base, rx) = serve(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_string(),
        ]);
        HttpLoader
            .load(&ResourceRequest::get(Url::parse(&base).unwrap()))
            .unwrap();
        let head = seen(rx)[0].to_ascii_lowercase();
        assert!(
            head.contains(&format!("user-agent: {}", USER_AGENT.to_ascii_lowercase())),
            "missing User-Agent in {head}"
        );
        assert!(head.contains("accept: text/html"), "missing Accept in {head}");
        assert!(
            head.contains("accept-language: en-us"),
            "missing Accept-Language in {head}"
        );
    }

    #[test]
    fn missing_file_is_an_io_error() {
        let url = Url::from_file_path("/definitely/not/here.html").unwrap();
        let error = FileLoader.load(&ResourceRequest::get(url)).unwrap_err();
        assert!(matches!(error, LoadError::Io(_)));
    }

    #[test]
    fn default_loader_rejects_unknown_schemes() {
        let url = Url::parse("ftp://example.com/x").unwrap();
        let error = DefaultLoader.load(&ResourceRequest::get(url)).unwrap_err();
        assert!(matches!(error, LoadError::UnsupportedScheme(scheme) if scheme == "ftp"));
    }

    #[test]
    fn resolves_relative_references() {
        let base = Url::parse("https://example.com/docs/page.html").unwrap();
        assert_eq!(
            resolve(&base, "style.css").unwrap().as_str(),
            "https://example.com/docs/style.css"
        );
        assert_eq!(
            resolve(&base, "../other/a.html").unwrap().as_str(),
            "https://example.com/other/a.html"
        );
        assert_eq!(
            resolve(&base, "/root.css").unwrap().as_str(),
            "https://example.com/root.css"
        );
    }

    #[test]
    fn user_input_accepts_urls_and_paths() {
        assert_eq!(
            url_from_user_input("https://example.com/")
                .unwrap()
                .scheme(),
            "https"
        );
        assert_eq!(
            url_from_user_input("examples/hello.html").unwrap().scheme(),
            "file"
        );
    }

    /// Runs a stub HTTP server that answers one request per queued
    /// response, then reports the request heads it saw. Responses must
    /// carry `Connection: close` so ureq never pools a dead socket.
    fn serve(responses: Vec<String>) -> (String, std::sync::mpsc::Receiver<Vec<String>>) {
        use std::io::{BufRead as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut seen = Vec::new();
            for response in &responses {
                let (stream, _) = listener.accept().unwrap();
                let mut head = String::new();
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    head.push_str(&line);
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                }
                seen.push(head);
                let mut stream = stream;
                stream.write_all(response.as_bytes()).unwrap();
            }
            tx.send(seen).unwrap();
        });
        (base, rx)
    }

    fn seen(seen: std::sync::mpsc::Receiver<Vec<String>>) -> Vec<String> {
        seen.recv_timeout(std::time::Duration::from_secs(10))
            .unwrap()
    }

    #[test]
    fn same_origin_redirect_replays_chain_cookies() {
        let (base, rx) = serve(vec![
            "HTTP/1.1 302 Found\r\nLocation: /final\r\nSet-Cookie: hop=1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_string(),
        ]);
        let mut request = ResourceRequest::get(Url::parse(&format!("{base}/start")).unwrap());
        request.cookie = Some("base=1".to_string());
        let response = HttpLoader.load(&request).unwrap();
        assert_eq!(response.text(), "ok");
        assert_eq!(response.set_cookies, vec!["hop=1"]);
        let seen = seen(rx);
        assert!(
            seen[1]
                .to_ascii_lowercase()
                .contains("cookie: base=1; hop=1"),
            "second hop must carry base + chain cookies: {}",
            seen[1]
        );
    }

    #[test]
    fn cross_origin_redirect_drops_cookies() {
        // Two servers on different ports: same host, different origin.
        let (base_b, rx_b) = serve(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_string(),
        ]);
        let (base_a, rx_a) = serve(vec![format!(
            "HTTP/1.1 302 Found\r\nLocation: {base_b}/final\r\nSet-Cookie: hop=1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )]);
        let mut request = ResourceRequest::get(Url::parse(&format!("{base_a}/start")).unwrap());
        request.cookie = Some("base=1".to_string());
        let response = HttpLoader.load(&request).unwrap();
        assert_eq!(response.text(), "ok");
        let seen_b = seen(rx_b);
        assert!(
            !seen_b[0].to_ascii_lowercase().contains("cookie:"),
            "cross-origin hop must not see any cookie: {}",
            seen_b[0]
        );
        // The Set-Cookie is still reported for the caller's jar (which
        // does its own domain matching).
        assert_eq!(response.set_cookies, vec!["hop=1"]);
        seen(rx_a);
    }

    #[test]
    fn http_loader_captures_the_acao_header() {
        let (base, rx) = serve(vec![
            "HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_string(),
        ]);
        let request = ResourceRequest::get(Url::parse(&format!("{base}/data")).unwrap());
        let response = HttpLoader.load(&request).unwrap();
        assert_eq!(response.access_control_allow_origin.as_deref(), Some("*"));
        seen(rx);
    }

    #[test]
    fn preflight_sends_options_and_parses_the_grant() {
        let (base, rx) = serve(vec![
            "HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: https://a.test\r\nAccess-Control-Allow-Methods: GET, PUT\r\nAccess-Control-Allow-Headers: X-Token, Content-Type\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
        ]);
        let probe = CorsPreflight {
            url: Url::parse(&format!("{base}/data")).unwrap(),
            method: "PUT".to_string(),
            headers: vec!["x-token".to_string()],
        };
        let grant = HttpLoader.preflight(&probe).unwrap();
        assert_eq!(grant.allow_origin.as_deref(), Some("https://a.test"));
        assert_eq!(grant.allow_methods, vec!["get", "put"]);
        assert_eq!(grant.allow_headers, vec!["x-token", "content-type"]);
        assert!(grant.covers("PUT", &["x-token".to_string()], true));
        assert!(!grant.covers("DELETE", &[], false));
        let seen = seen(rx);
        let head = seen[0].to_ascii_lowercase();
        assert!(
            head.starts_with("options /data "),
            "probe method: {}",
            seen[0]
        );
        assert!(
            head.contains("access-control-request-method: put"),
            "{head}"
        );
        assert!(
            head.contains("access-control-request-headers: x-token"),
            "{head}"
        );
        assert!(
            !head.contains("cookie:"),
            "preflight is never credentialed: {head}"
        );
    }

    #[test]
    fn preflight_without_cors_headers_grants_nothing() {
        let (base, rx) = serve(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
        ]);
        let probe = CorsPreflight {
            url: Url::parse(&format!("{base}/data")).unwrap(),
            method: "PUT".to_string(),
            headers: Vec::new(),
        };
        let grant = HttpLoader.preflight(&probe).unwrap();
        assert_eq!(grant.allow_origin, None);
        assert!(!grant.covers("PUT", &[], false));
        seen(rx);
    }

    #[test]
    fn preflight_error_status_refuses() {
        let (base, rx) = serve(vec![
            "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_string(),
        ]);
        let probe = CorsPreflight {
            url: Url::parse(&format!("{base}/data")).unwrap(),
            method: "PUT".to_string(),
            headers: Vec::new(),
        };
        assert!(HttpLoader.preflight(&probe).is_err());
        seen(rx);
    }

    #[test]
    fn bodies_are_capped() {
        let data = vec![b'x'; 1024];
        assert_eq!(read_body_capped(&data[..], 10).unwrap().len(), 10);
        assert_eq!(
            read_body_capped(&data[..], MAX_BODY_BYTES).unwrap().len(),
            1024
        );
    }
}
