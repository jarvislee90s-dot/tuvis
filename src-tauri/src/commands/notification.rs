// 自定义通知浮窗 — 独立无边框置顶小窗，轮转 3 个槽位在右下角纵向堆叠。
// 跨平台实现：不夺键盘焦点（focusable(false)），点击卡片联动跳转会话终端。

use serde::Deserialize;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationPayload {
    pub agent_type: String,  // 原始类型（claude/codex/...），跳转标题打分用
    pub agent_label: String, // 显示名（Claude Code 等专有名词，不翻译）
    pub project_name: String,
    pub status_color: String,
    pub status: String, // 原始状态枚举，由通知页 i18n 翻译
    pub last_message: String,
    /// 会话标题：通知卡跳转走标题匹配层用（kimi/opencode）
    pub title: String,
    pub pid: u32,
    pub session_id: String,
    /// 2A 操作卡标记：Some(CODEX_TRUST_HINT_ACTION) 时前端渲染常驻操作卡
    /// （复制命令/不再提醒/X 暂忽略），None = 既有会话通知卡。既有字段语义
    /// 一律不动，操作卡的会话导向字段传空串/pid=0（无跳转语义）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// 操作卡要复制的命令文本（"/hooks"）——随载荷下发而非前端硬编码：Rust 侧
    /// 换文案不用动前端，卡面展示与复制同源；且卡面自带命令文本是复制失败的
    /// 手抄兜底（focusless 窗内 navigator.clipboard 不保证可用）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_command: Option<String>,
}

const SLOTS: usize = 3;
const W: f64 = 360.0;
const H: f64 = 110.0;
const MARGIN: f64 = 16.0;

/// 槽位占用表：值 = 占用时刻。
/// 不用 is_visible() 判忙：窗口要等事件送达 + 页面 show() 之后才可见（约 300ms+），
/// 同批第二条通知会误判空闲而覆盖第一条。时间戳占用 10 秒自动过期（通知只活 6 秒）。
static SLOT_OCCUPANCY: once_cell::sync::Lazy<
    std::sync::Mutex<std::collections::HashMap<usize, std::time::Instant>>,
> = once_cell::sync::Lazy::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

const SLOT_TTL: std::time::Duration = std::time::Duration::from_secs(10);

#[tauri::command]
pub async fn show_notification_window(
    app: AppHandle,
    payload: NotificationPayload,
) -> Result<(), String> {
    // [Bug A 取证插桩·临时] 命令入口单行摘要：每次浮窗触发必有一条本日志，
    // 与前端 [notif] SHOW_WINDOW / 浮窗页 [notifwin] shown 三点对账——
    // 有弹窗无本行 = 浮窗自重显（H3）；本行频现而前端门全关 = 存在其他调用方（H1）。
    log::info!(
        "[notif] cmd show_notification_window: agentType={} project={} status={} title={} sessionId={} pid={} at={}",
        payload.agent_type,
        payload.project_name,
        payload.status,
        payload.title.chars().take(60).collect::<String>(),
        payload.session_id,
        payload.pid,
        chrono::Local::now().to_rfc3339()
    );
    // 建房必须放独立线程（Windows 上在命令内直接创建 webview 会死锁，wry#583）；
    // 结果经 oneshot 回收向上传播：失败时前端 catch 到 Err、降级系统 toast，通知不丢。
    // await 发生在异步运行时线程，不阻塞主循环，不会复活死锁。
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(create_notification_window(&app, payload));
    });
    match rx.await {
        Ok(result) => result,
        Err(_) => Err("NOTIFY_WINDOW_THREAD_FAIL: 创建通知窗口线程异常".to_string()),
    }
}

/// 实际创建/复用通知窗口并发送事件（在独立线程执行）
fn create_notification_window(app: &AppHandle, payload: NotificationPayload) -> Result<(), String> {
    // 槽位选择：空闲优先；全忙则顶替最早占用的（时间戳判定，规避可见性竞态）
    let now = std::time::Instant::now();
    let slot = {
        let mut occupancy = SLOT_OCCUPANCY.lock().unwrap();
        occupancy.retain(|_, at| now.duration_since(*at) < SLOT_TTL);
        let s = (0..SLOTS)
            .find(|i| !occupancy.contains_key(i))
            .unwrap_or_else(|| {
                occupancy
                    .iter()
                    .min_by_key(|(_, at)| **at)
                    .map(|(i, _)| *i)
                    .unwrap_or(0)
            });
        occupancy.insert(s, now);
        s
    };
    // 右下角定位 + 槽位纵向堆叠（逻辑坐标；再上移避开任务栏高度）
    let mut mx = 1920.0;
    let mut my = 1080.0;
    if let Ok(Some(m)) = app.primary_monitor() {
        mx = m.size().width as f64 / m.scale_factor();
        my = m.size().height as f64 / m.scale_factor();
    }
    let x = mx - W - MARGIN;
    let y = my - H - MARGIN - 48.0 - (slot as f64) * (H + 8.0);

    let label = format!("notification-{slot}");
    match app.get_webview_window(&label) {
        Some(w) => {
            let _ = w.set_position(tauri::Position::Logical(tauri::LogicalPosition::new(x, y)));
        }
        None => {
            let _ = WebviewWindowBuilder::new(
                app,
                &label,
                WebviewUrl::App("index.html#/notification".into()),
            )
            .title("mam-notification")
            .decorations(false)
            .always_on_top(true)
            .skip_taskbar(true)
            .resizable(false)
            .transparent(true)
            .focusable(false) // 不夺键盘焦点的关键：通知永不参与焦点切换
            .visible(false) // 页面收到事件后才 show，避免白屏
            .inner_size(W, H)
            .position(x, y)
            .build()
            .map_err(|e| format!("NOTIFY_WINDOW_FAIL: 创建通知窗口失败: {}", e))?;
        }
    }
    // 定向发送到该槽位窗口（emit 全局广播会让所有槽位同时弹出）
    // 延迟发送规避"页面 JS 尚未注册 listener"的创建竞态；偶发丢失则该条不显示，下一条正常
    let app2 = app.clone();
    let payload2 = payload.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        let _ = app2.emit_to(&label, "notification:new", &payload2);
    });
    Ok(())
}

// —— 2A：操作型提醒（常驻直到用户处置）——
//
// 与会话通知的本质差异是生命周期：会话卡 6 秒自灭（前端 armTimer），操作卡必须
// 常驻到用户点按钮（复制/不再提醒 = 销账；X = 本次藏窗、pending 保留）。

/// 操作卡动作标记：前端据 payload.action 渲染操作卡形态（区别于会话通知卡）
pub const CODEX_TRUST_HINT_ACTION: &str = "codexTrustHint";

/// 操作卡专用单窗标签。取舍：**开专用单窗而非扩展三槽系统**——三槽为「6 秒生灭
/// 的会话通知」设计（槽位占用 10 秒 TTL 过期 + 全忙顶替最早槽），常驻操作卡进去
/// 会被顶替/过期，等于静默销账，违背「点击才销账」；扩展槽位系统支持常驻变体要
/// 动占用表/TTL/顶替策略三处语义，专用单窗只是右下角多一个独立 WebView，与三槽
/// 互不干扰，侵入面最小
const ACTION_WINDOW_LABEL: &str = "notification-action";
/// 操作卡尺寸：宽同会话卡（360），高度需容纳标题/说明/命令展示/双按钮（220）
const ACTION_W: f64 = 360.0;
const ACTION_H: f64 = 220.0;

/// codex 信任门操作卡载荷：会话导向字段全空/pid=0（操作卡无跳转语义，前端按
/// action 分支渲染、不读会话字段）
pub(crate) fn codex_trust_hint_payload() -> NotificationPayload {
    NotificationPayload {
        agent_type: "codex".to_string(),
        agent_label: "Codex".to_string(),
        project_name: String::new(),
        status_color: String::new(),
        status: String::new(),
        last_message: String::new(),
        title: String::new(),
        pid: 0,
        session_id: String::new(),
        action: Some(CODEX_TRUST_HINT_ACTION.to_string()),
        action_command: Some("/hooks".to_string()),
    }
}

/// 操作型提醒的 Rust 侧入口（供 setup 期 hooks::consume_codex_trust_notice 调用，
/// 不进命令表——调用点无前端 invoke 上下文）。延迟独立线程建房：延迟原因与
/// commands/pet.rs 同款——本入口在 setup 期被调，Windows 上主窗 WebView2 控件
/// 初始化期立即建副窗会偶发 E_INVALIDARG 幽灵窗（命令路径 show_notification_
/// window 无此问题：彼时主窗早已就绪，但它仍把建房放独立线程规避 wry#583 死锁）。
/// 失败仅记日志：KV 不动（消费≠销账），重启自然重试；操作卡常驻，晚 0.8 秒
/// 出现无感知成本
pub fn show_action_notification(app: &AppHandle, payload: NotificationPayload) {
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(800));
        if let Err(e) = create_action_notification_window(&app, payload) {
            log::warn!("[action-notif] 操作提醒窗创建失败: {}", e);
        }
    });
}

/// 实际创建/复用操作卡窗并发送事件（独立线程执行）：与 create_notification_
/// window 同管线（emit 延迟 300ms 规避页面 JS 监听注册竞态、visible(false) 待
/// 事件到达再 show 避免白屏），差异三点——①专用标签（不进槽位占用表，无 TTL
/// 过期）；②定位在三槽堆叠**上方**（虚拟 4 号槽，同堆叠公式）：常驻卡若占底部
/// 槽位，后续会话通知（槽 0）会盖住其按钮区；③高度 220（操作卡内容量，前端
/// 操作卡分支不做 110 还原）
fn create_action_notification_window(
    app: &AppHandle,
    payload: NotificationPayload,
) -> Result<(), String> {
    let mut mx = 1920.0;
    let mut my = 1080.0;
    if let Ok(Some(m)) = app.primary_monitor() {
        mx = m.size().width as f64 / m.scale_factor();
        my = m.size().height as f64 / m.scale_factor();
    }
    let x = mx - ACTION_W - MARGIN;
    let y = my - ACTION_H - MARGIN - 48.0 - (SLOTS as f64) * (H + 8.0);

    match app.get_webview_window(ACTION_WINDOW_LABEL) {
        Some(w) => {
            let _ = w.set_position(tauri::Position::Logical(tauri::LogicalPosition::new(x, y)));
        }
        None => {
            WebviewWindowBuilder::new(
                app,
                ACTION_WINDOW_LABEL,
                WebviewUrl::App("index.html#/notification".into()),
            )
            .title("mam-notification-action")
            .decorations(false)
            .always_on_top(true)
            .skip_taskbar(true)
            .resizable(false)
            .transparent(true)
            .focusable(false) // 同会话通知卡：不夺键盘焦点
            .visible(false) // 页面收到事件后才 show，避免白屏
            .inner_size(ACTION_W, ACTION_H)
            .position(x, y)
            .build()
            .map_err(|e| format!("NOTIFY_ACTION_WINDOW_FAIL: 创建操作提醒窗口失败: {}", e))?;
        }
    }
    let app2 = app.clone();
    let payload2 = payload.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        let _ = app2.emit_to(ACTION_WINDOW_LABEL, "notification:new", &payload2);
    });
    Ok(())
}

/// ack 动作值域校验（纯函数，单测点名）：copy（复制命令）/ never（不再提醒）
/// 合法，其余报错
fn validate_ack_action(action: &str) -> Result<(), String> {
    match action {
        "copy" | "never" => Ok(()),
        other => Err(format!(
            "NOTIFY_INVALID_ACTION: 非法销账动作: {other}（合法值: copy/never）"
        )),
    }
}

/// codex 信任门操作卡销账命令（2A，前端「复制命令」/「不再提醒」按钮调用）：
/// action 仅做入参校验与日志区分，两值对 KV 同义（ack_codex_trust_notice 置
/// shown + 清 pending）。X 暂忽略不走本命令（前端仅藏窗，KV 保留 → 下次启动
/// 再弹）
#[tauri::command]
pub fn codex_trust_notice_ack(action: String) -> Result<(), String> {
    validate_ack_action(&action)?;
    log::info!("[action-notif] codex 信任门操作卡销账: action={action}");
    crate::monitor::hooks::ack_codex_trust_notice();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 载荷兼容（2A）：操作卡载荷含 action/actionCommand（camelCase）、会话
    /// 导向字段空/pid=0；既有前端形态（不带新字段的 JSON）仍可反序列化
    /// （serde(default) = useNotification/settings 既有调用方的零回归门），且
    /// 会话卡序列化不携带空操作键（wire 形态与 2A 前完全一致）
    #[test]
    fn action_payload_roundtrip_and_legacy_compat() {
        let action = codex_trust_hint_payload();
        let json = serde_json::to_value(&action).unwrap();
        assert_eq!(json["action"], CODEX_TRUST_HINT_ACTION);
        assert_eq!(json["actionCommand"], "/hooks");
        assert_eq!(json["agentType"], "codex", "camelCase 既有字段不受影响");
        // 会话导向字段对操作卡传空/pid=0（前端操作卡分支不读这些字段）
        assert_eq!(json["pid"], 0);
        assert_eq!(json["sessionId"], "");

        // 既有前端调用方形态：不含新字段的 JSON 必须仍可反序列化
        let legacy = serde_json::json!({
            "agentType": "claude", "agentLabel": "Claude Code",
            "projectName": "p", "statusColor": "green", "status": "Idle",
            "lastMessage": "m", "title": "t", "pid": 1, "sessionId": "s"
        });
        let session: NotificationPayload = serde_json::from_value(legacy).unwrap();
        assert_eq!(session.action, None);
        assert_eq!(session.action_command, None);
        // 会话卡序列化不携带空操作键（前端载荷形态零变化）
        let session_json = serde_json::to_value(&session).unwrap();
        assert!(session_json.get("action").is_none());
        assert!(session_json.get("actionCommand").is_none());
    }

    /// ack 动作值域：copy/never 合法；其余（空串/大小写变体/snooze）报错——
    /// X 暂忽略不构成合法 ack 值（它根本不该调本命令）
    #[test]
    fn ack_action_validation() {
        assert!(validate_ack_action("copy").is_ok());
        assert!(validate_ack_action("never").is_ok());
        assert!(validate_ack_action("").is_err());
        assert!(validate_ack_action("Copy").is_err(), "大小写敏感");
        assert!(validate_ack_action("snooze").is_err());
    }
}
