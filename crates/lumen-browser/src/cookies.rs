//! A session cookie jar: stores `Set-Cookie` responses and builds the
//! `Cookie` header for matching requests (RFC 6265's practical core —
//! domain/path matching, Secure, HttpOnly, Max-Age/Expires deletion).
//! Header parsing comes from the `cookie` crate; cookies live for the
//! session (no disk persistence).

use lumen_platform::Url;

#[derive(Debug, Clone)]
struct StoredCookie {
    name: String,
    value: String,
    /// Lowercase domain without a leading dot.
    domain: String,
    /// Whether the cookie only matches the exact host (no Domain attr).
    host_only: bool,
    path: String,
    secure: bool,
    /// HttpOnly: sent over HTTP but invisible to `document.cookie`.
    http_only: bool,
}

#[derive(Debug, Default)]
pub(crate) struct CookieJar {
    cookies: Vec<StoredCookie>,
}

/// The default cookie path for a request URL (RFC 6265 §5.1.4).
fn default_path(url: &Url) -> String {
    let path = url.path();
    match path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(index) => path[..index].to_string(),
    }
}

impl CookieJar {
    /// Stores one raw `Set-Cookie` header value against the response URL.
    pub(crate) fn store(&mut self, url: &Url, header: &str) {
        let Ok(parsed) = cookie::Cookie::parse(header.to_string()) else {
            return;
        };
        let Some(host) = url.host_str() else {
            return;
        };
        let secure = parsed.secure().unwrap_or(false);
        // RFC 6265bis §5.5: a Secure cookie is only accepted from a
        // secure origin — this covers both http `Set-Cookie` responses
        // and `document.cookie` writes on http pages.
        if secure && url.scheme() != "https" {
            return;
        }
        let (domain, host_only) = match parsed.domain() {
            Some(domain) => {
                let domain = domain.trim_start_matches('.').to_ascii_lowercase();
                // A cookie may only widen to a suffix of the host.
                let host = host.to_ascii_lowercase();
                if host != domain && !host.ends_with(&format!(".{domain}")) {
                    return;
                }
                (domain, false)
            }
            None => (host.to_ascii_lowercase(), true),
        };
        // A Path attribute that is not absolute is ignored, falling
        // back to the default path (RFC 6265 §5.2.4).
        let path = match parsed.path() {
            Some(path) if path.starts_with('/') => path.to_string(),
            _ => default_path(url),
        };
        let expired = parsed
            .max_age()
            .is_some_and(|age| age.is_zero() || age.is_negative())
            || matches!(
                parsed.expires(),
                Some(cookie::Expiration::DateTime(when))
                    if when <= cookie::time::OffsetDateTime::now_utc()
            );
        let name = parsed.name().to_string();
        self.cookies.retain(|existing| {
            !(existing.name == name && existing.domain == domain && existing.path == path)
        });
        if expired {
            return;
        }
        self.cookies.push(StoredCookie {
            name,
            value: parsed.value().to_string(),
            domain,
            host_only,
            path,
            secure,
            http_only: parsed.http_only().unwrap_or(false),
        });
    }

    /// The `Cookie` header value for an HTTP request, if anything matches.
    pub(crate) fn header_for_http(&self, url: &Url) -> Option<String> {
        self.matched_header(url, true)
    }

    /// `document.cookie`'s view: same matching, but HttpOnly cookies are
    /// invisible to script.
    pub(crate) fn header_for(&self, url: &Url) -> Option<String> {
        self.matched_header(url, false)
    }

    fn matched_header(&self, url: &Url, include_http_only: bool) -> Option<String> {
        let host = url.host_str()?.to_ascii_lowercase();
        let path = url.path();
        let https = url.scheme() == "https";
        let matched: Vec<String> = self
            .cookies
            .iter()
            .filter(|cookie| {
                let domain_ok = if cookie.host_only {
                    host == cookie.domain
                } else {
                    host == cookie.domain || host.ends_with(&format!(".{}", cookie.domain))
                };
                // RFC 6265 §5.1.4: exact match, or a prefix match where
                // the cookie path ends in '/' or the next character of
                // the request path is '/'.
                let path_ok = path == cookie.path
                    || (path.starts_with(&cookie.path)
                        && (cookie.path.ends_with('/')
                            || path[cookie.path.len()..].starts_with('/')));
                let secure_ok = !cookie.secure || https;
                let script_ok = include_http_only || !cookie.http_only;
                domain_ok && path_ok && secure_ok && script_ok
            })
            .map(|cookie| format!("{}={}", cookie.name, cookie.value))
            .collect();
        (!matched.is_empty()).then(|| matched.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(value: &str) -> Url {
        value.parse().unwrap()
    }

    #[test]
    fn stores_matches_and_deletes() {
        let mut jar = CookieJar::default();
        let page = url("https://shop.example.com/sepet/liste");
        jar.store(&page, "sid=abc123; Path=/; HttpOnly");
        jar.store(&page, "theme=dark; Domain=example.com; Path=/");
        jar.store(&page, "secret=s; Secure; Path=/");

        // Same host: all three.
        assert_eq!(
            jar.header_for_http(&page).unwrap(),
            "sid=abc123; theme=dark; secret=s"
        );
        // Sibling subdomain: only the Domain=example.com cookie.
        assert_eq!(
            jar.header_for_http(&url("https://blog.example.com/")).unwrap(),
            "theme=dark"
        );
        // Plain http: the Secure cookie stays home.
        assert_eq!(
            jar.header_for_http(&url("http://shop.example.com/")).unwrap(),
            "sid=abc123; theme=dark"
        );
        // Foreign domain: nothing.
        assert!(jar.header_for_http(&url("https://evil.test/")).is_none());
        // Max-Age=0 deletes (logout).
        jar.store(&page, "sid=; Path=/; Max-Age=0");
        assert_eq!(jar.header_for_http(&page).unwrap(), "theme=dark; secret=s");
    }

    #[test]
    fn foreign_domain_attribute_is_rejected() {
        let mut jar = CookieJar::default();
        jar.store(&url("https://a.test/"), "x=1; Domain=evil.test");
        assert!(jar.header_for_http(&url("https://evil.test/")).is_none());
    }

    #[test]
    fn http_only_is_hidden_from_script_but_sent_over_http() {
        let mut jar = CookieJar::default();
        let page = url("https://a.test/");
        jar.store(&page, "sid=abc; HttpOnly; Path=/");
        jar.store(&page, "tema=koyu; Path=/");
        // HTTP requests carry both…
        assert_eq!(jar.header_for_http(&page).unwrap(), "sid=abc; tema=koyu");
        // …but document.cookie only sees the script-visible one.
        assert_eq!(jar.header_for(&page).unwrap(), "tema=koyu");
    }

    #[test]
    fn path_matching_follows_rfc_6265() {
        let mut jar = CookieJar::default();
        let page = url("https://a.test/docs/page");
        jar.store(&page, "a=1; Path=/docs");
        jar.store(&page, "b=2; Path=/docs/");
        // Exact match; "/docs/" does not match "/docs".
        assert_eq!(jar.header_for_http(&url("https://a.test/docs")).unwrap(), "a=1");
        // Prefix + next char '/', plus the cookie path ending in '/'.
        assert_eq!(
            jar.header_for_http(&url("https://a.test/docs/x")).unwrap(),
            "a=1; b=2"
        );
        // A bare string prefix is NOT a match.
        assert!(jar.header_for_http(&url("https://a.test/docsify")).is_none());
    }

    #[test]
    fn relative_path_attribute_falls_back_to_default_path() {
        let mut jar = CookieJar::default();
        jar.store(&url("https://a.test/dir/page"), "x=1; Path=relative");
        // The default path of /dir/page is /dir.
        assert_eq!(
            jar.header_for_http(&url("https://a.test/dir/other")).unwrap(),
            "x=1"
        );
        assert!(jar.header_for_http(&url("https://a.test/")).is_none());
    }

    #[test]
    fn secure_cookie_is_only_accepted_over_https() {
        let mut jar = CookieJar::default();
        // Rejected from an insecure origin…
        jar.store(&url("http://a.test/"), "s=1; Secure; Path=/");
        assert!(jar.header_for_http(&url("https://a.test/")).is_none());
        // …accepted over https, and still never sent over plain http.
        jar.store(&url("https://a.test/"), "s=1; Secure; Path=/");
        assert_eq!(jar.header_for_http(&url("https://a.test/")).unwrap(), "s=1");
        assert!(jar.header_for_http(&url("http://a.test/")).is_none());
    }
}
