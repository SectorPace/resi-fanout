#!/usr/bin/env bash
# WireGuard (wg-quick) PostDown hook: undo the source policy route.
#
# wg-quick invokes this as:  <script> down <interface>
# and exports INTERFACE / CONFIG_FILE / TABLE / ADDRESSES. It does NOT export
# `address` or `ifconfig_local` (OpenVPN names), so the previous
# `LOCAL="${address%%,*}"` aborted this script under `set -u` before any
# cleanup ran, leaking an `ip rule` + populated routing table per tunnel.
#
# Everything here is best-effort: a failure to clean up must not turn into a
# non-zero exit, because this hook runs while wg-quick is already tearing the
# interface down.

set -u

TABLE="${WARP_TABLE:-22000}"
IFACE="${2:-${INTERFACE:-}}"
LOCAL=""

[ -n "${ADDRESSES:-}" ] && LOCAL="${ADDRESSES%%,*}"
LOCAL="${LOCAL%%/*}"
if [ -z "${LOCAL}" ] && [ -n "${IFACE}" ]; then
  LOCAL="$(ip -4 -o addr show dev "${IFACE}" 2>/dev/null | awk '{print $4}' | cut -d/ -f1 | head -1)"
fi

if [ -n "${LOCAL}" ]; then
  # Delete duplicates too: repeated up/down cycles with the same table id
  # otherwise accumulate identical rules. Bounded, because the exit status of
  # `ip rule del` (not-found) is the only thing that ends the loop.
  i=0
  while [ "${i}" -lt 8 ] && ip rule del from "${LOCAL}" lookup "${TABLE}" 2>/dev/null; do
    i=$((i + 1))
  done
fi

ip route flush table "${TABLE}" 2>/dev/null || true
rm -f "/var/lib/resi-fanout/warp/tun.ip" 2>/dev/null || true
exit 0