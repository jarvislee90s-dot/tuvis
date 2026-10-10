pub mod approve;
// 屏读锚点账本（2026-09-23 用户裁决）：全工具全场景的屏读文案**单一真源**——每个
// (tool, scenario, slot) 槽位持「一组已知文案」，屏上出现哪句认哪句；版本号只作
// 诊断备注、不参与筛选（裁决理由见模块文档）。起因 = codex 0.156.1 改了菜单 footer
// 措辞，写死的单句锚失效 → 权限切换整条静默不可用。
pub mod anchor_ledger;
pub mod capability;
pub mod confirm;
// 问答交互方言表（2026-10-05 推广批 F1）：四家交互差异=数据——新工具接入=填表
// +账本加行+探测定案，不新写阶段机。claude 六机不进表（活体参照语义，用户裁决）。
pub mod dialect;
pub mod question_screen_oc;
// 新建会话状态机内核（spec §4，C4 进程锚定段起）：起窗后按「目标目录 cwd + 新进程」
// 发现 TUI pid；C5 弹窗处置状态机追加于本文件。
pub mod create;
// 物化发现（C6）：首句注入后按工具落盘口径产出候选 session id（显式 base 路径参数
// ——tempdir 可测；opencode 三件套拷贝红线），候选由调用方 confirm 戳终判。
pub mod create_discover;
// 新建会话路径校验纯核（spec §2，C2）：只判不建（递归创建在状态机校验段）；黑名单双表
// ——SENSITIVE_DIRS 凭据表全局段匹配（策略扩展，见模块文档）+ CREATE_SYSTEM_DIRS 系统目录表。
pub mod create_path;
// 通用 N 选项审批对话框屏读解析（批次丙 T5）：纯函数跨平台可测，屏读源在
// windows_console::read_screen_window（仅 Windows 有屏读 → macOS 自然降级二元卡）。
// 丁T3 起同模块承载「对话框在场 = 控制类注入红线」的单点判据
// （`blocks_control_injection` + 屏读探测 `probe_screen_dialog`，§2.7 裁8/9）与
// `RemoteState.dialog_probe` 缝的生产实现。
//
// **守卫覆盖面（丁T3 现状 + 已知缺口登记，勿误读为全覆盖）**：
// - **有守卫**：`remote::api::session_mode_switch`（模式切换的 shift+tab 与斜杠两路
//   ——投递前屏读，在场即 409 `blocked_by_dialog`，零注入零审计）；
// - **无守卫（已知缺口，下批收口点见 `queue` 模块文档的同名小节）**：队列放行
//   （`queue::flush_one` / `try_flush`）与「立即发送」（`remote::api::session_queue_jump`）
//   ——两条都在可输入态投递，而对话框在场时状态同样可能是 Waiting。
pub mod dialog;
pub mod engine;
pub mod families;
// 无头注入底座（Task 6 / spec H4+H6）：runner（并发/超时/取消/kill 树）+ receipt
// 归一 + 版本门控探针。**只做共享底座**——四家工具通道见 Task 8/9/11/13。
pub mod headless;
// 模式切换内核（批次丙 T6）：统一模式枚举 + 各工具切换机制映射 + 屏读回显解析
pub mod mode;
pub mod normalize;
pub mod question;
pub mod queue;
// R5 一键 resume 窗口（M6R–M9R Task 11）：命令表 + 终端 spawn 核心（spawner 缝）
pub mod resume;
pub mod routing;
// 注入时序常量族（**单一事实源**，D20 / 计划 §2.9）：注入后屏读轮询的步长与各阶段
// 总窗 + 文本分块/提交延迟；模块文档含 D20 三条规则与 (c) 例外、以及每条自裁值
// 指向的 `#[ignore]` 实测项。改时序常量前先读那里。
pub mod timing;
#[cfg(windows)]
pub mod windows_console;

/// 实机 E2E 专用测试支撑面（M9R–M9R 批次 Task 12 起，`tests/m9r_e2e.rs` 与
/// C8 `tests/create_e2e.rs` 两个集成测试目标消费）。
/// **非公开 API 承诺**：`doc(hidden)` 不进文档；只 re-export Windows 执行层的
/// spec 感知入口与统计类型（两例 E2E 直调引擎/屏读所需的最小面），零新逻辑零转发。
/// 生产代码不得消费本模块——生产注入一律经 `engine::Injector` 缝（`RealInjector`
/// 装配）与旧薄壳（`locate_and_inject` / `locate_and_send_key`）。
#[doc(hidden)]
#[cfg(windows)]
pub mod e2e_support {
    pub use super::windows_console::{inject_key_spec, inject_text_spec, InjectStats};
    /// 丁T5：问答阶段机的屏读能力（E2E 夹具的 `screen_probe` 缝要按 pid 真读一屏）。
    /// 与 [`super::dialog::probe_screen_dialog`] 的区别：后者返回**解析结论**，本项
    /// 返回**逐行原文**——阶段机的各段判据在内核里，缝要给的是能力（理由见
    /// `remote::server::ScreenProbeFn` 的文档）。薄壳而非 re-export：底层
    /// `read_screen_window` 是 `pub(crate)`，re-export 不出 crate（且本模块的
    /// `doc(hidden)` 面不该扩底层可见性）。
    pub fn read_screen_lines(pid: u32) -> Option<Vec<String>> {
        super::windows_console::read_screen_window(pid).ok()
    }
}

/// 桌面端写审计查看（W5 只读入口）：返回最近 limit 条（缺省 100），最新在前。
/// AuditRow serde camelCase 序列化即前端载荷；无 device_id 字段（设备标识不外泄，
/// 展示侧只用 device_name）。st 不需要：审计读不走注入器/远端状态，全局 DB 直查。
#[tauri::command]
pub fn inject_list_audit(limit: Option<usize>) -> serde_json::Value {
    // 锁自愈取锁（P3 统一）：毒锁不连坐——前一次持锁 panic 后审计查看仍可用
    let conn = crate::database::connection::DB
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let rows = crate::database::dao::write_audit::recent_conn(
        &conn, // IPC 入参封顶：LIMIT 超大值等于全表读入内存
        limit.unwrap_or(100).min(1000) as i64,
    );
    serde_json::json!({ "items": rows })
}

/// 审计写口（DB + events::audit 日志并行，M4 T2e 惯例）：channel = 注入器名，
/// summary 只存摘要（W5 防审计库膨胀）。conn 由调用方短临界区传入（锁内只 SQL）。
/// 原 Task 5 的 queue.rs 私有版提升至此（Task 6）：flush/jump/fail 落账（queue::settle）
/// 与 session-send / queue / retract 端点审计两处共用，单一出口防词表漂移。
/// 字段化入参而非 QueueRow：端点侧审计（send/queue/retract）没有整行可传。
/// action 词表（W5）：send|queue|flush|jump|retract|approve|reject|fail|key|open
/// （open = Task 11 一键 resume，Task 7 的预留标注已兑现）。Phase C 追加
/// `create|dialog`（远程新建会话：create=任务终态行、dialog=弹窗处置逐条留痕）。
/// 批次乙 T8 追加
/// `answer`（AskUserQuestion 问答应答，select/toggle/submit/cancel 四动作统一
/// 记 answer，摘要区分见 question::AnswerAction::audit_label）。批次丙追加
/// `mode`（T6 模式切换，摘要=「切换模式至 <target>」）。**T9 打断式插队不新增
/// 词**：Esc 中断是 `jump` 动作的**内部分步**（先中断再投递），审计仍记 `jump`
/// ——用户视角是一个动作（见 queue::try_flush_with 的 interrupt_first 分支）。
/// **丁T3 追加 `slash`**（裁2：`/` 开头消息裸注入不带签名——前后缀都会破坏命令与
/// 参数；终端不留痕是可接受的，溯源走审计页：action=slash + device_name 列。
/// 判据与实际注入形态同源单点：`normalize::is_slash_message`，见 session_send 的
/// action 选择处）。
/// **Task 6 追加 `headless` / `headless_cancel`**（H6/H4：无头 turn 落账与移动端
/// 取消；词表常量与落账出口单点在 `inject::headless`——`ACTION_HEADLESS` /
/// `ACTION_HEADLESS_CANCEL` + `headless::audit_headless`）。该表 `action` 列为
/// TEXT NOT NULL、**无 CHECK 约束**（`database/schema.rs`），扩词**不需要 migration**。
#[allow(clippy::too_many_arguments)]
pub(crate) fn audit_write_channel(
    conn: &rusqlite::Connection,
    device_id: &str,
    device_name: &str,
    agent_type: &str,
    session_id: &str,
    channel: &str,
    content: &str,
    action: &str,
    result: &str,
) {
    let summary = normalize::summarize(content, normalize::AUDIT_SUMMARY_CHARS);
    crate::database::dao::write_audit::record_conn(
        conn,
        chrono::Utc::now().timestamp_millis(),
        device_id,
        device_name,
        agent_type,
        session_id,
        channel,
        action,
        &summary,
        result,
    );
    // 日志留痕并行（remote_audit target 可 grep 追溯）
    crate::remote::events::audit(
        action,
        &format!("sid={session_id} channel={channel} result={result}"),
    );
}

/// 审计写口薄壳（**既有调用方零改动**）：channel = 终端注入器名。无头动作不走这里
/// ——无头通道名不是终端注入器名，其落账出口 = [`headless::audit_headless`]（同表同
/// 摘要口径，只把 channel 显式传入）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn audit_write(
    conn: &rusqlite::Connection,
    st: &crate::remote::server::RemoteState,
    device_id: &str,
    device_name: &str,
    agent_type: &str,
    session_id: &str,
    content: &str,
    action: &str,
    result: &str,
) {
    audit_write_channel(
        conn,
        device_id,
        device_name,
        agent_type,
        session_id,
        st.injector.name(),
        content,
        action,
        result,
    );
}
