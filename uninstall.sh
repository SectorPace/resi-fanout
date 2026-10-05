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

# 残留清理：进程被杀后，内核里的接口与策略路由不会自动消失，
# 卸载后还留着会让其它程序的出站被误路由
cleanup_runtime() {
  # WARP（WireGuard）接口
  command -v wg >/dev/null 2>&1 && wg show warp-rf >/dev/null 2>&1 && ip link del warp-rf 2>/dev/null || true
  command -v wg >/dev/null 2>&1 && wg show warp >/dev/null 2>&1 && ip link del warp 2>/dev/null || true
  # OpenVPN 隧道（resi-fanout 用的 tun*）
  for d in /sys/class/net/tun*; do
    [ -e "${d}" ] || continue
    n="$(basename "$d")"
    case "${n}" in
      tun*) ip link del "${n}" 2>/dev/null || true ;;
    esac
  done
  # 源策略路由表（VPN Gate/WARP/MASQUE 用 21000-22999 段）
  for t in $(seq 21000 22999); do
    ip rule del lookup "${t}" 2>/dev/null || true
    ip route flush table "${t}" 2>/dev/null || true
  done
}
command -v ip >/dev/null 2>&1 && cleanup_runtime || true

rm -rf "${PREFIX}"
# 不要在这里 `rm -rf /tmp/tmp.*`。`mktemp -d` 不带模板时的默认名字恰好就是
# /tmp/tmp.XXXXXXXXXX，所以那个 glob 匹配的是**本机上任何**并发进程的临时工作
# 目录——另一个安装器、pip/virtualenv 的暂存、tar 解包、systemd-tmpfiles、
# CI 步骤——以 root 身份无提示删除别人正在进行中的文件。
# 而且它本来也帮不上忙：uninstall 是独立进程，install.sh 自己的 TMPROOT 由
# 它开头的 `trap cleanup EXIT` 负责清理，这个 glob 既定位不到我们的目录，
# 也清理不了另一个进程里的那一个。

if [ "${PURGE}" = "1" ]; then
  rm -rf "${CONF_DIR}" "${DATA_DIR}"
  if command -v swapoff >/dev/null 2>&1; then
    swapoff /swapfile-resi 2>/dev/null || true
  fi
  rm -f /swapfile-resi
  id -u "${APP}" >/dev/null 2>&1 && userdel "${APP}" || true
  echo "resi-fanout fully removed"
else
  echo "resi-fanout stopped; config/data kept at ${CONF_DIR} and ${DATA_DIR}"
  echo "use --purge to remove them too"
fi
