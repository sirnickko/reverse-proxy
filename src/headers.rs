//! Hop-by-hop header filtering and X-Forwarded-* injection.
//!
//! Hop-by-hop headers are consumed by a single transport link and must not
//! be forwarded (RFC 7230 ss.6.1). We also remove any headers the client
//! explicitly lists in its `Connection:` value.

/// Standard hop-by-hop header names (all lowercase for comparison).
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

/// Remove all hop-by-hop headers AND any headers explicitly named in the
/// `Connection:` header value from `headers`.
pub fn strip_hop_by_hop(headers: &mut Vec<(String, String)>) {
    let extra: Vec<String> = headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("connection"))
        .flat_map(|(_, v)| {
            v.split(',')
                .map(|s| s.trim().to_ascii_lowercase())
                .collect::<Vec<_>>()
        })
        .collect();

    headers.retain(|(k, _)| {
        let kl = k.to_ascii_lowercase();
        !HOP_BY_HOP.contains(&kl.as_str()) && !extra.contains(&kl)
    });
}

/// Append the client IP to an existing `X-Forwarded-For` header, or create one.
pub fn add_x_forwarded_for(headers: &mut Vec<(String, String)>, client_ip: &str) {
    for (k, v) in headers.iter_mut() {
        if k.eq_ignore_ascii_case("x-forwarded-for") {
            v.push_str(", ");
            v.push_str(client_ip);
            return;
        }
    }
    headers.push(("X-Forwarded-For".to_string(), client_ip.to_string()));
}

/// Set (or replace) `X-Forwarded-Proto`.
pub fn set_x_forwarded_proto(headers: &mut Vec<(String, String)>, proto: &str) {
    headers.retain(|(k, _)| !k.eq_ignore_ascii_case("x-forwarded-proto"));
    headers.push(("X-Forwarded-Proto".to_string(), proto.to_string()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(k: &str, v: &str) -> (String, String) { (k.to_string(), v.to_string()) }

    #[test]
    fn strips_standard_hop_by_hop() {
        let mut h = vec![
            hdr("Connection",        "keep-alive"),
            hdr("Keep-Alive",        "timeout=5"),
            hdr("Transfer-Encoding", "chunked"),
            hdr("Content-Type",      "text/html"),
            hdr("Upgrade",           "websocket"),
        ];
        strip_hop_by_hop(&mut h);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].0, "Content-Type");
    }

    #[test]
    fn strips_connection_listed_headers() {
        let mut h = vec![
            hdr("Connection", "X-Custom, X-Other"),
            hdr("X-Custom",   "v1"),
            hdr("X-Other",    "v2"),
            hdr("X-Keep",     "yes"),
        ];
        strip_hop_by_hop(&mut h);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].0, "X-Keep");
    }

    #[test]
    fn proxy_connection_stripped() {
        let mut h = vec![hdr("Proxy-Connection", "keep-alive"), hdr("Host", "a.com")];
        strip_hop_by_hop(&mut h);
        assert!(h.iter().all(|(k, _)| k == "Host"));
    }

    #[test]
    fn x_forwarded_for_created() {
        let mut h: Vec<(String, String)> = vec![];
        add_x_forwarded_for(&mut h, "1.2.3.4");
        assert_eq!(h, vec![hdr("X-Forwarded-For", "1.2.3.4")]);
    }

    #[test]
    fn x_forwarded_for_appended() {
        let mut h = vec![hdr("X-Forwarded-For", "10.0.0.1")];
        add_x_forwarded_for(&mut h, "1.2.3.4");
        assert_eq!(h[0].1, "10.0.0.1, 1.2.3.4");
    }

    #[test]
    fn x_forwarded_proto_replaced() {
        let mut h = vec![hdr("X-Forwarded-Proto", "https")];
        set_x_forwarded_proto(&mut h, "http");
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].1, "http");
    }
}
