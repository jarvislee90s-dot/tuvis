//! 扫描节奏状态机（纯函数，无 IO）。
//! 输入四信号：活跃会话 / 远控在线 / 有人在看 / 未覆盖工具在跑；输出 2s / 10s / 30s。
//! 升档立即生效；降档需在候选状态稳定一个「当前档位间隔」后才切换（防抖）。
//!
//! 时钟约定（6.3）：时间参数一律是**单调时钟**（生产注入 `scan_bus::mono_now()`），
//! 类型 `Duration`。判定只读两次观测的差值，绝对时间基无关——墙上时间（系统时钟
//! 向前/向后调整、NTP 跳变）不参与任何降档/防抖判定，只用于日志与 UI 展示。
//! 采样语义：降档确认发生在越过边界的首次观测，观测间隔很长时下一次调用即按当时信号确认。
//!
//! Away 30s 的前提是「事件唤醒能兜住」：dsh/openclaw 没有事件覆盖（见
//! `scan_bus::UNCOVERED_TOOLS`），它们有进程在跑或有活跃会话时必须把下限锁在
//! Idle 10s，不允许进入 30s 无唤醒盲区（2026-10-31 用户裁决）。

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
    /// 无事件唤醒覆盖的工具（dsh/openclaw）有进程在跑或有活跃会话。
    /// 它们没有 hook / 廉价文件信号兜底，进入 Away 30s 会形成发现盲区，
    /// 故目标档位下限为 Idle 10s。
    pub uncovered_tool_running: bool,
}

impl Signals {
    fn target(&self) -> Mode {
        if self.active_sessions {
            Mode::Active
        } else if self.remote_online || self.user_watching || self.uncovered_tool_running {
            Mode::Idle
        } else {
            Mode::Away
        }
    }
}

pub struct Cadence {
    mode: Mode,
    candidate: Option<Mode>,
    /// 候选状态首次被观测到的单调时刻（与 `observe` 的 `now` 同一时间基）
    candidate_since: Duration,
}

impl Cadence {
    pub fn new(now: Duration) -> Self {
        // 启动先按 Idle（首扫完成后按真实信号收敛）。`candidate_since` 只是无害基线：
        // 真正的候选时刻由 `observe` 在立项时写入。
        Self {
            mode: Mode::Idle,
            candidate: None,
            candidate_since: now,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// 返回 Some(new_mode) 表示发生了切换。
    /// `now` 必须是单调时钟（`scan_bus::mono_now()`）；判定只比较差值，
    /// 因此绝对时间基整体平移（如墙上时钟跳变）不会改变任何决策。
    pub fn observe(&mut self, signals: Signals, now: Duration) -> Option<Mode> {
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
                if now.saturating_sub(self.candidate_since) >= self.mode.interval() {
                    self.mode = target;
                    self.candidate = None;
                    Some(target)
                } else {
                    None
                }
            }
            _ => {
                self.candidate = Some(target);
                self.candidate_since = now;
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试时间统一用单调毫秒（与生产 `mono_now()` 同量纲）
    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn sig(active: bool, remote: bool, watching: bool, uncovered: bool) -> Signals {
        Signals {
            active_sessions: active,
            remote_online: remote,
            user_watching: watching,
            uncovered_tool_running: uncovered,
        }
    }

    #[test]
    fn active_is_immediate_and_has_priority() {
        let mut c = Cadence::new(ms(0));
        let m = c.observe(sig(true, false, false, false), ms(1_000));
        assert_eq!(m, Some(Mode::Active));
        assert_eq!(c.mode(), Mode::Active);
        assert_eq!(c.mode().interval(), Duration::from_secs(2));
    }

    #[test]
    fn downgrade_requires_one_stable_interval() {
        let mut c = Cadence::new(ms(0));
        c.observe(sig(true, false, false, false), ms(0));
        // 降到 Idle：第一次只立项，不切换
        let m = c.observe(sig(false, true, false, false), ms(2_000));
        assert_eq!(m, None);
        assert_eq!(c.mode(), Mode::Active);
        // 边界前 1ms：1999ms < 2000ms，仍不切换
        let m = c.observe(sig(false, true, false, false), ms(3_999));
        assert_eq!(m, None);
        assert_eq!(c.mode(), Mode::Active);
        // 满一个 Active 间隔后切换
        let m = c.observe(sig(false, true, false, false), ms(4_000));
        assert_eq!(m, Some(Mode::Idle));
    }

    #[test]
    fn away_requires_no_remote_and_no_watcher() {
        let mut c = Cadence::new(ms(0));
        // Idle → Away 同样要防抖
        c.observe(sig(false, true, false, false), ms(0));
        c.observe(sig(false, false, false, false), ms(10_000));
        let m = c.observe(sig(false, false, false, false), ms(20_000));
        assert_eq!(m, Some(Mode::Away));
        assert_eq!(c.mode().interval(), Duration::from_secs(30));
        // 远控回来 → Away 升到 Idle 立即生效（升档无防抖）
        let m = c.observe(sig(false, true, false, false), ms(20_001));
        assert_eq!(m, Some(Mode::Idle));
        assert_eq!(c.mode(), Mode::Idle);
    }

    #[test]
    fn candidate_resets_when_signal_flaps_back() {
        let mut c = Cadence::new(ms(0));
        c.observe(sig(false, true, false, false), ms(0));
        c.observe(sig(false, false, false, false), ms(10_000));
        // 又出现远控 → 候选作废，保持 Idle
        let m = c.observe(sig(false, true, false, false), ms(11_000));
        assert_eq!(m, None);
        assert_eq!(c.mode(), Mode::Idle);
        // 若候选未在 11_000 作废，保留的旧候选会在 20_000 误触发 → 此处必须仍为 None
        let m = c.observe(sig(false, false, false, false), ms(20_000));
        assert_eq!(m, None);
        assert_eq!(c.mode(), Mode::Idle);
        // 20_000 才重新立项，21_000 尚未满一个 Idle 间隔
        let m = c.observe(sig(false, false, false, false), ms(21_000));
        assert_eq!(m, None);
        // 从重新立项起满 10_000ms → 切换 Away
        let m = c.observe(sig(false, false, false, false), ms(30_000));
        assert_eq!(m, Some(Mode::Away));
    }

    /// 6.3：完整降升档序列（全部 Duration 注入）——Active→Idle 与 Idle→Away
    /// 各需一个「当前档位间隔」的稳定期，升档立即生效。
    #[test]
    fn active_idle_away_cycle_requires_debounce_on_every_downgrade() {
        let mut c = Cadence::new(ms(0));
        assert_eq!(
            c.observe(sig(true, false, false, false), ms(0)),
            Some(Mode::Active)
        );
        // Active(2s) → Idle：2s 防抖
        assert_eq!(c.observe(sig(false, true, false, false), ms(2_000)), None);
        assert_eq!(c.observe(sig(false, true, false, false), ms(3_999)), None);
        assert_eq!(
            c.observe(sig(false, true, false, false), ms(4_000)),
            Some(Mode::Idle)
        );
        // Idle(10s) → Away：10s 防抖
        assert_eq!(c.observe(sig(false, false, false, false), ms(5_000)), None);
        assert_eq!(c.observe(sig(false, false, false, false), ms(14_999)), None);
        assert_eq!(
            c.observe(sig(false, false, false, false), ms(15_000)),
            Some(Mode::Away)
        );
        // 升档无防抖
        assert_eq!(
            c.observe(sig(true, false, false, false), ms(15_001)),
            Some(Mode::Active)
        );
    }

    /// 6.3：绝对时间基无关——整条时间线平移一个巨大常量（模拟墙上时钟跳变
    /// 后改用新的时间基）不改变任何决策；决策只依赖观测差值。
    #[test]
    fn constant_timeline_offset_does_not_change_decisions() {
        let run = |offset: Duration| {
            let mut c = Cadence::new(offset);
            let mut out = Vec::new();
            for (at, signals) in [
                (0u64, sig(true, false, false, false)),
                (2_000, sig(false, true, false, false)),
                (4_000, sig(false, true, false, false)),
                (15_000, sig(false, false, false, false)),
                (25_000, sig(false, false, false, false)),
                (25_001, sig(true, false, false, false)),
            ] {
                out.push(c.observe(signals, offset + ms(at)));
            }
            out
        };
        let baseline = run(ms(0));
        assert_eq!(
            baseline,
            vec![
                Some(Mode::Active),
                None,
                Some(Mode::Idle),
                None,
                Some(Mode::Away),
                Some(Mode::Active)
            ]
        );
        assert_eq!(
            run(ms(1_700_000_000_000)),
            baseline,
            "时间基整体平移不得改变降/升档判定"
        );
    }

    /// 6.3 回归锁：节奏判定不得读墙上时间。实现部分只允许 Duration 入参，
    /// 不允许出现系统时钟/纪元毫秒类型——新增代码若回退到墙上时间会在此失败。
    #[test]
    fn cadence_has_no_wall_clock_inputs() {
        let src = include_str!("cadence.rs");
        let impl_end = src.find("mod tests").expect("测试模块标记缺失");
        let body = &src[..impl_end];
        for banned in [
            "Utc::now",
            "SystemTime",
            "chrono",
            "timestamp_millis",
            "i64",
        ] {
            assert!(
                !body.contains(banned),
                "cadence 实现不得引用墙上时间/纪元毫秒：{banned}"
            );
        }
        assert!(body.contains("candidate_since: Duration"));
        assert!(body.contains("pub fn observe(&mut self, signals: Signals, now: Duration)"));
    }

    /// 2026-10-31 裁决：dsh/openclaw（无事件覆盖）在跑时不允许进入 Away 30s，
    /// 目标档位下限 Idle 10s；两者都不跑（信号为 false）时才允许 Away。
    #[test]
    fn uncovered_tool_running_blocks_away() {
        let mut c = Cadence::new(ms(0));
        // 先正常收敛到 Away（无远控、无人看、无活跃会话、无未覆盖工具）
        c.observe(sig(false, false, false, false), ms(0));
        let m = c.observe(sig(false, false, false, false), ms(10_000));
        assert_eq!(m, Some(Mode::Away));

        // dsh/openclaw 在跑 → Away 升回 Idle（升档立即生效）
        let uncovered = sig(false, false, false, true);
        let m = c.observe(uncovered, ms(10_001));
        assert_eq!(m, Some(Mode::Idle), "未覆盖工具在跑时不得停在 Away");
        // 持续在跑：稳定 Idle，不再尝试降档
        assert_eq!(c.observe(uncovered, ms(60_000)), None);
        assert_eq!(c.mode(), Mode::Idle);

        // 两者都不跑 → 才允许重新降 Away（一个 Idle 间隔防抖后）
        let idle = sig(false, false, false, false);
        assert_eq!(c.observe(idle, ms(70_000)), None);
        let m = c.observe(idle, ms(80_000));
        assert_eq!(m, Some(Mode::Away), "未覆盖工具全部退出后才允许 Away");
    }

    /// 未覆盖工具在跑时，Active 的降档目标是 Idle 而非 Away（不得跨档跳进盲区）。
    #[test]
    fn uncovered_tool_running_degrades_active_to_idle_not_away() {
        let mut c = Cadence::new(ms(0));
        assert_eq!(
            c.observe(sig(true, false, false, false), ms(0)),
            Some(Mode::Active)
        );
        let uncovered = sig(false, false, false, true);
        // 降档防抖：第一次只立项
        assert_eq!(c.observe(uncovered, ms(1_000)), None);
        // 满一个 Active 间隔 → Idle（不是 Away）
        assert_eq!(c.observe(uncovered, ms(3_000)), Some(Mode::Idle));
        assert_eq!(c.mode(), Mode::Idle);
        assert_eq!(c.mode().interval(), Duration::from_secs(10));
    }
}
