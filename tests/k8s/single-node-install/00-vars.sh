# Source me from every other script.
# Pinned versions: production-quality means deterministic.

set -euo pipefail

# Kubernetes minor. Patch resolves to latest at install time.
export K8S_MINOR="${K8S_MINOR:-v1.32}"

# CRI-O matches kube minor.
export CRIO_VERSION="${CRIO_VERSION:-v1.32}"

# Cilium CLI version (installs latest stable Cilium itself).
export CILIUM_CLI_VERSION="${CILIUM_CLI_VERSION:-v0.16.20}"

# Pod CIDR for Cilium. 10.244.0.0/16 is the flannel-style default; we
# use 10.42.0.0/16 to avoid colliding with any host bridge.
export POD_CIDR="${POD_CIDR:-10.42.0.0/16}"
export SERVICE_CIDR="${SERVICE_CIDR:-10.96.0.0/16}"

# rspacefs install paths on the target.
export RSPACEFS_BIN_DIR="${RSPACEFS_BIN_DIR:-/usr/local/bin}"
export RSPACEFS_MOUNT_BIN="${RSPACEFS_BIN_DIR}/rspacefs-mount"
export RSPACEFS_CTL_BIN="${RSPACEFS_BIN_DIR}/rspacefs-ctl"

# CRI-O storage root. CRI-O default is /var/lib/containers/storage. We
# keep that — the mount_program runs underneath transparently.
export CRIO_STORAGE_ROOT="${CRIO_STORAGE_ROOT:-/var/lib/containers/storage}"

# Where the install scripts and binaries were dropped (e.g. ~/k8s-install).
# Auto-detect from this file's location if not set.
export RSPACEFS_INSTALL_DIR="${RSPACEFS_INSTALL_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)}"

# Common log helper.
log() { printf '\033[1;36m[%s]\033[0m %s\n' "$(basename "${BASH_SOURCE[1]:-install}")" "$*"; }
die() { printf '\033[1;31m[ERROR]\033[0m %s\n' "$*" >&2; exit 1; }

# Idempotency: skip if marker file exists.
marker_path() { printf '/var/lib/rspacefs-install/%s.done' "$1"; }
mark_done() {
  install -d /var/lib/rspacefs-install
  touch "$(marker_path "$1")"
}
is_done() { [ -f "$(marker_path "$1")" ]; }

# Re-run a script with `FORCE=1` to bypass the markers.
if [ "${FORCE:-0}" = "1" ]; then
  rm -f /var/lib/rspacefs-install/*.done 2>/dev/null || true
fi
