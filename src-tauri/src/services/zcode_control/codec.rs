//! 官方二进制 RPC 编解码与可靠收帧（生产计划 T2.1）。
//!
//! # 证据与边界
//!
//! wire 形状钉在固定官方源码 `zai-org/ZCode @ 3c5b2276` 与 B 工作包参照实现
//! `scripts/zcode-bridge-probe/sessionBridge.ts`（E2）：
//!
//! - 每条 Channel 消息 = `serialize(header) + serialize(body)`；元素格式为
//!   `[1 字节类型标签][VQL 长度][数据]`。标签：`Undefined=0 / String=1 /
//!   Buffer=2 / VSBuffer=3 / Array=4 / Object=5(JSON) / Int=6(VQL)`
//!   （`packages/rpc/src/serialization.ts`）。本实现是官方序列化的**最小往返子集**：
//!   出站只用 Undefined / String / Array / Object / Int，入站额外容忍 Buffer/VSBuffer
//!   （按 UTF-8 文本读，与参照实现一致）。
//! - 请求 header = `[RequestType, id, channelName, methodName]`，body = 参数**数组**；
//!   事件订阅 header = `[EventListen, id, channelName, eventName]`，body = 原始 arg
//!   （不包数组）；应答 header = `[ResponseType, id]`；Initialize 的 header 是
//!   `[200]`，body 为 `undefined` → 6 字节 `04 01 06 C8 01 00`。
//! - rpc-frame = Base64(Channel 字节) + CRC32(IEEE) + 物理 seq/messageSeq/分片字段
//!   （`shared/web-remote-control-rpc-transport.ts`）。物理信封（`{type:"data",
//!   payload, client_ts, server_ts}` 的精确 UTF-8 字节）上限 1,048,576 字节；
//!   **出站只发单分片**，超预算在构造帧之前以 `payload_too_large` 拒绝，绝不静默截断。
//!   逻辑消息上限（官方 `maxMessageBytes`）为 16 MiB，由多分片承载。
//! - rpc-frame-ack 只表示「peer 受理了该物理帧」，**不是** Channel 业务应答：
//!   ACK 不等于业务成功，业务结果必须来自 `201/202` 的 [`ChannelMessage`]。
//!
//! # 数值语义（与参照实现逐字节对齐）
//!
//! 参照实现把「整数」一律用 32 位 VQL 写出（`value >>> 0`），超过 32 位的整数会被
//! 截断；本实现照做（`as u32`），保证对拍字节一致。非整数走 Object(JSON) 回退。
//! `Value::Null` 作为 [`encode_listen`] 的 body 时表示「arg 缺省」（Undefined 标签 0），
//! 嵌套 null 仍按 JSON `null` 编码。
//!
//! # 收帧预算（两个上限分工：物理帧 ≠ 逻辑消息）
//!
//! - **物理信封**（单条 WS 帧）1,048,576 字节；**逻辑 Channel 消息**（多分片重组后）
//!   16,777,216 字节（官方 `maxMessageBytes`），单条消息最多 64 个分片（官方
//!   `maxFragments`）。分片存在的唯一理由就是逻辑消息大于物理帧，因此
//!   [`decode_channel`] 只受逻辑上限约束。
//! - 并发重组 32 个 messageSeq、同时在途分片 1024 个、已完成消息保留至多 1024 条
//!   分片指纹（FIFO 淘汰）、暂存总字节 16,777,216、重组 TTL 10,000ms（从该
//!   messageSeq 首片到达起算）。
//! - **不按来帧声明的长度分配**：数组解码按实际元素逐个增长（不 `with_capacity`
//!   声明值）；整条消息组装前先核对 `stagedBytes == messageBytes`（与官方组建器
//!   同序），声明值只做预算校验。
//! - 超限、冲突或 TTL 到期一律返回冻结码并丢弃该重组（不交付任何字节），调用方
//!   必须重同步，不得在吞掉错误后继续宣告投影新鲜。
//!
//! 错误只含冻结码（[`codes`]），不携带任何原始字节/正文/身份值。

use std::collections::{HashMap, VecDeque};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::types::{codes, ZResult, ZcodeError};

/// Channel 方法调用请求类型（`channels.shared.ts` RequestType.Promise）。
pub const REQUEST_TYPE_PROMISE: u32 = 100;
/// Channel 事件订阅请求类型（RequestType.EventListen）。
pub const REQUEST_TYPE_EVENT_LISTEN: u32 = 102;
/// 桥 ChannelServer 就绪信号（ResponseType.Initialize）。
pub const RESPONSE_TYPE_INITIALIZE: u32 = 200;
/// Promise 成功应答。
pub const RESPONSE_TYPE_PROMISE_SUCCESS: u32 = 201;
/// Promise 失败应答。
pub const RESPONSE_TYPE_PROMISE_ERROR: u32 = 202;
/// 事件推送（EventFire）。
pub const RESPONSE_TYPE_EVENT_FIRE: u32 = 204;

/// 物理信封上限（官方 `maxPhysicalFrameBytes`，1 MiB）：只约束**单条 WS 帧**的信封字节。
pub const MAX_PHYSICAL_FRAME_BYTES: usize = 1_048_576;
/// 逻辑 Channel 消息上限（官方 `maxMessageBytes`，16 MiB）：多分片重组后的消息字节。
pub const MAX_CHANNEL_MESSAGE_BYTES: usize = 16_777_216;
/// 单消息最大分片数（官方 `maxFragments`）。
pub const MAX_FRAGMENTS_PER_MESSAGE: usize = 64;
/// 最大并发重组消息数（探针既有限额）。
pub const MAX_CONCURRENT_ASSEMBLIES: usize = 32;
/// 同时在途分片 / 去重指纹的暂存上限（探针既有限额）。
pub const MAX_STAGED_FRAMES: usize = 1024;
/// 入站暂存总字节（Tuvis 本地限额）。
pub const MAX_TOTAL_CACHED_BYTES: usize = 16_777_216;
/// 重组存活时间（Tuvis 本地限额，毫秒）。
pub const ASSEMBLY_TTL_MS: u64 = 10_000;

/// 官方 JSON 时间戳上界（`Number.MAX_SAFE_INTEGER`，用于信封字节预算的最坏情况）。
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
/// 序列化/反序列化递归深度上限（防恶意嵌套）。
const MAX_NESTING_DEPTH: usize = 64;

fn invalid() -> ZcodeError {
    ZcodeError::of(codes::UNSUPPORTED_PROTOCOL)
}

fn too_large() -> ZcodeError {
    ZcodeError::of(codes::PAYLOAD_TOO_LARGE)
}

fn stale_identity() -> ZcodeError {
    ZcodeError::of(codes::STALE_GENERATION)
}

// ---------------------------------------------------------------------------
// rpc-frame 身份
// ---------------------------------------------------------------------------

/// rpc-frame 身份（wire camelCase）：只有 bridge ready 回显过的字段才输出。
///
/// `bridge_generation` / `recovery_id` 是**通道代际身份**，只按 ready 回显原样携带；
/// workspaceKey/taskId 属于桥映射（T2.2），不是 rpc-frame identity。对端按严格相等
/// 判帧身份（含 `undefined`），因此缺省字段必须保持缺席，绝不虚构默认值。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcIdentity {
    /// 桥会话 id（ready 回显，非空）。
    pub bridge_session_id: String,
    /// 桥通道代际（仅 ready 声明时存在）。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub bridge_generation: Option<u64>,
    /// 恢复 id（仅 ready 声明时存在）。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub recovery_id: Option<String>,
}

impl RpcIdentity {
    /// 只声明 bridgeSessionId 的身份（其它字段保持缺席）。
    pub fn new(bridge_session_id: impl Into<String>) -> Self {
        Self {
            bridge_session_id: bridge_session_id.into(),
            bridge_generation: None,
            recovery_id: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Channel 消息
// ---------------------------------------------------------------------------

/// 解码后的 Channel 消息（请求或应答共用）。
///
/// `id` 只对应答/事件有值；Initialize(200) 的 header 是单元素数组 → `None`。
/// `body` 缺省（body 为 undefined）时映射为 [`Value::Null`]。
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelMessage {
    /// 消息类型（100/102/200/201/202/204）。
    pub message_type: u32,
    /// Channel 请求 id（应答/事件路由用）。
    pub id: Option<u32>,
    /// 反序列化后的 body。
    pub body: Value,
}

// ---------------------------------------------------------------------------
// 序列化（serialization.ts 最小往返子集）
// ---------------------------------------------------------------------------

/// VQL（7 bit 小端变长）写出。
fn write_vql(out: &mut Vec<u8>, value: u32) {
    if value == 0 {
        out.push(0);
        return;
    }
    let mut v = value;
    loop {
        let mut byte = (v & 0x7f) as u8;
        v >>= 7;
        if v > 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if v == 0 {
            return;
        }
    }
}

/// 按官方类型标签序列化一个值（深度受限，字符串/JSON 长度以 u32 VQL 表示）。
fn write_value(out: &mut Vec<u8>, value: &Value, depth: usize) -> ZResult<()> {
    if depth > MAX_NESTING_DEPTH {
        return Err(too_large());
    }
    match value {
        Value::Number(number) => {
            if let Some(i) = number.as_i64() {
                // 与参照实现一致：整数走 Int 标签，超出 32 位按 `>>> 0` 截断。
                out.push(6);
                write_vql(out, i as u32);
            } else if let Some(u) = number.as_u64() {
                out.push(6);
                write_vql(out, u as u32);
            } else {
                // 非整数走 Object(JSON) 回退。
                write_json_value(out, value)?;
            }
        }
        Value::String(text) => {
            let len = u32::try_from(text.len()).map_err(|_| too_large())?;
            out.push(1);
            write_vql(out, len);
            out.extend_from_slice(text.as_bytes());
        }
        Value::Array(items) => {
            let len = u32::try_from(items.len()).map_err(|_| too_large())?;
            out.push(4);
            write_vql(out, len);
            for item in items {
                write_value(out, item, depth + 1)?;
            }
        }
        // Undefined 在 serde_json 中不可表达；出站不产生该标签（body 缺省由
        // encode_listen 显式写出）。
        Value::Null | Value::Bool(_) | Value::Object(_) => write_json_value(out, value)?,
    }
    Ok(())
}

/// Object(JSON) 回退：紧凑 JSON（serde_json 默认不转义非 ASCII，与 `JSON.stringify` 一致）。
fn write_json_value(out: &mut Vec<u8>, value: &Value) -> ZResult<()> {
    let text = serde_json::to_vec(value).map_err(|_| invalid())?;
    let len = u32::try_from(text.len()).map_err(|_| too_large())?;
    out.push(5);
    write_vql(out, len);
    out.extend_from_slice(&text);
    Ok(())
}

/// 组装 header + body，并在返回前校验物理信封预算（超预算在发帧前拒绝）。
fn encode_message(header: &[Value], body: &Value) -> ZResult<Vec<u8>> {
    let mut out = Vec::new();
    write_value(&mut out, &Value::Array(header.to_vec()), 0)?;
    write_value(&mut out, body, 0)?;
    if out.len() > MAX_PHYSICAL_FRAME_BYTES {
        return Err(too_large());
    }
    Ok(out)
}

/// 编码一次 Channel 方法调用（RequestType.Promise=100；body 是参数**数组**）。
pub fn encode_call(id: u32, channel: &str, method: &str, args: Vec<Value>) -> ZResult<Vec<u8>> {
    let header = vec![
        Value::from(REQUEST_TYPE_PROMISE),
        Value::from(id),
        Value::from(channel),
        Value::from(method),
    ];
    encode_message(&header, &Value::Array(args))
}

/// 编码一次 Channel 事件订阅（EventListen=102；body 是原始 arg，不包数组）。
///
/// `arg = Value::Null` 表示「arg 缺省」（Undefined 标签 0），对齐参照实现的
/// `channel.listen(name)` 无参形态。
pub fn encode_listen(id: u32, channel: &str, event: &str, arg: Value) -> ZResult<Vec<u8>> {
    let header = vec![
        Value::from(REQUEST_TYPE_EVENT_LISTEN),
        Value::from(id),
        Value::from(channel),
        Value::from(event),
    ];
    let mut out = Vec::new();
    write_value(&mut out, &Value::Array(header), 0)?;
    if arg.is_null() {
        out.push(0);
    } else {
        write_value(&mut out, &arg, 0)?;
    }
    if out.len() > MAX_PHYSICAL_FRAME_BYTES {
        return Err(too_large());
    }
    Ok(out)
}

/// 测试专用：编码一条 201 应答（header `[201, id]` + body）。
///
/// 生产侧是客户端，**从不发送**应答；本函数只为 T8.1 回环假服务合成官方方向的
/// 业务应答。复用同一个私有 [`encode_message`]，保证假体字节与生产解码面同源
/// （不另写一套编码，避免夹具与生产编码漂移）。
#[cfg(test)]
pub(crate) fn encode_response_for_test(id: u32, body: Value) -> ZResult<Vec<u8>> {
    let header = vec![Value::from(RESPONSE_TYPE_PROMISE_SUCCESS), Value::from(id)];
    encode_message(&header, &body)
}

// ---------------------------------------------------------------------------
// 反序列化
// ---------------------------------------------------------------------------

/// 字节游标（所有读取都做边界检查，绝不按声明长度无界分配）。
struct Decoder<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.pos)
    }

    fn read_byte(&mut self) -> ZResult<u8> {
        let byte = *self.bytes.get(self.pos).ok_or_else(invalid)?;
        self.pos += 1;
        Ok(byte)
    }

    fn read_vql(&mut self) -> ZResult<u32> {
        let mut value: u64 = 0;
        let mut shift = 0u32;
        for _ in 0..5 {
            let byte = self.read_byte()?;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return u32::try_from(value).map_err(|_| invalid());
            }
            shift += 7;
        }
        Err(invalid())
    }

    fn read_slice(&mut self, len: usize) -> ZResult<&'a [u8]> {
        if len > self.remaining() {
            return Err(invalid());
        }
        let slice = &self.bytes[self.pos..self.pos + len];
        self.pos += len;
        Ok(slice)
    }

    fn read_value(&mut self, depth: usize) -> ZResult<Value> {
        if depth > MAX_NESTING_DEPTH {
            return Err(invalid());
        }
        let tag = self.read_byte()?;
        match tag {
            0 => Ok(Value::Null),
            6 => Ok(Value::from(u64::from(self.read_vql()?))),
            1 | 2 | 3 | 5 => {
                let len = self.read_vql()? as usize;
                let slice = self.read_slice(len)?;
                if tag == 5 {
                    serde_json::from_slice(slice).map_err(|_| invalid())
                } else {
                    let text = std::str::from_utf8(slice).map_err(|_| invalid())?;
                    Ok(Value::String(text.to_owned()))
                }
            }
            4 => {
                let len = self.read_vql()? as usize;
                // 每个元素至少 1 字节：声明长度不可能超过剩余字节。
                if len > self.remaining() {
                    return Err(invalid());
                }
                // 不按声明长度预分配：声明可被放大数万倍（嵌套帧每层都声明
                // ≈剩余字节数）。增长只跟随实际读取到的元素，受输入字节数约束。
                let mut items = Vec::new();
                for _ in 0..len {
                    items.push(self.read_value(depth + 1)?);
                }
                Ok(Value::Array(items))
            }
            _ => Err(invalid()),
        }
    }
}

/// 解码一条 Channel 消息（请求或应答）；形状不符返回 `unsupported_protocol`。
///
/// 大小上限是**逻辑消息** [`MAX_CHANNEL_MESSAGE_BYTES`]（官方 `maxMessageBytes`，
/// 16 MiB），不是物理帧 1 MiB：多分片重组后的合法大消息必须能解码。
pub fn decode_channel(bytes: &[u8]) -> ZResult<ChannelMessage> {
    if bytes.is_empty() {
        return Err(invalid());
    }
    if bytes.len() > MAX_CHANNEL_MESSAGE_BYTES {
        return Err(too_large());
    }
    let mut decoder = Decoder::new(bytes);
    let header = decoder.read_value(0)?;
    let header = header.as_array().ok_or_else(invalid)?;
    if header.is_empty() {
        return Err(invalid());
    }
    let message_type = header[0]
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(invalid)?;
    let id = match header.get(1) {
        None | Some(Value::Null) => None,
        Some(value) => match value.as_u64() {
            Some(raw) => Some(u32::try_from(raw).map_err(|_| invalid())?),
            None => None,
        },
    };
    // body 缺省（header-only）映射为 Null；body 存在但截断则报错。
    let body = if decoder.remaining() == 0 {
        Value::Null
    } else {
        decoder.read_value(0)?
    };
    Ok(ChannelMessage {
        message_type,
        id,
        body,
    })
}

// ---------------------------------------------------------------------------
// CRC32（IEEE，与官方 crc32WireBytes 同语义）
// ---------------------------------------------------------------------------

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                0xedb8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
    }
    crc ^ 0xffff_ffff
}

pub(crate) fn crc32_hex(bytes: &[u8]) -> String {
    format!("{:08x}", crc32(bytes))
}

// ---------------------------------------------------------------------------
// rpc-frame 出站（单分片）
// ---------------------------------------------------------------------------

fn insert_identity(payload: &mut Map<String, Value>, identity: &RpcIdentity) {
    payload.insert(
        "bridgeSessionId".to_owned(),
        Value::from(identity.bridge_session_id.clone()),
    );
    if let Some(generation) = identity.bridge_generation {
        payload.insert("bridgeGeneration".to_owned(), Value::from(generation));
    }
    if let Some(recovery_id) = &identity.recovery_id {
        payload.insert("recoveryId".to_owned(), Value::from(recovery_id.clone()));
    }
}

/// 编码一条单分片 rpc-frame；信封超 1 MiB 或消息为空 → 冻结码拒绝（绝不截断）。
pub fn encode_frame(
    identity: &RpcIdentity,
    seq: u64,
    message_seq: u64,
    bytes: &[u8],
) -> ZResult<Value> {
    if identity.bridge_session_id.trim().is_empty()
        || identity
            .recovery_id
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
        || seq == 0
        || message_seq == 0
        || bytes.is_empty()
    {
        return Err(invalid());
    }
    if bytes.len() > MAX_PHYSICAL_FRAME_BYTES {
        return Err(too_large());
    }
    let mut payload = Map::new();
    payload.insert("zcode_type".to_owned(), Value::from("rpc-frame"));
    insert_identity(&mut payload, identity);
    payload.insert("seq".to_owned(), Value::from(seq));
    payload.insert("messageSeq".to_owned(), Value::from(message_seq));
    payload.insert("fragmentIndex".to_owned(), Value::from(0u64));
    payload.insert("fragmentCount".to_owned(), Value::from(1u64));
    payload.insert("messageBytes".to_owned(), Value::from(bytes.len() as u64));
    let mut checksum = Map::new();
    checksum.insert("algorithm".to_owned(), Value::from("crc32"));
    checksum.insert("value".to_owned(), Value::from(crc32_hex(bytes)));
    payload.insert("checksum".to_owned(), Value::Object(checksum));
    payload.insert(
        "dataBase64".to_owned(),
        Value::from(base64::engine::general_purpose::STANDARD.encode(bytes)),
    );
    let payload = Value::Object(payload);

    // 物理信封预算按官方最坏情况计量（同时带 client_ts/server_ts 上界）。
    let envelope = serde_json::json!({
        "type": "data",
        "payload": payload,
        "client_ts": MAX_SAFE_INTEGER,
        "server_ts": MAX_SAFE_INTEGER,
    });
    let envelope_len = serde_json::to_vec(&envelope).map_err(|_| invalid())?.len();
    if envelope_len > MAX_PHYSICAL_FRAME_BYTES {
        return Err(too_large());
    }
    Ok(payload)
}

/// 编码接收侧 rpc-frame-ack（只回显 identity 声明过的字段）。
pub fn encode_ack(identity: &RpcIdentity, ack_message_seq: u64) -> Value {
    let mut payload = Map::new();
    payload.insert("zcode_type".to_owned(), Value::from("rpc-frame-ack"));
    insert_identity(&mut payload, identity);
    payload.insert("ackMessageSeq".to_owned(), Value::from(ack_message_seq));
    Value::Object(payload)
}

// ---------------------------------------------------------------------------
// 入站重组
// ---------------------------------------------------------------------------

/// 收帧预算（Tuvis 本地限额；生产用 [`FrameLimits::default`]）。
#[derive(Debug, Clone, Copy)]
pub struct FrameLimits {
    /// 并发重组消息上限。
    pub max_concurrent_assemblies: usize,
    /// 同时在途分片 / 去重指纹上限。
    pub max_staged_frames: usize,
    /// 暂存总字节上限。
    pub max_total_bytes: usize,
    /// 重组 TTL（毫秒）。
    pub assembly_ttl_ms: u64,
}

impl Default for FrameLimits {
    fn default() -> Self {
        Self {
            max_concurrent_assemblies: MAX_CONCURRENT_ASSEMBLIES,
            max_staged_frames: MAX_STAGED_FRAMES,
            max_total_bytes: MAX_TOTAL_CACHED_BYTES,
            assembly_ttl_ms: ASSEMBLY_TTL_MS,
        }
    }
}

/// 已解出的分片（只保留实际字节，不保留整段 base64）。
struct FragmentChunk {
    fingerprint: String,
    bytes: Vec<u8>,
}

/// 单个 messageSeq 的重组状态。
struct Assembly {
    message_seq: u64,
    fragment_count: usize,
    message_bytes: usize,
    checksum: String,
    first_seen_ms: u64,
    chunks: Vec<Option<FragmentChunk>>,
    received: usize,
    staged_bytes: usize,
}

/// 已校验的入站帧（身份/预算/CRC 指纹均已就绪）。
struct ParsedFrame {
    message_seq: u64,
    fragment_index: usize,
    fragment_count: usize,
    message_bytes: usize,
    checksum: String,
    chunk: Vec<u8>,
    fingerprint: String,
}

fn required_str<'a>(object: &'a Map<String, Value>, key: &str) -> ZResult<&'a str> {
    object.get(key).and_then(Value::as_str).ok_or_else(invalid)
}

fn required_u64(object: &Map<String, Value>, key: &str) -> ZResult<u64> {
    object.get(key).and_then(Value::as_u64).ok_or_else(invalid)
}

fn optional_u64(object: &Map<String, Value>, key: &str) -> ZResult<Option<u64>> {
    match object.get(key) {
        None => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or_else(invalid),
    }
}

/// 可靠的被动 rpc-frame 组装器（每方向一个实例）。
///
/// 语义：身份严格相等绑定；分片按 `(messageSeq, fragmentIndex)` 暂存（允许同一消息
/// 的分片乱序到达）；重复分片（同指纹）只确认不重复交付；元数据/CRC 冲突即拒绝并
/// 丢弃该重组；TTL 到期或超预算同样丢弃并**返回错误要求重同步**（不静默清理，
/// 也不在本帧交付任何字节）。与官方组建器一致：只有实际暂存字节数
/// 等于声明 `messageBytes` 时才组装整条消息。**坏帧永不交付**。
pub struct FrameAssembler {
    identity: RpcIdentity,
    limits: FrameLimits,
    assemblies: HashMap<u64, Assembly>,
    settled: HashMap<(u64, u64), String>,
    settled_order: VecDeque<(u64, u64)>,
    in_flight_fragments: usize,
    staged_bytes: usize,
}

impl FrameAssembler {
    /// 以给定身份与生产预算构造。
    pub fn new(identity: RpcIdentity) -> Self {
        Self::with_limits(identity, FrameLimits::default())
    }

    /// 自定义预算（测试与上层本地收紧用）。
    pub(crate) fn with_limits(identity: RpcIdentity, limits: FrameLimits) -> Self {
        Self {
            identity,
            limits,
            assemblies: HashMap::new(),
            settled: HashMap::new(),
            settled_order: VecDeque::new(),
            in_flight_fragments: 0,
            staged_bytes: 0,
        }
    }

    /// 当前并发重组数。
    pub fn active_assemblies(&self) -> usize {
        self.assemblies.len()
    }

    /// 当前暂存字节数。
    pub fn staged_bytes(&self) -> usize {
        self.staged_bytes
    }

    /// 丢弃全部重组与去重指纹（超限/断线后的显式重同步）。
    pub fn reset(&mut self) {
        self.assemblies.clear();
        self.settled.clear();
        self.settled_order.clear();
        self.in_flight_fragments = 0;
        self.staged_bytes = 0;
    }

    fn drop_assembly(&mut self, message_seq: u64) {
        if let Some(assembly) = self.assemblies.remove(&message_seq) {
            self.in_flight_fragments = self.in_flight_fragments.saturating_sub(assembly.received);
            self.staged_bytes = self.staged_bytes.saturating_sub(assembly.staged_bytes);
        }
    }

    /// 清理过期重组；返回是否有重组因 TTL 到期被丢弃（调用方必须重同步）。
    fn expire(&mut self, now_ms: u64) -> bool {
        let ttl = self.limits.assembly_ttl_ms;
        let expired: Vec<u64> = self
            .assemblies
            .iter()
            .filter(|(_, assembly)| now_ms.saturating_sub(assembly.first_seen_ms) >= ttl)
            .map(|(message_seq, _)| *message_seq)
            .collect();
        let dropped = !expired.is_empty();
        for message_seq in expired {
            self.drop_assembly(message_seq);
        }
        dropped
    }

    fn remember_settled(&mut self, key: (u64, u64), fingerprint: String) {
        if self.settled.insert(key, fingerprint).is_none() {
            self.settled_order.push_back(key);
        }
        while self.settled.len() > self.limits.max_staged_frames.max(1) {
            let Some(oldest) = self.settled_order.pop_front() else {
                break;
            };
            self.settled.remove(&oldest);
        }
    }

    /// 校验并解析一帧；任何形状/身份/预算问题都在这里以冻结码拒绝。
    fn parse_frame(&self, frame: &Value) -> ZResult<ParsedFrame> {
        let object = frame.as_object().ok_or_else(invalid)?;
        if required_str(object, "zcode_type")? != "rpc-frame" {
            return Err(invalid());
        }
        let raw_len = serde_json::to_vec(frame).map_err(|_| invalid())?.len();
        if raw_len > MAX_PHYSICAL_FRAME_BYTES {
            return Err(too_large());
        }

        // 身份严格相等（含字段缺席）。
        let bridge_session_id = required_str(object, "bridgeSessionId")?;
        let bridge_generation = optional_u64(object, "bridgeGeneration")?;
        let recovery_id = match object.get("recoveryId") {
            None => None,
            Some(value) => Some(
                value
                    .as_str()
                    .filter(|text| !text.trim().is_empty())
                    .ok_or_else(invalid)?
                    .to_owned(),
            ),
        };
        if bridge_session_id != self.identity.bridge_session_id
            || bridge_generation != self.identity.bridge_generation
            || recovery_id != self.identity.recovery_id
        {
            return Err(stale_identity());
        }

        let seq = required_u64(object, "seq")?;
        let message_seq = required_u64(object, "messageSeq")?;
        if seq == 0 || message_seq == 0 {
            return Err(invalid());
        }
        let fragment_index =
            usize::try_from(required_u64(object, "fragmentIndex")?).map_err(|_| invalid())?;
        let fragment_count =
            usize::try_from(required_u64(object, "fragmentCount")?).map_err(|_| invalid())?;
        let message_bytes =
            usize::try_from(required_u64(object, "messageBytes")?).map_err(|_| too_large())?;
        if fragment_count == 0 || fragment_index >= fragment_count || fragment_count > message_bytes
        {
            return Err(invalid());
        }
        if fragment_count > MAX_FRAGMENTS_PER_MESSAGE || message_bytes > MAX_CHANNEL_MESSAGE_BYTES {
            return Err(too_large());
        }

        let checksum = object
            .get("checksum")
            .and_then(Value::as_object)
            .ok_or_else(invalid)?;
        if required_str(checksum, "algorithm")? != "crc32" {
            return Err(invalid());
        }
        let checksum = required_str(checksum, "value")?;
        if checksum.len() != 8
            || !checksum
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(invalid());
        }
        let checksum = checksum.to_owned();

        let data = required_str(object, "dataBase64")?;
        if data.is_empty() {
            return Err(invalid());
        }
        // 先做字符串长度硬闸，再解码，避免恶意超长 base64 先分配 buffer。
        if data.len() > MAX_PHYSICAL_FRAME_BYTES {
            return Err(too_large());
        }
        let chunk = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|_| invalid())?;
        if chunk.is_empty() {
            return Err(invalid());
        }
        if chunk.len() > message_bytes {
            return Err(invalid());
        }
        // 规范 base64（重新编码必须逐字符相等）。
        if base64::engine::general_purpose::STANDARD.encode(&chunk) != data {
            return Err(invalid());
        }

        let fingerprint = format!(
            "{message_seq}:{fragment_index}:{fragment_count}:{message_bytes}:{checksum}:{}:{}",
            chunk.len(),
            crc32_hex(&chunk)
        );
        Ok(ParsedFrame {
            message_seq,
            fragment_index,
            fragment_count,
            message_bytes,
            checksum,
            chunk,
            fingerprint,
        })
    }

    /// 接受一条入站 data payload 的 rpc-frame，返回本次完整交付的消息字节。
    ///
    /// 未完成/重复帧返回空 vec；坏帧/超限/身份不符/TTL 到期返回冻结码
    /// （**不交付任何字节**）。TTL 到期属于终态：调用方必须 [`reset`](Self::reset)
    /// 后重新同步，不得在吞掉错误后继续宣告投影新鲜。
    pub fn accept(&mut self, frame: Value, now_ms: u64) -> ZResult<Vec<Vec<u8>>> {
        if self.expire(now_ms) {
            // 有进行中的重组过期：整条消息已不可恢复，本帧也不组装。
            return Err(invalid());
        }
        let parsed = self.parse_frame(&frame)?;

        let key = (parsed.message_seq, parsed.fragment_index as u64);
        if let Some(known) = self.settled.get(&key) {
            return if *known == parsed.fingerprint {
                // 已完成消息的重复分片：只确认，不重复交付。
                Ok(Vec::new())
            } else {
                Err(invalid())
            };
        }

        let mut conflicting_duplicate = false;
        if let Some(assembly) = self.assemblies.get(&parsed.message_seq) {
            let metadata_conflict = assembly.fragment_count != parsed.fragment_count
                || assembly.message_bytes != parsed.message_bytes
                || assembly.checksum != parsed.checksum;
            if metadata_conflict {
                self.drop_assembly(parsed.message_seq);
                return Err(invalid());
            }
            if let Some(existing) = assembly
                .chunks
                .get(parsed.fragment_index)
                .and_then(Option::as_ref)
            {
                if existing.fingerprint == parsed.fingerprint {
                    return Ok(Vec::new());
                }
                // 同槽不同内容 = 冲突重复：丢弃整条重组，要求重同步。
                conflicting_duplicate = true;
            }
        } else if self.assemblies.len() >= self.limits.max_concurrent_assemblies {
            return Err(too_large());
        }
        if conflicting_duplicate {
            self.drop_assembly(parsed.message_seq);
            return Err(invalid());
        }

        if self.in_flight_fragments + 1 > self.limits.max_staged_frames {
            return Err(too_large());
        }
        if self.staged_bytes + parsed.chunk.len() > self.limits.max_total_bytes {
            return Err(too_large());
        }

        let message_seq = parsed.message_seq;
        let fragment_index = parsed.fragment_index;
        let chunk_len = parsed.chunk.len();
        let complete = {
            let entry = self
                .assemblies
                .entry(message_seq)
                .or_insert_with(|| Assembly {
                    message_seq,
                    fragment_count: parsed.fragment_count,
                    message_bytes: parsed.message_bytes,
                    checksum: parsed.checksum.clone(),
                    first_seen_ms: now_ms,
                    chunks: std::iter::repeat_with(|| None)
                        .take(parsed.fragment_count)
                        .collect(),
                    received: 0,
                    staged_bytes: 0,
                });
            let Some(slot) = entry.chunks.get_mut(fragment_index) else {
                return Err(invalid());
            };
            *slot = Some(FragmentChunk {
                fingerprint: parsed.fingerprint,
                bytes: parsed.chunk,
            });
            entry.received += 1;
            entry.staged_bytes += chunk_len;
            entry.received >= entry.fragment_count
        };
        self.in_flight_fragments += 1;
        self.staged_bytes += chunk_len;
        if !complete {
            return Ok(Vec::new());
        }

        let Some(assembly) = self.assemblies.remove(&message_seq) else {
            return Err(invalid());
        };
        self.in_flight_fragments = self.in_flight_fragments.saturating_sub(assembly.received);
        self.staged_bytes = self.staged_bytes.saturating_sub(assembly.staged_bytes);

        // 与官方组建器同序：先核对实际暂存字节 == 声明 messageBytes，再分配。
        if assembly.staged_bytes != assembly.message_bytes {
            return Err(invalid());
        }
        let mut joined = Vec::with_capacity(assembly.staged_bytes);
        for chunk in &assembly.chunks {
            let Some(chunk) = chunk else {
                return Err(invalid());
            };
            joined.extend_from_slice(&chunk.bytes);
        }
        debug_assert_eq!(joined.len(), assembly.message_bytes);
        if crc32_hex(&joined) != assembly.checksum {
            // 坏 CRC：整条消息丢弃，零交付。
            return Err(invalid());
        }
        for (index, chunk) in assembly.chunks.iter().enumerate() {
            if let Some(chunk) = chunk {
                self.remember_settled(
                    (assembly.message_seq, index as u64),
                    chunk.fingerprint.clone(),
                );
            }
        }
        Ok(vec![joined])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 线程本地分配计量（只用于「恶意声明长度不得放大分配」的回归测试）。
    ///
    /// 测试各自独立线程，`Cell` 计数互不干扰；`System` 承担真实分配，本包装只记账。
    mod test_alloc {
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::cell::Cell;

        thread_local! {
            static CURRENT: Cell<usize> = const { Cell::new(0) };
            static PEAK: Cell<usize> = const { Cell::new(0) };
        }

        pub struct CountingAllocator;

        fn note_alloc(size: usize) {
            CURRENT.with(|current| {
                let next = current.get() + size;
                current.set(next);
                PEAK.with(|peak| {
                    if next > peak.get() {
                        peak.set(next);
                    }
                });
            });
        }

        fn note_dealloc(size: usize) {
            CURRENT.with(|current| current.set(current.get().saturating_sub(size)));
        }

        unsafe impl GlobalAlloc for CountingAllocator {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                let pointer = System.alloc(layout);
                if !pointer.is_null() {
                    note_alloc(layout.size());
                }
                pointer
            }

            unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
                System.dealloc(pointer, layout);
                note_dealloc(layout.size());
            }

            unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
                let next = System.realloc(pointer, layout, new_size);
                if !next.is_null() {
                    note_dealloc(layout.size());
                    note_alloc(new_size);
                }
                next
            }
        }

        #[global_allocator]
        static GLOBAL: CountingAllocator = CountingAllocator;

        /// 清零当前线程的计数（在构造输入之后调用）。
        pub fn reset() {
            CURRENT.with(|current| current.set(0));
            PEAK.with(|peak| peak.set(0));
        }

        /// 当前线程自 [`reset`] 以来的峰值存活分配字节数。
        pub fn peak() -> usize {
            PEAK.with(std::cell::Cell::get)
        }
    }

    /// 夹具在仓库根 `tests/fixtures/`（与 `contract.json` 同目录），编译期嵌入。
    fn fixture() -> Value {
        let raw = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../tests/fixtures/zcode-official-bridge/wire-vectors.json"
        ));
        serde_json::from_str(raw).expect("parse wire-vectors.json")
    }

    fn hex_to_bytes(hex: &str) -> Vec<u8> {
        assert!(hex.len().is_multiple_of(2), "hex length must be even");
        (0..hex.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).expect("hex byte"))
            .collect()
    }

    fn bytes_to_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn named<'a>(root: &'a Value, group: &str, name: &str) -> &'a Value {
        root[group]
            .as_array()
            .unwrap_or_else(|| panic!("group {group}"))
            .iter()
            .find(|item| item["name"] == name)
            .unwrap_or_else(|| panic!("vector {group}/{name}"))
    }

    fn identity_from(vector: &Value) -> RpcIdentity {
        serde_json::from_value(vector["identity"].clone()).expect("identity")
    }

    fn args_from(vector: &Value) -> Vec<Value> {
        vector["args"].as_array().cloned().unwrap_or_default()
    }

    fn corrupt_chunk_base64(frame: &mut Value) {
        let data = frame["dataBase64"].as_str().expect("dataBase64").to_owned();
        let mut bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .expect("base64");
        let last = bytes.last_mut().expect("non-empty chunk");
        *last ^= 0xff;
        frame["dataBase64"] = Value::from(base64::engine::general_purpose::STANDARD.encode(bytes));
    }

    /// 合成一条合法的 Channel 应答消息（header + 可控长度的字符串 body）。
    fn synthetic_channel_message(body_len: usize) -> Vec<u8> {
        let header = json!([RESPONSE_TYPE_PROMISE_SUCCESS, 9u32]);
        let body = Value::String("a".repeat(body_len));
        let mut bytes = Vec::new();
        write_value(&mut bytes, &header, 0).expect("encode header");
        write_value(&mut bytes, &body, 0).expect("encode body");
        bytes
    }

    /// 按官方分片信封形状合成一帧（与 `encode_frame` 同字段，允许指定分片位置）。
    fn synthetic_frame(
        message: &[u8],
        message_seq: u64,
        index: usize,
        count: usize,
        chunk: &[u8],
    ) -> Value {
        let seq = index as u64 + 1;
        let message_bytes = message.len();
        json!({
            "zcode_type": "rpc-frame",
            "bridgeSessionId": "bridge-test-0001",
            "seq": seq,
            "messageSeq": message_seq,
            "fragmentIndex": index,
            "fragmentCount": count,
            "messageBytes": message_bytes,
            "checksum": {"algorithm": "crc32", "value": crc32_hex(message)},
            "dataBase64": base64::engine::general_purpose::STANDARD.encode(chunk),
        })
    }

    /// 恶意嵌套帧：每层数组都声明「剩余字节数」，能通过 `len <= remaining` 守卫。
    fn hostile_nested_frame(target: usize) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(target);
        for _ in 0..MAX_NESTING_DEPTH + 6 {
            let remaining = target - bytes.len();
            bytes.push(4); // Array 标签
            let declared = u32::try_from(remaining - 4).expect("fits u32");
            write_vql(&mut bytes, declared);
        }
        bytes.resize(target, 0);
        bytes
    }

    #[test]
    fn initialize_golden_vector() {
        assert_eq!(
            decode_channel(&[4, 1, 6, 200, 1, 0]).unwrap().message_type,
            200
        );

        let fixture = fixture();
        let bytes = hex_to_bytes(fixture["initialize"]["hex"].as_str().expect("hex"));
        assert_eq!(bytes, vec![0x04, 0x01, 0x06, 0xc8, 0x01, 0x00]);
        let decoded = decode_channel(&bytes).expect("decode initialize");
        assert_eq!(decoded.message_type, RESPONSE_TYPE_INITIALIZE);
        assert_eq!(decoded.id, None);
        assert_eq!(decoded.body, Value::Null);
    }

    #[test]
    fn call_args_are_an_array_not_a_bare_object() {
        let fixture = fixture();
        let vector = named(&fixture, "calls", "send_text");
        let id = vector["id"].as_u64().expect("id") as u32;
        let channel = vector["channel"].as_str().expect("channel");
        let method = vector["method"].as_str().expect("method");
        let bytes = encode_call(id, channel, method, args_from(vector)).expect("encode call");
        assert_eq!(bytes_to_hex(&bytes), vector["hex"].as_str().expect("hex"));
        let decoded = decode_channel(&bytes).expect("decode call");
        assert_eq!(decoded.message_type, REQUEST_TYPE_PROMISE);
        assert_eq!(decoded.id, Some(id));
        assert_eq!(decoded.body, Value::Array(args_from(vector)));

        let decoded_call_body = decode_channel(
            &encode_call(
                7,
                "zcode-agent",
                "sendConversationCommandV4",
                vec![json!({"sessionId": "session_test"})],
            )
            .expect("encode call"),
        )
        .expect("decode call")
        .body;
        assert_eq!(
            decoded_call_body,
            serde_json::json!([{"sessionId":"session_test"}])
        );
        assert!(!decoded_call_body.is_object());
    }

    #[test]
    fn duplicate_fragment_delivered_once_and_bad_crc_rejected() {
        let fixture = fixture();
        let vector = named(&fixture, "fragments", "two_fragments");
        let frames = vector["frames"].as_array().expect("frames").clone();
        assert_eq!(frames.len(), 2, "two-fragment vector");

        let mut assembler = FrameAssembler::new(identity_from(vector));
        let mut assembled_delivery_count = 0;
        for (index, frame) in frames.iter().enumerate() {
            let delivered = assembler.accept(frame.clone(), 0).expect("accept fragment");
            assembled_delivery_count += delivered.len();
            if index == 0 {
                // 重复首片：只确认，不重复暂存，不产生交付。
                assert!(assembler
                    .accept(frame.clone(), 0)
                    .expect("duplicate fragment")
                    .is_empty());
            }
        }
        // 完成后重发全部分片：去重，不再交付。
        for frame in &frames {
            assert!(assembler
                .accept(frame.clone(), 0)
                .expect("settled duplicate")
                .is_empty());
        }

        let mut bad_crc_assembler = FrameAssembler::new(identity_from(vector));
        let mut bad_frames = frames.clone();
        corrupt_chunk_base64(&mut bad_frames[1]);
        let mut bad_crc_delivery_count = 0;
        for frame in &bad_frames {
            match bad_crc_assembler.accept(frame.clone(), 0) {
                Ok(delivered) => bad_crc_delivery_count += delivered.len(),
                Err(error) => assert!(error.is(codes::UNSUPPORTED_PROTOCOL)),
            }
        }

        assert_eq!(assembled_delivery_count, 1);
        assert_eq!(bad_crc_delivery_count, 0);
    }

    #[test]
    fn encode_frames_match_reference_vectors() {
        let fixture = fixture();
        for vector in fixture["frames"].as_array().expect("frames") {
            let identity = identity_from(vector);
            let bytes = hex_to_bytes(vector["messageHex"].as_str().expect("messageHex"));
            let frame = encode_frame(
                &identity,
                vector["seq"].as_u64().expect("seq"),
                vector["messageSeq"].as_u64().expect("messageSeq"),
                &bytes,
            )
            .expect("encode frame");
            assert_eq!(frame, vector["frame"], "frame value: {}", vector["name"]);
            assert_eq!(
                serde_json::to_string(&frame).expect("serialize frame"),
                serde_json::to_string(&vector["frame"]).expect("serialize reference"),
                "frame key order: {}",
                vector["name"]
            );
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(frame["dataBase64"].as_str().expect("dataBase64"))
                .expect("base64");
            assert_eq!(decoded, bytes);
            assert_eq!(
                frame["checksum"]["value"].as_str().expect("checksum"),
                crc32_hex(&bytes)
            );
            assert_eq!(frame["fragmentIndex"], json!(0));
            assert_eq!(frame["fragmentCount"], json!(1));
            assert_eq!(frame["messageBytes"], json!(bytes.len()));
        }
    }

    #[test]
    fn encode_ack_matches_reference_vectors() {
        let fixture = fixture();
        for vector in fixture["acks"].as_array().expect("acks") {
            let identity = identity_from(vector);
            let ack = encode_ack(&identity, vector["ackMessageSeq"].as_u64().expect("seq"));
            assert_eq!(ack, vector["payload"], "ack: {}", vector["name"]);
            assert_eq!(
                serde_json::to_string(&ack).expect("serialize ack"),
                serde_json::to_string(&vector["payload"]).expect("serialize reference"),
                "ack key order: {}",
                vector["name"]
            );
        }
    }

    #[test]
    fn decode_response_vectors() {
        let fixture = fixture();
        for vector in fixture["responses"].as_array().expect("responses") {
            let bytes = hex_to_bytes(vector["hex"].as_str().expect("hex"));
            let decoded = decode_channel(&bytes).expect("decode response");
            assert_eq!(
                u64::from(decoded.message_type),
                vector["responseType"].as_u64().expect("responseType")
            );
            assert_eq!(decoded.id, Some(vector["id"].as_u64().expect("id") as u32));
            assert_eq!(decoded.body, vector["body"], "body: {}", vector["name"]);
        }
    }

    #[test]
    fn unicode_payloads_roundtrip() {
        let fixture = fixture();
        let vector = named(&fixture, "calls", "unicode");
        let bytes = encode_call(
            vector["id"].as_u64().expect("id") as u32,
            vector["channel"].as_str().expect("channel"),
            vector["method"].as_str().expect("method"),
            args_from(vector),
        )
        .expect("encode unicode call");
        assert_eq!(bytes_to_hex(&bytes), vector["hex"].as_str().expect("hex"));
        let decoded = decode_channel(&bytes).expect("decode unicode call");
        assert_eq!(decoded.body, Value::Array(args_from(vector)));
        let text = decoded.body[0]["note"].as_str().expect("note");
        assert!(text.contains("兔维斯"));
        assert!(text.contains("🚀"));
    }

    #[test]
    fn event_listen_body_is_not_wrapped_as_args() {
        let fixture = fixture();
        let vector = named(&fixture, "listens", "conversation_with_arg");
        let arg = vector["arg"].clone();
        let bytes = encode_listen(
            vector["id"].as_u64().expect("id") as u32,
            vector["channel"].as_str().expect("channel"),
            vector["event"].as_str().expect("event"),
            arg.clone(),
        )
        .expect("encode listen");
        assert_eq!(bytes_to_hex(&bytes), vector["hex"].as_str().expect("hex"));
        let decoded = decode_channel(&bytes).expect("decode listen");
        assert_eq!(decoded.message_type, REQUEST_TYPE_EVENT_LISTEN);
        assert_eq!(decoded.body, arg);
        assert_ne!(decoded.body, Value::Array(vec![arg]));
    }

    #[test]
    fn listen_without_arg_encodes_undefined_body() {
        let fixture = fixture();
        let vector = named(&fixture, "listens", "controller_without_arg");
        let bytes = encode_listen(
            vector["id"].as_u64().expect("id") as u32,
            vector["channel"].as_str().expect("channel"),
            vector["event"].as_str().expect("event"),
            Value::Null,
        )
        .expect("encode listen");
        assert_eq!(bytes_to_hex(&bytes), vector["hex"].as_str().expect("hex"));
        assert_eq!(bytes.last(), Some(&0u8));
        let decoded = decode_channel(&bytes).expect("decode listen");
        assert_eq!(decoded.message_type, REQUEST_TYPE_EVENT_LISTEN);
        assert_eq!(decoded.id, Some(3));
        assert_eq!(decoded.body, Value::Null);
    }

    #[test]
    fn truncated_vql_is_rejected() {
        let fixture = fixture();
        let vector = named(&fixture, "calls", "send_text");
        let bytes = hex_to_bytes(vector["hex"].as_str().expect("hex"));
        // 正控：完整字节必须可解，截断才谈得上拒绝。
        assert!(decode_channel(&bytes).is_ok());

        // VQL 续位被截断。
        assert!(decode_channel(&[4, 1, 6, 0x80]).is_err());
        // VQL 超过 5 字节（u32 溢出）。
        assert!(decode_channel(&[4, 1, 6, 0x80, 0x80, 0x80, 0x80, 0x80]).is_err());
        // 字符串声明长度超过剩余字节。
        assert!(decode_channel(&[4, 1, 1, 0x05, 0x61]).is_err());

        // 从方法名/参数 JSON 中部截断。
        for cut in [8usize, 40, bytes.len() / 2, bytes.len() - 3] {
            assert!(decode_channel(&bytes[..cut]).is_err(), "cut={cut}");
        }
    }

    #[test]
    fn decode_channel_rejects_malformed_headers() {
        // 正控：合法 Initialize 必须可解。
        assert!(decode_channel(&[4, 1, 6, 200, 1, 0]).is_ok());
        assert!(decode_channel(&[]).is_err());
        // header 不是数组。
        assert!(decode_channel(&[6, 1]).is_err());
        // 空 header 数组。
        assert!(decode_channel(&[4, 0]).is_err());
        // message_type 不是整数。
        assert!(decode_channel(&[4, 1, 1, 1, b'A', 0]).is_err());
        // 未知类型标签。
        assert!(decode_channel(&[9]).is_err());
        // 1 MiB+1 是物理信封问题，不是解码问题：解码侧只按形状拒绝
        // （全零帧的 header 不是数组），不得报 payload_too_large。
        assert!(decode_channel(&vec![0u8; MAX_PHYSICAL_FRAME_BYTES + 1])
            .unwrap_err()
            .is(codes::UNSUPPORTED_PROTOCOL));
        // 物理上限在物理层（出站分帧）拒绝。
        let identity = RpcIdentity::new("bridge-test-0001");
        assert!(
            encode_frame(&identity, 1, 1, &vec![0u8; MAX_PHYSICAL_FRAME_BYTES + 1])
                .unwrap_err()
                .is(codes::PAYLOAD_TOO_LARGE)
        );
        // 解码侧的上限是**逻辑**消息 16 MiB（多分片重组后的字节数）。
        assert!(decode_channel(&vec![0u8; MAX_CHANNEL_MESSAGE_BYTES + 1])
            .unwrap_err()
            .is(codes::PAYLOAD_TOO_LARGE));
    }

    #[test]
    fn out_of_order_fragments_complete_once() {
        let fixture = fixture();
        let vector = named(&fixture, "fragments", "three_fragments");
        let frames = vector["frames"].as_array().expect("frames").clone();
        let mut assembler = FrameAssembler::new(identity_from(vector));
        let mut delivered = Vec::new();
        for index in [2usize, 0, 1] {
            delivered.extend(
                assembler
                    .accept(frames[index].clone(), 0)
                    .expect("accept out-of-order fragment"),
            );
        }
        assert_eq!(delivered.len(), 1);
        assert_eq!(
            bytes_to_hex(&delivered[0]),
            vector["messageHex"].as_str().expect("messageHex")
        );
        assert_eq!(assembler.active_assemblies(), 0);
        assert_eq!(assembler.staged_bytes(), 0);
    }

    #[test]
    fn missing_fragment_stays_incomplete() {
        let fixture = fixture();
        let vector = named(&fixture, "fragments", "three_fragments");
        let frames = vector["frames"].as_array().expect("frames").clone();
        let mut assembler = FrameAssembler::new(identity_from(vector));
        assert!(assembler.accept(frames[0].clone(), 0).unwrap().is_empty());
        assert!(assembler.accept(frames[2].clone(), 0).unwrap().is_empty());
        assert_eq!(assembler.active_assemblies(), 1);
        assert!(assembler.staged_bytes() > 0);
        // 缺片不会被当作完整消息交付；补充片到达后才交付一次。
        let delivered = assembler.accept(frames[1].clone(), 0).expect("fill gap");
        assert_eq!(delivered.len(), 1);
        assert_eq!(
            bytes_to_hex(&delivered[0]),
            vector["messageHex"].as_str().expect("messageHex")
        );
    }

    #[test]
    fn wrong_identity_frames_are_rejected() {
        let fixture = fixture();
        let vector = named(&fixture, "fragments", "two_fragments");
        let frames = vector["frames"].as_array().expect("frames").clone();
        let mut assembler = FrameAssembler::new(identity_from(vector));

        let mut other_session = frames[0].clone();
        other_session["bridgeSessionId"] = json!("bridge-test-9999");
        assert!(assembler
            .accept(other_session, 0)
            .unwrap_err()
            .is(codes::STALE_GENERATION));

        let mut other_generation = frames[0].clone();
        other_generation["bridgeGeneration"] = json!(99);
        assert!(assembler
            .accept(other_generation, 0)
            .unwrap_err()
            .is(codes::STALE_GENERATION));

        let mut extra_recovery = frames[0].clone();
        extra_recovery["recoveryId"] = json!("recovery-test-0001");
        assert!(assembler
            .accept(extra_recovery, 0)
            .unwrap_err()
            .is(codes::STALE_GENERATION));

        // 错误身份的坏帧不得清空/污染正确身份的重组。
        assert!(assembler
            .accept(frames[0].clone(), 0)
            .expect("correct identity")
            .is_empty());
        assert_eq!(assembler.active_assemblies(), 1);
    }

    #[test]
    fn bad_frames_are_not_delivered() {
        let fixture = fixture();
        let vector = named(&fixture, "fragments", "two_fragments");
        let frames = vector["frames"].as_array().expect("frames").clone();
        let mut assembler = FrameAssembler::new(identity_from(vector));

        // ack 不是帧，不能进入重组，也不能被当成业务成功。
        assert!(assembler
            .accept(encode_ack(&RpcIdentity::new("bridge-test-0001"), 1), 0)
            .unwrap_err()
            .is(codes::UNSUPPORTED_PROTOCOL));
        // 非法 base64。
        let mut bad_base64 = frames[0].clone();
        bad_base64["dataBase64"] = json!("not base64!!");
        assert!(assembler.accept(bad_base64, 0).is_err());
        // 声明长度与实际不符（分片数 > 消息字节）。
        let mut bad_shape = frames[0].clone();
        bad_shape["fragmentCount"] = json!(9999u64);
        assert!(assembler.accept(bad_shape, 0).is_err());
        assert_eq!(assembler.active_assemblies(), 0);
        assert_eq!(assembler.staged_bytes(), 0);
    }

    #[test]
    fn oversized_frames_are_rejected_with_frozen_codes() {
        let fixture = fixture();
        let vector = named(&fixture, "fragments", "two_fragments");
        let frames = vector["frames"].as_array().expect("frames").clone();
        let mut assembler = FrameAssembler::new(identity_from(vector));

        // 声明 4 GiB 消息：只做预算校验，不按声明分配。
        let mut huge_declared = frames[0].clone();
        huge_declared["messageBytes"] = json!(4_294_967_296u64);
        assert!(assembler
            .accept(huge_declared, 0)
            .unwrap_err()
            .is(codes::PAYLOAD_TOO_LARGE));
        assert_eq!(assembler.staged_bytes(), 0);

        // 官方 maxFragments=64：65 片即拒绝。
        let mut too_many_fragments = frames[0].clone();
        too_many_fragments["fragmentCount"] = json!(65u64);
        assert!(assembler
            .accept(too_many_fragments, 0)
            .unwrap_err()
            .is(codes::PAYLOAD_TOO_LARGE));

        // 声明 16 MiB、实际只给 1 字节分片：核对 stagedBytes == messageBytes 之前
        // 不得组装/分配整条消息，直接坏帧拒绝并清空账本。
        let mut mismatched = frames[0].clone();
        mismatched["fragmentCount"] = json!(1u64);
        mismatched["fragmentIndex"] = json!(0u64);
        mismatched["messageBytes"] = json!((MAX_CHANNEL_MESSAGE_BYTES - 1) as u64);
        assert!(assembler
            .accept(mismatched, 0)
            .unwrap_err()
            .is(codes::UNSUPPORTED_PROTOCOL));
        assert_eq!(assembler.active_assemblies(), 0);
        assert_eq!(assembler.staged_bytes(), 0);

        // 超长 base64 在解码前拒绝。
        let mut oversized = frames[0].clone();
        oversized["dataBase64"] = json!("A".repeat(MAX_PHYSICAL_FRAME_BYTES + 1));
        assert!(assembler
            .accept(oversized, 0)
            .unwrap_err()
            .is(codes::PAYLOAD_TOO_LARGE));

        // 出站：空消息/超过物理上限的消息在发帧前拒绝。
        let identity = RpcIdentity::new("bridge-test-0001");
        assert!(encode_frame(&identity, 1, 1, &[]).is_err());
        let big = vec![0u8; MAX_PHYSICAL_FRAME_BYTES];
        assert!(encode_frame(&identity, 1, 1, &big)
            .unwrap_err()
            .is(codes::PAYLOAD_TOO_LARGE));
    }

    #[test]
    fn multi_fragment_message_over_physical_frame_decodes() {
        // 逻辑消息 1.5 MiB > 单帧 1 MiB，但 < 官方 maxMessageBytes 16 MiB。
        let message = synthetic_channel_message(1_500_000);
        assert!(
            message.len() > MAX_PHYSICAL_FRAME_BYTES,
            "message {} must exceed one physical frame",
            message.len()
        );
        assert!(message.len() < MAX_CHANNEL_MESSAGE_BYTES);

        let fragment_count = 3usize;
        let chunk_len = message.len().div_ceil(fragment_count);
        let mut assembler = FrameAssembler::new(RpcIdentity::new("bridge-test-0001"));
        let mut delivered = Vec::new();
        for index in 0..fragment_count {
            let start = index * chunk_len;
            let end = (start + chunk_len).min(message.len());
            let frame = synthetic_frame(&message, 7, index, fragment_count, &message[start..end]);
            // 物理信封仍受 1 MiB 约束（分片存在的前提）。
            let envelope_len = serde_json::to_vec(&frame).expect("frame json").len();
            assert!(
                envelope_len <= MAX_PHYSICAL_FRAME_BYTES,
                "frame envelope {envelope_len} must stay within physical limit"
            );
            delivered.extend(assembler.accept(frame, 0).expect("accept fragment"));
        }
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0], message);
        assert_eq!(assembler.active_assemblies(), 0);
        assert_eq!(assembler.staged_bytes(), 0);

        let decoded = decode_channel(&delivered[0]).expect("decode reassembled 1.5 MiB message");
        assert_eq!(decoded.message_type, RESPONSE_TYPE_PROMISE_SUCCESS);
        assert_eq!(decoded.body, Value::String("a".repeat(1_500_000)));
    }

    #[test]
    fn hostile_declared_lengths_fail_fast_without_allocation() {
        // 1 MiB 帧、每层数组都声明「剩余字节数」：按声明预分配会放大到 GiB 级。
        let frame = hostile_nested_frame(MAX_PHYSICAL_FRAME_BYTES);
        assert_eq!(frame.len(), MAX_PHYSICAL_FRAME_BYTES);

        test_alloc::reset();
        let result = decode_channel(&frame);
        let peak = test_alloc::peak();
        assert!(result.unwrap_err().is(codes::UNSUPPORTED_PROTOCOL));
        assert!(
            peak <= 4 * frame.len(),
            "peak allocation {peak} bytes for a {} byte frame",
            frame.len()
        );

        // 单个数组声明 u32::MAX 个元素：剩余字节不够，立即拒绝。
        test_alloc::reset();
        let absurd = decode_channel(&[4, 0xff, 0xff, 0xff, 0xff, 0x0f]);
        assert!(absurd.unwrap_err().is(codes::UNSUPPORTED_PROTOCOL));
        assert!(test_alloc::peak() <= 4 * 6);
    }

    #[test]
    fn expired_reassembly_requires_resync() {
        let fixture = fixture();
        let vector = named(&fixture, "fragments", "three_fragments");
        let frames = vector["frames"].as_array().expect("frames").clone();
        let limits = FrameLimits {
            assembly_ttl_ms: 1_000,
            ..FrameLimits::default()
        };
        let mut assembler = FrameAssembler::with_limits(identity_from(vector), limits);

        assert!(assembler.accept(frames[0].clone(), 0).unwrap().is_empty());
        assert_eq!(assembler.active_assemblies(), 1);

        // TTL 到期：丢弃进行中的重组；触发过期的这一帧也不得被交付/暂存。
        assert!(assembler
            .accept(frames[1].clone(), 1_000)
            .unwrap_err()
            .is(codes::UNSUPPORTED_PROTOCOL));
        assert_eq!(assembler.active_assemblies(), 0);
        assert_eq!(assembler.staged_bytes(), 0);

        // 显式重同步后，同一消息从头重组可以完成。
        assembler.reset();
        let mut delivered = Vec::new();
        for frame in &frames {
            delivered.extend(
                assembler
                    .accept(frame.clone(), 2_000)
                    .expect("fresh assembly"),
            );
        }
        assert_eq!(delivered.len(), 1);
        assert_eq!(
            bytes_to_hex(&delivered[0]),
            vector["messageHex"].as_str().expect("messageHex")
        );
    }

    #[test]
    fn production_limits_match_official_and_local_budgets() {
        // 官方限额（`b2/web-remote-control-rpc-transport.ts` 固定 SHA 3c5b2276）。
        assert_eq!(MAX_PHYSICAL_FRAME_BYTES, 1_048_576);
        assert_eq!(MAX_CHANNEL_MESSAGE_BYTES, 16_777_216);
        assert_eq!(MAX_FRAGMENTS_PER_MESSAGE, 64);
        // Tuvis 本地预算（探针既有预算；暂存总字节取官方 maxMessageBytes）。
        assert_eq!(MAX_CONCURRENT_ASSEMBLIES, 32);
        assert_eq!(MAX_STAGED_FRAMES, 1024);
        assert_eq!(MAX_TOTAL_CACHED_BYTES, 16_777_216);
        assert_eq!(ASSEMBLY_TTL_MS, 10_000);

        let limits = FrameLimits::default();
        assert_eq!(limits.max_concurrent_assemblies, MAX_CONCURRENT_ASSEMBLIES);
        assert_eq!(limits.max_staged_frames, MAX_STAGED_FRAMES);
        assert_eq!(limits.max_total_bytes, MAX_TOTAL_CACHED_BYTES);
        assert_eq!(limits.assembly_ttl_ms, ASSEMBLY_TTL_MS);
    }

    #[test]
    fn reassembly_ttl_expires_incomplete_messages() {
        let fixture = fixture();
        let vector = named(&fixture, "fragments", "three_fragments");
        let frames = vector["frames"].as_array().expect("frames").clone();
        let limits = FrameLimits {
            assembly_ttl_ms: 1_000,
            ..FrameLimits::default()
        };
        let mut assembler = FrameAssembler::with_limits(identity_from(vector), limits);

        assert!(assembler.accept(frames[0].clone(), 0).unwrap().is_empty());
        assert_eq!(assembler.active_assemblies(), 1);

        // TTL 到期后再有帧到达：过期重组被丢弃，本帧不暂存、零交付，
        // 且返回要求重同步的冻结错误（不是 Ok(empty) 静默丢弃）。
        let mut other_message = frames[0].clone();
        other_message["messageSeq"] = json!(500u64);
        assert!(assembler
            .accept(other_message, 1_000)
            .unwrap_err()
            .is(codes::UNSUPPORTED_PROTOCOL));
        assert_eq!(assembler.active_assemblies(), 0);
        assert_eq!(assembler.staged_bytes(), 0);
        assert_eq!(assembler.in_flight_fragments, 0);
    }

    #[test]
    fn reassembly_caps_reject_and_require_resync() {
        let fixture = fixture();
        let vector = named(&fixture, "fragments", "two_fragments");
        let frames = vector["frames"].as_array().expect("frames").clone();

        // 并发重组上限。
        let concurrency_limits = FrameLimits {
            max_concurrent_assemblies: 1,
            ..FrameLimits::default()
        };
        let mut assembler = FrameAssembler::with_limits(identity_from(vector), concurrency_limits);
        assert!(assembler.accept(frames[0].clone(), 0).unwrap().is_empty());
        let mut other_message = frames[0].clone();
        other_message["messageSeq"] = json!(777u64);
        assert!(assembler
            .accept(other_message, 0)
            .unwrap_err()
            .is(codes::PAYLOAD_TOO_LARGE));
        // 显式重同步后恢复。
        assembler.reset();
        assert_eq!(assembler.active_assemblies(), 0);
        assert_eq!(assembler.staged_bytes(), 0);

        // 暂存总字节上限。
        let byte_limits = FrameLimits {
            max_total_bytes: 4,
            ..FrameLimits::default()
        };
        let mut assembler = FrameAssembler::with_limits(identity_from(vector), byte_limits);
        assert!(assembler
            .accept(frames[0].clone(), 0)
            .unwrap_err()
            .is(codes::PAYLOAD_TOO_LARGE));

        // 在途分片上限。
        let frame_limits = FrameLimits {
            max_staged_frames: 1,
            ..FrameLimits::default()
        };
        let mut assembler = FrameAssembler::with_limits(identity_from(vector), frame_limits);
        assert!(assembler.accept(frames[0].clone(), 0).unwrap().is_empty());
        assert!(assembler
            .accept(frames[1].clone(), 0)
            .unwrap_err()
            .is(codes::PAYLOAD_TOO_LARGE));
    }

    #[test]
    fn ack_does_not_equal_business_success() {
        let fixture = fixture();
        let identity = RpcIdentity::new("bridge-test-0001");
        let ack = encode_ack(&identity, 11);
        assert_eq!(ack["zcode_type"], json!("rpc-frame-ack"));
        assert_eq!(ack["ackMessageSeq"], json!(11));
        // ACK 不是 Channel 消息：不含 messageType/id，不能解码成业务应答。
        assert!(decode_channel(serde_json::to_vec(&ack).unwrap().as_slice()).is_err());
        assert!(ack.get("messageType").is_none());
        assert!(ack.get("id").is_none());
        assert!(ack.get("status").is_none());

        // 业务成功只能来自 201 的 ChannelMessage（带 id 与业务 body）。
        let success = named(&fixture, "responses", "promise_success");
        let decoded = decode_channel(&hex_to_bytes(success["hex"].as_str().expect("hex")))
            .expect("decode success");
        assert_eq!(decoded.message_type, RESPONSE_TYPE_PROMISE_SUCCESS);
        assert_eq!(decoded.id, Some(7));
        assert_eq!(decoded.body["ack"]["mode"], json!("snapshot"));
    }
}
