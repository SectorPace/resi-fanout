import io

p = "install.sh"
s = io.open(p, encoding="utf-8").read()

# 1) ui_url：URL 与提示分行，不再把括号说明塞进 URL
old = '''ui_url() {
  c python3 || { echo "(需要 python3)"; return; }
  python3 - "$CONF" <<'PY'
import json, sys, socket
cfg = json.load(open(sys.argv[1]))
s = cfg["server"]
host, _, port = s["listen"].rpartition(":")
if host in ("0.0.0.0", "::", ""):
    try:
        host = socket.gethostbyname(socket.gethostname())
    except Exception:
        host = "127.0.0.1"
if host == "127.0.0.1":
    host = "127.0.0.1 (仅本机，公网访问需隧道或 --with-tls)"
scheme = "https" if s.get("tls", {}).get("enabled") else "http"
print(f"{scheme}://{host}:{port}{s.get('base_path', '')}/")
PY
}'''
new = '''ui_url() {
  c python3 || { echo "(需要 python3)"; return; }
  python3 - "$CONF" <<'PY'
import json, sys, socket
cfg = json.load(open(sys.argv[1]))
s = cfg["server"]
host, _, port = s["listen"].rpartition(":")
if host in ("0.0.0.0", "::", ""):
    try:
        host = socket.gethostbyname(socket.gethostname())
    except Exception:
        host = "127.0.0.1"
scheme = "https" if s.get("tls", {}).get("enabled") else "http"
print(f"{scheme}://{host}:{port}{s.get('base_path', '')}/")
if host == "127.0.0.1":
    print("  (仅本机可访问；公网请用 SSH 隧道或 --with-tls)")
PY
}'''
assert old in s, "ui_url"
s = s.replace(old, new, 1)

# 2) status 概览：改用单引号 python -c，避免嵌套转义碎裂
old2 = '''    if [ -f "$CONF" ] && c python3; then
      api /api/status 2>/dev/null | python3 -c "
import json, sys
try:
    d = json.load(sys.stdin)
    print(f\\"节点 {d['total']} | 存活 {d['alive']} | 住宅 {d['residential']} | 端口 {d['ports']}/{d['max_ports']}\\")
except Exception:
    pass" 2>/dev/null
    fi'''
new2 = '''    if [ -f "$CONF" ] && c python3; then
      api /api/status 2>/dev/null | python3 -c 'import json,sys
try:
    d = json.load(sys.stdin)
    print("节点 %d | 存活 %d | 住宅 %d | 端口 %d/%d" % (d["total"], d["alive"], d["residential"], d["ports"], d["max_ports"]))
except Exception:
    pass' 2>/dev/null
    fi'''
assert old2 in s, "status line"
s = s.replace(old2, new2, 1)
io.open(p, "w", encoding="utf-8").write(s)
print("rf 脚本已修正")
