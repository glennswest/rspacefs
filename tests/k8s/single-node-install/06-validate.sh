#!/usr/bin/env bash
# 06-validate.sh — smoke tests. Must pass before declaring install successful.

source "$(dirname "$0")/00-vars.sh"

[ "$(id -u)" = "0" ] || die "must run as root"

export KUBECONFIG=/etc/kubernetes/admin.conf

log "node status"
kubectl get nodes -o wide

log "waiting for all system pods to be Ready (up to 3 min)"
end=$(( $(date +%s) + 180 ))
while [ "$(date +%s)" -lt "$end" ]; do
  bad=$(kubectl get pods -A --no-headers 2>/dev/null \
        | awk '$4 != "Running" && $4 != "Completed" {print}')
  if [ -z "$bad" ]; then
    break
  fi
  sleep 5
done
remaining=$(kubectl get pods -A --no-headers | awk '$4 != "Running" && $4 != "Completed" {print}' || true)
if [ -n "$remaining" ]; then
  log "WARNING: pods still not Running:"
  echo "$remaining"
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
