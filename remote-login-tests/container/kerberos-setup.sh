#!/bin/bash
# Inside the niri-rl container, as root: a throwaway Kerberos realm for testing grd's single
# sign-on (preauthenticated users skip the greeter). Re-runnable.
#
# - Realm RL.TEST, KDC on 127.0.0.1:18888 only (the container shares the host's network), run
#   as the transient unit rl-kdc
# - Principals: rltest (password rltest), TERMSRV/localhost and TERMSRV/127.0.0.1 for grd
# - grd: keytab /var/lib/gnome-remote-desktop/krb5.keytab, auth methods credentials + kerberos
# - Replaces the container's /etc/krb5.conf (the original is kept as /etc/krb5.conf.orig)
#
# On the host, use kerberos-connect.sh; it brings its own krb5.conf and credential cache.
#
# Undo: systemctl stop rl-kdc; rm -rf /var/lib/krb5kdc /var/lib/gnome-remote-desktop/krb5.keytab;
#       mv /etc/krb5.conf.orig /etc/krb5.conf; grdctl --system rdp set-auth-methods credentials
set -euo pipefail

REALM=RL.TEST
KDC=127.0.0.1:18888
DIR=/var/lib/krb5kdc
KEYTAB=/var/lib/gnome-remote-desktop/krb5.keytab
export KRB5_KDC_PROFILE=$DIR/kdc.conf

[ -e /etc/krb5.conf.orig ] || cp /etc/krb5.conf /etc/krb5.conf.orig
cat > /etc/krb5.conf <<EOF
[libdefaults]
	default_realm = $REALM
	dns_lookup_kdc = false
	dns_lookup_realm = false
	rdns = false
	dns_canonicalize_hostname = false

[realms]
	$REALM = {
		kdc = $KDC
	}
EOF

install -d -m 700 "$DIR"
cat > "$KRB5_KDC_PROFILE" <<EOF
[kdcdefaults]
	kdc_listen = $KDC
	kdc_tcp_listen = $KDC

[realms]
	$REALM = {
		database_name = $DIR/principal
		key_stash_file = $DIR/.k5.$REALM
	}
EOF

if [ ! -e "$DIR/principal" ]; then
  kdb5_util create -s -r "$REALM" -P "$(head -c 24 /dev/urandom | base64)" > /dev/null
fi

kadmin.local -q "addprinc -pw rltest rltest" > /dev/null 2>&1 || true
services=()
for host in localhost 127.0.0.1; do
  kadmin.local -q "addprinc -randkey TERMSRV/$host" > /dev/null 2>&1 || true
  services+=("TERMSRV/$host")
done
rm -f "$KEYTAB"
kadmin.local -q "ktadd -k $KEYTAB ${services[*]}" > /dev/null
chown gnome-remote-desktop: "$KEYTAB"
chmod 600 "$KEYTAB"

systemctl stop rl-kdc 2>/dev/null || true
systemctl reset-failed rl-kdc 2>/dev/null || true
systemd-run --unit rl-kdc -E KRB5_KDC_PROFILE="$KRB5_KDC_PROFILE" /usr/bin/krb5kdc -n > /dev/null

grdctl --system rdp set-kerberos-keytab "$KEYTAB"
grdctl --system rdp set-auth-methods credentials,kerberos
systemctl restart gnome-remote-desktop
klist -k "$KEYTAB"
echo "KDC: $(systemctl is-active rl-kdc)"
