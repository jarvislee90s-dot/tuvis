//! E11 测试专用确定性停点（默认关，仅 debug 证据构建启用）。
//!
//! 目的：在**真实发送响应已返回 Accepted、动作终态尚未落账**的窗口内提供一次性
//! 停点，让验收者能精确强杀本次影子 Tuvis，从而得到真实的 `reserved → unknown`
//! 恢复样本——而不是「发送后立即杀」的竞态替代。
//!
//! # 启用条件（全部满足才停）
//! 1. 构建含 `codex-ipc-e2e-stop` feature 且为 debug（release 由 `codex_control/mod.rs`
//!    的编译门直接拒绝）；
//! 2. `TUVIS_HOME` 指向**影子数据目录**（未设即拒绝启用——绝不在真实账本上停点）；
//! 3. 环境变量 `TUVIS_E2E_STOP_ACTION` **精确等于**本动作 actionId（不通配）；
//! 4. 该 action 首次命中（重复同 action 不再停）。
//!
//! # 停点行为
//! - 在 `$TUVIS_HOME/codex-e2e-stop/reached.json` 写入 action/turn/阶段并 flush；
//! - 最多等 120 秒；出现 `release` 文件或超时即继续原落账流程；
//! - 日志与文件不记录消息正文、设备凭据或原始协议对象。

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;

/// 停点最长等待（E11 步骤 2：验收者须在 120 秒内强杀影子 Tuvis）。
pub const STOP_TIMEOUT_MS: u64 = 120_000;
/// 轮询 release 文件的步长。
const RELEASE_POLL_MS: u64 = 200;

/// 一次性登记（同一 action 只停一次；进程内即可，重启后重新构建本进程状态）。
static STOPPED: Lazy<Mutex<std::collections::HashSet<String>>> =
    Lazy::new(|| Mutex::new(std::collections::HashSet::new()));

/// 影子 home（启用前置）。空串/纯空白视同未设。
fn shadow_home() -> Option<PathBuf> {
    std::env::var("TUVIS_HOME")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn stop_dir() -> Option<PathBuf> {
    shadow_home().map(|h| h.join("codex-e2e-stop"))
}

/// 等待上限（测试可经 `TUVIS_E2E_STOP_TIMEOUT_MS` 收窄；生产/验收不设 = 120s）。
fn timeout_ms() -> u64 {
    std::env::var("TUVIS_E2E_STOP_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|v| v.min(STOP_TIMEOUT_MS))
        .unwrap_or(STOP_TIMEOUT_MS)
}

/// 目标 actionId（缺省/空白 = 不停任何动作）。
fn target_action() -> Option<String> {
    std::env::var("TUVIS_E2E_STOP_ACTION")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// **停点入口**：在 `dispatch.send_text(...).await` 返回 Accepted 且 turnId 非空之后、
/// `finish_with_turn_barrier` 之前调用。返回 `true` 表示确实停过（供调用方观测）。
pub async fn maybe_pause(action_id: &str, turn_id: &str) -> bool {
    let Some(target) = target_action() else {
        return false;
    };
    if target != action_id {
        return false;
    }
    let Some(dir) = stop_dir() else {
        log::warn!("E2E 停点拒绝启用：TUVIS_HOME 未指向影子数据目录");
        return false;
    };
    // 一次性：重复同 action 不停（避免恢复循环/重试再次命中）
    {
        let mut stopped = STOPPED.lock().unwrap_or_else(|e| e.into_inner());
        if !stopped.insert(action_id.to_string()) {
            return false;
        }
    }
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::warn!("E2E 停点目录创建失败（继续原落账）: {e}");
        return false;
    }
    let reached = dir.join("reached.json");
    let payload = serde_json::json!({
        "actionId": action_id,
        "turnId": turn_id,
        "phase": "accepted_before_finish",
    });
    match std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&reached)
    {
        Ok(mut f) => {
            if let Err(e) = writeln!(f, "{payload}").and_then(|_| f.flush()) {
                log::warn!("E2E 停点 reached.json 写入失败（继续原落账）: {e}");
                return false;
            }
        }
        Err(e) => {
            log::warn!("E2E 停点 reached.json 打开失败（继续原落账）: {e}");
            return false;
        }
    }
    log::info!(
        "E2E 停点命中：动作 {action_id} 已 Accepted，等待 release（最长 {}s）",
        timeout_ms() / 1000
    );

    let deadline = Instant::now() + Duration::from_millis(timeout_ms());
    let release = dir.join("release");
    while Instant::now() < deadline {
        if release.exists() {
            log::info!("E2E 停点收到 release，继续原落账");
            break;
        }
        tokio::time::sleep(Duration::from_millis(RELEASE_POLL_MS)).await;
    }
    if Instant::now() >= deadline {
        log::warn!("E2E 停点超时（未收到 release），继续原落账");
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 环境隔离守卫：Drop 时恢复全部相关变量（测试串行由 ENV_LOCK 保证）。
    struct EnvGuard {
        home: Option<String>,
        action: Option<String>,
        timeout: Option<String>,
    }

    impl EnvGuard {
        fn set(home: Option<&str>, action: Option<&str>, timeout: Option<&str>) -> Self {
            let g = Self {
                home: std::env::var("TUVIS_HOME").ok(),
                action: std::env::var("TUVIS_E2E_STOP_ACTION").ok(),
                timeout: std::env::var("TUVIS_E2E_STOP_TIMEOUT_MS").ok(),
            };
            match home {
                Some(v) => std::env::set_var("TUVIS_HOME", v),
                None => std::env::remove_var("TUVIS_HOME"),
            }
            match action {
                Some(v) => std::env::set_var("TUVIS_E2E_STOP_ACTION", v),
                None => std::env::remove_var("TUVIS_E2E_STOP_ACTION"),
            }
            match timeout {
                Some(v) => std::env::set_var("TUVIS_E2E_STOP_TIMEOUT_MS", v),
                None => std::env::remove_var("TUVIS_E2E_STOP_TIMEOUT_MS"),
            }
            g
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.home {
                Some(v) => std::env::set_var("TUVIS_HOME", v),
                None => std::env::remove_var("TUVIS_HOME"),
            }
            match &self.action {
                Some(v) => std::env::set_var("TUVIS_E2E_STOP_ACTION", v),
                None => std::env::remove_var("TUVIS_E2E_STOP_ACTION"),
            }
            match &self.timeout {
                Some(v) => std::env::set_var("TUVIS_E2E_STOP_TIMEOUT_MS", v),
                None => std::env::remove_var("TUVIS_E2E_STOP_TIMEOUT_MS"),
            }
        }
    }

    fn unique(prefix: &str) -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{prefix}-{n:x}")
    }

    /// 不匹配的 action：零文件零暂停
    #[tokio::test]
    async fn unmatched_action_does_not_stop() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let action = unique("a");
        let home = dir.path().to_string_lossy().to_string();
        let _env = EnvGuard::set(Some(&home), Some("other-action"), None);
        let stopped = maybe_pause(&action, "turn-1").await;
        assert!(!stopped);
        assert!(!dir.path().join("codex-e2e-stop/reached.json").exists());
    }

    /// 无影子 TUVIS_HOME：拒绝启用（绝不在真实账本上停点）
    #[tokio::test]
    async fn missing_shadow_home_refuses() {
        let _g = ENV_LOCK.lock().unwrap();
        let action = unique("b");
        let _env = EnvGuard::set(None, Some(&action), None);
        let stopped = maybe_pause(&action, "turn-1").await;
        assert!(!stopped);
    }

    /// 命中：reached.json 携带 action/turn/阶段，flush 后可见；release 立刻放行；
    /// 重复同 action 不再停
    #[tokio::test]
    async fn matched_action_stops_once_and_release_resumes() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let action = unique("c");
        let home = dir.path().to_string_lossy().to_string();
        let stop_dir = dir.path().join("codex-e2e-stop");
        std::fs::create_dir_all(&stop_dir).unwrap();
        std::fs::write(stop_dir.join("release"), b"go").unwrap();
        let _env = EnvGuard::set(Some(&home), Some(&action), Some("5000"));
        let stopped = maybe_pause(&action, "turn-1").await;
        assert!(stopped, "匹配 action + 影子 home 必须停");
        let reached = std::fs::read_to_string(stop_dir.join("reached.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&reached).unwrap();
        assert_eq!(v["actionId"], action);
        assert_eq!(v["turnId"], "turn-1");
        assert_eq!(v["phase"], "accepted_before_finish");
        // 一次性：同 action 第二次直接返回 false（不重复停）
        std::fs::remove_file(stop_dir.join("release")).unwrap();
        let again = maybe_pause(&action, "turn-1").await;
        assert!(!again, "重复同 action 不再停");
    }

    /// 超时收尾：无 release 也按上限继续（测试收窄到 100ms）
    #[tokio::test]
    async fn timeout_resumes_without_release() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let action = unique("d");
        let home = dir.path().to_string_lossy().to_string();
        let started = std::time::Instant::now();
        let _env = EnvGuard::set(Some(&home), Some(&action), Some("100"));
        let stopped = maybe_pause(&action, "turn-1").await;
        assert!(stopped);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "超时收尾不得挂死"
        );
    }
}
