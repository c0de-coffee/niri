#!/bin/bash
# On the host: build niri-remote-desktop-git and remote-dm-git from the current working tree
# (uncommitted changes included, via a throwaway clone with one temporary commit) and install
# them in the container with pacman -U.
#
# usage: install-packages.sh [niri|remote-dm|all]   (default all; niri takes a few minutes)
#
# Build dirs live under ~/.cache/niri-pkgbuild (niri's release build doesn't fit in /tmp);
# packages end up in $OUT. Nothing is written into the repository.
set -euo pipefail

WHAT=${1:-all}
REPO=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
OUT=${OUT:-${XDG_CACHE_HOME:-$HOME/.cache}/niri-pkgbuild/out}
WORK=${XDG_CACHE_HOME:-$HOME/.cache}/niri-pkgbuild
R=$HOME/.local/share/niri-rl/ssh

mkdir -p "$WORK" "$OUT"
snapshot=$WORK/snapshot
rm -rf "$snapshot"
git clone -q --branch "$(git -C "$REPO" branch --show-current)" "$REPO" "$snapshot"
git -C "$REPO" diff --binary HEAD | git -C "$snapshot" apply --allow-empty
git -C "$REPO" ls-files --others --exclude-standard -z \
  | (cd "$REPO" && tar --null -cf - -T -) | tar -xf - -C "$snapshot"
git -C "$snapshot" add -A
git -C "$snapshot" -c user.name=snapshot -c user.email=snapshot@invalid \
  commit -qm "snapshot of the working tree" --allow-empty

build() { # package dir
  local dir=$REPO/packaging/$1 pkgbuild
  # makepkg rewrites pkgver in the PKGBUILD; build from a copy.
  pkgbuild=$WORK/$1
  rm -rf "$pkgbuild"
  mkdir -p "$pkgbuild"
  cp "$dir/PKGBUILD" "$pkgbuild/"
  (cd "$pkgbuild" && NIRI_REPO=$snapshot NIRI_BRANCH=$(git -C "$snapshot" branch --show-current) \
    BUILDDIR=$WORK/build SRCDEST=$WORK/src PKGDEST=$OUT makepkg -fd --nocheck --noconfirm \
    > "$WORK/$1.log" 2>&1) || { tail -20 "$WORK/$1.log"; exit 1; }
  ls -t "$OUT"/"$1"-[0-9r]*-x86_64.pkg.tar.zst | head -1
}

pkgs=()
case $WHAT in
  niri) pkgs+=("$(build niri-remote-desktop-git)") ;;
  remote-dm) pkgs+=("$(build remote-dm-git)") ;;
  all) pkgs+=("$(build remote-dm-git)" "$(build niri-remote-desktop-git)") ;;
  *) echo "usage: $0 [niri|remote-dm|all]" >&2; exit 2 ;;
esac

remote=()
for pkg in "${pkgs[@]}"; do
  name=$(basename "$pkg")
  $R "mkdir -p /root/pkgs && cat > /root/pkgs/$name" < "$pkg"
  remote+=("/root/pkgs/$name")
done
$R "pacman -U --noconfirm ${remote[*]} | tail -3 && systemctl try-restart remote-dm"
echo "[install-packages] installed: ${pkgs[*]##*/}"
