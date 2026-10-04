#!/usr/bin/env bash
# resi-fanout uninstaller.
#   sudo bash uninstall.sh            # stop service, keep config & data
#   sudo bash uninstall.sh --purge    # also remove config & data

set -euo pipefail

APP="resi-fanout"
PREFIX="/opt/${APP}"
CONF_DIR="/etc/${APP}"
DATA_DIR="/var/lib/${APP}"
SERVICE="${APP}.service"
PURGE="0"

[ "${1:-}" = "--purge" ] && PURGE="1"
[ "$(id -u)" = "0" ] || { echo "please run as root" >&2; exit 1; }

systemctl disable --now "${SERVICE}" 2>/dev/null || true
systemctl disable --now resi-fanout-acme.timer 2>/dev/null || true
rm -f "/etc/systemd/system/${SERVICE}" \
      "/etc/systemd/system/resi-fanout-acme.service" \
      "/etc/systemd/system/resi-fanout-acme.timer" \
      "/usr/local/bin/rf"
systemctl daemon-reload 2>/dev/null || true

rm -rf "${PREFIX}"

if [ "${PURGE}" = "1" ]; then
  rm -rf "${CONF_DIR}" "${DATA_DIR}"
  id -u "${APP}" >/dev/null 2>&1 && userdel "${APP}" || true
  echo "resi-fanout fully removed"
else
  echo "resi-fanout stopped; config/data kept at ${CONF_DIR} and ${DATA_DIR}"
  echo "use --purge to remove them too"
fi
