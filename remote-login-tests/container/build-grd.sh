#!/bin/bash
# Inside the niri-rl container, as root: build and install gnome-remote-desktop with our patches
# (/opt/niri/patches/gnome-remote-desktop/) on top of Arch's package.
#
# The build itself is packaging/gnome-remote-desktop/build.sh, run as an unprivileged "builder"
# user; root only installs the dependencies and the built package.
set -euo pipefail

PATCHES=/opt/niri/patches/gnome-remote-desktop

pacman -S --needed --noconfirm base-devel git pacman-contrib \
  asciidoc ffnvcodec-headers glib2-devel meson vulkan-headers \
  cairo dconf freerdp fuse3 glib2 krb5 libdrm libei libepoxy libfdk-aac libnotify libpipewire \
  libsecret libva libvncserver libxkbcommon opus pipewire polkit systemd tpm2-tss \
  vulkan-icd-loader

id builder >/dev/null 2>&1 || useradd --create-home builder

# /opt/niri is read-only, so give the builder its own copy of the script and the patches.
rm -rf ~builder/niri-grd
mkdir -p ~builder/niri-grd/packaging ~builder/niri-grd/patches
cp -r /opt/niri/packaging/gnome-remote-desktop ~builder/niri-grd/packaging/
cp -r "$PATCHES" ~builder/niri-grd/patches/
chown -R builder: ~builder/niri-grd

runuser -u builder -- ~builder/niri-grd/packaging/gnome-remote-desktop/build.sh ~builder/grd-build

pacman -U --noconfirm ~builder/grd-build/gnome-remote-desktop-*.pkg.tar.zst
pacman -Q gnome-remote-desktop
