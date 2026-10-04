import io

# ============ install.sh ============
p = "install.sh"
s = io.open(p, encoding="utf-8").read()

# (1) 统一临时目录 + trap 清理（此前每次安装都在 /tmp 残留几十 MB）
old_vars = 'SERVICE="${APP}.service"'
assert old_vars in s
s = s.replace(old_vars, old_vars + '''

TMPROOT="$(mktemp -d)"
cleanup() { [ -n "${TMPROOT:-}" ] && rm -rf "$TMPROOT"; }
trap cleanup EXIT INT TERM''', 1)
s = s.replace('SRC_DIR="$(mktemp -d)/src"', 'SRC_DIR="${TMPROOT}/src"')
s = s.replace('    T="$(mktemp -d)"', '    T="${TMPROOT}/lego" && mkdir -p "$T"')
s = s.replace('T="$(mktemp -d)"\n    if curl -fsSL "${LEGO_URL}"', 'T="${TMPROOT}/lego" && mkdir -p "$T"\n    if curl -fsSL "${LEGO_URL}"')

# (2) config.json 属主无条件修正（原来只在 TLS 成功分支，root:root 0640 会让
#     服务账号读不到配置 → 崩溃循环）
old_chmod = '  chmod 640 "${CONF_DIR}/config.json"'
assert old_chmod in s
s = s.replace(old_chmod, '  chmod 640 "${CONF_DIR}/config.json"\n  chown root:"${APP}" "${CONF_DIR}/config.json" 2>/dev/null || true', 1)

# (3) mihomo / wgcf 下载不再留下 0 字节可执行文件
old_mihomo = '''  if [ -n "${MURL}" ] && curl -fsSL "${MURL}" | gzip -dc > /usr/local/bin/mihomo 2>/dev/null; then
    chmod +x /usr/local/bin/mihomo
  else
    warn "mihomo 下载失败（网络问题？），可手动安装后重试"
  fi'''
new_mihomo = '''  TMPB="${TMPROOT}/mihomo.bin"
  if [ -n "${MURL}" ] && curl -fsSL "${MURL}" | gzip -dc > "${TMPB}" 2>/dev/null && [ -s "${TMPB}" ]; then
    install -m 755 "${TMPB}" /usr/local/bin/mihomo
  else
    rm -f /usr/local/bin/mihomo
    warn "mihomo 下载失败（网络问题？），MASQUE 节点需手动安装 mihomo"
  fi'''
if old_mihomo in s:
    s = s.replace(old_mihomo, new_mihemo if False else new_mihomo, 1)
else:
    import re
    m = re.search(r'  if \[ -n "\$\{MURL\}" \].*?warn "mihomo 下载失败.*?\n  fi', s, re.S)
    assert m, "mihomo block"
    s = s[:m.start()] + new_mihomo + s[m.end():]

old_wgcf = '''    if [ -n "${WGURL}" ] && curl -fsSL "${WGURL}" -o /usr/local/bin/wgcf; then
      chmod +x /usr/local/bin/wgcf
    else
      warn "wgcf 下载失败，可手动安装后重试"
    fi'''
new_wgcf = '''    TMPB="${TMPROOT}/wgcf.bin"
    if [ -n "${WGURL}" ] && curl -fsSL "${WGURL}" -o "${TMPB}" && [ -s "${TMPB}" ]; then
      install -m 755 "${TMPB}" /usr/local/bin/wgcf
    else
      rm -f /usr/local/bin/wgcf
      warn "wgcf 下载失败，可手动安装后重试"
    fi'''
if old_wgcf in s:
    s = s.replace(old_wgcf, new_wgcf, 1)

# (4) rf 的 api() 支持 HTTPS（默认部署就是 HTTPS，写死 http 会让菜单全废）
old_api = '''req = urllib.request.Request(
    f"http://127.0.0.1:{port}{sys.argv[2]}",
    headers={"Authorization": f"Bearer {key}"} if key else {})
print(urllib.request.urlopen(req, timeout=15).read().decode())'''
new_api = '''scheme = "https" if s.get("tls", {}).get("enabled") else "http"
req = urllib.request.Request(
    f"{scheme}://127.0.0.1:{port}{sys.argv[2]}",
    headers={"Authorization": f"Bearer {key}"} if key else {})
ctx = ssl.create_default_context() if scheme == "https" else None
print(urllib.request.urlopen(req, timeout=15, context=ctx).read().decode())'''
if old_api in s:
    s = s.replace(old_api, new_api, 1)
else:
    old_api2 = '''req = urllib.request.Request(
    f"http://127.0.0.1:{port}{sys.argv[2]}",
    headers={"Authorization": f"Bearer {key}"} if key else {})
print(urllib.request.urlopen(req, timeout=15).read().decode())'''
    if old_api2 in s:
        s = s.replace(old_api2, new_api.replace("s.get(\"tls\"", "cfg[\"server\"].get(\"tls\"", 1), 1)
    else:
        import re
        m = re.search(r'import json, sys, urllib\.request\n.*?\nPY\n\}', s, re.S)
        assert m, "rf api block"
        blk = m.group(0)
        nb = blk.replace("import json, sys, urllib.request", "import json, sys, ssl, urllib.request")
        nb = nb.replace('f"http://127.0.0.1:{port}', 'f"{scheme}://127.0.0.1:{port}')
        nb = nb.replace('print(urllib.request.urlopen(req, timeout=15).read().decode())',
                        'ctx = ssl.create_default_context() if scheme == "https" else None\nprint(urllib.request.urlopen(req, timeout=15, context=ctx).read().decode())')
        s = s[:m.start()] + nb + s[m.end():]

io.open(p, "w", encoding="utf-8").write(s)
print("install.sh: trap/属主/下载安全/rf https")

# ============ warp.rs: wgcf 显式 HOME（systemd User= + ProtectHome 会挡住 $HOME）============
p = "backend/src/warp.rs"
s = io.open(p, encoding="utf-8").read()
old = '''    let mut cmd = Command::new("wgcf");
    cmd.arg("register");'''
new = '''    // systemd 以专用用户运行且 ProtectHome=true，默认 $HOME 不可写，
    // 这里显式指定 HOME 到数据目录
    let home_dir = std::path::Path::new(&cfg.warp.conf_path)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/var/lib/resi-fanout/warp"));
    let _ = tokio::fs::create_dir_all(&home_dir).await;
    let mut cmd = Command::new("wgcf");
    cmd.env("HOME", &home_dir).arg("register");'''
assert old in s
s = s.replace(old, new, 1)
old_read = '''    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let src = PathBuf::from(home).join(".wgcf/wgcf-profile.conf");'''
new_read = '''    let src = home_dir.join(".wgcf/wgcf-profile.conf");'''
assert old_read in s
s = s.replace(old_read, new_read, 1)
io.open(p, "w", encoding="utf-8").write(s)
print("warp.rs: wgcf HOME 指向数据目录")

# ============ warp-up.sh: set -u 下 address 未定义会中止钩子 ============
p = "scripts/warp-up.sh"
s = io.open(p, encoding="utf-8").read()
old = '''LOCAL="${ifconfig_local:-}"
DEV="${dev:-}"
# wg-quick exports `address` (may be a comma separated CIDR list)
[ -z "${LOCAL}" ] && LOCAL="${address%%,*}"
LOCAL="${LOCAL%%/*}"'''
new = '''LOCAL="${ifconfig_local:-}"
DEV="${dev:-}"
# wg-quick 不保证导出 ifconfig_local；address 也可能不存在（set -u 下会中止），
# 最后再用 ip 命令从接口上兜底取地址
[ -z "${LOCAL}" ] && LOCAL="${address:-}"
[ -z "${LOCAL}" ] && LOCAL="$(ip -4 -o addr show dev "${INTERFACE:-${dev}}" 2>/dev/null | awk '{print $4}' | cut -d/ -f1 | head -1)"
LOCAL="${LOCAL%%/*}"'''
assert old in s
s = s.replace(old, new, 1)
io.open(p, "w", encoding="utf-8").write(s)
print("warp-up.sh: 地址兜底")

# ============ vpn-up.sh: 路由没装上就不该宣布隧道 up ============
p = "scripts/vpn-up.sh"
s = io.open(p, encoding="utf-8").read()
old = '''ip rule add from "${LOCAL}" lookup "${TABLE}" 2>/dev/null || true
ip route replace default via "${GW}" dev "${DEV}" table "${TABLE}" 2>/dev/null || true

printf '%s' "${LOCAL}" > "${VPN_IPFILE}" 2>/dev/null || true
exit 0'''
new = '''ip rule add from "${LOCAL}" lookup "${TABLE}" 2>/dev/null || true
# 点对点场景 via 网关常失败，回退到 dev 路由；两条都失败就 exit 1 让 openvpn
# 放弃这个节点（否则会拿着宿主机的默认路由假装隧道通了，出口 IP 判错）
if ! ip route replace default via "${GW}" dev "${DEV}" table "${TABLE}" 2>/dev/null; then
  ip route replace default dev "${DEV}" table "${TABLE}" 2>/dev/null || exit 1
fi
# 校验规则与路由确实生效
ip rule show | grep -q "from ${LOCAL} lookup ${TABLE}" || exit 1
[ -n "$(ip route show table "${TABLE}" 2>/dev/null)" ] || exit 1

printf '%s' "${LOCAL}" > "${VPN_IPFILE}" 2>/dev/null || true
exit 0'''
assert old in s
s = s.replace(old, new, 1)
io.open(p, "w", encoding="utf-8").write(s)
print("vpn-up.sh: 策略路由校验")