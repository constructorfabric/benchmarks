//! Outbound query-string construction.
//!
//! Route matching validates the inbound query parameters against the route's
//! `query_allowlist`; auth plugins may then add a parameter (an API key in
//! `location: query`), so the query is kept as an ordered list of decoded
//! pairs and re-rendered at the end.

/// The decoded query parameters of the outbound request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutboundQuery {
    params: Vec<(String, String)>,
}

impl OutboundQuery {
    /// Parse a raw query string (the part after `?`). `None` yields no
    /// parameters. Malformed percent-escapes are passed through verbatim.
    #[must_use]
    pub fn parse(raw: Option<&str>) -> Self {
        let mut params = Vec::new();
        if let Some(raw) = raw {
            for pair in raw.split('&') {
                if pair.is_empty() {
                    continue;
                }
                let (name, value) = match pair.split_once('=') {
                    Some((name, value)) => (name, value),
                    None => (pair, ""),
                };
                params.push((percent_decode(name), percent_decode(value)));
            }
        }
        Self { params }
    }

    /// Set `name` to `value`, replacing any existing occurrence.
    pub fn set_query_param(&mut self, name: &str, value: &str) {
        self.params.retain(|(existing, _)| existing != name);
        self.params.push((name.to_owned(), value.to_owned()));
    }

    /// Append `name` to `value` without removing existing occurrences.
    pub fn append_query_param(&mut self, name: &str, value: &str) {
        self.params.push((name.to_owned(), value.to_owned()));
    }

    /// Every parameter, in order.
    #[must_use]
    pub fn all(&self) -> &[(String, String)] {
        &self.params
    }

    /// The distinct parameter names.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        let mut names = Vec::new();
        for (name, _) in &self.params {
            if !names
                .iter()
                .any(|existing: &&str| existing.eq_ignore_ascii_case(name))
            {
                names.push(name.as_str());
            }
        }
        names
    }

    /// Whether `name` is present (case-insensitive, as query names are).
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.params
            .iter()
            .any(|(existing, _)| existing.eq_ignore_ascii_case(name))
    }

    /// Render the query string, or `None` when there is nothing to render.
    #[must_use]
    pub fn render(&self) -> Option<String> {
        if self.params.is_empty() {
            return None;
        }
        let rendered = self
            .params
            .iter()
            .map(|(name, value)| format!("{}={}", percent_encode(name), percent_encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        Some(rendered)
    }
}

/// Percent-encode a query component (RFC 3986 §3.4).
///
/// The pair separators `&` and `=`, and the characters that would be read as
/// something else (`%`, `+`, `#`), are always escaped; a space becomes `%20`
/// rather than the form-encoding `+`, which an upstream would decode back into
/// a literal space anyway.
fn percent_encode(value: &str) -> String {
    const QUERY_SAFE: &[u8] = b"!$'()*+,;:@/?-._~";
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || QUERY_SAFE.contains(byte) {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Percent-decode a query component, leaving malformed escapes as-is.
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 3 <= bytes.len() => {
                match u8::from_str_radix(&raw[index + 1..index + 3], 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_re_renders() {
        let query = OutboundQuery::parse(Some("model=gpt-4&stream=true"));
        assert_eq!(query.names(), vec!["model", "stream"]);
        assert_eq!(query.render().as_deref(), Some("model=gpt-4&stream=true"));
    }

    #[test]
    fn empty_and_missing_queries_render_nothing() {
        assert!(OutboundQuery::parse(None).render().is_none());
        assert!(OutboundQuery::parse(Some("")).render().is_none());
    }

    #[test]
    fn set_query_param_replaces_existing() {
        let mut query = OutboundQuery::parse(Some("api_key=old&x=1"));
        query.set_query_param("api_key", "new");
        assert_eq!(query.render().as_deref(), Some("x=1&api_key=new"));
        assert!(query.contains("API_KEY"));
    }

    #[test]
    fn decodes_percent_escapes() {
        let query = OutboundQuery::parse(Some("a=1%202&b=c%2Bd"));
        assert_eq!(query.all()[0], ("a".to_owned(), "1 2".to_owned()));
        assert_eq!(query.all()[1], ("b".to_owned(), "c+d".to_owned()));
    }

    #[test]
    fn encodes_special_characters() {
        let mut query = OutboundQuery::default();
        query.set_query_param("a b", "c&d=e");
        assert_eq!(query.render().as_deref(), Some("a%20b=c%26d%3De"));
    }

    #[test]
    fn bare_parameters_survive_a_round_trip() {
        let query = OutboundQuery::parse(Some("flag"));
        assert_eq!(query.names(), vec!["flag"]);
    }
}
