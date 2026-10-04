#!/usr/bin/env bash
# WireGuard (wg-quick) PostUp hook for the WARP tunnel.
#
# wg-quick invokes this as:  <script> up <interface>
# and exports INTERFACE / CONFIG_FILE / TABLE / ADDRESSES (with %i expanded to
# the interface name). It does NOT export `dev`, `address` or `ifconfig_local`
# — those are OpenVPN script-env names — so every lookup below has a default
# and the interface is taken from $2/INTERFACE instead of `dev`.
#
# Installs source policy routing so only sockets bound to the tunnel IP go
# through the tunnel; the host's default route stays untouched (the managed
# config sets `Table = off`).
#
# Table id = local fanout port, injected by resi-fanout via WARP_TABLE.

set -u

TABLE="${WARP_TABLE:-22000}"
IFACE="${2:-${INTERFACE:-}}"
LOCAL=""

[ -n "${ADDRESSES:-}" ] && LOCAL="${ADDRESSES%%,*}"
LOCAL="${LOCAL%%/*}"
if [ -z "${LOCAL}" ] && [ -n "${IFACE}" ]; then
  LOCAL="$(ip -4 -o addr show dev "${IFACE}" 2>/dev/null | awk '{print $4}' | cut -d/ -f1 | head -1)"
fi

if [ -z "${IFACE}" ] || [ -z "${LOCAL}" ]; then
  echo "warp-up: cannot determine interface/address (iface='${IFACE}' local='${LOCAL}')" >&2
  exit 1
fi

# Fail loudly. Silently continuing here leaves a live-looking tunnel whose
# packets egress the host's default route instead of the tunnel, while the UI
# still reports the port as up.
ip rule del from "${LOCAL}" lookup "${TABLE}" 2>/dev/null || true
if ! ip rule add from "${LOCAL}" lookup "${TABLE}"; then
  echo "warp-up: failed to install ip rule from ${LOCAL} lookup ${TABLE}" >&2
  exit 1
fi
# WireGuard interfaces are point-to-point: no gateway needed
if ! ip route replace default dev "${IFACE}" table "${TABLE}"; then
  echo "warp-up: failed to install default route for ${IFACE} in table ${TABLE}" >&2
  ip rule del from "${LOCAL}" lookup "${TABLE}" 2>/dev/null || true
  exit 1
fi

# Purely informational (nothing reads it); never fatal.
printf '%s' "${LOCAL}" > "/var/lib/resi-fanout/warp/tun.ip" 2>/dev/null || true
exit 0