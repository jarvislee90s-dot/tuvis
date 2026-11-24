//! DSH 控制面错误边界（计划 §2.3：`DshError {code:&'static str, detail:String}`）。
//!
//! 约定（与 [`crate::errcode`] 与 `scripts/check-error-codes.mjs` 门禁同一套规则）：
//! * `code` 是 `DSH_` 前缀机器码；Tauri 边界统一转「CODE: detail」字符串形态
//!   （[`DshError::to_message`]），前端命中码后取 `errors.<camelCase(码)>` 文案
//!   （zh/en 双键由门禁对账）；
//! * 生产码唯一登记处 = [`DSH_ERROR_REGISTRY`]（「码: 中文说明」字面量表）——门禁
//!   据此识别码并要求 zh/en 双键；[`DshError::new`] 在 debug 构建硬失败未登记码
//!   （`services::usage::UsageError::new` 同款纪律）；
//! * `detail` 只放英文机器诊断与定位参数（UsageError 同款惯例），**不得携带
//!   凭据/owner Cookie/令牌**（全局约束：所有凭据脱敏）；
//! * HTTP 远程层的 `{error,reasonCode,actionId?}` 映射与 403 回配对语义由后续
//!   remote 路由任务在本错误码之上实现，本模块不越界。

/// 生产错误码登记表（权威表；新增码四处同步：
/// ① 本表字面量 ② `src/i18n/locales/zh.json` 的 `errors.*` ③ `en.json` 同位键
/// ④ 构造点引用 [`codes`] 常量）。
///
/// 表值 = 「码: 中文说明」：这一形态同时是错误码门禁（`check-error-codes.mjs`）
/// 的对账来源——门禁据此把码登记为「源码在用」，并要求 `errors.<camelCase(码)>`
/// 在 zh/en 双键齐备；中文说明即 zh 文案，en 文案为对应英文句。
pub const DSH_ERROR_REGISTRY: &[&str] = &[
    "DSH_FRAME_INVALID: 原生帧不是合法的 server-response 信封",
    "DSH_RPC_ID_MISMATCH: 原生响应 rpcId 与请求不一致",
    "DSH_ACTION_UNKNOWN: 未知操作类型，拒绝为动作",
    "DSH_ACTION_INVALID: 动作载荷形状非法",
    "DSH_UNSUPPORTED: 该能力尚未接入",
    "DSH_DESKTOP_NOT_INSTALLED: 未找到 DeepSeek Harness 桌面端安装",
    "DSH_PROFILE_NOT_INITIALIZED: 桌面端 profile 尚未初始化",
    "DSH_HOST_NOT_RUNNING: DeepSeek Harness 桌面端未在运行",
    "DSH_TARGET_AMBIGUOUS: 检测到多个桌面端实例，无法唯一定位",
    "DSH_HOST_LISTENER_MISSING: 桌面端进程未确认本机回环监听",
    "DSH_DESKTOP_EXIT_PENDING: 桌面端尚未退出，作业正在等待用户退出",
    "DSH_NATIVE_LOCK_BUSY: 官方 profile 锁被占用，无法安全写入",
    "DSH_PACKAGE_CONFLICT: 固定包自有文件被用户改动，操作拒绝继续",
    "DSH_PACKAGE_INVALID: 嵌入固定包缺失或校验失败",
    "DSH_CLI_FAILED: 官方 CLI 调用失败",
    "DSH_VERIFY_FAILED: 作业结果核验未通过",
    "DSH_REVOKE_FAILED: 卸载前撤销绑定失败",
    "DSH_JOB_UNKNOWN: 管理作业不存在",
    "DSH_JOB_BUSY: 该实例已有管理作业正在运行",
    "DSH_STATE_IO: 管理状态账本读写失败",
    "DSH_AUTH_REPLAY: 授权挑战 nonce 已被使用，判定为重放",
    "DSH_AUTH_SIGNATURE: 授权挑战签名验证未通过",
    "DSH_AUTH_PROTOCOL: 本机授权握手协议代际不匹配或帧非法",
    "DSH_AUTH_EXPIRED: 授权挑战已过期",
    "DSH_AUTH_EPOCH_STALE: 授权绑定的是旧代际宿主，已随换代失效",
    "DSH_AUTH_NOT_APPROVED: 对端尚未经用户批准，拒绝交付凭据",
    "DSH_AUTH_NO_PENDING: 当前没有等待批准的插件连接",
    "DSH_AUTH_UNKNOWN_BINDING: 绑定 ID 不存在",
    "DSH_PEER_MISMATCH: 对端自报身份与 OS 层真实身份不一致",
    "DSH_PEER_NOT_HOST: 连接方进程不是 DSH Desktop 宿主",
    "DSH_IPC_STATE: 本机 IPC/rendezvous 状态读写失败",
    "DSH_PEER_UNAUTHORIZED: 宿主拒绝了该 Peer 的凭据（401，终态不自动重试）",
    "DSH_PEER_CONFLICT: 宿主已有该 Peer 关联的 API socket（409，终态不自动重连）",
    "DSH_CONNECTION_CLOSED: DSH 归属连接已关闭，写入被拒绝",
    "DSH_STATE_PATH_TOO_LONG: DSH 状态目录路径过长，本机套接字无法绑定；请把 TUVIS_DSH_STATE_DIR 设到更短的目录",
    "DSH_WRITE_TIMEOUT: 写入超时，送达未知（不自动重投）",
    "DSH_READ_TIMEOUT: 读取超时，本轮读取未完成（点重试）",
    "DSH_REDIRECT_BLOCKED: 重定向试图离开已验证的 loopback authority",
    "DSH_FRAME_OVERSIZE: 帧超过 2MiB 上限，已拒绝",
    "DSH_CONNECT_FAILED: 与宿主建立官方连接失败",
    "DSH_STREAM_ERROR: mux 流错误帧（原生错误明细在 detail）",
    "DSH_PEER_REVOKED: 该绑定已被撤销，连接与重连一律拒绝",
    "DSH_CONTRACT_RULES_MISMATCH: 契约识别请求的规则代际与当前规则表不一致，请重新识别",
    "DSH_ACTION_ID_CONFLICT: 同一动作 ID 携带了不同载荷，拒绝复用",
    "DSH_ACTION_NOT_FOUND: 动作账本中不存在该动作或不属于当前设备",
    "DSH_ACTION_STATE_CONFLICT: 动作当前状态不允许该操作",
    "DSH_ACTION_EVIDENCE_MISMATCH: 效果证据与动作不匹配，拒绝提升为成功",
    "DSH_ACTION_WRITE_GATED: 启动恢复未完成或失败，DSH 写入口已关闭",
    "DSH_CAPABILITY_DENIED: 能力门拒绝（契约未匹配/自动适配关闭/未取证）",
    "DSH_SESSION_OWNERSHIP_UNPROVEN: 无法证明该会话由所选实例提供，已拒绝写入（请重新选择实例）",
    "DSH_INSTANCE_SWITCHED: 所选实例已变化，请返回原实例后重试",
    "DSH_SUBAGENT_READONLY: 子 Agent 会话只读，无法发送或上传附件",
    "DSH_ATTACHMENT_MACOS_UNSAFE: macOS 无法保证附件发布时的目录归属，DSH 附件上传已停用。",
    "DSH_INSTANCE_UNVERIFIED: 实例运行归属未核验，已拒绝连接（请重新选择或重新授权）",
    "DSH_CLI_SOURCE_UNVERIFIED: 未找到经核验的 dsh CLI 来源，拒绝执行未核验命令（请先启动一次 dsh 宿主或安装官方 CLI）",
    "DSH_CONSOLE_DEVICE_REQUIRED: 控制台入口缺少已鉴权设备，拒绝签发委托",
    "DSH_SEND_RECONNECTING: 发送通道正在恢复连接（宿主重启后自动重连中），请稍候重试",
    "DSH_SEND_NOT_AUTHORIZED: 发送凭据未就绪，请在电脑上检查所选实例并完成授权",
    "DSH_SWITCH_OFF: 远程控制开关未开启，授权入口已关闭（请先打开「远程控制 DSH」）",
    "DSH_SWITCH_ACTIVE: 开关正在管理该实例的生命周期，请通过开关操作",
    "DSH_SWITCH_REVOKE_PENDING: 关闭尚未完成（关闭流程未收敛），请再次关闭以重试",
];

/// 码常量（构造点引用，避免裸字符串漂移；词表见 [`DSH_ERROR_REGISTRY`]）。
pub mod codes {
    /// 原生帧结构非法（非 UTF-8/非 JSON/type 不符/缺字段）。
    pub const FRAME_INVALID: &str = "DSH_FRAME_INVALID";
    /// 响应 rpcId 与请求不一致（外来/错代帧）。
    pub const RPC_ID_MISMATCH: &str = "DSH_RPC_ID_MISMATCH";
    /// 动作 type 不在白名单（不接收任意 ns/method）。
    pub const ACTION_UNKNOWN: &str = "DSH_ACTION_UNKNOWN";
    /// 已知动作变体的载荷形状非法（缺字段/未知字段）。
    pub const ACTION_INVALID: &str = "DSH_ACTION_INVALID";
    /// 能力未接入（T01 占位边界；任何阶段不把占位显示成已支持）。
    pub const UNSUPPORTED: &str = "DSH_UNSUPPORTED";
    /// 未安装：常规安装根找不到官方 Desktop（npm 全局 dsh 不算安装）。
    pub const DESKTOP_NOT_INSTALLED: &str = "DSH_DESKTOP_NOT_INSTALLED";
    /// profile 未初始化：有安装但 desktop profile 缺失或缺官方标记文件。
    pub const PROFILE_NOT_INITIALIZED: &str = "DSH_PROFILE_NOT_INITIALIZED";
    /// 未运行：安装与 profile 都在，但没有合格宿主进程。
    pub const HOST_NOT_RUNNING: &str = "DSH_HOST_NOT_RUNNING";
    /// 实例多义：多于一个合格宿主（monitor 取第一个的口径 control 面禁用）。
    pub const TARGET_AMBIGUOUS: &str = "DSH_TARGET_AMBIGUOUS";
    /// 监听缺失：宿主进程在，但未确认其名下的回环监听（假监听/非回环都算）。
    pub const HOST_LISTENER_MISSING: &str = "DSH_HOST_LISTENER_MISSING";
    /// 退出未确认：Desktop 宿主仍在运行，作业停在等待退出（不是错误，是忙态）。
    pub const DESKTOP_EXIT_PENDING: &str = "DSH_DESKTOP_EXIT_PENDING";
    /// 原生锁占用：官方 CLI/profile 锁被占（宿主或并发 CLI），拒绝盲写。
    pub const NATIVE_LOCK_BUSY: &str = "DSH_NATIVE_LOCK_BUSY";
    /// 固定包冲突：自有文件被用户改动，按指纹如实报冲突（不静默覆盖）。
    pub const PACKAGE_CONFLICT: &str = "DSH_PACKAGE_CONFLICT";
    /// 固定包非法：嵌入包缺失/超预算/SHA-256 不符（任何 CLI 调用之前拒绝）。
    pub const PACKAGE_INVALID: &str = "DSH_PACKAGE_INVALID";
    /// CLI 失败：官方 CLI 非零退出/无法启动，且不属于锁占用失败族。
    pub const CLI_FAILED: &str = "DSH_CLI_FAILED";
    /// 核验未通过：CLI exit 0 之后 list/profile 文件态核验不达标（无假成功）。
    pub const VERIFY_FAILED: &str = "DSH_VERIFY_FAILED";
    /// 撤权失败：卸载前置的绑定撤销失败（§2.5 卸载先撤绑定，序不可换）。
    pub const REVOKE_FAILED: &str = "DSH_REVOKE_FAILED";
    /// 作业不存在：job_id 查无此作业。
    pub const JOB_UNKNOWN: &str = "DSH_JOB_UNKNOWN";
    /// 同一实例已有其他进程的管理作业。
    pub const JOB_BUSY: &str = "DSH_JOB_BUSY";
    /// 账本 IO 失败：management.json 的原子读写失败（审计面降级，不谎报状态）。
    pub const STATE_IO: &str = "DSH_STATE_IO";
    /// 重放：挑战 nonce 已用过（一次性纪律；同帧二次出现即拒）。
    pub const AUTH_REPLAY: &str = "DSH_AUTH_REPLAY";
    /// 签名不符：Ed25519 验签失败（公钥不在绑定表 / 消息被改）。
    pub const AUTH_SIGNATURE: &str = "DSH_AUTH_SIGNATURE";
    /// 协议不符：bindingProtocol 代际不同或握手帧形状非法。
    pub const AUTH_PROTOCOL: &str = "DSH_AUTH_PROTOCOL";
    /// 挑战过期：expiry 超出 60 秒窗口或已过当前时刻。
    pub const AUTH_EXPIRED: &str = "DSH_AUTH_EXPIRED";
    /// 旧代际：握手/凭据绑定的 hostEpoch 与当前发现代不一致（换代即失效）。
    pub const AUTH_EPOCH_STALE: &str = "DSH_AUTH_EPOCH_STALE";
    /// 未批准：用户尚未在本机 UI 批准该对端，不得交付任何凭据。
    pub const AUTH_NOT_APPROVED: &str = "DSH_AUTH_NOT_APPROVED";
    /// 无待批对端：绑定发起时当前没有等待批准的插件连接。
    pub const AUTH_NO_PENDING: &str = "DSH_AUTH_NO_PENDING";
    /// 绑定不存在：binding_id 查无此绑定。
    pub const AUTH_UNKNOWN_BINDING: &str = "DSH_AUTH_UNKNOWN_BINDING";
    /// 对端身份不符：自报 PID/启动时刻/可执行文件与 OS 层观测不一致（含 PID 复用）。
    pub const PEER_MISMATCH: &str = "DSH_PEER_MISMATCH";
    /// 非宿主进程：OS 层核验判定连接方不是 DSH Desktop 宿主（直接断连）。
    pub const PEER_NOT_HOST: &str = "DSH_PEER_NOT_HOST";
    /// 本机 IPC 状态失败：rendezvous/绑定账本/套接字状态面的读写失败。
    pub const IPC_STATE: &str = "DSH_IPC_STATE";
    /// 凭据被拒（401）：宿主拒绝该 Peer 的 cookie——终态，不自动重试/重连。
    pub const PEER_UNAUTHORIZED: &str = "DSH_PEER_UNAUTHORIZED";
    /// socket 冲突（409）：一 Peer 只允许一条 API socket——终态，不自动重连。
    pub const PEER_CONFLICT: &str = "DSH_PEER_CONFLICT";
    /// 归属连接已关闭：mux socket 断开即移除 Peer，后续写入立即拒绝（不排队）。
    pub const CONNECTION_CLOSED: &str = "DSH_CONNECTION_CLOSED";
    /// 状态目录路径过长：`<state_dir>/ipc-<16hex>.sock` 超过 AF_UNIX `sun_path`
    /// 上限（103 字节），绑定必然失败——先给出可诊断的定位错误。
    pub const STATE_PATH_TOO_LONG: &str = "DSH_STATE_PATH_TOO_LONG";
    /// 写超时：单次一元请求 10 秒无响应——送达未知，映射上游 UnknownDelivery，不重投。
    pub const WRITE_TIMEOUT: &str = "DSH_WRITE_TIMEOUT";
    /// 读超时（发现②加固，2026-11-16 真机证据：只读消息走内核楔死 → 请求无限期
    /// 挂）：只读解析在预算内未完成——读取超时降级，可重试。/m 的
    /// session-messages 端点映射 HTTP 504（前端「读取超时，点重试」语义）。
    /// 超时 ≠ 通过：只读门按「不可证明」拒绝，硬下界不放松。
    pub const READ_TIMEOUT: &str = "DSH_READ_TIMEOUT";
    /// 重定向拦截：任何试图离开钉定 loopback authority 的 3xx 一律失败（不盲随）。
    pub const REDIRECT_BLOCKED: &str = "DSH_REDIRECT_BLOCKED";
    /// 帧超限：JSON 帧超过 2MiB 上限（有界拒绝，不无界缓冲）。
    pub const FRAME_OVERSIZE: &str = "DSH_FRAME_OVERSIZE";
    /// 建连失败：升级/交换阶段的其他 HTTP 状态或 IO 失败（非 401/409 终态族）。
    pub const CONNECT_FAILED: &str = "DSH_CONNECT_FAILED";
    /// mux 流错误帧：宿主 `{type:"error",streamId,error:{code,message}}`（原生码在 detail）。
    pub const STREAM_ERROR: &str = "DSH_STREAM_ERROR";
    /// 绑定已撤权：该句柄的闸门已翻转——拒绝建连/重连（撤权后不回退权限）。
    pub const PEER_REVOKED: &str = "DSH_PEER_REVOKED";
    /// 规则代际不符：契约识别只按当前 [`super::contract::CONTRACT_RULE_VERSION`]
    /// 的规则表出证据，请求其它代际一律拒绝（不把新规则结果冒充旧代际证据）。
    pub const CONTRACT_RULES_MISMATCH: &str = "DSH_CONTRACT_RULES_MISMATCH";
    /// T09：同 action_id 异 payload（客户端 bug 或跨设备撞号）。
    pub const ACTION_ID_CONFLICT: &str = "DSH_ACTION_ID_CONFLICT";
    /// T09：动作不存在或不属于当前设备（跨设备按不存在回答）。
    pub const ACTION_NOT_FOUND: &str = "DSH_ACTION_NOT_FOUND";
    /// T09：动作存在但当前状态不允许该操作（不是「不存在」）。
    pub const ACTION_STATE_CONFLICT: &str = "DSH_ACTION_STATE_CONFLICT";
    /// T09：效果证据与动作（操作/代际/native id）不匹配。
    pub const ACTION_EVIDENCE_MISMATCH: &str = "DSH_ACTION_EVIDENCE_MISMATCH";
    /// T09：启动恢复门未放行/失败——DSH 写入口关闭（读路径仍可用）。
    pub const ACTION_WRITE_GATED: &str = "DSH_ACTION_WRITE_GATED";
    /// P1-A（T10 复审）：能力门拒绝——detail 携带 capability reason（ASCII 机器码）。
    pub const CAPABILITY_DENIED: &str = "DSH_CAPABILITY_DENIED";
    /// H04（§3.3）：主会话写归属无法证明——会话不在该实例 home 下，或被多个活体
    /// 实例同时认领。只读/拒绝，不猜（最终证明归 H05 受信插件握手）。
    pub const SESSION_OWNERSHIP_UNPROVEN: &str = "DSH_SESSION_OWNERSHIP_UNPROVEN";
    pub const INSTANCE_SWITCHED: &str = "DSH_INSTANCE_SWITCHED";
    pub const SUBAGENT_READONLY: &str = "DSH_SUBAGENT_READONLY";
    pub const ATTACHMENT_MACOS_UNSAFE: &str = "DSH_ATTACHMENT_MACOS_UNSAFE";
    /// H04 审查 I1/I2：实例运行归属未核验——Web 宿主 home 无正面证据（argv 无
    /// home，H02），或已登记实例的受信插件指纹不可核验。resolve 一律拒绝，不猜。
    pub const INSTANCE_UNVERIFIED: &str = "DSH_INSTANCE_UNVERIFIED";
    /// H05 审查 I5-r2：web 目标的 dsh CLI 来源无法证明（宿主 argv 入口缺安装树锚/
    /// PATH 解析不出官方形态入口）——拒绝执行未核验命令，绝不按同名猜。
    pub const CLI_SOURCE_UNVERIFIED: &str = "DSH_CLI_SOURCE_UNVERIFIED";
    /// H09：控制台能力签发缺少已鉴权设备——不签发不带设备绑定的委托。
    pub const CONSOLE_DEVICE_REQUIRED: &str = "DSH_CONSOLE_DEVICE_REQUIRED";
    /// T3 发送预检四轴之一：绑定在场但发送凭据是旧代际/未交付——活体跟踪拍会
    /// 自动重发布当代记录并重交付（宿主重启后的恢复窗口），不是契约或归属问题。
    pub const SEND_RECONNECTING: &str = "DSH_SEND_RECONNECTING";
    /// T3 发送预检四轴之一：无 Bound 绑定/已撤权/当代宿主不可证——需要用户在
    /// 电脑上检查所选实例（授权/启动），不是「正在恢复」。
    pub const SEND_NOT_AUTHORIZED: &str = "DSH_SEND_NOT_AUTHORIZED";
    /// Task 4 单开关：远控开关未开启，授权激活入口拒绝（先打开「远程控制 DSH」）。
    pub const SWITCH_OFF: &str = "DSH_SWITCH_OFF";
    /// Task 4 单开关：开关正持有该实例生命周期，生命周期命令拒绝绕过直调。
    pub const SWITCH_ACTIVE: &str = "DSH_SWITCH_ACTIVE";
    /// Round-8 P1-1（Ruling）：Failed(Revoke) 态开启被拒——撤销授权尚未完成，
    /// 旧绑定仍在盘上；唯一出路是再次关闭（重试撤权）收敛。
    pub const SWITCH_REVOKE_PENDING: &str = "DSH_SWITCH_REVOKE_PENDING";
}

/// DSH 控制面结构化错误（§2.3 固定形状）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DshError {
    /// 机器码（`DSH_` 前缀；词表见 [`codes`] / [`DSH_ERROR_REGISTRY`]）。
    pub code: &'static str,
    /// 英文机器诊断（不含凭据；用户文案由前端按码取 locale）。
    pub detail: String,
}

impl DshError {
    /// 生产构造唯一入口：debug 构建下拒绝未登记码（防「码表全绿、前端静默收敛」）。
    /// （注意：本断言文案刻意避开 errcode 门禁的未迁移启发式触发词——那套
    /// `format!/Err(/…error…` 上下文词表会把这个 CJK 字面量误计入迁移棘轮。）
    pub fn new(code: &'static str, detail: impl Into<String>) -> Self {
        debug_assert!(
            is_registered(code),
            "错误码 `{code}` 未登记进 DSH_ERROR_REGISTRY（权威表；登记处：\
             ① 本文件 DSH_ERROR_REGISTRY 字面量 ② zh+en locale 的 dshXxx 双键）"
        );
        Self {
            code,
            detail: detail.into(),
        }
    }

    /// Tauri 边界形态：「CODE: detail」（errcode.rs 约定）。
    pub fn to_message(&self) -> String {
        format!("{}: {}", self.code, self.detail)
    }
}

/// 码是否在登记表（「码: 」精确前缀匹配，拒掉 `DSH_FRAME` 这类半截码）。
fn is_registered(code: &str) -> bool {
    DSH_ERROR_REGISTRY
        .iter()
        .any(|entry| entry.starts_with(code) && entry.as_bytes().get(code.len()) == Some(&b':'))
}

/// 公开只读判定（F09-C）：作业登记的 reasonCode 可能来自旧构建/外部账本，只有
/// 已登记码才能原样写入开关记录并上屏——未登记的字符串一律回退到兜底码，避免把
/// 内部日志或路径泄成用户可见状态。
pub fn is_known_code(code: &str) -> bool {
    is_registered(code)
}

impl std::fmt::Display for DshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_message())
    }
}

impl std::error::Error for DshError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// 登记表与码常量互为镜像：常量全在表内，表内词全部能被精确识别。
    #[test]
    fn registry_and_code_constants_stay_in_sync() {
        for code in [
            codes::FRAME_INVALID,
            codes::RPC_ID_MISMATCH,
            codes::ACTION_UNKNOWN,
            codes::ACTION_INVALID,
            codes::UNSUPPORTED,
            codes::DESKTOP_NOT_INSTALLED,
            codes::PROFILE_NOT_INITIALIZED,
            codes::HOST_NOT_RUNNING,
            codes::TARGET_AMBIGUOUS,
            codes::HOST_LISTENER_MISSING,
            codes::DESKTOP_EXIT_PENDING,
            codes::NATIVE_LOCK_BUSY,
            codes::PACKAGE_CONFLICT,
            codes::PACKAGE_INVALID,
            codes::CLI_FAILED,
            codes::VERIFY_FAILED,
            codes::REVOKE_FAILED,
            codes::JOB_UNKNOWN,
            codes::STATE_IO,
            codes::AUTH_REPLAY,
            codes::AUTH_SIGNATURE,
            codes::AUTH_PROTOCOL,
            codes::AUTH_EXPIRED,
            codes::AUTH_EPOCH_STALE,
            codes::AUTH_NOT_APPROVED,
            codes::AUTH_NO_PENDING,
            codes::AUTH_UNKNOWN_BINDING,
            codes::PEER_MISMATCH,
            codes::PEER_NOT_HOST,
            codes::IPC_STATE,
            codes::PEER_UNAUTHORIZED,
            codes::PEER_CONFLICT,
            codes::CONNECTION_CLOSED,
            codes::STATE_PATH_TOO_LONG,
            codes::WRITE_TIMEOUT,
            codes::READ_TIMEOUT,
            codes::REDIRECT_BLOCKED,
            codes::FRAME_OVERSIZE,
            codes::CONNECT_FAILED,
            codes::STREAM_ERROR,
            codes::PEER_REVOKED,
            codes::CONTRACT_RULES_MISMATCH,
            codes::ACTION_ID_CONFLICT,
            codes::ACTION_NOT_FOUND,
            codes::ACTION_STATE_CONFLICT,
            codes::ACTION_EVIDENCE_MISMATCH,
            codes::ACTION_WRITE_GATED,
            codes::CAPABILITY_DENIED,
            codes::SESSION_OWNERSHIP_UNPROVEN,
            codes::INSTANCE_SWITCHED,
            codes::SUBAGENT_READONLY,
            codes::ATTACHMENT_MACOS_UNSAFE,
            codes::INSTANCE_UNVERIFIED,
            codes::CLI_SOURCE_UNVERIFIED,
            codes::CONSOLE_DEVICE_REQUIRED,
            codes::SEND_RECONNECTING,
            codes::SEND_NOT_AUTHORIZED,
            codes::SWITCH_OFF,
            codes::SWITCH_ACTIVE,
            codes::SWITCH_REVOKE_PENDING,
        ] {
            assert!(is_registered(code), "{code} 未进登记表");
        }
        assert_eq!(DSH_ERROR_REGISTRY.len(), 61, "新增码须同步本用例与四处登记");
        // 半截码不得误判为已登记。
        assert!(!is_registered("DSH_FRAME"));
        assert!(!is_registered("DSH_UNKNOWN_MADE_UP"));
    }

    /// Display/边界形态：CODE: detail。
    #[test]
    fn display_uses_coded_message_form() {
        let e = DshError::new(codes::UNSUPPORTED, "capability not wired yet");
        assert_eq!(e.to_message(), "DSH_UNSUPPORTED: capability not wired yet");
        assert_eq!(e.to_string(), e.to_message());
    }
}
