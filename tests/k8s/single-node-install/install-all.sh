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
bash "${HERE}/02-crio.sh"
bash "${HERE}/03-rspacefs.sh"
bash "${HERE}/04-kubeadm.sh"
bash "${HERE}/05-cni.sh"
bash "${HERE}/06-validate.sh"

log "==== install complete ===="
log "kubeconfig: /etc/kubernetes/admin.conf (also \$HOME/.kube/config)"
log "next: deploy beatup/benchmark workloads from tests/k8s/workloads/"
