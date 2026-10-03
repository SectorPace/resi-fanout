# Resi-Fanout

[![CI](https://github.com/SectorPace/resi-fanout/actions/workflows/ci.yml/badge.svg)](https://github.com/SectorPace/resi-fanout/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/tag/SectorPace/resi-fanout?label=release)](https://github.com/SectorPace/resi-fanout/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

> 参考 [byJoey/fanout](https://github.com/byJoey/fanout) 的思路：把上游代理"扇出"成一批本地端口，供 3x-ui / Xray 作为出站使用。
>
> Rust 后端 + TypeScript 前端 + Linux 一键脚本。

## 它做什么

```
 免费代理源(可扩展)          存活检测 + 住宅识别              本地端口扇出            3x-ui
┌─────────────────┐   ┌──────────────────────┐   ┌────────────────────┐   ┌──────────────┐
│ monosans json   │   │ 通过代理请求 ip-api    │   │ 127.0.0.1:20000 ──►│   │ Xray outbound│
│ TheSpeedX txt   │──►│ · 连通性 / 延迟        │──►│ 127.0.0.1:20001 ──►│──►│ socks 127.0. │
│ hideip.me txt   │   │ · 出口 IP / 国家       │   │   ...每节点一个端口 │   │ 0.1:2000x    │
│ geonode api     │   │ · hosting=false →住宅 │   │ (socks/http/mixed) │   │ + 路由规则    │
│ 你的付费源 URL   │   └──────────────────────┘   └────────────────────┘   └──────────────┘
└─────────────────┘
```

- **自动抓取**：内置 9 个免费代理源（monosans / TheSpeedX / hideip.me / proxifly / geonode），支持任意输出 `ip:port` 或 `proto://ip:port` 的自定义 URL —— **付费住宅代理服务商的提取链接也能直接填进去**。
- **健康检查 + 住宅识别**：每个代理用它自己去请求 `ip-api.com`，拿到出口 IP、国家、ISP 和 `hosting` 标志；`hosting=false` 即判定为**住宅/家宽**线路，机房 IP 会被标记。
- **扇出到本地端口**：每个存活代理绑定一个本地端口（默认 `127.0.0.1:20000+`），本地口支持 SOCKS5 / HTTP / mixed。延迟最低的代理拿最低的端口，死掉自动回收。
- **VPN Gate 隧道（可选）**：接入 [VPN Gate](https://www.vpngate.net) 公共中继列表（大量家宽志愿者节点），自动挑选最优服务器拉起 OpenVPN 旁挂隧道，**每条隧道一个本地 SOCKS 端口**，隧道出口同样做住宅识别，机房出口可自动换点。
- **接入 3x-ui**：两种方式
  1. Web UI「接入 3x-ui」页一键生成 Xray `outbounds` / 路由规则，粘进面板的 Xray 配置即可；
  2. `scripts/3xui-push.sh` 自动写入 3x-ui 数据库（`settings.xrayTemplateConfig`，先备份）并重启面板。
- **调度**：定时抓取新列表、复检全池、淘汰死节点，全部持久化到 `state.json`，重启不丢。

## 快速开始（Linux）

```bash
# 一键安装（自动匹配架构下载 Release 预编译包，无需装工具链）
curl -fsSL https://raw.githubusercontent.com/SectorPace/resi-fanout/main/install.sh | sudo bash

# 带参数的一键安装（注意 bash -s -- 后跟参数）
curl -fsSL https://raw.githubusercontent.com/SectorPace/resi-fanout/main/install.sh | sudo bash -s -- --with-vpngate --with-warp --with-3xui

# 或者 clone 后本地运行
git clone https://github.com/SectorPace/resi-fanout.git && cd resi-fanout
sudo bash install.sh
```

```bash
# 常用参数
--port 7654        # 指定 API/UI 端口
--with-3xui        # 装完自动把出站推进本机 3x-ui
--with-vpngate     # 装 openvpn 并启用 VPN Gate 隧道
--from-source      # 强制源码编译（预编译包要求 glibc >= 2.35，Debian 12 / Ubuntu 22.04+；更老系统用这个）
--no-frontend      # 跳过 npm 构建（用仓库自带 dist）
--repo <git-url>   # 指定仓库地址
```

脚本会自动：装 Rust/Node 工具链 → 构建前后端 → 生成配置（随机 API Key）→ 注册 systemd 服务 `resi-fanout`。内存 <1GB 的小鸡会自动加 2G swap 保证编译不 OOM。

安装完成后：

| 项目 | 路径 |
|---|---|
| API / Web UI | `http://127.0.0.1:7654`（建议 `ssh -L 7654:127.0.0.1:7654` 隧道访问） |
| 配置 | `/etc/resi-fanout/config.json` |
| 数据 | `/var/lib/resi-fanout/state.json` |
| 日志 | `journalctl -u resi-fanout -f` |

首次启动会立即抓取 + 检测一轮（几千个候选约 1–3 分钟），打开 UI「总览」看进度。

## 接入 3x-ui

### 方式 A：接管面板入站（fanout 式，推荐）

为**每个出口克隆一条面板入站**：客户端连不同入站 → 从不同国家/住宅 IP 出去（和 byJoey/fanout 一样）。写库前自动备份 `x-ui.db`，完成后可选自动重启面板。

UI 里：「接入 3x-ui」页 → 加载面板入站 → 选一个模板入站（沿用它的协议/流控/UUID）→ 预览 → 执行联动。也可用命令行：

```bash
bash /opt/resi-fanout/scripts/3xui-push.sh --api http://127.0.0.1:7654 --key <KEY> \
    --link-inbounds --template-id 1 --host panel.example.com

bash /opt/resi-fanout/scripts/3xui-push.sh --key <KEY> --unlink     # 解绑清理
```

联动会做三件事：克隆入站（新端口从 `xui.inbound_port_base` 起分配，tag `resi-in-<端口>`）→ 写入 `resi-<端口>` socks 出站 → 加 `入站 → 出站` 路由规则。**兼容 3x-ui v2/v3 两种数据库布局**（v2 客户端内嵌在 settings JSON，v3 是独立 clients 表并自动复制客户端），客户端链接（vless/trojan/ss/socks）一并生成。

### 方式 B：手动粘贴出站配置

UI「接入 3x-ui」页可生成 Xray `outbounds` / 路由规则，粘进面板的 Xray 配置即可。生成的出站长这样（每个端口一条）：

```json
{ "tag": "resi-20000", "protocol": "socks",
  "settings": { "servers": [ { "address": "127.0.0.1", "port": 20000 } ] } }
```

### 方式 C：一个入站在所有出口间轮换（负载均衡）

选「负载均衡」模式会额外生成 Xray `observatory`（探测 `resi-` 前缀出站）+ `routing.balancers`（`leastPing` 策略），配合一条 `inboundTag → balancerTag: resi-bal` 规则，让单个入站自动在全部住宅出口间按延迟轮换。

### 方式 D：脚本一键合并出站（不建入站）

```bash
bash /opt/resi-fanout/scripts/3xui-push.sh \
    --api http://127.0.0.1:7654 --key <API_KEY>

# 只推住宅端口，并把某入站分流到指定出站：
bash scripts/3xui-push.sh --key <KEY> --residential \
    --rule-inbound "vmess-in,trojan-in" --outbound resi-20000
```

脚本逻辑：从 API 拉取片段 → 合并进 `/etc/x-ui/x-ui.db` 的 `xrayTemplateConfig`（同 tag 替换、新 tag 追加，写库前自动备份）→ 重启 x-ui。

### 客户端直连测试

不经过 3x-ui 也可以直接用这些端口：

```bash
curl --socks5-hostname 127.0.0.1:20000 http://ip-api.com/json
```

## 公网访问 + ACME IP 证书

想让 UI/API 从公网访问，推荐用 **ACME 签发的 IP 证书**（Let's Encrypt 从 2026-01 起已正式开放 IP 证书，6 天有效期）：

```bash
sudo bash install.sh --with-tls
```

它会做这些事：

- 探测公网 IP，安装 [lego](https://github.com/go-acme/lego)（支持 RFC 8738 IP 证书），向 Let's Encrypt 申请 **面向 IP 的证书**（HTTP-01，需要 80 端口能从公网访问；失败会给出手动/DNS-01 提示）；
- 证书落到 `/etc/resi-fanout/tls/{fullchain.pem,privkey.pem}`，服务直接跑 HTTPS（axum 0.7 没有 TLS，TLS 层我们自己用 rustls 实现）；
- **证书热重载**：6 天证书会被频繁替换，服务每 5 分钟检查文件变化，换证书不用重启、不断连接；
- 注册 `resi-fanout-acme.timer` 每 12 小时自动续期；
- 自动生成**随机访问路径**（如 `/Kf3x9a71`）并监听 `0.0.0.0`，访问 `https://<你的IP>:端口/随机路径`。

暴露公网时请注意：

- 一定保留安装时生成的 **API Key**（所有 `/api` 请求都要 `Authorization: Bearer <key>`）；
- 随机路径只是减少被扫描发现，**不能替代鉴权**；
- 端口不一定要开 443：`--port 7654` 可以任选，但要记得在防火墙/安全组放行；
- 也可以完全手工：把任意证书路径填进 `server.tls` 后重启服务即可。



[VPN Gate](https://www.vpngate.net) 是筑波大学的学术实验项目，公共中继里包含大量**家庭宽带的志愿者节点**（日本/韩国/美国尤多），适合补充海外住宅线路。开启后每台选中的服务器会拉起一条 OpenVPN 旁挂隧道，**每条隧道一个本地 SOCKS 端口**（默认 `21000+`），与代理端口一起统一接入 3x-ui。

```bash
sudo bash install.sh --with-vpngate     # 装 openvpn + 启用
# 或手动: apt install openvpn 后把 config.json 里 vpngate.enabled 改为 true 并重启服务
```

工作方式：

- 定时抓取官方列表（速度 / Ping / 会话数 / 是否记日志），按 `score` 排序，按 `vpngate.countries` 国家白名单与 `min_speed_mbps` 过滤，自动选 `max_servers` 台；
- **节点累积**：官方 API 单次只返回约 100 台在线节点且不支持分页，所以见过的节点全部缓存（`cache_days` 天内），下线后仍可择机重连——持续运行一天通常能积累到数百台可选节点，UI 里显示「在线 N 台 / 累计缓存 M 台」；
- **多源回退**：官方 API（HTTPS → HTTP）→ `vpngate.mirror_urls` 快照镜像 → 额外 OVPN 源；每个响应都会做**快照校验**（必须含 `OpenVPN_ConfigData_Base64` 表头、大小/行数上限），错误页不会污染节点池；全部失败时回落到本地快照 `/var/lib/resi-fanout/vpngate/snapshot.csv`（带 sha256 与抓取源记录，UI 里能看到生效源和新鲜度）。多源之间还会**合并去重**（官方 100 + 镜像 99 实测合并出 195 台）。
- **额外源**：`vpngate.extra_urls` 可填任意返回原始 OpenVPN 配置文本的 URL（VPN Gate 镜像站、社区配置合集等），单个 .ovpn 或多段配置拼接都支持，国家码从证书 `C=` 里提取；速度未知的节点不参与速度过滤；
- 每条隧道 `route-nopull` + **源地址策略路由**（`ip rule from <tun-ip> lookup <table>`，table = 本地端口号），主机默认路由完全不受影响，断开时自动清理；
- 隧道建立后用同一套 ip-api 逻辑做**出口住宅识别**，UI 里显示出口国家/ISP/是否住宅；`vpngate.only_residential: true` 时机房出口自动杀掉换下一台；
- 连接失败重试 3 次后自动轮换下一个候选，服务器从池中消失也会自动摘除。

**要求**：`openvpn` 已安装；服务需要 root 或 `CAP_NET_ADMIN`（`--with-vpngate` 安装的 systemd 单元已自动授予 `AmbientCapabilities=CAP_NET_ADMIN CAP_NET_RAW`）；`/dev/net/tun` 可用（KVM/物理机没问题，部分 LXC/OpenVZ 容器默认禁用 tun）。UI 的 VPN Gate 页会显示每台服务器的日志策略（「不记录」= 该志愿者声明不留活动日志）。

## Cloudflare WARP（WireGuard 出口）

WARP 作为另一种隧道来源，接入方式和 VPN Gate 一样：起来后占一个本地 SOCKS 端口（默认 `22000`），自动进入 `/api/ports`、3x-ui 入站联动和负载均衡。

```bash
sudo bash install.sh --with-warp    # 装 wireguard-tools + wgcf
```

两种拿配置的方式：

1. **官方 wgcf 注册**（UI「VPN Gate」页填 WARP+ 许可 key → 点「用 wgcf 注册」）。Cloudflare 的注册接口有 TLS 指纹校验，所以直接调官方工具最稳；WARP+ 许可会透传给 `wgcf register --license <key>`。
2. **粘贴现成配置**：把 wgcf 生成的 `wgcf-profile.conf` 或手写配置粘进文本框点「导入这份配置」：

```ini
[Interface]
PrivateKey = <你的私钥>
Address = 172.16.0.2/32
Address = 2606:4700:110:8b49:6e47:6be6:43c7:999c/128
DNS = 162.159.193.10

[Peer]
PublicKey = bmXOC+F1FxEMF9dyiK2H5/1SUtzH0JuVo51h2wPfgyo=
Endpoint = engage.cloudflareclient.com:2408
AllowedIPs = 0.0.0.0/0, ::/0
PersistentKeepalive = 60
```

实现要点：托管配置用 `Table = off` + 我们自己的 `warp-up.sh` 源策略路由（表号 = 本地端口），所以**主机默认路由不受影响**；DNS 走宿主解析；隧道出口同样做 ip-api 分类（WARP 出口一般是 Cloudflare 机房 IP，会标「机房」）。私钥只保存在本机 `/var/lib/resi-fanout/warp/warp.conf`，不进仓库。

UI 还会**生成 Xray 原生 wireguard 出站**（就是上面那份配置转成 outbound JSON，复制进 3x-ui 的 Xray 配置即可用），两条路线可以任选或并用：

```json
{ "tag": "warp", "protocol": "wireguard",
  "settings": { "secretKey": "<私钥>", "address": ["172.16.0.2/32", "2606:4700:...:999c/128"],
    "mtu": 1280,
    "peers": [{ "publicKey": "bmXOC+F1FxEMF9dyiK2H5/1SUtzH0JuVo51h2wPfgyo=",
      "endpoint": "engage.cloudflareclient.com:2408",
      "allowedIPs": ["0.0.0.0/0", "::/0"], "keepAlive": 60 }] } }
```

### 从 Clash / Mihomo 配置导入 MASQUE 节点

新版 WARP 还有 MASQUE 传输（HTTP/3 CONNECT-UDP），在 Clash/Mihomo 里是 `type: masque`。UI「VPN Gate」页有导入框：把配置里的 `proxies` 段整段粘进去 → 解析节点 → 选一个 → **按 MASQUE 应用**。

```bash
sudo bash install.sh --with-masque   # 顺便下载 mihomo（原生 MASQUE 实现）
```

要点：

- **MASQUE 节点的密钥是 Cloudflare 的多算法容器**（DER 里同时带 X25519 与 X448），只有 Mihomo 能正确消费，所以这类节点一律走 Mihomo 旁挂：resi-fanout 生成一份只含该节点的最小 mihomo 配置、拉起 mihomo，它的 mixed-port 就成为一个新的本地出口端口（默认 `22100`），同样进入 3x-ui 入站联动。
- 导入时密钥**原样保留**（逐字节透传，不做任何改写），避免破坏 Cloudflare 的密钥格式；这类节点选「按 WireGuard 应用」会被明确拒绝，因为容器里取不出标准 WireGuard 密钥对。
- 如果你粘的是普通 `type: wireguard` 节点（32 字节裸密钥），可以直接「按 WireGuard 应用」，走 wg-quick 隧道那条路。
- 解析器只取顶层 `proxies:` 段单独解析——Clash 配置里的 YAML 锚点/合并键（`<<: *domain`）会让整篇解析失败。


## 配置说明（config.json）

| 字段 | 默认 | 说明 |
|---|---|---|
| `server.listen` | `127.0.0.1:7654` | API/UI 监听地址 |
| `server.api_key` | 安装时随机 | 非空则所有 `/api` 需 `Authorization: Bearer` |
| `fanout.bind` / `base_port` / `max_ports` | `127.0.0.1` / `20000` / `100` | 扇出端口范围 |
| `fanout.mode` | `socks` | 本地口协议：`socks` / `http` / `mixed` |
| `filter.only_residential` | `false` | **只把住宅 IP 扇出成端口** |
| `filter.countries` | `[]` | 国家白名单，如 `["US","JP"]` |
| `checker.timeout_secs` / `concurrency` | `8` / `256` | 检测超时与并发 |
| `scheduler.refresh_minutes` | `30` | 抓取周期（0=关闭调度） |
| `scheduler.recheck_minutes` | `20` | 全池复检周期 |
| `sources[]` | 9 个免费源 | `kind`: `text` / `monosans` / `geonode` |
| `vpngate.enabled` / `max_servers` | `false` / `3` | VPN Gate 隧道开关与数量 |
| `vpngate.cache_days` / `max_pool` / `extra_urls` | `30` / `800` / `[]` | 节点累积天数、池上限、额外 OVPN 源 |
| `xui.db_path` / `script_path` | `/etc/x-ui/x-ui.db` | 面板数据库与联动脚本路径 |
| `xui.inbound_port_base` / `host` | `31000` / `127.0.0.1` | 联动入站起始端口、客户端链接域名 |

> **关于"住宅代理"**：免费列表里绝大多数是机房 IP，本项目靠 `ip-api.com` 的 `hosting` 标志把住宅/家宽节点**识别并筛选**出来（UI 中标「住宅」，可 `only_residential: true` 只扇出住宅）。想要稳定的高质量住宅线路，建议把付费服务商的提取 URL 加进 `sources`（输出 `ip:port` 即可），检测和住宅判定逻辑完全通用。

## 手动运行（不装 systemd）

```bash
cd backend
cargo build --release
./target/release/resi-fanout serve --config config.json --data ./data --web ../frontend/dist

# 一次性抓取+检测后退出（适合 cron）
./target/release/resi-fanout refresh --config config.json --data ./data
```

## 开发

```bash
# 后端
cd backend && cargo check        # Linux 默认 rustls，无需系统 OpenSSL
# Windows 本机调试（Schannel，无需 C 工具链）：
cargo check --no-default-features --features tls-native

# 前端
cd frontend && npm install && npm run build   # 产物在 frontend/dist
npm run dev                                    # vite 代理 /api → 127.0.0.1:7654
```

## 安全提示 / 免责声明

- 所有本地端口默认只绑 `127.0.0.1`，不要把 `fanout.bind` 或 API 暴露到公网；API 务必设置强 `api_key`。
- 免费公共代理是**不可信第三方**：请勿通过它们传输登录凭据、隐私数据等敏感流量。本工具仅做可达性转发，不改变流量的加密与信任模型（HTTPS 端到端加密不受影响，HTTP 明文流量对代理可见）。
- 请遵守当地法律法规，仅用于自有流量的合规用途。抓取的公共代理归原所有者所有，感谢 monosans / TheSpeedX / hideip.me / proxifly / geonode 等项目。
- 许可证：MIT。
