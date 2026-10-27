<div align="right">
  <strong>English</strong> | <a href="README.zh-CN.md">简体中文</a>
</div>

# remote-dm

A display manager for remote logins only: RDP into a machine, get a login screen, log in, and land
in your own headless session. Several people can be logged in at once, and reconnecting brings you
back to your running session.

gnome-remote-desktop's (grd's) remote login mode is meant to work with GDM. remote-dm is the part
of GDM that grd needs, without GDM:

- Towards **grd's system daemon** it implements GDM's remote display interface
  (`org.gnome.DisplayManager`: `RemoteDisplayFactory` and `RemoteDisplay` objects), so grd works
  unmodified.
- Towards **greeters** it speaks **greetd's protocol**, so any greetd greeter can be the login
  screen (regreet by default; [Noctalia Greeter](https://github.com/noctalia-dev/noctalia-greeter)
  is supported too).
- Sessions are seatless, remote logind sessions. By default both the greeter and the user session
  run headless niri. The compositor needs to implement mutter's `RemoteDesktop`/`ScreenCast`
  interfaces, including virtual outputs (`RecordVirtual`), like [my fork of niri](../README.md).

## How a connection goes

```
RDP client ──① connects──▶ grd's system daemon (port 3389, checks the RDP username and password)
                             │
                             ▼
                         remote-dm ──② starts──▶ greeter session (as rdm-greeter-N)
                             │                    first redirect: the client reconnects here
                             │                    and sees the login screen
                             │ ③ system account password
                             ▼
                         user session (the user's own headless niri)
                         second redirect: the client reconnects here; the greeter session ends
```

1. grd's system daemon accepts an RDP connection and calls `CreateRemoteDisplay`.
2. remote-dm starts a greeter session as a free greeter user (`rdm-greeter-N`, a system user just
   for the login screen, see [Configuration](#configuration)), running headless
   niri, `grd --handover`, the greeter and `remote-dm session-manager`. grd hands the connection to
   that session.
3. The user logs in on the greeter. remote-dm authenticates with PAM and starts the user session
   (headless niri and `grd --handover`), or picks the user's existing one.
4. grd hands the connection over to the user session. The greeter session ends.

Each handover makes the RDP client reconnect, following grd's server redirection. Windows' Remote
Desktop Connection (mstsc) doesn't do that by default; see [Connecting](#connecting). If a client
doesn't come back within 30 seconds, grd drops it, and remote-dm ends the greeter session that was
started for it.

## Installing

### 1. Packages

On Arch Linux, the simplest way is to install the prebuilt packages from this repository's
[Releases](https://github.com/c0de-coffee/niri/releases) (together with a niri with remote desktop
support):

```sh
sudo pacman -U niri-remote-desktop-*.pkg.tar.zst remote-dm-*.pkg.tar.zst
```

Or build them with the PKGBUILDs in `packaging/` of this repository: `remote-dm` builds the release,
`remote-dm-git` the latest commit of the `remote-desktop` branch.

```sh
cd packaging/remote-dm && makepkg -si
```

Elsewhere:

```sh
./install.sh              # builds, then installs to /usr and /etc (as root)
DESTDIR=pkg ./install.sh  # for packaging
```

### 2. Setup

Give grd's system daemon a TLS certificate and RDP credentials, then enable the services:

```sh
sudo grdctl --system rdp set-tls-cert /path/to/cert.crt
sudo grdctl --system rdp set-tls-key /path/to/cert.key
sudo grdctl --system rdp set-credentials USER PASSWORD   # RDP-level login, see "Two passwords"
sudo grdctl --system rdp enable
sudo systemctl enable --now remote-dm
```

`remote-dm.service` conflicts with `gdm.service` (both own `org.gnome.DisplayManager`), so GDM has
to go. Local logins can use greetd or another display manager.

To use Noctalia Greeter as the login screen, put this in `/etc/remote-dm/config.toml`:

```toml
[greeter]
command = "exec niri --headless --session -c /etc/remote-dm/greeter-noctalia.kdl"
```

If you manage your niri desktop's configuration with Nyxuri, you can also use my Nyxuri fork,
[c0de-coffee/Nyxuri-remote](https://github.com/c0de-coffee/Nyxuri-remote) (upstream Nyxuri doesn't
have this): `nyxuri on remote-login` does all of the above, with Noctalia Greeter as the login
screen.

### Uninstalling

```sh
sudo systemctl disable --now remote-dm   # ends all remote sessions
sudo grdctl --system rdp disable
sudo systemctl enable gdm                # to go back to GDM
```

If Nyxuri set it up, `nyxuri off remote-login` puts everything back the way it was.

## Connecting

### Two passwords

You enter two different sets of credentials:

1. **The RDP username and password**, asked by the client when connecting. They only get you into
   grd and have nothing to do with system accounts. Set them with
   `grdctl --system rdp set-credentials`; with Nyxuri, you can also change them in Noctalia's
   Remote Login panel.
2. **Your system account's password**, on the login screen, as when logging in locally.

### Windows' Remote Desktop Connection (mstsc)

mstsc doesn't follow server redirection by default, so a plain connection gets stuck at the first
step or keeps disconnecting. Connect with an `.rdp` file instead:

1. Open Remote Desktop Connection, enter the computer, click "Show Options", and on the "General"
   tab click "Save As" to save an `.rdp` file.
2. Open the file in Notepad and add this line at the end:

   ```
   use redirection server name:i:1
   ```

3. From now on, connect by double-clicking the `.rdp` file. The first time, it warns about an
   untrusted certificate: compare the fingerprint (see [Troubleshooting](#certificate-warning))
   and connect.

**Multiple monitors**: to span all of the client's monitors, check "Use all my monitors for the
remote session" on the "Display" tab and connect full screen, or add this line to the `.rdp` file:

```
use multimon:i:1
```

Which monitor is on the left and which one is primary only follows Windows' display settings when
gnome-remote-desktop has our optional patch; see the
[known issues](../README.md#known-issues-and-limitations) in the fork's README.

### FreeRDP

FreeRDP follows redirection by default:

```sh
xfreerdp3 /v:HOST /u:RDP_USER /p:RDP_PASSWORD
# /multimon for multiple monitors, /f for full screen
```

`sdl-freerdp3` takes the same options.

### 1Remote

[1Remote](https://github.com/1Remote/1Remote) has to connect in mstsc.exe mode:

1. Turn on mstsc.exe mode in the connection's settings.
2. Add this to the additional `.rdp` settings:

   ```
   use redirection server name:i:1
   ```

   For multiple monitors, also add:

   ```
   use multimon:i:1
   ```

### Other clients

Remmina, Windows App on macOS and mobile clients are untested. A client has to support RDP server
redirection to get past the login screen.

### Disconnecting, reconnecting and logging out

- Closing the client only disconnects; the session keeps running. Connect again and log in as the
  same user on the login screen to get back to it.
- To end the session, log out in it: click your desktop's "Log out" (such as the one in Noctalia's
  session menu), or use niri's quit shortcut (`Mod+Shift+E` by default). You can also run
  `loginctl terminate-user USER` on the computer.

### When the user is logged in on the computer itself

A user can't have a local and a remote session at the same time: both desktops would fight over
the user's session bus, and the remote one ends up grey. GDM doesn't allow it either.

So when the user is logged in locally, the login screen asks after the password whether to log out
the local session. `yes` logs it out (**anything unsaved there is lost**) and continues to the
remote session; anything else cancels the login and leaves the local session alone.

## Kerberos single sign-on

With Kerberos, grd has already authenticated the user and passes the user name along
(`preauthenticated-user`). remote-dm then skips the greeter, like GDM: it starts the user session
right away with the `remote-dm-autologin` PAM service (no password), or picks the user's existing
one. If the user is logged in locally, the greeter is shown anyway, to ask.

It needs a Kerberos realm and a keytab for grd's `TERMSRV/<host>` principal:

```sh
sudo grdctl --system rdp set-kerberos-keytab /path/to/krb5.keytab
sudo grdctl --system rdp set-auth-methods credentials,kerberos
```

To always show the greeter, set `preauthenticated-pam-service = ""` in `[session]`.

## Configuration

See [`data/config.toml`](data/config.toml) (installed as `/etc/remote-dm/config.toml`) for all
options and their defaults; all of them are optional.

Greeter sessions don't run as your user but as dedicated system users (greeter users). Each
connection at the login screen takes a greeter user of its own (sessions of one user would share
its session bus and get in each other's way). 4 are created by default (`rdm-greeter-1` to
`rdm-greeter-4`), so **at most 4 connections can be at the login screen at once**; a 5th is
refused. Once someone logs in, their greeter session ends and the greeter user is free for the next
connection, so this doesn't limit how many remote users can be logged in at the same time. For more
greeter users, add them to `sysusers.d/remote-dm.conf`.

## Troubleshooting

The logs to look at first:

```sh
journalctl -b -u remote-dm               # remote-dm
journalctl -b -u gnome-remote-desktop    # grd's system daemon
journalctl -b _UID=$(id -u USER)         # niri, grd and the rest of that user's session
loginctl list-sessions                   # sessions; rdm-greeter-N ones are greeter sessions
```

**Connects, then disconnects right away or keeps reconnecting**: most likely the client doesn't
follow server redirection. mstsc needs `use redirection server name:i:1` in its `.rdp` file.

**Can't connect**:
- `ss -ltnp | grep 3389` should show gnome-remote-desktop listening.
- With a firewall, allow TCP port 3389.
- `systemctl status remote-dm gnome-remote-desktop`: both need to be running.

<a id="certificate-warning"></a>**Certificate warning**: the certificate is self-signed, so clients
warn the first time. `sudo grdctl --system status` shows the "TLS fingerprint" (with Nyxuri, so
does Noctalia's Remote Login panel); if it matches what the client shows, trust it.

**Grey or black screen after logging in**:
- Is the user logged in on the computer itself? See
  [above](#when-the-user-is-logged-in-on-the-computer-itself).
- Look for `gnome-remote-desktop-daemon` errors in the user's log, and whether niri started.

**A login screen stays around**: when a client doesn't come back after a redirect, grd drops it
after 30 seconds and remote-dm ends that greeter session.

**remote-dm doesn't start, with "Two services allocated for org.gnome.DisplayManager"**: GDM is
still running. Disable it and reboot.

**New fonts, apps etc. don't show up after reconnecting**: reconnecting brings you back to the
same session, whose programs keep running. For a fresh session, end the old one first with
`loginctl terminate-user USER`.

## Installed files

| File | |
|---|---|
| `/usr/bin/remote-dm` | the daemon; also `remote-dm session-manager` and `remote-dm session-worker` |
| `/usr/lib/systemd/system/remote-dm.service` | |
| `/usr/lib/sysusers.d/remote-dm.conf` | greeter users `rdm-greeter-1` … `-4` |
| `/usr/lib/tmpfiles.d/remote-dm.conf` | their home directories |
| `/usr/share/dbus-1/system.d/org.gnome.DisplayManager.conf` | only root may own the name, only grd's system user may call it |
| `/etc/pam.d/remote-dm`, `/etc/pam.d/remote-dm-greeter` | PAM services for user and greeter sessions |
| `/etc/pam.d/remote-dm-autologin` | PAM service for users grd already authenticated (single sign-on) |
| `/etc/remote-dm/config.toml` | configuration (all optional) |
| `/etc/remote-dm/greeter.kdl` | niri config of greeter sessions (regreet) |
| `/etc/remote-dm/greeter-noctalia.kdl` | the same with Noctalia Greeter |

## License

GPL-3.0-only: `src/pam/` and `src/scrambler.rs` are copied from
[greetd](https://git.sr.ht/~kennylevinsen/greetd).
