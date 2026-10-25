#!/bin/bash
# On the host: restart remote-dm in the container, connect to the container's system grd with
# FreeRDP for N seconds (default 25), then print the interesting logs from both sides.
# Logs: $OUT/rdp.log (client), $OUT/container.log (journal of remote-dm, grd, greeter sessions).
DUR=${1:-25}
OUT=${OUT:-${XDG_CACHE_HOME:-$HOME/.cache}/niri-rl-test}
mkdir -p "$OUT"
R=$HOME/.local/share/niri-rl/ssh

$R 'bash /opt/niri/remote-login-tests/container/rdm-run.sh' || exit 1
since=$($R 'date "+%Y-%m-%d %H:%M:%S"')
timeout "$DUR" xfreerdp3 /v:127.0.0.1:3393 /u:rdp /p:rdp /cert:ignore /size:1280x720 $RDP_ARGS \
  > "$OUT/rdp.log" 2>&1
echo "[connect] xfreerdp3 exit: $?"
$R "uids=\$(getent passwd | awk -F: '/^rdm-greeter-/ {printf \" + _UID=%s\", \$3}'); \
  journalctl --since '$since' --no-pager -o short-monotonic \
    _SYSTEMD_UNIT=remote-dm.service + _SYSTEMD_UNIT=gnome-remote-desktop.service \$uids" \
  | sed 's/\x1b\[[0-9;]*m//g' > "$OUT/container.log"
echo "[connect] logs in $OUT/rdp.log and $OUT/container.log"
