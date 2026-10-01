#!/usr/bin/env bash
# OpenVPN client down-hook: undo the source policy routing installed by
# vpn-up.sh and remove the published tunnel IP.

set -u

TABLE="${VPN_TABLE:-}"
IPFILE="${VPN_IPFILE:-}"
LOCAL="${ifconfig_local:-}"

[ -n "$TABLE" ] && {
  [ -n "$LOCAL" ] && ip rule del from "$LOCAL" lookup "$TABLE" 2>/dev/null || true
  ip route flush table "$TABLE" 2>/dev/null || true
}
[ -n "$IPFILE" ] && rm -f "$IPFILE"

exit 0
