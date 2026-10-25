#!/bin/bash
# Inside the niri-rl container, as root: build and install noctalia-greeter from the AUR.
#
# This runs the AUR package's PKGBUILD (it downloads the v1.6.0 release from
# github.com/noctalia-dev/noctalia-greeter, checks its sha256 and builds it with meson) and its
# post-install script. makepkg runs as an unprivileged "builder" user; root only installs the
# dependencies and the built package.
set -euo pipefail

pacman -S --needed --noconfirm base-devel git \
  meson ninja nlohmann-json pkgconf stb wayland-protocols \
  cairo dbus fontconfig freetype2 glib2 greetd libglvnd librsvg libwebp libxkbcommon libxml2 \
  pango tomlplusplus wayland wlroots0.20

id builder >/dev/null 2>&1 || useradd --create-home builder

runuser -u builder -- bash -euo pipefail -c '
  cd ~
  rm -rf noctalia-greeter
  git clone --depth 1 https://aur.archlinux.org/noctalia-greeter.git
  cd noctalia-greeter
  makepkg --force --noconfirm
'

pacman -U --noconfirm ~builder/noctalia-greeter/noctalia-greeter-*.pkg.tar.zst
command -v noctalia-greeter
