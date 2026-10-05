# Changelog

本项目遵循 [SemVer](https://semver.org/lang/zh-CN/)。当前处于 **1.0.x** 稳定线：

| 版本区间 | 含义 |
|---|---|
| 1.0.z | 修补版：修 bug、安装脚本/文档调整，不新增功能 |
| 1.y.0 | 新功能（y+1），向后兼容 |
| 2.0.0 | 有破坏性变更 |

## v1.0.2 — 第三轮全量审查修复

对全部 46 个文件再做一次完整审查（编译产物与生成文件除外），修复下列问题。

### 安全
- **openvpn：清洗器仍可绕过（可远程执行任意脚本）**。上一轮按 `[' ', '\t']` 切分指令名，但 OpenVPN 的 `parse_line()` 用的是 C 的 `isspace()`，并且会剥掉一层引号。因此 `up\f/tmp/evil.sh`、`up\r/tmp/evil.sh`、`"up" /tmp/evil.sh` 等写法都会被当成一个不透明的整体 token，**不在**黑名单里而被原样写入 `.ovpn`——而 OpenVPN 仍会把它解析成 `up` + `/tmp/evil.sh` 并执行。叠加 `script-security 2` 与 `CAP_NET_ADMIN`，等于以服务用户身份远程执行代码。改为按 OpenVPN 的分词规则切分（`first_param`，含全部 C 空白与引号剥离），并补齐遗漏的执行/dlopen 类指令：`route-pre-down`、`client-disconnect`、`tls-crypt-v2-verify`、`dns-updown`、`auth-user-pass-verify`、`providers`、`pkcs11-providers`。
- **api：Bearer token 比较不是常数时间**。`str` 的 `PartialEq` 走 `memcmp`，会在首个不同字节处短路。这是整个管理面（`PUT /api/config` 可改写 `api_key`/`listen`/`tls.enabled`）唯一的凭据关卡，而项目本就是设计为在 `0.0.0.0` 上对外提供。改为 XOR 累积比较。
- **warp：`PostUp` 里的 shell 注入**。`warp.interface` 与 `scripts_dir` 被原样插入 `PostUp = <script> up <iface>`，而 wg-quick 用 `sh -c` 执行它。新增接口名与路径的字符白名单校验（可达性仅限能写 config.json，但爆炸半径贴着 root）。
- **uninstall.sh：`rm -rf /tmp/tmp.*` 会删掉本机其它并发进程的临时目录**。`mktemp -d` 的默认模板恰好就是这个形状。已删除该行。

### 正确性
- **api：`ports_assign` 自死锁，整个服务被卡死**。该 handler 在持有 `state.cfg` **写锁**的情况下调用 `save_config()`，而后者第一句就是 `self.cfg.read().await`；tokio 的 RwLock 写优先，同一任务持写锁再申请读锁会永久阻塞。后果不是这一个请求挂起，而是 `/api/status`、`/api/proxies`、调度循环、中继监管、检测器全部永久阻塞。且 `auto_assign` 默认就是 `true`，默认配置下点一次「开放端口」即触发。
- **install.sh：lego 装错了地方，TLS 流程从来不可能成功**。`LEGO_BIN` 指向的是安装器自己的临时目录，而 `fetch_verify` 的最后一步是 `install -m 755 <src> <dest>`；`PATH` 没有包含该目录，`trap cleanup EXIT` 又会删掉它。于是日志打印「lego 已安装：/usr/local/bin/lego」，实际执行 `lego` 得到 command not_found。由于 lego 不在四个目标发行版的默认仓库里，`command -v lego` 通常失败，**这就是默认路径**——`--with-tls` 下的默认安装会静默降级成本机 HTTP。
- **install.sh：`rf api()` 无法与 TLS 服务通信**。URL 硬编码 `127.0.0.1`，而证书是 lego 为公网 IP 签的（唯一 SAN 是 `iPAddress:<公网IP>`），Python 默认上下文做完整身份校验，每次都 `CERTIFICATE_VERIFY_FAILED`。更糟的是 `status_panel` 把 stderr 丢掉、下游 python 又是裸 `except`，于是菜单永远显示「运行中」而代理统计恒为 0。改为关闭身份匹配但保留证书链校验，并让失败以非零退出码上报。
- **install.sh：`install_pkgs` 在定义之前被调用**。它在第 258 行被调用、到第 280 行才定义，bash 只在定义语句执行时绑定函数名，所以「无预编译包 → 回退源码构建」这条路径会以 `command not found` 终止并给出误导性的「git is required」。定义与 `PKG` 探测已上移到调用点之前。
- **install.sh：GitHub API 失败会中止整个安装**。`LATEST_TAG="$(curl … | python3 …)"` 在 `pipefail` 下让 curl 的 22 胜出，errexit 直接终止，`releases/latest` 回退分支永远走不到；而未认证的 api.github.com 只有 60 次/时/IP。
- **warp：MASQUE 扇出端口与 mihomo 边车端口是同一个**。mihomo 已用 `mixed-port` 占用该端口，我们的扇出监听器再绑一次：要么 EADDRINUSE 导致 masque 端口永远发布不出来（3x-ui 联动失效），要么我们的监听器赢下端口、其上游指向自己，客户端每接入一次就再进一次监听器并再次拨向同一端口，直到 fd 耗尽拖垮 API。新增 `warp.masque_port`，并让 `ensure_proxy_listener` 分别接收扇出端口与上游端口，使二者不可能再被混用。
- **xui_db.py / 3xui-push.sh：面板备份在 WAL 下丢数据**。`VACUUM INTO` 需要 SQLite ≥ 3.27（CentOS 7 是 3.7.17），且目标已存在时报错——备份名只有秒级精度，同一秒跑两次就静默退化到 `shutil.copy2`，而后者在 WAL 下只拿到主库文件，丢掉全部未 checkpoint 的已提交事务。改用 SQLite 自身的 backup API，并在备份失败时中止而不是继续改面板。
- **vpn-up.sh：每次失败退出都泄漏一条 `ip rule`**。`exit 1` 是故意用来让 OpenVPN 放弃节点的，而重连每次都换一个新隧道 IP，`ip rule add` 不会去重。三处失败退出现在都会先回滚。`vpn-down.sh` 也改成有界循环删重复项（原来单次 `del` 只删一条）。
- **tls：证书重载失败后永不重试**。`last = now` 在尝试之前就被赋值，而 `fingerprint()` 只是证书文件的 (len, mtime)，于是失败后每个 tick 都认为「没变化」。ACME 续期时 cert/key 是成对写入的，落在两次写之间的 tick 会读到不匹配的密钥——对一张 6 天的证书来说，服务会在整个续期窗口里继续提供**已过期**的证书，只留一条 warn。
- **config：手工编辑的 config.json 绕过全部校验**。这些不变量原先只存在于 `PUT /api/config` 里，`#[serde(default)]` 让 `{"fanout": {"max_ports": 99999999}}` 这类文件照常启动。校验已提取为共享的 `Config::validate()`，读写两条路径都调用。悬空符号链接也不再被当成「文件不存在」而被写入默认配置（默认 `api_key` 为空）。
- **xui_db.py：reality 入站 `shortIds: []` 触发 IndexError**。`dict.get(key, default)` 的默认值只在键**不存在**时生效。
- **vpngate：`min_speed_mbps * 1_000_000` 整数溢出**。该值来自配置且未做上限裁剪；release 构建会静默回绕成很小的阈值、debug 构建直接 panic，而 `rank` 跑在中继监管的 `tokio::spawn` 里，panic 会带走唯一持有 `running` 的任务。改用 `saturating_mul`。
- **relay / xui / sources**：扇出监听器每接入一个连接就 spawn 一个无上限任务（完成 SOCKS5 握手后可占用 2 个 fd 长达 10 分钟），现按 `tls::MAX_CONNECTIONS` 的先例加上限；xui 的端口→代理映射改为显式「先到先得」（原先 `HashMap::values()` 的迭代顺序不定，`collect()` 保留的是最后一个，与被替换掉的线性 `find()` 不同）；解析代理源时不再整体 clone 整个 JSON 数组。
- **scheduler：状态保存持续失败时会无限重试**。保留 `dirty`（清掉会丢改动）的同时加了指数退避，避免把整个多 MB 池每 30 秒重新序列化一次并刷爆日志。

### 前端
- **committed `frontend/dist` 陈旧了两个版本**。`install.sh` 在 npm 不可用（`--no-frontend`，或工具链安装失败）时回退到仓库里这份 dist，而它仍带着 v1.0.1 已修掉的 `residential_only` 回归——这类安装会让后端静默丢弃未分类端口。已重建 dist，并在 CI 中加了按提交拓扑判断的陈旧门禁。
- **配置页首次加载必然 401 后永不恢复**：只在启动时渲染一次，用户在页头填入 key 后该标签页仍停在「未授权」直到整页刷新。改为切页与保存 key 时重渲染，并加了过期响应守卫。
- **API Key 在配置页以明文渲染**（`f()` 没有 `type` 参数），而页头输入框本来就是 `password`；清空它会静默关闭整个 `/api` 的鉴权而控制台照常可用，现要求显式确认。
- **复制按钮在明文 HTTP 下彻底失效**：Clipboard API 只在 SecureContext 下存在，而明文 HTTP 是受支持的部署方式，三处按钮直接抛 `TypeError`。提取 `copyText()` 统一处理。复制文案也改为按 scheme 生成——`fanout.mode: http` 时文案仍写「socks 链接」，实际吐出的却是 `http://`。
- 代理源的 `protocol` 改为白名单校验：拼成 `"sock5"` 以前能通过前端校验，然后让整份配置保存 422、表单上其它修改全部丢失。数字输入框现在拒绝空值/非整数/负数——`Number("")` 是 `0` 而不是 `NaN`，清空「起始端口」会存下 `base_port: 0`，relay 绑到临时端口、客户端链接全断而界面零报错。「仅住宅端口」在直连模式下现在真正生效（此前发出的请求与「生成」逐字节相同），3x-ui 端口勾选列表随切页刷新。
- `api.ts` 补齐了此前缺失的 15 个配置字段（含整个 `warp` 段），使「绝不丢弃未暴露的段」这条不变量真正能被 `tsc` 守住。

### 构建与文档
- CI 此前**完全没有 Python 门禁**，而 `scripts/xui_db.py` 直接写 3x-ui 面板数据库。新增 `py_compile` 步骤。
- `release.yml` 增加按 tag 串行的 `concurrency`（`cancel-in-progress: false`）：强推 tag 会触发第二个 run，两个 `publish` 作业写同名资源会交错成残缺附件集；而中途取消恰恰会造成同样的残缺，所以刻意不取消。
- 删除 `patch_audit2.py`：一次性补丁脚本，import 即执行并就地改写仓库文件，且已完全失效。
- README 修正 `fanout.max_ports`（文档写 `100`，实际 `20`），补上 `fanout.auto_assign` 与整个 `warp.*` 配置表。
- `config.example.json` 补上 `fanout.auto_assign`、三个 `warp.mihomo_*` 键与新的 `warp.masque_port`。

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