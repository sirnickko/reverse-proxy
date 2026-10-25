//! Core connection handler and request-forwarding logic.
//!
//! handle_connection() runs the keep-alive loop for one client connection.
//! forward() opens a fresh upstream connection per request (stage 1;
//! connection pooling comes in stage 2).

use std::{
    io::{BufReader, Write},
    net::TcpStream,
    time::Instant,
};

use crate::{headers, request, response, upstream};

/// Handle one client TCP connection for its full keep-alive lifetime.
/// Called once per accepted socket, from a worker thread.
pub fn handle_connection(client: TcpStream, upstream_addr: &str) {
    let client_ip = client
        .peer_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    client.set_read_timeout(Some(std::time::Duration::from_secs(10))).ok();
    let mut client = client;

    loop {
        // ---- Parse request (head + body) ------------------------------------
        // We use by_ref() so the borrow is released when the block ends,
        // allowing us to write through `client` afterwards.
        let req = {
            let mut reader = BufReader::new(std::io::Read::by_ref(&mut client));
            match request::parse(&mut reader) {
                Ok(r) => r,
                Err(request::ParseError::ConnectionClosed) => break,
                Err(request::ParseError::BodyTooLarge) => {
                    send_error(&mut client, 413, "Request Entity Too Large");
                    break;
                }
                Err(e) => {
                    eprintln!("[proxy] parse error from {client_ip}: {e}");
                    send_error(&mut client, 400, "Bad Request");
                    break;
                }
            }
        }; // reader dropped -- borrow of client released

        // ---- Determine client keep-alive semantics --------------------------
        let conn_val = req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("connection"))
            .map(|(_, v)| v.to_ascii_lowercase());
        let client_keep_alive = match req.version.as_str() {
            "1.1" => conn_val.as_deref() != Some("close"),
            _     => conn_val.as_deref() == Some("keep-alive"),
        };

        let t0 = Instant::now();

        // ---- Forward to upstream --------------------------------------------
        match forward(&req, &client_ip, upstream_addr) {
            Ok((status, reason, resp_headers, body)) => {
                let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
                eprintln!(
                    "[proxy] {} {} -> {} {}  {elapsed_ms:.2}ms",
                    req.method, req.target, upstream_addr, status,
                );
                let body_slice: &[u8] = if req.method == "HEAD" { &[] } else { &body };
                if response::write_response(
                    &mut client,
                    status,
                    &reason,
                    &resp_headers,
                    body_slice,
                    client_keep_alive,
                ).is_err() {
                    break;
                }
                if !client_keep_alive { break; }
            }
            Err(e) => {
                eprintln!("[proxy] upstream error ({upstream_addr}): {e}");
                let (code, reason) = if e.contains("timed out") {
                    (504u16, "Gateway Timeout")
                } else {
                    (502u16, "Bad Gateway")
                };
                send_error(&mut client, code, reason);
                break;
            }
        }
    }
}

/// Forward one request to upstream_addr.
/// Returns (status, reason, filtered_headers, body_bytes) on success.
fn forward(
    req: &request::Request,
    client_ip: &str,
    upstream_addr: &str,
) -> Result<(u16, String, Vec<(String, String)>, Vec<u8>), String> {
    // Connect to upstream.
    let mut up = upstream::connect(upstream_addr)
        .map_err(|e| format!("connect: {e}"))?;

    // Build forwarded request headers.
    let mut fwd_headers = req.headers.clone();
    headers::strip_hop_by_hop(&mut fwd_headers);
    headers::add_x_forwarded_for(&mut fwd_headers, client_ip);
    headers::set_x_forwarded_proto(&mut fwd_headers, "http");
    // Use Connection: close upstream; we buffer the body to re-use the
    // client connection independently.
    fwd_headers.push(("Connection".to_string(), "close".to_string()));
    // Fix Content-Length after potential de-chunking.
    fwd_headers.retain(|(k, _)| {
        !k.eq_ignore_ascii_case("content-length")
            && !k.eq_ignore_ascii_case("transfer-encoding")
    });
    if !req.body.is_empty() {
        fwd_headers.push(("Content-Length".to_string(), req.body.len().to_string()));
    }

    // Send request to upstream.
    write!(up, "{} {} HTTP/1.1\r\n", req.method, req.target)
        .map_err(|e| format!("write request line: {e}"))?;
    for (k, v) in &fwd_headers {
        write!(up, "{k}: {v}\r\n").map_err(|e| format!("write header: {e}"))?;
    }
    write!(up, "\r\n").map_err(|e| format!("write blank line: {e}"))?;
    if !req.body.is_empty() {
        up.write_all(&req.body).map_err(|e| format!("write body: {e}"))?;
    }
    up.flush().map_err(|e| format!("flush upstream: {e}"))?;

    // Read response from upstream.
    let mut up_reader = BufReader::new(&up);
    let head = response::parse_head(&mut up_reader, &req.method)
        .map_err(|e| format!("parse response: {e}"))?;
    let body = response::read_body(&mut up_reader, &head.framing)
        .map_err(|e| format!("read response body: {e}"))?;

    // Filter hop-by-hop from response headers.
    let mut resp_headers = head.headers;
    headers::strip_hop_by_hop(&mut resp_headers);
    // Remove Content-Length / Transfer-Encoding -- we add our own C-L.
    resp_headers.retain(|(k, _)| {
        !k.eq_ignore_ascii_case("content-length")
            && !k.eq_ignore_ascii_case("transfer-encoding")
    });

    Ok((head.status, head.reason, resp_headers, body))
}

/// Write a minimal plain-text error response and close.
fn send_error<W: Write>(w: &mut W, code: u16, reason: &str) {
    let body = format!("{code} {reason}\n");
    let _ = write!(
        w,
        "HTTP/1.1 {code} {reason}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len(),
    );
    let _ = w.flush();
}
