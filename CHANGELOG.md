# Changelog

本项目遵循 [SemVer](https://semver.org/lang/zh-CN/)。当前处于 **1.0.x** 稳定线：

| 版本区间 | 含义 |
|---|---|
| 1.0.z | 修补版：修 bug、安装脚本/文档调整，不新增功能 |
| 1.y.0 | 新功能（y+1），向后兼容 |
| 2.0.0 | 有破坏性变更 |

## v1.0.1 — 全量审查修复

对全部 39 个文件做了一次完整审查并修复下列问题。均为修补版，无新增功能。

### 安全
- **openvpn：第三方配置清洗可绕过（可远程执行任意脚本）**。原实现只匹配 `"up "` 这类带空格的字面前缀，而 OpenVPN 的空格与 Tab 都是参数分隔符，`up\t/tmp/evil.sh` 可绕过；`plugin`/`config`/`setenv` 原本根本不在黑名单。改为按解析后的指令名匹配，并新增 `crl-verify`（其第三个参数会被当作命令执行）。
- **openvpn：畸形内联标签会让清洗器自我关闭**。`<ca`（缺 `>`）曾使文件剩余部分进入透传模式。现在只有 `INLINE_TAGS` 里格式正确的开标签才进入透传，遇到任何闭标签即退出。
- **sources：SSRF**。恶意代理源可把条目指向 `127.0.0.1`、内网或 `169.254.169.254`，把服务变成跳板。现在**所有**解析路径（含此前完全绕过校验的 monosans/geonode JSON 源）统一过滤；同时拒绝主机名（否则等同于换个途径的 SSRF）与 IPv4 映射 IPv6（`::ffff:127.0.0.1`）。
- **config：读取失败时静默降级为默认配置**，默认 `api_key` 为空，一次 EACCES 就会让整个 `/api` 免鉴权上线，并试图用默认值覆盖用户配置。现在仅「文件不存在」才回落，写配置改为临时文件 + rename。
- **relay：握手与中继无超时**，少量慢连接即可耗尽 fd 拖垮整个端口。
- **warp：WireGuard 私钥与 mihomo 配置以 0644 落盘**，全机可读。
- **tls：公网 HTTPS 监听缺少读超时**。
- **checker：`classify_url` 配成 `https://` 时**会往 443 发明文请求，把整池判死；**HTTP 429 也不再算作「代理已死」**。

### 正确性
- **warp-up.sh / warp-down.sh：WARP 策略路由从未生效**。二者读的都是 OpenVPN 的脚本变量名（`dev`/`address`/`ifconfig_local`），wg-quick 并不导出。warp-up 因此永远在装路由前 `exit 0`（叠加 `Table = off`，扇出端口实际从未走隧道）；warp-down 则在 `set -u` 下中止，清理从不执行、持续泄漏 `ip rule`。
- **api：TLS 证书加载失败的降级分支必然 `EADDRINUSE`**，导致证书损坏时进入 systemd 重启循环 —— 正是该分支注释声称要避免的情况。
- **scheduler：`busy` 只在直线成功路径清零**，周期任务内任何 panic 都会让 `/api/refresh`、`/api/check` 永久 409。改由 Drop guard 释放。
- **3xui-push.sh：空 `api_key` 时 `KEY` 变成字面量 `''`** 并仍发送 `Authorization: Bearer ''`；面板备份改用 `VACUUM INTO`（裸 `cp` 在 WAL 模式下会丢数据）。
- **install.sh：`--no-tls` 重装不会还原 `listen`/`tls`**，服务仍监听 `0.0.0.0`，摘要行还把 HTTPS 报成 `http://`。
- **openvpn：隧道有启动截止时间**（`resolv-retry infinite` 下不再永久占死槽位）；销毁改用 SIGTERM，`down` 钩子得以执行。
- **relay：中继超时改用单调时钟**，避免时钟回拨使超时失效、时钟前跳同时切断所有会话。
- **vpngate：`cert_country` 的偏移量按实际匹配到的模式计算**，「C = JP」形式（注释里写明支持）此前恒解析失败，配了国家白名单时这些节点被静默剔除。
- **warp：`normalize_key` 真正实现「唯一候选才接受」**的多算法密钥拒绝。

### 供应链
- **install.sh：应用 tarball 校验 sha256，但 `wgcf`/`mihomo`/`lego`/`NodeSource` 四个以 root 身份装入 `/usr/local/bin` 的来源此前零校验**。新增 `fetch_verify`，有校验文件或 `*_SHA256` 环境变量时强制校验，缺失时明确告警。

### 前端与 CI
- 修复「代理源」文本框从未被保存逻辑读取导致的**静默丢数据**；复制链接硬编码 `socks5://`；已写入面板时「本地端口」列显示 inbound tag。
- 3x-ui 直连模式的 `residential_only` 保持为 `false`（曾被误改为 `true`，会让未分类端口被静默丢弃、预览为空）。
- CI：第三方 action 钉到完整 SHA、补 `permissions: contents: read`、shellcheck 不再 `|| true`、`version-sync` 补 `tags:` 触发、补 `timeout-minutes` 与 `concurrency`；发布改为全部架构构建成功后才创建 Release。
- `frontend/dist` 保持入库（install.sh 在无 npm 时依赖它兜底），不加入 `.gitignore`。

## v1.0.0 — 首个正式版

从 0.x 开发线一路迭代而来的首个稳定版本，包含以下完整能力：

### 节点来源
- 9 个免费代理源（monosans / TheSpeedX ×3 / hideip.me ×3 / proxifly / geonode），支持自定义源（付费住宅服务商的提取 URL 即可直接接入）
- **健康检测 + 住宅识别**：通过每个代理自身请求 ip-api.com，拿到出口 IP / 国家 / ISP / `hosting` 标志，`hosting=false` 判定为住宅线路
- **VPN Gate**：官方 API（HTTPS → HTTP → 快照镜像 → 本地快照兜底）四级回退，节点池累积缓存（默认 30 天），OpenVPN 旁挂隧道（`route-nopull` + 源地址策略路由，不影响主机路由）
- **Cloudflare WARP**：`wgcf register`（支持 WARP+ 许可）或粘贴 WireGuard 配置，隧道自动起本地 SOCKS 端口
- **MASQUE 节点**：可从 Clash/Mihomo 配置导入，由 mihomo 旁挂消费（Cloudflare 多算法密钥容器只有 mihomo 能正确处理）

### 出口分发
- 每个存活出口一个本地端口（默认 20000+），延迟最低的拿最低端口，死掉自动回收并重新分配
- 本地端口支持 SOCKS5 / HTTP / mixed
- WARP / MASQUE 隧道端口与代理端口统一进端口列表

### 3x-ui 集成
- **入站接管**（fanout 式）：为每个出口克隆一条面板入站（沿用模板的协议/流控/UUID），自动写入 socks 出站与路由规则，生成客户端链接；兼容 3x-ui v2/v3 两种数据库布局
- **负载均衡模式**：Xray `observatory` + `balancers`（leastPing），单个入站在所有出口间轮换
- 一键推送脚本 `3xui-push.sh`（合并出站 / `--link-inbounds` 入站接管 / `--unlink` 解绑），写库前自动备份

### 部署与安全
- 一键安装：自动匹配架构下载预编译包（无需编译工具链），默认签发 **ACME IP 证书**（Let's Encrypt 面向 IP 的 6 天证书）并开启公网 HTTPS + 随机访问路径，失败自动降级为仅本机 HTTP
- 证书热重载 + 12 小时自动续期定时器（`resi-fanout-acme.timer`）
- 默认安装 openvpn，VPN Gate 默认启用；systemd 默认授予隧道所需 `CAP_NET_ADMIN`
- 随机 base path + API Key 双层访问保护；TLS 层独立实现（可选 cargo feature）
- 全局管理命令 `rf`：交互式菜单（状态 / 启停 / 抓取 / 日志 / 更新 / 卸载），也支持子命令脚本化调用
- Web UI（原生 TypeScript + Vite）：总览、节点池、本地端口、VPN Gate、CF WARP、配置、3x-ui 联动

> 说明：开发期间曾产生过一批临时版本号（v1.0.1 ~ v1.7.4），均已合并到本首个正式版，对应的代码演进记录保留在 git 历史中。