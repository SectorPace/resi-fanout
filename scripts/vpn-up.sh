#!/usr/bin/env bash
# OpenVPN client up-hook. OpenVPN calls this with its env vars set:
#   ifconfig_local, ifconfig_remote, route_vpn_gateway, dev
# resi-fanout injects via `setenv`:
#   VPN_TABLE  routing table id (= local fanout port)
#   VPN_IPFILE where to publish the tunnel IP
#
# Installs source policy routing: packets with the tunnel's source IP use
# this table, whose default route goes through the tunnel. The host's
# default route is untouched.

set -u

TABLE="${VPN_TABLE:-}"
IPFILE="${VPN_IPFILE:-}"
LOCAL="${ifconfig_local:-}"
DEV="${dev:-}"
GW="${route_vpn_gateway:-${ifconfig_remote:-}}"

[ -n "$TABLE" ] && [ -n "$IPFILE" ] && [ -n "$LOCAL" ] && [ -n "$DEV" ] && [ -n "$GW" ] || exit 0

ip rule add from "$LOCAL" lookup "$TABLE" 2>/dev/null || true
ip route replace default via "$GW" dev "$DEV" table "$TABLE" 2>/dev/null || true

printf '%s' "$LOCAL" > "$IPFILE"
exit 0
