#!/usr/bin/env bash
# 01-prereqs.sh — kernel modules, sysctls, swap, SELinux, firewall.
# Idempotent.

source "$(dirname "$0")/00-vars.sh"

if is_done 01-prereqs; then
  log "prereqs already done, skipping (FORCE=1 to redo)"
  exit 0
fi

[ "$(id -u)" = "0" ] || die "must run as root"

# ── Distro / kernel guard ───────────────────────────────────────────────
# Fedora 43 (kernel 6.17) ships an iptables/nftables stack that breaks
# kube-proxy and Cilium inside their containers ("iptables is not available
# on this host" / "Unable to redirect iptables binaries"). Until upstream
# k8s + Cilium ship clean nftables-mode container images, hard-stop on F43.
# Override with FORCE_DISTRO=1 if you're hunting that bug specifically.
if [ -f /etc/os-release ]; then
  . /etc/os-release
  case "${VERSION_ID:-unknown}" in
    42|41)
      log "distro check OK: ${PRETTY_NAME:-Fedora ${VERSION_ID}}"
      ;;
    43|44|45)
      if [ "${FORCE_DISTRO:-0}" != "1" ]; then
        die "unsupported distro ${PRETTY_NAME:-Fedora ${VERSION_ID}}: see REIMAGE.md (reimage to Fedora 42). Override with FORCE_DISTRO=1 only if you're debugging the iptables/nftables regression."
      fi
      log "WARNING: running on ${PRETTY_NAME} despite FORCE_DISTRO=1 — expect kube-proxy/CNI failures"
      ;;
    *)
      log "distro ${PRETTY_NAME:-Fedora ${VERSION_ID}} not on the tested list (Fedora 41/42); proceeding anyway"
      ;;
  esac
fi

log "disabling swap (and Fedora's zram-generator which re-enables it via /dev/zram0)"
# Fedora 41+ ships zram-generator-defaults which creates /dev/zram0 as a swap
# device on every boot. swapoff alone is not enough; kubelet will see zram
# swap re-enable shortly after and exit with --fail-swap-on=true. Remove
# the defaults package or write an override that sets zram-size = 0.
if rpm -q zram-generator-defaults >/dev/null 2>&1; then
  log "removing zram-generator-defaults"
  dnf -y remove zram-generator-defaults >/dev/null
fi
# Also drop a config override in case zram-generator itself remains.
install -d /etc/systemd/zram-generator.conf.d
cat >/etc/systemd/zram-generator.conf.d/00-disable.conf <<'EOF'
[zram0]
zram-size = 0
EOF
# Stop any existing zram swap.
systemctl stop dev-zram0.swap 2>/dev/null || true
swapoff -a
sed -i.bak -E 's|^(\s*[^#].*\s+swap\s+.*)$|# \1|' /etc/fstab

log "loading kernel modules"
cat >/etc/modules-load.d/k8s.conf <<EOF
overlay
br_netfilter
EOF
modprobe overlay
modprobe br_netfilter

log "applying sysctls"
cat >/etc/sysctl.d/99-k8s.conf <<EOF
net.bridge.bridge-nf-call-iptables  = 1
net.bridge.bridge-nf-call-ip6tables = 1
net.ipv4.ip_forward                 = 1
# FUSE limits — rspacefs-mount needs to handle many concurrent opens.
fs.fuse.max_user_bgreq    = 4096
fs.fuse.max_user_congthresh = 4096
EOF
sysctl --system >/dev/null

log "SELinux to permissive (kubelet + crio compat; flip back to enforcing once SELinux policy is upstreamed)"
if command -v setenforce >/dev/null && [ "$(getenforce 2>/dev/null || echo Disabled)" = "Enforcing" ]; then
  setenforce 0
  sed -i.bak 's/^SELINUX=enforcing/SELINUX=permissive/' /etc/selinux/config 2>/dev/null || true
fi

log "opening firewall ports (firewalld if active)"
if systemctl is-active --quiet firewalld; then
  # API server, kubelet, CNI, cilium hubble, NodePort range.
  for p in 6443/tcp 10250/tcp 10255/tcp 2379-2380/tcp 30000-32767/tcp 4240/tcp 4244/tcp 8472/udp; do
    firewall-cmd --permanent --add-port="$p" >/dev/null
  done
  firewall-cmd --reload >/dev/null
else
  log "firewalld not active — skipping (kubeadm preflight will warn but proceed)"
fi

log "installing base deps"
dnf -y install \
  iproute-tc \
  ethtool \
  socat \
  conntrack-tools \
  iptables-nft \
  curl \
  jq \
  tar \
  >/dev/null

mark_done 01-prereqs
log "prereqs done"
