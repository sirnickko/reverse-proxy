//! HTTP/1.x response parser + body reader.
//!
//! After parsing status line and headers, the body is buffered into a Vec<u8>
//! (chunked bodies are de-chunked). The proxy re-sends with a plain
//! Content-Length header, which lets it maintain client keep-alive even when
//! the upstream used Connection: close.

use std::io::{BufRead, BufReader, Read, Write};

const MAX_HEAD_BYTES: usize = 16_384;
const MAX_HEADERS:    usize = 100;
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024; // 64 MiB

/// Parsed response head (status line + headers, no body).
#[derive(Debug)]
pub struct ResponseHead {
    pub version: String,
    pub status:  u16,
    pub reason:  String,
    pub headers: Vec<(String, String)>,
    pub framing: BodyFraming,
}

/// How the response body is framed.
#[derive(Debug, Clone)]
pub enum BodyFraming {
    /// 1xx / 204 / 304 / HEAD -- no body at all (RFC 7230 s3.3).
    NoBody,
    ContentLength(usize),
    Chunked,
    /// Neither header present -- read until upstream closes connection.
    ReadUntilClose,
}

/// Parse status line + headers from an upstream response.
/// `request_method` is needed for the HEAD/204/304 no-body rule.
pub fn parse_head<R: Read>(
    reader: &mut BufReader<R>,
    request_method: &str,
) -> Result<ResponseHead, String> {
    let mut status_line = String::new();
    let mut total = 0usize;
    let n = reader.read_line(&mut status_line).map_err(|e| e.to_string())?;
    if n == 0 { return Err("upstream closed before sending a response".into()); }
    total += n;

    let sl = status_line.trim();
    let mut parts = sl.splitn(3, ' ');
    let ver_str = parts.next().ok_or("bad status line: missing version")?;
    let version = if ver_str.starts_with("HTTP/") {
        ver_str["HTTP/".len()..].to_string()
    } else {
        return Err(format!("bad HTTP version: {ver_str:?}"));
    };
    let status: u16 = parts
        .next().ok_or("bad status line: missing status code")?
        .parse().map_err(|e| format!("bad status code: {e}"))?;
    let reason = parts.next().unwrap_or("").to_string();

    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).map_err(|e| e.to_string())?;
        if n == 0 { return Err("upstream closed mid-headers".into()); }
        total += n;
        if total > MAX_HEAD_BYTES { return Err("response headers too large".into()); }
        let trimmed = line.trim();
        if trimmed.is_empty() { break; }
        if headers.len() >= MAX_HEADERS { return Err("too many response headers".into()); }
        if let Some((k, v)) = trimmed.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }

    let framing = determine_framing(request_method, status, &headers);
    Ok(ResponseHead { version, status, reason, headers, framing })
}

fn determine_framing(method: &str, status: u16, headers: &[(String, String)]) -> BodyFraming {
    if (100..200).contains(&status) || status == 204 || status == 304 || method == "HEAD" {
        return BodyFraming::NoBody;
    }
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("transfer-encoding")
            && v.to_ascii_lowercase().contains("chunked")
        {
            return BodyFraming::Chunked;
        }
    }
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("content-length") {
            if let Ok(n) = v.trim().parse::<usize>() {
                return BodyFraming::ContentLength(n);
            }
        }
    }
    BodyFraming::ReadUntilClose
}

/// Read and buffer the response body. Chunked bodies are de-chunked.
pub fn read_body<R: Read>(
    reader: &mut BufReader<R>,
    framing: &BodyFraming,
) -> std::io::Result<Vec<u8>> {
    match framing {
        BodyFraming::NoBody => Ok(Vec::new()),
        BodyFraming::ContentLength(n) => {
            let n = *n;
            if n > MAX_BODY_BYTES {
                return Err(std::io::Error::new(std::io::ErrorKind::Other, "response body too large"));
            }
            let mut buf = vec![0u8; n];
            reader.read_exact(&mut buf)?;
            Ok(buf)
        }
        BodyFraming::Chunked => read_chunked(reader),
        BodyFraming::ReadUntilClose => {
            let mut buf = Vec::new();
            reader.read_to_end(&mut buf)?;
            Ok(buf)
        }
    }
}

fn read_chunked<R: Read>(reader: &mut BufReader<R>) -> std::io::Result<Vec<u8>> {
    let mut body: Vec<u8> = Vec::new();
    loop {
        let mut size_line = String::new();
        reader.read_line(&mut size_line)?;
        let hex = size_line.trim().split(';').next().unwrap_or("").trim();
        let chunk_size = usize::from_str_radix(hex, 16).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bad chunk size: {hex:?}"))
        })?;
        if chunk_size == 0 {
            loop {
                let mut trail = String::new();
                let n = reader.read_line(&mut trail)?;
                if n == 0 || trail.trim().is_empty() { break; }
            }
            break;
        }
        if body.len() + chunk_size > MAX_BODY_BYTES {
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "chunked response body too large"));
        }
        let start = body.len();
        body.resize(start + chunk_size, 0);
        reader.read_exact(&mut body[start..])?;
        let mut crlf = String::new();
        reader.read_line(&mut crlf)?;
    }
    Ok(body)
}

/// Write a proxied response to `writer`.
/// Adds Content-Length from body.len(); strips Transfer-Encoding (body is
/// already de-chunked); adds Connection header based on keep_alive.
pub fn write_response<W: Write>(
    writer: &mut W,
    status: u16,
    reason: &str,
    headers: &[(String, String)],
    body: &[u8],
    keep_alive: bool,
) -> std::io::Result<()> {
    write!(writer, "HTTP/1.1 {status} {reason}\r\n")?;
    for (k, v) in headers {
        write!(writer, "{k}: {v}\r\n")?;
    }
    let conn = if keep_alive { "keep-alive" } else { "close" };
    write!(writer, "Connection: {conn}\r\n")?;
    if keep_alive {
        write!(writer, "Keep-Alive: timeout=5, max=100\r\n")?;
    }
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(body)?;
    writer.flush()
}

// ---- Unit tests ------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    fn parse(raw: &str, method: &str) -> ResponseHead {
        let mut r = BufReader::new(raw.as_bytes());
        parse_head(&mut r, method).unwrap()
    }

    #[test]
    fn simple_200() {
        let h = parse("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n", "GET");
        assert_eq!(h.status, 200);
        assert_eq!(h.reason, "OK");
        assert!(matches!(h.framing, BodyFraming::ContentLength(5)));
    }

    #[test]
    fn head_response_no_body() {
        let h = parse("HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n", "HEAD");
        assert!(matches!(h.framing, BodyFraming::NoBody));
    }

    #[test]
    fn chunked_framing() {
        let h = parse("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n", "GET");
        assert!(matches!(h.framing, BodyFraming::Chunked));
    }

    #[test]
    fn no_length_header_read_until_close() {
        let h = parse("HTTP/1.1 200 OK\r\n\r\n", "GET");
        assert!(matches!(h.framing, BodyFraming::ReadUntilClose));
    }

    #[test]
    fn status_204_no_body() {
        let h = parse("HTTP/1.1 204 No Content\r\n\r\n", "DELETE");
        assert!(matches!(h.framing, BodyFraming::NoBody));
    }

    #[test]
    fn status_304_no_body() {
        let h = parse("HTTP/1.1 304 Not Modified\r\nETag: \"abc\"\r\n\r\n", "GET");
        assert!(matches!(h.framing, BodyFraming::NoBody));
    }

    #[test]
    fn status_404_forwarded() {
        let h = parse("HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\n", "GET");
        assert_eq!(h.status, 404);
        assert!(matches!(h.framing, BodyFraming::ContentLength(9)));
    }

    #[test]
    fn chunked_body_de_chunked() {
        let data = "5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let mut r = BufReader::new(data.as_bytes());
        let body = read_chunked(&mut r).unwrap();
        assert_eq!(body, b"hello world");
    }

    #[test]
    fn content_length_body_read() {
        let mut r = BufReader::new("hello world".as_bytes());
        let body = read_body(&mut r, &BodyFraming::ContentLength(11)).unwrap();
        assert_eq!(body, b"hello world");
    }

    #[test]
    fn no_body_returns_empty() {
        let mut r = BufReader::new("".as_bytes());
        let body = read_body(&mut r, &BodyFraming::NoBody).unwrap();
        assert!(body.is_empty());
    }
}
