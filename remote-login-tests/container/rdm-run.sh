#!/bin/bash
# Inside the niri-rl container, as root: (re)start remote-dm as a transient system service, so it
# isn't part of a logind session (pam_systemd won't create sessions from inside one).
set -u
systemctl stop remote-dm 2>/dev/null
systemctl reset-failed remote-dm 2>/dev/null
# Leftover greeter sessions from earlier runs.
for user in $(getent passwd | cut -d: -f1 | grep '^rdm-greeter-'); do
  loginctl terminate-user "$user" 2>/dev/null
done
sleep 0.5
systemd-run --quiet --unit=remote-dm \
  --setenv=RUST_LOG="${RUST_LOG:-remote_dm=debug}" \
  /usr/local/bin/remote-dm "$@"
sleep 0.5
systemctl is-active remote-dm
