//! TPS 短时抽样 IPC（Spec §6 七命令逐字；2026-10-24 计划 Task 7）。
//!
//! ## 命令与契约
//!
//! | IPC 命令 | 入参（camelCase 上行） | 返回 |
//! |---|---|---|
//! | `tps_snapshot` | `selection` | [`TpsSnapshotResult`] |
//! | `tps_points` | `selection` + `viewRange`(2m\|10m\|1h\|today) | [`TpsPointsResult`] |
//! | `tps_refresh` | `selection` | [`TpsRefreshResult`]（manual 读取 + manual 点） |
//! | `tps_speed_dashboard` | `query`（V2，2026-10-26 计划 Task 6） | [`v2::model::SpeedDashboardSnapshot`] |
//! | `tps_view_heartbeat` | `viewId` + `visible` | `mode`（租约续/交后调度模式） |
//! | `tps_install_preview` | `tool` | `InstallPreview`（敏感值已脱敏/指纹） |
//! | `tps_install_apply` | `previewId` + `choice` | [`TpsInstallReceiptDto`]（**凭证剥离**） |
//! | `tps_uninstall` | `tool` | `null` |
//!
//! Rust snake_case ↔ IPC camelCase 由 serde `rename_all` 镜像（模型层已带）。
//!
//! ## 错误语义（控制器裁决 R9 的落地选择：**机器可读 `reason` 字段**）
//!
//! 安装族错误（冲突 / 预览漂移 / 预览过期 / 预览不存在 / 卸载冲突）在库内共用
//! `usage-settings-invalid` 一个码（detail 前缀定位）——**不新登错误码**：
//! `USAGE_CODES` 表在 `services/usage/error.rs`（本任务只读），新码还牵动前端
//! `KNOWN_USAGE_CODES` 与 zh/en locale 三处同步。改为：命令错误信封
//! [`TpsCommandError`] 增加 `reason` 字段，由 [`install_error_reason`] 从 detail
//! 前缀映射（`conflict` / `stale-preview` / `expired-preview` / `preview-not-found` /
//! `uninstall-conflict`；非安装族 = null）。IPC 与服务 API 的错误语义一致性由同一
//! 映射函数保证（锁：tests/tps_sampling_test.rs `ipc_error_reasons_consistent_with_api_errors`）。
//!
//! ## 凭证纪律（Spec §5「记录不进日志/报告」的 IPC 半边）
//!
//! `InstallReceipt` 的 Serialize 含**明文 Bearer**（tps_state 私有存储用）——命令层
//! 绝不原样回传 webview：[`TpsInstallReceiptDto`] 只保留 `receiver.port`，敏感键
//! （`install::SENSITIVE_ENV_KEYS`）的旧值置空（指纹在 `oldFingerprint`）。

use crate::services::tps::collector;
use crate::services::tps::install::{self, InstallChoice, InstallPreview, InstallReceipt};
use crate::services::tps::model::TpsSelection;
use crate::services::tps::query::{self, TpsViewRange};
use crate::services::tps::v2;
use crate::services::usage::error::UsageError;
use serde::Serialize;

// ==== 错误信封（reason 分类法，见模块头）====

/// tps 安装族命令的错误信封：code/detail 沿用 `UsageError`，另带机器可读 reason
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TpsCommandError {
    pub code: String,
    pub detail: String,
    pub reason: Option<String>,
}

impl From<UsageError> for TpsCommandError {
    fn from(e: UsageError) -> Self {
        TpsCommandError {
            reason: install_error_reason(&e.detail).map(str::to_string),
            code: e.code,
            detail: e.detail,
        }
    }
}

/// 安装族 detail 前缀 → 机器可读 reason（IPC 与 API 错误一致性的唯一映射点）：
/// * `tps-install-conflict` → `conflict`（已有相同信号目标，默认保留不覆盖）
/// * `tps-install-preview-stale` → `stale-preview`（预览后设置漂移，须重新 preview）
/// * `tps-install-preview-expired` → `expired-preview`（预览过期，须重新 preview）
/// * `tps-install-preview-not-found` → `preview-not-found`（已应用/已清理）
/// * `tps-uninstall-conflict` → `uninstall-conflict`（用户后改项保留）
/// * 其余（IO/库错/未知工具）→ None（不瞎分类）
pub fn install_error_reason(detail: &str) -> Option<&'static str> {
    const TABLE: &[(&str, &str)] = &[
        ("tps-install-conflict", "conflict"),
        ("tps-install-preview-stale", "stale-preview"),
        ("tps-install-preview-expired", "expired-preview"),
        ("tps-install-preview-not-found", "preview-not-found"),
        ("tps-uninstall-conflict", "uninstall-conflict"),
    ];
    TABLE
        .iter()
        .find(|(prefix, _)| detail.starts_with(prefix))
        .map(|(_, reason)| *reason)
}

// ==== 回执 IPC DTO（凭证剥离；见模块头）====

/// 单键回执的 IPC 面（敏感键 oldValue 置空、installedValue 脱敏——headers 键的
/// 安装值内嵌 `Authorization=Bearer <凭证>`，明文绝不回传 webview；指纹保留在
/// oldFingerprint，真实值只在 tps_state 私有存储供卸载比对）
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TpsReceiptEntryDto {
    pub key: String,
    pub old_exists: bool,
    /// 敏感键（OTEL_EXPORTER_OTLP_HEADERS）恒 None——明文绝不回传 webview
    pub old_value: Option<serde_json::Value>,
    /// 敏感键恒 `[已脱敏]`（展示面；安装值仍写盘供 Harness 遥测使用）
    pub installed_value: String,
    pub old_fingerprint: Option<String>,
}

/// 接收器身份的 IPC 面：只带端口（凭证只在 tps_state 私有存储，永不上 IPC）
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TpsReceiverPublicDto {
    pub port: u16,
}

/// 安装回执 IPC DTO（`InstallReceipt` 的脱敏镜像）
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TpsInstallReceiptDto {
    pub tool: String,
    pub applied_at_ms: i64,
    pub choice: InstallChoice,
    pub entries: Vec<TpsReceiptEntryDto>,
    pub receiver: Option<TpsReceiverPublicDto>,
}

impl From<&InstallReceipt> for TpsInstallReceiptDto {
    fn from(r: &InstallReceipt) -> Self {
        TpsInstallReceiptDto {
            tool: r.tool.clone(),
            applied_at_ms: r.applied_at_ms,
            choice: r.choice,
            entries: r
                .entries
                .iter()
                .map(|e| {
                    let sensitive = install::SENSITIVE_ENV_KEYS.contains(&e.key.as_str());
                    TpsReceiptEntryDto {
                        key: e.key.clone(),
                        old_exists: e.old_exists,
                        old_value: if sensitive {
                            None // 敏感键旧值置空（指纹已在 old_fingerprint）
                        } else {
                            e.old_value.clone()
                        },
                        installed_value: if sensitive {
                            "[已脱敏]".to_string() // 安装值内嵌 Bearer，展示面脱敏
                        } else {
                            e.installed_value.clone()
                        },
                        old_fingerprint: e.old_fingerprint.clone(),
                    }
                })
                .collect(),
            receiver: r
                .receiver
                .as_ref()
                .map(|c| TpsReceiverPublicDto { port: c.port }),
        }
    }
}

// ==== 刷新 / 心跳载荷 ====

/// tps_refresh 返回（限频/单飞让路时 refreshed=false + reason）
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TpsRefreshResult {
    pub refreshed: bool,
    /// `rate-limited` / `busy` / `disabled`；成功 = null
    pub reason: Option<String>,
    pub snapshot: Option<query::TpsSnapshotResult>,
}

/// tps_view_heartbeat 返回（心跳后的调度模式；前端可据此微调提示）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TpsHeartbeatResult {
    /// `background` / `foreground` / `disabled`
    pub mode: &'static str,
}

fn mode_str(m: crate::services::tps::scheduler::SamplerMode) -> &'static str {
    use crate::services::tps::scheduler::SamplerMode::*;
    match m {
        Background => "background",
        Foreground => "foreground",
        Disabled => "disabled",
    }
}

// ==== 七命令（签名逐字锁在 tests/tps_sampling_test.rs）====

/// 快照（近 24h 事实的最近调用/当前回合 + 真实来源状态；总授权关闭 → disabled）
#[tauri::command]
pub async fn tps_snapshot(selection: TpsSelection) -> Result<query::TpsSnapshotResult, UsageError> {
    tauri::async_runtime::spawn_blocking(move || {
        let conn = crate::database::connection::DB
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        query::snapshot(&conn, &selection, crate::services::usage::now_ms())
    })
    .await
    .map_err(|e| UsageError::internal(format!("tps snapshot task failed: {e}")))?
}

/// 曲线（view_range 尾查；known_series 供空窗对齐；runId/trigger/status 供断线）
#[tauri::command]
pub async fn tps_points(
    selection: TpsSelection,
    view_range: TpsViewRange,
) -> Result<query::TpsPointsResult, UsageError> {
    tauri::async_runtime::spawn_blocking(move || {
        let conn = crate::database::connection::DB
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        query::points(
            &conn,
            &selection,
            view_range,
            crate::services::usage::now_ms(),
        )
    })
    .await
    .map_err(|e| UsageError::internal(format!("tps points task failed: {e}")))?
}

/// V2 速度 dashboard（2026-10-26 计划 Task 6，契约 `tps_speed_dashboard(query)`）：
/// 只读区间/桶聚合——与 `tps_snapshot` 同款开连接方式，绝不触发采集/读取器/源扫描
/// （Spec §2「查询不触发源扫描或模型请求」）；总授权关闭 ⇒ disabled 空载荷不删历史。
#[tauri::command]
pub async fn tps_speed_dashboard(
    query: v2::model::SpeedDashboardQuery,
) -> Result<v2::model::SpeedDashboardSnapshot, UsageError> {
    tauri::async_runtime::spawn_blocking(move || {
        let conn = crate::database::connection::DB
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        v2::query::dashboard(&conn, &query, crate::services::usage::now_ms())
    })
    .await
    .map_err(|e| UsageError::internal(format!("tps speed dashboard task failed: {e}")))?
}

/// 手动刷新（立即读取 + manual 点；最小间隔 5 秒，限频/繁忙时 refreshed=false）
#[tauri::command]
pub async fn tps_refresh(selection: TpsSelection) -> Result<TpsRefreshResult, UsageError> {
    tauri::async_runtime::spawn_blocking(move || {
        let (refreshed, reason, _written) = collector::manual_refresh_execute()?;
        let conn = crate::database::connection::DB
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let snapshot = query::snapshot(&conn, &selection, crate::services::usage::now_ms())?;
        Ok(TpsRefreshResult {
            refreshed,
            reason: reason.map(str::to_string),
            snapshot: Some(snapshot),
        })
    })
    .await
    .map_err(|e| UsageError::internal(format!("tps refresh task failed: {e}")))?
}

/// 视图心跳（窗口与 TPS 区块实际可见才传 visible=true；30 秒一拍，租约 90 秒）
#[tauri::command]
pub async fn tps_view_heartbeat(
    view_id: String,
    visible: bool,
) -> Result<TpsHeartbeatResult, UsageError> {
    collector::heartbeat_view(&view_id, visible);
    Ok(TpsHeartbeatResult {
        mode: mode_str(collector::current_mode()),
    })
}

/// 安装预览（只读设置文件 + 端口真实探测；敏感统计 header 只给脱敏值/指纹）
#[tauri::command]
pub async fn tps_install_preview(tool: String) -> Result<InstallPreview, TpsCommandError> {
    tauri::async_runtime::spawn_blocking(move || install::preview(&tool).map_err(Into::into))
        .await
        .map_err(|e| {
            TpsCommandError::from(UsageError::internal(format!(
                "tps preview task failed: {e}"
            )))
        })?
}

/// 应用预览（指纹漂移拒绝；凭证剥离的回执）
#[tauri::command]
pub async fn tps_install_apply(
    preview_id: String,
    choice: InstallChoice,
) -> Result<TpsInstallReceiptDto, TpsCommandError> {
    tauri::async_runtime::spawn_blocking(move || {
        install::apply(&preview_id, choice)
            .map(|receipt| TpsInstallReceiptDto::from(&receipt))
            .map_err(Into::into)
    })
    .await
    .map_err(|e| {
        TpsCommandError::from(UsageError::internal(format!("tps apply task failed: {e}")))
    })?
}

/// 卸载自有统计项（仍匹配安装值才处置；用户后改项保留并报冲突）
#[tauri::command]
pub async fn tps_uninstall(tool: String) -> Result<(), TpsCommandError> {
    tauri::async_runtime::spawn_blocking(move || install::uninstall(&tool).map_err(Into::into))
        .await
        .map_err(|e| {
            TpsCommandError::from(UsageError::internal(format!(
                "tps uninstall task failed: {e}"
            )))
        })?
}

// ==== IPC 契约自检（签名逐字 / 注册 / 凭证纪律的源码级锁）====
#[cfg(test)]
mod tests {
    /// 命令注册表所在文件（generate_handler 与 setup 闭包）
    const LIB_SRC: &str = include_str!("../lib.rs");
    const SELF_SRC: &str = include_str!("tps.rs");

    /// 七命令 + V2 dashboard 逐字注册（Spec §6 IPC 契约的名字面；锁与集成测试互补）
    #[test]
    fn seven_commands_are_registered_once() {
        for cmd in [
            "commands::tps::tps_snapshot",
            "commands::tps::tps_points",
            "commands::tps::tps_refresh",
            "commands::tps::tps_speed_dashboard",
            "commands::tps::tps_view_heartbeat",
            "commands::tps::tps_install_preview",
            "commands::tps::tps_install_apply",
            "commands::tps::tps_uninstall",
        ] {
            assert_eq!(
                LIB_SRC.matches(cmd).count(),
                1,
                "{cmd} 必须在 generate_handler 注册恰好一次"
            );
        }
    }

    /// reason 映射表 = 安装族错误前缀全收录；未知前缀不瞎分类
    #[test]
    fn reason_table_covers_install_family_only() {
        assert_eq!(
            super::install_error_reason("tps-install-conflict: x"),
            Some("conflict")
        );
        assert_eq!(
            super::install_error_reason("tps-install-preview-stale: x"),
            Some("stale-preview")
        );
        assert_eq!(
            super::install_error_reason("tps-install-preview-expired: x"),
            Some("expired-preview")
        );
        assert_eq!(
            super::install_error_reason("tps-install-preview-not-found: x"),
            Some("preview-not-found")
        );
        assert_eq!(
            super::install_error_reason("tps-uninstall-conflict: x"),
            Some("uninstall-conflict")
        );
        assert_eq!(
            super::install_error_reason("usage-source-io: tps 其他错误"),
            None
        );
    }

    /// 回执 DTO 源码纪律：凭证字段不得出现在 IPC DTO（明文只在 tps_state 私有存储）。
    /// 判串用 concat! 拆写——字面量直写会被 include_str! 的自扫描命中（自指假阳）。
    #[test]
    fn receipt_dto_never_carries_credential_field() {
        let needle = concat!("credential", ":");
        assert!(
            !SELF_SRC.contains(needle),
            "TpsInstallReceiptDto 不得携带 credential 字段（凭证永不上 IPC）"
        );
    }
}
