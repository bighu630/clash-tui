# mihomo-tui

Linux 下的 [mihomo](https://github.com/MetaCubeX/mihomo)（Clash Meta 内核）终端控制器。
Rust + ratatui 实现，无需浏览器/桌面环境，在纯终端里完成订阅管理、配置合并、节点切换与流量监控。

## 功能总览

四个页面，顶部 Tabs 切换，底部为按键提示栏与最近通知（成功 `[✓]` / 失败 `[✗]`，保留最近 3 条）：

**仪表盘（首页）**
```
┌ 仪表盘  订阅  规则组  规则 ────────────────────────────────────────┐
│ 模式: rule [m] | TUN: 关 [t] | IPv6: 开 [6] | 出口IP: 1.2.3.4 [r] | API: 已连接 │
│                                                                  │
│  上行 ▅▃▅▇▅▂▃▅▇▅▃ 12.3 KB/s        ↑ 总流量  1.2 GB               │
│  下行 ▂▄▆▇▆▄▂▃▅▇▆▄ 456.7 KB/s      ↓ 总流量  8.9 GB               │
│                                   内存 82.3 MB  ▂▄▆▃▅              │
│                                                                  │
│ Tab/←→ 切页 | m 模式 | t TUN | 6 IPv6 | r 出口IP | s 设置 | ? 帮助 | q 退出 │
└──────────────────────────────────────────────────────────────────┘
```
- 左 60%：实时网速双 Sparkline（上行绿色、下行蓝色，120 样本环形缓冲）+ 当前速率
- 右 40%：总流量（upTotal/downTotal 大数字）+ 内存占用（inuse + Sparkline）
- `s`：网络设置表单（端口 / allow-lan / log-level / TUN stack、auto-route、mtu、dns-hijack / DNS 基础项）→ 存盘 → 合并 → 校验 → 应用（结构性变更流程）

**订阅管理**
```
[★] 机场A    | 节点 12 · 组 3 · 规则 50 | 上次拉取 08-10 12:00
[  ] 机场B    | 节点 8 · 组 1 · 规则 20 | 上次拉取 08-09 09:00
```
- `a` 添加订阅（名称 + URL）→ 拉取并解析；`Enter` 激活（合并 → 校验 → 应用）；`r` 刷新；`d` 删除（确认弹窗）
- 激活订阅的节点自动成为「规则组」页的组员候选

**规则组**
```
🚀 节点选择 | select   | 12 成员 | http://www.gstatic.com/generate_204 | 300s
自动选择   | url-test | 10 成员 | http://www.gstatic.com/generate_204 | 300s
```
- `n` 新建表单（名称 / 类型下拉 select·url-test·fallback / 测速 URL / interval / tolerance）
- `Enter` 编辑；`m` 成员勾选弹窗（支持输入过滤）；`d` 删除（确认弹窗）

**规则**
```
DOMAIN-SUFFIX, example.com, 🚀 节点选择
GEOIP, CN, DIRECT
MATCH, 🚀 节点选择
```
- `n` 新建（类型下拉 DOMAIN/DOMAIN-SUFFIX/DOMAIN-KEYWORD/GEOIP/PROCESS-NAME/MATCH + payload + 目标下拉）
- `Enter` 编辑；`d` 删除；`K`/`J` 上移/下移（顺序即优先级，自定义规则恒在订阅规则之前）

## 快速开始

### 前提

- **Arch Linux**（或任何能装 mihomo 的 Linux 发行版；Arch 上 `sudo pacman -S mihomo`）
- mihomo 已安装并作为 **systemd 服务**存在（安装器要求 `systemctl list-unit-files` 中出现 `mihomo.service`）
- 如需 **TUN 模式**：进程须持有 root 或 `CAP_NET_ADMIN`（+`CAP_NET_RAW`）能力，且内核有 `/dev/net/tun`。官方 unit 已通过 `AmbientCapabilities` 提供，见下文「手动安装」

### 编译

```bash
cargo build --release
./target/release/mihomo-tui
```

首次启动会自动检测提权组件（`/usr/local/sbin/mihomo-apply` 可执行 + `/etc/sudoers.d/99-mihomo` 存在），
缺失时弹出确认框。确认后 TUI 退出 raw 模式、恢复终端，进入**交互式 sudo**（输入密码），自动完成：

1. 校验 mihomo 二进制与 systemd 单元
2. 创建系统组 `mihomo-admin`（已存在则跳过）
3. 安装提权脚本 `/usr/local/sbin/mihomo-apply`（root:root 0755）
4. 写入 `/etc/sudoers.d/99-mihomo`（0440，`visudo -cf` 校验通过才生效）
5. 将当前用户加入 `mihomo-admin` 组
6. 提示 `sudo systemctl enable --now mihomo`（不自动执行，由你决定；UI 会询问）

> **重新登录终端**后组成员资格生效，此后 `sudo -n` 免密调用提权脚本。

### 添加订阅

`a` → 输入名称与订阅 URL → 回车拉取。支持完整 YAML 订阅（`proxies`/`proxy-groups`/`rules`）
与 ShareLinks 订阅（base64 或明文的分享链接，7 种协议：vless / vmess / trojan / ss / ssr / hysteria2 / tuic）。

### 激活

`Enter` 激活订阅 → 自动完成「合并 → `mihomo -t` 预校验 → sudo 提权应用」三步：

- 合并器把网络设置、自定义组/规则与订阅内容组装成 `config.yaml`
- 预校验失败（配置语法/引用错误）直接把 mihomo 的报错弹给你，**不进入 sudo**
- 提权脚本负责：原子替换 `/etc/mihomo/config.yaml` → `systemctl restart mihomo` →
  健康检查失败自动回滚上一份配置并重启

## 手动安装（等价步骤）

不想用首启引导时，可手动执行以下命令（TUI 安装器做的事完全一致）：

```bash
sudo pacman -S mihomo

# 1. 官方推荐布局：二进制 /usr/local/bin/mihomo，配置目录 /etc/mihomo
#    创建 /etc/systemd/system/mihomo.service，内容见下方「官方 unit」。
sudo systemctl daemon-reload
sudo systemctl enable --now mihomo

# 2. 创建授权组
sudo groupadd --system mihomo-admin

# 3. 安装提权脚本（内容与仓库 resources/mihomo-apply.sh 相同）
sudo tee /usr/local/sbin/mihomo-apply > /dev/null <<'SCRIPT'
#!/usr/bin/env bash
# ...（resources/mihomo-apply.sh 全文，见仓库）
SCRIPT
sudo chown root:root /usr/local/sbin/mihomo-apply
sudo chmod 755 /usr/local/sbin/mihomo-apply

# 4. 写入 sudoers 规则
sudo tee /etc/sudoers.d/99-mihomo > /dev/null <<'EOF'
%mihomo-admin ALL=(root) NOPASSWD: /usr/local/sbin/mihomo-apply
EOF
sudo chmod 0440 /etc/sudoers.d/99-mihomo
sudo visudo -cf /etc/sudoers.d/99-mihomo

# 5. 当前用户入组（重新登录后生效）
sudo usermod -aG mihomo-admin "$USER"
```

**验证**：重新登录后 `sudo -n /usr/local/sbin/mihomo-apply < /tmp/config.yaml` 应免密执行。

### 官方 mihomo.service unit

来自 [mihomo 官方文档](https://wiki.metacubex.one/en/startup/service/)（`CapabilityBoundingSet` + `AmbientCapabilities` 必须同时出现——前者只是"允许"，后者才让非 root 服务实际获得能力）：

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

- **TUN 权限**由 `CAP_NET_ADMIN`/`CAP_NET_RAW` 提供（创建 TUN 设备、改路由/防火墙必需）；不用 TUN 可裁剪这两项
- `ExecReload=/bin/kill -HUP $MAINPID`：改完配置 `systemctl reload mihomo` 即可热重载，无需重启

## 按键表

**全局**（任意页面）：

| 按键 | 功能 |
|---|---|
| `Tab` / `←` `→` / `1`-`4` | 切换页面（仪表盘/订阅/规则组/规则） |
| `?` | 帮助弹窗（列出全部按键） |
| `q` / `Esc`（无弹窗时）/ `Ctrl-C` | 退出 |

**仪表盘**：

| 按键 | 功能 |
|---|---|
| `m` | 循环切换模式 rule / global / direct（PATCH 热切） |
| `t` | 开关 TUN（PATCH 热切，需进程持有 CAP_NET_ADMIN） |
| `6` | 开关 IPv6（PATCH 热切） |
| `r` | 手动刷新出口 IP |
| `s` | 网络设置表单（结构性变更：保存 → 合并 → 校验 → 应用重启） |

**订阅页**：`a` 添加 · `Enter` 激活 · `r` 刷新 · `d` 删除

**规则组页**：`n` 新建 · `Enter` 编辑 · `m` 编辑成员 · `d` 删除

**规则页**：`n` 新建 · `Enter` 编辑 · `d` 删除 · `K` 上移 · `J` 下移

弹窗通用：`Enter` 确认 · `Esc` 取消 · 勾选列表 `Space` 勾选、`/` 过滤。

## 架构

```
src/
  main.rs       终端初始化/恢复、panic hook（崩溃也保证恢复终端）
  app.rs        AppState + 事件循环（tokio::select!：键盘 / 1s tick / traffic / memory / 命令通道）
  ui/           四个页面 + 通用弹窗组件（FormPopup/CheckboxList/ConfirmPopup/MessagePopup/SelectList）
  core/         纯逻辑层（无 TUI 依赖，可单测）
    models.rs     数据模型（NetworkSettings/Tun/Dns/Subscription/Overrides…）
    settings.rs   配置文件读写（~/.config/mihomo-tui/，原子替换）
    subscription.rs / parsers/   订阅拉取、识别（YAML vs ShareLinks）、7 协议解析
    merger.rs     合并器：网络段 + 自定义组/规则 + 订阅内容 → config.yaml
    client.rs     REST 客户端（/configs /traffic /memory /proxies…）
    apply.rs      mihomo -t 预校验 + sudo 提权应用
  service/
    installer.rs 首装检测与提权组件安装（本组件）
resources/
  mihomo-apply.sh  提权脚本（root 侧：校验→原子替换→重启→回滚）
```

### 合并器组装顺序与去重规则

输出 `config.yaml` 顶层键顺序：网络段 → `proxy-groups` → `rules` → `proxies`。

组装顺序：

1. **网络段**：port / socks-port / mixed-port / allow-lan / mode / ipv6 / log-level / external-controller / secret / tun / dns（全部字段）
2. **proxy-groups** = 自定义组 + 订阅组 + 自动组（兜底，需要时）
3. **rules** = 自定义规则 + 订阅规则 + 默认模板（兜底，需要时）
4. **proxies** = 订阅节点

去重与冲突规则（全部记录 warning 展示）：

| 冲突 | 处理 |
|---|---|
| 订阅 proxies 内重名节点 | 保留第一个 |
| 自定义组名 = 订阅组名 | 保留自定义，丢弃订阅同名组 |
| 订阅组名 = 节点名 | 丢弃该订阅组（节点名优先） |
| 自定义组名 = 节点名 | **MergeError**（用户必须改名） |
| 自定义规则 target / 组成员不存在 | **MergeError**（消息指明规则/组与缺失项） |
| 订阅规则 / 组成员引用缺失 | 丢弃该项 + warning |

兜底模板：订阅有节点但组列表为空 → 注入 select 组「🚀 节点选择」；
订阅无规则 → 注入 `GEOIP,CN,DIRECT` + `MATCH,🚀 节点选择`；
无激活订阅 → 只输出网络段 + 自定义内容（mihomo 以直连运行）。

### 混合生效策略

| 变更类型 | 生效方式 |
|---|---|
| mode / ipv6 / tun.enable / log-level / allow-lan / 端口 | **PATCH 热切**：`PATCH /configs` 即时生效，不重载（仪表盘 `m`/`t`/`6`） |
| 订阅切换 / 组 / 规则 / DNS / 端口结构性修改（`s` 表单） | **结构性重启**：合并 → `mihomo -t` 预校验 → 提权脚本原子替换 → `systemctl restart` → 失败自动回滚 |
| external-controller / secret 修改 | 进程重启（需改 `settings.toml` 后重启 mihomo 与本程序） |

## 配置文件

全部位于 `~/.config/mihomo-tui/`（首次运行自动创建）：

| 文件 | 格式 | 内容 |
|---|---|---|
| `settings.toml` | TOML | 网络设置（NetworkSettings，含 tun/dns 嵌套） |
| `subscriptions.toml` | YAML | 订阅列表（含解析缓存） |
| `overrides.toml` | YAML | 自定义规则组与规则 |

**settings.toml**（示例）：

```toml
mode = "rule"
ipv6 = true
allow_lan = false
port = 7890
socks_port = 7891
mixed_port = 7892
log_level = "info"
external_controller = "127.0.0.1:9090"
secret = "0123456789abcdef0123456789abcdef"

[tun]
enable = false
stack = "mixed"
auto_route = true
dns_hijack = ["any:53"]
mtu = 9000

[dns]
enable = true
listen = "0.0.0.0:1053"
enhanced_mode = "fake-ip"
fake_ip_range = "198.18.0.1/16"
nameserver = ["https://doh.pub/dns-query"]
default_nameserver = ["223.5.5.5"]
fallback = ["tls://dns.alidns.com", "tls://dot.pub"]
fake_ip_filter = ["*.lan", "+.local"]
```

**subscriptions.toml**（示例，YAML 序列化）：

```yaml
- name: 机场A
  url: https://example.com/api/v1/client/subscribe?token=xxx
  last_fetch: 2026-08-10T12:00:00Z
  active: true
  cache:
    proxies:
      - name: 🇯🇵 JP
        kind: vless
        yaml:
          type: vless
          server: 1.2.3.4
          port: 443
          uuid: xxxx
          tls: true
          network: ws
    proxy_groups:
      - name: 自动选择
        type: url-test
        proxies: ["🇯🇵 JP"]
    rules:
      - DOMAIN-SUFFIX,google.com,自动选择
    fetched_at: 2026-08-10T12:00:00Z
```

**overrides.toml**（示例，YAML 序列化）：

```yaml
groups:
  - name: 🚀 节点选择
    group_type: select
    url: http://www.gstatic.com/generate_204
    interval: 300
    tolerance: 0
    proxies: ["🇯🇵 JP", "🇭🇰 HK"]
rules:
  - rule_type: DOMAIN-SUFFIX
    payload: example.com
    target: 🚀 节点选择
```

## FAQ

**Q：为什么安装/应用时还要输 sudo 密码？**
安装器与提权应用都使用交互式 sudo（安全考虑，不缓存凭据）。安装完成后**重新登录终端**，
`mihomo-admin` 组成员资格生效，此后 `sudo -n /usr/local/sbin/mihomo-apply` 免密调用。

**Q：TUN 打不开 / 提示权限不足？**
TUN 需要 `CAP_NET_ADMIN`（+`CAP_NET_RAW`）能力与 `/dev/net/tun`。用官方 systemd unit
（`AmbientCapabilities`）即可；二进制方式运行可 `sudo setcap cap_net_admin,cap_net_raw=ep /usr/local/bin/mihomo`。

**Q：订阅支持哪些格式？**
完整 YAML（`proxies`/`proxy-groups`/`rules`）、base64 包裹的 YAML、以及 ShareLinks
（base64 或明文行式链接），共 7 种协议：vless / vmess / trojan / ss（含 plugin）/ ssr / hysteria2 / tuic。
含 `proxy-providers` 而无 `proxies` 的订阅暂不支持（会明确报错）。

**Q：合并报错是什么意思？**
合并器报错（MergeError）都会指明具体规则/组与缺失项，常见三类：
- 自定义规则的目标不是任何节点/组/内置目标（DIRECT、REJECT、REJECT-DROP、COMPATIBLE、PASS、PASS-RULE、GLOBAL）→ 改目标或建组
- 自定义组成员不在激活订阅的节点列表里 → 改成员名
- 自定义组名与节点名冲突 → 改组名
订阅侧的引用问题（订阅组/规则引用不存在的节点）不会中断，丢弃并给 warning。

**Q：应用失败会自动回滚吗？**
会。TUI 侧先 `mihomo -t -f` 预校验（失败直接弹 mihomo 报错，不进 sudo）；
提权脚本在替换前保留 `config.yaml.bak`，重启后 `systemctl is-active` 健康检查失败
即恢复备份并再次重启，stderr 返回 `rolling back` 说明。

**Q：GEOIP 规则报错 / 不生效？**
`GEOIP,CN,DIRECT` 等规则依赖 GeoIP 数据库（geodata）。mihomo 启动时会按需下载；
离线环境需手动放置 geodata 文件（如 `/etc/mihomo/GeoIP.dat` 与 `GeoSite.dat`，并确认
服务进程对配置目录有写权限——官方 unit 的 `CAP_DAC_OVERRIDE` 正是为此）。

**Q：订阅组之间互相引用会怎样？**
订阅组引用其他订阅组（组链式引用，如 A 组把 B 组列为成员）不受支持：合并器只认
「组→节点」直连引用，此类成员会被丢弃并给 warning；若因此成员清空，该组整体丢弃。
不建议在订阅里使用组链式结构——需要的话在 TUI 里用自定义规则组引用节点。

**Q：配置文件在哪，怎么备份？**
`~/.config/mihomo-tui/` 三个文件即全部状态（含订阅缓存），`cp -r` 即可备份/迁移。

## 开发

```bash
cargo test                  # 单元测试（合并器/解析器/安装器/设置存取）
cargo clippy -- -D warnings
cargo build --release

# 合并产物样例：读取 ~/.config/mihomo-tui 三文件 → 输出合并后的 config.yaml
cargo run --example merge_sample > /tmp/config.yaml
MIHOMO_TUI_SETTINGS_DIR=/path/to/settings cargo run --example merge_sample > /tmp/config.yaml
mihomo -t -f /tmp/config.yaml   # 用真实 mihomo 校验合并产物
```

## 许可证

MIT
