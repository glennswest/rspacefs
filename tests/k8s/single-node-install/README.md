# Single-Node Kubernetes Install — Production Quality

Idempotent installer for a single-node Kubernetes cluster with CRI-O and
rspacefs as the containers-storage `mount_program`. Designed so the same
logic can be patched into a Single-Node OpenShift (SNO) installer later.

## Target

- **Fedora 42** (kernel 6.11). This is the pinned baseline.
  - Fedora 43 (kernel 6.17) is NOT supported: kube-proxy's iptables wrapper
    and Cilium 1.16.x both hit a netfilter regression there and fail with
    "iptables is not available on this host" / "Unable to redirect iptables
    binaries". F42 / 6.11 is the last release where mainstream container
    tooling Just Works without per-component nftables conversion.
  - Fedora 41 (kernel 6.11/6.12) also works.
- Kubernetes latest stable (v1.32.x at time of writing)
- CRI-O 1.32 (matches kube minor)
- rspacefs-mount as the containers-storage mount_program — every CRI-O
  image pull lands as rspacefs lowerdirs
- Flannel CNI by default (small, production-grade, no kernel-version
  surprises). Set `CNI=cilium` on `05-cni.sh` to use Cilium instead
  once you're on a kernel/Cilium combo with the iptables issue fixed.

## Layout

| Script | Purpose |
|---|---|
| `00-vars.sh`           | Versions, paths, environment. Source from every other script. |
| `01-prereqs.sh`        | Swap off, kernel modules, sysctls, firewall, SELinux. |
| `02-rspacefs.sh`       | Install rspacefs-mount + rspacefs CLI to /usr/local/bin (must precede crio start). |
| `03-crio.sh`           | Install CRI-O, configure storage.conf with rspacefs mount_program. |
| `04-kubeadm.sh`        | Install kubelet/kubeadm/kubectl, `kubeadm init`, untaint control-plane. |
| `05-cni.sh`            | Install Cilium via the Cilium CLI. |
| `06-validate.sh`       | Smoke tests — all system pods Ready, simple workload runs. |
| `install-all.sh`       | Run 01..06 in order. Idempotent — safe to re-run. |
| `uninstall.sh`         | `kubeadm reset` + remove CRI-O + clean storage. Destructive. |

## Use

```bash
# Cross-compile rspacefs-mount for the target host first:
cd $REPO && ./tests/k8s/single-node-install/build-bin.sh

# Copy scripts + binaries to the target host:
scp -r tests/k8s/single-node-install fedora@<host>:~/k8s-install/
scp target/x86_64-unknown-linux-gnu/release/rspacefs-mount \
    target/x86_64-unknown-linux-gnu/release/rspacefs-ctl \
    fedora@<host>:~/k8s-install/

# Run the installer:
ssh fedora@<host> "sudo ~/k8s-install/install-all.sh"
```

When `install-all.sh` returns, the cluster is up. `kubectl get nodes`
shows the host Ready; `kubectl get pods -A` shows kube-system + cilium
all Running.

## How the SNO patch will reuse this

The SNO installer (`openshift-install agent create cluster`) drops a
self-contained Ignition image onto a node. The relevant patches are:

1. **machineconfig snippet** that places `/etc/containers/storage.conf`
   with `mount_program = /usr/local/bin/rspacefs-mount` (replicates
   `02-crio.sh` step 4).
2. **systemd unit** for `rspacefs-mount.service` if we ever want a global
   long-lived daemon (currently rspacefs-mount is invoked per-pull by
   CRI-O as the mount_program — no service needed).
3. **bootstrap file** that drops the `rspacefs-mount` binary at the
   correct path before kubelet starts pulling pause/etcd/etc.

All three are derivable from this directory's scripts.
