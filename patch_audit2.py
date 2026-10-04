import io

p = "install.sh"
s = io.open(p, encoding="utf-8").read()

# (1) 统一临时目录 + trap 清理（此前每次安装都在 /tmp 残留几十 MB）
if "TMPROOT" not in s:
    s = s.replace('SERVICE="${APP}.service"', '''SERVICE="${APP}.service"

TMPROOT="$(mktemp -d)"
cleanup() { [ -n "${TMPROOT:-}" ] && rm -rf "$TMPROOT"; }
trap cleanup EXIT INT TERM''', 1)
    s = s.replace('SRC_DIR="$(mktemp -d)/src"', 'SRC_DIR="${TMPROOT}/src"')
    s = s.replace('    T="$(mktemp -d)"', '    T="${TMPROOT}/lego"; mkdir -p "$T"')

# (2) config.json 属主无条件修正（降级路径下 root:root 0640 → 服务账号读不到 → 崩溃循环）
old_chmod = '  chmod 640 "${CONF_DIR}/config.json"'
if old_chmod in s:
    s = s.replace(old_chmod, '  chmod 640 "${CONF_DIR}/config.json"\n  chown root:"${APP}" "${CONF_DIR}/config.json" 2>/dev/null || true', 1)

# (3) mihomo / wgcf 下载不留 0 字节文件
old_mihomo = '''  if [ -n "${MURL}" ] && curl -fsSL "${MURL}" | gzip -dc > /usr/local/bin/mihomo 2>/dev/null; then
    chmod +x /usr/local/bin/mihomo
  else
    warn "mihomo download failed — MASQUE nodes can still be imported, just run mihomo manually"
  fi'''
new_mihomo = '''  TMPB="${TMPROOT}/mihomo.bin"
  if [ -n "${MURL}" ] && curl -fsSL "${MURL}" | gzip -dc > "${TMPB}" 2>/dev/null && [ -s "${TMPB}" ]; then
    install -m 755 "${TMPB}" /usr/local/bin/mihomo
  else
    rm -f /usr/local/bin/mihomo
    warn "mihomo download failed — MASQUE nodes can still be imported, just run mihomo manually"
  fi'''
assert old_mihomo in s, "mihomo"
s = s.replace(old_mihomo, new_mihomo, 1)

old_wgcf = '''    if [ -n "${WGURL}" ] && curl -fsSL "${WGURL}" -o /usr/local/bin/wgcf; then
      chmod +x /usr/local/bin/wgcf
    else
      warn "wgcf download skipped — you can still paste a WireGuard config in the UI"
    fi'''
new_wgcf = '''    TMPB="${TMPROOT}/wgcf.bin"
    if [ -n "${WGURL}" ] && curl -fsSL "${WGURL}" -o "${TMPB}" && [ -s "${TMPB}" ]; then
      install -m 755 "${TMPB}" /usr/local/bin/wgcf
    else
      rm -f /usr/local/bin/wgcf
      warn "wgcf download skipped — you can still paste a WireGuard config in the UI"
    fi'''
assert old_wgcf in s, "wgcf"
s = s.replace(old_wgcf, new_wgcf, 1)

# (4) rf 的 api() 支持 HTTPS
old_api = '''req = urllib.request.Request(
    f"http://127.0.0.1:{port}{sys.argv[2]}",
    headers={"Authorization": f"Bearer {key}"} if key else {})
print(urllib.request.urlopen(req, timeout=15).read().decode())'''
new_api = '''scheme = "https" if cfg["server"].get("tls", {}).get("enabled") else "http"
req = urllib.request.Request(
    f"{scheme}://127.0.0.1:{port}{sys.argv[2]}",
    headers={"Authorization": f"Bearer {key}"} if key else {})
ctx = ssl.create_default_context() if scheme == "https" else None
print(urllib.request.urlopen(req, timeout=15, context=ctx).read().decode())'''
assert old_api in s, "rf api"
s = s.replace(old_api, new_api, 1)
# rf 的 api() 需要 import ssl + 变量名确认
import re
m = re.search(r"(  python3 - \"\$CONF\" \"\$\{1:-/api/status\}\" <<'PY'\n)import json, sys, urllib\.request", s)
assert m, "rf api import"
s = s.replace("import json, sys, urllib.request", "import json, sys, ssl, urllib.request", 1)
# cfg 变量名（rf 的 api() 用的是 cfg 还是 c）
if "cfg = json.load" not in s:
    s = s.replace('c = json.load(open(sys.argv[1]))', 'cfg = json.load(open(sys.argv[1]))', 1)
    s = s.replace('host, _, port = c["server"]["listen"]', 'host, _, port = cfg["server"]["listen"]', 1)
    s = s.replace('key = c["server"]["api_key"]', 'key = cfg["server"]["api_key"]', 1)

io.open(p, "w", encoding="utf-8").write(s)
print("install.sh 已修: trap 清理 / config 属主 / 下载不留空文件 / rf 走 HTTPS")

# ---- warp.rs: wgcf 显式 HOME ----
p = "backend/src/warp.rs"
s = io.open(p, encoding="utf-8").read()
if 'env("HOME"' not in s:
    old = '''    let mut cmd = Command::new("wgcf");
    cmd.arg("register");'''
    new = '''    // systemd 以专用用户运行且 ProtectHome=true，默认 $HOME 不可写，
    // 显式把 HOME 指到数据目录，wgcf 才能写 ~/.wgcf
    let home_dir = std::path::Path::new(&cfg.warp.conf_path)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/var/lib/resi-fanout/warp"));
    let _ = tokio::fs::create_dir_all(&home_dir).await;
    let mut cmd = Command::new("wgcf");
    cmd.env("HOME", &home_dir).arg("register");'''
    assert old in s, "wgcf cmd"
    s = s.replace(old, new, 1)
    old_read = '''    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let src = PathBuf::from(home).join(".wgcf/wgcf-profile.conf");'''
    new_read = '''    let src = home_dir.join(".wgcf/wgcf-profile.conf");'''
    assert old_read in s, "wgcf read"
    s = s.replace(old_read, new_read, 1)
    io.open(p, "w", encoding="utf-8").write(s)
    print("warp.rs 已修: wgcf HOME")

# ---- warp-up.sh ----
p = "scripts/warp-up.sh"
s = io.open(p, encoding="utf-8").read()
if "${address:-}" not in s:
    old = '[ -z "${LOCAL}" ] && LOCAL="${address%%,*}"'
    new = '''[ -z "${LOCAL}" ] && LOCAL="${address:-}"
[ -z "${LOCAL}" ] && LOCAL="$(ip -4 -o addr show dev "${INTERFACE:-${dev}}" 2>/dev/null | awk '{print $4}' | cut -d/ -f1 | head -1)"'''
    assert old in s, "warp-up address"
    s = s.replace(old, new, 1)
    io.open(p, "w", encoding="utf-8").write(s)
    print("warp-up.sh 已修")

# ---- vpn-up.sh: 路由没装上不宣布 up ----
p = "scripts/vpn-up.sh"
s = io.open(p, encoding="utf-8").read()
if 'ip rule show | grep -q' not in s:
    old = '''ip route replace default via "${GW}" dev "${DEV}" table "${TABLE}" 2>/dev/null || true

printf '%s' "${LOCAL}" > "${VPN_IPFILE}" 2>/dev/null || true'''
    new = '''# via 网关常因点对点而失败，回退 dev 路由；都失败则 exit 1，
# 让 openvpn 放弃该节点（否则会拿宿主默认路由假装隧道通了，出口 IP 判错）
if ! ip route replace default via "${GW}" dev "${DEV}" table "${TABLE}" 2>/dev/null; then
  ip route replace default dev "${DEV}" table "${TABLE}" 2>/dev/null || exit 1
fi
# 校验规则与路由真的生效
ip rule show | grep -q "from ${LOCAL} lookup ${TABLE}" || exit 1
[ -n "$(ip route show table "${TABLE}" 2>/dev/null)" ] || exit 1

printf '%s' "${LOCAL}" > "${VPN_IPFILE}" 2>/dev/null || true'''
    assert old in s, "vpn-up route"
    s = s.replace(old, new, 1)
    io.open(p, "w", encoding="utf-8").write(s)
    print("vpn-up.sh 已修")