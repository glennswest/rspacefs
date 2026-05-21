#!/usr/bin/env bash
# build-set.sh — assemble a deep-layer OCI image with N layers.
#
# Usage:
#   ./build-set.sh <N>           # uses local podman; pushes nothing
#   PUSH=registry/host:5000 ./build-set.sh <N>
#                                # builds and pushes to PUSH/rspacefs-deep:N
#
# Each layer commits exactly one tiny change: a unique file + an
# overwrite of /etc/profile.d/shared.sh. Every 10th layer also creates
# a whiteout of an older layer's file.

set -euo pipefail

N="${1:?usage: build-set.sh <layers>}"
case "$N" in 100|130|150|200) ;; *) echo "warning: nonstandard layer count $N (expected 100/130/150/200)" ;; esac

PUSH="${PUSH:-}"
TAG="rspacefs-deep:${N}"

# We use buildah because it lets us snapshot+commit per layer cheaply.
command -v buildah >/dev/null || { echo "ERROR: buildah is required (dnf install buildah)"; exit 2; }

echo "==> building $TAG (${N} layers)"
ctr=$(buildah from --quiet scratch)
# Bootstrap layer — a minimal /bin/sh + busybox so the image is runnable.
# We pull busybox once and copy in its rootfs so subsequent layers have a shell.
BUSYBOX_REF="docker.io/library/busybox:1.37"
busybox_ctr=$(buildah from --quiet "$BUSYBOX_REF")
busybox_mnt=$(buildah mount "$busybox_ctr")
buildah copy --quiet "$ctr" "$busybox_mnt/bin"      /bin
buildah copy --quiet "$ctr" "$busybox_mnt/usr/bin"  /usr/bin
buildah copy --quiet "$ctr" "$busybox_mnt/lib"      /lib 2>/dev/null || true
buildah copy --quiet "$ctr" "$busybox_mnt/lib64"    /lib64 2>/dev/null || true
buildah umount "$busybox_ctr" >/dev/null
buildah rm "$busybox_ctr" >/dev/null
buildah config --cmd '/bin/sh -c "ls /data | wc -l; cat /etc/profile.d/shared.sh"' "$ctr"
buildah commit --quiet "$ctr" "rspacefs-deep:bootstrap" >/dev/null
buildah rm "$ctr" >/dev/null

prev="rspacefs-deep:bootstrap"
for i in $(seq -w 1 "$N"); do
  ctr=$(buildah from --quiet "$prev")
  # Each layer adds a unique file.
  buildah run "$ctr" -- /bin/sh -c "mkdir -p /data && echo 'layer-${i}' > /data/layer-${i}.txt"
  # Each layer overwrites the shared file.
  buildah run "$ctr" -- /bin/sh -c "mkdir -p /etc/profile.d && echo 'wins-from-layer-${i}' > /etc/profile.d/shared.sh"
  # Every 10th layer: whiteout an older file (delete it).
  if [ $((10#$i % 10)) -eq 0 ] && [ "$((10#$i))" -gt 10 ]; then
    old=$(printf '%03d' $((10#$i - 10)))
    buildah run "$ctr" -- /bin/sh -c "rm -f /data/layer-${old}.txt" || true
  fi
  prev="rspacefs-deep:tmp-${i}"
  buildah commit --quiet "$ctr" "$prev" >/dev/null
  buildah rm "$ctr" >/dev/null
  # Progress indicator every 10 layers.
  if [ $((10#$i % 10)) -eq 0 ]; then printf '.'; fi
done
echo

# Final commit — tag as the canonical layered image.
buildah tag "$prev" "$TAG"
echo "==> built $TAG"

# Clean intermediate tmp-* tags (keep disk reasonable).
for i in $(seq -w 1 "$N"); do
  buildah rmi "rspacefs-deep:tmp-${i}" >/dev/null 2>&1 || true
done
buildah rmi "rspacefs-deep:bootstrap" >/dev/null 2>&1 || true

# Layer count check.
COUNT=$(buildah inspect --type image --format '{{len .Manifest.Layers}}' "$TAG" 2>/dev/null \
        || buildah inspect "$TAG" 2>/dev/null | python3 -c 'import json,sys; d=json.load(sys.stdin); print(len(d["OCIv1"]["rootfs"]["diff_ids"]))')
echo "==> $TAG has $COUNT layers (target ${N})"

if [ -n "$PUSH" ]; then
  full="${PUSH}/${TAG}"
  buildah tag "$TAG" "$full"
  buildah push --quiet "$full"
  echo "==> pushed $full"
fi
