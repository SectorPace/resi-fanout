#!/usr/bin/env bash
# WireGuard (wg-quick) PostDown hook: undo the source policy route.

set -u

TABLE="${WARP_TABLE:-22000}"
LOCAL="${ifconfig_local:-}"
[ -z "${LOCAL}" ] && LOCAL="${address%%,*}"
LOCAL="${LOCAL%%/*}"

[ -n "${LOCAL}" ] && ip rule del from "${LOCAL}" lookup "${TABLE}" 2>/dev/null
ip route flush table "${TABLE}" 2>/dev/null
rm -f "/var/lib/resi-fanout/warp/tun.ip" 2>/dev/null
exit 0