#!/usr/bin/env bash
# bench-startup.sh — measure container-start latency through rspacefs.
#
# For each image:
#   1. Pull (cold) — discard time
#   2. Run+exit 5x — average "kubectl run" → Succeeded
#   3. Force re-pull (crictl rmi + pull) — measure cold-start path
#
# Output: /tmp/rspacefs-bench-startup-<timestamp>.csv

set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
RUN_ID="$(date +%Y%m%d-%H%M%S)"
OUT="/tmp/rspacefs-bench-startup-${RUN_ID}.csv"
NS="${NS:-rspacefs-bench}"

# A small but representative image set — avoid pull-time noise dominating.
IMAGES=(
  "docker.io/library/alpine:3.20"
  "docker.io/library/busybox:1.37"
  "docker.io/library/python:3.12-slim"
  "docker.io/library/node:22-alpine"
  "docker.io/library/redis:7-alpine"
  "docker.io/library/nginx:alpine"
  "registry.access.redhat.com/ubi9/ubi-minimal:latest"
  "gcr.io/distroless/static-debian12:nonroot"
)

ITERATIONS="${ITERATIONS:-5}"

echo "image,phase,iter,seconds,result" >"$OUT"
kubectl create ns "$NS" --dry-run=client -o yaml | kubectl apply -f - >/dev/null

# Warm — pull all once first.
for img in "${IMAGES[@]}"; do
  sudo crictl --runtime-endpoint unix:///var/run/crio/crio.sock pull "$img" >/dev/null 2>&1 || true
done

run_once() {
  local img="$1" name="$2"
  local start end
  start=$(date +%s.%N)
  kubectl run "$name" -n "$NS" --image="$img" --restart=Never --image-pull-policy=IfNotPresent \
    --command -- /bin/sh -c "true" >/dev/null 2>&1
  kubectl wait pod/"$name" -n "$NS" --for=jsonpath='{.status.phase}'=Succeeded --timeout=60s >/dev/null 2>&1 || true
  end=$(date +%s.%N)
  local phase
  phase=$(kubectl get pod -n "$NS" "$name" -o jsonpath='{.status.phase}' 2>/dev/null || echo Unknown)
  kubectl delete pod "$name" -n "$NS" --wait=false >/dev/null 2>&1 || true
  awk "BEGIN { printf \"%.3f\", $end - $start }"
  echo "$phase" >/tmp/__phase
}

i=0
for img in "${IMAGES[@]}"; do
  i=$((i+1))
  # WARM iterations — image already pulled.
  for it in $(seq 1 "$ITERATIONS"); do
    dur=$(run_once "$img" "bench-warm-${i}-${it}")
    phase=$(cat /tmp/__phase)
    echo "$img,warm,$it,$dur,$phase" >>"$OUT"
  done
  # COLD iteration — rm image, pull again, measure.
  sudo crictl --runtime-endpoint unix:///var/run/crio/crio.sock rmi "$img" >/dev/null 2>&1 || true
  dur=$(run_once "$img" "bench-cold-${i}")
  phase=$(cat /tmp/__phase)
  echo "$img,cold,1,$dur,$phase" >>"$OUT"
done

echo "=== summary (median seconds per phase) ==="
awk -F, 'NR>1 {key=$1"|"$2; v[key]=v[key]" "$4} END {
  for (k in v) {
    n=split(v[k], a, " "); asort(a);
    med = (n%2==1) ? a[(n+1)/2] : (a[n/2]+a[n/2+1])/2;
    printf "%-50s %s  median=%.3fs (n=%d)\n", k, "", med, n
  }
}' "$OUT" | sort

echo
echo "full CSV: $OUT"
