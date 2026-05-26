# Concurrency in rspacefs-mount

Tracks issue [#23](https://github.com/glennswest/rspacefs/issues/23).

## The problem

`fuser::Session::run()` is a **single-threaded read-dispatch loop**. It owns
one receive buffer, reads exactly one kernel request into it, calls
`req.dispatch(self)` — which invokes the matching `Filesystem` method with
`&mut self` — and only then loops back to read the next request. The buffer is
reused to keep memory flat, so the *read* side is intentionally serial.

The consequence: while a single op handler is running, **no other op can even
be read off `/dev/fuse`**. One slow `read` across a cold-cache page, or a
verity block that has to be hashed, stalls every other container sharing that
daemon — `getattr`, `lookup`, reads of unrelated files, all of it.

FUSE_PASSTHROUGH already removes the daemon from the hot path for *non-verified*
read-only opens (the kernel reads the backing fd directly). So the ops that
still block the loop are: verity-protected reads, the streaming/buffered
fallback reads, and writes.

## The fuser-sanctioned answer

`Session::run`'s own docstring:

> This read-dispatch-loop is non-concurrent to prevent having multiple buffers
> (which take up much memory), but the filesystem methods **may run concurrent
> by spawning threads**.

The mechanism: every `Reply*` object (`ReplyData`, `ReplyWrite`, `ReplyEmpty`,
…) is built on a `Box<dyn ReplySender>`, and `ReplySender: Send + Sync +
'static`. So a `Reply*` is `Send` — a handler can move it onto a worker thread,
return immediately (freeing the dispatch loop to read the next request), and
let the worker call `reply.data(...)` / `reply.error(...)` whenever the slow
work finishes. The kernel correlates the response by the request's `unique`,
which the reply carries.

We do **not** switch FUSE protocols, fork fuser, or add an async runtime. We
keep `Session::run()` exactly as-is and offload selected handlers.

## Design

```
          /dev/fuse
              │  (single dispatch thread — fuser::Session::run)
              ▼
   ┌─────────────────────────────────────────────┐
   │ RspacefsFuse::read / write                   │
   │  • clone Arc<Mutex<OpenFile>> for this fh     │
   │  • clone Arc<Stats>, make OwnedOpScope        │
   │  • pool.execute(move || { … reply … })  ──────┼──┐  returns immediately
   └─────────────────────────────────────────────┘  │
              │ (loop reads next request)            │
              ▼                                       ▼
   metadata ops run inline                  worker pool (N = --io-threads)
   (lookup/getattr/readdir/…)                 lock the per-handle Mutex,
   on the dispatch thread —                   do the I/O, reply, drop scope
   no locking needed                          (latency recorded on drop)
```

### What's shared, and how

| State | Sync | Why |
|-------|------|-----|
| `inodes`, `paths`, `next_ino` | none | Touched only by metadata ops, which run on the single dispatch thread. |
| `open_files: HashMap<u64, Arc<Mutex<OpenFile>>>` | outer map: none | The map itself is mutated only on the dispatch thread (`open`/`create` insert, `release` removes). Workers receive a **clone of the inner `Arc<Mutex<OpenFile>>`** before being dispatched — they never touch the map. |
| `OpenFile` (per handle) | `Mutex` | A worker locks one handle for the duration of its I/O. Two reads on the *same* fh serialize (this protects the single seek cursor in `Streaming`); reads on *different* handles run fully in parallel — that's the win. |
| `backing_cache` | already `Mutex` | Unchanged. |
| `Stats` | atomics | Unchanged. `OwnedOpScope` owns an `Arc<Stats>` so the latency/in-flight guard can ride along into the worker (the borrowed `OpScope<'a>` can't cross threads). |

### Which ops offload

Offloaded to the pool: **`read`, `write`** — the data-path ops named in #23,
and the ones that block on real I/O.

Still inline on the dispatch thread (documented follow-up):
`open` (buffered opens `read_to_end` the file), `release`/`fsync` (dirty
write-back). These are lower-frequency than `read`/`write` in a container
rootfs and are correct as-is; offloading them needs the worker to mutate the
`open_files` map, which means locking the outer map. Deferred.

### Panic + poison containment

Each worker body runs under `run_protected()`, which mirrors the `protect!`
macro: `catch_unwind` → on panic, bump `faults_panic`, emit a fault event
(journald + `/dev/kmsg`), and let the dropped `reply` fall through to fuser's
default `ENOSYS`. A panic in a worker stays contained to that one op; the pool
thread survives.

If a worker panics while holding a handle `Mutex`, the lock is poisoned. The
next locker recovers via `into_inner()` (bumping `faults_lock_poisoned`) rather
than propagating — one corrupted op must not permanently `EIO` the handle.

### The pool

`pool.rs`: a fixed-size, std-only thread pool (`mpsc` channel of
`Box<dyn FnOnce() + Send>` + N worker threads sharing an
`Arc<Mutex<Receiver>>`). No `rayon`, no `tokio` — consistent with the rest of
the daemon (the metrics HTTP server is hand-rolled std too). Size comes from
`--io-threads` (default `std::thread::available_parallelism()`). On `Drop` the
sender is dropped and workers are joined, so a clean unmount drains in-flight
I/O.

## Acceptance (from #23)

- bigbust ([#11](https://github.com/glennswest/rspacefs/issues/11)) shows lower
  p99 time-to-container and higher pods/sec on the same hardware.
- No data races (verify under `cargo test` + a TSan / loom pass if added).

## Status

Pool + `read`/`write` offload + `--io-threads` landed; correctness verified by
the build of record (forcicd, Linux). Throughput validation against bigbust is
pending the K8s cluster (blocked on the Fedora 42 reimage, see work plan).
