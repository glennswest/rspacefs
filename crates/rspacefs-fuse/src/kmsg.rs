//! Best-effort writer for `/dev/kmsg` — the kernel ring buffer that `dmesg`
//! reads from. Used for fault-class messages that MUST be visible even when
//! the daemon's stdout / journald destination is gone (the host journal is
//! down, the container was orphaned, the operator only has serial console).
//!
//! Format follows the kernel's relaxed contract: a leading `<N>` priority
//! prefix (`<3>` = LOG_ERR, `<4>` = LOG_WARNING) plus a free-form payload,
//! one record per `write(2)`. The kernel itself enforces the 1024-byte limit
//! and timestamps the record. We open `/dev/kmsg` lazily, write, close —
//! no caching, no buffering, no async; designed to be safe to call from a
//! panic handler or a signal-adjacent context.
//!
//! All errors are deliberately swallowed: if the kernel can't accept the
//! write (no permission inside an unprivileged container, /dev/kmsg missing
//! in a minimal namespace), we lose this one message rather than crashing
//! the daemon.

use std::fmt::Write as _;
use std::io::Write;

/// Syslog priority levels mapped to the kernel `KERN_*` macros.
#[allow(dead_code)]
#[derive(Copy, Clone)]
pub enum Prio {
    Err = 3,
    Warn = 4,
    Info = 6,
}

/// Write one record to `/dev/kmsg`. Truncates the payload to ~900 bytes so
/// the kernel doesn't reject it. Errors are silently dropped — this is a
/// "shout into the void if you can" channel, never a hard dependency.
pub fn write(prio: Prio, msg: &str) {
    let mut line = String::with_capacity(msg.len() + 32);
    let _ = write!(
        line,
        "<{}>rspacefs-mount[{}]: ",
        prio as u8,
        std::process::id()
    );
    // Strip newlines — one record per write; embedded newlines split the
    // record at the kernel side and confuse downstream parsers.
    for ch in msg.chars().take(900) {
        if ch == '\n' || ch == '\r' {
            line.push(' ');
        } else {
            line.push(ch);
        }
    }
    let f = std::fs::OpenOptions::new().write(true).open("/dev/kmsg");
    if let Ok(mut f) = f {
        let _ = f.write_all(line.as_bytes());
    }
}
