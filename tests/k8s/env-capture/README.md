# Test Environment Capture + History

Every K8s test run on a node should drop a snapshot here so we can:

1. Tell which kernel, package set, and rspacefs binary produced a given result.
2. Reproduce the exact same environment on a fresh host with one script.
3. Compare runs over time (e.g. "did the regression appear between this kernel and that one?").

## Layout

```
tests/k8s/
├── env-capture/
│   ├── capture-env.sh       snapshot the current host
│   ├── recreate-env.sh      reinstall a captured snapshot on a fresh host
│   └── README.md            this file
└── runs/
    ├── HISTORY.md           one line per run, newest first
    └── <run-id>/
        ├── snapshot.md      human-readable env summary
        ├── snapshot.json    machine-readable env dump
        ├── packages.txt     full `rpm -qa` output
        ├── kernel.txt       /proc/version + lsmod summary
        ├── rspacefs-bin/    sha256sum + size of installed binaries
        ├── storage.conf     /etc/containers/storage.conf at capture time
        ├── crio.d/          /etc/crio/crio.conf.d/ snapshot
        ├── kubelet.conf     /var/lib/kubelet/config.yaml
        ├── kube-versions    kubelet/kubeadm/kubectl/crio/cilium/podman versions
        ├── git-commit       rspacefs source commit the binary was built from
        ├── kubectl-state/   `kubectl get` dumps for nodes/pods/svc at capture time
        └── test-results/    whatever the test driver wrote (CSVs, logs, etc.)
```

A `<run-id>` is `<YYYYMMDD-HHMMSS>-<hostname>-<short-purpose>`, e.g.
`20260521-2125Z-test1.g8.lo-bootstrap`, `20260521-2230Z-test1.g8.lo-beatup`.

## Use

```bash
# Snapshot right now (e.g. after `install-all.sh` succeeds):
sudo ./capture-env.sh --purpose bootstrap

# After a beatup or benchmark run, pass --results <dir> so its output
# is bundled into the snapshot:
sudo ./capture-env.sh --purpose beatup --results /tmp/rspacefs-beatup-...

# To reproduce a snapshot on a fresh host:
sudo ./recreate-env.sh /path/to/snapshot/
```

## Why this matters

When a regression shows up in the beatup or benchmark, the first questions
are always:

- What kernel did it happen on?
- Which rspacefs binary?
- Which CRI-O + kubelet versions?
- What did `storage.conf` look like?

Without a captured snapshot the answer is "I'll dig in the journal and
hope" — which doesn't scale. With snapshots stored alongside the test
results, every regression has a one-command bisection target: the latest
green snapshot vs. the first red one.

## How this couples to the installer

`install-all.sh` calls `capture-env.sh --purpose bootstrap` as its final
step. So every successful install produces a row in `HISTORY.md` with the
exact environment that came up green. Failed installs don't write a
snapshot, so HISTORY.md stays clean.

## Kernel sourcing

`snapshot.md` records:

- `/proc/version` — kernel build banner
- `uname -a`
- `rpm -q kernel-core` — Fedora package + repo
- `/etc/dnf/dnf.conf` — repo list at install time

If the test ever runs on a non-Fedora host or with a custom kernel, the
captured info still lets the next person see what was running.
