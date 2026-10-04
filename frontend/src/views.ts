import { api, type Config, type PortEntry, type Snippet, type VpngateInfo } from "./api";
import { badge, el, toast } from "./main";

const PAGE_SIZE = 100;

const selectedKeys = new Set<string>();

const proxyState = {
  alive: "1",
  residential: false,
  proto: "",
  country: "",
  q: "",
  offset: 0
};

export function renderProxies(root: HTMLElement): void {
  root.replaceChildren(
    el(
      "div",
      { class: "filterbar" },
      el(
        "select",
        {
          onchange: (e) => {
            proxyState.alive = (e.target as HTMLSelectElement).value;
            void reload(true);
          }
        },
        el("option", { value: "1" }, "仅存活"),
        el("option", { value: "0" }, "仅失效"),
        el("option", { value: "" }, "全部状态")
      ),
      el("select", {
        onchange: (e) => {
          proxyState.proto = (e.target as HTMLSelectElement).value;
          void reload(true);
        }
      },
        el("option", { value: "" }, "全部协议"),
        el("option", { value: "http" }, "http"),
        el("option", { value: "socks4" }, "socks4"),
        el("option", { value: "socks5" }, "socks5")
      ),
      el("input", {
        placeholder: "国家代码，如 US",
        style: "width:110px",
        value: proxyState.country,
        onchange: (e) => {
          proxyState.country = (e.target as HTMLInputElement).value.trim();
          void reload(true);
        }
      }),
      el("input", {
        placeholder: "搜索 ip / isp…",
        style: "flex:1",
        value: proxyState.q,
        onchange: (e) => {
          proxyState.q = (e.target as HTMLInputElement).value.trim();
          void reload(true);
        }
      }),
      el("label", { class: "chk" },
        checkbox(proxyState.residential, (v) => {
          proxyState.residential = v;
          void reload();
        }),
        " 仅住宅"
      ),
      el("button", { class: "primary", onclick: () => void assignSelected() }, "为勾选节点开放端口"),
      el("span", { id: "sel-info", class: "dim" }, "已选 0 个")
    ),
    el("div", { id: "proxy-table" }),
    el(
      "div",
      { class: "pager" },
      el("button", { id: "pg-prev", onclick: () => page(-1) }, "上一页"),
      el("span", { id: "pg-info" }),
      el("button", { id: "pg-next", onclick: () => page(1) }, "下一页")
    )
  );

  function checkbox(v: boolean, on: (v: boolean) => void): HTMLInputElement {
    const c = el("input", { type: "checkbox" }) as HTMLInputElement;
    c.checked = v;
    c.addEventListener("change", () => on(c.checked));
    return c;
  }

  async function page(delta: number): Promise<void> {
    proxyState.offset = Math.max(0, proxyState.offset + delta * PAGE_SIZE);
    await reload();
  }

  let proxySeq = 0;
  async function assignSelected(): Promise<void> {
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


  async function reload(resetPaging = false): Promise<void> {
    const box = document.getElementById("proxy-table");
    if (!box) return;
    // 换筛选条件时回到第一页，否则会停在旧 offset 上看到空表
    if (resetPaging) proxyState.offset = 0;
    // 请求序号：快速切换筛选时，先发的请求即使后到也不能覆盖新结果
    const mySeq = ++proxySeq;
    const params = new URLSearchParams({
      limit: String(PAGE_SIZE),
      offset: String(proxyState.offset)
    });
    if (proxyState.alive !== "") params.set("alive", proxyState.alive);
    if (proxyState.residential) params.set("residential", "1");
    if (proxyState.proto) params.set("proto", proxyState.proto);
    if (proxyState.country) params.set("country", proxyState.country);
    if (proxyState.q) params.set("q", proxyState.q);
    try {
      const { total, items } = await api.proxies(params.toString());
      if (mySeq !== proxySeq) return;
      const info = document.getElementById("pg-info");
      if (info) {
        info.textContent = items.length
          ? `第 ${proxyState.offset + 1}-${proxyState.offset + items.length} 条 / 共 ${total}`
          : `共 ${total} 条`;
      }
      box.replaceChildren(
        el(
          "table",
          { class: "tbl" },
          el(
            "thead",
            {},
            el(
              "tr",
              {},
              el("th", { style: "width:28px" }, ""),
              el("th", {}, "代理"),
              el("th", {}, "协议"),
              el("th", {}, "国家"),
              el("th", {}, "ISP"),
              el("th", {}, "延迟"),
              el("th", {}, "状态"),
              el("th", {}, "本地端口")
            )
          ),
          el(
            "tbody",
            {},
            ...items.map((p) =>
              el(
                "tr",
                {},
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
                el("td", { class: "mono" }, p.key),
                el("td", {}, p.protocol),
                el("td", {}, p.country_code || "—"),
                el("td", { class: "dim" }, p.isp || "—"),
                el("td", {}, p.latency_ms != null ? `${p.latency_ms}ms` : "—"),
                el(
                  "td",
                  {},
                  p.alive ? badge("存活", "ok") : badge("失效", "err"),
                  " ",
                  p.residential ? badge("住宅", "res") : p.hosting === true ? badge("机房", "") : ""
                ),
                el("td", { class: "mono" }, p.local_port ? String(p.local_port) : "—")
              )
            )
          )
        )
      );
    } catch (e) {
      box.replaceChildren(el("div", { class: "error" }, String(e)));
    }
  }

  void reload();
}

export function renderPorts(root: HTMLElement): void {
  root.replaceChildren(
    el(
      "div",
      { class: "actions" },
      el("button", {
        onclick: async () => {
          try {
            const { items } = await api.ports();
            const lines = items.map((p) => `socks5://127.0.0.1:${p.port}#${p.country_code || p.protocol}-${p.port}`);
            await navigator.clipboard.writeText(lines.join("\n"));
            toast(`已复制 ${lines.length} 条 socks 链接`);
          } catch (e) { toast(String(e), false); }
        }
      }, "复制全部 socks 链接"),
      el("button", { onclick: () => void reload() }, "刷新列表")
    ),
    el("div", { id: "port-table" })
  );

  async function reload(): Promise<void> {
    const box = document.getElementById("port-table");
    if (!box) return;
    try {
      const { items } = await api.ports();
      box.replaceChildren(
        el(
          "table",
          { class: "tbl" },
          el(
            "thead",
            {},
            el(
              "tr",
              {},
              el("th", {}, "本地端口"),
              el("th", {}, "上游代理"),
              el("th", {}, "协议"),
              el("th", {}, "国家"),
              el("th", {}, "延迟"),
              el("th", {}, "类型"),
              el("th", {}, "")
            )
          ),
          el(
            "tbody",
            {},
            ...items.map((p: PortEntry) =>
              el(
                "tr",
                {},
                el("td", { class: "mono strong" }, String(p.port)),
                el("td", { class: "mono" }, p.key),
                el("td", {}, p.protocol),
                el("td", {}, p.country_code || "—"),
                el("td", {}, p.latency_ms != null ? `${p.latency_ms}ms` : "—"),
                el(
                  "td",
                  {},
                  p.residential ? badge("住宅", "res") : badge("机房", ""),
                  " ",
                  p.kind === "vpngate" ? badge("VPN Gate", "") : ""
                ),
                el("td", {}, el("button", {
                  onclick: async () => {
                    try {
                      await api.portsRelease({ ports: [p.port] });
                      toast(`已释放端口 ${p.port}（节点仍在池中）`);
                      await reload();
                    } catch (e) { toast(String(e), false); }
                  }
                }, "释放"))
              )
            )
          )
        )
      );
    } catch (e) {
      box.replaceChildren(el("div", { class: "error" }, String(e)));
    }
  }

  void reload();
}

export function renderVpngate(root: HTMLElement): void {
  root.replaceChildren(
    el(
      "div",
      { class: "actions" },
      el("button", { class: "primary", onclick: () => void reload() }, "刷新"),
      el("button", {
        onclick: async () => {
          try {
            await api.vpngateRebuild();
            toast("已清空隧道分配，管理器将重新选点");
            setTimeout(() => void reload(), 1500);
          } catch (e) { toast(String(e), false); }
        }
      }, "重新选点"),
      el("span", { id: "vg-hint", class: "dim" })
    ),
    el("div", { id: "vg-tunnels" }),
    el("h3", {}, "候选服务器（按 Score 排序，前 50）"),
    el("div", { id: "vg-pool" })
  );

  let vgSeq = 0;
  async function reload(): Promise<void> {
    const mySeq = ++vgSeq;
    let info: VpngateInfo;
    const hint = document.getElementById("vg-hint");
    try {
      info = await api.vpngate();
    } catch (e) {
      const t = document.getElementById("vg-tunnels");
      if (t) t.replaceChildren(el("div", { class: "error" }, String(e)));
      return;
    }
    if (mySeq !== vgSeq) return;
    if (hint) {
      const m = (info as unknown as { meta?: { source?: string; rows?: number; at?: number } }).meta;
      const src = m?.source ? ` · 源 ${m.source.split("/")[2] ?? m.source}` : "";
      hint.textContent = info.enabled
        ? ` 已启用 · 在线 ${info.pool_size} 台 · 累计缓存 ${info.pool_cached} 台${src} · 更新 ${fmtTs2(info.pool_ts)}`
        : " 未启用：在「配置」页开启 vpngate.enabled 并安装 openvpn";
    }
    const tb = document.getElementById("vg-tunnels");
    if (tb) {
      // WARP / MASQUE 的出口在「CF WARP」页展示，这里只列 VPN Gate 中继
      const own = info.tunnels.filter((t) => t.server_key !== "warp" && t.server_key !== "masque");
      tb.replaceChildren(
        el(
          "table",
          { class: "tbl" },
          el("thead", {}, el("tr", {},
            el("th", {}, "本地端口"), el("th", {}, "服务器"), el("th", {}, "出口国家"),
            el("th", {}, "ISP"), el("th", {}, "延迟"), el("th", {}, "隧道状态"), el("th", {}, "出口类型")
          )),
          el("tbody", {},
            ...(own.length
              ? own.map((t) => el("tr", {},
                  el("td", { class: "mono strong" }, String(t.local_port)),
                  el("td", { class: "mono" }, t.hostname),
                  el("td", {}, t.country_code || "—"),
                  el("td", { class: "dim" }, t.isp || "—"),
                  el("td", {}, t.latency_ms != null ? `${t.latency_ms}ms` : "—"),
                  el("td", {}, badge(t.status, t.status === "up" ? "ok" : t.status === "spawning" ? "warn" : "err")),
                  el("td", {}, t.residential ? badge("住宅", "res") : t.hosting === true ? badge("机房", "") : t.alive ? badge("住宅?", "res") : "—")
                ))
              : [el("tr", {}, el("td", { colspan: "7", class: "empty" }, info.enabled ? "隧道建立中…（首次连接约需 10-30 秒）" : "未启用"))])
          )
        )
      );
    }
    const pb = document.getElementById("vg-pool");
    if (pb) {
      pb.replaceChildren(
        el(
          "table",
          { class: "tbl" },
          el("thead", {}, el("tr", {},
            el("th", {}, "主机"), el("th", {}, "国家"), el("th", {}, "速度"),
            el("th", {}, "Ping"), el("th", {}, "会话数"), el("th", {}, "运行时长"), el("th", {}, "日志"), el("th", {}, "Score"), el("th", {}, "上次在线")
          )),
          el("tbody", {},
            ...info.top.slice(0, 50).map((s) => el("tr", {},
              el("td", { class: "mono" }, s.hostname),
              el("td", {}, s.country_code || "—"),
              el("td", {}, `${s.speed_mbps} Mbps`),
              el("td", {}, `${s.ping_ms}ms`),
              el("td", {}, String(s.sessions)),
              el("td", {}, fmtUptime2(s.uptime_secs)),
              el("td", {}, s.logs_kept == null ? "—" : s.logs_kept ? badge("记录", "warn") : badge("不记录", "ok")),
              el("td", {}, String(s.score)),
              el("td", { class: "dim" }, s.last_seen ? fmtTs2(s.last_seen) : "缓存")
            ))
          )
        )
      );
    }
  }

  function fmtTs2(ts?: number | null): string {
    return ts ? new Date(ts * 1000).toLocaleString("zh-CN", { hour12: false }) : "—";
  }
  function fmtUptime2(secs: number): string {
    const d = Math.floor(secs / 86400);
    const h = Math.floor((secs % 86400) / 3600);
    return d > 0 ? `${d}天${h}时` : `${h}时`;
  }

  void reload();
  const timer = window.setInterval(() => void reload(), 15000);
  const obs = new MutationObserver(() => {
    if (!document.body.contains(root)) {
      window.clearInterval(timer);
      obs.disconnect();
    }
  });
  obs.observe(document.body, { childList: true, subtree: true });
}

export function renderWarp(root: HTMLElement): void {
  root.replaceChildren(
    el(
      "fieldset",
      {},
      el("legend", {}, "Cloudflare WARP（WireGuard 出口）"),
      el("div", { id: "warp-status", class: "dim" }, "加载中…"),
      el(
        "div",
        { class: "filterbar" },
        el("input", { id: "warp-license", placeholder: "WARP+ 许可 key（可选）", style: "flex:1" }),
        el("button", {
          onclick: async () => {
            const lic = (document.getElementById("warp-license") as HTMLInputElement).value.trim();
            try {
              const r = await api.warpRegister(lic || undefined);
              toast(r.msg);
              await loadWarp();
            } catch (e) { toast(String(e), false); }
          }
        }, "用 wgcf 注册"),
        el("button", {
          onclick: async () => {
            try {
              await api.warpConnect();
              toast("已拉起 WireGuard 隧道");
              setTimeout(() => void loadWarp(), 3000);
            } catch (e) { toast(String(e), false); }
          }
        }, "连接"),
        el("button", {
          onclick: async () => {
            try {
              await api.warpDisconnect();
              toast("已断开");
              await loadWarp();
            } catch (e) { toast(String(e), false); }
          }
        }, "断开"),
        el("button", {
          onclick: () => {
            const ta = document.getElementById("warp-out") as HTMLTextAreaElement | null;
            if (!ta || !ta.value) { toast("还没有 WARP 配置", false); return; }
            void navigator.clipboard.writeText(ta.value);
            toast("已复制 Xray 出站");
          }
        }, "复制 Xray 出站")
      ),
      el("textarea", {
        id: "warp-config",
        class: "code",
        rows: "6",
        style: "width:100%",
        placeholder: "[Interface]\nPrivateKey = ...\nAddress = 172.16.0.2/32\n\n[Peer]\nPublicKey = bmXOC+F1FxEMF9dyiK2H5/1SUtzH0JuVo51h2wPfgyo=\nEndpoint = engage.cloudflareclient.com:2408\nAllowedIPs = 0.0.0.0/0, ::/0\nPersistentKeepalive = 60"
      }),
      el(
        "div",
        { class: "actions" },
        el("button", {
          onclick: async () => {
            const cfg = (document.getElementById("warp-config") as HTMLTextAreaElement).value;
            try {
              await api.warpImport(cfg);
              toast("配置已保存，连接后生效");
              await loadWarp();
            } catch (e) { toast(String(e), false); }
          }
        }, "导入这份配置"),
        el("small", { class: "dim" }, "支持 wgcf 生成的配置或手写 WireGuard 配置；私钥只存在本机 /var/lib/resi-fanout/warp/warp.conf")
      ),
      el("textarea", { id: "warp-out", class: "code", rows: "8", style: "width:100%;display:none" })
    ),
    el(
      "fieldset",
      {},
      el("legend", {}, "从 Clash / Mihomo 配置导入 MASQUE 节点"),
      el("textarea", {
        id: "clash-yaml",
        class: "code",
        rows: "5",
        style: "width:100%",
        placeholder: "把 Clash / Mihomo 配置里的 proxies 段整段粘进来（含 type: masque 的节点）"
      }),
      el(
        "div",
        { class: "filterbar" },
        el("select", { id: "clash-pick" }, el("option", { value: "-1" }, "先点「解析节点」")),
        el("button", {
          onclick: async () => {
            const yaml = (document.getElementById("clash-yaml") as HTMLTextAreaElement).value;
            if (!yaml.trim()) { toast("先粘贴 Clash 配置", false); return; }
            try {
              const r = await api.warpImportClash(yaml);
              const sel = document.getElementById("clash-pick") as HTMLSelectElement;
              sel.replaceChildren(
                ...r.nodes.map((n) =>
                  el("option", { value: String(n.index) }, `${n.name} · ${n.server}:${n.port} · ${n.kind}`)
                )
              );
              toast(`解析到 ${r.count} 个 MASQUE/WireGuard 节点`);
            } catch (e) { toast(String(e), false); }
          }
        }, "解析节点"),
        el("button", {
          class: "primary",
          onclick: () => void applyClash("masque")
        }, "按 MASQUE 应用（推荐）"),
        el("button", {
          onclick: () => void applyClash("wireguard")
        }, "按 WireGuard 应用")
      ),
      el("small", { class: "dim" },
        "MASQUE 节点用 Cloudflare 的多算法密钥容器，只有 Mihomo 能正确消费，因此走 Mihomo 旁挂；应用后该端口会出现在「本地端口」页并可联动 3x-ui。"),
      el("div", { id: "clash-result" })
    ),
    el("h3", {}, "WARP / MASQUE 出口状态"),
    el("div", { id: "warp-tunnels" })
  );

  async function applyClash(mode: "masque" | "wireguard"): Promise<void> {
    const out = document.getElementById("clash-result");
    const sel = document.getElementById("clash-pick") as HTMLSelectElement;
    const yaml = (document.getElementById("clash-yaml") as HTMLTextAreaElement).value;
    const index = Number(sel.value);
    if (index < 0) { toast("先解析并选择一个节点", false); return; }
    try {
      const r = await api.warpApplyClash(yaml, index, mode);
      const msg = [
        `已应用：${r.node}`,
        r.mode === "masque" ? `Mihomo 旁挂端口 ${r.port}` : "已存为 WireGuard 配置，点「连接」生效",
        r.sidecar_started === false && r.hint ? `（未自动启动：${r.hint}）` : ""
      ].filter(Boolean).join(" · ");
      if (out) out.replaceChildren(el("div", { class: "error", style: "border-color:var(--ok);color:var(--ok);background:rgba(63,185,111,.1)" }, msg));
      toast(msg);
      await loadWarp();
    } catch (e) {
      if (out) out.replaceChildren(el("div", { class: "error" }, String(e)));
      toast(String(e), false);
    }
  }

  let warpSeq = 0;
  async function loadWarp(): Promise<void> {
    const mySeq = ++warpSeq;
    const box = document.getElementById("warp-status");
    const out = document.getElementById("warp-out") as HTMLTextAreaElement | null;
    if (!box) return;
    try {
      const w = await api.warp();
      const tools = [
        w.tools.wg_quick ? "wg-quick ✓" : "wg-quick ✗",
        w.tools.wgcf ? "wgcf ✓" : "wgcf ✗"
      ].join(" · ");
      const parts = [
        `工具 ${tools}`,
        `配置 ${w.profile_present ? "已导入" : "缺失"}`,
        w.up ? `隧道已连接 (tun ${w.tun_ip ?? "?"})` : "隧道未连接",
        w.exit_ip ? `出口 ${w.exit_ip} · ${w.country ?? "?"} · ${w.isp ?? "?"}${w.latency_ms != null ? ` · ${w.latency_ms}ms` : ""}` : "",
        w.error ? `错误：${w.error}` : ""
      ].filter(Boolean);
      if (mySeq !== warpSeq) return;
      box.textContent = parts.join("  |  ");
      if (w.xray_outbound && out) {
        out.style.display = "";
        out.value = JSON.stringify(w.xray_outbound, null, 2);
      }
    } catch (e) {
      box.textContent = String(e);
    }
    const tb = document.getElementById("warp-tunnels");
    if (!tb) return;
    // VPN Gate 页也在拉同一份 /api/vpngate，这里降频到 ~60s 避免重复请求
    warpTick += 1;
    if (warpTick % 4 !== 0 && warpRows.length) {
      renderWarpTunnels(warpRows);
      return;
    }
    try {
      const vg = await api.vpngate();
      warpRows = vg.tunnels.filter((t) => t.server_key === "warp" || t.server_key === "masque");
      renderWarpTunnels(warpRows);
    } catch {
      // vpngate 接口失败不影响 WARP 状态显示
    }
  }

  let warpTick = 0;
  let warpRows: import("./api").VpnTunnel[] = [];

  function renderWarpTunnels(rows: import("./api").VpnTunnel[]): void {
    const tb = document.getElementById("warp-tunnels");
    if (!tb) return;
    tb.replaceChildren(
        el(
          "table",
          { class: "tbl" },
          el("thead", {}, el("tr", {},
            el("th", {}, "本地端口"), el("th", {}, "出口"), el("th", {}, "出口国家"),
            el("th", {}, "ISP"), el("th", {}, "延迟"), el("th", {}, "状态"), el("th", {}, "类型")
          )),
          el("tbody", {},
            ...(rows.length
              ? rows.map((t) => el("tr", {},
                  el("td", { class: "mono strong" }, String(t.local_port)),
                  el("td", { class: "mono" }, t.exit_ip || t.tun_ip || "—"),
                  el("td", {}, t.country_code || "—"),
                  el("td", { class: "dim" }, t.isp || "—"),
                  el("td", {}, t.latency_ms != null ? `${t.latency_ms}ms` : "—"),
                  el("td", {}, badge(t.status, t.status === "up" ? "ok" : "warn")),
                  el("td", {},
                    t.server_key === "warp" ? badge("WARP WireGuard", "acc") : badge("MASQUE", "acc"),
                    " ",
                    t.residential ? badge("住宅", "res") : t.hosting === true ? badge("机房", "") : "")
                ))
              : [el("tr", {}, el("td", { colspan: "7", class: "empty" }, "暂无 WARP / MASQUE 出口"))])
          )
        )
      );
  }
  void loadWarp();
  const warpTimer = window.setInterval(() => void loadWarp(), 15000);
  const warpObs = new MutationObserver(() => {
    if (!document.body.contains(root)) {
      window.clearInterval(warpTimer);
      warpObs.disconnect();
    }
  });
  warpObs.observe(document.body, { childList: true, subtree: true });
}

export async function renderConfig(root: HTMLElement): Promise<void> {
  let cfg: Config;
  try {
    cfg = await api.config();
  } catch (e) {
    root.replaceChildren(el("div", { class: "error" }, String(e)));
    return;
  }

  const f = (label: string, id: string, value: string | number, hint = ""): HTMLElement =>
    el(
      "div",
      { class: "field" },
      el("label", { for: id }, label),
      el("input", { id, value: String(value) }),
      hint ? el("small", { class: "dim" }, hint) : el("span")
    );

  const chk = (id: string, label: string, v: boolean): HTMLElement => {
    const c = el("input", { id, type: "checkbox" }) as HTMLInputElement;
    c.checked = v;
    return el("label", { class: "chk field-inline" }, c, " ", label);
  };

  const get = (id: string): string => (document.getElementById(id) as HTMLInputElement).value.trim();
  const getn = (id: string): number => Number(get(id));

  root.replaceChildren(
    el(
      "div",
      { class: "grid2" },
      el(
        "fieldset",
        {},
        el("legend", {}, "服务"),
        f("监听地址 (API/UI)", "c-listen", cfg.server.listen),
        f("API Key（留空=不鉴权）", "c-apikey", cfg.server.api_key),
        f("前端目录", "c-web", cfg.server.web_root)
      ),
      el(
        "fieldset",
        {},
        el("legend", {}, "扇出端口"),
        f("绑定地址", "c-bind", cfg.fanout.bind),
        f("起始端口", "c-base", cfg.fanout.base_port),
        el(
          "div",
          { class: "field" },
          el("label", { for: "c-mode" }, "本地端口协议"),
          el(
            "select",
            { id: "c-mode" },
            el("option", { value: "socks", ...(cfg.fanout.mode === "socks" ? { selected: "" } : {}) }, "socks"),
            el("option", { value: "http", ...(cfg.fanout.mode === "http" ? { selected: "" } : {}) }, "http"),
            el("option", { value: "mixed", ...(cfg.fanout.mode === "mixed" ? { selected: "" } : {}) }, "mixed（自动识别）")
          )
        ),
        f("最大端口数", "c-max", cfg.fanout.max_ports)
      ),
      el(
        "fieldset",
        {},
        el("legend", {}, "检测与调度"),
        f("检测超时(秒)", "c-timeout", cfg.checker.timeout_secs),
        f("并发数", "c-conc", cfg.checker.concurrency),
        f("节点池上限", "c-pool", cfg.checker.max_pool),
        f("抓取间隔(分钟, 0=停)", "c-refresh", cfg.scheduler.refresh_minutes),
        f("复检间隔(分钟)", "c-recheck", cfg.scheduler.recheck_minutes),
        f("失效保留(天)", "c-prune", cfg.scheduler.prune_days)
      ),
      el(
        "fieldset",
        {},
        el("legend", {}, "过滤"),
        chk("c-resi", "仅扇出住宅 IP（hosting=false）", cfg.filter.only_residential),
        f("国家白名单(逗号分隔, 空=全部)", "c-countries", cfg.filter.countries.join(",")),
        f("协议白名单(逗号分隔, 空=全部)", "c-protocols", cfg.filter.protocols.join(","))
      ),
      el(
        "fieldset",
        {},
        el("legend", {}, "VPN Gate（OpenVPN 旁挂隧道）"),
        chk("c-vg-enabled", "启用（需要 openvpn 与 root/NET_ADMIN）", cfg.vpngate.enabled),
        f("起始端口", "c-vg-base", cfg.vpngate.base_port),
        f("隧道数量", "c-vg-max", cfg.vpngate.max_servers),
        f("国家白名单(逗号分隔, 空=最优)", "c-vg-countries", cfg.vpngate.countries.join(",")),
        f("最低速度(Mbps)", "c-vg-speed", cfg.vpngate.min_speed_mbps),
        f("openvpn 可执行文件", "c-vg-bin", cfg.vpngate.openvpn_bin),
        chk("c-vg-resi", "仅保留住宅出口隧道（机房出口自动换点）", cfg.vpngate.only_residential)
      )
    ),
      el(
        "fieldset",
        {},
        el("legend", {}, "3x-ui 面板联动"),
        f("面板数据库路径", "c-xui-db", cfg.xui.db_path, "找不到时会在常见位置自动探测"),
        f("入站起始端口", "c-xui-port", cfg.xui.inbound_port_base),
        f("入站 tag 前缀", "c-xui-in", cfg.xui.inbound_prefix),
        f("出站 tag 前缀", "c-xui-out", cfg.xui.outbound_prefix),
        chk("c-xui-restart", "写库后自动重启面板", cfg.xui.auto_restart)
      ),
    el(
      "fieldset",
      {},
      el("legend", {}, "代理源（JSON 数组）"),
      el("small", { class: "dim" },
        "kind: text（纯文本 ip:port）/ monosans / geonode；付费住宅服务商只要输出 ip:port 或 proto://ip:port 的 URL 也能直接接入"),
      el("textarea", {
        id: "c-sources",
        class: "code",
        rows: "10",
        style: "width:100%"
      }, JSON.stringify(cfg.sources, null, 2)),
      el(
        "div",
        { class: "actions" },
        el(
          "button",
          {
            class: "primary",
            onclick: async () => {
              try {
                // 基于服务端当前配置做增量覆盖：只改表单里出现的字段，
                // 绝不能丢掉 base_path / tls / warp / xui 等未暴露的段
                // （丢掉会把公网 HTTPS + 随机路径降级成明文 HTTP）
                const next: Config = {
                  ...cfg,
                  server: {
                    ...cfg.server,
                    listen: get("c-listen"),
                    api_key: get("c-apikey"),
                    web_root: get("c-web")
                  },
                  fanout: {
                    ...cfg.fanout,
                    bind: get("c-bind"),
                    base_port: getn("c-base"),
                    mode: (document.getElementById("c-mode") as HTMLSelectElement).value,
                    max_ports: getn("c-max")
                  },
                  checker: {
                    ...cfg.checker,
                    timeout_secs: getn("c-timeout"),
                    concurrency: getn("c-conc"),
                    max_pool: getn("c-pool")
                  },
                  scheduler: {
                    ...cfg.scheduler,
                    refresh_minutes: getn("c-refresh"),
                    recheck_minutes: getn("c-recheck"),
                    prune_days: getn("c-prune")
                  },
                  filter: {
                    ...cfg.filter,
                    only_residential: (document.getElementById("c-resi") as HTMLInputElement).checked,
                    countries: get("c-countries")
                      ? get("c-countries").split(",").map((s) => s.trim()).filter(Boolean)
                      : [],
                    protocols: get("c-protocols")
                      ? get("c-protocols").split(",").map((s) => s.trim()).filter(Boolean)
                      : []
                  },
                  xui: {
                    ...cfg.xui,
                    db_path: get("c-xui-db"),
                    inbound_port_base: getn("c-xui-port"),
                    inbound_prefix: get("c-xui-in"),
                    outbound_prefix: get("c-xui-out"),
                    auto_restart: (document.getElementById("c-xui-restart") as HTMLInputElement).checked
                  },
                  vpngate: {
                    ...cfg.vpngate,
                    enabled: (document.getElementById("c-vg-enabled") as HTMLInputElement).checked,
                    base_port: getn("c-vg-base"),
                    max_servers: getn("c-vg-max"),
                    countries: get("c-vg-countries")
                      ? get("c-vg-countries").split(",").map((s) => s.trim()).filter(Boolean)
                      : [],
                    min_speed_mbps: getn("c-vg-speed"),
                    only_residential: (document.getElementById("c-vg-resi") as HTMLInputElement).checked
                  }
                };
                await api.saveConfig(next);
                toast("配置已保存并生效");
              } catch (e) {
                toast(String(e), false);
              }
            }
          },
          "保存配置"
        )
      )
    )
  );
}

export function renderXui(root: HTMLElement): void {
  root.replaceChildren(
    el(
      "div",
      { class: "grid2" },
      el(
        "fieldset",
        {},
        el("legend", {}, "接管 3x-ui 面板入站（fanout 式）"),
        el("small", { class: "dim" },
          "为每个出口克隆一条面板入站：客户端连不同入站 → 从不同国家/住宅 IP 出去。写库前自动备份。"),
        el("div", { id: "xui-panel-status", class: "dim" }, "未加载面板"),
        el(
          "div",
          { class: "filterbar" },
          el("select", { id: "xui-template" }, el("option", { value: "0" }, "加载面板入站中…")),
          el("input", { id: "xui-host", placeholder: "链接域名，如 panel.example.com", style: "flex:1" }),
          el(
            "button",
            { onclick: () => void loadPanelInbounds() },
            "加载面板入站"
          ),
          el("button", { onclick: () => void doLink(true) }, "预览"),
          el("button", { class: "primary", onclick: () => void doLink(false) }, "执行联动"),
          el("button", {
            onclick: async () => {
              if (!confirm("解绑会删除面板里所有 resi-in-* 入站及其路由规则，确定？")) return;
              try {
                const r = await api.xuiUnlink();
                toast(`已解绑 ${r.removed?.length ?? 0} 条入站（${r.restart || ""}）`);
                await loadPanelInbounds();
              } catch (e) { toast(String(e), false); }
            }
          }, "解绑")
        ),
        el("div", { id: "xui-links" })
      ),
      el(
        "fieldset",
        {},
        el("legend", {}, "生成出站配置"),
        el("small", { class: "dim" }, "选择要接入 3x-ui 的本地端口（默认全部已分配端口），生成 Xray outbounds。"),
        el(
          "select",
          { id: "xui-mode" },
          el("option", { value: "direct" }, "直连：每端口一条规则（配合面板入站联动）"),
          el("option", { value: "balancer" }, "负载均衡：一个入站在所有出口间轮换（leastPing）")
        ),
        el("input", { id: "xui-inbound", placeholder: "负载均衡模式：填入站 tag（可选）", style: "width:100%;margin:8px 0" }),
        el("div", { id: "xui-ports", class: "portlist" }),
        el(
          "div",
          { class: "field" },
          el("label", { for: "xui-prefix" }, "出站 tag 前缀"),
          el("input", { id: "xui-prefix", value: "resi" })
        ),
        el(
          "div",
          { class: "actions" },
          el("button", { class: "primary", onclick: () => void gen(false) }, "生成"),
          el("button", { onclick: () => void gen(true) }, "仅住宅端口")
        )
      )
    ),
    el("div", { id: "xui-output" })
  );

  async function loadPanelInbounds(): Promise<void> {
    const status = document.getElementById("xui-panel-status");
    const sel = document.getElementById("xui-template") as HTMLSelectElement;
    if (status) status.textContent = "正在读取面板数据库…";
    try {
      const info = await api.xuiInbounds();
      const list = info.inbounds || [];
      sel.replaceChildren(
        ...(list.length
          ? list.map((i) =>
              el(
                "option",
                { value: String(i.id) },
                `#${i.id} ${i.tag || i.remark || "-"} · ${i.protocol || "?"}:${i.port || "?"} · ${i.clients ?? 0}客户端${i.enable === false ? "（已禁用）" : ""}`
              )
            )
          : [el("option", { value: "0" }, "面板里还没有入站")])
      );
      if (status) {
        status.textContent = `已加载 ${list.length} 条入站${info.clients_table ? `（客户端在 ${info.clients_table} 表，v3 布局）` : "（客户端内嵌于 settings，v2 布局）"}`;
      }
    } catch (e) {
      if (status) status.textContent = `读取失败：${e}（检查 xui.db_path / xui.script_path 配置）`;
    }
  }

  function linkBody(): { template_id: number; ports?: number[]; residential_only: boolean } {
    const sel = document.getElementById("xui-template") as HTMLSelectElement;
    const mode = (document.getElementById("xui-mode") as HTMLSelectElement).value;
    const body: { template_id: number; ports?: number[]; residential_only: boolean } = {
      template_id: Number(sel.value) || 0,
      residential_only: mode === "balancer" ? false : false
    };
    if (mode !== "balancer") {
      const checked = checkedPorts();
      if (checked.length) body.ports = checked;
    }
    const host = (document.getElementById("xui-host") as HTMLInputElement).value.trim();
    if (host) (body as Record<string, unknown>).host = host;
    return body;
  }

  async function doLink(preview: boolean): Promise<void> {
    const out = document.getElementById("xui-links");
    if (!out) return;
    const body = linkBody();
    if (!body.template_id) {
      toast("先点「加载面板入站」并选择一个模板入站", false);
      return;
    }
    try {
      const r = preview ? await api.xuiPreview(body) : await api.xuiLink(body);
      const rows = r.plan || r.created || [];
      out.replaceChildren(
        el("h3", {}, preview ? "联动预览（未写入）" : "已写入面板"),
        el(
          "table",
          { class: "tbl" },
          el("thead", {}, el("tr", {}, el("th", {}, "本地端口"), el("th", {}, "面板入站端口"), el("th", {}, "客户端链接"))),
          el(
            "tbody",
            {},
            ...rows.map((row) =>
              el(
                "tr",
                {},
                el("td", { class: "mono" }, String((row as Record<string, unknown>).fanout_port ?? (row as Record<string, unknown>).inbound_tag ?? "")),
                el("td", { class: "mono" }, String(row.inbound_port)),
                el("td", { class: "mono small" }, row.link || "")
              )
            )
          )
        ),
        r.backup ? el("small", { class: "dim" }, `数据库备份：${r.backup}`) : el("span"),
        r.restart ? el("small", { class: "dim" }, ` ${r.restart}`) : el("span"),
        preview
          ? el("button", { onclick: () => void doLink(false) }, "确认写入面板")
          : el("button", {
              onclick: () => {
                void navigator.clipboard.writeText(rows.map((x) => x.link || "").join("\n"));
                toast("已复制链接");
              }
            }, "复制全部链接")
      );
      if (!preview) await loadPanelInbounds();
    } catch (e) {
      out.replaceChildren(el("div", { class: "error" }, String(e)));
    }
  }

  void loadPanelInbounds();

  async function gen(residentialOnly: boolean): Promise<void> {
    const out = document.getElementById("xui-output");
    if (!out) return;
    const params = new URLSearchParams();
    const prefix = (document.getElementById("xui-prefix") as HTMLInputElement).value.trim() || "resi";
    const mode = (document.getElementById("xui-mode") as HTMLSelectElement).value;
    const inbound = (document.getElementById("xui-inbound") as HTMLInputElement).value.trim();
    params.set("prefix", prefix);
    params.set("mode", mode);
    if (mode === "balancer") {
      params.set("residential", residentialOnly ? "1" : "0");
      if (residentialOnly) params.set("residential", "1");
      if (inbound) params.set("inbound", inbound);
    } else {
      const checked = checkedPorts();
      if (checked.length) params.set("ports", checked.join(","));
    }
    try {
      const sn: Snippet = await api.snippet(params.toString());
      const block = (title: string, data: unknown): HTMLElement =>
        el(
          "fieldset",
          {},
          el("legend", {}, title),
          el("textarea", { class: "code", rows: "10", style: "width:100%" }, JSON.stringify(data, null, 2)),
          el("button", {
            onclick: (e) => {
              const btn = e.target as HTMLButtonElement;
              const ta = btn.previousElementSibling as HTMLTextAreaElement;
              void navigator.clipboard.writeText(ta.value);
              toast("已复制");
            }
          }, "复制")
        );
      out.replaceChildren(
        el("div", { class: "dim" }, sn.usage),
        block("outbounds（合并进 Xray 配置的 outbounds 数组）", sn.outbounds),
        block("rules_example（分流规则示例）", sn.rules_example),
        block("observatory + balancers（负载均衡模式需要）", sn.balancer_extra),
        block("full_template（3x-ui 数据库无模板时可用）", sn.full_template)
      );
    } catch (e) {
      out.replaceChildren(el("div", { class: "error" }, String(e)));
    }
  }

  function checkedPorts(): number[] {
    return Array.from(
      document.querySelectorAll<HTMLInputElement>("#xui-ports input:checked")
    ).map((c) => Number(c.dataset.port));
  }

  void (async () => {
    const box = document.getElementById("xui-ports");
    if (!box) return;
    try {
      const { items } = await api.ports();
      box.replaceChildren(
        ...items.map((p) => {
          const c = el("input", { type: "checkbox", "data-port": String(p.port) }) as HTMLInputElement;
          c.checked = true;
          return el(
            "label",
            { class: "chk" },
            c,
            ` ${p.port} (${p.protocol}${p.residential ? "·住宅" : ""}${p.country_code ? "·" + p.country_code : ""})`
          );
        })
      );
    } catch {
      box.replaceChildren(el("span", { class: "dim" }, "无法加载端口列表"));
    }
  })();
}
