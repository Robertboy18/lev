//! Common HTTP URL validation.
//!
//! Protocol modules still own paths, authentication, and response handling.

use std::net::IpAddr;

use anyhow::{Context, Result, bail};

/// An absolute HTTP(S) URL with a host and no embedded credentials.
pub(crate) struct HttpUrl {
    uri: ureq::http::Uri,
}

impl HttpUrl {
    /// Parse the structural rules shared by all HTTP clients.
    pub(crate) fn parse(value: &str, subject: &str) -> Result<Self> {
        let uri: ureq::http::Uri = value
            .parse()
            .with_context(|| format!("{subject} must be an absolute HTTP(S) URL: {value:?}"))?;
        let scheme = uri
            .scheme_str()
            .with_context(|| format!("{subject} must be absolute: {value:?}"))?;
        if !matches!(scheme, "http" | "https") {
            bail!("{subject} uses unsupported URL scheme {scheme:?}");
        }
        let authority = uri
            .authority()
            .with_context(|| format!("{subject} must contain a host"))?;
        if authority.as_str().contains('@') {
            bail!("{subject} must not contain embedded credentials");
        }
        if uri.host().is_none_or(str::is_empty) {
            bail!("{subject} must contain a host");
        }
        Ok(Self { uri })
    }

    /// Require HTTPS except for loopback HTTP or an explicit insecure opt-in.
    pub(crate) fn require_secure(&self, subject: &str, allow_insecure_http: bool) -> Result<()> {
        if self.uri.scheme_str() == Some("http") && !allow_insecure_http && !self.is_loopback() {
            bail!("{subject} must use HTTPS; HTTP is allowed only for loopback");
        }
        Ok(())
    }

    /// Whether the URL contains a query component.
    pub(crate) fn has_query(&self) -> bool {
        self.uri
            .path_and_query()
            .and_then(|path| path.query())
            .is_some()
    }

    /// Whether requests and all redirects should be restricted to HTTPS.
    pub(crate) fn is_https(&self) -> bool {
        self.uri.scheme_str() == Some("https")
    }

    fn is_loopback(&self) -> bool {
        let host = self
            .uri
            .host()
            .expect("validated HTTP URL always contains a host");
        host.eq_ignore_ascii_case("localhost")
            || host
                .trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    }
}

#[cfg(test)]
mod tests {
    use super::HttpUrl;

    #[test]
    fn distinguishes_https_from_allowed_loopback_http() {
        let secure = HttpUrl::parse("https://example.com/cache", "test URL").unwrap();
        secure.require_secure("test URL", false).unwrap();
        assert!(secure.is_https());

        let loopback = HttpUrl::parse("http://127.0.0.1:8080/cache", "test URL").unwrap();
        loopback.require_secure("test URL", false).unwrap();
        assert!(!loopback.is_https());

        let remote_http = HttpUrl::parse("http://example.com/cache", "test URL").unwrap();
        assert!(remote_http.require_secure("test URL", false).is_err());
    }
}
