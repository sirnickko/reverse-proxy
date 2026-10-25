//! Integration tests for the reverse proxy.
//!
//! Each test:
//!   1. Spins up a fake TCP backend (acts as the upstream server).
//!   2. Spins up the proxy accept loop pointing at that backend.
//!   3. Sends a raw HTTP request to the proxy.
//!   4. Asserts on the raw HTTP response.

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use rproxy::proxy;

// ---- Helpers ----------------------------------------------------------------

/// Spin up a fake HTTP backend on a random port.
fn spawn_backend<F>(handler: F) -> u16
where
    F: Fn(String) -> String + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handler = Arc::new(handler);
    thread::spawn(move || {
        for stream in listener.incoming() {
            if let Ok(mut s) = stream {
                let h = Arc::clone(&handler);
                thread::spawn(move || {
                    let raw = read_raw_request(&mut s);
                    let response = h(raw);
                    let _ = s.write_all(response.as_bytes());
                    let _ = s.flush();
                });
            }
        }
    });
    port
}

/// Read one complete HTTP request (headers + body) using a BufReader.
fn read_raw_request(stream: &mut TcpStream) -> String {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let cloned = stream.try_clone().unwrap();
    let mut reader = BufReader::new(cloned);

    let mut headers = String::new();
    let mut content_length = 0usize;

    // Read header lines one at a time.
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 { break; }
        // Extract Content-Length while reading.
        if line.to_ascii_lowercase().starts_with("content-length:") {
            if let Some(n) = line.split(':').nth(1) {
                content_length = n.trim().parse().unwrap_or(0);
            }
        }
        let done = line == "\r\n" || line == "\n";
        headers.push_str(&line);
        if done { break; }
    }

    // Read body based on the Content-Length we found in headers.
    let mut body = String::new();
    if content_length > 0 {
        let mut buf = vec![0u8; content_length];
        reader.read_exact(&mut buf).ok();
        body = String::from_utf8_lossy(&buf).into_owned();
    }

    headers + &body
}

/// Spin up the proxy accept loop pointing at upstream_port.
fn spawn_proxy(upstream_port: u16) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let upstream = format!("127.0.0.1:{upstream_port}");
    thread::spawn(move || {
        for stream in listener.incoming() {
            if let Ok(s) = stream {
                let up = upstream.clone();
                thread::spawn(move || proxy::handle_connection(s, &up));
            }
        }
    });
    thread::sleep(Duration::from_millis(20));
    port
}

/// Send a raw HTTP request and read until EOF.
fn roundtrip(proxy_port: u16, request: &str) -> String {
    let mut s = TcpStream::connect(format!("127.0.0.1:{proxy_port}")).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    s.write_all(request.as_bytes()).unwrap();
    s.flush().unwrap();
    let mut resp = String::new();
    let _ = s.read_to_string(&mut resp);
    resp
}

/// Read exactly one HTTP response using Content-Length (no EOF required).
fn read_one_response(stream: &mut TcpStream) -> String {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let cloned = stream.try_clone().unwrap();
    let mut reader = BufReader::new(cloned);

    let mut headers = String::new();
    let mut content_length = 0usize;

    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 { break; }
        if line.to_ascii_lowercase().starts_with("content-length:") {
            if let Some(n) = line.split(':').nth(1) {
                content_length = n.trim().parse().unwrap_or(0);
            }
        }
        let done = line == "\r\n" || line == "\n";
        headers.push_str(&line);
        if done { break; }
    }

    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).ok();
    headers + &String::from_utf8_lossy(&body)
}

// ---- Tests ------------------------------------------------------------------

#[test]
fn simple_get_forwarded() {
    let bp = spawn_backend(|_| {
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello".into()
    });
    let pp = spawn_proxy(bp);
    let resp = roundtrip(pp, "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "unexpected: {resp:?}");
    assert!(resp.ends_with("hello"),                 "body missing: {resp:?}");
}

#[test]
fn post_body_forwarded() {
    let received = Arc::new(Mutex::new(String::new()));
    let rc = Arc::clone(&received);
    let bp = spawn_backend(move |req| {
        if let Some(idx) = req.find("\r\n\r\n") {
            *rc.lock().unwrap() = req[idx + 4..].to_string();
        }
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".into()
    });
    let pp = spawn_proxy(bp);
    roundtrip(
        pp,
        "POST /submit HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndata",
    );
    // Give the backend handler thread time to store the body.
    thread::sleep(Duration::from_millis(100));
    assert_eq!(*received.lock().unwrap(), "data", "body not forwarded");
}

#[test]
fn hop_by_hop_stripped_from_request() {
    let received = Arc::new(Mutex::new(String::new()));
    let rc = Arc::clone(&received);
    let bp = spawn_backend(move |req| {
        *rc.lock().unwrap() = req;
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
    });
    let pp = spawn_proxy(bp);
    roundtrip(
        pp,
        "GET / HTTP/1.1\r\nHost: localhost\r\nKeep-Alive: timeout=5\r\nProxy-Connection: keep-alive\r\nConnection: close\r\n\r\n",
    );
    thread::sleep(Duration::from_millis(50));
    let req = received.lock().unwrap().to_ascii_lowercase();
    assert!(!req.contains("keep-alive: "),     "Keep-Alive leaked to upstream");
    assert!(!req.contains("proxy-connection"), "Proxy-Connection leaked to upstream");
}

#[test]
fn hop_by_hop_stripped_from_response() {
    let bp = spawn_backend(|_| {
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nKeep-Alive: timeout=5\r\nConnection: close\r\n\r\nhi".into()
    });
    let pp = spawn_proxy(bp);
    let resp = roundtrip(pp, "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    assert!(!resp.to_ascii_lowercase().contains("keep-alive: timeout"), "Keep-Alive leaked to client");
}

#[test]
fn x_forwarded_for_added() {
    let received = Arc::new(Mutex::new(String::new()));
    let rc = Arc::clone(&received);
    let bp = spawn_backend(move |req| {
        *rc.lock().unwrap() = req;
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
    });
    let pp = spawn_proxy(bp);
    roundtrip(pp, "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    thread::sleep(Duration::from_millis(50));
    let req = received.lock().unwrap().to_ascii_lowercase();
    assert!(req.contains("x-forwarded-for:"), "X-Forwarded-For missing: {req}");
}

#[test]
fn x_forwarded_proto_set() {
    let received = Arc::new(Mutex::new(String::new()));
    let rc = Arc::clone(&received);
    let bp = spawn_backend(move |req| {
        *rc.lock().unwrap() = req;
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
    });
    let pp = spawn_proxy(bp);
    roundtrip(pp, "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    thread::sleep(Duration::from_millis(50));
    let req = received.lock().unwrap().to_ascii_lowercase();
    assert!(req.contains("x-forwarded-proto: http"), "X-Forwarded-Proto missing: {req}");
}

#[test]
fn returns_502_when_backend_unreachable() {
    let dead_port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    thread::sleep(Duration::from_millis(10));

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let pp = listener.local_addr().unwrap().port();
    let upstream = format!("127.0.0.1:{dead_port}");
    thread::spawn(move || {
        for stream in listener.incoming() {
            if let Ok(s) = stream {
                let up = upstream.clone();
                thread::spawn(move || proxy::handle_connection(s, &up));
            }
        }
    });
    thread::sleep(Duration::from_millis(20));

    let resp = roundtrip(pp, "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    assert!(
        resp.starts_with("HTTP/1.1 502") || resp.starts_with("HTTP/1.1 504"),
        "expected 502/504, got: {resp:?}",
    );
}

#[test]
fn large_response_body_forwarded_intact() {
    let large: String = "x".repeat(1_000_000);
    let lb = large.clone();
    let bp = spawn_backend(move |_| {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            lb.len(), lb
        )
    });
    let pp = spawn_proxy(bp);
    let resp = roundtrip(pp, "GET /large HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    assert!(resp.contains("200 OK"), "status wrong");
    let body_start = resp.find("\r\n\r\n").unwrap() + 4;
    assert_eq!(resp[body_start..].len(), 1_000_000, "body length mismatch");
}

#[test]
fn chunked_upstream_response_de_chunked_for_client() {
    let bp = spawn_backend(|_| {
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n\
         5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n".into()
    });
    let pp = spawn_proxy(bp);
    let resp = roundtrip(pp, "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    assert!(resp.contains("200 OK"));
    assert!(
        resp.to_ascii_lowercase().contains("content-length: 11"),
        "Content-Length: 11 missing: {resp}"
    );
    assert!(
        !resp.to_ascii_lowercase().contains("transfer-encoding"),
        "Transfer-Encoding leaked to client: {resp}"
    );
    assert!(resp.ends_with("hello world"), "body wrong: {resp:?}");
}

#[test]
fn non_200_status_codes_forwarded() {
    for (code, phrase, body) in [
        (404u16, "Not Found",             "not found"),
        (500u16, "Internal Server Error", "oops"),
        (403u16, "Forbidden",             "go away"),
    ] {
        let bp = spawn_backend(move |_| {
            format!(
                "HTTP/1.1 {code} {phrase}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        });
        let pp = spawn_proxy(bp);
        let resp = roundtrip(pp, "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        assert!(resp.starts_with(&format!("HTTP/1.1 {code}")), "expected {code}: {resp:?}");
        assert!(resp.ends_with(body), "body missing for {code}: {resp:?}");
    }
}

#[test]
fn head_request_no_body_to_client() {
    let bp = spawn_backend(|_| {
        "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n".into()
    });
    let pp = spawn_proxy(bp);
    let resp = roundtrip(pp, "HEAD / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "status wrong: {resp:?}");
    let body_start = resp.find("\r\n\r\n").unwrap() + 4;
    assert!(resp[body_start..].is_empty(), "HEAD must have empty body");
}

#[test]
fn host_header_preserved_to_backend() {
    let received = Arc::new(Mutex::new(String::new()));
    let rc = Arc::clone(&received);
    let bp = spawn_backend(move |req| {
        *rc.lock().unwrap() = req;
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
    });
    let pp = spawn_proxy(bp);
    roundtrip(pp, "GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n");
    thread::sleep(Duration::from_millis(50));
    let req = received.lock().unwrap().clone();
    assert!(req.contains("Host: example.com"), "Host not forwarded: {req}");
}

#[test]
fn query_string_forwarded_intact() {
    let received = Arc::new(Mutex::new(String::new()));
    let rc = Arc::clone(&received);
    let bp = spawn_backend(move |req| {
        if let Some(line) = req.lines().next() {
            *rc.lock().unwrap() = line.to_string();
        }
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
    });
    let pp = spawn_proxy(bp);
    roundtrip(
        pp,
        "GET /search?q=rust+proxy&page=2 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    thread::sleep(Duration::from_millis(50));
    let line = received.lock().unwrap().clone();
    assert!(line.contains("/search?q=rust+proxy&page=2"), "query string lost: {line}");
}

#[test]
fn keep_alive_two_requests_one_connection() {
    let counter = Arc::new(Mutex::new(0u32));
    let c = Arc::clone(&counter);
    let bp = spawn_backend(move |_| {
        *c.lock().unwrap() += 1;
        "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello".into()
    });
    let pp = spawn_proxy(bp);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{pp}")).unwrap();

    stream.write_all(b"GET /first HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
    let first = read_one_response(&mut stream);
    assert!(first.contains("200 OK"),  "first: {first}");
    assert!(first.ends_with("hello"),  "first body: {first}");

    stream.write_all(b"GET /second HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").unwrap();
    let second = read_one_response(&mut stream);
    assert!(second.contains("200 OK"), "second: {second}");
    assert!(second.ends_with("hello"), "second body: {second}");

    thread::sleep(Duration::from_millis(100));
    assert_eq!(*counter.lock().unwrap(), 2, "backend must receive exactly 2 requests");
}

#[test]
fn chunked_request_body_de_chunked_before_upstream() {
    let received = Arc::new(Mutex::new(String::new()));
    let rc = Arc::clone(&received);
    let bp = spawn_backend(move |req| {
        *rc.lock().unwrap() = req;
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
    });
    let pp = spawn_proxy(bp);
    roundtrip(
        pp,
        "POST / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n\
         5\r\nhello\r\n0\r\n\r\n",
    );
    thread::sleep(Duration::from_millis(100));
    let req = received.lock().unwrap().clone();
    let lower = req.to_ascii_lowercase();
    assert!(!lower.contains("transfer-encoding"), "chunked TE leaked upstream: {req}");
    assert!(lower.contains("content-length: 5"),  "Content-Length missing upstream: {req}");
    // Body comes after the blank line.
    let body_part = req.split("\r\n\r\n").nth(1).unwrap_or("");
    assert_eq!(body_part, "hello", "body wrong: {req}");
}
