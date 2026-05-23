# OpenShift / Kubernetes — rspacefs metrics

`rspacefs-mount` exposes a Prometheus `/metrics` endpoint when started
with `--metrics-addr HOST:PORT`. The endpoint returns the same data
that's available via the control socket's `metrics-text` command, but
served over plain HTTP so OpenShift's user-workload monitoring (or any
other Prometheus) can scrape it directly.

## What's exposed

| Metric | Type | Labels | What it means |
|---|---|---|---|
| `rspacefs_uptime_seconds` | gauge | `mount` | seconds since this daemon started |
| `rspacefs_ops_total` | counter | `mount`, `op` | FUSE op invocations, one series per op name (`lookup`, `getattr`, ...) |
| `rspacefs_bytes_read_total` | counter | `mount` | bytes returned via `read()` |
| `rspacefs_bytes_written_total` | counter | `mount` | bytes accepted via `write()` |
| `rspacefs_passthrough_opens_total` | counter | `mount` | opens served via FUSE_PASSTHROUGH (kernel-direct) |
| `rspacefs_streaming_opens_total` | counter | `mount` | opens served via daemon streaming |
| `rspacefs_buffered_opens_total` | counter | `mount` | opens served via in-memory buffer (writable) |
| `rspacefs_copy_ups_total` | counter | `mount` | copy-ups from lower → upper |
| `rspacefs_copy_up_bytes_total` | counter | `mount` | bytes copied during copy-up |
| `rspacefs_reflinks_ok_total` | counter | `mount` | copy-ups that took FICLONE fast path |
| `rspacefs_reflinks_fallback_total` | counter | `mount` | copy-ups that fell back to byte copy |
| `rspacefs_backing_cache_hits_total` | counter | `mount` | BackingId cache reuse |
| `rspacefs_backing_cache_misses_total` | counter | `mount` | BackingId cache miss (BACKING_OPEN ioctl) |
| `rspacefs_errors_total` | counter | `mount`, `kind` | errors returned to clients, by kind (`io` / `enoent` / `other`) |
| `rspacefs_open_handles` | gauge | `mount` | current file handles in the open table |
| `rspacefs_last_op_unix_ms` | gauge | `mount` | epoch-ms of the last op (liveness signal) |

## DaemonSet pattern (per-process listener)

Each container start spawns one `rspacefs-mount` process per mount.
Putting an HTTP listener in each process is one port per mount — fine
for handfuls, fragile for hundreds. Two deployment shapes:

1. **Direct (small clusters)** — give each `rspacefs-mount` a unique
   port from an ephemeral range. ServiceMonitor selects by label on a
   wrapper Service. Pros: zero extra binaries. Cons: port pool to
   manage; one Endpoint per process.

2. **Node exporter (recommended for production)** — a single
   `rspacefs-node-exporter` per node aggregates all live processes
   from `/run/rspacefs/*.sock`, sums per-op counters, and re-exposes
   one node-level `/metrics` with `pid` as an extra label. Pros: one
   port per node; clean aggregation; survives mount/unmount churn.
   Cons: separate binary (not yet shipped — task #9 follow-up).

The node-exporter is the long-term shape; for the test cluster the
direct mode is fine.

## Direct-mode setup (test cluster)

Start `rspacefs-mount` with `--metrics-addr 127.0.0.1:9090` (or pick a
per-process port). The address binds to localhost by default. To
scrape from another host or pod, bind to `0.0.0.0:9090` or use a
sidecar proxy.

```bash
rspacefs-mount \
  --upper /var/lib/containers/storage/overlay/<id>/diff \
  --lower /var/lib/containers/storage/overlay/l/<id> \
  --metrics-addr 0.0.0.0:9090 \
  /var/lib/containers/storage/overlay/<id>/merged
```

Test the endpoint:

```bash
curl -s http://127.0.0.1:9090/metrics
curl -s http://127.0.0.1:9090/healthz
```

## OpenShift ServiceMonitor

Once a Service fronts the `rspacefs-mount` metrics port (or, with the
node-exporter, fronts the per-node endpoint), point a ServiceMonitor
at it:

```yaml
apiVersion: monitoring.coreos.com/v1
kind: ServiceMonitor
metadata:
  name: rspacefs
  namespace: openshift-user-workload-monitoring
  labels:
    app: rspacefs
spec:
  selector:
    matchLabels:
      app: rspacefs
  endpoints:
    - port: metrics
      path: /metrics
      interval: 30s
      scrapeTimeout: 25s
  namespaceSelector:
    matchNames:
      - openshift-machine-config-operator  # or wherever rspacefs runs
```

Pair with a Service:

```yaml
apiVersion: v1
kind: Service
metadata:
  name: rspacefs
  namespace: openshift-machine-config-operator
  labels:
    app: rspacefs
spec:
  selector:
    app: rspacefs
  ports:
    - name: metrics
      port: 9090
      targetPort: 9090
```

## Healthz

`GET /healthz` returns `200 OK` with body `ok\n` as long as the
metrics thread is alive. Use it as a kubelet liveness probe for the
node-exporter DaemonSet (when it exists).

## Cross-references

- `crates/rspacefs-fuse/src/metrics.rs` — the HTTP server impl
- `crates/rspacefs-fuse/src/stats.rs` — the counter struct + Prometheus rendering
- Control-socket equivalent: `rspacefs ctl --socket /run/rspacefs.sock metrics`
