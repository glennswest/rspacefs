//! Operational telemetry for `rspacefs-mount`.
//!
//! Every FUSE op increments a counter; reads/writes also accumulate bytes.
//! Held in an `Arc<Stats>` so the FS adapter and the control-socket thread
//! share the same atomics. All counters use `Relaxed` ordering — we don't
//! care about cross-thread ordering, only that the increments don't tear.
//!
//! Three views are exposed via the control socket:
//!
//! - `stats` — JSON snapshot of every counter and gauge.
//! - `metrics-text` — Prometheus text-format dump of the same data; `curl`
//!   the control socket and pipe straight into a scraper.
//! - `ops` — short ring of the most recent ops with timestamps, durations,
//!   and result codes — what's been happening in the last few seconds.
//! - `debug` — internal state dump: open handles, backing-cache size, pid,
//!   rss bytes, mount paths.
//!
//! Each rspacefs-mount process is one-mount-per-process under
//! `mount_program`, so a node-level scrape collects from every PID. A
//! future `rspacefs-node-exporter` will aggregate; for now the per-socket
//! data is the primitive.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Latency-histogram bucket upper bounds in microseconds.
///
/// 16 finite buckets plus an implicit `+Inf` overflow tracked via `count -
/// sum(buckets)`. Boundaries chosen so the steady-state hot path
/// (passthrough open: ~10 µs, cached lookup: ~5 µs, copy-up: ~ms-range)
/// each lands in a different bucket — gives meaningful p50 / p99
/// resolution without per-op tail spew.
pub const LAT_BUCKETS_US: &[u64] = &[
    50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 25_000, 50_000, 100_000, 250_000, 500_000,
    1_000_000, 2_500_000, 5_000_000,
];
pub const LAT_N: usize = 16;

/// Top-level counters and gauges. All atomic, all `Relaxed`.
pub struct Stats {
    pub started_at: SystemTime,
    pub ops: OpCounters,
    pub hists: OpHistograms,
    pub bytes_read: AtomicU64,
    pub bytes_written: AtomicU64,
    /// Open-file count broken down by handle kind.
    pub passthrough_opens: AtomicU64,
    pub streaming_opens: AtomicU64,
    pub buffered_opens: AtomicU64,
    /// Copy-up: how many times a write triggered upper-layer materialisation,
    /// total bytes copied, and how many took the FICLONE reflink fast path.
    pub copy_ups: AtomicU64,
    pub copy_up_bytes: AtomicU64,
    pub reflinks_ok: AtomicU64,
    pub reflinks_fallback: AtomicU64,
    /// Per-inode BackingId cache. Hits = reused an existing kernel backing
    /// fd instead of issuing a fresh `BACKING_OPEN` ioctl.
    pub backing_cache_hits: AtomicU64,
    pub backing_cache_misses: AtomicU64,
    /// Errors returned to the kernel, by category.
    pub errors_io: AtomicU64,
    pub errors_enoent: AtomicU64,
    pub errors_other: AtomicU64,
    /// Hardening (#24): faults the daemon caught + recovered from
    /// instead of crashing. Each class corresponds to one row in
    /// `docs/faults.md`.
    pub faults_panic: AtomicU64,
    pub faults_session_retry: AtomicU64,
    pub faults_lock_poisoned: AtomicU64,
    pub faults_unexpected_errno: AtomicU64,
    /// Last op's millis-since-epoch. Useful as an "is this daemon alive
    /// and serving traffic?" liveness signal.
    pub last_op_unix_ms: AtomicU64,
    /// Current number of file handles in the open table (gauge).
    pub open_handles: AtomicI64,
    /// Ring buffer of recent ops for the `ops` view.
    recent: Mutex<RecentRing>,
}

/// Latency histogram for one FUSE op. 16 atomic counters (one per bucket
/// in `LAT_BUCKETS_US`) plus `count` (total observations including
/// over-the-last-bucket) and `sum_us` (sum of observed latencies). All
/// `Relaxed` — Prometheus quantile semantics tolerate per-observation
/// reorderings; we only need the histogram to converge.
pub struct LatHist {
    pub buckets: [AtomicU64; LAT_N],
    pub count: AtomicU64,
    pub sum_us: AtomicU64,
    pub in_flight: AtomicI64,
}

impl LatHist {
    fn new() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            count: 0.into(),
            sum_us: 0.into(),
            in_flight: 0.into(),
        }
    }

    /// Record one observation. `us` is the latency in microseconds.
    pub fn observe(&self, us: u64) {
        for (i, &b) in LAT_BUCKETS_US.iter().enumerate() {
            if us <= b {
                self.buckets[i].fetch_add(1, Ordering::Relaxed);
                break;
            }
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(us, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> LatHistSnapshot {
        LatHistSnapshot {
            buckets: std::array::from_fn(|i| self.buckets[i].load(Ordering::Relaxed)),
            count: self.count.load(Ordering::Relaxed),
            sum_us: self.sum_us.load(Ordering::Relaxed),
            in_flight: self.in_flight.load(Ordering::Relaxed),
        }
    }
}

#[derive(Serialize, Clone)]
pub struct LatHistSnapshot {
    pub buckets: [u64; LAT_N],
    pub count: u64,
    pub sum_us: u64,
    pub in_flight: i64,
}

/// Per-op latency histograms, one per FUSE op. Same field layout as
/// `OpCounters` so a future macro could collapse both.
pub struct OpHistograms {
    pub lookup: LatHist,
    pub getattr: LatHist,
    pub setattr: LatHist,
    pub readdir: LatHist,
    pub mkdir: LatHist,
    pub rmdir: LatHist,
    pub open: LatHist,
    pub read: LatHist,
    pub write: LatHist,
    pub release: LatHist,
    pub create: LatHist,
    pub unlink: LatHist,
    pub rename: LatHist,
    pub readlink: LatHist,
    pub symlink: LatHist,
    pub getxattr: LatHist,
    pub listxattr: LatHist,
    pub setxattr: LatHist,
    pub removexattr: LatHist,
    pub fsync: LatHist,
    pub flush: LatHist,
    pub statfs: LatHist,
    pub poll: LatHist,
}

impl OpHistograms {
    fn new() -> Self {
        Self {
            lookup: LatHist::new(),
            getattr: LatHist::new(),
            setattr: LatHist::new(),
            readdir: LatHist::new(),
            mkdir: LatHist::new(),
            rmdir: LatHist::new(),
            open: LatHist::new(),
            read: LatHist::new(),
            write: LatHist::new(),
            release: LatHist::new(),
            create: LatHist::new(),
            unlink: LatHist::new(),
            rename: LatHist::new(),
            readlink: LatHist::new(),
            symlink: LatHist::new(),
            getxattr: LatHist::new(),
            listxattr: LatHist::new(),
            setxattr: LatHist::new(),
            removexattr: LatHist::new(),
            fsync: LatHist::new(),
            flush: LatHist::new(),
            statfs: LatHist::new(),
            poll: LatHist::new(),
        }
    }
}

/// Per-op-kind counter set. Numbers match the FUSE op names exactly so
/// metrics labels are obvious.
pub struct OpCounters {
    pub lookup: AtomicU64,
    pub getattr: AtomicU64,
    pub setattr: AtomicU64,
    pub readdir: AtomicU64,
    pub mkdir: AtomicU64,
    pub rmdir: AtomicU64,
    pub open: AtomicU64,
    pub read: AtomicU64,
    pub write: AtomicU64,
    pub release: AtomicU64,
    pub create: AtomicU64,
    pub unlink: AtomicU64,
    pub rename: AtomicU64,
    pub readlink: AtomicU64,
    pub symlink: AtomicU64,
    pub getxattr: AtomicU64,
    pub listxattr: AtomicU64,
    pub setxattr: AtomicU64,
    pub removexattr: AtomicU64,
    pub fsync: AtomicU64,
    pub flush: AtomicU64,
    pub statfs: AtomicU64,
    pub poll: AtomicU64,
}

impl OpCounters {
    fn new() -> Self {
        Self {
            lookup: 0.into(),
            getattr: 0.into(),
            setattr: 0.into(),
            readdir: 0.into(),
            mkdir: 0.into(),
            rmdir: 0.into(),
            open: 0.into(),
            read: 0.into(),
            write: 0.into(),
            release: 0.into(),
            create: 0.into(),
            unlink: 0.into(),
            rename: 0.into(),
            readlink: 0.into(),
            symlink: 0.into(),
            getxattr: 0.into(),
            listxattr: 0.into(),
            setxattr: 0.into(),
            removexattr: 0.into(),
            fsync: 0.into(),
            flush: 0.into(),
            statfs: 0.into(),
            poll: 0.into(),
        }
    }
}

impl Stats {
    pub fn new() -> Self {
        Self {
            started_at: SystemTime::now(),
            ops: OpCounters::new(),
            hists: OpHistograms::new(),
            bytes_read: 0.into(),
            bytes_written: 0.into(),
            passthrough_opens: 0.into(),
            streaming_opens: 0.into(),
            buffered_opens: 0.into(),
            copy_ups: 0.into(),
            copy_up_bytes: 0.into(),
            reflinks_ok: 0.into(),
            reflinks_fallback: 0.into(),
            backing_cache_hits: 0.into(),
            backing_cache_misses: 0.into(),
            errors_io: 0.into(),
            errors_enoent: 0.into(),
            errors_other: 0.into(),
            faults_panic: 0.into(),
            faults_session_retry: 0.into(),
            faults_lock_poisoned: 0.into(),
            faults_unexpected_errno: 0.into(),
            last_op_unix_ms: 0.into(),
            open_handles: 0.into(),
            recent: Mutex::new(RecentRing::new(128)),
        }
    }

    /// Bump a counter, mark the last-op timestamp, and push an entry into
    /// the recent-ops ring. Called at the top of every FUSE op.
    pub fn record(&self, op: Op, ino: u64, bytes: u64, rc: i32) {
        let ctr = self.counter_for(op);
        ctr.fetch_add(1, Ordering::Relaxed);
        self.last_op_unix_ms.store(now_ms(), Ordering::Relaxed);
        match op {
            Op::Read => {
                self.bytes_read.fetch_add(bytes, Ordering::Relaxed);
            }
            Op::Write => {
                self.bytes_written.fetch_add(bytes, Ordering::Relaxed);
            }
            _ => {}
        }
        if rc != 0 {
            match rc {
                libc::ENOENT => {
                    self.errors_enoent.fetch_add(1, Ordering::Relaxed);
                }
                libc::EIO => {
                    self.errors_io.fetch_add(1, Ordering::Relaxed);
                }
                _ => {
                    self.errors_other.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        // Lock contention here is fine — recent ring is for human-debug
        // and is only sampled at scrape time.
        if let Ok(mut r) = self.recent.lock() {
            r.push(RecentOp {
                ts_ms: now_ms(),
                op,
                ino,
                bytes,
                rc,
            });
        }
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            uptime_secs: self.started_at.elapsed().map(|d| d.as_secs()).unwrap_or(0),
            hists: HistSnapshot {
                lookup: self.hists.lookup.snapshot(),
                getattr: self.hists.getattr.snapshot(),
                setattr: self.hists.setattr.snapshot(),
                readdir: self.hists.readdir.snapshot(),
                mkdir: self.hists.mkdir.snapshot(),
                rmdir: self.hists.rmdir.snapshot(),
                open: self.hists.open.snapshot(),
                read: self.hists.read.snapshot(),
                write: self.hists.write.snapshot(),
                release: self.hists.release.snapshot(),
                create: self.hists.create.snapshot(),
                unlink: self.hists.unlink.snapshot(),
                rename: self.hists.rename.snapshot(),
                readlink: self.hists.readlink.snapshot(),
                symlink: self.hists.symlink.snapshot(),
                getxattr: self.hists.getxattr.snapshot(),
                listxattr: self.hists.listxattr.snapshot(),
                setxattr: self.hists.setxattr.snapshot(),
                removexattr: self.hists.removexattr.snapshot(),
                fsync: self.hists.fsync.snapshot(),
                flush: self.hists.flush.snapshot(),
                statfs: self.hists.statfs.snapshot(),
                poll: self.hists.poll.snapshot(),
            },
            ops: OpSnapshot {
                lookup: self.ops.lookup.load(Ordering::Relaxed),
                getattr: self.ops.getattr.load(Ordering::Relaxed),
                setattr: self.ops.setattr.load(Ordering::Relaxed),
                readdir: self.ops.readdir.load(Ordering::Relaxed),
                mkdir: self.ops.mkdir.load(Ordering::Relaxed),
                rmdir: self.ops.rmdir.load(Ordering::Relaxed),
                open: self.ops.open.load(Ordering::Relaxed),
                read: self.ops.read.load(Ordering::Relaxed),
                write: self.ops.write.load(Ordering::Relaxed),
                release: self.ops.release.load(Ordering::Relaxed),
                create: self.ops.create.load(Ordering::Relaxed),
                unlink: self.ops.unlink.load(Ordering::Relaxed),
                rename: self.ops.rename.load(Ordering::Relaxed),
                readlink: self.ops.readlink.load(Ordering::Relaxed),
                symlink: self.ops.symlink.load(Ordering::Relaxed),
                getxattr: self.ops.getxattr.load(Ordering::Relaxed),
                listxattr: self.ops.listxattr.load(Ordering::Relaxed),
                setxattr: self.ops.setxattr.load(Ordering::Relaxed),
                removexattr: self.ops.removexattr.load(Ordering::Relaxed),
                fsync: self.ops.fsync.load(Ordering::Relaxed),
                flush: self.ops.flush.load(Ordering::Relaxed),
                statfs: self.ops.statfs.load(Ordering::Relaxed),
                poll: self.ops.poll.load(Ordering::Relaxed),
            },
            bytes_read: self.bytes_read.load(Ordering::Relaxed),
            bytes_written: self.bytes_written.load(Ordering::Relaxed),
            passthrough_opens: self.passthrough_opens.load(Ordering::Relaxed),
            streaming_opens: self.streaming_opens.load(Ordering::Relaxed),
            buffered_opens: self.buffered_opens.load(Ordering::Relaxed),
            copy_ups: self.copy_ups.load(Ordering::Relaxed),
            copy_up_bytes: self.copy_up_bytes.load(Ordering::Relaxed),
            reflinks_ok: self.reflinks_ok.load(Ordering::Relaxed),
            reflinks_fallback: self.reflinks_fallback.load(Ordering::Relaxed),
            backing_cache_hits: self.backing_cache_hits.load(Ordering::Relaxed),
            backing_cache_misses: self.backing_cache_misses.load(Ordering::Relaxed),
            errors_io: self.errors_io.load(Ordering::Relaxed),
            errors_enoent: self.errors_enoent.load(Ordering::Relaxed),
            errors_other: self.errors_other.load(Ordering::Relaxed),
            last_op_unix_ms: self.last_op_unix_ms.load(Ordering::Relaxed),
            open_handles: self.open_handles.load(Ordering::Relaxed),
        }
    }

    pub fn recent(&self, max: usize) -> Vec<RecentOp> {
        self.recent
            .lock()
            .map(|r| r.collect(max))
            .unwrap_or_default()
    }

    /// Start timing one op. Returned guard increments in-flight on
    /// construction and, on drop, records the elapsed latency into the
    /// per-op histogram + decrements in-flight.
    pub fn scope(&self, op: Op) -> OpScope<'_> {
        self.hist_for(op).in_flight.fetch_add(1, Ordering::Relaxed);
        OpScope {
            stats: self,
            op,
            started: Instant::now(),
        }
    }

    pub fn hist_for(&self, op: Op) -> &LatHist {
        match op {
            Op::Lookup => &self.hists.lookup,
            Op::Getattr => &self.hists.getattr,
            Op::Setattr => &self.hists.setattr,
            Op::Readdir => &self.hists.readdir,
            Op::Mkdir => &self.hists.mkdir,
            Op::Rmdir => &self.hists.rmdir,
            Op::Open => &self.hists.open,
            Op::Read => &self.hists.read,
            Op::Write => &self.hists.write,
            Op::Release => &self.hists.release,
            Op::Create => &self.hists.create,
            Op::Unlink => &self.hists.unlink,
            Op::Rename => &self.hists.rename,
            Op::Readlink => &self.hists.readlink,
            Op::Symlink => &self.hists.symlink,
            Op::Getxattr => &self.hists.getxattr,
            Op::Listxattr => &self.hists.listxattr,
            Op::Setxattr => &self.hists.setxattr,
            Op::Removexattr => &self.hists.removexattr,
            Op::Fsync => &self.hists.fsync,
            Op::Flush => &self.hists.flush,
            Op::Statfs => &self.hists.statfs,
            Op::Poll => &self.hists.poll,
        }
    }

    fn counter_for(&self, op: Op) -> &AtomicU64 {
        match op {
            Op::Lookup => &self.ops.lookup,
            Op::Getattr => &self.ops.getattr,
            Op::Setattr => &self.ops.setattr,
            Op::Readdir => &self.ops.readdir,
            Op::Mkdir => &self.ops.mkdir,
            Op::Rmdir => &self.ops.rmdir,
            Op::Open => &self.ops.open,
            Op::Read => &self.ops.read,
            Op::Write => &self.ops.write,
            Op::Release => &self.ops.release,
            Op::Create => &self.ops.create,
            Op::Unlink => &self.ops.unlink,
            Op::Rename => &self.ops.rename,
            Op::Readlink => &self.ops.readlink,
            Op::Symlink => &self.ops.symlink,
            Op::Getxattr => &self.ops.getxattr,
            Op::Listxattr => &self.ops.listxattr,
            Op::Setxattr => &self.ops.setxattr,
            Op::Removexattr => &self.ops.removexattr,
            Op::Fsync => &self.ops.fsync,
            Op::Flush => &self.ops.flush,
            Op::Statfs => &self.ops.statfs,
            Op::Poll => &self.ops.poll,
        }
    }
}

/// RAII guard returned by `Stats::scope`. Increments in-flight on
/// construction and on drop records the elapsed latency into the per-op
/// histogram + decrements in-flight. One line at the top of each FUSE op
/// method is all that's needed.
pub struct OpScope<'a> {
    stats: &'a Stats,
    op: Op,
    started: Instant,
}

impl<'a> Drop for OpScope<'a> {
    fn drop(&mut self) {
        let elapsed_us = self.started.elapsed().as_micros() as u64;
        let h = self.stats.hist_for(self.op);
        h.observe(elapsed_us);
        h.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Copy, Clone, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    Lookup,
    Getattr,
    Setattr,
    Readdir,
    Mkdir,
    Rmdir,
    Open,
    Read,
    Write,
    Release,
    Create,
    Unlink,
    Rename,
    Readlink,
    Symlink,
    Getxattr,
    Listxattr,
    Setxattr,
    Removexattr,
    Fsync,
    Flush,
    Statfs,
    Poll,
}

#[derive(Serialize, Clone, Copy)]
pub struct RecentOp {
    pub ts_ms: u64,
    pub op: Op,
    pub ino: u64,
    pub bytes: u64,
    pub rc: i32,
}

struct RecentRing {
    buf: Vec<RecentOp>,
    head: usize,
    len: usize,
    cap: usize,
}

impl RecentRing {
    fn new(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
            head: 0,
            len: 0,
            cap,
        }
    }

    fn push(&mut self, op: RecentOp) {
        if self.buf.len() < self.cap {
            self.buf.push(op);
            self.len = self.buf.len();
        } else {
            self.buf[self.head] = op;
            self.head = (self.head + 1) % self.cap;
        }
    }

    /// Return up to `max` most-recent ops, newest first.
    fn collect(&self, max: usize) -> Vec<RecentOp> {
        let take = max.min(self.len);
        let mut out = Vec::with_capacity(take);
        for i in 0..take {
            // Walk backwards from the most recent write.
            let idx = if self.buf.len() < self.cap {
                self.len.saturating_sub(1 + i)
            } else {
                (self.head + self.cap - 1 - i) % self.cap
            };
            out.push(self.buf[idx]);
        }
        out
    }
}

#[derive(Serialize)]
pub struct HistSnapshot {
    pub lookup: LatHistSnapshot,
    pub getattr: LatHistSnapshot,
    pub setattr: LatHistSnapshot,
    pub readdir: LatHistSnapshot,
    pub mkdir: LatHistSnapshot,
    pub rmdir: LatHistSnapshot,
    pub open: LatHistSnapshot,
    pub read: LatHistSnapshot,
    pub write: LatHistSnapshot,
    pub release: LatHistSnapshot,
    pub create: LatHistSnapshot,
    pub unlink: LatHistSnapshot,
    pub rename: LatHistSnapshot,
    pub readlink: LatHistSnapshot,
    pub symlink: LatHistSnapshot,
    pub getxattr: LatHistSnapshot,
    pub listxattr: LatHistSnapshot,
    pub setxattr: LatHistSnapshot,
    pub removexattr: LatHistSnapshot,
    pub fsync: LatHistSnapshot,
    pub flush: LatHistSnapshot,
    pub statfs: LatHistSnapshot,
    pub poll: LatHistSnapshot,
}

#[derive(Serialize)]
pub struct StatsSnapshot {
    pub uptime_secs: u64,
    pub hists: HistSnapshot,
    pub ops: OpSnapshot,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub passthrough_opens: u64,
    pub streaming_opens: u64,
    pub buffered_opens: u64,
    pub copy_ups: u64,
    pub copy_up_bytes: u64,
    pub reflinks_ok: u64,
    pub reflinks_fallback: u64,
    pub backing_cache_hits: u64,
    pub backing_cache_misses: u64,
    pub errors_io: u64,
    pub errors_enoent: u64,
    pub errors_other: u64,
    pub last_op_unix_ms: u64,
    pub open_handles: i64,
}

#[derive(Serialize)]
pub struct OpSnapshot {
    pub lookup: u64,
    pub getattr: u64,
    pub setattr: u64,
    pub readdir: u64,
    pub mkdir: u64,
    pub rmdir: u64,
    pub open: u64,
    pub read: u64,
    pub write: u64,
    pub release: u64,
    pub create: u64,
    pub unlink: u64,
    pub rename: u64,
    pub readlink: u64,
    pub symlink: u64,
    pub getxattr: u64,
    pub listxattr: u64,
    pub setxattr: u64,
    pub removexattr: u64,
    pub fsync: u64,
    pub flush: u64,
    pub statfs: u64,
    pub poll: u64,
}

/// Render a snapshot as Prometheus text. One metric per counter; one HELP
/// + TYPE line per family.
pub fn render_prom(snap: &StatsSnapshot, mountpoint: &str) -> String {
    let mut out = String::with_capacity(4096);
    let m = mountpoint.replace('"', "\\\"");

    macro_rules! counter {
        ($name:literal, $val:expr, $help:literal) => {{
            out.push_str(&format!("# HELP {} {}\n", $name, $help));
            out.push_str(&format!("# TYPE {} counter\n", $name));
            out.push_str(&format!("{}{{mount=\"{}\"}} {}\n", $name, m, $val));
        }};
    }
    macro_rules! gauge {
        ($name:literal, $val:expr, $help:literal) => {{
            out.push_str(&format!("# HELP {} {}\n", $name, $help));
            out.push_str(&format!("# TYPE {} gauge\n", $name));
            out.push_str(&format!("{}{{mount=\"{}\"}} {}\n", $name, m, $val));
        }};
    }
    macro_rules! op_ctr {
        ($name:literal, $val:expr) => {{
            out.push_str(&format!(
                "rspacefs_ops_total{{mount=\"{}\",op=\"{}\"}} {}\n",
                m, $name, $val
            ));
        }};
    }

    gauge!(
        "rspacefs_uptime_seconds",
        snap.uptime_secs,
        "seconds since rspacefs-mount started"
    );
    out.push_str("# HELP rspacefs_ops_total FUSE op invocations by op name\n");
    out.push_str("# TYPE rspacefs_ops_total counter\n");
    op_ctr!("lookup", snap.ops.lookup);
    op_ctr!("getattr", snap.ops.getattr);
    op_ctr!("setattr", snap.ops.setattr);
    op_ctr!("readdir", snap.ops.readdir);
    op_ctr!("mkdir", snap.ops.mkdir);
    op_ctr!("rmdir", snap.ops.rmdir);
    op_ctr!("open", snap.ops.open);
    op_ctr!("read", snap.ops.read);
    op_ctr!("write", snap.ops.write);
    op_ctr!("release", snap.ops.release);
    op_ctr!("create", snap.ops.create);
    op_ctr!("unlink", snap.ops.unlink);
    op_ctr!("rename", snap.ops.rename);
    op_ctr!("readlink", snap.ops.readlink);
    op_ctr!("symlink", snap.ops.symlink);
    op_ctr!("getxattr", snap.ops.getxattr);
    op_ctr!("listxattr", snap.ops.listxattr);
    op_ctr!("setxattr", snap.ops.setxattr);
    op_ctr!("removexattr", snap.ops.removexattr);
    op_ctr!("fsync", snap.ops.fsync);
    op_ctr!("flush", snap.ops.flush);
    op_ctr!("statfs", snap.ops.statfs);
    op_ctr!("poll", snap.ops.poll);

    counter!(
        "rspacefs_bytes_read_total",
        snap.bytes_read,
        "bytes returned to clients via read()"
    );
    counter!(
        "rspacefs_bytes_written_total",
        snap.bytes_written,
        "bytes accepted from clients via write()"
    );
    counter!(
        "rspacefs_passthrough_opens_total",
        snap.passthrough_opens,
        "opens served via FUSE_PASSTHROUGH (kernel-direct)"
    );
    counter!(
        "rspacefs_streaming_opens_total",
        snap.streaming_opens,
        "opens served via daemon streaming (verified or fallback)"
    );
    counter!(
        "rspacefs_buffered_opens_total",
        snap.buffered_opens,
        "opens served via in-memory read-modify-write buffer (writable opens)"
    );
    counter!(
        "rspacefs_copy_ups_total",
        snap.copy_ups,
        "copy-ups from a lower layer into upper"
    );
    counter!(
        "rspacefs_copy_up_bytes_total",
        snap.copy_up_bytes,
        "bytes copied during copy-up"
    );
    counter!(
        "rspacefs_reflinks_ok_total",
        snap.reflinks_ok,
        "copy-ups that took the FICLONE reflink fast path"
    );
    counter!(
        "rspacefs_reflinks_fallback_total",
        snap.reflinks_fallback,
        "copy-ups that fell back to a byte copy"
    );
    counter!(
        "rspacefs_backing_cache_hits_total",
        snap.backing_cache_hits,
        "BackingId cache reuse"
    );
    counter!(
        "rspacefs_backing_cache_misses_total",
        snap.backing_cache_misses,
        "BackingId cache miss (BACKING_OPEN ioctl issued)"
    );
    counter!(
        "rspacefs_errors_total{kind=\"io\"}",
        snap.errors_io,
        "EIO returned to client"
    );
    counter!(
        "rspacefs_errors_total{kind=\"enoent\"}",
        snap.errors_enoent,
        "ENOENT returned to client"
    );
    counter!(
        "rspacefs_errors_total{kind=\"other\"}",
        snap.errors_other,
        "other errno returned to client"
    );
    gauge!(
        "rspacefs_open_handles",
        snap.open_handles,
        "current file handles in the open table"
    );
    gauge!(
        "rspacefs_last_op_unix_ms",
        snap.last_op_unix_ms,
        "epoch-ms timestamp of the last op (liveness)"
    );

    // ── Latency histograms + in-flight gauges ─────────────────────────────
    out.push_str("# HELP rspacefs_op_latency_microseconds Per-op latency distribution\n");
    out.push_str("# TYPE rspacefs_op_latency_microseconds histogram\n");
    out.push_str("# HELP rspacefs_op_in_flight Currently-executing op count by op\n");
    out.push_str("# TYPE rspacefs_op_in_flight gauge\n");

    let hist_entries: [(&str, &LatHistSnapshot); 23] = [
        ("lookup", &snap.hists.lookup),
        ("getattr", &snap.hists.getattr),
        ("setattr", &snap.hists.setattr),
        ("readdir", &snap.hists.readdir),
        ("mkdir", &snap.hists.mkdir),
        ("rmdir", &snap.hists.rmdir),
        ("open", &snap.hists.open),
        ("read", &snap.hists.read),
        ("write", &snap.hists.write),
        ("release", &snap.hists.release),
        ("create", &snap.hists.create),
        ("unlink", &snap.hists.unlink),
        ("rename", &snap.hists.rename),
        ("readlink", &snap.hists.readlink),
        ("symlink", &snap.hists.symlink),
        ("getxattr", &snap.hists.getxattr),
        ("listxattr", &snap.hists.listxattr),
        ("setxattr", &snap.hists.setxattr),
        ("removexattr", &snap.hists.removexattr),
        ("fsync", &snap.hists.fsync),
        ("flush", &snap.hists.flush),
        ("statfs", &snap.hists.statfs),
        ("poll", &snap.hists.poll),
    ];
    for (op_name, h) in hist_entries {
        // Prometheus histograms are cumulative — each bucket reports
        // count <= le. We track per-bucket non-cumulative; sum here.
        let mut cumulative: u64 = 0;
        for (i, &bound) in LAT_BUCKETS_US.iter().enumerate() {
            cumulative += h.buckets[i];
            out.push_str(&format!(
                "rspacefs_op_latency_microseconds_bucket{{mount=\"{}\",op=\"{}\",le=\"{}\"}} {}\n",
                m, op_name, bound, cumulative
            ));
        }
        out.push_str(&format!(
            "rspacefs_op_latency_microseconds_bucket{{mount=\"{}\",op=\"{}\",le=\"+Inf\"}} {}\n",
            m, op_name, h.count
        ));
        out.push_str(&format!(
            "rspacefs_op_latency_microseconds_count{{mount=\"{}\",op=\"{}\"}} {}\n",
            m, op_name, h.count
        ));
        out.push_str(&format!(
            "rspacefs_op_latency_microseconds_sum{{mount=\"{}\",op=\"{}\"}} {}\n",
            m, op_name, h.sum_us
        ));
        out.push_str(&format!(
            "rspacefs_op_in_flight{{mount=\"{}\",op=\"{}\"}} {}\n",
            m, op_name, h.in_flight
        ));
    }
    out
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lat_hist_records_in_correct_bucket() {
        let h = LatHist::new();
        h.observe(40);       // bucket 0 (<=50us)
        h.observe(75);       // bucket 1 (<=100us)
        h.observe(75);       // bucket 1
        h.observe(900);      // bucket 4 (<=1000us)
        h.observe(10_000_000); // beyond last bucket — count++ sum++, no bucket
        let s = h.snapshot();
        assert_eq!(s.buckets[0], 1);
        assert_eq!(s.buckets[1], 2);
        assert_eq!(s.buckets[2], 0);
        assert_eq!(s.buckets[3], 0);
        assert_eq!(s.buckets[4], 1);
        assert_eq!(s.count, 5);
        assert_eq!(s.sum_us, 40 + 75 + 75 + 900 + 10_000_000);
    }

    #[test]
    fn op_scope_increments_and_decrements_in_flight() {
        let s = Stats::new();
        assert_eq!(s.hists.lookup.in_flight.load(Ordering::Relaxed), 0);
        {
            let _scope = s.scope(Op::Lookup);
            assert_eq!(s.hists.lookup.in_flight.load(Ordering::Relaxed), 1);
        }
        assert_eq!(s.hists.lookup.in_flight.load(Ordering::Relaxed), 0);
        // Drop should also have recorded one observation.
        assert_eq!(s.hists.lookup.count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn nested_scopes_dont_corrupt_in_flight() {
        let s = Stats::new();
        let scope_a = s.scope(Op::Read);
        let scope_b = s.scope(Op::Read);
        assert_eq!(s.hists.read.in_flight.load(Ordering::Relaxed), 2);
        drop(scope_b);
        assert_eq!(s.hists.read.in_flight.load(Ordering::Relaxed), 1);
        drop(scope_a);
        assert_eq!(s.hists.read.in_flight.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn prom_emits_cumulative_buckets_and_in_flight() {
        let s = Stats::new();
        s.hists.lookup.observe(40);
        s.hists.lookup.observe(900);
        let snap = s.snapshot();
        let out = render_prom(&snap, "/mnt/test");
        // Cumulative: bucket le=50 has 1, le=1000 has 2, +Inf has 2.
        assert!(out.contains("rspacefs_op_latency_microseconds_bucket{mount=\"/mnt/test\",op=\"lookup\",le=\"50\"} 1"));
        assert!(out.contains("rspacefs_op_latency_microseconds_bucket{mount=\"/mnt/test\",op=\"lookup\",le=\"1000\"} 2"));
        assert!(out.contains("rspacefs_op_latency_microseconds_bucket{mount=\"/mnt/test\",op=\"lookup\",le=\"+Inf\"} 2"));
        assert!(out.contains("rspacefs_op_latency_microseconds_count{mount=\"/mnt/test\",op=\"lookup\"} 2"));
        assert!(out.contains("rspacefs_op_in_flight{mount=\"/mnt/test\",op=\"lookup\"} 0"));
    }
}
