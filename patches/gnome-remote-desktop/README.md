# gnome-remote-desktop patches

Patches on top of gnome-remote-desktop 51.0 that remote login with niri needs.

| Patch | |
|---|---|
| `0001-clipboard-rdp-Convert-text-with-WinPR-s-UTF-8-format.patch` | Non-ASCII text copied on the RDP client arrives as `\uXXXX` escapes with FreeRDP ≥ 3.32.2. Not upstream yet. |

On Arch, `remote-login-tests/container/build-grd.sh` builds Arch's package with these patches
applied (as pkgrel N.1).
