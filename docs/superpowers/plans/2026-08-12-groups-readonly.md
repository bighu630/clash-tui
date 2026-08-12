# 规则组功能重构：只读展示 + 运行时切换 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 移除自定义规则组编辑能力，规则组页改为只读展示订阅规则组；select 组支持运行时切换节点（PUT /proxies）；自动类型组展示但禁选并提示；启用 store-selected 持久化；新增整组延迟测试。

**Architecture:** 合并器不再输出/校验自定义组，订阅组原样透传 + 输出 `profile: store-selected: true`；client.rs 新增 GroupInfo 与 get_proxies/switch_group/test_group_delay 三个 REST API；ui/groups.rs 从编辑页重写为只读列表 + 单选弹窗；app.rs 增加对应 UiCommand/UiEvent 数据流与旧数据迁移。

**Tech Stack:** Rust、ratatui、crossterm、tokio、reqwest、serde_json、serde_yaml、percent-encoding（新增直接依赖，url 的传递依赖，版本 2）。

**用户已确认的需求决策（2026-08-12）：**
1. 规则管理页（自定义 rules）**保留**
2. select 组选择持久化**启用**（`profile: store-selected: true`）
3. 展示形式：列表行 `组名 | 类型 | 当前选择`，Enter 弹单选列表（select 组）/ 提示（auto 组），自动组 🔒 标记
4. 旧自定义组数据：**启动时清空 + 一次提示**（方案 A）
5. 延迟测试**做**：`GET /group/{name}/delay`

**协调事项：** `.worktrees/fix-dashboard-toggle-persist`（未合并，改 dashboard/settings/client.rs 测试/merger.rs 测试）与 `.worktrees/dashboard-connections`（仅文档）与本计划**不冲突**（改动区域不同）；合并回 main 时如遇同文件冲突手动 resolve。本计划在独立 worktree `refactor/groups-readonly` 中执行。

---

## 文件结构

| 文件 | 变更 | 职责 |
|---|---|---|
| `Cargo.toml` | 加 `percent-encoding = "2"` | 组名/URL 路径编码 |
| `src/core/models.rs` | `Overrides.groups`/`UserGroup` 标注废弃（保留供迁移反序列化） | 模型 |
| `src/core/merger.rs` | 大改：移除自定义组全部逻辑 + 订阅组原样透传 + profile 键；测试重写 | 合并器 |
| `src/core/client.rs` | 新增 `GroupInfo`、`get_proxies`、`switch_group`、`test_group_delay` + 假服务器测试 | REST API |
| `src/app.rs` | AppState.proxy_groups、UiCommand/UiEvent 3 组新变体、切页刷新、事件处理、迁移函数、HELP_LINES/page_hints、测试 | 状态机 |
| `src/ui/groups.rs` | 全量重写：只读列表 + SelectorPopup 单选弹窗 + 自动组禁选提示 | 规则组页 |
| `src/ui/rules.rs` | `target_options` 移除自定义组 | 规则页（保留） |
| `README.md` | 功能列表/使用指南/按键/FAQ 更新 | 文档 |

---

## Task 1: core 模型与合并器简化（W1）

**Files:**
- Modify: `src/core/models.rs`
- Modify: `src/core/merger.rs`
- Modify: `src/ui/rules.rs`
- Test: `src/core/merger.rs`（内嵌 tests 模块）

- [ ] **Step 1: models.rs 标注废弃**

`src/core/models.rs` 中 `Overrides` 与 `UserGroup` 的 doc 注释修改：

```rust
/// 用户覆盖配置（自定义规则；groups 已废弃，仅保留反序列化以迁移旧数据）。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Overrides {
    /// 已废弃：仅用于读取旧 overrides.toml 迁移（启动时清空），合并器/UI 不再使用。
    #[serde(default)]
    pub groups: Vec<UserGroup>,
    #[serde(default)]
    pub rules: Vec<UserRule>,
}

/// 自定义规则组（已废弃：仅用于反序列化旧 overrides.toml 以启动迁移，不再创建）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserGroup {
    pub name: String,
    /// "select" | "url-test" | "fallback"
    pub group_type: String,
    #[serde(default = "default_test_url")]
    pub url: String,
    #[serde(default = "default_group_interval")]
    pub interval: u64,
    #[serde(default)]
    pub tolerance: u64,
    /// 组员 = 节点名 / 其他策略组名 / 内置目标名（如 DIRECT）
    #[serde(default)]
    pub proxies: Vec<String>,
}
```

（字段本身不动，`default_test_url`/`default_group_interval` 保留——UserGroup 反序列化默认值仍需要。）

- [ ] **Step 2: merger.rs 重构合并流程**

`src/core/merger.rs` 中 `pub fn merge` 整体重写为以下流程（保留 `AUTO_GROUP_NAME`、`DEFAULT_RULES`、`MergeContext`、`MergeOutput`、`MergeError`、`kv`、`str_seq`、`val_str`、`inject_auto_group`；**删除** `is_valid_member`、`dfs_cycle`、`find_cycle`）：

```rust
/// 组装 config.yaml。顶层键顺序：网络段 → profile → proxy-groups → rules → proxies。
/// 订阅组原样透传（不再去重/剔除失效成员/循环检测，订阅侧问题由 mihomo -t 预校验兜底）。
pub fn merge(ctx: MergeContext) -> Result<MergeOutput, MergeError> {
    let mut warnings: Vec<String> = Vec::new();

    // ---------- 1. 订阅节点（重名去重：保留首个） ----------
    let mut nodes: Vec<Value> = Vec::new();
    let mut node_names: Vec<String> = Vec::new();
    if let Some(cache) = ctx.subscription.and_then(|s| s.cache.as_ref()) {
        for p in &cache.proxies {
            if node_names.contains(&p.name) {
                warnings.push(format!("订阅节点重名：「{}」重复，保留第一个", p.name));
                continue;
            }
            node_names.push(p.name.clone());
            nodes.push(p.yaml.clone());
        }
    }

    // ---------- 2. 订阅组原样透传（保序、保字段，不做任何过滤/校验） ----------
    let mut groups: Vec<Value> = Vec::new();
    let mut group_names: Vec<String> = Vec::new();
    if let Some(cache) = ctx.subscription.and_then(|s| s.cache.as_ref()) {
        for g in &cache.proxy_groups {
            let Some(m) = g.as_mapping() else { continue };
            let Some(name) = m.get(Value::String("name".into())).and_then(|v| v.as_str()) else {
                continue;
            };
            group_names.push(name.to_string());
            groups.push(Value::Mapping(m.clone()));
        }
    }

    // ---------- 3. 兜底自动组：有节点但无任何组 ----------
    if !nodes.is_empty() && groups.is_empty() {
        inject_auto_group(
            &mut groups,
            &mut group_names,
            &node_names,
            &mut warnings,
            "订阅有节点但无任何组",
        );
    }

    // ---------- 4. 引用校验目标集 ----------
    let mut targets: Vec<String> = node_names.clone();
    targets.extend(group_names.iter().cloned());
    targets.extend(BUILTIN_TARGETS.iter().map(|s| s.to_string()));

    // ---------- 5. 自定义规则（target 校验 → MergeError） ----------
    let mut rules: Vec<String> = Vec::new();
    for r in &ctx.overrides.rules {
        let s = if r.rule_type == "MATCH" {
            format!("MATCH,{}", r.target)
        } else {
            format!("{},{},{}", r.rule_type, r.payload, r.target)
        };
        if !targets.contains(&r.target) {
            return Err(MergeError {
                message: format!(
                    "自定义规则「{s}」的目标「{}」不存在（可用：订阅节点/组/内置 {}）",
                    r.target,
                    BUILTIN_TARGETS.join("/")
                ),
            });
        }
        rules.push(s);
    }

    // ---------- 6. 订阅规则（去重 + 目标校验，丢弃记 warning） ----------
    if let Some(cache) = ctx.subscription.and_then(|s| s.cache.as_ref()) {
        for r in &cache.rules {
            if rules.contains(r) {
                warnings.push(format!("订阅规则「{r}」与已有规则重复，已丢弃"));
                continue;
            }
            // 目标解析：mihomo 规则格式 TYPE,payload,target[,选项...]。
            // MATCH 类（无 payload）目标在第 2 段，其余类型目标在第 3 段。
            let parts: Vec<&str> = r.split(',').collect();
            let target = match parts.as_slice() {
                ["MATCH", t, ..] => *t,
                [_, _, t, ..] => *t,
                _ => {
                    warnings.push(format!("订阅规则「{r}」格式异常，已丢弃"));
                    continue;
                }
            };
            if !targets.contains(&target.to_string()) {
                warnings.push(format!(
                    "订阅规则「{r}」的目标「{target}」不存在，已丢弃该规则"
                ));
                continue;
            }
            rules.push(r.clone());
        }
    }

    // ---------- 7. 兜底默认规则：有节点但无任何规则 ----------
    if !nodes.is_empty() && rules.is_empty() {
        inject_auto_group(
            &mut groups,
            &mut group_names,
            &node_names,
            &mut warnings,
            "订阅无规则，默认规则模板引用自动组",
        );
        warnings.push("订阅无规则，已注入默认规则模板".into());
        rules.extend(DEFAULT_RULES.iter().map(|s| s.to_string()));
    }

    // ---------- 8. 组装（serde_yaml::Mapping 保序） ----------
    let s = ctx.settings;
    let mut root = Mapping::new();
    kv(&mut root, "port", s.port);
    kv(&mut root, "socks-port", s.socks_port);
    kv(&mut root, "mixed-port", s.mixed_port);
    kv(&mut root, "allow-lan", s.allow_lan);
    kv(&mut root, "mode", s.mode.clone());
    kv(&mut root, "ipv6", s.ipv6);
    kv(&mut root, "log-level", s.log_level.clone());
    kv(&mut root, "external-controller", s.external_controller.clone());
    kv(&mut root, "secret", s.secret.clone());
    let mut tun = Mapping::new();
    kv(&mut tun, "enable", s.tun.enable);
    kv(&mut tun, "stack", s.tun.stack.clone());
    kv(&mut tun, "auto-route", s.tun.auto_route);
    kv(&mut tun, "dns-hijack", str_seq(s.tun.dns_hijack.iter().cloned()));
    kv(&mut tun, "mtu", s.tun.mtu);
    kv(&mut root, "tun", Value::Mapping(tun));
    let mut dns = Mapping::new();
    kv(&mut dns, "enable", s.dns.enable);
    kv(&mut dns, "listen", s.dns.listen.clone());
    kv(&mut dns, "enhanced-mode", s.dns.enhanced_mode.clone());
    kv(&mut dns, "fake-ip-range", s.dns.fake_ip_range.clone());
    kv(&mut dns, "nameserver", str_seq(s.dns.nameserver.iter().cloned()));
    kv(&mut dns, "default-nameserver", str_seq(s.dns.default_nameserver.iter().cloned()));
    kv(&mut dns, "fallback", str_seq(s.dns.fallback.iter().cloned()));
    kv(&mut dns, "fake-ip-filter", str_seq(s.dns.fake_ip_filter.iter().cloned()));
    kv(&mut root, "dns", Value::Mapping(dns));
    // select 组选择持久化：重启后保持运行时切换的节点
    let mut profile = Mapping::new();
    kv(&mut profile, "store-selected", true);
    kv(&mut root, "profile", Value::Mapping(profile));

    if !groups.is_empty() {
        kv(&mut root, "proxy-groups", Value::Sequence(groups));
    }
    if !rules.is_empty() {
        kv(&mut root, "rules", str_seq(rules));
    }
    if !nodes.is_empty() {
        kv(&mut root, "proxies", Value::Sequence(nodes));
    }

    let config = serde_yaml::to_string(&Value::Mapping(root))
        .map_err(|e| MergeError { message: format!("YAML 序列化失败: {e}") })?;
    Ok(MergeOutput { config, warnings })
}
```

注意：`use crate::core::models::...` 中移除 `default_group_interval, default_test_url`（不再使用）；`HashMap` import 一并移除。

- [ ] **Step 3: 删除/重写 merger 测试**

`src/core/merger.rs` tests 模块中**删除**以下测试（自定义组相关，不再适用）：
`custom_group_wins_over_sub_group`、`custom_group_bad_member_is_error`、`custom_group_name_conflicts_with_node`、`empty_custom_group_is_error`、`empty_custom_group_is_error_without_nodes`、`duplicate_custom_group_names_are_error`、`url_test_and_fallback_group_fields`、`custom_group_with_builtin_member_ok`、`custom_group_references_later_custom_group`、`top_level_select_references_sub_groups`、`sub_group_references_custom_group_kept`、`direct_cycle_custom_group_is_error`、`indirect_cycle_is_error`、`sub_group_cycle_is_error`、`custom_group_references_missing_group_is_error`、`sub_group_references_dropped_sub_group_cascades`、`sub_rule_referencing_dropped_group_is_dropped`（订阅组不再被丢弃）。

**保留但修改**：
- `full_merge`：去掉自定义组（`o.groups.push` 两行删除，规则目标改为订阅组），顶层键顺序期望改为 `["port","socks-port","mixed-port","allow-lan","mode","ipv6","log-level","external-controller","secret","tun","dns","profile","proxy-groups","rules","proxies"]`，并断言 `v["profile"]["store-selected"] == Value::Bool(true)`。
- `sub_group_name_conflicts_with_node` → 改名 `sub_group_name_same_as_node_passthrough`：重名订阅组**不再丢弃**，断言输出组数 = 2（原"冲突节点"组 + "正常组"），warnings 为空。
- `sub_group_missing_members_dropped` → 改名 `sub_group_missing_members_passthrough`：幽灵成员**不再剔除**，断言"好组"成员数 = 2（`["节点1","幽灵"]`），warnings 为空。
- `default_rules_with_custom_groups_injects_auto_group` → 去掉自定义组，断言自动组 + 默认规则注入。

**新增测试**：

```rust
// ---- 补充：订阅组原样透传（重名/空成员/幽灵成员/循环均不干预） ----

#[test]
fn subscription_groups_passthrough_verbatim() {
    let s = sub(
        vec![node("节点1"), node("节点2")],
        vec![
            sub_group("重名组", &["节点1"]),
            sub_group("重名组", &["节点2"]), // 重名保留（mihomo -t 兜底）
            sub_group("空组", &[]),         // 空组保留
            sub_group("幽灵组", &["不存在"]), // 失效成员保留
        ],
        vec!["MATCH,幽灵组".into()],
    );
    let out = do_merge(Overrides::default(), Some(s));
    assert!(out.warnings.is_empty(), "透传不应有警告: {:?}", out.warnings);
    let v = parse_out(&out);
    let gs = v["proxy-groups"].as_sequence().unwrap();
    assert_eq!(gs.len(), 4, "全部组原样保留: {gs:?}");
    assert_eq!(gs[0]["name"], Value::String("重名组".into()));
    assert_eq!(gs[1]["name"], Value::String("重名组".into()));
    assert_eq!(gs[2]["name"], Value::String("空组".into()));
    let members: Vec<String> = gs[3]["proxies"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|m| m.as_str().unwrap().to_string())
        .collect();
    assert_eq!(members, vec!["不存在"], "幽灵成员原样保留");
}

#[test]
fn subscription_group_cycle_passthrough() {
    let s = sub(
        vec![node("节点1")],
        vec![sub_group("S1", &["S2"]), sub_group("S2", &["S1"])],
        vec![],
    );
    let out = do_merge(Overrides::default(), Some(s));
    let v = parse_out(&out);
    let gs = v["proxy-groups"].as_sequence().unwrap();
    assert_eq!(gs.len(), 3, "循环组透传 + 兜底自动组: {gs:?}");
    assert_eq!(gs[0]["name"], Value::String("S1".into()));
    assert_eq!(gs[1]["name"], Value::String("S2".into()));
}

#[test]
fn profile_store_selected_written() {
    let s = sub(vec![node("节点1")], vec![], vec!["MATCH,节点1".into()]);
    let out = do_merge(Overrides::default(), Some(s));
    let v = parse_out(&out);
    assert_eq!(v["profile"]["store-selected"], Value::Bool(true));
}

#[test]
fn sub_group_url_test_extra_fields_passthrough() {
    // url-test 组带 url/interval/tolerance 等扩展字段：原样输出不丢字段
    let mut m = Mapping::new();
    m.insert(Value::String("name".into()), Value::String("自动选择".into()));
    m.insert(Value::String("type".into()), Value::String("url-test".into()));
    m.insert(Value::String("url".into()), Value::String("http://x/generate_204".into()));
    m.insert(Value::String("interval".into()), Value::Number(120.into()));
    m.insert(Value::String("tolerance".into()), Value::Number(50.into()));
    m.insert(
        Value::String("proxies".into()),
        Value::Sequence(vec![Value::String("节点1".into())]),
    );
    let s = sub(vec![node("节点1")], vec![Value::Mapping(m)], vec!["MATCH,自动选择".into()]);
    let out = do_merge(Overrides::default(), Some(s));
    let v = parse_out(&out);
    let gs = v["proxy-groups"].as_sequence().unwrap();
    assert_eq!(gs.len(), 1);
    assert_eq!(gs[0]["url"], Value::String("http://x/generate_204".into()));
    assert_eq!(gs[0]["interval"], Value::Number(120.into()));
    assert_eq!(gs[0]["tolerance"], Value::Number(50.into()));
}
```

保留原样：`duplicate_subscription_proxies_keep_first`、`fallback_auto_group_and_default_rules`、`no_subscription_no_proxies_no_template`、`builtin_targets_are_valid`、`match_rule_serialization`、`geoip_rule_serialization`、`duplicate_sub_rules_dropped`、`subscription_without_cache_is_empty`、`sub_group_builtin_members_kept`、`sub_group_only_builtin_members_survives`、`custom_rule_bad_target_is_error`、`sub_group_chain_reference_kept`。

- [ ] **Step 4: rules.rs target_options 移除自定义组**

`src/ui/rules.rs` `fn target_options` 中删除这一段（自定义组名）：

```rust
        // 2. 自定义组名
        for g in &st.overrides.groups {
            if seen.insert(g.name.clone()) {
                opts.push(g.name.clone());
            }
        }
```

（保留 BUILTIN_TARGETS + 激活订阅组名两段；同时更新该文件顶部 doc 注释中"目标下拉 = 内置目标 ∪ 自定义组 ∪ 激活订阅组"为"内置目标 ∪ 激活订阅组"。）

- [ ] **Step 5: 运行测试**

```bash
cargo test --lib merger 2>&1 | tail -30
```
Expected: 所有保留/新增测试通过，无编译错误。

- [ ] **Step 6: 提交**

```bash
git add -A && git commit -m "refactor: 合并器移除自定义规则组逻辑，订阅组原样透传 + store-selected 持久化"
```

---

## Task 2: REST 客户端新增组 API（W2）

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/core/client.rs`
- Test: `src/core/client.rs`（内嵌 tests 模块）

- [ ] **Step 1: Cargo.toml 加依赖**

`Cargo.toml` 的 `[dependencies]` 中 `url = "2"` 行后加：

```toml
percent-encoding = "2"
```

- [ ] **Step 2: 新增 GroupInfo 与编码辅助**

`src/core/client.rs` 中 `MemoryFrame` 之后新增：

```rust
/// 运行时策略组信息（GET /proxies 中的组条目）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GroupInfo {
    pub name: String,
    /// "Selector" | "URLTest" | "Fallback" | "LoadBalance" | "Relay" | "Compatible" | "Pass"
    pub group_type: String,
    /// 当前选中项（自动组为当前生效节点；无选择时 None）
    pub now: Option<String>,
    /// 全部可选成员
    pub all: Vec<String>,
}

impl FromStr for GroupInfo {
    type Err = ApiError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let v: serde_json::Value =
            serde_json::from_str(s).map_err(|e| ApiError::Json(e.to_string()))?;
        Ok(Self {
            name: v.get("name").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
            group_type: v.get("type").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
            now: v.get("now").and_then(|x| x.as_str()).map(|s| s.to_string()),
            all: v
                .get("all")
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|i| i.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default(),
        })
    }
}
```

文件顶部 import 处加 `use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};`，并新增常量与编码函数（`impl Client` 外、`LineStream` 前）：

```rust
/// 整组延迟测试：单节点超时（ms）。超时节点 mihomo 返回 8000，UI 按 >= 此值显示超时。
pub const GROUP_DELAY_TIMEOUT_MS: u16 = 5000;
/// 整组延迟测试：测试 URL（与合并器默认测速地址一致）。
pub const GROUP_DELAY_TEST_URL: &str = "http://www.gstatic.com/generate_204";

/// 路径段百分号编码（组名可含中文/emoji/空格）。
fn encode_path_segment(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}
```

- [ ] **Step 3: 新增三个 API 方法**

`impl Client` 中 `memory_stream` 之后新增：

```rust
    /// GET /proxies → 全部策略组信息（仅含带 all 字段的组条目，节点被过滤）。
    pub async fn get_proxies(&self) -> Result<Vec<GroupInfo>, ApiError> {
        let body = self.request_text(reqwest::Method::GET, "/proxies").await?;
        let v: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| ApiError::Json(e.to_string()))?;
        let mut out = Vec::new();
        if let Some(proxies) = v.get("proxies").and_then(|x| x.as_object()) {
            for (key, entry) in proxies {
                if entry.get("all").and_then(|x| x.as_array()).is_none() {
                    continue; // 节点无 all 字段，仅策略组保留
                }
                let mut gi: GroupInfo = entry
                    .to_string()
                    .parse()
                    .map_err(|e: ApiError| ApiError::Json(format!("组「{key}」解析失败: {e}")))?;
                if gi.name.is_empty() {
                    gi.name = key.clone();
                }
                out.push(gi);
            }
        }
        Ok(out)
    }

    /// PUT /proxies/{name}，body {"name": target}：切换 select 组当前节点。
    pub async fn switch_group(&self, name: &str, target: &str) -> Result<(), ApiError> {
        let mut req = self
            .http
            .put(self.url(&format!("/proxies/{}", encode_path_segment(name))))
            .timeout(REQUEST_TIMEOUT)
            .header(CONTENT_TYPE, "application/json");
        if let Some(auth) = self.auth_header() {
            req = req.header(AUTHORIZATION, auth);
        }
        let resp = req
            .body(serde_json::json!({ "name": target }).to_string())
            .send()
            .await
            .map_err(|e| ApiError::Conn(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(ApiError::Status(resp.status().as_u16()));
        }
        Ok(())
    }

    /// GET /group/{name}/delay → 整组延迟测试，返回 (节点名, 延迟ms) 列表。
    /// 组内节点多时整体较慢，请求超时放宽到 30s。
    pub async fn test_group_delay(&self, name: &str) -> Result<Vec<(String, u16)>, ApiError> {
        let path = format!(
            "/group/{}/delay?url={}&timeout={}",
            encode_path_segment(name),
            encode_path_segment(GROUP_DELAY_TEST_URL),
            GROUP_DELAY_TIMEOUT_MS
        );
        let mut req = self
            .http
            .get(self.url(&path))
            .timeout(Duration::from_secs(30));
        if let Some(auth) = self.auth_header() {
            req = req.header(AUTHORIZATION, auth);
        }
        let resp = req.send().await.map_err(|e| ApiError::Conn(e.to_string()))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| ApiError::Http(e.to_string()))?;
        if !status.is_success() {
            return Err(ApiError::Status(status.as_u16()));
        }
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| ApiError::Json(e.to_string()))?;
        let mut out = Vec::new();
        if let Some(proxies) = v.get("proxies").and_then(|x| x.as_object()) {
            for (n, d) in proxies {
                if let Some(ms) = d.as_u64() {
                    out.push((n.clone(), ms.min(u16::MAX as u64) as u16));
                }
            }
        }
        Ok(out)
    }
```

- [ ] **Step 4: 假服务器扩展 + 新测试**

`spawn_api_server` 中路由分支补充（`"/configs"` 分支前插入）：

```rust
                    let encoded = first.split(' ').nth(1).unwrap_or("/").to_string();
                    let path = encoded.split('?').next().unwrap_or("").to_string();
                    if first.starts_with("PUT") && path.starts_with("/proxies/") {
                        // PUT /proxies/{name} → 204（非 200，用于断言成功路径）
                        let _ = sock
                            .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .await;
                        return;
                    }
                    let body = match path.as_str() {
                        "/proxies" => r#"{"proxies":{
                            "节点A":{"name":"节点A","type":"Shadowsocks","history":[]},
                            "手动选择":{"name":"手动选择","type":"Selector","now":"节点B","all":["节点A","节点B","DIRECT"]},
                            "自动选择":{"name":"自动选择","type":"URLTest","now":"节点A","all":["节点A","节点B"]}
                        }}"#.to_string(),
                        _ if path.starts_with("/group/") && path.ends_with("/delay") => {
                            r#"{"proxies":{"节点A":123,"节点B":8000}}"#.to_string()
                        }
                        "/version" => ...（原样保留）
```

注意：原实现里 `let path = first.split(' ').nth(1).unwrap_or("/");` 直接用于 match——需要改成先切掉 query 再匹配（新增上面两行，原 `let path` 行删除）。`/group/%E8%87%AA.../delay` 带编码路径 + query，`starts_with("/group/") && ends_with("/delay")` 匹配。`ends_with("/delay")` 在去掉 query 后成立。

新增测试（tests 模块末尾）：

```rust
    #[test]
    fn group_info_from_json() {
        let gi = GroupInfo::from_str(
            r#"{"name":"手动选择","type":"Selector","now":"节点B","all":["节点A","节点B","DIRECT"]}"#,
        )
        .unwrap();
        assert_eq!(gi.name, "手动选择");
        assert_eq!(gi.group_type, "Selector");
        assert_eq!(gi.now.as_deref(), Some("节点B"));
        assert_eq!(gi.all, vec!["节点A", "节点B", "DIRECT"]);
    }

    #[test]
    fn group_info_no_now_is_none() {
        let gi = GroupInfo::from_str(r#"{"name":"g","type":"URLTest","all":["a"]}"#).unwrap();
        assert_eq!(gi.now, None);
        assert_eq!(gi.all, vec!["a"]);
    }

    #[tokio::test]
    async fn get_proxies_filters_nodes_keeps_groups() {
        let (port, _rx) = spawn_api_server().await;
        let groups = client_on(port).get_proxies().await.unwrap();
        let names: Vec<&str> = groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, vec!["手动选择", "自动选择"], "节点应被过滤: {names:?}");
        assert_eq!(groups[0].group_type, "Selector");
        assert_eq!(groups[0].now.as_deref(), Some("节点B"));
        assert_eq!(groups[0].all.len(), 3);
        assert_eq!(groups[1].group_type, "URLTest");
    }

    #[tokio::test]
    async fn switch_group_sends_put_with_body_and_auth() {
        let (port, mut rx) = spawn_api_server().await;
        client_on(port)
            .switch_group("手动选择", "节点A")
            .await
            .unwrap();
        let req = rx.recv().await.expect("服务器应收到请求");
        assert!(req.starts_with("PUT /proxies/"), "请求行: {req}");
        assert!(
            req.to_lowercase().contains("authorization: bearer testsecret"),
            "应带 Bearer 鉴权: {req}"
        );
        assert!(req.contains(r#"{"name":"节点A"}"#), "body 应为选择目标: {req}");
    }

    #[tokio::test]
    async fn switch_group_encodes_unicode_name() {
        let (port, mut rx) = spawn_api_server().await;
        client_on(port).switch_group("🚀 节点选择", "DIRECT").await.unwrap();
        let req = rx.recv().await.expect("服务器应收到请求");
        let first_line = req.lines().next().unwrap_or("");
        assert!(
            first_line.starts_with("PUT /proxies/%F0%9F%9A%80%20"),
            "组名应百分号编码: {first_line}"
        );
        assert!(!first_line.contains('🚀'), "不应含裸 emoji: {first_line}");
    }

    #[tokio::test]
    async fn test_group_delay_parses_proxies_map() {
        let (port, _rx) = spawn_api_server().await;
        let list = client_on(port).test_group_delay("自动选择").await.unwrap();
        assert_eq!(list, vec![("节点A".to_string(), 123), ("节点B".to_string(), 8000)]);
    }

    #[tokio::test]
    async fn switch_group_http_400_returns_status_error() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                match sock.read(&mut tmp).await {
                    Ok(0) => break,
                    Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    Err(_) => break,
                }
            }
            let _ = sock
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await;
        });
        let e = client_on(port)
            .switch_group("不存在", "x")
            .await
            .unwrap_err();
        assert!(matches!(e, ApiError::Status(400)), "期望 Status(400)，实际: {e}");
    }
```

注意：`test_group_delay_parses_proxies_map` 断言顺序——serde_json 无 preserve_order 时 object 迭代为 BTreeMap 字母序（"节点A" < "节点B" ✓）。

- [ ] **Step 5: 运行测试**

```bash
cargo test --lib client 2>&1 | tail -30
```
Expected: 新旧测试全部通过。若 `GroupInfo` 缺 `PartialEq` 编译错——已 derive。若假服务器 query 切分有误（`first.split(' ').nth(1)` 是 `"GET /path?x HTTP/1.1"` 的第二段，含 query），按 Step 4 的 `encoded.split('?').next()` 处理。

- [ ] **Step 6: 提交**

```bash
git add -A && git commit -m "feat: client 新增 get_proxies/switch_group/test_group_delay 与组信息模型"
```

---

## Task 3: app.rs 状态机与迁移（W3）

**Files:**
- Modify: `src/app.rs`
- Test: `src/app.rs`（内嵌 tests 模块）

- [ ] **Step 1: AppState 新字段 + 迁移函数**

`src/app.rs`：

```rust
use crate::core::client::{Client, GroupInfo, MemoryFrame, RuntimeConfig, TrafficFrame};
```

`AppState` 增加字段（`exit_ip` 之后）：

```rust
    /// 运行时策略组快照（GET /proxies；规则组页数据源）
    pub proxy_groups: Vec<GroupInfo>,
```

`AppState::load()` 的 overrides 加载处改造（`let overrides = match load_overrides() ...` 之后）：

```rust
        let mut overrides = match load_overrides() {
            Ok(o) => o,
            Err(e) => {
                notices.push_back(format!("[✗] 加载规则覆盖失败: {e}"));
                Overrides::default()
            }
        };
        // 旧版自定义规则组数据迁移（方案 A：清空 + 一次提示）
        if let Some(msg) = migrate_legacy_groups(&mut overrides) {
            if let Err(e) = save_overrides(&overrides) {
                notices.push_back(format!("[✗] 旧数据清理落盘失败: {e}"));
            }
            notices.push_back(msg);
        }
```

`AppState::load` 的 `Self { ... }` 构造中加 `proxy_groups: Vec::new(),`。模块级新增纯函数（`now_rfc3339` 附近）：

```rust
/// 迁移旧版自定义规则组：非空则清空并返回提示（无旧数据返回 None）。纯函数便于测试。
fn migrate_legacy_groups(overrides: &mut Overrides) -> Option<String> {
    if overrides.groups.is_empty() {
        return None;
    }
    let n = overrides.groups.len();
    overrides.groups.clear();
    Some(format!("[!] 已清空 {n} 个旧版自定义规则组（规则组页现只读展示订阅内容）"))
}
```

- [ ] **Step 2: UiCommand/UiEvent 新变体 + spawn_command + on_ui_event**

```rust
pub enum UiCommand {
    PatchConfigs(serde_json::Value),
    ApplyConfig(String),
    FetchSubscription(usize),
    FetchExitIp,
    ReloadConfigs,
    InstallSetup,
    /// 拉取运行时策略组（GET /proxies）
    RefreshGroups,
    /// 切换 select 组当前节点（PUT /proxies）
    SwitchGroup { group: String, target: String },
    /// 整组延迟测试（GET /group/{name}/delay）
    TestGroupDelay(String),
}

pub enum UiEvent {
    PatchDone(Result<(), String>),
    ApplyDone(Result<ApplyOutcome, String>),
    SubscriptionFetched(usize, Result<SubscriptionCache, String>),
    ExitIp(Result<String, String>),
    ConfigsRefreshed(Result<RuntimeConfig, String>),
    GroupsRefreshed(Result<Vec<GroupInfo>, String>),
    GroupSwitched { group: String, target: String, result: Result<(), String> },
    GroupDelayDone { group: String, result: Result<Vec<(String, u16)>, String> },
}
```

`spawn_command` 的 `UiCommand::ReloadConfigs` 分支后新增：

```rust
            UiCommand::RefreshGroups => {
                let ui_tx = ui_tx.clone();
                tokio::spawn(async move {
                    let res = client.get_proxies().await.map_err(|e| e.to_string());
                    let _ = ui_tx.send(UiEvent::GroupsRefreshed(res));
                });
            }
            UiCommand::SwitchGroup { group, target } => {
                let ui_tx = ui_tx.clone();
                tokio::spawn(async move {
                    let res = client
                        .switch_group(&group, &target)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = ui_tx.send(UiEvent::GroupSwitched { group, target, result: res });
                });
            }
            UiCommand::TestGroupDelay(group) => {
                let ui_tx = ui_tx.clone();
                tokio::spawn(async move {
                    let res = client
                        .test_group_delay(&group)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = ui_tx.send(UiEvent::GroupDelayDone { group, result: res });
                });
            }
```

`on_ui_event` 的 `ConfigsRefreshed` 分支后新增三个分支：

```rust
            UiEvent::GroupsRefreshed(res) => match res {
                Ok(groups) => self.state.proxy_groups = groups,
                // API 连接失败已有独立通知（set_api_ok），此处静默清空降级到订阅缓存展示
                Err(_) => self.state.proxy_groups.clear(),
            },
            UiEvent::GroupSwitched { group, target, result } => match result {
                Ok(()) => {
                    self.state
                        .notice(format!("[✓] 已切换「{group}」→「{target}」"));
                    let _ = self.cmd_tx.send(UiCommand::RefreshGroups);
                }
                Err(e) => self.popup_error("切换失败", e),
            },
            UiEvent::GroupDelayDone { group, result } => match result {
                Ok(list) => {
                    self.result_popup = Some(MessagePopup::new(
                        format!("延迟测试：{group}"),
                        delay_lines(&list),
                    ));
                    self.state.notice(format!("[✓] 延迟测试完成：{group}"));
                    let _ = self.cmd_tx.send(UiCommand::RefreshGroups);
                }
                Err(e) => self.popup_error("延迟测试失败", e),
            },
```

模块级新增纯函数（`page_hints` 附近，测试用）：

```rust
/// 延迟测试结果行：按延迟升序，超时（>= GROUP_DELAY_TIMEOUT_MS）排最后。
fn delay_lines(list: &[(String, u16)]) -> Vec<String> {
    let mut items: Vec<(&String, u16)> = list.iter().collect();
    items.sort_by_key(|(_, ms)| *ms);
    items
        .iter()
        .map(|(n, ms)| {
            if *ms >= GROUP_DELAY_TIMEOUT_MS {
                format!("{n}  超时")
            } else {
                format!("{n}  {ms}ms")
            }
        })
        .collect()
}
```

import 更新：`use crate::core::client::{Client, GroupInfo, MemoryFrame, RuntimeConfig, TrafficFrame};`、`use crate::core::client::GROUP_DELAY_TIMEOUT_MS;`（或合并进上一条）。

- [ ] **Step 3: 切页触发刷新 + 订阅事件触发刷新**

`handle_key` 中现有四个切页分支（`Tab`/`BackTab`/`Right`/`Left`/数字 `'1'..='4'`）统一改为调用新 helper（保留原语义）：

```rust
            KeyCode::Tab => self.switch_page((self.current + 1) % self.pages.len()),
            KeyCode::BackTab => {
                self.switch_page((self.current + self.pages.len() - 1) % self.pages.len());
            }
            KeyCode::Right => self.switch_page((self.current + 1) % self.pages.len()),
            KeyCode::Left => {
                self.switch_page((self.current + self.pages.len() - 1) % self.pages.len());
            }
            KeyCode::Char(c) if ('1'..='4').contains(&c) => {
                self.switch_page(c.to_digit(10).unwrap_or(1) as usize - 1);
            }
```

`impl App` 内新增（`handle_key` 之前）：

```rust
    /// 切页；进入规则组页（index 2）时刷新运行时策略组。
    fn switch_page(&mut self, idx: usize) {
        self.current = idx;
        if idx == 2 {
            let _ = self.cmd_tx.send(UiCommand::RefreshGroups);
        }
    }
```

`on_ui_event` 的 `UiEvent::SubscriptionFetched` Ok 分支末尾（`self.state.notice(...)` 后）：

```rust
                    // 订阅内容变化可能影响规则组：当前在规则组页则刷新
                    if self.current == 2 {
                        let _ = self.cmd_tx.send(UiCommand::RefreshGroups);
                    }
```

- [ ] **Step 4: HELP_LINES 与 page_hints 更新**

`HELP_LINES` 中"规则组:"段替换为：

```rust
    "规则组:",
    "  Enter              切换节点（select 组）",
    "  r                  整组延迟测试",
    "  R                  刷新组列表",
```

`page_hints` 的 `2 =>` 分支替换为：

```rust
        2 => vec![
            ("Enter".into(), "切换".into()),
            ("r".into(), "测速".into()),
            ("R".into(), "刷新".into()),
        ],
```

- [ ] **Step 5: 测试更新与新增**

`test_app` 返回类型改为 `(App<TestBackend>, mpsc::UnboundedReceiver<UiCommand>)`，`AppState` 构造加 `proxy_groups: Vec::new(),`，返回 `(app, cmd_rx)`。更新现有调用点：

```rust
    fn test_app(h: u16) -> (App<TestBackend>, mpsc::UnboundedReceiver<UiCommand>) {
        ...
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        ...
        (App { ... }, cmd_rx)
    }
```

现有三个测试（`draw_tiny_terminal_no_panic`、`exit_ip_recovery_*`）改为 `let (mut app, _rx) = test_app(h);`。

新增测试：

```rust
    #[test]
    fn migrate_legacy_groups_clears_and_reports() {
        let mut o = Overrides::default();
        assert_eq!(migrate_legacy_groups(&mut o), None, "无旧数据不提示");
        o.groups.push(UserGroup {
            name: "旧组".into(),
            group_type: "select".into(),
            url: String::new(),
            interval: 0,
            tolerance: 0,
            proxies: vec!["节点1".into()],
        });
        let msg = migrate_legacy_groups(&mut o).expect("有旧数据应提示");
        assert!(o.groups.is_empty(), "应清空");
        assert!(msg.contains("1 个旧版自定义规则组"), "提示应含数量: {msg}");
    }

    #[test]
    fn groups_refreshed_updates_state() {
        let (mut app, _rx) = test_app(24);
        let groups = vec![GroupInfo {
            name: "手动选择".into(),
            group_type: "Selector".into(),
            now: Some("节点A".into()),
            all: vec!["节点A".into(), "DIRECT".into()],
        }];
        app.on_ui_event(UiEvent::GroupsRefreshed(Ok(groups.clone())));
        assert_eq!(app.state.proxy_groups, groups);
        app.on_ui_event(UiEvent::GroupsRefreshed(Err("连接失败".into())));
        assert!(app.state.proxy_groups.is_empty(), "失败应清空降级");
    }

    #[test]
    fn group_switched_success_notices_and_refreshes() {
        let (mut app, mut rx) = test_app(24);
        app.on_ui_event(UiEvent::GroupSwitched {
            group: "手动选择".into(),
            target: "节点A".into(),
            result: Ok(()),
        });
        assert!(
            app.state.notices.iter().any(|n| n.contains("已切换「手动选择」→「节点A」")),
            "应通知切换成功: {:?}",
            app.state.notices
        );
        let cmd = rx.try_recv().expect("应发送刷新命令");
        assert!(matches!(cmd, UiCommand::RefreshGroups), "命令: {cmd:?}");
    }

    #[test]
    fn group_switched_failure_popup() {
        let (mut app, _rx) = test_app(24);
        app.on_ui_event(UiEvent::GroupSwitched {
            group: "g".into(),
            target: "x".into(),
            result: Err("HTTP 状态 400".into()),
        });
        assert_eq!(app.result_popup.as_ref().unwrap().title(), "切换失败");
    }

    #[test]
    fn group_delay_done_popup_and_refresh() {
        let (mut app, mut rx) = test_app(24);
        app.on_ui_event(UiEvent::GroupDelayDone {
            group: "自动选择".into(),
            result: Ok(vec![
                ("节点B".to_string(), 8000),
                ("节点A".to_string(), 123),
            ]),
        });
        let popup = app.result_popup.as_ref().expect("应有结果弹窗");
        assert_eq!(popup.title(), "延迟测试：自动选择");
        let _ = rx.try_recv().expect("应发送刷新命令");
    }

    #[test]
    fn delay_lines_sort_and_timeout() {
        let lines = delay_lines(&[
            ("B".to_string(), 8000),
            ("A".to_string(), 123),
            ("C".to_string(), 5000),
        ]);
        assert_eq!(
            lines,
            vec!["A  123ms".to_string(), "C  超时".to_string(), "B  超时".to_string()],
            "升序 + 超时标记: {lines:?}"
        );
    }
```

注意 `test_app` 中 `cmd_rx` 之前是 `let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();` 且 `App` 构造使用 `cmd_rx`（App 持有 `cmd_rx: mpsc::UnboundedReceiver<UiCommand>`）。修改后 App 仍持有 cmd_rx 字段，测试额外 clone 一份 rx 用于断言——`UnboundedReceiver` 不可 clone，改为：test_app 内部 `let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();` 传给 App，再创建 `let (test_tx, test_rx) = mpsc::unbounded_channel();`？不行——主循环才消费 cmd_rx，测试直接调 on_ui_event 不经过主循环，App 内的 cmd_rx 不会收到命令。**正确方案**：`App` 的 `cmd_rx` 字段保留，但测试需要另一个观察通道——将 `spawn_command` 里 send 的目标是 `self.cmd_tx`（App 持有的发送端）。测试想要观察命令：把 test_app 的 `cmd_tx` clone 出来返回：

```rust
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        ...
        let probe = cmd_tx.clone(); // 测试观察端
        (App { ..., cmd_tx, cmd_rx, ... }, probe)
```

返回类型 `(App<TestBackend>, mpsc::UnboundedSender<UiCommand>)`，测试用 `probe.send` 不适用——观察用 `try_recv` 需要 Receiver。改用：测试直接构造 `let (probe_tx, probe_rx) = mpsc::unbounded_channel();`，把 `app.cmd_tx = probe_tx`？cmd_tx 是 pub 字段吗？App 字段私有（同模块测试可访问——tests 模块在 app.rs 内部，**可以**访问私有字段）。所以：test_app 返回 App 后，测试里 `app.cmd_tx = probe_tx.clone()` 会破坏 App 自身发送？不：App 用 cmd_tx **发送**命令给主循环；测试想观察的正是 App 发出去的命令。把 `app.cmd_tx` 替换为 probe_tx 即可（App 的 cmd_rx 没人消费也无妨——unbounded）。所以：

```rust
    fn test_app(h: u16) -> (App<TestBackend>, mpsc::UnboundedReceiver<UiCommand>) {
        ...
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        ...
        let (probe_tx, probe_rx) = mpsc::unbounded_channel();
        app.cmd_tx = probe_tx;
        (app, probe_rx)
    }
```

App 结构体里 cmd_tx/cmd_rx 都在，cmd_rx 闲置即可。测试 `rx.try_recv()` 观察 App 发出的命令。✓（同模块测试可访问私有字段。）

- [ ] **Step 6: 运行测试**

```bash
cargo test --lib 2>&1 | tail -40
```
Expected: app 测试全绿（此时 groups.rs 尚未重写，页面仍编译——Task 4 会替换）。

- [ ] **Step 7: 提交**

```bash
git add -A && git commit -m "feat: app 状态机支持组刷新/切换/延迟测试与旧自定义组迁移"
```

---

## Task 4: 规则组页重写（W4）

**Files:**
- Rewrite: `src/ui/groups.rs`
- Test: `src/ui/groups.rs`（内嵌 tests 模块，纯函数）

- [ ] **Step 1: 页面全量重写**

`src/ui/groups.rs` 替换为以下内容（保留 SelectList 复用、弹窗状态机模式；删除 Form/Members/Confirm 全部编辑逻辑）：

```rust
//! 规则组页：只读展示运行时策略组（GET /proxies），select 组可切换节点，
//! 自动选择组（url-test/fallback 等）展示但禁选并提示；支持整组延迟测试。
//!
//! 数据源优先级：运行时策略组（含当前选择 now/成员 all）→ 激活订阅缓存组
//! （API 不可用时降级展示名称/类型/成员数，无当前选择）。

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::app::{AppState, UiCommand};
use crate::core::client::GroupInfo;
use crate::ui::widgets::{centered_rect, MessagePopup, SelectList};
use crate::ui::Page;

/// 页面内弹窗状态机
enum GroupPopup {
    /// select 组节点单选
    Selector(SelectorPopup),
    /// 错误/提示
    Message(MessagePopup),
}

pub struct GroupsPage {
    list: SelectList,
    popup: Option<GroupPopup>,
    /// 弹窗对应的组索引
    pending: Option<usize>,
    /// 列表数据签名：内容变化时重建 SelectList
    sig: String,
}

impl Default for GroupsPage {
    fn default() -> Self {
        Self::new()
    }
}

impl GroupsPage {
    pub fn new() -> Self {
        Self {
            list: SelectList::new(Vec::new()),
            popup: None,
            pending: None,
            sig: String::new(),
        }
    }

    /// 运行时组类型 → 展示用 kebab 形式（与订阅 YAML 的 type 一致）。
    fn type_display(t: &str) -> String {
        match t {
            "Selector" => "select".to_string(),
            "URLTest" => "url-test".to_string(),
            "Fallback" => "fallback".to_string(),
            "LoadBalance" => "load-balance".to_string(),
            "Relay" => "relay".to_string(),
            "Compatible" => "compatible".to_string(),
            "Pass" => "pass".to_string(),
            other => other.to_string(),
        }
    }

    /// 是否可手动切换：仅 Selector（select）组。
    fn is_switchable(t: &str) -> bool {
        t == "Selector"
    }

    /// 运行时行：`名称 | select | 当前: 节点X`
    fn row(g: &GroupInfo) -> String {
        let now = g.now.as_deref().unwrap_or("-");
        format!("{} | {} | 当前: {}", g.name, Self::type_display(&g.group_type), now)
    }

    /// 降级行（订阅缓存）：`名称 | select | 成员5 | 当前: -`
    fn fallback_row(name: &str, group_type: &str, members: usize) -> String {
        format!("{name} | {group_type} | 成员{members} | 当前: -")
    }

    /// 订阅缓存组签名：激活订阅 proxy_groups 的 name/type/成员数。
    fn fallback_sig(st: &AppState) -> String {
        let mut items: Vec<(String, String, usize)> = Vec::new();
        if let Some(act) = st.subs.iter().find(|s| s.active) {
            if let Some(cache) = &act.cache {
                for g in &cache.proxy_groups {
                    let m = match g.as_mapping() {
                        Some(m) => m,
                        None => continue,
                    };
                    let name = m
                        .get(serde_yaml::Value::String("name".into()))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                        .to_string();
                    let t = m
                        .get(serde_yaml::Value::String("type".into()))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                        .to_string();
                    let n = m
                        .get(serde_yaml::Value::String("proxies".into()))
                        .and_then(|v| v.as_sequence())
                        .map(|s| s.len())
                        .unwrap_or(0);
                    items.push((name, t, n));
                }
            }
        }
        format!("{items:?}")
    }

    fn sig_of(st: &AppState) -> String {
        format!("{:?}|{}", st.proxy_groups, Self::fallback_sig(st))
    }

    fn rebuild_list(st: &AppState) -> SelectList {
        if !st.proxy_groups.is_empty() {
            let rows: Vec<String> = st.proxy_groups.iter().map(Self::row).collect();
            SelectList::new(rows).with_title(" 规则组（运行时，Enter 切换 / r 测速 / R 刷新） ".to_string())
        } else {
            let mut rows: Vec<String> = Vec::new();
            if let Some(act) = st.subs.iter().find(|s| s.active) {
                if let Some(cache) = &act.cache {
                    for g in &cache.proxy_groups {
                        let m = match g.as_mapping() {
                            Some(m) => m,
                            None => continue,
                        };
                        let name = m
                            .get(serde_yaml::Value::String("name".into()))
                            .and_then(|v| v.as_str())
                            .unwrap_or("?")
                            .to_string();
                        let t = m
                            .get(serde_yaml::Value::String("type".into()))
                            .and_then(|v| v.as_str())
                            .unwrap_or("?")
                            .to_string();
                        let n = m
                            .get(serde_yaml::Value::String("proxies".into()))
                            .and_then(|v| v.as_sequence())
                            .map(|s| s.len())
                            .unwrap_or(0);
                        rows.push(Self::fallback_row(&name, &t, n));
                    }
                }
            }
            SelectList::new(rows).with_title(" 规则组（API 不可用，展示订阅缓存） ".to_string())
        }
    }

    /// 当前选中组：运行时优先。
    fn current_group<'a>(&self, st: &'a AppState) -> Option<&'a GroupInfo> {
        let idx = self.list.selected();
        st.proxy_groups.get(idx)
    }

    /// Enter：select 组 → 单选弹窗；自动组/降级 → 提示。
    fn start_select(&mut self, st: &mut AppState) -> Option<UiCommand> {
        let Some(g) = self.current_group(st) else {
            self.popup = Some(GroupPopup::Message(MessagePopup::new(
                "无法切换".to_string(),
                vec![
                    "运行时 API 不可用（mihomo 未运行或未连接），无法获取/切换节点。".to_string(),
                    "请确认 mihomo 服务已启动，或按 R 刷新。".to_string(),
                ],
            )));
            return None;
        };
        if !Self::is_switchable(&g.group_type) {
            self.popup = Some(GroupPopup::Message(MessagePopup::new(
                "不可手动切换".to_string(),
                vec![format!(
                    "「{}」是 {} 自动选择组，节点由 mihomo 自动测速/健康检查决定，不可手动切换。",
                    g.name,
                    Self::type_display(&g.group_type)
                )],
            )));
            return None;
        }
        if g.all.is_empty() {
            self.popup = Some(GroupPopup::Message(MessagePopup::new(
                "没有可选节点".to_string(),
                vec![format!("「{}」没有可切换的成员。", g.name)],
            )));
            return None;
        }
        let idx = self.list.selected();
        self.pending = Some(idx);
        self.popup = Some(GroupPopup::Selector(SelectorPopup::new(
            format!("选择节点：{}", g.name),
            g.all.clone(),
            g.now.clone(),
        )));
        None
    }

    /// r：整组延迟测试（需要选中组；降级模式提示）。
    fn start_delay_test(&mut self, st: &AppState) -> Option<UiCommand> {
        let Some(g) = self.current_group(st) else {
            self.popup = Some(GroupPopup::Message(MessagePopup::new(
                "无法测速".to_string(),
                vec!["运行时 API 不可用，无法执行延迟测试。".to_string()],
            )));
            return None;
        };
        Some(UiCommand::TestGroupDelay(g.name.clone()))
    }

    /// 单选确认：发切换命令。
    fn confirm_select(&mut self, target: String, st: &mut AppState) -> Option<UiCommand> {
        let group = self
            .pending
            .and_then(|idx| st.proxy_groups.get(idx))
            .map(|g| g.name.clone());
        self.pending = None;
        match group {
            Some(name) => Some(UiCommand::SwitchGroup { group: name, target }),
            None => None,
        }
    }

    fn handle_popup(&mut self, popup: GroupPopup, key: KeyEvent, st: &mut AppState) -> Option<UiCommand> {
        match popup {
            GroupPopup::Selector(mut p) => match p.handle_key(key) {
                Some(SelectAction::Confirm(target)) => self.confirm_select(target, st),
                Some(SelectAction::Cancel) => {
                    self.pending = None;
                    None
                }
                None => {
                    self.popup = Some(GroupPopup::Selector(p));
                    None
                }
            },
            GroupPopup::Message(mut p) => {
                if p.handle_key(key) {
                    None // 关闭
                } else {
                    self.popup = Some(GroupPopup::Message(p));
                    None
                }
            }
        }
    }
}

impl Page for GroupsPage {
    fn popup_open(&self) -> bool {
        self.popup.is_some()
    }

    fn handle_key(&mut self, key: KeyEvent, st: &mut AppState) -> Option<UiCommand> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        if let Some(popup) = self.popup.take() {
            return self.handle_popup(popup, key, st);
        }
        match key.code {
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Char('k') | KeyCode::Up => {
                self.list.handle_key(key);
                None
            }
            KeyCode::Enter => self.start_select(st),
            KeyCode::Char('r') => self.start_delay_test(st),
            KeyCode::Char('R') => Some(UiCommand::RefreshGroups),
            _ => None,
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, st: &AppState) {
        let sig = Self::sig_of(st);
        if sig != self.sig {
            self.sig = sig;
            self.list = Self::rebuild_list(st);
        }
        if self.list.is_empty() {
            let hint = Paragraph::new(Line::from(
                "无可用规则组（无激活订阅或 mihomo 未运行），按 R 刷新",
            ))
            .block(Block::default().borders(Borders::ALL).title(" 规则组 "));
            f.render_widget(hint, centered_rect(60, 30, area));
        } else {
            self.list.render(f, area);
        }
        if let Some(popup) = &mut self.popup {
            match popup {
                GroupPopup::Selector(p) => p.render(f, area),
                GroupPopup::Message(p) => p.render(f, area),
            }
        }
    }
}

/// 单选弹窗动作。
enum SelectAction {
    Confirm(String),
    Cancel,
}

/// select 组节点单选弹窗：j/k 移动、Enter 确认、Esc 取消、当前项 ▶ 标记。
struct SelectorPopup {
    title: String,
    items: Vec<String>,
    now: Option<String>,
    selected: usize,
}

impl SelectorPopup {
    fn new(title: String, items: Vec<String>, now: Option<String>) -> Self {
        Self {
            title,
            items,
            now,
            selected: 0,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<SelectAction> {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if self.selected + 1 < self.items.len() {
                    self.selected += 1;
                }
                None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                None
            }
            KeyCode::Enter => Some(SelectAction::Confirm(self.items[self.selected].clone())),
            KeyCode::Esc => Some(SelectAction::Cancel),
            _ => None,
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect) {
        let rect = centered_rect(60, 60, area);
        let items: Vec<ListItem> = self
            .items
            .iter()
            .map(|n| {
                let mark = if self.now.as_deref() == Some(n.as_str()) { "▶ " } else { "  " };
                ListItem::new(format!("{mark}{n}"))
            })
            .collect();
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(self.title.clone()))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        let mut state = ListState::default();
        state.select(Some(self.selected));
        f.render_stateful_widget(list, rect, &mut state);
        let footer = Paragraph::new(Line::from("j/k 移动  Enter 切换  Esc 取消"));
        let chunks = ratatui::layout::Layout::default()
            .direction(ratatui::layout::Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(rect);
        f.render_widget(footer, chunks[1]);
    }
}
```

import 修正：使用到的 `Constraint`/`Direction`/`Layout` 需在顶部 import（上面代码里 render 用了 `ratatui::layout::Layout::default()` 全路径 + `Constraint` 裸名——统一改为顶部 import：`use ratatui::layout::{Constraint, Direction, Layout, Rect};`，render 内用 `Layout::default()`/`Direction::Vertical`/`Constraint::Min`/`Constraint::Length`）。

- [ ] **Step 2: 纯函数测试**

tests 模块（文件末尾）：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_display_mapping() {
        assert_eq!(GroupsPage::type_display("Selector"), "select");
        assert_eq!(GroupsPage::type_display("URLTest"), "url-test");
        assert_eq!(GroupsPage::type_display("Fallback"), "fallback");
        assert_eq!(GroupsPage::type_display("LoadBalance"), "load-balance");
        assert_eq!(GroupsPage::type_display("Relay"), "relay");
        assert_eq!(GroupsPage::type_display("Compatible"), "compatible");
        assert_eq!(GroupsPage::type_display("Pass"), "pass");
        assert_eq!(GroupsPage::type_display("未知类型"), "未知类型");
    }

    #[test]
    fn is_switchable_only_selector() {
        assert!(GroupsPage::is_switchable("Selector"));
        assert!(!GroupsPage::is_switchable("URLTest"));
        assert!(!GroupsPage::is_switchable("Fallback"));
        assert!(!GroupsPage::is_switchable("LoadBalance"));
        assert!(!GroupsPage::is_switchable("Relay"));
    }

    #[test]
    fn row_format() {
        let g = GroupInfo {
            name: "手动选择".into(),
            group_type: "Selector".into(),
            now: Some("节点A".into()),
            all: vec!["节点A".into()],
        };
        assert_eq!(GroupsPage::row(&g), "手动选择 | select | 当前: 节点A");
        let g2 = GroupInfo {
            name: "自动".into(),
            group_type: "URLTest".into(),
            now: None,
            all: vec![],
        };
        assert_eq!(GroupsPage::row(&g2), "自动 | url-test | 当前: -");
    }

    #[test]
    fn fallback_row_format() {
        assert_eq!(GroupsPage::fallback_row("订阅组", "select", 3), "订阅组 | select | 成员3 | 当前: -");
    }

    #[test]
    fn selector_popup_navigation_and_confirm() {
        let mut p = SelectorPopup::new(
            "选择节点：g".into(),
            vec!["A".into(), "B".into(), "C".into()],
            Some("B".into()),
        );
        // 初始选中第一项
        assert!(matches!(
            p.handle_key(KeyEvent::new(KeyCode::Enter, KeyEventKind::Press)),
            Some(SelectAction::Confirm(ref t)) if t == "A"
        ));
        // 移到 B（当前项）再确认
        let _ = p.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyEventKind::Press));
        assert!(matches!(
            p.handle_key(KeyEvent::new(KeyCode::Enter, KeyEventKind::Press)),
            Some(SelectAction::Confirm(ref t)) if t == "B"
        ));
        // Esc 取消
        assert!(matches!(
            p.handle_key(KeyEvent::new(KeyCode::Esc, KeyEventKind::Press)),
            Some(SelectAction::Cancel)
        ));
        // 越界保护
        let mut p2 = SelectorPopup::new("t".into(), vec!["A".into()], None);
        let _ = p2.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyEventKind::Press));
        assert!(matches!(
            p2.handle_key(KeyEvent::new(KeyCode::Enter, KeyEventKind::Press)),
            Some(SelectAction::Confirm(ref t)) if t == "A"
        ));
    }
}
```

`KeyEvent::new(KeyCode, KeyEventKind)`——crossterm 0.28 的 KeyEvent::new 签名是 `KeyEvent::new(code, modifiers)`！检查：crossterm 0.28 `KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)`。而现有代码用 `key.kind == KeyEventKind::Release` 判断——KeyEvent 有 kind 字段。crossterm 0.28 KeyEvent 结构：`KeyEvent { code, modifiers, kind, state }`，`new(code, modifiers)`（kind 默认 Press）。**修正测试**：用 `KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)`（需要 `use crossterm::event::KeyModifiers;`）。本页面 handle_key 只检查 Release，Press 默认即可。

- [ ] **Step 3: 运行测试**

```bash
cargo test --lib 2>&1 | tail -40
```
Expected: 全部通过（含 app/merger/client 测试）。

- [ ] **Step 4: 端到端验证（真实 mihomo）**

```bash
cargo build 2>&1 | tail -5
curl -s -H "Authorization: Bearer $(grep -oP '(?<=^secret: ).*' /etc/mihomo/config.yaml | head -1)" http://127.0.0.1:9090/proxies | python3 -c "
import json,sys
d=json.load(sys.stdin)
for name,g in d['proxies'].items():
    if 'all' in g:
        print(name, g['type'], g.get('now','-'))
" | head -20
```
确认真实 mihomo 的 /proxies 输出中组条目含 name/type/now/all（用于核对 GroupInfo 解析假设）。随后手工运行 TUI 验证：进入规则组页 → 列表展示 → select 组 Enter 弹单选 → 切换后 notice → url-test 组 Enter 提示不可切换 → r 测速弹结果。TUI 交互验证需要真实终端，由人工执行（或跳过，单测+API 联测已覆盖逻辑）。

- [ ] **Step 5: 提交**

```bash
git add -A && git commit -m "feat: 规则组页重写为只读展示+select 切换+自动组禁选+延迟测试"
```

---

## Task 5: README 更新（W5）

**Files:**
- Modify: `README.md`

- [ ] **Step 1: 功能列表更新**

`README.md` 中"激活订阅的节点、其他自定义组、订阅组与内置目标…"（约 L66）与 **规则组** 功能条目（约 L68）改为：

```markdown
- 规则组页只读展示订阅/运行时策略组：select 组可切换节点（PUT /proxies，重启后保持），
  url-test/fallback 等自动组展示但禁选并提示；支持整组延迟测试
```

（删除"激活订阅的节点、其他自定义组、订阅组与内置目标自动成为规则组页的组员候选"句。）

- [ ] **Step 2: 使用指南"规则组"章节重写（约 L145-169）**

替换为：

```markdown
### 规则组（只读展示 + 运行时切换）

规则组不再支持自定义编辑（增删改已移除），页面展示 mihomo 运行时实际生效的策略组：

1. 列表行 = `组名 | 类型 | 当前选择`。数据来自 `GET /proxies`（API 不可用时降级展示
   激活订阅缓存中的组，当前选择显示 `-`）
2. `Enter` 切换节点：**select（手动选择）组**弹出成员单选列表（`▶` 标记当前项），
   `j/k` 移动、`Enter` 确认——通过 `PUT /proxies/{组名}` 切换，成功即时生效并刷新
3. **自动选择组**（url-test / fallback / load-balance 等）按 `Enter` 会提示
   "自动选择，不可手动切换"——节点由 mihomo 自动测速/健康检查决定
4. `r` 整组延迟测试（`GET /group/{组名}/delay`）：结果弹窗按延迟升序显示各节点
   延迟，超时显示"超时"；`R` 手动刷新组列表（进入页面/切换节点/测速后自动刷新）
5. 选择持久化：合并器输出 `profile: store-selected: true`，select 组的运行时选择
   写入 mihomo 缓存，重启后保持
```

- [ ] **Step 3: 按键表更新（约 L283）**

```markdown
**规则组页**：`Enter` 切换节点（select 组）· `r` 整组延迟测试 · `R` 刷新
```

- [ ] **Step 4: overrides.toml 说明更新（约 L371）**

```markdown
| `overrides.toml` | YAML | 自定义规则（旧版自定义规则组字段已废弃，启动时自动清空） |
```

- [ ] **Step 5: FAQ 更新（约 L491-498 合并报错条目）**

"合并报错是什么意思？"条目替换为：

```markdown
**Q：合并报错是什么意思？**
合并器报错（MergeError）现在只有一类：自定义规则的目标不是任何节点/策略组/内置目标
（DIRECT、REJECT、REJECT-DROP、COMPATIBLE、PASS、PASS-RULE、GLOBAL）→ 改目标。
订阅侧的内容（节点/组/规则）原样透传进 config.yaml，不做过滤校验；订阅本身有问题
（重名组、空组、循环引用等）由 `mihomo -t` 预校验拦截，报错会直接弹给你。
```

新增 FAQ 条目：

```markdown
**Q：为什么 url-test/fallback 组不能切换节点？**
自动选择组的出口由 mihomo 的延迟测速/健康检查自动决定，手动固定没有意义（测速后会被
覆盖）。只有 select（手动选择）组支持运行时切换；选择结果通过 `profile: store-selected`
写入缓存，重启后保持。

**Q：规则组页显示"API 不可用"或只有缓存数据？**
规则组列表以 mihomo 运行时状态（GET /proxies）为准；mihomo 未运行或连接失败时降级
展示激活订阅缓存中的组（无当前选择）。启动 mihomo 后按 `R` 刷新即可恢复。
```

- [ ] **Step 6: 自检与提交**

```bash
grep -rn "自定义规则组\|自定义组\|按 m 勾选\|新建组\|编辑组" README.md | head -20
```
逐个检查残留引用是否已更新（README 中"合并器组装顺序与去重规则"相关章节若存在自定义组描述，一并更新或删除）。然后：

```bash
git add -A && git commit -m "docs: README 更新规则组只读+切换+测速+持久化说明"
```

---

## Task 6: 集成验证与收尾

- [ ] **Step 1: 全量构建与测试**

```bash
cargo build 2>&1 | tail -5
cargo test 2>&1 | tail -20
cargo clippy --all-targets 2>&1 | tail -10
```
Expected: build/test/clippy 全绿（clippy 若有 warning 一并修复）。

- [ ] **Step 2: 端到端（真实 mihomo）**

本机 `/etc/mihomo` 存在且服务 active：
1. `curl` 核对 `GET /proxies` 输出与 GroupInfo 解析假设一致（组条目含 name/type/now/all）
2. 若有可用终端，运行 `./target/debug/mihomo-tui` 人工验证：规则组页展示、select 切换、自动组提示、r 测速、R 刷新
3. 确认 `mihomo -t -f <合并产物>` 通过且 config.yaml 含 `profile: store-selected: true`

- [ ] **Step 3: 更新 README 顶部功能清单（如有遗漏）并提交**

```bash
git add -A && git commit -m "chore: 规则组只读化集成验证与收尾"
```

---

## 自审记录

- **Spec 覆盖**：移除编辑能力（Task 1/4）✓；订阅组解析展示（Task 4）✓；select 切换 PUT /proxies（Task 2/3/4）✓；自动组禁选提示（Task 4 is_switchable）✓；store-selected 持久化（Task 1 profile 键）✓；延迟测试（Task 2/3/4）✓；旧数据迁移方案 A（Task 3）✓；规则页保留（Task 1 Step 4）✓；README（Task 5）✓；测试（各 Task）✓。
- **占位符扫描**：无 TODO/TBD；所有代码块完整。
- **类型一致性**：GroupInfo 在 client.rs 定义（Task 2），app.rs（Task 3）与 groups.rs（Task 4）引用；UiCommand/UiEvent 变体签名在 Task 3 定义、Task 4 页面使用；`SelectAction` 在 groups.rs 内定义使用；`KeyEvent::new(code, modifiers)` 按 crossterm 0.28 签名（Task 4 Step 2 已注明）。
- **依赖顺序**：W1‖W2 → W3‖W4‖W5；Task 4 依赖 Task 2（GroupInfo）与 Task 3（UiCommand 变体）——若并行执行 W4，需先合并 W2/W3 的分支或在同一 worktree 内按顺序提交（推荐：同一 worktree 顺序执行 W1→W2→W3‖W4‖W5 或两两串行）。
