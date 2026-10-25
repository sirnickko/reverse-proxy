//! HTTP/1.x request parser.
//!
//! Parses the request line, headers, and body in one call.
//! Chunked request bodies are de-chunked into a plain Vec<u8> so the rest
//! of the proxy never has to think about transfer encoding.

use std::io::{BufRead, BufReader, Read};

/// Maximum total bytes for request line + all headers combined.
const MAX_HEADER_BYTES: usize = 8_192;
/// Maximum number of individual header fields.
const MAX_HEADERS: usize = 100;
/// Maximum request body the proxy will buffer (16 MiB).
pub const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// A fully parsed HTTP/1.x request.
#[derive(Debug)]
pub struct Request {
    pub method:  String,
    /// Raw request-target, e.g. `/path?q=1` (query string included).
    pub target:  String,
    /// "1.0" or "1.1".
    pub version: String,
    /// Headers in original order; keys are NOT lowercased.
    pub headers: Vec<(String, String)>,
    /// Body bytes, de-chunked if the client used chunked encoding.
    pub body:    Vec<u8>,
}

/// How the request body is framed.
#[derive(Debug, Clone)]
pub enum BodyFraming {
    None,
    ContentLength(usize),
    Chunked,
}

/// Errors that can occur while parsing.
#[derive(Debug)]
pub enum ParseError {
    /// Clean EOF -- normal end of a keep-alive session.
    ConnectionClosed,
    /// Protocol violation.
    Malformed(String),
    /// Body exceeds MAX_BODY_BYTES.
    BodyTooLarge,
    /// Underlying I/O error.
    Io(std::io::Error),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::ConnectionClosed => write!(f, "connection closed"),
            ParseError::Malformed(s)     => write!(f, "malformed request: {s}"),
            ParseError::BodyTooLarge     => write!(f, "request body too large"),
            ParseError::Io(e)            => write!(f, "I/O error: {e}"),
        }
    }
}

impl From<std::io::Error> for ParseError {
    fn from(e: std::io::Error) -> Self {
        use std::io::ErrorKind::*;
        match e.kind() {
            ConnectionReset | ConnectionAborted | UnexpectedEof | TimedOut | WouldBlock => {
                ParseError::ConnectionClosed
            }
            _ => ParseError::Io(e),
        }
    }
}

/// Parse one complete HTTP/1.x request (head + body) from `reader`.
///
/// The caller must keep the same BufReader alive for the entire connection.
/// Creating a new one per request silently discards buffered bytes,
/// corrupting pipelined requests.
pub fn parse<R: Read>(reader: &mut BufReader<R>) -> Result<Request, ParseError> {
    // ---- 1. Request line ---------------------------------------------------
    let mut request_line = String::new();
    let mut header_bytes = 0usize;

    // RFC 7230 s3.5 -- skip blank lines that may precede the request.
    loop {
        let n = reader.read_line(&mut request_line)?;
        if n == 0 { return Err(ParseError::ConnectionClosed); }
        header_bytes += n;
        if header_bytes > MAX_HEADER_BYTES {
            return Err(ParseError::Malformed("request line too long".into()));
        }
        if !request_line.trim().is_empty() { break; }
        request_line.clear();
    }

    let rl = request_line.trim();
    let mut parts = rl.splitn(3, ' ');
    let method = parts
        .next()
        .ok_or_else(|| ParseError::Malformed("missing method".into()))?
        .to_ascii_uppercase();
    let target = parts
        .next()
        .ok_or_else(|| ParseError::Malformed("missing request-target".into()))?
        .to_string();
    let ver_str = parts
        .next()
        .ok_or_else(|| ParseError::Malformed("missing HTTP version".into()))?;
    let version = if ver_str.starts_with("HTTP/") {
        ver_str["HTTP/".len()..].to_string()
    } else {
        return Err(ParseError::Malformed(format!("bad version: {ver_str:?}")));
    };

    // ---- 2. Headers --------------------------------------------------------
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 { return Err(ParseError::ConnectionClosed); }
        header_bytes += n;
        if header_bytes > MAX_HEADER_BYTES {
            return Err(ParseError::Malformed("headers too large".into()));
        }
        let trimmed = line.trim();
        if trimmed.is_empty() { break; }
        if headers.len() >= MAX_HEADERS {
            return Err(ParseError::Malformed("too many headers".into()));
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }

    // ---- 3. Body -----------------------------------------------------------
    let framing = body_framing(&headers);
    let body = read_body(reader, &framing)?;

    Ok(Request { method, target, version, headers, body })
}

/// Determine body framing from request headers.
pub fn body_framing(headers: &[(String, String)]) -> BodyFraming {
    // Transfer-Encoding: chunked takes priority (RFC 7230 s3.3.3 rule 3).
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("transfer-encoding")
            && v.to_ascii_lowercase().contains("chunked")
        {
            return BodyFraming::Chunked;
        }
    }
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("content-length") {
            match v.trim().parse::<usize>() {
                Ok(0)  => return BodyFraming::None,
                Ok(n)  => return BodyFraming::ContentLength(n),
                Err(_) => {}
            }
        }
    }
    BodyFraming::None
}

/// Read and buffer the request body. Chunked bodies are de-chunked.
pub fn read_body<R: Read>(
    reader: &mut BufReader<R>,
    framing: &BodyFraming,
) -> Result<Vec<u8>, ParseError> {
    match framing {
        BodyFraming::None => Ok(Vec::new()),
        BodyFraming::ContentLength(n) => {
            if *n > MAX_BODY_BYTES { return Err(ParseError::BodyTooLarge); }
            let mut buf = vec![0u8; *n];
            reader.read_exact(&mut buf)?;
            Ok(buf)
        }
        BodyFraming::Chunked => read_chunked(reader),
    }
}

fn read_chunked<R: Read>(reader: &mut BufReader<R>) -> Result<Vec<u8>, ParseError> {
    let mut body: Vec<u8> = Vec::new();
    loop {
        let mut size_line = String::new();
        reader.read_line(&mut size_line)?;
        let hex = size_line.trim().split(';').next().unwrap_or("").trim();
        let chunk_size = usize::from_str_radix(hex, 16)
            .map_err(|_| ParseError::Malformed(format!("bad chunk size: {hex:?}")))?;
        if chunk_size == 0 {
            loop {
                let mut trail = String::new();
                let n = reader.read_line(&mut trail)?;
                if n == 0 || trail.trim().is_empty() { break; }
            }
            break;
        }
        if body.len() + chunk_size > MAX_BODY_BYTES { return Err(ParseError::BodyTooLarge); }
        let start = body.len();
        body.resize(start + chunk_size, 0);
        reader.read_exact(&mut body[start..])?;
        let mut crlf = String::new();
        reader.read_line(&mut crlf)?;
    }
    Ok(body)
}

// ---- Unit tests ------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    fn parse_str(s: &str) -> Result<Request, ParseError> {
        let mut r = BufReader::new(s.as_bytes());
        parse(&mut r)
    }

    #[test]
    fn simple_get() {
        let req = parse_str("GET / HTTP/1.1\r\nHost: example.com\r\n\r\n").unwrap();
        assert_eq!(req.method,  "GET");
        assert_eq!(req.target,  "/");
        assert_eq!(req.version, "1.1");
        assert_eq!(req.headers.len(), 1);
        assert!(req.body.is_empty());
    }

    #[test]
    fn get_with_query_string() {
        let req = parse_str("GET /search?q=hello&page=2 HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(req.target, "/search?q=hello&page=2");
    }

    #[test]
    fn post_with_content_length_body() {
        let req = parse_str("POST /submit HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello").unwrap();
        assert_eq!(req.body, b"hello");
    }

    #[test]
    fn chunked_body_decoded() {
        let s = "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let req = parse_str(s).unwrap();
        assert_eq!(req.body, b"hello");
    }

    #[test]
    fn multi_chunk_body_decoded() {
        let s = "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let req = parse_str(s).unwrap();
        assert_eq!(req.body, b"hello world");
    }

    #[test]
    fn blank_lines_before_request_skipped() {
        let req = parse_str("\r\n\r\nGET / HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(req.method, "GET");
    }

    #[test]
    fn empty_stream_connection_closed() {
        assert!(matches!(parse_str(""), Err(ParseError::ConnectionClosed)));
    }

    #[test]
    fn method_uppercased() {
        let req = parse_str("get / HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(req.method, "GET");
    }

    #[test]
    fn multiple_headers_preserved() {
        let s = "GET / HTTP/1.1\r\nHost: a.com\r\nAccept: */*\r\nX-Foo: bar\r\n\r\n";
        let req = parse_str(s).unwrap();
        assert_eq!(req.headers.len(), 3);
    }

    #[test]
    fn body_framing_none() {
        let h = vec![("Host".to_string(), "a.com".to_string())];
        assert!(matches!(body_framing(&h), BodyFraming::None));
    }

    #[test]
    fn body_framing_content_length() {
        let h = vec![("Content-Length".to_string(), "42".to_string())];
        assert!(matches!(body_framing(&h), BodyFraming::ContentLength(42)));
    }

    #[test]
    fn body_framing_chunked() {
        let h = vec![("Transfer-Encoding".to_string(), "chunked".to_string())];
        assert!(matches!(body_framing(&h), BodyFraming::Chunked));
    }

    #[test]
    fn chunked_takes_priority_over_content_length() {
        let h = vec![
            ("Transfer-Encoding".to_string(), "chunked".to_string()),
            ("Content-Length".to_string(), "99".to_string()),
        ];
        assert!(matches!(body_framing(&h), BodyFraming::Chunked));
    }

    #[test]
    fn http10_version_parsed() {
        let req = parse_str("GET / HTTP/1.0\r\n\r\n").unwrap();
        assert_eq!(req.version, "1.0");
    }
}
