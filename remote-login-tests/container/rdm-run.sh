#!/bin/bash
# Inside the niri-rl container, as root: restart the installed remote-dm service, ending the
# greeter sessions of earlier runs. (Install or update it with /opt/niri/remote-dm/install.sh.)
set -u
for user in $(getent passwd | cut -d: -f1 | grep '^rdm-greeter-'); do
  loginctl terminate-user "$user" 2>/dev/null
done
systemctl restart remote-dm
sleep 0.5
systemctl is-active remote-dm
