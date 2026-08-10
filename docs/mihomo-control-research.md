# mihomo（Clash Meta）控制方式调研报告

> 调研对象：MetaCubeX/mihomo（v1.19.27 源码 + 官方文档 wiki.metacubex.one / Meta-Docs 仓库）
> 调研日期：本报告基于 2026 年可获取的官方资料与源码核实

---

## 1. 外部控制器 REST API 完整能力清单

通过 `external-controller: 127.0.0.1:9090`（可选 `secret: ""` 鉴权，请求头 `Authorization: Bearer ${secret}`）暴露。以下端点均已在官方 API 文档（Meta-Docs `docs/api/index.en.md`）及 `hub/route/*.go` 源码中核实。

### 1.1 运行配置（/configs）——三种方法对应三种不同的"改配置"能力

| 方法 | 端点 | 作用 | 说明 |
|---|---|---|---|
| `GET` | `/configs` | 读取当前运行配置 | 返回 `port`、`socks-port`、`mixed-port`、`mode`、`log-level`、`allow-lan`、`ipv6`、`tun` 等字段 |
| `PATCH` | `/configs` | **增量热更新**（不重载整个配置） | 传 `{"mixed-port": 7890}` 这类部分字段，HTTP 204 |
| `PUT` | `/configs?force=true` | **完整重载配置** | 传 `{"path": "...", "payload": ""}`；path 必须是绝对路径且位于工作目录或 `SAFE_PATHS` 环境变量白名单内；payload 非空时直接以字符串作为新配置 |

`PATCH /configs` 支持的字段（源码 `hub/route/configs.go` 的 `configSchema` 逐字段核实）：

- **端口类**：`port`、`socks-port`、`redir-port`、`tproxy-port`、`mixed-port`（重建对应监听器）
- **网络类**：`allow-lan`、`bind-address`、`ipv6`（内部切换 `resolver.DisableIPv6`）、`interface-name`、`tcp-concurrent`、`sniffing`、`find-process-mode`
- **行为类**：`mode`（`rule`/`global`/`direct`）、`log-level`
- **TUN 类**：`tun`（见 1.4，**含 `enable`，可运行时开关**）
- **入站服务类**：`tuic-server`、`ss-config`、`vmess-config`、`tcptun-config`、`udptun-config`
- **鉴权类**：`skip-auth-prefixes`、`lan-allowed-ips`、`lan-disallowed-ips`

**PATCH 不支持的字段（重要）**：`dns` 整个子树、`rules`、`proxies`/`proxy-groups`、`proxy-providers`、`rule-providers`、`tunnels`、`hosts`、`external-controller` 本身。这些只能走 `PUT /configs` 完整重载或重启进程。

### 1.2 模式切换

```bash
# 切换到 rule / global / direct
curl -X PATCH -H "Authorization: Bearer ${secret}" \
  -d '{"mode": "rule"}' http://127.0.0.1:9090/configs
```

### 1.3 代理节点获取与切换

```bash
# 列出所有代理/策略组（Selector/URLTest/Fallback/LoadBalance 组含 now/all 字段）
curl -H "Authorization: Bearer ${secret}" http://127.0.0.1:9090/proxies

# 切换某策略组（Selector）选中的节点
curl -X PUT -H "Authorization: Bearer ${secret}" \
  -d '{"name": "🇯🇵 JP 东京"}' http://127.0.0.1:9090/proxies/🚀\ 节点选择

# 清除自动策略组（URLTest/Fallback）的固定选择
curl -X DELETE -H "Authorization: Bearer ${secret}" \
  http://127.0.0.1:9090/proxies/🚀\ 节点选择

# 单节点延迟测试
curl -H "Authorization: Bearer ${secret}" \
  "http://127.0.0.1:9090/proxies/🇯🇵%20JP%20东京/delay?url=https://www.gstatic.com/generate_204&timeout=5000"

# 策略组批量延迟测试
curl -H "Authorization: Bearer ${secret}" \
  "http://127.0.0.1:9090/group/🚀%20节点选择/delay?url=https://www.gstatic.com/generate_204&timeout=5000"
```

### 1.4 TUN 能否通过 API 开关？

**能。** 这是 mihomo 相对旧版 Clash 的重要增强，源码核实：

- `hub/route/configs.go` 的 `patchConfigs()` 中 `listener.ReCreateTun(pointerOrDefaultTun(general.Tun, listener.LastTunConf), tunnel.Tunnel)` 总是被调用；
- `listener/listener.go` 的 `ReCreateTun()`：新配置与 `LastTunConf` 不同时先 `closeTunListener()`（销毁 TUN 设备），若 `enable: false` 则直接返回，否则 `sing_tun.New()` 重建设备并重配路由/防火墙规则。

```bash
# 运行时开启 TUN
curl -X PATCH -H "Authorization: Bearer ${secret}" \
  -d '{"tun": {"enable": true, "stack": "mixed", "auto-route": true, "dns-hijack": ["any:53"]}}' \
  http://127.0.0.1:9090/configs

# 运行时关闭 TUN
curl -X PATCH -H "Authorization: Bearer ${secret}" \
  -d '{"tun": {"enable": false}}' http://127.0.0.1:9090/configs
```

**注意**：运行时开关 TUN 意味着进程要能创建设备、改路由表——进程必须持有 root 或 `CAP_NET_ADMIN`（+`CAP_NET_RAW`）能力，否则 API 调用会失败（源码中错误路径会回置 `enable=false` 并记日志）。所以"API 能开 TUN"的前提是**进程启动时**就具备相应权限；权限本身无法通过 API 获得。

### 1.5 重载配置

```bash
# 方式 A：从磁盘文件完整重载（路径需在工作目录或 SAFE_PATHS 内）
curl -X PUT -H "Authorization: Bearer ${secret}" \
  -d '{"path": "/etc/mihomo/config.yaml", "payload": ""}' \
  "http://127.0.0.1:9090/configs?force=true"

# 方式 B：直接以字符串为新配置重载（不落盘）
curl -X PUT -H "Authorization: Bearer ${secret}" \
  -d '{"path": "", "payload": "mixed-port: 7890\nmode: rule\n..."}' \
  "http://127.0.0.1:9090/configs?force=true"

# 方式 C：重启内核（等价于重载）
curl -X POST -H "Authorization: Bearer ${secret}" \
  -d '{"path": "", "payload": ""}' http://127.0.0.1:9090/restart
```

### 1.6 其他实用端点（官方文档核实）

| 端点 | 方法 | 用途 |
|---|---|---|
| `/version` | GET | 版本信息 |
| `/logs` | GET/WS | 实时日志（`?level=debug`） |
| `/traffic`、`/memory` | GET/WS | 实时流量、内存 |
| `/connections` | GET/WS、DELETE | 查看/关闭连接 |
| `/rules`、`/rules/disable` | GET、PATCH | 查看规则；**临时禁用规则**（重启后失效，`{"0": true}`） |
| `/providers/proxies/{name}` | PUT | **运行时更新订阅**（拉取最新节点） |
| `/providers/proxies/{name}/healthcheck` | GET | 触发健康检查 |
| `/providers/rules/{name}` | PUT | 更新规则集 |
| `/cache/fakeip/flush`、`/cache/dns/flush` | POST | 清空 fake-ip / DNS 缓存 |
| `/dns/query` | GET | 调试 DNS 解析 |
| `/upgrade` | POST | 在线升级内核（可选 `?force=true`） |
| `/storage/{key}` | GET/PUT/DELETE | KV 存储 |
| `/debug/gc`、`/debug/pprof` | PUT/GET | 调试（需 log-level: debug） |

---

## 2. config.yaml 关键配置项正确写法

### 2.1 顶层通用项

```yaml
# 监听端口：HTTP、SOCKS、混合（HTTP+SOCKS）、透明代理（Linux/macOS/Android）
port: 7890          # 置 0 关闭
socks-port: 7891
mixed-port: 7892
redir-port: 7893    # 仅 TCP，Linux/Android/macOS
tproxy-port: 7894   # TCP+UDP，仅 Linux/Android

# 局域网访问
allow-lan: false
bind-address: "*"           # 仅 allow-lan: true 时生效；"*"=所有地址
lan-allowed-ips: [0.0.0.0/0, "::/0"]
lan-disallowed-ips: []      # 黑名单优先于白名单

# 运行模式：rule / global / direct
mode: rule

# IPv6 总开关：关闭会阻断所有 IPv6 连接并屏蔽 DNS AAAA 记录
ipv6: true

# 外部控制器（RESTful API）
external-controller: 127.0.0.1:9090
secret: ""                  # API 密钥，留空则无需鉴权
# external-controller-unix: mihomo.sock   # Unix socket（不校验 secret！）
# external-controller-tls: 127.0.0.1:9443 # HTTPS API，需配置 tls 段
# external-ui: /path/to/ui                # Web 面板（metacubexd 等）

# 其他常用
log-level: info             # silent/error/warning/info/debug
find-process-mode: strict   # always/strict/off
tcp-concurrent: true
unified-delay: true
interface-name: en0         # 出口网卡
profile:
  store-selected: true      # 记住 API 选择的节点，重启后恢复
  store-fake-ip: true
```

### 2.2 TUN

```yaml
tun:
  enable: true
  stack: system        # system / gvisor（默认）/ mixed（TCP 走 system，UDP 走 gvisor）
  device: utun0        # macOS 必须 utun 开头
  auto-route: true     # 自动设置全局路由，把流量引入 TUN
  auto-redirect: true  # Linux 专用：自动配置 iptables/nftables 重定向 TCP（需 auto-route）
  auto-detect-interface: true
  strict-route: true   # auto-route 下强制严格路由，防泄漏
  dns-hijack:
    - any:53
    - tcp://any:53
  mtu: 9000
  gso: true            # Linux 专用
  inet6-address: fdfe:dcba:9876::1/126   # 需顶层 ipv6: true 才生效
  udp-timeout: 300
  iproute2-table-index: 2022
  iproute2-rule-index: 9000
  # 精细控制（Linux/Android）
  include-uid: [0]
  exclude-uid: [1000]
  exclude-uid-range: [1000-99999]
  include-mac-address: [00:11:22:33:44:55]   # 需 auto-route + auto-redirect
```

### 2.3 DNS

```yaml
dns:
  enable: true
  listen: 0.0.0.0:1053        # 内置 DNS 监听；TUN 模式下 dns-hijack 会劫持 53 到此
  ipv6: false                 # 为 false 时 AAAA 返回空结果
  enhanced-mode: fake-ip      # fake-ip / redir-host（默认 redir-host）
  fake-ip-range: 198.18.0.1/16
  fake-ip-filter:
    - '*.lan'
    - '+.local'
  default-nameserver:         # 用于解析 DNS 服务器域名的上游，必须是 IP
    - 223.5.5.5
  nameserver:
    - https://doh.pub/dns-query
  fallback:                   # 海外备用 DNS（配置后默认启用 fallback-filter，geoip-code: CN）
    - tls://8.8.4.4
  nameserver-policy:
    '+.cn': [https://dns.alidns.com/dns-query]
  proxy-server-nameserver:    # 代理节点域名专用解析，避免鸡生蛋问题
    - https://doh.pub/dns-query
```

---

## 3. 关键结论：配置项 → 生效方式对照表

> 三种"生效方式"：
> **A. PATCH 热切换** = `PATCH /configs` 即时生效，无需重载/重启；
> **B. 完整重载** = 改配置文件后 `PUT /configs?force=true`（API）或 `systemctl reload`（SIGHUP）或 `systemctl restart`；
> **C. 进程重启** = 只有启动期读取/持有资源才生效，无法热更新。

| 配置项 | 生效方式 | 备注 |
|---|---|---|
| `mode` (rule/global/direct) | **A. PATCH 热切换** | `{"mode":"global"}` |
| `port` / `socks-port` / `mixed-port` / `redir-port` / `tproxy-port` | **A. PATCH 热切换** | 监听器即时重建 |
| `allow-lan` / `bind-address` / `lan-allowed-ips` / `lan-disallowed-ips` / `skip-auth-prefixes` | **A. PATCH 热切换** | |
| `ipv6` | **A. PATCH 热切换** | 注意仅影响内核收包/DNS AAAA 屏蔽，TUN 的 v6 地址等深绑定项仍建议重载 |
| `log-level` / `find-process-mode` / `tcp-concurrent` / `sniffing` / `interface-name` | **A. PATCH 热切换** | |
| `tun`（**含 `enable`**、stack、auto-route、dns-hijack、mtu、uid 过滤等 tunSchema 全字段） | **A. PATCH 热切换** | 前提：进程已有 root/CAP_NET_ADMIN；失败时回退为关闭并记日志 |
| `tuic-server` / `ss-config` / `vmess-config` | **A. PATCH 热切换** | 内置入站服务 |
| 策略组节点选择（Selector 选中项） | **A. PATCH 热切换** | `PUT /proxies/{name}`，即"选择节点" |
| 订阅内容（proxy-provider 节点列表） | **B. 完整重载** | 但有专门端点 `PUT /providers/proxies/{name}` 即时拉取更新，无需整配置重载 |
| `dns` **整个子树**（enable、listen、nameserver、fallback、enhanced-mode、fake-ip-filter、fake-ip-range、nameserver-policy…） | **B. 完整重载** | **PATCH 不支持 dns 字段**（configSchema 中不存在）。改配置后 `PUT /configs?force=true` 或重启进程；fake-ip 缓存可用 `POST /cache/fakeip/flush` 清掉 |
| `rules` / `sub-rules` | **B. 完整重载**（临时禁用可用 `PATCH /rules/disable`，重启失效） | |
| `proxies` 新增/删除节点、`proxy-groups` 结构 | **B. 完整重载** | 改已有节点参数同理 |
| `proxy-providers` / `rule-providers` 定义（url、interval、path 等） | **B. 完整重载** | 拉取动作可单独触发 |
| `tunnels` / `hosts` / `sniffer` / `profile` / `geodata-*` 等其余解析期字段 | **B. 完整重载** | |
| `external-controller` / `external-controller-unix/tls` / `secret` | **C. 进程重启** | 改了当前 API 就断了，只能改文件后重启进程（或 SIGHUP，但新监听地址需重启最稳妥） |
| 命令行参数 `-d` / `-f` / `-ext-ctl` / `-secret` 与环境变量（`SAFE_PATHS`、`SKIP_SYSTEM_IPV6_CHECK` 等） | **C. 进程重启** | |
| 二进制/内核版本 | **C. 进程重启**（另有 `POST /upgrade` 在线升级） | |

### 与预期对照

- ✅ **符合预期**：mode、节点选择 → API 热切换；dns 相关 → 必须完整重载（改配置 + `PUT /configs?force=true` 或重启进程）。
- ⚠️ **超出预期**：**TUN 在 mihomo 中可通过 `PATCH /configs` 运行时开关**（v1.19 源码证实）。传统 Clash 中"TUN 必须重启"的说法对 mihomo 已不成立，但运行时切换要求进程已持有网络管理权限，且切换期间会短暂销毁/重建 TUN 设备与路由（存量连接会中断）。
- ⚠️ **补充**：dns 不一定要"重启进程"——`PUT /configs?force=true` 即可完整重载（包括 DNS），进程不退出；只有改 external-controller/secret 等才真正需要重启。

---

## 4. 配置校验命令 `mihomo -t`

源码 `main.go` 核实：`-t` = "test configuration and exit"。

```bash
# 校验默认目录（-d 指定的目录，默认找 config.yaml）
mihomo -t -d /etc/mihomo

# 校验指定文件
mihomo -t -f /path/to/config.yaml

# 输出示例（成功）
# configuration file /etc/mihomo/config.yaml test is successful

# 失败：打印具体错误行，退出码 1（适合 CI / 部署前检查）
mihomo -t -f /etc/mihomo/config.yaml; echo "exit=$?"
```

部署脚本建议：

```bash
mihomo -t -d /etc/mihomo && systemctl restart mihomo
```

---

## 5. TUN 模式对系统权限的要求

| 要求 | 说明 |
|---|---|
| `/dev/net/tun` | 内核 TUN 驱动必须存在（大部分发行版默认有；容器/WSL/LXC 需宿主机放行） |
| **root 或 CAP_NET_ADMIN** | 创建 TUN 设备、修改网卡/路由表必需；这是硬性要求，非 root 用户默认不行 |
| CAP_NET_RAW（建议） | 部分栈/功能（如 gvisor 之外的原生栈、DNS 劫持监听）需要 |
| iproute2（`ip` 命令） | `auto-route: true` 时用于管理策略路由（ip rule/ip route） |
| 防火墙 | Linux 上 `system`/`mixed` 栈需放行 TUN 出站；`auto-redirect` 会自动配置 iptables/nftables（需 CAP_NET_ADMIN 写入） |

**auto-route 行为**：自动向系统写入策略路由（默认 iproute2 表索引 2022、规则起始索引 9000，可配 `iproute2-table-index`/`iproute2-rule-index`），把全局流量导入 TUN 设备；`strict-route: true` 时强制严格路由（使不支持的网络不可达、所有连接进 TUN），防止泄漏。与公司 VPN、Docker、虚拟化网桥共存时可能出现路由竞争，可改用 `route-address`/`route-exclude-address`/`include-interface` 等收窄范围。

非 root 运行示例：

```bash
sudo setcap cap_net_admin,cap_net_raw=ep /usr/local/bin/mihomo
# 或用 systemd 的 AmbientCapabilities（见下）
```

---

## 6. 常见 systemd 部署

**官方推荐布局**（wiki "Create a running service"）：二进制 `/usr/local/bin/mihomo`，配置 `/etc/mihomo/config.yaml`（`-d /etc/mihomo` 指定目录）。

`/etc/systemd/system/mihomo.service`（官方文档原样）：

```ini
[Unit]
Description=mihomo Daemon, Another Clash Kernel.
After=network.target NetworkManager.service systemd-networkd.service iwd.service

[Service]
Type=simple
LimitNPROC=500
LimitNOFILE=1000000
CapabilityBoundingSet=CAP_NET_ADMIN CAP_NET_RAW CAP_NET_BIND_SERVICE CAP_SYS_TIME CAP_SYS_PTRACE CAP_DAC_READ_SEARCH CAP_DAC_OVERRIDE
AmbientCapabilities=CAP_NET_ADMIN CAP_NET_RAW CAP_NET_BIND_SERVICE CAP_SYS_TIME CAP_SYS_PTRACE CAP_DAC_READ_SEARCH CAP_DAC_OVERRIDE
Restart=always
ExecStartPre=/usr/bin/sleep 1s
ExecStart=/usr/local/bin/mihomo -d /etc/mihomo
ExecReload=/bin/kill -HUP $MAINPID

[Install]
WantedBy=multi-user.target
```

要点：

- `CapabilityBoundingSet` + `AmbientCapabilities` 同时出现：BoundingSet 只"允许"，Ambient 才让非 root 服务实际获得能力——两者缺一不可（社区 issue #1915：deb 包曾因缺 CAP_DAC_OVERRIDE 导致无法在 /etc/mihomo 创建 geoip 文件）；
- `ExecReload=/bin/kill -HUP $MAINPID`：SIGHUP 会重读配置文件（main.go 核实），**改完配置用 `systemctl reload` 即可，无需重启**；
- 若不用 TUN，`CAP_NET_ADMIN` 等可以裁剪。

运维命令：

```bash
systemctl daemon-reload
systemctl enable --now mihomo

# 改完 config.yaml 后：
mihomo -t -d /etc/mihomo                    # 1. 先校验
systemctl reload mihomo                     # 2. 热重载（SIGHUP，进程不退出）
# 或 systemctl restart mihomo               # 3. 需要彻底重启时

systemctl status mihomo
journalctl -u mihomo -o cat -e              # 最近日志
journalctl -u mihomo -o cat -f              # 跟踪日志
```

---

## 参考来源

- 官方 API 文档：`MetaCubeX/Meta-Docs` → `docs/api/index.en.md`（wiki.metacubex.one/en/api/）
- 官方配置文档：`docs/config/general.en.md`、`docs/config/inbound/tun.en.md`、`docs/config/inbound/port.en.md`、`docs/config/dns/index.en.md`
- 官方 systemd 文档：`docs/startup/service/index.en.md`（wiki.metacubex.one/en/startup/service/）
- 源码核实：`MetaCubeX/mihomo` v1.19.27 → `hub/route/configs.go`（PATCH/PUT /configs 字段）、`hub/route/proxies.go`（节点切换）、`listener/listener.go`（ReCreateTun 运行时 TUN 开关）、`main.go`（`-t` 校验、SIGHUP 重载）
