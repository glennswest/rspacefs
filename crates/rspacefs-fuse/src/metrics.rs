//! Tiny HTTP/1.0 server that exposes `Stats` over `/metrics` in Prometheus
//! text-exposition format, plus a `/healthz` liveness probe.
//!
//! Why std-only (no axum/hyper)?
//! - We're one connection per scrape, fired on a multi-second cadence.
//! - The daemon must stay lean — every container start spawns one of these
//!   per mount, so the per-process overhead is multiplied by hundreds.
//! - The handful of routes is fixed; we don't need a router.
//!
//! The handler reads from `Arc<Stats>` lock-free and writes back
//! `render_prom(snapshot, mountpoint)`. No allocator pressure beyond the
//! one String the renderer builds.
//!
//! ## Routes
//!
//! - `GET /metrics`  → Prometheus text. 200 OK, Content-Type:
//!   `text/plain; version=0.0.4`.
//! - `GET /healthz`  → 200 OK, body `ok`. Liveness — only fails if the
//!   thread is dead.
//! - Anything else   → 404.
//!
//! ## How OpenShift scrapes it
//!
//! A `ServiceMonitor` in the `openshift-user-workload-monitoring`
//! namespace points at a Service that fronts a per-node Endpoint of
//! `rspacefs-node-exporter` (separate binary, future work — aggregates
//! per-PID rspacefs-mount instances into one node-level /metrics). For
//! now, individual processes expose their own port and you point a
//! per-process target at each.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

use crate::stats::{render_prom, Stats};

pub fn spawn_metrics_server(
    addr: &str,
    stats: Arc<Stats>,
    mountpoint: String,
) -> std::io::Result<thread::JoinHandle<()>> {
    let listener = TcpListener::bind(addr)?;
    tracing::info!(addr = %addr, "metrics HTTP listening");

    let handle = thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                continue;
            };
            let stats = Arc::clone(&stats);
            let mp = mountpoint.clone();
            // One thread per connection. Scrape volume is one-per-15-seconds
            // typical; we can absorb the overhead without a thread pool.
            thread::spawn(move || {
                if let Err(e) = handle_conn(stream, &stats, &mp) {
                    tracing::debug!("metrics conn: {e}");
                }
            });
        }
    });
    Ok(handle)
}

fn handle_conn(mut stream: TcpStream, stats: &Stats, mountpoint: &str) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    // Drain headers until empty line. We don't read any of them.
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");

    let (status, ctype, body) = match (method, path) {
        ("GET", "/metrics") => {
            let snap = stats.snapshot();
            let text = render_prom(&snap, mountpoint);
            ("200 OK", "text/plain; version=0.0.4; charset=utf-8", text)
        }
        ("GET", "/healthz") => ("200 OK", "text/plain", "ok\n".to_string()),
        ("GET", _) => ("404 Not Found", "text/plain", "not found\n".to_string()),
        _ => (
            "405 Method Not Allowed",
            "text/plain",
            "only GET\n".to_string(),
        ),
    };

    write!(
        stream,
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        ctype,
        body.len()
    )?;
    stream.write_all(body.as_bytes())?;
    stream.flush()?;
    Ok(())
}
