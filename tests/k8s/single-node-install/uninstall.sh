#!/usr/bin/env bash
# uninstall.sh — destructive teardown. kubeadm reset + remove packages + clean storage.
# Run only if you want to start over.

source "$(dirname "$0")/00-vars.sh"

[ "$(id -u)" = "0" ] || die "must run as root"

if [ "${YES:-0}" != "1" ]; then
  cat >&2 <<EOF
This will destroy the cluster on this host:
  - kubeadm reset --force
  - dnf remove cri-o kubelet kubeadm kubectl
  - delete /var/lib/etcd, /var/lib/kubelet, /var/lib/containers, /etc/cni/net.d, /etc/kubernetes
  - delete /etc/containers/storage.conf and restore pre-rspacefs backup if present
  - remove /var/lib/rspacefs-install markers

Re-run with YES=1 to proceed.
EOF
  exit 1
fi

log "kubeadm reset"
kubeadm reset --force --cri-socket=unix:///var/run/crio/crio.sock || true

log "stopping services"
systemctl disable --now kubelet 2>/dev/null || true
systemctl disable --now crio    2>/dev/null || true

log "uninstalling cilium objects"
export KUBECONFIG=/etc/kubernetes/admin.conf
cilium uninstall 2>/dev/null || true

log "removing packages"
dnf -y remove cri-o cri-tools kubelet kubeadm kubectl 2>/dev/null || true

log "wiping state"
rm -rf /var/lib/etcd /var/lib/kubelet /var/lib/containers /etc/cni/net.d \
       /etc/kubernetes /var/lib/rspacefs-install /var/log/kubeadm-init.log \
       /etc/crio/crio.conf.d/10-rspacefs.conf

log "restoring storage.conf"
if [ -f /etc/containers/storage.conf.pre-rspacefs ]; then
  mv /etc/containers/storage.conf.pre-rspacefs /etc/containers/storage.conf
else
  rm -f /etc/containers/storage.conf
fi

log "removing rspacefs binaries"
rm -f "$RSPACEFS_MOUNT_BIN" "$RSPACEFS_CLI_BIN"

log "uninstall complete — host is clean"
