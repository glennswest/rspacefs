#!/usr/bin/env bash
# 04-kubeadm.sh — install kubelet/kubeadm/kubectl, kubeadm init, untaint.
# Idempotent.

source "$(dirname "$0")/00-vars.sh"

if is_done 04-kubeadm; then
  log "kubeadm init already done, skipping (use uninstall.sh to redo)"
  exit 0
fi

[ "$(id -u)" = "0" ] || die "must run as root"

log "configuring Kubernetes ${K8S_MINOR} package repo"
cat >/etc/yum.repos.d/kubernetes.repo <<EOF
[kubernetes]
name=Kubernetes
baseurl=https://pkgs.k8s.io/core:/stable:/${K8S_MINOR}/rpm/
enabled=1
gpgcheck=1
gpgkey=https://pkgs.k8s.io/core:/stable:/${K8S_MINOR}/rpm/repodata/repomd.xml.key
exclude=kubelet kubeadm kubectl cri-tools kubernetes-cni
EOF

log "installing kubelet kubeadm kubectl"
dnf -y install --disableexcludes=kubernetes kubelet kubeadm kubectl >/dev/null

# Pin kubelet cgroup driver to systemd to match CRI-O.
log "configuring kubelet cgroup driver = systemd"
mkdir -p /etc/sysconfig
cat >/etc/sysconfig/kubelet <<EOF
KUBELET_EXTRA_ARGS="--cgroup-driver=systemd --container-runtime-endpoint=unix:///var/run/crio/crio.sock --runtime-request-timeout=15m"
EOF

systemctl enable kubelet

log "running kubeadm init (this can take ~60s while it pulls control-plane images)"
# --pod-network-cidr matches what we'll tell Cilium.
kubeadm init \
  --pod-network-cidr="${POD_CIDR}" \
  --service-cidr="${SERVICE_CIDR}" \
  --cri-socket=unix:///var/run/crio/crio.sock \
  --skip-phases=addon/kube-proxy \
  | tee /var/log/kubeadm-init.log

# Skipping kube-proxy because Cilium replaces it. If you want kube-proxy
# instead of Cilium's eBPF dataplane, drop the --skip-phases flag.

log "setting up kubeconfig for the invoking user"
INSTALL_USER="${SUDO_USER:-$(logname 2>/dev/null || echo root)}"
INSTALL_HOME=$(getent passwd "$INSTALL_USER" | cut -d: -f6)
mkdir -p "$INSTALL_HOME/.kube"
cp -f /etc/kubernetes/admin.conf "$INSTALL_HOME/.kube/config"
chown -R "$INSTALL_USER:$INSTALL_USER" "$INSTALL_HOME/.kube"
log "kubeconfig at $INSTALL_HOME/.kube/config (also /etc/kubernetes/admin.conf for root)"

# Root convenience kubeconfig.
mkdir -p /root/.kube
cp -f /etc/kubernetes/admin.conf /root/.kube/config

log "untainting control-plane (single-node)"
export KUBECONFIG=/etc/kubernetes/admin.conf
# Both taint keys for safety across versions.
kubectl taint nodes --all node-role.kubernetes.io/control-plane- 2>/dev/null || true
kubectl taint nodes --all node-role.kubernetes.io/master-        2>/dev/null || true

mark_done 04-kubeadm
log "kubeadm init complete"
