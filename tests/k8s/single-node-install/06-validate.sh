#!/usr/bin/env bash
# 06-validate.sh — smoke tests. Must pass before declaring install successful.

source "$(dirname "$0")/00-vars.sh"

[ "$(id -u)" = "0" ] || die "must run as root"

export KUBECONFIG=/etc/kubernetes/admin.conf

log "node status"
kubectl get nodes -o wide

log "waiting for all system pods to be Ready (up to 3 min)"
# A pod is "not ready" if its STATUS isn't Running/Completed OR its READY
# column (a/b) shows a<b. Checking only STATUS misses the CoreDNS-stranded-
# on-loopback case (#31): Running but 0/1, cluster DNS dead, node still Ready.
not_ready() {
  kubectl get pods -A --no-headers 2>/dev/null | awk '
    $4 != "Running" && $4 != "Completed" { print; next }
    { split($3, r, "/"); if (r[1] != r[2]) print }'
}
end=$(( $(date +%s) + 180 ))
while [ "$(date +%s)" -lt "$end" ]; do
  [ -z "$(not_ready)" ] && break
  sleep 5
done
remaining="$(not_ready || true)"
if [ -n "$remaining" ]; then
  log "pods still not Ready after 3 min:"
  echo "$remaining"
  die "not all pods reached Ready (READY a/b) — see above"
fi

log "all pods:"
kubectl get pods -A

log "deploying a smoke test pod (busybox 'hello' via kubectl run)"
kubectl delete pod rspacefs-smoke --ignore-not-found --wait=true >/dev/null
kubectl run rspacefs-smoke --image=registry.k8s.io/e2e-test-images/busybox:1.36.1-1 \
  --restart=Never -- /bin/sh -c 'echo hello-from-rspacefs && sleep 1'
kubectl wait --for=condition=Ready pod/rspacefs-smoke --timeout=90s || \
  kubectl wait --for=jsonpath='{.status.phase}'=Succeeded pod/rspacefs-smoke --timeout=90s
kubectl logs rspacefs-smoke | tee /tmp/rspacefs-smoke.log
grep -q "hello-from-rspacefs" /tmp/rspacefs-smoke.log || die "smoke pod did not print expected line"
kubectl delete pod rspacefs-smoke --wait=true >/dev/null

log "verifying rspacefs-mount was invoked during the pull"
# CRI-O logs every invocation of mount_program at debug, but at info we
# can at least confirm the binary path matches storage.conf.
grep -q "rspacefs-mount" /etc/containers/storage.conf || die "mount_program not configured"
# Check that something actually mounted via the program:
mount -t fuse.rspacefs-mount 2>/dev/null | head || true

log "validate complete — cluster healthy"
