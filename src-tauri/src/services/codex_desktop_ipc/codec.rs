//! 有界帧编解码：`4-byte LE u32 长度 + UTF-8 JSON`（双平台同构，探测定案）。
//!
//! 上限纪律：协议上限 [`PROTOCOL_MAX_FRAME`] = 268435456（Desktop 侧 Mc）；Tuvis
//! 本地接收预算 [`LOCAL_RECV_BUDGET`] = 16 MiB。**0 或超过任一上限，先拒绝再分配**
//! （头部即判，绝不先按长度开缓冲），错误为 [`ProtocolError::Capacity`]，由上层
//! 关闭该连接——不能冒充无 owner。UTF-8/JSON 错误同为 [`ProtocolError::InvalidFrame`]
//! 且关闭连接。JSON 解析深度保留 serde 默认限制。

use super::types::ProtocolError;

/// 协议级帧上限（Desktop 内置 `Mc`）。
pub const PROTOCOL_MAX_FRAME: usize = 268_435_456;
/// Tuvis 本地接收预算（计划「协议与工程值」）。
pub const LOCAL_RECV_BUDGET: usize = 16 * 1024 * 1024;

/// 帧头长度。
const HEADER: usize = 4;

/// 累积式帧解码器：`push` 任意到达的分片/合并字节，吐出完整 JSON 帧。
/// 缓冲总量受 [`LOCAL_RECV_BUDGET`] 约束（含无法成帧的垃圾字节流）。
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// 追加字节并解出所有完整帧。任何违规（长度 0/超限、UTF-8/JSON 非法、缓冲超预算）
    /// 返回 `Err`，此后该解码器不可再用（上层必须关闭连接）。
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<serde_json::Value>, ProtocolError> {
        self.buf.extend_from_slice(bytes);
        if self.buf.len() > LOCAL_RECV_BUDGET {
            // 含「无帧头的持续垃圾流」：头部未定也要限总量
            return Err(ProtocolError::Capacity(format!(
                "receive buffer exceeds budget (>{} bytes)",
                LOCAL_RECV_BUDGET
            )));
        }
        let mut out = Vec::new();
        loop {
            if self.buf.len() < HEADER {
                break;
            }
            let len =
                u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
            // 头部即判：0 / 协议上限 / 本地预算，先拒绝再分配
            if len == 0 {
                return Err(ProtocolError::InvalidFrame("zero-length frame".into()));
            }
            if len > PROTOCOL_MAX_FRAME {
                return Err(ProtocolError::Capacity(format!(
                    "frame exceeds protocol cap {len} > {PROTOCOL_MAX_FRAME}"
                )));
            }
            if len > LOCAL_RECV_BUDGET {
                return Err(ProtocolError::Capacity(format!(
                    "frame exceeds local receive budget {len} > {LOCAL_RECV_BUDGET}"
                )));
            }
            if self.buf.len() - HEADER < len {
                break; // 等更多分片
            }
            let body: Vec<u8> = self.buf.drain(..HEADER + len).skip(HEADER).collect();
            let text = std::str::from_utf8(&body).map_err(|e| {
                ProtocolError::InvalidFrame(format!("frame body is not UTF-8: {e}"))
            })?;
            let value = serde_json::from_str(text)
                .map_err(|e| ProtocolError::InvalidFrame(format!("frame body is not JSON: {e}")))?;
            out.push(value);
        }
        Ok(out)
    }
}

/// 编码一帧：4-byte LE 长度 + UTF-8 JSON。空载荷或超过协议上限拒绝。
pub fn encode_frame(value: &serde_json::Value) -> Result<Vec<u8>, ProtocolError> {
    let body = serde_json::to_vec(value)
        .map_err(|e| ProtocolError::InvalidFrame(format!("serialize failed: {e}")))?;
    if body.is_empty() {
        return Err(ProtocolError::InvalidFrame(
            "empty payload (zero length reserved as invalid frame)".into(),
        ));
    }
    if body.len() > PROTOCOL_MAX_FRAME {
        return Err(ProtocolError::Capacity(format!(
            "frame exceeds protocol cap {} > {PROTOCOL_MAX_FRAME}",
            body.len()
        )));
    }
    let mut out = Vec::with_capacity(HEADER + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fragmented_and_coalesced_frames() {
        assert!(FrameDecoder::new().push(&0u32.to_le_bytes()).is_err());
        assert!(FrameDecoder::new()
            .push(&268435457u32.to_le_bytes())
            .is_err());
        let frame = encode_frame(&json!({"text":"你好🐇"})).unwrap();
        let mut d = FrameDecoder::new();
        assert!(d.push(&frame[..3]).unwrap().is_empty());
        assert_eq!(d.push(&frame[3..]).unwrap()[0]["text"], "你好🐇");
    }

    #[test]
    fn coalesced_frames_in_single_push_decode_in_order() {
        let f1 = encode_frame(&json!({"seq":1})).unwrap();
        let f2 = encode_frame(&json!({"seq":2})).unwrap();
        let mut d = FrameDecoder::new();
        let mut both = f1;
        both.extend_from_slice(&f2);
        let out = d.push(&both).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["seq"], 1);
        assert_eq!(out[1]["seq"], 2);
    }

    #[test]
    fn zero_length_rejected_before_allocation() {
        let mut d = FrameDecoder::new();
        assert!(matches!(
            d.push(&0u32.to_le_bytes()),
            Err(ProtocolError::InvalidFrame(_))
        ));
    }

    #[test]
    fn oversize_length_rejected_before_allocation() {
        // 协议上限 + 1
        let mut d = FrameDecoder::new();
        assert!(matches!(
            d.push(&268435457u32.to_le_bytes()),
            Err(ProtocolError::Capacity(_))
        ));
        // 协议上限内但超出 Tuvis 本地预算（17 MiB）：同样头部即拒
        let mut d = FrameDecoder::new();
        let len = (LOCAL_RECV_BUDGET + 1024) as u32;
        assert!(matches!(
            d.push(&len.to_le_bytes()),
            Err(ProtocolError::Capacity(_))
        ));
    }

    #[test]
    fn headerless_accumulation_is_bounded() {
        // 无完整帧头的持续垃圾流：超过本地预算即拒（不得无界累积）
        let mut d = FrameDecoder::new();
        let chunk = vec![0x20u8; 1024 * 1024];
        for _ in 0..(LOCAL_RECV_BUDGET / chunk.len() + 1) {
            if let Err(e) = d.push(&chunk) {
                assert!(matches!(e, ProtocolError::Capacity(_)));
                return;
            }
        }
        panic!("垃圾流未被预算拦截");
    }

    #[test]
    fn frame_body_must_be_utf8_and_json() {
        let mut d = FrameDecoder::new();
        let mut raw = 5u32.to_le_bytes().to_vec();
        raw.extend_from_slice(&[0xff, 0xfe, 0x41, 0x00, 0x42]);
        assert!(matches!(d.push(&raw), Err(ProtocolError::InvalidFrame(_))));

        let mut d = FrameDecoder::new();
        let body = b"not json";
        let mut raw = (body.len() as u32).to_le_bytes().to_vec();
        raw.extend_from_slice(body);
        assert!(matches!(d.push(&raw), Err(ProtocolError::InvalidFrame(_))));
    }

    #[test]
    fn encode_rejects_empty_and_oversize() {
        assert!(encode_frame(&serde_json::Value::Null).is_ok()); // "null" 4 字节合法
                                                                 // 空串载荷（长度 0 帧头语义被保留给非法帧）
        let big = serde_json::Value::String("x".repeat(PROTOCOL_MAX_FRAME + 1));
        assert!(matches!(
            encode_frame(&big),
            Err(ProtocolError::Capacity(_))
        ));
    }
}
