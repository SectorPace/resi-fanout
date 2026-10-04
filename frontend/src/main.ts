import { api, fmtTs, fmtUptime, getKey, setKey, type Status } from "./api";
import "./style.css";
import { renderProxies, renderPorts, renderConfig, renderXui, renderVpngate, renderWarp } from "./views";

export function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  attrs: Record<string, string | ((e: Event) => void)> = {},
  ...children: (Node | string | null | undefined)[]
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k.startsWith("on") && typeof v === "function") {
      node.addEventListener(k.slice(2), v as EventListener);
    } else if (k === "class") {
      node.className = v as string;
    } else if (k === "style") {
      node.setAttribute("style", v as string);
    } else {
      node.setAttribute(k, v as string);
    }
  }
  for (const c of children) {
    if (c == null) continue;
    node.append(c instanceof Node ? c : document.createTextNode(c));
  }
  return node;
}

let statusTimer: number | undefined;

export const app = document.getElementById("app") as HTMLDivElement;

function nav(tab: string): void {
  document.querySelectorAll<HTMLElement>(".tab").forEach((t) => {
    t.classList.toggle("active", t.dataset.tab === tab);
  });
  document.querySelectorAll<HTMLElement>(".view").forEach((v) => {
    v.style.display = v.id === `view-${tab}` ? "" : "none";
  });
  if (statusTimer) {
    window.clearInterval(statusTimer);
    statusTimer = undefined;
  }
  if (tab === "status") {
    pollStatus();
    statusTimer = window.setInterval(pollStatus, 5000);
  }
}

async function pollStatus(): Promise<void> {
  const box = document.getElementById("status-cards");
  const srcBox = document.getElementById("status-sources");
  if (!box || !srcBox) return;
  try {
    const s: Status = await api.status();
    const verChip = document.getElementById("ver-chip");
    if (verChip) verChip.textContent = `v${s.version}`;
    box.replaceChildren(
      card("节点总数", String(s.total)),
      card("存活", String(s.alive), s.alive > 0 ? "ok" : "warn"),
      card("住宅 IP", String(s.residential), s.residential > 0 ? "res" : ""),
      card("已分配端口", `${s.ports} / ${s.max_ports}`),
      card("VPN Gate 出口", `${s.vpn_up} 在线`),
      card("运行时长", fmtUptime(s.uptime_secs)),
      card("上次刷新", fmtTs(s.last_refresh))
    );
    srcBox.replaceChildren(
      el("h3", {}, "数据源状态"),
      el(
        "table",
        { class: "tbl" },
        el("thead", {}, el("tr", {}, el("th", {}, "名称"), el("th", {}, "条数"), el("th", {}, "状态"), el("th", {}, "时间"))),
        el(
          "tbody",
          {},
          ...s.sources.map((x) =>
            el(
              "tr",
              {},
              el("td", {}, x.name),
              el("td", {}, String(x.count)),
              el("td", {}, x.ok ? badge("正常", "ok") : badge(x.error || "失败", "err")),
              el("td", {}, fmtTs(x.ts))
            )
          )
        )
      )
    );
    const busy = document.getElementById("busy-indicator");
    if (busy) busy.textContent = s.busy ? "● 后台任务运行中" : "";
  } catch (e) {
    box.replaceChildren(el("div", { class: "error" }, String(e)));
  }
}

function card(label: string, value: string, tone = ""): HTMLElement {
  return el(
    "div",
    { class: `card ${tone}` },
    el("div", { class: "card-label" }, label),
    el("div", { class: "card-value" }, value)
  );
}

export function badge(text: string, tone = ""): HTMLElement {
  return el("span", { class: `badge ${tone}` }, text);
}

export function toast(msg: string, ok = true): void {
  const t = el("div", { class: `toast ${ok ? "ok" : "err"}` }, msg);
  document.body.append(t);
  window.setTimeout(() => t.remove(), 3500);
}

function buildShell(): void {
  app.replaceChildren(
    el(
      "header",
      { class: "topbar" },
      el("h1", {}, "Resi-Fanout", el("small", {}, " · 住宅代理扇出 → 3x-ui")),
      el("span", { id: "busy-indicator", class: "busy" }),
      el(
        "span",
        { class: "keybox" },
        el("input", {
          id: "api-key",
          type: "password",
          placeholder: "API Key（可选）",
          value: getKey()
        }),
        el(
          "button",
          {
            onclick: () => {
              const input = document.getElementById("api-key") as HTMLInputElement;
              setKey(input.value.trim());
              toast("API Key 已保存");
            }
          },
          "保存"
        )
      )
    ),
    el(
      "nav",
      { class: "tabs" },
      el("button", { class: "tab active", "data-tab": "status", onclick: () => nav("status") }, "总览"),
      el("button", { class: "tab", "data-tab": "proxies", onclick: () => nav("proxies") }, "节点池"),
      el("button", { class: "tab", "data-tab": "ports", onclick: () => nav("ports") }, "本地端口"),
      el("button", { class: "tab", "data-tab": "vpngate", onclick: () => nav("vpngate") }, "VPN Gate"),
      el("button", { class: "tab", "data-tab": "warp", onclick: () => nav("warp") }, "CF WARP"),
      el("button", { class: "tab", "data-tab": "config", onclick: () => nav("config") }, "配置"),
      el("button", { class: "tab", "data-tab": "xui", onclick: () => nav("xui") }, "接入 3x-ui")
    ),
    el("main", { class: "content" },
      el("section", { id: "view-status", class: "view" },
        el("div", { id: "status-cards", class: "cards" }),
        el(
          "div",
          { class: "actions" },
          el("button", {
            class: "primary",
            onclick: async () => {
              try {
                await api.refresh();
                toast("已触发抓取+检测");
              } catch (e) { toast(String(e), false); }
            }
          }, "立即抓取并检测"),
          el("button", {
            onclick: async () => {
              try {
                await api.check();
                toast("已触发全量检测");
              } catch (e) { toast(String(e), false); }
            }
          }, "重新检测全部")
        ),
        el("div", { id: "status-sources" })
      ),
      el("section", { id: "view-proxies", class: "view", style: "display:none" }),
      el("section", { id: "view-ports", class: "view", style: "display:none" }),
      el("section", { id: "view-vpngate", class: "view", style: "display:none" }),
      el("section", { id: "view-warp", class: "view", style: "display:none" }),
      el("section", { id: "view-config", class: "view", style: "display:none" }),
      el("section", { id: "view-xui", class: "view", style: "display:none" })
    ),
    el("footer", { class: "footer" },
      el("span", {}, "Resi-Fanout — 住宅代理扇出控制台"),
      el("a", { href: "https://github.com/SectorPace/resi-fanout", target: "_blank" }, "GitHub")
    )
  );

  renderProxies(document.getElementById("view-proxies") as HTMLElement);
  renderPorts(document.getElementById("view-ports") as HTMLElement);
  void renderConfig(document.getElementById("view-config") as HTMLElement);
  renderXui(document.getElementById("view-xui") as HTMLElement);
  renderVpngate(document.getElementById("view-vpngate") as HTMLElement);
  renderWarp(document.getElementById("view-warp") as HTMLElement);
}

buildShell();
void pollStatus();
statusTimer = window.setInterval(pollStatus, 5000);
