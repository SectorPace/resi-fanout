#!/usr/bin/env bash
# WireGuard (wg-quick) PostUp hook for the WARP tunnel.
# wg-quick exports: dev, interface, address (CIDR list), table...
#
# Installs source policy routing so only sockets bound to the tunnel IP go
# through the tunnel; the host's default route stays untouched (the managed
# config sets `Table = off`).
#
# Table id = local fanout port, injected by resi-fanout via WARP_TABLE.

set -u

TABLE="${WARP_TABLE:-22000}"
LOCAL="${ifconfig_local:-}"
DEV="${dev:-}"
# wg-quick exports `address` (may be a comma separated CIDR list)
[ -z "${LOCAL}" ] && LOCAL="${address:-}"
[ -z "${LOCAL}" ] && LOCAL="$(ip -4 -o addr show dev "${INTERFACE:-${dev}}" 2>/dev/null | awk '{print $4}' | cut -d/ -f1 | head -1)"
LOCAL="${LOCAL%%/*}"

[ -n "${LOCAL}" ] && [ -n "${DEV}" ] || exit 0

# WireGuard interfaces are point-to-point: no gateway needed
ip rule add from "${LOCAL}" lookup "${TABLE}" 2>/dev/null || true
ip route replace default dev "${DEV}" table "${TABLE}" 2>/dev/null || true

printf '%s' "${LOCAL}" > "/var/lib/resi-fanout/warp/tun.ip" 2>/dev/null || true
exit 0