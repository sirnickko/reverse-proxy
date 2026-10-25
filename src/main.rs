use std::net::TcpListener;

use rproxy::{pool::ThreadPool, proxy};

fn main() {
    let args = Args::parse();

    let listener = TcpListener::bind(&args.listen).unwrap_or_else(|e| {
        eprintln!("Error: cannot bind to {}: {e}", args.listen);
        std::process::exit(1);
    });

    let pool = ThreadPool::new(args.workers);

    println!("=====================================");
    println!("     Rust Reverse Proxy  v0.1");
    println!("=====================================");
    println!("  Listen   http://{}", args.listen);
    println!("  Upstream {}", args.upstream);
    println!("  Workers  {}", args.workers);
    println!("  Ctrl+C to stop");
    println!("=====================================");
    println!();

    for stream in listener.incoming() {
        match stream {
            Ok(tcp) => {
                let upstream = args.upstream.clone();
                if !pool.execute(move || proxy::handle_connection(tcp, &upstream)) {
                    eprintln!("[proxy] worker queue full -- dropping connection");
                }
            }
            Err(e) => eprintln!("[proxy] accept error: {e}"),
        }
    }
}

// ---- CLI argument parsing ---------------------------------------------------

struct Args {
    listen:   String,
    upstream: String,
    workers:  usize,
}

impl Args {
    fn parse() -> Self {
        let raw: Vec<String> = std::env::args().collect();
        let mut a = Args {
            listen:   "127.0.0.1:8000".to_string(),
            upstream: "127.0.0.1:4433".to_string(),
            workers:  8,
        };
        let mut i = 1;
        while i < raw.len() {
            match raw[i].as_str() {
                "--listen"   | "-l" => { if let Some(v) = raw.get(i+1) { a.listen   = v.clone(); i += 1; } }
                "--upstream" | "-u" => { if let Some(v) = raw.get(i+1) { a.upstream = v.clone(); i += 1; } }
                "--workers"  | "-w" => { if let Some(v) = raw.get(i+1) { a.workers  = v.parse().unwrap_or(8); i += 1; } }
                arg if arg.starts_with("--listen=")   => { a.listen   = arg["--listen=".len()..].to_string(); }
                arg if arg.starts_with("--upstream=") => { a.upstream = arg["--upstream=".len()..].to_string(); }
                arg if arg.starts_with("--workers=")  => { a.workers  = arg["--workers=".len()..].parse().unwrap_or(8); }
                _ => {}
            }
            i += 1;
        }
        a
    }
}
