#!/usr/bin/env bash
# OpenVPN client down-hook: undo the source policy routing installed by
# vpn-up.sh and remove the published tunnel IP.

set -u

TABLE="${VPN_TABLE:-}"
IPFILE="${VPN_IPFILE:-}"
LOCAL="${ifconfig_local:-}"

[ -n "$TABLE" ] && {
  [ -n "$LOCAL" ] || LOCAL="$(ip -4 -o route show table "$TABLE" 2>/dev/null | head -1 | grep -o 'src [0-9.]*' | cut -d' ' -f2)"
  if [ -n "$LOCAL" ]; then
    # 循环删除重复项：反复 up/down 会累积完全相同的规则，单次 del 只删一条。
    # 有界，因为结束循环的唯一信号就是 del 的「找不到」退出码。与 warp-down.sh 一致。
    i=0
    while [ "${i}" -lt 8 ] && ip rule del from "$LOCAL" lookup "$TABLE" 2>/dev/null; do
      i=$((i + 1))
    done
  fi
  ip route flush table "$TABLE" 2>/dev/null || true
}
[ -n "$IPFILE" ] && rm -f "$IPFILE"

exit 0
