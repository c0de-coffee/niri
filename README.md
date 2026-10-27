<div align="right">
  <strong>English</strong> | <a href="README.zh-CN.md">简体中文</a>
</div>

# niri with remote login

> [!NOTE]
> This is a fork of [niri-wm/niri](https://github.com/niri-wm/niri) by Ivan Molodetskikh (YaLTeR),
> maintained by [c0de-coffee](https://github.com/c0de-coffee). **For what niri is, its
> documentation and configuration, see the original repository:** https://github.com/niri-wm/niri.
> This README only covers what the fork adds.
>
> Report problems with remote login here, not to niri.
>
> This fork was built with Claude.

## What the fork adds

GNOME-style **remote login over RDP** for niri:

- Connect with a remote desktop client, get a login screen, log in with your system account, and
  land in **your own niri session**.
- Remote sessions are headless: they don't use or show up on the computer's own screens.
- Sessions keep running when you disconnect; connecting again brings you back to yours.
- Several users can be logged in remotely at once, each in their own session.
- The server side is the gnome-remote-desktop package from Arch's repositories; remote login
  doesn't need it modified. Two optional patches fix non-ASCII clipboard text and the
  multi-monitor layout (see [Known issues](#known-issues-and-limitations)).

It has two parts:

1. **niri** (the `remote-desktop` branch) adds:
   - mutter's `org.gnome.Mutter.RemoteDesktop`: keyboard and pointer input through libei,
     clipboard, Caps Lock and Num Lock state.
   - `org.gnome.Mutter.ScreenCast.RecordVirtual`: virtual outputs that follow the client's window
     size; one per monitor when the client uses several, at that monitor's scale.
   - `niri --headless`: renders on the GPU without any physical outputs.
   - Used locally, it behaves like regular niri.
2. **[remote-dm](remote-dm/README.md)**: a display manager just for remote logins. It stands in
   for GDM towards gnome-remote-desktop and for greetd towards greeters. How a connection goes,
   client setup and troubleshooting are in its README.

## Quick start (Arch Linux)

1. **Install the packages**: download `niri-remote-desktop` and `remote-dm` from
   [Releases](https://github.com/c0de-coffee/niri-remote/releases), then:

   ```sh
   sudo pacman -U niri-remote-desktop-*.pkg.tar.zst remote-dm-*.pkg.tar.zst
   ```

   `niri-remote-desktop` replaces the `niri` package. To build them yourself, use the PKGBUILDs in
   [`packaging/`](packaging/): `niri-remote-desktop` and `remote-dm` build the release,
   `*-git` the latest commit of the branch.

2. **Set it up**: set up gnome-remote-desktop and enable remote-dm as in
   [remote-dm's README](remote-dm/README.md#installing).

   If you manage your desktop with Nyxuri, my Nyxuri fork
   [c0de-coffee/Nyxuri-remote](https://github.com/c0de-coffee/Nyxuri-remote) can do it for you
   (the original Nyxuri can't): `nyxuri on remote-login` does the setup above and adds a settings
   panel to Noctalia.

3. **Connect**: see [Connecting](remote-dm/README.md#connecting). Windows' Remote Desktop
   Connection needs one extra line in its `.rdp` file, or it can't get through.

## Known issues and limitations

- **One user can't be logged in locally and remotely at the same time**: both desktops would
  fight over the user's session bus. When you log in remotely while logged in locally, the login
  screen asks after the password whether to log out the local session, and cancels the login
  otherwise. GNOME's login screen (since GNOME 46) also asks first, and only "Force Stop" logs the
  local session out.
- **Multiple monitors**: with a multi-monitor client, the remote desktop spans all of the client's
  monitors, each at the scale it has on the client (GNOME's remote login supports this too;
  gnome-remote-desktop allows up to 16). Unpatched gnome-remote-desktop doesn't tell the
  compositor how the client's monitors are arranged or which one is primary (GNOME has the same
  problem), so niri lays them out left to right in the order the client lists them. Then actions
  like "focus monitor left/right" may go the opposite way from what you see, and the login screen
  may open on a monitor other than the primary one. Moving the pointer between monitors isn't
  affected. The optional gnome-remote-desktop patch `0002` in
  [`patches/gnome-remote-desktop/`](patches/gnome-remote-desktop/) fixes this: niri then arranges
  the monitors as in the client's display settings and opens windows on its primary monitor.
- **Clipboard with non-ASCII text**: in text copied on the client, every character outside ASCII
  (Chinese, accented letters, emoji, …) shows up remotely as escapes like
  `\u6d4b\u8bd5`. That's a bug in gnome-remote-desktop 51 itself (showing up with FreeRDP 3.32 or
  later); it happens with GNOME too and has nothing to do with this fork. Remote login works
  without fixing it. To fix it, rebuild gnome-remote-desktop with the patches in
  [`patches/gnome-remote-desktop/`](patches/gnome-remote-desktop/):
  [`packaging/gnome-remote-desktop/build.sh`](packaging/gnome-remote-desktop/build.sh) does it on
  Arch (it applies both patches).
- Packages are for Arch Linux only.

## What else is in the repository

| Directory | |
|---|---|
| [`remote-dm/`](remote-dm/) | the remote login display manager |
| [`packaging/`](packaging/) | Arch PKGBUILDs |
| [`patches/`](patches/) | patches for gnome-remote-desktop |
| [`remote-login-tests/`](remote-login-tests/) | test scripts, including a systemd-nspawn test container |

## License and credits

GPL-3.0-or-later, like niri; remote-dm is GPL-3.0-only (it contains code from greetd).

Thanks to Ivan Molodetskikh (YaLTeR) and everyone who works on niri, which all of this builds on,
and to the projects it relies on: [Smithay](https://github.com/Smithay/smithay) (including its
libei support), [GNOME Remote Desktop](https://gitlab.gnome.org/GNOME/gnome-remote-desktop) and
[mutter](https://gitlab.gnome.org/GNOME/mutter), whose remote login this follows,
[greetd](https://git.sr.ht/~kennylevinsen/greetd), whose greeter protocol and PAM session code
remote-dm uses, and [FreeRDP](https://github.com/FreeRDP/FreeRDP).
