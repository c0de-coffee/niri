#!/bin/bash
# Inside the niri-rl container, as root: build and install gnome-remote-desktop with our patches
# (/opt/niri/patches/gnome-remote-desktop/) on top of Arch's package.
#
# Uses Arch's official packaging (pinned below), which builds the signed upstream tag. Our package
# gets pkgrel N.1, so it replaces Arch's N, and an Arch update to N+1 replaces ours again (check
# whether the patches are still needed then). makepkg runs as an unprivileged "builder" user; root
# only installs the dependencies and the built package.
set -euo pipefail

ARCH_PKG=https://gitlab.archlinux.org/archlinux/packaging/packages/gnome-remote-desktop.git
ARCH_COMMIT=9a0ef38 # 51.0-2
PATCHES=/opt/niri/patches/gnome-remote-desktop

pacman -S --needed --noconfirm base-devel git pacman-contrib \
  asciidoc ffnvcodec-headers glib2-devel meson vulkan-headers \
  cairo dconf freerdp fuse3 glib2 krb5 libdrm libei libepoxy libfdk-aac libnotify libpipewire \
  libsecret libva libvncserver libxkbcommon opus pipewire polkit systemd tpm2-tss \
  vulkan-icd-loader

id builder >/dev/null 2>&1 || useradd --create-home builder

rm -rf ~builder/grd-patches
cp -r "$PATCHES" ~builder/grd-patches
chown -R builder: ~builder/grd-patches

runuser -u builder -- env ARCH_PKG="$ARCH_PKG" ARCH_COMMIT="$ARCH_COMMIT" bash -euo pipefail -c '
  cd ~
  rm -rf gnome-remote-desktop
  git clone -q "$ARCH_PKG" gnome-remote-desktop
  cd gnome-remote-desktop
  git checkout -q "$ARCH_COMMIT"

  # The keys that sign the upstream tag.
  gpg --batch --quiet --import keys/pgp/*.asc

  cp ~/grd-patches/*.patch .
  python3 - <<EOF
import glob, os, re
patches = sorted(p for p in glob.glob("*.patch") if p != "0001-Lock-user-account.patch")
s = open("PKGBUILD").read()
s = re.sub(r"^pkgrel=(\d+)$", r"pkgrel=\1.1", s, count=1, flags=re.M)
s = s.replace("  0001-Lock-user-account.patch\n)",
              "  0001-Lock-user-account.patch\n" + "".join(f"  ours-{p}\n" for p in patches) + ")")
s = s.replace("  git apply -3 ../0001-Lock-user-account.patch\n",
              "  git apply -3 ../0001-Lock-user-account.patch\n"
              + "".join(f"  git apply -3 ../ours-{p}\n" for p in patches))
open("PKGBUILD", "w").write(s)
for p in patches:
    os.rename(p, "ours-" + p)
EOF
  updpkgsums
  makepkg --force --noconfirm
'

pacman -U --noconfirm ~builder/gnome-remote-desktop/gnome-remote-desktop-*.pkg.tar.zst
pacman -Q gnome-remote-desktop
