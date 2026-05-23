//! Unix-socket control surface for a running `rspacefs-mount` daemon.
//!
//! When the binary is started with `--control-socket /path/to/sock`, a
//! background thread listens on that socket and accepts newline-delimited
//! JSON commands. Each line is one request; the daemon writes one line
//! back as the response.
//!
//! ## Protocol
//!
//! Request (one JSON object per line):
//! ```text
//! { "cmd": "ping" }
//! { "cmd": "status" }
//! { "cmd": "invalidate" }
//! { "cmd": "stats" }
//! { "cmd": "metrics-text" }
//! { "cmd": "info" }
//! { "cmd": "ops", "n": 32 }
//! { "cmd": "debug" }
//! ```
//!
//! Response (one JSON object per line):
//! ```text
//! { "ok": true, "data": { ... } }
//! { "ok": false, "error": "..." }
//! ```
//!
//! `metrics-text` puts the Prometheus exposition text inside `data` as a
//! single string; the CLI `rspacefs ctl ... metrics` strips the envelope
//! so the output is a clean Prometheus payload pipe-able into a scraper.
//!
//! ## Notifier integration (item D)
//!
//! The `invalidate` command uses `fuser::Notifier::inval_entry` to push
//! kernel cache invalidations: for every known top-level child name in the
//! merged tree, send `inval_entry(ROOT_INO, name)`. The kernel drops its
//! dentry cache for those entries; the next access re-enters the daemon.
//! This is the foundation of "live ops" — manifest rotation, layer hot-
//! swap, etc. will reuse this machinery.
//!
//! ## Future commands (v2)
//!
//! - `verify <layer_index>` — run full-tree verity rescan
//! - `snapshot <output>` — capture the upper into a new dir (hardlink/reflink)
//! - `reload-manifest <layer> <manifest> <tree>` — hot-swap a verity manifest
//! - `swap-lower <index> <path>` — hot-replace a lower layer
//!
//! All ride on the same protocol shape; just new `cmd` values.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use fuser::Notifier;
use serde::{Deserialize, Serialize};
use vfs::VfsPath;

use crate::stats::{render_prom, Stats};

const ROOT_INO: u64 = 1;

/// Read-mostly snapshot of the mount's configuration, shared between the
/// FUSE Filesystem and the control thread. Held in an `Arc` so cloning is
/// cheap.
#[derive(Clone)]
pub struct ControlState {
    pub mountpoint: PathBuf,
    pub upper: PathBuf,
    pub lowers: Vec<PathBuf>,
    pub verified_layers: Vec<bool>,
    pub mount_time: SystemTime,
    /// Root of the merged tree as a `VfsPath` — used by `invalidate` to
    /// enumerate top-level entries.
    pub root: VfsPath,
    /// Operational counters and gauges. Cloned from the FS adapter at
    /// startup so `stats` / `metrics-text` / `ops` reads don't lock the FS.
    pub stats: Arc<Stats>,
}

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
enum Request {
    /// Round-trip liveness probe.
    Ping,
    /// Mount configuration snapshot (paths, layers, uptime).
    Status,
    /// `FUSE_NOTIFY_INVAL_ENTRY` for every top-level merged entry.
    Invalidate,
    /// JSON snapshot of every counter and gauge.
    Stats,
    /// Prometheus text-format dump of the same counters. Pipe straight
    /// into a scrape target.
    MetricsText,
    /// Config + binary-version dump. Subset of `status` with `version`,
    /// pid, executable path, fuse-passthrough capability.
    Info,
    /// Recent FUSE op ring (most-recent first). `n` caps the count.
    #[serde(rename = "ops")]
    Ops {
        #[serde(default = "default_ops_limit")]
        n: usize,
    },
    /// Internal-state dump for debugging. Open-handle count, RSS bytes,
    /// last-op timestamp, layer count.
    Debug,
}

fn default_ops_limit() -> usize {
    32
}

#[derive(Serialize)]
struct Response<T: Serialize> {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct StatusData {
    mountpoint: String,
    upper: String,
    lowers: Vec<LowerInfo>,
    uptime_secs: u64,
    version: &'static str,
    fuse_passthrough: bool,
}

#[derive(Serialize)]
struct LowerInfo {
    path: String,
    verified: bool,
}

#[derive(Serialize)]
struct InvalidateData {
    entries_invalidated: usize,
    errors: usize,
}

#[derive(Serialize)]
struct InfoData {
    mountpoint: String,
    upper: String,
    lower_count: usize,
    verified_lower_count: usize,
    uptime_secs: u64,
    version: &'static str,
    pid: u32,
    exe: String,
    fuse_passthrough: bool,
}

#[derive(Serialize)]
struct DebugData {
    pid: u32,
    open_handles: i64,
    last_op_unix_ms: u64,
    uptime_secs: u64,
    rss_bytes: Option<u64>,
    mountpoint: String,
    layer_count: usize,
}

/// Spawn the control-socket listener thread. Returns the listener so the
/// caller can choose to keep it alive or drop it (drop closes the socket).
pub fn spawn_control_thread(
    socket_path: PathBuf,
    state: Arc<ControlState>,
    notifier: Notifier,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    // Best-effort: remove a stale socket from a previous crash.
    let _ = std::fs::remove_file(&socket_path);
    if let Some(parent) = socket_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let listener = UnixListener::bind(&socket_path)?;
    tracing::info!("control socket listening at {}", socket_path.display());

    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    let state = Arc::clone(&state);
                    let notifier = notifier.clone();
                    std::thread::spawn(move || {
                        if let Err(e) = handle_client(s, state, notifier) {
                            tracing::warn!("control client: {}", e);
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!("control accept error: {}", e);
                    break;
                }
            }
        }
    });
    Ok(handle)
}

fn handle_client(
    stream: UnixStream,
    state: Arc<ControlState>,
    notifier: Notifier,
) -> std::io::Result<()> {
    let reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(req) => dispatch(req, &state, &notifier),
            Err(e) => serde_json::to_string(&Response::<()> {
                ok: false,
                data: None,
                error: Some(format!("bad request: {}", e)),
            })
            .unwrap(),
        };

        writer.write_all(resp.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()?;
    }
    Ok(())
}

fn dispatch(req: Request, state: &ControlState, notifier: &Notifier) -> String {
    match req {
        Request::Ping => to_ok(&"pong"),
        Request::Status => to_ok(&status_payload(state)),
        Request::Invalidate => match do_invalidate(state, notifier) {
            Ok(data) => to_ok(&data),
            Err(e) => to_err(&e.to_string()),
        },
        Request::Stats => to_ok(&state.stats.snapshot()),
        Request::MetricsText => {
            // Wrap the Prometheus text dump in the same Response envelope
            // so clients can pipe `.data` straight to a scrape target.
            let snap = state.stats.snapshot();
            let mp = state.mountpoint.display().to_string();
            to_ok(&render_prom(&snap, &mp))
        }
        Request::Info => to_ok(&info_payload(state)),
        Request::Ops { n } => to_ok(&state.stats.recent(n)),
        Request::Debug => to_ok(&debug_payload(state)),
    }
}

fn info_payload(state: &ControlState) -> InfoData {
    let exe = std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    InfoData {
        mountpoint: state.mountpoint.display().to_string(),
        upper: state.upper.display().to_string(),
        lower_count: state.lowers.len(),
        // verified_layers[0] is the upper — skip it for the lower count.
        verified_lower_count: state.verified_layers.iter().skip(1).filter(|v| **v).count(),
        uptime_secs: state.mount_time.elapsed().map(|d| d.as_secs()).unwrap_or(0),
        version: env!("CARGO_PKG_VERSION"),
        pid: std::process::id(),
        exe,
        fuse_passthrough: true,
    }
}

fn debug_payload(state: &ControlState) -> DebugData {
    DebugData {
        pid: std::process::id(),
        open_handles: state
            .stats
            .open_handles
            .load(std::sync::atomic::Ordering::Relaxed),
        last_op_unix_ms: state
            .stats
            .last_op_unix_ms
            .load(std::sync::atomic::Ordering::Relaxed),
        uptime_secs: state.mount_time.elapsed().map(|d| d.as_secs()).unwrap_or(0),
        rss_bytes: read_rss_bytes(),
        mountpoint: state.mountpoint.display().to_string(),
        // +1 because index 0 of verified_layers is the upper.
        layer_count: state.verified_layers.len(),
    }
}

/// Best-effort read of this process's RSS in bytes from /proc/self/status.
/// Linux-only side-channel — fine since rspacefs-mount only runs on Linux.
fn read_rss_bytes() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            // Format: `VmRSS:    1234 kB`
            let n: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(n * 1024);
        }
    }
    None
}

fn status_payload(state: &ControlState) -> StatusData {
    let lowers = state
        .lowers
        .iter()
        .zip(state.verified_layers.iter().skip(1)) // index 0 is the upper
        .map(|(p, v)| LowerInfo {
            path: p.display().to_string(),
            verified: *v,
        })
        .collect();
    StatusData {
        mountpoint: state.mountpoint.display().to_string(),
        upper: state.upper.display().to_string(),
        lowers,
        uptime_secs: state.mount_time.elapsed().map(|d| d.as_secs()).unwrap_or(0),
        version: env!("CARGO_PKG_VERSION"),
        fuse_passthrough: true,
    }
}

/// Invalidate the kernel's dentry cache for every top-level entry in the
/// merged tree by sending `FUSE_NOTIFY_INVAL_ENTRY` for each name. Used to
/// force the kernel to re-enter the daemon after upper / lower state
/// changes (snapshot promotion, manifest rotation, ...).
fn do_invalidate(state: &ControlState, notifier: &Notifier) -> std::io::Result<InvalidateData> {
    use std::collections::BTreeSet;

    // Collect top-level names visible through the merged tree. BTreeSet
    // dedups + orders for stable output.
    let mut names: BTreeSet<String> = BTreeSet::new();
    if let Ok(iter) = state.root.read_dir() {
        for entry in iter {
            names.insert(entry.filename());
        }
    }

    let mut errors = 0;
    for name in &names {
        let os_name = std::ffi::OsString::from(name);
        if let Err(e) = notifier.inval_entry(ROOT_INO, &os_name) {
            tracing::warn!("inval_entry failed for {}: {}", name, e);
            errors += 1;
        }
    }
    Ok(InvalidateData {
        entries_invalidated: names.len().saturating_sub(errors),
        errors,
    })
}

fn to_ok<T: Serialize>(v: &T) -> String {
    serde_json::to_string(&Response {
        ok: true,
        data: Some(v),
        error: None,
    })
    .unwrap_or_else(|_| r#"{"ok":false,"error":"serialize failed"}"#.to_string())
}

fn to_err(msg: &str) -> String {
    serde_json::to_string(&Response::<()> {
        ok: false,
        data: None,
        error: Some(msg.to_string()),
    })
    .unwrap_or_else(|_| r#"{"ok":false,"error":"serialize failed"}"#.to_string())
}
