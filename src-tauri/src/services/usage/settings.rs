//! 用量设置读写（settings KV，键 `usage.settings`，值是 `UsageSettings` 的 JSON）。
//! 校验在 `merge_patch` 里做（越界一律 `usage-settings-invalid`，不静默夹取）。
use super::error::UsageError;
use super::model::{UsageSettings, UsageSettingsPatch};
use super::provider::parse_provider_rules;
use crate::database::{get_setting, set_setting};

pub const SETTINGS_KEY: &str = "usage.settings";

pub fn parse_settings(raw: Option<&str>) -> UsageSettings {
    let mut settings = raw
        .and_then(|s| serde_json::from_str::<UsageSettings>(s).ok())
        .unwrap_or_default();
    // 旧版本允许 1–3650 天；2026-10-21 用户裁决上限 90 天。存量值在读取时收敛，
    // 避免用户不改设置却继续保留超长明细。
    settings.detail_retention_days = settings.detail_retention_days.clamp(1, 90);
    settings
}

pub fn merge_patch(
    cur: &UsageSettings,
    patch: &UsageSettingsPatch,
) -> Result<UsageSettings, UsageError> {
    let mut next = cur.clone();
    if let Some(v) = patch.enabled {
        next.enabled = v;
    }
    if let Some(v) = patch.mini_bar_range {
        next.mini_bar_range = v;
    }
    if let Some(v) = patch.mini_bar_tool_rows {
        if !(1..=7).contains(&v) {
            return Err(UsageError::new(
                "usage-settings-invalid",
                "floating bar tool count must be 1-7",
            ));
        }
        next.mini_bar_tool_rows = v;
    }
    if let Some(v) = patch.detail_retention_days {
        if !(1..=90).contains(&v) {
            return Err(UsageError::new(
                "usage-settings-invalid",
                "detail retention must be 1-90 days",
            ));
        }
        next.detail_retention_days = v;
    }
    if let Some(v) = patch.collect_interval_min {
        if !(1..=1440).contains(&v) {
            return Err(UsageError::new(
                "usage-settings-invalid",
                "fallback collect interval must be 1-1440 min",
            ));
        }
        next.collect_interval_min = v;
    }
    if let Some(v) = &patch.provider_map_rules {
        if !v.trim().is_empty() && parse_provider_rules(v).is_empty() {
            return Err(UsageError::new(
                "usage-settings-invalid",
                "provider map must be JSON {\"rules\":[{\"prefix\":…,\"provider\":…}]}",
            ));
        }
        next.provider_map_rules = v.clone();
    }
    if let Some(v) = &patch.export_quote {
        next.export_quote = v.clone();
    }
    if let Some(v) = &patch.export_pose {
        if v.trim().is_empty() {
            return Err(UsageError::new(
                "usage-settings-invalid",
                "share pose must be non-empty (use random)",
            ));
        }
        next.export_pose = v.clone();
    }
    Ok(next)
}

/// 单测注入的设置覆盖（**仅 `#[cfg(test)]`**，评审 C5）：让调度层/采集器的单测
/// **完全不碰数据库**——旧版几个用例经 `save()` 写的是开发机真实 `~/.tuvis/tuvis.db`。
/// 用**全局静态量**而不是 `thread_local`：`concurrent_calls_share_one_run` 会在 4 个
/// 子线程里调用 `collect_with` → `load()`，thread_local 传不进去。
#[cfg(test)]
static SETTINGS_OVERRIDE: once_cell::sync::Lazy<std::sync::Mutex<Option<UsageSettings>>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(None));

/// 单测专用：注入一份设置（不落库、不读库）；传 `None` 复位。
#[cfg(test)]
pub fn set_override_for_test(s: Option<UsageSettings>) {
    *SETTINGS_OVERRIDE.lock().unwrap_or_else(|e| e.into_inner()) = s;
}

pub fn load() -> UsageSettings {
    #[cfg(test)]
    if let Some(s) = SETTINGS_OVERRIDE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        return s; // 单测注入优先：本分支在生产构建里不存在
    }
    parse_settings(get_setting(SETTINGS_KEY).as_deref())
}

/// 连接注入版（单测/查询层用）：**不触碰全局 DB**，避免单测污染真实 `~/.tuvis/tuvis.db`
pub fn load_from_conn(conn: &rusqlite::Connection) -> UsageSettings {
    parse_settings(crate::database::dao::settings::get_setting_conn(conn, SETTINGS_KEY).as_deref())
}

/// 写设置（**Task 20 fix round 1 / 裁决 F：返回 `Result` + 写后回读**）。
///
/// 为什么不能只 `set_setting` 了事：DAO 的 `set_setting_conn`（Task 1 的
/// `dao/settings.rs:27-32`）把 `conn.execute` 的错误丢进 `let _ =` → **写失败会被报成成功**，
/// 用户看到新值、重启回旧值（`usage_set_settings` 正是这条静默丢失的用户可见入口）。
/// 本层的最小修复**不改 DAO**（不越界改 Task 1/2 的文件）：写后**回读**同一 key，
/// 与本次写入不一致即以**契约里已有的** `usage-db-failed` 报错（不新造码）。
/// 锁：`usage_ipc_test::set_settings_reports_persist_failure_instead_of_success`
/// （用 `settings` 表上的 RAISE(ABORT) 触发器造真实写失败）。
pub fn save(s: &UsageSettings) -> Result<(), UsageError> {
    let json = serde_json::to_string(s).map_err(|e| {
        UsageError::new("usage-db-failed", format!("usage settings serialize: {e}"))
    })?;
    set_setting(SETTINGS_KEY, &json);
    if get_setting(SETTINGS_KEY).as_deref() != Some(json.as_str()) {
        return Err(UsageError::new(
            "usage-db-failed",
            format!("usage settings write did not persist (readback mismatch): key={SETTINGS_KEY}"),
        ));
    }
    Ok(())
}
// ==== usage 自省锁：以下为排除区（测试代码），勿删勿复制 ====
#[cfg(test)]
mod tests {
    use super::*;

    fn raw(days: i64) -> String {
        format!(
            r#"{{"enabled":true,"miniBarRange":"today","miniBarToolRows":3,"detailRetentionDays":{days},"collectIntervalMin":10,"providerMapRules":"","exportQuote":"","exportPose":"random"}}"#
        )
    }

    #[test]
    fn parse_settings_clamps_legacy_retention_into_1_to_90() {
        assert_eq!(
            parse_settings(Some(&raw(3650))).detail_retention_days,
            90,
            "旧版允许的 3650 天必须在读取时收敛到 90"
        );
        assert_eq!(
            parse_settings(Some(&raw(0))).detail_retention_days,
            1,
            "低于 1 天的脏值必须收敛到 1"
        );
        assert_eq!(parse_settings(Some(&raw(30))).detail_retention_days, 30);
        assert_eq!(
            parse_settings(None).detail_retention_days,
            90,
            "缺省保留期仍为 90 天"
        );
        assert_eq!(
            parse_settings(Some("not-json")).detail_retention_days,
            90,
            "损坏的存量 JSON 回退默认值"
        );
    }

    #[test]
    fn merge_patch_enforces_retention_upper_bound_90() {
        let cur = UsageSettings::default();
        let accepted = merge_patch(
            &cur,
            &UsageSettingsPatch {
                detail_retention_days: Some(90),
                ..UsageSettingsPatch::default()
            },
        )
        .unwrap();
        assert_eq!(accepted.detail_retention_days, 90);

        for rejected in [91, 3650, 0] {
            let error = merge_patch(
                &cur,
                &UsageSettingsPatch {
                    detail_retention_days: Some(rejected),
                    ..UsageSettingsPatch::default()
                },
            )
            .unwrap_err();
            assert_eq!(error.code, "usage-settings-invalid");
            assert_eq!(error.detail, "detail retention must be 1-90 days");
        }
    }
}
// ==== usage 自省锁：排除区结束，勿删勿复制 ====
