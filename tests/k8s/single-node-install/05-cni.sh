#!/usr/bin/env bash
# 05-cni.sh — install a CNI plugin. Default: flannel.
#
# Why flannel: it's the smallest, simplest, production-grade CNI that
# Just Works on modern kernels. Cilium 1.16.3 hits an iptables-wrapper
# bug on kernel ≥ 6.17 where its iptables-nft binary fails inside the
# cilium-agent container ("Unable to redirect iptables binaries").
# That's a Cilium image issue, not a rspacefs / kernel issue, and not
# the test we're trying to run here. Flannel sidesteps it entirely.
#
# Set CNI=cilium to try Cilium anyway (e.g. once you're on a fixed
# Cilium release).

source "$(dirname "$0")/00-vars.sh"

if is_done 05-cni; then
  log "cni already installed, skipping"
  exit 0
fi

[ "$(id -u)" = "0" ] || die "must run as root"

export KUBECONFIG=/etc/kubernetes/admin.conf
CNI="${CNI:-flannel}"

case "$CNI" in
  flannel)
    # Flannel default pod CIDR is 10.244.0.0/16 — must match what kubeadm
    # was initialised with. 04-kubeadm uses POD_CIDR=10.42.0.0/16 by
    # default; we substitute that into the flannel manifest before apply.
    log "installing flannel CNI (pod CIDR ${POD_CIDR})"
    TMP="/tmp/kube-flannel.yml"
    curl -sSLf https://raw.githubusercontent.com/flannel-io/flannel/master/Documentation/kube-flannel.yml -o "$TMP"
    # Rewrite the pod CIDR to whatever kubeadm was given.
    sed -i.bak "s#10.244.0.0/16#${POD_CIDR}#g" "$TMP"
    kubectl apply -f "$TMP"
    log "waiting for flannel DaemonSet to be Ready (up to 3 min)"
    kubectl -n kube-flannel rollout status ds/kube-flannel-ds --timeout=180s || die "flannel didn't converge"
    ;;
  cilium)
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
      --set k8sServiceHost="$(hostname -I | awk '{print $1}')" \
      --set k8sServicePort=6443 \
      --set ipv4NativeRoutingCIDR="${POD_CIDR}"
    log "waiting for cilium pods to be Ready (up to 5 min)"
    cilium status --wait --wait-duration 5m || die "cilium failed to converge"
    ;;
  *)
    die "unknown CNI=${CNI} (supported: flannel, cilium)"
    ;;
esac

mark_done 05-cni
log "CNI (${CNI}) installed and healthy"
