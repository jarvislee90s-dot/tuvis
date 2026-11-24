//! DSH 原生信封编解码（计划 §2.3 固定入口；T01 只做信封层）。
//!
//! 信封契约（0.2.1-rc.3 实测，Win 报告 §3.2 步骤 6 / Mac 报告 §3.1、§2 表第 9 行）：
//! 请求 = `{"type":"client-request","rpcId":<str>,"method":"<ns/method>","payload":{"args":{…}}}`
//! 响应 = `{"type":"server-response","rpcId":<str>,"result":{"ok":true,"value":…}|{"ok":false,"error":{"code":…}}}`
//!
//! 边界：本模块只做**信封层**——JSON 解析、`type` 判别、rpcId 身份核对（外来帧
//! 拒绝）、typed 成功/失败归一、动作载荷闭集解析。业务语义（逐 Operation 的
//! 充分成功条件）由后续 contract/actions 任务完成——[`NativeResponse`] 不等于
//! 业务成功（§2.2）。

use super::error::{codes, DshError};
use super::model::{ActionPayload, NativeError, NativeResponse, NativeResult};

/// JSON 帧上限（计划：JSON 帧 2MiB；transport 的入站 mux 帧、出站信封与一元
/// 响应体统一引用本常量——单一来源，不在 transport 另抄）。
pub const MAX_JSON_FRAME_BYTES: usize = 2 * 1024 * 1024;

/// 编码 client-request 信封（请求 =
/// `{"type":"client-request","rpcId":<str>,"method":"<ns/method>","payload":{"args":{…}}}`）。
///
/// 边界：`method` 必须是 `ns/method` 两段（不接收任意路径，§2.2 闭集纪律）；
/// 出站体积超 [`MAX_JSON_FRAME_BYTES`] → `DSH_FRAME_OVERSIZE`（有界出站）。
pub fn encode_request(
    rpc_id: &str,
    method: &str,
    args: &serde_json::Value,
) -> Result<Vec<u8>, DshError> {
    let segments: Vec<&str> = method.split('/').collect();
    if segments.len() != 2 || segments.iter().any(|s| s.is_empty()) {
        return Err(DshError::new(
            codes::FRAME_INVALID,
            format!("method must be `ns/method` (got `{method}`)"),
        ));
    }
    let envelope = serde_json::json!({
        "type": "client-request",
        "rpcId": rpc_id,
        "method": method,
        "payload": { "args": args },
    });
    let bytes = serde_json::to_vec(&envelope).map_err(|e| {
        DshError::new(
            codes::FRAME_INVALID,
            format!("request envelope serialize failed: {e}"),
        )
    })?;
    if bytes.len() > MAX_JSON_FRAME_BYTES {
        return Err(DshError::new(
            codes::FRAME_OVERSIZE,
            format!(
                "request envelope is {} bytes (cap {MAX_JSON_FRAME_BYTES})",
                bytes.len()
            ),
        ));
    }
    Ok(bytes)
}

/// 编码 mux open 帧（`{type:"open",streamId,endpoint,payload:{args}}`；
/// E2 实测键形，macos 报告 §3.2 步骤 4）。出站体积受 [`MAX_JSON_FRAME_BYTES`] 界。
pub fn encode_open(
    stream_id: &str,
    endpoint: &str,
    args: &serde_json::Value,
) -> Result<Vec<u8>, DshError> {
    if stream_id.is_empty() || endpoint.is_empty() {
        return Err(DshError::new(
            codes::FRAME_INVALID,
            "open frame needs non-empty streamId and endpoint",
        ));
    }
    let frame = serde_json::json!({
        "type": "open",
        "streamId": stream_id,
        "endpoint": endpoint,
        "payload": { "args": args },
    });
    let bytes = serde_json::to_vec(&frame).map_err(|e| {
        DshError::new(
            codes::FRAME_INVALID,
            format!("open frame serialize failed: {e}"),
        )
    })?;
    if bytes.len() > MAX_JSON_FRAME_BYTES {
        return Err(DshError::new(
            codes::FRAME_OVERSIZE,
            format!(
                "open frame is {} bytes (cap {MAX_JSON_FRAME_BYTES})",
                bytes.len()
            ),
        ));
    }
    Ok(bytes)
}

/// 编码 mux cancel 帧（`{type:"cancel",streamId}`；订阅句柄 Drop 时尽力发出）。
/// cancel 帧极小，无体积上限问题。
pub fn encode_cancel(stream_id: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ "type": "cancel", "streamId": stream_id }))
        .unwrap_or_else(|_| br#"{"type":"cancel","streamId":"s-invalid"}"#.to_vec())
}

/// 流签名形态守卫：mux 帧键形（`type ∈ {open,item,end,cancel}` 且带 `streamId`）。
/// 一元通道上出现该形态 = 流签名落在错通道（E2 实测宿主侧 `gateway/signature-invalid`
/// 的客户端侧对偶）——调用方必须拒绝，不得硬解成 server-response 信封。
pub fn is_stream_signature_shape(value: &serde_json::Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    match obj.get("type").and_then(|v| v.as_str()) {
        Some("open") | Some("item") | Some("end") | Some("cancel") => obj.contains_key("streamId"),
        _ => false,
    }
}

/// 解码原生 `server-response` 帧，并核对响应身份 = 请求 `rpc_id`。
///
/// 拒绝（错误码）：
/// * 非 UTF-8 / 非 JSON / 非 object / `type != "server-response"` / 缺 `rpcId` / 缺 `result`
///   → `DSH_FRAME_INVALID`；
/// * `rpcId` ≠ 期望值（外来帧/错代帧，**不得串话**）→ `DSH_RPC_ID_MISMATCH`。
///
/// `result.ok:false` 是**合法**的 typed 失败（`NativeResult::Failure`），不是解码错误。
pub fn decode_response(bytes: &[u8], rpc_id: &str) -> Result<NativeResponse, DshError> {
    let text = std::str::from_utf8(bytes).map_err(|e| {
        DshError::new(
            codes::FRAME_INVALID,
            format!("response body is not UTF-8: {e}"),
        )
    })?;
    let raw: serde_json::Value = serde_json::from_str(text).map_err(|e| {
        DshError::new(
            codes::FRAME_INVALID,
            format!("response body is not valid JSON: {e}"),
        )
    })?;
    let obj = raw.as_object().ok_or_else(|| {
        DshError::new(
            codes::FRAME_INVALID,
            "response envelope is not a JSON object",
        )
    })?;
    let frame_type = obj.get("type").and_then(|v| v.as_str());
    if frame_type != Some("server-response") {
        return Err(DshError::new(
            codes::FRAME_INVALID,
            format!("envelope type is not server-response (got {frame_type:?})"),
        ));
    }
    let wire_rpc_id = obj
        .get("rpcId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| DshError::new(codes::FRAME_INVALID, "envelope has no string rpcId"))?;
    if wire_rpc_id != rpc_id {
        return Err(DshError::new(
            codes::RPC_ID_MISMATCH,
            format!(
                "response rpcId=`{wire_rpc_id}` does not match request rpcId=`{rpc_id}` (foreign frame)"
            ),
        ));
    }
    let result = obj
        .get("result")
        .ok_or_else(|| DshError::new(codes::FRAME_INVALID, "envelope has no result object"))?;
    let native_result = match result.get("ok") {
        Some(serde_json::Value::Bool(true)) => NativeResult::Success {
            value: result
                .get("value")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        },
        Some(serde_json::Value::Bool(false)) => {
            let raw_error = result
                .get("error")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            // error 对象形状不符（如宿主升级改形）→ 保守记 unknown，不猜具体码。
            let error: NativeError = serde_json::from_value(raw_error).unwrap_or(NativeError {
                code: "unknown".into(),
                message: None,
            });
            NativeResult::Failure { error }
        }
        _ => {
            return Err(DshError::new(
                codes::FRAME_INVALID,
                "result.ok is missing or not a boolean",
            ))
        }
    };
    Ok(NativeResponse {
        rpc_id: wire_rpc_id.to_string(),
        result: native_result,
    })
}

/// 已知动作 tag 白名单（与 [`ActionPayload`] serde tag 逐字对应；闭集纪律）。
const KNOWN_ACTION_TAGS: &[&str] = &[
    "send",
    "approval",
    "answer",
    "select_model",
    "cancel_current",
    "queue_edit",
    "queue_remove",
    "steer",
    "rename",
    "fork",
    "archive",
    "unarchive",
    "pin",
    "unpin",
    "command",
];

fn is_known_action_tag(tag: &str) -> bool {
    KNOWN_ACTION_TAGS.contains(&tag)
}

/// `DSH_ACTION_UNKNOWN` 构造（tag 可选携带，detail 只含类型词、不含载荷内容）。
fn unknown_action(reason: &str, tag: Option<&str>) -> DshError {
    match tag {
        Some(tag) => DshError::new(codes::ACTION_UNKNOWN, format!("{reason}: type=`{tag}`")),
        None => DshError::new(codes::ACTION_UNKNOWN, reason.to_string()),
    }
}

/// 解析远程动作载荷（`POST /dsh/action` 的 `payload`）。**闭集**：
/// * 非 object / 缺 `type` / `type` 不在白名单 → `DSH_ACTION_UNKNOWN`（不接收任意
///   ns/method，§2.2）；
/// * 已知变体载荷形状非法（缺字段/未知字段）→ `DSH_ACTION_INVALID`。
///
/// deny_unknown_fields 纪律由两层落实：内层载荷结构（serde deny_unknown_fields）
/// 拒绝结构变体的未知字段；单元变体（serde 会忽略多余键）在此补一道
/// 「除 type 外不得携带任何字段」的检查。
pub fn parse_action(raw: &serde_json::Value) -> Result<ActionPayload, DshError> {
    let obj = raw
        .as_object()
        .ok_or_else(|| unknown_action("action payload is not a JSON object", None))?;
    let tag = obj
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| unknown_action("action payload has no string type tag", None))?;
    let payload: ActionPayload = serde_json::from_value(raw.clone()).map_err(|e| {
        if is_known_action_tag(tag) {
            DshError::new(
                codes::ACTION_INVALID,
                format!("action `{tag}` payload is invalid: {e}"),
            )
        } else {
            unknown_action("action type is not in the whitelist", Some(tag))
        }
    })?;
    if payload.is_unit_variant() && obj.len() > 1 {
        return Err(DshError::new(
            codes::ACTION_INVALID,
            format!("action `{tag}` carries no fields"),
        ));
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 缺 result / type 不符 / ok 非布尔的帧 → FRAME_INVALID。
    #[test]
    fn envelope_shape_failures_are_frame_invalid() {
        for (name, body) in [
            (
                "no result",
                br#"{"type":"server-response","rpcId":"r1"}"#.as_slice(),
            ),
            (
                "bad ok",
                br#"{"type":"server-response","rpcId":"r1","result":{"ok":"yes"}}"#.as_slice(),
            ),
            (
                "wrong type",
                br#"{"type":"client-request","rpcId":"r1","result":{"ok":true}}"#.as_slice(),
            ),
            ("array body", br#"[1,2]"#.as_slice()),
        ] {
            let err = decode_response(body, "r1").unwrap_err();
            assert_eq!(err.code, codes::FRAME_INVALID, "{name}");
        }
    }

    /// ok:true 无 value → Null；ok:false 无 error → unknown 码（不猜）。
    #[test]
    fn result_defaults_are_conservative() {
        let ok = decode_response(
            br#"{"type":"server-response","rpcId":"r1","result":{"ok":true}}"#,
            "r1",
        )
        .unwrap();
        assert_eq!(
            ok.result,
            NativeResult::Success {
                value: serde_json::Value::Null
            }
        );

        let bad = decode_response(
            br#"{"type":"server-response","rpcId":"r1","result":{"ok":false}}"#,
            "r1",
        )
        .unwrap();
        match bad.result {
            NativeResult::Failure { error } => assert_eq!(error.code, "unknown"),
            other => panic!("期望 Failure，得到 {other:?}"),
        }
    }

    /// parse_action：rename/单词边界（send 的驼峰字段）与未知 tag 的 detail 形态。
    #[test]
    fn parse_action_detail_carries_tag_only() {
        let err = parse_action(&serde_json::json!({"type": "terminal.create"})).unwrap_err();
        assert_eq!(err.code, codes::ACTION_UNKNOWN);
        assert!(
            err.detail.contains("terminal.create"),
            "detail 记类型词：{}",
            err.detail
        );

        let send = parse_action(&serde_json::json!({
            "type": "send", "text": "hi", "receiptIds": ["r"],
            "queueOnly": true, "bypassPausedQueue": false
        }))
        .unwrap();
        match send {
            ActionPayload::Send(p) => {
                assert_eq!(p.text, "hi");
                assert!(p.queue_only);
                assert_eq!(p.receipt_ids, vec!["r".to_string()]);
            }
            other => panic!("期望 Send，得到 {other:?}"),
        }
    }

    /// encode_request：E2 实测信封键形 + ns/method 闭集。
    #[test]
    fn request_envelope_shape_is_pinned() {
        let bytes = encode_request("p1", "session/modelCatalog", &serde_json::json!({})).unwrap();
        let raw: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(raw["type"], "client-request");
        assert_eq!(raw["rpcId"], "p1");
        assert_eq!(raw["method"], "session/modelCatalog");
        assert_eq!(raw["payload"]["args"], serde_json::json!({}));

        // 非 ns/method 两段 → 拒绝（不接收任意路径）。
        for bad in ["session", "a/b/c", "/x", "x/"] {
            let err = encode_request("r", bad, &serde_json::json!({})).unwrap_err();
            assert_eq!(err.code, codes::FRAME_INVALID, "{bad}");
        }
    }

    /// open 帧键形（E2 实测）与 cancel 帧键形。
    #[test]
    fn mux_open_and_cancel_frames_match_observed_keys() {
        let open = encode_open("ev", "$events", &serde_json::json!({})).unwrap();
        let raw: serde_json::Value = serde_json::from_slice(&open).unwrap();
        assert_eq!(raw["type"], "open");
        assert_eq!(raw["streamId"], "ev");
        assert_eq!(raw["endpoint"], "$events");
        assert_eq!(raw["payload"]["args"], serde_json::json!({}));

        let cancel: serde_json::Value = serde_json::from_slice(&encode_cancel("s-1")).unwrap();
        assert_eq!(
            cancel,
            serde_json::json!({"type": "cancel", "streamId": "s-1"})
        );
    }

    /// 流签名形态守卫：mux 帧键形识别（一元通道拒绝的判据）。
    #[test]
    fn stream_signature_shape_guard() {
        for frame in [
            serde_json::json!({"type": "item", "streamId": "s", "value": {}}),
            serde_json::json!({"type": "end", "streamId": "s"}),
            serde_json::json!({"type": "open", "streamId": "s"}),
            serde_json::json!({"type": "cancel", "streamId": "s"}),
        ] {
            assert!(is_stream_signature_shape(&frame), "{frame}");
        }
        for envelope in [
            serde_json::json!({"type": "server-response", "rpcId": "r", "result": {"ok": true}}),
            serde_json::json!({"type": "item", "value": {}}),
            serde_json::json!({"type": "error", "streamId": "s"}),
        ] {
            assert!(!is_stream_signature_shape(&envelope), "{envelope}");
        }
    }
}
