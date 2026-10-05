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

# 任何失败退出都必须先把刚装上的规则撤掉。exit 1 在这里是**故意**用来让
# openvpn 放弃这个节点的（见下），而重连时每次都从 openvpn 的地址池里取一个
# 不同的隧道 IP，所以 `ip rule add` 不会去重——一个抖动的节点每失败一次就漏一条
# 永久规则，RPDB 越滚越大，每次出站都要遍历它。
rollback() {
  ip rule del from "$LOCAL" lookup "$TABLE" 2>/dev/null || true
  ip route flush table "$TABLE" 2>/dev/null || true
}

# 点对点场景 via 网关常常失败，回退到 dev 路由；两条都失败就 exit 1，
# 让 openvpn 放弃这个节点 —— 否则会拿着宿主机的默认路由假装隧道通了，
# 出口 IP 与住宅/机房判定都会错。
if ! ip route replace default via "$GW" dev "$DEV" table "$TABLE" 2>/dev/null; then
  ip route replace default dev "$DEV" table "$TABLE" 2>/dev/null || { rollback; exit 1; }
fi
# 校验规则与路由真的生效
ip rule show | grep -q "from ${LOCAL} lookup ${TABLE}" || { rollback; exit 1; }
[ -n "$(ip route show table "$TABLE" 2>/dev/null)" ] || { rollback; exit 1; }

printf '%s' "$LOCAL" > "$IPFILE"
exit 0
