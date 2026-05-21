#!/usr/bin/env bash
# 01-prereqs.sh — kernel modules, sysctls, swap, SELinux, firewall.
# Idempotent.

source "$(dirname "$0")/00-vars.sh"

if is_done 01-prereqs; then
  log "prereqs already done, skipping (FORCE=1 to redo)"
  exit 0
fi

[ "$(id -u)" = "0" ] || die "must run as root"

log "disabling swap"
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
