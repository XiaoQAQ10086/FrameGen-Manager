//! 错误码归类（P1-15 折中版）。
//!
//! **现状不变**：core 的报错仍然是中文串（中文界面直接显示它，用户报障时也是那句话）。
//! 这里给每一类错误一个**稳定编号**：sidecar 把它拼进错误响应（`{ok:false,error,code}`），
//! 界面在英文下按编号出英文 —— 编号是稳定的翻译键，中文原句多一个标点都不会让它失效。
//!
//! 为什么归类规则放 core：以前 sidecar 自己维护一份 `err_code`，界面词表里又是一摊中文原文，
//! 两边各写一份、迟早对不上。现在**唯一来源**是这里的 [`RULES`]；
//! `dev-only/en-dict.json` 的 `core` 段按编号给英文，`dev-only/i18n-coverage.py` 会核对
//! 「这里出现的每个编号都有英文」，缺一个就算预检失败。
//!
//! 归类是**关键词命中**（不解析语法）：宁可粗一点，也不能把错误码归错 ——
//! 界面只在「这句中文没有译文」时才拿编号兜底，所以归错顶多是英文不够具体，不会误导排查。
//! 顺序有意义：先命中的规则胜出（驱动 > 占用 > 权限 > 资产 …），新增规则请往后加。

/// 一条归类规则：文案里出现 `any` 里任意一个关键词，就归到 `code`。
pub struct Rule {
    /// 稳定编号，格式 `ABC-123`（界面词表按它取英文）
    pub code: &'static str,
    /// 关键词（一律小写 —— 比较前会把文案也转小写，中英文都适用）
    pub any: &'static [&'static str],
}

/// 归类规则表（**唯一来源**，顺序即优先级）。
pub const RULES: &[Rule] = &[
    // 原有 9 条：顺序与 sidecar 当年那份一致，行为不变
    Rule {
        code: "DRV-001",
        any: &["驱动", "580"],
    },
    Rule {
        code: "FILE-002",
        any: &["正被占用", "被占用"],
    },
    Rule {
        code: "ACL-003",
        any: &["没有权限", "只读", "拒绝访问"],
    },
    Rule {
        code: "AST-004",
        // 不能再收「缺少」：那会把「缺少参数 version」这类请求错误也吞成「内置资产缺失」，
        // 用户看到的 AST-004 完全是误报（1.6.7 实测）。资产类文案本身都带「资产」。
        any: &["内置组件不存在", "资产"],
    },
    Rule {
        code: "REC-005",
        any: &["没有找到本工具", "部署记录读不出来", "记录读不出来"],
    },
    Rule {
        code: "RBK-006",
        any: &["回滚"],
    },
    Rule {
        code: "NET-007",
        any: &["下载", "网络", "连接失败"],
    },
    Rule {
        code: "RSH-008",
        any: &["安装器", "reshade"],
    },
    Rule {
        code: "IPC-009",
        any: &["unknown command"],
    },
    Rule {
        // 前后端 IPC 参数不匹配（界面漏传 / 传错类型）：不是资产问题，单独给一个码
        code: "REQ-017",
        any: &["缺少参数", "参数 version", "必须是字符串"],
    },
    // 以下为此前一律落到 GEN-999 的类别：补上编号，英文界面才有具体说法
    Rule {
        code: "SM-010",
        any: &["smooth motion", "解锁组件"],
    },
    Rule {
        code: "D5-011",
        any: &["dlss5", "vulkan", "addon"],
    },
    Rule {
        code: "BAK-012",
        any: &["备份"],
    },
    Rule {
        code: "SCAN-013",
        any: &["扫描", "游戏库"],
    },
    Rule {
        code: "CFG-014",
        any: &["配置"],
    },
    Rule {
        code: "IO-015",
        any: &["写入", "复制", "创建目录", "删除", "读取文件失败"],
    },
    Rule {
        // sidecar 的安全闸门：用户把系统目录（C:\Windows…）当成游戏目录塞进来
        code: "SAFE-016",
        any: &["拒绝在系统目录"],
    },
];

/// 一条都没命中时的编号。
pub const FALLBACK: &str = "GEN-999";

/// 给一句报错归类。命中先出现的规则即返回，都不中返回 [`FALLBACK`]。
pub fn classify(msg: &str) -> &'static str {
    let low = msg.to_lowercase();
    for r in RULES {
        for kw in r.any {
            if low.contains(kw) {
                return r.code;
            }
        }
    }
    FALLBACK
}

/// 全部编号（含兜底），给「每个编号都必须有英文」的门禁用。
pub fn all_codes() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = RULES.iter().map(|r| r.code).collect();
    v.push(FALLBACK);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 编号格式必须是 `ABC-123`（界面词表、`main.js` 的 `[码]` 后缀、截图报障都按这个认）。
    #[test]
    fn codes_are_well_formed_and_unique() {
        let mut seen: Vec<&str> = Vec::new();
        for code in all_codes() {
            let bytes = code.as_bytes();
            let dash = bytes.iter().position(|b| *b == b'-').expect("缺少连字符");
            assert!(
                (2..=5).contains(&dash),
                "前缀应是 2~5 个字符（大写字母开头、可含数字）：{code}"
            );
            // 前缀约定：大写字母开头，后面可以带数字（D5-011 这种）
            assert!(
                bytes[0].is_ascii_uppercase(),
                "前缀不是大写字母开头：{code}"
            );
            assert!(
                bytes[..dash]
                    .iter()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()),
                "前缀含非法字符：{code}"
            );
            let tail = &code[dash + 1..];
            assert_eq!(tail.len(), 3, "编号尾部应是 3 位数字：{code}");
            assert!(
                tail.bytes().all(|b| b.is_ascii_digit()),
                "编号尾部不是数字：{code}"
            );
            assert!(!seen.contains(&code), "编号重复：{code}");
            seen.push(code);
        }
    }

    /// 每个关键词都真的能归到自己那条规则上（防止被前面的规则吞掉 —— 那样编号就白设了）。
    #[test]
    fn every_keyword_maps_to_its_own_code() {
        for r in RULES {
            for kw in r.any {
                assert_eq!(
                    classify(kw),
                    r.code,
                    "关键词 {kw:?} 被前面的规则抢走了（期望 {}）",
                    r.code
                );
            }
        }
    }

    /// 真实文案的归类：几条最常出现在用户截图里的。
    #[test]
    fn real_messages_classify() {
        assert_eq!(
            classify("version.dll 正被占用，请先完全退出游戏。"),
            "FILE-002"
        );
        assert_eq!(classify("没有权限写入 D:\\Game"), "ACL-003");
        assert_eq!(
            classify("内置资产校验不通过（6 项）：\n· version.dll（缺失）"),
            "AST-004"
        );
        assert_eq!(
            classify("没有找到本工具对该目录的部署记录（D:\\Game）"),
            "REC-005"
        );
        assert_eq!(classify("unknown command: deploy_state"), "IPC-009");
        assert_eq!(classify("缺少参数 version"), "REQ-017");
        assert_eq!(
            classify("备份 version.dll 自身已损坏，中止（目录一个字都没动）"),
            "BAK-012"
        );
        assert_eq!(classify("今天天气不错"), FALLBACK);
    }
}
