//! TPS 调度器（Spec §2 固定采样规则的状态机面；2026-10-24 计划 Task 7）。
//!
//! ## 纪律
//!
//! * **纯状态机**：无 IO、无时钟、无全局态——时间一律由入参注入（Spec §7「用假时钟
//!   测试，不真等 1 小时」）；tokio/线程循环在 collector 的运行接线里，本模块**不含
//!   任何 timer**（后台模式没有 5 秒 timer 的可执行证据就在这里：任何动作都只在
//!   `tick` 被调用且间隔锚点到期时发出）。
//! * **绝不回填过去的点**：锚点是内存态，Tuvis 没跑的时间段没有 tick，就没有点
//!   （Spec §2「曲线不填补 Tuvis 退出期间不存在的采样点」）；每个 tick 至多发一个
//!   Scheduled 点——缺失的计划点直接缺失（Spec §2「缺失的计划采样点均断线」）。
//! * **视图租约**（Spec §2「多窗口通过 view_id 租约计数，心跳30秒、租约90秒，只有
//!   窗口与TPS区块实际可见才续约」）：可见性由前端决定，本层只数**活租约**；
//!   `heartbeat(view_id, visible=false)` 立即交还租约，`tick` 剪掉过期租约——已退出
//!   的视图不可能永远维持前台。
//! * **runId 生命周期**：构造时给一段 runId；总授权关闭→重开必须换新 runId
//!   （Spec §2「重新开启启动新 runId 采样段，不能跨关闭期连线」）。runId 的**生成**
//!   （随机/时间戳）在 collector 的运行接线（[`crate::services::tps::collector::
//!   generate_run_id`]），本模块只持串——保持纯函数可测。
//!
//! ## 间隔常量（Spec §2 逐字）
//!
//! | 规则 | 常量 | 值 |
//! |---|---|---|
//! | 后台读取/采样点 | [`BACKGROUND_INTERVAL_MS`] | 600_000 |
//! | 可见期增量读取 | [`VISIBLE_READ_INTERVAL_MS`] | 5_000 |
//! | 可见期曲线点 | [`VISIBLE_POINT_INTERVAL_MS`] | 60_000 |
//! | manual 最小间隔 | [`MANUAL_MIN_INTERVAL_MS`] | 5_000 |
//! | 租约有效期 | [`LEASE_TTL_MS`] | 90_000 |

use std::collections::BTreeMap;

use crate::services::tps::model::PointTrigger;

/// 后台节奏（无可见视图）：每 600 秒一次有界读取 + 一个 Scheduled 采样点
pub const BACKGROUND_INTERVAL_MS: i64 = 600_000;
/// 可见期增量读取间隔
pub const VISIBLE_READ_INTERVAL_MS: i64 = 5_000;
/// 可见期曲线点间隔
pub const VISIBLE_POINT_INTERVAL_MS: i64 = 60_000;
/// manual 刷新最小间隔（Spec §2「立即刷新读取并生成一个manual点，最小间隔5秒」）
pub const MANUAL_MIN_INTERVAL_MS: i64 = 5_000;
/// 视图租约有效期（心跳 30 秒续一次，租 90 秒；Spec §2）
pub const LEASE_TTL_MS: i64 = 90_000;

/// 调度模式（brief 锚点：租约过期后 `mode == SamplerMode::Background`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SamplerMode {
    /// 无可见视图（也无活租约）：600 秒节奏
    Background,
    /// 至少一个视图可见（入参 visible 或活租约）：5s/60s 节奏
    Foreground,
    /// 用量采集总授权关闭（复用 usage.enabled，不设第二开关）：停调度
    Disabled,
}

/// 某模式下的计划出点节奏（可见 60 秒 / 其余 600 秒）——点随身携带，前端判缺计划点用
pub fn point_cadence_ms(mode: SamplerMode) -> i64 {
    match mode {
        SamplerMode::Foreground => VISIBLE_POINT_INTERVAL_MS,
        _ => BACKGROUND_INTERVAL_MS,
    }
}

/// 一个 tick 发出的动作（运行接线消费：Read → 采集轮；EmitPoint → 建点落库）
#[derive(Debug, Clone, PartialEq)]
pub enum TickAction {
    /// 执行一次**有界**增量读取（采集轮）
    Read,
    /// 在 at_ms 生成全部 series 的采样点（trigger = startup|scheduled|manual）
    /// 建点；`cadence_ms` = 发点时生效的计划节奏（可见 60000 / 后台 600000，独立复核 R11），
    /// startup / manual 点同样带当时模式的节奏，前端据此判定缺失的计划采样点
    EmitPoint {
        at_ms: i64,
        trigger: PointTrigger,
        cadence_ms: i64,
    },
}

/// 一个 tick 的完整产物（模式 + 到期动作）
#[derive(Debug, Clone, PartialEq)]
pub struct TickOutcome {
    pub mode: SamplerMode,
    pub actions: Vec<TickAction>,
}

/// 调度器状态机。锁与运行循环在 collector（[`crate::services::tps::collector`]），
/// 本结构自身不带同步原语。
#[derive(Debug)]
pub struct Sampler {
    run_id: String,
    enabled: bool,
    /// 启用/重开后的第一次 tick 先采一次（Spec §2「启动后先采一次」）
    startup_pending: bool,
    /// view_id → 租约到期时刻（最后心跳 + [`LEASE_TTL_MS`]）
    leases: BTreeMap<String, i64>,
    /// 最近一次读取锚点（背景/前台共用：模式切换从既有锚点顺延，不补发）
    last_read_ms: Option<i64>,
    /// 最近一次 Scheduled 点锚点（manual 点**不**动它：manual 与计划节奏互相独立）
    last_point_ms: Option<i64>,
    /// 最近一次 manual 放行时刻（限频锚点）
    last_manual_ms: Option<i64>,
}

impl Sampler {
    /// 构造（`enabled` = 用量采集总授权的当前值）。
    pub fn new(run_id: impl Into<String>, enabled: bool) -> Self {
        Self {
            run_id: run_id.into(),
            enabled,
            startup_pending: enabled,
            leases: BTreeMap::new(),
            last_read_ms: None,
            last_point_ms: None,
            last_manual_ms: None,
        }
    }

    /// 当前采样段 id（点落库时由运行接线取用；跨关闭期不连线由「重开必换」保证）
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// 只读的模式判定（不剪租约、不推进锚点——查询面/命令面用；tick 的产物才是权威）
    pub fn mode_at(&self, now_ms: i64) -> SamplerMode {
        if !self.enabled {
            return SamplerMode::Disabled;
        }
        let lease_alive = self.leases.values().any(|expiry| *expiry > now_ms);
        if lease_alive {
            SamplerMode::Foreground
        } else {
            SamplerMode::Background
        }
    }

    /// 总授权翻转（运行接线每步调用；**只在边沿**动作）：
    /// * false→true：换新 runId + 预约 Startup 点 + 清租约（关闭期不存在可见视图）；
    /// * true→false：停调度（模式 Disabled、动作清空），历史不动；
    /// * 已启用时的重复 enable 是无操作（运行循环每秒调用本函数，不得反复换 runId）。
    pub fn set_enabled(&mut self, enabled: bool, new_run_id: &str) {
        if enabled == self.enabled {
            return;
        }
        self.enabled = enabled;
        if enabled {
            self.run_id = new_run_id.to_string();
            self.startup_pending = true;
            self.leases.clear();
            // 锚点保留：重开的第一批动作由 Startup 路径即时发出，600s/60s 锚点从
            // Startup 时刻顺延——不回填关闭期的任何点
        }
    }

    /// 视图心跳（Spec §2「心跳30秒、租约90秒，只有窗口与TPS区块实际可见才续约」）：
    /// * `visible=true`：续租到 now + [`LEASE_TTL_MS`]；
    /// * `visible=false`：立即交还租约（前端决定可见性，隐藏即交）。
    pub fn heartbeat(&mut self, view_id: &str, visible: bool, now_ms: i64) {
        if visible {
            self.leases
                .insert(view_id.to_string(), now_ms + LEASE_TTL_MS);
        } else {
            self.leases.remove(view_id);
        }
    }

    /// manual 刷新（Spec §2「立即刷新读取并生成一个manual点，最小间隔5秒」）。
    /// 限频窗口内 → None（IPC 面回 refreshed=false + reason）。
    pub fn manual_refresh(&mut self, now_ms: i64) -> Option<Vec<TickAction>> {
        if !self.enabled {
            return None;
        }
        if let Some(t) = self.last_manual_ms {
            if now_ms - t < MANUAL_MIN_INTERVAL_MS {
                return None;
            }
        }
        let cadence_ms = point_cadence_ms(self.mode_at(now_ms));
        self.last_manual_ms = Some(now_ms);
        Some(vec![
            TickAction::Read,
            TickAction::EmitPoint {
                at_ms: now_ms,
                trigger: PointTrigger::Manual,
                cadence_ms,
            },
        ])
    }

    /// 推进一步（运行接线的节拍调用；纯函数——假时钟测试逐毫秒推进）。
    ///
    /// 前台判定 = 入参 visible ∨ 存在活租约（先剪掉 `now >= 到期` 的过期租约）。
    /// 每个 tick 至多发一个读取 + 一个 Scheduled 点：不补发、不追赶（缺失的计划点
    /// 直接缺失，Spec §2「缺失的计划采样点均断线」）。
    pub fn tick(&mut self, now_ms: i64, visible: bool) -> TickOutcome {
        // 1. 剪掉过期租约（到期时刻 ≤ now 即过期——租 90 秒，第 90 秒整已不活）
        self.leases.retain(|_, expiry| *expiry > now_ms);
        let foreground = visible || !self.leases.is_empty();
        let mode = if !self.enabled {
            SamplerMode::Disabled
        } else if foreground {
            SamplerMode::Foreground
        } else {
            SamplerMode::Background
        };

        let mut actions = Vec::new();
        if !self.enabled {
            return TickOutcome { mode, actions };
        }

        // 2. 启用/重开后的第一次 tick：先采一次（读取 + Startup 点）
        if self.startup_pending {
            self.startup_pending = false;
            self.last_read_ms = Some(now_ms);
            self.last_point_ms = Some(now_ms);
            actions.push(TickAction::Read);
            actions.push(TickAction::EmitPoint {
                at_ms: now_ms,
                trigger: PointTrigger::Startup,
                cadence_ms: point_cadence_ms(mode),
            });
        }

        // 3. 到期的节奏动作（锚点共用：模式切换顺延，不补发过去的点）
        let (read_gap, point_gap) = match mode {
            SamplerMode::Foreground => (VISIBLE_READ_INTERVAL_MS, VISIBLE_POINT_INTERVAL_MS),
            _ => (BACKGROUND_INTERVAL_MS, BACKGROUND_INTERVAL_MS),
        };
        let read_due = self.last_read_ms.is_none_or(|t| now_ms - t >= read_gap);
        let point_due = self.last_point_ms.is_none_or(|t| now_ms - t >= point_gap);
        if read_due {
            self.last_read_ms = Some(now_ms);
            actions.push(TickAction::Read);
        }
        if point_due {
            self.last_point_ms = Some(now_ms);
            actions.push(TickAction::EmitPoint {
                at_ms: now_ms,
                trigger: PointTrigger::Scheduled,
                cadence_ms: point_gap,
            });
        }
        TickOutcome { mode, actions }
    }
}

// ==== 以下为 Task 7 Step 1 的假时钟红测试（brief 锚点逐字） ====
#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_700_000_000_000;

    /// 同一 tick 内 due 判定用「>=」（间隔恰好等于阈值即到期）——锁语义防回归
    #[test]
    fn interval_boundary_is_inclusive() {
        let mut s = Sampler::new("run", true);
        s.tick(T0, false); // Startup
        let out = s.tick(T0 + VISIBLE_POINT_INTERVAL_MS, true);
        assert!(out.actions.iter().any(|a| matches!(
            a,
            TickAction::EmitPoint {
                trigger: PointTrigger::Scheduled,
                ..
            }
        )));
    }
}
