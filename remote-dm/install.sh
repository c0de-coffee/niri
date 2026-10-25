#!/bin/sh
# Install remote-dm.
#
# usage: ./install.sh [--no-build]
#   DESTDIR      staging directory (for packaging); empty installs to the system
#   PREFIX       default /usr
#   SYSCONFDIR   default /etc
#
# Existing files in $SYSCONFDIR/remote-dm are kept; new versions are installed next to them as
# *.new. When installing to the system (no DESTDIR), also creates the greeter users and their
# homes, and reloads systemd and D-Bus. It doesn't enable anything; see the end of the output.
set -eu
cd "$(dirname "$0")"

PREFIX=${PREFIX:-/usr}
SYSCONFDIR=${SYSCONFDIR:-/etc}
D=${DESTDIR:-}

if [ "${1:-}" != --no-build ]; then
  cargo build --release --locked
fi

install_conf() { # src dest: don't overwrite local changes
  # cksum rather than cmp: it's in coreutils, cmp may not be installed.
  if [ -e "$D$2" ] && [ "$(cksum < "$1")" != "$(cksum < "$D$2")" ]; then
    install -Dm644 "$1" "$D$2.new"
    echo "kept $2, new version installed as $2.new"
  else
    install -Dm644 "$1" "$D$2"
  fi
}

install -Dm755 target/release/remote-dm "$D$PREFIX/bin/remote-dm"

unit=$(mktemp)
sed "s|@BINDIR@|$PREFIX/bin|" data/remote-dm.service.in > "$unit"
install -Dm644 "$unit" "$D$PREFIX/lib/systemd/system/remote-dm.service"
rm -f "$unit"
install -Dm644 data/sysusers.d/remote-dm.conf "$D$PREFIX/lib/sysusers.d/remote-dm.conf"
install -Dm644 data/tmpfiles.d/remote-dm.conf "$D$PREFIX/lib/tmpfiles.d/remote-dm.conf"
install -Dm644 data/org.gnome.DisplayManager.conf \
  "$D$PREFIX/share/dbus-1/system.d/org.gnome.DisplayManager.conf"

install_conf data/pam.d/remote-dm "$SYSCONFDIR/pam.d/remote-dm"
install_conf data/pam.d/remote-dm-greeter "$SYSCONFDIR/pam.d/remote-dm-greeter"
install_conf data/config.toml "$SYSCONFDIR/remote-dm/config.toml"
install_conf data/greeter/greeter.kdl "$SYSCONFDIR/remote-dm/greeter.kdl"
install_conf data/greeter/greeter-noctalia.kdl "$SYSCONFDIR/remote-dm/greeter-noctalia.kdl"

if [ -z "$D" ]; then
  systemd-sysusers remote-dm.conf
  systemd-tmpfiles --create remote-dm.conf
  systemctl daemon-reload
  busctl call org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus ReloadConfig
fi

cat <<'NEXT'

remote-dm installed. To use it:
  1. Install a greetd greeter (the default greeter.kdl starts regreet) and
     gnome-remote-desktop.
  2. Set up gnome-remote-desktop's system daemon (remote login), e.g.:
       grdctl --system rdp set-tls-cert /path/to/cert.crt
       grdctl --system rdp set-tls-key /path/to/cert.key
       grdctl --system rdp set-credentials USER PASSWORD   # RDP-level login
       grdctl --system rdp enable
  3. systemctl enable --now remote-dm
     (conflicts with gdm.service: both own org.gnome.DisplayManager)
NEXT
