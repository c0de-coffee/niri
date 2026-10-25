#!/bin/bash
# One-time setup of the "niri-rl" systemd-nspawn container for remote login testing.
#
# Run as root (sudo). Re-running skips steps that are already done.
#
# What it does:
# - installs arch-install-scripts on the host (for pacstrap)
# - installs a small Arch into /var/lib/machines/niri-rl, reusing the host package cache
# - container: own system bus and logind (so it can own org.gnome.DisplayManager without
#   touching the host's GDM), host network shared, GPU render node passed in (not the display
#   card), ~/niri bind-mounted read-only at /opt/niri
# - user namespace (PrivateUsers=pick): root in the container is unprivileged on the host
# - sshd in the container on 127.0.0.1:2222 only, key login only, with a key generated for this
#   (~/.local/share/niri-rl/id_ed25519 of the invoking user)
# - test user "rltest" (password "rltest") for remote login
#
# Undo: machinectl stop niri-rl; rm -rf /var/lib/machines/niri-rl \
#   /etc/systemd/nspawn/niri-rl.nspawn /etc/systemd/system/systemd-nspawn@niri-rl.service.d
set -euo pipefail

NAME=niri-rl
ROOT=/var/lib/machines/$NAME
SSH_PORT=2222

if [ "$(id -u)" -ne 0 ]; then
  echo "run with sudo" >&2
  exit 1
fi
USER_NAME=${SUDO_USER:?run with sudo from your normal user}
USER_HOME=$(getent passwd "$USER_NAME" | cut -d: -f6)
NIRI_SRC=$USER_HOME/niri
KEY_DIR=$USER_HOME/.local/share/niri-rl

PACKAGES=(
  base openssh
  # niri's runtime libraries (the binary itself comes from /opt/niri)
  cairo pango libinput libdisplay-info libxkbcommon xkeyboard-config seatd libwacom lua54
  mesa vulkan-radeon
  pipewire wireplumber
  gnome-remote-desktop fuse3 polkit
  # test clients
  mesa-utils gnome-text-editor ttf-dejavu
)

echo "== host: arch-install-scripts"
pacman -S --needed --noconfirm arch-install-scripts

if [ ! -e "$ROOT/etc/os-release" ]; then
  echo "== pacstrap into $ROOT"
  mkdir -p "$ROOT"
  pacstrap -c -K "$ROOT" "${PACKAGES[@]}"
else
  echo "== $ROOT already installed, skipping pacstrap"
fi

echo "== container config"
echo "$NAME" > "$ROOT/etc/hostname"

# sshd: loopback only, key only.
mkdir -p "$ROOT/etc/ssh/sshd_config.d"
cat > "$ROOT/etc/ssh/sshd_config.d/10-niri-rl.conf" <<EOF
ListenAddress 127.0.0.1
Port $SSH_PORT
PermitRootLogin prohibit-password
PasswordAuthentication no
KbdInteractiveAuthentication no
EOF
mkdir -p "$ROOT/etc/systemd/system/multi-user.target.wants"
ln -sf /usr/lib/systemd/system/sshd.service \
  "$ROOT/etc/systemd/system/multi-user.target.wants/sshd.service"

# Key for logging in, owned by the invoking user.
if [ ! -e "$KEY_DIR/id_ed25519" ]; then
  sudo -u "$USER_NAME" mkdir -p "$KEY_DIR"
  sudo -u "$USER_NAME" ssh-keygen -q -t ed25519 -N "" -C "niri-rl container" -f "$KEY_DIR/id_ed25519"
fi
install -d -m 700 "$ROOT/root/.ssh"
install -m 600 "$KEY_DIR/id_ed25519.pub" "$ROOT/root/.ssh/authorized_keys"

# Test user for remote login. Done from the host with --root: running these through
# systemd-nspawn gives them an interactive console, and chpasswd then waits on the terminal.
if ! grep -q '^rltest:' "$ROOT/etc/passwd"; then
  useradd --root "$ROOT" -m -G video,render rltest
fi
echo "rltest:rltest" | chpasswd --root "$ROOT"

mkdir -p /etc/systemd/nspawn
cat > /etc/systemd/nspawn/$NAME.nspawn <<EOF
[Exec]
Boot=yes
PrivateUsers=pick

[Files]
PrivateUsersOwnership=auto
Bind=/dev/dri/renderD128
BindReadOnly=$NIRI_SRC:/opt/niri

[Network]
# Share the host network: sshd listens on 127.0.0.1:$SSH_PORT only; the container's
# gnome-remote-desktop gets its own port (the host's system grd has 3389).
VirtualEthernet=no
EOF

# Allow the GPU render node in the container's device cgroup.
mkdir -p /etc/systemd/system/systemd-nspawn@$NAME.service.d
cat > /etc/systemd/system/systemd-nspawn@$NAME.service.d/gpu.conf <<EOF
[Service]
DeviceAllow=/dev/dri/renderD128 rw
EOF
systemctl daemon-reload

echo "== starting $NAME"
machinectl start "$NAME"

echo
echo "done. log in with:"
echo "  ssh -i $KEY_DIR/id_ed25519 -p $SSH_PORT root@127.0.0.1"
