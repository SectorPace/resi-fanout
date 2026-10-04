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
# 点对点场景 via 网关常常失败，回退到 dev 路由；两条都失败就 exit 1，
# 让 openvpn 放弃这个节点 —— 否则会拿着宿主机的默认路由假装隧道通了，
# 出口 IP 与住宅/机房判定都会错。
if ! ip route replace default via "$GW" dev "$DEV" table "$TABLE" 2>/dev/null; then
  ip route replace default dev "$DEV" table "$TABLE" 2>/dev/null || exit 1
fi
# 校验规则与路由真的生效
ip rule show | grep -q "from ${LOCAL} lookup ${TABLE}" || exit 1
[ -n "$(ip route show table "$TABLE" 2>/dev/null)" ] || exit 1

printf '%s' "$LOCAL" > "$IPFILE"
exit 0
