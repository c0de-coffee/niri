#!/bin/bash
# Inside the niri-rl container, as root: configure and start gnome-remote-desktop's system daemon
# (remote login mode), and install remote-dm's D-Bus policy. Re-runnable.
#
# RDP: port 3393 (the host's own system grd has 3389), credentials rdp/rdp. These are the RDP
# layer's credentials; the user then logs in on the greeter.
set -euo pipefail

PORT=3393
STATE=/var/lib/gnome-remote-desktop

# remote-dm owns org.gnome.DisplayManager (GDM isn't installed here).
install -D -m 644 /opt/niri/remote-dm/data/org.gnome.DisplayManager.conf \
  /etc/dbus-1/system.d/org.gnome.DisplayManager.conf
busctl call org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus ReloadConfig

# TLS certificate for the system daemon.
if [ ! -e "$STATE/rdp-tls.crt" ]; then
  install -d -o gnome-remote-desktop -g gnome-remote-desktop "$STATE"
  openssl req -new -newkey rsa:2048 -days 365 -nodes -x509 -subj /CN=niri-rl \
    -keyout "$STATE/rdp-tls.key" -out "$STATE/rdp-tls.crt" 2>/dev/null
  chown gnome-remote-desktop: "$STATE/rdp-tls.key" "$STATE/rdp-tls.crt"
fi

grdctl --system rdp set-tls-cert "$STATE/rdp-tls.crt"
grdctl --system rdp set-tls-key "$STATE/rdp-tls.key"
grdctl --system rdp set-port "$PORT"
grdctl --system rdp disable-port-negotiation
grdctl --system rdp set-credentials rdp rdp
grdctl --system rdp enable
grdctl --system status
