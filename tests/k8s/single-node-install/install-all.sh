#!/usr/bin/env bash
# install-all.sh — orchestrator. Run scripts 01..06 in order, fail fast.

set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
source "${HERE}/00-vars.sh"

[ "$(id -u)" = "0" ] || die "must run as root (use sudo)"

log "==== rspacefs single-node K8s installer ===="
log "K8S_MINOR=${K8S_MINOR}  CRIO_VERSION=${CRIO_VERSION}  CILIUM_CLI_VERSION=${CILIUM_CLI_VERSION}"
log "RSPACEFS_INSTALL_DIR=${RSPACEFS_INSTALL_DIR}"

bash "${HERE}/01-prereqs.sh"
# rspacefs binaries must exist before crio.service starts (storage.conf
# references mount_program; crio refuses to come up without it).
bash "${HERE}/02-rspacefs.sh"
bash "${HERE}/03-crio.sh"
bash "${HERE}/04-kubeadm.sh"
bash "${HERE}/05-cni.sh"
bash "${HERE}/06-validate.sh"

log "==== install complete ===="
log "kubeconfig: /etc/kubernetes/admin.conf (also \$HOME/.kube/config)"
log "next: deploy beatup/benchmark workloads from tests/k8s/workloads/"

# Drop a snapshot of the just-built environment so we can reproduce it later.
# Best-effort — failure here doesn't break the install.
CAP="${HERE}/../env-capture/capture-env.sh"
if [ -x "$CAP" ]; then
  log "capturing env snapshot"
  bash "$CAP" --purpose bootstrap || log "(env snapshot failed; not fatal)"
fi
