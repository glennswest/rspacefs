#!/usr/bin/env bash
# build-bin.sh — cross-compile rspacefs-mount and rspacefs for the install target.
# Run from the rspacefs repo root.

set -euo pipefail

ARCH="${ARCH:-x86_64}"  # set ARCH=aarch64 for arm64 hosts
TARGET="${ARCH}-unknown-linux-gnu"

cd "$(dirname "$0")/../../.."  # repo root

echo "==> building rspacefs-mount, rspacefs for ${TARGET}"
if ! rustup target list --installed | grep -q "^${TARGET}$"; then
  echo "==> adding rust target ${TARGET}"
  rustup target add "${TARGET}"
fi

# We pin musl-style static linking off for now (glibc is fine on Fedora 41+).
# Use cross if you hit linker issues — see docs/cross-build.md.
cargo build --release --target "${TARGET}" \
  -p rspacefs-fuse -p rspacefs-cli

OUT="target/${TARGET}/release"
echo "==> built:"
ls -la "${OUT}/rspacefs-mount" "${OUT}/rspacefs"

DEST="tests/k8s/single-node-install"
cp -f "${OUT}/rspacefs-mount" "${DEST}/rspacefs-mount"
cp -f "${OUT}/rspacefs"       "${DEST}/rspacefs"
echo "==> staged for scp:"
ls -la "${DEST}/rspacefs-mount" "${DEST}/rspacefs"

cat <<EOF

Next:
  scp -r tests/k8s/single-node-install fedora@<host>:~/k8s-install/
  ssh fedora@<host> "sudo ~/k8s-install/install-all.sh"
EOF
