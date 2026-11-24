//! 代际镜像守卫（T07；计划「旧代际不污染」——撤权/断链/换代立即拒绝新写，
//! 旧代际的晚帧不得进入镜像态）。
//!
//! 设计：镜像持有**当前代际**（hub 视角，宿主重启换代时由调用方
//! [`EventMirror::advance_generation`] 推进）；每条入站事件带其来源连接的
//! 代际入镜。代际不符（旧代际迟到帧）→ `apply` 返回 `None`——事件被丢弃，
//! 镜像态停在当前代际（晚帧测试断言 `mirror_after_late.epoch == current_epoch`）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// 入镜事件：打上当前代际的原生事件载荷。
#[derive(Debug, Clone, PartialEq)]
pub struct MirroredEvent {
    /// 入镜时镜像的当前代际。
    pub epoch: u64,
    /// 原生事件载荷（mux `item.value` 原样；业务投影由后续任务做）。
    pub value: serde_json::Value,
}

/// 镜像观测快照（晚帧测试断言 `mirror_after_late.epoch == current_epoch`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorSnapshot {
    /// 当前代际。
    pub epoch: u64,
    /// 最近一条入镜事件的代际（尚无入镜 → None）。
    pub latest_epoch: Option<u64>,
    /// 累计入镜条数（旧代际丢弃不计）。
    pub applied: u64,
}

struct MirrorInner {
    latest: Option<MirroredEvent>,
    applied: u64,
}

/// 代际镜像（连接核心共享一把；线程安全）。
pub struct EventMirror {
    epoch: AtomicU64,
    inner: Mutex<MirrorInner>,
}

impl EventMirror {
    /// 以连接的绑定代际建镜。
    pub fn new(epoch: u64) -> Self {
        Self {
            epoch: AtomicU64::new(epoch),
            inner: Mutex::new(MirrorInner {
                latest: None,
                applied: 0,
            }),
        }
    }

    /// 当前代际。
    pub fn current_epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// 推进代际（宿主重启/撤权重连）；只前进不回退（错乱的旧代数值不得倒退镜像）。
    pub fn advance_generation(&self, epoch: u64) {
        let mut current = self.epoch.load(Ordering::SeqCst);
        while epoch > current {
            match self
                .epoch
                .compare_exchange(current, epoch, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    /// 入镜：来源代际 = 当前代际才收录并打代；否则丢弃（返回 None，不污染）。
    pub fn apply(&self, frame_epoch: u64, value: serde_json::Value) -> Option<MirroredEvent> {
        if frame_epoch != self.current_epoch() {
            return None;
        }
        let event = MirroredEvent {
            epoch: frame_epoch,
            value,
        };
        let mut inner = self.inner.lock().unwrap();
        inner.latest = Some(event.clone());
        inner.applied += 1;
        Some(event)
    }

    /// 最近一条入镜事件。
    pub fn latest(&self) -> Option<MirroredEvent> {
        self.inner.lock().unwrap().latest.clone()
    }

    /// 观测快照。
    pub fn snapshot(&self) -> MirrorSnapshot {
        let inner = self.inner.lock().unwrap();
        MirrorSnapshot {
            epoch: self.epoch.load(Ordering::SeqCst),
            latest_epoch: inner.latest.as_ref().map(|e| e.epoch),
            applied: inner.applied,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 同代入镜打代；换代后旧代帧丢弃且快照停在当前代。
    #[test]
    fn late_frame_from_old_generation_is_dropped() {
        let mirror = EventMirror::new(1);
        assert!(mirror.apply(1, json!({"type": "turn/start"})).is_some());
        mirror.advance_generation(2);
        assert!(mirror.apply(1, json!({"type": "late"})).is_none());
        let snapshot = mirror.snapshot();
        assert_eq!(snapshot.epoch, 2);
        assert_eq!(snapshot.latest_epoch, Some(1));
        assert_eq!(snapshot.applied, 1);
        // 新代事件恢复入镜；advance 只前进不回退。
        assert!(mirror.apply(2, json!({"type": "turn/start"})).is_some());
        mirror.advance_generation(1);
        assert_eq!(mirror.current_epoch(), 2);
    }
}
