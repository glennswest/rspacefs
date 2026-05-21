#!/usr/bin/env bash
# capture-env.sh — snapshot kernel + packages + binary versions + config
# of the current K8s test host into tests/k8s/runs/<id>/.
#
# Designed to be idempotent and forgiving: each section that fails leaves
# a stub but doesn't abort the rest. The snapshot is always usable.

set -uo pipefail

PURPOSE="generic"
RESULTS_DIR=""
ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"   # tests/k8s
RUNS_DIR="${ROOT_DIR}/runs"
HISTORY="${RUNS_DIR}/HISTORY.md"

while [ $# -gt 0 ]; do
  case "$1" in
    --purpose)  PURPOSE="$2"; shift 2 ;;
    --results)  RESULTS_DIR="$2"; shift 2 ;;
    --runs-dir) RUNS_DIR="$2"; HISTORY="${RUNS_DIR}/HISTORY.md"; shift 2 ;;
    -h|--help)
      cat <<EOF
usage: $0 [--purpose <label>] [--results <dir>] [--runs-dir <dir>]

Snapshot the current host into tests/k8s/runs/<id>/.
EOF
      exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

HOSTNAME_S="$(hostname -s 2>/dev/null || echo unknown)"
TS="$(date -u +%Y%m%d-%H%M%SZ)"
RUN_ID="${TS}-${HOSTNAME_S}-${PURPOSE}"
SNAP="${RUNS_DIR}/${RUN_ID}"

install -d "$SNAP/rspacefs-bin" "$SNAP/crio.d" "$SNAP/kubectl-state" "$SNAP/test-results"

cap() {
  local title="$1" out="$2"; shift 2
  echo "  capturing: $title → $out"
  if ! ( "$@" ) > "$SNAP/$out" 2> "$SNAP/${out}.err"; then
    echo "    (warning: command failed; see ${out}.err)"
  fi
  # Drop empty .err files
  [ -s "$SNAP/${out}.err" ] || rm -f "$SNAP/${out}.err"
}

echo "==> capturing env to ${SNAP}"

# Kernel
cap "uname"        "kernel.txt"     bash -c 'echo "--- uname -a ---"; uname -a;
                                              echo; echo "--- /proc/version ---"; cat /proc/version;
                                              echo; echo "--- lsmod (top 30 by refcount) ---"; lsmod | sort -k3 -nr | head -30'
cap "kernel rpm"   "kernel-rpm.txt" bash -c 'rpm -qi kernel-core 2>/dev/null; echo; rpm -qa "kernel*"'

# OS + repo info
cap "os-release"   "os-release.txt"   cat /etc/os-release
cap "dnf repos"    "dnf-repos.txt"    bash -c 'cat /etc/yum.repos.d/*.repo 2>/dev/null | sed "/^password/d;/^token/d"'

# Package list (full)
cap "rpm -qa"      "packages.txt"     rpm -qa --queryformat '%{NAME}-%{VERSION}-%{RELEASE}.%{ARCH}\n'

# Runtime + cluster components
cap "kubelet ver"  "kube-versions"    bash -c '
  set +e
  echo "--- kubelet ---"; kubelet --version 2>&1
  echo "--- kubeadm ---"; kubeadm version -o short 2>&1
  echo "--- kubectl ---"; kubectl version --client 2>&1 | head -5
  echo "--- crio ---";    crio --version 2>&1 | head -5
  echo "--- crun ---";    crun --version 2>&1 | head -3
  echo "--- conmon ---";  conmon --version 2>&1 | head -3
  echo "--- runc ---";    runc --version 2>&1 | head -3
  echo "--- cilium-cli ---"; cilium version 2>&1 | head -3
  echo "--- podman ---";  podman --version 2>&1
  echo "--- fuser ---";   ldconfig -p 2>&1 | grep -i libfuse | head -3
'

# rspacefs binaries
RSPACEFS_MOUNT="/usr/local/bin/rspacefs-mount"
RSPACEFS_CLI="/usr/local/bin/rspacefs"
if [ -x "$RSPACEFS_MOUNT" ]; then
  cp -p "$RSPACEFS_MOUNT" "$SNAP/rspacefs-bin/"
  (cd "$SNAP/rspacefs-bin" && sha256sum rspacefs-mount > rspacefs-mount.sha256)
  cap "rspacefs-mount --version" "rspacefs-bin/rspacefs-mount.version" "$RSPACEFS_MOUNT" --version
fi
if [ -x "$RSPACEFS_CLI" ]; then
  cp -p "$RSPACEFS_CLI" "$SNAP/rspacefs-bin/"
  (cd "$SNAP/rspacefs-bin" && sha256sum rspacefs > rspacefs.sha256)
  cap "rspacefs --version" "rspacefs-bin/rspacefs.version" "$RSPACEFS_CLI" --version
fi

# Source-of-binary
for src in "$HOME/rspacefs-src" "/root/rspacefs-src" "/opt/rspacefs-src"; do
  if [ -d "$src/.git" ]; then
    cap "rspacefs git commit" "git-commit" bash -c "
      cd '$src'
      echo '--- rev-parse HEAD ---'
      git rev-parse HEAD
      echo '--- describe ---'
      git describe --tags --always --dirty 2>/dev/null || true
      echo '--- status (porcelain) ---'
      git status --porcelain | head -20
      echo '--- log -3 ---'
      git log --oneline -3
    "
    break
  fi
done

# Container storage + CRI-O config
cap "storage.conf"  "storage.conf"      cat /etc/containers/storage.conf
cap "crio.conf"     "crio.conf"         cat /etc/crio/crio.conf
if [ -d /etc/crio/crio.conf.d ]; then
  for f in /etc/crio/crio.conf.d/*; do
    [ -f "$f" ] || continue
    cp -p "$f" "$SNAP/crio.d/"
  done
fi

# Kubelet config
cap "kubelet config" "kubelet.conf"     cat /var/lib/kubelet/config.yaml
cap "kubeadm-flags"  "kubeadm-flags"    cat /var/lib/kubelet/kubeadm-flags.env

# Static pod manifests (control plane spec)
if [ -d /etc/kubernetes/manifests ]; then
  install -d "$SNAP/manifests"
  cp -p /etc/kubernetes/manifests/* "$SNAP/manifests/" 2>/dev/null || true
fi

# cluster live state (best-effort — may be empty if cluster not up)
if [ -f /etc/kubernetes/admin.conf ]; then
  cap "nodes"     "kubectl-state/nodes.txt"     kubectl --kubeconfig=/etc/kubernetes/admin.conf get nodes -o wide
  cap "pods"      "kubectl-state/pods.txt"      kubectl --kubeconfig=/etc/kubernetes/admin.conf get pods -A -o wide
  cap "services"  "kubectl-state/services.txt"  kubectl --kubeconfig=/etc/kubernetes/admin.conf get svc -A
  cap "events"    "kubectl-state/events.txt"    kubectl --kubeconfig=/etc/kubernetes/admin.conf get events -A --sort-by=.lastTimestamp 2>/dev/null
  cap "crictl"    "kubectl-state/crictl-ps.txt" crictl --runtime-endpoint unix:///var/run/crio/crio.sock ps -a
  cap "crictl-pods" "kubectl-state/crictl-pods.txt" crictl --runtime-endpoint unix:///var/run/crio/crio.sock pods
fi

# Mounts (proves rspacefs is/was in use)
cap "fuse mounts"   "fuse-mounts.txt"   bash -c 'grep -E "rspacefs|fuse" /proc/mounts'
cap "rspacefs procs" "rspacefs-procs.txt" bash -c 'pgrep -af rspacefs-mount'

# Test results bundle, if any
if [ -n "$RESULTS_DIR" ] && [ -d "$RESULTS_DIR" ]; then
  echo "  bundling test results from $RESULTS_DIR"
  rsync -a "$RESULTS_DIR/" "$SNAP/test-results/" 2>/dev/null || cp -r "$RESULTS_DIR/." "$SNAP/test-results/"
fi

# ---- snapshot.md (human-readable summary) ----
SUMMARY="$SNAP/snapshot.md"
{
  echo "# Snapshot — ${RUN_ID}"
  echo
  echo "- **purpose:** ${PURPOSE}"
  echo "- **host:** $(hostname)"
  echo "- **captured:** $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "- **kernel:** $(uname -srm)"
  echo "- **kernel rpm:** $(rpm -q kernel-core 2>/dev/null || echo n/a)"
  os_id=$(. /etc/os-release; echo "$PRETTY_NAME")
  echo "- **os:** ${os_id}"
  echo "- **rspacefs-mount sha256:** $(awk '{print $1}' "$SNAP/rspacefs-bin/rspacefs-mount.sha256" 2>/dev/null || echo n/a)"
  if [ -f "$SNAP/git-commit" ]; then
    commit=$(awk 'NR==2{print; exit}' "$SNAP/git-commit")
    echo "- **rspacefs commit:** \`${commit}\`"
  fi
  echo "- **kubelet:** $(kubelet --version 2>/dev/null || echo n/a)"
  echo "- **crio:** $(crio --version 2>/dev/null | head -1 || echo n/a)"
  echo
  echo "## Reproduce"
  echo
  echo '```bash'
  echo "cd /path/to/rspacefs"
  echo "tests/k8s/env-capture/recreate-env.sh tests/k8s/runs/${RUN_ID}/"
  echo '```'
  echo
  echo "## Contents"
  echo
  echo '```text'
  (cd "$SNAP" && find . -maxdepth 2 -print | sort)
  echo '```'
} > "$SUMMARY"

# ---- snapshot.json (machine-readable) ----
{
  echo "{"
  echo "  \"run_id\": \"${RUN_ID}\","
  echo "  \"purpose\": \"${PURPOSE}\","
  echo "  \"host\": \"$(hostname)\","
  echo "  \"captured_at_unix\": $(date -u +%s),"
  echo "  \"captured_at_iso\": \"$(date -u +%Y-%m-%dT%H:%M:%SZ)\","
  echo "  \"kernel\": \"$(uname -srm)\","
  echo "  \"kernel_rpm\": \"$(rpm -q kernel-core 2>/dev/null || echo n/a)\","
  echo "  \"os\": \"$(. /etc/os-release; echo \"$PRETTY_NAME\")\","
  echo "  \"rspacefs_mount_sha256\": \"$(awk '{print $1}' "$SNAP/rspacefs-bin/rspacefs-mount.sha256" 2>/dev/null || echo n/a)\","
  echo "  \"kubelet_version\": \"$(kubelet --version 2>/dev/null | awk '{print $2}' || echo n/a)\","
  echo "  \"crio_version\": \"$(crio --version 2>/dev/null | head -1 | awk '{print $3}' || echo n/a)\""
  echo "}"
} > "$SNAP/snapshot.json"

# ---- HISTORY.md (append, newest first) ----
install -d "$RUNS_DIR"
[ -f "$HISTORY" ] || cat >"$HISTORY" <<'EOF'
# Test Run History

Newest first. Each line is one captured env snapshot. Click into `runs/<run-id>/snapshot.md` for the full env.

| When (UTC) | Host | Purpose | Kernel | rspacefs-mount sha256 (short) | kubelet | crio | Snapshot |
|---|---|---|---|---|---|---|---|
EOF

short_sha=$(awk '{print $1}' "$SNAP/rspacefs-bin/rspacefs-mount.sha256" 2>/dev/null | head -c 12 || echo n/a)
new_line="| $(date -u +%Y-%m-%dT%H:%M:%SZ) | ${HOSTNAME_S} | ${PURPOSE} | $(uname -r) | \`${short_sha}\` | $(kubelet --version 2>/dev/null | awk '{print $2}' || echo n/a) | $(crio --version 2>/dev/null | head -1 | awk '{print $3}' || echo n/a) | [\`${RUN_ID}\`](./${RUN_ID}/snapshot.md) |"

# Insert after the header (line 4 = the underline of the table)
python3 - "$HISTORY" "$new_line" <<'PY'
import sys
path, line = sys.argv[1], sys.argv[2]
with open(path) as f:
    lines = f.readlines()
# Find the table header underline (starts with "|---")
for i, L in enumerate(lines):
    if L.startswith("|---"):
        insert_at = i + 1
        break
else:
    insert_at = len(lines)
lines.insert(insert_at, line + "\n")
with open(path, "w") as f:
    f.writelines(lines)
PY

echo "==> snapshot ready: $SNAP"
echo "    summary:   $SUMMARY"
echo "    HISTORY:   $HISTORY (new row inserted)"
echo
echo "to reproduce on a fresh host:"
echo "    ./recreate-env.sh $SNAP"
