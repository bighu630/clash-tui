# IP-CIDR 规则类型支持 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在规则页面“添加规则”中新增 IP-CIDR 类型支持（本次按用户确认扩至 IP-CIDR、IP-CIDR6、SRC-IP-CIDR、GEOSITE 四类），含 UI 下拉、CIDR 严格校验、no-resolve 自动追加、持久化与合并，保持与 mihomo 语义一致并通过 mihomo -t 回滚兜底。

**Architecture:** UI 层扩展 RULE_TYPES 并在 submit_form 增加 CIDR 校验，rule_to_string/parse_rule 与 merger 序列化时对 IP-CIDR/IP-CIDR6/SRC-IP-CIDR 自动追加 `,no-resolve`（对用户透明），models 注释同步，settings/merger 持久化无需改动但需补测试验证往返与旧数据兼容。

**Tech Stack:** Rust / ratatui / crossterm / serde_yaml / toml / std::net::IpAddr（零新依赖，CIDR 校验手写以避免引入 ipnet）

---

## File Structure

- Modify: `src/ui/rules.rs` — RULE_TYPES、rule_to_string、parse_rule、校验函数、submit_form、handle_popup 重建逻辑
- Modify: `src/core/models.rs:237-244` — UserRule 注释同步新类型
- Modify: `src/core/merger.rs:109-135` — 自定义规则序列化自动追加 no-resolve（若 rule_to_string 已处理则此处复用或同步）
- Test: `src/ui/rules.rs` 内新增 #[cfg(test)] 或 `tests/` — 规则解析/序列化/校验单测
- Test: `src/core/merger.rs` 内现有 tests 补充 IP-CIDR 往返与 no-resolve 用例

---

### Task 1: 扩展 RULE_TYPES 与模型注释

**Files:**
- Modify: `src/ui/rules.rs:24` — RULE_TYPES 6→10
- Modify: `src/core/models.rs:237-238` — UserRule 注释

- [ ] **Step 1: 修改 src/ui/rules.rs RULE_TYPES**

```rust
const RULE_TYPES: [&str; 10] = [
    "DOMAIN",
    "DOMAIN-SUFFIX",
    "DOMAIN-KEYWORD",
    "GEOSITE",
    "GEOIP",
    "IP-CIDR",
    "IP-CIDR6",
    "SRC-IP-CIDR",
    "PROCESS-NAME",
    "MATCH",
];
```

顺序说明：DOMAIN 组 → GEOSITE/GEOIP → IP-CIDR 组 → PROCESS-NAME → MATCH 置底（mihomo 要求 MATCH 最后）。若需 SRC-IP-CIDR6 可后续追加，保持 MATCH 末位。

- [ ] **Step 2: 更新 src/core/models.rs 注释**

```rust
/// 自定义规则。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserRule {
    /// DOMAIN|DOMAIN-SUFFIX|DOMAIN-KEYWORD|GEOSITE|GEOIP|IP-CIDR|IP-CIDR6|SRC-IP-CIDR|PROCESS-NAME|MATCH
    pub rule_type: String,
    /// MATCH 时为空串
    pub payload: String,
    pub target: String,
}
```

- [ ] **Step 3: 运行 cargo test 快速检查编译**

Run: `cargo test --lib 2>&1 | tail -n 20`
Expected: 编译通过（测试可能因未改校验而仍绿）

- [ ] **Step 4: Commit**

```bash
git add src/ui/rules.rs src/core/models.rs
git commit -m "feat(rules): extend RULE_TYPES with GEOSITE/IP-CIDR/IP-CIDR6/SRC-IP-CIDR"
```

---

### Task 2: 新增 CIDR 校验与 no-resolve 辅助函数

**Files:**
- Modify: `src/ui/rules.rs` — 新增函数 `is_cidr_type`, `needs_no_resolve`, `is_valid_cidr`
- Modify: `src/core/merger.rs` — 可选同步 `needs_no_resolve`（或从 rules 复用，若跨 crate 则在 merger 内复制一份）

- [ ] **Step 1: 在 src/ui/rules.rs RULE_TYPES 后新增辅助函数**

```rust
/// 需要自动追加 no-resolve 的类型
fn needs_no_resolve(rule_type: &str) -> bool {
    matches!(rule_type, "IP-CIDR" | "IP-CIDR6" | "SRC-IP-CIDR")
}

/// 是否为 CIDR 类型（需做格式校验）
fn is_cidr_type(rule_type: &str) -> bool {
    matches!(rule_type, "IP-CIDR" | "IP-CIDR6" | "SRC-IP-CIDR")
}

/// 严格 CIDR 校验：支持 1.1.1.1/32 与 ::1/128，校验 IP 合法性与前缀长度
fn is_valid_cidr(rule_type: &str, payload: &str) -> bool {
    let payload = payload.trim();
    let mut parts = payload.split('/');
    let ip_str = match parts.next() { Some(s) => s.trim(), None => return false };
    let prefix_str = match parts.next() { Some(s) => s.trim(), None => return false };
    if parts.next().is_some() { return false; } // 多于一个 '/'
    let ip: std::net::IpAddr = match ip_str.parse() { Ok(v) => v, Err(_) => return false };
    let prefix: u8 = match prefix_str.parse() { Ok(v) => v, Err(_) => return false };
    match rule_type {
        "IP-CIDR" => ip.is_ipv4() && prefix <= 32,
        "IP-CIDR6" => ip.is_ipv6() && prefix <= 128,
        "SRC-IP-CIDR" => {
            // 兼容双栈：按 IP 版本校验前缀
            if ip.is_ipv4() { prefix <= 32 } else { prefix <= 128 }
        }
        _ => false,
    }
}
```

边界：空串/多斜杠/非数字前缀/越界前缀 均为 false；GEOSITE 不走此校验。

- [ ] **Step 2: 在 src/core/merger.rs 顶部（或 merge 函数附近）新增同款 needs_no_resolve**

```rust
fn needs_no_resolve(rule_type: &str) -> bool {
    matches!(rule_type.trim(), "IP-CIDR" | "IP-CIDR6" | "SRC-IP-CIDR")
}
```

避免跨 crate 引用 UI 层函数。

- [ ] **Step 3: 运行 cargo test**

Run: `cargo test is_valid_cidr -- --nocapture 2>&1 | tail -n 30`
Expected: 当前无调用可先 PASS（若已写单测则验证）

- [ ] **Step 4: Commit**

```bash
git add src/ui/rules.rs src/core/merger.rs
git commit -m "feat(rules): add CIDR validation and no-resolve helpers"
```

---

### Task 3: 更新 rule_to_string / parse_rule 处理 no-resolve 往返

**Files:**
- Modify: `src/ui/rules.rs:31-60` — rule_to_string 自动追加，parse_rule 兼容解析带 no-resolve 的串

- [ ] **Step 1: 修改 rule_to_string**

```rust
pub fn rule_to_string(r: &UserRule) -> String {
    if r.rule_type == "MATCH" {
        format!("MATCH,{}", r.target)
    } else if needs_no_resolve(&r.rule_type) {
        format!("{},{},{},no-resolve", r.rule_type, r.payload, r.target)
    } else {
        format!("{},{},{}", r.rule_type, r.payload, r.target)
    }
}
```

- [ ] **Step 2: 修改 parse_rule 兼容 no-resolve**

```rust
pub fn parse_rule(s: &str) -> Option<UserRule> {
    let s = s.trim();
    if s.is_empty() { return None; }
    // 先检测尾部 no-resolve（大小写不敏感），剥离后再 splitn(3)
    let mut raw = s;
    let mut _had_no_resolve = false;
    if raw.to_ascii_lowercase().ends_with(",no-resolve") {
        if let Some(idx) = raw.to_ascii_lowercase().rfind(",no-resolve") {
            raw = raw[..idx].trim_end().to_string().leak(); // 避免 leak 用临时 String：改为 let lower = raw.to_ascii_lowercase(); if lower.ends_with...
        }
    }
    // 更简洁实现：
    // let lower = raw.to_ascii_lowercase();
    // let stripped = if lower.ends_with(",no-resolve") { &raw[..raw.len()-",no-resolve".len()] } else { raw };
    let mut parts = stripped.splitn(3, ',');
    // ... 原有逻辑
}
```

完整实现（避免泄漏）：

```rust
pub fn parse_rule(s: &str) -> Option<UserRule> {
    let s = s.trim();
    if s.is_empty() { return None; }
    let lower = s.to_ascii_lowercase();
    let stripped = if lower.ends_with(",no-resolve") {
        s[..s.len() - ",no-resolve".len()].trim_end()
    } else {
        s
    };
    let mut parts = stripped.splitn(3, ',');
    let rule_type = parts.next()?.trim();
    if rule_type.is_empty() { return None; }
    if rule_type == "MATCH" {
        let target = parts.next()?.trim();
        if target.is_empty() { return None; }
        Some(UserRule { rule_type: rule_type.to_string(), payload: String::new(), target: target.to_string() })
    } else {
        let payload = parts.next()?.trim();
        let target = parts.next()?.trim();
        if payload.is_empty() || target.is_empty() { return None; }
        Some(UserRule { rule_type: rule_type.to_string(), payload: payload.to_string(), target: target.to_string() })
    }
}
```

- [ ] **Step 3: 编写单测（同文件 #[cfg(test)]）**

```rust
#[test]
fn rule_to_string_ip_cidr_appends_no_resolve() {
    let r = UserRule { rule_type: "IP-CIDR".into(), payload: "192.168.0.0/16".into(), target: "DIRECT".into() };
    assert_eq!(rule_to_string(&r), "IP-CIDR,192.168.0.0/16,DIRECT,no-resolve");
}
#[test]
fn parse_rule_strips_no_resolve() {
    let r = parse_rule("IP-CIDR,192.168.0.0/16,DIRECT,no-resolve").unwrap();
    assert_eq!(r.rule_type, "IP-CIDR");
    assert_eq!(r.payload, "192.168.0.0/16");
    assert_eq!(r.target, "DIRECT");
}
#[test]
fn parse_rule_no_resolve_case_insensitive() {
    let r = parse_rule("IP-CIDR6,2001:db8::/32,DIRECT,NO-RESOLVE").unwrap();
    assert_eq!(r.rule_type, "IP-CIDR6");
}
```

- [ ] **Step 4: Run**

Run: `cargo test rule_to_string -- --nocapture`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/ui/rules.rs
git commit -m "feat(rules): rule string handles no-resolve roundtrip"
```

---

### Task 4: 表单提交增加 CIDR 校验与 merger 序列化追加 no-resolve

**Files:**
- Modify: `src/ui/rules.rs:225-260` — submit_form 增加校验
- Modify: `src/core/merger.rs:109-135` — 自定义规则序列化追加 no-resolve

- [ ] **Step 1: 在 submit_form 的非空校验后插入 CIDR 校验**

```rust
if is_cidr_type(&rule_type) && !is_valid_cidr(&rule_type, &payload) {
    self.popup = Some(RulePopup::Message(MessagePopup::new(
        "输入有误".to_string(),
        vec![format!("{rule_type} 的 CIDR 格式错误: {payload} (示例 192.168.0.0/16 或 2001:db8::/32)")],
    )));
    return None;
}
// 可选：GEOSITE 非空校验已由 payload.is_empty 覆盖，无需额外
```

需确保校验在 target 非空校验之前或之后均可，但需在构建 UserRule 之前。

- [ ] **Step 2: 同步 merger.rs 自定义规则序列化**

```rust
let s = if rule_type_trim == "MATCH" {
    format!("MATCH,{target_trim}")
} else if needs_no_resolve(rule_type_trim) {
    format!("{rule_type_trim},{payload_trim},{target_trim},no-resolve")
} else {
    format!("{rule_type_trim},{payload_trim},{target_trim}")
};
```

- [ ] **Step 3: 运行 cargo test + clippy**

Run: `cargo test 2>&1 | tail -n 30`
Run: `cargo clippy -- -D warnings 2>&1 | tail -n 30`
Expected: 全绿，无警告

- [ ] **Step 4: Commit**

```bash
git add src/ui/rules.rs src/core/merger.rs
git commit -m "feat(rules): validate CIDR in form and auto-append no-resolve in merger"
```

---

### Task 5: 补测试与旧数据兼容验证

**Files:**
- Modify: `src/ui/rules.rs` — 补充 is_valid_cidr 边界单测
- Modify: `src/core/merger.rs` — 补充合并排序与 no-resolve 测试
- Modify: `src/core/models.rs` — 可选补充反序列化旧 overrides 的兼容测试

- [ ] **Step 1: 追加 is_valid_cidr 单测**

```rust
#[test]
fn cidr_validation() {
    assert!(is_valid_cidr("IP-CIDR", "192.168.0.0/16"));
    assert!(is_valid_cidr("IP-CIDR", "1.1.1.1/32"));
    assert!(!is_valid_cidr("IP-CIDR", "2001:db8::/32")); // IPv6 不允许
    assert!(is_valid_cidr("IP-CIDR6", "2001:db8::/32"));
    assert!(is_valid_cidr("IP-CIDR6", "::1/128"));
    assert!(!is_valid_cidr("IP-CIDR6", "192.168.0.0/16"));
    assert!(is_valid_cidr("SRC-IP-CIDR", "10.0.0.0/8"));
    assert!(is_valid_cidr("SRC-IP-CIDR", "2001:db8::/32"));
    assert!(!is_valid_cidr("IP-CIDR", "192.168.0.0/33"));
    assert!(!is_valid_cidr("IP-CIDR", "999.0.0.0/16"));
    assert!(!is_valid_cidr("IP-CIDR", "192.168.0.0"));
    assert!(!is_valid_cidr("IP-CIDR", "192.168.0.0/"));
    assert!(!is_valid_cidr("IP-CIDR", "/16"));
    assert!(is_valid_cidr("IP-CIDR", " 192.168.0.0/16 ")); // trim
}
```

- [ ] **Step 2: 合并器测试**

```rust
#[test]
fn merge_custom_ip_cidr_has_no_resolve() {
    // 构造 ctx 含一条 IP-CIDR 自定义规则，验证 merge 输出 rules 含 ",no-resolve"
}
#[test]
fn merge_ip_cidr6_and_src_ip_cidr() {
    // 同上，覆盖三种 CIDR 类型
}
#[test]
fn merge_old_overrides_still_loads() {
    // 反序列化不含新类型字段的旧 overrides.toml，确认 Default 成功
}
```

- [ ] **Step 3: 运行全量验证**

Run: `cargo test 2>&1 | tail -n 20`
Run: `cargo clippy -- -D warnings 2>&1 | tail -n 20`
Expected: 全绿

- [ ] **Step 4: 手动 E2E 验证（可选在 worker 内模拟）**

- 构造 Overrides { rules: [IP-CIDR 192.168.0.0/16 DIRECT, GEOSITE google DIRECT] }
- save_overrides → load → merge → 检查 config.yaml 的 rules 数组
- 确认 IP-CIDR 条目为 `IP-CIDR,192.168.0.0/16,DIRECT,no-resolve` 且 mihomo -t 语义正确（若环境无 mihomo 二进制则仅校验字符串）

- [ ] **Step 5: Commit**

```bash
git add src/ui/rules.rs src/core/merger.rs src/core/models.rs
git commit -m "test(rules): add CIDR and no-resolve coverage, verify backward compat"
```

---

## Self-Review Checklist

- [x] 覆盖 IP-CIDR / IP-CIDR6 / SRC-IP-CIDR / GEOSITE 四类型（用户确认范围）
- [x] no-resolve 自动追加（用户确认策略）且往返解析兼容大小写
- [x] CIDR 严格校验含 1.1.1.1/32、::1/128，前缀越界与家族错配均拦截
- [x] 校验失败通过 MessagePopup 阻止保存
- [x] models 注释同步，旧 overrides 兼容（UserRule 结构未变，仅注释）
- [x] merger 自定义规则序列化与去重逻辑保持兼容（订阅侧已支持 no-resolve）
- [x] 无新依赖（std::net 手写），避免 Cargo.toml 变更风险
- [x] 测试覆盖序列化、解析、校验、合并四维度

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-09-01-ip-cidr-support.md`. Two execution options:

1. Subagent-Driven (recommended) - I dispatch a fresh subagent per task, review between tasks, fast iteration
2. Inline Execution - Execute tasks in this session using executing-plans, batch execution with checkpoints

Which approach?
