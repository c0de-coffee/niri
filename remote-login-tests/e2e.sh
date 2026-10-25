#!/bin/bash
# Isolated end-to-end test: niri (nested) + grd user mode + xfreerdp3, on a private session bus.
T="$1"; DUR="${2:-20}"
rm -rf "$T/iso"; mkdir -p "$T/iso"/{config,data,keyring,cache}
export XDG_CONFIG_HOME="$T/iso/config" XDG_DATA_HOME="$T/iso/data" XDG_CACHE_HOME="$T/iso/cache"
export GSETTINGS_BACKEND=keyfile
mkdir -p "$T/iso/data/keyrings"; printf login > "$T/iso/data/keyrings/default"
eval "$(printf test | gnome-keyring-daemon --unlock --components=secrets --control-directory="$T/iso/keyring")"
export GNOME_KEYRING_CONTROL

openssl req -new -newkey rsa:2048 -days 7 -nodes -x509 -subj /CN=niri-test \
  -keyout "$T/iso/tls.key" -out "$T/iso/tls.crt" >/dev/null 2>&1
PORT=3399
# Write settings directly: `grdctl rdp enable` would also toggle the real systemd user unit.
mkdir -p "$XDG_CONFIG_HOME/glib-2.0/settings"
# GRD_MODE=user (default): screen sharing mode, mirrors the primary monitor (RecordMonitor).
# GRD_MODE=headless: always extends (RecordVirtual), credentials in a file.
GRD_MODE="${GRD_MODE:-user}"
if [ "$GRD_MODE" = headless ]; then
  GRD_ARGS=--headless
  cat > "$XDG_CONFIG_HOME/glib-2.0/settings/keyfile" <<KEYS
[org/gnome/desktop/remote-desktop/rdp]
tls-cert='$T/iso/tls.crt'
tls-key='$T/iso/tls.key'

[org/gnome/desktop/remote-desktop/rdp/headless]
enable=true
port=uint16 $PORT
negotiate-port=false
KEYS
  timeout 10 grdctl --headless rdp set-credentials test test || echo "[e2e] set-credentials failed"
else
  GRD_ARGS=
  cat > "$XDG_CONFIG_HOME/glib-2.0/settings/keyfile" <<KEYS
[org/gnome/desktop/remote-desktop/rdp]
enable=true
port=uint16 $PORT
negotiate-port=false
view-only=false
tls-cert='$T/iso/tls.crt'
tls-key='$T/iso/tls.key'
KEYS
  timeout 10 grdctl rdp set-credentials test test || echo "[e2e] set-credentials failed"
fi
grdctl $GRD_ARGS status 2>&1 | sed 's/^/[grdctl] /'

RUST_LOG="${NIRI_LOG:-niri=debug}" "$HOME/niri/target/debug/niri" $NIRI_ARGS -c "${NIRI_CONFIG:-$T/niri.kdl}" > "$T/niri.log" 2>&1 &
NIRI=$!
for i in $(seq 1 50); do busctl --user status org.gnome.Mutter.RemoteDesktop >/dev/null 2>&1 && break; sleep 0.2; done

LD_PRELOAD="$GRD_PRELOAD" G_MESSAGES_DEBUG=all /usr/lib/gnome-remote-desktop-daemon $GRD_ARGS > "$T/grd.log" 2>&1 &
GRD=$!
sleep 3
owner=$(ss -ltnpH "sport = :$PORT" | grep -o 'pid=[0-9]*' | head -1 | cut -d= -f2)
if [ -z "$owner" ] || [ "$owner" != "$GRD" ]; then
  echo "[e2e] port $PORT is owned by pid '${owner:-none}', not test grd $GRD; aborting"
else
timeout "$DUR" xfreerdp3 /v:127.0.0.1:$PORT /u:test /p:test /cert:ignore /size:1280x720 $RDP_ARGS > "$T/rdp.log" 2>&1
echo "[e2e] xfreerdp3 exit: $?"
fi
sleep 1
kill $GRD $NIRI 2>/dev/null; wait 2>/dev/null
# Kill everything attached to this private bus (keyring, gvfsd, prompters...).
for e in /proc/[0-9]*/environ; do
  pid=${e#/proc/}; pid=${pid%/environ}
  [ "$pid" = "$$" ] && continue
  { tr '\0' '\n' < "$e"; } 2>/dev/null | grep -qxF "DBUS_SESSION_BUS_ADDRESS=$DBUS_SESSION_BUS_ADDRESS" && kill "$pid" 2>/dev/null
done
