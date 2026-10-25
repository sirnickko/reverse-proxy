# reverse-proxy

A stage-1 HTTP/1.x reverse proxy written in pure-std Rust (no external crates).

```
client ---> rproxy :8000 ---> your-server :4433
```

## Features (stage 1)

| Feature | Detail |
|---------|--------|
| **Transparent forwarding** | All methods, paths, query strings, request bodies |
| **Hop-by-hop stripping** | `Connection`, `Keep-Alive`, `Transfer-Encoding`, `Upgrade`, `Proxy-Connection` stripped from both request and response per RFC 7230 s6.1 |
| **X-Forwarded-For / Proto** | Appended / set on every forwarded request |
| **All body framings** | `Content-Length`, `Transfer-Encoding: chunked` (de-chunked), read-until-close |
| **Client keep-alive** | Proxy re-uses the client connection; always opens fresh upstream connection per request |
| **Error responses** | 502 Bad Gateway, 504 Gateway Timeout, 400, 413 |
| **Bounded thread pool** | Fixed worker count with bounded queue; overload returns 503 |

## Usage

```bash
# Point at your existing HTTPS server backend
cargo run --bin rproxy -- --listen 127.0.0.1:8000 --upstream 127.0.0.1:4433

# Custom worker count
cargo run --bin rproxy -- -l 0.0.0.0:8000 -u 127.0.0.1:4433 -w 16
```

| Flag | Short | Default | Description |
|------|-------|---------|-------------|
| `--listen` | `-l` | `127.0.0.1:8000` | Address the proxy binds to |
| `--upstream` | `-u` | `127.0.0.1:4433` | Backend to forward to |
| `--workers` | `-w` | `8` | Worker thread count |

## Running tests

```bash
cargo test
```

## Architecture

```
src/
  lib.rs       -- exports all modules for integration tests
  main.rs      -- CLI parsing, TcpListener accept loop, ThreadPool dispatch
  pool.rs      -- bounded SyncChannel-based thread pool
  proxy.rs     -- keep-alive connection loop + forward() per request
  request.rs   -- HTTP/1.x request parser (request-line, headers, body)
  response.rs  -- HTTP/1.x response parser, body reader, write_response()
  headers.rs   -- hop-by-hop filtering, X-Forwarded-* injection
  upstream.rs  -- TcpStream::connect with timeout
tests/
  integration.rs -- fake-backend integration tests
```

## Related project

**[HTTP server](https://github.com/sirnickko/HTTP-server)** -- the upstream server this proxy sits in front of.

## What is next (stage 2)

- Round-robin load balancing (`--upstream u1,u2,u3`)
- Least-connections routing
- Health checks -- background pinger; remove dead backends automatically
- In-memory response cache honouring ETag / Cache-Control (CDN concept)
- Layer-4 TCP forwarder to show the OSI model difference
