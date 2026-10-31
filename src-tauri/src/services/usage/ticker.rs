//! 用量采集兜底 ticker：可见触发之外的低频保障，不依赖任何页面。
//! 步长必须大于 `collectIntervalMin`，否则最小间隔会让隔轮空跑。
//!
//! * debug 构建**默认关闭**（避免开发期把真实账本 `~/.tuvis/tuvis.db` 灌脏，
//!   见 2026-10-07 采集事故纪律）；`TUVIS_USAGE_TICKER=1` 显式开启；
//! * 每轮以 `collect(false)` 走服务层既有单飞 + 最小间隔，**绝不 force**；
//! * **设置读取与采集轮次双双 `catch_unwind`**：毒锁 / `DB` 初始化失败（读取间隔时
//!   panic）与采集 panic 都不得让常驻线程静默死亡（与 scan bus 的 F1 自愈同构）。

use std::time::Duration;

/// 兜底步长 = 采集间隔 + 2 分钟缓冲（分钟数下限 1）。
pub(crate) fn step_ms(interval_min: u64) -> u64 {
    interval_min.max(1) * 60_000 + 120_000
}

/// debug 构建默认关闭；`TUVIS_USAGE_TICKER=1` 显式开启。
pub(crate) fn default_enabled(debug: bool, env: Option<String>) -> bool {
    if env.as_deref() == Some("1") {
        return true;
    }
    !debug
}

/// 当前设置的采集间隔（分钟，下限 1）。设置类型是 `i64`，负值/0 一律收敛到 1。
fn usage_interval_min() -> u64 {
    crate::services::usage::settings::load()
        .collect_interval_min
        .max(1) as u64
}

/// 设置读取的 panic 安全包装：毒锁 / `DB` 初始化失败会在 `load()` 处 panic，
/// 捕获后退回 1 分钟步长，常驻线程不因设置读取而死。函数指针天然 `UnwindSafe`。
fn protected_interval_min() -> u64 {
    std::panic::catch_unwind(usage_interval_min).unwrap_or_else(|e| {
        log::error!(
            "usage: 兜底 ticker 读取采集间隔 panic，按 1 分钟步长继续（{:?}）",
            e
        );
        1
    })
}

pub fn spawn_runtime() {
    let env = std::env::var("TUVIS_USAGE_TICKER").ok();
    if !default_enabled(cfg!(debug_assertions), env) {
        log::info!("usage: 兜底 ticker 在 debug 构建默认关闭（TUVIS_USAGE_TICKER=1 可开）");
        return;
    }
    std::thread::spawn(|| {
        // 启动日志要拿步长 ⇒ 线程内先读一次设置（不在 setup 线程里抢 DB 锁）；
        // 与每轮读取同样走 panic 安全包装，线程不会死在第一条日志上
        log::info!(
            "usage: 兜底 ticker 已启动（步长 {} 分钟）",
            step_ms(protected_interval_min()) / 60_000
        );
        loop {
            // 每轮重读设置：用户改采集间隔后下一轮自动跟随。
            // 读取本身也在 catch_unwind 内（毒锁 / DB 初始化失败不得杀死常驻线程）
            let interval = protected_interval_min();
            std::thread::sleep(Duration::from_millis(step_ms(interval)));
            // F7 自愈：采集 panic 时捕获并退避，常驻线程不自杀
            let round = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let r = crate::services::usage::collect::collect(false);
                log::info!(
                    "usage: 兜底采集完成（{} 源 / {} 条新记录 / {}ms）",
                    r.sources.len(),
                    r.total_new_records,
                    r.duration_ms
                );
                // `usage-collected` 已由 `collect_with` 在**真实扫描完成处**统一广播
                // （0 条新记录也发；最小间隔内复用不发）——本线程**不得**再条件 emit，
                // 否则同一轮会双发、且「0 条新记录」这一轮前端收不到刷新。
            }));
            if let Err(e) = round {
                log::error!("usage: 兜底采集轮次 panic，已捕获并继续（{:?}）", e);
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    });
}

// ==== usage 自省锁：以下为排除区（测试代码），勿删勿复制 ====
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_is_min_interval_plus_buffer() {
        assert_eq!(step_ms(10), 10 * 60_000 + 120_000);
        // 1 分钟（下限）= 60s + 2min 缓冲；`1 * 60_000` 会触发 clippy::identity_op，故直写
        assert_eq!(step_ms(1), 60_000 + 120_000);
    }

    #[test]
    fn enabled_by_default_only_in_release() {
        assert_eq!(
            default_enabled(cfg!(debug_assertions), None),
            !cfg!(debug_assertions)
        );
        assert!(default_enabled(true, Some("1".into())));
    }
}
// ==== usage 自省锁：排除区结束，勿删勿复制 ====
