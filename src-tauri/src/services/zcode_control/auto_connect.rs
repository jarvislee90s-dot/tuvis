//! ZCode 自动连接编排（凭据复用设计 §5.2）。
//!
//! 本模块只做**一次**启动恢复/一次连接（阶段 C3）：
//! - 每次尝试都**现查**前置条件：远程总开关、用户自动连接意图、已保存链接、
//!   Hub 不在关闭/冲突/已配对/握手中；任一不满足就停，绝不自动 OAuth；
//! - 授权缺失时先经 [`ZcodeHub::restore_authorization`] 读系统存储，仍无效即停；
//! - 连接只调用既有 [`ZcodeHub::connect`]，共用 `connect_lock` 与关闭纪元；
//!   本模块不调用 `execute`，不恢复任何写动作。
//!
//! 单 worker：`start_auto_connect` 幂等（`AtomicBool`），worker 只持 `Weak<ZcodeHub>`
//! 与一个 `Notify`；Hub 释放后自动退出。阶段 H 才补 1s/3s/10s 网络退避，C3 不重试。
//!
//! 用户意图落在既有 settings 表（设计 §3.3，无 schema 迁移）：
//! `remote.zcode_auto_connect` 缺键 = true、非法值 = false；**读取失败按失败处理**
//! （不当作缺键），写入一律 checked，失败如实回报 `storage_failed`。

use std::sync::{Arc, Weak};

use rusqlite::Connection;
use tokio::sync::Notify;

use crate::database::dao::settings::{try_get_setting_conn, try_set_setting_conn};
use crate::remote::pairing::DeviceStore;

use super::config::{get_config, ConfigState};
use super::hub::ZcodeHub;
use super::types::{codes, ZResult, ZcodeError};

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
pub(crate) fn persist_auto_connect(store: &DeviceStore, enabled: bool) -> ZResult<()> {
    store
        .with(|conn| {
            try_set_setting_conn(
                conn,
                AUTO_CONNECT_KEY,
                if enabled { "true" } else { "false" },
            )
        })
        .map_err(|_| ZcodeError::of(codes::STORAGE_FAILED))
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

    /// 唤醒自动连接 worker（授权成功 / 链接更新 / 远程总开关开启时调用）。
    ///
    /// 未启动时只留一个唤醒许可（`Notify` 语义），`start` 后仍会先做一次尝试；
    /// 每次唤醒只做一次尝试，不排队、不重试。
    pub fn wake_auto_connect(&self) {
        self.auto_notify_handle().notify_one();
    }

    /// 单次自动连接尝试：条件每次现查，只调用既有 [`Self::connect`]。
    ///
    /// 顺序：内存暂停/前置条件 → 恢复授权（不打开浏览器）→ 条件复核 → connect。
    /// 任何不满足都直接返回；本阶段不重试、不调用 `execute`。
    async fn try_auto_connect(&self) {
        // 测试观察口：证明本次尝试已开始（负向用例据此等待 worker 跑完，不用 sleep）。
        #[cfg(test)]
        self.record_auto_attempt();
        if self.auto_connect_paused() {
            return;
        }
        let Some(gate) = auto_connect_gate(self.auto_connect_store()) else {
            return;
        };
        if !gate.allows() || !self.auto_connect_runtime_ready() {
            return;
        }
        let status = self.restore_authorization().await;
        if !status.authorized {
            // 恢复失败/无凭据：保持 needs_auth，绝不自动打开浏览器。
            return;
        }
        // await 期间条件可能翻转（用户断开/远程关闭/换链接/KICKED）：连接前全部现查。
        if self.auto_connect_paused() {
            return;
        }
        let Some(gate) = auto_connect_gate(self.auto_connect_store()) else {
            return;
        };
        if !gate.allows() || !self.auto_connect_runtime_ready() {
            return;
        }
        // 共用既有连接内核与 connect_lock；失败留给状态面，不在 C3 重试。
        let _ = self.connect().await;
    }
}

/// 单 worker 驱动：先做一次启动尝试，之后每次唤醒一次尝试。
async fn drive_auto_connect(hub: Weak<ZcodeHub>, notify: Arc<Notify>) {
    if let Some(hub) = hub.upgrade() {
        hub.try_auto_connect().await;
    }
    loop {
        notify.notified().await;
        let Some(hub) = hub.upgrade() else {
            break;
        };
        hub.try_auto_connect().await;
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
}
