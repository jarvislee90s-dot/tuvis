//! Pending Request 层：`conversationState.requests[]` → `PendingView` 投影。
//!
//! 纪律（spec 3.4/3.5 + 计划公共约定）：
//! * requestId 按不透明标识处理（整数/字符串），对外只发无损 `request_key`；
//! * `context_token` 是服务端随机 UUID，绑定 boot/epoch/generation/thread/request；
//!   同一 request 的流式 revision 变化不换 token；内容变化、消失/重现、owner 换代
//!   必须换 token；
//! * 未知 method → `PendingKind::Unsupported`（actionable=false），不暴露 raw
//!   method/参数对象；已知但 profile 能力未验证同样 actionable=false；
//! * 不假设 `availableDecisions` 在场——产品语义只有批准/拒绝/作答。

use super::types::{
    CapabilitySet, PendingKind, PendingRequestId, PendingView, ProtocolError, QuestionOption,
    QuestionView,
};
use std::collections::HashMap;

/// token 缓存条目：内容指纹 + 当前 token。
#[derive(Clone)]
struct TokenEntry {
    fingerprint: u64,
    token: String,
}

/// pending 投影器（每 Thread 一个；owner/epoch 换代时由 mirror 调 `reset`）。
/// token 缓存用内部互斥锁：投影只需 `&self`（mirror.project 免 &mut）。
#[derive(Default)]
pub struct PendingProjector {
    tokens: std::sync::Mutex<HashMap<String, TokenEntry>>,
}

impl PendingProjector {
    pub fn new() -> Self {
        Self::default()
    }

    /// owner/epoch 换代：全部 token 失效（旧卡一律拒绝）。
    pub fn reset(&self) {
        self.tokens.lock().expect("token lock").clear();
    }

    /// 把 `requests[]` 投影为 `PendingView` 列表（输入数组顺序即输出顺序）。
    /// 投影后清除已消失请求的 token（R4：`[r]→[]→[r]` 重现时生成新 token，
    /// 旧卡不可复活）。
    pub fn project(
        &self,
        requests: &[serde_json::Value],
        caps: &CapabilitySet,
    ) -> Vec<PendingView> {
        let mut tokens = self.tokens.lock().expect("token lock");
        // R4：本轮仍在场的 request_key 之外的缓存条目一律清除
        let live: std::collections::HashSet<String> = requests
            .iter()
            .filter_map(|req| {
                let id: PendingRequestId = serde_json::from_value(req.get("id")?.clone()).ok()?;
                Some(id.to_key())
            })
            .collect();
        tokens.retain(|key, _| live.contains(key));
        requests
            .iter()
            .filter_map(|req| project_one(req, caps, &mut tokens))
            .collect()
    }

    /// 校验请求回显的 context_token 是否仍有效。
    pub fn token_valid(&self, request_key: &str, context_token: &str) -> bool {
        self.tokens
            .lock()
            .expect("token lock")
            .get(request_key)
            .is_some_and(|e| e.token == context_token)
    }
}

fn fingerprint(method: &str, value: &serde_json::Value) -> u64 {
    // 规范化序列化指纹（R4：含 method——同 params 不同 method 必须视为不同上下文）：
    // `method + "\n" + canonical params`；内容变化即可检出（无需密码学强度）
    let canonical = format!(
        "{method}\n{}",
        serde_json::to_string(value).unwrap_or_default()
    );
    canonical.bytes().fold(0xcbf29ce484222325u64, |acc, b| {
        (acc ^ u64::from(b)).wrapping_mul(0x100000001b3)
    })
}

fn project_one(
    req: &serde_json::Value,
    caps: &CapabilitySet,
    tokens: &mut HashMap<String, TokenEntry>,
) -> Option<PendingView> {
    let id_value = req.get("id")?;
    let request_id: PendingRequestId = serde_json::from_value(id_value.clone()).ok()?;
    let request_key = request_id.to_key();
    let params = req
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

    let (kind, actionable, disabled, command, cwd, reason, permission_summary, questions) =
        match method {
            "item/commandExecution/requestApproval" => {
                let cap_ok = caps.command;
                (
                    PendingKind::CommandApproval,
                    cap_ok,
                    // 方法已知、该 build 能力未验证 → unverified（与「方法未知」拆码，
                    // 前端文案可区分：「当前 Desktop 版本未验证该操作」）
                    (!cap_ok).then(|| "capability_unverified".to_string()),
                    Some(
                        params
                            .get("command")
                            .and_then(|c| c.as_str())
                            .unwrap_or("")
                            .to_string(),
                    ),
                    params
                        .get("cwd")
                        .and_then(|c| c.as_str())
                        .map(str::to_string),
                    params
                        .get("reason")
                        .and_then(|r| r.as_str())
                        .map(str::to_string),
                    None,
                    Vec::new(),
                )
            }
            "item/permissions/requestApproval" => {
                let cap_ok = caps.permissions;
                (
                    PendingKind::PermissionsApproval,
                    cap_ok,
                    // 同上：已知方法的未验证能力 → unverified
                    (!cap_ok).then(|| "capability_unverified".to_string()),
                    None,
                    params
                        .get("cwd")
                        .and_then(|c| c.as_str())
                        .map(str::to_string),
                    params
                        .get("reason")
                        .and_then(|r| r.as_str())
                        .map(str::to_string),
                    Some(permissions_summary(params.get("permissions"))),
                    Vec::new(),
                )
            }
            "item/tool/requestUserInput" => {
                let cap_ok = caps.blocking_input;
                let questions = params
                    .get("questions")
                    .and_then(|q| q.as_array())
                    .map(|arr| arr.iter().filter_map(question_view).collect())
                    .unwrap_or_default();
                (
                    PendingKind::UserInput,
                    cap_ok,
                    // 同上：已知方法的未验证能力 → unverified
                    (!cap_ok).then(|| "capability_unverified".to_string()),
                    None,
                    None,
                    None,
                    None,
                    questions,
                )
            }
            _ => (
                // 未知 method：只显示「此请求暂不支持远程处理」，不暴露 raw method/参数
                // （保持 capability_unavailable——与「已知但未验证」的 unverified 拆分）
                PendingKind::Unsupported,
                false,
                Some("capability_unavailable".to_string()),
                None,
                None,
                None,
                None,
                Vec::new(),
            ),
        };

    // token：同 request 同 method 同内容 → 复用；内容/方法变化、重现 → 换新
    let fp = fingerprint(method, &params);
    let token = match tokens.get(&request_key) {
        Some(entry) if entry.fingerprint == fp => entry.token.clone(),
        _ => {
            let t = super::types::new_uuid();
            tokens.insert(
                request_key.clone(),
                TokenEntry {
                    fingerprint: fp,
                    token: t.clone(),
                },
            );
            t
        }
    };

    Some(PendingView {
        request_key,
        context_token: token,
        kind,
        actionable,
        disabled_reason_code: disabled,
        command,
        cwd,
        reason,
        permission_summary,
        questions,
    })
}

/// 权限对象 → 人类可读摘要（不含原始对象；远端不得上送自定义 permissions）。
fn permissions_summary(permissions: Option<&serde_json::Value>) -> String {
    let Some(p) = permissions else {
        return "—".into();
    };
    let mut parts: Vec<String> = Vec::new();
    if p.get("network").is_some_and(|n| !n.is_null()) {
        parts.push("network".into());
    }
    if let Some(fs) = p.get("fileSystem") {
        if let Some(read) = fs.get("read").and_then(|r| r.as_array()) {
            if !read.is_empty() {
                parts.push(format!("fs.read: {}", read.len()));
            }
        }
        if let Some(write) = fs.get("write").and_then(|w| w.as_array()) {
            if !write.is_empty() {
                parts.push(format!("fs.write: {}", write.len()));
            }
        }
    }
    if parts.is_empty() {
        "—".into()
    } else {
        parts.join(" / ")
    }
}

fn question_view(q: &serde_json::Value) -> Option<QuestionView> {
    let id = q.get("id")?.as_str()?.to_string();
    Some(QuestionView {
        id,
        header: q
            .get("header")
            .and_then(|h| h.as_str())
            .unwrap_or("")
            .to_string(),
        question: q
            .get("question")
            .and_then(|q| q.as_str())
            .unwrap_or("")
            .to_string(),
        is_other: q.get("isOther").and_then(|b| b.as_bool()).unwrap_or(false),
        is_secret: q.get("isSecret").and_then(|b| b.as_bool()).unwrap_or(false),
        options: q
            .get("options")
            .and_then(|o| o.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|o| QuestionOption {
                        label: o
                            .get("label")
                            .and_then(|l| l.as_str())
                            .unwrap_or("")
                            .to_string(),
                        description: o
                            .get("description")
                            .and_then(|d| d.as_str())
                            .unwrap_or("")
                            .to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// 从 `PendingView` 找回原始 requestId（响应构造用；不经过网络）。
pub fn parse_request_key(key: &str) -> Result<PendingRequestId, ProtocolError> {
    PendingRequestId::from_key(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps_all() -> CapabilitySet {
        CapabilitySet {
            send: true,
            state: true,
            command: true,
            permissions: true,
            blocking_input: true,
        }
    }

    #[test]
    fn unknown_method_is_unsupported_without_raw_leak() {
        let p = PendingProjector::new();
        let views = p.project(
            &[serde_json::json!({
                "id": 5,
                "method": "item/file/requestApproval",
                "params": { "path": "/etc/hosts", "secretStuff": "x" }
            })],
            &caps_all(),
        );
        assert_eq!(views.len(), 1);
        let v = &views[0];
        assert_eq!(v.kind, PendingKind::Unsupported);
        assert!(!v.actionable);
        assert_eq!(
            v.disabled_reason_code.as_deref(),
            Some("capability_unavailable")
        );
        let rendered = serde_json::to_string(v).unwrap();
        assert!(!rendered.contains("item/file/requestApproval"));
        assert!(!rendered.contains("secretStuff"));
        assert!(!rendered.contains("/etc/hosts"));
        assert_eq!(v.request_key, "n:5");
    }

    #[test]
    fn capability_false_is_not_actionable_but_visible() {
        let p = PendingProjector::new();
        let caps = CapabilitySet {
            command: false,
            ..caps_all()
        };
        let views = p.project(
            &[serde_json::json!({
                "id": "s-9",
                "method": "item/commandExecution/requestApproval",
                "params": { "command": "cargo test", "cwd": "/w", "reason": "r" }
            })],
            &caps,
        );
        let v = &views[0];
        assert_eq!(v.kind, PendingKind::CommandApproval);
        assert!(!v.actionable);
        // 拆码契约：方法已知但能力未验证 → capability_unverified（非 unavailable——
        // 那是「方法未知」的码；前端据此显示「当前 Desktop 版本未验证该操作」）
        assert_eq!(
            v.disabled_reason_code.as_deref(),
            Some("capability_unverified")
        );
        assert_eq!(v.request_key, "s:s-9");
        assert_eq!(v.command.as_deref(), Some("cargo test"));
    }

    #[test]
    fn token_stable_across_revision_touches_rotates_on_content_change() {
        let p = PendingProjector::new();
        let caps = caps_all();
        let req = serde_json::json!({
            "id": 12,
            "method": "item/permissions/requestApproval",
            "params": { "cwd": "/w", "permissions": { "fileSystem": { "write": ["/tmp/x"] } } }
        });
        let v1 = p.project(std::slice::from_ref(&req), &caps).remove(0);
        let v2 = p.project(&[req], &caps).remove(0);
        assert_eq!(
            v1.context_token, v2.context_token,
            "同内容同 token（流式刷新不换）"
        );

        let changed = serde_json::json!({
            "id": 12,
            "method": "item/permissions/requestApproval",
            "params": { "cwd": "/w", "permissions": { "fileSystem": { "write": ["/tmp/y"] } } }
        });
        let v3 = p.project(&[changed], &caps).remove(0);
        assert_ne!(v1.context_token, v3.context_token, "内容变化换 token");
        assert!(p.token_valid(&v3.request_key, &v3.context_token));
        assert!(!p.token_valid(&v3.request_key, &v1.context_token));

        p.reset();
        let v4 = p.project(&[v3_origin()], &caps).remove(0);
        assert_ne!(
            v4.context_token, v3.context_token,
            "owner 换代后 token 全换"
        );
    }

    fn v3_origin() -> serde_json::Value {
        serde_json::json!({
            "id": 12,
            "method": "item/permissions/requestApproval",
            "params": { "cwd": "/w", "permissions": { "fileSystem": { "write": ["/tmp/y"] } } }
        })
    }

    #[test]
    fn vanished_request_revokes_token_and_reappear_mints_new() {
        // R4：[r]→[]→[r] 重现时必须生成新 token（消失即撤销，旧卡不可复用）
        let p = PendingProjector::new();
        let caps = caps_all();
        let req = serde_json::json!({
            "id": 12, "method": "item/permissions/requestApproval", "params": { "cwd": "/w" }
        });
        let v1 = p.project(std::slice::from_ref(&req), &caps).remove(0);
        // 消失
        let gone = p.project(&[], &caps);
        assert!(gone.is_empty());
        assert!(
            !p.token_valid(&v1.request_key, &v1.context_token),
            "请求消失即撤销 token"
        );
        // 重现：新 token
        let v2 = p.project(&[req], &caps).remove(0);
        assert_ne!(
            v1.context_token, v2.context_token,
            "重现生成新 token，不复活旧 token"
        );
        assert!(p.token_valid(&v2.request_key, &v2.context_token));
    }

    #[test]
    fn fingerprint_includes_method() {
        // R4：指纹 = method + canonical params——同 params 不同 method 的 token 必须不同
        let p = PendingProjector::new();
        let caps = caps_all();
        let a = serde_json::json!({
            "id": 7, "method": "item/permissions/requestApproval", "params": { "cwd": "/w" }
        });
        let b = serde_json::json!({
            "id": 7, "method": "item/commandExecution/requestApproval", "params": { "cwd": "/w" }
        });
        let va = p.project(&[a], &caps).remove(0);
        let vb = p.project(&[b], &caps).remove(0);
        assert_ne!(
            va.context_token, vb.context_token,
            "指纹含 method：同 params 不同 method 换 token"
        );
    }
}
