//! ZCode 自动连接编排（凭据复用设计 §5.2）。
//!
//! - 每次尝试都**现查**前置条件：远程总开关、用户自动连接意图、已保存链接、
//!   Hub 不在关闭/冲突/已配对/握手中；任一不满足就停，绝不自动 OAuth；
//! - 授权缺失时先经 [`ZcodeHub::restore_authorization`] 读系统存储，仍无效即停；
//!   **启动恢复只尝试一次**：助手已恢复过（成功/锁定/失败都算）后，worker 只读内存
//!   快照，不再第二次读原生存储；显式 `connect_requested` 仍可重试恢复；
//! - 连接只调用既有 [`ZcodeHub::connect`]，共用 `connect_lock` 与关闭纪元；
//!   本模块不调用 `execute`，不恢复任何写动作。
//!
//! 单 worker：`start_auto_connect` 幂等（`AtomicBool`），worker 只持 `Weak<ZcodeHub>`
//! 与一个 `Notify`；Hub 释放后自动退出。阶段 H 的有界网络恢复：首次立即尝试，仅
//! `offline` 才按 1s/3s/10s 退避（最多 3 次额外尝试）；等待用 `select!` 监听唤醒，
//! 新触发/暂停/remote 关闭/忘记/换链接（Hub 代际变化）立即取消旧轮次；配对成功重置
//! 预算，本模块自己产生的 offline 事件不重置预算、绝不触发新连接。
//!
//! 用户意图落在既有 settings 表（设计 §3.3，无 schema 迁移）：
//! `remote.zcode_auto_connect` 缺键 = true、非法值 = false；**读取失败按失败处理**
//! （不当作缺键），写入一律 checked，失败如实回报 `storage_failed`。
//!
//! 意图写按**单调代际**排序（P2-6）：用户写（授权成功/显式连接/显式断开/忘记）
//! 在 DB 锁内预占新代、恒为最新；KICKED 的延迟写在事件到达时预占一代，落库前
//! 代际已过期即跳过——迟到的暂停绝不覆盖用户后来的连接/断开意图。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use rusqlite::Connection;
use tokio::sync::Notify;

use crate::database::dao::settings::{try_get_setting_conn, try_set_setting_conn};
use crate::remote::pairing::DeviceStore;

use super::config::{get_config, ConfigState};
use super::hub::ZcodeHub;
use super::types::{codes, ConnectionState, ZResult, ZcodeError};

/// 自动连接用户意图（设计 §3.3）：`true` / `false`；缺键 = true，非法值 = false。
pub(crate) const AUTO_CONNECT_KEY: &str = "remote.zcode_auto_connect";

/// 自动连接前置条件（**每次尝试现查**，不是构造时快照）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AutoConnectGate {
    /// 远程总开关（`remote.enabled`）为 true；缺键/非法值均按关闭处理。
    pub(crate) remote_enabled: bool,
    /// 用户自动连接意图（缺键 = true，非法值 = false）。
    pub(crate) auto_intent: bool,
    /// 已保存格式合法的官方链接（`ConfigState::Saved`）。
    pub(crate) link_saved: bool,
}

impl AutoConnectGate {
    /// 三个条件全部满足才允许自动连接。
    pub(crate) fn allows(self) -> bool {
        self.remote_enabled && self.auto_intent && self.link_saved
    }
}

/// 连接级读取：`remote.enabled` / 自动连接意图 / 链接形态一次读完（同一个 DB 锁闭包，
/// 不嵌套 `DeviceStore::with`）。读取失败一律 `Err`——调用方按「不自动连接」处理。
fn read_gate_conn(conn: &Connection) -> rusqlite::Result<AutoConnectGate> {
    let remote_enabled = try_get_setting_conn(conn, crate::remote::KEY_ENABLED)?
        .map(|value| value == "true")
        .unwrap_or(false);
    let auto_intent = match try_get_setting_conn(conn, AUTO_CONNECT_KEY)? {
        None => true,
        Some(value) => value == "true",
    };
    Ok(AutoConnectGate {
        remote_enabled,
        auto_intent,
        link_saved: get_config(conn).config_state == ConfigState::Saved,
    })
}

/// 现查自动连接前置；`None` = settings 读取失败（安全侧：本次不自动连接）。
pub(crate) fn auto_connect_gate(store: &DeviceStore) -> Option<AutoConnectGate> {
    store.with(read_gate_conn).ok()
}

/// 持久化自动连接意图（checked write；失败不假报已保存）。
///
/// P2-6 起**仅供测试直写**：生产一律走 [`write_auto_intent`]，由它在 DB 锁内复核
/// 意图代际（迟到的 KICKED 写不得覆盖更新的用户意图）。
#[cfg(test)]
pub(crate) fn persist_auto_connect(store: &DeviceStore, enabled: bool) -> ZResult<()> {
    store
        .with(|conn| write_auto_connect_conn(conn, enabled))
        .map_err(|_| ZcodeError::of(codes::STORAGE_FAILED))
}

/// DB 锁内的裸写（调用方负责代际复核与 checked 错误映射）。
fn write_auto_connect_conn(conn: &Connection, enabled: bool) -> rusqlite::Result<()> {
    try_set_setting_conn(
        conn,
        AUTO_CONNECT_KEY,
        if enabled { "true" } else { "false" },
    )
}

/// 自动连接意图写入的代际凭据（P2-6）：所有意图写都带一个代，落库前在 DB 锁内
/// 复核「仍是最新代」。
///
/// 用户写（授权成功/显式连接/显式断开/忘记/连接直接返回冲突）用 [`Self::Latest`]：
/// 在 DB 锁内**预占新代**，因此恒为最新；KICKED 的延迟写用 [`Self::Captured`] 携带
/// 事件到达时预占的代际——用户随后的写会拿到更高代，迟到的 KICKED 写必须跳过。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutoIntentClaim {
    /// 用户意图写：落库时在 DB 锁内预占新代（此后为最新）。
    Latest,
    /// 延迟写（KICKED）：事件到达时预占的代际；落库前仍是当前代才写。
    Captured(u64),
}

/// 为一次延迟写预占新代（KICKED 事件到达时调用，先于任何后续用户写）。
pub(crate) fn capture_auto_intent(generation: &AtomicU64) -> AutoIntentClaim {
    AutoIntentClaim::Captured(generation.fetch_add(1, Ordering::SeqCst) + 1)
}

/// 统一意图写入口（P2-6）：**在 DB 锁内**复核代际，被更新的用户意图取代时跳过。
///
/// 返回值：`Ok(true)` = 已落库；`Ok(false)` = 代际已过期，主动跳过（不是错误）；
/// `Err` = 存储失败（用户写必须原样上抛，绝不假报已保存）。
///
/// 只靠 DB 互斥不足以保证顺序：KICKED 的 blocking 写可能在用户写之后才拿到锁，
/// 那时 DB 里已是用户的更新值——代际复核把它拦下。
pub(crate) fn write_auto_intent(
    store: &DeviceStore,
    generation: &AtomicU64,
    claim: AutoIntentClaim,
    enabled: bool,
) -> ZResult<bool> {
    store
        .with(|conn| {
            match claim {
                // 用户写：在本锁内预占新代——同一临界区里的后续写只会看到更高代。
                AutoIntentClaim::Latest => {
                    generation.fetch_add(1, Ordering::SeqCst);
                }
                // 延迟写：事件到达时的代已过期 ⇒ 用户更新过意图，跳过。
                AutoIntentClaim::Captured(expected) => {
                    if generation.load(Ordering::SeqCst) != expected {
                        return Ok(false);
                    }
                }
            }
            write_auto_connect_conn(conn, enabled).map(|_| true)
        })
        .map_err(|_| ZcodeError::of(codes::STORAGE_FAILED))
}

/// 失败退避档位（设计 §5.2：1s／3s／10s，最多 3 次额外尝试）。
const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(3),
    Duration::from_secs(10),
];

/// 一次网络故障轮次的重连预算（设计 §5.2）。
///
/// 只由单 worker 持有；`reset` 仅在配对成功或显式新触发（授权成功/换链接/重新开启
/// 远程）开新一轮时调用——**绝不**因本进程自己产生的 offline 事件重置。
#[derive(Debug, Default)]
struct ReconnectBudget {
    used: usize,
}

impl ReconnectBudget {
    /// 取下一次退避延迟；预算耗尽返回 `None`（本轮停止，等待显式触发）。
    fn next_delay(&mut self) -> Option<Duration> {
        let delay = *RETRY_DELAYS.get(self.used)?;
        self.used += 1;
        Some(delay)
    }

    /// 配对成功或显式新触发：恢复完整预算。
    fn reset(&mut self) {
        self.used = 0;
    }
}

/// 单次自动连接尝试的结果分类（只有 `Offline` 允许进入退避）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoAttempt {
    /// 已配对：连接成功。
    Paired,
    /// 网络/relay 不可达：唯一可重试的结果。
    Offline,
    /// 前置条件已不允许或不可重试（needs_auth/needs_config/conflict/unknown/暂停等）。
    Stopped,
    /// Hub 代际已推进（换/删链接、显式断开）：旧轮次立即作废，按新代际重开。
    Retargeted,
}

/// 一轮自动连接编排的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoRound {
    /// 本轮结束（成功/停机/预算耗尽）：等下一次显式唤醒。
    Done,
    /// 等待被新唤醒取消：立即按该触发开新一轮。
    Triggered,
}

impl ZcodeHub {
    /// 启动唯一自动连接 worker（幂等；构造 Hub 本身仍无自动连接副作用）。
    ///
    /// 必须在 Tokio runtime 内调用；worker 只持 `Weak`，Hub 释放即退出。
    /// 启动后先做一次尝试，随后由 [`Self::wake_auto_connect`] 逐次唤醒。
    pub fn start_auto_connect(self: &Arc<Self>) {
        if !self.auto_worker_begin() {
            return;
        }
        let hub = Arc::downgrade(self);
        let notify = self.auto_notify_handle();
        tokio::spawn(async move {
            drive_auto_connect(hub, notify).await;
        });
    }

    /// 唤醒自动连接 worker（启动恢复结束 / 授权成功 / 链接更新 / 远程总开关变化）。
    ///
    /// 未启动时只留一个唤醒许可（`Notify` 语义），`start` 后仍会先做一次尝试；
    /// 每次唤醒 = 一次**显式新触发**：取消正在等待的旧轮次并按新状态重开预算。
    /// 本模块绝不因自己产生的 offline 事件调用它。
    pub fn wake_auto_connect(&self) {
        self.auto_notify_handle().notify_one();
    }

    /// 单次自动连接尝试：条件每次现查，只调用既有 [`Self::connect`]。
    ///
    /// 顺序：内存暂停/代际 → 前置条件 → 恢复授权（不打开浏览器）→ 条件复核 →
    /// connect。`generation` 是本轮开始时绑定的 Hub 代际：等待/授权恢复期间换代
    /// （换/删链接、显式断开、KICKED）即作废本次尝试，绝不向旧目标发起连接。
    async fn auto_connect_attempt(&self, generation: u64) -> AutoAttempt {
        // 测试观察口：证明本次尝试已开始（负向用例据此等待 worker 跑完，不用 sleep）。
        #[cfg(test)]
        self.record_auto_attempt();
        if self.auto_connect_paused() {
            return AutoAttempt::Stopped;
        }
        if self.auto_connect_generation() != generation {
            return AutoAttempt::Retargeted;
        }
        let Some(gate) = auto_connect_gate(self.auto_connect_store()) else {
            return AutoAttempt::Stopped;
        };
        if !gate.allows() || !self.auto_connect_runtime_ready() {
            return AutoAttempt::Stopped;
        }
        // 启动恢复只尝试一次：助手已恢复过（成功/锁定/失败都算）就不再读原生存储，
        // 本次尝试只用内存快照；用户显式 connect 仍可经 Hub 重试恢复。
        let status = if self.auto_restore_attempted() {
            self.auth_status()
        } else {
            self.restore_authorization().await
        };
        if !status.authorized {
            // 恢复失败/无凭据：保持 needs_auth，绝不自动打开浏览器、不重试。
            return AutoAttempt::Stopped;
        }
        // await 期间条件可能翻转（用户断开/远程关闭/换链接/KICKED）：连接前全部现查。
        if self.auto_connect_paused() {
            return AutoAttempt::Stopped;
        }
        if self.auto_connect_generation() != generation {
            return AutoAttempt::Retargeted;
        }
        let Some(gate) = auto_connect_gate(self.auto_connect_store()) else {
            return AutoAttempt::Stopped;
        };
        if !gate.allows() || !self.auto_connect_runtime_ready() {
            return AutoAttempt::Stopped;
        }
        // 共用既有连接内核与 connect_lock（不新增第二连接器）。
        let result = self.connect().await;
        // 迟到结果若跨了代际：旧目标/旧轮次一律作废（connect 自己也会回 stale）。
        if self.auto_connect_generation() != generation {
            return AutoAttempt::Retargeted;
        }
        match result {
            Ok(view) if view.state == ConnectionState::Paired => AutoAttempt::Paired,
            Ok(view) if view.state == ConnectionState::Offline => AutoAttempt::Offline,
            Err(error) if error.is(codes::OFFLINE) => AutoAttempt::Offline,
            // needs_auth/needs_config/conflict/unknown（含 stale_generation）立即停。
            _ => AutoAttempt::Stopped,
        }
    }

    /// 一次故障轮次：首次立即尝试；仅 offline 才按 1s/3s/10s 退避，至多 3 次额外。
    ///
    /// 等待用 `select!` 同时监听 sleep 与唤醒：新触发/暂停/remote 关闭/忘记/换链接
    /// 都会取消旧轮次；等待后重查代际与前置，绝不向旧目标补连。
    async fn auto_connect_round(&self, notify: &Notify, budget: &mut ReconnectBudget) -> AutoRound {
        let mut generation = self.auto_connect_generation();
        loop {
            match self.auto_connect_attempt(generation).await {
                AutoAttempt::Paired => {
                    // 配对成功 = 本轮故障结束；下一次真实掉线（由显式触发重开）用新预算。
                    budget.reset();
                    return AutoRound::Done;
                }
                AutoAttempt::Stopped => return AutoRound::Done,
                AutoAttempt::Retargeted => {
                    // 换代是新的用户触发（换/删链接、显式断开）：立即按新代际重开，
                    // 不沿用旧目标的剩余预算。
                    budget.reset();
                    generation = self.auto_connect_generation();
                }
                AutoAttempt::Offline => {
                    // 等待前复核：已暂停/关闭/无授权/已配对时不再占着定时器。
                    if !self.auto_retry_allowed() {
                        return AutoRound::Done;
                    }
                    let Some(delay) = budget.next_delay() else {
                        // 预算耗尽：显示离线，等显式连接/授权/换链接/重开远程。
                        return AutoRound::Done;
                    };
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = notify.notified() => return AutoRound::Triggered,
                    }
                    // 等待后重查：换链接/关闭/冲突推进了代际 → 旧轮次作废。
                    let current = self.auto_connect_generation();
                    if current != generation {
                        budget.reset();
                        generation = current;
                        continue;
                    }
                    if !self.auto_retry_allowed() {
                        return AutoRound::Done;
                    }
                    // 下一轮循环的 attempt 会现查 gate/auth/runtime ready。
                }
            }
        }
    }

    /// 退避等待后的前置复核（与 attempt 入口同口径；不触发任何网络/存储）。
    fn auto_retry_allowed(&self) -> bool {
        if self.auto_connect_paused() || !self.auto_connect_runtime_ready() {
            return false;
        }
        auto_connect_gate(self.auto_connect_store())
            .map(|gate| gate.allows())
            .unwrap_or(false)
    }
}

/// 单 worker 驱动：先做一次启动尝试，之后每次唤醒开一轮（显式触发重置预算）。
async fn drive_auto_connect(hub: Weak<ZcodeHub>, notify: Arc<Notify>) {
    let mut budget = ReconnectBudget::default();
    let mut immediate = true;
    loop {
        if !immediate {
            notify.notified().await;
        }
        immediate = false;
        let Some(hub) = hub.upgrade() else {
            break;
        };
        // 每个显式触发都是新的故障轮次（显式连接/授权成功/链接更新/远程开启）。
        budget.reset();
        if hub.auto_connect_round(&notify, &mut budget).await == AutoRound::Triggered {
            // 唤醒许可已被本轮消费：立即开新一轮，不能退回外层等待（否则丢触发）。
            immediate = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::KEY_ENABLED;
    use crate::services::zcode_control::config::set_config;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    const URL: &str = "https://zcode.z.ai/remote/v4?sid=synthetic-auto-0001&name=SyntheticAuto";

    fn owned_store() -> DeviceStore {
        DeviceStore::memory()
    }

    fn stored(url: &str) -> DeviceStore {
        let store = DeviceStore::memory();
        store.with(|conn| set_config(conn, url).unwrap());
        store
    }

    fn set_setting(store: &DeviceStore, key: &str, value: &str) {
        store.with(|conn| try_set_setting_conn(conn, key, value).unwrap());
    }

    fn stored_value(store: &DeviceStore, key: &str) -> Option<String> {
        store.with(|conn| try_get_setting_conn(conn, key).unwrap())
    }

    /// 缺键 = true；显式 true/false 原样；非法值 = false；读取失败 = `None`（不得当缺键）。
    #[test]
    fn auto_intent_defaults_true_rejects_invalid_and_read_errors() {
        let store = owned_store();
        // settings 表存在但缺键。
        assert!(auto_connect_gate(&store).is_some());
        assert!(auto_connect_gate(&store).unwrap().auto_intent);

        set_setting(&store, AUTO_CONNECT_KEY, "false");
        assert!(!auto_connect_gate(&store).unwrap().auto_intent);
        set_setting(&store, AUTO_CONNECT_KEY, "true");
        assert!(auto_connect_gate(&store).unwrap().auto_intent);
        set_setting(&store, AUTO_CONNECT_KEY, "yes");
        assert!(
            !auto_connect_gate(&store).unwrap().auto_intent,
            "非法值按 false（不恢复即安全侧）"
        );

        // 无 settings 表 = 读取失败：不自动连接，绝不按缺键 true 处理。
        let broken = DeviceStore::Owned(Arc::new(Mutex::new(
            rusqlite::Connection::open_in_memory().unwrap(),
        )));
        assert!(auto_connect_gate(&broken).is_none(), "读取失败不得当作缺键");
        assert!(
            persist_auto_connect(&broken, true).is_err(),
            "checked 写失败必须如实回报"
        );
    }

    /// 总开关/自动意图/链接三者缺一不可；读取都走同一 checked 口径。
    #[test]
    fn gate_requires_remote_enabled_auto_intent_and_saved_link() {
        let store = owned_store();
        // 未保存链接 + 总开关缺键（关闭）→ 不允许。
        let gate = auto_connect_gate(&store).unwrap();
        assert!(!gate.remote_enabled && gate.auto_intent && !gate.link_saved);
        assert!(!gate.allows());

        store.with(|conn| set_config(conn, URL).unwrap());
        assert!(!auto_connect_gate(&store).unwrap().allows());

        set_setting(&store, KEY_ENABLED, "true");
        assert!(auto_connect_gate(&store).unwrap().allows());

        set_setting(&store, AUTO_CONNECT_KEY, "false");
        assert!(!auto_connect_gate(&store).unwrap().allows());

        set_setting(&store, AUTO_CONNECT_KEY, "true");
        assert!(auto_connect_gate(&store).unwrap().allows());

        // 非法总开关值同样按关闭处理。
        set_setting(&store, KEY_ENABLED, "1");
        assert!(!auto_connect_gate(&store).unwrap().allows());
    }

    /// checked 写入：true/false 都是显式两值，写成功真的可读回。
    #[test]
    fn persist_auto_connect_roundtrip() {
        let store = stored(URL);
        persist_auto_connect(&store, false).unwrap();
        assert_eq!(
            stored_value(&store, AUTO_CONNECT_KEY).as_deref(),
            Some("false")
        );
        persist_auto_connect(&store, true).unwrap();
        assert_eq!(
            stored_value(&store, AUTO_CONNECT_KEY).as_deref(),
            Some("true")
        );
    }

    // ------------------------------------------------------------------
    // P2-6：意图代际（用户写恒最新；KICKED 延迟写携带旧代必须跳过）
    // ------------------------------------------------------------------

    /// 事件到达时预占的旧代，在用户更新的 true 之后落库必须被跳过。
    #[test]
    fn captured_false_claim_is_skipped_after_newer_true_intent() {
        let store = stored(URL);
        let generation = AtomicU64::new(0);
        // KICKED 先到：预占一代（此代之后被用户写超越）。
        let kicked = capture_auto_intent(&generation);

        // 用户随后显式连接：预占更高代并落 true。
        assert!(
            write_auto_intent(&store, &generation, AutoIntentClaim::Latest, true).unwrap(),
            "用户写必须落库"
        );
        assert_eq!(
            stored_value(&store, AUTO_CONNECT_KEY).as_deref(),
            Some("true")
        );

        // 迟到的 KICKED 写携带旧代：必须跳过，不得把用户意图改回 false。
        let written = write_auto_intent(&store, &generation, kicked, false).unwrap();
        assert!(!written, "旧代际的 KICKED 延迟写必须被跳过");
        assert_eq!(
            stored_value(&store, AUTO_CONNECT_KEY).as_deref(),
            Some("true"),
            "用户更新的 true 必须保持"
        );
    }

    /// 镜像方向：旧代的 true 延迟写不得覆盖用户更新的 false。
    #[test]
    fn captured_true_claim_is_skipped_after_newer_false_intent() {
        let store = stored(URL);
        let generation = AtomicU64::new(0);
        let stale = capture_auto_intent(&generation);

        assert!(
            write_auto_intent(&store, &generation, AutoIntentClaim::Latest, false).unwrap(),
            "用户断开写必须落库"
        );
        assert_eq!(
            stored_value(&store, AUTO_CONNECT_KEY).as_deref(),
            Some("false")
        );

        let written = write_auto_intent(&store, &generation, stale, true).unwrap();
        assert!(!written, "旧代际的 true 延迟写必须被跳过");
        assert_eq!(
            stored_value(&store, AUTO_CONNECT_KEY).as_deref(),
            Some("false"),
            "用户更新的 false 必须保持"
        );
    }

    /// 事件晚于用户写：预占的代际最新，写入必须生效（KICKED 暂停不被误跳过）。
    #[test]
    fn captured_claim_newer_than_user_write_is_written() {
        let store = stored(URL);
        let generation = AtomicU64::new(0);
        write_auto_intent(&store, &generation, AutoIntentClaim::Latest, true).unwrap();

        let kicked = capture_auto_intent(&generation);
        assert!(
            write_auto_intent(&store, &generation, kicked, false).unwrap(),
            "事件晚于用户写时，KICKED 的 false 必须落库"
        );
        assert_eq!(
            stored_value(&store, AUTO_CONNECT_KEY).as_deref(),
            Some("false")
        );
    }

    /// 存储失败绝不因代际路径被吞掉：用户写与延迟写都如实回报。
    #[test]
    fn write_auto_intent_reports_storage_failure() {
        let broken = DeviceStore::Owned(Arc::new(Mutex::new(
            rusqlite::Connection::open_in_memory().unwrap(),
        )));
        let generation = AtomicU64::new(0);
        assert!(write_auto_intent(&broken, &generation, AutoIntentClaim::Latest, true).is_err());
        let claim = capture_auto_intent(&generation);
        assert!(write_auto_intent(&broken, &generation, claim, false).is_err());
    }

    /// 空转驱动不应残留任务或 panic；这里只验证 gate 在「读失败」时快速返回。
    #[tokio::test]
    async fn gate_read_failure_does_not_block_or_retry() {
        let broken = DeviceStore::Owned(Arc::new(Mutex::new(
            rusqlite::Connection::open_in_memory().unwrap(),
        )));
        assert!(auto_connect_gate(&broken).is_none());
        tokio::time::timeout(Duration::from_millis(50), async {
            // 模拟工作循环的一次判断：读失败直接返回，不 sleep/不重试。
            let Some(gate) = auto_connect_gate(&broken) else {
                return;
            };
            assert!(!gate.allows());
        })
        .await
        .expect("读失败必须立即返回");
    }

    // ------------------------------------------------------------------
    // H1：有界网络恢复（`tokio::time::start_paused` 虚拟时间，不真实等待 10s）
    // ------------------------------------------------------------------

    use crate::services::zcode_control::auth::{AuthStatus, OAuthProvider, OpenAuthorize};
    use crate::services::zcode_control::bridge::DataOutcome;
    use crate::services::zcode_control::hub::{BridgePump, HubBackend};
    use crate::services::zcode_control::relay::{DeviceRoute, RelayEvent, RelayLag};
    use crate::services::zcode_control::types::{
        ActionContext, ActionReceipt, BoundTarget, CapabilityView, ConnectionState, ConnectionView,
        ControlAction, ControlEventKind, SessionView, TargetKey,
    };
    use futures::future::BoxFuture;
    use serde_json::Value;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::mpsc;

    const URL_NEXT: &str =
        "https://zcode.z.ai/remote/v4?sid=synthetic-auto-0002&name=SyntheticAuto2";

    fn offline_view() -> ConnectionView {
        ConnectionView {
            state: ConnectionState::Offline,
            generation: 0,
            reason_code: Some(codes::OFFLINE.into()),
            device_label: None,
        }
    }

    fn paired_view() -> ConnectionView {
        ConnectionView {
            state: ConnectionState::Paired,
            generation: 0,
            reason_code: None,
            device_label: Some("synthetic-auto".into()),
        }
    }

    /// H1 fake：记录 connect 次数/目标与 execute 次数，按队列返回结果（默认 paired）。
    #[derive(Default)]
    struct FakeConnector {
        connect_calls: AtomicUsize,
        restore_calls: AtomicUsize,
        authorize_calls: AtomicUsize,
        execute_calls: AtomicUsize,
        disconnect_calls: AtomicUsize,
        reset_calls: AtomicUsize,
        routes: Mutex<Vec<String>>,
        results: Mutex<VecDeque<ZResult<ConnectionView>>>,
        auth: Mutex<AuthStatus>,
        /// 在途 `connect` 的闸（一次取走；并发触发用例验证单 owner）。
        connect_gate: Mutex<Option<Arc<Notify>>>,
    }

    impl FakeConnector {
        fn authorized() -> Self {
            Self {
                auth: Mutex::new(AuthStatus {
                    authorized: true,
                    remembered: true,
                    restoring: false,
                    provider: Some(OAuthProvider::Bigmodel),
                    problem: None,
                }),
                ..Self::default()
            }
        }

        fn queue(&self, results: Vec<ZResult<ConnectionView>>) {
            let mut queue = self.results.lock().unwrap();
            for result in results {
                queue.push_back(result);
            }
        }

        fn queue_offline(&self, count: usize) {
            let mut queue = self.results.lock().unwrap();
            for _ in 0..count {
                queue.push_back(Err(ZcodeError::offline()));
            }
        }

        fn gate_connect(&self) -> Arc<Notify> {
            let gate = Arc::new(Notify::new());
            *self.connect_gate.lock().unwrap() = Some(gate.clone());
            gate
        }

        fn connect_count(&self) -> usize {
            self.connect_calls.load(Ordering::SeqCst)
        }
    }

    impl HubBackend for FakeConnector {
        fn authorize<'a>(
            &'a self,
            _provider: OAuthProvider,
            _open: OpenAuthorize,
        ) -> BoxFuture<'a, ZResult<()>> {
            self.authorize_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(ZcodeError::of(codes::NEEDS_AUTH)) })
        }

        fn connect<'a>(&'a self, route: DeviceRoute) -> BoxFuture<'a, ZResult<ConnectionView>> {
            self.connect_calls.fetch_add(1, Ordering::SeqCst);
            self.routes.lock().unwrap().push(route.sid);
            let gate = self.connect_gate.lock().unwrap().take();
            let result = self
                .results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(paired_view()));
            Box::pin(async move {
                if let Some(gate) = gate {
                    gate.notified().await;
                }
                result
            })
        }

        fn disconnect<'a>(&'a self) -> BoxFuture<'a, ()> {
            self.disconnect_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
        }

        fn reset<'a>(&'a self, _generation: u64) -> BoxFuture<'a, ()> {
            self.reset_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
        }

        fn restore_authorization<'a>(&'a self) -> BoxFuture<'a, AuthStatus> {
            self.restore_calls.fetch_add(1, Ordering::SeqCst);
            let status = self.auth.lock().unwrap().clone();
            Box::pin(async move { status })
        }

        fn auth_status(&self) -> AuthStatus {
            self.auth.lock().unwrap().clone()
        }

        fn bind_session<'a>(&'a self, _key: &'a TargetKey) -> BoxFuture<'a, ZResult<BoundTarget>> {
            Box::pin(async { Err(ZcodeError::of(codes::NOT_READY)) })
        }

        fn session<'a>(&'a self, _target: &'a BoundTarget) -> BoxFuture<'a, ZResult<SessionView>> {
            Box::pin(async { Err(ZcodeError::of(codes::NOT_READY)) })
        }

        fn execute<'a>(
            &'a self,
            _ctx: &'a ActionContext,
            _target: &'a BoundTarget,
            _action: &'a ControlAction,
        ) -> BoxFuture<'a, ZResult<ActionReceipt>> {
            self.execute_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(ZcodeError::of(codes::NOT_READY)) })
        }

        fn policy(&self) -> CapabilityView {
            CapabilityView::default()
        }
    }

    /// 只用于接收冲突事件的空桥。
    struct TestPump;

    impl BridgePump for TestPump {
        fn on_data<'a>(&'a self, _payload: Value) -> BoxFuture<'a, DataOutcome> {
            Box::pin(async {
                DataOutcome {
                    result: Ok(()),
                    changed: Vec::new(),
                }
            })
        }

        fn invalidate<'a>(&'a self) -> BoxFuture<'a, ()> {
            Box::pin(async {})
        }
    }

    fn enable_remote(store: &DeviceStore, enabled: bool) {
        set_setting(store, KEY_ENABLED, if enabled { "true" } else { "false" });
    }

    async fn wait_until(mut condition: impl FnMut() -> bool) {
        for _ in 0..10_000 {
            if condition() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("条件在让步循环内未成立");
    }

    /// 让 worker 把当前可运行步骤跑完（不推进虚拟时间）。
    async fn settle_auto() {
        for _ in 0..256 {
            tokio::task::yield_now().await;
        }
    }

    /// 推进虚拟时间并让被唤醒的 worker 跑起来。
    async fn advance_and_settle(duration: Duration) {
        tokio::time::advance(duration).await;
        settle_auto().await;
    }

    #[test]
    fn retry_budget_is_finite() {
        let mut budget = ReconnectBudget::default();
        assert_eq!(budget.next_delay(), Some(Duration::from_secs(1)));
        assert_eq!(budget.next_delay(), Some(Duration::from_secs(3)));
        assert_eq!(budget.next_delay(), Some(Duration::from_secs(10)));
        assert_eq!(budget.next_delay(), None);
        // 新一轮（配对成功/显式触发）恢复完整预算。
        budget.reset();
        assert_eq!(budget.next_delay(), Some(Duration::from_secs(1)));
        assert_eq!(budget.next_delay(), Some(Duration::from_secs(3)));
        assert_eq!(budget.next_delay(), Some(Duration::from_secs(10)));
        assert_eq!(budget.next_delay(), None);
    }

    /// offline 才退避：首次立即尝试，之后严格 1s/3s，配对后停止且不再自动重连。
    #[tokio::test(start_paused = true)]
    async fn offline_then_paired_retries_with_virtual_backoff() {
        let fake = Arc::new(FakeConnector::authorized());
        fake.queue(vec![
            Err(ZcodeError::offline()),
            Err(ZcodeError::offline()),
            Ok(paired_view()),
        ]);
        let store = stored(URL);
        enable_remote(&store, true);
        let hub = Arc::new(ZcodeHub::with_parts(
            store,
            fake.clone(),
            CapabilityView::default(),
        ));

        hub.start_auto_connect();
        wait_until(|| fake.connect_count() == 1).await;
        assert!(!hub.connection().is_paired());

        advance_and_settle(Duration::from_millis(999)).await;
        assert_eq!(fake.connect_count(), 1, "1s 之前不得提前重试");
        advance_and_settle(Duration::from_millis(1)).await;
        wait_until(|| fake.connect_count() == 2).await;

        advance_and_settle(Duration::from_millis(2_999)).await;
        assert_eq!(fake.connect_count(), 2, "第二次退避必须是 3s");
        advance_and_settle(Duration::from_millis(1)).await;
        wait_until(|| fake.connect_count() == 3).await;
        wait_until(|| hub.connection().state == ConnectionState::Paired).await;

        advance_and_settle(Duration::from_secs(60)).await;
        assert_eq!(fake.connect_count(), 3, "paired 后不得继续自动重连");
        assert_eq!(
            fake.execute_calls.load(Ordering::SeqCst),
            0,
            "自动恢复只能连接，绝不重放动作"
        );
    }

    /// 自己的 offline 事件（每次失败都会广播）不得重置预算：一轮至多 4 次 connect。
    #[tokio::test(start_paused = true)]
    async fn own_offline_events_do_not_restart_budget() {
        let fake = Arc::new(FakeConnector::authorized());
        fake.queue_offline(8);
        let store = stored(URL);
        enable_remote(&store, true);
        let hub = Arc::new(ZcodeHub::with_parts(
            store,
            fake.clone(),
            CapabilityView::default(),
        ));
        let mut events = hub.subscribe();

        hub.start_auto_connect();
        wait_until(|| fake.connect_count() == 1).await;
        advance_and_settle(Duration::from_secs(1)).await;
        wait_until(|| fake.connect_count() == 2).await;
        advance_and_settle(Duration::from_secs(3)).await;
        wait_until(|| fake.connect_count() == 3).await;
        advance_and_settle(Duration::from_secs(10)).await;
        wait_until(|| fake.connect_count() == 4).await;

        advance_and_settle(Duration::from_secs(60)).await;
        assert_eq!(
            fake.connect_count(),
            4,
            "1 次首次 + 至多 3 次额外；offline 事件不得无限重置预算"
        );
        assert_eq!(hub.auto_attempt_count(), 4, "预算耗尽后不得再进入新的尝试");

        // 失败确实产生了 offline 连接事件（正是可能诱导无限重试的那类事件）。
        let mut offline_events = 0;
        while let Ok(event) = events.try_recv() {
            if event.kind == ControlEventKind::Connection
                && event.data["state"] == Value::String("offline".into())
            {
                offline_events += 1;
            }
        }
        assert!(offline_events >= 1, "每次失败都应广播 offline 连接事件");
    }

    /// KICKED / 手动暂停必须取消待重试轮次（旧定时器不得在新用户操作后复活）。
    #[tokio::test(start_paused = true)]
    async fn kicked_and_manual_pause_cancel_pending_retry() {
        // --- KICKED：冲突终态停止本轮，唤醒也不得抢回 ---
        let fake = Arc::new(FakeConnector::authorized());
        fake.queue_offline(8);
        let store = stored(URL);
        enable_remote(&store, true);
        let (events, receiver) = mpsc::channel(8);
        let hub = Arc::new(ZcodeHub::with_pump_and_lag(
            store.clone(),
            fake.clone(),
            CapabilityView::default(),
            receiver,
            Arc::new(TestPump),
            RelayLag::new(),
        ));
        hub.start_auto_connect();
        wait_until(|| fake.connect_count() == 1).await;
        advance_and_settle(Duration::from_secs(1)).await;
        wait_until(|| fake.connect_count() == 2).await;
        let attempts_before_conflict = hub.auto_attempt_count();

        events
            .send(RelayEvent::State(ConnectionView {
                state: ConnectionState::Conflict,
                generation: 7,
                reason_code: None,
                device_label: None,
            }))
            .await
            .unwrap();
        wait_until(|| hub.connection().state == ConnectionState::Conflict).await;
        // 取消是可观察的：worker 立即结束旧等待并重跑一次（被冲突/暂停挡下）。
        wait_until(|| hub.auto_attempt_count() > attempts_before_conflict).await;

        let calls_after_conflict = fake.connect_count();
        hub.wake_auto_connect();
        advance_and_settle(Duration::from_secs(30)).await;
        assert_eq!(
            fake.connect_count(),
            calls_after_conflict,
            "KICKED 后不得自动重连（含任意重启触发）"
        );

        // --- 手动暂停：持久 false + 取消待重试 ---
        let fake = Arc::new(FakeConnector::authorized());
        fake.queue_offline(8);
        let store = stored(URL);
        enable_remote(&store, true);
        let hub = Arc::new(ZcodeHub::with_parts(
            store.clone(),
            fake.clone(),
            CapabilityView::default(),
        ));
        hub.start_auto_connect();
        wait_until(|| fake.connect_count() == 1).await;
        advance_and_settle(Duration::from_secs(1)).await;
        wait_until(|| fake.connect_count() == 2).await;
        let attempts_before_pause = hub.auto_attempt_count();

        hub.disconnect_requested().await.unwrap();
        wait_until(|| hub.auto_attempt_count() > attempts_before_pause).await;
        assert_eq!(
            stored_value(&store, AUTO_CONNECT_KEY).as_deref(),
            Some("false"),
            "手动暂停必须持久 auto=false"
        );

        let calls_after_pause = fake.connect_count();
        // 重新打开门但**不**给新触发：若旧定时器仍在，它会在 3s 后发起连接。
        set_setting(&store, AUTO_CONNECT_KEY, "true");
        hub.resume_auto_connect();
        advance_and_settle(Duration::from_secs(3)).await;
        advance_and_settle(Duration::from_secs(10)).await;
        advance_and_settle(Duration::from_secs(60)).await;
        assert_eq!(
            fake.connect_count(),
            calls_after_pause,
            "手动暂停必须取消旧重试定时器（不得向旧目标补连）"
        );
    }

    /// 自动恢复只做 connect：重连全程零 execute（不重放任何写动作）。
    ///
    /// 这里用 `Ok(offline)`（结果状态确为 offline）触发重试，与 `Err(offline)` 的
    /// 错误码路径一起覆盖设计 §5.2 的两种离线形态。
    #[tokio::test(start_paused = true)]
    async fn reconnect_never_replays_actions() {
        let fake = Arc::new(FakeConnector::authorized());
        fake.queue(vec![Ok(offline_view()), Ok(paired_view())]);
        let store = stored(URL);
        enable_remote(&store, true);
        let hub = Arc::new(ZcodeHub::with_parts(
            store,
            fake.clone(),
            CapabilityView::default(),
        ));

        hub.start_auto_connect();
        wait_until(|| fake.connect_count() == 1).await;
        advance_and_settle(Duration::from_secs(1)).await;
        wait_until(|| hub.connection().state == ConnectionState::Paired).await;

        assert_eq!(
            fake.execute_calls.load(Ordering::SeqCst),
            0,
            "自动恢复绝不调用 execute/重放动作"
        );
        assert_eq!(fake.connect_count(), 2);
    }

    /// 换链接 = Hub 代际变化：取消旧轮次，立即改连新目标，不再向旧目标重试。
    #[tokio::test(start_paused = true)]
    async fn link_change_cancels_pending_retry_and_connects_new_target() {
        let fake = Arc::new(FakeConnector::authorized());
        fake.queue(vec![Err(ZcodeError::offline()), Ok(paired_view())]);
        let store = stored(URL);
        enable_remote(&store, true);
        let hub = Arc::new(ZcodeHub::with_parts(
            store.clone(),
            fake.clone(),
            CapabilityView::default(),
        ));

        hub.start_auto_connect();
        wait_until(|| fake.connect_count() == 1).await;

        store.with(|conn| set_config(conn, URL_NEXT).unwrap());
        hub.config_changed().await;
        hub.wake_auto_connect();

        // 不推进虚拟时间：新触发必须立即取消旧 1s 等待并改连新目标。
        wait_until(|| fake.connect_count() == 2).await;
        wait_until(|| hub.connection().state == ConnectionState::Paired).await;
        {
            let routes = fake.routes.lock().unwrap();
            assert_eq!(routes.len(), 2, "旧定时器不得再向旧目标发起连接");
            assert_eq!(routes[0], "synthetic-auto-0001");
            assert_eq!(routes[1], "synthetic-auto-0002");
        }
        advance_and_settle(Duration::from_secs(60)).await;
        assert_eq!(fake.connect_count(), 2);
    }

    /// 远程关闭取消待重试；旧定时器不得在重新开启门后偷偷连接。
    #[tokio::test(start_paused = true)]
    async fn remote_close_cancels_pending_retry() {
        let fake = Arc::new(FakeConnector::authorized());
        fake.queue_offline(8);
        let store = stored(URL);
        enable_remote(&store, true);
        let hub = Arc::new(ZcodeHub::with_parts(
            store.clone(),
            fake.clone(),
            CapabilityView::default(),
        ));
        hub.start_auto_connect();
        wait_until(|| fake.connect_count() == 1).await;
        advance_and_settle(Duration::from_secs(1)).await;
        wait_until(|| fake.connect_count() == 2).await;

        let attempts_before_close = hub.auto_attempt_count();
        enable_remote(&store, false);
        hub.wake_auto_connect();
        wait_until(|| hub.auto_attempt_count() > attempts_before_close).await;
        let calls_after_close = fake.connect_count();

        // 重新开启总开关但不给新触发：陈旧定时器不得复活。
        enable_remote(&store, true);
        advance_and_settle(Duration::from_secs(3)).await;
        advance_and_settle(Duration::from_secs(10)).await;
        advance_and_settle(Duration::from_secs(60)).await;
        assert_eq!(
            fake.connect_count(),
            calls_after_close,
            "远程关闭必须取消旧轮次，不得向仍保存的链接补连"
        );
    }

    /// needs_auth 立即停：不进入退避、不打开浏览器、不重复读存储。
    #[tokio::test(start_paused = true)]
    async fn needs_auth_stops_without_retry() {
        let fake = Arc::new(FakeConnector::default());
        let store = stored(URL);
        enable_remote(&store, true);
        let hub = Arc::new(ZcodeHub::with_parts(
            store,
            fake.clone(),
            CapabilityView::default(),
        ));

        hub.start_auto_connect();
        wait_until(|| fake.restore_calls.load(Ordering::SeqCst) == 1).await;
        settle_auto().await;
        assert_eq!(fake.connect_count(), 0);
        assert_eq!(
            fake.authorize_calls.load(Ordering::SeqCst),
            0,
            "绝不由自动路径打开浏览器"
        );
        assert_eq!(hub.auto_attempt_count(), 1, "needs_auth 立即停，不进入退避");
        advance_and_settle(Duration::from_secs(60)).await;
        assert_eq!(fake.connect_count(), 0);
        assert_eq!(hub.auto_attempt_count(), 1);
        assert_eq!(fake.restore_calls.load(Ordering::SeqCst), 1);
    }

    /// 并发唤醒（连接在途时）不得开出第二个连接器：单 worker + connect_lock 单 owner。
    #[tokio::test(start_paused = true)]
    async fn concurrent_wakes_keep_single_owner_and_one_connection() {
        let fake = Arc::new(FakeConnector::authorized());
        let gate = fake.gate_connect();
        let store = stored(URL);
        enable_remote(&store, true);
        let hub = Arc::new(ZcodeHub::with_parts(
            store,
            fake.clone(),
            CapabilityView::default(),
        ));

        hub.start_auto_connect();
        wait_until(|| fake.connect_count() == 1).await;
        hub.wake_auto_connect();
        hub.wake_auto_connect();
        settle_auto().await;
        assert_eq!(
            fake.connect_count(),
            1,
            "在途连接期间的唤醒不得开第二条连接"
        );

        gate.notify_one();
        wait_until(|| hub.connection().state == ConnectionState::Paired).await;
        advance_and_settle(Duration::from_secs(60)).await;
        assert_eq!(fake.connect_count(), 1);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);
    }

    /// 真实掉线（覆盖前 `paired`、新状态 `offline`）唤醒新一轮；worker 自身失败
    /// （覆盖前 `connecting`/`offline`）绝不唤醒、绝不重置预算。
    #[tokio::test(start_paused = true)]
    async fn real_drop_opens_new_round_but_worker_failure_does_not() {
        let fake = Arc::new(FakeConnector::authorized());
        fake.queue(vec![
            Ok(paired_view()),          // 初始连接
            Err(ZcodeError::offline()), // 真实掉线后的第一次恢复（被闸停住）
            Err(ZcodeError::offline()), // 新轮 1s 档
            Err(ZcodeError::offline()), // 新轮 3s 档
            Ok(paired_view()),          // 新轮 10s 档
        ]);
        let store = stored(URL);
        enable_remote(&store, true);
        let (events, receiver) = mpsc::channel(8);
        let hub = Arc::new(ZcodeHub::with_pump_and_lag(
            store.clone(),
            fake.clone(),
            CapabilityView::default(),
            receiver,
            Arc::new(TestPump),
            RelayLag::new(),
        ));
        hub.start_auto_connect();
        wait_until(|| hub.connection().state == ConnectionState::Paired).await;
        assert_eq!(fake.connect_count(), 1);
        let attempts_paired = hub.auto_attempt_count();

        // 真实掉线：事件泵覆盖前是 paired，新状态 offline。
        let gate = fake.gate_connect();
        events
            .send(RelayEvent::State(ConnectionView {
                state: ConnectionState::Offline,
                generation: 2,
                reason_code: Some(codes::OFFLINE.into()),
                device_label: None,
            }))
            .await
            .unwrap();
        wait_until(|| fake.connect_count() == 2).await;
        assert_eq!(
            hub.connection().state,
            ConnectionState::Connecting,
            "真实掉线唤醒的 attempt 必须已在握手中"
        );
        let attempts_after_drop = hub.auto_attempt_count();
        assert!(
            attempts_after_drop > attempts_paired,
            "真实掉线必须开新一轮"
        );

        // worker 自身失败的形态：connecting → offline（覆盖前不是 paired）不得唤醒。
        events
            .send(RelayEvent::State(ConnectionView {
                state: ConnectionState::Offline,
                generation: 3,
                reason_code: Some(codes::OFFLINE.into()),
                device_label: None,
            }))
            .await
            .unwrap();
        wait_until(|| hub.connection().state == ConnectionState::Offline).await;
        settle_auto().await;
        assert_eq!(
            hub.auto_attempt_count(),
            attempts_after_drop,
            "worker 自身失败的 offline 不得开新轮"
        );
        assert_eq!(fake.connect_count(), 2);

        gate.notify_one();
        settle_auto().await;
        // 真实掉线开的新轮从 1s 首档重算；沿用旧轮剩余预算会变成 3s。
        advance_and_settle(Duration::from_millis(999)).await;
        assert_eq!(fake.connect_count(), 2, "1s 之前不得重试");
        advance_and_settle(Duration::from_millis(1)).await;
        wait_until(|| fake.connect_count() == 3).await;

        // worker 自身失败落定后的 offline 事件（覆盖前已是 offline）同样不得唤醒。
        let attempts_after_failure = hub.auto_attempt_count();
        events
            .send(RelayEvent::State(ConnectionView {
                state: ConnectionState::Offline,
                generation: 4,
                reason_code: Some(codes::OFFLINE.into()),
                device_label: None,
            }))
            .await
            .unwrap();
        settle_auto().await;
        assert_eq!(hub.auto_attempt_count(), attempts_after_failure);
        assert_eq!(fake.connect_count(), 3);

        advance_and_settle(Duration::from_millis(2_999)).await;
        assert_eq!(fake.connect_count(), 3, "第二次退避必须是 3s");
        advance_and_settle(Duration::from_millis(1)).await;
        wait_until(|| fake.connect_count() == 4).await;
        advance_and_settle(Duration::from_millis(9_999)).await;
        assert_eq!(fake.connect_count(), 4, "第三次退避必须是 10s");
        advance_and_settle(Duration::from_millis(1)).await;
        wait_until(|| fake.connect_count() == 5).await;
        wait_until(|| hub.connection().state == ConnectionState::Paired).await;

        advance_and_settle(Duration::from_secs(60)).await;
        assert_eq!(fake.connect_count(), 5, "恢复配对后不得再自动重连");
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);
    }

    /// 显式 connect 的 offline 失败 = 新用户触发：立即取消旧退避，预算从 1s 首档重开。
    #[tokio::test(start_paused = true)]
    async fn explicit_connect_offline_restarts_retry_budget() {
        let fake = Arc::new(FakeConnector::authorized());
        fake.queue(vec![
            Err(ZcodeError::offline()), // worker 首次
            Err(ZcodeError::offline()), // 用户 connect_requested
            Err(ZcodeError::offline()), // 被唤醒后的新一轮首次（立即）
            Ok(paired_view()),          // 新预算 1s 档
        ]);
        let store = stored(URL);
        enable_remote(&store, true);
        let hub = Arc::new(ZcodeHub::with_parts(
            store,
            fake.clone(),
            CapabilityView::default(),
        ));

        hub.start_auto_connect();
        wait_until(|| fake.connect_count() == 1).await;

        let error = hub.connect_requested().await.unwrap_err();
        assert!(error.is(codes::OFFLINE));
        // 不推进虚拟时间：旧 1s 等待必须被取消，worker 立即开新一轮（第 3 次 connect）。
        wait_until(|| fake.connect_count() == 3).await;
        assert_eq!(hub.connection().state, ConnectionState::Offline);

        // 预算已重置：新一轮的下一档是 1s（若沿用旧轮次则是 3s）。
        advance_and_settle(Duration::from_millis(999)).await;
        assert_eq!(fake.connect_count(), 3, "新轮首档 1s 之前不得重试");
        advance_and_settle(Duration::from_millis(1)).await;
        wait_until(|| fake.connect_count() == 4).await;
        wait_until(|| hub.connection().state == ConnectionState::Paired).await;
        advance_and_settle(Duration::from_secs(60)).await;
        assert_eq!(fake.connect_count(), 4);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);
    }

    /// 显式 connect 成功不额外唤醒：一次点击只增加一次 backend connect。
    #[tokio::test(start_paused = true)]
    async fn explicit_connect_success_does_not_extra_wake() {
        let fake = Arc::new(FakeConnector::authorized());
        fake.queue(vec![Err(ZcodeError::offline()), Ok(paired_view())]);
        let store = stored(URL);
        enable_remote(&store, true);
        let hub = Arc::new(ZcodeHub::with_parts(
            store,
            fake.clone(),
            CapabilityView::default(),
        ));

        hub.start_auto_connect();
        wait_until(|| fake.connect_count() == 1).await;

        let view = hub.connect_requested().await.unwrap();
        assert_eq!(view.state, ConnectionState::Paired);
        assert_eq!(fake.connect_count(), 2);
        settle_auto().await;
        assert_eq!(
            fake.connect_count(),
            2,
            "成功不得额外唤醒第二个 backend connect"
        );
        advance_and_settle(Duration::from_secs(60)).await;
        assert_eq!(fake.connect_count(), 2);
    }
}
