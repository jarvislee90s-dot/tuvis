//! 固定包（`@tuvis/dsh-remote-access`）安装/修复/卸载后台作业（T04，§2.3
//! `DshPluginManager`）。
//!
//! 设计要点（§2.5 装配顺序 + 计划全局约束 + Mac 补探测 §2.1/§2.3 实测）：
//! * **目标单源**：安装/profile/宿主判定全部走 [`identity`]（`resolve_installation`
//!   拿自带 CLI，`discover` 判宿主在否），本模块不自建探测；
//! * **等待门**：宿主在场 → 作业停 `WaitingForDesktopExit`（忙态显示等待用户
//!   退出），轮询进程源直到退出；**绝不 kill/重启/关应用**，也绝不阻塞 `start()`
//!   调用方（返回 job_id 即返）；
//! * **`--profile` 传名称不是路径**（审查 R01）：官方 `resolveProfileDir(name, home)`
//!   只接受名称并拒绝任何路径分隔符，profile 目录永远是 `$DSH_HOME/profiles/<name>`。
//!   故每次调用都传 [`PROFILE_NAME`]，并把发现到的 DSH home 以 **`DSH_HOME`**
//!   环境覆盖随请求下发（父进程未必设过该变量，靠继承会落到另一套 profile）；
//! * **独立 args 调官方 CLI**：`plugin --profile desktop add|list|remove`，绝不拼
//!   shell 字符串；生产 runner 复用 [`crate::remote::NoWindow`] 抑制 Windows
//!   控制台闪窗（环境覆盖面见 [`super::model::CommandRequest`]）；
//! * **无「exit 0 即成功」**（审查 R02）：核验按官方真实的**四态**契约
//!   （Mac 报告 §2.3）——① tgz sha256 == manifest；② profile `package.json`
//!   的 `dependencies[包名]` **且** `dsh.profile.bundles` 含包名；③
//!   `node_modules/<包名>/package.json` 存在且版本匹配；④ `plugin list` 含
//!   `包名@版本`。**不读 `cordis.yml`**（官方 CLI 从不写它，卸载后空目录还残留
//!   `node_modules/@tuvis/`，都不能当判据）；
//! * **稳定版本化缓存**（审查「新增事实」#2）：`add` 之前把嵌入 tgz 物化到
//!   `<state_dir>/plugin-cache/@tuvis-dsh-remote-access-<ver>.tgz`（兔维斯自有、
//!   跨应用更新存活），并从该路径安装——profile 里的 `file:` 依赖因此指向稳定
//!   位置，而不是临时目录或会随更新消失的 app 内路径；缺失/哈希不符时由嵌入
//!   字节重新同步；
//! * **自有指纹账本**：缓存文件的 SHA-256 记入 `<TUVIS_HOME>/dsh/management.json`
//!   （原子替换、非敏感字段）；卸载前对账，用户改动 → `DSH_PACKAGE_CONFLICT`
//!   如实上报，绝不静默覆盖/回灌整文件快照（官方恢复行为保留）；
//! * **卸载先撤绑定**（§2.5 序固定）：`BindingRevoker::revoke_profile` 先行，
//!   随后任何失败都保持撤权已发生；测试注入显式空存储，生产（T06 接线）不得
//!   对已存在绑定静默跳过撤销；
//! * **卸载完成退役插件身份**（P1-2 同钥循环复装）：撤权已证 + 包移除成功后
//!   一并删除该实例的插件身份目录（`<dshHome>/tuvis-remote/<instanceId>/` 及
//!   桌面 legacy `<dshHome>/profiles/desktop/tuvis-remote/`——兔维斯自有命名
//!   空间）并清空实例登记的指纹锚（注入 [`IdentityRetirer`]）；删除失败 →
//!   卸载报失败可重试（旧钥在场绝不谎报成功——重装读回旧钥会被「插件本地
//!   revoked 标志 + 撤权墓碑」双重拒绝，再开启永远连不上）。升级兼容：撤权在证
//!   + 旧身份在场时，安装/修复路径装新代**之前**先退役旧身份；
//! * **任务锁按 profile**：活跃作业期间重复 `start()`（同类双击/异类连点）一律
//!   复用同一 job_id，绝不并行建作业；
//! * **取消**：阶段间检查点（等待轮询 / Applying 后 / Verifying 后）；取消的
//!   作业报 `Cancelled`，绝不报 `Succeeded`。
//!
//! 持久化说明：`management.json` 与插件缓存是**兔维斯自有数据面**（TUVIS_HOME
//! 之下），与写入同世界用 std::fs 读写（测试注入 tempdir，绝不触真实
//! `~/.tuvis`）；DSH 侧事实（profile 文件态、安装面）只经注入的只读文件源。

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::error::{codes, DshError};
use super::identity;
use super::instances::HostKind;
use super::model::{
    ActivationState, AuthorizationState, CommandRequest, DshEnvironment, InstallationState,
    ManagementJob, ManagementKind, ManagementPhase, ManagementStatus, RunOutcome,
};

// ============================================================
// 嵌入资源与固定常量
// ============================================================

/// 固定包名（官方 CLI `list`/`remove` 的定位词；管理命令不接受任意包名）。
pub const PACKAGE_NAME: &str = "@tuvis/dsh-remote-access";

/// desktop profile 名称（官方 `resolveProfileDir` 的入参**只能是名称**——
/// 审查 R01：传绝对路径会被拒绝，install/repair/list/remove 全部做不成）。
pub const PROFILE_NAME: &str = "desktop";

/// web profile 名称（H05：Web 实例的 `--profile` 入参；H02 字段表实测形态
/// `<home>/profiles/web`）。Desktop profile 归 Electron 所有、npm CLI 不得碰，
/// 两个实例域的 profile 名从此互不越界。
pub const WEB_PROFILE_NAME: &str = "web";

/// 把 CLI 绑定到已发现 DSH home 的环境变量（官方 `dsh-home-paths` 的优先级：
/// 显式配置 > `$DSH_HOME` > `~/.dsh`）。
pub const DSH_HOME_ENV: &str = "DSH_HOME";

/// 嵌入固定包字节（`include_bytes!` 进二进制；路径模式同 `services/tps/install.rs`）。
pub const EMBEDDED_PACKAGE_BYTES: &[u8] =
    include_bytes!("../../../../resources/dsh/remote-access.tgz");

/// 嵌入包描述原文（`manifest.json`：版本/SHA-256/绑定协议版本）。
const EMBEDDED_PACKAGE_MANIFEST: &str = include_str!("../../../../resources/dsh/manifest.json");

/// 包级预算（计划全局约束「目录头 64 MiB」参考值；超出即拒，任何 CLI 调用前）。
const MAX_PACKAGE_BYTES: u64 = 64 * 1024 * 1024;
/// SHA-256 流式分块（嵌入字节也走流式哈希，与文件源大文件同一纪律）。
const HASH_CHUNK: usize = 64 * 1024;
/// profile 小文件读取预算（profile `package.json` / 插件 `package.json` 都是小文本）。
const PROFILE_READ_BUDGET: u64 = 1024 * 1024;
/// 等待桌面退出的轮询间隔（作业后台自转；测试以 job() 截止轮询观测，不依赖间隔值）。
const WAIT_POLL: Duration = Duration::from_millis(25);
/// 失败诊断 detail 的尾部截断（detail 只放英文机器诊断，不含凭据）。
const DETAIL_TAIL_CHARS: usize = 200;

/// 插件缓存子目录（兔维斯 state_dir 之下；跨应用更新存活）。
const PLUGIN_CACHE_DIR: &str = "plugin-cache";

/// 嵌入包描述（解析产物；字段与 `resources/dsh/manifest.json` 一一对应）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedManifest {
    pub plugin_version: String,
    pub sha256: String,
    pub binding_protocol: u32,
}

/// 解析嵌入描述（畸形描述 = `DSH_PACKAGE_INVALID`，绝不带着未知包去调 CLI）。
pub fn embedded_manifest() -> Result<EmbeddedManifest, DshError> {
    let value: serde_json::Value =
        serde_json::from_str(EMBEDDED_PACKAGE_MANIFEST).map_err(|e| {
            DshError::new(
                codes::PACKAGE_INVALID,
                format!("embedded dsh manifest is not valid JSON: {e}"),
            )
        })?;
    Ok(EmbeddedManifest {
        plugin_version: value["pluginVersion"]
            .as_str()
            .ok_or_else(|| DshError::new(codes::PACKAGE_INVALID, "manifest missing pluginVersion"))?
            .to_string(),
        sha256: value["sha256"]
            .as_str()
            .ok_or_else(|| DshError::new(codes::PACKAGE_INVALID, "manifest missing sha256"))?
            .to_string(),
        binding_protocol: value["bindingProtocol"].as_u64().ok_or_else(|| {
            DshError::new(codes::PACKAGE_INVALID, "manifest missing bindingProtocol")
        })? as u32,
    })
}

// ============================================================
// 撤销绑定注入面（§2.5）
// ============================================================

/// 绑定撤销面（§2.5 装配顺序：T04 独立实现 manager，注入 revoker）。
///
/// 卸载**必须先**撤销绑定再动包（序固定）；测试注入显式空存储。生产实现
/// （T06 接真实绑定存储）绝不允许对已存在的绑定静默跳过撤销。
pub trait BindingRevoker: Send + Sync {
    /// 按 profile 撤销本机绑定（profile_id = desktop profile 目录字符串）。
    fn revoke_profile(&self, profile_id: &str) -> Result<(), DshError>;
}

/// 插件身份退役面（P1-2 同钥循环复装）：旧钥退役（身份目录已删）后清空实例
/// 登记的指纹锚——再开启装出的全新钥走首绑 TOFU，不进重绑面板；旧钥由撤权
/// 墓碑拦截，不因锚清空复活。生产实现 = 清空 `InstanceRegistry` 的
/// `identity_fingerprint`；未注入 = 跳过登记清空（仅目录删除）。
pub type IdentityRetirer = Arc<dyn Fn(&str) -> Result<(), DshError> + Send + Sync>;

/// 授权状态读取面（T06：manager `status()` 的 authorization/bindingId 单一来源）。
/// 生产装配注入 auth 单例的观测适配；未注入（测试缺省）= Unbound / 无绑定 id。
pub trait DshAuthorizationReader: Send + Sync {
    /// 授权三档（Unbound | Bound | Revoked）。
    fn authorization_state(&self) -> AuthorizationState;
    /// 当前活跃（Bound）绑定 id（撤权按钮的真实 id 来源）。
    fn active_binding_id(&self) -> Option<String>;
}

// ============================================================
// 持久化账本（management.json：原子替换、非敏感字段）
// ============================================================

/// 自有文件指纹回执（owned 包的路径 + 安装时 SHA-256；用户改动对账用）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct OwnedFileReceipt {
    path: String,
    sha256: String,
}

/// 作业回执（审计面；phase/reason 与内存态同词）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JobReceipt {
    job_id: String,
    kind: ManagementKind,
    phase: ManagementPhase,
    reason_code: Option<String>,
    updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum CliOrigin {
    MacBundled { wrapper: PathBuf },
    Npm { entry: PathBuf },
}

/// 受理门调用上下文（复审 Major-2 收紧）：`allow_consumed_evidence` 仅由
/// 本作业已成功撤权（Uninstall）的 CLI 门置位——例外只覆盖「Bound 证据被
/// 自己消费」，活体冲突等其余不可核验原因不适用。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct AdmissionScope {
    pub(crate) allow_consumed_evidence: bool,
}

pub(crate) type AdmissionGate =
    Arc<dyn Fn(AdmissionScope) -> Result<std::path::PathBuf, DshError> + Send + Sync>;

struct CliProof {
    path: PathBuf,
    bundled_digest: Option<String>,
    npm_digest: Option<String>,
    job_guard: Option<Arc<JobGuard>>,
    /// 证据终核（复审 Major-1）：`run_cli` 在全部异步校验后、`runner.run` 前
    /// 调用——CLI 门通过后的准备/校验窗口内证据变化在此兜底。
    evidence_gate: Option<Arc<dyn Fn() -> Result<(), DshError> + Send + Sync>>,
}

struct JobGuard {
    logical_profile: PathBuf,
    #[cfg(unix)]
    physical: Option<super::profile_activity_lock::ProfileLock>,
    _fallback: Option<std::fs::File>,
}

impl JobGuard {
    fn validate(&self) -> Result<(), DshError> {
        let _ = &self.logical_profile;
        #[cfg(unix)]
        if let Some(lock) = &self.physical {
            lock.validate_target(&self.logical_profile)?;
        }
        Ok(())
    }
}

/// 解析 `mgmt-<clock>-<seq>` 作业 ID 的序号部分（非该形态 → `None`）。
fn job_seq_number(job_id: &str) -> Option<u64> {
    let rest = job_id.strip_prefix("mgmt-")?;
    let (_, suffix) = rest.rsplit_once('-')?;
    suffix.parse::<u64>().ok()
}

/// 是否终态（Succeeded/Failed/Cancelled）。
fn terminal_phase(phase: ManagementPhase) -> bool {
    matches!(
        phase,
        ManagementPhase::Succeeded | ManagementPhase::Failed | ManagementPhase::Cancelled
    )
}

/// 账本回执 → 对外作业（**仅终态**；非终态行可能是崩溃遗留，不冒充活跃作业）。
fn receipt_job_if_terminal(receipt: &JobReceipt) -> Option<ManagementJob> {
    terminal_phase(receipt.phase).then(|| ManagementJob {
        job_id: receipt.job_id.clone(),
        kind: receipt.kind,
        phase: receipt.phase,
        reason_code: receipt.reason_code.clone(),
    })
}

/// `<TUVIS_HOME>/dsh/management.json` 的形状（全部非敏感：本机路径 + 状态机词）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ManagementLedger {
    #[serde(default)]
    jobs: Vec<JobReceipt>,
    #[serde(default)]
    owned_files: Vec<OwnedFileReceipt>,
    #[serde(default)]
    plugin_version: Option<String>,
    #[serde(default)]
    cli_origin: Option<CliOrigin>,
    #[serde(default)]
    cli_pending: Option<CliOrigin>,
    #[serde(default)]
    cli_unresolved: bool,
}

fn state_dir(tuvis_home: &Path) -> PathBuf {
    tuvis_home.join("dsh")
}

/// 插件缓存目录（`<state_dir>/plugin-cache`；兔维斯自有、跨应用更新存活）。
fn plugin_cache_dir(tuvis_home: &Path) -> PathBuf {
    state_dir(tuvis_home).join(PLUGIN_CACHE_DIR)
}

/// H05 按实例的管理账本（管理目标不是 `tuvis_home/dsh/management.json`）。
/// 两个实例的作业回执、自有指纹与版本各记各的账（影子安装互不污染）。
fn instance_ledger_path(tuvis_home: &Path, instance_id: &str) -> PathBuf {
    state_dir(tuvis_home)
        .join("instances")
        .join(instance_id)
        .join("management.json")
}

fn ledger_path(tuvis_home: &Path) -> PathBuf {
    state_dir(tuvis_home).join("management.json")
}

/// 稳定版本化缓存路径
/// （`<TUVIS_HOME>/dsh/plugin-cache/@tuvis-dsh-remote-access-<ver>.tgz`）。
///
/// 为什么不是临时目录/app 内路径（审查「新增事实」#2）：官方 `add` 把**安装时刻
/// 的绝对路径**以 `file:` 形式钉进用户 profile；若该路径会随应用更新消失，
/// 「修复」以外的任何时刻都指向一个不存在的 tgz，可维护性承诺就不成立。
fn plugin_cache_path(tuvis_home: &Path, version: &str) -> PathBuf {
    plugin_cache_dir(tuvis_home).join(format!("{}-{version}.tgz", PACKAGE_NAME.replace('/', "-")))
}

/// 恢复记账回执（与正常成功记账同口径：自有指纹 = 稳定缓存路径 + manifest 哈希；
/// 版本不匹配/manifest 不可解析 → 空回执，调用方的核验会先行拒绝）。
fn bookkeeping_receipts(env: &DshEnvironment, version: &str) -> Vec<OwnedFileReceipt> {
    match embedded_manifest() {
        Ok(manifest) if manifest.plugin_version == version => {
            let pkg = plugin_cache_path(&env.tuvis_home, &manifest.plugin_version);
            vec![OwnedFileReceipt {
                path: pkg.to_string_lossy().to_string(),
                sha256: manifest.sha256,
            }]
        }
        _ => Vec::new(),
    }
}

/// 原子替换写（temp + rename；避免半截账本被读走）。
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), DshError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            DshError::new(
                codes::STATE_IO,
                format!("create_dir_all {}: {e}", parent.display()),
            )
        })?;
    }
    use std::io::Write;
    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp-{}-{sequence}", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|e| DshError::new(codes::STATE_IO, format!("create {}: {e}", tmp.display())))?;
    let result = file
        .write_all(bytes)
        .and_then(|()| std::fs::rename(&tmp, path));
    if let Err(error) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(DshError::new(
            codes::STATE_IO,
            format!("write {}: {error}", path.display()),
        ));
    }
    Ok(())
}

/// 仅供测试在账本比较通过后暂停提交；回调按路径筛选自身夹具。
#[cfg(feature = "dsh-test-probes")]
pub mod ledger_commit_probe {
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    pub type Callback = Arc<dyn Fn(&Path) + Send + Sync>;
    /// 写失败注入判据（直写 `Option<Arc<dyn Fn…>>` 会触发 type_complexity）。
    pub type FailWritePredicate = Arc<dyn Fn(&Path) -> bool + Send + Sync>;
    static CALLBACK: Mutex<Option<Callback>> = Mutex::new(None);
    static BEFORE_LOCK: Mutex<Option<Callback>> = Mutex::new(None);
    static FAIL_WRITE: Mutex<Option<FailWritePredicate>> = Mutex::new(None);

    pub fn set_before_lock(callback: Option<Callback>) {
        *BEFORE_LOCK.lock().unwrap() = callback;
    }

    pub fn set_fail_write(callback: Option<FailWritePredicate>) {
        *FAIL_WRITE.lock().unwrap() = callback;
    }

    /// 前 `successes` 次 persist 放行、其后对 `path` 的 persist 全部失败——
    /// 阶段化写失败注入：让 start 的初始回执成功、而作业内部的来源/阶段提交
    /// 失败（按提交序号计时，不是墙钟）。
    pub fn set_fail_write_after(path: &Path, successes: u32) {
        let path = path.to_path_buf();
        let remaining = Mutex::new(successes);
        let predicate: FailWritePredicate = Arc::new(move |candidate: &Path| {
            if candidate != path {
                return false;
            }
            let mut left = remaining.lock().unwrap();
            if *left > 0 {
                *left -= 1;
                false
            } else {
                true
            }
        });
        set_fail_write(Some(predicate));
    }

    pub(super) fn before_lock(path: &Path) {
        let callback = BEFORE_LOCK.lock().unwrap().clone();
        if let Some(callback) = callback {
            callback(path);
        }
    }

    pub(super) fn fail_write(path: &Path) -> bool {
        FAIL_WRITE
            .lock()
            .unwrap()
            .clone()
            .is_some_and(|callback| callback(path))
    }

    pub fn set(callback: Option<Callback>) {
        *CALLBACK.lock().unwrap() = callback;
    }

    pub(super) fn after_compare(path: &Path) {
        let callback = CALLBACK.lock().unwrap().clone();
        if let Some(callback) = callback {
            callback(path);
        }
    }
}

/// 一次读取同时保留账本及其原始代际；损坏或不可读不可当成首次安装。
fn load_ledger_at(path: &Path) -> (ManagementLedger, Option<Vec<u8>>, bool) {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(ledger) => (ledger, Some(bytes), false),
            Err(_) => (ManagementLedger::default(), Some(bytes), true),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            (ManagementLedger::default(), None, false)
        }
        Err(_) => (ManagementLedger::default(), None, true),
    }
}

// ============================================================
// 插件身份目录（兔维斯自有命名空间；P1-2 同钥循环复装）
// ============================================================

/// 插件身份目录列表（P1-2）：旧钥/信任锚/本地 revoked 标志的持久落点，删除 =
/// **退役**（信任物销毁面，绝不恢复）。
/// * 实例域：`<dshHome>/tuvis-remote/<instance_id>/`（插件
///   `instanceFileStorage` 的实例槽，hostId = 登记实例 ID）；
/// * legacy 桌面域：`<dshHome>/profiles/desktop/tuvis-remote/`（旧单例布局
///   逐字保留；env.home 按实例证明，桌面域目录不跨实例）。
///
/// **home 契约（Round-8 P1-3）**：`env.home` 必须是**登记核验的实例 DSH home**
/// （`instances::verified_instance_home`——安装管理器经 `init_instance_manager`
/// 装配即满足；开关探测入口在委托前覆写）。兔维斯侧推导 env.home 对自定义
/// DSH_HOME 宿主是错目录——探测与退役都会查错位置。
pub(crate) fn plugin_identity_dirs(
    env: &DshEnvironment,
    target: Option<&InstanceTarget>,
) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    match target {
        Some(target) => {
            dirs.push(env.home.join("tuvis-remote").join(&target.instance_id));
            if target.kind == HostKind::Desktop {
                dirs.push(
                    env.home
                        .join("profiles")
                        .join(PROFILE_NAME)
                        .join("tuvis-remote"),
                );
            }
        }
        None => dirs.push(
            env.home
                .join("profiles")
                .join(PROFILE_NAME)
                .join("tuvis-remote"),
        ),
    }
    dirs
}

/// 删除插件身份目录（P1-2 卸载完成段 / 升级兼容清理共用）：在场才删、整目录
/// 退役；任何删除失败如实上抛（调用方报作业失败可重试——旧钥在场绝不谎报成功）。
fn remove_plugin_identity_dirs(
    env: &DshEnvironment,
    target: Option<&InstanceTarget>,
) -> Result<(), DshError> {
    for dir in plugin_identity_dirs(env, target) {
        if !env.files.exists(&dir) {
            continue;
        }
        // 回归接缝（test-only unix）：按路径注入删除失败（可重试性回归）。
        #[cfg(all(test, unix))]
        if identity_removal_probe::fail(&dir) {
            return Err(DshError::new(
                codes::STATE_IO,
                format!("identity removal probe: refused {}", dir.display()),
            ));
        }
        std::fs::remove_dir_all(&dir).map_err(|error| {
            DshError::new(
                codes::STATE_IO,
                format!("remove plugin identity dir {}: {error}", dir.display()),
            )
        })?;
    }
    Ok(())
}

/// 回归接缝（test-only unix）：按路径注入插件身份目录删除失败
/// （P1-2 卸载可重试性回归；与 auth 域 `ledger_write_probe` 同形）。
#[cfg(all(test, unix))]
pub(crate) mod identity_removal_probe {
    use std::path::Path;
    use std::sync::{Arc, Mutex, OnceLock};

    type FailPredicate = Arc<dyn Fn(&Path) -> bool + Send + Sync>;

    fn fail_predicate() -> &'static Mutex<Option<FailPredicate>> {
        static PREDICATE: OnceLock<Mutex<Option<FailPredicate>>> = OnceLock::new();
        PREDICATE.get_or_init(|| Mutex::new(None))
    }

    pub fn set_fail(predicate: Option<FailPredicate>) {
        *fail_predicate().lock().unwrap() = predicate;
    }

    pub(super) fn fail(path: &Path) -> bool {
        fail_predicate()
            .lock()
            .unwrap()
            .clone()
            .is_some_and(|predicate| predicate(path))
    }
}

/// H05 按实例的管理目标：`--profile` 名与核验落点由 kind/profile_dir 决定，
/// 账本与就绪门按 instance_id 归域——同一进程可并行服务多实例的影子安装。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceTarget {
    pub instance_id: String,
    pub kind: HostKind,
    pub profile_dir: PathBuf,
}

impl InstanceTarget {
    /// 官方 CLI 的 `--profile` 入参名（名称而非路径——审查 R01）。
    pub fn profile_name(&self) -> &'static str {
        match self.kind {
            HostKind::Web => WEB_PROFILE_NAME,
            HostKind::Desktop => PROFILE_NAME,
        }
    }
}

// ============================================================
// 管理器内存态
// ============================================================

/// 作业内存记录（`cancelled` 是取消旗标：`cancel()` 置位，作业在阶段检查点消费）。
/// H05 审查 I5：web 目标的 CLI 源在 **start() 时**从活宿主 argv 捕获并随作业参数
/// 传入 drive_job（等待退出后进程表已无宿主，argv 捕获必须前移；作业不跨进程
/// 恢复，故只随任务参数走，不入账本回执）。
struct JobRecord {
    job: ManagementJob,
    profile_dir: PathBuf,
    cancelled: bool,
    updated_at_ms: i64,
}

impl JobRecord {
    /// 活跃 = 未达终态（Queued/Waiting/Applying/Verifying）。
    fn active(&self) -> bool {
        !matches!(
            self.job.phase,
            ManagementPhase::Succeeded | ManagementPhase::Failed | ManagementPhase::Cancelled
        )
    }
}

/// 共享内层（manager 与后台作业任务共享）。`ledger_path` 由构造期固定
/// （实例目标 → 实例账本；无目标 → legacy 路径），作业任务经同一 Arc 读写。
struct Inner {
    jobs: BTreeMap<String, JobRecord>,
    ledger: ManagementLedger,
    ledger_path: PathBuf,
    ledger_invalid: AtomicBool,
    ledger_snapshot: Mutex<Option<Vec<u8>>>,
    /// 重装配冻结旗标：登记配置变化后按新登记重装配时置位，旧句柄禁止
    /// 再 start（旧作业的查询/取消不受影响）。与 `start` 同锁串行标记。
    retired: bool,
}

/// 固定包管理器（§2.3 签名固定：status/start/job/cancel）。
pub struct DshPluginManager {
    env: Arc<DshEnvironment>,
    revoker: Arc<dyn BindingRevoker>,
    /// T06：授权状态读取面（None = 未接入 → Unbound，不冒充有绑定）。
    authorization: Option<Arc<dyn DshAuthorizationReader>>,
    /// P1-2：插件身份退役面（None = 未接线 → 仅目录删除，不清登记锚）。
    identity_retirer: Option<IdentityRetirer>,
    /// H05 实例目标（None = 旧桌面单例语义）。
    target: Option<InstanceTarget>,
    /// 作业受理证据门（复审 P1，生产接线）：每次 `start` 前复核登记 home
    /// 证据与活体冲突——缓存命中返回的旧句柄同样不得在证据失效后接收
    /// 新作业。`status`/`job`/`cancel` 不经过本门（旧作业查询/取消保留）。
    job_admission: Option<AdmissionGate>,
    inner: Arc<Mutex<Inner>>,
    seq: AtomicU64,
}

impl DshPluginManager {
    /// 注入构造（生产从系统解析 env；测试全注入，不碰真实 `~/.dsh`/`~/.tuvis`）。
    pub fn new(env: DshEnvironment, revoker: Arc<dyn BindingRevoker>) -> Self {
        let legacy_ledger = load_ledger_at(&ledger_path(&env.tuvis_home));
        Self::assemble(env, revoker, None, legacy_ledger)
    }

    /// 实例作用域构造（H05）：CLI `--profile` 与 `DSH_HOME` 绑到该实例的
    /// profile/home，账本落 `<tuvis_home>/dsh/instances/<instance_id>/`，
    /// 就绪门只盯该实例的活体。
    pub fn for_instance(
        env: DshEnvironment,
        revoker: Arc<dyn BindingRevoker>,
        target: InstanceTarget,
    ) -> Self {
        let instance_id = target.instance_id.clone();
        let ledger = load_ledger_at(&instance_ledger_path(&env.tuvis_home, &instance_id));
        Self::assemble(env, revoker, Some(target), ledger)
    }

    /// 装配内核（构造唯一路径）。
    fn assemble(
        env: DshEnvironment,
        revoker: Arc<dyn BindingRevoker>,
        target: Option<InstanceTarget>,
        loaded: (ManagementLedger, Option<Vec<u8>>, bool),
    ) -> Self {
        let ledger_path = match &target {
            Some(t) => instance_ledger_path(&env.tuvis_home, &t.instance_id),
            None => ledger_path(&env.tuvis_home),
        };
        let (ledger, ledger_snapshot, ledger_invalid) = loaded;
        // job_id 冲突门（重装配/重启）：账本回执 `mgmt-<clock>-<seq>` 的最大序号
        // 把进程内序号顶到其上——固定时钟的测试里重装配也不能复用已落盘 ID。
        let mut seq = 0u64;
        for receipt in &ledger.jobs {
            if let Some(value) = job_seq_number(&receipt.job_id) {
                seq = seq.max(value.saturating_add(1));
            }
        }
        // 终态回执跨重装配可查询：装载时把账本中的终态回执并入内存作业表。
        // 非终态回执可能是崩溃遗留（`refresh_idle_ledger` 会把含非终态的账本判为
        // 不可合并），这里不把它们冒充成活跃作业。
        let jobs: BTreeMap<String, JobRecord> = ledger
            .jobs
            .iter()
            .filter(|receipt| {
                matches!(
                    receipt.phase,
                    ManagementPhase::Succeeded
                        | ManagementPhase::Failed
                        | ManagementPhase::Cancelled
                )
            })
            .map(|receipt| {
                (
                    receipt.job_id.clone(),
                    JobRecord {
                        job: ManagementJob {
                            job_id: receipt.job_id.clone(),
                            kind: receipt.kind,
                            phase: receipt.phase,
                            reason_code: receipt.reason_code.clone(),
                        },
                        profile_dir: PathBuf::new(),
                        cancelled: receipt.phase == ManagementPhase::Cancelled,
                        updated_at_ms: receipt.updated_at_ms,
                    },
                )
            })
            .collect();
        Self {
            env: Arc::new(env),
            revoker,
            authorization: None,
            identity_retirer: None,
            job_admission: None,
            target,
            inner: Arc::new(Mutex::new(Inner {
                jobs,
                ledger,
                ledger_path,
                ledger_invalid: AtomicBool::new(ledger_invalid),
                ledger_snapshot: Mutex::new(ledger_snapshot),
                retired: false,
            })),
            seq: AtomicU64::new(seq),
        }
    }

    /// 注入授权状态读取面（builder 风格；T06 生产装配 auth 单例的观测适配）。
    pub fn with_authorization_reader(mut self, reader: Arc<dyn DshAuthorizationReader>) -> Self {
        self.authorization = Some(reader);
        self
    }

    /// 注入插件身份退役面（builder 风格；P1-2 生产装配登记面清空实现）。
    pub fn with_identity_retirer(mut self, retirer: IdentityRetirer) -> Self {
        self.identity_retirer = Some(retirer);
        self
    }

    /// 就地接授权读取面（缓存装饰器用：管理器已按 `&mut` 传出，不经 move 重建）。
    pub(crate) fn attach_authorization_reader(&mut self, reader: Arc<dyn DshAuthorizationReader>) {
        self.authorization = Some(reader);
    }

    /// 就地接插件身份退役面（缓存装饰器用；P1-2 生产装配登记面清空实现）。
    pub(crate) fn attach_identity_retirer(&mut self, retirer: IdentityRetirer) {
        self.identity_retirer = Some(retirer);
    }

    /// 就地接作业受理证据门（缓存装饰器用，复审 P1）：每次 `start` 前调用，
    /// 证据失效/冲突时拒绝新作业；`status`/`job`/`cancel` 不经过本门。
    pub(crate) fn attach_job_admission_gate(&mut self, gate: AdmissionGate) {
        self.job_admission = Some(gate);
    }

    /// 本管理器装配时绑定的 DSH home（受理门对位用，复审 Important-1）：
    /// 登记键不变而绑定证据变化时，当前证明的 home 与装配 home 不一致即拒绝
    /// 旧句柄——不能让按 A 装配的管理器按 B 的证据过门后继续下发 A。
    pub(crate) fn assembled_home(&self) -> &std::path::Path {
        &self.env.home
    }

    /// 生成下一个作业 ID（`mgmt-<clock>-<seq>`；序号单调递增）。
    fn next_job_id(&self, clock_ms: i64) -> String {
        format!(
            "mgmt-{}-{}",
            clock_ms,
            self.seq.fetch_add(1, Ordering::SeqCst)
        )
    }

    /// 把进程内序号顶到账本与内存回执的最大 `mgmt-<clock>-<seq>` 序号之上
    /// （`refresh_idle_ledger` 合并外部账本后必须调用；见 `start` 内冲突门）。
    fn raise_seq_above_ledger(&self, guard: &Inner) {
        let mut needed = 0u64;
        for receipt in &guard.ledger.jobs {
            if let Some(value) = job_seq_number(&receipt.job_id) {
                needed = needed.max(value.saturating_add(1));
            }
        }
        for job_id in guard.jobs.keys() {
            if let Some(value) = job_seq_number(job_id) {
                needed = needed.max(value.saturating_add(1));
            }
        }
        self.seq.fetch_max(needed, Ordering::SeqCst);
    }

    /// 重装配冻结（缓存内核专用）：在管理器内部锁下**原子地**标记本句柄禁止
    /// `start`，并回报当前是否存在活跃作业。「检查是否空闲」与「冻结」必须同锁
    /// 完成——分开做会让并发 `start` 把新作业落进即将被替换的旧配置（新管理器
    /// 不认识该作业，旧作业还继续跑）。冻结不影响 `status`/`job`/`cancel`：
    /// 活跃作业仍可查询、可取消。锁中毒按保守处理（视作仍有活跃作业）。
    pub(crate) fn retire_and_report_active(&self) -> bool {
        let Ok(mut guard) = self.inner.lock() else {
            return true;
        };
        guard.retired = true;
        guard.jobs.values().any(|record| record.active())
    }

    /// 本机管理状态（§2.3）：真实计算的四档安装态 + 授权态 + 活跃作业。
    ///
    /// 「无绑定」（authorization=Unbound + 安装事实分档）与「尚未实现」
    /// （T01 hub 占位的 `DSH_UNSUPPORTED`）状态可分，绝不返回 blanket success。
    /// H05：实例目标模式下安装档取**该实例** profile 的文件态（不经桌面安装门），
    /// 运行档只看该实例的活体。
    pub fn status(&self) -> Result<ManagementStatus, DshError> {
        let (operation, owned_files, persisted_version) = {
            let guard = self.inner.lock().unwrap();
            (
                guard
                    .jobs
                    .values()
                    .find(|r| r.active())
                    .map(|r| r.job.clone()),
                guard.ledger.owned_files.clone(),
                guard.ledger.plugin_version.clone(),
            )
        };
        // 安装/profile 门：实例目标 = 该实例 profile 的文件态；无目标 =
        // 桌面安装门（identity 单源；未安装/未初始化分开上码）。
        let (installation, mut reason, plugin_version) = match self.target.as_ref() {
            Some(target) => match self.plugin_presence(&target.profile_dir)? {
                Presence::Absent => (InstallationState::Missing, None, None),
                Presence::Unhealthy => (
                    InstallationState::Broken,
                    Some(codes::VERIFY_FAILED.to_string()),
                    None,
                ),
                Presence::Healthy(version) => {
                    if owned_drift(&owned_files) {
                        (
                            InstallationState::Conflict,
                            Some(codes::PACKAGE_CONFLICT.to_string()),
                            version.or(persisted_version),
                        )
                    } else {
                        (
                            InstallationState::Installed,
                            None,
                            version.or(persisted_version),
                        )
                    }
                }
            },
            None => match identity::resolve_installation(&self.env) {
                Err(e) if e.code == codes::DESKTOP_NOT_INSTALLED => {
                    (InstallationState::Missing, Some(e.code.to_string()), None)
                }
                Err(e) if e.code == codes::PROFILE_NOT_INITIALIZED => {
                    (InstallationState::Missing, Some(e.code.to_string()), None)
                }
                Err(e) => return Err(e),
                Ok(installation) => match self.plugin_presence(&installation.profile_dir)? {
                    Presence::Absent => (InstallationState::Missing, None, None),
                    Presence::Unhealthy => (
                        InstallationState::Broken,
                        Some(codes::VERIFY_FAILED.to_string()),
                        None,
                    ),
                    Presence::Healthy(version) => {
                        if owned_drift(&owned_files) {
                            // 用户改过自有文件：冲突如实可见（验收⑥），不静默覆盖。
                            (
                                InstallationState::Conflict,
                                Some(codes::PACKAGE_CONFLICT.to_string()),
                                version.or(persisted_version),
                            )
                        } else {
                            (
                                InstallationState::Installed,
                                None,
                                version.or(persisted_version),
                            )
                        }
                    }
                },
            },
        };
        // 激活档：保存选择 ≠ 实际运行。在场（Installed/Conflict）→ Enabled/Running；
        // RestartRequired 无证据不冒报；缺席/残缺 → Disabled。
        // H05 审查 I5：web 目标的活体按**原始进程表**的 web 形态判定（Web 候选锚在
        // env.home，不能定位任意 home 的宿主——候选面语义如实受限）。
        let host_running = match self.target.as_ref() {
            Some(target) => match target.kind {
                HostKind::Web => identity::web_host_alive(&self.env),
                HostKind::Desktop => {
                    identity::discover_instance(&self.env, &target.instance_id).is_ok()
                }
            },
            None => matches!(host_gate(&self.env), HostGate::Busy),
        };
        let activation = if matches!(
            installation,
            InstallationState::Installed | InstallationState::Conflict
        ) {
            if host_running {
                ActivationState::Running
            } else {
                ActivationState::Enabled
            }
        } else {
            ActivationState::Disabled
        };
        // reason 优先级：活跃作业的 reason（等待中 = 退出未确认）> 状态 reason。
        let reason_code = operation
            .as_ref()
            .and_then(|job| job.reason_code.clone())
            .or(reason.take());
        // 授权档：auth 读取面注入时取真值（Bound/Revoked/Unbound）；
        // 未注入（测试缺省）= Unbound——绝不冒充有绑定。
        // （活跃绑定 id 是命令层 DTO 叠加字段，不进 §2.2 固定类型。）
        let authorization = match self.authorization.as_ref() {
            Some(reader) => reader.authorization_state(),
            None => AuthorizationState::Unbound,
        };
        Ok(ManagementStatus {
            installation,
            activation,
            authorization,
            operation,
            desktop_version: None,
            plugin_version,
            reason_code,
        })
    }

    /// 启动管理作业，返回 job_id（§2.3；任务锁按 profile，不按窗口）。
    ///
    /// 活跃作业期间重复 `start()`（同类双击/异类连点）**复用同一 job_id**：
    /// 双击同 profile 只会得到一个作业（验收②），调用方可比 kind 显示「作业进行中」。
    /// 忙态不报错——宿主在场时作业进 `WaitingForDesktopExit` 等用户退出。
    /// H05：实例目标模式下作业只针对该实例的 profile/活体。
    pub fn start(&self, kind: ManagementKind) -> Result<String, DshError> {
        // 作业受理证据门（复审 P1，生产接线）：证据失效/活体冲突时拒绝新作业。
        // 本门先于其他检查——旧句柄不得在任何新作业路径上继续。
        if let Some(gate) = self.job_admission.as_ref() {
            gate(AdmissionScope::default())?;
        }
        // 目标单源门：实例目标查该实例 profile 官方标记；无目标走桌面安装门
        // （未安装 / profile 未初始化在此明确报错，不建作业）。
        let profile_dir = match self.target.as_ref() {
            Some(target) => {
                if !identity::profile_initialized(&self.env, &target.profile_dir) {
                    return Err(DshError::new(
                        codes::PROFILE_NOT_INITIALIZED,
                        format!(
                            "instance profile missing official marker: {}",
                            target.profile_dir.display()
                        ),
                    ));
                }
                target.profile_dir.clone()
            }
            None => identity::resolve_installation(&self.env)?.profile_dir,
        };
        let target = self.target.clone();
        let mut guard = self.inner.lock().unwrap();
        if guard.retired {
            // 重装配冻结：本句柄已被缓存内核按新登记替换，不得再启动作业。
            return Err(DshError::new(
                codes::INSTANCE_SWITCHED,
                "management handle superseded by reassembly; stale handle cannot start jobs",
            ));
        }
        if let Some(existing) = guard
            .jobs
            .values()
            .find(|r| r.active() && r.profile_dir == profile_dir)
        {
            // 任务锁按 profile：活跃作业复用，绝不并行建作业。
            return Ok(existing.job.job_id.clone());
        }
        if let Some(parent) = guard.ledger_path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                DshError::new(
                    codes::STATE_IO,
                    format!("create management directory: {error}"),
                )
            })?;
        }
        let logical_profile = self.env.home.join("profiles").join(
            target
                .as_ref()
                .map(|t| t.profile_name())
                .unwrap_or(PROFILE_NAME),
        );
        #[cfg(unix)]
        let physical_profile =
            self.env.files.requires_physical_profile_lock() || profile_dir.is_dir();
        #[cfg(not(unix))]
        let physical_profile = false;
        let job_lock = if physical_profile {
            #[cfg(unix)]
            {
                JobGuard {
                    logical_profile: logical_profile.clone(),
                    physical: Some(super::profile_activity_lock::acquire(&profile_dir)?),
                    _fallback: None,
                }
            }
            #[cfg(not(unix))]
            {
                unreachable!()
            }
        } else {
            // 虚拟文件源和尚未完成原生锁验证的平台沿用既有状态目录锁。
            let job_lock_path = guard.ledger_path.with_extension("job.lock");
            let lock = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(job_lock_path)
                .map_err(|error| {
                    DshError::new(
                        codes::STATE_IO,
                        format!("open management job lock: {error}"),
                    )
                })?;
            lock.try_lock_exclusive().map_err(|error| {
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    DshError::new(codes::JOB_BUSY, "another management job is running")
                } else {
                    DshError::new(codes::STATE_IO, format!("lock management job: {error}"))
                }
            })?;
            JobGuard {
                logical_profile,
                #[cfg(unix)]
                physical: None,
                _fallback: Some(lock),
            }
        };
        let job_lock = Arc::new(job_lock);
        // 崩溃遗留对账（复审 P2，两轮）：以**最新磁盘账本**为准，覆盖冷启动
        // 与已缓存管理器两条路径——后者 `refresh_idle_ledger` 会把「快照≠磁盘
        // 且含非终态」判为不可合并且不更新内存账本，只看装配时账本会永远错过
        // 遗留、随后被无效标记卡死。此时持有 profile 锁 = 其他写者已退场，
        // 非终态即崩溃遗留：Applying/Verifying 先核对实际安装状态再落终态
        // （Healthy/Absent = 操作结果可证 → Succeeded；其余 → Failed），
        // 其余非终态落 Failed(STATE_IO)。对账后的账本落盘成功才受理新作业
        // （幂等——落盘失败不建作业，磁盘保持原值可重对账）。
        let fresh = load_ledger_at(&guard.ledger_path);
        let leftovers: Vec<(String, ManagementKind, ManagementPhase)> = match &fresh {
            (_, _, true) => Vec::new(), // 账本损坏：交给 refresh/persist 的既定硬失败路径
            (ledger, _, false) => ledger
                .jobs
                .iter()
                .filter(|receipt| !terminal_phase(receipt.phase))
                .map(|receipt| (receipt.job_id.clone(), receipt.kind, receipt.phase))
                .collect(),
        };
        if !leftovers.is_empty() {
            let now = self.env.clock.now_ms();
            let (fresh_ledger, fresh_bytes, fresh_invalid): &(ManagementLedger, _, _) = &fresh;
            debug_assert!(!fresh_invalid, "leftovers 非空时账本必已成功解析");
            // 恢复基底 = **完整 fresh 账本**（复审 Major-1）：只把遗留挑进旧缓存
            // 再以 fresh 为 CAS 基线，会把 fresh 里其他写者的终态回执与较新的
            // 来源/自有文件字段用旧缓存覆盖回去——输出必须保留 fresh 的全部内容。
            let mut adopted = fresh_ledger.clone();
            // 未提交来源状态保护：内存 cli_*/自有文件/版本与原快照不一致时，
            // 说明本管理器有尚未落盘的更新——覆盖在恢复基底之上，不丢。
            let snapshot_ledger: ManagementLedger = guard
                .ledger_snapshot
                .lock()
                .map_err(|_| DshError::new(codes::STATE_IO, "management ledger snapshot poisoned"))?
                .as_ref()
                .and_then(|bytes| serde_json::from_slice(bytes).ok())
                .unwrap_or_default();
            let uncommitted_source = guard.ledger.cli_origin != snapshot_ledger.cli_origin
                || guard.ledger.cli_pending != snapshot_ledger.cli_pending
                || guard.ledger.cli_unresolved != snapshot_ledger.cli_unresolved;
            if uncommitted_source {
                adopted.cli_origin = guard.ledger.cli_origin.clone();
                adopted.cli_pending = guard.ledger.cli_pending.clone();
                adopted.cli_unresolved = guard.ledger.cli_unresolved;
            }
            if guard.ledger.owned_files != snapshot_ledger.owned_files {
                adopted.owned_files = guard.ledger.owned_files.clone();
            }
            if guard.ledger.plugin_version != snapshot_ledger.plugin_version {
                adopted.plugin_version = guard.ledger.plugin_version.clone();
            }
            // 本管理器内存作业表里比磁盘新的状态（上次 persist 失败的作业等）
            // 一并覆盖回恢复基底，防止对账落盘把内存态抹回旧值。
            for record in guard.jobs.values() {
                let receipt = JobReceipt {
                    job_id: record.job.job_id.clone(),
                    kind: record.job.kind,
                    phase: record.job.phase,
                    reason_code: record.job.reason_code.clone(),
                    updated_at_ms: record.updated_at_ms,
                };
                adopted.jobs.retain(|item| item.job_id != receipt.job_id);
                adopted.jobs.push(receipt);
            }
            // 恢复判定产出（记账字段记在 adopted 上——guard.ledger 随后被
            // adopted 整体覆盖，先写旧账本会被冲掉）。
            let mut recovered_version: Option<String> = None;
            let mut published_job_ids: Vec<String> = Vec::new();
            for (job_id, kind, phase) in &leftovers {
                // 本管理器已知终态优先（复审 Major-1）：内存作业表里已是终态的
                // 回执（上次 persist 失败保留下来的）是**已知事实**，不得用磁盘
                // phase 重新恢复判定改写——只对合成后仍非终态的遗留执行恢复。
                let memory_terminal = guard
                    .jobs
                    .get(job_id)
                    .is_some_and(|record| !record.active());
                if memory_terminal {
                    continue;
                }
                let Some(receipt) = adopted.jobs.iter_mut().find(|r| &r.job_id == job_id) else {
                    continue;
                };
                if terminal_phase(receipt.phase) {
                    // adopted 经内存 upsert 已带终态（如撤权失败的 Failed）——
                    // 同上，保留已知事实。
                    continue;
                }
                // 恢复成功判定按**原作业 kind**（复审 Major-2）：Uninstall 只有
                // 插件确实移除才算成功（Healthy = 仍有安装痕迹，卸载未证完成）；
                // Install/Repair 需要目标版本在安装位（同名旧版本不算——与正常
                // 安装成功核验一致，版本必须匹配内嵌 manifest）——证据不足一律
                // 保守 Failed。
                let settled = match (kind, phase) {
                    (ManagementKind::Uninstall, ManagementPhase::Applying)
                    | (ManagementKind::Uninstall, ManagementPhase::Verifying) => {
                        // 撤权证据与文件态缺一不可（复审 P2）：Applying 在撤权
                        // 前落盘，撤权前崩溃 + 插件本已缺失时，仅凭 Absent 会把
                        // 「绑定还在」的卸载报成成功。Bound 行或 revoke-pending
                        // 在场 = 撤权未证完成；账本不可读 = 证据不足。
                        let revocation = target.as_ref().and_then(|t| {
                            super::auth::revocation_settled_for(
                                &self.env,
                                &t.instance_id,
                                &profile_dir.to_string_lossy(),
                            )
                        });
                        match (self.plugin_presence(&profile_dir), revocation) {
                            (Ok(Presence::Absent), Some(true)) => {
                                // 第九轮 P1-2 复审 Important-2：恢复成功判据补上
                                // 信任物退役——成功回执落盘**之前**必须完成身份
                                // 目录删除 + 登记锚清空，不得先发布成功再依赖随后
                                // 的补完作业补齐事实（成功回执落盘后、补完作业完
                                // 成前再次崩溃且作业 ID 未换 → 下次启动凭成功回执
                                // 直接 Off，身份目录与登记锚永久残留）。退役失败
                                // → Failed(STATE_IO) 如实可重试（重试 disable 清
                                // 作业 ID 重走完整卸载；幂等——已退役即原样成功）。
                                match retire_plugin_identity(
                                    &self.env,
                                    target.as_ref(),
                                    self.identity_retirer.as_ref(),
                                ) {
                                    Ok(()) => (ManagementPhase::Succeeded, None),
                                    Err(error) => {
                                        log::warn!(
                                            "dsh leftover uninstall retired incompletely at recovery: {}",
                                            error.detail
                                        );
                                        (ManagementPhase::Failed, Some(codes::STATE_IO.to_string()))
                                    }
                                }
                            }
                            (Ok(Presence::Absent), _) => (
                                ManagementPhase::Failed,
                                Some(codes::REVOKE_FAILED.to_string()),
                            ),
                            (Ok(Presence::Healthy(_)) | Ok(Presence::Unhealthy), _) => (
                                ManagementPhase::Failed,
                                Some(codes::VERIFY_FAILED.to_string()),
                            ),
                            (Err(_), _) => {
                                (ManagementPhase::Failed, Some(codes::STATE_IO.to_string()))
                            }
                        }
                    }
                    (
                        ManagementKind::Install | ManagementKind::Repair,
                        ManagementPhase::Applying | ManagementPhase::Verifying,
                    ) => {
                        // 记账核验与正常成功同一口径（复审 P2）：恢复成功必须
                        // 重做归档哈希与 CLI 清单核验，并把自有指纹/版本记入
                        // 账本——否则首装记账前崩溃的恢复会报成功，后续卸载
                        // 却跳过自有包改动检查。
                        let expected = embedded_manifest().ok().map(|m| m.plugin_version);
                        let bookkeeping = self.recover_install_bookkeeping(&profile_dir, &expected);
                        match (
                            self.plugin_presence(&profile_dir),
                            bookkeeping,
                            expected.as_ref(),
                        ) {
                            (
                                Ok(Presence::Healthy(Some(installed))),
                                Some(recovered),
                                Some(expected_version),
                            ) if installed == *recovered && *recovered == *expected_version => {
                                // 统一发布（复审 P2）：记账字段记在 adopted 上
                                // （guard.ledger 随后被 adopted 整体覆盖），与
                                // 内存回执一起由对账收尾一次落盘。
                                recovered_version = Some(expected_version.clone());
                                (ManagementPhase::Succeeded, None)
                            }
                            (
                                Ok(Presence::Healthy(Some(_)))
                                | Ok(Presence::Healthy(None))
                                | Ok(Presence::Absent)
                                | Ok(Presence::Unhealthy),
                                _,
                                _,
                            ) => (
                                ManagementPhase::Failed,
                                Some(codes::VERIFY_FAILED.to_string()),
                            ),
                            (Err(_), _, _) => {
                                (ManagementPhase::Failed, Some(codes::STATE_IO.to_string()))
                            }
                        }
                    }
                    _ => (ManagementPhase::Failed, Some(codes::STATE_IO.to_string())),
                };
                receipt.phase = settled.0;
                receipt.reason_code = settled.1.clone();
                receipt.updated_at_ms = now;
                let memory_terminal = guard
                    .jobs
                    .get(job_id)
                    .is_some_and(|record| !record.active());
                if !memory_terminal {
                    // 本次对账真正推断/发布的记录：persist 失败时要回收。
                    published_job_ids.push(job_id.clone());
                    guard.jobs.insert(
                        job_id.clone(),
                        JobRecord {
                            job: ManagementJob {
                                job_id: job_id.clone(),
                                kind: *kind,
                                phase: settled.0,
                                reason_code: settled.1.clone(),
                            },
                            profile_dir: PathBuf::new(),
                            cancelled: false,
                            updated_at_ms: now,
                        },
                    );
                }
            }
            if let Some(version) = recovered_version.as_ref() {
                adopted.owned_files = bookkeeping_receipts(&self.env, version);
                adopted.plugin_version = Some(version.clone());
            }
            // 恢复结果 + CAS 基线推进到所依据的 fresh 字节：persist 比较
            // 「fresh 以来是否有他人再改」，期间他人变更仍会被拒。
            // 统一发布（复审 P2）：内存账本、可查询回执与磁盘由对账收尾一次
            // 提交——persist 成功才发布（含磁盘吸收的外部终态回执进内存查询
            // 面）；失败则回滚内存账本与快照、移除本次发布的记录（既有记录
            // 含内存已知终态不动；磁盘原样、幂等可重对账）。
            let previous_ledger = guard.ledger.clone();
            let previous_snapshot = guard
                .ledger_snapshot
                .lock()
                .map_err(|_| DshError::new(codes::STATE_IO, "management ledger snapshot poisoned"))?
                .clone();
            guard.ledger = adopted;
            *guard.ledger_snapshot.lock().map_err(|_| {
                DshError::new(codes::STATE_IO, "management ledger snapshot poisoned")
            })? = fresh_bytes.clone();
            if let Err(error) = persist(&guard) {
                guard.ledger = previous_ledger;
                *guard.ledger_snapshot.lock().map_err(|_| {
                    DshError::new(codes::STATE_IO, "management ledger snapshot poisoned")
                })? = previous_snapshot;
                // 只回收本次发布的记录；既有记录（含内存已知终态）不动。
                for job_id in &published_job_ids {
                    guard.jobs.remove(job_id);
                }
                return Err(error);
            }
            // 磁盘吸收的终态回执（外部写入的 X 等）进内存查询面：job() 与
            // 磁盘一致。
            let disk_terminals: Vec<JobReceipt> = guard
                .ledger
                .jobs
                .iter()
                .filter(|receipt| terminal_phase(receipt.phase))
                .cloned()
                .collect();
            for receipt in disk_terminals {
                if !guard.jobs.contains_key(&receipt.job_id) {
                    guard.jobs.insert(
                        receipt.job_id.clone(),
                        JobRecord {
                            job: ManagementJob {
                                job_id: receipt.job_id.clone(),
                                kind: receipt.kind,
                                phase: receipt.phase,
                                reason_code: receipt.reason_code.clone(),
                            },
                            profile_dir: PathBuf::new(),
                            cancelled: receipt.phase == ManagementPhase::Cancelled,
                            updated_at_ms: receipt.updated_at_ms,
                        },
                    );
                }
            }
        }
        // 对账之后再刷新：遗留已落终态，refresh 的 incomplete 检查不会再把
        // 本路径判成不可合并（缓存路径的 poison 被解除）。刷新把磁盘上**其他
        // 管理器**写入的账本（含我们刚落盘的恢复回执）合并进内存——`job()`
        // 的可查询面由此与磁盘一致。
        refresh_idle_ledger(&mut guard);
        // 刷新可能合并**其他管理器**写入的账本回执：序号必须顶到最新账本之上，
        // 否则固定时钟下会生成与已落盘回执相同的 job_id（persist 的 BTreeMap
        // 同键插入会用本作业覆盖别人的回执）。
        self.raise_seq_above_ledger(&guard);
        let clock_ms = self.env.clock.now_ms();
        let mut job_id = self.next_job_id(clock_ms);
        for _ in 0..16 {
            let clashes_memory = guard.jobs.contains_key(&job_id);
            let clashes_ledger = guard.ledger.jobs.iter().any(|r| r.job_id == job_id);
            if !clashes_memory && !clashes_ledger {
                break;
            }
            job_id = self.next_job_id(clock_ms);
        }
        if guard.jobs.contains_key(&job_id) || guard.ledger.jobs.iter().any(|r| r.job_id == job_id)
        {
            return Err(DshError::new(
                codes::STATE_IO,
                format!("unable to allocate a unique management job id (last tried {job_id})"),
            ));
        }
        let now = self.env.clock.now_ms();
        guard.jobs.insert(
            job_id.clone(),
            JobRecord {
                job: ManagementJob {
                    job_id: job_id.clone(),
                    kind,
                    phase: ManagementPhase::Queued,
                    reason_code: None,
                },
                profile_dir: profile_dir.clone(),
                cancelled: false,
                updated_at_ms: now,
            },
        );
        if let Err(e) = persist(&guard) {
            // 初始回执写盘失败 → 不建后台作业：移除内存占位、释放物理锁后
            // 如实报错。作业没有可审计的落盘起点，静默继续会留下「已受理」
            // 的假象（对账/恢复都无法解释这个作业）。
            guard.jobs.remove(&job_id);
            drop(guard);
            drop(job_lock);
            return Err(e);
        }
        drop(guard);
        let env = self.env.clone();
        let revoker = self.revoker.clone();
        let inner = self.inner.clone();
        let task_job_id = job_id.clone();
        // 后台任务自转（§2.3：start 只返回 job_id）；测试在 tokio current-thread
        // 运行时内以 job() 轮询驱动，生产由 T05 的 async IPC 上下文提供运行时。
        let ctx = JobContext {
            kind,
            profile_dir: profile_dir.clone(),
            target,
            job_lock,
            admission: self.job_admission.clone(),
            retirer: self.identity_retirer.clone(),
        };
        tokio::spawn(async move {
            drive_job(env, revoker, inner, task_job_id, ctx).await;
        });
        Ok(job_id)
    }

    /// 测试账本提交锁；活动作业互斥由生产入口回归另行覆盖。
    #[cfg(feature = "dsh-test-probes")]
    pub fn test_only_commit_receipt(&self, id: &str, kind: ManagementKind) -> Result<(), DshError> {
        let mut guard = self.inner.lock().unwrap();
        guard.jobs.insert(
            id.to_string(),
            JobRecord {
                job: ManagementJob {
                    job_id: id.to_string(),
                    kind,
                    phase: ManagementPhase::Queued,
                    reason_code: None,
                },
                profile_dir: PathBuf::new(),
                cancelled: false,
                updated_at_ms: self.env.clock.now_ms(),
            },
        );
        persist(&guard)
    }

    /// 按 job_id 查询作业（§2.3）。
    ///
    /// 查询面 = 内存作业表（在跑活体 + 装配时并入的终态回执）**∪ 账本终态回执**。
    /// 后一层是 F09-A 的修复点：账本可能在本管理器装配**之后**才写下终态（另一进程
    /// 的作业、或崩溃遗留回执），只读内存快照会查不到，调用方（开关外表）便无从
    /// 证明「这个 jobId 早已 Failed」，只能靠猜。终态回执不会翻转成活跃态，因此这
    /// 个兜底不会把已结束的作业冒充成在跑；命中只在内存 miss 时发生（`view` 每拍
    /// 至多一次读盘），不进入热路径。
    pub fn job(&self, id: &str) -> Result<ManagementJob, DshError> {
        let ledger_path = {
            let guard = self.inner.lock().unwrap();
            if let Some(record) = guard.jobs.get(id) {
                return Ok(record.job.clone());
            }
            if let Some(job) = guard
                .ledger
                .jobs
                .iter()
                .find(|r| r.job_id == id)
                .and_then(receipt_job_if_terminal)
            {
                return Ok(job);
            }
            guard.ledger_path.clone()
        };
        // 装配快照之后写入的终态回执：只做一次只读装载，不回写、不改状态机
        // （账本损坏 → 视作不可证，按 JOB_UNKNOWN 回答，绝不猜成活跃）。
        let (fresh, _, invalid) = load_ledger_at(&ledger_path);
        if !invalid {
            if let Some(job) = fresh
                .jobs
                .iter()
                .find(|r| r.job_id == id)
                .and_then(receipt_job_if_terminal)
            {
                return Ok(job);
            }
        }
        Err(DshError::new(
            codes::JOB_UNKNOWN,
            format!("no such management job: {id}"),
        ))
    }

    /// 取消作业（§2.3）。置取消旗标并返回当前快照；作业在下一阶段检查点转
    /// `Cancelled`（已终态的作业原样返回，不改写历史）。
    pub fn cancel(&self, id: &str) -> Result<ManagementJob, DshError> {
        let mut guard = self.inner.lock().unwrap();
        let record = guard.jobs.get_mut(id).ok_or_else(|| {
            DshError::new(codes::JOB_UNKNOWN, format!("no such management job: {id}"))
        })?;
        record.cancelled = true;
        Ok(record.job.clone())
    }

    // --------------------------------------------------------
    // profile 插件态观察（只经注入文件源；不打开会话内容面）
    // --------------------------------------------------------

    /// 崩溃恢复的安装记账核验（复审 P2）：归档哈希 + 版本口径与正常成功一致
    /// ——归档哈希直接读稳定缓存比对 manifest（与 `verify_plugin_cache` 同
    /// 口径），版本要求包 manifest == 内嵌目标版本（正常核验的 CLI 清单在
    /// 恢复路径无 CLI 源可跑，文件面是能证明的硬下界；CLI 面由下次正常作业
    /// 补验）。返回观测版本（None = 任一核验不成立）。
    fn recover_install_bookkeeping(
        &self,
        profile_dir: &Path,
        expected: &Option<String>,
    ) -> Option<String> {
        let manifest = embedded_manifest().ok()?;
        if expected.as_ref() != Some(&manifest.plugin_version) {
            return None;
        }
        verify_plugin_cache(&self.env, &manifest).ok()?;
        match self.plugin_presence(profile_dir).ok()? {
            Presence::Healthy(Some(version)) if version == manifest.plugin_version => Some(version),
            _ => None,
        }
    }

    /// profile 文件态（四态的 ②③；① 是包哈希，④ 要跑 CLI，故 status 只给 ②③）。
    fn plugin_presence(&self, profile_dir: &Path) -> Result<Presence, DshError> {
        let manifest = self.read_profile_manifest(profile_dir)?;
        let dependency = manifest
            .as_ref()
            .is_some_and(|m| profile_dependency_declared(m, PACKAGE_NAME));
        let bundle = manifest
            .as_ref()
            .is_some_and(|m| profile_bundles_contain(m, PACKAGE_NAME));
        // ③ 包内容：必须看 `node_modules/<包名>/package.json`，**不是**
        // `node_modules/@tuvis/` ——官方 remove 后作用域空目录还留着（§2.3）。
        let package_dir = plugin_node_dir(profile_dir);
        let package_present = self.env.files.exists(&package_dir);
        let version = self.read_plugin_version(&package_dir)?;

        match (dependency, bundle, package_present) {
            (false, false, false) => Ok(Presence::Absent),
            (true, true, true) => Ok(Presence::Healthy(version)),
            // 半态（只有一部分标记/包内容）= 残缺，不猜哪边对。
            _ => Ok(Presence::Unhealthy),
        }
    }

    /// profile `package.json` 解析产物（缺失/畸形 → None：不冒充有配置）。
    fn read_profile_manifest(
        &self,
        profile_dir: &Path,
    ) -> Result<Option<serde_json::Value>, DshError> {
        let path = profile_manifest_path(profile_dir);
        if !self.env.files.exists(&path) {
            return Ok(None);
        }
        let bytes = self.env.files.read_small(&path, PROFILE_READ_BUDGET)?;
        Ok(serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .filter(serde_json::Value::is_object))
    }

    fn read_plugin_version(&self, package_dir: &Path) -> Result<Option<String>, DshError> {
        let pkg = join_lexical(&package_dir.to_string_lossy(), &["package.json"]);
        if !self.env.files.exists(&pkg) {
            return Ok(None);
        }
        let bytes = self.env.files.read_small(&pkg, PROFILE_READ_BUDGET)?;
        Ok(serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| v["version"].as_str().map(str::to_string)))
    }
}

/// ② profile `dependencies[包名]` 是否声明（官方 add 写 `file:<tgz>`）。
fn profile_dependency_declared(manifest: &serde_json::Value, name: &str) -> bool {
    manifest["dependencies"]
        .as_object()
        .is_some_and(|deps| deps.contains_key(name))
}

/// ② profile `dsh.profile.bundles` 是否含包名（bundle 才是装载开关）。
fn profile_bundles_contain(manifest: &serde_json::Value, name: &str) -> bool {
    manifest["dsh"]["profile"]["bundles"]
        .as_array()
        .is_some_and(|bundles| bundles.iter().any(|v| v.as_str() == Some(name)))
}

/// profile 内自有插件目录（官方 CLI add 的落点投影；③ 看的是**这个**目录，
/// 不是它的 `@tuvis` 父目录）。词法拼接保持 profile_dir 的分隔符形态（identity
/// 产出的 Win 形态路径在 std Path::join 下会混入 '/'，与文件源观测的键形态
/// 漂移——identity 私有工具同款逻辑在侧复刻）。
fn plugin_node_dir(profile_dir: &Path) -> PathBuf {
    let mut rel: Vec<&str> = vec!["node_modules"];
    rel.extend(PACKAGE_NAME.split('/'));
    join_lexical(&profile_dir.to_string_lossy(), &rel)
}

/// profile 的 `package.json`（② 的读取目标；官方 profile 独占该文件）。
fn profile_manifest_path(profile_dir: &Path) -> PathBuf {
    join_lexical(&profile_dir.to_string_lossy(), &["package.json"])
}

/// 以 base 的分隔符语义拼接相对路径（identity.rs 同款；不查文件系统）。
fn join_lexical(base: &str, rel: &[&str]) -> PathBuf {
    let sep = if base.contains('\\') { "\\" } else { "/" };
    let trimmed = base.trim_end_matches(['/', '\\']);
    PathBuf::from(format!("{trimmed}{sep}{}", rel.join(sep)))
}

/// 插件在场三分档（Absent/Unhealthy/Healthy）。
enum Presence {
    Absent,
    Unhealthy,
    Healthy(Option<String>),
}

// ============================================================
// 后台作业状态机
// ============================================================

/// 宿主门判定（identity 单源）：在场 = 忙（等待）；退出 = 放行；其余 = 致命错。
enum HostGate {
    /// 目标宿主确认在场。
    Busy,
    /// 无法确认目标已退出（归属不可证/未核验宿主在场）：保守等待但给出
    /// 可见原因（复审 Minor-1），取消出口保持可用。
    BusyUnproven(&'static str),
    Exited,
    Fatal(DshError),
}

fn host_gate(env: &DshEnvironment) -> HostGate {
    match identity::discover(env) {
        Ok(_) => HostGate::Busy,
        Err(e) => match e.code {
            codes::HOST_NOT_RUNNING => HostGate::Exited,
            // 定位歧义/监听未就绪 = 桌面仍在（可能启动中）：继续等，不选边不杀。
            codes::TARGET_AMBIGUOUS | codes::HOST_LISTENER_MISSING => HostGate::Busy,
            _ => HostGate::Fatal(e),
        },
    }
}

/// 实例作用域的宿主门（H05）：门只盯**该实例**的当代活体——其它实例的存亡
/// 与本作业无关。退出 = 该实例不在场；歧义/监听未就绪 = 仍在（继续等）。
/// H05 审查 I5：web 目标的活体 = 原始进程表里存在 web 形态进程（H02：Web argv
/// 不含 home，候选面锚定不了任意 home 的宿主——按形态判定是当前能证明的边界，
/// 如实申报）。
fn host_gate_for(env: &DshEnvironment, target: Option<&InstanceTarget>) -> HostGate {
    match target {
        None => host_gate(env),
        Some(target) => match target.kind {
            HostKind::Web => {
                // 退出门限定到**目标实例**（复审 P2）：目标归属为 Live → 忙；
                // 归属为 Offline（无任何未归属原始事实）→ 已退出；归属不可证
                // （存在无法归属的 Web 原始事实，可能是目标也可能无关宿主）→
                // 保守继续等待并给出可见原因——绝不在目标是否退出未证时进入
                // 撤权/CLI。
                match identity::observe_web_target(env, &target.instance_id) {
                    Ok(identity::WebTargetObservation::Live(..)) => HostGate::Busy,
                    Ok(identity::WebTargetObservation::Offline) => HostGate::Exited,
                    Err(_) => HostGate::BusyUnproven(codes::INSTANCE_UNVERIFIED),
                }
            }
            HostKind::Desktop => {
                // 退出门只看**目标实例**的进程形态事实（复审 P2），且**不以监听
                // 在场为前提**（F09-B）：宿主退出时先关监听、后结束进程，两拍
                // 不同步——「监听消失」不是「已退出」。
                //
                // 旧实现走 `discover_detailed` 的候选核验面：监听一消失，
                // `desktop_candidate` 就返回 `DSH_HOST_LISTENER_MISSING`，落到
                // `desktop_error` 后被当致命错，等待中的卸载因此失败（用户现场）。
                // 现按三态分流：
                // ① 目标进程仍在（无论监听是否还在）→ 继续等待；
                // ② 只有无法归属的 Desktop 宿主形态进程 → 保守等待 + 可见原因；
                // ③ 已证目标进程退出、且无可归属疑点 → 放行卸载。
                // 无关实例的宿主（能派生出别的实例 ID）不阻塞本作业。
                match identity::observe_desktop_exit(env, &target.instance_id) {
                    identity::DesktopExitObservation::TargetAlive => HostGate::Busy,
                    identity::DesktopExitObservation::Gone => HostGate::Exited,
                    // 归属不可证（profile 令牌缺失/目录不在/多台不成器）：保持原
                    // 安全分支——真正的发现失败（如 profile 未初始化）作业失败、
                    // CLI 零调用；只有「多台无法归属」这类歧义才保守等待。绝不
                    // 在目标是否退出未证时移包或强杀。
                    identity::DesktopExitObservation::Unproven => {
                        match identity::discover_detailed(env) {
                            Ok(discovery) => match discovery.desktop_error {
                                Some(e) => HostGate::Fatal(e),
                                None if discovery.unresolved_desktop_hosts > 0 => {
                                    HostGate::BusyUnproven(codes::INSTANCE_UNVERIFIED)
                                }
                                // 没有候选也没有可诊断错误：仍不可证，保守等待。
                                None => HostGate::BusyUnproven(codes::INSTANCE_UNVERIFIED),
                            },
                            Err(e) => HostGate::Fatal(e),
                        }
                    }
                }
            }
        },
    }
}

/// 作业驱动上下文（H05 审查 I5 后参数成组：目标 + CLI 源一起走）。
fn resolve_web_cli(
    env: &DshEnvironment,
    inner: &Mutex<Inner>,
    target: &InstanceTarget,
) -> Option<CliProof> {
    use identity::WebTargetObservation;
    let saved = {
        let guard = inner.lock().ok()?;
        if guard.ledger_invalid.load(Ordering::SeqCst) {
            return None;
        }
        guard.ledger.cli_origin.clone()
    };
    let observation = match identity::observe_web_target(env, &target.instance_id) {
        Ok(observation) => observation,
        Err(_) => {
            let mut guard = inner.lock().ok()?;
            guard.ledger.cli_unresolved = true;
            let _ = persist(&guard);
            return None;
        }
    };
    let (origin, evidence) = match observation {
        WebTargetObservation::Live(process, observed) => {
            {
                let mut guard = inner.lock().ok()?;
                guard.ledger.cli_unresolved = true;
                if persist(&guard).is_err() {
                    return None;
                }
            }
            let origin = if let Some(wrapper) = identity::bundled_wrapper_for_process(&process) {
                CliOrigin::MacBundled { wrapper }
            } else {
                let entry = process
                    .cmdline
                    .iter()
                    .find_map(|token| identity::verified_cli_for_entry(env, token))?;
                CliOrigin::Npm { entry }
            };
            if !identity::target_observation_still_valid(env, &process, &observed) {
                return None;
            }
            {
                let mut guard = inner.lock().ok()?;
                if guard.ledger.cli_origin != saved {
                    return None;
                }
                guard.ledger.cli_pending = Some(origin.clone());
                if persist(&guard).is_err() {
                    // 持久化失败时仍在内存保留阻断状态，后续回执可重试落盘。
                    return None;
                }
            }
            (origin, Some((process, observed)))
        }
        WebTargetObservation::Offline => {
            {
                let guard = inner.lock().ok()?;
                if guard.ledger.cli_unresolved || guard.ledger.cli_pending.is_some() {
                    return None;
                }
            }
            let origin = match saved.clone() {
                Some(origin) => origin,
                None => CliOrigin::Npm {
                    entry: identity::resolve_path_cli_entry(
                        env,
                        std::env::var_os("PATH").as_deref(),
                    )?,
                },
            };
            (origin, None)
        }
    };
    let proof = match origin.clone() {
        CliOrigin::MacBundled { wrapper } => {
            let digest = identity::bundled_cli_fingerprint(env, &wrapper)?;
            CliProof {
                path: wrapper,
                bundled_digest: Some(digest),
                npm_digest: None,
                job_guard: None,
                evidence_gate: None,
            }
        }
        CliOrigin::Npm { entry } => {
            let digest = identity::npm_cli_fingerprint(env, &entry)?;
            CliProof {
                path: entry,
                bundled_digest: None,
                npm_digest: Some(digest),
                job_guard: None,
                evidence_gate: None,
            }
        }
    };
    if let Some((process, observed)) = evidence {
        if !identity::target_observation_still_valid(env, &process, &observed) {
            return None;
        }
        let mut guard = inner.lock().ok()?;
        if guard.ledger.cli_pending.as_ref() != Some(&origin) {
            return None;
        }
        let prior_origin = guard.ledger.cli_origin.clone();
        let prior_pending = guard.ledger.cli_pending.clone();
        let prior_unresolved = guard.ledger.cli_unresolved;
        guard.ledger.cli_origin = Some(origin);
        guard.ledger.cli_pending = None;
        guard.ledger.cli_unresolved = false;
        if persist(&guard).is_err() {
            guard.ledger.cli_origin = prior_origin;
            guard.ledger.cli_pending = prior_pending;
            guard.ledger.cli_unresolved = prior_unresolved;
            return None;
        }
    } else if saved.is_none() {
        let mut guard = inner.lock().ok()?;
        if guard.ledger.cli_origin.is_some()
            || guard.ledger.cli_unresolved
            || guard.ledger.cli_pending.is_some()
        {
            return None;
        }
        guard.ledger.cli_origin = Some(origin);
        if persist(&guard).is_err() {
            guard.ledger.cli_origin = None;
            return None;
        }
    }
    Some(proof)
}

struct JobContext {
    kind: ManagementKind,
    profile_dir: PathBuf,
    target: Option<InstanceTarget>,
    job_lock: Arc<JobGuard>,
    /// 受理证据门（复审 P1）：与 `start` 入口同一校验，由后台在**等待结束、
    /// 产生副作用前**再跑一次——等待期间 home 证据可能变化（绑定账本切换、
    /// 活体冲突）。
    admission: Option<AdmissionGate>,
    /// 插件身份退役面（P1-2）：卸载完成段/升级兼容清理在删目录后调用——生产
    /// 清空实例登记的指纹锚；未注入 = 仅目录删除。
    retirer: Option<IdentityRetirer>,
}

/// 作业主驱动：等待门 → Applying → Verifying → 终态（含取消检查点）。
async fn drive_job(
    env: Arc<DshEnvironment>,
    revoker: Arc<dyn BindingRevoker>,
    inner: Arc<Mutex<Inner>>,
    job_id: String,
    ctx: JobContext,
) {
    let JobContext {
        kind,
        profile_dir,
        target,
        job_lock,
        admission,
        retirer,
    } = ctx;
    let cli_override = if let Some(web_target) = target.as_ref().filter(|t| t.kind == HostKind::Web)
    {
        let env = env.clone();
        let inner = inner.clone();
        let web_target = web_target.clone();
        tokio::task::spawn_blocking(move || resolve_web_cli(&env, &inner, &web_target))
            .await
            .ok()
            .flatten()
    } else {
        None
    };
    let profile_name = target
        .as_ref()
        .map(|t| t.profile_name())
        .unwrap_or(PROFILE_NAME);
    set_phase(
        &env,
        &inner,
        &job_id,
        ManagementPhase::WaitingForDesktopExit,
        Some(codes::DESKTOP_EXIT_PENDING),
    );
    // 等待门：宿主在场就等用户退出（不 kill、不重启、不阻塞调用方）。
    // H05：门只盯本实例的活体（host_gate_for）。
    loop {
        if is_cancelled(&inner, &job_id) {
            return finalize_cancelled(&env, &inner, &job_id);
        }
        match host_gate_for(&env, target.as_ref()) {
            HostGate::Busy => {
                set_phase_if_reason_differs(
                    &env,
                    &inner,
                    &job_id,
                    ManagementPhase::WaitingForDesktopExit,
                    Some(codes::DESKTOP_EXIT_PENDING),
                );
            }
            HostGate::BusyUnproven(reason) => {
                // 目标是否已退出不可证：保守等待 + 可见原因（复审 Minor-1），
                // 取消出口保持可用。
                set_phase_if_reason_differs(
                    &env,
                    &inner,
                    &job_id,
                    ManagementPhase::WaitingForDesktopExit,
                    Some(reason),
                );
            }
            HostGate::Exited => break,
            HostGate::Fatal(e) => {
                set_phase(&env, &inner, &job_id, ManagementPhase::Failed, Some(e.code));
                log::warn!(
                    "dsh management job {job_id} failed at exit gate: {}",
                    e.detail
                );
                return;
            }
        }
        tokio::time::sleep(WAIT_POLL).await;
    }
    if is_cancelled(&inner, &job_id) {
        return finalize_cancelled(&env, &inner, &job_id);
    }
    // 副作用前证据复核（复审 P1）：等待期间 home 证据可能变化（绑定账本切换、
    // 活体冲突）。与 `start` 受理同一校验在后台继续前再跑一次——失败即 Failed，
    // 撤权与 CLI 都零调用。证明的 home 记入作业上下文：后续门对位 + 卸载
    // 撤权消费绑定证据后仍可证明「本作业期间 home 未变」。
    let mut proven_home: Option<std::path::PathBuf> = None;
    if let Some(admission) = admission.as_ref() {
        match admission(AdmissionScope::default()) {
            Ok(home) => proven_home = Some(home),
            Err(e) => {
                set_phase(&env, &inner, &job_id, ManagementPhase::Failed, Some(e.code));
                log::warn!(
                    "dsh management job {job_id} failed at post-wait evidence gate: {}",
                    e.detail
                );
                return;
            }
        }
    }
    // Applying：官方 CLI 动作（install/repair = add；uninstall = 撤权 → 对账 → remove）。
    // Applying 的阶段提交必须**落盘成功**才继续（目标核验之后、撤权/CLI 之前）：
    // 等待期间账本目录变得不可写等普通写失败不置 `ledger_invalid`，只有让本次
    // 提交失败即终止，才能保证副作用前账本可用。
    if let Err(error) = commit_phase(&env, &inner, &job_id, ManagementPhase::Applying, None) {
        set_phase(
            &env,
            &inner,
            &job_id,
            ManagementPhase::Failed,
            Some(codes::STATE_IO),
        );
        log::warn!(
            "dsh management job {job_id} failed at applying: ledger commit failed: {}",
            error.detail
        );
        return;
    }
    // 目标核验先于一切副作用（撤权或 CLI）：逻辑 profile 仍指向持锁目录、
    // 账本仍可用——目标过期/账本不可写时撤权器与 CLI 都零调用。核验通过后
    // 才进入 H05 审查 N3 的「先撤权再卸载 CLI」顺序。
    let check_guard = job_lock.clone();
    if !tokio::task::spawn_blocking(move || check_guard.validate().is_ok())
        .await
        .unwrap_or(false)
    {
        set_phase(
            &env,
            &inner,
            &job_id,
            ManagementPhase::Failed,
            Some(codes::STATE_IO),
        );
        return;
    }
    let ledger_invalid = inner.lock().unwrap().ledger_invalid.load(Ordering::SeqCst);
    if ledger_invalid {
        set_phase(
            &env,
            &inner,
            &job_id,
            ManagementPhase::Failed,
            Some(codes::STATE_IO),
        );
        return;
    }
    // H05 审查 N3：卸载的**撤权先于一切 CLI 依赖**——CLI 来源不可核验/定位失败
    // 时作业会在此后失败，但「先撤后删」的序不因此跳过：该实例全部匹配绑定
    // （Bound → 墓碑、凭据 → 清空、闸门 → 翻转）必须已经撤掉。安装/修复保留
    // CLI 前置核验（不产生撤权副作用）。
    if matches!(kind, ManagementKind::Uninstall) {
        // 副作用入口证据复核（复审 P1）：post-wait 复验与撤权之间隔着账本写入
        // 与异步路径检查——在撤权前**最后一次**校验同一份证据，撤权与 CLI 的
        // 证据基础保持一致（home 对位 post-wait 证明值）。
        if let Some(admission) = admission.as_ref() {
            match admission(AdmissionScope::default()) {
                Ok(home) if proven_home.as_ref() == Some(&home) => {}
                Ok(_) => {
                    set_phase(
                        &env,
                        &inner,
                        &job_id,
                        ManagementPhase::Failed,
                        Some(codes::INSTANCE_SWITCHED),
                    );
                    log::warn!(
                        "dsh management job {job_id} failed at revocation evidence gate: home changed mid-job"
                    );
                    return;
                }
                Err(e) => {
                    set_phase(&env, &inner, &job_id, ManagementPhase::Failed, Some(e.code));
                    log::warn!(
                        "dsh management job {job_id} failed at revocation evidence gate: {}",
                        e.detail
                    );
                    return;
                }
            }
        }
        if let Err(e) = revoker.revoke_profile(&profile_dir.to_string_lossy()) {
            set_phase(
                &env,
                &inner,
                &job_id,
                ManagementPhase::Failed,
                Some(codes::REVOKE_FAILED),
            );
            log::warn!(
                "dsh management job {job_id} failed at applying: binding revocation failed before uninstall: {} ({})",
                e.code,
                e.detail
            );
            return;
        }
    }
    // 本作业的撤权消费了绑定证据（Bound → 墓碑）：此后证据门遇到
    // INSTANCE_UNVERIFIED 且 home 已在前序门证明时，属**本作业按序完成**
    // 的证据转换，不是外部失效（复审 Major-2）——物理对位仍由 JobGuard
    // 与 CLI 门的其他检查承担。
    let revocation_consumed_evidence = matches!(kind, ManagementKind::Uninstall);
    // CLI 门统一核（drive_job 侧 + run_cli 执行前各一次）：Ok(home) 必须对位
    // post-wait 证明值；Unproven 且撤权已由本作业完成 → 放行（证据被自己
    // 消费）；其余 Err → 拒绝。
    let cli_evidence_gate: Option<Arc<dyn Fn() -> Result<(), DshError> + Send + Sync>> =
        admission.clone().map(|admission| {
            let proven = proven_home.clone();
            Arc::new(move || {
                match admission(AdmissionScope {
                    allow_consumed_evidence: revocation_consumed_evidence,
                }) {
                    Ok(home) if proven.as_ref() == Some(&home) => Ok(()),
                    Ok(_) => Err(DshError::new(
                        codes::INSTANCE_SWITCHED,
                        "registered home changed mid-job; re-select required",
                    )),
                    Err(e) => Err(e),
                }
            }) as Arc<dyn Fn() -> Result<(), DshError> + Send + Sync>
        });
    // CLI 源在撤权之后、移包之前定死（I5-r2）：web 目标必须持有启动时锚定的已核验
    // 绝对路径（None = 无法证明 → DSH_CLI_SOURCE_UNVERIFIED 拒绝，绝不执行未核验
    // 来源）；桌面目标 = Desktop 自带 CLI（resolve 失败码原样落终态）。
    let mut cli_path: CliProof = match target.as_ref().map(|t| t.kind) {
        Some(HostKind::Web) => match cli_override {
            Some(path) => path,
            None => {
                set_phase(
                    &env,
                    &inner,
                    &job_id,
                    ManagementPhase::Failed,
                    Some(codes::CLI_SOURCE_UNVERIFIED),
                );
                log::warn!("dsh management job {job_id} failed: no verified web cli source");
                return;
            }
        },
        _ => match identity::resolve_cli(&env) {
            Ok(installation) => CliProof {
                path: installation.cli_path,
                bundled_digest: None,
                npm_digest: None,
                job_guard: None,
                evidence_gate: None,
            },
            Err(e) => {
                set_phase(&env, &inner, &job_id, ManagementPhase::Failed, Some(e.code));
                log::warn!(
                    "dsh management job {job_id} failed at applying: {}",
                    e.detail
                );
                return;
            }
        },
    };
    cli_path.job_guard = Some(job_lock.clone());
    cli_path.evidence_gate = cli_evidence_gate;
    // 副作用入口证据复核（复审 P1，CLI 侧）：撤权后到 CLI 执行之间同样有窗口
    // （CLI 源定位、自有对账、缓存同步），执行前最后一次校验同一份证据；
    // 准备完成后的残余窗口由 run_cli 的终核兜底。
    if let Some(gate) = cli_path.evidence_gate.as_ref() {
        if let Err(e) = gate() {
            set_phase(&env, &inner, &job_id, ManagementPhase::Failed, Some(e.code));
            log::warn!(
                "dsh management job {job_id} failed at cli evidence gate: {}",
                e.detail
            );
            return;
        }
    }
    // P1-2 升级兼容清理（安装/修复）：撤权在证（指向本 profile 的撤权墓碑在场
    // 且无 Bound 行）+ 旧身份目录在场 → 装新代**之前**先退役旧身份（目录删除 +
    // 登记锚清空）。旧构建的关闭流程只移包不动身份——重装读回旧钥会被「插件本地
    // revoked 标志 + 撤权墓碑」双重拒绝，永远连不上；升级用户不得被墙。删除失败
    // → 作业报失败可重试（退役是信任物销毁面，绝不静默跳过）。
    if matches!(kind, ManagementKind::Install | ManagementKind::Repair) {
        if let Err(error) =
            retire_stale_identity_if_revoked(&env, target.as_ref(), retirer.as_ref())
        {
            set_phase(
                &env,
                &inner,
                &job_id,
                ManagementPhase::Failed,
                Some(error.code),
            );
            log::warn!(
                "dsh management job {job_id} failed at stale identity retirement: {}",
                error.detail
            );
            return;
        }
    }
    let applied = match kind {
        ManagementKind::Install | ManagementKind::Repair => {
            apply_add(&env, profile_name, &cli_path).await
        }
        ManagementKind::Uninstall => {
            apply_uninstall_removal(&env, &inner, &profile_dir, profile_name, &cli_path).await
        }
    };
    if let Err(e) = applied {
        set_phase(&env, &inner, &job_id, ManagementPhase::Failed, Some(e.code));
        log::warn!(
            "dsh management job {job_id} failed at applying: {}",
            e.detail
        );
        return;
    }
    if is_cancelled(&inner, &job_id) {
        return finalize_cancelled(&env, &inner, &job_id);
    }
    // Verifying：exit 0 之后的双核验（list 输出 + profile 文件态）。
    set_phase(&env, &inner, &job_id, ManagementPhase::Verifying, None);
    let verified = match kind {
        ManagementKind::Install | ManagementKind::Repair => {
            verify_installed(&env, &profile_dir, profile_name, &cli_path)
                .await
                .map(Some)
        }
        ManagementKind::Uninstall => verify_removed(&env, &profile_dir, profile_name, &cli_path)
            .await
            .map(|()| None),
    };
    let plugin_version = match verified {
        Err(e) => {
            set_phase(&env, &inner, &job_id, ManagementPhase::Failed, Some(e.code));
            log::warn!(
                "dsh management job {job_id} failed at verifying: {}",
                e.detail
            );
            return;
        }
        Ok(version) => version,
    };
    if is_cancelled(&inner, &job_id) {
        return finalize_cancelled(&env, &inner, &job_id);
    }
    // P1-2 卸载完成段：撤权已证（Applying 先撤全部匹配绑定）+ 包移除成功
    // （verify_removed 已过）→ 一并退役该实例的插件身份（目录删除 + 登记锚清空）。
    // 旧钥/信任锚/本地 revoked 标志都持久在身份目录里——不退役则重装读回旧钥，
    // 被「本地 revoked 标志 + 撤权墓碑」双重拒绝，再开启永远连不上。删除失败 →
    // 作业报失败可重试（旧钥在场绝不谎报成功）。删除 = 信任物销毁（退役），绝不恢复。
    if matches!(kind, ManagementKind::Uninstall) {
        if let Err(error) = retire_plugin_identity(&env, target.as_ref(), retirer.as_ref()) {
            set_phase(
                &env,
                &inner,
                &job_id,
                ManagementPhase::Failed,
                Some(error.code),
            );
            log::warn!(
                "dsh management job {job_id} failed at plugin identity retirement: {}",
                error.detail
            );
            return;
        }
    }
    // 成功记账（自有指纹/版本入账本；卸载清账）后落终态。
    let ledger_result = {
        let mut guard = inner.lock().unwrap();
        match kind {
            ManagementKind::Install | ManagementKind::Repair => {
                if let Ok(manifest) = embedded_manifest() {
                    let pkg = plugin_cache_path(&env.tuvis_home, &manifest.plugin_version);
                    guard.ledger.owned_files = vec![OwnedFileReceipt {
                        path: pkg.to_string_lossy().to_string(),
                        sha256: manifest.sha256,
                    }];
                }
                guard.ledger.plugin_version = plugin_version;
            }
            ManagementKind::Uninstall => {
                guard.ledger.owned_files.clear();
                guard.ledger.plugin_version = None;
            }
        }
        persist(&guard)
    };
    if let Err(error) = ledger_result {
        log::warn!(
            "dsh management ledger persist failed after CLI: {}",
            error.detail
        );
        set_phase(
            &env,
            &inner,
            &job_id,
            ManagementPhase::Failed,
            Some(codes::STATE_IO),
        );
        return;
    }
    set_phase(&env, &inner, &job_id, ManagementPhase::Succeeded, None);
}

/// Applying（install/repair）：稳定缓存同步（SHA-256 先于一切 CLI 调用）→ 官方 add。
/// CLI 源（审查 I5）：`cli_override` = web 目标启动时捕获的宿主 argv dsh 入口；
/// None = desktop 目标 → resolve Desktop 自带 CLI（无 profile 门；目标门在 start）。
async fn apply_add(
    env: &DshEnvironment,
    profile_name: &str,
    cli: &CliProof,
) -> Result<(), DshError> {
    let manifest = embedded_manifest()?;
    let package_path = sync_plugin_cache(env, &manifest)?;
    let args = cli_args(profile_name, "add", Some(&package_path));
    let outcome = run_cli(env, cli, args).await?;
    if outcome.exit_code != 0 {
        return Err(classify_cli_failure(&outcome));
    }
    Ok(())
}

/// Applying（uninstall）· 移除段（撤权已在 drive_job 先行——N3：撤权先于 CLI
/// 依赖）：自有指纹对账 → 官方 remove。
///
/// 幂等容忍（P1-2 可重试性）：身份退役失败的重试会在包**已移除**的落点上重跑
/// remove——官方 CLI 对「移除不存在的包」若报错，文件态核验（`verify_removed`
/// 全过：manifest 双标记、包内容、官方 list 均证已移除）可证包确实不在 → 容忍
/// 本次 CLI 退出码，继续身份退役；核验不过则维持原 CLI 失败如实上抛。
async fn apply_uninstall_removal(
    env: &DshEnvironment,
    inner: &Mutex<Inner>,
    profile_dir: &Path,
    profile_name: &str,
    cli: &CliProof,
) -> Result<(), DshError> {
    // 自有指纹对账：用户改过自有文件 → 冲突如实上报（不静默删除用户工作；
    // 官方恢复行为保留——修复路径走官方 add，本函数不回灌整文件旧快照）。
    if let Some(conflict) = owned_conflict(inner) {
        return Err(conflict);
    }
    // remove 带固定包名（独立 args；管理命令不接受任意包名）。
    let args = cli_args(profile_name, "remove", Some(Path::new(PACKAGE_NAME)));
    let outcome = run_cli(env, cli, args).await?;
    if outcome.exit_code != 0 {
        let cli_failure = classify_cli_failure(&outcome);
        // 幂等容忍：文件态 + 官方 list 双证包已移除 → remove 的非零退出码按
        // 「已不存在」处理（卸载的完成判定以核验面为准，不以退出码为准）。
        if verify_removed(env, profile_dir, profile_name, cli)
            .await
            .is_ok()
        {
            log::warn!(
                "dsh uninstall remove exited {} but the package is provably absent; treating as removed ({})",
                outcome.exit_code,
                cli_failure.detail
            );
            return Ok(());
        }
        return Err(cli_failure);
    }
    Ok(())
}

/// P1-2 卸载完成段：退役该实例的插件身份（目录删除 + 登记锚清空）。
fn retire_plugin_identity(
    env: &DshEnvironment,
    target: Option<&InstanceTarget>,
    retirer: Option<&IdentityRetirer>,
) -> Result<(), DshError> {
    remove_plugin_identity_dirs(env, target)?;
    if let (Some(retire), Some(target)) = (retirer, target) {
        retire(&target.instance_id)?;
    }
    Ok(())
}

/// P1-2 升级兼容清理（**钥域判据单源**，复审 Critical）：撤权在证（域内无
/// Bound 行 + 墓碑留存被撤钥）时先清登记锚（幂等——覆盖「手删文件但锚仍在」的
/// 部分恢复态）；身份目录仅在**钥域**命中（身份文件的公钥 = 被撤钥，或身份文件
/// 在场但读不出公钥的保守面）才删除——生产插件**先写身份再握手**
/// （auth.ts:478/648），profile 级判据会把重开后刚生成的新钥误判成死钥，
/// 删新钥 → 再生成 → 再删 = 无限重装循环。
fn retire_stale_identity_if_revoked(
    env: &DshEnvironment,
    target: Option<&InstanceTarget>,
    retirer: Option<&IdentityRetirer>,
) -> Result<(), DshError> {
    let Some(target) = target else {
        return Ok(()); // legacy 单例域无实例登记面；卸载完成段照常退役目录
    };
    let Some(revoked_keys) = super::auth::revoked_tombstone_keys_for(env, &target.instance_id)
    else {
        return Ok(()); // 账本不可核验 → 保守不动
    };
    if revoked_keys.is_empty() {
        return Ok(()); // Bound 行在场（任何钥活着）或从无墓碑 → 身份绝不可动
    }
    // 撤权在证：登记锚随旧钥退役（幂等；与目录删除解耦——锚清空不能等钥域判定）。
    if let Some(retire) = retirer {
        retire(&target.instance_id)?;
    }
    if dead_identity_in_force(env, target) {
        remove_plugin_identity_dirs(env, Some(target))?;
    }
    Ok(())
}

/// 身份文件公钥探针（只读；P1-2 复审 Critical 的钥域比对面）：读在场身份文件的
/// `publicKeySpki`（SPKI DER base64url——与插件 identity.json 字段同形、与墓碑
/// `revokedKeySpki` 同编码），**绝不解私钥材料**。
struct IdentityKeyProbe {
    /// 任一身份文件在场。
    present: bool,
    /// 可读文件的公钥（多文件时任一可读即取；全部不可读 → None）。
    public_spki: Option<String>,
}

fn probe_plugin_identity_keys(
    env: &DshEnvironment,
    target: Option<&InstanceTarget>,
) -> IdentityKeyProbe {
    let mut present = false;
    let mut public_spki = None;
    for dir in plugin_identity_dirs(env, target) {
        let file = dir.join("identity.json");
        if !env.files.exists(&file) {
            continue;
        }
        present = true;
        if public_spki.is_none() {
            public_spki = env
                .files
                .read_small(&file, PROFILE_READ_BUDGET)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .and_then(|value| {
                    value
                        .get("publicKeySpki")
                        .and_then(|spki| spki.as_str())
                        .map(str::to_owned)
                })
                .filter(|spki| !spki.is_empty());
        }
    }
    IdentityKeyProbe {
        present,
        public_spki,
    }
}

/// P1-2 死钥判据（**钥域单源**——开关 tick_checking/tick_activation 与安装路径
/// 共用，复审 Critical）：插件身份文件里的公钥就是本域撤权墓碑留存的那把被撤钥
/// （且域内无 Bound 行保护任何钥）→ 身份已死：重装读回它会被「本地 revoked 标志
/// + 撤权墓碑」双重拒绝，永远连不上。
///
/// * 无被撤钥（Bound 在场/从无墓碑）或证据不可核验 → false：身份绝不可动；
/// * 身份文件不在场 → false（无可退役）；
/// * 身份公钥 = 被撤钥 → true（真死钥——退役解墙）；
/// * 身份公钥可读且 ≠ 被撤钥 → **false（全新身份必须放行）**——这是防无限重装
///   循环的关键半边；
/// * 身份文件在场但读不出公钥（缺失/畸形/字段空）→ **保守 true**（生不出钥的
///   插件同样连不上；与「删身份是退役不是恢复手段」一致——若属误判，插件重启
///   重写可读身份后本判据即放行，收敛不循环）。
///
/// **home 契约（Round-8 P1-3）**：`env.home` 必须是登记核验的实例 DSH home
/// （调用方经 `instances::verified_instance_home` 覆写或经 `init_instance_manager`
/// 装配的管理器 env 传入）——开关 tick 与安装路径 `retire_stale_identity_if_revoked`
/// 共用本谓词，home 也必须同源（兔维斯推导 env.home 对自定义 DSH_HOME 宿主是
/// 错目录）。
pub(crate) fn dead_identity_in_force(env: &DshEnvironment, target: &InstanceTarget) -> bool {
    let Some(revoked_keys) = super::auth::revoked_tombstone_keys_for(env, &target.instance_id)
    else {
        return false;
    };
    if revoked_keys.is_empty() {
        return false;
    }
    let probe = probe_plugin_identity_keys(env, Some(target));
    if !probe.present {
        return false;
    }
    match probe.public_spki {
        Some(public_spki) => revoked_keys.contains(&public_spki),
        // 在场但不可读：保守按死钥（文档见上）。
        None => true,
    }
}

/// Verifying（install/repair）：按官方真实契约的四态核验，返回观测到的插件版本
/// （「CLI exit 0」从不是成功）。
///
/// ① tgz 哈希（`sync_plugin_cache` 已在 Applying 之前对账 manifest，此处复核缓存
/// 未被并发改动）；② profile `package.json` 的 `dependencies[包名]` **且**
/// `dsh.profile.bundles` 含包名；③ `node_modules/<包名>/package.json` 存在且版本
/// 匹配 manifest；④ `plugin list` 输出含 `包名@版本`（**空输出 + rc=0 = 无依赖**，
/// 不是错误）。
async fn verify_installed(
    env: &DshEnvironment,
    profile_dir: &Path,
    profile_name: &str,
    cli: &CliProof,
) -> Result<String, DshError> {
    let manifest = embedded_manifest()?;
    // ① 稳定缓存仍在场且哈希未漂移（任何 CLI 之后复核一次）。
    verify_plugin_cache(env, &manifest)?;
    // ② profile 两处标记必须同时在（缺一即未装好）。
    let profile_manifest = env
        .files
        .exists(&profile_manifest_path(profile_dir))
        .then(|| {
            env.files
                .read_small(&profile_manifest_path(profile_dir), PROFILE_READ_BUDGET)
                .ok()
                .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        });
    let profile_manifest = profile_manifest.flatten();
    let dependency = profile_manifest
        .as_ref()
        .is_some_and(|m| profile_dependency_declared(m, PACKAGE_NAME));
    let bundle = profile_manifest
        .as_ref()
        .is_some_and(|m| profile_bundles_contain(m, PACKAGE_NAME));
    if !dependency || !bundle {
        return Err(DshError::new(
            codes::VERIFY_FAILED,
            format!(
                "profile package.json does not declare {PACKAGE_NAME} (dependencies={dependency} bundles={bundle})"
            ),
        ));
    }
    // ③ 包内容与版本（看包自己的 package.json，不看 `@tuvis` 父目录）。
    let package_dir = plugin_node_dir(profile_dir);
    let package_manifest = join_lexical(&package_dir.to_string_lossy(), &["package.json"]);
    if !env.files.exists(&package_manifest) {
        return Err(DshError::new(
            codes::VERIFY_FAILED,
            format!(
                "node_modules package manifest missing: {}",
                package_manifest.display()
            ),
        ));
    }
    let installed_version = serde_json::from_slice::<serde_json::Value>(
        &env.files
            .read_small(&package_manifest, PROFILE_READ_BUDGET)?,
    )
    .ok()
    .and_then(|v| v["version"].as_str().map(str::to_string))
    .ok_or_else(|| {
        DshError::new(
            codes::VERIFY_FAILED,
            "node_modules package manifest has no version field",
        )
    })?;
    if installed_version != manifest.plugin_version {
        return Err(DshError::new(
            codes::VERIFY_FAILED,
            format!(
                "installed package version {installed_version} does not match manifest {}",
                manifest.plugin_version
            ),
        ));
    }
    // ④ list 输出必须含 `包名@版本`；空输出（rc=0）是「无依赖」→ 核验失败。
    let listed = list_via_cli(env, profile_name, cli).await?;
    let entry = listed
        .iter()
        .find(|p| p.name == PACKAGE_NAME)
        .ok_or_else(|| {
            DshError::new(
                codes::VERIFY_FAILED,
                format!("official list output does not contain {PACKAGE_NAME}"),
            )
        })?;
    let listed_version = entry
        .version
        .clone()
        .unwrap_or_else(|| installed_version.clone());
    if listed_version != manifest.plugin_version {
        return Err(DshError::new(
            codes::VERIFY_FAILED,
            format!(
                "official list version {listed_version} does not match manifest {}",
                manifest.plugin_version
            ),
        ));
    }
    Ok(listed_version)
}

/// Verifying（uninstall）：②③④ 的**否定面**，且 ② 必须**两处标记都撤**
/// （Mac 报告 §2.3：remove 撤回 bundles 项并删除 dependencies 项）。
async fn verify_removed(
    env: &DshEnvironment,
    profile_dir: &Path,
    profile_name: &str,
    cli: &CliProof,
) -> Result<(), DshError> {
    let profile_manifest = env
        .files
        .exists(&profile_manifest_path(profile_dir))
        .then(|| {
            env.files
                .read_small(&profile_manifest_path(profile_dir), PROFILE_READ_BUDGET)
                .ok()
                .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        });
    let profile_manifest = profile_manifest.flatten();
    if let Some(manifest) = profile_manifest.as_ref() {
        if profile_dependency_declared(manifest, PACKAGE_NAME) {
            return Err(DshError::new(
                codes::VERIFY_FAILED,
                "profile package.json still declares the dependency after remove",
            ));
        }
        if profile_bundles_contain(manifest, PACKAGE_NAME) {
            return Err(DshError::new(
                codes::VERIFY_FAILED,
                "profile dsh.profile.bundles still lists the package after remove",
            ));
        }
    }
    // ③ 的否定面：包自己的 package.json 必须消失（`node_modules/@tuvis/` 空目录
    // 官方会留下，故不作为判据）。
    let package_manifest = join_lexical(
        &plugin_node_dir(profile_dir).to_string_lossy(),
        &["package.json"],
    );
    if env.files.exists(&package_manifest) {
        return Err(DshError::new(
            codes::VERIFY_FAILED,
            format!(
                "node_modules package manifest is still present after remove: {}",
                package_manifest.display()
            ),
        ));
    }
    // ④ 的否定面：list 不得再列出该包（空输出 + rc=0 是正确结论）。
    let listed = list_via_cli(env, profile_name, cli).await?;
    if listed.iter().any(|p| p.name == PACKAGE_NAME) {
        return Err(DshError::new(
            codes::VERIFY_FAILED,
            format!("official list output still contains {PACKAGE_NAME}"),
        ));
    }
    Ok(())
}

fn finalize_cancelled(env: &DshEnvironment, inner: &Mutex<Inner>, job_id: &str) {
    set_phase(env, inner, job_id, ManagementPhase::Cancelled, None);
}

fn is_cancelled(inner: &Mutex<Inner>, job_id: &str) -> bool {
    inner
        .lock()
        .unwrap()
        .jobs
        .get(job_id)
        .map(|r| r.cancelled)
        .unwrap_or(false)
}

/// 阶段推进 + 回执持久化（账本写失败降级为告警：审计面损失，不谎报状态）。
fn set_phase(
    env: &DshEnvironment,
    inner: &Mutex<Inner>,
    job_id: &str,
    phase: ManagementPhase,
    reason_code: Option<&str>,
) {
    let mut guard = inner.lock().unwrap();
    if let Some(record) = guard.jobs.get_mut(job_id) {
        record.job.phase = phase;
        record.job.reason_code = reason_code.map(str::to_string);
        record.updated_at_ms = env.clock.now_ms();
    }
    if let Err(e) = persist(&guard) {
        log::warn!("dsh management ledger persist failed: {}", e.detail);
        if matches!(phase, ManagementPhase::Succeeded) {
            if let Some(record) = guard.jobs.get_mut(job_id) {
                record.job.phase = ManagementPhase::Failed;
                record.job.reason_code = Some(codes::STATE_IO.to_string());
            }
        }
    }
}

/// 等待期原因更新：仅在 phase/reason 实际变化时写盘（退出门每拍轮询，
/// 不能逐拍 persist）。未变化时零写、零锁外调用。
fn set_phase_if_reason_differs(
    env: &DshEnvironment,
    inner: &Mutex<Inner>,
    job_id: &str,
    phase: ManagementPhase,
    reason_code: Option<&str>,
) {
    let mut guard = inner.lock().unwrap();
    let changed = guard.jobs.get(job_id).is_some_and(|record| {
        record.job.phase != phase || record.job.reason_code.as_deref() != reason_code
    });
    if !changed {
        return;
    }
    if let Some(record) = guard.jobs.get_mut(job_id) {
        record.job.phase = phase;
        record.job.reason_code = reason_code.map(str::to_string);
        record.updated_at_ms = env.clock.now_ms();
    }
    if let Err(e) = persist(&guard) {
        log::warn!("dsh management ledger persist failed: {}", e.detail);
    }
}

/// 副作用前的阶段提交：与 [`set_phase`] 同一内存转移，但**落盘失败即上抛**。
/// 账本不可用时作业必须在撤权/CLI 之前终止——目标原文：目标过期或账本写失败
/// 时，CLI 和撤权器都零调用（`ledger_invalid` 标记覆盖不了普通写盘失败）。
fn commit_phase(
    env: &DshEnvironment,
    inner: &Mutex<Inner>,
    job_id: &str,
    phase: ManagementPhase,
    reason_code: Option<&str>,
) -> Result<(), DshError> {
    let mut guard = inner.lock().unwrap();
    if let Some(record) = guard.jobs.get_mut(job_id) {
        record.job.phase = phase;
        record.job.reason_code = reason_code.map(str::to_string);
        record.updated_at_ms = env.clock.now_ms();
    }
    persist(&guard)
}

/// 仅在获得 profile 活动锁且本管理器没有活动作业时刷新合法终态账本。
fn refresh_idle_ledger(inner: &mut Inner) {
    if inner.ledger_invalid.load(Ordering::SeqCst) {
        return;
    }
    let (fresh, bytes, invalid) = load_ledger_at(&inner.ledger_path);
    let mut snapshot = match inner.ledger_snapshot.lock() {
        Ok(value) => value,
        Err(_) => {
            inner.ledger_invalid.store(true, Ordering::SeqCst);
            return;
        }
    };
    if invalid {
        inner.ledger_invalid.store(true, Ordering::SeqCst);
        return;
    }
    if *snapshot == bytes {
        return;
    }
    let incomplete = fresh.jobs.iter().any(|job| {
        !matches!(
            job.phase,
            ManagementPhase::Succeeded | ManagementPhase::Failed | ManagementPhase::Cancelled
        )
    });
    let snapshot_ledger = snapshot
        .as_ref()
        .and_then(|bytes| serde_json::from_slice::<ManagementLedger>(bytes).ok())
        .unwrap_or_default();
    let uncommitted_source = inner.ledger.cli_unresolved != snapshot_ledger.cli_unresolved
        || inner.ledger.cli_pending != snapshot_ledger.cli_pending
        || inner.ledger.cli_origin != snapshot_ledger.cli_origin;
    if (snapshot.is_some() && bytes.is_none()) || uncommitted_source || incomplete {
        inner.ledger_invalid.store(true, Ordering::SeqCst);
        return;
    }
    inner.ledger = fresh;
    *snapshot = bytes;
}

/// 内存态 → 账本快照并原子落盘（账本路径由构造期固定：实例目标 → 实例账本）。
fn persist(inner: &Inner) -> Result<(), DshError> {
    if inner.ledger_invalid.load(Ordering::SeqCst) {
        return Err(DshError::new(
            codes::STATE_IO,
            "management ledger cannot be read",
        ));
    }
    let mut receipts: BTreeMap<String, JobReceipt> = inner
        .ledger
        .jobs
        .iter()
        .cloned()
        .map(|receipt| (receipt.job_id.clone(), receipt))
        .collect();
    for record in inner.jobs.values() {
        let receipt = JobReceipt {
            job_id: record.job.job_id.clone(),
            kind: record.job.kind,
            phase: record.job.phase,
            reason_code: record.job.reason_code.clone(),
            updated_at_ms: record.updated_at_ms,
        };
        receipts.insert(receipt.job_id.clone(), receipt);
    }
    let ledger = ManagementLedger {
        jobs: receipts.into_values().collect(),
        owned_files: inner.ledger.owned_files.clone(),
        plugin_version: inner.ledger.plugin_version.clone(),
        cli_origin: inner.ledger.cli_origin.clone(),
        cli_pending: inner.ledger.cli_pending.clone(),
        cli_unresolved: inner.ledger.cli_unresolved,
    };
    let bytes = serde_json::to_vec_pretty(&ledger)
        .map_err(|e| DshError::new(codes::STATE_IO, format!("serialize management ledger: {e}")))?;
    if let Some(parent) = inner.ledger_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            DshError::new(
                codes::STATE_IO,
                format!("create management directory: {error}"),
            )
        })?;
    }
    #[cfg(feature = "dsh-test-probes")]
    {
        if ledger_commit_probe::fail_write(&inner.ledger_path) {
            return Err(DshError::new(
                codes::STATE_IO,
                "injected management ledger failure",
            ));
        }
        ledger_commit_probe::before_lock(&inner.ledger_path);
    }
    let lock_path = inner.ledger_path.with_extension("lock");
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|error| {
            DshError::new(codes::STATE_IO, format!("open management lock: {error}"))
        })?;
    lock_file.lock_exclusive().map_err(|error| {
        DshError::new(codes::STATE_IO, format!("lock management ledger: {error}"))
    })?;
    let mut snapshot = inner
        .ledger_snapshot
        .lock()
        .map_err(|_| DshError::new(codes::STATE_IO, "management ledger lock failed"))?;
    let current = match std::fs::read(&inner.ledger_path) {
        Ok(current) => Some(current),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            inner.ledger_invalid.store(true, Ordering::SeqCst);
            return Err(DshError::new(
                codes::STATE_IO,
                format!("read management ledger: {error}"),
            ));
        }
    };
    if current != *snapshot {
        inner.ledger_invalid.store(true, Ordering::SeqCst);
        return Err(DshError::new(
            codes::STATE_IO,
            "management ledger changed externally",
        ));
    }
    #[cfg(feature = "dsh-test-probes")]
    ledger_commit_probe::after_compare(&inner.ledger_path);
    atomic_write(&inner.ledger_path, &bytes)?;
    *snapshot = Some(bytes);
    Ok(())
}

/// 自有指纹对账：任一 owned 文件内容漂移/缺失 → 冲突（首个即报，定位明确）。
fn owned_conflict(inner: &Mutex<Inner>) -> Option<DshError> {
    let receipts = inner.lock().unwrap().ledger.owned_files.clone();
    for receipt in receipts {
        match std::fs::read(&receipt.path) {
            Ok(bytes) => {
                if sha256_hex(&bytes) != receipt.sha256 {
                    return Some(DshError::new(
                        codes::PACKAGE_CONFLICT,
                        format!("owned file was modified by the user: {}", receipt.path),
                    ));
                }
            }
            Err(_) => {
                return Some(DshError::new(
                    codes::PACKAGE_CONFLICT,
                    format!("owned file is missing: {}", receipt.path),
                ));
            }
        }
    }
    None
}

/// status() 侧的同款对账（有无漂移，不带错误 detail）。
fn owned_drift(receipts: &[OwnedFileReceipt]) -> bool {
    receipts.iter().any(|r| match std::fs::read(&r.path) {
        Ok(bytes) => sha256_hex(&bytes) != r.sha256,
        Err(_) => true,
    })
}

// ============================================================
// 官方 CLI 调用面（独立 args；生产 runner 接线处复用 remote::NoWindow）
// ============================================================

/// CLI 参数契约：`plugin --profile <名称> <op> [arg]`（独立 args，绝不拼 shell）。
///
/// **只传名称**（审查 R01）：官方 `resolveProfileDir(name, home)` 拒绝任何路径
/// 分隔符；传 profile 目录路径会让 install/repair/list/remove 全部失败。
/// H05：名称由管理目标决定（desktop / web），两个实例域的 profile 互不越界。
fn cli_args(profile_name: &str, op: &str, arg: Option<&Path>) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "plugin".into(),
        "--profile".into(),
        profile_name.into(),
        op.into(),
    ];
    if let Some(arg) = arg {
        args.push(arg.as_os_str().to_owned());
    }
    args
}

/// 受控执行（runner 注入面；生产实现须走 `remote::NoWindow` 防闪窗）。
///
/// 每次调用都把**已发现**的 DSH home 以 `DSH_HOME` 下发（审查 R01）：父进程环境
/// 不可依赖，否则 CLI 会去操作默认 `~/.dsh` 下的另一套 profile。
async fn run_cli(
    env: &DshEnvironment,
    cli: &CliProof,
    args: Vec<OsString>,
) -> Result<RunOutcome, DshError> {
    if let Some(guard) = &cli.job_guard {
        let guard = guard.clone();
        tokio::task::spawn_blocking(move || guard.validate())
            .await
            .map_err(|_| {
                DshError::new(codes::STATE_IO, "profile lock verification worker failed")
            })??;
    }
    if let Some(expected) = &cli.bundled_digest {
        let check_env = env.clone();
        let check_path = cli.path.clone();
        let expected = expected.clone();
        let valid = tokio::task::spawn_blocking(move || {
            identity::bundled_cli_fingerprint(&check_env, &check_path).as_ref() == Some(&expected)
        })
        .await
        .unwrap_or(false);
        if !valid {
            return Err(DshError::new(
                codes::CLI_SOURCE_UNVERIFIED,
                "bundled cli source changed",
            ));
        }
    }
    if let Some(expected) = &cli.npm_digest {
        let check_env = env.clone();
        let check_path = cli.path.clone();
        let expected = expected.clone();
        let valid = tokio::task::spawn_blocking(move || {
            identity::npm_cli_fingerprint(&check_env, &check_path).as_ref() == Some(&expected)
        })
        .await
        .unwrap_or(false);
        if !valid {
            return Err(DshError::new(
                codes::CLI_SOURCE_UNVERIFIED,
                "npm cli source changed",
            ));
        }
    }
    if let Some(guard) = &cli.job_guard {
        let guard = guard.clone();
        tokio::task::spawn_blocking(move || guard.validate())
            .await
            .map_err(|_| {
                DshError::new(codes::STATE_IO, "profile lock verification worker failed")
            })??;
    }
    // 证据终核（复审 Major-1）：CLI 门通过后的准备/异步校验窗口在此兜底——
    // 实际 `runner.run` 前最后一次校验，留待执行期间变化的证据不进入 CLI。
    if let Some(gate) = &cli.evidence_gate {
        gate()?;
    }
    let request = CommandRequest::new(&cli.path, args)
        .with_env(DSH_HOME_ENV, env.home.as_os_str().to_owned());
    let runner = env.runner.clone();
    tokio::task::spawn_blocking(move || runner.run(&request))
        .await
        .map_err(|_| DshError::new(codes::CLI_FAILED, "official cli worker failed"))?
        .map_err(|e| {
            DshError::new(
                codes::CLI_FAILED,
                format!("official cli spawn failed: {}", e.detail),
            )
        })
}

/// 官方 `plugin list`（exit 0 也只是信封事实，解析交给调用方核验）。
///
/// **空 stdout + rc=0 = 没有依赖**（Mac 报告 §2.3 实测），故这里返回**空表**而不是
/// 错误——「没装」是正常结论，由调用方按方向（装/卸）判定。
async fn list_via_cli(
    env: &DshEnvironment,
    profile_name: &str,
    cli: &CliProof,
) -> Result<Vec<ListedPlugin>, DshError> {
    let args = cli_args(profile_name, "list", None);
    let outcome = run_cli(env, cli, args).await?;
    if outcome.exit_code != 0 {
        return Err(classify_cli_failure(&outcome));
    }
    parse_list(&outcome.stdout).ok_or_else(|| {
        DshError::new(
            codes::VERIFY_FAILED,
            format!(
                "cannot parse official list output: {}",
                tail(&outcome.stdout)
            ),
        )
    })
}

/// list 输出的一条投影（名字必有，版本尽力）。
struct ListedPlugin {
    name: String,
    version: Option<String>,
}

/// 解析 `plugin list` 输出。官方形态是**树形文本**（Mac 报告 §2.3）：
/// ```text
/// dsh-profile-desktop /path/to/profiles/desktop (PRIVATE)
/// └── @tuvis/dsh-remote-access@0.1.0
/// 1 package
/// ```
/// **空输出 = 无依赖**（不是失败）。因此：
/// * 空白 stdout → 空表（`Some(vec![])`）；
/// * JSON 形态（历史/其它版本）仍宽容解析；
/// * 其余按 `名称@版本` 行抽取（bundle 图例行、`N package` 计数行都跳过）。
fn parse_list(stdout: &str) -> Option<Vec<ListedPlugin>> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        // 官方语义：无依赖。不是解析失败。
        return Some(Vec::new());
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        let array = if value.is_array() {
            Some(&value)
        } else {
            value.get("plugins").or_else(|| value.get("installed"))
        };
        let Some(array) = array.and_then(|a| a.as_array()) else {
            return None; // JSON 但形状不认识：不猜
        };
        return Some(
            array
                .iter()
                .filter_map(|item| {
                    let name = item
                        .get("name")
                        .or_else(|| item.get("package"))
                        .and_then(|n| n.as_str())?
                        .to_string();
                    let version = item
                        .get("version")
                        .and_then(|n| n.as_str())
                        .map(str::to_string);
                    Some(ListedPlugin { name, version })
                })
                .collect(),
        );
    }
    // 树形文本：取含 `@` 的 `名称@版本` 词（`└── <name>@<ver>`）。
    let mut out = Vec::new();
    for line in trimmed.lines() {
        let body = line.trim().trim_start_matches(|c: char| {
            c == '├' || c == '└' || c == '─' || c == '│' || c.is_whitespace()
        });
        if body.is_empty() || body.contains(" package") || body.contains("(PRIVATE)") {
            continue;
        }
        // 只认形如 `<pkg>@<ver>` 的单个词（logo 行/路径行不含 `@` 或含空格）。
        if body.contains(char::is_whitespace) {
            continue;
        }
        if let Some((name, version)) = body.rsplit_once('@') {
            // scoped 包名以 `@` 开头：`@tuvis/dsh-remote-access@0.1.0` 的
            // rsplit_once 正好切在版本前（版本里不会有 `@`）。
            if !name.is_empty() && !version.is_empty() {
                out.push(ListedPlugin {
                    name: name.to_string(),
                    version: Some(version.to_string()),
                });
            }
        }
    }
    Some(out)
}

/// CLI 失败分类：锁占用族独立成码（`DSH_NATIVE_LOCK_BUSY`），其余 `DSH_CLI_FAILED`。
fn classify_cli_failure(outcome: &RunOutcome) -> DshError {
    let hay = format!("{} {}", outcome.stderr, outcome.stdout).to_lowercase();
    if hay.contains("lock")
        || hay.contains("ebusy")
        || hay.contains("eagain")
        || hay.contains("in use")
    {
        DshError::new(
            codes::NATIVE_LOCK_BUSY,
            format!(
                "official cli reports a busy native lock (exit {}): {}",
                outcome.exit_code,
                tail(&outcome.stderr)
            ),
        )
    } else {
        DshError::new(
            codes::CLI_FAILED,
            format!(
                "official cli exited {}: {}",
                outcome.exit_code,
                tail(&outcome.stderr)
            ),
        )
    }
}

/// detail 尾部截断（按字符，不撕 UTF-8；只放英文机器诊断，不含凭据）。
fn tail(s: &str) -> String {
    let trimmed = s.trim();
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= DETAIL_TAIL_CHARS {
        trimmed.to_string()
    } else {
        chars[chars.len() - DETAIL_TAIL_CHARS..].iter().collect()
    }
}

// ============================================================
// 稳定版本化插件缓存（SHA-256 先于任何 CLI 调用）
// ============================================================

/// 同步稳定缓存并返回其路径（`add` 的安装源）。
///
/// 语义（审查「新增事实」#2）：缓存是**兔维斯自有、版本化、跨应用更新存活**的
/// 落点——`add` 必须从它安装，profile 的 `file:` 依赖才指向一个长期有效的绝对
/// 路径。已存在且哈希匹配则原样复用；缺失或哈希不符（含被改动）则从嵌入字节
/// 重新同步（修复路径）。嵌入包自身先对 manifest 校验（任何写盘之前）。
fn sync_plugin_cache(
    env: &DshEnvironment,
    manifest: &EmbeddedManifest,
) -> Result<PathBuf, DshError> {
    let path = plugin_cache_path(&env.tuvis_home, &manifest.plugin_version);
    if std::fs::read(&path)
        .ok()
        .is_some_and(|bytes| sha256_hex(&bytes) == manifest.sha256.to_lowercase())
    {
        return Ok(path); // 命中：不重写（保持 mtime 稳定，减少无谓 IO）
    }
    // 未命中/缺失/被改坏 → 校验嵌入包后重新同步。
    verify_embedded_package(manifest)?;
    atomic_write(&path, EMBEDDED_PACKAGE_BYTES)?;
    Ok(path)
}

/// 复核稳定缓存的存在与哈希（Verifying 的 ①；任何 CLI 之后确认未被并发改动）。
fn verify_plugin_cache(env: &DshEnvironment, manifest: &EmbeddedManifest) -> Result<(), DshError> {
    let path = plugin_cache_path(&env.tuvis_home, &manifest.plugin_version);
    let bytes = std::fs::read(&path).map_err(|e| {
        DshError::new(
            codes::VERIFY_FAILED,
            format!(
                "stable plugin cache is unreadable at {}: {e}",
                path.display()
            ),
        )
    })?;
    let digest = sha256_hex(&bytes);
    if digest != manifest.sha256.to_lowercase() {
        return Err(DshError::new(
            codes::VERIFY_FAILED,
            format!(
                "stable plugin cache sha256 mismatch at {}: expected {}, got {digest}",
                path.display(),
                manifest.sha256
            ),
        ));
    }
    Ok(())
}

fn verify_embedded_package(manifest: &EmbeddedManifest) -> Result<(), DshError> {
    if EMBEDDED_PACKAGE_BYTES.is_empty() {
        return Err(DshError::new(
            codes::PACKAGE_INVALID,
            "embedded package is empty",
        ));
    }
    if EMBEDDED_PACKAGE_BYTES.len() as u64 > MAX_PACKAGE_BYTES {
        return Err(DshError::new(
            codes::PACKAGE_INVALID,
            format!(
                "embedded package over budget: {} bytes",
                EMBEDDED_PACKAGE_BYTES.len()
            ),
        ));
    }
    let mut hasher = Sha256::new();
    for chunk in EMBEDDED_PACKAGE_BYTES.chunks(HASH_CHUNK) {
        hasher.update(chunk);
    }
    let digest: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if digest != manifest.sha256.to_lowercase() {
        return Err(DshError::new(
            codes::PACKAGE_INVALID,
            format!(
                "embedded package sha256 mismatch: expected {}, got {digest}",
                manifest.sha256
            ),
        ));
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ============================================================
// 单元测试（纯函数面：嵌入包校验 / list 解析 / 失败分类 / argv 契约）
// ============================================================

#[cfg(test)]
#[path = "install_admission_tests.rs"]
mod admission_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// 嵌入包描述与真实资源对得上（版本/协议/哈希自洽）。
    #[test]
    fn embedded_manifest_matches_resource() {
        let manifest = embedded_manifest().unwrap();
        assert!(!manifest.plugin_version.is_empty());
        // 与运行期协议常量单点对齐（曾因测试写死字面量漏检归档漂移）。
        assert_eq!(
            manifest.binding_protocol,
            super::super::auth::BINDING_PROTOCOL
        );
        verify_embedded_package(&manifest).unwrap();
    }

    /// 哈希不匹配 → DSH_PACKAGE_INVALID（任何 CLI 调用之前拒绝）。
    #[test]
    fn tampered_manifest_is_rejected() {
        let mut manifest = embedded_manifest().unwrap();
        manifest.sha256 = format!("{:064x}", 0);
        let err = verify_embedded_package(&manifest).unwrap_err();
        assert_eq!(err.code, codes::PACKAGE_INVALID);
    }

    /// list JSON 形态解析（plugins 数组，取 name/version）。
    #[test]
    fn parse_list_json_shape() {
        let parsed = parse_list(r#"{"plugins":[{"name":"@tuvis/dsh-remote-access","version":"0.1.0"},{"name":"other","version":"2.0.0"}]}"#).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name, PACKAGE_NAME);
        assert_eq!(parsed[0].version.as_deref(), Some("0.1.0"));
        let empty = parse_list(r#"{"plugins":[]}"#).unwrap();
        assert!(empty.is_empty());
    }

    /// 非 JSON 树形文本按 `名称@版本` 抽取（§2.3 实测形态）；计数行/logo 行跳过。
    #[test]
    fn parse_list_reads_official_tree_output() {
        let tree = "dsh-profile-desktop /Users/x/.dsh/profiles/desktop (PRIVATE)\n\
                    └── @tuvis/dsh-remote-access@0.1.0\n1 package\n";
        let parsed = parse_list(tree).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, PACKAGE_NAME);
        assert_eq!(parsed[0].version.as_deref(), Some("0.1.0"));
    }

    /// 空 stdout + rc=0 = 「无依赖」（§2.3 实测），必须解析成空表而不是失败。
    #[test]
    fn parse_list_treats_empty_stdout_as_no_dependencies() {
        for empty in ["", "   \n\t\n"] {
            let parsed = parse_list(empty).expect("空输出是合法结论，不是解析失败");
            assert!(parsed.is_empty(), "空输出 = 无依赖：{empty:?}");
        }
    }

    /// JSON 形态仍宽容可读（历史/其它版本输出），但非 JSON 也不再退化为
    /// 「文本包含包名」——树形行必须带版本才算一条。
    #[test]
    fn parse_list_json_shape_and_unknown_text() {
        let parsed = parse_list(
            r#"{"plugins":[{"name":"@tuvis/dsh-remote-access","version":"0.1.0"},{"name":"other","version":"2.0.0"}]}"#,
        )
        .unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name, PACKAGE_NAME);
        assert_eq!(parsed[0].version.as_deref(), Some("0.1.0"));
        // 只有 logo 行（无 `名称@版本` 词）→ 空表。
        let logo_only = parse_list("dsh-profile-desktop /p (PRIVATE)\n0 package\n").unwrap();
        assert!(logo_only.is_empty(), "无依赖的 logo 行不得凭空产出条目");
    }

    /// 失败分类：锁占用词 → NATIVE_LOCK_BUSY；其余 → CLI_FAILED。
    #[test]
    fn cli_failure_classification() {
        let lock = RunOutcome {
            exit_code: 1,
            stdout: String::new(),
            stderr: "Error: EBUSY: profile lock held".into(),
        };
        assert_eq!(classify_cli_failure(&lock).code, codes::NATIVE_LOCK_BUSY);
        let plain = RunOutcome {
            exit_code: 1,
            stdout: String::new(),
            stderr: "Error: something else".into(),
        };
        assert_eq!(classify_cli_failure(&plain).code, codes::CLI_FAILED);
    }

    /// argv 契约：`plugin --profile desktop add <pkg>` / `remove <包名>`——
    /// **只传 profile 名称**（审查 R01：官方拒绝路径分隔符）。
    #[test]
    fn cli_args_use_profile_name_never_a_path() {
        let pkg = Path::new("/tmp/pkg.tgz");
        let add = cli_args(PROFILE_NAME, "add", Some(pkg));
        assert_eq!(add[0], OsString::from("plugin"));
        assert_eq!(add[1], OsString::from("--profile"));
        assert_eq!(add[2], OsString::from(PROFILE_NAME));
        assert!(
            !add[2].to_string_lossy().contains('/') && !add[2].to_string_lossy().contains('\\'),
            "profile 参数必须是名称：{:?}",
            add[2]
        );
        assert_eq!(add[3], OsString::from("add"));
        assert_eq!(add[4], OsString::from(pkg.as_os_str()));
        let remove = cli_args(PROFILE_NAME, "remove", None);
        assert_eq!(remove.len(), 4);
        assert_eq!(remove[3], OsString::from("remove"));
    }

    /// 稳定缓存落点形态：`<TUVIS_HOME>/dsh/plugin-cache/@tuvis-dsh-remote-access-<ver>.tgz`
    /// （包名的 `/` 换成 `-`，避免在缓存目录里造层级）。
    #[test]
    fn plugin_cache_path_is_stable_and_versioned() {
        let home = Path::new("/tmp/tuvis-home");
        let path = plugin_cache_path(home, "0.1.0");
        assert_eq!(
            path,
            PathBuf::from("/tmp/tuvis-home/dsh/plugin-cache/@tuvis-dsh-remote-access-0.1.0.tgz")
        );
        assert_eq!(
            plugin_cache_dir(home),
            PathBuf::from("/tmp/tuvis-home/dsh/plugin-cache")
        );
    }
}
