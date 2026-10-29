//! 扫描节奏状态机（纯函数，无 IO）。
//! 输入三信号：活跃会话 / 远控在线 / 有人在看；输出 2s / 10s / 30s。
//! 升档立即生效；降档需在候选状态稳定一个「当前档位间隔」后才切换（防抖）。
//! 时钟约定：`now_ms` 必须单调（生产传入系统时钟纪元毫秒），时间回拨只会推迟降档确认而不会 panic。
//! 采样语义：降档确认发生在越过边界的首次观测，观测间隔很长时下一次调用即按当时信号确认。

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Active,
    Idle,
    Away,
}

impl Mode {
    pub fn interval(self) -> Duration {
        match self {
            Mode::Active => Duration::from_secs(2),
            Mode::Idle => Duration::from_secs(10),
            Mode::Away => Duration::from_secs(30),
        }
    }

    fn rank(self) -> u8 {
        match self {
            Mode::Active => 2,
            Mode::Idle => 1,
            Mode::Away => 0,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Signals {
    pub active_sessions: bool,
    pub remote_online: bool,
    pub user_watching: bool,
}

impl Signals {
    fn target(&self) -> Mode {
        if self.active_sessions {
            Mode::Active
        } else if self.remote_online || self.user_watching {
            Mode::Idle
        } else {
            Mode::Away
        }
    }
}

pub struct Cadence {
    mode: Mode,
    candidate: Option<Mode>,
    candidate_since_ms: i64,
}

impl Cadence {
    pub fn new(_now_ms: i64) -> Self {
        // 启动先按 Idle（首扫完成后按真实信号收敛）
        Self {
            mode: Mode::Idle,
            candidate: None,
            candidate_since_ms: 0,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// 返回 Some(new_mode) 表示发生了切换
    pub fn observe(&mut self, signals: Signals, now_ms: i64) -> Option<Mode> {
        let target = signals.target();
        if target == self.mode {
            self.candidate = None;
            return None;
        }
        if target.rank() > self.mode.rank() {
            self.mode = target;
            self.candidate = None;
            return Some(target);
        }
        match self.candidate {
            Some(c) if c == target => {
                if now_ms - self.candidate_since_ms >= self.mode.interval().as_millis() as i64 {
                    self.mode = target;
                    self.candidate = None;
                    Some(target)
                } else {
                    None
                }
            }
            _ => {
                self.candidate = Some(target);
                self.candidate_since_ms = now_ms;
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_is_immediate_and_has_priority() {
        let mut c = Cadence::new(0);
        let m = c.observe(
            Signals {
                active_sessions: true,
                remote_online: false,
                user_watching: false,
            },
            1_000,
        );
        assert_eq!(m, Some(Mode::Active));
        assert_eq!(c.mode(), Mode::Active);
        assert_eq!(c.mode().interval().as_millis(), 2_000);
    }

    #[test]
    fn downgrade_requires_one_stable_interval() {
        let mut c = Cadence::new(0);
        c.observe(
            Signals {
                active_sessions: true,
                remote_online: false,
                user_watching: false,
            },
            0,
        );
        // 降到 Idle：第一次只立项，不切换
        let m = c.observe(
            Signals {
                active_sessions: false,
                remote_online: true,
                user_watching: false,
            },
            2_000,
        );
        assert_eq!(m, None);
        assert_eq!(c.mode(), Mode::Active);
        // 边界前 1ms：1999ms < 2000ms，仍不切换
        let m = c.observe(
            Signals {
                active_sessions: false,
                remote_online: true,
                user_watching: false,
            },
            3_999,
        );
        assert_eq!(m, None);
        assert_eq!(c.mode(), Mode::Active);
        // 满一个 Active 间隔后切换
        let m = c.observe(
            Signals {
                active_sessions: false,
                remote_online: true,
                user_watching: false,
            },
            4_000,
        );
        assert_eq!(m, Some(Mode::Idle));
    }

    #[test]
    fn away_requires_no_remote_and_no_watcher() {
        let mut c = Cadence::new(0);
        // Idle → Away 同样要防抖
        c.observe(
            Signals {
                active_sessions: false,
                remote_online: true,
                user_watching: false,
            },
            0,
        );
        c.observe(
            Signals {
                active_sessions: false,
                remote_online: false,
                user_watching: false,
            },
            10_000,
        );
        let m = c.observe(
            Signals {
                active_sessions: false,
                remote_online: false,
                user_watching: false,
            },
            20_000,
        );
        assert_eq!(m, Some(Mode::Away));
        assert_eq!(c.mode().interval().as_millis(), 30_000);
        // 远控回来 → Away 升到 Idle 立即生效（升档无防抖）
        let m = c.observe(
            Signals {
                active_sessions: false,
                remote_online: true,
                user_watching: false,
            },
            20_001,
        );
        assert_eq!(m, Some(Mode::Idle));
        assert_eq!(c.mode(), Mode::Idle);
    }

    #[test]
    fn candidate_resets_when_signal_flaps_back() {
        let mut c = Cadence::new(0);
        c.observe(
            Signals {
                active_sessions: false,
                remote_online: true,
                user_watching: false,
            },
            0,
        );
        c.observe(
            Signals {
                active_sessions: false,
                remote_online: false,
                user_watching: false,
            },
            10_000,
        );
        // 又出现远控 → 候选作废，保持 Idle
        let m = c.observe(
            Signals {
                active_sessions: false,
                remote_online: true,
                user_watching: false,
            },
            11_000,
        );
        assert_eq!(m, None);
        assert_eq!(c.mode(), Mode::Idle);
        // 若候选未在 11_000 作废，保留的旧候选会在 20_000 误触发 → 此处必须仍为 None
        let m = c.observe(
            Signals {
                active_sessions: false,
                remote_online: false,
                user_watching: false,
            },
            20_000,
        );
        assert_eq!(m, None);
        assert_eq!(c.mode(), Mode::Idle);
        // 20_000 才重新立项，21_000 尚未满一个 Idle 间隔
        let m = c.observe(
            Signals {
                active_sessions: false,
                remote_online: false,
                user_watching: false,
            },
            21_000,
        );
        assert_eq!(m, None);
        // 从重新立项起满 10_000ms → 切换 Away
        let m = c.observe(
            Signals {
                active_sessions: false,
                remote_online: false,
                user_watching: false,
            },
            30_000,
        );
        assert_eq!(m, Some(Mode::Away));
    }
}
