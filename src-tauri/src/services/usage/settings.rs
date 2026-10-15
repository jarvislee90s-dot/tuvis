//! 用量设置读写（settings KV，键 `usage.settings`，值是 `UsageSettings` 的 JSON）。
//! 校验在 `merge_patch` 里做（越界一律 `usage-settings-invalid`，不静默夹取）。
use super::error::UsageError;
use super::model::{UsageSettings, UsageSettingsPatch};
use super::provider::parse_provider_rules;
use crate::database::{get_setting, set_setting};

pub const SETTINGS_KEY: &str = "usage.settings";

pub fn parse_settings(raw: Option<&str>) -> UsageSettings {
    raw.and_then(|s| serde_json::from_str::<UsageSettings>(s).ok())
        .unwrap_or_default()
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
                "浮窗分工具条数须在 1–7",
            ));
        }
        next.mini_bar_tool_rows = v;
    }
    if let Some(v) = patch.detail_retention_days {
        if !(1..=3650).contains(&v) {
            return Err(UsageError::new(
                "usage-settings-invalid",
                "明细保留期须在 1–3650 天",
            ));
        }
        next.detail_retention_days = v;
    }
    if let Some(v) = patch.collect_interval_min {
        if !(1..=1440).contains(&v) {
            return Err(UsageError::new(
                "usage-settings-invalid",
                "采集兜底间隔须在 1–1440 分钟",
            ));
        }
        next.collect_interval_min = v;
    }
    if let Some(v) = &patch.provider_map_rules {
        if !v.trim().is_empty() && parse_provider_rules(v).is_empty() {
            return Err(UsageError::new(
                "usage-settings-invalid",
                "供应商映射规则必须是 {\"rules\":[{\"prefix\":…,\"provider\":…}]} 形态的 JSON",
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
                "分享图姿态不得为空串（随机请用 random）",
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
            format!("用量设置未能落库（写后回读不一致）：key={SETTINGS_KEY}"),
        ));
    }
    Ok(())
}
// ==== usage 自省锁：以下为排除区（测试代码），勿删勿复制 ====
// ==== usage 自省锁：排除区结束，勿删勿复制 ====
