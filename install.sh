#!/usr/bin/env bash
# resi-fanout installer for Linux (Debian/Ubuntu/CentOS/Arch).
# Builds the Rust backend + TS frontend, installs a systemd service,
# and optionally pushes the fanout ports into a local 3x-ui panel.
#
#   sudo bash install.sh                     # default install
#   sudo bash install.sh --port 7654 --with-3xui
#   sudo bash install.sh --repo https://github.com/you/resi-fanout.git
#
# Reference project: https://github.com/byJoey/fanout

set -euo pipefail

APP="resi-fanout"
PREFIX="/opt/${APP}"
CONF_DIR="/etc/${APP}"
DATA_DIR="/var/lib/${APP}"
SERVICE="${APP}.service"
API_PORT="7654"
WITH_3XUI="0"
WITH_VPNGATE="0"
WITH_WARP="0"
WITH_MASQUE="0"
WITH_TLS="1"    # 默认公网+TLS；--no-tls 可关闭
REPO_URL="${REPO_URL:-https://github.com/SectorPace/resi-fanout.git}"
GH_REPO="${GH_REPO:-SectorPace/resi-fanout}"
NO_FRONTEND="0"
FROM_SOURCE="0"

log()  { printf '\033[1;34m[install]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[warn]\033[0m %s\n' "$*"; }
green() { printf '\033[1;32m[ok]\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31m[error]\033[0m %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --port)       API_PORT="${2:?}"; shift 2 ;;
    --repo)       REPO_URL="${2:?}"; GH_REPO="${2#*github.com/}"; GH_REPO="${GH_REPO%.git}"; shift 2 ;;
    --with-3xui)  WITH_3XUI="1"; shift ;;
    --with-vpngate) WITH_VPNGATE="1"; shift ;;
    --with-warp)   WITH_WARP="1"; shift ;;
    --with-masque) WITH_MASQUE="1"; shift ;;
    --with-tls)   WITH_TLS="1"; shift ;;   # 默认已开，保留兼容
    --no-tls)     WITH_TLS="0"; shift ;;   # 不签证书，仅本机 HTTP
    --from-source) FROM_SOURCE="1"; shift ;;
    --no-frontend) NO_FRONTEND="1"; shift ;;
    -h|--help)
      sed -n '2,12p' "$0"; exit 0 ;;
    *) die "unknown option: $1 (see --help)" ;;
  esac
done

[ "$(id -u)" = "0" ] || die "please run as root: sudo bash $0"

# When piped (curl ... | bash) BASH_SOURCE is "bash"; dirname then resolves
# to the current directory, so the local-tree checks below simply miss and
# the prebuilt-release / clone paths take over.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" 2>/dev/null && pwd)" || SCRIPT_DIR=""

# ---------------------------------------------------------------- source tree
# priority: local checkout > local release-tarball layout > GitHub release
# download (fast, no toolchain) > git clone + build (--from-source forces this)
SRC_DIR=""
PREBUILT="0"

if [ "${FROM_SOURCE}" != "1" ] && [ -n "${SCRIPT_DIR}" ] && [ -f "${SCRIPT_DIR}/backend/Cargo.toml" ] && [ -f "${SCRIPT_DIR}/frontend/package.json" ]; then
  SRC_DIR="${SCRIPT_DIR}"
  log "using source tree at ${SRC_DIR}"
elif [ "${FROM_SOURCE}" != "1" ] && [ -n "${SCRIPT_DIR}" ] && [ -f "${SCRIPT_DIR}/bin/${APP}" ]; then
  # release tarball layout: bin/ web/ scripts/ config.example.json
  SRC_DIR="${SCRIPT_DIR}"
  PREBUILT="1"
  log "using prebuilt binary from ${SRC_DIR}/bin/${APP} (skipping build)"
elif [ "${FROM_SOURCE}" != "1" ]; then
  case "$(uname -m)" in
    x86_64)          TGT="x86_64-unknown-linux-gnu" ;;
    aarch64|arm64)   TGT="aarch64-unknown-linux-gnu" ;;
    *)               TGT="" ;;
  esac
  TMP="$(mktemp -d)"
  if [ -n "${TGT}" ] && curl -fsSL "https://github.com/${GH_REPO}/releases/latest/download/resi-fanout-${TGT}.tar.gz" -o "${TMP}/app.tar.gz" 2>/dev/null; then
    log "downloaded prebuilt release for ${TGT} — installing (no toolchain needed)"
    tar xzf "${TMP}/app.tar.gz" -C "${TMP}"
    SRC_DIR="${TMP}/resi-fanout-${TGT}"
    PREBUILT="1"
  else
    warn "no prebuilt release for this machine — falling back to source build (installs Rust, takes a few minutes)"
    command -v git >/dev/null 2>&1 || install_pkgs git || die "git is required for source install"
    git clone --depth 1 "${REPO_URL}" "${TMP}/src" >&2
    SRC_DIR="${TMP}/src"
  fi
else
  [ -n "${REPO_URL}" ] || die "--from-source needs --repo <git-url> or a clone directory"
  if [ -f "${SCRIPT_DIR}/backend/Cargo.toml" ]; then
    SRC_DIR="${SCRIPT_DIR}"
  else
    TMP="$(mktemp -d)"
    git clone --depth 1 "${REPO_URL}" "${TMP}/src" >&2
    SRC_DIR="${TMP}/src"
  fi
  log "building from source at ${SRC_DIR}"
fi

# ---------------------------------------------------------------- system deps
PKG=""
for m in apt-get dnf yum pacman; do
  if command -v "$m" >/dev/null 2>&1; then PKG="$m"; break; fi
done

install_pkgs() {
  case "$PKG" in
    apt-get) DEBIAN_FRONTEND=noninteractive apt-get install -y "$@" ;;
    dnf)     dnf install -y "$@" ;;
    yum)     yum install -y "$@" ;;
    pacman)  pacman -S --noconfirm --needed "$@" ;;
    *)       warn "no known package manager; install manually: $*"; return 1 ;;
  esac
}

log "installing base packages (curl git ca-certificates python3 openvpn)"
{ apt-get update -y >/dev/null 2>&1 || true; } 2>/dev/null || true
install_pkgs curl git ca-certificates python3 openvpn iproute2 \
  || warn "some packages failed (VPN Gate tunnels need openvpn)"

if [ "${WITH_WARP}" = "1" ]; then
  log "installing wireguard-tools (Cloudflare WARP tunnel)"
  install_pkgs wireguard-tools iproute2 || warn "wireguard-tools install failed — WARP tunnel will not start"
  if ! command -v wgcf >/dev/null 2>&1; then
    log "downloading wgcf (official WARP profile generator)"
    ARCH_WG="$(uname -m)"
    case "${ARCH_WG}" in
      x86_64)        WGURL="https://github.com/ViRb3/wgcf/releases/latest/download/wgcf_2.2.22_linux_amd64" ;;
      aarch64|arm64) WGURL="https://github.com/ViRb3/wgcf/releases/latest/download/wgcf_2.2.22_linux_arm64" ;;
      *)             WGURL="" ;;
    esac
    if [ -n "${WGURL}" ] && curl -fsSL "${WGURL}" -o /usr/local/bin/wgcf; then
      chmod +x /usr/local/bin/wgcf
    else
      warn "wgcf download skipped — you can still paste a WireGuard config in the UI"
    fi
  fi
fi

# small VPS: make sure the Rust build doesn't OOM
mem_kb=$(grep MemTotal /proc/meminfo 2>/dev/null | awk '{print $2}' || echo 999999999)
if [ "${mem_kb:-999999999}" -lt 1000000 ] && ! swapon --show 2>/dev/null | grep -q .; then
  log "low memory detected, creating 2G swapfile for the build"
  if [ ! -f /swapfile-resi ]; then
    dd if=/dev/zero of=/swapfile-resi bs=1M count=2048 status=none
    chmod 600 /swapfile-resi
    mkswap /swapfile-resi >/dev/null
  fi
  swapon /swapfile-resi 2>/dev/null || true
fi

if [ "${WITH_MASQUE}" = "1" ]; then
  log "installing mihomo (native MASQUE support for Clash-style nodes)"
  ARCH_M="$(uname -m)"
  case "${ARCH_M}" in
    x86_64)        MURL="https://github.com/MetaCubeX/mihomo/releases/latest/download/mihomo-linux-amd64-v1.19.12.gz" ;;
    aarch64|arm64) MURL="https://github.com/MetaCubeX/mihomo/releases/latest/download/mihomo-linux-arm64-v1.19.12.gz" ;;
    *)             MURL="" ;;
  esac
  if [ -n "${MURL}" ] && curl -fsSL "${MURL}" | gzip -dc > /usr/local/bin/mihomo 2>/dev/null; then
    chmod +x /usr/local/bin/mihomo
  else
    warn "mihomo download failed — MASQUE nodes can still be imported, just run mihomo manually"
  fi
fi

# ---------------------------------------------------------------- ACME IP cert (default on)
# 默认签发 Let's Encrypt IP 证书并监听 0.0.0.0；任何一步失败都会
# 自动降级为仅本机 HTTP（--no-tls 可显式关闭）。
TLS_DIR="${CONF_DIR}/tls"
BASE_PATH=""

try_issue_cert() {
  PUBLIC_IP="${ACME_IP:-$(curl -fsS --max-time 10 https://api.ipify.org 2>/dev/null || curl -fsS --max-time 10 https://ifconfig.me/ip 2>/dev/null || true)}"
  if [ -z "${PUBLIC_IP}" ]; then
    warn "无法探测公网 IP（可用 ACME_IP=<你的IP> 指定）—— 降级为仅本机 HTTP"
    return 1
  fi

  if ! command -v lego >/dev/null 2>&1; then
    log "安装 lego（ACME 客户端，支持 RFC 8738 IP 证书）"
    case "$(uname -m)" in
      x86_64)        LEGO_URL="https://github.com/go-acme/lego/releases/download/v5.5.2/lego_v5.5.2_linux_amd64.tar.gz" ;;
      aarch64|arm64) LEGO_URL="https://github.com/go-acme/lego/releases/download/v5.5.2/lego_v5.5.2_linux_arm64.tar.gz" ;;
      *) warn "该架构没有 lego 预编译包"; return 1 ;;
    esac
    T="$(mktemp -d)"
    if curl -fsSL "${LEGO_URL}" | tar xz -C "${T}" && [ -f "${T}/lego" ]; then
      install -m 755 "${T}/lego" /usr/local/bin/lego
    else
      warn "lego 下载失败（404/网络）。手动安装：
      curl -fsSL ${LEGO_URL} | tar xz -C /usr/local/bin && chmod +x /usr/local/bin/lego"
      return 1
    fi
  fi

  mkdir -p "${TLS_DIR}"
  # lego v5：旗标放在 run 子命令之后；--server 支持 letsencrypt 短代码；
  # run 兼具续期（--renew-days 默认按证书生命周期的 1/3 自动判断）
  ACME_ARGS="--accept-tos --server letsencrypt --profile shortlived --http --path ${TLS_DIR} --domains ${PUBLIC_IP} --renew-days 2"
  [ -n "${ACME_EMAIL:-}" ] && ACME_ARGS="--email ${ACME_EMAIL} ${ACME_ARGS}"

  log "为 ${PUBLIC_IP} 申请证书（HTTP-01 需要 80 端口可从公网访问）"
  if ! lego run ${ACME_ARGS}; then
    warn "shortlived profile 申请失败，改用默认 profile 重试"
    lego run --accept-tos --server letsencrypt --http --path ${TLS_DIR} --domains ${PUBLIC_IP} --renew-days 2       || { warn "证书申请失败：80 端口需可从公网访问（被占用就停掉占用者，或改用 DNS-01）—— 降级为仅本机 HTTP"; return 1; }
  fi

  # lego 各版本落盘位置不同（v4 顶层 <ip>.crt/<ip>.key，v5 可能带子目录
  # 或只输出合并的 .pem），所以递归查找并按内容校验
  CRT=""
  KEY=""
  for f in $(find "${TLS_DIR}" -type f \( -name '*.crt' -o -name '*.pem' \) 2>/dev/null); do
    if grep -q "BEGIN CERTIFICATE" "$f" 2>/dev/null; then CRT="$f"; break; fi
  done
  for f in $(find "${TLS_DIR}" -type f -name '*.key' 2>/dev/null); do
    if grep -q "PRIVATE KEY" "$f" 2>/dev/null; then KEY="$f"; break; fi
  done
  if [ -n "${CRT}" ] && [ -z "${KEY}" ] && command -v openssl >/dev/null 2>&1; then
    # 只有合并 pem：从里面拆出私钥
    openssl pkey -in "${CRT}" -out "${TLS_DIR}/privkey.pem" >/dev/null 2>&1 || true
    [ -s "${TLS_DIR}/privkey.pem" ] && KEY="${TLS_DIR}/privkey.pem"
  fi
  if [ -z "${CRT}" ] || [ -z "${KEY}" ]; then
    warn "未在 ${TLS_DIR} 找到可用的证书/私钥（lego 输出: $(ls -R "${TLS_DIR}" 2>/dev/null | tr '\n' ' ' | cut -c1-160)）—— 降级为仅本机 HTTP"
    return 1
  fi
  cp -f "${CRT}" "${TLS_DIR}/fullchain.pem"
  cp -f "${KEY}" "${TLS_DIR}/privkey.pem"
  chmod 600 "${TLS_DIR}/privkey.pem"
  green "证书就绪：${CRT##*/} + ${KEY##*/}"
  return 0
}

if [ "${WITH_TLS}" = "1" ]; then
  log "准备 ACME IP 证书（Let's Encrypt，6 天证书自动续期；失败会降级为仅本机 HTTP）"
  if try_issue_cert; then
    BASE_PATH="/$(head -c 8 /dev/urandom | od -An -tx1 | tr -d ' \n')"
    TLS_ENABLED="https://"
    log "证书就绪：https://<你的IP>:${API_PORT}${BASE_PATH}"

    # renewal: 6-day certs, so renew twice a day; the service hot-reloads it
    log "注册自动续期定时器（resi-fanout-acme.timer）"
    cat > /etc/systemd/system/resi-fanout-acme.service <<EOF2
[Unit]
Description=Renew the ACME IP certificate used by resi-fanout
After=network-online.target

[Service]
Type=oneshot
ExecStart=/bin/sh -c 'lego run --accept-tos --server letsencrypt --profile shortlived --http --path ${TLS_DIR} --domains ${PUBLIC_IP} --renew-days 2'
EOF2
    cat > /etc/systemd/system/resi-fanout-acme.timer <<EOF2
[Unit]
Description=Twice-daily ACME IP certificate renewal

[Timer]
OnBootSec=10min
OnUnitActiveSec=12h
Persistent=true

[Install]
WantedBy=timers.target
EOF2
    systemctl daemon-reload
    systemctl enable --now resi-fanout-acme.timer >/dev/null 2>&1 || warn "定时器注册失败，可手动续期"
  else
    WITH_TLS="0"
    warn "已降级：本次安装仅监听 127.0.0.1（HTTP）。之后可重跑安装重试 TLS。"
  fi
fi

# ---------------------------------------------------------------- toolchain
if [ "${PREBUILT:-0}" != "1" ]; then
  if ! command -v cargo >/dev/null 2>&1; then
    log "installing Rust (rustup)"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --default-toolchain stable --profile minimal
    . "$HOME/.cargo/env"
  fi
  log "rust: $(cargo --version)"

  build_frontend="1"
  if [ "${NO_FRONTEND}" = "1" ]; then
    build_frontend="0"
  elif ! command -v npm >/dev/null 2>&1; then
    log "installing Node.js (needed to build the web UI)"
    case "$PKG" in
      apt-get)
        curl -fsSL https://deb.nodesource.com/setup_20.x | bash - >/dev/null 2>&1 || true
        install_pkgs nodejs || build_frontend="0" ;;
      dnf|yum)
        curl -fsSL https://rpm.nodesource.com/setup_20.x | bash - >/dev/null 2>&1 || true
        install_pkgs nodejs || build_frontend="0" ;;
      pacman) install_pkgs nodejs npm || build_frontend="0" ;;
      *) build_frontend="0" ;;
    esac
  fi

  DIST=""
  if [ "${build_frontend}" = "1" ] && command -v npm >/dev/null 2>&1; then
    log "building frontend (npm)"
    ( cd "${SRC_DIR}/frontend" && { npm ci --no-audit --no-fund 2>/dev/null || npm install --no-audit --no-fund; } && npm run build )
    DIST="${SRC_DIR}/frontend/dist"
  elif [ -d "${SRC_DIR}/frontend/dist" ]; then
    warn "npm unavailable — using the prebuilt dist shipped in the repo"
    DIST="${SRC_DIR}/frontend/dist"
  else
    warn "no frontend available; the API will work but there is no web UI"
  fi

  log "building backend (cargo, release) — this can take a few minutes"
  ( cd "${SRC_DIR}/backend" && cargo build --release )
  BINSRC="${SRC_DIR}/backend/target/release/${APP}"
else
  BINSRC="${SRC_DIR}/bin/${APP}"
  DIST="${SRC_DIR}/web"
fi

# ---------------------------------------------------------------- layout
log "installing to ${PREFIX}"
install -d "${PREFIX}/bin" "${PREFIX}/web" "${PREFIX}/scripts" "${CONF_DIR}" "${DATA_DIR}"
install -m 755 "${BINSRC}" "${PREFIX}/bin/${APP}"
install -m 755 "${SRC_DIR}/scripts/3xui-push.sh" "${SRC_DIR}/scripts/vpn-up.sh" "${SRC_DIR}/scripts/vpn-down.sh" "${SRC_DIR}/scripts/warp-up.sh" "${SRC_DIR}/scripts/warp-down.sh" "${PREFIX}/scripts/" 2>/dev/null || true
install -m 644 "${SRC_DIR}/scripts/xui_db.py" "${PREFIX}/scripts/xui_db.py" 2>/dev/null || true
if [ -n "${DIST}" ] && [ -f "${DIST}/index.html" ]; then
  cp -r "${DIST}/." "${PREFIX}/web/"
fi

if [ ! -f "${CONF_DIR}/config.json" ]; then
  # source checkout keeps it under backend/, the release tarball puts it at
  # the root of the extracted directory
  CONF_EXAMPLE=""
  for cand in "${SRC_DIR}/backend/config.example.json" "${SRC_DIR}/config.example.json"; do
    if [ -f "${cand}" ]; then CONF_EXAMPLE="${cand}"; break; fi
  done
  [ -n "${CONF_EXAMPLE}" ] || die "config.example.json not found under ${SRC_DIR}"
  API_KEY="$(cat /proc/sys/kernel/random/uuid | tr -d '-')"
  sed -e "s/__API_KEY__/${API_KEY}/" \
      -e "s|127.0.0.1:7654|127.0.0.1:${API_PORT}|" \
      "${CONF_EXAMPLE}" > "${CONF_DIR}/config.json"
  if [ "${WITH_VPNGATE}" = "1" ]; then
    python3 - "${CONF_DIR}/config.json" <<'PYEOF'
import json, sys
p = sys.argv[1]
c = json.load(open(p))
c["vpngate"]["enabled"] = True
json.dump(c, open(p, "w"), indent=2, ensure_ascii=False)
PYEOF
  fi
  if [ "${WITH_WARP}" = "1" ]; then
    python3 - "${CONF_DIR}/config.json" <<'PYEOF'
import json, sys
p = sys.argv[1]
c = json.load(open(p))
c["warp"]["enabled"] = True
json.dump(c, open(p, "w"), indent=2, ensure_ascii=False)
PYEOF
  fi
  chmod 640 "${CONF_DIR}/config.json"
  log "wrote ${CONF_DIR}/config.json (API key: ${API_KEY})"
else
  API_KEY="$(python3 -c "import json;print(json.load(open('${CONF_DIR}/config.json'))['server']['api_key'])" 2>/dev/null || true)"
  warn "config already exists, keeping it"
fi

id -u "${APP}" >/dev/null 2>&1 || useradd -r -M -s /usr/sbin/nologin "${APP}"
chown -R "${APP}:${APP}" "${DATA_DIR}"
chown    root:"${APP}"  "${CONF_DIR}" 2>/dev/null || true
chmod 750 "${CONF_DIR}" 2>/dev/null || true

# TLS 证书要等 config.json 存在之后才能写进去
if [ "${WITH_TLS}" = "1" ]; then
  python3 - "${CONF_DIR}/config.json" "${TLS_DIR}/fullchain.pem" "${TLS_DIR}/privkey.pem" "${BASE_PATH}" "${API_PORT}" <<'PYEOF2'
import json, sys
cfg_path, cert, key, base, port = sys.argv[1:6]
c = json.load(open(cfg_path))
c["server"]["tls"] = {"enabled": True, "cert_path": cert, "key_path": key, "reload_secs": 300}
c["server"]["base_path"] = base
c["server"]["listen"] = f"0.0.0.0:{port}"
json.dump(c, open(cfg_path, "w"), indent=2, ensure_ascii=False)
PYEOF2
  chown root:"${APP}" "${CONF_DIR}/config.json" 2>/dev/null || true
fi

log "installing global CLI: /usr/local/bin/rf"
cat > /usr/local/bin/rf <<'RFEOF'
#!/usr/bin/env bash
# rf — resi-fanout 管理脚本（参考 sing-box-yg 的交互菜单风格）
#   直接输入 rf 打开管理菜单；也支持子命令脚本化调用：
#   rf status|start|stop|restart|logs [n|-f]|api <path>|ui|key|update|uninstall|version

red='\033[0;31m'; green='\033[0;32m'; yellow='\033[0;33m'; blue='\033[0;36m'; plain='\033[0m'
red()    { echo -e "${red}$*${plain}"; }
green()  { echo -e "${green}$*${plain}"; }
yellow() { echo -e "${yellow}$*${plain}"; }
blue()   { echo -e "${blue}$*${plain}"; }
readp()  { echo -en "${yellow}$1${plain}"; read -r "$2"; }

CONF="${RF_CONF:-/etc/resi-fanout/config.json}"
SVC="resi-fanout"
c() { command -v "$1" >/dev/null 2>&1; }

api() {
  c python3 || { echo "需要 python3"; return 1; }
  python3 - "$CONF" "${1:-/api/status}" <<'PY'
import json, sys, urllib.request
cfg = json.load(open(sys.argv[1]))
host, _, port = cfg["server"]["listen"].rpartition(":")
key = cfg["server"]["api_key"]
req = urllib.request.Request(
    f"http://127.0.0.1:{port}{sys.argv[2]}",
    headers={"Authorization": f"Bearer {key}"} if key else {})
print(urllib.request.urlopen(req, timeout=15).read().decode())
PY
}

svc_ctl() {
  c systemctl || { yellow "本机没有 systemctl（可能未安装或非 Linux）"; return 1; }
  systemctl "$1" "$SVC" && blue "✔ $1 完成"
}

ui_url() {
  c python3 || return 1
  python3 - "$CONF" <<'PY'
import json, sys, socket, urllib.request
cfg = json.load(open(sys.argv[1]))
s = cfg["server"]
host, _, port = s["listen"].rpartition(":")

def public_ip():
    for url in ("https://api.ipify.org", "https://ifconfig.me/ip"):
        try:
            with urllib.request.urlopen(url, timeout=5) as r:
                ip = r.read().decode().strip()
                if ip and not ip.startswith(("10.", "127.", "192.168.", "172.")):
                    return ip
        except Exception:
            pass
    return None

if host in ("0.0.0.0", "::", ""):
    # 云主机 hostname 常解析到内网地址，优先用公网 IP
    host = public_ip() or socket.gethostbyname(socket.gethostname()) or "127.0.0.1"
scheme = "https" if s.get("tls", {}).get("enabled") else "http"
print(f"{scheme}://{host}:{port}{s.get('base_path', '')}/")
if host.startswith(("10.", "192.168.", "172.")):
    print("  提示：这是内网地址，公网访问请用你的公网 IP 替换")
print(f"  若打不开：检查云安全组/防火墙是否放行 {port} 端口")
PY
}
}

status_panel() {
  if c systemctl && systemctl is-active --quiet "$SVC" 2>/dev/null; then
    blue "服务状态：运行中"
  elif c systemctl; then
    red "服务状态：未运行（菜单 2 启动）"
  else
    yellow "服务状态：未知（本机无 systemctl）"
  fi
  if [ -f "$CONF" ] && c python3; then
    api /api/status 2>/dev/null | python3 -c 'import json,sys
try:
    d = json.load(sys.stdin)
    print("代理节点：%d（存活 %d，住宅 %d）" % (d["total"], d["alive"], d["residential"]))
    print("本地端口：%d / %d" % (d["ports"], d["max_ports"]))
    print("VPN Gate 出口：%d 在线" % d["vpn_up"])
    print("上次刷新：" + ("运行中…" if d.get("busy") else str(d.get("last_refresh") or "—")))
except Exception:
    pass' 2>/dev/null
  fi
  blue "UI 地址：$(ui_url 2>/dev/null)"
}

do_menu() {
  while true; do
    clear 2>/dev/null || true
    red  "~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~"
    green "  Resi-Fanout 管理菜单 · 住宅代理扇出 → 3x-ui"
    red  "~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~"
    status_panel
    red  "~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~"
    green " 1. 服务状态总览（刷新）"
    green " 2. 启动服务        3. 停止服务        4. 重启服务"
    green " 5. 立即抓取并检测代理"
    green " 6. 查看最近日志     7. 实时日志"
    green " 8. 查看 UI 地址 / API Key"
    green " 9. 更新 resi-fanout"
    yellow " 10. 卸载"
    red  " 0. 退出"
    readp "请输入数字【0-10】：" choice
    case "$choice" in
      1) : ;;
      2) svc_ctl start ;;
      3) svc_ctl stop ;;
      4) svc_ctl restart ;;
      5) api /api/refresh >/dev/null 2>&1 && green "已触发抓取+检测（几分钟后看菜单统计）" || red "触发失败（服务未运行？）" ;;
      6) c journalctl && journalctl -u "$SVC" -n 50 --no-pager || yellow "需要 journalctl" ;;
      7) c journalctl && journalctl -u "$SVC" -f --no-pager || yellow "需要 journalctl" ;;
      8) blue "UI：$(ui_url 2>/dev/null)"; blue "Key：$(python3 -c "import json;print(json.load(open('$CONF'))['server']['api_key'] or '（未设置）')" 2>/dev/null)" ;;
      9) curl -fsSL https://raw.githubusercontent.com/SectorPace/resi-fanout/main/install.sh | sudo bash ;;
      10) readp "确认卸载？[y/N]：" yn
          [ "$yn" = "y" ] && curl -fsSL https://raw.githubusercontent.com/SectorPace/resi-fanout/main/uninstall.sh | sudo bash && exit 0 ;;
      0|*) exit 0 ;;
    esac
    readp "按回车返回菜单…" _
  done
}

do_logs() {
  c journalctl || { yellow "需要 journalctl"; return 1; }
  if [ "${1:-}" = "-f" ]; then journalctl -u "$SVC" -f --no-pager
  else journalctl -u "$SVC" -n "${1:-50}" --no-pager; fi
}

do_update() {
  curl -fsSL https://raw.githubusercontent.com/SectorPace/resi-fanout/main/install.sh | sudo bash
}

do_uninstall() {
  readp "确认卸载 resi-fanout？[y/N]：" yn
  [ "$yn" = "y" ] && curl -fsSL https://raw.githubusercontent.com/SectorPace/resi-fanout/main/uninstall.sh | sudo bash
}

case "${1:-}" in
  ""|menu) do_menu ;;
  status)  status_panel ;;
  start|stop|restart) svc_ctl "$1" ;;
  logs)    shift; do_logs "$@" ;;
  api)     shift; api "${1:-/api/status}" ;;
  ui|url)  ui_url ;;
  key)     python3 -c "import json;print(json.load(open('$CONF'))['server']['api_key'] or '（未设置）')" 2>/dev/null ;;
  update)  do_update ;;
  uninstall) do_uninstall ;;
  version) "/opt/resi-fanout/bin/resi-fanout" version 2>/dev/null || api /api/status 2>/dev/null | python3 -c 'import json,sys
try: print("v" + json.load(sys.stdin)["version"])
except Exception: print("unknown")' ;;
  *) sed -n '3,10p' "$0" ;;
esac
RFEOF
chmod +x /usr/local/bin/rf

log "writing systemd unit ${SERVICE}"
# VPN Gate / WARP tunnels need NET_ADMIN (openvpn is installed by default)
CAPS=$'AmbientCapabilities=CAP_NET_ADMIN CAP_NET_RAW\nCapabilityBoundingSet=CAP_NET_ADMIN CAP_NET_RAW'
cat > "/etc/systemd/system/${SERVICE}" <<EOF
[Unit]
Description=Resi-Fanout: residential proxy fanout for 3x-ui
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=${APP}
ExecStart=${PREFIX}/bin/${APP} serve --config ${CONF_DIR}/config.json --data ${DATA_DIR} --web ${PREFIX}/web
Restart=on-failure
RestartSec=3
LimitNOFILE=65535
NoNewPrivileges=true
${CAPS}
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=${DATA_DIR} ${CONF_DIR}
PrivateTmp=true

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
# enable --now 对已在运行的服务不会重启，配置改动（如 TLS/监听地址）不会生效
systemctl enable "${SERVICE}" >/dev/null 2>&1 || true
systemctl restart "${SERVICE}"

sleep 2
if systemctl is-active --quiet "${SERVICE}"; then
  log "service is running"
else
  warn "service did not come up — check: journalctl -u ${SERVICE} -e"
fi

API_KEY_NOW="${API_KEY:-$(python3 -c "import json;print(json.load(open('${CONF_DIR}/config.json'))['server']['api_key'])" 2>/dev/null || echo '')}"

cat <<EOF

============================================================
 ${APP} installed
  API/UI : ${TLS_ENABLED:-http://}${PUBLIC_IP:-127.0.0.1}:${API_PORT}${BASE_PATH:-}  (web root: ${PREFIX}/web)
  API key: ${API_KEY_NOW:-<empty>}
  config : ${CONF_DIR}/config.json
  data   : ${DATA_DIR}
  logs   : journalctl -u ${SERVICE} -f

 管理菜单: 输入 rf 打开交互菜单（状态/启停/抓取/日志/更新/卸载）

 next steps:
  1. 公网访问需在云安全组/防火墙放行 ${API_PORT} 端口（仅本机则用 ssh -L ${API_PORT}:127.0.0.1:${API_PORT}）
     注：扇出的代理端口（20000+）只监听 127.0.0.1，不需要对公网开放；数量可在「配置」页调整
  2. wait for the first fetch+check cycle (~1-3 min), check 总览
  3. integrate with 3x-ui:
     bash ${PREFIX}/scripts/3xui-push.sh \\
        --api http://127.0.0.1:${API_PORT} --key <API_KEY>
     or use the UI tab 接入 3x-ui → generate outbounds and paste them
     into the panel's Xray config.
============================================================
EOF

if [ "${WITH_3XUI}" = "1" ]; then
  log "pushing outbounds into local 3x-ui panel"
  bash "${SRC_DIR}/scripts/3xui-push.sh" \
    --api "http://127.0.0.1:${API_PORT}" \
    --key "${API_KEY_NOW}" || warn "3x-ui push failed — run it manually later"
fi
