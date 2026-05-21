# Enhancement: rspacefs-registry — Rust OCI Registry Head on Top of rspacefs

## Motivation

rspacefs already has a Rust OCI registry implementation embedded in `pkg/registry/` (carried over from the mkube project's mirror). Promoting that to a first-class component of rspacefs gives us:

1. **End-to-end test surface.** Push images to a local rspacefs-served registry, pull them back through CRI-O on a node that also uses rspacefs as `mount_program`. We can verify the byte-for-byte identity of layers from push side → pull side → mounted rootfs in one process.
2. **Direct push/pull during dev loop.** No need to round-trip through GHCR / Docker Hub during iteration. `podman push localhost:5000/foo` lands directly in the rspacefs store.
3. **Long-term Quay parity goal.** With the storage primitive (rspacefs LayerFS + verity) and an HTTP registry head, we have the bones of a Quay-equivalent — namespaces, robot auth, mirroring, image signing, scanning hooks. A multi-quarter project, but rspacefs is the natural home because layers are already its data model.

## Scope

### v0.x — Minimal Registry Head (this enhancement)

- OCI Distribution Spec v1.1 compliant — blobs, manifests, manifest lists, repository names, mounting.
- Single tenancy, single namespace, no auth (or static htpasswd).
- HTTP only on `:5000` (TLS via the same cert as the registry — out of scope for v0).
- Storage backend: pluggable. Default = filesystem-on-rspacefs (so the registry stores its own blobs through the same code path it serves them via mount_program).
- One static binary `rspacefs-registry`. Cross-compiles for x86_64 + aarch64. Single rust crate `rspacefs-registry` added to the workspace.

### v0.x+1 — Direct Storage Integration

- Skip the FS-as-storage indirection. Push lands blobs directly into the containers-storage layer directory (i.e. the same `/var/lib/containers/storage/overlay/l/...` that CRI-O reads). Pull serves them back from there. Zero copy in the same-node case.
- This is the unique value vs. Docker registry / Zot — registry and runtime share the same storage substrate.

### v1.0 — Quay Parity (multi-quarter)

- Multi-tenancy (namespaces / orgs).
- Robot accounts + scope-based RBAC.
- Image signing (cosign verification on push, allowlist enforcement on pull).
- Repository mirroring (pull-through cache from upstream registries).
- Garbage collection (mark-and-sweep across manifests).
- Vulnerability scanning hook (call out to Clair / Trivy / Grype, attach reports).
- Web UI (separate Rust + WASM front-end, not in the registry crate).

## Architecture (v0.x)

```
                    ┌─────────────────────────────────┐
                    │  rspacefs-registry (Rust binary) │
                    │  ─────────────────────────────  │
podman push ────────►  /v2/_catalog                   │
podman pull ◄───────┤  /v2/<name>/blobs/<digest>      │
                    │  /v2/<name>/manifests/<ref>     │
                    │  /v2/<name>/blobs/uploads       │
                    │                                  │
                    │  Storage trait:                  │
                    │    - blob_put / blob_get         │
                    │    - manifest_put / manifest_get │
                    │    - tag_list                    │
                    │                                  │
                    └────────┬─────────────────────────┘
                             │
                ┌────────────┴────────────┐
                │                         │
       ┌────────▼─────┐           ┌──────▼────────────┐
       │ FS backend   │           │ rspacefs-storage  │
       │ (default v0) │           │ backend (v0.x+1)   │
       │ /var/lib/    │           │ same dirs CRI-O   │
       │   rspacefs/  │           │ reads via mount_  │
       │   registry/  │           │ program           │
       └──────────────┘           └───────────────────┘
```

Crate: `crates/rspacefs-registry/` — depends on `rspacefs-core` (LayerFS) and `rspacefs-verity` (manifest hashes). HTTP via `axum` or `hyper` directly (TBD; benchmark both).

## OpenShift Integration

Deploy as a DaemonSet on each node, expose via Service + Route. ImagePullPolicy=Always against `image-registry.openshift-image-registry.svc:5000` (or our rspacefs-registry Route hostname) — kubelet pulls land through rspacefs's storage path.

## Test Plan

1. v0 acceptance: `podman push localhost:5000/test/img:v1` → `podman pull localhost:5000/test/img:v1` → `podman run localhost:5000/test/img:v1` works end-to-end.
2. Run the rspacefs k8s beatup test (`tests/k8s/workloads/beatup.sh`) using the rspacefs-registry as the source — every image pull funnels through our code.
3. v0.x+1 acceptance: a `podman push` directly populates `/var/lib/containers/storage/overlay/l/` such that a fresh `podman run` on the SAME host needs zero copies — the blob is already in the runtime store.

## Out of Scope (for this enhancement)

- TLS termination (do it with sidecar or Route).
- ImageStream-style admission webhooks.
- Cross-region replication (single-node for now).
- UI / Web console.

## Risks

- The unified-storage angle (v0.x+1) requires understanding containers-storage's blob naming + manifest layout in detail. Not trivial. Worth deferring v0.x+1 until v0 is stable.
- Quay's existing customer surface is enormous. Parity is a multi-year goal, not a sprint. Don't promise parity timeline — promise "we will not break compatibility with the OCI spec, and we will close specific Quay-feature gaps as they're justified."

## Open Questions

- Should we vendor the existing `pkg/registry/` from mkube wholesale, or rewrite cleanly for the workspace?  Decision: review the mkube code; if it's a clean Tower service we can lift it; if it's RouterOS-tangled we rewrite.
- Authentication shape for v0 — htpasswd static or unsigned? Decision: htpasswd, behind a `--auth-file` flag, default off (warn on stdout).
