//! DSH 本机管理 IPC（计划 §2.4「本机 IPC」九命令逐字；T05）。
//!
//! ## 命令与契约（§2.4 固定九命令；**不接受包名/命令/任意安装路径**——目标由
//! `identity` 单源解析，前端只发起动作）
//!
//! | IPC 命令 | 入参（camelCase 上行） | 返回 |
//! |---|---|---|
//! | `dsh_remote_status` | — | [`DshRemoteStatusDto`]（ManagementStatus + autoAdaptEnabled） |
//! | `dsh_plugin_install` | — | `job_id` |
//! | `dsh_plugin_repair` | — | `job_id` |
//! | `dsh_plugin_uninstall` | — | `job_id` |
//! | `dsh_plugin_job` | `jobId` | [`ManagementJob`] |
//! | `dsh_plugin_cancel` | `jobId` | [`ManagementJob`] |
//! | `dsh_binding_start` | — | [`DshBindingStartDto`]（Challenge flatten + 命令层扩展） |
//! | `dsh_binding_approve` | `bindingId` | `()`（T06 文档化扩展命令） |
//! | `dsh_binding_revoke` | `bindingId` | `()`（T06 接真实 auth） |
//! | `dsh_set_auto_adapt` | `enabled` | 生效值（三态缺省开） |
//!
//! ## 错误语义
//!
//! `DshError` 在此边界统一转既有「`CODE: detail`」字符串（[`DshError::to_message`]；
//! `errcode.rs` 约定），前端 `formatInvokeError` 命中码后取 `errors.<camelCase(码)>`
//! 文案。绑定命令 T06 起接真实 auth：无待批连接 → `DSH_AUTH_NO_PENDING`；
//! 未知绑定 → `DSH_AUTH_UNKNOWN_BINDING`；挑战过期 → `DSH_AUTH_EXPIRED`。
//!
//! ## §2.4 之外的文档化扩展
//!
//! * `dsh_binding_approve(bindingId)`：用户批准命令——§2.4 固定九命令没有
//!   「批准」位，而 §2.5 要求「由用户批准后固定对端公钥」，故以扩展命令登记；
//! * [`DshBindingStartDto`]：`dsh_binding_start` 返回体在 `BindingChallenge`
//!   之上叠加双端身份核对面（客户端摘要 + 本端公钥指纹）；
//! * [`DshRemoteStatusDto.bindingId`]：状态命令叠加的活跃绑定 id（撤权按钮的
//!   真实 id 来源），不进 §2.2 固定类型。
//!
//! ## 自动适配开关（T05 新设置键）
//!
//! `remote.dsh_auto_adapt`（镜像 Codex 的 `remote.codex_auto_adapt` 三态口径）：
//! 缺键/乱串 = 未显式写过 → 生效值**开**；"true"/"false" = 用户显式选择。消费方
//! （后续 capability 任务的活门）只认本模块的解析单点。
//!
//! ## 单开关「远程控制 DSH」（Task 4 文档化扩展）
//!
//! 四命令：`dsh_switch_get/enable/disable/repair`——状态机与持久化在
//! `services::dsh_control::switch`（按选定实例落盘 `<state>/instances/<id>/
//! remote-switch.json`），前端只消费后端实际状态。**R2 开关门**：授权激活类
//! 命令（`dsh_binding_start/approve/rebind/arm_intent`）在开关未开启时拒绝
//! （`DSH_SWITCH_OFF`）；生命周期类命令（`dsh_plugin_install/repair/uninstall`
//! 与 `dsh_binding_revoke`）在开关持有生命周期期间拒绝直调（`DSH_SWITCH_ACTIVE`）
//! ——底层原语保留，开关与「修复插件」经服务内部复用，不经公开门。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DshInstanceRequest {
    pub instance_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DshInstanceCandidateDto {
    pub instance_id: String,
    pub kind: &'static str,
    pub display_name: String,
    pub profile_dir: std::path::PathBuf,
    /// 该实例**当代是否有活体**。
    ///
    /// 为什么必须有这个字段：列表既包含活体候选，也包含**已登记但当前未运行**的
    /// 实例。用户按提示退出 DSH 之后，活体发现为空；若列表只报活体，前端就再也
    /// 拿不到 instanceId，「重开」入口随之消失——而那正是这个状态下唯一的出口。
    /// `false` 的条目只用于让用户重开/删除，不表示可用。
    pub live: bool,
    pub selected: bool,
    /// 仅已核验活体提供；供重启前后比较，不暴露环境内容。
    pub host_epoch: Option<String>,
}

pub fn instance_list_core(
    env: &crate::services::dsh_control::model::DshEnvironment,
    registry: &crate::services::dsh_control::instances::InstanceRegistry,
) -> Result<Vec<DshInstanceCandidateDto>, String> {
    use crate::services::dsh_control::instances::HostKind;
    let kind_word = |kind: HostKind| match kind {
        HostKind::Desktop => "desktop",
        HostKind::Web => "web",
    };
    // 活体候选（发现失败不当硬故障：已登记项仍要报给用户）。
    let live = crate::services::dsh_control::identity::discover_all(env).unwrap_or_default();
    let selected = registry
        .selected()
        .map_err(coded)?
        .map(|entry| entry.instance_id);
    let mut out: Vec<DshInstanceCandidateDto> = live
        .iter()
        .map(|candidate| DshInstanceCandidateDto {
            instance_id: candidate.instance_id.clone(),
            kind: kind_word(candidate.kind),
            display_name: candidate.display_name.clone(),
            profile_dir: candidate.profile_dir.clone(),
            live: true,
            selected: selected.as_deref() == Some(candidate.instance_id.as_str()),
            host_epoch: candidate.verified.then(|| candidate.host.epoch.to_string()),
        })
        .collect();
    let live_ids: std::collections::HashSet<String> =
        out.iter().map(|dto| dto.instance_id.clone()).collect();
    // 已登记但当前无活体的实例：补进列表（`live: false`）。
    //
    // 这是「退出 DSH 后仍能重开」的数据前提。登记读取失败不阻塞活体部分——
    // 那只会少几个离线条目，不会让可用条目消失。
    for registered in registry.registered_all().unwrap_or_default() {
        if live_ids.contains(&registered.instance_id) {
            continue;
        }
        out.push(DshInstanceCandidateDto {
            selected: selected.as_deref() == Some(registered.instance_id.as_str()),
            instance_id: registered.instance_id.clone(),
            kind: kind_word(registered.kind),
            display_name: crate::services::dsh_control::identity::display_name_for(
                &registered.profile_dir,
            ),
            profile_dir: registered.profile_dir,
            live: false,
            host_epoch: None,
        });
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod select_test_probe {
    use std::sync::{Arc, Mutex};

    pub type Hook = Arc<dyn Fn(&str, &str) -> bool + Send + Sync>;
    pub static AFTER_READ: Mutex<Option<Hook>> = Mutex::new(None);
    pub static BEFORE_CLOSE: Mutex<Option<Hook>> = Mutex::new(None);
    pub static CLOSE: Mutex<Option<Hook>> = Mutex::new(None);

    pub fn call(slot: &Mutex<Option<Hook>>, previous: &str, target: &str) -> bool {
        let hook = slot.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook(previous, target)
        } else {
            false
        }
    }
}

pub fn instance_select_core(
    env: &crate::services::dsh_control::model::DshEnvironment,
    registry: &crate::services::dsh_control::instances::InstanceRegistry,
    request: DshInstanceRequest,
    switch_resume: &dyn Fn(&str),
) -> Result<(), String> {
    let candidates = crate::services::dsh_control::identity::discover_all(env).map_err(coded)?;
    let mut matches = candidates
        .into_iter()
        .filter(|c| c.instance_id == request.instance_id && c.verified);
    let candidate = matches.next().ok_or_else(|| {
        DshError::new(codes::HOST_NOT_RUNNING, "instance is not present").to_message()
    })?;
    if matches.next().is_some() {
        return Err(
            DshError::new(codes::TARGET_AMBIGUOUS, "instance identity is ambiguous").to_message(),
        );
    }
    let home = crate::services::dsh_control::instances::session_home(&candidate.host.profile_dir);
    let registered = registry
        .register_candidate_with_home(candidate.kind, &candidate.profile_dir, Some(&home))
        .map_err(coded)?;
    if registered.instance_id != request.instance_id {
        return Err(
            DshError::new(codes::INSTANCE_SWITCHED, "instance identity changed").to_message(),
        );
    }
    // 切换实例必须关闭「上一实例」正开着的控制台页面：能力令牌只绑设备 + 实例 +
    // 期限，**不绑「当前选择」**——不关就会让旧实例的活控制台在切走后继续推流。
    //
    // P1-4（2026-11-14 用户裁决：切走关页 ≠ 撤权）：这里只做 **close-pages**
    // （drain 旧实例活连接 + 取消），**保留**其网页凭据与能力——切回该实例时凭
    // 保留凭据直接打开控制台，无需重交付。「撤销该实例的授权」是独立的撤权链
    // 动作（`revoke_local` → `revoke_console_access_for_instance`），不由切换触发。
    // 代际失效判定不变：宿主重启换代后旧能力照旧失效（P1-2c 既有闸门）。
    // 销项复审【2】P2：读前任与翻转选择并入同一把选择文件锁（`select_exclusive`
    // 原子对）——此前「锁外 `selected()` + 锁内 `select()`」两段式在并发切换下
    // 两个 writer 都会读到同一旧选择，关错页/漏关活连接。实际前任的关页动作
    // （含测试探针）必须在选择租约存续期内完成：租约一放，并发 writer 的新一代
    // 选择即可落地；迟到的关页被边界挡住，不得越过边界关别的 writer 刚开的页面。
    // 销项复审 P1：选择翻转持提交边界独占——先阻断新许可、排空存量在途写，
    // Guard 持到选择落盘结束；翻转先落地时，发送的提交点门必然读到新选择并
    // 拒绝（0 次写出）；在途写先完成时，消息在旧状态权威期内已提交。
    let _commit_flip = crate::services::dsh_control::production::commit_flip_begin();
    let (previous, _selected_now, _selection_lease) = registry
        .select_exclusive(&registered.instance_id)
        .map_err(coded)?;
    // 销项复审 P1-5：选择落盘后在同一翻转独占段内同步更新进程内选择镜像
    // ——提交许可区的内存判据据此拒绝写向已离开的实例（盘读不出许可区）。
    crate::services::dsh_control::production::record_instance_selection(
        previous.as_deref(),
        &registered.instance_id,
    );
    // P1-1：实例换代 → **同步封存**实时问答（先停驱动再清表）。放在翻转独占段内：
    // 旧实例的 in-flight 回执此刻起一律被代次校验拒绝，不会写进新代际。
    crate::services::dsh_control::seal_live_questions(&registered.instance_id);
    if let Some(previous) = previous
        .as_deref()
        .filter(|id| *id != registered.instance_id)
    {
        crate::services::dsh_control::seal_live_questions(previous);
    }
    drop(_commit_flip);
    #[cfg(test)]
    select_test_probe::call(
        &select_test_probe::AFTER_READ,
        previous.as_deref().unwrap_or(""),
        &registered.instance_id,
    );
    if let Some(previous) = previous.filter(|id| id != &registered.instance_id) {
        #[cfg(test)]
        {
            select_test_probe::call(
                &select_test_probe::BEFORE_CLOSE,
                &previous,
                &registered.instance_id,
            );
            if !select_test_probe::call(
                &select_test_probe::CLOSE,
                &previous,
                &registered.instance_id,
            ) {
                crate::remote::console_mount::close_instance_pages(&previous);
            }
        }
        #[cfg(not(test))]
        crate::remote::console_mount::close_instance_pages(&previous);
    }
    // P1-3（Round-7 Task 3）：切换后补位——新选实例若有非终态开关记录，拉起
    // 本代驱动器（同代幂等）。启动扫描已覆盖重启场景，这里覆盖「驱动器会话内
    // 退场（如账本瞬时不可读）后用户切回该实例」的补位。fail-soft：恢复失败
    // 不改变选择结果（闭包实现自记日志）。
    switch_resume(&registered.instance_id);
    Ok(())
}

/// 实例服务来源：生产 = 缓存单例（作业/授权状态在同一对象上）；测试 = 注入构造。
pub trait InstanceServices {
    fn services(
        &self,
        id: &str,
    ) -> Result<
        (
            std::sync::Arc<DshPluginManager>,
            std::sync::Arc<crate::services::dsh_control::auth::DshAuthService>,
        ),
        DshError,
    >;
}

impl<F> InstanceServices for F
where
    F: Fn(
        &str,
    ) -> Result<
        (
            std::sync::Arc<DshPluginManager>,
            std::sync::Arc<crate::services::dsh_control::auth::DshAuthService>,
        ),
        DshError,
    >,
{
    fn services(
        &self,
        id: &str,
    ) -> Result<
        (
            std::sync::Arc<DshPluginManager>,
            std::sync::Arc<crate::services::dsh_control::auth::DshAuthService>,
        ),
        DshError,
    > {
        self(id)
    }
}

pub fn instance_status_core(
    env: &crate::services::dsh_control::model::DshEnvironment,
    registry: &crate::services::dsh_control::instances::InstanceRegistry,
    request: Option<DshInstanceRequest>,
) -> Result<serde_json::Value, String> {
    // 测试/注入入口：按 env 现建（隔离环境无全局单例）。
    let injected = |id: &str| -> Result<_, DshError> {
        let auth = std::sync::Arc::new(
            crate::services::dsh_control::production::init_instance_auth(env, id)?,
        );
        let registered = registry.registered(id)?;
        // M06 统一判定（注入壳与生产缓存同一语义）：登记 home 必须可证明，
        // 不可证明的记录拒绝装配管理器（需启动原实例重新选择）。
        let registered = {
            let physical = std::fs::canonicalize(&registered.profile_dir).map_err(|_| {
                DshError::new(
                    codes::INSTANCE_UNVERIFIED,
                    format!(
                        "instance {id} registered profile is missing; start the original instance and re-select it"
                    ),
                )
            })?;
            let mut registered = registered;
            match crate::services::dsh_control::instances::resolve_registered_home(
                Some(env),
                &registered,
                &physical,
            ) {
                crate::services::dsh_control::instances::RegisteredHomeResolution::Proven(home) => {
                    registered.dsh_home = Some(home);
                    registered
                }
                crate::services::dsh_control::instances::RegisteredHomeResolution::Unproven(
                    reason,
                ) => {
                    return Err(DshError::new(
                        codes::INSTANCE_UNVERIFIED,
                        format!(
                            "instance {id} dsh home unproven ({reason}); start the original instance and re-select it"
                        ),
                    ));
                }
            }
        };
        let base = std::sync::Arc::new(crate::services::dsh_control::model::DshEnvironment {
            home: env.home.clone(),
            tuvis_home: env.tuvis_home.clone(),
            dsh_state_dir: env.dsh_state_dir.clone(),
            processes: env.processes.clone(),
            files: env.files.clone(),
            runner: env.runner.clone(),
            clock: env.clock.clone(),
            environments: env.environments.clone(),
        });
        let auth_for_revoker = std::sync::Arc::clone(&auth);
        let revoker =
            crate::services::dsh_control::production::InstanceBindingRevoker::with_resolver(
                base,
                id.to_owned(),
                std::sync::Arc::new(move || Ok(std::sync::Arc::clone(&auth_for_revoker))),
            );
        let manager = crate::services::dsh_control::production::init_instance_manager(
            env,
            &registered,
            std::sync::Arc::new(revoker),
        )
        .with_authorization_reader(std::sync::Arc::new(
            crate::services::dsh_control::auth::AuthServiceReader(std::sync::Arc::clone(&auth)),
        ));
        Ok((std::sync::Arc::new(manager), auth))
    };
    instance_status_core_with(env, registry, request, &injected)
}

/// 状态核心（服务来源可注入；生产壳传缓存单例）。
pub fn instance_status_core_with<S: InstanceServices + ?Sized>(
    env: &crate::services::dsh_control::model::DshEnvironment,
    registry: &crate::services::dsh_control::instances::InstanceRegistry,
    request: Option<DshInstanceRequest>,
    services: &S,
) -> Result<serde_json::Value, String> {
    // 目标唯一来源 = 持久选择；显式请求只允许指向同一实例。
    let selected = registry.selected().map_err(coded)?.ok_or_else(|| {
        DshError::new(codes::TARGET_AMBIGUOUS, "no instance selected").to_message()
    })?;
    if let Some(request) = &request {
        if request.instance_id != selected.instance_id {
            return Err(DshError::new(
                codes::INSTANCE_SWITCHED,
                "requested instance is not the selected one",
            )
            .to_message());
        }
    }
    let id = selected.instance_id.clone();
    // 候选必须仍在场且归属可证明；消失时不回落到别的实例。
    let candidate = crate::services::dsh_control::identity::discover_instance_candidate(env, &id)
        .map_err(coded)?;
    if !candidate.verified {
        return Err(DshError::new(
            codes::INSTANCE_UNVERIFIED,
            "instance home attribution is unverified",
        )
        .to_message());
    }
    if candidate.instance_id != selected.instance_id
        || candidate.profile_dir != selected.profile_dir
        || candidate.kind != selected.kind
    {
        return Err(DshError::new(
            codes::INSTANCE_SWITCHED,
            "instance identity changed since selection",
        )
        .to_message());
    }
    let _registered = registry.registered(&id).map_err(coded)?;
    let (manager, auth) = services.services(&id).map_err(coded)?;
    let status = manager.status().map_err(coded)?;
    let binding_id = auth.active_binding_id();
    let mut value = serde_json::to_value(status).map_err(|e| e.to_string())?;
    if let Some(object) = value.as_object_mut() {
        object.insert("instanceId".to_owned(), serde_json::json!(id));
        object.insert("bindingId".to_owned(), serde_json::json!(binding_id));
    }
    Ok(value)
}

use crate::services::dsh_control::error::{codes, DshError};
use crate::services::dsh_control::install::DshPluginManager;
use crate::services::dsh_control::model::{ManagementJob, ManagementKind};

/// DSH 自动适配开关键（通用设置 KV；镜像 `remote.codex_auto_adapt` 的三态口径）。
pub const KEY_AUTO_ADAPT: &str = "remote.dsh_auto_adapt";

// ============================================================
// 自动适配三态解析（缺省开的单点；remote::auto_adapt_setting_from 同款口径）
// ============================================================

/// 三态解析：`None` = 未显式写过（缺键/乱串/空白）→ 调用方取缺省**开**。
pub fn auto_adapt_setting_from(v: Option<&str>) -> Option<bool> {
    match v.map(str::trim) {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    }
}

/// 生效值（纯函数）：缺键/乱串 = 缺省**开**（计划全局约束「默认自动适配开启」）。
pub fn auto_adapt_enabled_from(v: Option<&str>) -> bool {
    auto_adapt_setting_from(v).unwrap_or(true)
}

// ============================================================
// 命令层 DTO（ManagementStatus 不改 §2.2 固定字段，autoAdaptEnabled 在此叠加）
// ============================================================

/// `dsh_remote_status` 返回体：§2.2 [`crate::services::dsh_control::model::ManagementStatus`]
/// 原样 flatten + 命令层叠加的 `autoAdaptEnabled`（设置页开关初值）与
/// `bindingId`（T06：撤权按钮的真实 id；无活跃绑定 = null）。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DshRemoteStatusDto {
    #[serde(flatten)]
    pub status: crate::services::dsh_control::model::ManagementStatus,
    /// DSH 自动适配开关生效值（缺省开；写走 `dsh_set_auto_adapt`）。
    pub auto_adapt_enabled: bool,
    /// 活跃（Bound）绑定 id（文档化扩展；§2.2 固定类型不含此字段）。
    pub binding_id: Option<String>,
}

/// `dsh_binding_start` 返回体：§2.4 [`crate::services::dsh_control::model::BindingChallenge`]
/// flatten + 命令层叠加的双端身份核对面（§2.5「首次本机 UI 显示双方身份」）。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DshBindingStartDto {
    #[serde(flatten)]
    pub challenge: crate::services::dsh_control::model::BindingChallenge,
    /// 待批插件端可执行文件名（非秘密；用户核对面）。
    pub client_executable: Option<String>,
    /// 待批插件端 OS PID。
    pub client_pid: Option<u32>,
    /// 本端（兔维斯）公钥指纹（SHA-256 hex 前 16 位）。
    pub rust_fingerprint: String,
    pub instance_id: Option<String>,
    pub old_fingerprint: Option<String>,
    pub new_fingerprint: String,
}

// ============================================================
// 可测核心（命令薄壳 → 纯函数核；测试注入 manager/连接，不触生产单例）
// ============================================================

/// 通用设置 KV 的 DSH 自动适配生效值（全局连接；生产命令壳与测试共用）。
pub fn current_auto_adapt() -> bool {
    auto_adapt_enabled_from(crate::database::dao::settings::get_setting(KEY_AUTO_ADAPT).as_deref())
}

/// 管理命令错误边界（唯一出口）：`DshError` → 「CODE: detail」字符串。
fn coded(e: DshError) -> String {
    e.to_message()
}

/// `dsh_remote_status` 核心：manager 真实状态 + 命令层叠加 autoAdaptEnabled /
/// bindingId（T06：活跃绑定 id 由 auth 读取面给出，命令层透传）。
pub fn remote_status_core(
    mgr: &DshPluginManager,
    auto_adapt_enabled: bool,
    binding_id: Option<String>,
) -> Result<DshRemoteStatusDto, String> {
    Ok(DshRemoteStatusDto {
        status: mgr.status().map_err(coded)?,
        auto_adapt_enabled,
        binding_id,
    })
}

/// R09：授权域不可用时的状态叠加（**不新增平行通道**，直接改既有字段）。
///
/// 语义选择：域的失败是「控制面残缺」，最贴近的既有档是
/// `installation = Broken`（卡面已有 rose 徽标 + 修复按钮）+ `reasonCode = 具体码`
/// ——前端 `reasonText` 会把它渲染成对应文案（`errors.dshIpcState` 等），而安装/
/// 修复按钮本来就在那里，这就是「修复出口」。
///
/// 为什么不动 `activation`/`authorization`：前者描述**已安装插件的运行态**，
/// 后者描述**绑定**；域初始化失败时两者都无真值可言（authorization 保持
/// UBound 是诚实口径，不能因为 IPC 坏了就说「已撤销」）。
///
/// **M-3 修复**：域降级原因是**背景信息**，只在状态里还没有原因时补位。用户刚点了
/// 卸载/安装、作业以 `DSH_CLI_FAILED` / `DSH_VERIFY_FAILED` 收场时，覆盖成域码会
/// 让卡面显示「域不可用」而不是「这次操作为什么失败」——看不到自己那次操作的真实
/// 结论就不知道下一步该修什么。作业自己的码优先；`installation = Broken` 与清空
/// `binding_id` 的降级语义不变（修复出口仍指向同一处）。
pub fn apply_domain_failure(status: &mut DshRemoteStatusDto, reason_code: &str) {
    status.status.installation = crate::services::dsh_control::model::InstallationState::Broken;
    status.status.activation = crate::services::dsh_control::model::ActivationState::Disabled;
    // 只在没有更具体的原因时补位（作业失败码 > 域降级码）。
    if status.status.reason_code.is_none() {
        status.status.reason_code = Some(reason_code.to_string());
    }
    status.binding_id = None; // 读取面拿不到真值时不得留着上一次的 id
}

/// `dsh_plugin_install` / `dsh_plugin_repair` / `dsh_plugin_uninstall` 核心。
pub fn plugin_start_core(mgr: &DshPluginManager, kind: ManagementKind) -> Result<String, String> {
    mgr.start(kind).map_err(coded)
}

/// `dsh_plugin_job` 核心。
pub fn plugin_job_core(mgr: &DshPluginManager, job_id: &str) -> Result<ManagementJob, String> {
    mgr.job(job_id).map_err(coded)
}

/// `dsh_plugin_cancel` 核心。
pub fn plugin_cancel_core(mgr: &DshPluginManager, job_id: &str) -> Result<ManagementJob, String> {
    mgr.cancel(job_id).map_err(coded)
}

/// `dsh_set_auto_adapt` 核心：恒写显式两值（写后即「显式选择」，永远被尊重），
/// 返回生效值。与 Codex 侧差异：DSH 的消费活门由后续 capability 任务接本键，
/// T05 只落盘 + 在 `dsh_remote_status` 叠加下发。
pub fn set_auto_adapt_core(enabled: bool) -> Result<bool, String> {
    crate::database::dao::settings::set_setting(
        KEY_AUTO_ADAPT,
        if enabled { "true" } else { "false" },
    );
    Ok(enabled)
}

/// `dsh_binding_start` 核心（T06 真实实现）：有待批连接 → 返回其挑战 + 双端
/// 核对面；无 → `DSH_AUTH_NO_PENDING`（DSH 未运行 / 插件未连接都如实透出）。
pub fn binding_start_core(
    auth: &crate::services::dsh_control::auth::DshAuthService,
) -> Result<DshBindingStartDto, String> {
    let pending = auth.pending_binding().ok_or_else(|| {
        coded(DshError::new(
            codes::AUTH_NO_PENDING,
            "no pending dsh plugin connection; keep the DSH desktop app running with the plugin installed",
        ))
    })?;
    Ok(DshBindingStartDto {
        challenge: pending.binding.clone(),
        client_executable: pending
            .client
            .executable
            .file_name()
            .map(|n| n.to_string_lossy().to_string()),
        client_pid: Some(pending.client.pid),
        rust_fingerprint: auth.rust_fingerprint(),
        instance_id: auth.instance_scope_id().map(str::to_owned),
        old_fingerprint: None,
        new_fingerprint: crate::services::dsh_control::auth::key_digest(
            &pending.client.public_key_spki,
        ),
    })
}

/// `dsh_binding_approve` 核心（T06 文档化扩展命令）：用户批准后钉公钥放行。
/// async：批准要向等待中的连接推放行帧（机制要求；壳层直接 await）。
pub async fn binding_approve_core(
    auth: &std::sync::Arc<crate::services::dsh_control::auth::DshAuthService>,
    binding_id: &str,
) -> Result<(), String> {
    auth.approve(binding_id).await.map_err(coded)
}

pub async fn binding_rebind_core(
    registry: &crate::services::dsh_control::instances::InstanceRegistry,
    auth: &std::sync::Arc<crate::services::dsh_control::auth::DshAuthService>,
    instance_id: &str,
    binding_id: &str,
    expected_old_fingerprint: &str,
) -> Result<(), String> {
    let selected = registry.selected().map_err(coded)?;
    if selected
        .as_ref()
        .map(|selected| selected.instance_id.as_str())
        != Some(instance_id)
    {
        return Err(coded(DshError::new(
            codes::AUTH_SIGNATURE,
            "selected instance changed",
        )));
    }
    auth.approve_rebind(instance_id, binding_id, expected_old_fingerprint)
        .await
        .map_err(coded)
}

/// `dsh_binding_revoke` 核心（T06 真实实现）：关受信 Peer + 清凭据 + 禁重连。
pub async fn binding_revoke_core(
    auth: &std::sync::Arc<crate::services::dsh_control::auth::DshAuthService>,
    binding_id: &str,
) -> Result<(), String> {
    auth.revoke(binding_id).await.map_err(coded)
}

// ============================================================
// Tauri 命令壳（九命令逐字；生产单例 GREEN 接线）
// ============================================================

/// 本机管理状态（含 autoAdaptEnabled 叠加）。
///
/// **R09**：授权域启动期初始化失败时，状态面照常返回（不整块消失），但把
/// 具体原因码叠加成 `installation = Broken` + `reasonCode`——卡面显示「域不可用」
/// 与既有的安装/修复按钮（修复出口）。失败标记是进程级单次判定的结果，不重试。
#[tauri::command]
pub async fn dsh_remote_status() -> Result<DshRemoteStatusDto, String> {
    // I1 终审收尾：bindingId / authorization（manager 装配）/ 失败原因**同一域
    // 解析**——production_manager 与 binding_auth 共用 binding_domain 判定；
    // 域初始化失败（无论 legacy 还是实例域）都如实叠加降级原因（R09 口径）。
    let binding_auth = crate::services::dsh_control::production::binding_auth();
    let binding_id = binding_auth
        .as_ref()
        .ok()
        .and_then(|svc| svc.active_binding_id());
    // H05 审查 C3：管理器装配/登记读取失败原样上抛（不静默回落 legacy——
    // 那会在已选实例时操作错误的目标）。
    let mut dto = remote_status_core(
        production_manager()?.as_ref(),
        current_auto_adapt(),
        binding_id,
    )?;
    if let Err(e) = &binding_auth {
        apply_domain_failure(&mut dto, e.code);
    }
    Ok(dto)
}

/// 本机候选实例列表（只报事实；不自动登记、不自动选择）。
#[tauri::command]
pub async fn dsh_instance_list() -> Result<Vec<DshInstanceCandidateDto>, String> {
    tokio::task::spawn_blocking(|| {
        let env = crate::services::dsh_control::production::production_env();
        let registry =
            crate::services::dsh_control::production::production_registry().map_err(coded)?;
        instance_list_core(&env, &registry)
    })
    .await
    .map_err(|error| format!("DSH_IPC_STATE: instance discovery task failed: {error}"))?
}

/// 明确选择实例（稳定 instanceId；不接受路径/命令）。
#[tauri::command]
pub async fn dsh_instance_select(request: DshInstanceRequest) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        let env = crate::services::dsh_control::production::production_env();
        let registry =
            crate::services::dsh_control::production::production_registry().map_err(coded)?;
        instance_select_core(
            &env,
            &registry,
            request,
            // P1-3：切换后开关驱动器补位（生产装配形状；fail-soft）。
            &crate::services::dsh_control::production::resume_switch_driver_after_select,
        )
    })
    .await
    .map_err(|error| format!("DSH_IPC_STATE: instance selection task failed: {error}"))?
}

/// 按持久选择（或显式同一 instanceId）查询该实例的管理状态。
#[tauri::command]
pub async fn dsh_instance_status(
    request: Option<DshInstanceRequest>,
) -> Result<serde_json::Value, String> {
    tokio::task::spawn_blocking(move || {
        let env = crate::services::dsh_control::production::production_env();
        let registry =
            crate::services::dsh_control::production::production_registry().map_err(coded)?;
        // 状态与安装/取消命令共用缓存单例。
        let services = |id: &str| crate::services::dsh_control::production::instance_services(id);
        instance_status_core_with(&env, &registry, request, &services)
    })
    .await
    .map_err(|error| format!("DSH_IPC_STATE: instance status task failed: {error}"))?
}

/// 受控重开：**必须由用户明确点击**，只启动已核验实例对应的 Desktop 应用。
///
/// 不接受路径/命令参数——目标来自 identity 单源解析；实例未核验或不在场即拒绝。
/// 本命令只做「启动应用」，从不等候、不强杀、不代为退出。
#[tauri::command]
pub async fn dsh_restart_host(request: DshInstanceRequest) -> Result<(), String> {
    let env = crate::services::dsh_control::production::production_env();
    let registry =
        crate::services::dsh_control::production::production_registry().map_err(coded)?;
    dsh_restart_host_core(&env, &registry, request)
}

pub fn dsh_restart_host_core(
    env: &crate::services::dsh_control::model::DshEnvironment,
    registry: &crate::services::dsh_control::instances::InstanceRegistry,
    request: DshInstanceRequest,
) -> Result<(), String> {
    // 只允许当前选择：不能借「重开」切换目标。
    let selected = registry.selected().map_err(coded)?.ok_or_else(|| {
        DshError::new(codes::TARGET_AMBIGUOUS, "no instance selected").to_message()
    })?;
    if selected.instance_id != request.instance_id {
        return Err(DshError::new(
            codes::INSTANCE_SWITCHED,
            "restart must target the selected instance",
        )
        .to_message());
    }
    // 重开发生在「按提示先退出 DSH」之后：此时目标必然不在场，不能要求活体。
    // 用**已登记身份**（kind + 规范化 profile → 派生 ID）证明目标未变；
    // 身份不符则拒绝，绝不按 PID/路径猜。
    if crate::services::dsh_control::instances::instance_id_for(
        selected.kind,
        &selected.profile_dir,
    ) != request.instance_id
    {
        return Err(DshError::new(
            codes::INSTANCE_SWITCHED,
            "registered instance identity changed; re-select required",
        )
        .to_message());
    }
    let registered = registry.registered(&request.instance_id).map_err(coded)?;
    if registered.profile_dir != selected.profile_dir || registered.kind != selected.kind {
        return Err(DshError::new(
            codes::INSTANCE_SWITCHED,
            "registered instance profile changed; re-select required",
        )
        .to_message());
    }
    // 把应用绑定到**该实例的 home**：M06 统一判定——home 必须可证明（登记值
    // 自洽，或经可信绑定账本唯一恢复）。只剩物理路径的旧记录不再从物理 profile
    // 反推 home：直连与外链在该形态下不可区分，反推会重开到另一个实例的会话。
    let physical_profile = std::fs::canonicalize(&registered.profile_dir).map_err(|_| {
        coded(DshError::new(
            codes::INSTANCE_UNVERIFIED,
            format!(
                "instance {} registered profile is missing; start the original instance and re-select it",
                registered.instance_id
            ),
        ))
    })?;
    let instance_home = match crate::services::dsh_control::instances::resolve_registered_home(
        Some(env),
        &registered,
        &physical_profile,
    ) {
        crate::services::dsh_control::instances::RegisteredHomeResolution::Proven(home) => home,
        crate::services::dsh_control::instances::RegisteredHomeResolution::Unproven(reason) => {
            return Err(coded(DshError::new(
                codes::INSTANCE_UNVERIFIED,
                format!(
                    "instance {} dsh home unproven ({}); start the original instance and re-select it",
                    registered.instance_id, reason
                ),
            )));
        }
    };
    {
        let logical = instance_home
            .join("profiles")
            .join(registered.kind.as_str());
        if std::fs::canonicalize(&logical).ok()
            != std::fs::canonicalize(&registered.profile_dir).ok()
            || !logical.exists()
        {
            return Err(coded(DshError::new(
                codes::INSTANCE_UNVERIFIED,
                "registered home no longer identifies the profile",
            )));
        }
    }
    // P2：**按宿主类型分流，且必须在解析 Desktop 安装之前**。
    //
    // 旧实现不分 kind 一律往下走 `resolve_installation` + `spawn_desktop_app`：
    // 选中的是 Web 实例时，「重开」会去解析 Desktop 安装（拿不到就报「desktop
    // profile 缺失」这种与目标无关的错误），拿到就真的拉起 Desktop 应用——启动
    // 了一个用户没在等的程序，而且它的 home 可能还是别的实例。
    //
    // Web 宿主只能由用户在自己终端里跑 `dsh web`（那是他们的进程与参数）；我们
    // 给出可复制的 home 与理由，不替他们起进程。
    if selected.kind == crate::services::dsh_control::instances::HostKind::Web {
        return Err(DshError::new(
            codes::UNSUPPORTED,
            format!(
                "web instances are started by the user's own `dsh web` with DSH_HOME={}; this app does not launch web hosts",
                instance_home.display()
            ),
        )
        .to_message());
    }
    // 只启动与该实例同一套安装（不接受调用方给出的路径）。profile 相关核验
    // 已在上方对**所选实例的 profile** 完成（统一判定 + 逻辑槽位对位）；这里
    // 只解析可执行文件——`resolve_installation` 的默认 profile 官方标记门会
    // 让「所选实例 profile 正常、默认 profile 未初始化」的重开被无关状态挡住
    // （复审 P2），故用无 profile 门的 `resolve_cli`。
    let installation = crate::services::dsh_control::identity::resolve_cli(env).map_err(coded)?;
    crate::services::dsh_control::production::spawn_desktop_app(
        &installation.executable,
        &instance_home,
    )
    .map_err(coded)
}

/// 启动固定包安装作业（返回 job_id）。R2 开关门：开关持有生命周期期间拒绝
/// 绕过直调（用开关；修复走 `dsh_switch_repair`）。
#[tauri::command]
pub async fn dsh_plugin_install() -> Result<String, String> {
    lifecycle_gate_for_selected().await?;
    plugin_start_core(production_manager()?.as_ref(), ManagementKind::Install)
}

/// 启动固定包修复作业（返回 job_id）。R2 开关门同安装（修复入口 =
/// `dsh_switch_repair`）。
#[tauri::command]
pub async fn dsh_plugin_repair() -> Result<String, String> {
    lifecycle_gate_for_selected().await?;
    plugin_start_core(production_manager()?.as_ref(), ManagementKind::Repair)
}

/// 启动固定包卸载作业（返回 job_id）。R2 开关门：关闭必须经开关（同步失效
/// 段 + 自动卸载）；开关未持有的实例保留直调清理口。
#[tauri::command]
pub async fn dsh_plugin_uninstall() -> Result<String, String> {
    lifecycle_gate_for_selected().await?;
    plugin_start_core(production_manager()?.as_ref(), ManagementKind::Uninstall)
}

/// 按 job_id 查询管理作业。
#[tauri::command]
pub async fn dsh_plugin_job(job_id: String) -> Result<ManagementJob, String> {
    plugin_job_core(production_manager()?.as_ref(), &job_id)
}

/// 取消管理作业（阶段检查点生效）。
#[tauri::command]
pub async fn dsh_plugin_cancel(job_id: String) -> Result<ManagementJob, String> {
    plugin_cancel_core(production_manager()?.as_ref(), &job_id)
}

/// 发起本机绑定（幂等启动本机 IPC 服务；有待批连接则返回挑战 + 双端核对面）。
/// R2 开关门：开关未开启时拒绝（远控授权只能经开关激活）。
#[tauri::command]
pub async fn dsh_binding_start() -> Result<DshBindingStartDto, String> {
    // I1 终审：入口与发送通道同一域判定（选定/唯一登记 → 实例域；无登记 → legacy）
    authorization_gate_for_selected().await?;
    let auth = crate::services::dsh_control::production::binding_auth().map_err(coded)?;
    auth.serve().await.map_err(coded)?;
    let mut result = binding_start_core(&auth)?;
    if auth.instance_scope_id().is_some() {
        if let Some(selected) = crate::services::dsh_control::production::production_registry()
            .map_err(coded)?
            .selected()
            .map_err(coded)?
        {
            if Some(selected.instance_id.as_str()) == auth.instance_scope_id()
                && !selected.identity_fingerprint.is_empty()
            {
                result.old_fingerprint = Some(selected.identity_fingerprint);
            }
        }
    }
    Ok(result)
}

/// **一键启用**（H10）：布防一次性授权意图，让下一个通过全部核验的新钥握手
/// 被自动完成绑定。
///
/// 为什么需要它：H10 的产品语义是「点一次就配好」，而此前 `begin_authorization_intent`
/// 在生产链上**没有任何调用者**——用户必须手动走「安装 → 绑定 → 核对码 → 批准」
/// 四步，且一旦离开页面就得重来。
///
/// 语义边界（照抄内核契约，不在这里放宽）：
/// * 意图 TTL = 一个挑战窗口（60 秒），**只消费一次**；
/// * 消费前提是对方通过 OS 对端身份 + 固定域 + 签名挑战全部核验——
///   「布防」不等于「免核验」，只是省掉最后一次人工点击；
/// * 过期后落回人工批准路径（语义不变）。
pub async fn dsh_binding_arm_intent_core<S>(
    env: &crate::services::dsh_control::model::DshEnvironment,
    registry: &std::sync::Arc<crate::services::dsh_control::instances::InstanceRegistry>,
    request: DshInstanceRequest,
    services: &S,
) -> Result<(), String>
where
    S: InstanceServices + Clone + Send + Sync + 'static,
{
    use crate::services::dsh_control::model::{ActivationState, InstallationState};
    let env = env.clone();
    let registry_for_probe = std::sync::Arc::clone(registry);
    let request_for_probe = request.clone();
    let services = services.clone();
    let (auth, revision, host_epoch, switch_generation) = tokio::task::spawn_blocking(move || {
        let (selected, revision) = registry_for_probe.selection_snapshot().map_err(coded)?;
        let selected = selected.ok_or_else(|| {
            DshError::new(codes::TARGET_AMBIGUOUS, "no instance selected").to_message()
        })?;
        if selected.instance_id != request_for_probe.instance_id {
            return Err(DshError::new(
                codes::INSTANCE_SWITCHED,
                "requested instance is not selected",
            )
            .to_message());
        }
        let candidate = crate::services::dsh_control::identity::discover_instance_candidate(
            &env,
            &request_for_probe.instance_id,
        )
        .map_err(coded)?;
        if !candidate.verified
            || candidate.kind != selected.kind
            || candidate.profile_dir != selected.profile_dir
        {
            return Err(DshError::new(
                codes::INSTANCE_UNVERIFIED,
                "selected instance is not verified",
            )
            .to_message());
        }
        let (manager, auth) = services
            .services(&request_for_probe.instance_id)
            .map_err(coded)?;
        let status = manager.status().map_err(coded)?;
        if status.installation != InstallationState::Installed
            || status.activation != ActivationState::Running
        {
            return Err(DshError::new(
                codes::HOST_NOT_RUNNING,
                "selected instance plugin is not installed and running",
            )
            .to_message());
        }
        let package = selected
            .profile_dir
            .join("node_modules/@tuvis/dsh-remote-access");
        let manifest = env
            .files
            .read_small(&package.join("package.json"), 1024 * 1024)
            .map_err(coded)?;
        let manifest: serde_json::Value = serde_json::from_slice(&manifest).map_err(|_| {
            DshError::new(codes::INSTANCE_UNVERIFIED, "plugin manifest is invalid").to_message()
        })?;
        let embedded = crate::services::dsh_control::install::embedded_manifest().map_err(coded)?;
        let entry = package.join("lib/index.js");
        if manifest.get("name").and_then(|value| value.as_str())
            != Some(crate::services::dsh_control::install::PACKAGE_NAME)
            || manifest.get("version").and_then(|value| value.as_str())
                != Some(embedded.plugin_version.as_str())
            || !env.files.is_readable_regular_file(&entry)
            || env
                .files
                .read_small(&entry, 8 * 1024 * 1024)
                .map_err(coded)?
                .is_empty()
        {
            return Err(DshError::new(
                codes::INSTANCE_UNVERIFIED,
                "installed plugin package is incomplete or incompatible",
            )
            .to_message());
        }
        // P1-1：人工布防同样绑定开关操作代次（布防时持久化记录的 generation）——
        // 关闭→再开启换代后的迟到握手在授权提交点同样被拒。记录缺席（壳层授权门
        // 已拒的不可达态）保守置 None（提交点只按 enabled 判定）。
        let switch_generation =
            crate::services::dsh_control::switch::persisted_record(&request_for_probe.instance_id)
                .map(|record| record.generation);
        Ok::<_, String>((auth, revision, candidate.host.epoch, switch_generation))
    })
    .await
    .map_err(|_| {
        DshError::new(codes::IPC_STATE, "intent verification task failed").to_message()
    })??;
    auth.serve().await.map_err(coded)?;
    let registry = std::sync::Arc::clone(registry);
    tokio::task::spawn_blocking(move || {
        auth.begin_selected_authorization_intent_generation(
            registry,
            &request.instance_id,
            revision,
            host_epoch,
            switch_generation,
        )
        .map_err(coded)
    })
    .await
    .map_err(|_| DshError::new(codes::IPC_STATE, "intent arm task failed").to_message())?
}

/// Tauri 命令壳：一键启用（布防一次性意图）。
///
/// 域判定与 `dsh_binding_start` 完全一致（`binding_auth()` 单源）：选定/唯一登记
/// → 实例域；无登记 → legacy。不另建第二套选择逻辑。
#[tauri::command]
pub async fn dsh_binding_arm_intent(request: DshInstanceRequest) -> Result<(), String> {
    // R2 开关门：布防授权意图必须在开关开启窗口内（开关自身经服务内部布防）。
    authorization_gate_for_selected().await?;
    let (env, registry) = tokio::task::spawn_blocking(|| {
        Ok::<_, String>((
            crate::services::dsh_control::production::production_env(),
            crate::services::dsh_control::production::production_registry().map_err(coded)?,
        ))
    })
    .await
    .map_err(|_| DshError::new(codes::IPC_STATE, "intent setup task failed").to_message())??;
    let services = |id: &str| crate::services::dsh_control::production::instance_services(id);
    dsh_binding_arm_intent_core(&env, &registry, request, &services).await
}

/// 用户批准绑定（T06 文档化扩展命令；批准后钉对端公钥并放行）。
/// R2 开关门：开关未开启时拒绝。
#[tauri::command]
pub async fn dsh_binding_approve(binding_id: String) -> Result<(), String> {
    authorization_gate_for_selected().await?;
    crate::services::dsh_control::production::binding_auth()
        .map_err(coded)?
        .approve(&binding_id)
        .await
        .map_err(coded)
}

/// 用户明确确认更换已撤销的插件身份。R2 开关门：开关未开启时拒绝
/// （再开启属于一次新的明确授权，只能经开关发起）。
#[tauri::command]
pub async fn dsh_binding_rebind(
    instance_id: String,
    binding_id: String,
    expected_old_fingerprint: String,
) -> Result<(), String> {
    authorization_gate_for_selected().await?;
    let registry =
        crate::services::dsh_control::production::production_registry().map_err(coded)?;
    let auth = crate::services::dsh_control::production::binding_auth().map_err(coded)?;
    binding_rebind_core(
        &registry,
        &auth,
        &instance_id,
        &binding_id,
        &expected_old_fingerprint,
    )
    .await
}

/// 撤销绑定（关受信 Peer、清凭据、禁自动重连）。R2 开关门：开关持有生命周期
/// 期间拒绝直调（关闭 = 开关的同步失效段；开关未持有则保留清理口）。
#[tauri::command]
pub async fn dsh_binding_revoke(binding_id: String) -> Result<(), String> {
    lifecycle_gate_for_selected().await?;
    // I1 终审：撤权与入口/通道同一域判定——撤错域会留下别的实例活绑定
    crate::services::dsh_control::production::binding_auth()
        .map_err(coded)?
        .revoke(&binding_id)
        .await
        .map_err(coded)
}

/// 写 DSH 自动适配开关（显式两值；返回生效值）。
#[tauri::command]
pub async fn dsh_set_auto_adapt(enabled: bool) -> Result<bool, String> {
    set_auto_adapt_core(enabled)
}

// ============================================================
// Task 4：单开关「远程控制 DSH」（后端持久状态机；前端只消费实际状态）
// ============================================================

use crate::services::dsh_control::switch::{
    gate_authorization, gate_lifecycle, SwitchRecord, SwitchView,
};

/// 目标解析（开关命令共用）：显式请求必须等于持久选择；无选择 = 先选择。
fn switch_target(
    registry: &crate::services::dsh_control::instances::InstanceRegistry,
    request: Option<&DshInstanceRequest>,
) -> Result<String, String> {
    let selected = registry.selected().map_err(coded)?.ok_or_else(|| {
        DshError::new(codes::TARGET_AMBIGUOUS, "no instance selected").to_message()
    })?;
    if let Some(request) = request {
        if request.instance_id != selected.instance_id {
            return Err(DshError::new(
                codes::INSTANCE_SWITCHED,
                "requested instance is not the selected one",
            )
            .to_message());
        }
    }
    Ok(selected.instance_id)
}

/// 读取实例开关账本（生产缓存服务）。**缺席语义（含读取失败 → None）由调用方
/// 的门定义**：授权门按未开启拒绝（fail-closed），生命周期门按未持有放行——
/// 读取口径单点在 `switch::persisted_record`（P1-1 起命令壳与生产执行门共用）。
fn switch_record_for(instance_id: &str) -> Option<SwitchRecord> {
    crate::services::dsh_control::switch::persisted_record(instance_id)
}

/// 生命周期门（按持久选择；**真无选择** = legacy 域放行）。R2：开关持有生命
/// 周期期间，公开 IPC 不得绕过开关直调安装/修复/卸载/撤权。记录缺席（含读取
/// 失败 → None）原样交给纯函数判定——开关未持有生命周期时放行。
async fn lifecycle_gate_for_selected() -> Result<(), String> {
    tokio::task::spawn_blocking(|| {
        let registry =
            crate::services::dsh_control::production::production_registry().map_err(coded)?;
        match registry.selected().map_err(coded)? {
            Some(selected) => {
                gate_lifecycle(switch_record_for(&selected.instance_id).as_ref()).map_err(coded)
            }
            None => Ok(()),
        }
    })
    .await
    .map_err(|error| format!("{}: switch gate task failed: {error}", codes::IPC_STATE))?
}

/// 授权激活门（按持久选择；**真无选择** = legacy 域放行）。R2：开关未开启时，
/// 公开 IPC 不得绕过开关激活远控（绑定/批准/布防/重绑）。**记录缺席（含读取
/// 失败 → None）同样拒绝**——「选定但从未物化 Off 记录」的实例不得成为绕过窗
/// （复审 Important-1；契约与 `gate_functions_reflect_switch_states` 同一口径）。
async fn authorization_gate_for_selected() -> Result<(), String> {
    tokio::task::spawn_blocking(|| {
        let registry =
            crate::services::dsh_control::production::production_registry().map_err(coded)?;
        match registry.selected().map_err(coded)? {
            Some(selected) => {
                gate_authorization(switch_record_for(&selected.instance_id).as_ref()).map_err(coded)
            }
            None => Ok(()),
        }
    })
    .await
    .map_err(|error| format!("{}: switch gate task failed: {error}", codes::IPC_STATE))?
}

/// `dsh_switch_get`：当前选定实例的开关状态外表（含实时就绪结果）。
#[tauri::command]
pub async fn dsh_switch_get(request: Option<DshInstanceRequest>) -> Result<SwitchView, String> {
    tokio::task::spawn_blocking(move || {
        let registry =
            crate::services::dsh_control::production::production_registry().map_err(coded)?;
        let instance_id = switch_target(&registry, request.as_ref())?;
        crate::services::dsh_control::production::switch_for_instance(&instance_id)
            .map_err(coded)?
            .view()
            .map_err(coded)
    })
    .await
    .map_err(|error| format!("{}: switch status task failed: {error}", codes::IPC_STATE))?
}

/// `dsh_switch_enable`：开启（本次安装 + 实例级授权意图；线性推进由后端驱动）。
#[tauri::command]
pub async fn dsh_switch_enable(request: DshInstanceRequest) -> Result<SwitchView, String> {
    tokio::task::spawn_blocking(move || {
        let registry =
            crate::services::dsh_control::production::production_registry().map_err(coded)?;
        let instance_id = switch_target(&registry, Some(&request))?;
        crate::services::dsh_control::production::switch_for_instance(&instance_id)
            .map_err(coded)?
            .enable()
            .map_err(coded)
    })
    .await
    .map_err(|error| format!("{}: switch enable task failed: {error}", codes::IPC_STATE))?
}

/// `dsh_switch_disable`：关闭（同步失效段 + 自动卸载；等待宿主正常退出）。
#[tauri::command]
pub async fn dsh_switch_disable(request: DshInstanceRequest) -> Result<SwitchView, String> {
    tokio::task::spawn_blocking(move || {
        let registry =
            crate::services::dsh_control::production::production_registry().map_err(coded)?;
        let instance_id = switch_target(&registry, Some(&request))?;
        crate::services::dsh_control::production::switch_for_instance(&instance_id)
            .map_err(coded)?
            .disable()
            .map_err(coded)
    })
    .await
    .map_err(|error| format!("{}: switch disable task failed: {error}", codes::IPC_STATE))?
}

/// `dsh_switch_repair`：修复插件（只作用于当前实例；完成后自动接续原流程）。
#[tauri::command]
pub async fn dsh_switch_repair(request: DshInstanceRequest) -> Result<SwitchView, String> {
    tokio::task::spawn_blocking(move || {
        let registry =
            crate::services::dsh_control::production::production_registry().map_err(coded)?;
        let instance_id = switch_target(&registry, Some(&request))?;
        crate::services::dsh_control::production::switch_for_instance(&instance_id)
            .map_err(coded)?
            .repair()
            .map_err(coded)
    })
    .await
    .map_err(|error| format!("{}: switch repair task failed: {error}", codes::IPC_STATE))?
}

/// 生产管理器入口（装配单点在 `services::dsh_control::production`；壳层持 Arc
/// 调核心，测试不经此路径——测试注入 manager 直调核心）。
///
/// H05 审查 C3：`Result`——真无选择才回落 legacy 兼容单例；选择/登记读取失败与
/// 按实例装配失败原样上抛（coded 给前端），绝不静默换域。
fn production_manager() -> Result<std::sync::Arc<DshPluginManager>, String> {
    crate::services::dsh_control::production::production_manager().map_err(coded)
}

// ============================================================
// IPC 契约自检（九命令逐字注册；源码级锁，tps.rs 同款）
// ============================================================
#[cfg(test)]
mod tests {
    /// 命令注册表所在文件（generate_handler）
    const LIB_SRC: &str = include_str!("../lib.rs");

    /// §2.4 九命令逐字注册恰好一次；Task 4 文档化扩展命令（单开关）同锁。
    #[test]
    fn nine_commands_are_registered_once() {
        for cmd in [
            "commands::dsh::dsh_remote_status",
            "commands::dsh::dsh_plugin_install",
            "commands::dsh::dsh_plugin_repair",
            "commands::dsh::dsh_plugin_uninstall",
            "commands::dsh::dsh_plugin_job",
            "commands::dsh::dsh_plugin_cancel",
            "commands::dsh::dsh_binding_start",
            "commands::dsh::dsh_binding_arm_intent",
            "commands::dsh::dsh_binding_revoke",
            "commands::dsh::dsh_set_auto_adapt",
            // Task 4 文档化扩展（单开关全生命周期）。
            "commands::dsh::dsh_switch_get",
            "commands::dsh::dsh_switch_enable",
            "commands::dsh::dsh_switch_disable",
            "commands::dsh::dsh_switch_repair",
        ] {
            assert_eq!(
                LIB_SRC.matches(cmd).count(),
                1,
                "{cmd} 必须在 generate_handler 注册恰好一次"
            );
        }
    }

    /// 三态口径：缺键/乱串 = 缺省开；显式两值一律尊重。
    #[test]
    fn auto_adapt_three_state_defaults_on() {
        assert_eq!(super::auto_adapt_setting_from(None), None);
        assert_eq!(super::auto_adapt_setting_from(Some("  ")), None);
        assert_eq!(super::auto_adapt_setting_from(Some("garbage")), None);
        assert_eq!(super::auto_adapt_setting_from(Some("true")), Some(true));
        assert_eq!(super::auto_adapt_setting_from(Some("false")), Some(false));
        assert!(super::auto_adapt_enabled_from(None), "缺省开");
        assert!(
            !super::auto_adapt_enabled_from(Some("false")),
            "显式关被尊重"
        );
        assert!(
            super::auto_adapt_enabled_from(Some("nonsense")),
            "乱串视同未写"
        );
    }
}
