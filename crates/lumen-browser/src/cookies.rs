//! A session cookie jar: stores `Set-Cookie` responses and builds the
//! `Cookie` header for matching requests (RFC 6265's practical core —
//! domain/path matching, Secure, Max-Age/Expires deletion). Header
//! parsing comes from the `cookie` crate; cookies live for the session
//! (no disk persistence).

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
        let path = parsed
            .path()
            .map_or_else(|| default_path(url), str::to_string);
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
            secure: parsed.secure().unwrap_or(false),
        });
    }

    /// The `Cookie` header value for a request, if anything matches.
    pub(crate) fn header_for(&self, url: &Url) -> Option<String> {
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
                let path_ok = path.starts_with(&cookie.path);
                let secure_ok = !cookie.secure || https;
                domain_ok && path_ok && secure_ok
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
            jar.header_for(&page).unwrap(),
            "sid=abc123; theme=dark; secret=s"
        );
        // Sibling subdomain: only the Domain=example.com cookie.
        assert_eq!(
            jar.header_for(&url("https://blog.example.com/")).unwrap(),
            "theme=dark"
        );
        // Plain http: the Secure cookie stays home.
        assert_eq!(
            jar.header_for(&url("http://shop.example.com/")).unwrap(),
            "sid=abc123; theme=dark"
        );
        // Foreign domain: nothing.
        assert!(jar.header_for(&url("https://evil.test/")).is_none());
        // Max-Age=0 deletes (logout).
        jar.store(&page, "sid=; Path=/; Max-Age=0");
        assert_eq!(
            jar.header_for(&page).unwrap(),
            "theme=dark; secret=s"
        );
    }

    #[test]
    fn foreign_domain_attribute_is_rejected() {
        let mut jar = CookieJar::default();
        jar.store(&url("https://a.test/"), "x=1; Domain=evil.test");
        assert!(jar.header_for(&url("https://evil.test/")).is_none());
    }
}
