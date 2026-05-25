# Fault classes — `rspacefs-mount`

The daemon sits in the kernel's filesystem path. If it exits, every
process touching the FUSE mount sees `Transport endpoint is not
connected` and crashes. That makes daemon stability a *kernel-stability*
concern, not just a userspace one.

This document enumerates every fault class the daemon can encounter,
how the daemon reacts, what surfaces in telemetry, and whether the
fault should ever fire in steady state.

Every fault is logged with three sinks:

1. **`tracing` event** — structured JSON via `tracing_journald` or
   stderr, depending on `--log-format`. Fields include `fault`, `op`,
   `panic`/`errno`, and any op-specific context.
2. **`/dev/kmsg`** — one human-readable line so faults show up in
   `dmesg` even if journald is unreachable. Disable with
   `--kmsg-faults=false` (see issue #25 for rationale).
3. **`Stats` counter** — bumped at the same site. Surfaced via the
   control socket (`rspacefs ctl stats`) and `/metrics`.

## Fault index

| `fault_id` | Counter | Recovery | Steady-state? |
|---|---|---|---|
| `op_panic` | `rspacefs_faults_panic_total` | Op-local: panic caught, ENOSYS returned to kernel, daemon continues | **No** — a panic is a bug. Fires once → file an issue. |
| `session_retry` | `rspacefs_faults_session_retry_total` | Session-level: `session.run()` panic caught, mount cleaned up, daemon exits with non-zero so the supervisor restarts it | **No** — pre-#22 this was the daemon-killing path. |
| `lock_poisoned` | `rspacefs_faults_lock_poisoned_total` | Op returns EIO to caller, daemon continues. The poisoned mutex stays poisoned for the process lifetime. | **No** — implies a prior `op_panic` already happened with a lock held. |
| `unexpected_errno` | `rspacefs_faults_unexpected_errno_total` | Returned to caller as-is | **Some** — e.g. ENOSPC on copy-up of a large file on a tight disk. Counter mostly serves as a rate-of-weirdness signal. |

## `op_panic` — Panic inside a FUSE op handler

**Symptom:** A FUSE op (lookup / read / etc.) hits an unexpected
condition that triggers `panic!()` — typically an `unwrap()` on an
unexpected `None`, an out-of-bounds index, or an arithmetic overflow in
debug builds.

**Without hardening:** Panic propagates from the FUSE op closure into
`fuser`'s dispatch loop, out of `Session::run()`, up through `main()`,
and the process exits. The kernel mount becomes
`Transport endpoint is not connected`. Every consumer crashes. This is
the bug class that **#24** exists to prevent.

**Recovery:** The `protect!` macro in `crates/rspacefs-fuse/src/fs.rs`
wraps every FUSE op body in `std::panic::catch_unwind` with
`AssertUnwindSafe`. On a caught panic the macro:

1. Increments `faults_panic`
2. Emits a `tracing::error!(fault = "op_panic", ...)` event
3. Writes one line to `/dev/kmsg` at priority `LOG_ERR`
4. Lets the dropped `reply` object trigger `fuser`'s default ENOSYS
   reply so the kernel sees a complete (if degraded) response instead
   of waiting

The kernel mount stays alive. The session loop continues serving the
next request.

**Caller surface:** The client sees ENOSYS for the panicked op. The
specific request fails, but the next request to the same path
succeeds. A container won't notice unless the panic is deterministic
on the same op — in which case the op spirals to ENOSYS-loop and the
container hangs (still better than the whole node crashing).

**What to do when you see this:** It's a bug. The fault line includes
the panic payload (`payload="...message..."`). Find the panic source
in the named op, fix the root cause, ship.

## `session_retry` — Panic from `Session::run` itself

**Symptom:** Something inside libfuse / fuser's dispatch (not our op
handler) hit a panic. The session-wrapper `catch_unwind` from #22
fires.

**Recovery:** The daemon runs `cleanup_mount_on_exit()` (lazy unmount
via `umount2(MNT_DETACH)`), bumps `faults_session_retry`, logs the
panic, and exits with non-zero so the supervisor restarts it.

**Why not in-process restart?** The kernel mount table entry is
already tied to the dead session's fd. Rebuilding the session
in-process requires unmount + remount, and the kernel sees
`ENOTCONN` for the window between. A supervisor restart accomplishes
the same window with cleaner semantics and a fresh address space.

**What to do when you see this:** It's almost always a `fuser`
upstream bug or a kernel/libfuse mismatch. Capture the kmsg line + the
journald fields and open an issue.

## `lock_poisoned` — `Mutex` left in a poisoned state

**Symptom:** `self.backing_cache.lock()` or `self.recent.lock()`
returns `Err(_)`. The mutex was held during a previous panic.

**Recovery:** The op returns EIO to the kernel; the daemon stays up.
The poisoned mutex stays poisoned — the next op to lock it will fail
the same way. In practice this matters for:

- `backing_cache` — passthrough opens stop caching, every open issues
  a fresh `BACKING_OPEN` ioctl. Slow but functional.
- `recent` — the recent-ops ring is unavailable to the control
  socket. `rspacefs ctl ops` returns empty.

**Why not recover the lock?** `PoisonError::into_inner()` is available
but it implies you've reasoned about whether the state under the lock
is consistent. For both of our mutexes, the state is monotone
(insert-only cache + monotone-tail ring), so it would be safe — but
the *original* panic that caused the poison is the real bug. Fix
that, not the symptom.

**What to do when you see this:** Look at the preceding `op_panic`
event in journald with the same `pid`. That's the root cause.

## `unexpected_errno` — Non-standard errno returned

**Symptom:** A system call returned an errno we don't have specific
handling for (ENOSPC, EDQUOT, EBUSY on a layer file, etc.).

**Recovery:** Returned to the FUSE caller as-is. Counter bumped. No
log spam — these are noisy in normal operation (ENOSPC during a
tight-disk run is *expected*, just notable).

**Steady state:** Some. Treat the counter as a rate-of-weirdness
signal — alert on rate-of-change, not absolute value.

## Other invariants the daemon protects

- **Mount cleanup on every exit path** — `cleanup_mount_on_exit()`
  fires whether the daemon exits normally, panics at the session
  level, or has its tempdir cleaned up. Fixed in #22 via
  `libc::umount2(MNT_DETACH)`. No more stale kernel mount entries
  after daemon death.

- **Empty upper synthesised when buildah doesn't pass one** — #19. The
  daemon never panics on missing `upperdir=`; it creates a disposable
  one in `$XDG_RUNTIME_DIR`.

- **No internal `unwrap()` on user-supplied paths** — every path
  resolution in fs.rs goes through `match { Some(p) => ..., None =>
  return reply.error(ENOENT) }`. Malformed FUSE requests can't panic
  the daemon directly; they have to find a bug in our typed code first.

## Testing fault recovery

Acceptance criterion from #24: 1000 simulated faults must leave zero
stale mounts. The fault-injection harness (deferred — separate
follow-up) drives this by:

1. Mounting an rspacefs FS with a debug-feature-gated panic hook
2. Issuing 1000 `getattr` ops with a payload that triggers the
   injected panic
3. Verifying the daemon survives all 1000 (counter `faults_panic`
   == 1000) and the kernel mount is still serviceable
4. Unmounting + verifying no `/proc/self/mountinfo` entries remain
