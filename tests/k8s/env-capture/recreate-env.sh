#!/usr/bin/env bash
# recreate-env.sh — given a captured env snapshot, install matching versions
# on a fresh host so a regression can be reproduced.
#
# Usage:
#   sudo ./recreate-env.sh <snapshot-dir>
#
# What it does, in order:
#   1. Print a diff between the snapshot and the current host.
#   2. Install the matching kernel (if --install-kernel) — reboot required.
#   3. Install the matching CRI-O, kubelet, kubeadm, kubectl packages.
#   4. Install the matching rspacefs-mount binary (by sha256 from snapshot).
#   5. Drop the matching /etc/containers/storage.conf + /etc/crio/crio.conf.d/.
#   6. Restart crio and kubelet.
#
# Does NOT re-run kubeadm init. That's left to install-all.sh; this script
# just rebuilds the environment substrate.

set -uo pipefail

SNAP="${1:-}"
[ -n "$SNAP" ] && [ -d "$SNAP" ] || { echo "usage: $0 <snapshot-dir>" >&2; exit 2; }
[ "$(id -u)" = "0" ] || { echo "must run as root" >&2; exit 2; }

cd "$SNAP"

echo "==> reading snapshot.json"
[ -f snapshot.json ] || { echo "no snapshot.json in $SNAP" >&2; exit 2; }

SNAP_KERNEL=$(awk -F'"' '/"kernel":/ {print $4}' snapshot.json)
SNAP_KERNEL_RPM=$(awk -F'"' '/"kernel_rpm":/ {print $4}' snapshot.json)
SNAP_KUBE=$(awk -F'"' '/"kubelet_version":/ {print $4}' snapshot.json)
SNAP_CRIO=$(awk -F'"' '/"crio_version":/ {print $4}' snapshot.json)
SNAP_MOUNT_SHA=$(awk -F'"' '/"rspacefs_mount_sha256":/ {print $4}' snapshot.json)

HOST_KERNEL=$(uname -srm)
HOST_KERNEL_RPM=$(rpm -q kernel-core 2>/dev/null || echo n/a)
HOST_KUBE=$(kubelet --version 2>/dev/null | awk '{print $2}' || echo n/a)
HOST_CRIO=$(crio --version 2>/dev/null | head -1 | awk '{print $3}' || echo n/a)

echo
printf '%-22s %-35s %-35s\n' "component" "snapshot" "current host"
printf '%-22s %-35s %-35s\n' "----------------------" "-----------------------------------" "-----------------------------------"
printf '%-22s %-35s %-35s\n' "kernel"        "$SNAP_KERNEL"      "$HOST_KERNEL"
printf '%-22s %-35s %-35s\n' "kernel rpm"    "$SNAP_KERNEL_RPM"  "$HOST_KERNEL_RPM"
printf '%-22s %-35s %-35s\n' "kubelet"       "$SNAP_KUBE"        "$HOST_KUBE"
printf '%-22s %-35s %-35s\n' "crio"          "$SNAP_CRIO"        "$HOST_CRIO"
printf '%-22s %-35s %-35s\n' "rspacefs-mount sha (12)" "${SNAP_MOUNT_SHA:0:12}" "$(sha256sum /usr/local/bin/rspacefs-mount 2>/dev/null | head -c 12)"
echo

if [ "${DRY_RUN:-0}" = "1" ]; then
  echo "DRY_RUN=1 — stopping after diff"
  exit 0
fi

# ── packages ────────────────────────────────────────────────────────────
if [ "$SNAP_KERNEL_RPM" != "$HOST_KERNEL_RPM" ] && [ -n "${INSTALL_KERNEL:-}" ]; then
  echo "==> installing kernel package $SNAP_KERNEL_RPM (reboot required afterwards)"
  dnf -y install "$SNAP_KERNEL_RPM"
fi

# CRI-O — match version. Fedora dnf lets us pin via version-release suffix.
if [ "$SNAP_CRIO" != "n/a" ] && [ "$SNAP_CRIO" != "$HOST_CRIO" ]; then
  echo "==> installing cri-o-$SNAP_CRIO"
  dnf -y install "cri-o-$SNAP_CRIO" || dnf -y install cri-o
fi

# kubelet/kubeadm/kubectl — version from the snapshot
if [ "$SNAP_KUBE" != "n/a" ] && [ "$SNAP_KUBE" != "$HOST_KUBE" ]; then
  kube_ver="${SNAP_KUBE#v}"   # strip leading 'v' for rpm
  echo "==> installing kubelet/kubeadm/kubectl $kube_ver"
  dnf -y install "kubelet-${kube_ver}" "kubeadm-${kube_ver}" "kubectl-${kube_ver}" || \
    echo "  (warning: exact rpm version may not be in repo; install latest from same minor)"
fi

# ── rspacefs binary ────────────────────────────────────────────────────
if [ -f rspacefs-bin/rspacefs-mount ]; then
  echo "==> installing rspacefs-mount from snapshot"
  install -m 0755 rspacefs-bin/rspacefs-mount /usr/local/bin/rspacefs-mount
  if [ -f rspacefs-bin/rspacefs ]; then
    install -m 0755 rspacefs-bin/rspacefs /usr/local/bin/rspacefs
  fi
  echo "  rspacefs-mount sha256: $(sha256sum /usr/local/bin/rspacefs-mount | awk '{print $1}')"
  echo "  expected from snapshot: $SNAP_MOUNT_SHA"
fi

# ── configs ─────────────────────────────────────────────────────────────
if [ -f storage.conf ]; then
  echo "==> restoring /etc/containers/storage.conf"
  install -d /etc/containers
  install -m 0644 storage.conf /etc/containers/storage.conf
fi
if [ -d crio.d ]; then
  echo "==> restoring /etc/crio/crio.conf.d/"
  install -d /etc/crio/crio.conf.d
  cp -p crio.d/* /etc/crio/crio.conf.d/
fi

# ── restart services ────────────────────────────────────────────────────
echo "==> restarting crio + kubelet"
systemctl daemon-reload
systemctl restart crio
systemctl restart kubelet 2>/dev/null || true

echo
echo "==> environment recreated. To bring the cluster up:"
echo "    sudo ~/k8s-install/install-all.sh"
echo
echo "Then re-run the test workload that produced the original snapshot."
