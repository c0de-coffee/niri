# gnome-remote-desktop patches

Optional patches on top of gnome-remote-desktop 51.0 for remote login with niri. Remote login
works without them.

| Patch | |
|---|---|
| `0001-clipboard-rdp-Convert-text-with-WinPR-s-UTF-8-format.patch` | Non-ASCII text copied on the RDP client arrives as `\uXXXX` escapes with FreeRDP ≥ 3.32.2. Not upstream yet. |
| `0002-rdp-pipewire-stream-Tell-the-compositor-where-each-v.patch` | With several client monitors, niri doesn't know how the client arranges them or which one is primary: "monitor on the left/right" actions go the wrong way and windows open on the first monitor rather than the primary one. Adds each monitor's client position and primary flag next to its preferred scale. Optional; without it niri lays them out in creation order. Not upstream yet. |

On Arch, [`packaging/gnome-remote-desktop/build.sh`](../../packaging/gnome-remote-desktop/build.sh)
builds Arch's package with these patches applied, as pkgrel N.1 (so an Arch update to N+1
replaces it again; check whether the patches are still needed then):

```sh
packaging/gnome-remote-desktop/build.sh
sudo pacman -U ~/.cache/niri-pkgbuild/gnome-remote-desktop/gnome-remote-desktop-[0-9]*.pkg.tar.zst
```

It needs the package's build dependencies; `MAKEPKG_FLAGS=-s` lets makepkg install them.
