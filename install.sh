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
REPO_URL="${REPO_URL:-https://github.com/SectorPace/resi-fanout.git}"
GH_REPO="${GH_REPO:-SectorPace/resi-fanout}"
NO_FRONTEND="0"
FROM_SOURCE="0"

log()  { printf '\033[1;34m[install]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[warn]\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31m[error]\033[0m %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --port)       API_PORT="${2:?}"; shift 2 ;;
    --repo)       REPO_URL="${2:?}"; GH_REPO="${2#*github.com/}"; GH_REPO="${GH_REPO%.git}"; shift 2 ;;
    --with-3xui)  WITH_3XUI="1"; shift ;;
    --with-vpngate) WITH_VPNGATE="1"; shift ;;
    --with-warp)   WITH_WARP="1"; shift ;;
    --with-masque) WITH_MASQUE="1"; shift ;;
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

log "installing base packages (curl git ca-certificates python3)"
{ apt-get update -y >/dev/null 2>&1 || true; } 2>/dev/null || true
install_pkgs curl git ca-certificates python3 || warn "continue anyway"

if [ "${WITH_VPNGATE}" = "1" ]; then
  log "installing openvpn (VPN Gate sidecar tunnels)"
  install_pkgs openvpn iproute2 || warn "openvpn install failed — VPN Gate tunnels will not start"
fi

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

log "writing systemd unit ${SERVICE}"
CAPS=""
if [ "${WITH_VPNGATE}" = "1" ]; then
  CAPS=$'AmbientCapabilities=CAP_NET_ADMIN CAP_NET_RAW\nCapabilityBoundingSet=CAP_NET_ADMIN CAP_NET_RAW'
fi
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
systemctl enable --now "${SERVICE}"

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
  API/UI : http://127.0.0.1:${API_PORT}  (web root: ${PREFIX}/web)
  API key: ${API_KEY_NOW:-<empty>}
  config : ${CONF_DIR}/config.json
  data   : ${DATA_DIR}
  logs   : journalctl -u ${SERVICE} -f

 next steps:
  1. open the UI (port-forward via ssh -L ${API_PORT}:127.0.0.1:${API_PORT})
  2. wait for the first fetch+check cycle (~1-3 min), check 总览
  3. integrate with 3x-ui:
     bash ${SRC_DIR}/scripts/3xui-push.sh \\
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
