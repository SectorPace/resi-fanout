# Changelog

本项目遵循 [SemVer](https://semver.org/lang/zh-CN/)。当前处于 **1.0.x** 稳定线：

| 版本区间 | 含义 |
|---|---|
| 1.0.z | 修补版：修 bug、安装脚本/文档调整，不新增功能 |
| 1.y.0 | 新功能（y+1），向后兼容 |
| 2.0.0 | 有破坏性变更 |

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