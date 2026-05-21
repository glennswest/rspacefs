#!/usr/bin/env bash
# run-deep.sh — pull each deep-layer set through CRI-O and verify the
# resulting merged view via a pod.
#
# Each set is run twice:
#   1. As image:N pulled from the cluster's CRI-O storage (uses rspacefs).
#   2. (optional, --compare fuse) Pulled in a sibling fuse-overlayfs
#      containers-storage root for direct comparison.

set -euo pipefail

COMPARE=""
while [ $# -gt 0 ]; do
  case "$1" in
    --compare) COMPARE="$2"; shift 2 ;;
    *) shift ;;
  esac
done

REGISTRY="${REGISTRY:-localhost:5000}"
SETS=(100 130 150 200)
RUN_ID="$(date +%Y%m%d-%H%M%S)"
OUT="/tmp/rspacefs-deep-${RUN_ID}"
mkdir -p "$OUT"
echo "set,backend,seconds,layer_count_observed,result" >"$OUT/results.csv"

NS="rspacefs-deep"
kubectl create ns "$NS" --dry-run=client -o yaml | kubectl apply -f - >/dev/null

run_one() {
  local n="$1" backend="$2"
  local img="${REGISTRY}/rspacefs-deep:${n}"
  local pod="deep-${backend}-${n}"
  local start end dur observed phase
  start=$(date +%s.%N)
  kubectl run "$pod" -n "$NS" --image="$img" --restart=Never --image-pull-policy=Always \
    --command -- /bin/sh -c 'ls /data | wc -l; echo "shared:"; cat /etc/profile.d/shared.sh' >/dev/null 2>&1
  kubectl wait pod/"$pod" -n "$NS" --for=jsonpath='{.status.phase}'=Succeeded --timeout=300s >/dev/null 2>&1 || true
  end=$(date +%s.%N)
  dur=$(awk "BEGIN { printf \"%.2f\", $end - $start }")
  phase=$(kubectl get pod -n "$NS" "$pod" -o jsonpath='{.status.phase}' 2>/dev/null || echo Unknown)
  observed=$(kubectl logs -n "$NS" "$pod" 2>/dev/null | head -1 | tr -d '[:space:]')
  echo "${n},${backend},${dur},${observed:-0},${phase}" >>"$OUT/results.csv"
  kubectl delete pod -n "$NS" "$pod" --wait=false >/dev/null 2>&1 || true
  printf 'set=%s backend=%s dur=%ss observed=%s phase=%s\n' "$n" "$backend" "$dur" "$observed" "$phase"
}

for n in "${SETS[@]}"; do
  run_one "$n" rspacefs
done

if [ "$COMPARE" = "fuse" ]; then
  echo "==> NOTE: comparison runs need a separate node configured with fuse-overlayfs."
  echo "==> Either taint a second node and re-run there with --compare set, or run podman directly outside the cluster."
fi

echo
echo "==> results:"
column -ts, "$OUT/results.csv"
echo
echo "==> output dir: $OUT"
