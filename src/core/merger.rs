//! 合并器：网络设置 + 用户覆盖 + 激活订阅 → mihomo config.yaml。
//! 语义严格遵循 plans §2 core/merger.rs；测试覆盖 plans §5 全部 14 条。

use serde_yaml::{Mapping, Value};

use crate::core::models::{NetworkSettings, Overrides, Subscription, BUILTIN_TARGETS};

/// 兜底自动组名。
pub const AUTO_GROUP_NAME: &str = "🚀 节点选择";
/// 兜底默认规则模板。
pub const DEFAULT_RULES: [&str; 2] = ["GEOIP,CN,DIRECT", "MATCH,🚀 节点选择"];

pub struct MergeContext<'a> {
    pub settings: &'a NetworkSettings,
    pub overrides: &'a Overrides,
    /// 激活订阅
    pub subscription: Option<&'a Subscription>,
}

#[derive(Debug)]
pub struct MergeOutput {
    pub config: String,
    pub warnings: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct MergeError {
    pub message: String,
}

fn kv(m: &mut Mapping, k: &str, v: impl Into<Value>) {
    m.insert(Value::String(k.to_string()), v.into());
}

fn str_seq<I: IntoIterator<Item = S>, S: Into<String>>(items: I) -> Value {
    Value::Sequence(items.into_iter().map(|s| Value::String(s.into())).collect())
}

/// 注入自动组「🚀 节点选择」（组员=全部节点名）。组名已存在时跳过（保留自定义同名组）。
fn inject_auto_group(
    groups: &mut Vec<Value>,
    group_names: &mut Vec<String>,
    node_names: &[String],
    warnings: &mut Vec<String>,
    reason: &str,
) {
    if group_names.iter().any(|n| n == AUTO_GROUP_NAME) {
        return;
    }
    warnings.push(format!("{reason}，已注入自动组「{AUTO_GROUP_NAME}」"));
    let mut m = Mapping::new();
    kv(&mut m, "name", AUTO_GROUP_NAME);
    kv(&mut m, "type", "select");
    kv(&mut m, "proxies", str_seq(node_names.iter().cloned()));
    groups.push(Value::Mapping(m));
    group_names.push(AUTO_GROUP_NAME.to_string());
}

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
    kv(
        &mut root,
        "external-controller",
        s.external_controller.clone(),
    );
    kv(&mut root, "secret", s.secret.clone());
    let mut tun = Mapping::new();
    kv(&mut tun, "enable", s.tun.enable);
    kv(&mut tun, "stack", s.tun.stack.clone());
    kv(&mut tun, "auto-route", s.tun.auto_route);
    kv(
        &mut tun,
        "dns-hijack",
        str_seq(s.tun.dns_hijack.iter().cloned()),
    );
    kv(&mut tun, "mtu", s.tun.mtu);
    kv(&mut root, "tun", Value::Mapping(tun));
    let mut dns = Mapping::new();
    kv(&mut dns, "enable", s.dns.enable);
    kv(&mut dns, "listen", s.dns.listen.clone());
    kv(&mut dns, "enhanced-mode", s.dns.enhanced_mode.clone());
    kv(&mut dns, "fake-ip-range", s.dns.fake_ip_range.clone());
    kv(
        &mut dns,
        "nameserver",
        str_seq(s.dns.nameserver.iter().cloned()),
    );
    kv(
        &mut dns,
        "default-nameserver",
        str_seq(s.dns.default_nameserver.iter().cloned()),
    );
    kv(
        &mut dns,
        "fallback",
        str_seq(s.dns.fallback.iter().cloned()),
    );
    kv(
        &mut dns,
        "fake-ip-filter",
        str_seq(s.dns.fake_ip_filter.iter().cloned()),
    );
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

    let config = serde_yaml::to_string(&Value::Mapping(root)).map_err(|e| MergeError {
        message: format!("YAML 序列化失败: {e}"),
    })?;
    Ok(MergeOutput { config, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models::{
        Overrides, ProxyNode, RunMode, Subscription, SubscriptionCache, TunSettings, UserRule,
    };
    use serde_yaml::{Mapping, Value};

    fn node(name: &str) -> ProxyNode {
        let mut m = Mapping::new();
        m.insert(Value::String("name".into()), Value::String(name.into()));
        m.insert(Value::String("type".into()), Value::String("ss".into()));
        m.insert(
            Value::String("server".into()),
            Value::String("1.2.3.4".into()),
        );
        m.insert(Value::String("port".into()), Value::Number(8388.into()));
        ProxyNode {
            name: name.into(),
            kind: "ss".into(),
            yaml: Value::Mapping(m),
        }
    }

    fn sub_group(name: &str, members: &[&str]) -> Value {
        let mut m = Mapping::new();
        m.insert(Value::String("name".into()), Value::String(name.into()));
        m.insert(Value::String("type".into()), Value::String("select".into()));
        m.insert(
            Value::String("proxies".into()),
            Value::Sequence(
                members
                    .iter()
                    .map(|s| Value::String(s.to_string()))
                    .collect(),
            ),
        );
        Value::Mapping(m)
    }

    fn sub(nodes: Vec<ProxyNode>, groups: Vec<Value>, rules: Vec<String>) -> Subscription {
        Subscription {
            name: "测试订阅".into(),
            url: "https://example.com/sub".into(),
            last_fetch: None,
            active: true,
            cache: Some(SubscriptionCache {
                proxies: nodes,
                proxy_groups: groups,
                rules,
                fetched_at: "t".into(),
            }),
        }
    }

    fn rule(rule_type: &str, payload: &str, target: &str) -> UserRule {
        UserRule {
            rule_type: rule_type.into(),
            payload: payload.into(),
            target: target.into(),
        }
    }

    fn do_merge(overrides: Overrides, sub: Option<Subscription>) -> MergeOutput {
        merge(MergeContext {
            settings: &NetworkSettings::default(),
            overrides: &overrides,
            subscription: sub.as_ref(),
        })
        .expect("merge 应成功")
    }

    fn do_merge_err(overrides: Overrides, sub: Option<Subscription>) -> MergeError {
        merge(MergeContext {
            settings: &NetworkSettings::default(),
            overrides: &overrides,
            subscription: sub.as_ref(),
        })
        .expect_err("merge 应失败")
    }

    fn parse_out(out: &MergeOutput) -> Value {
        serde_yaml::from_str(&out.config).expect("输出应为合法 yaml")
    }

    fn top_keys(v: &Value) -> Vec<String> {
        v.as_mapping()
            .unwrap()
            .keys()
            .map(|k| k.as_str().unwrap().to_string())
            .collect()
    }

    // ---- 1. 完整合并 ----

    #[test]
    fn full_merge() {
        let mut o = Overrides::default();
        o.rules.push(rule("DOMAIN-SUFFIX", "example.com", "订阅组"));
        let s = sub(
            vec![node("节点1"), node("节点2")],
            vec![
                sub_group("订阅组", &["节点2"]),
                sub_group("订阅组2", &["节点1", "节点2"]),
            ],
            vec!["DOMAIN,test.com,订阅组".into(), "MATCH,订阅组".into()],
        );
        let out = do_merge(o, Some(s));
        assert!(out.warnings.is_empty(), "警告: {:?}", out.warnings);

        let v = parse_out(&out);
        // 顶层键顺序
        let keys = top_keys(&v);
        let want = [
            "port",
            "socks-port",
            "mixed-port",
            "allow-lan",
            "mode",
            "ipv6",
            "log-level",
            "external-controller",
            "secret",
            "tun",
            "dns",
            "profile",
            "proxy-groups",
            "rules",
            "proxies",
        ];
        assert_eq!(keys, want, "顶层键顺序");

        // 网络段字段
        assert_eq!(v["port"], Value::Number(7890.into()));
        assert_eq!(v["mode"], Value::String("rule".into()));
        assert_eq!(v["tun"]["stack"], Value::String("mixed".into()));
        assert_eq!(v["dns"]["enhanced-mode"], Value::String("fake-ip".into()));
        assert_eq!(
            v["dns"]["nameserver"][0],
            Value::String("https://doh.pub/dns-query".into())
        );
        assert_eq!(v["secret"].as_str().unwrap().len(), 32);

        // select 组选择持久化
        assert_eq!(v["profile"]["store-selected"], Value::Bool(true));

        // 组顺序：订阅组原样透传
        let gs = v["proxy-groups"].as_sequence().unwrap();
        assert_eq!(gs[0]["name"], Value::String("订阅组".into()));
        assert_eq!(gs[1]["name"], Value::String("订阅组2".into()));
        assert_eq!(gs[0]["type"], Value::String("select".into()));

        // 规则顺序：自定义规则在前
        let rs = v["rules"].as_sequence().unwrap();
        assert_eq!(
            rs[0],
            Value::String("DOMAIN-SUFFIX,example.com,订阅组".into())
        );
        assert_eq!(rs[1], Value::String("DOMAIN,test.com,订阅组".into()));
        assert_eq!(rs[2], Value::String("MATCH,订阅组".into()));

        // 节点
        let ps = v["proxies"].as_sequence().unwrap();
        assert_eq!(ps.len(), 2);
        assert_eq!(ps[0]["name"], Value::String("节点1".into()));
        assert_eq!(ps[1]["name"], Value::String("节点2".into()));
    }

    // ---- 2. 订阅内 proxies 重名 → 保留首个 ----

    #[test]
    fn duplicate_subscription_proxies_keep_first() {
        let s = sub(
            vec![node("a"), node("a"), node("b")],
            vec![sub_group("g", &["a", "b"])],
            vec![],
        );
        let out = do_merge(Overrides::default(), Some(s));
        let v = parse_out(&out);
        let ps = v["proxies"].as_sequence().unwrap();
        assert_eq!(ps.len(), 2);
        assert_eq!(ps[0]["name"], Value::String("a".into()));
        assert_eq!(ps[1]["name"], Value::String("b".into()));
        assert!(
            out.warnings.iter().any(|w| w.contains("a")),
            "警告应提及重名节点: {:?}",
            out.warnings
        );
    }

    // ---- 4. 订阅组名与节点名冲突 → 原样透传（不再丢弃） ----

    #[test]
    fn sub_group_name_same_as_node_passthrough() {
        let s = sub(
            vec![node("冲突节点")],
            vec![
                sub_group("冲突节点", &[]),
                sub_group("正常组", &["冲突节点"]),
            ],
            vec!["MATCH,正常组".into()],
        );
        let out = do_merge(Overrides::default(), Some(s));
        assert!(out.warnings.is_empty(), "警告: {:?}", out.warnings);
        let v = parse_out(&out);
        let gs = v["proxy-groups"].as_sequence().unwrap();
        assert_eq!(gs.len(), 2, "重名组原样透传: {gs:?}");
        assert_eq!(gs[0]["name"], Value::String("冲突节点".into()));
        assert_eq!(gs[1]["name"], Value::String("正常组".into()));
    }

    // ---- 5. 自定义规则 target 缺失 → MergeError ----

    #[test]
    fn custom_rule_bad_target_is_error() {
        let mut o = Overrides::default();
        o.rules.push(rule("DOMAIN", "x.com", "不存在的目标"));
        let e = do_merge_err(o, None);
        assert!(
            e.message.contains("DOMAIN,x.com,不存在的目标") && e.message.contains("不存在的目标"),
            "错误信息应含规则与缺失目标: {}",
            e.message
        );
    }

    // ---- 6. 自定义组成员缺失 → MergeError（自定义组已废弃：仅保留自定义规则校验） ----

    // ---- 7. 兜底：仅节点无组无规则 → 自动组 + 默认规则 ----

    #[test]
    fn fallback_auto_group_and_default_rules() {
        let s = sub(vec![node("节点1"), node("节点2")], vec![], vec![]);
        let out = do_merge(Overrides::default(), Some(s));
        let v = parse_out(&out);
        let gs = v["proxy-groups"].as_sequence().unwrap();
        assert_eq!(gs.len(), 1);
        assert_eq!(gs[0]["name"], Value::String(AUTO_GROUP_NAME.into()));
        let members = gs[0]["proxies"].as_sequence().unwrap();
        assert_eq!(members.len(), 2);
        assert_eq!(members[0], Value::String("节点1".into()));
        assert_eq!(members[1], Value::String("节点2".into()));
        let rs = v["rules"].as_sequence().unwrap();
        assert_eq!(
            rs.iter()
                .map(|r| r.as_str().unwrap().to_string())
                .collect::<Vec<_>>(),
            DEFAULT_RULES.to_vec()
        );
        assert!(
            out.warnings.len() >= 2,
            "应有自动组+默认规则两条警告: {:?}",
            out.warnings
        );
    }

    // ---- 8. 无激活订阅 → 仅网络 + 自定义规则（无组） ----

    #[test]
    fn no_subscription_no_proxies_no_template() {
        let mut o = Overrides::default();
        o.rules.push(rule("MATCH", "", "DIRECT"));
        let out = do_merge(o, None);
        assert!(out.warnings.is_empty(), "警告: {:?}", out.warnings);
        let v = parse_out(&out);
        let keys = top_keys(&v);
        assert!(
            !keys.contains(&"proxy-groups".to_string()),
            "不应有 proxy-groups 键: {keys:?}"
        );
        assert!(
            !keys.contains(&"proxies".to_string()),
            "不应有 proxies 键: {keys:?}"
        );
        assert!(keys.len() <= 13, "不应注入模板: {keys:?}");
        let rs = v["rules"].as_sequence().unwrap();
        assert_eq!(rs[0], Value::String("MATCH,DIRECT".into()));
    }

    // ---- 10. 订阅组成员不存在 → 原样透传（不再剔除） ----

    #[test]
    fn sub_group_missing_members_passthrough() {
        let s = sub(
            vec![node("节点1")],
            vec![
                sub_group("好组", &["节点1", "幽灵"]),
                sub_group("全坏组", &["幽灵1", "幽灵2"]),
            ],
            vec!["MATCH,好组".into()],
        );
        let out = do_merge(Overrides::default(), Some(s));
        assert!(out.warnings.is_empty(), "警告: {:?}", out.warnings);
        let v = parse_out(&out);
        let gs = v["proxy-groups"].as_sequence().unwrap();
        assert_eq!(gs.len(), 2, "全部组原样透传: {gs:?}");
        assert_eq!(gs[0]["name"], Value::String("好组".into()));
        assert_eq!(gs[1]["name"], Value::String("全坏组".into()));
        let members = gs[0]["proxies"].as_sequence().unwrap();
        assert_eq!(members.len(), 2, "幽灵成员原样保留");
        assert_eq!(members[0], Value::String("节点1".into()));
        assert_eq!(members[1], Value::String("幽灵".into()));
    }

    // ---- 11. 内置 target 合法 ----

    #[test]
    fn builtin_targets_are_valid() {
        let mut o = Overrides::default();
        for t in BUILTIN_TARGETS {
            o.rules.push(rule("MATCH", "", t));
        }
        let s = sub(vec![node("节点1")], vec![], vec![]);
        let out = do_merge(o, Some(s));
        let v = parse_out(&out);
        let rs = v["rules"].as_sequence().unwrap();
        assert_eq!(rs.len(), BUILTIN_TARGETS.len());
        // 兜底默认规则不应注入（已有自定义规则）
        assert_eq!(
            rs[0],
            Value::String(format!("MATCH,{}", BUILTIN_TARGETS[0]))
        );
    }

    // ---- 12. MATCH 规则序列化无 payload ----

    #[test]
    fn match_rule_serialization() {
        let mut o = Overrides::default();
        o.rules.push(UserRule {
            rule_type: "MATCH".into(),
            payload: "".into(),
            target: "节点1".into(),
        });
        let s = sub(vec![node("节点1")], vec![], vec![]);
        let out = do_merge(o, Some(s));
        let v = parse_out(&out);
        let rs = v["rules"].as_sequence().unwrap();
        assert_eq!(rs[0], Value::String("MATCH,节点1".into()));
    }

    // ---- 13. GEOIP 规则 ----

    #[test]
    fn geoip_rule_serialization() {
        let mut o = Overrides::default();
        o.rules.push(rule("GEOIP", "CN", "DIRECT"));
        let s = sub(vec![node("节点1")], vec![], vec![]);
        let out = do_merge(o, Some(s));
        let v = parse_out(&out);
        let rs = v["rules"].as_sequence().unwrap();
        assert_eq!(rs[0], Value::String("GEOIP,CN,DIRECT".into()));
    }

    // ---- 补充：订阅有节点但无规则 → 兜底默认规则需先注入自动组（无悬空引用） ----

    #[test]
    fn default_rules_with_custom_groups_injects_auto_group() {
        let s = sub(vec![node("节点1"), node("节点2")], vec![], vec![]);
        let out = do_merge(Overrides::default(), Some(s));
        let v = parse_out(&out);
        // 默认规则注入且自动组存在 → 无悬空引用
        let rs = v["rules"].as_sequence().unwrap();
        assert_eq!(
            rs.iter()
                .map(|r| r.as_str().unwrap().to_string())
                .collect::<Vec<_>>(),
            DEFAULT_RULES.to_vec()
        );
        let gs = v["proxy-groups"].as_sequence().unwrap();
        let names: Vec<&str> = gs.iter().map(|g| g["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec![AUTO_GROUP_NAME], "自动组: {names:?}");
        // 自动组成员 = 全部节点
        let auto = gs
            .iter()
            .find(|g| g["name"].as_str() == Some(AUTO_GROUP_NAME))
            .unwrap();
        assert_eq!(auto["proxies"].as_sequence().unwrap().len(), 2);
        assert!(
            out.warnings.iter().any(|w| w.contains("已注入自动组")),
            "应警告注入自动组: {:?}",
            out.warnings
        );
    }

    // ---- 补充：订阅规则去重（与自定义规则重复） ----

    #[test]
    fn duplicate_sub_rules_dropped() {
        let mut o = Overrides::default();
        o.rules.push(rule("DOMAIN", "x.com", "节点1"));
        let s = sub(
            vec![node("节点1")],
            vec![],
            vec![
                "DOMAIN,x.com,节点1".into(),
                "DOMAIN,x.com,节点1".into(),
                "DOMAIN,y.com,节点1".into(),
            ],
        );
        let out = do_merge(o, Some(s));
        let v = parse_out(&out);
        let rs = v["rules"].as_sequence().unwrap();
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[0], Value::String("DOMAIN,x.com,节点1".into()));
        assert_eq!(rs[1], Value::String("DOMAIN,y.com,节点1".into()));
    }

    // ---- 补充：订阅缓存缺失视为无内容 ----

    #[test]
    fn subscription_without_cache_is_empty() {
        let s = Subscription {
            name: "无缓存".into(),
            url: "https://x".into(),
            last_fetch: None,
            active: true,
            cache: None,
        };
        let out = do_merge(Overrides::default(), Some(s));
        let v = parse_out(&out);
        let keys = top_keys(&v);
        assert!(!keys.contains(&"proxies".to_string()));
    }

    // ---- 补充：订阅组含内置保留名成员（DIRECT/REJECT）→ 保留且不警告 ----

    #[test]
    fn sub_group_builtin_members_kept() {
        let s = sub(
            vec![node("节点1")],
            vec![sub_group("🔰 选择节点", &["DIRECT", "REJECT", "节点1"])],
            vec!["MATCH,🔰 选择节点".into()],
        );
        let out = do_merge(Overrides::default(), Some(s));
        assert!(out.warnings.is_empty(), "警告: {:?}", out.warnings);
        let v = parse_out(&out);
        let gs = v["proxy-groups"].as_sequence().unwrap();
        assert_eq!(gs.len(), 1);
        assert_eq!(gs[0]["name"], Value::String("🔰 选择节点".into()));
        let members: Vec<String> = gs[0]["proxies"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|m| m.as_str().unwrap().to_string())
            .collect();
        assert_eq!(members, vec!["DIRECT", "REJECT", "节点1"], "成员顺序不变");
    }

    // ---- 补充：订阅组仅含内置名成员 → 组保留（回归保护 kept.is_empty→丢组 路径） ----

    #[test]
    fn sub_group_only_builtin_members_survives() {
        let s = sub(
            vec![node("节点1")], // 有节点但组不引用它（组只含 DIRECT 是合法配置）
            vec![sub_group("仅内置组", &["DIRECT"])],
            vec!["MATCH,仅内置组".into()],
        );
        let out = do_merge(Overrides::default(), Some(s));
        let v = parse_out(&out);
        let gs = v["proxy-groups"].as_sequence().unwrap();
        assert_eq!(gs.len(), 1, "组不应被丢弃: {gs:?}");
        assert_eq!(gs[0]["name"], Value::String("仅内置组".into()));
        let members = gs[0]["proxies"].as_sequence().unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0], Value::String("DIRECT".into()));
    }

    // ---- 订阅组链式引用 ----

    // 1. 订阅组链式引用（A 引用 B）→ 保留无警告
    #[test]
    fn sub_group_chain_reference_kept() {
        let s = sub(
            vec![node("节点1")],
            vec![sub_group("A组", &["B组"]), sub_group("B组", &["节点1"])],
            vec!["MATCH,A组".into()],
        );
        let out = do_merge(Overrides::default(), Some(s));
        assert!(out.warnings.is_empty(), "警告: {:?}", out.warnings);
        let v = parse_out(&out);
        let gs = v["proxy-groups"].as_sequence().unwrap();
        assert_eq!(gs.len(), 2);
        assert_eq!(gs[0]["name"], Value::String("A组".into()));
        assert_eq!(gs[1]["name"], Value::String("B组".into()));
        let members: Vec<String> = gs[0]["proxies"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|m| m.as_str().unwrap().to_string())
            .collect();
        assert_eq!(members, vec!["B组"], "A 组引用 B 组应保留");
    }

    // ---- 补充：订阅规则带 no-resolve 选项（IP-CIDR,payload,组,no-resolve）→
    // 目标解析取第 3 段（payload 之后），规则原样保留 ----------------

    #[test]
    fn sub_rule_with_no_resolve_option_kept() {
        let s = sub(
            vec![node("节点1")],
            vec![sub_group("组A", &["节点1"])],
            vec!["IP-CIDR,0.0.0.0/8,组A,no-resolve".into()],
        );
        let out = do_merge(Overrides::default(), Some(s));
        assert!(out.warnings.is_empty(), "警告: {:?}", out.warnings);
        let v = parse_out(&out);
        let rs = v["rules"].as_sequence().unwrap();
        assert_eq!(
            rs[0],
            Value::String("IP-CIDR,0.0.0.0/8,组A,no-resolve".into()),
            "规则应原样保留（含 no-resolve 选项），不被误当目标"
        );
    }

    // ---- 补充：MATCH 类规则（无 payload）目标在第 2 段 → 保留 ----

    #[test]
    fn sub_rule_match_target_is_second_segment() {
        let s = sub(
            vec![node("节点1")],
            vec![sub_group("组A", &["节点1"])],
            vec!["MATCH,组A".into()],
        );
        let out = do_merge(Overrides::default(), Some(s));
        assert!(out.warnings.is_empty(), "警告: {:?}", out.warnings);
        let v = parse_out(&out);
        let rs = v["rules"].as_sequence().unwrap();
        assert_eq!(rs[0], Value::String("MATCH,组A".into()));
    }

    // ---- 补充：带 no-resolve 的规则目标确实不存在 → 仍丢弃（选项不影响校验） ----

    #[test]
    fn sub_rule_no_resolve_with_missing_target_dropped() {
        let s = sub(
            vec![node("节点1")],
            vec![sub_group("组A", &["节点1"])],
            vec![
                "IP-CIDR,8.8.8.8/32,不存在组,no-resolve".into(),
                "MATCH,组A".into(),
            ],
        );
        let out = do_merge(Overrides::default(), Some(s));
        let v = parse_out(&out);
        let rs = v["rules"].as_sequence().unwrap();
        assert_eq!(rs.len(), 1, "不存在组的规则应被丢弃: {rs:?}");
        assert_eq!(rs[0], Value::String("MATCH,组A".into()));
        assert!(
            out.warnings
                .iter()
                .any(|w| w.contains("IP-CIDR,8.8.8.8/32,不存在组,no-resolve")
                    && w.contains("已丢弃")),
            "应警告丢弃: {:?}",
            out.warnings
        );
    }

    // ---- 补充：格式异常的订阅规则（段数不足）→ 丢弃 + 格式异常 warning ----

    #[test]
    fn malformed_sub_rule_dropped_with_format_warning() {
        let s = sub(
            vec![node("节点1")],
            vec![sub_group("组A", &["节点1"])],
            vec!["FOO,BAR".into(), "MATCH,组A".into()],
        );
        let out = do_merge(Overrides::default(), Some(s));
        let v = parse_out(&out);
        let rs = v["rules"].as_sequence().unwrap();
        assert_eq!(rs.len(), 1, "格式异常的规则应被丢弃: {rs:?}");
        assert_eq!(rs[0], Value::String("MATCH,组A".into()));
        assert!(
            out.warnings
                .iter()
                .any(|w| w.contains("FOO,BAR") && w.contains("格式异常")),
            "应警告格式异常: {:?}",
            out.warnings
        );
    }

    // ---- 补充：settings 非默认值（mode/ipv6/tun.enable）写入 config.yaml ----
    // 仪表盘热切开关双写 settings.toml 后，merge 必须把持久化值带进 config.yaml，
    // 结构性变更（订阅更新/切换 → 重启）后开关状态不丢失。

    #[test]
    fn settings_mode_ipv6_tun_written_to_config() {
        let s = NetworkSettings {
            mode: "global".into(),
            ipv6: true,
            tun: TunSettings {
                enable: true,
                ..Default::default()
            },
            run_mode: RunMode::Systemd,
            ..Default::default()
        };
        let out = merge(MergeContext {
            settings: &s,
            overrides: &Overrides::default(),
            subscription: None,
        })
        .expect("merge 应成功");
        let v = parse_out(&out);
        assert_eq!(v["mode"], Value::String("global".into()));
        assert_eq!(v["ipv6"], Value::Bool(true));
        assert_eq!(v["tun"]["enable"], Value::Bool(true));
    }

    // ---- 补充：订阅组原样透传（重名/空成员/幽灵成员/循环均不干预） ----

    #[test]
    fn subscription_groups_passthrough_verbatim() {
        let s = sub(
            vec![node("节点1"), node("节点2")],
            vec![
                sub_group("重名组", &["节点1"]),
                sub_group("重名组", &["节点2"]), // 重名保留（mihomo -t 兜底）
                sub_group("空组", &[]),          // 空组保留
                sub_group("幽灵组", &["不存在"]), // 失效成员保留
            ],
            vec!["MATCH,幽灵组".into()],
        );
        let out = do_merge(Overrides::default(), Some(s));
        assert!(
            out.warnings.is_empty(),
            "透传不应有警告: {:?}",
            out.warnings
        );
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
        m.insert(
            Value::String("name".into()),
            Value::String("自动选择".into()),
        );
        m.insert(
            Value::String("type".into()),
            Value::String("url-test".into()),
        );
        m.insert(
            Value::String("url".into()),
            Value::String("http://x/generate_204".into()),
        );
        m.insert(Value::String("interval".into()), Value::Number(120.into()));
        m.insert(Value::String("tolerance".into()), Value::Number(50.into()));
        m.insert(
            Value::String("proxies".into()),
            Value::Sequence(vec![Value::String("节点1".into())]),
        );
        let s = sub(
            vec![node("节点1")],
            vec![Value::Mapping(m)],
            vec!["MATCH,自动选择".into()],
        );
        let out = do_merge(Overrides::default(), Some(s));
        let v = parse_out(&out);
        let gs = v["proxy-groups"].as_sequence().unwrap();
        assert_eq!(gs.len(), 1);
        assert_eq!(gs[0]["url"], Value::String("http://x/generate_204".into()));
        assert_eq!(gs[0]["interval"], Value::Number(120.into()));
        assert_eq!(gs[0]["tolerance"], Value::Number(50.into()));
    }
}
