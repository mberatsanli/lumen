//! Resource loading behind a swappable [`ResourceLoader`] trait.
//!
//! The engine crates never fetch anything; orchestration code passes a
//! loader in. `file://` and plain paths are served by [`FileLoader`],
//! `http(s)://` by [`HttpLoader`] (via `ureq` — HTTP transport and TLS are
//! deliberately not hand-written, unlike the parsing/layout pipeline).

pub use url::Url;

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
}

impl ResourceResponse {
    /// Body decoded as UTF-8, lossily.
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
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
pub trait ResourceLoader {
    fn load(&self, request: &ResourceRequest) -> Result<ResourceResponse, LoadError>;
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
        let body = std::fs::read(&path)?;
        Ok(ResourceResponse {
            final_url: request.url.clone(),
            content_type: guess_content_type(&path).map(str::to_string),
            body,
            set_cookies: Vec::new(),
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

/// Merges the caller's `Cookie` header with cookies collected along the
/// redirect chain (later hops override earlier same-name values). The
/// redirects here stay on one host, so `name=value` pairs suffice without
/// full domain/path matching.
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

        for _ in 0..=MAX_REDIRECTS {
            let cookie = chain_cookie_header(request.cookie.as_deref(), &chain);
            let mut response = match &body {
                Some((content_type, bytes)) => {
                    let mut builder = agent()
                        .post(url.as_str())
                        .header("Content-Type", content_type);
                    if let Some(cookie) = &cookie {
                        builder = builder.header("Cookie", cookie);
                    }
                    builder
                        .send(&bytes[..])
                        .map_err(|error| LoadError::Http(error.to_string()))?
                }
                None => {
                    let mut builder = agent().get(url.as_str());
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
                url = resolve(&url, location)?;
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
            let bytes = response
                .body_mut()
                .read_to_vec()
                .map_err(|error| LoadError::Http(error.to_string()))?;
            return Ok(ResourceResponse {
                final_url,
                content_type,
                body: bytes,
                set_cookies: all_set_cookies,
            });
        }
        Err(LoadError::Http(format!(
            "too many redirects (>{MAX_REDIRECTS})"
        )))
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
}
