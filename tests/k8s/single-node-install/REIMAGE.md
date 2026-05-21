# Reimaging a Test Host to Fedora 42

This installer targets **Fedora 42** (kernel 6.11). Fedora 43 (kernel 6.17) is
unsupported — see `README.md` "Target" for the iptables-wrapper / nftables
regression that breaks kube-proxy and Cilium there.

If your test host is on a different release, reimage it before running
`install-all.sh`.

## Get the installer media

```bash
# Fedora 42 Cloud Edition (qcow2) — for cloud / KVM / Proxmox VMs
curl -LO https://download.fedoraproject.org/pub/fedora/linux/releases/42/Cloud/x86_64/images/Fedora-Cloud-Base-Generic-42-1.6.x86_64.qcow2

# Fedora 42 Server NetInstall (ISO) — for bare metal
curl -LO https://download.fedoraproject.org/pub/fedora/linux/releases/42/Server/x86_64/iso/Fedora-Server-netinst-x86_64-42-1.6.iso
```

(URLs may need bumping to the current 42.x.y point release.)

## Cloud-init for the qcow2 path

If you're booting the Cloud image (recommended for a quick spin), seed it
with cloud-init:

```yaml
#cloud-config
users:
  - name: fedora
    sudo: ALL=(ALL) NOPASSWD:ALL
    ssh_authorized_keys:
      - ssh-ed25519 AAAA...   # your key
ssh_pwauth: false
package_update: true
package_upgrade: false   # avoid pulling in F43 packages by mistake
runcmd:
  - hostnamectl set-hostname test1.g8.lo
```

## Verify after first boot

```bash
ssh fedora@test1.g8.lo "
  . /etc/os-release && echo \"distro: \$PRETTY_NAME\"
  uname -srm
"
# expected: distro: Fedora Linux 42 (...)
# expected: Linux 6.11.x-...fc42.x86_64 x86_64
```

If the kernel reports `fc42` and the OS reports Fedora 42, you're good.

## Then run the installer

```bash
# On your workstation (this repo):
rsync -az tests/k8s/single-node-install fedora@test1.g8.lo:~/k8s-install/
# Cross-build rspacefs-mount + rspacefs for x86_64 Linux, scp them in:
./tests/k8s/single-node-install/build-bin.sh
scp target/x86_64-unknown-linux-gnu/release/rspacefs-mount \
    target/x86_64-unknown-linux-gnu/release/rspacefs \
    fedora@test1.g8.lo:~/k8s-install/

# On the host:
ssh fedora@test1.g8.lo "sudo ~/k8s-install/install-all.sh"
```

## Don't `dnf system-upgrade` to F43

Once F42 is up, do NOT run `dnf system-upgrade` to F43. The kernel jump to
6.17 breaks the install — same root cause as the original swap from F43 → F42.
Pin the F42 release in `/etc/dnf/dnf.conf` if you're worried:

```ini
[main]
releasever=42
```
