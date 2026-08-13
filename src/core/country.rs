//! 国家/地区代码（ISO 3166-1 alpha-2）→ 中文展示名。
//! 纯函数、无依赖，独立模块便于单测；映射表覆盖常见国家/地区，
//! 未覆盖代码由调用方降级（英文名 → 代码 → 未知）。

/// 常见国家/地区中文名表。覆盖 ~90 个常用项；
/// 特别行政区/地区用「中国香港」「中国台湾」「中国澳门」。
pub fn zh_name(code: &str) -> Option<&'static str> {
    let c = code.to_ascii_uppercase();
    Some(match c.as_str() {
        "US" => "美国",
        "CN" => "中国",
        "HK" => "中国香港",
        "TW" => "中国台湾",
        "MO" => "中国澳门",
        "JP" => "日本",
        "KR" => "韩国",
        "KP" => "朝鲜",
        "SG" => "新加坡",
        "MY" => "马来西亚",
        "TH" => "泰国",
        "VN" => "越南",
        "PH" => "菲律宾",
        "ID" => "印度尼西亚",
        "IN" => "印度",
        "PK" => "巴基斯坦",
        "BD" => "孟加拉国",
        "LK" => "斯里兰卡",
        "NP" => "尼泊尔",
        "MM" => "缅甸",
        "KH" => "柬埔寨",
        "LA" => "老挝",
        "BN" => "文莱",
        "AE" => "阿联酋",
        "SA" => "沙特阿拉伯",
        "IL" => "以色列",
        "TR" => "土耳其",
        "IR" => "伊朗",
        "QA" => "卡塔尔",
        "KW" => "科威特",
        "OM" => "阿曼",
        "BH" => "巴林",
        "JO" => "约旦",
        "LB" => "黎巴嫩",
        "IQ" => "伊拉克",
        "SY" => "叙利亚",
        "YE" => "也门",
        "AF" => "阿富汗",
        "GE" => "格鲁吉亚",
        "AM" => "亚美尼亚",
        "AZ" => "阿塞拜疆",
        "KZ" => "哈萨克斯坦",
        "UZ" => "乌兹别克斯坦",
        "MN" => "蒙古",
        "GB" => "英国",
        "DE" => "德国",
        "FR" => "法国",
        "NL" => "荷兰",
        "BE" => "比利时",
        "CH" => "瑞士",
        "AT" => "奥地利",
        "IT" => "意大利",
        "ES" => "西班牙",
        "PT" => "葡萄牙",
        "SE" => "瑞典",
        "NO" => "挪威",
        "DK" => "丹麦",
        "FI" => "芬兰",
        "IE" => "爱尔兰",
        "PL" => "波兰",
        "CZ" => "捷克",
        "SK" => "斯洛伐克",
        "HU" => "匈牙利",
        "RO" => "罗马尼亚",
        "BG" => "保加利亚",
        "GR" => "希腊",
        "RU" => "俄罗斯",
        "UA" => "乌克兰",
        "BY" => "白俄罗斯",
        "MD" => "摩尔多瓦",
        "EE" => "爱沙尼亚",
        "LV" => "拉脱维亚",
        "LT" => "立陶宛",
        "SI" => "斯洛文尼亚",
        "HR" => "克罗地亚",
        "RS" => "塞尔维亚",
        "CY" => "塞浦路斯",
        "MT" => "马耳他",
        "IS" => "冰岛",
        "LU" => "卢森堡",
        "LI" => "列支敦士登",
        "MC" => "摩纳哥",
        "AD" => "安道尔",
        "AU" => "澳大利亚",
        "NZ" => "新西兰",
        "CA" => "加拿大",
        "MX" => "墨西哥",
        "BR" => "巴西",
        "AR" => "阿根廷",
        "CL" => "智利",
        "PE" => "秘鲁",
        "CO" => "哥伦比亚",
        "VE" => "委内瑞拉",
        "UY" => "乌拉圭",
        "PY" => "巴拉圭",
        "BO" => "玻利维亚",
        "EC" => "厄瓜多尔",
        "CR" => "哥斯达黎加",
        "PA" => "巴拿马",
        "CU" => "古巴",
        "DO" => "多米尼加",
        "PR" => "波多黎各",
        "ZA" => "南非",
        "EG" => "埃及",
        "NG" => "尼日利亚",
        "KE" => "肯尼亚",
        "MA" => "摩洛哥",
        "DZ" => "阿尔及利亚",
        "TN" => "突尼斯",
        "ET" => "埃塞俄比亚",
        "TZ" => "坦桑尼亚",
        "GH" => "加纳",
        "FJ" => "斐济",
        _ => return None,
    })
}

/// 展示名解析：中文（查表）> 英文（trim 非空）> 代码 > None。
/// 调用方（UI）在 None 时显示「未知」。
pub fn country_display(code: Option<&str>, en: Option<&str>) -> Option<String> {
    if let Some(code) = code {
        if let Some(zh) = zh_name(code) {
            return Some(zh.to_string());
        }
    }
    if let Some(en) = en {
        let t = en.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    code.map(str::trim)
        .filter(|c| !c.is_empty())
        .map(|c| c.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zh_name_common_codes() {
        assert_eq!(zh_name("US"), Some("美国"));
        assert_eq!(zh_name("us"), Some("美国")); // 大小写不敏感
        assert_eq!(zh_name("HK"), Some("中国香港"));
        assert_eq!(zh_name("TW"), Some("中国台湾"));
        assert_eq!(zh_name("JP"), Some("日本"));
        assert_eq!(zh_name("GB"), Some("英国"));
    }

    #[test]
    fn zh_name_uncovered_returns_none() {
        assert_eq!(zh_name("ZZ"), None);
        assert_eq!(zh_name(""), None);
        assert_eq!(zh_name("USA"), None); // 3 字母非 alpha-2
    }

    #[test]
    fn display_priority_zh_over_en_over_code() {
        assert_eq!(
            country_display(Some("HK"), Some("Hong Kong")),
            Some("中国香港".to_string())
        );
        // 代码未覆盖：英文名
        assert_eq!(
            country_display(Some("ZZ"), Some("Zzzland")),
            Some("Zzzland".to_string())
        );
        // 只有代码：显示代码本身
        assert_eq!(country_display(Some("ZZ"), None), Some("ZZ".to_string()));
        // 全无：None（UI 显示「未知」）
        assert_eq!(country_display(None, None), None);
    }

    #[test]
    fn display_trims_empty_en() {
        assert_eq!(country_display(None, Some("   ")), None);
        assert_eq!(country_display(None, Some("")), None);
        assert_eq!(
            country_display(None, Some(" United States ")),
            Some("United States".to_string())
        );
    }

    #[test]
    fn display_blank_code_returns_none() {
        // 空白代码不得兜底为 Some(" ")：trim 后为空应返回 None（UI 显示「未知」）
        assert_eq!(country_display(Some("  "), None), None);
    }
}
