//! 本机双向身份授权与可撤绑定（T06；计划 §2.5「授权握手落点」+ §2.3
//! `auth::{authorize,revoke}` 生产入口）。
//!
//! ## 握手设计（bindingProtocol = 1）
//!
//! * **角色**：Rust = 本机 IPC **服务端**（[`local_ipc`]：UDS/命名管道，当前用户
//!   限定），在 `<app_data_home>/.tuvis/dsh/`（覆盖变量 `TUVIS_DSH_STATE_DIR`）
//!   发布**rendezvous 记录**（原子替换、非敏感：随机 IPC 地址 + 协议代际 + Rust
//!   侧进程身份 + Rust 公钥；**绝不含 owner 凭据**）；插件（DSH 宿主内）读
//!   rendezvous **主动连接**。
//! * **OS 层对端核验**：真实 peer PID 来自内核（UDS peercred / 管道 sid），叠加
//!   进程表扫描确认该 PID 就是唯一 DSH Desktop 宿主（复用 T02 双特征判定），
//!   并与客户端自报的 PID/启动时刻/可执行文件三对——不一致即
//!   `DSH_PEER_MISMATCH`（PID 复用/路径冒充不得通过）。
//! * **首次绑定（TOFU + 人工确认 + 两端核对码）**：新公钥的客户端只会得到
//!   `pending_approval` + [`BindingChallenge`]（binding_id / display_code /
//!   expires_at_ms）；本地设置卡与插件本地 UI 显示**同一 display_code**，用户在
//!   **兔维斯 UI 批准**后才固定对端公钥（`approve`）。待批记录**按客户端公钥
//!   幂等**（R04）：插件每 3 秒重发 hello，同一未批准公钥在自己那份挑战未过期
//!   期间恒拿回同一对 binding_id + display_code；只有到期（60 秒 TTL）才换新——
//!   否则两端核对码翻动，人工核对完批准时批的是旧 id。
//! * **每代握手**：Ed25519 签名挑战。nonce 32 字节、一次性、TTL 60 秒；签名
//!   消息 = [`BindingChallengeMessage`] 固定 6 字段（bindingProtocol / 双公钥
//!   SHA-256 摘要 / nonce / hostId / epoch / expiry）的紧凑 UTF-8 JSON——
//!   **绝不签任意对象序列化**。公钥走 SPKI DER、签名 64 字节 raw 的 base64url。
//!   Rust 侧验签用 ed25519-dalek；插件侧用 Node 内置 crypto（同曲线同消息）。
//! * **凭据方向**：插件在自己的 Host 内兑换官方 API Cookie（注入式
//!   exchange，真实 /connect 属 G1 实机验证项），并**只经已授权的本机 IPC**
//!   交付给 Rust（[`AuthorizedPeer`] 秘密句柄，仅内存）。批准前服务端对客户端
//!   **零凭据写出**，也未接受过任何 cookie 字节。
//! * **撤权**（[`DshAuthService::revoke`]）：立即关受信 Peer 连接、清钉公钥与
//!   已存凭据、拒绝自动重连（撤权后的公钥回到「待重新绑定」）；已放行的原生
//!   操作不承诺回滚（如实申报）。
//!
//! ## 持久化与秘密纪律
//!
//! * `<app_data_home>/.tuvis/dsh/authorization.key`：Rust 授权私钥（32 字节种子，0600）；
//! * `<app_data_home>/.tuvis/dsh/bindings.json`：绑定账本（**非秘密**元数据 + 钉公钥，0600）；
//! * `<app_data_home>/.tuvis/dsh/rendezvous.json`：非敏感、原子替换（0600）；
//!   （以上三项的实际根：设了 `TUVIS_DSH_STATE_DIR` 时**恰好**是该目录而非
//!   `<app_data_home>/.tuvis/dsh`——变量即状态目录，插件到同一目录读 rendezvous。）
//! * owner Cookie 只在内存（[`AuthorizedPeer`] 无 Serialize、Debug 全脱敏）；
//!   display_code 是短时两端核对码，**不是**长效 owner 凭据。
//!
//! 全部 IO 走注入 [`DshEnvironment`]（测试注入 tempdir/假时钟/假进程源；生产
//! 装配在 `production.rs`，manager 与 auth 共用同一 env Arc）。

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::error::{codes, DshError};
use super::identity;
use super::install::{BindingRevoker, PACKAGE_NAME};
use super::local_ipc::{IpcClientConnection, IpcListener};
use super::model::{
    AuthorizationState, AuthorizedPeer, BindingChallenge, ClientIdentity, DshEnvironment, LocalHost,
};
use super::transport::PeerGate;

// ============================================================
// 协议常量
// ============================================================

/// 本机授权握手协议代际（与插件 `BINDING_PROTOCOL`、manifest 逐字一致）。
pub const BINDING_PROTOCOL: u32 = 2;
/// 挑战 TTL（计划全局约束：挑战 nonce 60 秒一次性）。
pub const CHALLENGE_TTL_MS: i64 = 60_000;
/// expiry 允许的时钟slack（对端与本端时钟微小漂移；超出即拒）。
const EXPIRY_SLACK_MS: i64 = 1_000;
/// 一次性 nonce 环形容量（超容量整体退役；正常节奏远用不满）。
const NONCE_CAPACITY: usize = 4096;
/// 待批记录容量（按客户端公钥各一条；超容量退役最旧的一条）。
///
/// 为什么要上限：待批记录由**未批准的客户端**驱动，每把新公钥都会产生一条，无上限
/// 就是一个可被本机任意进程（能连上 UDS 的当前用户）撑爆的内存面。8 条足够覆盖
/// 「用户先后在多台 profile / 多次重装插件」的核对场景，超出时如实退役最旧的。
const PENDING_CAPACITY: usize = 8;
/// display_code 字母表（去掉易混字符的 30 字符集）。
const DISPLAY_ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";
/// Ed25519 SPKI DER 的固定 12 字节前缀（AlgorithmIdentifier Ed25519 + BIT WRAP）。
const ED25519_SPKI_PREFIX: [u8; 12] = [
    // BIT STRING 长度字节是 0x21（=33：32 字节密钥 + 0 个未用位）。曾误写 0x42
    //（=66，非法 DER）：自身 SPKI 畸形且拒绝一切合法客户端 hello——单测两侧共用
    // 同一 wrap 常量故从未抓获，2026-10-30 G1-Mac 真机首握当场暴露。
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

// ============================================================
// 密钥与编码（基础设施）
// ============================================================

/// OS 随机 u64（种子/绑定号/display_code 的唯一随机源；rand 0.10 已有依赖）。
pub fn random_u64() -> u64 {
    rand::random::<u64>()
}

/// base64url 无填充编码。
pub fn b64url_encode(bytes: &[u8]) -> String {
    B64URL.encode(bytes)
}

/// base64url 无填充解码（畸形 → `DSH_AUTH_PROTOCOL`）。
pub fn b64url_decode(text: &str) -> Result<Vec<u8>, DshError> {
    B64URL.decode(text).map_err(|e| {
        DshError::new(
            codes::AUTH_PROTOCOL,
            format!("base64url decode failed: {e}"),
        )
    })
}

/// SPKI DER 包皮：固定前缀 + 32 字节 raw（严格长度；Ed25519 的 SPKI 是常量壳）。
pub fn spki_wrap_raw(raw: &[u8; 32]) -> Vec<u8> {
    let mut der = Vec::with_capacity(44);
    der.extend_from_slice(&ED25519_SPKI_PREFIX);
    der.extend_from_slice(raw);
    der
}

/// SPKI DER 拆皮（形状不符 → `DSH_AUTH_PROTOCOL`；不猜）。
pub fn spki_unwrap(der: &[u8]) -> Result<[u8; 32], DshError> {
    if der.len() != 44 || der[..12] != ED25519_SPKI_PREFIX {
        return Err(DshError::new(
            codes::AUTH_PROTOCOL,
            format!("not an Ed25519 SPKI DER ({} bytes)", der.len()),
        ));
    }
    let mut raw = [0u8; 32];
    raw.copy_from_slice(&der[12..]);
    Ok(raw)
}

/// SPKI DER 的 SHA-256 十六进制摘要（双公钥摘要的字段来源）。
pub fn key_digest(spki: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(spki);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 两端核对码（6 字符；短时比对用，不是长效凭据）。
pub fn display_code() -> String {
    (0..6)
        .map(|_| DISPLAY_ALPHABET[(random_u64() as usize) % DISPLAY_ALPHABET.len()] as char)
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HomeBinding {
    pub home_root: String,
    pub host_pid: u32,
    pub host_started_at_ms: i64,
}

/// 签名消息（固定字段与顺序；serde_json 紧凑 UTF-8 JSON 即被签字节）。
///
/// `key_digests = [Rust 公钥摘要, 客户端公钥摘要]`（计划「双公钥摘要」一个字段槽）；
/// `epoch` 用十进制**字符串**承载（u64 全域超出 JS Number 安全整数，两侧逐字节
/// 对齐是签名成立的前提）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BindingChallengeMessage {
    pub binding_protocol: u32,
    pub key_digests: [String; 2],
    pub nonce: String,
    pub host_id: String,
    pub epoch: String,
    pub expiry: i64,
    pub home_binding: HomeBinding,
}

impl BindingChallengeMessage {
    /// 被签字节（紧凑 JSON；两侧都必须用同一实现产出）。
    pub fn to_signing_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("BindingChallengeMessage 序列化不可失败")
    }
}

/// Rust 侧 Ed25519 签名密钥（种子派生；SPKI DER + 摘要随行）。
pub struct RustSigningKey {
    signing: SigningKey,
    /// SPKI DER（进 rendezvous / serverHello）。
    pub spki: Vec<u8>,
    /// 公钥 SHA-256 摘要（进签名消息 keyDigests[0]）。
    pub digest: String,
}

impl RustSigningKey {
    /// 生成新密钥（OS 随机种子）。
    pub fn generate() -> Self {
        let mut seed = [0u8; 32];
        for chunk in seed.chunks_mut(8) {
            let v = random_u64().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
        Self::from_seed(&seed)
    }

    /// 种子派生（加载既有密钥的唯一路径）。
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        let signing = SigningKey::from_bytes(seed);
        let spki = spki_wrap_raw(&signing.verifying_key().to_bytes());
        let digest = key_digest(&spki);
        Self {
            signing,
            spki,
            digest,
        }
    }

    /// 对消息签名（64 字节 raw）。
    pub fn sign(&self, msg: &[u8]) -> Vec<u8> {
        self.signing.sign(msg).to_bytes().to_vec()
    }

    /// 私钥种子（32 字节；只在内存中重建同一把密钥时使用，绝不落盘/入日志）。
    pub fn seed(&self) -> [u8; 32] {
        self.signing.to_bytes()
    }

    /// 验证客户端签名（SPKI DER → VerifyingKey；任何形状/验签失败 → Err）。
    pub fn verify_client(
        &self,
        client_spki: &[u8],
        msg: &[u8],
        sig: &[u8],
    ) -> Result<(), DshError> {
        let raw = spki_unwrap(client_spki)?;
        let pair = match VerifyingKey::from_bytes(&raw) {
            Ok(vk) => match Signature::from_slice(sig) {
                Ok(s) => Some((vk, s)),
                Err(_) => None,
            },
            Err(_) => None,
        };
        let ok = pair
            .map(|(vk, s)| vk.verify(msg, &s).is_ok())
            .unwrap_or(false);
        if ok {
            Ok(())
        } else {
            Err(DshError::new(
                codes::AUTH_SIGNATURE,
                "ed25519 challenge signature did not verify",
            ))
        }
    }
}

// ============================================================
// wire 帧（NDJSON 一行一帧；camelCase 与 TS 镜像一致）
// ============================================================

/// 客户端帧（插件 → Rust）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientFrame {
    /// 握手 hello（自报身份 + 签名挑战）。
    Hello(HelloFrame),
    /// 凭据交付（只被已批准绑定接受；owner Cookie 只经本机 IPC）。
    Credential(CredentialFrame),
}

/// hello 帧（自报 + 挑战签名；服务端会逐项与 OS 层观测对表）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HelloFrame {
    pub binding_protocol: u32,
    /// 客户端公钥（SPKI DER 的 base64url）。
    pub public_key_spki: String,
    /// 自报可执行文件路径（须与进程表一致）。
    pub executable: String,
    /// 自报 PID（须与内核 peercred 一致）。
    pub pid: u32,
    /// 自报启动时刻（须与进程表一致；PID 复用防线）。
    pub started_at_ms: i64,
    pub host_id: String,
    /// 当前代际（十进制字符串；旧代际 → `DSH_AUTH_EPOCH_STALE`）。
    pub epoch: String,
    /// 32 字节一次性 nonce（base64url）。
    pub nonce: String,
    /// 挑战到期（epoch 毫秒；60 秒窗口）。
    pub expiry: i64,
    /// 宿主声明的有效数据根与本次握手代际，参与双向签名。
    pub home_binding: HomeBinding,
    /// Ed25519 签名（64 字节 raw 的 base64url；被签 = BindingChallengeMessage）。
    pub signature: String,
}

/// 凭据帧（owner Cookie 交付；仅受信 Peer 可发，服务端不入日志/DTO）。
///
/// **Debug 手工脱敏**（M-5）：`cookie` 是 owner 长效凭据，绝不能经 `{:?}` 落到
/// 任何日志/错误上下文/调试打印里。与 [`AuthorizedPeer`] / `RedeemedConnect` 同一
/// 纪律——**不 derive Debug**，改为输出 `<redacted:len=N>`（长度是既有脱敏谱系的
/// 口径，见 `transport::RedeemedConnect` 的 Debug）。`binding_id` 是非秘密元数据，
/// 撤权与排障都要它，故照常显示。
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CredentialFrame {
    pub binding_id: String,
    /// **API operator 凭据**（`/connect?interactions=answer` 的 302 set-cookie，
    /// payload `version: 2`）：只够开 `/api/remote.mux`。
    pub cookie: String,
    /// **浏览器会话 cookie**（`GET /?token=` 的 303 set-cookie，payload
    /// `version: 1`）：官方首页 `authorizeIndex` 只认这个版本。
    ///
    /// 可选：旧插件不发 → 控制台首页通道如实标为不可用（**绝不**拿 API 凭据顶替，
    /// 那只会得到一个恒未授权的页面）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_cookie: Option<String>,
}

impl std::fmt::Debug for CredentialFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialFrame")
            .field("binding_id", &self.binding_id)
            .field(
                "cookie",
                &format_args!("<redacted:len={}>", self.cookie.len()),
            )
            .field(
                "browser_cookie",
                &format_args!(
                    "<redacted:len={}>",
                    self.browser_cookie.as_ref().map(String::len).unwrap_or(0)
                ),
            )
            .finish()
    }
}

/// 服务端帧（Rust → 插件）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum ServerFrame {
    /// serverHello：握手应答（Rust 签名同一挑战消息；state 携带批准态）。
    ServerHello(ServerHelloFrame),
    /// 批准放行（用户在兔维斯 UI 批准后推给等待中的客户端）。
    Granted { binding_id: String },
    /// 撤权通知（服务端随后立即关闭连接）。
    Revoked { binding_id: String },
    /// 拒绝（机器码 + 英文诊断；detail 不含凭据）。
    Error { error: String, detail: String },
}

/// serverHello 帧（Rust 侧签名 + 批准态 + 首绑核对码）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerHelloFrame {
    pub binding_protocol: u32,
    /// Rust 公钥（SPKI DER base64url；与 rendezvous 一致，插件固定核验）。
    pub public_key_spki: String,
    /// 与 hello 相同的 nonce（回显）。
    pub nonce: String,
    pub expiry: i64,
    pub host_id: String,
    pub epoch: String,
    pub home_binding: HomeBinding,
    /// Rust 对同一 BindingChallengeMessage 的签名。
    pub signature: String,
    pub decision_signature: String,
    /// `pending_approval` | `authorized` | `revoked`。
    pub state: String,
    pub binding_id: Option<String>,
    /// 首绑时的两端核对码（仅 pending_approval）。
    pub display_code: Option<String>,
    /// 待批记录的**真实审批截止**（毫秒；仅 pending_approval）。
    ///
    /// 为什么需要它：有效期内同钥重连是幂等的（同一对 id/码），但应答里的 `expiry`
    /// 是**本次挑战**的新到期（now+60s），而记录的实际截止仍是首次那一个。插件若
    /// 按 `expiry` 武装到期定时器，T+55s 重连拿到的码会被当成还能再活 60 秒——显示
    /// 出来却是「已过期、批不了」的死码（点了就是 AUTH_EXPIRED）。
    ///
    /// 与批准状态、绑定号和核对码共同由 decision_signature 保护。
    pub pending_expires_at: Option<i64>,
}

/// 解析服务端帧（NDJSON 单行 → [`ServerFrame`]；测试客户端与 IPC 循环共用）。
pub fn parse_server_frame(line: &str) -> Result<ServerFrame, DshError> {
    serde_json::from_str(line).map_err(|e| {
        DshError::new(
            codes::AUTH_PROTOCOL,
            format!("server frame is not valid: {e}"),
        )
    })
}

/// 解析客户端帧（NDJSON 单行 → [`ClientFrame`]）。
pub fn parse_client_frame(line: &str) -> Result<ClientFrame, DshError> {
    serde_json::from_str(line).map_err(|e| {
        DshError::new(
            codes::AUTH_PROTOCOL,
            format!("client frame is not valid: {e}"),
        )
    })
}

/// 序列化服务端帧为单行（IPC 写出用）。
pub fn server_frame_line(frame: &ServerFrame) -> String {
    serde_json::to_string(frame).expect("ServerFrame 序列化不可失败")
}

/// 序列化客户端帧为单行。
pub fn client_frame_line(frame: &ClientFrame) -> String {
    serde_json::to_string(frame).expect("ClientFrame 序列化不可失败")
}

// ============================================================
// 绑定账本（bindings.json：非秘密元数据 + 钉公钥；0600）
// ============================================================

/// 绑定状态（账本 wire 词；Bound=信任在，Revoked=撤权墓碑）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoredBindingState {
    Bound,
    Revoked,
}

/// 一条绑定（账本行；公钥/路径/PID 均非秘密）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredBinding {
    pub binding_id: String,
    /// 钉住的客户端公钥（SPKI DER base64url）。**信任物**：撤权即清空——
    /// 拒绝重连的锚点另存 [`StoredBinding::revoked_key_spki`]（墓碑留存，
    /// 不再是可信任的密钥材料）。
    pub public_key_spki: String,
    /// 撤权墓碑：被撤公钥的留存（拒绝同钥自动重连用；非信任物）。
    #[serde(default)]
    pub revoked_key_spki: String,
    pub executable: String,
    pub pid: u32,
    pub started_at_ms: i64,
    pub profile_dir: String,
    pub created_at_ms: i64,
    pub state: StoredBindingState,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BindingLedger {
    #[serde(default)]
    bindings: Vec<StoredBinding>,
}

/// DSH 状态目录（**R10 唯一解析点**）：`TUVIS_DSH_STATE_DIR` 已设 → 恰好该目录；
/// 未设 → `<tuvis_home>/dsh`。授权密钥 / 绑定账本 / rendezvous / 本机套接字
/// 同在此目录下。
fn state_dir(env: &DshEnvironment) -> PathBuf {
    super::production::resolve_state_dir(
        &super::production::DshEnvSnapshot {
            dsh_state_dir: env.dsh_state_dir.clone(),
        },
        &env.tuvis_home,
    )
}

// ============================================================
// 实例作用域（H05：按实例隔离 state / 绑定 / 凭据 / 端点）
// ============================================================

/// 实例 ID 形态 = 登记派生输出（32 个小写十六进制字符）。路径段只接受该形态
/// （防 `../` 逃逸与任意目录名注入——instance_id 虽出自登记面，仍纵深防御）。
fn is_instance_id_shape(instance_id: &str) -> bool {
    instance_id.len() == 32
        && instance_id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// 实例隔离子目录：`<state_dir>/instances/<instance_id>/`。授权私钥 / 绑定账本 /
/// rendezvous / 撤权待落盘标记全部落在这里——A/B 实例的绑定、凭据、信任根互不可见。
/// `None` = instance_id 形态非法（不产路径，不猜）。
pub fn instance_state_dir(env: &DshEnvironment, instance_id: &str) -> Option<PathBuf> {
    if !is_instance_id_shape(instance_id) {
        return None;
    }
    Some(state_dir(env).join("instances").join(instance_id))
}

/// 实例作用域 env（H05 装配内核；生产单例与测试共用）：在基础 env 之上把 DSH
/// 状态目录指到该实例的隔离子目录。其余适配器（进程/文件/runner/时钟）共享同一
/// 活体事实（跨实例发现本来就是同一进程表）。
pub fn instance_scoped_env(
    base: &DshEnvironment,
    instance_id: &str,
) -> Result<DshEnvironment, DshError> {
    let dir = instance_state_dir(base, instance_id).ok_or_else(|| {
        DshError::new(
            codes::IPC_STATE,
            format!("invalid dsh instance id: {instance_id}"),
        )
    })?;
    Ok(DshEnvironment {
        home: base.home.clone(),
        tuvis_home: base.tuvis_home.clone(),
        dsh_state_dir: Some(dir),
        processes: base.processes.clone(),
        files: base.files.clone(),
        runner: base.runner.clone(),
        clock: base.clock.clone(),
        environments: base.environments.clone(),
    })
}

/// 实例作用域的本机套接字落点（H05）：**文件**在实例隔离子目录，而**套接字**落
/// 基础状态目录、名 = `ipc-<instance_id>.sock`。为什么不在实例子目录里：AF_UNIX
/// `sun_path` 103 字节上限——默认安装根（`~/Library/Application Support/.tuvis/dsh`
/// 一类）加 `instances/<32hex>/` 一级必然超限；套接字在基础目录用长名（41 字节）
/// 预算即可容纳，且各实例端点天然互不可见、跨重启地址稳定。
fn instance_socket_placement(env: &DshEnvironment, instance_id: &str) -> Option<(PathBuf, String)> {
    if !is_instance_id_shape(instance_id) {
        return None;
    }
    Some((state_dir(env), format!("ipc-{instance_id}.sock")))
}

/// 该实例 auth 域最新 Bound 行钉住公钥的 SHA-256 hex 摘要（H05 指纹探针的生产
/// 来源；H02 补测二实证机制：受信插件公钥指纹绑定与校验）。无账本 / 畸形 /
/// 无 Bound 行 → `None` = **不可核验**（resolve 据此拒绝，绝不当作通过——
/// 隔离测试环境与撤权后的实例同口径）。
pub fn pinned_fingerprint_for(env: &DshEnvironment, instance_id: &str) -> Option<String> {
    let dir = instance_state_dir(env, instance_id)?;
    let bytes = std::fs::read(dir.join("bindings.json")).ok()?;
    let ledger: BindingLedger = serde_json::from_slice(&bytes).ok()?;
    let bound = ledger
        .bindings
        .iter()
        .rev()
        .find(|b| b.state == StoredBindingState::Bound)?;
    let spki = b64url_decode(&bound.public_key_spki).ok()?;
    Some(key_digest(&spki))
}

/// 升级证据只读面（第九轮 P2 复审 Important 修复）：该实例账本**最新 Bound 行**
/// 的（公钥 SHA-256 摘要, 是否被「待落盘撤销标记」指名）。无账本 / 畸形 / 无
/// Bound 行 / 摘要解不出 / **标记不可读或损坏 JSON**（保守口径照
/// [`revocation_settled_for`]：仅 `NotFound` = 无标记）→ `None` = **不可证明**
/// （证据不足口径，与 [`pinned_fingerprint_for`] 同源）。**只读**：绝不消费
/// 标记、绝不装配授权域——撤权意图未收敛由调用方按证据不足处理，标记的补刀
/// 仍只发生在真实域装配（[`DshAuthService::build`] → `consume_revoke_pending`）。
pub(crate) fn latest_bound_evidence_for(
    env: &DshEnvironment,
    instance_id: &str,
) -> Option<(String, bool)> {
    let dir = instance_state_dir(env, instance_id)?;
    let bytes = std::fs::read(dir.join("bindings.json")).ok()?;
    let ledger: BindingLedger = serde_json::from_slice(&bytes).ok()?;
    let bound = ledger
        .bindings
        .iter()
        .rev()
        .find(|b| b.state == StoredBindingState::Bound)?;
    let digest = b64url_decode(&bound.public_key_spki)
        .ok()
        .map(|spki| key_digest(&spki))?;
    // 待落盘撤权标记：**保守口径照 [`revocation_settled_for`]**——仅 `NotFound`
    // 视为无标记；其余读取错误（权限/IO）与解析失败一律 `None`（不可证明），
    // 恢复核验据此拒绝。绝不用 `.ok()` 把「读不出撤权意图」折叠成「无待撤权」
    //（核验通过后的真实装配会在域构造里消费畸形标记——畸形删除分支——把
    // 撤权意图洗掉）。落点双路径与写入口/既有口径同源：生产撤权经实例作用域
    // env 写 `<state>/instances/<id>/revoke-pending.json`；旧版本可能写在基础
    // `state_dir` 下，两处任一命中都算未收敛。
    let pending_paths = [
        dir.join("revoke-pending.json"),
        state_dir(env).join("revoke-pending.json"),
    ];
    for path in pending_paths {
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<RevokePendingFile>(&bytes) {
                Ok(pending) => {
                    if pending
                        .profiles
                        .iter()
                        .any(|p| profile_matches(&bound.profile_dir, p))
                    {
                        return Some((digest, true));
                    }
                }
                Err(_) => return None,
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
    }
    Some((digest, false))
}

/// 该实例全部 Bound 行的（公钥 SHA-256 摘要, 绑定时原始逻辑 profile 入口）
/// ——M06 离线 home 恢复的证据面。无账本 / 畸形 → `None` = **不可核验**
/// （与 [`pinned_fingerprint_for`] 同口径，绝不把缺失当通过）。摘要解不出
/// 的行以空摘要返回（不会与任何登记指纹相等，即不构成证据）。
pub(crate) fn bound_rows_for(
    env: &DshEnvironment,
    instance_id: &str,
) -> Option<Vec<(String, String)>> {
    let dir = instance_state_dir(env, instance_id)?;
    let bytes = std::fs::read(dir.join("bindings.json")).ok()?;
    let ledger: BindingLedger = serde_json::from_slice(&bytes).ok()?;
    Some(
        ledger
            .bindings
            .iter()
            .filter(|b| b.state == StoredBindingState::Bound)
            .map(|b| {
                let digest = b64url_decode(&b.public_key_spki)
                    .ok()
                    .map(|spki| key_digest(&spki))
                    .unwrap_or_default();
                (digest, b.profile_dir.clone())
            })
            .collect(),
    )
}

/// 该实例全部**绑定起源行**的（公钥 SHA-256 摘要, 绑定时原始逻辑 profile 入口）
/// （Round-8 P1-3 退役登记的只读历史证据面）：Bound 行取 `public_key_spki`，
/// 撤权墓碑行取 `revoked_key_spki`——撤权把公钥移位进墓碑字段。与
/// [`bound_rows_for`]（当代 Bound 专用）互补：关闭+卸载把 Bound 行翻成墓碑，
/// 但墓碑**永久留存**被撤钥——退役登记（锚已被卸载清空）凭保留的只读证据
/// 摘要对位后，绑定起源行仍是可核验的归属证据。无账本 / 畸形 → `None` =
/// **不可核验**（绝不把缺失当通过）。摘要解不出的行以空摘要返回（不会与任何
/// 登记证据相等，即不构成证据）。
pub(crate) fn historical_rows_for(
    env: &DshEnvironment,
    instance_id: &str,
) -> Option<Vec<(String, String)>> {
    let dir = instance_state_dir(env, instance_id)?;
    let bytes = std::fs::read(dir.join("bindings.json")).ok()?;
    let ledger: BindingLedger = serde_json::from_slice(&bytes).ok()?;
    Some(
        ledger
            .bindings
            .iter()
            .map(|b| {
                let spki = match b.state {
                    StoredBindingState::Bound => &b.public_key_spki,
                    StoredBindingState::Revoked => &b.revoked_key_spki,
                };
                let digest = b64url_decode(spki)
                    .ok()
                    .map(|spki| key_digest(&spki))
                    .unwrap_or_default();
                (digest, b.profile_dir.clone())
            })
            .collect(),
    )
}

/// 该实例绑定账本**最新一条绑定起源行**（Bound 行取 `public_key_spki`；撤权
/// 墓碑行取 `revoked_key_spki`）钉住公钥的 SHA-256 hex 摘要（Round-8 P1-3
/// 只读历史钉住源，**仅退役登记**的只读面消费——锚定登记继续用
/// [`pinned_fingerprint_for`] 的 Bound 行口径，撤权即不可证明）。无账本 / 畸形
/// / 无任何携带公钥的绑定起源行 → `None` = **不可核验**（绝不当作通过）。
pub(crate) fn historical_fingerprint_for(
    env: &DshEnvironment,
    instance_id: &str,
) -> Option<String> {
    let rows = historical_rows_for(env, instance_id)?;
    // 「最新绑定起源行」按账本**追加序**取末行——任何未来的账本压缩/重写必须
    // 保留起源行的先后次序，否则本口径（与撤权墓碑判据）会把旧钥当最新。
    let (digest, _) = rows.last()?;
    if digest.is_empty() {
        return None;
    }
    Some(digest.clone())
}

/// 该实例是否存在「指向 profile 的 Bound 行」或「待落盘撤销标记」（M06 恢复
/// 对账的撤权证据面）：两者任一在场 = 撤权**未证完成**（崩溃可能发生在撤权
/// 前后交界，不能仅凭插件文件缺失断定卸载成功）。无账本/畸形 → `None` =
/// 不可核验（调用方按证据不足保守处理，绝不当作「已撤干净」）。
pub(crate) fn revocation_settled_for(
    env: &DshEnvironment,
    instance_id: &str,
    profile_dir: &str,
) -> Option<bool> {
    let dir = instance_state_dir(env, instance_id)?;
    let bound = match std::fs::read(dir.join("bindings.json")) {
        Ok(bytes) => match serde_json::from_slice::<BindingLedger>(&bytes) {
            Ok(ledger) => ledger.bindings.iter().any(|b| {
                b.state == StoredBindingState::Bound && profile_matches(&b.profile_dir, profile_dir)
            }),
            Err(_) => return None,
        },
        // 无账本文件 = 从未有绑定，撤权面自然闭合。
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => return None,
    };
    // I-6 待落盘撤销标记：域不可用 + 账本写不成时留下的补刀清单——在场即
    // 撤权链未闭合。生产撤权经 `instance_scoped_env` 写**实例作用域**标记
    // （`<state>/instances/<id>/revoke-pending.json`，复审 Major-3）；旧版本
    // 可能写在基础 `state_dir` 下，两处任一命中都算未闭合。
    let pending_paths = [
        dir.join("revoke-pending.json"),
        state_dir(env).join("revoke-pending.json"),
    ];
    let mut pending = false;
    for path in pending_paths {
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<RevokePendingFile>(&bytes) {
                Ok(file) => {
                    if file
                        .profiles
                        .iter()
                        .any(|entry| profile_matches(entry, profile_dir))
                    {
                        pending = true;
                        break;
                    }
                }
                Err(_) => return None,
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
    }
    Some(!bound && !pending)
}

/// 该实例绑定账本中**撤权墓碑留存的被撤公钥**列表（b64url SPKI；P1-2 复审
/// Critical 的**钥域**判据面）：实例授权域按实例隔离，账本行全部属于本实例，
/// 故按域取被撤钥即可，不做 profile 路径匹配——真实握手行的 profileDir 是
/// 宿主 cmdline 的逻辑 profile 路径，与登记面的物理路径可能不同形，路径匹配
/// 会把真墓碑误判成「无墓碑」。
///
/// * 存在 Bound 行（域内任何钥活着）→ 空列表：身份绝不可动（墓碑永久留存，
///   撤权→重绑后账本同时有墓碑与 Bound 行——身份目录属于现钥）；
/// * 无账本 = 从未有绑定 → 空列表；
/// * 账本不可读 → `None`（不可核验，调用方保守不动）。
pub(crate) fn revoked_tombstone_keys_for(
    env: &DshEnvironment,
    instance_id: &str,
) -> Option<Vec<String>> {
    let dir = instance_state_dir(env, instance_id)?;
    match std::fs::read(dir.join("bindings.json")) {
        Ok(bytes) => match serde_json::from_slice::<BindingLedger>(&bytes) {
            Ok(ledger) => {
                if ledger
                    .bindings
                    .iter()
                    .any(|b| b.state == StoredBindingState::Bound)
                {
                    return Some(Vec::new());
                }
                Some(
                    ledger
                        .bindings
                        .iter()
                        .filter(|b| {
                            b.state == StoredBindingState::Revoked && !b.revoked_key_spki.is_empty()
                        })
                        .map(|b| b.revoked_key_spki.clone())
                        .collect(),
                )
            }
            Err(_) => None,
        },
        Err(_) => Some(Vec::new()),
    }
}

fn key_path(env: &DshEnvironment) -> PathBuf {
    state_dir(env).join("authorization.key")
}

fn ledger_path(env: &DshEnvironment) -> PathBuf {
    state_dir(env).join("bindings.json")
}

fn rendezvous_path(env: &DshEnvironment) -> PathBuf {
    state_dir(env).join("rendezvous.json")
}

fn read_optional_file(path: &std::path::Path) -> std::io::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// 0600 原子替换写（bindngs/rendezvous/私钥同款；权限即当前用户限定）。
fn atomic_write_0600(path: &std::path::Path, bytes: &[u8]) -> Result<(), DshError> {
    // 回归接缝（test-only unix / dsh-test-probes）：按路径注入写失败（P1-1 撤权
    // 写盘失败模拟；Round-8 集成回归复用同一判据形态）。
    #[cfg(all(unix, any(test, feature = "dsh-test-probes")))]
    if ledger_write_probe::fail_write(path) {
        return Err(DshError::new(
            codes::IPC_STATE,
            format!("ledger write probe: refused {}", path.display()),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                DshError::new(
                    codes::IPC_STATE,
                    format!("create_dir_all {}: {e}", parent.display()),
                )
            })?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).map_err(
                |e| {
                    DshError::new(
                        codes::IPC_STATE,
                        format!("chmod 0700 {}: {e}", parent.display()),
                    )
                },
            )?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, bytes).map_err(|e| {
            DshError::new(codes::IPC_STATE, format!("write {}: {e}", tmp.display()))
        })?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).map_err(|e| {
            DshError::new(
                codes::IPC_STATE,
                format!("chmod 0600 {}: {e}", tmp.display()),
            )
        })?;
        std::fs::rename(&tmp, path).map_err(|e| {
            DshError::new(
                codes::IPC_STATE,
                format!("rename to {}: {e}", path.display()),
            )
        })?;
        Ok(())
    }
    #[cfg(windows)]
    {
        // 创建即受保护：目录与临时文件都在创建调用时携带当前用户 DACL，
        // 不存在「先写后 icacls」的宽权限窗口。
        if let Some(parent) = path.parent() {
            // 逐级创建并带受保护 DACL；已存在目录由后续 verify_private 判定。
            let mut current = std::path::PathBuf::new();
            for component in parent.components() {
                current.push(component);
                if current.as_os_str().is_empty() {
                    continue;
                }
                if super::windows_security::is_dir(&current) {
                    continue;
                }
                super::windows_security::create_private_dir(&current)?;
            }
            super::windows_security::verify_private(parent)?;
        }
        // 唯一临时名：CREATE_NEW 保证不会复用预置对象，也不会因旧 tmp 失败。
        let nonce = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let tmp = path.with_extension(format!("{nonce}.tmp"));
        // 守卫覆盖所有错误出口：仅在成功 rename 后解除清理责任。
        struct TempGuard<'a> {
            path: &'a std::path::Path,
            armed: bool,
        }
        impl Drop for TempGuard<'_> {
            fn drop(&mut self) {
                if self.armed {
                    let _ = std::fs::remove_file(self.path);
                }
            }
        }
        let mut guard = TempGuard {
            path: &tmp,
            armed: false,
        };
        super::windows_security::create_private_file(&tmp, bytes)?;
        guard.armed = true;
        super::windows_security::verify_private(&tmp)?;
        std::fs::rename(&tmp, path).map_err(|e| {
            DshError::new(
                codes::IPC_STATE,
                format!("rename to {}: {e}", path.display()),
            )
        })?;
        guard.armed = false;
        super::windows_security::verify_private(path)?;
        Ok(())
    }
}

/// 加载绑定账本。
///
/// * 文件不存在 = 空账本（首次使用）；
/// * 存在但 ACL 校验失败、不可读或解析失败 = `Err`，不得降级为空账本——
///   空账本会让墓碑消失、并让 `consume_revoke_pending` 误删待撤销标记。
fn load_ledger(env: &DshEnvironment) -> Result<BindingLedger, DshError> {
    let path = ledger_path(env);
    // 只把「明确不存在」当成首次使用；访问错误、I/O 错误与链接都不能降级为空账本。
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BindingLedger::default()),
        Err(e) => {
            return Err(DshError::new(
                codes::IPC_STATE,
                format!("stat binding ledger {}: {e}", path.display()),
            ))
        }
    }
    #[cfg(windows)]
    {
        super::windows_security::reject_reparse(&path)?;
        super::windows_security::verify_private(&path)?;
    }
    let bytes = std::fs::read(&path).map_err(|e| {
        DshError::new(
            codes::IPC_STATE,
            format!("read binding ledger {}: {e}", path.display()),
        )
    })?;
    serde_json::from_slice(&bytes).map_err(|e| {
        DshError::new(
            codes::IPC_STATE,
            format!("parse binding ledger {}: {e}", path.display()),
        )
    })
}

/// 撤销待落盘标记（I-6 兜底；`<state_dir>/revoke-pending.json`）。
///
/// 域初始化失败时卸载撤权仍要落盘「该 profile 已撤」，但**连账本都写不成**的
/// 情形（磁盘只读/临时文件位置被占）不能把卸载堵死。此时写下本标记，由下一次
/// 成功 [`DshAuthService::new`] 消费（把匹配的 Bound 行改成撤权墓碑）后删除。
fn revoke_pending_path(env: &DshEnvironment) -> PathBuf {
    state_dir(env).join("revoke-pending.json")
}

/// 待落盘的撤销请求（只记 profile 口径；不含秘密）。
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RevokePendingFile {
    /// 待撤的 profile 目录（落账口径，逐条按 [`profile_matches`] 匹配）。
    #[serde(default)]
    profiles: Vec<String>,
}

/// 把一个 profile 的 Bound 行就地改成撤权墓碑（**纯账本变换**，不触内存/闸门）。
///
/// 语义与 [`DshAuthService::revoke_local`] 的账本部分一致：钉住的公钥移进
/// `revoked_key_spki`（信任物销毁，墓碑留存），状态置 `Revoked`。返回改动的行数。
fn revoke_profile_in_ledger(ledger: &mut BindingLedger, profile_id: &str) -> usize {
    let mut changed = 0;
    for record in ledger.bindings.iter_mut() {
        if record.state != StoredBindingState::Bound
            || !profile_matches(&record.profile_dir, profile_id)
        {
            continue;
        }
        let revoked = std::mem::take(&mut record.public_key_spki);
        record.revoked_key_spki = revoked;
        record.state = StoredBindingState::Revoked;
        changed += 1;
    }
    changed
}

/// 域不可用时的**账本只写**撤销（I-6）。
///
/// 返回 `Ok(changed)` = 已落盘（changing 行数）；`Err` = 连账本都写不成。
fn revoke_profile_ledger_only(env: &DshEnvironment, profile_id: &str) -> Result<usize, DshError> {
    let mut ledger = load_ledger(env)?;
    let changed = revoke_profile_in_ledger(&mut ledger, profile_id);
    if changed == 0 {
        return Ok(0); // 没有可撤的行：不必造一次写（也不造标记）
    }
    let bytes = serde_json::to_vec_pretty(&ledger)
        .map_err(|e| DshError::new(codes::IPC_STATE, format!("serialize bindings: {e}")))?;
    atomic_write_0600(&ledger_path(env), &bytes)?;
    Ok(changed)
}

/// 追加一条待落盘撤销请求（合并去重；写失败只记日志——卸载出口不因此失败）。
fn remember_revoke_pending(env: &DshEnvironment, profile_id: &str) {
    let path = revoke_pending_path(env);
    let mut pending: RevokePendingFile = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    if pending.profiles.iter().any(|p| p == profile_id) {
        return;
    }
    pending.profiles.push(profile_id.to_string());
    let bytes = match serde_json::to_vec_pretty(&pending) {
        Ok(bytes) => bytes,
        Err(e) => {
            log::warn!("dsh revoke-pending marker serialize failed (retry on next start): {e}");
            return;
        }
    };
    if let Err(e) = atomic_write_0600(&path, &bytes) {
        log::warn!(
            "dsh revoke-pending marker write failed at {} (kept for next start): {}: {}",
            path.display(),
            e.code,
            e.detail
        );
    }
}

/// 消费待落盘撤销标记（[`DshAuthService::new`] 调用）。
///
/// 成功应用后删除标记；应用失败保留标记（下次再试），只记日志、不阻断服务创建。
fn consume_revoke_pending(env: &DshEnvironment, ledger: &mut BindingLedger) {
    let path = revoke_pending_path(env);
    let Ok(bytes) = std::fs::read(&path) else {
        return;
    };
    let Ok(pending) = serde_json::from_slice::<RevokePendingFile>(&bytes) else {
        // 畸形标记无法解释：删掉（不猜），避免永久残留。
        log::warn!(
            "dsh revoke-pending marker unreadable, dropped: {}",
            path.display()
        );
        let _ = std::fs::remove_file(&path);
        return;
    };
    let mut changed = 0;
    for profile_id in &pending.profiles {
        changed += revoke_profile_in_ledger(ledger, profile_id);
    }
    if changed > 0 {
        // R09 纪律：构造路径不用 expect——序列化对 BindingLedger 结构性不可失败，
        // 仍以降级（保留标记下次再试）替代 panic。
        let out = match serde_json::to_vec_pretty(&*ledger) {
            Ok(out) => out,
            Err(e) => {
                log::warn!("dsh revoke-pending marker serialize failed (kept for next start): {e}");
                return;
            }
        };
        if let Err(e) = atomic_write_0600(&ledger_path(env), &out) {
            log::warn!(
                "dsh revoke-pending marker apply failed (kept for next start): {}: {}",
                e.code,
                e.detail
            );
            return;
        }
    }
    if let Err(e) = std::fs::remove_file(&path) {
        log::warn!("DSH 撤销待落盘标记删除失败（{}）：{e}", path.display());
    }
}

/// 私钥文件形状（0600；种子是唯一秘密字节）。
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KeyFile {
    version: u32,
    seed: String,
}

/// 加载或创建 Rust 授权私钥（文件缺失则生成并 0600 落盘）。
fn load_or_create_key(env: &DshEnvironment) -> Result<RustSigningKey, DshError> {
    let path = key_path(env);
    #[cfg(windows)]
    match std::fs::symlink_metadata(&path) {
        // 旧密钥先校验 ACL，再读取；校验失败不得进入「重新生成」路径。
        Ok(_) => {
            super::windows_security::reject_reparse(&path)?;
            super::windows_security::verify_private(&path)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(DshError::new(
                codes::IPC_STATE,
                format!("authorization key stat failed: {e}"),
            ))
        }
    }
    match std::fs::read(&path) {
        Ok(bytes) => {
            if let Ok(file) = serde_json::from_slice::<KeyFile>(&bytes) {
                if let Ok(seed) = b64url_decode(&file.seed) {
                    if let Ok(seed) = <[u8; 32]>::try_from(seed.as_slice()) {
                        return Ok(RustSigningKey::from_seed(&seed));
                    }
                }
            }
            return Err(DshError::new(
                codes::IPC_STATE,
                format!("authorization key file is unreadable: {}", path.display()),
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        // 已存在但读不到（共享冲突/权限/IO）绝不能生成新钥覆盖。
        Err(e) => {
            return Err(DshError::new(
                codes::IPC_STATE,
                format!("read authorization key {}: {e}", path.display()),
            ))
        }
    }
    let key = RustSigningKey::generate();
    let file = KeyFile {
        version: 1,
        seed: b64url_encode(key.signing.to_bytes().as_slice()),
    };
    atomic_write_0600(
        &path,
        serde_json::to_vec_pretty(&file)
            .expect("KeyFile 序列化不可失败")
            .as_slice(),
    )?;
    Ok(key)
}

// ============================================================
// rendezvous 记录（非敏感；原子替换）
// ============================================================

/// 进程身份（rendezvous 内的 Rust 侧身份投影）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RendezvousIdentity {
    pub platform: String,
    pub pid: u32,
    pub started_at_ms: i64,
    pub executable: String,
}

/// rendezvous 记录：**只含**随机 IPC 地址、协议代际、进程身份、Rust 公钥。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RendezvousRecord {
    pub binding_protocol: u32,
    /// 本机 IPC 端点（UDS 路径 / 管道名）。
    pub address: String,
    pub host_id: String,
    /// 当前代际（十进制字符串；换代后插件据此重新发起握手）。
    pub epoch: String,
    pub process_identity: RendezvousIdentity,
    /// Rust 公钥（SPKI DER base64url；插件信任锚）。
    pub public_key_spki: String,
    /// 停用标记（H05 审查 I3 交接）：实例域发布时把仍在场的 legacy 记录改写为
    /// `retired = true`——插件据此忽略旧记录（同 PID 单匹配 → 绑定实例域）。
    /// 新发布的记录恒为 false（serde default；旧记录无此字段读回 false）。
    #[serde(default)]
    pub retired: bool,
}

/// I3-r3：根 rendezvous 的**写窗口**——legacy 发布（根写）与实例域交接（根写）
/// 共用同一排他窗口（进程内 gate + 跨进程 flock 锁文件），窗口内重读 retired 位
/// 做**写合并**：预构造的 stale 记录不得在窗口外提交撤销交接结果。
static ROOT_RENDEZVOUS_GATE: Mutex<()> = Mutex::new(());

/// 根 rendezvous 的跨进程排他锁文件（`<根>/rendezvous.lock`；flock；写完即释放
/// ——与 uds 端点接管锁同款语义，崩溃自动释放）。
fn root_rendezvous_lock(root_dir: &std::path::Path) -> Result<std::fs::File, DshError> {
    let lock_path = root_dir.join("rendezvous.lock");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| {
            DshError::new(
                codes::IPC_STATE,
                format!("open rendezvous lock {}: {e}", lock_path.display()),
            )
        })?;
    use fs2::FileExt;
    file.lock_exclusive().map_err(|e| {
        DshError::new(
            codes::IPC_STATE,
            format!("flock rendezvous lock {}: {e}", lock_path.display()),
        )
    })?;
    Ok(file)
}

/// 交接代际判定（I3-r3①）：**完整四元组**（实例身份 + PID + 启动时刻 + epoch）
/// 一致才算同代际——PID 复用（新进程、同 PID、启动时刻/epoch 不同）不构成旧
/// publisher，不得把新发布锁死为 retired。
fn same_handover_generation(existing: &RendezvousRecord, host: &LocalHost) -> bool {
    existing.process_identity.pid == host.pid
        && existing.process_identity.started_at_ms == host.started_at_ms
        && existing.host_id == host.host_id
        && existing.epoch == host.epoch.to_string()
}

fn build_rendezvous_record(
    address: &str,
    key: &RustSigningKey,
    host: &LocalHost,
    retired: bool,
) -> RendezvousRecord {
    RendezvousRecord {
        binding_protocol: BINDING_PROTOCOL,
        address: address.to_string(),
        host_id: host.host_id.clone(),
        epoch: host.epoch.to_string(),
        process_identity: RendezvousIdentity {
            platform: host.platform.clone(),
            pid: host.pid,
            started_at_ms: host.started_at_ms,
            executable: host.executable.to_string_lossy().to_string(),
        },
        public_key_spki: b64url_encode(&key.spki),
        retired,
    }
}

/// 发布 rendezvous（用调用方解析好的当代宿主 → 原子替换；失败如实上抛）。
///
/// H05 审查 I3-r3：
/// * 实例域记录（独立文件）：直接写，不涉根交接单调。
/// * 根记录（legacy 域）：进根写窗口——窗口内重读现有 retired 位做**写合并**：
///   `retired = true` 且同代际（完整四元组）→ 保留 retired（绝不撤销交接结果）；
///   PID 复用/换代 → 新代际正常发布（不锁死）。「读→改→写」在窗口内原子，
///   与实例域的 retire 更新互斥。
fn publish_rendezvous(
    env: &DshEnvironment,
    address: &str,
    key: &RustSigningKey,
    host: &LocalHost,
    root_scope: bool,
) -> Result<(), DshError> {
    let target = rendezvous_path(env);
    if !root_scope {
        let record = build_rendezvous_record(address, key, host, false);
        return atomic_write_0600(
            &target,
            serde_json::to_vec_pretty(&record)
                .expect("RendezvousRecord 序列化不可失败")
                .as_slice(),
        );
    }
    let gate = ROOT_RENDEZVOUS_GATE.lock().unwrap();
    let lock = root_rendezvous_lock(&state_dir(env))?;
    // 窗口内重读（写合并）：预构造的 retired=false 在此被现场观测覆盖。
    let mut retired = false;
    if let Ok(bytes) = std::fs::read(&target) {
        if let Ok(existing) = serde_json::from_slice::<RendezvousRecord>(&bytes) {
            if existing.retired && same_handover_generation(&existing, host) {
                retired = true;
            }
        }
    }
    let record = build_rendezvous_record(address, key, host, retired);
    let result = atomic_write_0600(
        &target,
        serde_json::to_vec_pretty(&record)
            .expect("RendezvousRecord 序列化不可失败")
            .as_slice(),
    );
    drop(lock);
    drop(gate);
    result
}

// ============================================================
// 服务内存态
// ============================================================

/// 活体跟踪拍间隔（毫秒）：监听已启动后，发布不再依赖新连接进来——由后台拍
/// 主动跟踪已启用实例的宿主代际（退出/换代 → 失效旧代 + 重发布）。插件侧自己的
/// 重连节奏是 1s 起步指数退避（上限 30s），本拍只决定兔维斯侧的发现延迟。
const LIVENESS_POLL_MS: u64 = 2_000;

/// 一拍活体跟踪的结论（观测面；测试与日志消费）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostTracking {
    /// 当代记录已与在场宿主一致（无需写盘）。
    Current,
    /// 为在场宿主（重）发布了当代记录；`replaced` = 本拍先失效了一个已退出旧代。
    Published { replaced: bool },
    /// 宿主不在场：`invalidated` = 本拍停用了旧代记录并失效其连接/临时凭据。
    HostAbsent { invalidated: bool },
    /// 本域监听端未启动：无地址可发布（等待 serve）。
    NoListener,
}

/// T3 发送预检的当代握手/发送凭据三态（[`DshAuthService::control_peer_state`]
/// 的结论；hub 据此把「正在恢复连接」与「请在电脑上检查所选实例」分开呈现，
/// 不与实例归属/会话归属/契约门折叠成同一句话）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlPeerState {
    /// 当代发送凭据在位——发送预检放行（其余轴另行判定）。
    Ready,
    /// Bound 绑定在场但凭据是旧代际/未交付——活体跟踪拍会自动重发布并重交付，
    /// 可恢复暂态 → 前端呈现「正在恢复连接」。
    Recovering,
    /// 无 Bound 绑定 / 闸门已撤 / 当代宿主不可证 → 前端呈现「请在电脑上检查
    /// 所选实例」。
    NotAuthorized,
}

/// 待批绑定记录（内存；批准后转正入账本）。
struct PendingRecord {
    binding: BindingChallenge,
    client: ClientIdentity,
    /// 客户端公钥 SPKI 的 base64url（钉公钥入账本用；同时是待批记录的**幂等键**）。
    client_spki_b64: String,
    /// 该客户端的连接（批准时推 Granted 用；同一公钥重连时更新为最新连接）。
    conn: Arc<dyn IpcClientConnection>,
    host: LocalHost,
    handshake_gate: Arc<tokio::sync::Mutex<()>>,
}

struct EventProof {
    conn: Arc<dyn IpcClientConnection>,
    challenge: BindingChallengeMessage,
    decision_signature: String,
    binding_id: String,
}

/// 已批准但 cookie 未到的连接注册（binding_id → 连接）。
type TrustedPeers = HashMap<String, Arc<dyn IpcClientConnection>>;

fn unauthenticated_probe_budget() -> Arc<tokio::sync::Semaphore> {
    static BUDGET: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    Arc::clone(BUDGET.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1))))
}

struct Inner {
    /// 待批记录（**按客户端公钥分别持有**，不是单一槽位）。
    ///
    /// R04 修复：插件每 3 秒重发一次 hello，若每次首绑都换成新的
    /// binding_id+display_code（旧实现单一 `Option` 直接覆盖），两端核对码在人工
    /// 核对窗口里一直翻动，用户点批准时批的往往是已被覆盖的旧 id
    /// （`DSH_AUTH_UNKNOWN_BINDING`）。现在同一未批准公钥在**自己那份挑战未过期**
    /// 期间一律返回同一对 id/码；只有到期才换新。新记录追加在尾部（`pending_binding`
    /// 取最新一条给设置卡）。
    pending: Vec<PendingRecord>,
    /// 每把已验签公钥当前握手连接；替换后旧挑战不得再提交或恢复。
    latest_key_connections: HashMap<String, Arc<dyn IpcClientConnection>>,
    trusted: TrustedPeers,
    event_proofs: Vec<EventProof>,
    /// 握手通过时核验的宿主代际，不能在凭据交付时重新盖章。
    trusted_hosts: HashMap<String, LocalHost>,
    /// R01 晋升：经「观测启动环境 + 内核对端 + 插件签名」证明过 home 的 Web 宿主
    /// 代际。存在即表示该实例的当代 Web 归属已核验（`current_live` 据此返回
    /// `verified = true`）；换代或撤权即清除。
    proven_web_hosts: HashMap<String, LocalHost>,
    /// 一次性 nonce 窗口（FIFO 退役 + 集合查重）。
    used_nonces: VecDeque<([u8; 32], i64)>,
    nonce_set: HashSet<[u8; 32]>,
    /// nonce 窗口所属代际（换代清零）。
    nonce_epoch: Option<u64>,
    /// 已交付凭据的秘密句柄，**按 binding_id 分槽**（T09-G 先行安全门；owner
    /// Cookie 只在内存——不落盘/不入日志/不入 DTO，T09 hub 消费）。
    ///
    /// 旧实现是单一 `Option<AuthorizedPeer>`：交付覆盖写、撤权无条件清空——
    /// A/B 都已交付时后交付者覆盖先交付者（先交付者取句柄得
    /// `DSH_AUTH_NOT_APPROVED`），撤 A 会清掉 B 已交付的句柄。现在：交付按
    /// `cred.binding_id` 插入（同绑定重交付覆盖自身）；`authorize_grant` 按调用方
    /// 自己的 Bound 行 binding_id 取；`revoke_local` 只移除该绑定的条目。
    /// `trusted`（按 binding）与 `gates`（按 binding）本就分槽，句柄是最后一份。
    peers: HashMap<String, AuthorizedPeer>,
    /// 每 binding 一份的撤权闸门（R08）。
    ///
    /// 闸门在**批准时**创建（`binding_id` 一经批准就有资格连接），随凭据交付挂到
    /// 句柄上再由 transport 登记活连接。`revoke` 翻转它 → 此后该 binding 的建连/
    /// 重连一律拒绝，且在途连接被 socket 层关闭。撤权后条目保留（闸门永久 revoked，
    /// 复位等于悄悄重开权限）。
    gates: HashMap<String, Arc<PeerGate>>,
    /// 交付时保留的**网页凭据副本**（P1-4 自动重交付的恢复源；按 binding 分槽）。
    ///
    /// 为什么需要：控制台凭据装在 `console_lifecycle` / `console_mount`，可能被
    /// 整域撤权（revoke_console_access / 历史遗留全局 revoke bug）清空，而插件连接持续
    /// 在场、不会重新握手 → 没有任何重交付触发点。活体跟踪拍发现「受信当代
    /// peer 在场 + 控制台凭据缺失」时按此副本经**同一交付链**重装（Task 4 契约）。
    /// 与 `peers` 同等纪律：仅内存、不落盘、不入日志/DTO（无 derive，组装点显式
    /// 字段，秘密只经字段访问）。
    web_deliveries: HashMap<String, RetainedWebDelivery>,
}

/// 交付时保留的网页凭据（api + browser 双凭据 + 交付时代际/authority + 闸门）。
/// 刻意不 derive Debug/Clone 以外的任何 trait；秘密字段不实现 Display。
struct RetainedWebDelivery {
    api: String,
    browser: String,
    epoch: u64,
    authority: String,
    gate: Arc<PeerGate>,
}

#[cfg(feature = "dsh-test-probes")]
type CredentialHook = Arc<dyn Fn(&str) + Send + Sync>;
#[cfg(feature = "dsh-test-probes")]
static BEFORE_NATIVE_CREDENTIAL_COMMIT: std::sync::OnceLock<CredentialHook> =
    std::sync::OnceLock::new();
#[cfg(feature = "dsh-test-probes")]
static CREDENTIAL_QUEUE_ADVANCE: std::sync::OnceLock<CredentialHook> = std::sync::OnceLock::new();
#[cfg(feature = "dsh-test-probes")]
static CREDENTIAL_HANDLED: std::sync::OnceLock<CredentialHook> = std::sync::OnceLock::new();

#[cfg(feature = "dsh-test-probes")]
pub fn init_before_native_credential_commit_hook_for_test(hook: CredentialHook) {
    let _ = BEFORE_NATIVE_CREDENTIAL_COMMIT.set(hook);
}
#[cfg(feature = "dsh-test-probes")]
pub fn init_credential_queue_advance_hook_for_test(hook: CredentialHook) {
    let _ = CREDENTIAL_QUEUE_ADVANCE.set(hook);
}
#[cfg(feature = "dsh-test-probes")]
pub fn init_credential_handled_hook_for_test(hook: CredentialHook) {
    let _ = CREDENTIAL_HANDLED.set(hook);
}

/// 授权服务的可观测绑定视图（测试边界观测；无秘密）。
#[derive(Debug, Clone)]
pub struct PendingBindingView {
    pub binding: BindingChallenge,
    pub client: ClientIdentity,
    /// 批准并收到凭据前恒为 None（unapproved_client_gets_no_cookie 的观测面）。
    credential: Option<AuthorizedPeer>,
}

impl PendingBindingView {
    /// 秘密句柄（批准 + cookie 交付后才有；调用方不得落盘/入日志/入 DTO）。
    pub fn credential(&self) -> Option<&AuthorizedPeer> {
        self.credential.as_ref()
    }
}

#[cfg(feature = "dsh-test-probes")]
type CredentialAcceptedHook =
    Arc<dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync>;

#[cfg(feature = "dsh-test-probes")]
static CREDENTIAL_ACCEPTED_HOOK: std::sync::OnceLock<CredentialAcceptedHook> =
    std::sync::OnceLock::new();

#[cfg(feature = "dsh-test-probes")]
#[doc(hidden)]
pub fn init_credential_accepted_hook_for_test(
    hook: CredentialAcceptedHook,
) -> Result<(), CredentialAcceptedHook> {
    CREDENTIAL_ACCEPTED_HOOK.set(hook)
}

#[cfg(feature = "dsh-test-probes")]
static HELLO_REGISTERED_HOOK: std::sync::OnceLock<CredentialAcceptedHook> =
    std::sync::OnceLock::new();

#[cfg(feature = "dsh-test-probes")]
#[doc(hidden)]
pub fn init_hello_registered_hook_for_test(
    hook: CredentialAcceptedHook,
) -> Result<(), CredentialAcceptedHook> {
    HELLO_REGISTERED_HOOK.set(hook)
}

#[cfg(feature = "dsh-test-probes")]
static INTENT_CONSUMED_HOOK: std::sync::OnceLock<CredentialAcceptedHook> =
    std::sync::OnceLock::new();

#[cfg(feature = "dsh-test-probes")]
#[doc(hidden)]
pub fn init_intent_consumed_hook_for_test(
    hook: CredentialAcceptedHook,
) -> Result<(), CredentialAcceptedHook> {
    INTENT_CONSUMED_HOOK.set(hook)
}

#[cfg(feature = "dsh-test-probes")]
static INTENT_COMMIT_STAGE_HOOK: std::sync::OnceLock<Arc<dyn Fn(&'static str) + Send + Sync>> =
    std::sync::OnceLock::new();

#[cfg(feature = "dsh-test-probes")]
#[doc(hidden)]
pub fn init_intent_commit_stage_hook_for_test(
    hook: Arc<dyn Fn(&'static str) + Send + Sync>,
) -> Result<(), Arc<dyn Fn(&'static str) + Send + Sync>> {
    INTENT_COMMIT_STAGE_HOOK.set(hook)
}

#[cfg(feature = "dsh-test-probes")]
fn intent_commit_stage(mode: &FingerprintCommitMode, stage: &'static str) {
    if matches!(mode, FingerprintCommitMode::Intent { .. }) {
        if let Some(hook) = INTENT_COMMIT_STAGE_HOOK.get() {
            hook(stage);
        }
    }
}

// ============================================================
// 回归接缝（test-only）：授权提交点停点 + 撤权写盘失败注入
// ============================================================

/// 授权提交点停点（P1-1 回归接缝）：意图已消费、绑定未写的窗口内确定性插队
/// （模拟「关闭完成 → 迟到握手恢复」竞态）。**按实例键 + 一次性**——并发 lib
/// 单测透明（未注册的实例走 None 路径照常提交；注册的实例触发一次即自摘，
/// 同实例后续消费不再停驻）；互斥只护查表/登记，绝不横跨 hook 调用
/// （hook 可能长时间停驻等待放行；round-6 纪律：接缝不持全局锁跨调用）。
#[cfg(all(test, unix))]
pub(crate) mod commit_test_hooks {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};

    type Hook = Arc<dyn Fn() + Send + Sync>;

    fn hooks() -> &'static Mutex<HashMap<String, Hook>> {
        static HOOKS: OnceLock<Mutex<HashMap<String, Hook>>> = OnceLock::new();
        HOOKS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    pub fn set_after_intent_consumed(instance_id: &str, hook: Option<Hook>) {
        let mut guard = hooks().lock().unwrap();
        match hook {
            Some(hook) => {
                guard.insert(instance_id.to_owned(), hook);
            }
            None => {
                guard.remove(instance_id);
            }
        }
    }

    /// Round-8 回归接缝：提交已过外层门、**尚未进入绑定互斥**的窗口内确定性插队
    /// （模拟「disable 先完成 → 迟到提交在互斥内门被拒」的第二个交错面）。停点在
    /// `pin_new_binding` 调用之前——此刻提交线程不持 `binding_mutation`，disable
    /// 可完整跑完；放行后提交进入互斥、撞上互斥内门（enabled=false）被拒。
    /// 一次性 + 按实例键，同 [`after_intent_consumed`] 纪律。
    pub fn set_before_commit_mutex(instance_id: &str, hook: Option<Hook>) {
        set_after_intent_consumed(&format!("{instance_id}::before_commit_mutex"), hook);
    }

    pub fn before_commit_mutex(instance_id: &str) {
        after_intent_consumed(&format!("{instance_id}::before_commit_mutex"));
    }

    /// Round-8 回归接缝：提交**已进绑定互斥**（互斥内门已过）、账本写盘前的停点
    /// （模拟「提交持互斥停驻 → disable 必须等在互斥上 → 提交落地后撤权段串行
    /// 其后撤走新鲜绑定」的第一个交错面）。一次性 + 按实例键（并发 lib 单测里
    /// 其他实例的提交查表落空照常落账，互不毒化），同 [`after_intent_consumed`]
    /// 纪律——锁只护查表，hook 停驻不占互斥锁（停点自身持 `binding_mutation`，
    /// hook 内不得触碰授权域）。
    pub fn set_commit_write_pause(instance_id: &str, hook: Option<Hook>) {
        set_after_intent_consumed(&format!("{instance_id}::commit_write_pause"), hook);
    }

    pub fn commit_write_pause(instance_id: &str) {
        after_intent_consumed(&format!("{instance_id}::commit_write_pause"));
    }

    pub fn after_intent_consumed(instance_id: &str) {
        // 查表与调用分离：锁在取出后即释放，hook 停驻不占互斥锁。**一次性**：
        // 触发即自摘——同一实例的后续意图消费（如回归用例末尾的正面对照握手）
        // 不再被停驻，接缝永不横跨两次消费。
        let hook = hooks().lock().unwrap().remove(instance_id);
        if let Some(hook) = hook {
            hook();
        }
    }

    pub fn hooks_empty() -> bool {
        hooks().lock().unwrap().is_empty()
    }

    /// Round-8：按**组合键**观测是否仍布防（并发 lib 单测纪律——全局
    /// [`hooks_empty`] 会被其他测试的在途 hook 打破，按例断言只看自己的键）。
    pub fn key_armed(composed_key: &str) -> bool {
        hooks().lock().unwrap().contains_key(composed_key)
    }
}

/// 绑定账本写盘失败注入（P1-1 回归接缝，模拟撤权写盘失败）：按路径筛选——
/// `ledger_commit_probe` 同款判据形态。只在 `atomic_write_0600` 入口生效（写前
/// 即拒，磁盘原样 → 调用方走「可证明未生效」分支）。
///
/// **全局槽纪律（并发单测 hazard，用例必须遵守）**：槽是进程级单例（非实例键），
/// 隔离完全依赖**判据按夹具路径精确筛选**（临时目录唯一 → 路径即实例键）——
/// 判据不得无路径条件地放行，且用例必须在收尾 `set_fail_write(None)` 清槽；
/// 泄漏的判据只影响它筛中的那一个（已删除的临时）路径，不毒化其他用例。
///
/// 门控：`cfg(test)`（lib 单测）∪ `dsh-test-probes`（集成测试进程，Round-8
/// 撤权切断回归；生产构建两个条件都不成立 → 模块不存在）。
#[cfg(all(unix, any(test, feature = "dsh-test-probes")))]
pub mod ledger_write_probe {
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    pub type FailWritePredicate = Arc<dyn Fn(&Path) -> bool + Send + Sync>;
    static FAIL_WRITE: Mutex<Option<FailWritePredicate>> = Mutex::new(None);

    pub fn set_fail_write(predicate: Option<FailWritePredicate>) {
        *FAIL_WRITE.lock().unwrap() = predicate;
    }

    pub(super) fn fail_write(path: &Path) -> bool {
        FAIL_WRITE
            .lock()
            .unwrap()
            .clone()
            .is_some_and(|predicate| predicate(path))
    }
}

#[derive(Debug, Clone)]
pub enum FingerprintCommitMode {
    Normal,
    Intent { selection_revision: u64 },
    ExplicitRebind { expected_old: String },
}

#[derive(Debug, Clone)]
pub struct FingerprintCommitReceipt {
    pub instance_id: String,
    pub previous: String,
    pub committed: String,
    pub changed: bool,
    /// 提交前的登记只读证据（第九轮 P1-2 复审 Important-1）：补偿回滚必须把
    /// 锚与证据**一并**恢复到提交前状态——旧锚为空时只回滚锚会让证据停留在
    /// 失败提交推进的新钥上，卸载保留的历史只读证据随之丢失（历史 403）。
    pub previous_read_evidence: String,
}

/// 实例登记指纹锚点（P1-1a）。
///
/// 授权域在「本代已核验」成立时把对端指纹交给它落进登记面。生产实现 =
/// `InstanceRegistry`（见 `production::RegistryFingerprintAnchor`）；测试注入内存
/// 替身，**不注入 = 不锚**（保持 hermetic）。
pub trait RegistryFingerprintAnchor: Send + Sync {
    /// 把指纹锚到该实例。
    ///
    /// 契约：空登记值 → 写入；同值 → 幂等 no-op；**非空且不同值 → 拒绝**
    /// （身份不连续，必须由用户显式重新选择/授权，不得静默换钉）。
    fn anchor_fingerprint(&self, instance_id: &str, fingerprint: &str) -> Result<(), DshError>;

    fn commit_fingerprint(
        &self,
        instance_id: &str,
        fingerprint: &str,
        mode: &FingerprintCommitMode,
    ) -> Result<FingerprintCommitReceipt, DshError>;

    /// 按提交回执补偿回滚（第九轮 P1-2 Important-1 契约收紧）：锚恢复为
    /// `receipt.previous` 的同时，登记只读证据一并恢复为
    /// `receipt.previous_read_evidence`（提交前值）——证据是「曾被授权」的历史
    /// 事实，失败提交不得把它推进到未落账的新钥。
    fn rollback_fingerprint(&self, receipt: &FingerprintCommitReceipt) -> Result<(), DshError>;

    fn current_fingerprint(&self, instance_id: &str) -> Result<String, DshError>;

    /// **预检**：这个指纹会不会被接受（不写任何东西）。
    ///
    /// 为什么需要：批准路径先 `pin_new_binding` 把新钥写进 Bound 账本，再锚定
    /// 登记值。若锚定因「身份不连续」被拒，账本里已经多出一条新绑定，而用户
    /// 看到的是一次失败批准——半完成状态（新钥已受信，但登记面还是旧身份，
    /// 后续写入全被指纹门拒）。因此必须在落账本**之前**问一次。
    ///
    /// 默认 `true` 只表示「本实现不做预检」，让既有替身（不关心登记面）不被
    /// 无谓阻塞；生产实现**必须**覆盖成真实判定。
    fn accepts(&self, _instance_id: &str, _fingerprint: &str) -> bool {
        true
    }
}

/// P1-1 开关执行门（授权提交点注入面）：由**开关域**在装配点登记（auth 不反向
/// 依赖 switch——job-admission gate 同款闭包注入）。参数 = 布防意图携带的操作
/// 代次（钉钥重连等无意图授权 = `None`，只按 enabled 判定）；开关关闭/记录缺席
/// （含不可读）/布防代次与当前代次不匹配 → `Err`（生产实现统一 `DSH_SWITCH_OFF`）。
/// 实现纪律：闭包在**绑定互斥内**也可能被调（钉钥重连分支）——只做无锁小文件读，
/// 绝不取开关 action_lock，也绝不跨 await 持任何锁。
pub type SwitchAuthorizationGate = Arc<dyn Fn(Option<u64>) -> Result<(), DshError> + Send + Sync>;

#[derive(Clone)]
struct AuthorizationIntent {
    expires_at_ms: i64,
    selection: Option<(Arc<super::instances::InstanceRegistry>, String, u64, u64)>,
    /// P1-1：布防时开关记录的操作代次（开关驱动器布防时携带；人工布防 = None）。
    /// 授权提交点据此核对「意图所属的授权代次」——迟到握手在提交点退场。
    switch_generation: Option<u64>,
    token: u64,
}

/// fail-closed 触发时的**被撤绑定证据**（内存态，与 `revoke_write_pending` 同生命
/// 周期层；第九轮 P1-1 进程内恢复核验 ③④ 的对位材料）：盘上墓碑必须与此处的
/// binding_id / 被撤钥摘要对上，才允许清 `binding_faulted`——证明收敛的正是引发
/// fail-closed 的那次撤权。提交路径的 fail-closed（绑定写入结果不可确认）没有
/// 被撤墓碑可对，不登记证据 → 恢复核验保守失败（交重启装载面裁决）。
#[derive(Debug, Clone)]
struct BindingFaultEvidence {
    binding_id: String,
    /// 被撤钥摘要（`key_digest(spki)`；与登记锚指纹同算法同形态）。
    revoked_key_digest: String,
}

/// DSH 本机授权服务（T06 主体）。
///
/// H05：实例作用域（`instance_scope = Some(id)`）把整个域钉在一个已登记实例上——
/// state 子目录、绑定账本、rendezvous、本机套接字端点、握手目标宿主全部按实例
/// 隔离；`None` = 旧单例语义（桌面唯一宿主，路径布局逐字保留）。
pub struct DshAuthService {
    env: Arc<DshEnvironment>,
    key: RustSigningKey,
    /// 实例作用域（None = 旧单例）。
    instance_scope: Option<String>,
    /// 实例作用域的本机套接字落点（目录 + 固定名；None = 旧随机名行为）。
    socket: Option<(PathBuf, String)>,
    listener: Mutex<Option<Arc<dyn IpcListener>>>,
    ledger: Mutex<BindingLedger>,
    /// 串行化同一实例的绑定提交和撤权，不跨异步发送持有。
    binding_mutation: Mutex<()>,
    /// Round-8 P1-1（Ruling 9）：撤权写盘待收敛集合——内存面已失效（受信 Peer 摘除
    /// /闸门翻转/控制台吊销）且内存账本已记 Revoked，但账本落盘失败（可证明未生效
    /// 或不可证明）的 binding id。「已失效是事实，落盘是记账」：重试撤权据此找到
    /// 目标**只补写盘**（内存账本行已是 Revoked，重跑墓碑变换会洗掉
    /// revoked_key_spki）。仅在 binding_mutation 内读写。
    revoke_write_pending: Mutex<HashMap<String, ()>>,
    /// fail-closed 证据槽（第九轮 P1-1）：引发 `binding_faulted` 的那次撤权的
    /// 被撤绑定证据；恢复核验通过后摘除。仅在 binding_mutation 内读写。
    fault_evidence: Mutex<Option<BindingFaultEvidence>>,
    binding_faulted: std::sync::atomic::AtomicBool,
    rendezvous_gate: Mutex<()>,
    rendezvous_sequence: AtomicU64,
    /// 最近一次成功发布的宿主代际（活体跟踪的对照面；None = 本域尚未发布过）。
    ///
    /// 为什么在内存记账而不读盘判代：发布成功与否决定「退出时该失效谁」——盘上
    /// 记录可能被并发写窗口改写，记账面必须与发布动作同窗口更新（rendezvous_gate 内）。
    published_host: Mutex<Option<LocalHost>>,
    /// 活体跟踪任务只允许启动一次（serve 幂等多次调用不重复 spawn）。
    liveness_watcher_started: std::sync::atomic::AtomicBool,
    /// 接受循环任务只允许启动一次（P1-3：spawn 与发布解耦——首次 serve 在 bind
    /// 后、spawn 前失败也会经幂等分支补位，CAS 防并发 double-spawn）。
    accept_loop_started: std::sync::atomic::AtomicBool,
    /// 活体跟踪拍开关（默认开；测试接缝 `without_liveness_watcher` 置 false——
    /// 需要手工编排 refresh 时序的回归不得被真实时钟的周期拍插队）。
    liveness_watcher_enabled: bool,
    inner: Mutex<Inner>,
    intent: Mutex<Option<AuthorizationIntent>>,
    /// P1-1 开关执行门（授权提交点）：None = 未装配（legacy 域 / 测试基线）→ 不拦截；
    /// 生产实例域在装配点登记（`production::auth_for_instance`）。互斥只护读写槽，
    /// 调用前 clone 出 Arc 即放锁——绝不持锁跨调用。
    switch_gate: Mutex<Option<SwitchAuthorizationGate>>,
    seq: AtomicU64,
    /// 指纹锚点（P1-1a；None = 未接线，授权成立时不回写登记值）。
    registry_anchor: Option<Arc<dyn RegistryFingerprintAnchor>>,
}

impl DshAuthService {
    /// 注入构造（加载/创建私钥 + 绑定账本；测试注入 tempdir env）。
    pub fn new(env: DshEnvironment) -> Result<Self, DshError> {
        Self::build(env, None, None)
    }

    /// 实例作用域构造（H05）：传入**基础** env，内部把 DSH 状态目录改指
    /// `<state_dir>/instances/<instance_id>/`，并固定实例套接字名。instance_id
    /// 形态非法 → `DSH_IPC_STATE`（不产路径，不猜）。
    pub fn new_for_instance(base: DshEnvironment, instance_id: &str) -> Result<Self, DshError> {
        let socket = instance_socket_placement(&base, instance_id).ok_or_else(|| {
            DshError::new(
                codes::IPC_STATE,
                format!("invalid dsh instance id: {instance_id}"),
            )
        })?;
        let scoped = instance_scoped_env(&base, instance_id)?;
        Self::build(scoped, Some(instance_id.to_owned()), Some(socket))
    }

    #[cfg(feature = "dsh-test-probes")]
    #[doc(hidden)]
    pub fn rendezvous_sequence_for_test(&self) -> u64 {
        self.rendezvous_sequence
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 装配内核（构造唯一路径；域状态自盘载 + I-6 待落盘撤销补刀）。
    fn build(
        env: DshEnvironment,
        instance_scope: Option<String>,
        socket: Option<(PathBuf, String)>,
    ) -> Result<Self, DshError> {
        let key = load_or_create_key(&env)?;
        let mut ledger = load_ledger(&env)?;
        // I-6：消费上次「域不可用 + 账本写不成」留下的待落盘撤销标记——
        // 撤权必须在启动时补刀，否则修复+重装后旧的 Bound 行会免批准复活。
        consume_revoke_pending(&env, &mut ledger);
        Ok(Self {
            env: Arc::new(env),
            key,
            instance_scope,
            socket,
            listener: Mutex::new(None),
            ledger: Mutex::new(ledger),
            binding_mutation: Mutex::new(()),
            revoke_write_pending: Mutex::new(HashMap::new()),
            fault_evidence: Mutex::new(None),
            binding_faulted: std::sync::atomic::AtomicBool::new(false),
            rendezvous_gate: Mutex::new(()),
            rendezvous_sequence: AtomicU64::new(0),
            published_host: Mutex::new(None),
            liveness_watcher_started: std::sync::atomic::AtomicBool::new(false),
            accept_loop_started: std::sync::atomic::AtomicBool::new(false),
            liveness_watcher_enabled: true,
            inner: Mutex::new(Inner {
                pending: Vec::new(),
                latest_key_connections: HashMap::new(),
                trusted: TrustedPeers::new(),
                trusted_hosts: HashMap::new(),
                proven_web_hosts: HashMap::new(),
                event_proofs: Vec::new(),
                used_nonces: VecDeque::new(),
                nonce_set: HashSet::new(),
                nonce_epoch: None,
                peers: HashMap::new(),
                gates: HashMap::new(),
                web_deliveries: HashMap::new(),
            }),
            intent: Mutex::new(None),
            switch_gate: Mutex::new(None),
            seq: AtomicU64::new(0),
            registry_anchor: None,
        })
    }

    /// 接线实例登记指纹锚点（P1-1a builder；生产装配在 `production::init_instance_auth`
    /// 调用，测试注入内存替身或留空）。
    pub fn with_registry_anchor(mut self, anchor: Arc<dyn RegistryFingerprintAnchor>) -> Self {
        self.registry_anchor = Some(anchor);
        self
    }

    /// P1-1：登记开关执行门（装配点接线；job-admission gate 同款注入）。重复登记
    /// 以最后一次为准（装配点单次；测试可换桩）。
    pub fn set_switch_gate(&self, gate: SwitchAuthorizationGate) {
        *self.switch_gate.lock().unwrap_or_else(|e| e.into_inner()) = Some(gate);
    }

    /// P1-1：授权提交点核对开关执行门。None（未装配）= 放行——legacy 域不归开关
    /// 管、测试基线零注入回归；装配后由闭包统一判定（关闭/缺席/代次不符 → Err）。
    fn check_switch_gate(&self, armed_generation: Option<u64>) -> Result<(), DshError> {
        let gate = self
            .switch_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match gate {
            Some(gate) => gate(armed_generation),
            None => Ok(()),
        }
    }

    /// 注入监听端（测试 mem 传输；生产在 [`Self::serve`] 内自建）。
    pub fn with_listener(self, listener: Box<dyn IpcListener>) -> Self {
        *self.listener.lock().unwrap() = Some(Arc::from(listener));
        self
    }

    /// 关闭活体跟踪拍（测试接缝）：需要手工编排 refresh 时序/采样屏障的回归
    /// （M04 并发与受压发布）不得被真实时钟的周期拍插队采样。生产装配不走此处。
    pub fn without_liveness_watcher(mut self) -> Self {
        self.liveness_watcher_enabled = false;
        self
    }

    /// 本域的当代活体（H05 审查 C1）：连同**归属核验标志**一并交还。实例作用域 =
    /// `discover_instance_candidate` 的候选（`verified` 可能是 false——授权面必须
    /// 据此拒绝绑定/交付）；旧单例 = `discover`（语义上只放行唯一已核验候选，
    /// `verified` 恒 true）。握手目标、rendezvous 发布、凭据归属钉定全部经此单源。
    fn current_live(&self) -> Result<(LocalHost, bool), DshError> {
        match &self.instance_scope {
            None => Ok((identity::discover(&self.env)?, true)),
            Some(id) => match identity::discover_instance_candidate(&self.env, id) {
                Ok(candidate) => Ok((candidate.host, candidate.verified)),
                // R01：无已识别 profile 的唯一 Web 仍可发布受限待批入口——
                // 用原始进程事实构造宿主，但归属仍未核验（false）。
                Err(e) if e.code == codes::HOST_NOT_RUNNING => {
                    // 受限入口不依赖登记：直接用观测环境反推实例 ID 并匹配。
                    let host = identity::restricted_web_host(&self.env, id)?;
                    // R01 晋升：该代际若已被「观测环境 + 内核身份 + 签名」证明过，
                    // 这就是已核验归属；换代（epoch 变）不继承。
                    let proven = self
                        .inner
                        .lock()
                        .unwrap()
                        .proven_web_hosts
                        .get(id)
                        .is_some_and(|recorded| recorded.epoch == host.epoch && recorded == &host);
                    Ok((host, proven))
                }
                Err(e) => Err(e),
            },
        }
    }

    /// R01 晋升的唯一登记点：把「该实例本代宿主归属已证明」记入内存集合。
    ///
    /// 三路共用（人工批准 / 自动授权 / 已钉钥重连）——**任何**已确立「本代信任」
    /// 的路径都必须走这里，否则 Web 重启后的凭据交付会因本代证明缺失被
    /// `INSTANCE_UNVERIFIED` 拦掉。没有实例作用域（非实例分域装配）时不登记。
    fn record_proven_web_host(&self, inner: &mut Inner, host: &LocalHost) {
        if let Some(instance_id) = self.instance_scope.clone() {
            inner.proven_web_hosts.insert(instance_id, host.clone());
        }
    }

    /// 本域的当代活体宿主（rendezvous 发布 / 落账 profile 归属等非授权面用途）。
    fn current_host(&self) -> Result<LocalHost, DshError> {
        Ok(self.current_live()?.0)
    }

    /// R02（复审 P1）：**当代代际**已核验密钥摘要——历史钉住（账本 Bound 行）与
    /// 当代观测（本方法）严格分离。
    ///
    /// 返回 Some 当且仅当：存在一个**在本宿主当代（epoch == current_host().epoch）
    /// 通过 OS 对端身份 + 签名挑战核验**的受信 Peer（trusted + trusted_hosts 均有
    /// 记录），且该 binding 在账本中为 Bound 态（摘要取自该行的钉公钥）。换代未
    /// 重新握手（epoch 不匹配）、无活握手（trusted 空）、换钥未重绑 → None。
    ///
    /// 摘要仍从账本行取，但**逐 binding_id 定点**取——与「最新 Bound 行」的假阳性
    /// 不同：该行的存在由当代代际门背书（H05 授权即写行，行与代际绑定）。
    pub fn current_generation_key_digest(&self) -> Result<Option<String>, DshError> {
        // 第九轮 P1-1：拒绝前先尝试进程内恢复（fail-closed 后的下一次使用自动
        // 收敛——补写收敛成功后无需重启）。**仅 faulted 时才取绑定互斥**：本门
        // 是高频只读就绪面，无条件下锁会排在进行中的 pin 长段（握手重采在互斥
        // 内可被外部事件停住——dsh_auth_pending_pin_race_test 的批准跨栅栏形态）
        // 之后造成互等；binding_faulted 是 SeqCst 原子位，读到 false 即按原语义
        // 直进（置位发生在撤权路径持互斥期间，最坏晚一次调用自愈）。锁序
        // binding_mutation → inner 与撤权路径同向，无反序。
        if self
            .binding_faulted
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            let _mutation = self.binding_mutation.lock().unwrap();
            self.ensure_domain_recovered_locked()?;
        }
        let current_host = self.current_host()?;
        let inner = self.inner.lock().unwrap();
        for (binding_id, host) in &inner.trusted_hosts {
            if *host != current_host {
                continue; // 换代未重新握手：旧代际的核验不算当代
            }
            // R02 返工：存活判据——连接已关闭的旧 Peer 不再贡献当代指纹。
            let alive = inner
                .trusted
                .get(binding_id)
                .map(|c| !c.is_closed())
                .unwrap_or(false);
            if !alive {
                continue;
            }
            // 摘要从内存账本按 binding_id 定点取（该行的存在由当代代际门背书）
            let spki = self
                .ledger
                .lock()
                .unwrap()
                .bindings
                .iter()
                .find(|b| &b.binding_id == binding_id && b.state == StoredBindingState::Bound)
                .map(|b| b.public_key_spki.clone());
            if let Some(spki) = spki {
                let spki = b64url_decode(&spki)?;
                return Ok(Some(key_digest(&spki)));
            }
        }
        Ok(None)
    }

    /// legacy rendezvous 交接标记（H05 审查 I3）：实例域上线时把基础状态目录里
    /// 仍在场的 legacy 记录改写为 `retired = true`——插件候选读取据此忽略旧记录，
    /// 同 PID 不再出现「legacy + 实例」双匹配停摆；「先 legacy 后选择」的生命周期
    /// 显式交接。无 legacy 记录/已是 retired → no-op；旧单例（无实例套接字布局）
    /// 不做任何事。写失败只降级为告警（交接是可用性优化，不阻塞实例域本身）。
    fn retire_legacy_rendezvous(&self, host: &LocalHost) {
        let Some((base_dir, _)) = &self.socket else {
            return; // 旧单例：无交接语义
        };
        let legacy = base_dir.join("rendezvous.json");
        let Ok(bytes) = std::fs::read(&legacy) else {
            return; // 无 legacy 记录：没有要交接的东西
        };
        let Ok(record) = serde_json::from_slice::<RendezvousRecord>(&bytes) else {
            return; // 畸形 legacy 记录：不猜、不改写
        };
        if record.retired {
            return; // 已交接
        }
        // R04（复审 P2）：legacy 记录属于**别的实例/代际**时不退休——B 上线不得
        // 停用 A 的 legacy 入口。只有与本次交接的宿主身份（host_id+pid+启动时刻+
        // epoch）逐字匹配的记录才允许置 retired；异主记录保持原样。
        if !same_handover_generation(&record, host) {
            log::info!(
                "dsh legacy rendezvous belongs to another host generation; leave it untouched"
            );
            return;
        }
        // I3-r3：交接的「读→改→写」与 legacy 发布共用同一排他窗口——窗口内重读
        // （可能与并发 publish 交错），仍非 retired 才写 true；写合并保证最终态
        // 恒为 retired（不存在「读到 false → 交接写 true → 旧 publisher 提交
        // 预构造 false」的交错撤销）。
        let gate = ROOT_RENDEZVOUS_GATE.lock().unwrap();
        let lock = match root_rendezvous_lock(base_dir) {
            Ok(lock) => lock,
            Err(e) => {
                log::warn!(
                    "dsh legacy rendezvous retire lock failed: {} ({})",
                    e.code,
                    e.detail
                );
                return;
            }
        };
        (|| {
            let Ok(fresh_bytes) = std::fs::read(&legacy) else {
                return; // 窗口内记录消失：没有要交接的东西
            };
            let Ok(mut fresh) = serde_json::from_slice::<RendezvousRecord>(&fresh_bytes) else {
                return; // 畸形：不猜、不改写
            };
            if fresh.retired {
                return; // 已交接（窗口内被并发 retire/publish 处理）
            }
            // 窗口内记录可能已被异主 publish 覆盖：重读后再次核对代际归属
            if !same_handover_generation(&fresh, host) {
                return; // 异主记录：不碰
            }
            fresh.retired = true;
            let out = match serde_json::to_vec_pretty(&fresh) {
                Ok(out) => out,
                Err(e) => {
                    log::warn!("dsh legacy rendezvous retire marker serialize failed: {e}");
                    return;
                }
            };
            if let Err(e) = atomic_write_0600(&legacy, &out) {
                log::warn!(
                    "dsh legacy rendezvous retire marker write failed at {}: {} ({})",
                    legacy.display(),
                    e.code,
                    e.detail
                );
            }
        })();
        drop(lock);
        drop(gate);
    }

    /// Rust 公钥（SPKI DER；rendezvous / 测试观测用，非秘密）。
    pub fn public_key_spki(&self) -> &[u8] {
        &self.key.spki
    }

    /// 注入时钟的当前毫秒（握手 TTL 判定与测试观测同一来源）。
    pub fn now_ms(&self) -> i64 {
        self.env.clock.now_ms()
    }

    /// 本端公钥指纹（SHA-256 hex 前 16 位；两端身份核对面展示用，非秘密）。
    pub fn rust_fingerprint(&self) -> String {
        self.key.digest.chars().take(16).collect()
    }

    /// Rust 公钥摘要（客户端构造签名消息 keyDigests[0] 用）。
    pub fn key_digest(&self) -> &str {
        &self.key.digest
    }

    /// 本域钉住的实例 ID（`None` = 旧单例/legacy 域，没有实例登记可锚）。
    ///
    /// 授权面（批准、自动授权、凭据交付）用它把「已核验的对端指纹」回写到
    /// [`crate::services::dsh_control::instances::InstanceRegistry`]——登记值
    /// 是 resolve/写门唯一可比对的当代身份，缺这一步首绑之后永远
    /// `DSH_INSTANCE_UNVERIFIED`。
    pub fn instance_scope_id(&self) -> Option<&str> {
        self.instance_scope.as_deref()
    }

    /// 该实例**当代**已核验的对端指纹摘要（`None` = 本代尚无核验过的活绑定）。
    ///
    /// 与 [`Self::current_generation_key_digest`] 的区别：本方法**不要求**活
    /// 连接仍在（批准刚落账、连接随后关闭的窗口内仍要能锚登记值）；代际门
    /// （`trusted_hosts` 记录的宿主 == 当代宿主）与 Bound 账本行照旧是前提。
    pub fn verified_peer_fingerprint(&self) -> Result<Option<String>, DshError> {
        let current_host = self.current_host()?;
        let inner = self.inner.lock().unwrap();
        let binding_id = inner
            .trusted_hosts
            .iter()
            .find(|(_, host)| **host == current_host)
            .map(|(binding_id, _)| binding_id.clone());
        drop(inner);
        let Some(binding_id) = binding_id else {
            return Ok(None);
        };
        let spki = self
            .ledger
            .lock()
            .unwrap()
            .bindings
            .iter()
            .find(|b| b.binding_id == binding_id && b.state == StoredBindingState::Bound)
            .map(|b| b.public_key_spki.clone());
        match spki {
            Some(spki) => Ok(Some(key_digest(&b64url_decode(&spki)?))),
            None => Ok(None),
        }
    }

    /// 把本代已核验的对端指纹锚进实例登记（幂等；同值不动盘）。
    ///
    /// 锚点由 [`Self::with_registry_anchor`] 注入：生产装配注入真实登记，测试
    /// 注入内存替身或**不注入**（不注入 = 不锚，保持测试 hermetic——绝不能在这里
    /// 直接取 `production_registry()` 进程单例，那会让任何测试都写真实状态目录）。
    ///
    /// 只在**空登记值**（`register_candidate` 落的「尚未受信锚定」）上写：既有非空
    /// 值与当代核验值不一致 = 身份不连续，**拒绝**并要求用户显式重新选择/授权，
    /// 绝不因为一次新握手就静默换掉已钉身份。这条判定在锚点实现内。
    pub fn anchor_registry_fingerprint(&self) -> Result<(), DshError> {
        let Some(instance_id) = self.instance_scope.clone() else {
            return Ok(()); // 旧单例域：没有登记记录可锚
        };
        let Some(anchor) = self.registry_anchor.as_ref() else {
            return Ok(()); // 未注入锚点（测试/legacy）：不锚，不猜
        };
        let Some(fingerprint) = self.verified_peer_fingerprint()? else {
            return Ok(()); // 本代尚无核验过的活绑定：不需要锚
        };
        anchor.anchor_fingerprint(&instance_id, &fingerprint)
    }

    // --------------------------------------------------------
    // IPC 服务循环
    // --------------------------------------------------------

    /// 启动本机 IPC 服务：绑定 → 发布 rendezvous → 后台 accept 循环。
    /// 幂等（已启动直接返回）；每次 accept 刷新 rendezvous（换代跟随）。
    ///
    /// **路径长度策略提前**（Mac 补探测 §6 第 7 条）：生产监听端要占文件系统，
    /// 深 state 目录在 bind 时必然失败。这里在创建监听端之前就先校验，给出可诊断
    /// 的 `DSH_STATE_PATH_TOO_LONG`（而不是裸的 SUN_LEN 报错）。注入的监听端
    /// （测试 mem）不占文件系统，故不校验。
    ///
    /// **P1-3（Round-7 Task 3）：接受循环 spawn 与发布解耦**——bind 成功即启动
    /// 接受循环（CAS 幂等）。首次 serve 的发布失败（非 HOST_NOT_RUNNING 的
    /// fail-closed 归属门、状态 IO）照旧上抛（记录不发、不退役——fail-closed
    /// 面不放宽），但**接受循环照常在场**：每连接握手仍逐连接核验（负例回归
    /// 已有），发布由活体跟踪拍 / 连接触发刷新补齐。没有这条解耦，端口晚于进程
    /// 就绪（DSH 进程已在、回环监听未就绪）时首次 serve 失败会留下「已 bind 但
    /// 无人 accept」的监听端，重试永远走幂等分支——重控通道直到重启兔维斯。
    pub async fn serve(self: &Arc<Self>) -> Result<(), DshError> {
        // 已注入（测试 mem 传输）或已启动 → 复用；否则建生产监听端。
        let listener = {
            let slot = self.listener.lock().unwrap();
            match slot.as_ref() {
                Some(l) => Some(Arc::clone(l)),
                None => None,
            }
        };
        let listener = match listener {
            Some(l) => l,
            None => {
                // 实例作用域：套接字落基础状态目录、固定名 `ipc-<instance_id>.sock`
                // （预算按实际名长校验）；旧单例：状态目录 + 随机名，行为逐字保留。
                let (dir, name): (PathBuf, Option<String>) = match &self.socket {
                    Some((dir, name)) => (dir.clone(), Some(name.clone())),
                    None => (state_dir(&self.env), None),
                };
                if let Some(name) = &name {
                    super::local_ipc::check_socket_path_length(&dir, name.len())?;
                } else {
                    super::local_ipc::check_state_dir_length(&dir)?;
                }
                let l: Arc<dyn IpcListener> = Arc::from(
                    super::local_ipc::production_listener_named(dir, name.as_deref()),
                );
                *self.listener.lock().unwrap() = Some(Arc::clone(&l));
                l
            }
        };
        if listener.endpoint().is_some() {
            // 幂等：已 bind 的监听端不重复启动；活体跟踪与接受循环只补位
            // （已启动则 no-op）。补接受循环 = P1-3 的恢复面：首次 serve 在 bind
            // 后、spawn 前失败（发布上抛）时，重试从这里把循环补齐。
            self.ensure_liveness_watcher();
            self.ensure_accept_loop();
            return Ok(());
        }
        let address = listener.bind().await?;
        // P1-3：bind 成功即启动接受循环（先于发布——发布失败不再让它缺席）。
        self.ensure_accept_loop();
        // 活体跟踪同步补位：发布失败（含 HOST_NOT_RUNNING 推迟面）后由跟踪拍
        // 周期重试发布（宿主出现/变为可核验时补发布）。
        self.ensure_liveness_watcher();
        // 初始发布（Task 2）：**授权监听与握手记录发布分别维护**——监听启动成功后，
        // 发布失败不再让整个服务退役。宿主未在场（兔维斯先于 DSH 启动）只是推迟：
        // 活体跟踪拍会在宿主出现时补发布。其余失败（INSTANCE_UNVERIFIED 等
        // fail-closed 归属门、状态 IO）维持原上抛语义：不发布记录（fail-closed
        // 面不放宽），接受循环已在场（每连接握手逐连接核验）。
        let publish_service = Arc::clone(self);
        let initial =
            tokio::task::spawn_blocking(move || publish_service.publish_current(&address))
                .await
                .map_err(|_| DshError::new(codes::IPC_STATE, "rendezvous probe task failed"))?;
        match initial {
            Ok(_) => {}
            Err(e) if e.code == codes::HOST_NOT_RUNNING => {
                log::info!(
                    "dsh rendezvous publish deferred until a host appears: {} ({})",
                    e.code,
                    e.detail
                );
            }
            Err(e) => return Err(e),
        }
        Ok(())
    }

    /// 启动后台 accept 循环（P1-3 抽取 + CAS 幂等；至多一个任务）。循环体与
    /// 原内联实现逐字一致：每 accept 先做活体跟踪单拍（换代跟随 + 重发布）再
    /// 逐连接握手。
    fn ensure_accept_loop(self: &Arc<Self>) {
        if self
            .accept_loop_started
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_err()
        {
            return;
        }
        let svc = self.clone();
        tokio::spawn(async move {
            loop {
                // 在 guard 外 await（std MutexGuard 不 Send，不能跨 await 持有）。
                let listener = {
                    let slot = svc.listener.lock().unwrap();
                    slot.as_ref().map(Arc::clone)
                };
                let conn = match listener {
                    Some(l) => l.accept().await,
                    None => return,
                };
                match conn {
                    Ok(conn) => {
                        let sequence = svc
                            .rendezvous_sequence
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                            .wrapping_add(1);
                        // 所有实例共享预算；在提交阻塞任务之前等待，避免未认证连接占满线程池。
                        let permit = match unauthenticated_probe_budget().acquire_owned().await {
                            Ok(permit) => permit,
                            Err(_) => return,
                        };
                        let svc2 = svc.clone();
                        let conn: Arc<dyn IpcClientConnection> = Arc::from(conn);
                        tokio::spawn(async move {
                            // 新连接是「记录可能过期」的信号：统一走活体跟踪单拍
                            // （含换代失效 + 重发布；记录已一致则零写盘）。探测含进程/
                            // 监听/磁盘 IO，进阻塞线程；失败不阻塞连接处理——周期
                            // 跟踪拍会重试。
                            let refresh = Arc::clone(&svc2);
                            let _ = tokio::task::spawn_blocking(move || {
                                let _permit = permit;
                                if refresh
                                    .rendezvous_sequence
                                    .load(std::sync::atomic::Ordering::SeqCst)
                                    != sequence
                                {
                                    return;
                                }
                                if let Ok(outcome) = refresh.refresh_host_tracking() {
                                    log::trace!("dsh rendezvous refresh on accept: {outcome:?}");
                                }
                            })
                            .await;
                            svc2.handle_connection(conn).await;
                        });
                    }
                    Err(e) => {
                        log::warn!("dsh local ipc accept failed: {}", e.detail);
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                }
            }
        });
    }

    // --------------------------------------------------------
    // 活体跟踪与握手记录发布（监听已启动也必须能刷新记录；Task 2）
    // --------------------------------------------------------

    /// 当前监听端点（未 bind → None）。
    fn listener_endpoint(&self) -> Option<String> {
        self.listener
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|l| l.endpoint())
    }

    /// 已发布代际的宿主进程是否仍在进程表（pid + 启动时刻精确对——PID 复用必换
    /// 启动时刻，不会把复用者误判成旧代存活）。
    fn published_generation_alive(&self, host: &LocalHost) -> bool {
        self.env
            .processes
            .list_processes()
            .iter()
            .any(|p| p.pid == host.pid && p.started_at_ms == host.started_at_ms)
    }

    /// 盘上当代记录是否已与本域（宿主代际 + 监听地址 + 公钥）一致——一致就不重写
    /// （避免每拍写盘翻动文件、触发插件侧域比对）。
    fn record_is_current(&self, host: &LocalHost, address: &str) -> bool {
        let Ok(bytes) = std::fs::read(rendezvous_path(&self.env)) else {
            return false;
        };
        let Ok(record) = serde_json::from_slice::<RendezvousRecord>(&bytes) else {
            return false;
        };
        !record.retired
            && record.address == address
            && record.public_key_spki == b64url_encode(&self.key.spki)
            && same_handover_generation(&record, host)
    }

    /// 发布当代握手记录（rendezvous_gate 内）：换代先失效旧代连接与临时凭据，
    /// 再写新记录 + legacy 交接，最后记账已发布代际。
    fn publish_current(&self, address: &str) -> Result<LocalHost, DshError> {
        let _gate = self.rendezvous_gate.lock().unwrap();
        let host = self.current_host()?;
        let previous = self.published_host.lock().unwrap().clone();
        if let Some(old) = previous.as_ref() {
            if *old != host {
                // 换代：旧代连接与临时凭据就地失效（闸门不动——同钥重连沿用同一
                // 绑定的闸门；已撤闸门保持已撤）。
                self.invalidate_generation(old);
            }
        }
        let root_scope = self.socket.is_none();
        publish_rendezvous(&self.env, address, &self.key, &host, root_scope)?;
        self.retire_legacy_rendezvous(&host);
        *self.published_host.lock().unwrap() = Some(host.clone());
        Ok(host)
    }

    /// 宿主退出：停用盘上记录 + 失效旧代连接与临时凭据 + 清发布记账。
    fn retire_published_generation(&self, host: &LocalHost) {
        let _gate = self.rendezvous_gate.lock().unwrap();
        self.retire_record_marker(host);
        self.invalidate_generation(host);
        *self.published_host.lock().unwrap() = None;
    }

    /// 停用盘上当代记录（写 `retired = true`；记录缺失/畸形/异主/已停用 → no-op）。
    ///
    /// 与 [`publish_rendezvous`] 同代际判据（完整四元组）：只有确属本域已发布代际
    /// 的记录才停用——异主记录不碰（I3/R4 同款纪律）；失败降级为告警（记录指向
    /// 已退出代际时插件按 PID 过滤本就连不上，握手代际门兜底）。
    fn retire_record_marker(&self, host: &LocalHost) {
        let target = rendezvous_path(&self.env);
        let write_marker = |record: &mut RendezvousRecord| -> Result<(), DshError> {
            record.retired = true;
            atomic_write_0600(
                &target,
                serde_json::to_vec_pretty(record)
                    .expect("RendezvousRecord 序列化不可失败")
                    .as_slice(),
            )
        };
        if self.socket.is_none() {
            // 根域（legacy）：进根写窗口（I3-r3 同款：窗口内重读，只停用确属该代
            // 且未停用的记录）。
            let root_gate = ROOT_RENDEZVOUS_GATE.lock().unwrap();
            let lock = match root_rendezvous_lock(&state_dir(&self.env)) {
                Ok(lock) => lock,
                Err(e) => {
                    log::warn!(
                        "dsh rendezvous retire lock failed: {} ({})",
                        e.code,
                        e.detail
                    );
                    return;
                }
            };
            let result = (|| {
                let Ok(bytes) = std::fs::read(&target) else {
                    return Ok(());
                };
                let Ok(mut record) = serde_json::from_slice::<RendezvousRecord>(&bytes) else {
                    return Ok(());
                };
                if record.retired || !same_handover_generation(&record, host) {
                    return Ok(());
                }
                write_marker(&mut record)
            })();
            drop(lock);
            drop(root_gate);
            if let Err(e) = result {
                log::warn!(
                    "dsh rendezvous retire write failed: {} ({})",
                    e.code,
                    e.detail
                );
            }
            return;
        }
        // 实例域：独立文件（rendezvous_gate 已由调用方持有）。
        let Ok(bytes) = std::fs::read(&target) else {
            return;
        };
        let Ok(mut record) = serde_json::from_slice::<RendezvousRecord>(&bytes) else {
            return;
        };
        if record.retired || !same_handover_generation(&record, host) {
            return;
        }
        if let Err(e) = write_marker(&mut record) {
            log::warn!(
                "dsh rendezvous retire write failed: {} ({})",
                e.code,
                e.detail
            );
        }
    }

    /// 旧代失效（换代或退出）：关旧代受信连接、清信任/凭据/待批/证明。
    ///
    /// 闸门**不动**：同一 binding（同钥）换代重连必须沿用同一闸门；已撤闸门保持
    /// 已撤。旧代待批记录一并退场（未批准的挑战不得跨代存活）。
    fn invalidate_generation(&self, host: &LocalHost) -> usize {
        let mut inner = self.inner.lock().unwrap();
        let stale: Vec<String> = inner
            .trusted_hosts
            .iter()
            .filter(|(_, recorded)| **recorded == *host)
            .map(|(id, _)| id.clone())
            .collect();
        let mut closed: Vec<Arc<dyn IpcClientConnection>> = Vec::new();
        for id in &stale {
            if let Some(conn) = inner.trusted.remove(id) {
                closed.push(conn);
            }
            inner.trusted_hosts.remove(id);
            // 旧代临时凭据句柄（peers 槽）**保留但失效**：不显式清槽是既有合同
            // （T09-G `stale_generation_refusal_is_per_binding`）——authorize_grant /
            // peer_for_control 在消费点按当代代际+authority 复核，旧代句柄一律
            // `AUTH_EPOCH_STALE` / None（插件据诊断码知道「等待当代重交付」而非
            // 「从未交付」）。这里失效的是它的**可用面**：受信连接已关、信任与
            // 归属证明已清，凭据无法再经任何路径被消费。
        }
        inner.pending.retain(|p| {
            if p.host == *host {
                closed.push(Arc::clone(&p.conn));
                false
            } else {
                true
            }
        });
        inner
            .event_proofs
            .retain(|p| !closed.iter().any(|c| Arc::ptr_eq(c, &p.conn)));
        // 旧代 web 归属证明不跨代继承（R01 晋升本就按 epoch 判等，这里显式清除）。
        inner
            .proven_web_hosts
            .retain(|_, recorded| *recorded != *host);
        let count = closed.len();
        drop(inner);
        for conn in closed {
            conn.close();
        }
        count
    }

    /// 活体跟踪单拍（可周期调用；发现与发布失败由调用方重试，本方法不自行重试）。
    ///
    /// * 宿主在场且盘上记录已一致 → 无动作；否则（首次发布 / 兔维斯重启后 /
    ///   换代 / 地址变化）在发布窗口内**重采当代**并发布——首次采样过期的任务
    ///   也发布窗口内重采到的当代（绝不发布旧代，见 M04 并发回归）；
    /// * 换代（发布代际 ≠ 当代）在发布窗口内先失效旧代连接与临时凭据；
    /// * 发现失败时用**直接进程探针**区分「宿主真的退出」（停用记录 + 失效旧代）
    ///   与「发现面暂时证不出」（监听缺失/归属门：不动旧代、维持 fail-closed，
    ///   交由下一拍重试）——直接探针只在失败路径跑，成功路径保持每拍两次宿主
    ///   采样的既有节奏（M04 受压发布回归的采样合同）；
    /// * P1-4：宿主在场时顺带执行 [`Self::reconcile_console_credentials`]——
    ///   受信当代 peer 在场但控制台网页凭据缺失（整域撤权/遗留 bug 清空）→
    ///   自动重交付（跟踪拍是唯一触发点：插件连接持续在场时不会重新握手）。
    pub fn refresh_host_tracking(&self) -> Result<HostTracking, DshError> {
        let Some(address) = self.listener_endpoint() else {
            return Ok(HostTracking::NoListener);
        };
        let previous = self.published_host.lock().unwrap().clone();
        match self.current_live() {
            Ok((host, _verified)) => {
                // P1-4：凭据对账与发布解耦（对账失败只记日志，不影响跟踪结论）。
                self.reconcile_console_credentials();
                if previous.as_ref() == Some(&host) && self.record_is_current(&host, &address) {
                    return Ok(HostTracking::Current);
                }
                self.publish_current(&address)?;
                // previous 与当代不同 = 本拍发生了换代衔接（旧代连接/凭据已在
                // 发布窗口内失效）；首次发布则为 false。
                Ok(HostTracking::Published {
                    replaced: previous.as_ref().is_some_and(|old| *old != host),
                })
            }
            Err(e) => {
                let Some(old) = previous.as_ref() else {
                    // 从未发布过：没有要失效的旧代。未运行是常态（等宿主上场），
                    // 其余发现失败如实上抛（fail-closed：不发布、不失效）。
                    return if e.code == codes::HOST_NOT_RUNNING {
                        Ok(HostTracking::HostAbsent { invalidated: false })
                    } else {
                        Err(e)
                    };
                };
                if self.published_generation_alive(old) {
                    // 已发布代际的进程仍在场：只是发现面暂时证不出——不发布、
                    // 不失效（宿主没有退出，换代未发生），交由下一拍重试。
                    return Err(e);
                }
                // 已发布代际的宿主确实退出：停用记录 + 失效旧代连接与临时凭据。
                self.retire_published_generation(old);
                Ok(HostTracking::HostAbsent { invalidated: true })
            }
        }
    }

    /// 控制台网页凭据对账（P1-4 自动重交付触发点；同步方法，活体跟踪拍内执行）。
    ///
    /// 触发条件（缺一不可）：
    /// 1. 实例作用域在位（legacy 无作用域域无控制台凭据键，无从对账）；
    /// 2. 有 Bound 绑定且**受信连接在场**（插件持续在线——离线的插件重连时会
    ///    自然重新走 handshake/credential_ack 交付链，不需要本触发点）；
    /// 3. 开关执行门放行（P1-4 复审 Minor-2 防御纵深：传递性安全本已成立——
    ///    disable 撤活跃绑定 → 闸门撤 → 第 ② 步挡住；此处显式拒绝「残留非活跃
    ///    Bound 行」（T09-G 允许多 Bound 并存）给**已关闭实例**重交付凭据）；
    /// 4. 当代宿主在场且归属已核验（对账只在当代做；换代由重连链重交付）；
    /// 5. 该绑定有**交付时保留的网页凭据副本**，且其代际/authority 与当代一致；
    /// 6. 控制台凭据确有缺失（api 缺失；浏览器项只对「交付时带浏览器 cookie」
    ///    的副本参与判定——见 ④ 的收敛性说明）。
    ///
    /// 满足则经**与 handshake/credential_ack 完全相同的交付链**
    /// （`install_upstream_credentials_with_gate_at_sequence`）重装凭据。
    /// 副本闸门已撤/代际不符/安装失败 → 只记日志，交由下一拍重试或等待重连链。
    ///
    /// 收敛性：每次重装都把缺失面补齐（api + browser + mount 三面同链落位），
    /// 下一拍判定即全绿——不存在「每拍重装」的定态；api-only 交付（副本无
    /// 浏览器 cookie）的浏览器侧不参与缺失判定（该通道本来就不可用，重装永远
    /// 补不出 mount 凭据），只对 api 缺失做**一次性**收敛重装。
    ///
    /// 返回是否发生了重交付（测试/日志消费；调用方不据此失败）。
    fn reconcile_console_credentials(&self) -> bool {
        let Some(instance_id) = self.instance_scope.clone() else {
            return false;
        };
        let (closing, generation) = super::production::closing_snapshot(&instance_id);
        if closing {
            return false;
        }
        let sequence =
            crate::remote::console_mount::production().credential_sequence_for(&instance_id);
        // ① Bound 绑定 + 受信连接在场 + 闸门未撤。
        let binding_id = {
            let ledger = self.ledger.lock().unwrap();
            ledger
                .bindings
                .iter()
                .rev()
                .find(|b| b.state == StoredBindingState::Bound)
                .map(|b| b.binding_id.clone())
        };
        let Some(binding_id) = binding_id else {
            return false;
        };
        let connected = {
            let inner = self.inner.lock().unwrap();
            inner.trusted.contains_key(&binding_id)
                && !inner
                    .gates
                    .get(&binding_id)
                    .is_some_and(|gate| gate.is_revoked())
        };
        if !connected {
            return false;
        }
        // ② 开关执行门（P1-4 复审 Minor-2）：实例已关闭 → 不重交付。未接线门
        // （legacy/测试基线）恒放行，与授权提交点同一语义。
        if self.check_switch_gate(None).is_err() {
            return false;
        }
        // ③ 当代宿主（对账只认当代；副本代际不符 = 换代未重交付，走重连链）。
        let Ok((host, verified)) = self.current_live() else {
            return false;
        };
        if !verified {
            return false;
        }
        // ④ 交付时保留的副本（代际/authority 与当代一致 + 闸门未撤）。
        let retained = {
            let inner = self.inner.lock().unwrap();
            inner.web_deliveries.get(&binding_id).map(|delivery| {
                (
                    delivery.api.clone(),
                    delivery.browser.clone(),
                    delivery.epoch,
                    delivery.authority.clone(),
                    Arc::clone(&delivery.gate),
                )
            })
        };
        let Some((api, browser, epoch, authority, gate)) = retained else {
            return false;
        };
        if gate.is_revoked() || epoch != host.epoch || authority != host.authority {
            return false;
        }
        // ⑤ 凭据缺失判定。
        //
        // 浏览器项只对「交付时带浏览器 cookie」的副本参与判定（P1-4 复审
        // Important 收敛修正）：api-only 交付（旧插件不发 browserCookie / 兑换
        // 失败留空——显式支持态）的浏览器通道本来就不可用——mount 凭据永不安装、
        // lifecycle browser 恒空，若纳入判定则**永不收敛**：每拍重装、每拍 bump
        // 挂载的**全局**撤销序号（其他实例在飞的控制台入口登记被拒）+ 日志刷屏
        // ——恰好复刻本任务要消灭的「一个实例的状态打扰别人」。浏览器侧为空时
        // 只对 api 缺失做一次性收敛重装。
        let expects_browser = !browser.is_empty();
        let api_present = crate::remote::console_lifecycle::upstream_credential_matches(
            &instance_id,
            host.epoch,
            &host.authority,
        );
        // 浏览器侧完好 = 不期望浏览器 cookie（api-only 交付，视为恒完好）
        // 或 lifecycle browser 与 mount 凭据都按当代目标在位。
        let browser_side_ok = !expects_browser
            || (crate::remote::console_lifecycle::upstream_browser_cookie_for(
                &instance_id,
                host.epoch,
                &host.authority,
            )
            .is_some()
                && crate::remote::console_mount::production().browser_credential_installed(
                    &instance_id,
                    host.epoch,
                    &host.authority,
                ));
        if api_present && browser_side_ok {
            return false;
        }
        let _mutation = self.binding_mutation.lock().unwrap();
        if self.require_bound_credential(&binding_id).is_err() {
            return false;
        }
        let installed =
            super::production::with_open_instance_generation(&instance_id, generation, || {
                let inner = self.inner.lock().unwrap();
                if !inner
                    .trusted
                    .get(&binding_id)
                    .is_some_and(|conn| !conn.is_closed())
                    || !inner.web_deliveries.get(&binding_id).is_some_and(|entry| {
                        entry.epoch == epoch
                            && entry.authority == authority
                            && entry.api == api
                            && entry.browser == browser
                            && Arc::ptr_eq(&entry.gate, &gate)
                    })
                {
                    return Err("credential was superseded".to_string());
                }
                crate::remote::console_lifecycle::install_upstream_credentials_committing(
                    &instance_id,
                    api,
                    browser,
                    epoch,
                    &authority,
                    gate,
                    sequence,
                    || {},
                )
            })
            .map_err(|error| error.code.to_string())
            .and_then(|result| result);
        match installed {
            Ok(()) => {
                log::info!(
                    "dsh console web credentials redelivered by liveness tracking (instance={instance_id})"
                );
                true
            }
            Err(error) => {
                log::warn!(
                    "dsh console credential reconcile skipped (will retry): {error} (instance={instance_id})"
                );
                false
            }
        }
    }

    /// 启动活体跟踪拍（幂等；serve 成功路径与幂等路径都调用，任务至多一个）。
    ///
    /// 为什么需要它：常规刷新原来只挂在新连接上——新宿主没有匹配记录就不会来连，
    /// 旧记录便永远停在已退出的代际上（2026-11-13 已证事实）。跟踪拍让「发现 +
    /// 发布」脱离连接事件：宿主退出/换代/兔维斯重启后的记录修复全部可自愈。
    fn ensure_liveness_watcher(self: &Arc<Self>) {
        if !self.liveness_watcher_enabled {
            return;
        }
        if self
            .liveness_watcher_started
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_err()
        {
            return;
        }
        let svc = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(LIVENESS_POLL_MS)).await;
                let tracker = Arc::clone(&svc);
                // 发现与发布都含磁盘/进程表探测：进阻塞线程，不占异步执行线程。
                let outcome =
                    tokio::task::spawn_blocking(move || tracker.refresh_host_tracking()).await;
                match outcome {
                    Ok(Ok(HostTracking::Published { replaced })) => {
                        log::info!(
                            "dsh rendezvous republished by liveness tracking (replaced={replaced})"
                        );
                    }
                    Ok(Ok(HostTracking::HostAbsent { invalidated: true })) => {
                        log::info!(
                            "dsh host generation exited; record retired and old-generation connections/credentials invalidated"
                        );
                    }
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => {
                        log::warn!(
                            "dsh liveness tracking failed (will retry): {} ({})",
                            e.code,
                            e.detail
                        );
                    }
                    Err(_) => {
                        log::warn!("dsh liveness tracking task failed (will retry)");
                    }
                }
            }
        });
    }

    fn signed_event_line(
        &self,
        conn: &Arc<dyn IpcClientConnection>,
        binding_id: &str,
        event: &str,
    ) -> Option<String> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Event<'a> {
            purpose: &'static str,
            #[serde(flatten)]
            challenge: &'a BindingChallengeMessage,
            decision_signature: &'a str,
            event: &'a str,
            binding_id: &'a str,
        }
        let inner = self.inner.lock().unwrap();
        let proof = inner
            .event_proofs
            .iter()
            .find(|p| Arc::ptr_eq(&p.conn, conn) && p.binding_id == binding_id)?;
        let bytes = serde_json::to_vec(&Event {
            purpose: "tuvis.dsh.binding-event.v2",
            challenge: &proof.challenge,
            decision_signature: &proof.decision_signature,
            event,
            binding_id,
        })
        .ok()?;
        let signature = b64url_encode(&self.key.sign(&bytes));
        Some(
            serde_json::json!({
                "type": if event == "revocation" { "revoked" } else { "granted" },
                "binding_id": binding_id, "event": event, "eventSignature": signature,
            })
            .to_string(),
        )
    }

    /// 单连接处理循环（accept 任务体）：hello / credential 两种帧。
    async fn handle_connection(self: Arc<Self>, conn: Arc<dyn IpcClientConnection>) {
        let handshake_gate = Arc::new(tokio::sync::Mutex::new(()));
        loop {
            let line = match conn.recv().await {
                Ok(line) => line,
                Err(_) => {
                    Arc::clone(&conn).close();
                    break; // 对端关闭/IO 错误：退役连接后清线
                }
            };
            let frame = match parse_client_frame(&line) {
                Ok(frame) => frame,
                Err(e) => {
                    let _ = conn
                        .send(&server_frame_line(&ServerFrame::Error {
                            error: e.code.to_string(),
                            detail: e.detail.clone(),
                        }))
                        .await;
                    continue;
                }
            };
            match frame {
                ClientFrame::Hello(hello) => {
                    let _gate = handshake_gate.lock().await;
                    let service = Arc::clone(&self);
                    let probe_conn = Arc::clone(&conn);
                    let probe_hello = hello.clone();
                    let probe_gate = Arc::clone(&handshake_gate);
                    let runtime = tokio::runtime::Handle::current();
                    let permit = match unauthenticated_probe_budget().acquire_owned().await {
                        Ok(permit) => permit,
                        Err(_) => {
                            Arc::clone(&conn).close();
                            break;
                        }
                    };
                    let probed = tokio::task::spawn_blocking(move || {
                        runtime.block_on(service.handle_hello(
                            &probe_conn,
                            &probe_hello,
                            &probe_gate,
                            permit,
                        ))
                    })
                    .await
                    .unwrap_or_else(|_| {
                        Err(DshError::new(
                            codes::IPC_STATE,
                            "handshake probe task failed",
                        ))
                    });
                    let reply = match probed {
                        Ok(frame) => frame,
                        Err(e) => ServerFrame::Error {
                            error: e.code.to_string(),
                            detail: e.detail.clone(),
                        },
                    };
                    if let ServerFrame::ServerHello(ref response) = reply {
                        if response.state != "revoked" {
                            if let (Some(binding_id), Ok(client_spki)) =
                                (&response.binding_id, b64url_decode(&hello.public_key_spki))
                            {
                                let challenge = BindingChallengeMessage {
                                    binding_protocol: hello.binding_protocol,
                                    key_digests: [
                                        self.key.digest.clone(),
                                        key_digest(&client_spki),
                                    ],
                                    nonce: hello.nonce.clone(),
                                    host_id: hello.host_id.clone(),
                                    epoch: hello.epoch.clone(),
                                    expiry: hello.expiry,
                                    home_binding: hello.home_binding.clone(),
                                };
                                let mut inner = self.inner.lock().unwrap();
                                inner.event_proofs.retain(|p| !Arc::ptr_eq(&p.conn, &conn));
                                inner.event_proofs.push(EventProof {
                                    conn: Arc::clone(&conn),
                                    challenge,
                                    decision_signature: response.decision_signature.clone(),
                                    binding_id: binding_id.clone(),
                                });
                            }
                        }
                    }
                    if conn.send(&server_frame_line(&reply)).await.is_err() {
                        Arc::clone(&conn).close();
                        break;
                    }
                }
                ClientFrame::Credential(cred) => {
                    let scope = self
                        .instance_scope
                        .as_deref()
                        .map(|id| (id.to_owned(), super::production::instance_generation(id)));
                    let sequence = scope.as_ref().map(|(instance, _)| {
                        crate::remote::console_mount::production().credential_sequence_for(instance)
                    });
                    let reply = match self.signed_event_line(
                        &conn,
                        &cred.binding_id,
                        "credential_ack",
                    ) {
                        None => server_frame_line(&ServerFrame::Error {
                            error: codes::AUTH_NOT_APPROVED.to_string(),
                            detail: "missing authenticated handshake".to_string(),
                        }),
                        Some(ack) => {
                            let service = Arc::clone(&self);
                            let probe_conn = Arc::clone(&conn);
                            let probe_cred = cred.clone();
                            let accepted = tokio::task::spawn_blocking(move || {
                                service.verify_credential(&probe_conn, &probe_cred)
                            })
                            .await
                            .unwrap_or_else(|_| {
                                Err(DshError::new(
                                    codes::IPC_STATE,
                                    "credential probe task failed",
                                ))
                            });
                            let committed = match accepted {
                                Ok(host) => {
                                    #[cfg(feature = "dsh-test-probes")]
                                    if let Some(hook) = CREDENTIAL_ACCEPTED_HOOK.get() {
                                        hook().await;
                                    }
                                    let service = Arc::clone(&self);
                                    let probe_conn = Arc::clone(&conn);
                                    let commit_scope = scope.clone();
                                    tokio::task::spawn_blocking(move || {
                                        #[cfg(feature = "dsh-test-probes")]
                                        if let Some((instance, _)) = commit_scope.as_ref() {
                                            if let Some(hook) = BEFORE_NATIVE_CREDENTIAL_COMMIT.get() { hook(instance); }
                                            crate::remote::console_lifecycle::observe_console_credential_prepare(instance);
                                        }
                                        service.commit_credential(&probe_conn, &cred, host, commit_scope.as_ref(), sequence)
                                    }).await.unwrap_or_else(|_| Err(DshError::new(codes::IPC_STATE, "credential commit task failed")))
                                }
                                Err(error) => Err(error),
                            };
                            match committed {
                                Ok(()) => {
                                    if let Some((instance, generation)) = scope.as_ref() {
                                        if super::production::with_open_instance_generation(
                                            instance,
                                            *generation,
                                            || (),
                                        )
                                        .is_ok()
                                        {
                                            #[cfg(feature = "dsh-test-probes")]
                                            if let Some(hook) = CREDENTIAL_QUEUE_ADVANCE.get() {
                                                hook(instance);
                                            }
                                            let instance = instance.clone();
                                            let generation = *generation;
                                            tokio::spawn(async move {
                                                super::production_hub()
                                                    .on_instance_credentials_delivered(
                                                        &instance, generation,
                                                    )
                                                    .await;
                                            });
                                        }
                                    } else {
                                        tokio::spawn(async move {
                                            super::production_hub()
                                                .on_credentials_delivered()
                                                .await;
                                        });
                                    }
                                    ack
                                }
                                Err(e) => server_frame_line(&ServerFrame::Error {
                                    error: e.code.to_string(),
                                    detail: e.detail,
                                }),
                            }
                        }
                    };
                    #[cfg(feature = "dsh-test-probes")]
                    if let (Some(hook), Some((instance, _))) =
                        (CREDENTIAL_HANDLED.get(), scope.as_ref())
                    {
                        hook(instance);
                    }
                    if conn.send(&reply).await.is_err() {
                        break;
                    }
                }
            }
        }
        // R02 返工：连接退出清理——按**连接身份**核对（Arc::ptr_eq），只清本连接
        // 名下的 trusted/epochs；同 binding 的**新**连接（insert 已覆盖为它）不受
        // 本连接迟到退出影响。
        {
            let mut inner = self.inner.lock().unwrap();
            inner.event_proofs.retain(|p| !Arc::ptr_eq(&p.conn, &conn));
            inner.pending.retain(|p| !Arc::ptr_eq(&p.conn, &conn));
            inner
                .latest_key_connections
                .retain(|_, latest| !Arc::ptr_eq(latest, &conn));
            let stale: Vec<String> = inner
                .trusted
                .iter()
                .filter(|(_, c)| Arc::ptr_eq(c, &conn))
                .map(|(k, _)| k.clone())
                .collect();
            for k in stale {
                inner.trusted.remove(&k);
                inner.trusted_hosts.remove(&k);
                inner.peers.remove(&k);
            }
        }
    }

    // --------------------------------------------------------
    // 握手决策（T06 GREEN）
    // --------------------------------------------------------

    /// 构造服务端签名应答（对同一挑战消息签名；state/binding/code 随决策）。
    ///
    /// `pending_expires_at` 只在待批分支给值：那是**记录的真实审批截止**（幂等重连时
    /// 与本轮挑战 expiry 不同），已授权路径恒为 None。
    fn server_hello(
        &self,
        msg: &BindingChallengeMessage,
        state: &str,
        binding_id: Option<String>,
        display_code: Option<String>,
        pending_expires_at: Option<i64>,
    ) -> ServerFrame {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Decision<'a> {
            purpose: &'static str,
            #[serde(flatten)]
            challenge: &'a BindingChallengeMessage,
            state: &'a str,
            binding_id: &'a Option<String>,
            display_code: &'a Option<String>,
            pending_expires_at: Option<i64>,
        }
        let signature = self.key.sign(&msg.to_signing_bytes());
        let decision = Decision {
            purpose: "tuvis.dsh.binding-decision.v2",
            challenge: msg,
            state,
            binding_id: &binding_id,
            display_code: &display_code,
            pending_expires_at,
        };
        let decision_signature = self
            .key
            .sign(&serde_json::to_vec(&decision).expect("decision serialization"));
        ServerFrame::ServerHello(ServerHelloFrame {
            binding_protocol: BINDING_PROTOCOL,
            public_key_spki: b64url_encode(&self.key.spki),
            nonce: msg.nonce.clone(),
            expiry: msg.expiry,
            host_id: msg.host_id.clone(),
            epoch: msg.epoch.clone(),
            home_binding: msg.home_binding.clone(),
            signature: b64url_encode(&signature),
            decision_signature: b64url_encode(&decision_signature),
            state: state.to_string(),
            binding_id,
            display_code,
            pending_expires_at,
        })
    }

    /// 一次性 nonce 登记：换代清零 → 查重（重放）→ 入册。
    fn register_nonce(
        &self,
        nonce: [u8; 32],
        epoch: u64,
        expiry: i64,
        now: i64,
    ) -> Result<(), DshError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.nonce_epoch != Some(epoch) {
            inner.used_nonces.clear();
            inner.nonce_set.clear();
            inner.nonce_epoch = Some(epoch);
        }
        inner.used_nonces.retain(|(_, expires)| *expires >= now);
        inner.nonce_set = inner.used_nonces.iter().map(|(nonce, _)| *nonce).collect();
        if inner.nonce_set.contains(&nonce) {
            return Err(DshError::new(
                codes::AUTH_REPLAY,
                "challenge nonce was already used",
            ));
        }
        if inner.used_nonces.len() >= NONCE_CAPACITY {
            return Err(DshError::new(
                codes::AUTH_PROTOCOL,
                "challenge nonce window is full",
            ));
        }
        inner.nonce_set.insert(nonce);
        inner.used_nonces.push_back((nonce, expiry));
        Ok(())
    }

    /// 处理 hello：OS 核验 → 代际/nonce/签名核验 → 意图自动完成（H05）→
    /// 首绑（TOFU）或放行。
    async fn handle_hello(
        &self,
        conn: &Arc<dyn IpcClientConnection>,
        hello: &HelloFrame,
        handshake_gate: &Arc<tokio::sync::Mutex<()>>,
        probe_permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Result<ServerFrame, DshError> {
        // 协议代际（旧代际消息一律拒绝；版本/签名字段扩展走显式迁移——
        // 旧服务端对未来客户端**明确拒绝而非忽略**，签名挑战不漏签）。
        if hello.binding_protocol != BINDING_PROTOCOL {
            return Err(DshError::new(
                codes::AUTH_PROTOCOL,
                format!(
                    "binding protocol {} != {BINDING_PROTOCOL}",
                    hello.binding_protocol
                ),
            ));
        }
        // 宿主在否 + 当前代际（identity 单源；实例作用域 = 本实例的当代活体候选）。
        let (host, verified) = self.current_live()?;
        // H05/R01：归属证明分两层。
        // * `verified = true`（Desktop：argv/profile/监听三者钉死）→ 直接进入
        //   绑定与凭据交付；
        // * `verified = false`（Web：argv 不含 home）→ 只允许进入**待批**，并且
        //   必须由下面的「观测启动环境 home == 声明 home」与内核对端身份共同
        //   支撑；凭据交付前还会再次要求归属成立。这样首次无信任状态也能握手，
        //   又不把插件自报当成归属证明。
        // 代际核验先行：旧代际授权无论自报什么身份都先判死（stale grant 快速
        // 失效面——换代后旧代材料不得再进入任何后续核验分支）。
        if hello.epoch != host.epoch.to_string() {
            return Err(DshError::new(
                codes::AUTH_EPOCH_STALE,
                format!("grant epoch {} != current {}", hello.epoch, host.epoch),
            ));
        }
        if hello.host_id != host.host_id {
            return Err(DshError::new(codes::AUTH_PROTOCOL, "unknown host id"));
        }
        // OS 层对端核验：peercred PID ↔ 自报 PID ↔ 进程表（PID 复用/路径冒充防线；
        // 当前代内自报错启动身份/可执行文件照样在此被拒）。
        let peer = conn.peer_os();
        // R01：homeBinding 是客户端对「我是哪个宿主代际」的第二处声明，必须与
        // hello 的 PID/启动时刻一致，否则同一帧内可同时声明两个身份。
        if peer.pid == 0
            || peer.pid != hello.pid
            || peer.pid != hello.home_binding.host_pid
            || hello.home_binding.host_started_at_ms != hello.started_at_ms
        {
            return Err(DshError::new(
                codes::PEER_MISMATCH,
                format!(
                    "claimed identities disagree (os peer {}, hello {}, home binding {})",
                    peer.pid, hello.pid, hello.home_binding.host_pid
                ),
            ));
        }
        let proc = self
            .env
            .processes
            .list_processes()
            .into_iter()
            .find(|p| p.pid == peer.pid)
            .ok_or_else(|| {
                DshError::new(codes::PEER_MISMATCH, "peer pid is not in process table")
            })?;
        if proc.started_at_ms != hello.started_at_ms || proc.exe != hello.executable {
            return Err(DshError::new(
                codes::PEER_MISMATCH,
                "claimed executable/start identity does not match the process table",
            ));
        }
        // 连接方必须是**本域宿主**：内核对端 PID 必须等于本实例的当代活体 PID
        // （实例作用域；Web 宿主不是 Desktop 形态，形态核验由发现侧的 shape +
        // 监听属主承担——H05 起握手正是 Web 归属核验时刻）。旧单例语义保留
        // 「唯一合格 Desktop 宿主（双特征 + discover 同一 PID）」门。
        if proc.pid != host.pid
            || (self.instance_scope.is_none() && !identity::is_desktop_host_process(&proc))
        {
            return Err(DshError::new(
                codes::PEER_NOT_HOST,
                format!("peer pid {} is not this dsh domain's live host", peer.pid),
            ));
        }
        let claimed_home = super::instances::normalize_profile(std::path::Path::new(
            &hello.home_binding.home_root,
        ));
        if hello.home_binding.host_pid != host.pid
            || hello.home_binding.host_started_at_ms != host.started_at_ms
        {
            return Err(DshError::new(
                codes::INSTANCE_UNVERIFIED,
                "home binding does not match the live host generation",
            ));
        }
        if verified {
            if claimed_home
                != super::instances::normalize_profile(&super::instances::session_home(
                    &host.profile_dir,
                ))
            {
                return Err(DshError::new(
                    codes::INSTANCE_UNVERIFIED,
                    "home binding does not match the verified host",
                ));
            }
        } else {
            // 未核验（Web）：声明 home 必须与**实际观测到的启动环境**一致，
            // 且按该 home 派生的实例 ID 必须等于本域实例。读不到环境 → 拒绝，
            // 不把未知当作通过。
            let observed = self
                .env
                .environments
                .observed_environment(host.pid)
                .ok_or_else(|| {
                    DshError::new(
                        codes::INSTANCE_UNVERIFIED,
                        "web home attribution requires an observable startup environment",
                    )
                })?;
            let processes = self.env.processes.list_processes_fresh();
            let process = processes
                .iter()
                .find(|process| {
                    process.pid == host.pid
                        && process.started_at_ms == host.started_at_ms
                        && process.exe == host.executable
                })
                .ok_or_else(|| {
                    DshError::new(
                        codes::INSTANCE_UNVERIFIED,
                        "web process changed during observation",
                    )
                })?;
            let listeners = self.env.processes.listening_sockets_fresh();
            let process_matches = observed.matches_process(process);
            let after = self.env.environments.observed_environment(host.pid);
            let owned: Vec<_> = listeners
                .iter()
                .filter(|listener| {
                    listener.owner_pid == host.pid && listener.authority.starts_with("127.0.0.1:")
                })
                .collect();
            if !process_matches
                || !identity::is_web_host_cmdline(&process.cmdline)
                || after.as_ref() != Some(&observed)
                || owned.len() != 1
                || owned[0].authority != host.authority
            {
                return Err(DshError::new(
                    codes::INSTANCE_UNVERIFIED,
                    "web identity or listener changed during observation",
                ));
            }
            let observed_home = observed.derive_home().ok_or_else(|| {
                DshError::new(
                    codes::INSTANCE_UNVERIFIED,
                    "web host startup home is not proven",
                )
            })?;
            if super::instances::normalize_profile(&observed_home) != claimed_home {
                return Err(DshError::new(
                    codes::INSTANCE_UNVERIFIED,
                    "declared home does not match the host's observed startup home",
                ));
            }
            if super::instances::instance_id_for(
                super::instances::HostKind::Web,
                &claimed_home.join("profiles").join("web"),
            ) != host.host_id
            {
                return Err(DshError::new(
                    codes::INSTANCE_UNVERIFIED,
                    "declared home does not derive this instance id",
                ));
            }
        }
        // nonce 形状 + 60 秒 TTL 窗口。
        let nonce_bytes = b64url_decode(&hello.nonce)?;
        let nonce: [u8; 32] = <[u8; 32]>::try_from(nonce_bytes.as_slice())
            .map_err(|_| DshError::new(codes::AUTH_PROTOCOL, "nonce must be 32 bytes"))?;
        let now = self.env.clock.now_ms();
        if hello.expiry < now || hello.expiry > now + CHALLENGE_TTL_MS + EXPIRY_SLACK_MS {
            return Err(DshError::new(
                codes::AUTH_EXPIRED,
                format!(
                    "challenge expiry {} outside the 60s window (now {now})",
                    hello.expiry
                ),
            ));
        }
        // 客户端公钥 + 挑战签名（固定字段消息；不签任意对象序列化）。
        let client_spki = b64url_decode(&hello.public_key_spki)?;
        let msg = BindingChallengeMessage {
            binding_protocol: hello.binding_protocol,
            key_digests: [self.key.digest.clone(), key_digest(&client_spki)],
            nonce: hello.nonce.clone(),
            host_id: hello.host_id.clone(),
            epoch: hello.epoch.clone(),
            expiry: hello.expiry,
            home_binding: hello.home_binding.clone(),
        };
        let signature = b64url_decode(&hello.signature)?;
        self.key
            .verify_client(&client_spki, &msg.to_signing_bytes(), &signature)?;
        // 只登记已验签挑战；有效期内容量不足时拒绝，不淘汰仍有效的 nonce。
        self.register_nonce(nonce, host.epoch, hello.expiry, now)?;
        self.inner
            .lock()
            .unwrap()
            .latest_key_connections
            .insert(hello.public_key_spki.clone(), Arc::clone(conn));
        // 未认证的宿主采样和签名校验完成后释放预算，允许已验证握手并发收尾。
        drop(probe_permit);
        #[cfg(feature = "dsh-test-probes")]
        if let Some(hook) = HELLO_REGISTERED_HOOK.get() {
            hook().await;
        }
        // 本次握手观测到的客户端身份（OS 层已核验过的三事实；待批记录按最新观测更新）。
        let client_identity = ClientIdentity {
            public_key_spki: client_spki,
            executable: PathBuf::from(&hello.executable),
            pid: hello.pid,
            started_at_ms: hello.started_at_ms,
        };

        // 钉公钥 → 直接放行（每代以签名挑战验证，无需再次人工批准）。
        let pinned = {
            let ledger = self.ledger.lock().unwrap();
            ledger
                .bindings
                .iter()
                .rev()
                .find(|b| {
                    b.state == StoredBindingState::Bound
                        && b.public_key_spki == hello.public_key_spki
                })
                .map(|b| b.binding_id.clone())
        };
        if let Some(binding_id) = pinned {
            let _mutation = self.binding_mutation.lock().unwrap();
            // 第九轮 P1-1：拒绝前先尝试进程内恢复（本互斥已持，直接进内核）。
            self.ensure_domain_recovered_locked()?;
            let still_bound = self.ledger.lock().unwrap().bindings.iter().any(|binding| {
                binding.binding_id == binding_id
                    && binding.state == StoredBindingState::Bound
                    && binding.public_key_spki == hello.public_key_spki
            });
            if !still_bound {
                return Err(DshError::new(
                    codes::PEER_REVOKED,
                    "binding changed during reconnect",
                ));
            }
            // P1-1 开关执行门（授权面）：开关关闭（含撤权写盘失败的 Failed(Revoke)，
            // 绑定行原样保留）的实例，钉钥重连同样拿不到授权——绑定行在册也不能
            // 恢复受信连接。无布防代次（重连非意图授权）→ 只按 enabled 判定。
            self.check_switch_gate(None)?;
            if let (Some(instance), Some(anchor)) = (&self.instance_scope, &self.registry_anchor) {
                let fingerprint = key_digest(&client_identity.public_key_spki);
                if anchor.current_fingerprint(instance)? != fingerprint {
                    return Err(DshError::new(
                        codes::AUTH_SIGNATURE,
                        "persisted binding and fingerprint do not match",
                    ));
                }
            }
            if self.current_host()? != host {
                return Err(DshError::new(
                    codes::AUTH_EPOCH_STALE,
                    "host changed since handshake",
                ));
            }
            let mut inner = self.inner.lock().unwrap();
            if conn.is_closed()
                || !inner
                    .latest_key_connections
                    .get(&hello.public_key_spki)
                    .is_some_and(|latest| Arc::ptr_eq(latest, conn))
            {
                return Err(DshError::new(
                    codes::AUTH_NOT_APPROVED,
                    "handshake connection was replaced",
                ));
            }
            inner.peers.remove(&binding_id);
            inner.trusted_hosts.insert(binding_id.clone(), host.clone());
            inner.trusted.insert(binding_id.clone(), Arc::clone(conn));
            // R01 晋升：已钉钥重连同样是「本代宿主归属已证明」的路径。漏这里会让
            // Web 重启后的凭据交付被 INSTANCE_UNVERIFIED 拦掉（本代证明从未登记）。
            let host = host.clone();
            self.record_proven_web_host(&mut inner, &host);
            drop(inner);
            return Ok(self.server_hello(&msg, "authorized", Some(binding_id), None, None));
        }
        // 撤权墓碑的同钥 → 拒绝自动重连（重新绑定必须换新身份走 TOFU）。
        //
        // 永久撤权必须以本次挑战的签名决定表达，不能信任未认证的错误帧。
        let revoked_hit = {
            let ledger = self.ledger.lock().unwrap();
            ledger
                .bindings
                .iter()
                .find(|b| {
                    b.state == StoredBindingState::Revoked
                        && b.revoked_key_spki == hello.public_key_spki
                })
                .map(|b| b.binding_id.clone())
        };
        if let Some(binding_id) = revoked_hit {
            return Ok(self.server_hello(&msg, "revoked", Some(binding_id), None, None));
        }
        // H05：一次性授权意图（用户点击「启用」布防，TTL = 一个挑战窗口）。
        // 布防且未过期 → **这一个**通过全部核验（OS 身份 + 固定域 + 签名挑战）的
        // 新钥对端被自动完成绑定，意图即刻消费——绝不扩大为批准未来所有 pending；
        // 已过期/未布防 → 落到下方待批分支（人工批准语义不变）。
        let intent_candidate = self.intent.lock().unwrap().clone();
        // P1-1：布防意图携带的操作代次（提交点核对面——意图消费不可逆，代次在
        // 消费前从候选快照取）。
        let armed_switch_generation = intent_candidate
            .as_ref()
            .and_then(|candidate| candidate.switch_generation);
        let intent_mode = if let Some(candidate) = intent_candidate {
            let consume = || {
                let mut intent = self.intent.lock().unwrap();
                intent.as_ref().filter(|value| {
                    value.token == candidate.token && value.expires_at_ms >= self.env.clock.now_ms()
                })?;
                let inner = self.inner.lock().unwrap();
                if conn.is_closed()
                    || !inner
                        .latest_key_connections
                        .get(&hello.public_key_spki)
                        .is_some_and(|latest| Arc::ptr_eq(latest, conn))
                {
                    return None;
                }
                *intent = None;
                Some(())
            };
            match &candidate.selection {
                Some((registry, id, revision, epoch)) if *epoch == host.epoch => registry
                    .with_selection_revision(id, *revision, consume)
                    .ok()
                    .flatten()
                    .map(|()| {
                        (
                            FingerprintCommitMode::Intent {
                                selection_revision: *revision,
                            },
                            Some((Arc::clone(registry), id.clone(), *revision)),
                        )
                    }),
                None => consume().map(|()| (FingerprintCommitMode::Normal, None)),
                _ => None,
            }
        } else {
            None
        };
        if let Some((intent_mode, selection)) = intent_mode {
            #[cfg(feature = "dsh-test-probes")]
            if let Some(hook) = INTENT_CONSUMED_HOOK.get() {
                hook().await;
            }
            // 回归接缝（test-only）：意图已消费、绑定未写的窗口内确定性插队。
            #[cfg(all(test, unix))]
            commit_test_hooks::after_intent_consumed(self.instance_scope.as_deref().unwrap_or(""));
            // P1-1 开关执行门（授权提交点）：绑定写盘前的最后一道核对——持久化
            // 开关记录必须「开启」且与布防代次一致。关闭（disable 的 enabled=false
            // 先行落盘）/记录缺席/代次不匹配 → 拒绝（错误帧、不写绑定）：迟到握手
            // 在提交点退场，不得把已关闭的实例标成启用。此刻不持任何锁（意图已
            // 消费、selection guard 未取）——门闭包只做无锁小文件读。
            self.check_switch_gate(armed_switch_generation)?;
            // 等待测试 hook 后再取锁，避免跨 await 持有文件锁。
            let _selection_guard = match &selection {
                Some((registry, id, revision)) => {
                    Some(registry.selection_commit_guard(id, *revision)?)
                }
                None => None,
            };
            // H05 审查 I1：原子消费**同钥**旧 pending——旧核对码（更早一轮 hello 留下的
            // 待批记录）不得在自动绑定后仍可被批准，否则同一把钥会产生第二条 Bound，
            // 且「自动绑定→撤权→批准旧 pending」能把已撤旧钥复活。旧记录的连接一并
            // 关闭（持旧码的一端必须被告知作废）；**当前连接除外**（N1：同一连接重发
            // hello 时 p.conn 就是 conn——关掉它等于掐断本应答线，authorized 与凭据
            // 交付都送不出去；与既有 pending 复用分支的 F5 判别同口径）。
            let stale: Vec<Arc<dyn IpcClientConnection>> = {
                let mut inner = self.inner.lock().unwrap();
                if conn.is_closed()
                    || !inner
                        .latest_key_connections
                        .get(&hello.public_key_spki)
                        .is_some_and(|latest| Arc::ptr_eq(latest, conn))
                {
                    return Err(DshError::new(
                        codes::AUTH_NOT_APPROVED,
                        "handshake connection was replaced",
                    ));
                }
                let mut stale = Vec::new();
                inner.pending.retain(|p| {
                    if p.client_spki_b64 == hello.public_key_spki {
                        if !Arc::ptr_eq(&p.conn, conn) {
                            stale.push(Arc::clone(&p.conn));
                        }
                        false
                    } else {
                        true
                    }
                });
                stale
            };
            for c in stale {
                c.close();
            }
            // 回归接缝（Round-8）：外层门已过、绑定互斥未取的停点（一次性）。
            #[cfg(all(test, unix))]
            commit_test_hooks::before_commit_mutex(self.instance_scope.as_deref().unwrap_or(""));
            let fresh_id = self.next_binding_id();
            let binding_id = self.pin_new_binding(
                &fresh_id,
                &client_identity,
                &hello.public_key_spki,
                Arc::clone(conn),
                &host,
                false,
                hello.expiry,
                &intent_mode,
                armed_switch_generation,
            )?;
            return Ok(self.server_hello(&msg, "authorized", Some(binding_id), None, None));
        }
        // 首绑：TOFU + 人工确认 + 两端核对码（批准前零凭据）。
        //
        // R04（稳定性）：**同一未批准客户端公钥**在它自己那份挑战未过期期间，必须
        // 拿回同一对 binding_id + display_code——插件每 3 秒重发 hello，若每次都换新
        // 码，两端核对码会在人工核对窗口里翻动，用户点批准时批的是已被覆盖的旧 id。
        // 只有到期才换新；复用记录时把连接与身份事实更新为本次握手观测到的那一份
        // （批准时向最新连接推 Granted）。
        let now = self.env.clock.now_ms();
        let binding = {
            let mut inner = self.inner.lock().unwrap();
            if conn.is_closed()
                || !inner
                    .latest_key_connections
                    .get(&hello.public_key_spki)
                    .is_some_and(|latest| Arc::ptr_eq(latest, conn))
            {
                return Err(DshError::new(
                    codes::AUTH_NOT_APPROVED,
                    "handshake connection was replaced",
                ));
            }
            // 顺手清掉已过期的记录（到期 = 该对作废；不做后台清扫任务）。
            // P1-①（复审 T08 前门槛）：被驱逐记录的连接必须一并关闭——否则插件
            // 挂在旧连接的 recv 上永不重发 hello，旧核对码一直展示。
            let mut evicted: Vec<Arc<dyn IpcClientConnection>> = Vec::new();
            inner.pending.retain(|p| {
                if p.binding.expires_at_ms >= now {
                    true
                } else {
                    evicted.push(Arc::clone(&p.conn));
                    false
                }
            });
            drop(inner);
            for c in evicted {
                c.close();
            }
            let mut inner = self.inner.lock().unwrap();
            if conn.is_closed()
                || !inner
                    .latest_key_connections
                    .get(&hello.public_key_spki)
                    .is_some_and(|latest| Arc::ptr_eq(latest, conn))
            {
                return Err(DshError::new(
                    codes::AUTH_NOT_APPROVED,
                    "handshake connection was replaced",
                ));
            }
            match inner
                .pending
                .iter_mut()
                .find(|p| p.client_spki_b64 == hello.public_key_spki)
            {
                Some(existing) => {
                    // 未过期（上一行的 retain 已排除过期项）→ 幂等复用同一对。
                    existing.client = client_identity;
                    existing.host = host.clone();
                    existing.handshake_gate = Arc::clone(handshake_gate);
                    // P1-①：连接与身份事实更新为本次握手观测的那一份；旧连接
                    // 立即关闭（批准推 Granted 只走最新连接，旧线留着只会让
                    // 插件侧多一个挂死 recv）。
                    //
                    // F5（复审 Important-5）：**同一条连接**重发 hello（读循环重试、
                    // 幂等重放）时 stale 就是 conn 自己——不加指针判等就会在这里把
                    // 正在处理 hello 的应答线关掉，客户端既收不到应答也发不出下一帧。
                    let stale = std::mem::replace(&mut existing.conn, Arc::clone(conn));
                    if !Arc::ptr_eq(&stale, conn) {
                        stale.close();
                    }
                    existing.binding.clone()
                }
                None => {
                    let binding = BindingChallenge {
                        binding_id: self.next_binding_id(),
                        display_code: display_code(),
                        expires_at_ms: hello.expiry,
                    };
                    inner.pending.push(PendingRecord {
                        binding: binding.clone(),
                        client: client_identity,
                        client_spki_b64: hello.public_key_spki.clone(),
                        host: host.clone(),
                        handshake_gate: Arc::clone(handshake_gate),
                        conn: Arc::clone(conn),
                    });
                    // 容量上限：退役最旧的一条（未批准的客户端可自造记录，必须有界）。
                    while inner.pending.len() > PENDING_CAPACITY {
                        let retired = inner.pending.remove(0);
                        retired.conn.close();
                    }
                    binding
                }
            }
        };
        Ok(self.server_hello(
            &msg,
            "pending_approval",
            Some(binding.binding_id),
            Some(binding.display_code),
            Some(binding.expires_at_ms),
        ))
    }

    /// 发现和归属校验在发布锁外完成。提交时还要重查绑定和连接身份。
    fn verify_credential(
        &self,
        conn: &Arc<dyn IpcClientConnection>,
        cred: &CredentialFrame,
    ) -> Result<LocalHost, DshError> {
        {
            let _mutation = self.binding_mutation.lock().unwrap();
            self.ensure_domain_recovered_locked()?;
            self.require_bound_credential(&cred.binding_id)?;
        }
        if !self
            .inner
            .lock()
            .unwrap()
            .trusted
            .get(&cred.binding_id)
            .is_some_and(|peer| Arc::ptr_eq(peer, conn) && !peer.is_closed())
        {
            return Err(DshError::new(
                codes::AUTH_NOT_APPROVED,
                "credential connection is not trusted",
            ));
        }
        let (host, verified) = self.current_live()?;
        if !verified {
            return Err(DshError::new(
                codes::INSTANCE_UNVERIFIED,
                "instance home attribution is unverified; credential delivery refused",
            ));
        }
        Ok(host)
    }

    fn require_bound_credential(&self, binding_id: &str) -> Result<(), DshError> {
        if self.ledger.lock().unwrap().bindings.iter().any(|binding| {
            binding.state == StoredBindingState::Bound && binding.binding_id == binding_id
        }) {
            return Ok(());
        }
        Err(DshError::new(
            codes::AUTH_NOT_APPROVED,
            "credential binding is not approved",
        ))
    }

    fn commit_credential(
        &self,
        conn: &Arc<dyn IpcClientConnection>,
        cred: &CredentialFrame,
        host: LocalHost,
        scope: Option<&(String, u64)>,
        sequence: Option<crate::remote::console_mount::CredentialSequence>,
    ) -> Result<(), DshError> {
        let _mutation = self.binding_mutation.lock().unwrap();
        self.require_bound_credential(&cred.binding_id)?;
        let commit = || {
            let mut inner = self.inner.lock().unwrap();
            let same_connection = inner
                .trusted
                .get(&cred.binding_id)
                .is_some_and(|peer| Arc::ptr_eq(peer, conn) && !peer.is_closed());
            if !same_connection || inner.trusted_hosts.get(&cred.binding_id) != Some(&host) {
                return Err(DshError::new(
                    codes::AUTH_EPOCH_STALE,
                    "credential does not match the verified handshake host",
                ));
            }
            let gate = inner
                .gates
                .get(&cred.binding_id)
                .cloned()
                .unwrap_or_else(PeerGate::new);
            if gate.is_revoked() {
                return Err(DshError::new(
                    codes::PEER_REVOKED,
                    "credential binding was revoked",
                ));
            }
            let browser = cred.browser_cookie.clone().unwrap_or_default();
            let publish_native = |inner: &mut Inner, gate: Arc<PeerGate>, browser: String| {
                inner
                    .gates
                    .insert(cred.binding_id.clone(), Arc::clone(&gate));
                inner.peers.insert(
                    cred.binding_id.clone(),
                    AuthorizedPeer::with_authority(
                        &host.host_id,
                        cred.cookie.as_bytes().to_vec(),
                        &host.authority,
                        host.epoch,
                    )
                    .with_gate(Arc::clone(&gate))
                    .with_binding(cred.binding_id.clone()),
                );
                inner.web_deliveries.insert(
                    cred.binding_id.clone(),
                    RetainedWebDelivery {
                        api: cred.cookie.clone(),
                        browser,
                        epoch: host.epoch,
                        authority: host.authority.clone(),
                        gate,
                    },
                );
            };
            if let Some((instance, _)) = scope {
                crate::remote::console_lifecycle::install_upstream_credentials_committing(
                    instance,
                    cred.cookie.clone(),
                    browser.clone(),
                    host.epoch,
                    &host.authority,
                    Arc::clone(&gate),
                    sequence.expect("scoped credential has a sequence"),
                    || publish_native(&mut inner, gate, browser),
                )
                .map_err(|_| DshError::new(codes::IPC_STATE, "console credential commit rejected"))
            } else {
                publish_native(&mut inner, gate, browser);
                Ok(())
            }
        };
        if let Some((instance, generation)) = scope {
            super::production::with_open_instance_generation(instance, *generation, commit)?
        } else {
            commit()
        }
    }

    /// 钉公钥入账本 + 注册受信 Peer + 建撤权闸门（H05 抽取的**唯一**落账内核；
    /// 人工批准与授权意图自动完成共用同一段——落账形状、profile 归属与闸门语义
    /// 完全一致，不存在两套批准路径）。`binding_id` 由调用方给定：人工批准沿用
    /// 待批挑战的 id；意图自动完成用新生成的。
    ///
    /// H05 审查 I1：落账与**状态复核同锁原子**——
    /// * 同钥**撤权墓碑**在册 → `DSH_PEER_REVOKED`（撤权不因任何批准路径复活）；
    /// * 同钥**既有 Bound 行** → 不产生第二条绑定：把本连接按既有绑定登记为受信
    ///   Peer（与钉公钥重连同语义），返回既有 binding_id（调用方据此发 Granted）。
    // 挑战身份、代际与提交方式必须在同一内核中显式核验。
    #[allow(clippy::too_many_arguments)]
    fn pin_new_binding(
        &self,
        binding_id: &str,
        client: &ClientIdentity,
        client_spki_b64: &str,
        conn: Arc<dyn IpcClientConnection>,
        host: &LocalHost,
        require_live_proof: bool,
        deadline_ms: i64,
        mode: &FingerprintCommitMode,
        armed_switch_generation: Option<u64>,
    ) -> Result<String, DshError> {
        let _mutation = self.binding_mutation.lock().unwrap();
        if self.env.clock.now_ms() > deadline_ms {
            return Err(DshError::new(
                codes::AUTH_EXPIRED,
                "binding challenge expired during approval",
            ));
        }
        // P1-1 开关执行门（提交段内复核）：绑定互斥已持、距写账本仅剩无 await 的
        // 短段——disable 在「握手侧门已过」与「本写」之间插队完成落账的窗口在此
        // 收口（disable 全程持 action_lock；本门无锁读，不构成锁序环）。人工批准
        // 路径 armed = None（只按 enabled 判定）。
        self.check_switch_gate(armed_switch_generation)?;
        // 第九轮 P1-1：拒绝前先尝试进程内恢复（本互斥已持，直接进内核）——
        // 补写收敛后的下一次授权提交自动收敛，无需重启即可重新绑定。
        self.ensure_domain_recovered_locked()?;
        // 重采只确认原握手身份仍在，绝不以重采结果替旧握手盖章。
        if self.current_host()? != *host {
            return Err(DshError::new(
                codes::AUTH_EPOCH_STALE,
                "host changed since handshake",
            ));
        }
        let can_publish = |inner: &Inner, id: &str| {
            !conn.is_closed()
                && inner
                    .latest_key_connections
                    .get(client_spki_b64)
                    .is_some_and(|latest| Arc::ptr_eq(latest, &conn))
                && (!require_live_proof
                    || inner
                        .event_proofs
                        .iter()
                        .any(|p| Arc::ptr_eq(&p.conn, &conn) && p.binding_id == id))
                && inner
                    .trusted
                    .get(id)
                    .is_none_or(|current| Arc::ptr_eq(current, &conn))
        };
        if !can_publish(&self.inner.lock().unwrap(), binding_id) {
            return Err(DshError::new(
                codes::AUTH_NOT_APPROVED,
                "connection has ended or been replaced",
            ));
        }
        let profile_dir = host.profile_dir.to_string_lossy().to_string();
        let (effective_id, previous_ledger, receipt) = {
            let ledger = self.ledger.lock().unwrap().clone();
            // I1 复核 ①：同钥墓碑 → 拒绝（不复活已撤旧钥）。
            if ledger.bindings.iter().any(|b| {
                b.state == StoredBindingState::Revoked && b.revoked_key_spki == client_spki_b64
            }) {
                return Err(DshError::new(
                    codes::PEER_REVOKED,
                    "this client key was revoked; re-binding requires a fresh identity",
                ));
            }
            if let FingerprintCommitMode::ExplicitRebind { expected_old } = mode {
                let mut old_ids = Vec::new();
                for binding in &ledger.bindings {
                    let key = if binding.state == StoredBindingState::Revoked {
                        &binding.revoked_key_spki
                    } else {
                        &binding.public_key_spki
                    };
                    if !key.is_empty() && key_digest(&b64url_decode(key)?) == *expected_old {
                        if binding.state == StoredBindingState::Bound {
                            return Err(DshError::new(
                                codes::AUTH_SIGNATURE,
                                "old binding is still active",
                            ));
                        }
                        old_ids.push(binding.binding_id.clone());
                    }
                }
                if old_ids.is_empty() {
                    return Err(DshError::new(
                        codes::AUTH_SIGNATURE,
                        "old binding has no revocation tombstone",
                    ));
                }
                let inner = self.inner.lock().unwrap();
                if old_ids.iter().any(|id| {
                    inner.trusted.contains_key(id)
                        || inner.peers.contains_key(id)
                        || inner.gates.get(id).is_some_and(|gate| !gate.is_revoked())
                }) {
                    return Err(DshError::new(
                        codes::AUTH_SIGNATURE,
                        "old binding remains active",
                    ));
                }
            }
            // I1 复核 ②：同钥既有 Bound → 复用（不落第二条；与钉公钥重连同语义）。
            if let Some(existing) = ledger
                .bindings
                .iter()
                .rev()
                .find(|b| {
                    b.state == StoredBindingState::Bound && b.public_key_spki == client_spki_b64
                })
                .map(|b| b.binding_id.clone())
            {
                // 待批决定签名固定了 binding_id；并发已有另一条同钥绑定时，
                // 不能把当前连接悄然改挂到不同绑定号，要求重新握手。
                if existing != binding_id {
                    return Err(DshError::new(
                        codes::AUTH_NOT_APPROVED,
                        "binding changed; reconnect to use the existing binding",
                    ));
                }
                drop(ledger);
                if let (Some(instance), Some(anchor)) =
                    (&self.instance_scope, &self.registry_anchor)
                {
                    if anchor.current_fingerprint(instance)?
                        != key_digest(&b64url_decode(client_spki_b64)?)
                    {
                        return Err(DshError::new(
                            codes::AUTH_SIGNATURE,
                            "persisted binding and fingerprint do not match",
                        ));
                    }
                }
                if self.env.clock.now_ms() > deadline_ms {
                    return Err(DshError::new(
                        codes::AUTH_EXPIRED,
                        "binding challenge expired before anchor commit",
                    ));
                }
                let receipt = self.commit_binding_anchor(client_spki_b64, mode)?;
                if self.env.clock.now_ms() > deadline_ms {
                    if let (Some(anchor), Some(receipt)) = (&self.registry_anchor, receipt.as_ref())
                    {
                        if let Err(error) = anchor.rollback_fingerprint(receipt) {
                            self.fail_closed_binding_domain();
                            return Err(DshError::new(
                                codes::IPC_STATE,
                                format!("expired binding rollback failed: {}", error.detail),
                            ));
                        }
                    }
                    return Err(DshError::new(
                        codes::AUTH_EXPIRED,
                        "binding challenge expired during anchor commit",
                    ));
                }
                let mut inner = self.inner.lock().unwrap();
                if !can_publish(&inner, &existing) {
                    return Err(DshError::new(
                        codes::AUTH_NOT_APPROVED,
                        "connection has ended or been replaced",
                    ));
                }
                // R02 返工：同钥复用分支的 trusted/epochs 键 = 账本既有 id；
                // 代际登记统一在**内核单点**——人工批准/自动授权/已钉钥重连
                // 三路共用（此前自动授权漏登记 → 一键授权后指纹恒 None）。
                inner.trusted.insert(existing.clone(), Arc::clone(&conn));
                inner.trusted_hosts.insert(existing.clone(), host.clone());
                inner
                    .gates
                    .entry(existing.clone())
                    .or_insert_with(PeerGate::new);
                // R01 晋升：同钥复用既有绑定也是已证明归属的一路（第三处）。
                self.record_proven_web_host(&mut inner, host);
                return Ok(existing);
            }
            let previous_ledger = ledger.clone();
            let mut candidate = previous_ledger.clone();
            candidate.bindings.push(StoredBinding {
                binding_id: binding_id.to_owned(),
                public_key_spki: client_spki_b64.to_owned(),
                revoked_key_spki: String::new(),
                executable: client.executable.to_string_lossy().to_string(),
                pid: client.pid,
                started_at_ms: client.started_at_ms,
                profile_dir,
                created_at_ms: self.env.clock.now_ms(),
                state: StoredBindingState::Bound,
            });
            let bytes = serde_json::to_vec_pretty(&candidate)
                .map_err(|e| DshError::new(codes::IPC_STATE, format!("serialize bindings: {e}")))?;
            drop(ledger);
            let path = ledger_path(&self.env);
            let before = read_optional_file(&path).map_err(|error| {
                DshError::new(
                    codes::IPC_STATE,
                    format!("cannot read bindings before commit: {error}"),
                )
            })?;
            if self.env.clock.now_ms() > deadline_ms {
                return Err(DshError::new(
                    codes::AUTH_EXPIRED,
                    "binding challenge expired before anchor commit",
                ));
            }
            let receipt = self.commit_binding_anchor(client_spki_b64, mode)?;
            #[cfg(feature = "dsh-test-probes")]
            intent_commit_stage(mode, "after_anchor_cas");
            if self.env.clock.now_ms() > deadline_ms {
                if let (Some(anchor), Some(receipt)) = (&self.registry_anchor, receipt.as_ref()) {
                    if let Err(error) = anchor.rollback_fingerprint(receipt) {
                        self.fail_closed_binding_domain();
                        return Err(DshError::new(
                            codes::IPC_STATE,
                            format!("expired binding rollback failed: {}", error.detail),
                        ));
                    }
                }
                return Err(DshError::new(
                    codes::AUTH_EXPIRED,
                    "binding challenge expired during anchor commit",
                ));
            }
            #[cfg(feature = "dsh-test-probes")]
            intent_commit_stage(mode, "before_ledger_write");
            // 回归接缝（Round-8，test-only unix）：提交互斥内、账本写盘前的停点
            // （一次性、按实例键）——disable 的失效段必须等在本互斥上（Ruling 10）。
            #[cfg(all(test, unix))]
            commit_test_hooks::commit_write_pause(self.instance_scope.as_deref().unwrap_or(""));
            if let Err(error) = atomic_write_0600(&path, &bytes) {
                if !matches!(read_optional_file(&path), Ok(ref current) if *current == before) {
                    self.fail_closed_binding_domain();
                    return Err(DshError::new(
                        codes::IPC_STATE,
                        format!("binding write outcome uncertain: {}", error.detail),
                    ));
                }
                if let (Some(anchor), Some(receipt)) = (&self.registry_anchor, receipt.as_ref()) {
                    if let Err(rollback) = anchor.rollback_fingerprint(receipt) {
                        self.fail_closed_binding_domain();
                        return Err(DshError::new(
                            codes::IPC_STATE,
                            format!(
                                "binding write failed: {}; fingerprint rollback failed: {}",
                                error.detail, rollback.detail
                            ),
                        ));
                    }
                }
                return Err(error);
            }
            *self.ledger.lock().unwrap() = candidate;
            (binding_id.to_owned(), previous_ledger, receipt)
        };
        // 注册受信 Peer + 建该 binding 的撤权闸门。
        #[cfg(feature = "dsh-test-probes")]
        intent_commit_stage(mode, "before_trusted_publish");
        {
            let mut inner = self.inner.lock().unwrap();
            let expired = self.env.clock.now_ms() > deadline_ms;
            if expired || !can_publish(&inner, &effective_id) {
                drop(inner);
                let previous_bytes =
                    serde_json::to_vec_pretty(&previous_ledger).map_err(|error| {
                        DshError::new(
                            codes::IPC_STATE,
                            format!("serialize previous bindings: {error}"),
                        )
                    })?;
                if let Err(error) = atomic_write_0600(&ledger_path(&self.env), &previous_bytes) {
                    self.fail_closed_binding_domain();
                    return Err(DshError::new(
                        codes::IPC_STATE,
                        format!("binding rollback failed: {}", error.detail),
                    ));
                }
                *self.ledger.lock().unwrap() = previous_ledger;
                if let (Some(anchor), Some(receipt)) = (&self.registry_anchor, receipt.as_ref()) {
                    if let Err(error) = anchor.rollback_fingerprint(receipt) {
                        self.fail_closed_binding_domain();
                        return Err(DshError::new(
                            codes::IPC_STATE,
                            format!("fingerprint rollback failed: {}", error.detail),
                        ));
                    }
                }
                return Err(if expired {
                    DshError::new(
                        codes::AUTH_EXPIRED,
                        "binding challenge expired during commit",
                    )
                } else {
                    DshError::new(
                        codes::AUTH_NOT_APPROVED,
                        "connection has ended or been replaced",
                    )
                });
            }
            inner
                .trusted
                .insert(effective_id.clone(), Arc::clone(&conn));
            // R02 返工：新绑定同点登记代际（三路统一：内核单点）。
            inner
                .trusted_hosts
                .insert(effective_id.clone(), host.clone());
            // R01 晋升：人工批准 / 自动授权 / 已钉钥重连三路共用此点——把
            // 「该实例本代宿主归属」记入已证明集合，凭据交付据此放行。
            self.record_proven_web_host(&mut inner, host);
            inner
                .gates
                .entry(effective_id.clone())
                .or_insert_with(PeerGate::new);
        }
        Ok(effective_id)
    }

    fn fail_closed_binding_domain(&self) {
        self.fail_closed_binding_domain_with(None);
    }

    /// 带证据的 fail-closed（第九轮 P1-1）：撤权路径传入被撤绑定证据供进程内
    /// 恢复核验对位；提交路径（绑定写入结果不可确认等）传 None。证据槽只在
    /// 缺席时登记——连续/并发故障保留最早的对位材料（恢复通过时统一摘除）。
    fn fail_closed_binding_domain_with(&self, evidence: Option<BindingFaultEvidence>) {
        self.binding_faulted
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(evidence) = evidence {
            let mut slot = self.fault_evidence.lock().unwrap();
            if slot.is_none() {
                *slot = Some(evidence);
            }
        }
        let (gates, connections) = {
            let mut inner = self.inner.lock().unwrap();
            inner.peers.clear();
            // P1-4：域已不可证明 → 保留副本一并作废（重交付失去信任前提）。
            inner.web_deliveries.clear();
            let connections = inner
                .trusted
                .drain()
                .map(|(_, conn)| conn)
                .collect::<Vec<_>>();
            inner.trusted_hosts.clear();
            (
                inner.gates.values().cloned().collect::<Vec<_>>(),
                connections,
            )
        };
        for gate in gates {
            gate.revoke();
        }
        for connection in connections {
            connection.close();
        }
        // 账本状态不可证明 → 控制台吊销。第九轮 P1-1：与对等面（peers/trusted/
        // web_deliveries/gates 本就按实例分槽）收敛到**同一实例域**——关 A 时写盘
        // 结果不可确认，不得把实例 B 的活控制台陪葬。legacy 单例域
        // （instance_scope = None）只有一个宿主，保持全局吊销语义（与
        // `revoke_local_locked` 的分流同款）。
        if let Some(instance_id) = self.instance_scope.clone() {
            crate::remote::console_lifecycle::revoke_console_access_for_instance(&instance_id);
        } else {
            crate::remote::console_lifecycle::revoke_console_access();
        }
    }

    /// 进程内恢复内核（第九轮 P1-1，懒验证式——**不得只清 flag**；调用方必须已持
    /// `binding_mutation`）：仅当 `binding_faulted` 置位时核验；盘上证据全部成立
    /// → 清屏障（保持内存面已清空态）；任一不成立 → 保持 faulted（原因进日志，
    /// 拒绝照旧）。核验全部读**盘上**账本（重新 parse bindings.json，不信内存）：
    /// ① `revoke_write_pending` 空——待落盘撤销已收敛（重试关闭补写成功）；
    /// ② 盘上无任何 Bound 行——关闭后的事实状态（不可能仍有在册受信钥）；
    /// ③ 引发 fail-closed 的绑定行在盘上已是 Revoked 且 `revoked_key_spki` 摘要
    ///    对上 fail-closed 时登记的被撤钥摘要——收敛的正是那一次撤权；
    /// ④ 实例登记锚在场（instance_scope + registry_anchor）时：锚指纹 == 墓碑
    ///    摘要（收敛的正是当代锚定钥），或锚指纹为空（关闭流程退役段已清锚——
    ///    再开启走首绑 TOFU；①②③ 已证域收敛，空锚说明退役完成，不存在可复活
    ///    的当代锚定钥）；锚指纹为其他非空值 = 身份漂移 → 保持 faulted。锚缺席
    ///    （legacy 域 / 测试形态不注入锚）→ 账本证据 ①②③ 单独成立即可（无锚
    ///    可对，此处如实申报）。
    /// 内存面**保持已清空态**：fail-closed 摘除的 peers/trusted/web_deliveries 是
    /// 关闭后的事实状态，恢复只清屏障、不重建任何受信面；重新开放只经全量门的
    /// 新授权（布防意图 / 人工批准 / 钉钥重连）。
    fn ensure_domain_recovered_locked(&self) -> Result<(), DshError> {
        if !self
            .binding_faulted
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(());
        }
        match self.domain_recovery_blocker() {
            None => {
                self.binding_faulted
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                self.fault_evidence.lock().unwrap().take();
                log::info!(
                    "dsh binding domain recovered in-process (ledger evidence verified on disk)"
                );
                Ok(())
            }
            Some(reason) => {
                log::warn!("dsh binding domain recovery deferred: {reason}");
                Err(DshError::new(
                    codes::IPC_STATE,
                    format!("binding state requires recovery: {reason}"),
                ))
            }
        }
    }

    /// 恢复核验（返回 Some = 保持 faulted 的原因；None = 证据齐备可恢复）。
    fn domain_recovery_blocker(&self) -> Option<String> {
        // ① 待落盘撤销必须已收敛。
        if !self.revoke_write_pending.lock().unwrap().is_empty() {
            return Some("revoke write pending is not empty".to_owned());
        }
        // ③④ 的对位材料：无证据 = 不可证明引发故障的绑定已收敛（提交路径的
        // fail-closed 没有被撤墓碑可对）→ 保守保持 faulted。
        let evidence = self.fault_evidence.lock().unwrap().clone();
        let Some(evidence) = evidence else {
            return Some("no fault evidence recorded".to_owned());
        };
        // ② 盘上账本重 parse：无任何 Bound 行。
        let disk = match load_ledger(&self.env) {
            Ok(ledger) => ledger,
            Err(error) => return Some(format!("ledger reload failed: {}", error.detail)),
        };
        if disk
            .bindings
            .iter()
            .any(|binding| binding.state == StoredBindingState::Bound)
        {
            return Some("bound row remains on disk".to_owned());
        }
        // ③ 墓碑对位：该行 Revoked 且被撤钥摘要对上登记证据。
        let Some(row) = disk
            .bindings
            .iter()
            .rev()
            .find(|binding| binding.binding_id == evidence.binding_id)
        else {
            return Some("faulted binding row missing on disk".to_owned());
        };
        if row.state != StoredBindingState::Revoked {
            return Some(format!(
                "faulted binding {} is not revoked on disk",
                evidence.binding_id
            ));
        }
        let disk_digest = key_digest(&b64url_decode(&row.revoked_key_spki).unwrap_or_default());
        if disk_digest != evidence.revoked_key_digest {
            return Some("tombstone key does not match the revoked digest".to_owned());
        }
        // ④ 锚对位（在场时）；锚缺席（legacy 域/测试形态）→ ①②③ 单独成立。
        if let (Some(instance), Some(anchor)) = (&self.instance_scope, &self.registry_anchor) {
            match anchor.current_fingerprint(instance) {
                Ok(fingerprint) if fingerprint == evidence.revoked_key_digest => {}
                // 空锚 = 关闭流程退役段已清锚（见恢复内核 doc ④）。
                Ok(fingerprint) if fingerprint.is_empty() => {}
                Ok(fingerprint) => {
                    return Some(format!(
                        "registry fingerprint drifted: {fingerprint} != revoked digest"
                    ));
                }
                Err(error) => {
                    return Some(format!(
                        "registry fingerprint unavailable: {}",
                        error.detail
                    ));
                }
            }
        }
        None
    }

    fn commit_binding_anchor(
        &self,
        client_spki_b64: &str,
        mode: &FingerprintCommitMode,
    ) -> Result<Option<FingerprintCommitReceipt>, DshError> {
        match (&self.instance_scope, &self.registry_anchor) {
            (Some(instance), Some(anchor)) => {
                let fingerprint = key_digest(&b64url_decode(client_spki_b64)?);
                anchor
                    .commit_fingerprint(instance, &fingerprint, mode)
                    .map(Some)
            }
            _ => Ok(None),
        }
    }

    // --------------------------------------------------------
    // 绑定生命周期（用户面）
    // --------------------------------------------------------

    /// 布防一次性授权意图（H05：设置卡「启用 DSH 远程」点击的落点）。
    ///
    /// 语义：布防后**下一个**通过全部核验的新钥握手被自动完成该次绑定并消费意图
    /// （选定实例范围——意图挂在本域服务上，天然不跨实例）；TTL = 一个挑战窗口
    /// （60 秒），过期即失效，**不接受未来所有 pending**。人工批准路径不变。
    pub fn begin_authorization_intent(&self) -> Result<(), DshError> {
        let now = self.env.clock.now_ms();
        *self.intent.lock().unwrap() = Some(AuthorizationIntent {
            expires_at_ms: now + CHALLENGE_TTL_MS,
            selection: None,
            switch_generation: None,
            token: self.seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        });
        Ok(())
    }

    pub fn begin_selected_authorization_intent(
        &self,
        registry: Arc<super::instances::InstanceRegistry>,
        instance_id: &str,
        revision: u64,
        host_epoch: u64,
    ) -> Result<(), DshError> {
        self.begin_selected_authorization_intent_generation(
            registry,
            instance_id,
            revision,
            host_epoch,
            None,
        )
    }

    /// P1-1：开关驱动器布防（携带布防时开关记录的操作代次）。授权提交点据此核对
    /// 「意图所属代次」——关闭/再开启换代后的迟到握手在提交点退场，不得把已关闭
    /// 或已换代意图的授权落成绑定。
    pub fn begin_selected_authorization_intent_generation(
        &self,
        registry: Arc<super::instances::InstanceRegistry>,
        instance_id: &str,
        revision: u64,
        host_epoch: u64,
        switch_generation: Option<u64>,
    ) -> Result<(), DshError> {
        if self.instance_scope.as_deref() != Some(instance_id) || self.registry_anchor.is_none() {
            return Err(DshError::new(
                codes::INSTANCE_UNVERIFIED,
                "authorization domain is not anchored to the selected instance",
            ));
        }
        let selection_registry = Arc::clone(&registry);
        registry.with_selection_revision(instance_id, revision, || {
            *self.intent.lock().unwrap() = Some(AuthorizationIntent {
                expires_at_ms: self.env.clock.now_ms() + CHALLENGE_TTL_MS,
                selection: Some((
                    selection_registry,
                    instance_id.to_owned(),
                    revision,
                    host_epoch,
                )),
                switch_generation,
                token: self.seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
            });
        })
    }

    /// 取消当前待授权意图（Task 4 关闭语义：布防中的短时意图必须随关闭立即
    /// 失效——迟到的握手不得再被自动批准）。幂等：无意图时无操作。
    pub fn cancel_authorization_intent(&self) {
        *self.intent.lock().unwrap() = None;
    }

    /// 当前是否有未过期的待授权意图（Task 4 回归观测面；生产不消费）。
    #[cfg(test)]
    pub fn authorization_intent_armed_for_test(&self) -> bool {
        let intent = self.intent.lock().unwrap();
        intent
            .as_ref()
            .is_some_and(|candidate| candidate.expires_at_ms >= self.env.clock.now_ms())
    }

    /// 回归注入：真实账本 Bound 行（Task 4 开关状态机测试用；生产不消费）。
    /// 与 cp_seed_bound 同口径——塞的是真实账本行，非探针。
    #[cfg(test)]
    pub fn seed_bound_for_test(&self, binding_id: &str, spki_b64: &str, profile_dir: &str) {
        self.ledger.lock().unwrap().bindings.push(StoredBinding {
            binding_id: binding_id.to_owned(),
            public_key_spki: spki_b64.to_owned(),
            revoked_key_spki: String::new(),
            executable: "/fixture/dsh".to_owned(),
            pid: 1,
            started_at_ms: 1,
            profile_dir: profile_dir.to_owned(),
            created_at_ms: 2,
            state: StoredBindingState::Bound,
        });
    }

    /// 当前待批绑定视图（无 → None；设置卡 `dsh_binding_start` 的数据源）。
    ///
    /// 返回**最新**一条**未过期**的待批记录（同钥在 TTL 内恒为同一对 id/码；多条只
    /// 在多把未批准公钥并存时出现）。
    ///
    /// F4（复审 Important-4）：过期记录必须在这里就被清掉——视图是设置卡的唯一数据
    /// 源，不看过期就会把一个 approve → AUTH_EXPIRED 的死码展示给用户。清扫口径与
    /// [`Self::handle_hello`] 的驱逐完全一致（`expires_at_ms >= now` 保留，其余摘除
    /// 并关闭其连接——持有旧码的一端必须被告知作废）。
    pub fn pending_binding(&self) -> Option<PendingBindingView> {
        let now = self.env.clock.now_ms();
        let evicted: Vec<Arc<dyn IpcClientConnection>> = {
            let mut inner = self.inner.lock().unwrap();
            let mut evicted = Vec::new();
            inner.pending.retain(|p| {
                if p.binding.expires_at_ms >= now {
                    true
                } else {
                    evicted.push(Arc::clone(&p.conn));
                    false
                }
            });
            evicted
        };
        // 关连接要出锁（连接的 close 可能回调/阻塞）。
        for c in evicted {
            c.close();
        }
        let inner = self.inner.lock().unwrap();
        inner.pending.last().map(|p| PendingBindingView {
            binding: p.binding.clone(),
            client: p.client.clone(),
            credential: None, // 待批视图恒无凭据（批准前零凭据纪律）
        })
    }

    /// 用户批准（设置卡）：待批 → 钉公钥入账本 + 连接推 Granted。
    /// async：批准要向等待中的连接异步推放行帧（§2.3 对 revoke 未标 async 的
    /// 扩展说明：机制上撤权同样要推 Revoked 帧再关 Peer，入口签名不变）。
    ///
    /// **口径申报（Minor-2，不为此改行为）**：`pending_binding()` 现在会提前驱逐
    /// 过期记录（F4），因此「对一个已过期的码点批准」的失败码从
    /// `DSH_AUTH_EXPIRED` **漂移**为 `DSH_AUTH_UNKNOWN_BINDING`——记录已不在待批表里，
    /// 走到了「找不到」分支。两者语义等价：这个码**都不可批**，用户唯一的出路都是
    /// 「等插件重握手拿新码」。下面的 `AUTH_EXPIRED` 分支仍然保留：它覆盖「记录在
    /// 表内但此刻刚好过期，而驱逐尚未跑过」（例如 clock 在同一临界刻推进后直接
    /// 调用 approve，未经任何 `pending_binding()` / hello 驱逐）。
    pub async fn approve(self: &Arc<Self>, binding_id: &str) -> Result<(), DshError> {
        self.approve_with_mode(binding_id, FingerprintCommitMode::Normal)
            .await
    }

    pub async fn approve_rebind(
        self: &Arc<Self>,
        instance_id: &str,
        binding_id: &str,
        expected_old: &str,
    ) -> Result<(), DshError> {
        if self.instance_scope.as_deref() != Some(instance_id)
            || expected_old.is_empty()
            || self.registry_anchor.is_none()
        {
            return Err(DshError::new(
                codes::AUTH_SIGNATURE,
                "invalid rebind target",
            ));
        }
        self.approve_with_mode(
            binding_id,
            FingerprintCommitMode::ExplicitRebind {
                expected_old: expected_old.to_owned(),
            },
        )
        .await
    }

    async fn approve_with_mode(
        self: &Arc<Self>,
        binding_id: &str,
        mode: FingerprintCommitMode,
    ) -> Result<(), DshError> {
        // 调用方取消不能中断摘除 pending 后的提交、恢复与回执。
        let service = Arc::clone(self);
        let binding_id = binding_id.to_owned();
        tokio::spawn(async move { service.approve_with_mode_inner(&binding_id, mode).await })
            .await
            .map_err(|_| DshError::new(codes::IPC_STATE, "approval task failed"))?
    }

    async fn approve_with_mode_inner(
        self: &Arc<Self>,
        binding_id: &str,
        mode: FingerprintCommitMode,
    ) -> Result<(), DshError> {
        let handshake_gate = {
            let inner = self.inner.lock().unwrap();
            inner
                .pending
                .iter()
                .find(|p| p.binding.binding_id == binding_id)
                .map(|p| Arc::clone(&p.handshake_gate))
                .ok_or_else(|| {
                    DshError::new(
                        codes::AUTH_UNKNOWN_BINDING,
                        "pending binding no longer exists",
                    )
                })?
        };
        let _gate = handshake_gate.lock().await;
        // 单次取锁完成「找到 / 过期判定 / 摘除」——窗口与摘除不会两处漂移。
        let record = {
            let mut inner = self.inner.lock().unwrap();
            let now = self.env.clock.now_ms();
            let Some(pos) = inner
                .pending
                .iter()
                .position(|p| p.binding.binding_id == binding_id)
            else {
                return Err(DshError::new(
                    codes::AUTH_UNKNOWN_BINDING,
                    format!("no such pending binding: {binding_id}"),
                ));
            };
            if !Arc::ptr_eq(&inner.pending[pos].handshake_gate, &handshake_gate)
                || inner.pending[pos].conn.is_closed()
            {
                return Err(DshError::new(
                    codes::AUTH_NOT_APPROVED,
                    "pending connection changed or closed",
                ));
            }
            // 批准窗口 = 挑战 TTL（过期即拒，不假成功；同时把该条从内存摘掉）。
            // P1-①：摘除时关闭该连接——持有旧码的一端必须被告知作废，
            // 不能让它拿着过期核对码继续展示。
            if inner.pending[pos].binding.expires_at_ms < now {
                let stale = inner.pending.remove(pos);
                stale.conn.close();
                return Err(DshError::new(
                    codes::AUTH_EXPIRED,
                    "binding challenge expired; ask the client to reconnect",
                ));
            }
            inner.pending.remove(pos)
        };
        let client = record.client.clone();
        let spki_b64 = record.client_spki_b64.clone();
        let conn = Arc::clone(&record.conn);
        let host = record.host.clone();
        let service = Arc::clone(self);
        let work_conn = Arc::clone(&conn);
        let work_binding_id = binding_id.to_owned();
        let deadline_ms = record.binding.expires_at_ms;
        let attempt = tokio::task::spawn_blocking(move || -> Result<String, DshError> {
            if service
                .signed_event_line(&work_conn, &work_binding_id, "approval")
                .is_none()
            {
                return Err(DshError::new(
                    codes::AUTH_NOT_APPROVED,
                    "pending handshake is not ready",
                ));
            }
            // P1：**落账本之前**先问登记面会不会接受这枚指纹。
            //
            // 顺序曾经是「先 `pin_new_binding` 写 Bound 账本、再锚登记值」。锚定因
            // 「身份不连续」被拒时，账本里已经多出一条新绑定（新钥已受信），而用户
            // 看到的是一次失败批准——半完成状态：登记面还是旧身份，后续所有写入被
            // 指纹门拒，重试又因为新钥已 Bound 而走另一条分支。预检把它变成一次
            // 干净的拒绝（账本不动，用户重新授权即可）。
            if let (FingerprintCommitMode::Normal, Some(instance_id), Some(anchor)) =
                (&mode, &service.instance_scope, &service.registry_anchor)
            {
                if !anchor.accepts(instance_id, &key_digest(&b64url_decode(&spki_b64)?)) {
                    return Err(DshError::new(
                    codes::AUTH_SIGNATURE,
                    format!(
                        "instance {instance_id} is anchored to a different plugin identity; re-select/re-authorize required"
                    ),
                ));
                }
            }
            // 钉公钥入账本 + 受信 Peer + 闸门（H05 落账内核；常规路径沿用该次挑战的
            // binding_id——批的就是用户核对的那一个；同钥并发已 Bound 的竞态按内核
            // 复核复用既有绑定，Granted 携**落账生效**的 id）。
            service.pin_new_binding(
                &work_binding_id,
                &client,
                &spki_b64,
                work_conn,
                &host,
                true,
                deadline_ms,
                &mode,
                None,
            )
        }).await.unwrap_or_else(|_| Err(DshError::new(codes::IPC_STATE, "binding commit task failed")));
        let effective_id = match attempt {
            Ok(id) => id,
            Err(error) => {
                let mut inner = self.inner.lock().unwrap();
                let still_valid = !self
                    .binding_faulted
                    .load(std::sync::atomic::Ordering::SeqCst)
                    && !record.conn.is_closed()
                    && inner
                        .latest_key_connections
                        .get(&record.client_spki_b64)
                        .is_some_and(|latest| Arc::ptr_eq(latest, &record.conn))
                    && record.binding.expires_at_ms >= self.env.clock.now_ms()
                    && !inner.pending.iter().any(|p| {
                        p.binding.binding_id == binding_id
                            || p.client_spki_b64 == record.client_spki_b64
                    })
                    && inner
                        .event_proofs
                        .iter()
                        .any(|p| p.binding_id == binding_id && Arc::ptr_eq(&p.conn, &record.conn));
                if still_valid {
                    inner.pending.push(record);
                }
                return Err(error);
            }
        };
        // 放行事件绑定到原挑战与该连接上的待批决定。
        let frame = self
            .signed_event_line(&conn, &effective_id, "approval")
            .ok_or_else(|| {
                DshError::new(
                    codes::AUTH_PROTOCOL,
                    "missing authenticated approval context",
                )
            })?;
        match tokio::time::timeout(std::time::Duration::from_secs(2), conn.send(&frame)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                Arc::clone(&conn).close();
                Err(error)
            }
            Err(_) => {
                Arc::clone(&conn).close();
                Err(DshError::new(
                    codes::IPC_STATE,
                    "approval notification timed out",
                ))
            }
        }
    }

    /// 撤权：关受信 Peer、清钉公钥与凭据、拒绝自动重连（账本留撤权墓碑），
    /// 并向受信 Peer 推 Revoked 帧后关闭（交互路径的附加通知）。
    pub async fn revoke(self: &Arc<Self>, binding_id: &str) -> Result<(), DshError> {
        let service = Arc::clone(self);
        let binding_id = binding_id.to_owned();
        tokio::spawn(async move { service.revoke_inner(&binding_id).await })
            .await
            .map_err(|_| DshError::new(codes::IPC_STATE, "revocation task failed"))?
    }

    async fn revoke_inner(self: &Arc<Self>, binding_id: &str) -> Result<(), DshError> {
        let service = Arc::clone(self);
        let id = binding_id.to_owned();
        // 撤权不能排队在未认证连接的探测任务之后。
        let (send, receive) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("dsh-revoke".into())
            .spawn(move || {
                let _ = send.send(service.revoke_local(&id));
            })
            .map_err(|_| DshError::new(codes::IPC_STATE, "revocation thread failed"))?;
        let conn = receive
            .await
            .map_err(|_| DshError::new(codes::IPC_STATE, "revocation task failed"))??;
        if let Some(conn) = conn {
            // 先推通知（连接还在），再关闭。
            if let Some(frame) = self.signed_event_line(&conn, binding_id, "revocation") {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), conn.send(&frame))
                    .await;
            }
            Arc::clone(&conn).close();
        }
        Ok(())
    }

    /// 撤权同步核心（卸载路径；无帧推送）：关受信 Peer + 清信任 + 墓碑。
    /// 返回被关闭的连接（异步 [`Self::revoke`] 借此先推 Revoked 帧再关）。
    pub fn revoke_local(
        &self,
        binding_id: &str,
    ) -> Result<Option<Arc<dyn IpcClientConnection>>, DshError> {
        let _mutation = self.binding_mutation.lock().unwrap();
        self.revoke_local_locked(binding_id)
    }

    /// [`Self::revoke_local`] 的互斥内核（调用方必须已持 `binding_mutation`）。
    ///
    /// 先撤销内存中的信任与连接，再写账本。写盘失败时保留撤权结果并记入
    /// `revoke_write_pending`。重试只补写墓碑，开关执行门防止重启后旧绑定复活。
    fn revoke_local_locked(
        &self,
        binding_id: &str,
    ) -> Result<Option<Arc<dyn IpcClientConnection>>, DshError> {
        // 锁序为 ledger → pending。重试只写盘，保留此前生成的撤权墓碑。
        let retry_write = {
            let ledger = self.ledger.lock().unwrap();
            let pending = self.revoke_write_pending.lock().unwrap();
            let pos = ledger
                .bindings
                .iter()
                .rposition(|b| b.binding_id == binding_id)
                .ok_or_else(|| {
                    DshError::new(
                        codes::AUTH_UNKNOWN_BINDING,
                        format!("no such binding: {binding_id}"),
                    )
                })?;
            match ledger.bindings[pos].state {
                StoredBindingState::Bound => false,
                StoredBindingState::Revoked if pending.contains_key(binding_id) => true,
                _ => {
                    return Err(DshError::new(
                        codes::AUTH_UNKNOWN_BINDING,
                        format!("binding {binding_id} is not active"),
                    ));
                }
            }
        };
        // ② 内存面先行（全量撤权路径；重试路径已失效过一次，直接跳到落盘）。
        let conn = if retry_write {
            None
        } else {
            // 调用方负责关闭 IPC，交互撤权可先发送已签名通知。
            let (conn, gate) = {
                let mut inner = self.inner.lock().unwrap();
                inner.peers.remove(binding_id);
                inner.web_deliveries.remove(binding_id);
                let gate = inner
                    .gates
                    .entry(binding_id.to_string())
                    .or_insert_with(PeerGate::new)
                    .clone();
                let conn = inner.trusted.remove(binding_id);
                if let Some(ref revoked_conn) = conn {
                    inner
                        .latest_key_connections
                        .retain(|_, latest| !Arc::ptr_eq(latest, revoked_conn));
                }
                (conn, gate)
            };
            gate.revoke();
            // R01：撤权同时作废「已证明 Web 归属」，重新绑定需再次走环境证明。
            if let Some(instance_id) = self.instance_scope.clone() {
                self.inner
                    .lock()
                    .unwrap()
                    .proven_web_hosts
                    .remove(&instance_id);
            }
            // 实例绑定撤权只终止该实例的控制台和实时问答。
            if let Some(instance_id) = self.instance_scope.clone() {
                crate::remote::console_lifecycle::revoke_console_access_for_instance(&instance_id);
                // P1-1：撤权即**同步封存**实时问答（清空待答表 + 停掉常驻订阅）。
                // 撤权后旧 stream 的 clientId 已失效，留下的提问永远答不了——
                // 不清掉只会让手机端反复看到一张必然失败的卡片。
                crate::services::dsh_control::seal_live_questions(&instance_id);
            } else {
                crate::remote::console_lifecycle::revoke_console_access();
            }
            conn
        };
        // ③ 落盘（记账）：Bound → Revoked；钉公钥清空（信任物销毁），墓碑留存被撤钥。
        // 写失败**不回滚**（见函数头 Ruling 9 注）。
        {
            let mut ledger = self.ledger.lock().unwrap();
            let pos = ledger
                .bindings
                .iter()
                .rposition(|b| b.binding_id == binding_id)
                .ok_or_else(|| {
                    DshError::new(
                        codes::AUTH_UNKNOWN_BINDING,
                        format!("no such binding: {binding_id}"),
                    )
                })?;
            let path = ledger_path(&self.env);
            let previous_bytes = std::fs::read(&path).ok();
            if !retry_write {
                let revoked = std::mem::take(&mut ledger.bindings[pos].public_key_spki);
                ledger.bindings[pos].revoked_key_spki = revoked;
                ledger.bindings[pos].state = StoredBindingState::Revoked;
            }
            let bytes = match serde_json::to_vec_pretty(&*ledger) {
                Ok(bytes) => bytes,
                Err(e) => {
                    // 评审 Minor-2：与相邻写盘失败分支同口径——内存账本已变（墓碑
                    // 变换已做）但落盘没成 → 记待收敛 + 切断受信连接（内存面完整
                    // 生效，不因落盘侧失败回滚）。实践上不可达（固定结构序列化），
                    // 纯一致性对齐。
                    self.revoke_write_pending
                        .lock()
                        .unwrap()
                        .insert(binding_id.to_owned(), ());
                    drop(ledger);
                    if let Some(conn) = conn {
                        conn.close();
                    }
                    return Err(DshError::new(
                        codes::IPC_STATE,
                        format!("serialize bindings: {e}"),
                    ));
                }
            };
            if let Err(e) = atomic_write_0600(&path, &bytes) {
                // 记入待收敛集合：重试撤权据此只补写盘（内存行已是 Revoked）。
                self.revoke_write_pending
                    .lock()
                    .unwrap()
                    .insert(binding_id.to_owned(), ());
                let actual = std::fs::read(&path).ok();
                if !(previous_bytes.is_some() && actual == previous_bytes) {
                    // 写入是否已生效不可证明，不能保留任何受信连接（内存面已失效；
                    // 整域 fail-closed 是追加的保守面）。第九轮 P1-1：登记被撤绑定
                    // 证据（binding_id + 被撤钥摘要）供进程内恢复核验对位；实例域
                    // 限定的控制台吊销见 fail_closed 内注释。
                    let evidence = BindingFaultEvidence {
                        binding_id: binding_id.to_owned(),
                        revoked_key_digest: key_digest(
                            &b64url_decode(&ledger.bindings[pos].revoked_key_spki)
                                .unwrap_or_default(),
                        ),
                    };
                    drop(ledger);
                    self.fail_closed_binding_domain_with(Some(evidence));
                }
                // 写盘失败路径：受信连接在本函数内直接切断（不交还调用方——调用方
                // 拿到的是 Err，不会执行其关闭序；内存面「断活连接」必须完整生效）。
                if let Some(conn) = conn {
                    conn.close();
                }
                return Err(e);
            }
            self.revoke_write_pending.lock().unwrap().remove(binding_id);
        }
        Ok(conn)
    }

    /// 关闭撤权目标（Round-8 Ruling 9 重试收敛）：优先当代 Bound 行；无 Bound 但
    /// 存在「内存已撤、落盘未收敛」的待写行 → 返回该行（重试关闭据此补写盘）。
    /// 待收敛多行时按 id 排序取最小（评审 Minor-3：HashMap 遍历序不定，排序保证
    /// 重试收敛次序确定）。仅 [`Self::disable_invalidation`] 消费（用户面撤权按钮
    /// 仍走 [`Self::active_binding_id`]——那里没有待收敛态）。
    fn disable_revoke_target(&self) -> Option<String> {
        {
            let ledger = self.ledger.lock().unwrap();
            if let Some(binding_id) = ledger
                .bindings
                .iter()
                .rev()
                .find(|b| b.state == StoredBindingState::Bound)
                .map(|b| b.binding_id.clone())
            {
                return Some(binding_id);
            }
        }
        let pending = self.revoke_write_pending.lock().unwrap();
        let mut ids: Vec<&String> = pending.keys().collect();
        ids.sort();
        ids.into_iter().next().cloned()
    }

    /// 关闭的**同步失效段**（Round-8 Ruling 10：与绑定提交共用 `binding_mutation`
    /// 串行边界）——取消待授权意图 + 扫描撤权目标 + 撤权（含账本落盘）全程持互斥。
    /// 返回被关闭的受信连接（调用方负责 close）。绑定提交（`pin_new_binding`，同
    /// 持该互斥）因此与本段严格串行：要么提交先落地（本段随后看到并撤走新鲜绑定），
    /// 要么本段先完成（提交进互斥后被互斥内门 `check_switch_gate` 拒绝）——
    /// 「disable 在门检查与账本写入之间插队 → Off 却 Bound」的微窗就此关闭。
    ///
    /// 锁序核查（Ruling 10 前置，Round-8 实现者已验）：`binding_mutation` 的全部
    /// 取用点（钉钥重连分支 / `accept_credential` / `pin_new_binding` /
    /// `revoke_local`）在持锁期间唯一触达开关域的是 `check_switch_gate` 闭包——
    /// 其判定内核 `authorization_gate_check` 只做无锁小文件读（绝不取
    /// `action_lock`）；`action_lock` 的取用点（commit/read/enable/disable/repair/
    /// tick 复核）不持锁调任何取 `binding_mutation` 的入口。无反序路径，锁序
    /// action_lock → binding_mutation 单向成立。
    pub fn disable_invalidation(&self) -> Result<Option<Arc<dyn IpcClientConnection>>, DshError> {
        let _mutation = self.binding_mutation.lock().unwrap();
        // 迟到的握手不得再被自动批准（Task 4 关闭语义；intent 互斥在
        // binding_mutation 之内取用，无反序路径）。
        self.cancel_authorization_intent();
        let Some(binding_id) = self.disable_revoke_target() else {
            return Ok(None);
        };
        self.revoke_local_locked(&binding_id)
    }

    /// 授权状态（manager status 的数据源）：有 Bound → Bound；否则最近墓碑 →
    /// Revoked；无记录 → Unbound。
    pub fn authorization_state(&self) -> AuthorizationState {
        let ledger = self.ledger.lock().unwrap();
        if ledger
            .bindings
            .iter()
            .any(|b| b.state == StoredBindingState::Bound)
        {
            AuthorizationState::Bound
        } else if ledger
            .bindings
            .iter()
            .any(|b| b.state == StoredBindingState::Revoked)
        {
            AuthorizationState::Revoked
        } else {
            AuthorizationState::Unbound
        }
    }

    /// R01 晋升的只读观测面（测试/诊断）：该实例当代 Web 归属是否已被证明。
    ///
    /// `#[doc(hidden)]`：不导出任何凭据，仅回答「本代是否已核验归属」。
    #[doc(hidden)]
    pub fn web_home_verified_for_test(&self, instance_id: &str) -> Option<bool> {
        // 只读观测：是否已为该实例登记过归属证明，且记录的代际仍与当前活体一致。
        let recorded = {
            let inner = self.inner.lock().unwrap();
            inner.proven_web_hosts.get(instance_id).cloned()
        }?;
        let (host, _) = self.current_live().ok()?;
        Some(host.epoch == recorded.epoch && host == recorded)
    }

    /// 该实例当前 Bound 的 binding id（只读；测试撤权用）。
    #[doc(hidden)]
    pub fn active_binding_for_test(&self, _instance_id: &str) -> Option<String> {
        let ledger = self.ledger.lock().unwrap();
        ledger
            .bindings
            .iter()
            .rev()
            .find(|b| b.state == StoredBindingState::Bound)
            .map(|b| b.binding_id.clone())
    }

    /// 该绑定是否已有**交付的凭据槽位**（T09-G 复审 Nit-3 的观测面；C-6）。
    ///
    /// `#[doc(hidden)]`：非公开 API，仅供诊断/测试锁定 `revoke_local` 的
    /// `peers.remove` 粒度（只 remove 本 binding 条目，不牵连其他绑定）。只报
    /// 存在性——**不导出句柄、不导出任何字节**。
    #[doc(hidden)]
    pub fn has_delivered_peer(&self, binding_id: &str) -> bool {
        self.inner.lock().unwrap().peers.contains_key(binding_id)
    }

    /// hub 侧原生派发句柄（T10）：取当前 Bound 绑定已交付的授权句柄。
    ///
    /// 与 [`Self::authorize_grant`] 的差别：那是**插件进程**来取（须过 OS 进程
    /// 身份核验）；本入口是兔维斯自身（hub）取——只做三件事：取该绑定槽位、
    /// 复核闸门未撤、复核代际为当前代（换代未重交付 → None，不重盖章）。
    /// 无 Bound 绑定或凭据未交付 → None。
    pub async fn peer_for_control(self: &Arc<Self>) -> Option<AuthorizedPeer> {
        self.checked_peer_for_control().await.ok().flatten()
    }

    pub(crate) async fn checked_peer_for_control(
        self: &Arc<Self>,
    ) -> Result<Option<AuthorizedPeer>, DshError> {
        let service = Arc::clone(self);
        tokio::task::spawn_blocking(move || service.checked_peer_for_control_blocking())
            .await
            .map_err(|_| DshError::new(codes::IPC_STATE, "peer lookup task failed"))?
    }

    /// T3 发送预检的**三态诊断**（与 [`Self::checked_peer_for_control_blocking`] 同一
    /// 判定链，但把无句柄 的三种成因分开——发送预检要区分「正在恢复连接」与
    /// 「请在电脑上检查所选实例」，不把两者折成一个通用失败）：
    ///
    /// * `Ready`——当代发送凭据在位（`peer_for_control` 会给出句柄）；
    /// * `Recovering`——Bound 绑定在场但凭据是旧代际（或尚未交付）：宿主重启
    ///   后活体跟踪拍会自动重发布当代记录并重交付（Task 2 契约），属可恢复
    ///   暂态 →「正在恢复连接」；
    /// * `NotAuthorized`——无 Bound 绑定 / 闸门已撤 / 当代宿主不可证——需要
    ///   用户在电脑上检查所选实例（启动/授权）→「请在电脑上检查所选实例」。
    ///
    /// 内含阻塞发现（`current_host` → 进程扫描）：调用方必须在 blocking 池执行。
    pub fn control_peer_state(&self) -> ControlPeerState {
        let binding_id = {
            let ledger = self.ledger.lock().unwrap();
            ledger
                .bindings
                .iter()
                .rev()
                .find(|b| b.state == StoredBindingState::Bound)
                .map(|b| b.binding_id.clone())
        };
        let Some(binding_id) = binding_id else {
            return ControlPeerState::NotAuthorized;
        };
        let (peer, gate_revoked) = {
            let inner = self.inner.lock().unwrap();
            let peer = inner.peers.get(&binding_id).cloned();
            let gate_revoked = inner.gates.get(&binding_id).is_some_and(|g| g.is_revoked());
            (peer, gate_revoked)
        };
        if gate_revoked {
            return ControlPeerState::NotAuthorized;
        }
        let Some(peer) = peer else {
            // 绑定在场、凭据槽位未交付——批准后/重连后的交付在途（Task 2 的
            // 「同钥换代重连免批准并重新交付发送/网页凭据」），按恢复中呈现。
            return ControlPeerState::Recovering;
        };
        let Ok(current) = self.current_host() else {
            // 当代宿主不可证（与 peer_for_control 的 None 同因）——不是暂态恢复，
            // 引导用户去电脑上检查实例。
            return ControlPeerState::NotAuthorized;
        };
        if peer.epoch() != current.epoch || peer.authority() != current.authority {
            return ControlPeerState::Recovering;
        }
        ControlPeerState::Ready
    }

    /// 入口只读取已交付的授权代际，不探测进程，也不复制凭据。
    pub(crate) fn send_host_stamp(&self) -> Option<super::production::SendHostStamp> {
        let ledger = self.ledger.try_lock().ok()?;
        let binding = ledger
            .bindings
            .iter()
            .rev()
            .find(|row| row.state == StoredBindingState::Bound)?;
        let inner = self.inner.try_lock().ok()?;
        let peer = inner.peers.get(&binding.binding_id)?;
        if inner
            .gates
            .get(&binding.binding_id)
            .is_some_and(|gate| gate.is_revoked())
        {
            return None;
        }
        Some(super::production::SendHostStamp {
            instance: peer.host_id().to_owned(),
            epoch: peer.epoch(),
            authority: peer.authority().to_owned(),
        })
    }

    #[cfg(test)]
    fn peer_for_control_blocking(&self) -> Option<AuthorizedPeer> {
        self.checked_peer_for_control_blocking().ok().flatten()
    }

    fn checked_peer_for_control_blocking(&self) -> Result<Option<AuthorizedPeer>, DshError> {
        let binding_id = self.active_binding_id();
        let Some(binding_id) = binding_id else {
            return Ok(None);
        };
        let peer = {
            let inner = self.inner.lock().unwrap();
            if inner
                .gates
                .get(&binding_id)
                .is_some_and(|gate| gate.is_revoked())
            {
                return Ok(None);
            }
            inner.peers.get(&binding_id).cloned()
        };
        let Some(peer) = peer else {
            return Ok(None);
        };
        let current = self.current_host()?;
        if peer.epoch() != current.epoch || peer.authority() != current.authority {
            return Ok(None);
        }
        Ok(Some(peer))
    }

    /// 当前活跃（Bound）绑定 id（撤权按钮的真实 id 来源；无 → None）。
    pub fn active_binding_id(&self) -> Option<String> {
        let ledger = self.ledger.lock().unwrap();
        ledger
            .bindings
            .iter()
            .rev()
            .find(|b| b.state == StoredBindingState::Bound)
            .map(|b| b.binding_id.clone())
    }

    /// 生成 binding_id / 内部序号（单调 + 随机后缀）。
    fn next_binding_id(&self) -> String {
        format!(
            "binding-{}-{:08x}",
            self.env.clock.now_ms(),
            self.seq.fetch_add(1, Ordering::SeqCst)
        )
    }
}

// ============================================================
// 生产面（装配在 production.rs；此处只放 §2.3 签名固定的委托入口）
// ============================================================

/// §2.3 生产入口：`auth::authorize(host, client)`（async）。
///
/// 语义：对**已通过本机 IPC 握手核验**的客户端身份铸造/取回授权句柄——
/// 身份须与 OS 进程表一致、宿主代际须为当前代、公钥须已钉（已批准）。
/// IPC 循环内的完整握手（含签名挑战与 OS peercred）由 [`DshAuthService`]
/// 承载；本入口供 T09 hub/装配层在拿到 ClientIdentity 后取秘密句柄。
pub async fn authorize(
    host: LocalHost,
    client: ClientIdentity,
) -> Result<AuthorizedPeer, DshError> {
    super::production::production_auth()?
        .authorize_grant(&host, &client)
        .await
}

/// §2.3 生产入口：`auth::revoke(binding_id)`（async；契约原文未标 async 的
/// 文档化扩展——撤权要先向受信 Peer 推 Revoked 帧再关连接，机制上必须异步）。
pub async fn revoke(binding_id: &str) -> Result<(), DshError> {
    super::production::production_auth()?
        .revoke(binding_id)
        .await
}

impl DshAuthService {
    /// [`authorize`] 的服务内实现：身份与 OS 进程表一致 + 宿主当前代 +
    /// 公钥已钉 + 秘密句柄已在（cookie 已交付）→ 返回内存中的授权句柄。
    pub async fn authorize_grant(
        self: &Arc<Self>,
        host: &LocalHost,
        client: &ClientIdentity,
    ) -> Result<AuthorizedPeer, DshError> {
        let service = Arc::clone(self);
        let host = host.clone();
        let client = client.clone();
        tokio::task::spawn_blocking(move || service.authorize_grant_blocking(&host, &client))
            .await
            .map_err(|_| DshError::new(codes::IPC_STATE, "authorization probe task failed"))?
    }

    fn authorize_grant_blocking(
        &self,
        host: &LocalHost,
        client: &ClientIdentity,
    ) -> Result<AuthorizedPeer, DshError> {
        let proc = self
            .env
            .processes
            .list_processes()
            .into_iter()
            .find(|p| p.pid == client.pid)
            .ok_or_else(|| {
                DshError::new(codes::PEER_MISMATCH, "client pid is not in process table")
            })?;
        if proc.exe != client.executable || proc.started_at_ms != client.started_at_ms {
            return Err(DshError::new(
                codes::PEER_MISMATCH,
                "client identity does not match the process table",
            ));
        }
        let current = self.current_host()?;
        if current.epoch != host.epoch {
            return Err(DshError::new(
                codes::AUTH_EPOCH_STALE,
                format!(
                    "host epoch {} is stale (current {})",
                    host.epoch, current.epoch
                ),
            ));
        }
        // 撤权判定**先于**钉公钥判定：撤权会清空 `public_key_spki`（信任物销毁），
        // 若先查 pinned 会把「已撤权」误报成「未批准」——插件侧靠这个码区分
        // 「等用户批准」与「已被撤销，停止交付凭据」（group 3 的 wire 契约）。
        //
        // I-1 修复（本批）：判定必须收敛到**调用方自己的绑定**。旧实现写的是
        // `inner.gates.values().find(|g| g.is_revoked())`——`gates` 条目**永不删除**
        // （R08 刻意保留永久 revoked 闸门，复位等于悄悄重开权限），于是任意一次
        // 撤权都会毒化**此后全部**授权请求：另一台刚批准并交付凭据的客户端也拿
        // `DSH_PEER_REVOKED`，直到进程重启。
        //
        // 现在按客户端公钥解析账本里**自己的** Bound 行（行携带 binding_id），
        // 只查 `gates.get(binding_id)`。判定顺序（三条互斥分支）：
        //  ① 存在自己的 Bound 行且其闸门未翻转 → 放行（去取凭据句柄）；
        //  ② 存在自己的 Bound 行但闸门已翻转 → `DSH_PEER_REVOKED`；
        //  ③ 没有 Bound 行：有同钥撤权墓碑 → `DSH_PEER_REVOKED`（同钥被撤后重连的
        //     拒绝面）；否则 → `DSH_AUTH_NOT_APPROVED`（等用户批准）。
        //
        // 为什么「存在 Bound 行」优先于墓碑：重新批准是**用户显式动作**，落账为一条
        // 新的 Bound 行；同一把公钥的历史撤销墓碑不得否决这次重新批准（I-1 的
        // 第二面：撤 A → 同钥以新绑定重新批准 → 必须能授权）。
        let (own_binding_id, tombstoned) = {
            let ledger = self.ledger.lock().unwrap();
            let spki = b64url_encode(&client.public_key_spki);
            let bound_id = ledger
                .bindings
                .iter()
                .rev()
                .find(|b| b.state == StoredBindingState::Bound && b.public_key_spki == spki)
                .map(|b| b.binding_id.clone());
            let tombstoned = ledger
                .bindings
                .iter()
                .any(|b| b.state == StoredBindingState::Revoked && b.revoked_key_spki == spki);
            (bound_id, tombstoned)
        };
        let Some(own_binding_id) = own_binding_id else {
            // 没有自己的 Bound 行 → 要么被撤（墓碑），要么从未批准。
            if tombstoned {
                return Err(DshError::new(
                    codes::PEER_REVOKED,
                    "this binding was revoked; no authorized handle is issued",
                ));
            }
            return Err(DshError::new(
                codes::AUTH_NOT_APPROVED,
                "client key is not pinned; approve the binding first",
            ));
        };
        let own_gate_revoked = {
            let inner = self.inner.lock().unwrap();
            inner
                .gates
                .get(&own_binding_id)
                .map(|g| g.is_revoked())
                .unwrap_or(false)
        };
        if own_gate_revoked {
            return Err(DshError::new(
                codes::PEER_REVOKED,
                "this binding was revoked; no authorized handle is issued",
            ));
        }
        let (peer, gate) = {
            let inner = self.inner.lock().unwrap();
            // T09-G：按调用方**自己的**绑定取凭据槽（键 = 上面的 own_binding_id），
            // 不是全局单槽。缺 = **该绑定**尚未经可信 IPC 交付凭据——其他绑定是否
            // 已交付不影响这里的判定（也不得拿来顶替）。
            let peer = inner.peers.get(&own_binding_id).cloned().ok_or_else(|| {
                DshError::new(
                    codes::AUTH_NOT_APPROVED,
                    "no credential has been delivered over the trusted local IPC yet",
                )
            })?;
            // R08：闸门必须随句柄一起交出去（否则「已交付句柄」在撤权后仍能重连）。
            let gate = peer.gate().map(Arc::clone);
            (peer, gate)
        };
        // P1-②（2026-10-30 第一阶段复审 T08 前门槛）＋ T09-G（第二批复审放行）：
        // **核验**而非重盖章。
        //
        // ① 归属核验。单槽时代它是「槽里可能是别人的凭据」的功能屏障（F6/复审
        //   Important-6：归属必须先于闸门核验，不得拿 B 的状态回答 A 的问题）；
        //   T09-G 分槽后，句柄取自以 `own_binding_id` 为键的槽，槽键与句柄钉定的
        //   binding_id 恒等——本判定降级为**纵深防御断言**（内部状态被破坏时拒绝
        //   发放，而不是把别人的凭据发出去）。序不变：仍排在闸门/代际核验之前，
        //   归属不成立时正确答案就是「这份凭据不属于你」。
        if peer.binding_id() != own_binding_id {
            return Err(DshError::new(
                codes::AUTH_NOT_APPROVED,
                "the delivered credential belongs to a different binding; not issued to this caller",
            ));
        }
        // 归属成立后才谈闸门：撤权后不再发句柄（闸门已翻转 → 直接拒绝，不发一个
        // 必然连不上的句柄）。此处闸门与 peer 同属本调用者的绑定。
        if let Some(gate) = gate.as_ref() {
            if gate.is_revoked() {
                return Err(DshError::new(
                    codes::PEER_REVOKED,
                    "this binding was revoked; no authorized handle is issued",
                ));
            }
        }
        // ② 当代：交付点（accept_credential/R06）已把交付时的 authority/epoch 钉在
        //   句柄上；宿主换代后、该绑定经可信 IPC 重新交付当前代凭据之前，旧代
        //   Cookie 不许包装成新代句柄（transport 拿它连新宿主只会 401，还掩盖了
        //   「必须重新交付」的真状态）。句柄原样返回——不重盖章。
        if peer.epoch() != current.epoch || peer.authority() != current.authority {
            return Err(DshError::new(
                codes::AUTH_EPOCH_STALE,
                "host regenerated after this credential was delivered; waiting for a current-generation redelivery over the trusted local IPC",
            ));
        }
        Ok(peer)
    }
}

/// auth 单例 → manager 授权读取面的适配（生产装配注入；测试可自建同形）。
pub struct AuthServiceReader(pub Arc<DshAuthService>);

impl super::install::DshAuthorizationReader for AuthServiceReader {
    fn authorization_state(&self) -> AuthorizationState {
        self.0.authorization_state()
    }

    fn active_binding_id(&self) -> Option<String> {
        self.0.active_binding_id()
    }
}

/// 生产 BindingRevoker（替换 TbdBindingRevoker；卸载作业先撤绑定的落点）。
///
/// ## 匹配口径（I-4 修复，**已选并固定**）
///
/// 卸载作业传进来的是 `identity::resolve_installation().profile_dir`
/// （= `<env.home>/profiles/desktop`，`--profile desktop` 的推导口径）；而账本里记的
/// 是 **批准时** `identity::discover()` 的 profile_dir——宿主 argv 优先（自定义
/// DSH_HOME 时 argv 的 profile 根可以完全不同于兔维斯 env.home）。两者字符串不等
/// 时旧实现撤不掉任何东西，静默留下一个陈旧授权。
///
/// 本实现按两级匹配，任一命中即认定为**同一 profile**：
///  1. **归一化路径等价**：统一分隔符（`\` → `/`）、折叠重复分隔符、丢掉 `.` 段、
///     去掉尾分隔符后逐段相等（Windows 上再忽略大小写——盘符/路径大小写不敏感）；
///  2. **profile 名尾段**（`profiles/<name>`，`<name>` 逐字比较）：官方 CLI 的
///     `--profile` 只接受**名字**（`resolveProfileDir(name, home)`），所以名字相同
///     就是同一个逻辑 profile——不同 home 根是「同一 profile 在另一处落盘」。
///
/// 不做的是「按目录前缀/包含关系模糊匹配」：那会把 `profiles/desktop-2` 之类的
/// 兄弟 profile 误判成同一个，撤错对象比撤不到更糟（多撤 = 悄悄扩大撤权面）。
pub struct ProductionBindingRevoker;

/// 路径归一化（词法；不触文件系统）。返回去掉分隔符噪声的段序列。
fn normalized_segments(path: &str) -> Vec<String> {
    path.replace('\\', "/")
        .split('/')
        .filter(|segment| !segment.is_empty() && *segment != ".")
        .map(|segment| {
            if cfg!(windows) {
                segment.to_lowercase()
            } else {
                segment.to_string()
            }
        })
        .collect()
}

/// 两个 profile 标识是否指向同一 profile（I-4 的两级判据，见 [`ProductionBindingRevoker`]）。
pub fn profile_matches(recorded: &str, requested: &str) -> bool {
    if recorded == requested {
        return true;
    }
    let (recorded_segments, requested_segments) = (
        normalized_segments(recorded),
        normalized_segments(requested),
    );
    if recorded_segments == requested_segments {
        return true;
    }
    // 尾段 `profiles/<name>`：官方 CLI 的 profile 名口径（不同 home 根 = 同一逻辑 profile）。
    let tail = |segments: &Vec<String>| -> Option<(String, String)> {
        let len = segments.len();
        if len < 2 {
            return None;
        }
        Some((segments[len - 2].clone(), segments[len - 1].clone()))
    };
    match (tail(&recorded_segments), tail(&requested_segments)) {
        (Some(a), Some(b)) => a == b && a.0 == "profiles",
        _ => false,
    }
}

impl ProductionBindingRevoker {
    /// 撤权纯内核（**唯一**实现；生产入口 [`BindingRevoker::revoke_profile`] 传
    /// `production_auth()` + `production_env()` 的结果，测试注入「域可用 / 域坏」
    /// 两种结果，不触生产单例）。
    ///
    /// 三档语义（I-3 修复 + I-6 复审补口）：
    ///
    /// 1. **域不可用**（`Err`）→ **成功**，但**必须落盘撤销**。撤销闸门表
    ///    （`inner.gates`）与受信 Peer 注册（`inner.trusted`）只在内存里，域初始化
    ///    失败意味着本进程从未建起授权服务、没有活连接/在途句柄可撤；而卸载是用户
    ///    唯一的清理出口（状态可能已写到盘上），把「域坏了」变成作业 Failed 会让
    ///    用户没有出路。但持久账本里的 Bound 行**必须**改成撤权墓碑，否则修复并
    ///    重装后同一把插件钥会命中钉公钥分支、免用户批准直接 `authorized`
    ///    （违反 §2.5「先撤后删」）。做法 = [`revoke_profile_ledger_only`]（纯账本
    ///    变换 + 0600 原子写）；连账本都写不成时留 `revoke-pending.json`，由下一次
    ///    成功初始化消费——两种情形都仍返回 Ok（不堵死卸载出口）。
    /// 2. **该 profile 没有 Bound 绑定**（含只有墓碑）→ **成功**：没有可撤的东西。
    ///    旧实现把它当 `DSH_REVOKE_FAILED`，「装了但从未绑定过」的卸载必然失败。
    /// 3. **有 Bound 绑定** → 走 [`DshAuthService::revoke_local`]，**失败即
    ///    `DSH_REVOKE_FAILED`**（「先撤后删」的序与真实性都不放水：撤权真失败时
    ///    绝不允许作业继续到 remove）。
    pub fn revoke_with(
        auth: Result<Arc<DshAuthService>, DshError>,
        env: &DshEnvironment,
        profile_id: &str,
    ) -> Result<(), DshError> {
        let svc = match auth {
            Ok(svc) => svc,
            Err(e) => {
                // 域不可用 = 本进程未建立授权通道（闸门只在内存里）→ 无活连接可撤；
                // 但**盘上的 Bound 行必须撤**（I-6：否则修复后免批准复活）。
                match revoke_profile_ledger_only(env, profile_id) {
                    Ok(changed) => {
                        log::warn!(
                            "DSH 授权域不可用，卸载前撤权按账本只写完成（改动 {changed} 行）：{}: {}",
                            e.code,
                            e.detail
                        );
                    }
                    Err(write_err) => {
                        log::warn!(
                            "DSH 授权域不可用且账本只写失败，留待落盘撤销标记（下次启动补刀）：{}: {} / {}: {}",
                            e.code,
                            e.detail,
                            write_err.code,
                            write_err.detail
                        );
                        remember_revoke_pending(env, profile_id);
                    }
                }
                return Ok(());
            }
        };
        // P1-2：撤该 profile 的**全部** Bound 绑定——T09-G 分槽后同一 profile 可合法
        // 存在多条绑定（不同公钥/不同 Cookie）；只撤最新一条会让其余绑定免批准
        // 复活（违反 §2.5「先撤后删」），且与域不可用路径的账本兜底（for 循环撤
        // 全部匹配行）范围不一致。已撤的不可回滚（撤权不可逆），因此任一失败都
        // 立即返回 REVOKE_FAILED 并申报「已撤 n 条 + 失败绑定与原因」，绝不谎报全撤。
        //
        // Round-8 P1-1（评审 Important）：**待收敛行一并入列**——`revoke_local` 的
        // 「内存面先行」重排后，撤权写盘失败留下「内存 Revoked + revoke_write_pending
        // + 盘上 Bound」。若收集只看内存 Bound，重试卸载会跳过该行、收尾校验也因
        // 同一偏差放行 → 插件文件被 remove 而墓碑从未落盘 → 修复重装后同钥命中钉钥
        // 分支免批准复活（正是本函数 :4965/:4983 注释针对的 §2.5/I-6 面）。待收敛行
        // 经 `revoke_local` 的只补写盘路径收敛墓碑，收敛失败照常 REVOKE_FAILED。
        let binding_ids: Vec<String> = {
            let ledger = svc.ledger.lock().unwrap();
            let pending = svc.revoke_write_pending.lock().unwrap();
            ledger
                .bindings
                .iter()
                .filter(|b| {
                    profile_matches(&b.profile_dir, profile_id)
                        && (b.state == StoredBindingState::Bound
                            || (b.state == StoredBindingState::Revoked
                                && pending.contains_key(&b.binding_id)))
                })
                .map(|b| b.binding_id.clone())
                .collect()
        };
        // 卸载路径走同步核心（清信任 + 墓碑 + 关 Peer）；插件经 EOF/下次握手被拒
        // 感知撤权——语义完整，异步 Revoked 帧是交互路径的附加通知。
        //
        // 复审 Minor-3：收集锁释放到循环之前若并发 `approve` 插入了新 Bound 行，
        // 空列表/漏收都会让卸载继续——因此**所有路径**（含空列表）都走循环后的
        // 收尾校验「该 profile 无残留 Bound 才 Ok」。
        let mut revoked = 0usize;
        for binding_id in &binding_ids {
            match svc.revoke_local(binding_id) {
                Ok(conn) => {
                    if let Some(c) = conn {
                        c.close();
                    }
                    revoked += 1;
                }
                // 复审 M-2：收集列表后锁已释放——同进程交互撤权（`dsh_binding_revoke`）
                // 可能先撤掉这条。`AUTH_UNKNOWN_BINDING` 表示**该行已不是 Bound**
                // （目标后置条件已达成），应计为已撤并继续，而不是把「已撤」误报为
                // 卸载失败。只有其它失败（序列化/落盘等真实撤销失败）才中止。
                Err(e) if e.code == codes::AUTH_UNKNOWN_BINDING => {
                    let already = {
                        let ledger = svc.ledger.lock().unwrap();
                        // 账本里已无该 id，或该行已不是 Bound → 视为已撤。
                        ledger
                            .bindings
                            .iter()
                            .find(|b| b.binding_id == *binding_id)
                            .is_none_or(|b| b.state != StoredBindingState::Bound)
                    };
                    if already {
                        revoked += 1;
                    } else {
                        return Err(DshError::new(
                            codes::REVOKE_FAILED,
                            format!(
                                "revoking binding {binding_id} failed after {revoked}/{} revoked: {} ({})",
                                binding_ids.len(),
                                e.code,
                                e.detail
                            ),
                        ));
                    }
                }
                Err(e) => {
                    let pending: Vec<&str> =
                        binding_ids[revoked..].iter().map(String::as_str).collect();
                    return Err(DshError::new(
                        codes::REVOKE_FAILED,
                        format!(
                            "revoking binding {binding_id} failed after {revoked}/{} revoked (remaining: {:?}): {} ({})",
                            binding_ids.len(),
                            pending,
                            e.code,
                            e.detail
                        ),
                    ));
                }
            }
        }
        // 复审 m-2：收集与逐条撤权之间若 `approve` 插入了同 profile 的新 Bound 行，
        // 循环不会包含它、上面就会返回 Ok（卸载继续 → 新绑定免批准复活）。收尾
        // 重读账本：仍有 Bound 行 → fail-closed 报 REVOKE_FAILED（不回滚已撤项；
        // 撤权不可逆），由用户重试卸载。Round-8（评审 Important）：**待收敛行同算
        // 残留**——内存 Revoked 但落盘未收敛的行，其盘上墓碑尚未成立，放行等于
        // 「先撤后删」的撤字造假。
        {
            let ledger = svc.ledger.lock().unwrap();
            let pending = svc.revoke_write_pending.lock().unwrap();
            let leftover: Vec<&str> = ledger
                .bindings
                .iter()
                .filter(|b| {
                    profile_matches(&b.profile_dir, profile_id)
                        && (b.state == StoredBindingState::Bound
                            || (b.state == StoredBindingState::Revoked
                                && pending.contains_key(&b.binding_id)))
                })
                .map(|b| b.binding_id.as_str())
                .collect();
            if !leftover.is_empty() {
                return Err(DshError::new(
                    codes::REVOKE_FAILED,
                    format!(
                        "profile still has bound/unconverged bindings after revoke: {leftover:?}"
                    ),
                ));
            }
        }
        Ok(())
    }
}

impl BindingRevoker for ProductionBindingRevoker {
    fn revoke_profile(&self, profile_id: &str) -> Result<(), DshError> {
        Self::revoke_with(
            super::production::production_auth(),
            &super::production::production_env(),
            profile_id,
        )
    }
}

/// 固定包名再导出（撤权与包管理的关联点在 G1 runbook 文档；避免未用告警）。
pub const PACKAGE: &str = PACKAGE_NAME;

// ============================================================
// 单元测试（基础设施面：SPKI 包拆皮 / 签名消息字节稳定性 / 帧编解码）
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    use super::super::model::{
        CommandRequest, DshClock, DshCommandRunner, DshFileSource, DshListener, DshProcess,
        DshProcessSource, RunOutcome, TestUnobservableEnvironments,
    };

    /// SPKI 包皮/拆皮互逆；形状不符拒绝。
    #[test]
    fn spki_wrap_unwrap_roundtrip() {
        let raw = [7u8; 32];
        let der = spki_wrap_raw(&raw);
        assert_eq!(der.len(), 44);
        assert_eq!(spki_unwrap(&der).unwrap(), raw);
        assert!(spki_unwrap(&der[..40]).is_err());
        let mut tampered = der.clone();
        tampered[5] = 0x06; // 改算法 OID 字节
        assert!(spki_unwrap(&tampered).is_err());
    }

    /// 签名消息字节稳定性：字段序固定（bindingProtocol/keyDigests/nonce/hostId/epoch/expiry）。
    #[test]
    fn challenge_message_bytes_are_stable() {
        let msg = BindingChallengeMessage {
            binding_protocol: BINDING_PROTOCOL,
            key_digests: ["rust".into(), "client".into()],
            nonce: "abc".into(),
            host_id: "local".into(),
            epoch: "123".into(),
            expiry: 1_760_000_060_000,
            home_binding: HomeBinding {
                home_root: "/home/test".into(),
                host_pid: 1,
                host_started_at_ms: 2,
            },
        };
        let bytes = msg.to_signing_bytes();
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(
            text,
            r#"{"bindingProtocol":2,"keyDigests":["rust","client"],"nonce":"abc","hostId":"local","epoch":"123","expiry":1760000060000,"homeBinding":{"homeRoot":"/home/test","hostPid":1,"hostStartedAtMs":2}}"#
        );
    }

    /// Ed25519 签名/验签互逆；换消息/换公钥/截断签名都失败。
    #[test]
    fn sign_verify_roundtrip_and_rejections() {
        let rust = RustSigningKey::generate();
        let client = RustSigningKey::generate();
        let msg = b"challenge-message";
        let sig = client.sign(msg);
        rust.verify_client(&client.spki, msg, &sig).unwrap();
        assert!(rust.verify_client(&client.spki, b"other", &sig).is_err());
        assert!(rust.verify_client(&rust.spki, msg, &sig).is_err());
        assert!(rust.verify_client(&client.spki, msg, &sig[..63]).is_err());
    }

    /// 帧编解码往返；未知字段/未知类型拒绝（deny_unknown_fields 纪律）。
    #[test]
    fn frames_roundtrip_and_reject_unknown() {
        let hello = HelloFrame {
            binding_protocol: BINDING_PROTOCOL,
            public_key_spki: "pk".into(),
            executable: "exe".into(),
            pid: 1,
            started_at_ms: 2,
            host_id: "local".into(),
            epoch: "3".into(),
            nonce: "n".into(),
            expiry: 4,
            home_binding: HomeBinding {
                home_root: "/home/test".into(),
                host_pid: 1,
                host_started_at_ms: 2,
            },
            signature: "s".into(),
        };
        let line = client_frame_line(&ClientFrame::Hello(hello.clone()));
        assert_eq!(
            parse_client_frame(&line).unwrap(),
            ClientFrame::Hello(hello)
        );
        let mut missing_home_binding: serde_json::Value = serde_json::from_str(&line).unwrap();
        missing_home_binding
            .as_object_mut()
            .unwrap()
            .remove("homeBinding");
        assert!(parse_client_frame(&missing_home_binding.to_string()).is_err());
        assert!(parse_client_frame("{\"type\":\"hello\",\"bogus\":1}").is_err());
        assert!(parse_client_frame("{\"type\":\"wat\"}").is_err());
        let reply = ServerFrame::Error {
            error: "X".into(),
            detail: "d".into(),
        };
        assert_eq!(
            parse_server_frame(&server_frame_line(&reply)).unwrap(),
            reply
        );
    }

    /// display_code：6 字符、字母表内（短时核对码，不是长效凭据）。
    #[test]
    fn display_code_shape() {
        let code = display_code();
        assert_eq!(code.len(), 6);
        assert!(code.bytes().all(|b| DISPLAY_ALPHABET.contains(&b)));
    }

    // ============================================================
    // T3（复审 Important-2）：control_peer_state 三态直测——真实账本/句柄/闸门
    // 状态推导（非探针注入），并与 peer_for_control_blocking 逐态对齐
    // （Ready ⟺ Some；其余态 ⟺ None 且成因可辨）。
    // ============================================================

    const CP_HOME: &str = r"C:\Users\bunny\.dsh";
    const CP_PROFILE: &str = r"C:\Users\bunny\.dsh\profiles\desktop";
    const CP_INSTALL: &str = r"D:\Program Files\Deepseek-Harness";
    const CP_PID: u32 = 37096;
    const CP_STARTED_AT: i64 = 1_760_000_000_000;

    fn cp_lexical_join(base: &str, rel: &[&str]) -> PathBuf {
        let sep = if base.contains('\\') { "\\" } else { "/" };
        let trimmed = base.trim_end_matches(['/', '\\']);
        PathBuf::from(format!("{trimmed}{sep}{}", rel.join(sep)))
    }

    fn cp_files() -> BTreeMap<PathBuf, Vec<u8>> {
        let mut files = BTreeMap::new();
        files.insert(
            cp_lexical_join(CP_INSTALL, &["DeepSeek Harness.exe"]),
            b"mz".to_vec(),
        );
        files.insert(
            cp_lexical_join(CP_INSTALL, &["resources", "app-update.yml"]),
            b"provider: generic\n".to_vec(),
        );
        files.insert(
            cp_lexical_join(CP_PROFILE, &["package.json"]),
            b"{}".to_vec(),
        );
        files
    }

    fn cp_host_process() -> DshProcess {
        DshProcess {
            pid: CP_PID,
            exe: cp_lexical_join(CP_INSTALL, &["DeepSeek Harness.exe"]),
            cmdline: vec![
                cp_lexical_join(CP_INSTALL, &["DeepSeek Harness.exe"])
                    .to_string_lossy()
                    .into_owned(),
                "--expose-internals".into(),
                cp_lexical_join(
                    CP_INSTALL,
                    &[
                        "resources",
                        "app.asar",
                        "dsh",
                        "node_modules",
                        "@deepseek-ai",
                        "dsh-desktop-host",
                        "lib",
                        "index.js",
                    ],
                )
                .to_string_lossy()
                .into_owned(),
                cp_lexical_join(CP_INSTALL, &["resources", "app.asar", "dsh"])
                    .to_string_lossy()
                    .into_owned(),
                CP_PROFILE.into(),
            ],
            started_at_ms: CP_STARTED_AT,
        }
    }

    struct CpFiles(BTreeMap<PathBuf, Vec<u8>>, BTreeSet<PathBuf>);

    impl DshFileSource for CpFiles {
        fn exists(&self, path: &Path) -> bool {
            self.0.contains_key(path) || self.1.contains(path)
        }
        fn list_dir(&self, path: &Path) -> Vec<PathBuf> {
            let dir = path
                .to_string_lossy()
                .trim_end_matches(['/', '\\'])
                .to_string();
            let mut out: Vec<PathBuf> = self
                .0
                .keys()
                .chain(self.1.iter())
                .filter(|p| {
                    p.to_string_lossy()
                        .rsplit_once(['/', '\\'])
                        .map(|(parent, _)| parent == dir)
                        .unwrap_or(false)
                })
                .cloned()
                .collect();
            out.sort();
            out.dedup();
            out
        }
        fn read_small(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, DshError> {
            match self.0.get(path) {
                Some(bytes) if bytes.len() as u64 <= max_bytes => Ok(bytes.clone()),
                _ => Err(DshError::new(codes::UNSUPPORTED, "cp fixture: no file")),
            }
        }
        fn is_readable_regular_file(&self, path: &Path) -> bool {
            self.0.contains_key(path)
        }
    }

    struct CpProcs(Option<(DshProcess, DshListener)>);

    impl DshProcessSource for CpProcs {
        fn list_processes(&self) -> Vec<DshProcess> {
            self.0.iter().map(|(p, _)| p.clone()).collect()
        }
        fn listening_sockets(&self) -> Vec<DshListener> {
            self.0.iter().map(|(_, l)| l.clone()).collect()
        }
    }

    struct CpNoCommands;
    impl DshCommandRunner for CpNoCommands {
        fn run(&self, _: &CommandRequest) -> Result<RunOutcome, DshError> {
            panic!("control_peer_state 直测不得执行宿主命令")
        }
    }

    struct CpClock;
    impl DshClock for CpClock {
        fn now_ms(&self) -> i64 {
            CP_STARTED_AT
        }
    }

    /// 构造真实授权域（唯一 tempdir 状态目录）：`with_host=false` 时进程/监听
    /// 缺席（安装与 profile 仍在——current_host 按未运行如实失败）。
    fn cp_service(with_host: bool) -> DshAuthService {
        // 目录名必须逐调用唯一（并行单测各持一个授权域；共享目录会让
        // load_or_create_key 的 tmp+chmod+rename 互相踩踏）。
        static CP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = format!(
            "cp-fixture-{}-{}",
            std::process::id(),
            CP_SEQ.fetch_add(1, Ordering::SeqCst)
        );
        let tuvis_home = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&tuvis_home).expect("cp fixture state root");
        let env = DshEnvironment {
            home: PathBuf::from(CP_HOME),
            tuvis_home,
            dsh_state_dir: None,
            processes: Arc::new(CpProcs(with_host.then(|| {
                (
                    cp_host_process(),
                    DshListener {
                        owner_pid: CP_PID,
                        authority: "127.0.0.1:5214".into(),
                    },
                )
            }))),
            files: Arc::new(CpFiles(cp_files(), {
                let mut dirs = BTreeSet::new();
                dirs.insert(PathBuf::from(CP_PROFILE));
                dirs.insert(PathBuf::from(CP_INSTALL));
                dirs
            })),
            runner: Arc::new(CpNoCommands),
            clock: Arc::new(CpClock),
            environments: Arc::new(TestUnobservableEnvironments),
        };
        DshAuthService::new(env).expect("cp fixture service")
    }

    /// 账本塞一条 Bound 绑定（真实账本行，非探针）。
    fn cp_seed_bound(svc: &DshAuthService, binding_id: &str) {
        svc.ledger.lock().unwrap().bindings.push(StoredBinding {
            binding_id: binding_id.to_string(),
            public_key_spki: b64url_encode(b"cp-fixture-spki"),
            revoked_key_spki: String::new(),
            executable: cp_lexical_join(CP_INSTALL, &["DeepSeek Harness.exe"])
                .to_string_lossy()
                .into_owned(),
            pid: CP_PID,
            started_at_ms: CP_STARTED_AT,
            profile_dir: CP_PROFILE.to_string(),
            created_at_ms: CP_STARTED_AT + 1,
            state: StoredBindingState::Bound,
        });
    }

    /// 按给定代际交付句柄 + 挂闸门（真实 Inner 槽位）。
    fn cp_deliver_peer(svc: &DshAuthService, binding_id: &str, host: &LocalHost, epoch: u64) {
        let peer = AuthorizedPeer::with_authority(
            host.host_id.clone(),
            vec![1, 2, 3],
            host.authority.clone(),
            epoch,
        );
        let gate = PeerGate::new();
        let mut inner = svc.inner.lock().unwrap();
        inner.peers.insert(binding_id.to_string(), peer);
        inner.gates.insert(binding_id.to_string(), gate);
    }

    fn cp_current_host(svc: &DshAuthService) -> LocalHost {
        svc.current_host().expect("cp fixture 宿主在跑")
    }

    #[test]
    fn control_peer_state_without_binding_needs_desktop_attention() {
        let svc = cp_service(true);
        assert_eq!(
            svc.control_peer_state(),
            ControlPeerState::NotAuthorized,
            "无 Bound 绑定 = 请在电脑上检查所选实例"
        );
        assert!(svc.peer_for_control_blocking().is_none());
    }

    #[test]
    fn control_peer_state_with_missing_peer_slot_is_recovering() {
        let svc = cp_service(true);
        cp_seed_bound(&svc, "cp-b1");
        assert_eq!(
            svc.control_peer_state(),
            ControlPeerState::Recovering,
            "绑定在场、句柄未交付 = 交付在途（恢复中）"
        );
        assert!(svc.peer_for_control_blocking().is_none());
    }

    #[test]
    fn control_peer_state_current_generation_is_ready_and_parity_holds() {
        let svc = cp_service(true);
        cp_seed_bound(&svc, "cp-b1");
        let host = cp_current_host(&svc);
        cp_deliver_peer(&svc, "cp-b1", &host, host.epoch);
        assert_eq!(svc.control_peer_state(), ControlPeerState::Ready);
        assert!(
            svc.peer_for_control_blocking().is_some(),
            "Ready ⟺ peer_for_control 有句柄（两判定链同源）"
        );
    }

    #[test]
    fn control_peer_state_stale_generation_is_recovering_and_parity_holds() {
        let svc = cp_service(true);
        cp_seed_bound(&svc, "cp-b1");
        let host = cp_current_host(&svc);
        // 旧代际句柄（宿主重启换代后、重交付前的形态）。
        cp_deliver_peer(&svc, "cp-b1", &host, host.epoch + 1);
        assert_eq!(
            svc.control_peer_state(),
            ControlPeerState::Recovering,
            "旧代际句柄 = 正在恢复连接（活体跟踪拍会重交付）"
        );
        assert!(
            svc.peer_for_control_blocking().is_none(),
            "Recovering ⟺ peer_for_control 无句柄（消费面失效不回退）"
        );
    }

    #[test]
    fn control_peer_state_revoked_gate_needs_desktop_attention() {
        let svc = cp_service(true);
        cp_seed_bound(&svc, "cp-b1");
        let host = cp_current_host(&svc);
        cp_deliver_peer(&svc, "cp-b1", &host, host.epoch);
        svc.inner.lock().unwrap().gates["cp-b1"].revoke();
        assert_eq!(
            svc.control_peer_state(),
            ControlPeerState::NotAuthorized,
            "闸门已撤 = 请在电脑上检查所选实例（不是暂态恢复）"
        );
        assert!(svc.peer_for_control_blocking().is_none());
    }

    #[test]
    fn control_peer_state_without_a_provable_host_needs_desktop_attention() {
        let svc = cp_service(false);
        cp_seed_bound(&svc, "cp-b1");
        // 句柄在场（宿主刚刚退出时的形态）——排除「句柄未交付」的恢复中分支，
        // 让本用例只钉「当代宿主不可证 → NotAuthorized」这一因。
        cp_deliver_peer(
            &svc,
            "cp-b1",
            &LocalHost {
                host_id: "cp-host".into(),
                platform: "macos".into(),
                profile_dir: Default::default(),
                executable: PathBuf::from(CP_INSTALL).join("DeepSeek Harness.exe"),
                pid: CP_PID,
                started_at_ms: CP_STARTED_AT,
                epoch: 1,
                authority: "127.0.0.1:5214".into(),
                package_sha256: String::new(),
            },
            1,
        );
        assert!(svc.current_host().is_err(), "前置：宿主不可证");
        assert_eq!(
            svc.control_peer_state(),
            ControlPeerState::NotAuthorized,
            "当代宿主不可证 = 请在电脑上检查所选实例"
        );
        assert!(svc.peer_for_control_blocking().is_none());
    }
}
