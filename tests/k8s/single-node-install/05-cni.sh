#!/usr/bin/env bash
# 05-cni.sh — install Cilium CNI via the official CLI.
# Idempotent.

source "$(dirname "$0")/00-vars.sh"

if is_done 05-cni; then
  log "cilium already installed, skipping"
  exit 0
fi

[ "$(id -u)" = "0" ] || die "must run as root"

export KUBECONFIG=/etc/kubernetes/admin.conf

if ! command -v cilium >/dev/null; then
  log "installing cilium CLI ${CILIUM_CLI_VERSION}"
  ARCH="$(uname -m)"
  case "$ARCH" in
    x86_64) CARCH=amd64 ;;
    aarch64) CARCH=arm64 ;;
    *) die "unknown arch $ARCH" ;;
  esac
  TMPTAR="/tmp/cilium-cli.tar.gz"
  curl -sSLf "https://github.com/cilium/cilium-cli/releases/download/${CILIUM_CLI_VERSION}/cilium-linux-${CARCH}.tar.gz" -o "$TMPTAR"
  tar -C /usr/local/bin -xzf "$TMPTAR" cilium
  rm -f "$TMPTAR"
fi

log "installing Cilium (replaces kube-proxy via eBPF)"
cilium install \
  --set ipam.mode=kubernetes \
  --set kubeProxyReplacement=true \
  --set k8sServiceHost=$(hostname -I | awk '{print $1}') \
  --set k8sServicePort=6443 \
  --set ipv4NativeRoutingCIDR="${POD_CIDR}"

log "waiting for cilium pods to be Ready (up to 5 min)"
cilium status --wait --wait-duration 5m || die "cilium failed to converge"

mark_done 05-cni
log "cilium CNI installed and healthy"
