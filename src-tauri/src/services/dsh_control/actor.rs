//! 单会话 actor（T09）：同一 DSH 会话的动作严格串行，不同会话互不阻塞。
//!
//! **为什么需要**：DSH 原生会话自身有队列/暂停/阻塞语义，两个远程动作并发
//! 进入同一会话会产生「两端抢答」「结果未知」这类不可归因状态（计划 T09/T10）。
//! actor 把同一会话的所有写动作收敛成一条串行车道；只读快照不进车道。
//!
//! **锁纪律**：每会话一把 `Mutex`（按 session_id 懒建），持锁只包住同步派发
//! 步骤——不跨 await、不持有 DB 连接做网络 IO。不同会话互相独立，避免一个
//! 卡住的会话拖死全部。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::error::DshError;

type Lane = Arc<Mutex<()>>;

/// 车道表超过该规模时回收空闲车道（避免按会话数无界增长）。
const LANE_GC_THRESHOLD: usize = 256;

/// DSH 单会话 actor：只拥有**会话车道表**（不持数据库连接——连接由 hub 的
/// `DshDb` 提供，锁序为「车道 → 连接」，见 `mod::DshControlHub::dispatch`）。
pub struct DshSessionActor {
    lanes: Mutex<HashMap<String, Lane>>,
}

impl DshSessionActor {
    pub fn new() -> Self {
        Self {
            lanes: Mutex::new(HashMap::new()),
        }
    }

    /// 在指定会话的串行车道里执行 `f`。
    ///
    /// 同会话按调用顺序串行；不同会话各自独立。`f` 必须是同步步骤（不跨 await）。
    ///
    /// **锁纪律（复审 M7）**：车道 mutex 一旦因 panic 中毒，用 `into_inner()` 恢复
    /// 继续服务（否则该会话**永久**不可用）；返回 `Result` 只为把闭包内的失败原样
    /// 传出——调用方用 [`run_serialized_try`] 传 `Result` 闭包。
    pub fn run_serialized<F, T>(&self, session_id: &str, f: F) -> Result<T, DshError>
    where
        F: FnOnce() -> T,
    {
        let lane = self.lane(session_id);
        let _guard = lane.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(f())
    }

    /// 同 [`run_serialized`]，但闭包可以返回 `Result`——错误原样传播给调用方
    /// （不是「恒 Ok」的假成功）。
    pub fn run_serialized_try<F, T>(&self, session_id: &str, f: F) -> Result<T, DshError>
    where
        F: FnOnce() -> Result<T, DshError>,
    {
        let lane = self.lane(session_id);
        let _guard = lane.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        f()
    }

    /// 取/建会话车道（hub 泵路径用：在阻塞段内持车道做账本事务）。
    pub fn lane_for(&self, session_id: &str) -> Lane {
        self.lane(session_id)
    }

    /// 取/建车道并顺带回收**已无外部持有**的空闲车道（`Arc::strong_count == 1`
    /// 表示只有车道表自己持有），防止长期运行按会话数无界增长（复审 M7）。
    fn lane(&self, session_id: &str) -> Lane {
        let mut lanes = self.lanes.lock().unwrap_or_else(|p| p.into_inner());
        if lanes.len() > LANE_GC_THRESHOLD {
            lanes.retain(|_, lane| Arc::strong_count(lane) > 1);
        }
        Arc::clone(lanes.entry(session_id.to_string()).or_default())
    }

    /// 已登记的会话车道数（诊断/测试边界）。
    pub fn lane_count(&self) -> usize {
        self.lanes.lock().unwrap_or_else(|p| p.into_inner()).len()
    }
}

impl Default for DshSessionActor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 抽出车道表本身做并发验证（不借 `Connection`：它非 `Sync`）。
    fn lane_for(lanes: &Mutex<HashMap<String, Lane>>, session: &str) -> Lane {
        let mut map = lanes.lock().unwrap();
        Arc::clone(map.entry(session.to_string()).or_default())
    }

    /// 8 线程同时进入同一会话车道：临界区绝无重叠。
    #[test]
    fn shared_lane_is_mutually_exclusive_across_threads() {
        let lanes: Mutex<HashMap<String, Lane>> = Mutex::new(HashMap::new());
        let inside = std::sync::atomic::AtomicUsize::new(0);
        let max_inside = std::sync::atomic::AtomicUsize::new(0);

        std::thread::scope(|scope| {
            for _ in 0..8 {
                let lanes = &lanes;
                let inside = &inside;
                let max_inside = &max_inside;
                scope.spawn(move || {
                    let lane = lane_for(lanes, "s-shared");
                    let _guard = lane.lock().unwrap();
                    let now = inside.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    max_inside.fetch_max(now, std::sync::atomic::Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    inside.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                });
            }
        });
        assert_eq!(
            max_inside.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "同会话临界区绝不能被两个线程同时进入"
        );
    }

    /// 不同会话车道相互独立（各自一把锁）。
    #[test]
    fn different_sessions_use_different_lanes() {
        let lanes: Mutex<HashMap<String, Lane>> = Mutex::new(HashMap::new());
        let a = lane_for(&lanes, "s-1");
        let b = lane_for(&lanes, "s-2");
        assert!(!Arc::ptr_eq(&a, &b), "不同会话不得共用同一把锁");
        // 同会话再次取得同一把锁。
        let a2 = lane_for(&lanes, "s-1");
        assert!(Arc::ptr_eq(&a, &a2));
    }
}
