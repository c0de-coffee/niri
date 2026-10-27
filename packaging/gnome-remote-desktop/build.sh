#!/bin/bash
# Build Arch's gnome-remote-desktop package with our patches (patches/gnome-remote-desktop/).
#
# Usage: build.sh [WORKDIR]   (default: ~/.cache/niri-pkgbuild/gnome-remote-desktop)
# Run as a normal user; install the result with `sudo pacman -U WORKDIR/gnome-remote-desktop-*.pkg.tar.zst`.
# The build dependencies must be installed (MAKEPKG_FLAGS=-s lets makepkg install them with sudo).
#
# Uses Arch's official packaging (pinned below), which builds the signed upstream tag. Our package
# gets pkgrel N.1, so it replaces Arch's N, and an Arch update to N+1 replaces ours again (check
# whether the patches are still needed then). The keys that sign the upstream tag go into a
# GnuPG home inside WORKDIR, not your own keyring.
set -euo pipefail

ARCH_PKG=https://gitlab.archlinux.org/archlinux/packaging/packages/gnome-remote-desktop.git
ARCH_COMMIT=9a0ef38 # 51.0-2
PATCHES=$(cd "$(dirname "$0")/../../patches/gnome-remote-desktop" && pwd)
WORKDIR=${1:-$HOME/.cache/niri-pkgbuild/gnome-remote-desktop}

mkdir -p "$WORKDIR"
cd "$WORKDIR"
rm -rf src-pkg
git clone -q "$ARCH_PKG" src-pkg
cd src-pkg
git checkout -q "$ARCH_COMMIT"

export GNUPGHOME=$WORKDIR/gnupg
mkdir -p "$GNUPGHOME"
chmod 700 "$GNUPGHOME"
gpg --batch --quiet --import keys/pgp/*.asc

cp "$PATCHES"/*.patch .
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
# shellcheck disable=SC2086
makepkg --force --noconfirm ${MAKEPKG_FLAGS:-}

rm -f "$WORKDIR"/*.pkg.tar.zst
mv ./*.pkg.tar.zst "$WORKDIR"/
ls "$WORKDIR"/*.pkg.tar.zst
