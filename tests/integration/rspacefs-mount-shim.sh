#!/bin/sh
# rspacefs-mount-shim — daemonising wrapper for /usr/bin/rspacefs-mount.
#
# containers-storage's `mount_program` contract expects the helper to:
#   1. Set up the mount.
#   2. Fork into the background.
#   3. Exit 0 from the parent once the mount is live.
#
# rspacefs-mount itself runs in foreground (blocks until unmount). This
# shim does the daemonise dance: launches rspacefs-mount under setsid,
# waits for the mountpoint to become a real mount, then exits 0.
#
# Install as /usr/bin/rspacefs-mount-shim and reference from
# /etc/containers/storage.conf:
#
#   [storage.options]
#   mount_program = "/usr/bin/rspacefs-mount-shim"
#
# Future: rspacefs-mount itself should grow native daemonisation
# (fork + setsid + pipe-signal-success), at which point this shim
# is no longer needed.

LOG="${RSPACEFS_MOUNT_LOG:-/tmp/rspacefs-mount.log}"
BIN="${RSPACEFS_MOUNT_BIN:-/usr/bin/rspacefs-mount}"

echo "$(date -Iseconds) starting: $*" >> "$LOG"

setsid "$BIN" "$@" </dev/null >>"$LOG" 2>&1 &
disown

# Wait up to ~1s for the mount to become live.
MP=$(echo "$@" | awk '{print $NF}')
for _ in 1 2 3 4 5 6 7 8 9 10; do
  if mountpoint -q "$MP" 2>/dev/null; then
    exit 0
  fi
  sleep 0.1
done

echo "$(date -Iseconds) timeout waiting for $MP to become a mount" >> "$LOG"
exit 1
