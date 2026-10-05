// API client. The key lives in localStorage so the UI keeps working
// when the backend enforces "Authorization: Bearer <key>".

export interface ProxyItem {
  key: string;
  protocol: string;
  ip: string;
  port: number;
  country?: string | null;
  country_code?: string | null;
  isp?: string | null;
  anonymity?: string | null;
  alive: boolean;
  latency_ms?: number | null;
  hosting?: boolean | null;
  anon_flag?: boolean | null;
  exit_ip?: string | null;
  last_check?: number | null;
  fails: number;
  local_port?: number | null;
  residential: boolean;
}

export interface Status {
  version: string;
  uptime_secs: number;
  busy: boolean;
  total: number;
  alive: number;
  residential: number;
  ports: number;
  max_ports: number;
  vpn_enabled: boolean;
  vpn_total: number;
  vpn_up: number;
  vpn_residential: number;
  last_refresh?: number | null;
  next_refresh?: number | null;
  last_check_all?: number | null;
  sources: { name: string; ok: boolean; count: number; error?: string | null; ts: number }[];
}

export interface PortEntry {
  port: number;
  key: string;
  protocol: string;
  country_code?: string | null;
  latency_ms?: number | null;
  residential: boolean;
  kind: string; // proxy | vpngate
}

export interface VpnServer {
  hostname: string;
  ip: string;
  score: number;
  ping_ms: number;
  speed_mbps: number;
  country?: string | null;
  country_code?: string | null;
  sessions: number;
  uptime_secs: number;
  logs_kept?: boolean | null;
  operator?: string | null;
  last_seen?: number;
}

export interface VpnTunnel {
  server_key: string;
  hostname: string;
  local_port: number;
  status: string; // spawning | up | down | failed
  tun_ip?: string | null;
  attempts: number;
  alive: boolean;
  latency_ms?: number | null;
  country?: string | null;
  country_code?: string | null;
  isp?: string | null;
  hosting?: boolean | null;
  exit_ip?: string | null;
  residential: boolean;
}

export interface VpngateInfo {
  enabled: boolean;
  pool_ts?: number | null;
  pool_size: number;
  pool_cached: number;
  tunnels: VpnTunnel[];
  top: VpnServer[];
}

export interface XuiInbound {
  id: number;
  tag?: string;
  remark?: string;
  port?: number;
  protocol?: string;
  enable?: boolean;
  clients?: number;
}

export interface XuiInfo {
  ok: boolean;
  inbounds?: XuiInbound[];
  clients_table?: string | null;
  plan?: { fanout_port: number; inbound_port: number; inbound_tag: string; outbound_tag: string; remark: string; link: string }[];
  created?: { inbound_id: number; inbound_port: number; inbound_tag: string; link: string }[];
  removed?: { id: number; tag: string; port: number }[];
  backup?: string;
  restart?: string;
  error?: string;
}

export interface TlsCfg {
  enabled: boolean;
  cert_path: string;
  key_path: string;
  reload_secs: number;
}

export interface WarpCfg {
  enabled: boolean;
  conf_path: string;
  interface: string;
  local_port: number;
  auto_register: boolean;
  license: string;
  keepalive: number;
  mtu: number;
  mihomo_bin: string;
  mihomo_port: number;
  mihomo_conf: string;
}

export interface Config {
  server: { listen: string; api_key: string; web_root: string; base_path: string; tls: TlsCfg };
  fanout: { bind: string; base_port: number; mode: string; max_ports: number; auto_assign: boolean };
  checker: {
    timeout_secs: number;
    concurrency: number;
    max_pool: number;
    classify_url: string;
  };
  scheduler: { refresh_minutes: number; recheck_minutes: number; prune_days: number };
  filter: { only_residential: boolean; countries: string[]; protocols: string[] };
  vpngate: {
    enabled: boolean;
    base_port: number;
    max_servers: number;
    countries: string[];
    min_speed_mbps: number;
    openvpn_bin: string;
    only_residential: boolean;
    api_url: string;
    scripts_dir: string;
    cache_days: number;
    max_pool: number;
    mirror_urls: string[];
    extra_urls: string[];
  };
  xui: {
    db_path: string;
    script_path: string;
    inbound_port_base: number;
    inbound_prefix: string;
    outbound_prefix: string;
    host: string;
    auto_restart: boolean;
  };
  warp: WarpCfg;
  sources: { name: string; kind: string; url: string; protocol: string | null; enabled: boolean }[];
}

const KEY_STORE = "resi_fanout_api_key";

/**
 * The UI can be served under a random prefix (server.base_path, set by
 * `install.sh --with-tls`). Resolve the API prefix from where index.html
 * actually lives, otherwise every request would hit the root and 404.
 */
const BASE_PATH = (() => {
  let p = location.pathname.replace(/\/+$/, "");
  // 页面可能是 /<base>/index.html 或直接 /index.html 打开的，
  // 末段带 "." 就是文件名，不能算进 API 前缀，否则请求全 404
  const last = p.split("/").pop() ?? "";
  if (last.includes(".")) {
    p = p.slice(0, p.lastIndexOf("/"));
  }
  // 上面的 replace 已把 "/foo/" 收成 "/foo"、把 "/" 收成 ""，
  // 所以这里直接返回即可，不再需要 p === "/" 的兜底
  return p;
})();

const url = (path: string): string => `${BASE_PATH}${path}`;

export function getKey(): string {
  return localStorage.getItem(KEY_STORE) || "";
}
export function setKey(k: string): void {
  localStorage.setItem(KEY_STORE, k);
}

async function req<T>(method: string, path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = {};
  const key = getKey();
  if (key) headers["Authorization"] = `Bearer ${key}`;
  if (body !== undefined) headers["Content-Type"] = "application/json";
  // every API call goes through here, so the base prefix is applied once
  const res = await fetch(url(path), {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body)
  });
  if (res.status === 401) {
    throw new Error("未授权：请在右上角填写 API Key");
  }
  if (!res.ok) {
    const text = await res.text().catch(() => "");
    throw new Error(`HTTP ${res.status}: ${text || res.statusText}`);
  }
  // 全局唯一一处类型断言：请求打在同源、带 API Key 的管理接口上，
  // 响应 schema 由下面各 wrapper 的返回类型（与后端 api.rs 一一对应）保证，
  // 不在传输层再做一次运行时校验
  return res.json() as Promise<T>;
}

export interface ClashNode {
  index: number;
  name: string;
  kind: string;
  server: string;
  port: number;
  addresses: string[];
  mtu?: number | null;
  sni?: string | null;
}

export interface WarpStatus {
  enabled: boolean;
  profile_present: boolean;
  tools: { wg_quick: boolean; wg: boolean; wgcf: boolean };
  up: boolean;
  tun_ip?: string | null;
  local_port: number;
  exit_ip?: string | null;
  country?: string | null;
  country_code?: string | null;
  isp?: string | null;
  latency_ms?: number | null;
  hosting?: boolean | null;
  last_check?: number | null;
  error?: string | null;
  xray_outbound?: unknown;
}

export const api = {
  status: (): Promise<Status> => req("GET", "/api/status"),
  proxies: (query: string): Promise<{ total: number; items: ProxyItem[] }> =>
    req("GET", `/api/proxies?${query}`),
  ports: (): Promise<{ items: PortEntry[] }> => req("GET", "/api/ports"),
  portsAssign: (keys: string[]): Promise<{ ok: boolean; assigned: { key: string; port: number }[] }> =>
    req("POST", "/api/ports/assign", { keys }),
  portsRelease: (p: { ports?: number[]; keys?: string[] }): Promise<{ ok: boolean; released: number }> =>
    req("POST", "/api/ports/release", p),
  refresh: (): Promise<{ ok: boolean }> => req("POST", "/api/refresh"),
  check: (keys?: string[]): Promise<{ ok: boolean }> =>
    req("POST", "/api/check", keys && keys.length ? { keys } : {}),
  config: (): Promise<Config> => req("GET", "/api/config"),
  saveConfig: (c: Config): Promise<{ ok: boolean }> => req("PUT", "/api/config", c),
  snippet: (query: string): Promise<Snippet> => req("GET", `/api/3xui/snippet?${query}`),
  vpngate: (): Promise<VpngateInfo> => req("GET", "/api/vpngate"),
  vpngateRebuild: (): Promise<{ ok: boolean }> => req("POST", "/api/vpngate/rebuild"),
  xuiInbounds: (): Promise<XuiInfo> => req("GET", "/api/xui/inbounds"),
  xuiPreview: (body: { template_id: number; ports?: number[]; residential_only?: boolean }): Promise<XuiInfo> =>
    req("POST", "/api/xui/preview", body),
  xuiLink: (body: { template_id: number; ports?: number[]; residential_only?: boolean; host?: string }): Promise<XuiInfo> =>
    req("POST", "/api/xui/link", body),
  xuiUnlink: (): Promise<XuiInfo> => req("POST", "/api/xui/unlink"),
  warp: (): Promise<WarpStatus> => req("GET", "/api/warp"),
  warpRegister: (license?: string): Promise<{ ok: boolean; msg: string }> =>
    req("POST", "/api/warp/register", license ? { license } : {}),
  warpImport: (config: string): Promise<{ ok: boolean; endpoint?: string; msg: string }> =>
    req("POST", "/api/warp/import", { config }),
  warpConnect: (): Promise<{ ok: boolean; msg: string }> => req("POST", "/api/warp/connect"),
  warpDisconnect: (): Promise<{ ok: boolean; msg: string }> => req("POST", "/api/warp/disconnect"),
  warpImportClash: (yaml: string): Promise<{ ok: boolean; count: number; nodes: ClashNode[] }> =>
    req("POST", "/api/warp/import-clash", { yaml }),
  warpApplyClash: (
    yaml: string,
    index: number,
    mode: "masque" | "wireguard"
  ): Promise<{ ok: boolean; mode: string; node?: string; port?: number; hint?: string; sidecar_started?: boolean }> =>
    req("POST", "/api/warp/apply-clash", { yaml, index, mode })
};

export interface Snippet {
  prefix: string;
  mode: string;
  ports: number[];
  outbounds: unknown[];
  rules_example: unknown[];
  balancer_extra: unknown;
  full_template: unknown;
  usage: string;
}

export function fmtTs(ts?: number | null): string {
  if (!ts) return "—";
  return new Date(ts * 1000).toLocaleString("zh-CN", { hour12: false });
}

export function fmtUptime(secs: number): string {
  const d = Math.floor(secs / 86400);
  const h = Math.floor((secs % 86400) / 3600);
  const m = Math.floor((secs % 3600) / 60);
  if (d > 0) return `${d}天${h}时${m}分`;
  if (h > 0) return `${h}时${m}分`;
  return `${m}分${Math.floor(secs % 60)}秒`;
}
