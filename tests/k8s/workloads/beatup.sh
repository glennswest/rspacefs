#!/usr/bin/env bash
# beatup.sh — pull a list of images, run short-lived pods from each,
# delete, repeat. Verify rspacefs-mount stays alive and doesn't leak.
#
# Stats captured to /tmp/rspacefs-beatup-<timestamp>/:
#   - timing per pull / per run
#   - rspacefs-mount RSS over time
#   - kubelet/crio error counts
#   - final cluster pod state
#
# Run on the single-node K8s host with kubectl on PATH.

set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
RUN_ID="$(date +%Y%m%d-%H%M%S)"
OUT="/tmp/rspacefs-beatup-${RUN_ID}"
mkdir -p "$OUT"

IMAGES_FILE="${IMAGES_FILE:-${HERE}/beatup-images.txt}"
PARALLEL="${PARALLEL:-4}"          # concurrent pod runs in phase 3
RUNS_PER_IMAGE="${RUNS_PER_IMAGE:-4}"  # how many sequential runs per image in phase 2
NS="${NS:-rspacefs-beatup}"

log() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*" | tee -a "$OUT/run.log"; }

mapfile -t IMAGES < <(grep -vE '^\s*(#|$)' "$IMAGES_FILE")
log "starting beatup with ${#IMAGES[@]} images, ${RUNS_PER_IMAGE} runs each, parallel=${PARALLEL}"
log "output dir: $OUT"

kubectl create ns "$NS" --dry-run=client -o yaml | kubectl apply -f - >/dev/null

# Periodic snapshot of rspacefs-mount RSS in background.
(
  while true; do
    ts=$(date +%s)
    pid=$(pgrep -f rspacefs-mount | head -1 || true)
    if [ -n "$pid" ]; then
      rss=$(awk '/VmRSS:/ {print $2}' /proc/"$pid"/status 2>/dev/null || echo 0)
      printf '%s\t%s\t%s\n' "$ts" "$pid" "$rss" >>"$OUT/rss.tsv"
    fi
    sleep 5
  done
) &
RSS_PID=$!
trap 'kill $RSS_PID 2>/dev/null || true' EXIT

# ─── PHASE 1: pull all images via crictl ─────────────────────────────
log "PHASE 1: pulling ${#IMAGES[@]} images"
PULL_CSV="$OUT/pull.csv"
echo "image,seconds,result" >"$PULL_CSV"
for img in "${IMAGES[@]}"; do
  start=$(date +%s.%N)
  if sudo crictl --runtime-endpoint unix:///var/run/crio/crio.sock pull "$img" >/dev/null 2>&1; then
    rc=ok
  else
    rc=fail
  fi
  end=$(date +%s.%N)
  dur=$(awk "BEGIN { printf \"%.2f\", $end - $start }")
  echo "$img,$dur,$rc" >>"$PULL_CSV"
  printf '.'
done
echo
log "PHASE 1 done — see $PULL_CSV"

# ─── PHASE 2: run sequential pods per image ──────────────────────────
log "PHASE 2: ${RUNS_PER_IMAGE} sequential runs per image (sequential phase)"
RUN_CSV="$OUT/run-sequential.csv"
echo "image,run,seconds,result" >"$RUN_CSV"
run_n=0
for img in "${IMAGES[@]}"; do
  for r in $(seq 1 "$RUNS_PER_IMAGE"); do
    name="beatup-seq-$run_n"
    run_n=$((run_n + 1))
    start=$(date +%s.%N)
    # `kubectl run --attach --rm --restart=Never` blocks on container
    # exit, prints its stdout, and cleans up the pod — one round-trip
    # to the apiserver instead of run+wait+delete. The wait-for-Ready
    # pattern was sampling a transient state the pod never sat in for a
    # /bin/sh -c true workload (Pending → Running → Succeeded faster
    # than kubectl could observe Ready), so every pod cost 15s of
    # timeout regardless of real work (refs #10).
    # `if` context so a single failed pod run (e.g. an image that won't
    # pull) is recorded as Failed and the run continues — NOT aborted by
    # `set -e`. PHASE 1 already tolerates pull failures this way; a bad
    # image ref must not kill the whole 200-pod beatup at pod #41.
    if kubectl run "$name" -n "$NS" --image="$img" --restart=Never --image-pull-policy=IfNotPresent \
        --attach --rm \
        --command -- /bin/sh -c "true" >/dev/null 2>&1; then
      phase=Succeeded
    else
      phase=Failed
    fi
    end=$(date +%s.%N)
    dur=$(awk "BEGIN { printf \"%.2f\", $end - $start }")
    echo "$img,$r,$dur,$phase" >>"$RUN_CSV"
  done
done
log "PHASE 2 done — sequential runs: $run_n total"

# ─── PHASE 3: parallel run storm ─────────────────────────────────────
log "PHASE 3: parallel storm (${PARALLEL} concurrent pods, all images, 2 rounds)"
PAR_CSV="$OUT/run-parallel.csv"
echo "image,round,seconds,result" >"$PAR_CSV"
for round in 1 2; do
  i=0
  for img in "${IMAGES[@]}"; do
    name="beatup-par-r${round}-${i}"
    i=$((i + 1))
    (
      start=$(date +%s.%N)
      # `--attach --rm` waits for exit + cleans up; one round-trip
      # instead of run+wait+delete (refs #10).
      # Same failure tolerance as PHASE 2 (also guards this subshell's
      # own `set -e` so a failed run still records a row).
      if kubectl run "$name" -n "$NS" --image="$img" --restart=Never --image-pull-policy=IfNotPresent \
          --attach --rm \
          --command -- /bin/sh -c "true" >/dev/null 2>&1; then
        phase=Succeeded
      else
        phase=Failed
      fi
      end=$(date +%s.%N)
      dur=$(awk "BEGIN { printf \"%.2f\", $end - $start }")
      echo "$img,$round,$dur,$phase" >>"$PAR_CSV"
    ) &
    # cap concurrency
    while [ "$(jobs -r | wc -l)" -ge "$PARALLEL" ]; do sleep 0.2; done
  done
  wait
done
log "PHASE 3 done"

# ─── PHASE 4: cleanup ────────────────────────────────────────────────
log "PHASE 4: cleanup"
kubectl delete pods -n "$NS" --all --wait=true --timeout=120s >/dev/null 2>&1 || true
# Optional: image GC. crictl exposes rmi --prune
sudo crictl --runtime-endpoint unix:///var/run/crio/crio.sock rmi --prune >/dev/null 2>&1 || true

# Final state snapshot.
kubectl get nodes -o wide >"$OUT/final-nodes.txt"
kubectl get pods -A -o wide >"$OUT/final-pods.txt"
sudo journalctl -u crio --since '30 min ago' | grep -iE 'error|fatal|panic' | tail -100 >"$OUT/crio-errors.log" || true
sudo journalctl -u kubelet --since '30 min ago' | grep -iE 'error|fatal|panic' | tail -100 >"$OUT/kubelet-errors.log" || true

cp /proc/self/mountinfo "$OUT/mountinfo.txt"

# rspacefs-mount summary.
{
  echo "=== rspacefs-mount processes at end ==="
  pgrep -af rspacefs-mount || echo '(none)'
  echo
  echo "=== final RSS samples (last 5) ==="
  tail -5 "$OUT/rss.tsv" 2>/dev/null || echo '(no samples)'
} >"$OUT/rspacefs-final.txt"

log "beatup complete — results in $OUT"
ls -la "$OUT"
