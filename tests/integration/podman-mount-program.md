# Integration test: rspacefs-mount as containers-storage `mount_program`

End-to-end validation that `rspacefs-mount` can stand in for `fuse-overlayfs`
in a real `podman` / `containers-storage` install. This is the OpenShift /
CRI-O integration path described in
`mkube/enhancements/openshift-crio-rspacefs-integration.md`.

## Environment

| Item | Value |
|---|---|
| Host | `test1.g8.lo` |
| OS | Fedora 43 Cloud Edition |
| Kernel | `6.17.1-300.fc43.x86_64` |
| Container engine | podman 5.x, conmon 2.1.13, crun 1.24 |
| Storage driver | `overlay` (rootless) |
| `containers-storage` config | `~/.config/containers/storage.conf` (user-level) |
| `mount_program` | `/usr/bin/rspacefs-mount-shim` → `/usr/bin/rspacefs-mount` |

## Setup

```sh
# Install the binary
sudo install -m 0755 rspacefs-mount /usr/bin/rspacefs-mount

# Install the daemonising shim (mount_program is expected to fork+exit 0)
sudo install -m 0755 tests/integration/rspacefs-mount-shim.sh /usr/bin/rspacefs-mount-shim

# Point containers-storage at our shim
mkdir -p ~/.config/containers
cat > ~/.config/containers/storage.conf <<EOF
[storage]
driver = "overlay"
runroot = "$XDG_RUNTIME_DIR/containers"
graphroot = "$HOME/.local/share/containers/storage"

[storage.options]
mount_program = "/usr/bin/rspacefs-mount-shim"
mountopt = "nodev"
EOF

# Reset any pre-existing podman state so the new mount_program is exercised
podman system reset --force
```

## Procedure

```sh
: > /tmp/rspacefs-mount.log
podman pull docker.io/library/alpine:latest
podman run --rm docker.io/library/alpine:latest /bin/sh -c \
  "uname -srm; cat /etc/os-release | head -2; echo hello from container"
cat /tmp/rspacefs-mount.log
```

## Expected result

Container starts; rootfs is assembled by `rspacefs-mount` via FUSE;
`uname` / `cat` / `echo` succeed inside the container.

## Actual result — 2026-05-20 (run on test1.g8.lo)

```
--- pull alpine (fresh, exercises mount_program) ---
Getting image source signatures
Copying blob sha256:6a0ac1617861a677b045b7ff88545213ec31c0ff08763195a70a4a5adda577bb
Copying config sha256:3cb067eab609612d81b4d82ff8ad71d73482bb3059a87b642d7e14f0ed659cde
Writing manifest to image destination
3cb067eab609612d81b4d82ff8ad71d73482bb3059a87b642d7e14f0ed659cde

--- run alpine, echo from inside ---
Linux 6.17.1-300.fc43.x86_64 x86_64
NAME="Alpine Linux"
ID=alpine
hello from container

--- rspacefs-mount-shim log (proof it was called) ---
2026-05-20T13:22:59+00:00 starting: \
  -o lowerdir=/home/fedora/.local/share/containers/storage/overlay/l/2IIKYFJKHSSXNXSA2TUGJWWD74,\
     upperdir=/home/fedora/.local/share/containers/storage/overlay/313b7c6ba91472c27336c51c03c841b4dbfa9c37b89e20cbc56057f7eac58104/diff,\
     workdir=/home/fedora/.local/share/containers/storage/overlay/313b7c6ba91472c27336c51c03c841b4dbfa9c37b89e20cbc56057f7eac58104/work,\
     volatile,\
     context="system_u:object_r:container_file_t:s0:c133,c757" \
  /home/fedora/.local/share/containers/storage/overlay/313b7c6ba91472c27336c51c03c841b4dbfa9c37b89e20cbc56057f7eac58104/merged

INFO  starting rspacefs FUSE mount (mount_program mode)
      mountpoint=.../merged upper=.../diff lowers=1 verified_lowers=0
INFO  Mounting .../merged
INFO  FUSE_PASSTHROUGH enabled — read-only opens of non-verified files bypass the daemon
WARN  [Not Implemented] poll(ino: 0x4, fh: 1, ph: PollHandle(1), events: 8221, flags: 1)
INFO  unmounting session at .../merged
ERROR Unmount failed: Invalid argument (os error 22)
```

## Verdict

✅ End-to-end works. Container ran. rspacefs-mount was the mount_program. Reads
went through FUSE (passthrough where applicable). Container exit triggered
unmount cleanly enough for the next `podman run` to succeed.

## Issues surfaced (filed against the repo)

1. **`poll` op not implemented** — `WARN [Not Implemented] poll(...)` during
   container shell startup. Does not block execution but is noisy and could
   confuse callers that rely on `poll()` for fd-readiness on the mount.
   Fix: implement `Filesystem::poll` returning "always readable/writable"
   for regular files, or proxy to the backing fd.

2. **`Unmount failed: Invalid argument`** on session teardown — the kernel
   has already detached the mount (lazy unmount by containers-storage), so
   fuser's explicit `umount2` returns `EINVAL`. Cosmetic but worth
   silencing; ideally fuser's session-drop swallows `EINVAL` when the mount
   has already gone away.
