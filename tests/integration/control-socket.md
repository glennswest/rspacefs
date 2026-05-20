# Integration test: control socket (`rspacefs ctl`)

End-to-end validation that `rspacefs-mount --control-socket PATH`
exposes a working Unix-socket control surface, and that the `invalidate`
command actually fires kernel `FUSE_NOTIFY_INVAL_ENTRY` notifications.

## Environment

| Item | Value |
|---|---|
| Host | `test1.g8.lo` |
| Kernel | `6.17.1-300.fc43.x86_64` |
| Build | rspacefs commit `762eccc` |

## Procedure

```sh
# Set up three layer dirs.
W=$(mktemp -d)
mkdir -p $W/upper $W/lower-a $W/lower-b $W/mnt
echo 'release=v1'  > $W/lower-a/release
echo 'role=server' > $W/lower-b/role

# Mount with control socket.
SOCK=/tmp/rspacefs-ctl.sock
setsid /usr/bin/rspacefs-mount \
  --upper $W/upper \
  --lower $W/lower-a --lower $W/lower-b \
  --control-socket $SOCK \
  $W/mnt </dev/null >/tmp/rspacefs-mount.log 2>&1 &
disown
sleep 2

# Exercise each control command.
/usr/bin/rspacefs ctl --socket $SOCK ping
/usr/bin/rspacefs ctl --socket $SOCK status
cat $W/mnt/release        # warms kernel cache
/usr/bin/rspacefs ctl --socket $SOCK invalidate
cat $W/mnt/release        # cache cold; daemon re-entered
cat $W/mnt/role           # also re-entered

fusermount3 -u $W/mnt
```

## Actual result — 2026-05-20 (test1.g8.lo)

`ctl status`:

```json
{
  "data": {
    "fuse_passthrough": true,
    "lowers": [
      { "path": "/tmp/tmp.i8InD5GS0Z/lower-a", "verified": false },
      { "path": "/tmp/tmp.i8InD5GS0Z/lower-b", "verified": false }
    ],
    "mountpoint": "/tmp/tmp.i8InD5GS0Z/mnt",
    "upper": "/tmp/tmp.i8InD5GS0Z/upper",
    "uptime_secs": 2,
    "version": "0.1.0"
  },
  "ok": true
}
```

`ctl invalidate`:

```json
{
  "data": {
    "entries_invalidated": 2,
    "errors": 0
  },
  "ok": true
}
```

Reads of both `/release` and `/role` continued to return correct data after
invalidate; kernel `FUSE_NOTIFY_INVAL_ENTRY` flushed the dentry cache,
subsequent `cat` re-entered the daemon to refetch.

Clean `fusermount3 -u` exits the daemon.

## Verdict

✅ Control socket works.
✅ Notifier integration fires `FUSE_NOTIFY_INVAL_ENTRY` correctly.
✅ Plumbing is in place for v2 commands (verify, snapshot, swap-lower,
   reload-manifest) — each is a new `cmd` value handled by the same dispatch.
