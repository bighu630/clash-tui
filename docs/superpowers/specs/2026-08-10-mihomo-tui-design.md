# mihomo-tui 定稿设计（含对齐结论）

> 主规格见任务书（已与用户逐点确认）。本文档记录 2026-08-10 与用户对齐的剩余细节，全部经用户确认（"按照你推荐的，另外如果错误能反馈出来最好反馈出来"）。

## 1. 订阅解析范围（v1）

- 订阅格式两种：完整 YAML（mihomo/clash 格式，含 proxies/proxy-groups/rules）；ShareLinks（base64 编码或明文的行式分享链接列表）
- 协议解析器 7 种：`vless / vmess / trojan / ss（含 plugin 混淆）/ ssr / hysteria2 / tuic`
- 每个解析器独立模块（core/parsers/），各配单元测试（2+ fixture/协议）
- base64 订阅可能整体 base64 包裹 YAML → 解码后再识别
- YAML 订阅含 `proxy-providers` 而无可解析 `proxies` → 明确报错"暂不支持 proxy-providers 订阅"

## 2. 规则/规则组表单交互

- 规则组类型：select / url-test / fallback（load-balance 二期）
- 组表单字段：名称、类型下拉、测速 URL（默认 `http://www.gstatic.com/generate_204`，仅自动类型生效）、interval（默认 300s）、tolerance（默认 0，仅 fallback）
- 组员：弹窗 CheckboxList，列出激活订阅解析出的全部节点名，支持输入过滤
- 规则表单字段：类型下拉（DOMAIN / DOMAIN-SUFFIX / DOMAIN-KEYWORD / GEOIP / PROCESS-NAME / MATCH）、payload（MATCH 无 payload）、目标下拉（自定义组 ∪ 激活订阅组 ∪ 内置 DIRECT/REJECT/REJECT-DROP/COMPATIBLE/PASS/PASS-RULE/GLOBAL）
- 规则列表支持上移/下移（K/J）；顺序即优先级；自定义规则恒在订阅规则之前（合并器保证）

## 3. 首页仪表盘布局与交互

- 顶栏：tabs + 状态行（模式 `m` 循环 / TUN `t` / IPv6 `6` / 出口 IP `r` 手动刷新 / API 连接状态）
- 中部左 60%：实时网速双 Sparkline（up 绿、down 蓝，120 样本环形缓冲）+ 当前速率
- 中部右 40%：总流量 upTotal/downTotal 大数字、内存 inuse 数字 + Sparkline
- 底栏：按键提示 + 最近通知（成功/错误）
- 开关全部走 PATCH /configs 热切（mode/ipv6/tun.enable）；TUN 其他参数属结构性变更
- 仪表盘 `s`：网络设置表单（ports/allow-lan/log-level/tun stack/auto-route/mtu/dns 基础项）→ 存 settings.toml → 合并+校验+应用重启（结构性变更流程）

## 4. 默认模板兜底

- 激活订阅无 proxy-groups → 自动注入 select 组「🚀 节点选择」（组员=全部节点）
- 激活订阅无 rules → 注入默认规则模板：`GEOIP,CN,DIRECT` + `MATCH,🚀 节点选择`
- 无激活订阅 → 只输出网络配置+自定义组/规则（无代理可用，mihomo 可正常以直连运行）

## 5. 订阅切换失败回滚

- apply 脚本（root 侧）承担回滚：替换 config.yaml 前保留 .bak → restart → sleep 1s → `systemctl is-active` 失败则恢复 .bak 并 restart → 非零退出 + stderr 说明
- TUI 侧预校验：写临时文件 → `mihomo -t -f`（失败直接把 mihomo 报错展示给用户，不进 sudo）

## 6. 错误反馈（用户强调）

- 所有异步操作失败 → MessagePopup 展示完整错误文本（reqwest 错误 / mihomo -t stderr / sudo stderr / apply 脚本输出）
- 成功操作 → 底栏绿色通知（保留最近 3 条）
- API 断连 → 顶栏红色 "API 未连接" 常驻指示 + 通知区显示最近错误
- 合并器错误（MergeError）同步弹出，消息指明具体规则/目标/缺失项
