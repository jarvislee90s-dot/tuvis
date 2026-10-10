// 后端 → 前端事件出口（M4 基建）：远程模块运行在 tauri::async_runtime 任务里，
// 无窗口上下文——统一经全局 APP_HANDLE emit；无句柄（单测）时静默。
// 审计留痕（T2e）同文件收口：全部走 remote_audit target，日志侧可追溯。

use std::sync::OnceLock;
use tauri::Emitter;

/// lib.rs setup 里 set 的全局句柄（远程模块不直接依赖 tauri runtime 上下文）。
/// OnceLock 先到先得：setup 只跑一次，重复 set 的 Err 直接忽略
pub static APP_HANDLE: OnceLock<tauri::AppHandle> = OnceLock::new();

/// 向前端 emit 事件；无句柄或 emit 失败静默降级（通知是尽力而为，不阻断主流程）
pub fn emit_ui(event: &str, payload: impl serde::Serialize + Clone) {
    if let Some(app) = APP_HANDLE.get() {
        if let Err(e) = app.emit(event, payload) {
            log::warn!("emit {event} 失败: {e}");
        }
    }
}

/// 配对/吊销/停止动作审计留痕（T2e）：单行结构化，日志可 grep `remote_audit`
pub fn audit(action: &str, detail: &str) {
    log::info!(target: "remote_audit", "{action} {detail}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emit_ui_without_handle_is_silent() {
        // 无句柄不 panic（单测环境 OnceLock 未 set，get() 返回 None——静默分支）
        emit_ui("remote-changed", serde_json::json!({"enabled": true}));
    }

    #[test]
    fn audit_writes_log_line() {
        // 审计出口不 panic（真实留痕由 remote_audit target 的日志侧验证）
        audit("pair_pin_verified", "via=lan ip=127.0.0.1");
    }
}
