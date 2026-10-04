import io

# ---------- api.ts：新增端口分配接口与字段 ----------
p = "frontend/src/api.ts"
s = io.open(p, encoding="utf-8").read()

s = s.replace(
    "  ports: (): Promise<{ items: PortEntry[] }> => req(\"GET\", \"/api/ports\"),",
    """  ports: (): Promise<{ items: PortEntry[] }> => req("GET", "/api/ports"),
  portsAssign: (keys: string[]): Promise<{ ok: boolean; assigned: { key: string; port: number }[] }> =>
    req("POST", "/api/ports/assign", { keys }),
  portsRelease: (p: { ports?: number[]; keys?: string[] }): Promise<{ ok: boolean; released: number }> =>
    req("POST", "/api/ports/release", p),
  portsMode: (auto: boolean): Promise<{ ok: boolean; auto: boolean }> =>
    req("POST", "/api/ports/mode", { auto }),""", 1)

s = s.replace("  max_ports: number;\n  mode: string;", "  max_ports: number;\n  mode: string;\n  auto_assign: boolean;", 1)
s = s.replace("  max_ports: number;\n  ports: number;", "  max_ports: number;\n  auto_assign?: boolean;\n  ports: number;", 1)
io.open(p, "w", encoding="utf-8").write(s)
print("api.ts: 端口分配接口")

# ---------- views.ts ----------
p = "frontend/src/views.ts"
s = io.open(p, encoding="utf-8").read()

# Config 里 fanout 也需要 auto_assign（保存时不能丢）
s = s.replace("""                  fanout: {
                    ...cfg.fanout,
                    bind: get("c-bind"),""", """                  fanout: {
                    ...cfg.fanout,
                    bind: get("c-bind"),""", 1)

# 1) 节点池：加勾选状态 + 分配按钮
old_state = "const proxyState = {\n  alive: \"1\","
new_state = "const selectedKeys = new Set<string>();\n\nconst proxyState = {\n  alive: \"1\","
assert old_state in s
s = s.replace(old_state, new_state, 1)

old_filterbar = '''      el("label", { class: "chk" },
        checkbox(proxyState.residential, (v) => {
          proxyState.residential = v;
          void reload(true);
        }),
        " 仅住宅"
      )
    ),'''
new_filterbar = '''      el("label", { class: "chk" },
        checkbox(proxyState.residential, (v) => {
          proxyState.residential = v;
          void reload(true);
        }),
        " 仅住宅"
      ),
      el("button", { class: "primary", onclick: () => void assignSelected() }, "为勾选节点开放端口"),
      el("span", { id: "sel-info", class: "dim" }, "已选 0 个")
    ),'''
assert old_filterbar in s
s = s.replace(old_filterbar, new_filterbar, 1)

# 表格加勾选列
old_th = '''            el("thead", {}, el("tr", {},
              el("th", {}, "代理"),'''
new_th = '''            el("thead", {}, el("tr", {},
              el("th", { style: "width:28px" }, ""),
              el("th", {}, "代理"),'''
assert old_th in s
s = s.replace(old_th, new_th, 1)

old_row = '''            ...items.map((p) =>
              el("tr", {},
                el("td", { class: "mono" }, p.key),'''
new_row = '''            ...items.map((p) =>
              el("tr", {},
                el("td", {}, (() => {
                  const cb = el("input", { type: "checkbox" }) as HTMLInputElement;
                  cb.checked = selectedKeys.has(p.key);
                  cb.addEventListener("change", () => {
                    if (cb.checked) selectedKeys.add(p.key);
                    else selectedKeys.delete(p.key);
                    const info = document.getElementById("sel-info");
                    if (info) info.textContent = `已选 ${selectedKeys.size} 个`;
                  });
                  return cb;
                })()),
                el("td", { class: "mono" }, p.key),'''
assert old_row in s
s = s.replace(old_row, new_row, 1)

# 分配函数
old_assign_fn = '''  async function reload(resetPaging = false): Promise<void> {'''
new_assign_fn = '''  async function assignSelected(): Promise<void> {
    const keys = Array.from(selectedKeys);
    if (!keys.length) {
      toast("请先在表格里勾选要开放的节点", false);
      return;
    }
    try {
      const r = await api.portsAssign(keys);
      toast(`已为 ${r.assigned.length} 个节点开放端口`);
      selectedKeys.clear();
      await reload(true);
    } catch (e) { toast(String(e), false); }
  }

  async function reload(resetPaging = false): Promise<void> {'''
assert old_assign_fn in s
s = s.replace(old_assign_fn, new_assign_fn, 1)

# 2) 本地端口页：释放按钮 + 自动/手动开关
old_ports_actions = '''    el(
      "div",
      { class: "actions" },
      el("button", {
        onclick: async () => {
          try {
            const { items } = await api.ports();
            const lines = items.map((p) => `socks5://127.0.0.1:${p.port}#${p.country_code || p.protocol}-${p.port}`);
            await navigator.clipboard.writeText(lines.join("\\n"));
            toast(`已复制 ${lines.length} 条 socks 链接`);
          } catch (e) { toast(String(e), false); }
        }
      }, "复制全部 socks 链接"),
      el("button", { onclick: () => void reload() }, "刷新列表")
    ),'''
new_ports_actions = '''    el(
      "div",
      { class: "actions" },
      el("button", {
        onclick: async () => {
          try {
            const { items } = await api.ports();
            const lines = items.map((p) => `socks5://127.0.0.1:${p.port}#${p.country_code || p.protocol}-${p.port}`);
            await navigator.clipboard.writeText(lines.join("\\n"));
            toast(`已复制 ${lines.length} 条 socks 链接`);
          } catch (e) { toast(String(e), false); }
        }
      }, "复制全部 socks 链接"),
      el("button", { onclick: () => void reload() }, "刷新列表"),
      el("span", { id: "ports-hint", class: "dim" })
    ),'''
assert old_ports_actions in s
s = s.replace(old_ports_actions, new_ports_actions, 1)

old_tbody = '''            ...items.map((p: PortEntry) =>
              el("tr", {},
                el("td", { class: "mono strong" }, String(p.port)),
                el("td", { class: "mono" }, p.key),
                el("td", {}, p.protocol),
                el("td", {}, p.country_code || "—"),
                el("td", {}, p.latency_ms != null ? `${p.latency_ms}ms` : "—"),
                el("td", {},
                  p.residential ? badge("住宅", "res") : badge("机房", ""),
                  " ",
                  p.kind === "vpngate" ? badge("VPN Gate", "") : ""
                )
              )
            )'''
new_tbody = '''            ...items.map((p: PortEntry) =>
              el("tr", {},
                el("td", { class: "mono strong" }, String(p.port)),
                el("td", { class: "mono" }, p.key),
                el("td", {}, p.protocol),
                el("td", {}, p.country_code || "—"),
                el("td", {}, p.latency_ms != null ? `${p.latency_ms}ms` : "—"),
                el("td", {},
                  p.residential ? badge("住宅", "res") : badge("机房", ""),
                  " ",
                  p.kind === "vpngate" ? badge("VPN Gate", "") : ""
                ),
                el("td", {}, el("button", {
                  onclick: async () => {
                    try {
                      await api.portsRelease({ ports: [p.port] });
                      toast(`已释放端口 ${p.port}`);
                      await reload();
                    } catch (e) { toast(String(e), false); }
                  }
                }, "释放"))
              )
            )'''
assert old_tbody in s
s = s.replace(old_tbody, new_tbody, 1)

old_ports_header = '''            el("thead", {}, el("tr", {},
              el("th", {}, "本地端口"), el("th", {}, "上游代理"), el("th", {}, "协议"),
              el("th", {}, "国家"), el("th", {}, "延迟"), el("th", {}, "类型")
            ))'''
new_ports_header = '''            el("thead", {}, el("tr", {},
              el("th", {}, "本地端口"), el("th", {}, "上游代理"), el("th", {}, "协议"),
              el("th", {}, "国家"), el("th", {}, "延迟"), el("th", {}, "类型"), el("th", {}, "")
            ))'''
assert old_ports_header in s
s = s.replace(old_ports_header, new_ports_header, 1)

# reload 里显示自动/手动模式
old_reload_ports = '''  async function reload(): Promise<void> {
    const box = document.getElementById("port-table");
    if (!box) return;
    try {
      const { items } = await api.ports();'''
new_reload_ports = '''  async function reload(): Promise<void> {
    const box = document.getElementById("port-table");
    if (!box) return;
    try {
      const { items } = await api.ports();
      const hint = document.getElementById("ports-hint");
      if (hint) {
        const st = await api.status().catch(() => null);
        const auto = st?.auto_assign !== false;
        hint.textContent = auto
          ? "当前：自动分配（按延迟铺端口）· 点右侧切换为手动"
          : "当前：手动分配 · 在「节点池」勾选节点后点「为勾选节点开放端口」";
        const btn = document.getElementById("ports-mode-btn");
        if (btn) btn.textContent = auto ? "切换为手动分配" : "恢复自动分配";
      }'''
assert old_reload_ports in s
s = s.replace(old_reload_ports, new_reload_ports, 1)

# 开关按钮
old_btn = '''      el("span", { id: "ports-hint", class: "dim" })
    ),'''
new_btn = '''      el("button", {
        id: "ports-mode-btn",
        onclick: async () => {
          const st = await api.status().catch(() => null);
          const auto = st?.auto_assign !== false;
          try {
            await api.portsMode(!auto);
            toast(auto ? "已切换为手动分配" : "已恢复自动分配");
            await reload();
          } catch (e) { toast(String(e), false); }
        }
      }, "切换为手动分配"),
      el("span", { id: "ports-hint", class: "dim" })
    ),'''
assert old_btn in s
s = s.replace(old_btn, new_btn, 1)

io.open(p, "w", encoding="utf-8").write(s)
print("views.ts: 勾选分配 + 释放 + 模式切换")
