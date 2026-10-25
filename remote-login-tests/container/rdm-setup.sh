#!/bin/bash
# Inside the niri-rl container, as root: install remote-dm from the host build (build it there
# first: cargo build --release in ~/niri/remote-dm), plus the test bits. Re-runnable.
set -euo pipefail

cd /opt/niri/remote-dm
./install.sh --no-build
systemctl enable --now remote-dm

# niri built on the host, under its installed name (it isn't packaged in the container).
ln -sf /opt/niri/target/debug/niri /usr/local/bin/niri

# A session file for regreet to list (remote-dm ignores the greeter's choice anyway).
mkdir -p /usr/share/wayland-sessions
printf '[Desktop Entry]\nName=Niri\nExec=niri-session\nType=Application\n' \
  > /usr/share/wayland-sessions/niri.desktop
