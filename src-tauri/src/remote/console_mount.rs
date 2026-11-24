//! H09 控制台生产挂载：同源 `/console/<capability>/` 反代官方网页。
//!
//! ## 为什么挂在主服务器同源路径下
//!
//! 四渠道（LAN / quick / named / Tailscale）都只暴露**一个**监听，`quick` 更是
//! 只有一个随机域名；要额外拿到第二个域名就必须让用户加 DNS/账号，计划明确禁止。
//! 因此控制台只能作为**同一监听上的路径前缀**存在。
//!
//! ## 那为什么不是「仅加路径前缀」的越权实现
//!
//! 路径前缀本身不构成权限隔离；隔离来自**文档的 origin**：
//!
//! * 控制台页面由本模块以 `Content-Security-Policy: sandbox allow-scripts
//!   allow-forms allow-downloads` 送出（**不含 `allow-same-origin`**），
//!   脚本 origin 为 `null`；
//! * 因此控制台脚本既读不到主站 `localStorage`/cookie，也不能借用主站设备权限；
//! * `mam_device` 是 `SameSite=Lax`，opaque origin 属 cross-site，浏览器不会把它
//!   发给任何控制台请求——控制台授权**完全不依赖 cookie**，只认 URL 里的能力令牌。
//!
//! ## 令牌的可见范围
//!
//! 能力令牌出现在 sandbox 文档 URL 与其同前缀请求中。它**不是**主站凭据：主站
//! 管理 API（`/m/api/v1/*`）完全不认识它，`gate` 也不会因它放行。
//!
//! ## 官方页面对相对解析的实测依赖
//!
//! 官方 `index.html` 全部资源为 `./assets/...`；官方客户端 WS 用
//! `new URL(path.slice(1), document.baseURI)`、RPC 用
//! `fetch(`${channel}/${endpoint}`.slice(1))`——两处都是**相对文档基址**解析，
//! 因此前缀反代无需改写任何上游内容。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;

use crate::remote::console_capability::ConsoleCapability;

/// 控制台的沙箱策略：`opaque origin`（无 `allow-same-origin`）。
///
/// `allow-forms`/`allow-downloads` 供官方页面表单与下载；不开 `allow-popups`
/// （控制台自身不允许再开窗口，避免把能力令牌带给第三方文档）。
pub const SANDBOX_CSP: &str =
    "sandbox allow-scripts allow-forms allow-downloads; frame-ancestors 'none'; base-uri 'none'";

/// 单次响应体上限（官方页面资源与 API 载荷都在此量级内）。
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// T06：opaque origin 下的 **Web Storage 替身**（注入到官方入口 HTML 的最前面）。
///
/// **为什么需要**：控制台文档以 `sandbox`（不含 `allow-same-origin`）送出，origin 为
/// `null`；此时**读取** `window.localStorage` 属性本身即抛 `SecurityError`
/// （真实 Chrome 实测，见 `docs/engineering/evidence/2026-11-23-dsh-console-diagnosis/`）。
/// 官方客户端直接使用 `localStorage`（无抽象层、无 try/catch），于是首个异常就在这里，
/// 侧栏与聊天随后因依赖失败而连带不可用。
///
/// **边界（不放宽隔离）**：
/// * 只替换「存储」这一个能力，且**只在内存里**——不落盘、不跨页、不跨会话；
/// * 不改变 CSP、不引入 `allow-same-origin`：脚本仍是 opaque origin，仍读不到主站
///   存储与 cookie，仍不能借用设备权限；
/// * 只注入**官方入口 HTML**（`index_request` 且 `text/html` 且状态 200），
///   不改任何 JS 资源内容，也不改 API 响应。
const STORAGE_SHIM: &str = r#"<script>(function(){try{if(window.localStorage){window.localStorage.getItem("");return}}catch(e){}var m=new Map(),a={getItem:function(k){k=String(k);return m.has(k)?m.get(k):null},setItem:function(k,v){m.set(String(k),String(v))},removeItem:function(k){m.delete(String(k))},clear:function(){m.clear()},key:function(i){return Array.from(m.keys())[i]||null}};Object.defineProperty(a,"length",{get:function(){return m.size}});try{Object.defineProperty(window,"localStorage",{value:a,configurable:false,writable:false});Object.defineProperty(window,"sessionStorage",{value:a,configurable:false,writable:false})}catch(e){}})();</script>"#;

/// 把存储替身注入官方入口 HTML（必须在官方任何脚本之前执行）。
///
/// 定位点优先级：`<head>` 开标签后 → `<html>` 后 → 文档最前。任一失败都返回原文
/// （**不破坏页面**：宁可维持现状，也不产生半截 HTML）。
fn inject_storage_shim(html: Vec<u8>) -> Vec<u8> {
    const MAX_INJECT_HTML: usize = 512 * 1024;
    if html.len() > MAX_INJECT_HTML {
        return html;
    }
    let Ok(text) = std::str::from_utf8(&html) else {
        return html;
    };
    let lower = text.to_ascii_lowercase();
    let insert_at = lower
        .find("<head>")
        .map(|index| index + "<head>".len())
        .or_else(|| {
            lower.find("<head ").map(|index| {
                // 带属性的 head：插到该标签的 '>' 之后
                lower[index..]
                    .find('>')
                    .map(|offset| index + offset + 1)
                    .unwrap_or(index)
            })
        })
        .or_else(|| lower.find("<html>").map(|index| index + "<html>".len()))
        .or(Some(0));
    let Some(insert_at) = insert_at else {
        return html;
    };
    let (before, after) = text.split_at(insert_at);
    format!("{before}{STORAGE_SHIM}{after}").into_bytes()
}

/// 活连接登记项：**必须**同时带设备与实例归属，撤权才能精确到范围。
///
/// 只带实例无法正确实现「撤某台设备」——那会误断其他设备的连接；
/// 只带设备则无法实现「换实例」。
struct LiveConnection {
    id: u64,
    device_id: String,
    instance_id: String,
    cancel: tokio::sync::watch::Sender<bool>,
}

/// 授权状态内核：能力表与活连接表放在**同一把锁**下。
///
/// 为什么必须同锁（复审 Major-1）：撤权要「清能力 + 取消活连接」，而授权要
/// 「查能力 + 登记活连接」。两张表各自加锁时存在确定的窗口：
/// 请求已校验通过、撤权恰好完成、请求随后才登记 → 该连接永远收不到取消信号。
/// 同锁后这两个动作互斥，撤销要么发生在校验之前（请求被拒），要么发生在
/// 登记之后（该连接必被取消）。
///
/// 锁内**不做任何 IO**（`device_valid` 是外部闭包，须在锁外调用），
/// 因此不会把慢查询塞进临界区。
#[derive(Default)]
struct ConsoleState {
    capabilities: HashMap<String, ConsoleCapability>,
    credentials: HashMap<String, BrowserCredential>,
    live: Vec<LiveConnection>,
    next_live_id: u64,
    /// 撤销序号（M02c）：每次撤权/换实例/关远程都 +1。
    ///
    /// 为什么需要它：`device_valid` / `epoch_valid` 都是**锁外**调用（可能查库或
    /// 重新发现活体，耗时不可控）。检查通过之后、回锁登记之前，可能已经发生了一次
    /// 撤权——那条记录此刻已不该被服务。锁内复核序号即可识别这种「检查期间被撤」。
    ///
    /// 只比「能力是否还在」不够：撤销与重新签发之间，同名令牌可能已被重新登记。
    revocation_seq: u64,
    credential_revocation: u64,
    instance_credentials: HashMap<String, u64>,
}

#[derive(Clone, Copy)]
pub(crate) enum CredentialSequence {
    Legacy(u64),
    Scoped { global: u64, instance: u64 },
}

struct BrowserCredential {
    epoch: u64,
    authority: String,
    cookie: String,
    gate: Arc<crate::services::dsh_control::transport::PeerGate>,
}

/// 控制台挂载状态。
/// 「该实例的当代代际是否仍等于能力签发时的代际」判定（P1-2c）。
///
/// 单独起别名而不是内联 boxed closure：同一形状在结构体字段与 builder 参数里
/// 出现两次，内联会被 clippy 判为过复杂类型。
pub type EpochValidityCheck = Box<dyn Fn(&str, u64, &str) -> bool + Send + Sync>;

#[cfg(feature = "dsh-test-probes")]
pub type TestUpstreamSink = std::pin::Pin<
    Box<
        dyn futures::Sink<
                tokio_tungstenite::tungstenite::Message,
                Error = tokio_tungstenite::tungstenite::Error,
            > + Send,
    >,
>;
#[cfg(feature = "dsh-test-probes")]
pub type TestSinkWrapper = Arc<dyn Fn(TestUpstreamSink) -> TestUpstreamSink + Send + Sync>;

pub struct LivePermit {
    state: std::sync::Weak<Mutex<ConsoleState>>,
    id: u64,
    cancel: tokio::sync::watch::Receiver<bool>,
    cookie: String,
    gate_cancel: tokio::sync::watch::Receiver<bool>,
}

impl std::ops::Deref for LivePermit {
    type Target = tokio::sync::watch::Receiver<bool>;
    fn deref(&self) -> &Self::Target {
        &self.cancel
    }
}

impl std::ops::DerefMut for LivePermit {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.cancel
    }
}

impl Drop for LivePermit {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .live
                .retain(|live| live.id != self.id);
        }
    }
}

pub struct ConsoleMount {
    state: Arc<Mutex<ConsoleState>>,
    /// 设备是否仍然有效（生产 = 查设备库；测试可注入）。
    ///
    /// 能力令牌不依赖设备 cookie，若只在签发时校验一次，**撤设备/改 PIN 之后
    /// 旧令牌在过期前仍然可用**。因此每次请求都要重新问一次「这台设备现在还有效吗」。
    pub device_valid: Box<dyn Fn(&str) -> bool + Send + Sync>,
    /// 该实例**当代**代际是否仍等于能力签发时的代际（P1-2c）。
    ///
    /// 为什么必须有：上游端口会复用。若能力只绑 `authority`，宿主重启换端口后
    /// 旧能力可能指向一个**已不属于该实例**的端口，而我们仍会为它附上当代凭据
    /// ——等于把新代秘密送到陌生对端。代际不等即失效（用户重开控制台即可）。
    ///
    /// 生产实现用登记面重新发现；测试注入闭包。默认恒 true（未接线）。
    pub epoch_valid: EpochValidityCheck,
    pub now_ms: Box<dyn Fn() -> i64 + Send + Sync>,
    #[cfg(feature = "dsh-test-probes")]
    sink_wrapper: Option<TestSinkWrapper>,
}

impl ConsoleMount {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ConsoleState::default())),
            #[cfg(feature = "dsh-test-probes")]
            sink_wrapper: None,
            // 默认拒绝：未接线设备库时任何能力都不可用（fail-closed）。
            epoch_valid: Box::new(|_, _, _| true),
            device_valid: Box::new(|_| false),
            now_ms: Box::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64
            }),
        }
    }

    /// 装配生产设备库校验（`mam_device` 同源判定：撤销/过期即失效）。
    pub fn with_device_store(self, store: crate::remote::pairing::DeviceStore) -> Self {
        let clock = std::sync::Arc::new(self.now_ms);
        let clock_for_self = std::sync::Arc::clone(&clock);
        let clock_for_check = std::sync::Arc::clone(&clock);
        Self {
            now_ms: Box::new(move || clock_for_self()),
            device_valid: Box::new(move |device_id| {
                let now = clock_for_check();
                store.with(|conn| crate::remote::pairing::device_valid(conn, device_id, now))
            }),
            ..self
        }
    }

    /// 装配代际校验（P1-2c）：给定实例的当代代际是否仍等于签发时的代际。
    pub fn with_epoch_check(
        mut self,
        check: impl Fn(&str, u64) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.epoch_valid = Box::new(move |id, epoch, _| check(id, epoch));
        self
    }

    pub fn with_target_check(
        mut self,
        check: impl Fn(&str, u64, &str) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.epoch_valid = Box::new(check);
        self
    }

    #[cfg(feature = "dsh-test-probes")]
    #[doc(hidden)]
    pub fn with_test_sink_wrapper(mut self, wrapper: TestSinkWrapper) -> Self {
        self.sink_wrapper = Some(wrapper);
        self
    }

    pub fn now(&self) -> i64 {
        (self.now_ms)()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ConsoleState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// 登记一条能力（由已授权的管理面调用，调用方负责设备/实例核验）。
    pub fn register(&self, record: ConsoleCapability) -> Result<(), String> {
        let mut state = self.lock();
        state.capabilities.insert(record.token.clone(), record);
        Ok(())
    }

    pub fn install_browser_credential(
        &self,
        instance: &str,
        epoch: u64,
        authority: &str,
        browser: String,
        gate: Arc<crate::services::dsh_control::transport::PeerGate>,
    ) -> Result<(), String> {
        let sequence = self.revocation_sequence();
        self.install_browser_credential_if_current(
            instance, epoch, authority, browser, gate, sequence,
        )
    }

    pub fn install_browser_credential_if_current(
        &self,
        instance: &str,
        epoch: u64,
        authority: &str,
        browser: String,
        gate: Arc<crate::services::dsh_control::transport::PeerGate>,
        sequence: u64,
    ) -> Result<(), String> {
        self.install_browser_credential_committing(
            instance,
            epoch,
            authority,
            browser,
            gate,
            CredentialSequence::Legacy(sequence),
            || Ok(()),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn install_browser_credential_committing(
        &self,
        instance: &str,
        epoch: u64,
        authority: &str,
        browser: String,
        gate: Arc<crate::services::dsh_control::transport::PeerGate>,
        sequence: CredentialSequence,
        commit: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        let mut state = self.lock();
        let current = match sequence {
            CredentialSequence::Legacy(sequence) => state.revocation_seq == sequence,
            CredentialSequence::Scoped {
                global,
                instance: captured,
            } => {
                state.credential_revocation == global
                    && state
                        .instance_credentials
                        .get(instance)
                        .copied()
                        .unwrap_or(0)
                        == captured
            }
        };
        if gate.is_revoked() || !current {
            return Err("console_credential_invalid".into());
        }
        commit()?;
        state.revocation_seq = state.revocation_seq.wrapping_add(1);
        state
            .capabilities
            .retain(|_, record| record.instance_id != instance);
        let mut retained = Vec::with_capacity(state.live.len());
        for live in state.live.drain(..) {
            if live.instance_id == instance {
                live.cancel.send_replace(true);
            } else {
                retained.push(live);
            }
        }
        state.live = retained;
        state.credentials.remove(instance);
        if !browser.is_empty() {
            state.credentials.insert(
                instance.to_owned(),
                BrowserCredential {
                    epoch,
                    authority: authority.to_owned(),
                    cookie: browser,
                    gate,
                },
            );
        }
        Ok(())
    }

    #[cfg(feature = "dsh-test-probes")]
    pub(crate) fn raw_credential_present(&self, instance: &str) -> bool {
        self.lock().credentials.contains_key(instance)
    }

    pub(crate) fn credential_sequence_for(&self, instance: &str) -> CredentialSequence {
        let state = self.lock();
        CredentialSequence::Scoped {
            global: state.credential_revocation,
            instance: state
                .instance_credentials
                .get(instance)
                .copied()
                .unwrap_or(0),
        }
    }

    /// 在慢发现开始前采样，用于拒绝撤权后迟到的票据。
    pub fn revocation_sequence(&self) -> u64 {
        self.lock().revocation_seq
    }

    pub fn register_if_current(
        &self,
        record: ConsoleCapability,
        sequence: u64,
    ) -> Result<(), String> {
        // T3（移动端四态映射契约）：设备失效与换代/目标失效**分码**——移动端据此
        // 区分「设备授权已过期（重新配对）」与「实例已重启（稍后重试）」。
        if !(self.device_valid)(&record.device_id) {
            return Err("console_device_invalid".into());
        }
        if !(self.epoch_valid)(&record.instance_id, record.epoch, &record.authority) {
            return Err("console_target_invalid".into());
        }
        let mut state = self.lock();
        if state.revocation_seq != sequence || self.now() >= record.expires_at_ms {
            return Err("console_target_invalid".into());
        }
        if !state
            .credentials
            .get(&record.instance_id)
            .is_some_and(|credential| {
                !credential.gate.is_revoked()
                    && credential.epoch == record.epoch
                    && credential.authority == record.authority
            })
        {
            return Err("console_credential_invalid".into());
        }
        state.capabilities.insert(record.token.clone(), record);
        Ok(())
    }

    /// **原子授权**：校验能力 + 登记一条活连接，或拒绝。
    ///
    /// 返回 `(记录, 取消接收端)`。撤权若在校验前发生 → 拒绝；在登记后发生 →
    /// 该接收端必被置位（同一把锁保证二者必居其一）。
    ///
    /// `device_valid` 在锁外调用（可能是查库），随后在锁内复核「能力仍在」：
    /// 这样「查设备」耗时期间发生的撤权不会漏掉该连接。
    fn authorize_live(&self, token: &str) -> Option<(ConsoleCapability, LivePermit)> {
        let now = self.now();
        // 第一步（锁外）：取记录并记下当时的撤销序号。
        // 后面锁外做的事（查设备库、重新发现活体）都可能耗时，期间若发生过撤权，
        // 回锁时的序号会变大——那说明本次准入看到的状态已经过期。
        let (candidate, seq_at_probe) = {
            let state = self.lock();
            let found = state
                .capabilities
                .get(token)
                .filter(|record| now < record.expires_at_ms)
                // P1：引导令牌**只能**在根路径盖章换成会话，绝不直接服务请求。
                // 放它进来等于把「60 秒一次性链接」变成可直达 /api/*、可升 WS 的
                // 通用凭据——一次性语义就没了。
                .filter(|record| !record.bootstrap)
                .cloned()?;
            (found, state.revocation_seq)
        };
        if !(self.device_valid)(&candidate.device_id) {
            return None;
        }
        // P1-2c：代际复核放在锁外（可能要重新发现活体），随后在锁内复核对
        // 「能力仍在」。两步之间换代 → 锁内复核会因能力已被 revoke 而失败，
        // 或此处直接拒绝。
        if !(self.epoch_valid)(
            &candidate.instance_id,
            candidate.epoch,
            &candidate.authority,
        ) {
            return None;
        }
        // 第二步（锁内）：复核能力仍在，并登记活连接——与撤权互斥。
        let (tx, rx) = tokio::sync::watch::channel(false);
        let mut state = self.lock();
        // 锁外探测期间若发生过撤权，序号会变——本次准入作废（不登记活连接）。
        if state.revocation_seq != seq_at_probe {
            return None;
        }
        let current = state
            .capabilities
            .get(token)
            .filter(|record| self.now() < record.expires_at_ms)
            .filter(|record| *record == &candidate)
            .cloned()?;
        let credential = state.credentials.get(&current.instance_id)?;
        if credential.gate.is_revoked()
            || credential.epoch != current.epoch
            || credential.authority != current.authority
        {
            return None;
        }
        let cookie = credential.cookie.clone();
        let gate_cancel = credential.gate.subscribe_revocation();
        if credential.gate.is_revoked() {
            return None;
        }
        let id = state.next_live_id;
        state.next_live_id = state.next_live_id.wrapping_add(1);
        state.live.push(LiveConnection {
            id,
            device_id: current.device_id.clone(),
            instance_id: current.instance_id.clone(),
            cancel: tx,
        });
        Some((
            current,
            LivePermit {
                state: Arc::downgrade(&self.state),
                id,
                cancel: rx,
                cookie,
                gate_cancel,
            },
        ))
    }

    /// **设备绑定的票据兑换**（M02）。
    ///
    /// 契约：票据必须存在、未过期、`bootstrap == true`，且**签发时的设备**就是
    /// `device_id`。逐条检查通过后在同一把锁内**原子**摘除票据并换发页面会话；
    /// 任何一步不满足都返回 `None` 且**不改动表**（错设备的失败不得烧掉别人的票据）。
    ///
    /// 为什么要在锁内做完：两次独立取锁会留下「校验通过→撤权→仍换发出会话」的
    /// 窗口，等于给已撤销设备补发了一个可用会话。
    pub fn redeem_bootstrap_for_device(&self, ticket: &str, device_id: &str) -> Option<String> {
        let (record, sequence) = {
            let state = self.lock();
            let record = state
                .capabilities
                .get(ticket)
                .filter(|record| self.now() < record.expires_at_ms)
                .filter(|record| record.bootstrap && record.device_id == device_id)
                .cloned()?;
            (record, state.revocation_seq)
        };
        // 销项复审 P1：关闭即效面——「正在关闭」的实例不再兑换新引导票据
        // （入口已停发；存量票在此兜底拒兑）。
        if crate::services::dsh_control::production::is_closing(&record.instance_id) {
            return None;
        }
        if !(self.device_valid)(&record.device_id)
            || !(self.epoch_valid)(&record.instance_id, record.epoch, &record.authority)
        {
            return None;
        }
        let mut state = self.lock();
        let now = self.now();
        if state.revocation_seq != sequence
            || now >= record.expires_at_ms
            || state.capabilities.get(ticket) != Some(&record)
            || !state
                .credentials
                .get(&record.instance_id)
                .is_some_and(|credential| {
                    !credential.gate.is_revoked()
                        && credential.epoch == record.epoch
                        && credential.authority == record.authority
                })
        {
            return None;
        }
        // 校验通过：原子消费并换发会话。
        state.capabilities.remove(ticket);
        let fresh = crate::remote::random_hex_16();
        state.capabilities.insert(
            fresh.clone(),
            ConsoleCapability {
                token: fresh.clone(),
                bootstrap: false,
                expires_at_ms: now
                    .saturating_add(crate::remote::console_capability::CAPABILITY_TTL_MS),
                ..record
            },
        );
        Some(fresh)
    }

    /// 观测面：当前登记的活连接数（测试用来断言「转发确实退出、连接已释放」）。
    ///
    /// `#[doc(hidden)]`：只回数量，不导出任何令牌或凭据。
    #[doc(hidden)]
    pub fn live_count_for_test(&self) -> usize {
        self.lock().live.len()
    }

    /// 观测面：该令牌是否仍在能力表内（**不等同于**「可用」——可用还要过设备
    /// 有效性；这里只回答「记录还在不在」，用于验证撤销是否发生）。
    #[doc(hidden)]
    pub fn has_capability_for_test(&self, token: &str) -> bool {
        self.lock().capabilities.contains_key(token)
    }

    /// 观测面：只读校验（不登记活连接）。
    #[doc(hidden)]
    pub fn lookup_for_test(&self, token: &str) -> Option<ConsoleCapability> {
        self.lookup(token)
    }

    /// 观测/测试面：走与真实 WS 升级同一条原子路径登记活连接。
    ///
    /// `#[doc(hidden)]`：不导出任何能力/凭据，只返回记录与取消接收端。
    #[doc(hidden)]
    pub fn authorize_live_for_test(&self, token: &str) -> Option<(ConsoleCapability, LivePermit)> {
        self.authorize_live(token)
    }

    /// 只读校验（无活连接要登记的场景：预检、拒绝路径）。
    fn lookup(&self, token: &str) -> Option<ConsoleCapability> {
        let now = self.now();
        let record = {
            let state = self.lock();
            state
                .capabilities
                .get(token)
                .filter(|record| now < record.expires_at_ms)
                // 引导令牌不是会话：只能经已配对设备的兑换入口换成会话。
                .filter(|record| !record.bootstrap)
                .cloned()?
        };
        if !(self.device_valid)(&record.device_id) {
            return None;
        }
        // P1-2c：代际复核。换代后旧能力立即失效（含未过期）。
        (self.epoch_valid)(&record.instance_id, record.epoch, &record.authority).then_some(record)
    }

    /// 撤权：清空全部能力并取消活连接（撤设备/关远程共用）。
    pub fn revoke_all(&self) {
        let mut state = self.lock();
        // M02c：撤销序号 +1——任何在锁外探测中的准入都会因此作废。
        state.credential_revocation = state.credential_revocation.wrapping_add(1);
        state.instance_credentials.clear();
        state.revocation_seq = state.revocation_seq.wrapping_add(1);
        state.capabilities.clear();
        state.credentials.clear();
        for live in state.live.drain(..) {
            live.cancel.send_replace(true);
        }
    }

    /// 撤某台设备：**只**作废该设备的能力与其连接（撤设备/改 PIN）。
    pub fn revoke_device(&self, device_id: &str) {
        let mut state = self.lock();
        // M02c：撤销序号 +1——任何在锁外探测中的准入都会因此作废。
        state.revocation_seq = state.revocation_seq.wrapping_add(1);
        state
            .capabilities
            .retain(|_, record| record.device_id != device_id);
        let mut kept = Vec::with_capacity(state.live.len());
        for live in state.live.drain(..) {
            if live.device_id == device_id {
                live.cancel.send_replace(true);
            } else {
                kept.push(live);
            }
        }
        state.live = kept;
    }

    /// 换实例/撤该实例绑定：**只**吊销该实例的能力与其活连接。
    pub fn revoke_instance(&self, instance_id: &str) {
        let mut state = self.lock();
        // M02c：撤销序号 +1——任何在锁外探测中的准入都会因此作废。
        let generation = state
            .instance_credentials
            .entry(instance_id.to_owned())
            .or_default();
        *generation = generation.wrapping_add(1);
        state.revocation_seq = state.revocation_seq.wrapping_add(1);
        state.credentials.remove(instance_id);
        state
            .capabilities
            .retain(|_, record| record.instance_id != instance_id);
        let mut kept = Vec::with_capacity(state.live.len());
        for live in state.live.drain(..) {
            if live.instance_id == instance_id {
                live.cancel.send_replace(true);
            } else {
                kept.push(live);
            }
        }
        state.live = kept;
    }

    /// **关旧页面**（P1-4）：只 drain 并取消该实例的**活连接**，凭据与能力全部
    /// 保留。
    ///
    /// 为什么与 [`Self::revoke_instance`] 分开（2026-11-14 用户裁决：切走关页 ≠
    /// 撤权）：实例切换（`instance_select_core`）要关掉上一实例正开着的控制台
    /// 页面，但保留其凭据——切回时无需重交付即可直接打开。撤销授权（连凭据一起
    /// 清）走 [`Self::revoke_instance`]，由撤权链（`revoke_local`）触发。
    ///
    /// 撤销序号同样 +1：`authorize_live` 在锁外探测后回锁登记，不 +1 就存在
    /// 「close-pages 完成后、探测中的连接才登记」的窗口——那条连接永远收不到
    /// 取消信号（与撤权同款 Major-1 窗口）。代价是探测期间采样的入口登记会被拒
    /// （客户端重试即可，正确性优先）。
    ///
    /// 如实申报边界：凭据保留意味着已关页面若在浏览器里**主动重连**（重新发起
    /// 请求/WS），authorize_live 会再次成功——close-pages 是一次性 drain，不是
    /// 持续封锁；持续封锁 = 撤权（本裁决刻意分离的两个动作）。
    pub fn close_instance_pages(&self, instance_id: &str) {
        let mut state = self.lock();
        state.revocation_seq = state.revocation_seq.wrapping_add(1);
        let mut kept = Vec::with_capacity(state.live.len());
        for live in state.live.drain(..) {
            if live.instance_id == instance_id {
                live.cancel.send_replace(true);
            } else {
                kept.push(live);
            }
        }
        state.live = kept;
    }

    /// 只读观测面（P1-4 自动重交付触发用）：该实例在 (epoch, authority) 目标上是
    /// 否已安装**未撤销**的浏览器凭据。不导出任何秘密字节。
    #[doc(hidden)]
    pub fn browser_credential_installed(
        &self,
        instance_id: &str,
        epoch: u64,
        authority: &str,
    ) -> bool {
        self.lock()
            .credentials
            .get(instance_id)
            .is_some_and(|credential| {
                !credential.gate.is_revoked()
                    && credential.epoch == epoch
                    && credential.authority == authority
            })
    }
}

impl ConsoleMount {
    /// 测试/装配用：注入设备有效性判定（生产用 [`Self::with_device_store`]）。
    #[doc(hidden)]
    pub fn with_device_check(self, check: impl Fn(&str) -> bool + Send + Sync + 'static) -> Self {
        Self {
            device_valid: Box::new(check),
            ..self
        }
    }
}

impl Default for ConsoleMount {
    fn default() -> Self {
        Self::new()
    }
}

/// 生产单例：能力登记（管理面签发）与路由（转发）必须共享同一份状态。
///
/// **只**接真实设备库；没有任何测试开关可以改变生产行为（测试通过
/// [`ConsoleMount::new`] + [`ConsoleMount::with_device_check`] 自行装配 router）。
static MOUNT: std::sync::OnceLock<Arc<ConsoleMount>> = std::sync::OnceLock::new();

fn production_mount() -> Arc<ConsoleMount> {
    let mount = ConsoleMount::new().with_target_check(|instance_id, epoch, authority| {
        // P1-2c 生产实现：用登记面重新发现**当代**活体并比对代际。
        // 任何读取/发现失败都按「当代不可证」处理 → 拒绝（fail-closed）；
        // 未登记（如测试形态的状态目录）同样拒绝，绝不因为查不到就放行。
        let Ok(registry) = crate::services::dsh_control::production::production_registry() else {
            return false;
        };
        registry
            .resolve_selected()
            .map(|host| {
                host.host_id == instance_id && host.epoch == epoch && host.authority == authority
            })
            .unwrap_or(false)
    });
    match crate::remote::pairing_device_store() {
        Some(store) => Arc::new(mount.with_device_store(store)),
        // 拿不到设备库（极端早期）：保持 fail-closed，宁可不发控制台。
        None => Arc::new(mount),
    }
}

/// 当前生产挂载状态（管理面签发能力、授权层撤权都走这里）。
pub fn production() -> &'static Arc<ConsoleMount> {
    MOUNT.get_or_init(production_mount)
}

#[cfg(feature = "dsh-test-probes")]
#[doc(hidden)]
pub fn init_production_mount_for_test(mount: Arc<ConsoleMount>) -> Result<(), Arc<ConsoleMount>> {
    MOUNT.set(mount)
}

/// 全局撤权（供 `console_lifecycle::revoke_console_access` 调用）。
pub fn revoke_all() {
    production().revoke_all();
}

/// 全局撤某个实例（换实例/撤该实例绑定）。
pub fn revoke_instance(instance_id: &str) {
    production().revoke_instance(instance_id);
}

/// 全局关某实例的活页面（P1-4 切走原语）：只 drain 活连接，凭据与能力保留。
pub fn close_instance_pages(instance_id: &str) {
    production().close_instance_pages(instance_id);
}

/// 全局撤某台设备（撤设备/改 PIN 时调用）：该设备签发的全部能力作废。
pub fn revoke_device(device_id: &str) {
    production().revoke_device(device_id);
}

/// 组装 `/console/{token}` 子树。
///
/// 这里**不**注册任何 cookie 鉴权——授权完全由路径令牌承担，且该子树从不匹配
/// `/m/api/v1`，因此永远不会被设备门禁当作已放行路径。
pub fn router(mount: Arc<ConsoleMount>) -> Router {
    Router::new()
        .route("/console/{token}", any(index).options(preflight))
        .route("/console/{token}/", any(index).options(preflight))
        // OPTIONS 单独走预检：opaque origin（`Origin: null`）下浏览器会先发预检，
        // 若把预检当普通请求反代给上游，请求头会丢失且上游方法也不支持。
        .route("/console/{token}/{*rest}", any(proxy).options(preflight))
        // 最外层统一补安全头：提取器拒绝、提前返回与 101 升级都不能漏掉
        // 「控制台响应必须带沙箱策略」这条不变量。
        .layer(axum::middleware::map_response(
            |mut response: Response| async move {
                if response
                    .headers()
                    .get(header::CONTENT_SECURITY_POLICY)
                    .is_none()
                {
                    apply_sandbox_headers(&mut response);
                }
                response
            },
        ))
        .with_state(mount)
}

/// CORS 预检：只回allow 头，不触上游。
///
/// 控制台文档的 origin 是 opaque（`null`），所以必须显式允许 `null`——这不是
/// 放宽，而是把「谁可以读控制台的响应」限定为**那个沙箱文档**：
/// `Access-Control-Allow-Credentials` 一律不发，宿主凭据只在服务端。
async fn preflight(
    State(mount): State<Arc<ConsoleMount>>,
    request: axum::extract::Request,
) -> Response {
    let uri = request.uri();
    let token = uri
        .path()
        .strip_prefix("/console/")
        .and_then(|value| value.split('/').next())
        .unwrap_or_default();
    if !request
        .headers()
        .contains_key(header::ACCESS_CONTROL_REQUEST_METHOD)
    {
        let token = token.to_owned();
        return proxy(State(mount), Path((token, String::new())), request).await;
    }
    if mount.lookup(token).is_none() {
        return forbidden();
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    apply_cors(&mut response);
    apply_sandbox_headers(&mut response);
    response
}

/// opaque-origin CORS 头（合成夹具 `h09_sandbox_candidate` 已实测该形态可用）。
fn apply_cors(response: &mut Response) {
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("null"),
    );
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("*"),
    );
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("*"),
    );
    response.headers_mut().insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("*"),
    );
    response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Origin"));
}

/// 控制台根路径。
///
/// 若拿到的是一次性引导（P1-2e），**先盖章**再 302 到会话令牌的根路径：
/// 地址栏/历史里随即只留下已废的引导（第二次访问必然 403），页面后续请求用
/// 会话令牌。相对定位（`../<token>/`）保证四渠道共用同一入口。
async fn index(
    State(mount): State<Arc<ConsoleMount>>,
    Path(token): Path<String>,
    request: axum::extract::Request,
) -> Response {
    proxy(State(mount), Path((token, String::new())), request).await
}

async fn proxy(
    State(mount): State<Arc<ConsoleMount>>,
    Path((token, _)): Path<(String, String)>,
    request: axum::extract::Request,
) -> Response {
    let prefix = format!("/console/{token}");
    let Some(path) = request.uri().path().strip_prefix(&prefix) else {
        return forbidden();
    };
    let path = if path.is_empty() { "/" } else { path };
    if validated_upstream_path(path).is_none() {
        return forbidden();
    }
    let target = match request.uri().query() {
        Some(query) => format!("{path}?{query}"),
        None => path.to_owned(),
    };
    if request
        .headers()
        .get(header::UPGRADE)
        .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"))
    {
        return upgrade(&mount, &token, &target, request).await;
    }
    let method = request.method().clone();
    // 头必须在取 body 之前克隆（`into_body` 会消耗 request）。
    let headers = request.headers().clone();
    let body = match axum::body::to_bytes(request.into_body(), MAX_BODY_BYTES).await {
        Ok(body) => body,
        // P1-2b：正文读取失败**不得当作空正文**——那会把「上传被截断」伪装成
        // 一次成功的空上传。
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };
    serve_with(
        &mount,
        &token,
        &target,
        Some((method, body.to_vec(), headers)),
    )
    .await
}

/// 纯 GET 出口（首页与静态资源）。
#[cfg(test)]
async fn serve(
    mount: &Arc<ConsoleMount>,
    token: &str,
    target: &str,
    request: Option<(axum::http::Method, Vec<u8>, header::HeaderMap)>,
) -> Response {
    serve_with(mount, token, target, request).await
}

/// 上游响应体上限（首页与 API 都是小型 JSON/静态资源；240 MB 的 mux 快照不应
/// 经这条 HTTP 通道整块回灌内存）。
const MAX_UPSTREAM_BODY_BYTES: usize = 32 * 1024 * 1024;

/// 从入站请求挑出要转发的头（P1-2b）：只放必需的，绝不透传 cookie/origin 等。
fn forwarded_request_headers(
    headers: &header::HeaderMap,
) -> Vec<(header::HeaderName, HeaderValue)> {
    let connection = headers
        .get(header::CONNECTION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    headers
        .iter()
        .filter(|(name, _)| {
            let name = name.as_str();
            !matches!(
                name,
                "host"
                    | "accept-encoding"
                    | "cookie"
                    | "authorization"
                    | "origin"
                    | "referer"
                    | "forwarded"
                    | "connection"
                    | "upgrade"
                    | "te"
                    | "trailer"
                    | "transfer-encoding"
                    | "keep-alive"
                    | "content-length"
                    | "x-real-ip"
                    | "cf-connecting-ip"
            ) && !name.starts_with("proxy-")
                && !name.starts_with("sec-")
                && !name.starts_with("x-forwarded-")
                && !connection
                    .split(',')
                    .any(|value| value.trim().eq_ignore_ascii_case(name))
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

async fn serve_with(
    mount: &Arc<ConsoleMount>,
    token: &str,
    target: &str,
    request: Option<(axum::http::Method, Vec<u8>, header::HeaderMap)>,
) -> Response {
    // P1-2d：HTTP 也必须登记为**活请求**。只给 WS 登记时，一次慢上游/大响应会
    // 在撤权之后继续把数据读完并写回浏览器——撤权只是「以后不再发新的」而不是
    // 「现在停」。这里复用与 WS 同一条原子授权路径，随后在发送与读体两处竞速。
    let Some((capability, mut cancel)) = mount.authorize_live(token) else {
        // 未知/过期/已撤权能力：统一 403，不区分原因（不泄露令牌是否存在）。
        return forbidden();
    };
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, format!("?{query}")),
        None => (target, String::new()),
    };
    if validated_upstream_path(path).is_none() {
        return forbidden();
    }
    let index_request = path == "/"
        && request
            .as_ref()
            .is_none_or(|(method, _, _)| *method == axum::http::Method::GET);
    // 凭据缺失时在出站之前拒绝，不能向可能已易主的旧地址发请求。
    let cookie = cancel.cookie.clone();
    let url = format!("http://{}{}{}", capability.authority, path, query);
    // P1-2b：**禁止跟随重定向**——上游若回 3xx，跟随就可能把带凭据的请求送到
    // 实例之外的地址（`reqwest` 默认最多 10 跳，跨 host 也会跟）。
    let client = match reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let head_request = request
        .as_ref()
        .is_some_and(|(method, _, _)| *method == axum::http::Method::HEAD);
    let mut outgoing = match request {
        Some((method, bytes, headers)) => {
            let method = match reqwest::Method::from_bytes(method.as_str().as_bytes()) {
                Ok(method) => method,
                // 未知方法不明文拒绝，但也不猜——按 400 处理（上游也不认）。
                Err(_) => return StatusCode::BAD_REQUEST.into_response(),
            };
            let mut builder = client.request(method, &url);
            for (name, value) in forwarded_request_headers(&headers) {
                builder = builder.header(name, value);
            }
            builder.body(bytes)
        }
        None => client.get(&url),
    };
    // 上游凭据由本进程持有；浏览器自带的任何 cookie 一律不透传。
    outgoing = outgoing.header(reqwest::header::COOKIE, cookie);
    // 到期与撤权是**两个**独立条件：撤权靠取消信号，到期靠绝对时刻。
    // 只挂取消信号会让「页面开着、设备还在、但能力已到期」的请求继续成功。
    let expire_at = tokio::time::Instant::now()
        + std::time::Duration::from_millis(
            capability.expires_at_ms.saturating_sub(mount.now()).max(0) as u64,
        );
    // P1-2d：上游握手/首字节等待也要可被撤权**与到期**抢占（慢上游是主要阻塞点）。
    let upstream = {
        let send = outgoing.send();
        tokio::pin!(send);
        tokio::select! {
            biased;
            _ = wait_permit_revoked(&mut cancel) => return forbidden(),
            _ = tokio::time::sleep_until(expire_at) => return forbidden(),
            result = &mut send => match result {
                Ok(response) => response,
                Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
            },
        }
    };
    // 上游跳转必须留在已授权的挂载内，不代浏览器跟随或使用其他 authority。
    let location = if upstream.status().is_redirection() {
        match upstream.headers().get(header::LOCATION) {
            Some(location) => {
                let Some(url) = location
                    .to_str()
                    .ok()
                    .and_then(|value| upstream.url().join(value).ok())
                else {
                    return StatusCode::BAD_GATEWAY.into_response();
                };
                if url.origin() != upstream.url().origin()
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || validated_upstream_path(url.path()).is_none()
                {
                    return StatusCode::BAD_GATEWAY.into_response();
                }
                let query = url.query().map(|q| format!("?{q}")).unwrap_or_default();
                let fragment = url.fragment().map(|f| format!("#{f}")).unwrap_or_default();
                HeaderValue::from_str(&format!("/console/{token}{}{query}{fragment}", url.path()))
                    .ok()
            }
            None => None,
        }
    } else {
        None
    };
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    let boot_html = index_request
        && status == StatusCode::OK
        && content_type.is_some_and(|value| value.eq_ignore_ascii_case("text/html"));
    let upstream_headers = upstream.headers().clone();
    let streaming = !head_request
        && location.is_none()
        && content_type.is_some_and(|value| value.eq_ignore_ascii_case("text/event-stream"));
    if streaming {
        use futures::StreamExt as _;
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let revoke = cancel.cancel.clone();
        let credential_revoke = cancel.gate_cancel.clone();
        tokio::spawn(async move {
            let mut stream = Box::pin(upstream.bytes_stream());
            let transfer = async {
                while let Some(chunk) = stream.next().await {
                    let failed = chunk.is_err();
                    if tx.send(chunk).await.is_err() || failed {
                        break;
                    }
                }
            };
            tokio::select! {
                biased;
                _ = wait_permit_revoked(&mut cancel) => {},
                _ = tokio::time::sleep_until(expire_at) => {},
                _ = tx.closed() => {},
                _ = transfer => {},
            }
        });
        let body = tokio_stream::wrappers::ReceiverStream::new(rx).take_while(move |_| {
            futures::future::ready(
                !*revoke.borrow()
                    && !*credential_revoke.borrow()
                    && tokio::time::Instant::now() < expire_at,
            )
        });
        let mut response = Response::new(Body::from_stream(body));
        *response.status_mut() = status;
        copy_console_response_headers(&upstream_headers, &mut response);
        apply_sandbox_headers(&mut response);
        return response;
    }
    // P1-2b：声明长度超上限直接拒（不先读进内存再判断）。
    if upstream
        .content_length()
        .is_some_and(|len| len as usize > MAX_UPSTREAM_BODY_BYTES)
    {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    // P1-2b：正文读取失败**不得当作空正文**（旧实现 unwrap_or_default 会把
    // 截断/断流伪装成 200 空响应）。
    //
    // P1 复审：改成**流式累计**。`Content-Length` 可以缺失或撒谎（chunked /
    // 上游说谎），一次性 `bytes()` 会先把整个正文读进内存再判断上限——上限就
    // 形同虚设。逐块累计并在超限的当块立即断开，才是真上限。
    //
    // P1-2d：每个块都同时受撤权与到期保护——慢上游/大响应不得在撤权后继续读完。
    let upstream_length = upstream.headers().get(header::CONTENT_LENGTH).cloned();
    let bytes = {
        use futures::StreamExt as _;
        let mut stream = upstream.bytes_stream();
        let mut collected: Vec<u8> = Vec::new();
        loop {
            let chunk = tokio::select! {
                biased;
                _ = wait_permit_revoked(&mut cancel) => return forbidden(),
                _ = tokio::time::sleep_until(expire_at) => return forbidden(),
                next = stream.next() => next,
            };
            match chunk {
                Some(Ok(chunk)) => {
                    if collected.len().saturating_add(chunk.len()) > MAX_UPSTREAM_BODY_BYTES {
                        // 超限即断：不继续读、不缓存剩余部分。
                        return StatusCode::BAD_GATEWAY.into_response();
                    }
                    collected.extend_from_slice(&chunk);
                }
                Some(Err(_)) => return StatusCode::BAD_GATEWAY.into_response(),
                None => break,
            }
        }
        collected
    };
    if *cancel.cancel.borrow()
        || *cancel.gate_cancel.borrow()
        || tokio::time::Instant::now() >= expire_at
    {
        return forbidden();
    }
    // T06：只对**官方入口 HTML**（index + 200 + text/html）注入存储替身。
    // 这是 opaque origin 下官方客户端首个抛错点的唯一修复面；JS 资源与 API 响应
    // 一律不动，CSP 随后的 `apply_sandbox_headers` 保持不变（仍无 allow-same-origin）。
    let injected = boot_html;
    let body_bytes = if injected {
        inject_storage_shim(bytes)
    } else {
        bytes
    };
    let body_len = body_bytes.len();
    let mut response = Response::new(Body::from(body_bytes));
    *response.status_mut() = status;
    copy_console_response_headers(&upstream_headers, &mut response);
    if let Some(value) = if head_request {
        upstream_length
    } else {
        HeaderValue::from_str(&body_len.to_string()).ok()
    } {
        response.headers_mut().insert(header::CONTENT_LENGTH, value);
    }
    if let Some(location) = location {
        response.headers_mut().insert(header::LOCATION, location);
    }
    apply_sandbox_headers(&mut response);
    response
}

fn copy_console_response_headers(headers: &header::HeaderMap, response: &mut Response) {
    for (name, value) in headers {
        // 凭据、浏览器来源策略和跨站状态由挂载控制。
        if matches!(
            name.as_str(),
            "set-cookie"
                | "set-cookie2"
                | "connection"
                | "keep-alive"
                | "trailer"
                | "transfer-encoding"
                | "upgrade"
                | "content-security-policy"
                | "content-length"
                | "location"
                | "refresh"
                | "clear-site-data"
                | "service-worker-allowed"
                | "alt-svc"
                | "strict-transport-security"
                | "www-authenticate"
        ) || name.as_str().starts_with("access-control-")
            || name.as_str().starts_with("proxy-")
        {
            continue;
        }
        response.headers_mut().append(name.clone(), value.clone());
    }
}

/// WS 升级：在固定上游同一路径完成握手，再双向透传。
///
/// 取消/到期**包围整个生命周期**（复审 Major-2）：不只消息循环里的一个分支。
/// 上游不完成握手、停止读取或浏览器停止读取时，「撤权即关闭」仍必须成立。
async fn upgrade(
    mount: &Arc<ConsoleMount>,
    token: &str,
    target: &str,
    request: axum::extract::Request,
) -> Response {
    use axum::extract::ws::WebSocketUpgrade;
    use axum::extract::FromRequest;

    // 原子授权：校验 + 登记活连接（与撤权互斥），拿到取消接收端。
    let Some((capability, mut cancel)) = mount.authorize_live(token) else {
        log::debug!(target: "dsh_console_ws", "upgrade denied (page or credential unavailable)");
        return forbidden();
    };
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let protocols = request
        .headers()
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .cloned();
    let Ok(mut upgrade) = WebSocketUpgrade::from_request(request, &()).await else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Ok(mut outgoing) = format!("ws://{}{target}", capability.authority).into_client_request()
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Ok(cookie) = HeaderValue::from_str(&cancel.cookie) else {
        return forbidden();
    };
    outgoing.headers_mut().insert(header::COOKIE, cookie);
    if let Some(offered) = protocols {
        outgoing
            .headers_mut()
            .insert(header::SEC_WEBSOCKET_PROTOCOL, offered);
    }
    let deadline = tokio::time::Instant::now()
        + std::time::Duration::from_millis(
            capability.expires_at_ms.saturating_sub(mount.now()).max(0) as u64,
        );
    let (upstream, response) = tokio::select! {
        biased;
        _ = wait_permit_revoked(&mut cancel) => return forbidden(),
        _ = tokio::time::sleep_until(deadline) => return forbidden(),
        result = tokio_tungstenite::connect_async(outgoing) => match result {
            Ok(connection) => connection,
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                log::info!(target: "dsh_console_ws", "upstream upgrade rejected status={}", response.status().as_u16());
                return response.status().into_response();
            }
            Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
        }
    };
    if let Some(selected) = response
        .headers()
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())
    {
        upgrade = upgrade.protocols([selected.to_owned()]);
    }
    let mount = Arc::clone(mount);
    upgrade.on_upgrade(move |browser| async move {
        bridge_websocket(browser, upstream, cancel, deadline, mount).await;
    })
}

type UpstreamWebSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// 双向桥：撤权或到期都能**抢占**任意 await（握手、发送、关闭）。
async fn bridge_websocket(
    mut browser: axum::extract::ws::WebSocket,
    upstream: UpstreamWebSocket,
    mut cancel: LivePermit,
    expire_at: tokio::time::Instant,
    mount: Arc<ConsoleMount>,
) {
    static ATTEMPT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let attempt = ATTEMPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let started = tokio::time::Instant::now();
    // M01：撤权/到期放在**整个转发 future 的外层**。
    //
    // 此前取消与期限参数穿过 `run_bridge`，在每个 await 点内联 select——那样
    // 「已经进入一笔阻塞写」的 transfer 无法被丢弃：`send` 的 future 被内层
    // select 借走，外层没有句柄可以整块丢。改成外层持有 transfer future 后，
    // 撤销直接丢弃它（含握手、读、写），连接随之释放；`run_bridge` 不再接收
    // 取消参数，避免同一接收端被内外重复可变借用。
    let end = {
        let transfer = run_bridge(&mut browser, upstream, &mount);
        tokio::pin!(transfer);
        tokio::select! {
            biased;
            reason = wait_permit_revoked(&mut cancel) => (reason, None),
            _ = tokio::time::sleep_until(expire_at) => ("capability_expired", None),
            result = &mut transfer => result,
        }
    };
    let elapsed_ms = started.elapsed().as_millis();
    // 关闭浏览器端；不依赖上游配合（Close 只是礼貌，不是退出前提）。
    {
        use futures::SinkExt as _;
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), browser.close()).await;
    }
    drop(browser);
    drop(cancel);
    // 先释放连接，再输出诊断。不记录地址、路径、凭据、帧内容或关闭理由字符串。
    log::info!(target: "dsh_console_ws", "bridge={attempt} elapsed_ms={elapsed_ms} end={} code={:?}", end.0, end.1);
}

async fn wait_permit_revoked(permit: &mut LivePermit) -> &'static str {
    tokio::select! {
        biased;
        _ = wait_revoked(&mut permit.gate_cancel) => "credential_revoked",
        _ = wait_revoked(&mut permit.cancel) => "page_revoked",
    }
}

/// 等待取消信号被置位（含「已置位」的历史值）。
async fn wait_revoked(cancel: &mut tokio::sync::watch::Receiver<bool>) {
    loop {
        if *cancel.borrow_and_update() {
            return;
        }
        if cancel.changed().await.is_err() {
            return;
        }
    }
}

/// 主体：上游握手 + 双向转发。所有 await 都受 `expire_at` 与取消信号保护。
async fn run_bridge(
    browser: &mut axum::extract::ws::WebSocket,
    upstream: UpstreamWebSocket,
    _mount: &ConsoleMount,
) -> (&'static str, Option<u16>) {
    use axum::extract::ws::Message;
    use futures::{SinkExt, StreamExt};
    let (upstream_tx, mut upstream_rx) = upstream.split();
    #[cfg(feature = "dsh-test-probes")]
    let mut upstream_tx: TestUpstreamSink = match &_mount.sink_wrapper {
        Some(wrapper) => wrapper(Box::pin(upstream_tx)),
        None => Box::pin(upstream_tx),
    };
    #[cfg(not(feature = "dsh-test-probes"))]
    let mut upstream_tx = upstream_tx;
    loop {
        tokio::select! {
            inbound = browser.next() => {
                match inbound {
                    Some(Ok(Message::Text(text))) => {
                        if upstream_tx.send(tokio_tungstenite::tungstenite::Message::Text(text.as_str().into())).await.is_err() {
                            return ("upstream_write_failed", None);
                        }
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        if upstream_tx.send(tokio_tungstenite::tungstenite::Message::Binary(bytes)).await.is_err() {
                            return ("upstream_write_failed", None);
                        }
                    }
                    Some(Ok(Message::Ping(bytes))) => {
                        if upstream_tx
                            .send(tokio_tungstenite::tungstenite::Message::Ping(bytes))
                            .await
                            .is_err()
                        {
                            return ("upstream_ping_write_failed", None);
                        }
                    }
                    Some(Ok(Message::Close(frame))) => {
                        let code = frame.as_ref().map(|frame| frame.code);
                        let close = frame.map(|frame| tokio_tungstenite::tungstenite::protocol::CloseFrame { code: frame.code.into(), reason: frame.reason.as_str().into() });
                        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), upstream_tx.send(tokio_tungstenite::tungstenite::Message::Close(close))).await;
                        return ("browser_close", code);
                    }
                    None => return ("browser_eof", None),
                    Some(Err(_)) => return ("browser_read_failed", None),
                    Some(Ok(Message::Pong(bytes))) => {
                        if upstream_tx.send(tokio_tungstenite::tungstenite::Message::Pong(bytes)).await.is_err() { return ("upstream_write_failed", None); }
                    }
                }
            }
            outbound = upstream_rx.next() => {
                match outbound {
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                        if browser.send(Message::Text(text.to_string().into())).await.is_err() {
                            return ("browser_write_failed", None);
                        }
                    }
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(bytes))) => {
                        if browser.send(Message::Binary(bytes)).await.is_err() {
                            return ("browser_write_failed", None);
                        }
                    }
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Ping(bytes))) => {
                        if browser.send(Message::Ping(bytes)).await.is_err() {
                            return ("browser_write_failed", None);
                        }
                    }
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Close(frame))) => {
                        let code = frame.as_ref().map(|frame| u16::from(frame.code));
                        let close = frame.map(|frame| axum::extract::ws::CloseFrame { code: frame.code.into(), reason: frame.reason.as_str().into() });
                        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), browser.send(Message::Close(close))).await;
                        return ("upstream_close", code);
                    }
                    None => return ("upstream_eof", None),
                    Some(Err(_)) => return ("upstream_read_failed", None),
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Pong(bytes))) => {
                        if browser.send(Message::Pong(bytes)).await.is_err() { return ("browser_write_failed", None); }
                    }
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}

fn forbidden() -> Response {
    let mut response = StatusCode::FORBIDDEN.into_response();
    apply_sandbox_headers(&mut response);
    response
}

/// 所有控制台响应都携带沙箱策略：即使有人直接导航到能力 URL，
/// 脚本仍在 opaque origin 内，拿不到主站权限域。
fn apply_sandbox_headers(response: &mut Response) {
    // opaque origin 需要显式 CORS 才能读到响应体（与 CSP 的 sandbox 配套）。
    apply_cors(response);
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(SANDBOX_CSP),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
}

// 页面通道不再读 API 凭据（见 `credential_for_route`）：官方 `admit()` 用浏览器
// cookie 认作 `browserPeer`，而 API 凭据会把页面挂到 `/connect` 的共享单 Peer 上、
// 与插件争用同一条 WS。API 凭据仍保存在 `console_lifecycle`（插件自己用）。

/// 供路由装配复用：能力校验后的上游 authority（WS 子协议探测等）。
pub fn upstream_for(mount: &ConsoleMount, token: &str) -> Option<String> {
    mount.lookup(token).map(|record| record.authority)
}

/// 生产转发仅检查路径形状，业务路由由固定上游处理。
fn validated_upstream_path(path: &str) -> Option<String> {
    let path = percent_decode(path)?;
    let valid = path.starts_with('/')
        && !path.starts_with("//")
        && !path.contains(['\\', '\0', '\r', '\n'])
        && !path.split('/').any(|segment| matches!(segment, "." | ".."));
    valid.then_some(path)
}

/// 最小百分号解码（只处理 `%XX`；非法序列返回 None）。
fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            out.push(bytes[index]);
            index += 1;
            continue;
        }
        if index + 2 >= bytes.len() {
            return None;
        }
        let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok()?;
        let value = u8::from_str_radix(hex, 16).ok()?;
        out.push(value);
        index += 3;
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install_test_credential(mount: &ConsoleMount, instance: &str, authority: &str) {
        mount
            .install_browser_credential(
                instance,
                1,
                authority,
                "browser=test".into(),
                crate::services::dsh_control::transport::PeerGate::new(),
            )
            .unwrap();
    }

    fn bootstrap_record(mount: &ConsoleMount, authority: &str, epoch: u64) -> ConsoleCapability {
        ConsoleCapability {
            token: "tok-entry".into(),
            device_id: "dev-a".into(),
            instance_id: "inst-a".into(),
            authority: authority.into(),
            epoch,
            bootstrap: true,
            expires_at_ms: mount.now() + 60_000,
        }
    }

    /// T3（移动端四态映射契约）：签发登记的拒绝原因必须**按因分码**——设备失效
    /// 与换代/目标失效是两种用户可操作状态（重新配对 vs 稍后重试），不得折叠成
    /// 同一个 `console_target_invalid`。
    #[test]
    fn entry_registration_refuses_device_and_generation_with_distinct_reasons() {
        // 设备失效（device_check 拒绝）→ console_device_invalid。
        let mount = ConsoleMount::new()
            .with_device_check(|_| false)
            .with_target_check(|_, _, _| true);
        let error = mount
            .register_if_current(
                bootstrap_record(&mount, UNREACHABLE, 1),
                mount.revocation_sequence(),
            )
            .expect_err("设备失效必须拒绝");
        assert_eq!(
            error, "console_device_invalid",
            "设备失效必须是设备码：{error}"
        );
        // 设备有效、代际/目标失效 → console_target_invalid。
        let mount = ConsoleMount::new()
            .with_device_check(|_| true)
            .with_target_check(|_, _, _| false);
        let error = mount
            .register_if_current(
                bootstrap_record(&mount, UNREACHABLE, 1),
                mount.revocation_sequence(),
            )
            .expect_err("代际失效必须拒绝");
        assert_eq!(
            error, "console_target_invalid",
            "代际失效必须是目标码：{error}"
        );
    }

    fn mount_with(authority: &str) -> Arc<ConsoleMount> {
        // 默认构造是 fail-closed（设备校验恒拒绝）；单测显式放行其设备。
        let mount = ConsoleMount::new().with_device_check(|_| true);
        install_test_credential(&mount, "inst-a", authority);
        mount
            .register(ConsoleCapability {
                token: "tok".into(),
                device_id: "dev-a".into(),
                instance_id: "inst-a".into(),
                authority: authority.into(),
                epoch: 1,
                bootstrap: false,
                expires_at_ms: mount.now() + 60_000,
            })
            .unwrap();
        Arc::new(mount)
    }

    /// 权威不可达：任何「先连上游再判路由」的实现都会得到 502 而不是 403。
    const UNREACHABLE: &str = "127.0.0.1:1";

    #[tokio::test]
    async fn unknown_capability_is_forbidden_with_sandbox_policy() {
        let mount = mount_with(UNREACHABLE);
        let response = serve(&mount, "nope", "/", None).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(csp(&response).contains("sandbox"));
    }

    #[test]
    fn capability_without_browser_credential_is_rejected() {
        let mount = ConsoleMount::new().with_device_check(|_| true);
        mount
            .register(ConsoleCapability {
                token: "tok".into(),
                device_id: "dev-a".into(),
                instance_id: "inst-a".into(),
                authority: UNREACHABLE.into(),
                epoch: 1,
                bootstrap: false,
                expires_at_ms: mount.now() + 60_000,
            })
            .unwrap();
        assert!(mount.authorize_live("tok").is_none());
    }

    #[tokio::test]
    async fn expired_capability_is_forbidden() {
        let mount = ConsoleMount::new().with_device_check(|_| true);
        install_test_credential(&mount, "inst-a", UNREACHABLE);
        mount
            .register(ConsoleCapability {
                token: "tok".into(),
                device_id: "dev-a".into(),
                instance_id: "inst-a".into(),
                authority: UNREACHABLE.into(),
                epoch: 1,
                bootstrap: false,
                expires_at_ms: mount.now() - 1,
            })
            .unwrap();
        let mount = Arc::new(mount);
        assert_eq!(
            serve(&mount, "tok", "/", None).await.status(),
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn traversal_is_rejected_before_any_upstream_call() {
        let mount = mount_with(UNREACHABLE);
        for path in [
            "/api/../x",
            "/%2e%2e/x",
            "//host/x",
            "/a%5cb",
            "/a%00b",
            "/a%zz",
            "/a%",
        ] {
            assert_eq!(
                serve(&mount, "tok", path, None).await.status(),
                StatusCode::FORBIDDEN,
                "path {path:?} 必须在连接上游之前被拒绝"
            );
        }
    }

    #[tokio::test]
    async fn allowed_route_reaches_upstream_and_reports_gateway_failure() {
        // 允许路径 + 不可达权威 = 502：证明路由放行后确实尝试了上游
        // （而不是把「连不上」也说成「路由非法」）。
        // 绑定但不监听，独占一个本地端口，避免连接到机器上的其他服务。
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let authority = socket.local_addr().unwrap().to_string();
        let instance_id = "test-unreachable-session-list";
        let mount = ConsoleMount::new().with_device_check(|_| true);
        install_test_credential(&mount, instance_id, &authority);
        mount
            .register(ConsoleCapability {
                token: "tok".into(),
                device_id: "dev-a".into(),
                instance_id: instance_id.into(),
                authority,
                epoch: 1,
                bootstrap: false,
                expires_at_ms: mount.now() + 60_000,
            })
            .unwrap();
        let mount = Arc::new(mount);
        assert_eq!(
            serve_with(
                &mount,
                "tok",
                "/api/session/list",
                Some((
                    axum::http::Method::POST,
                    Vec::new(),
                    header::HeaderMap::new()
                )),
            )
            .await
            .status(),
            StatusCode::BAD_GATEWAY
        );
    }

    #[tokio::test]
    async fn responses_always_carry_opaque_sandbox_policy() {
        let mount = mount_with(UNREACHABLE);
        use tower::ServiceExt;
        let response = router(mount)
            .oneshot(
                axum::http::Request::builder()
                    .uri("/console/tok/settings")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let policy = csp(&response);
        assert!(policy.contains("sandbox"), "必须声明 sandbox: {policy}");
        assert!(
            !policy.contains("allow-same-origin"),
            "绝不能给控制台同源权限: {policy}"
        );
        assert!(
            response.headers().get(header::CACHE_CONTROL).is_some(),
            "控制台响应不得被缓存"
        );
    }

    /// 撤某个实例**不得**打断别的实例正在用的控制台。
    #[tokio::test]
    async fn instance_revocation_does_not_cancel_other_instances() {
        let mount = ConsoleMount::new().with_device_check(|_| true);
        for (token, instance) in [("tok-a", "inst-a"), ("tok-b", "inst-b")] {
            install_test_credential(&mount, instance, UNREACHABLE);
            mount
                .register(ConsoleCapability {
                    token: token.into(),
                    device_id: "dev-a".into(),
                    instance_id: instance.into(),
                    authority: UNREACHABLE.into(),
                    epoch: 1,
                    bootstrap: false,
                    expires_at_ms: mount.now() + 60_000,
                })
                .unwrap();
        }
        let mount = Arc::new(mount);
        // 走与真实 WS 升级同一条原子路径登记活连接。
        let (_, mut watcher_a) = mount.authorize_live("tok-a").expect("A 授权");
        let (_, mut watcher_b) = mount.authorize_live("tok-b").expect("B 授权");

        mount.revoke_instance("inst-a");

        assert!(*watcher_a.borrow_and_update(), "被撤实例的活连接必须取消");
        assert!(
            !*watcher_b.borrow_and_update(),
            "其他实例的活连接不得被误伤"
        );
        assert!(upstream_for(&mount, "tok-a").is_none(), "被撤实例能力失效");
        assert!(upstream_for(&mount, "tok-b").is_some(), "其他实例能力保留");
    }

    #[tokio::test]
    async fn revocation_drops_capabilities_and_cancels_live_connections() {
        let mount = mount_with(UNREACHABLE);
        let (_, mut watcher) = mount.authorize_live("tok").expect("授权");
        mount.revoke_instance("inst-a");
        assert!(upstream_for(&mount, "tok").is_none(), "能力必须失效");
        assert!(*watcher.borrow_and_update(), "活连接必须收到取消信号");
    }

    /// P1-4 close-pages 原语：只取消该实例的活连接，凭据与能力**全部保留**——
    /// 切走关页 ≠ 撤权（撤权是 revoke_instance 的独立动作）。
    #[test]
    fn close_instance_pages_cancels_only_live_connections() {
        let mount = ConsoleMount::new().with_device_check(|_| true);
        install_test_credential(&mount, "inst-a", UNREACHABLE);
        mount
            .register(ConsoleCapability {
                token: "tok-a".into(),
                device_id: "dev-a".into(),
                instance_id: "inst-a".into(),
                authority: UNREACHABLE.into(),
                epoch: 1,
                bootstrap: false,
                expires_at_ms: mount.now() + 60_000,
            })
            .unwrap();
        let mount = Arc::new(mount);
        let (_, mut watcher) = mount.authorize_live("tok-a").expect("活连接登记");
        // 关页前采样的登记序号（模拟探测中的在飞准入）。
        let sampled_before_close = mount.revocation_sequence();

        mount.close_instance_pages("inst-a");

        assert!(*watcher.borrow_and_update(), "活页面必须被取消");
        // 凭据保留：同目标重登记能力仍然成功（register_if_current 校验凭据）。
        let record = mount
            .lookup_for_test("tok-a")
            .expect("能力保留（未随关页撤销）");
        mount
            .register_if_current(
                ConsoleCapability {
                    token: "tok-a-return".into(),
                    device_id: record.device_id.clone(),
                    instance_id: record.instance_id.clone(),
                    authority: record.authority.clone(),
                    epoch: record.epoch,
                    bootstrap: false,
                    expires_at_ms: mount.now() + 60_000,
                },
                mount.revocation_sequence(),
            )
            .expect("凭据保留 → 切回可直接登记控制台入口");
        // 关页后再准入（浏览器重连）会重新登记为活连接（保留凭据的既定边界）。
        let _reconnect = mount.authorize_live("tok-a").expect("重连重新准入");
        // 关页 bump 撤销序号：关页前采样的登记会被拒（在飞准入不漏取消）。
        let stale = mount.register_if_current(
            ConsoleCapability {
                token: "tok-stale".into(),
                device_id: "dev-a".into(),
                instance_id: "inst-a".into(),
                authority: UNREACHABLE.into(),
                epoch: 1,
                bootstrap: false,
                expires_at_ms: mount.now() + 60_000,
            },
            sampled_before_close,
        );
        assert!(stale.is_err(), "关页前采样的入口登记必须被作废");
    }

    /// Major-1 回归：撤权发生在「设备校验之后、登记之前」时必须仍被拦住。
    ///
    /// 这是两张表各自加锁时最危险的窗口——请求已通过校验，撤权刚完成，
    /// 请求随后才登记，于是这条连接永远收不到取消信号。同锁后必须二者必居其一：
    /// 要么拒绝授权，要么登记后立刻收到取消。
    ///
    /// 关键在于**真的在 `device_valid` 内部触发撤权**：这个闭包运行在锁外，
    /// 正是原实现里窗口打开的位置。若实现没有在锁内复核，授权仍会成功，
    /// 而那次撤权（在登记之前发生）不会取消它——本用例随即变红。
    #[test]
    fn revocation_inside_device_check_cannot_leave_an_uncancelled_connection() {
        // 设备校验闭包需要回调挂载自身，用 `Arc` + `Weak` 打破循环引用：
        // `device_valid` 持有 Weak，调用时升级。
        let mount =
            std::sync::Arc::new(std::sync::Mutex::new(None::<std::sync::Weak<ConsoleMount>>));
        let slot = std::sync::Arc::clone(&mount);
        let built = ConsoleMount::new().with_device_check(move |_| {
            if let Some(weak) = slot.lock().unwrap().as_ref() {
                if let Some(mount) = weak.upgrade() {
                    // device_valid 内部完成撤权，然后报告「设备有效」
                    // （比真实撤权更苛刻）。
                    mount.revoke_all();
                }
            }
            true
        });
        let built = std::sync::Arc::new(built);
        *mount.lock().unwrap() = Some(std::sync::Arc::downgrade(&built));
        let mount = built;
        install_test_credential(&mount, "inst-a", UNREACHABLE);
        mount
            .register(ConsoleCapability {
                token: "tok".into(),
                device_id: "dev-a".into(),
                instance_id: "inst-a".into(),
                authority: UNREACHABLE.into(),
                epoch: 1,
                bootstrap: false,
                expires_at_ms: mount.now() + 60_000,
            })
            .unwrap();

        let outcome = mount.authorize_live("tok");
        assert!(
            outcome.is_none(),
            "撤权发生在设备校验期间时，授权必须在锁内复核后被拒——\
             否则这条连接收不到取消信号（撤权已完成、它却在之后才登记）"
        );
    }

    /// 反向顺序：先登记后撤权，连接必须真的被取消（不是泄漏）。
    #[test]
    fn revocation_after_registration_cancels_the_live_connection() {
        let mount = ConsoleMount::new().with_device_check(|_| true);
        install_test_credential(&mount, "inst-a", UNREACHABLE);
        mount
            .register(ConsoleCapability {
                token: "tok".into(),
                device_id: "dev-a".into(),
                instance_id: "inst-a".into(),
                authority: UNREACHABLE.into(),
                epoch: 1,
                bootstrap: false,
                expires_at_ms: mount.now() + 60_000,
            })
            .unwrap();
        let (_, mut live) = mount.authorize_live("tok").expect("授权");
        mount.revoke_all();
        assert!(*live.borrow_and_update(), "撤权必须取消已登记连接");
    }

    fn csp(response: &Response) -> String {
        response
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    }
    /// M02 兑换核心语义的**单元级**回归（不经过 HTTP/gate 栈）。
    ///
    /// 为什么单独放在这一层：挂载层的设备绑定是「票据绑设备」的最终保证。走 HTTP
    /// 测时，gate 会先按设备有效性挡一道，导致「挂载层是否真的比对了设备」这件事
    /// 被上游行为掩盖——实测：删掉挂载层的 device 过滤，HTTP 级用例仍然全绿。
    /// 这一层直接调用函数，变异必定可见。
    #[test]
    fn redeem_binds_the_ticket_to_the_issuing_device_and_consumes_it_once() {
        let mount = ConsoleMount::new().with_device_check(|_| true);
        let token = "ticket-under-test".to_string();
        install_test_credential(&mount, "inst-a", "127.0.0.1:5");
        mount
            .register(ConsoleCapability {
                token: token.clone(),
                device_id: "dev-a".into(),
                instance_id: "inst-a".into(),
                authority: "127.0.0.1:5".into(),
                epoch: 1,
                bootstrap: true,
                expires_at_ms: mount.now() + 60_000,
            })
            .expect("票据可登记");

        // 错设备：拒绝，且**不消费**。
        assert!(
            mount.redeem_bootstrap_for_device(&token, "dev-b").is_none(),
            "错设备必须被拒"
        );
        // 正确设备仍可兑换（错设备的尝试没有烧掉票据）。
        let session = mount
            .redeem_bootstrap_for_device(&token, "dev-a")
            .expect("错设备失败后，正确设备仍应能兑换");
        // 一次性：第二次失败。
        assert!(
            mount.redeem_bootstrap_for_device(&token, "dev-a").is_none(),
            "票据一次性：第二次兑换必须失败"
        );
        // 兑换出的会话不是 bootstrap（否则可以再次被兑换）。
        let record = mount.lookup_for_test(&session).expect("会话应存在");
        assert!(!record.bootstrap, "兑换出的必须是页面会话，不是引导");
        assert_eq!(record.device_id, "dev-a", "会话继承签发设备");
        assert_eq!(record.epoch, 1, "会话继承签发代际");
    }

    /// 过期票据不得兑换。
    #[test]
    fn an_expired_ticket_cannot_be_redeemed() {
        let mount = ConsoleMount::new().with_device_check(|_| true);
        install_test_credential(&mount, "inst-a", "127.0.0.1:5");
        mount
            .register(ConsoleCapability {
                token: "expired-ticket".into(),
                device_id: "dev-a".into(),
                instance_id: "inst-a".into(),
                authority: "127.0.0.1:5".into(),
                epoch: 1,
                bootstrap: true,
                expires_at_ms: mount.now() - 1,
            })
            .expect("票据可登记");
        assert!(
            mount
                .redeem_bootstrap_for_device("expired-ticket", "dev-a")
                .is_none(),
            "过期票据必须拒绝"
        );
    }

    // ===== T06：opaque origin 下的存储替身注入 =====

    #[test]
    fn storage_shim_is_injected_before_any_official_script() {
        let html = br#"<!doctype html><html><head><script src="./assets/app.js"></script></head><body></body></html>"#.to_vec();
        let out = String::from_utf8(inject_storage_shim(html)).unwrap();
        let shim_at = out
            .find("Object.defineProperty(window,\"localStorage\"")
            .expect("替身必须被注入");
        let script_at = out.find("assets/app.js").expect("官方脚本仍在");
        assert!(
            shim_at < script_at,
            "替身必须先于官方脚本执行（否则首个 SecurityError 仍会发生）"
        );
        // 只替换存储能力：不改 origin 策略
        assert!(!out.contains("allow-same-origin"), "不得借注入放宽沙箱");
    }

    #[test]
    fn storage_shim_keeps_existing_usable_storage_untouched() {
        let html = b"<html><head></head><body></body></html>".to_vec();
        let out = String::from_utf8(inject_storage_shim(html)).unwrap();
        // 替身自带探测：可用存储原样返回，不做替换
        assert!(
            out.contains("if(window.localStorage){window.localStorage.getItem"),
            "真实存储可用时必须直接返回，不安装替身"
        );
        assert!(out.contains(r#"Object.defineProperty(window,"localStorage""#));
        assert!(out.contains("sessionStorage"));
        assert!(out.contains("new Map()"), "替身必须是内存态，不落盘");
    }

    #[test]
    fn storage_shim_falls_back_to_document_start_without_head() {
        let html = b"<body><p>no head</p></body>".to_vec();
        let out = String::from_utf8(inject_storage_shim(html)).unwrap();
        assert!(
            out.starts_with("<script>"),
            "无 head 时插到文档最前: {out:.80}"
        );
        assert!(out.contains("no head"), "原文必须保留");
    }

    #[test]
    fn storage_shim_leaves_non_html_bytes_alone() {
        let bytes = vec![0xff, 0xfe, 0x00, 0x01];
        assert_eq!(
            inject_storage_shim(bytes.clone()),
            bytes,
            "非 UTF-8 原样返回"
        );
    }

    #[test]
    fn storage_shim_is_skipped_for_oversized_documents() {
        let mut html = Vec::new();
        html.extend_from_slice(b"<html><head></head><body>");
        html.extend(std::iter::repeat_n(b'x', 600 * 1024));
        html.extend_from_slice(b"</body></html>");
        let original = html.clone();
        assert_eq!(inject_storage_shim(html), original, "超预算不改写");
    }
}
