# remote-dm

A display manager for remote logins only: RDP into a machine, get a login screen, log in, and land
in your own headless session. Several people can be logged in at once, and reconnecting brings you
back to your running session.

It's the part of GDM that gnome-remote-desktop's remote login mode needs, without GDM:

- Towards **gnome-remote-desktop**'s system daemon it implements GDM's remote display interface
  (`org.gnome.DisplayManager`: `RemoteDisplayFactory` and `RemoteDisplay` objects), so grd works
  unmodified.
- Towards **greeters** it speaks **greetd's IPC protocol**, so any greetd greeter can be the login
  screen (regreet by default).
- Sessions are seatless, remote logind sessions. By default both the greeter and the user session
  run [niri](https://github.com/YaLTeR/niri) headless; the compositor needs to implement mutter's
  `RemoteDesktop`/`ScreenCast` interfaces, including virtual outputs (`RecordVirtual`).

## How a connection goes

1. grd's system daemon accepts an RDP connection and calls `CreateRemoteDisplay`.
2. remote-dm starts a greeter session as a free greeter user (`rdm-greeter-N`), running headless
   niri, `grd --handover`, the greeter and `remote-dm session-manager`. grd hands the connection to
   that session's `grd --handover`.
3. The user logs in on the greeter. remote-dm authenticates with PAM and starts the user session
   (headless niri and `grd --handover`), or picks the user's existing one.
4. grd hands the connection over to the user session; the RDP client reconnects once. The greeter
   session ends.

Each handover makes the RDP client reconnect, following grd's server redirection. Windows' Remote
Desktop Connection (mstsc) only does that with `use redirection server name:i:1` in its `.rdp`
file. If a client doesn't come back within 30 seconds, grd drops it, and remote-dm ends the greeter
session that was started for it.

A user can't have a remote session while also logged in on the computer itself: one user has one
session bus, and the two desktops would fight over it. Like GDM, remote-dm asks on the greeter after
the password whether to log out the other session (anything unsaved there is lost), and cancels the
login otherwise.

With single sign-on, grd has already authenticated the user with Kerberos and passes the user name
along (`preauthenticated-user`). remote-dm then skips the greeter, like GDM: it starts the user
session right away with the `remote-dm-autologin` PAM service (no password), or picks the user's
existing one. If the user is logged in on the computer itself, the greeter is shown instead, to ask.

## Installing

On Arch, build the packages in `packaging/` of this repository (they build the committed
`remote-desktop` branch): `remote-dm-git`, and `niri-remote-desktop-git` for a niri with remote
desktop support.

```sh
cd packaging/remote-dm-git && makepkg -si
```

Otherwise:

```sh
./install.sh            # builds, then installs to /usr and /etc (as root)
DESTDIR=pkg ./install.sh  # for packaging
```

Then set up gnome-remote-desktop's system daemon and enable the service:

```sh
grdctl --system rdp set-tls-cert /path/to/cert.crt
grdctl --system rdp set-tls-key /path/to/cert.key
grdctl --system rdp set-credentials USER PASSWORD   # RDP-level login
grdctl --system rdp enable
systemctl enable --now remote-dm
```

`remote-dm.service` conflicts with `gdm.service`: both own `org.gnome.DisplayManager`.

Installed files:

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
| `/etc/remote-dm/greeter-noctalia.kdl` | the same with [Noctalia Greeter](https://github.com/noctalia-dev/noctalia-greeter) |

## Configuration

See `data/config.toml` for all options and their defaults.

Any greetd greeter that runs as a plain Wayland client works. The default greeter config starts
regreet; to use Noctalia Greeter instead (its client, without its own compositor):

```toml
[greeter]
command = "exec niri --headless --session -c /etc/remote-dm/greeter-noctalia.kdl"
```

Single sign-on needs a Kerberos realm and a keytab for grd's `TERMSRV/<host>` principal:

```sh
grdctl --system rdp set-kerberos-keytab /path/to/krb5.keytab
grdctl --system rdp set-auth-methods credentials,kerberos
```

To always show the greeter instead, set `preauthenticated-pam-service = ""` in `[session]`.

Each greeter user allows one connection at the login screen at a time (concurrent sessions of one
user would share its session bus); add more to `sysusers.d/remote-dm.conf` for more.

## License

GPL-3.0-only: `src/pam/` and `src/scrambler.rs` are copied from
[greetd](https://git.sr.ht/~kennylevinsen/greetd).
