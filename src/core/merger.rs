//! 合并器：网络设置 + 用户覆盖 + 激活订阅 → mihomo config.yaml。
//! 语义严格遵循 plans §2 core/merger.rs；测试覆盖 plans §5 全部 14 条。

use serde_yaml::{Mapping, Value};

use crate::core::models::{
    default_group_interval, default_test_url, BUILTIN_TARGETS, NetworkSettings, Overrides,
    Subscription,
};

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

/// Value 的展示串（非字符串元素用 Debug）。
fn val_str(v: &Value) -> String {
    match v.as_str() {
        Some(s) => s.to_string(),
        None => format!("{v:?}"),
    }
}

/// 组装 config.yaml。顶层键顺序：网络段 → proxy-groups → rules → proxies。
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

    // ---------- 2. 自定义组（成员引用校验 → MergeError） ----------
    let mut groups: Vec<Value> = Vec::new();
    let mut group_names: Vec<String> = Vec::new();
    for g in &ctx.overrides.groups {
        if node_names.contains(&g.name) {
            return Err(MergeError {
                message: format!("自定义组「{}」与订阅节点重名，请修改组名", g.name),
            });
        }
        for member in &g.proxies {
            if !node_names.contains(member) {
                return Err(MergeError {
                    message: format!(
                        "自定义组「{}」的成员「{}」不在激活订阅节点中",
                        g.name, member
                    ),
                });
            }
        }
        let mut m = Mapping::new();
        kv(&mut m, "name", g.name.clone());
        kv(&mut m, "type", g.group_type.clone());
        kv(&mut m, "proxies", str_seq(g.proxies.iter().cloned()));
        match g.group_type.as_str() {
            "url-test" => {
                let url = if g.url.is_empty() { default_test_url() } else { g.url.clone() };
                let interval = if g.interval == 0 { default_group_interval() } else { g.interval };
                kv(&mut m, "url", url);
                kv(&mut m, "interval", interval);
            }
            "fallback" => {
                let url = if g.url.is_empty() { default_test_url() } else { g.url.clone() };
                let interval = if g.interval == 0 { default_group_interval() } else { g.interval };
                kv(&mut m, "url", url);
                kv(&mut m, "interval", interval);
                kv(&mut m, "tolerance", g.tolerance);
            }
            _ => {} // select：仅 name/type/proxies
        }
        group_names.push(g.name.clone());
        groups.push(Value::Mapping(m));
    }

    // ---------- 3. 订阅组（去重 + 成员校验，全部记 warning） ----------
    if let Some(cache) = ctx.subscription.and_then(|s| s.cache.as_ref()) {
        for g in &cache.proxy_groups {
            let Some(m) = g.as_mapping() else { continue };
            let Some(name) = m.get(Value::String("name".into())).and_then(|v| v.as_str()) else {
                continue;
            };
            let name = name.to_string();
            if group_names.contains(&name) {
                warnings.push(format!("订阅组「{name}」与自定义组重名，已丢弃订阅组"));
                continue;
            }
            if node_names.contains(&name) {
                warnings.push(format!("订阅组「{name}」与节点重名，已丢弃订阅组"));
                continue;
            }
            let mut m2 = m.clone();
            if let Some(Value::Sequence(members)) =
                m2.get_mut(Value::String("proxies".into()))
            {
                let mut kept: Vec<Value> = Vec::new();
                for mv in members.iter() {
                    match mv.as_str() {
                        Some(s) if node_names.contains(&s.to_string()) => kept.push(mv.clone()),
                        _ => {
                            warnings.push(format!(
                                "订阅组「{name}」的成员「{}」不存在，已丢弃该成员",
                                val_str(mv)
                            ));
                        }
                    }
                }
                if kept.is_empty() {
                    warnings.push(format!("订阅组「{name}」成员为空，已丢弃该组"));
                    continue;
                }
                *members = kept;
            }
            group_names.push(name.clone());
            groups.push(Value::Mapping(m2));
        }
    }

    // ---------- 4. 兜底自动组：有节点但无任何组 ----------
    if !nodes.is_empty() && groups.is_empty() {
        warnings.push("订阅有节点但无任何组，已注入自动组「🚀 节点选择」".into());
        let mut m = Mapping::new();
        kv(&mut m, "name", AUTO_GROUP_NAME);
        kv(&mut m, "type", "select");
        kv(&mut m, "proxies", str_seq(node_names.iter().cloned()));
        groups.push(Value::Mapping(m));
        group_names.push(AUTO_GROUP_NAME.to_string());
    }

    // ---------- 5. 引用校验目标集 ----------
    let mut targets: Vec<String> = node_names.clone();
    targets.extend(group_names.iter().cloned());
    targets.extend(BUILTIN_TARGETS.iter().map(|s| s.to_string()));

    // ---------- 6. 自定义规则（target 校验 → MergeError） ----------
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

    // ---------- 7. 订阅规则（去重 + 目标校验，丢弃记 warning） ----------
    if let Some(cache) = ctx.subscription.and_then(|s| s.cache.as_ref()) {
        for r in &cache.rules {
            if rules.contains(r) {
                warnings.push(format!("订阅规则「{r}」与已有规则重复，已丢弃"));
                continue;
            }
            // 目标 = 最后一段逗号分隔
            let target = r.rsplit(',').next().unwrap_or("");
            if !targets.contains(&target.to_string()) {
                warnings.push(format!(
                    "订阅规则「{r}」的目标「{target}」不存在，已丢弃该规则"
                ));
                continue;
            }
            rules.push(r.clone());
        }
    }

    // ---------- 8. 兜底默认规则：有节点但无任何规则 ----------
    if !nodes.is_empty() && rules.is_empty() {
        warnings.push("订阅无规则，已注入默认规则模板".into());
        rules.extend(DEFAULT_RULES.iter().map(|s| s.to_string()));
    }

    // ---------- 9. 组装（serde_yaml::Mapping 保序） ----------
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models::{
        Overrides, ProxyNode, Subscription, SubscriptionCache, UserGroup, UserRule,
    };
    use serde_yaml::{Mapping, Value};

    fn node(name: &str) -> ProxyNode {
        let mut m = Mapping::new();
        m.insert(Value::String("name".into()), Value::String(name.into()));
        m.insert(Value::String("type".into()), Value::String("ss".into()));
        m.insert(Value::String("server".into()), Value::String("1.2.3.4".into()));
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

    fn group(name: &str, members: &[&str]) -> UserGroup {
        UserGroup {
            name: name.into(),
            group_type: "select".into(),
            url: String::new(),
            interval: 0,
            tolerance: 0,
            proxies: members.iter().map(|s| s.to_string()).collect(),
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
        o.groups.push(group("自定义组", &["节点1"]));
        o.rules.push(rule("DOMAIN-SUFFIX", "example.com", "自定义组"));
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
            "port", "socks-port", "mixed-port", "allow-lan", "mode", "ipv6", "log-level",
            "external-controller", "secret", "tun", "dns", "proxy-groups", "rules", "proxies",
        ];
        assert_eq!(keys, want, "顶层键顺序");

        // 网络段字段
        assert_eq!(v["port"], Value::Number(7890.into()));
        assert_eq!(v["mode"], Value::String("rule".into()));
        assert_eq!(v["tun"]["stack"], Value::String("mixed".into()));
        assert_eq!(v["dns"]["enhanced-mode"], Value::String("fake-ip".into()));
        assert_eq!(v["dns"]["nameserver"][0], Value::String("https://doh.pub/dns-query".into()));
        assert_eq!(v["secret"].as_str().unwrap().len(), 32);

        // 组顺序：自定义组在前
        let gs = v["proxy-groups"].as_sequence().unwrap();
        assert_eq!(gs[0]["name"], Value::String("自定义组".into()));
        assert_eq!(gs[1]["name"], Value::String("订阅组".into()));
        assert_eq!(gs[2]["name"], Value::String("订阅组2".into()));
        // 自定义组序列化字段
        assert_eq!(gs[0]["type"], Value::String("select".into()));

        // 规则顺序：自定义规则在前
        let rs = v["rules"].as_sequence().unwrap();
        assert_eq!(rs[0], Value::String("DOMAIN-SUFFIX,example.com,自定义组".into()));
        assert_eq!(rs[1], Value::String("DOMAIN,test.com,订阅组".into()));
        assert_eq!(rs[2], Value::String("MATCH,订阅组".into()));

        // 节点
        let ps = v["proxies"].as_sequence().unwrap();
        assert_eq!(ps.len(), 2);
        assert_eq!(ps[0]["name"], Value::String("节点1".into()));
        assert_eq!(ps[1]["name"], Value::String("节点2".into()));
    }

    // ---- 2. 自定义组名与订阅组同名 → 保留自定义 ----

    #[test]
    fn custom_group_wins_over_sub_group() {
        let mut o = Overrides::default();
        o.groups.push(group("重名组", &["节点1"]));
        let s = sub(
            vec![node("节点1")],
            vec![sub_group("重名组", &["节点1"])],
            vec![],
        );
        let out = do_merge(o, Some(s));
        let v = parse_out(&out);
        let gs = v["proxy-groups"].as_sequence().unwrap();
        assert_eq!(gs.len(), 1);
        assert_eq!(gs[0]["name"], Value::String("重名组".into()));
        assert!(
            out.warnings.iter().any(|w| w.contains("重名组")),
            "警告应提及重名组: {:?}",
            out.warnings
        );
    }

    // ---- 3. 订阅内 proxies 重名 → 保留首个 ----

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

    // ---- 4. 订阅组名与节点名冲突 → 丢弃订阅组 ----

    #[test]
    fn sub_group_name_conflicts_with_node() {
        let s = sub(
            vec![node("冲突节点")],
            vec![sub_group("冲突节点", &[]), sub_group("正常组", &["冲突节点"])],
            vec![],
        );
        let out = do_merge(Overrides::default(), Some(s));
        let v = parse_out(&out);
        let gs = v["proxy-groups"].as_sequence().unwrap();
        assert_eq!(gs.len(), 1);
        assert_eq!(gs[0]["name"], Value::String("正常组".into()));
        assert!(
            out.warnings.iter().any(|w| w.contains("冲突节点")),
            "警告应提及冲突: {:?}",
            out.warnings
        );
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

    // ---- 6. 自定义组成员缺失 → MergeError ----

    #[test]
    fn custom_group_bad_member_is_error() {
        let mut o = Overrides::default();
        o.groups.push(group("我的组", &["幽灵节点"]));
        let e = do_merge_err(o, None);
        assert!(
            e.message.contains("我的组") && e.message.contains("幽灵节点"),
            "错误信息应含组与缺失成员: {}",
            e.message
        );
    }

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
            rs.iter().map(|r| r.as_str().unwrap().to_string()).collect::<Vec<_>>(),
            DEFAULT_RULES.to_vec()
        );
        assert!(out.warnings.len() >= 2, "应有自动组+默认规则两条警告: {:?}", out.warnings);
    }

    // ---- 8. 无激活订阅 → 仅网络 + 自定义段 ----

    #[test]
    fn no_subscription_no_proxies_no_template() {
        let mut o = Overrides::default();
        o.groups.push(UserGroup {
            name: "空组".into(),
            group_type: "select".into(),
            url: String::new(),
            interval: 0,
            tolerance: 0,
            proxies: vec![],
        });
        o.rules.push(rule("MATCH", "", "DIRECT"));
        let out = do_merge(o, None);
        assert!(out.warnings.is_empty(), "警告: {:?}", out.warnings);
        let v = parse_out(&out);
        let keys = top_keys(&v);
        assert!(keys.contains(&"proxy-groups".to_string()));
        assert!(keys.contains(&"rules".to_string()));
        assert!(!keys.contains(&"proxies".to_string()), "不应有 proxies 键: {keys:?}");
        assert!(keys.len() <= 13, "不应注入模板: {keys:?}");
        let gs = v["proxy-groups"].as_sequence().unwrap();
        assert_eq!(gs[0]["name"], Value::String("空组".into()));
        let rs = v["rules"].as_sequence().unwrap();
        assert_eq!(rs[0], Value::String("MATCH,DIRECT".into()));
    }

    // ---- 9. 订阅规则引用被丢弃的组 → 丢弃该规则 ----

    #[test]
    fn sub_rule_referencing_dropped_group_is_dropped() {
        let s = sub(
            vec![node("节点1")],
            vec![sub_group("坏组", &["幽灵"])], // 成员全缺失 → 组被丢弃
            vec!["DOMAIN,x.com,坏组".into(), "DOMAIN,y.com,节点1".into()],
        );
        let out = do_merge(Overrides::default(), Some(s));
        let v = parse_out(&out);
        let rs = v["rules"].as_sequence().unwrap();
        assert_eq!(rs.len(), 1);
        assert_eq!(rs[0], Value::String("DOMAIN,y.com,节点1".into()));
        assert!(
            out.warnings.iter().any(|w| w.contains("DOMAIN,x.com,坏组")),
            "应警告丢弃的规则: {:?}",
            out.warnings
        );
    }

    // ---- 10. 订阅组成员不存在 → 成员被丢弃 ----

    #[test]
    fn sub_group_missing_members_dropped() {
        let s = sub(
            vec![node("节点1")],
            vec![
                sub_group("好组", &["节点1", "幽灵"]),
                sub_group("全坏组", &["幽灵1", "幽灵2"]),
            ],
            vec![],
        );
        let out = do_merge(Overrides::default(), Some(s));
        let v = parse_out(&out);
        let gs = v["proxy-groups"].as_sequence().unwrap();
        assert_eq!(gs.len(), 1);
        assert_eq!(gs[0]["name"], Value::String("好组".into()));
        let members = gs[0]["proxies"].as_sequence().unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0], Value::String("节点1".into()));
        assert!(
            out.warnings.iter().any(|w| w.contains("幽灵")),
            "应警告丢弃成员: {:?}",
            out.warnings
        );
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
        assert_eq!(rs[0], Value::String(format!("MATCH,{}", BUILTIN_TARGETS[0])));
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

    // ---- 14. 自定义组名与节点名冲突 → MergeError ----

    #[test]
    fn custom_group_name_conflicts_with_node() {
        let mut o = Overrides::default();
        o.groups.push(group("节点1", &["节点1"]));
        let s = sub(vec![node("节点1")], vec![], vec![]);
        let e = do_merge_err(o, Some(s));
        assert!(
            e.message.contains("节点1"),
            "错误信息应含组名: {}",
            e.message
        );
    }

    // ---- 补充：url-test/fallback 组字段 ----

    #[test]
    fn url_test_and_fallback_group_fields() {
        let mut o = Overrides::default();
        o.groups.push(UserGroup {
            name: "测速组".into(),
            group_type: "url-test".into(),
            url: "http://example.com/204".into(),
            interval: 120,
            tolerance: 0,
            proxies: vec!["节点1".into()],
        });
        o.groups.push(UserGroup {
            name: "故障转移".into(),
            group_type: "fallback".into(),
            url: String::new(), // 空 → 用默认
            interval: 0,        // 0 → 用默认
            tolerance: 50,
            proxies: vec!["节点1".into()],
        });
        let s = sub(vec![node("节点1")], vec![], vec![]);
        let out = do_merge(o, Some(s));
        let v = parse_out(&out);
        let gs = v["proxy-groups"].as_sequence().unwrap();
        assert_eq!(gs.len(), 2);
        assert_eq!(gs[0]["url"], Value::String("http://example.com/204".into()));
        assert_eq!(gs[0]["interval"], Value::Number(120.into()));
        assert!(gs[0].get("tolerance").is_none(), "select/url-test 不应有 tolerance");
        assert_eq!(gs[1]["url"], Value::String("http://www.gstatic.com/generate_204".into()));
        assert_eq!(gs[1]["interval"], Value::Number(300.into()));
        assert_eq!(gs[1]["tolerance"], Value::Number(50.into()));
    }

    // ---- 补充：订阅规则去重（与自定义规则重复） ----

    #[test]
    fn duplicate_sub_rules_dropped() {
        let mut o = Overrides::default();
        o.rules.push(rule("DOMAIN", "x.com", "节点1"));
        let s = sub(
            vec![node("节点1")],
            vec![],
            vec!["DOMAIN,x.com,节点1".into(), "DOMAIN,x.com,节点1".into(), "DOMAIN,y.com,节点1".into()],
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
}
