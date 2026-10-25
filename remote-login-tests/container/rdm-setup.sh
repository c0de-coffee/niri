#!/bin/bash
# Inside the niri-rl container, as root: install remote-dm's files (greeter user, PAM services,
# greeter config). Re-runnable.
set -euo pipefail
D=/opt/niri/remote-dm/data

# One greeter user per concurrent remote display.
for n in 1 2 3 4; do
  if ! id rdm-greeter-$n >/dev/null 2>&1; then
    useradd --system --create-home --home-dir /var/lib/rdm-greeter-$n --shell /usr/bin/nologin \
      --groups video,render rdm-greeter-$n
  fi
done
# The first version used a single user.
if id rdm-greeter >/dev/null 2>&1; then
  loginctl terminate-user rdm-greeter 2>/dev/null || true
  userdel -r rdm-greeter 2>/dev/null || true
fi

# Binaries built on the host, under their installed names.
ln -sf /opt/niri/remote-dm/target/debug/remote-dm /usr/local/bin/remote-dm
ln -sf /opt/niri/target/debug/niri /usr/local/bin/niri
install -m 644 $D/pam.d/remote-dm-greeter /etc/pam.d/remote-dm-greeter
install -m 644 $D/pam.d/remote-dm /etc/pam.d/remote-dm
install -D -m 644 $D/greeter/greeter.kdl /etc/remote-dm/greeter.kdl
ls -l /etc/pam.d/system-remote-login
id rdm-greeter-1
ls -l /usr/local/bin/remote-dm /usr/local/bin/niri
