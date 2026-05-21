#!/usr/bin/env bash
# 03-rspacefs.sh — install rspacefs-mount and rspacefs-ctl binaries.
# Expects binaries to be present in $RSPACEFS_INSTALL_DIR.
# Idempotent (re-installs if checksums differ).

source "$(dirname "$0")/00-vars.sh"

[ "$(id -u)" = "0" ] || die "must run as root"

SRC_MOUNT="${RSPACEFS_INSTALL_DIR}/rspacefs-mount"
SRC_CTL="${RSPACEFS_INSTALL_DIR}/rspacefs-ctl"

[ -f "$SRC_MOUNT" ] || die "rspacefs-mount not found at $SRC_MOUNT — run build-bin.sh first and scp the binaries"
[ -f "$SRC_CTL" ]   || die "rspacefs-ctl not found at $SRC_CTL — run build-bin.sh first and scp the binaries"

install_one() {
  local src="$1" dst="$2"
  if [ -f "$dst" ] && cmp -s "$src" "$dst"; then
    log "$(basename "$dst") already current"
    return 0
  fi
  install -m 0755 "$src" "$dst"
  log "installed $dst"
}

install -d "$RSPACEFS_BIN_DIR"
install_one "$SRC_MOUNT" "$RSPACEFS_MOUNT_BIN"
install_one "$SRC_CTL"   "$RSPACEFS_CTL_BIN"

# Sanity probe — binary should at least respond to --help without crashing.
"$RSPACEFS_MOUNT_BIN" --help >/dev/null 2>&1 || die "rspacefs-mount --help failed — wrong arch?"
"$RSPACEFS_CTL_BIN"   --help >/dev/null 2>&1 || die "rspacefs-ctl --help failed — wrong arch?"

# If CRI-O was already running, kick it so the new mount_program is picked up.
if systemctl is-active --quiet crio; then
  log "restarting crio to pick up rspacefs-mount"
  systemctl restart crio
fi

mark_done 03-rspacefs
log "rspacefs binaries installed"
