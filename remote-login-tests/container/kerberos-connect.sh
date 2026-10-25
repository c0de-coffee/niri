#!/bin/bash
# On the host: like rdm-connect.sh, but logs in with Kerberos (single sign-on) as rltest, so grd
# passes rltest to remote-dm as a preauthenticated user and the greeter is skipped. Needs
# kerberos-setup.sh run in the container first.
#
# Uses its own krb5.conf and credential cache under $OUT; the host's Kerberos setup isn't touched.
# Set RESTART=0 to keep remote-dm (and its sessions) running.
DUR=${1:-25}
OUT=${OUT:-/tmp/claude-1000/-home-coffee/f907843a-d0ec-4338-a9e1-d2bace3cef1f/scratchpad/test}
R=$HOME/.local/share/niri-rl/ssh

mkdir -p "$OUT"
$R 'systemctl -q is-active rl-kdc || bash /opt/niri/remote-login-tests/container/kerberos-setup.sh' > /dev/null || exit 1
export KRB5_CONFIG=$OUT/krb5.conf KRB5CCNAME=FILE:$OUT/krb5cc
cat > "$KRB5_CONFIG" <<'EOF'
[libdefaults]
	default_realm = RL.TEST
	dns_lookup_kdc = false
	dns_lookup_realm = false
	rdns = false
	dns_canonicalize_hostname = false
	# Otherwise "localhost" gets the first DNS search domain appended.
	qualify_shortname = ""

[realms]
	RL.TEST = {
		kdc = 127.0.0.1:18888
	}
EOF
echo rltest | kinit rltest@RL.TEST > /dev/null || exit 1

if [ "${RESTART:-1}" = 1 ]; then
  $R 'bash /opt/niri/remote-login-tests/container/rdm-run.sh' || exit 1
fi
since=$($R 'date "+%Y-%m-%d %H:%M:%S"')
timeout "$DUR" xfreerdp3 /v:localhost:3393 /u:rltest /d:RL.TEST /p:rltest \
  /auth-pkg-list:kerberos /cert:ignore /size:1280x720 $RDP_ARGS > "$OUT/rdp.log" 2>&1
echo "[connect] xfreerdp3 exit: $?"
klist | grep TERMSRV
$R "uids=\$(getent passwd | awk -F: '/^rdm-greeter-|^rltest:/ {printf \" + _UID=%s\", \$3}'); \
  journalctl --since '$since' --no-pager -o short-monotonic \
    _SYSTEMD_UNIT=remote-dm.service + _SYSTEMD_UNIT=gnome-remote-desktop.service \$uids" \
  | sed 's/\x1b\[[0-9;]*m//g' > "$OUT/container.log"
echo "[connect] logs in $OUT/rdp.log and $OUT/container.log"
