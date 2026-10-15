//! 用量域结构化错误（对齐 services/pet/error.rs 的 PetRpcError 模式）。
//! code 是稳定错误码，前端 i18n 键 = `usage.rpc.<code>`；detail 经 `{{err}}` 插值展示。
//! 固定 detail 文案使用英文机器诊断；路径/表名/底层错误等动态定位参数保留。
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageError {
    pub code: String,
    pub detail: String,
}

impl UsageError {
    pub fn new(code: &str, detail: impl Into<String>) -> Self {
        // **码表是硬约束**（Task 20 fix round 1 / 评审 Minor 3）：本函数是**生产码的唯一入口**，
        // 而一致性断言只比对「USAGE_CODES ↔ 前端 `KNOWN_USAGE_CODES` ↔ zh/en locale 模板」——
        // 新增一个**没进表**的生产码时那三张表全绿，前端 `usageErrMsg` 却会把它静默收敛成
        // `usage-internal`（用户只看到通用错误，正是本任务点名的那一类静默失效）。
        // 故在 debug/dev 构建里直接硬失败；release 不 panic（不把开发期笔误升级成线上崩溃），
        // 同一份表的生产可见性 + 自省用例仍守着它。
        debug_assert!(
            USAGE_CODES.contains(&code),
            "错误码 `{code}` 未登记进 USAGE_CODES（权威表；登记处：① Rust USAGE_CODES / \
             ② 前端 KNOWN_USAGE_CODES / ③ zh+en locale 的 usage.rpc.* 模板）"
        );
        Self {
            code: code.into(),
            detail: detail.into(),
        }
    }
    /// 未映射的底层 IO/SQLite 错误统一收敛
    pub fn internal(detail: String) -> Self {
        Self::new("usage-internal", detail)
    }
}

/// 全量错误码单一清单——**权威表**。同一张码表的**登记处**（新增码必须逐处同步）：
/// ① 本表 `USAGE_CODES`；② 前端 `src/components/usage/usageErrors.ts` 的 `KNOWN_USAGE_CODES`；
/// ③ `zh.json` + `en.json` 的 `usage.rpc.*` 模板。原先还有第 4 处 = `src-tauri/tests/usage_ipc_test.rs`
/// 里另存的镜像副本，**已删除**（该集成测试改为直接 `use` 本表）。下面的测试负责锁住各处不漂移。
///
/// **不是 `#[cfg(test)]`**（Task 20 fix round 1 / 评审 Minor 3）：它必须被 `UsageError::new`
/// 在生产代码里引用，否则「生产码字面量」这一侧根本没有锁——`#[cfg(test)]` 下新增一个
/// 没进表的生产码，各断言全绿（评审实测）。`pub const` ⇒ 集成构建（如 `usage_ipc_test.rs`）
/// **直接可用**，这是删除那份镜像的前提（P2-1）。
pub const USAGE_CODES: &[&str] = &[
    "usage-db-failed",       // 账本库读写失败
    "usage-source-io",       // 源文件读取失败（该源本轮判失败，其余源继续）
    "usage-source-db-open",  // 源 SQLite 打不开（含 immutable 回退失败）
    "usage-range-invalid",   // 时间范围非法（custom 缺 from/to 或 from > to）
    "usage-groupby-invalid", // 分组维度非法（usage_records 只接受 tool | project，见 D6/D7）
    // 筛选条件在**当前档位算不出**（契约 §2 `UsageFilters` 通用条款 / §3 要点 4，第 9 个码）：
    // 首个实例 = 日档 + `subagentMode="parentsOnly"`（日聚合行不带 session_id）。
    // **不得**当作"没有子代理"或"全放行"——那是界面谎言（GC 7 / 说明书 §8.2）。
    "usage-filter-unavailable",
    "usage-settings-invalid", // 设置补丁越界（负数 / 保留期 <1 / 间隔 <1）
    // **`usage-disabled` 是预留码：当前没有任何发射点**（2026-10-06 第三轮裁决，**刻意不改实现**）。
    // 总开关关闭时后端返回的是**空结果**（`query.rs::empty_dashboard` / `empty_records`）而不是错误
    // ⇒ 这个码只存在于**三处登记**里（本表 / 前端 `KNOWN_USAGE_CODES` / zh+en 的 `usage.rpc.*`），
    // 真机上**不可达**。源码级证据 =
    // `commands::usage::usage_disabled_stays_reserved_with_no_production_emitter`。
    //
    // 为什么不去「真发出来」：那会把**冻结契约**（§3）里「总开关关闭」的语义从「空结果」改成
    // 「错误」，并打红既有用例 `master_switch_off_yields_empty_state_with_zero_collected_at`；
    // 且该码当前**没有消费方**。
    // **将来若要启用，需同步四处**：① 契约 §3 的返回语义；② `master_switch_off_*` 系列用例；
    // ③ 前端 `usageErrMsg` 的展示路径（白名单与 locale 键都已就位 ⇒ **不需**新增第 4 处登记）；
    // ④ 上面那条源码级锁（它会把新增的发射点判红，届时连同它的文档一起更新）。
    // ⚠️ 本条注释是**整行** `//` 注释（判据面会剥掉它）⇒ 不动自省锁的面尺寸；**不要**把它改成
    // 下面那行的行尾注释（那样会进面）。
    "usage-disabled", // 用量统计总开关关闭（查询返回空态而不是报错）
    "usage-internal", // 兜底
];

// ==== usage 自省锁：以下为排除区（测试代码），勿删勿复制 ====
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_shape_camel_case() {
        let e = UsageError::new("usage-range-invalid", "custom range missing from");
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"code\":\"usage-range-invalid\""));
        assert!(json.contains("\"detail\""));
        assert!(!json.contains("code_"));
    }

    #[test]
    fn internal_converges_unmapped() {
        let e = UsageError::internal("disk on fire".into());
        assert_eq!(e.code, "usage-internal");
        assert_eq!(e.detail, "disk on fire");
    }

    /// Minor 3 的**承重证明**：生产码没进表 → `UsageError::new` 必须**硬失败**（debug 构建）。
    /// 去掉 `debug_assert` 或把表还原成 `#[cfg(test)]` → 本用例真红（不再 panic）。
    #[test]
    #[should_panic(expected = "未登记进 USAGE_CODES")]
    fn new_rejects_code_missing_from_the_table() {
        let _ = UsageError::new("usage-not-registered", "unregistered production code");
    }

    /// 契约 §2 逐字：sourceId 的序列化形态是小写工具 id（**不是 camelCase**）
    #[test]
    fn source_id_serializes_as_lowercase_tool_ids() {
        // 任务书勘误（执行者补，已上报）：`use super::*` 拿不到 mod.rs 里
        // `pub use model::*;` 的 glob 再导出（glob 不传递 glob，实测 8×E0433），
        // 故与本模块内 `enum_serde_forms_match_contract` 同款，显式补一条导入。
        use crate::services::usage::model::*;
        for (v, s) in [
            (UsageSourceId::Claude, "\"claude\""),
            (UsageSourceId::Codex, "\"codex\""),
            (UsageSourceId::Kimi, "\"kimi\""),
            (UsageSourceId::OpenCode, "\"opencode\""),
            (UsageSourceId::WorkBuddy, "\"workbuddy\""),
            (UsageSourceId::ZCode, "\"zcode\""),
            (UsageSourceId::Dsh, "\"dsh\""),
        ] {
            assert_eq!(serde_json::to_string(&v).unwrap(), s);
            assert_eq!(v.db_id(), s.trim_matches('"'));
        }
        // 与既有 TOOL_IDS 同一套字符串（不含 openclaw：用量只做 7 源）
        for s in UsageSourceId::ALL {
            assert!(
                crate::adapter::TOOL_IDS.contains(&s.db_id()),
                "{} 必须在 TOOL_IDS 里",
                s.db_id()
            );
        }
    }

    /// 其余枚举的 serde 形态逐字对齐契约（防有人"顺手"改成 camelCase）
    #[test]
    fn enum_serde_forms_match_contract() {
        use crate::services::usage::model::*;
        assert_eq!(
            serde_json::to_string(&UsageRangePreset::Last5h).unwrap(),
            "\"last5h\""
        );
        assert_eq!(
            serde_json::to_string(&UsageRangePreset::Last30d).unwrap(),
            "\"last30d\""
        );
        assert_eq!(
            serde_json::to_string(&UsageGroupBy::Provider).unwrap(),
            "\"provider\""
        );
        assert_eq!(
            serde_json::to_string(&SubagentMode::ParentsOnly).unwrap(),
            "\"parentsOnly\""
        );
        assert_eq!(
            serde_json::to_string(&MiniBarRange::Last7d).unwrap(),
            "\"last7d\""
        );
        assert_eq!(
            serde_json::to_string(&SourceKind::Measured).unwrap(),
            "\"measured\""
        );
        assert_eq!(
            serde_json::to_string(&UsageMetric::ErrorModel).unwrap(),
            "\"errorModel\""
        );
        assert_eq!(
            serde_json::to_string(&UsageMetric::LongestTurn).unwrap(),
            "\"longestTurn\""
        );
        // 契约 §2 新列出的四个（本计划不出条目，但枚举值必须能序列化成契约写的形态）
        assert_eq!(
            serde_json::to_string(&UsageMetric::Sessions).unwrap(),
            "\"sessions\""
        );
        assert_eq!(
            serde_json::to_string(&UsageMetric::ToolAvgMs).unwrap(),
            "\"toolAvgMs\""
        );
        assert_eq!(
            serde_json::to_string(&UsageMetric::TopTool).unwrap(),
            "\"topTool\""
        );
        assert_eq!(
            serde_json::to_string(&UsageMetric::TopToolMs).unwrap(),
            "\"topToolMs\""
        );
    }

    /// 码表闭环（locale 侧那一处的机械锁）：每个码都必须在 zh.json 与 en.json 有 usage.rpc.<code>。
    /// 文件读不到时 panic 带明确信息（不静默跳过）——与 pet 的 rpc_codes_have_i18n_keys 同款。
    #[test]
    fn usage_codes_have_i18n_keys() {
        for locale in ["zh.json", "en.json"] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../src/i18n/locales")
                .join(locale);
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("无法读取 {}: {}", path.display(), e));
            let root: serde_json::Value = serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("{} 不是合法 JSON: {}", locale, e));
            let rpc = root
                .get("usage")
                .and_then(|u| u.get("rpc"))
                .and_then(|r| r.as_object())
                .unwrap_or_else(|| panic!("{} 缺少 usage.rpc 对象", locale));
            for code in USAGE_CODES {
                assert!(
                    rpc.contains_key(*code),
                    "错误码 `{}` 在 {} 的 usage.rpc.* 中没有对应 i18n 键",
                    code,
                    locale
                );
            }
        }
    }

    /// W5：行标签用的 i18n 键（`UNKNOWN_PROVIDER_LABEL_KEY`）也必须在 zh/en **同父同键**——
    /// 供应商不可得时不得回退成硬编码中文（Task 18 的 `group_of` 用它，Task 19 的 CSV 不落键）。
    #[test]
    fn unknown_provider_label_key_has_i18n_keys() {
        let key = crate::services::usage::UNKNOWN_PROVIDER_LABEL_KEY;
        assert_eq!(key, "usage.label.unknownProvider");
        for locale in ["zh.json", "en.json"] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../src/i18n/locales")
                .join(locale);
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("无法读取 {}: {}", path.display(), e));
            let root: serde_json::Value = serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("{} 不是合法 JSON: {}", locale, e));
            let v = root
                .get("usage")
                .and_then(|u| u.get("label"))
                .and_then(|l| l.get("unknownProvider"))
                .and_then(|s| s.as_str())
                .unwrap_or_else(|| panic!("{} 缺少 usage.label.unknownProvider", locale));
            assert!(
                !v.trim().is_empty(),
                "{} 的 usage.label.unknownProvider 不得为空",
                locale
            );
        }
    }
}
// ==== usage 自省锁：排除区结束，勿删勿复制 ====
