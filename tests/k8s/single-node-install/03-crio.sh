#!/usr/bin/env bash
# 03-crio.sh — install CRI-O, wire rspacefs as containers-storage mount_program.
# Idempotent. Must run AFTER 02-rspacefs.sh: storage.conf references
# /usr/local/bin/rspacefs-mount and crio refuses to start without it.

source "$(dirname "$0")/00-vars.sh"

if is_done 03-crio; then
  log "crio already done, skipping"
  exit 0
fi

[ "$(id -u)" = "0" ] || die "must run as root"

log "installing CRI-O ${CRIO_VERSION}"
dnf -y install cri-o cri-tools containers-common >/dev/null

log "writing /etc/containers/storage.conf with rspacefs mount_program"
# Back up any pre-existing storage.conf once.
[ -f /etc/containers/storage.conf ] && [ ! -f /etc/containers/storage.conf.pre-rspacefs ] \
  && cp /etc/containers/storage.conf /etc/containers/storage.conf.pre-rspacefs

install -d /etc/containers
cat >/etc/containers/storage.conf <<EOF
# Managed by rspacefs single-node installer. The mount_program below
# tells containers-storage to ask rspacefs-mount to assemble lower+upper
# instead of using fuse-overlayfs or kernel overlay.

[storage]
driver = "overlay"
runroot = "/run/containers/storage"
graphroot = "${CRIO_STORAGE_ROOT}"

[storage.options]
additionalimagestores = []

[storage.options.overlay]
# This is the integration point that hands CRI-O image lifecycle to rspacefs.
mount_program = "${RSPACEFS_MOUNT_BIN}"
mountopt = "nodev,metacopy=on"
EOF

log "writing /etc/crio/crio.conf.d/10-rspacefs.conf (set storage driver explicitly)"
install -d /etc/crio/crio.conf.d
cat >/etc/crio/crio.conf.d/10-rspacefs.conf <<EOF
[crio]
storage_driver = "overlay"
storage_option = [
  "overlay.mount_program=${RSPACEFS_MOUNT_BIN}",
  "overlay.mountopt=nodev,metacopy=on",
]

[crio.image]
pause_image = "registry.k8s.io/pause:3.10"
EOF

log "enabling crio.service"
systemctl daemon-reload
systemctl enable --now crio

# Confirm CRI-O came up cleanly.
sleep 2
if ! systemctl is-active --quiet crio; then
  journalctl -u crio --no-pager -n 50 >&2
  die "crio failed to start"
fi
log "crio is active: $(crictl --runtime-endpoint unix:///var/run/crio/crio.sock version 2>/dev/null | head -1)"

mark_done 03-crio
log "crio done"
