//! 本机实例登记。登记与选择仅保存配置，不表示运行期授权。
//!
//! H04（计划 §5 / §3.1 接口契约）：
//! * `instance_id` 是**本机登记的稳定 ID**——由「宿主类型 + 规范化 profile 目录」
//!   派生（[`instance_id_for`]），**不用 PID/端口生成**（端口复用、重启都不换
//!   身份，H02 取证）。派生式使发现侧（`identity::discover_all` 的候选）与登记
//!   侧天然同一 ID；受信插件公钥指纹的连续性由 [`InstanceRegistry::register`]
//!   （同 profile 换指纹拒绝）与 H05 握手核验承担，不进 ID 派生。
//! * [`InstanceCandidate`] 是发现侧 DTO（候选、选择、运行状态不混为同一 DTO）。
//! * [`InstanceRegistry::resolve_selected`] / [`InstanceRegistry::resolve_session`]
//!   每次经活体发现重新核验；实例消失或身份不连续 → 明确错误，**绝不回退首项、
//!   绝不自动切换**（spec §3.2）。
use super::error::{codes, DshError};
use super::model::{DshEnvironment, LocalHost, Target};
use crate::monitor::dsh::log::{
    cached_readonly_header, read_readonly_header, READONLY_HEADER_SCAN,
};
use crate::monitor::session_scan::SessionFileScan;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// 只读解析预算：生产恒用 `walk_budget::READONLY_ROOT_BUDGET`（9s，对齐动作
/// 路径快照既有 9s 口径）。NEEDS_FIX Important-4：不再设全局测试覆写——测试走
/// `resolve_readonly_root_with_budget_for_test` 的预算参数注入。超时 →
/// `DSH_READ_TIMEOUT`（读取超时，/m 的 session-messages 端点映射 HTTP 504——
/// 「读取超时，点重试」语义）。
fn readonly_root_budget() -> Duration {
    crate::monitor::walk_budget::READONLY_ROOT_BUDGET
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HostKind {
    Web,
    Desktop,
}

impl HostKind {
    /// ID 派生与诊断用的稳定词。
    pub fn as_str(self) -> &'static str {
        match self {
            HostKind::Web => "web",
            HostKind::Desktop => "desktop",
        }
    }
}

/// profile 目录规范化：canonicalize 成功（真实目录）用规范路径（解析符号链接，
/// 如 macOS 的 `/tmp` → `/private/tmp`），失败（Windows 词法夹具路径 / 目录尚
/// 不存在）退回去掉尾部分隔符的原词法形态。发现与登记共用本函数，保证同一
/// profile 在两侧派生同一实例 ID。只读 stat 语义，不写任何文件。
pub fn normalize_profile(profile_dir: &Path) -> PathBuf {
    std::fs::canonicalize(profile_dir).unwrap_or_else(|_| {
        PathBuf::from(profile_dir.to_string_lossy().trim_end_matches(['/', '\\']))
    })
}

/// 稳定实例 ID 派生（发现与登记唯一共用来源）。输入 = 宿主类型 + 规范化
/// profile 目录；**不含** PID/端口/启动时刻（运行代际不是持久身份）。
pub fn instance_id_for(kind: HostKind, profile_dir: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"dsh-instance-v1\0");
    hasher.update(kind.as_str());
    hasher.update(b"\0");
    hasher.update(normalize_profile(profile_dir).to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()[..32]
        .to_string()
}

/// 活体发现候选（§3.1 固定形状）。`host` 是当代活体身份（`LocalHost.host_id`
/// 已改为稳定实例 ID——wire 字段名不变）；`instance_id` 与登记面同一派生。
///
/// `verified` = **home 归属是否被正面证明**（审查 I1）：
/// * Desktop = true——宿主 argv 携带 profile 目录令牌（Win/Mac 报告 §1.2 权威
///   证据）+ 监听属主 + profile 官方标记，三者共同钉死该进程的 home；
/// * Web = false——Web argv 不含 home（H02 字段表），`profiles/web` 标记 +
///   监听属主只能证明「目录在、监听归它」，**不能证明进程用这个 home**（A profile
///   残留 + 唯一活跃 Web 实际用 B shadow home 即为反例）。未核验候选仅供登记/
///   展示可见，[`InstanceRegistry::resolve_selected`] / [`InstanceRegistry::resolve_session`]
///   **一律不消费**——归属正面证明（受信插件握手 + 实际 profile 关联）归 H05。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceCandidate {
    pub instance_id: String,
    pub kind: HostKind,
    pub display_name: String,
    pub profile_dir: PathBuf,
    pub host: LocalHost,
    /// home 归属是否被正面证明（见类型注释；resolve 面只消费 true）。
    pub verified: bool,
}

/// 当代受信插件指纹核验源（审查 I2）。登记过的实例在每次 resolve 时都要把
/// 「当代指纹」与登记值核对——指纹可由受信插件身份路由/握手应答给出（H02 补测二
/// 实证的机制）；H04 的生产接线归 H05，本 trait 是注入面。
pub trait InstanceIdentityProbe: Send + Sync {
    /// 返回 `Ok(None)` = 当代来源不可用（**不可核验**——resolve 据此拒绝，
    /// 绝不当作通过）；`Ok(Some(f))` = 当代观测指纹（sha256 指纹串）。
    fn current_fingerprint(&self, instance: &SelectedInstance) -> Result<Option<String>, DshError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectedInstance {
    pub instance_id: String,
    pub kind: HostKind,
    pub profile_dir: PathBuf,
    /// 已验证的 DSH home；旧登记记录可能缺失。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dsh_home: Option<PathBuf>,
    pub identity_fingerprint: String,
    /// 只读历史归属证据（Round-8 P1-3）：与 `identity_fingerprint` 在同一批写点
    /// 锚定/重绑（非空指纹写即同步落证据），**退役（`retire_identity_fingerprint`
    /// 清空当代锚）不清空**——关闭+卸载销毁的是插件身份目录与登记锚；登记行与
    /// 绑定账本的撤权墓碑（永久留存被撤钥）保留，只读历史据此仍可证明「本登记
    /// 行确曾被授权绑定该钥」。空串 = 从未锚定（无只读证据，只读一律拒绝——
    /// 候选登记不得凭空获得读权；所有权硬下界不放松）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub read_evidence_fingerprint: String,
}

#[derive(Default, Serialize, Deserialize)]
struct RegistryData {
    instances: Vec<SelectedInstance>,
    selected: Option<String>,
    #[serde(default)]
    selection_revision: u64,
}

/// 此登记不接受活体或签名自报作为授权；写入口须另行完成握手和归属核验。
///
/// Clone 语义（发现②预算包裹用）：克隆 = 同一登记文件句柄的另一个手柄
/// （path 相同；env/identity_probe 是 Arc 共享）——不复制登记数据，读写经
/// 同一路径与文件锁收敛。
#[derive(Clone)]
pub struct InstanceRegistry {
    path: PathBuf,
    /// 活体核验环境（[`InstanceRegistry::resolve_selected`] /
    /// [`InstanceRegistry::resolve_session`] 每次经它重新发现）。`None` = 仅登记
    /// 面（旧构造 [`InstanceRegistry::open`]），活体核验入口如实报错，不猜。
    env: Option<Arc<DshEnvironment>>,
    /// 当代受信插件指纹核验源（[`InstanceIdentityProbe`]；审查 I2）。
    /// `None` = 未接线 → 已登记实例的 resolve 一律按「不可核验」拒绝（fail-closed，
    /// 绝不当作通过）；H05 接线后由握手应答给出当代指纹。
    identity_probe: Option<Arc<dyn InstanceIdentityProbe>>,
}

/// 选择独占租约（[`InstanceRegistry::select_exclusive`] 返回的边界）：持有期间
/// `selection.lock` 文件锁不释放——其他选择翻转无法插入（连续切换原子对）。
/// 「关前任控制台活连接」等翻转后置动作必须在租约存续期内完成；Drop 即释放。
#[allow(dead_code)] // 锁体只作边界持有，从不读内容
pub struct SelectionLease(std::fs::File);

impl InstanceRegistry {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DshError> {
        Self::open_with_env(path, None)
    }

    /// 带活体环境的登记（生产装配与需要核验的测试用；env 的进程/监听/文件
    /// 适配器全部走注入面）。
    pub fn open_with_env(
        path: impl AsRef<Path>,
        env: Option<Arc<DshEnvironment>>,
    ) -> Result<Self, DshError> {
        let registry = Self {
            path: path.as_ref().to_path_buf(),
            env,
            identity_probe: None,
        };
        let _lock = registry.lock()?;
        registry.load()?;
        Ok(registry)
    }

    /// 接线当代指纹核验源（builder；H05 生产接线点，测试注入假探针）。
    pub fn with_identity_probe(mut self, probe: Arc<dyn InstanceIdentityProbe>) -> Self {
        self.identity_probe = Some(probe);
        self
    }
    fn lock(&self) -> Result<std::fs::File, DshError> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| DshError::new(codes::STATE_IO, "registry has no parent"))?;
        std::fs::create_dir_all(parent)
            .map_err(|e| DshError::new(codes::STATE_IO, e.to_string()))?;
        let lock_path = self.path.with_extension("lock");
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(lock_path)
            .map_err(|e| DshError::new(codes::STATE_IO, e.to_string()))?;
        file.lock_exclusive()
            .map_err(|e| DshError::new(codes::STATE_IO, e.to_string()))?;
        Ok(file)
    }
    /// 与 select 共用的跨进程锁，保护意图提交直到发布或补偿结束。
    fn selection_lock(&self) -> Result<std::fs::File, DshError> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| DshError::new(codes::STATE_IO, "registry has no parent"))?;
        std::fs::create_dir_all(parent)
            .map_err(|e| DshError::new(codes::STATE_IO, e.to_string()))?;
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(self.path.with_extension("selection.lock"))
            .map_err(|e| DshError::new(codes::STATE_IO, e.to_string()))?;
        file.lock_exclusive()
            .map_err(|e| DshError::new(codes::STATE_IO, e.to_string()))?;
        Ok(file)
    }

    pub fn selection_commit_guard(
        &self,
        id: &str,
        revision: u64,
    ) -> Result<std::fs::File, DshError> {
        let guard = self.selection_lock()?;
        self.with_selection_revision(id, revision, || ())?;
        Ok(guard)
    }

    fn load(&self) -> Result<RegistryData, DshError> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
                DshError::new(codes::STATE_IO, format!("invalid instance registry: {e}"))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RegistryData::default()),
            Err(e) => Err(DshError::new(
                codes::STATE_IO,
                format!("read instance registry: {e}"),
            )),
        }
    }
    fn save(&self, data: &RegistryData) -> Result<(), DshError> {
        let bytes =
            serde_json::to_vec(data).map_err(|e| DshError::new(codes::STATE_IO, e.to_string()))?;
        let parent = self
            .path
            .parent()
            .ok_or_else(|| DshError::new(codes::STATE_IO, "registry has no parent"))?;
        std::fs::create_dir_all(parent)
            .map_err(|e| DshError::new(codes::STATE_IO, e.to_string()))?;
        let tmp = self
            .path
            .with_extension(format!("{}.tmp", rand::random::<u64>()));
        let result = (|| {
            use std::io::Write;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut f = options.open(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
            std::fs::rename(&tmp, &self.path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result.map_err(|e| DshError::new(codes::STATE_IO, format!("write instance registry: {e}")))
    }
    pub fn register(
        &self,
        kind: HostKind,
        profile: &Path,
        fingerprint: &str,
    ) -> Result<SelectedInstance, DshError> {
        let profile_dir = std::fs::canonicalize(profile)
            .map_err(|e| DshError::new(codes::STATE_IO, format!("profile: {e}")))?;
        if !profile_dir.is_dir() {
            return Err(DshError::new(
                codes::PROFILE_NOT_INITIALIZED,
                "profile is not a directory",
            ));
        }
        if fingerprint.is_empty() {
            return Err(DshError::new(
                codes::AUTH_SIGNATURE,
                "empty plugin fingerprint",
            ));
        }
        let _lock = self.lock()?;
        let mut data = self.load()?;
        if let Some(existing) = data
            .instances
            .iter()
            .find(|i| i.kind == kind && i.profile_dir == profile_dir)
        {
            return if existing.identity_fingerprint.is_empty() {
                // H05 审查 I2：受信身份**落账**时刻——候选配置登记（空指纹）之后，
                // 首次授权完成把真实指纹锚到既有记录（同一 instance_id，不产生第二行）。
                // 空指纹不是「身份变化」，是「尚未锚定」。只读证据随锚同步落账
                // （Round-8 P1-3：锚定即证据——退役后只读历史归属凭它对位墓碑）。
                let mut anchored = existing.clone();
                anchored.identity_fingerprint = fingerprint.to_owned();
                anchored.read_evidence_fingerprint = fingerprint.to_owned();
                let previous = existing.identity_fingerprint.clone();
                let previous_evidence = existing.read_evidence_fingerprint.clone();
                if let Some(pos) = data
                    .instances
                    .iter()
                    .position(|i| i.instance_id == existing.instance_id)
                {
                    data.instances[pos] = anchored.clone();
                    if let Err(e) = self.save(&data) {
                        data.instances[pos].identity_fingerprint = previous;
                        data.instances[pos].read_evidence_fingerprint = previous_evidence;
                        return Err(e);
                    }
                }
                Ok(anchored)
            } else if existing.identity_fingerprint == fingerprint {
                // Round-8 P1-3 复审 Important 回填：升级携带的锚定记录（旧构建只写
                // 锚、无只读证据字段）经再登记触达时补齐证据——锚只能由授权侧写点
                // 落账，复制锚为证据不虚构授权。
                if existing.read_evidence_fingerprint.is_empty() {
                    let mut backfilled = existing.clone();
                    backfilled.read_evidence_fingerprint = existing.identity_fingerprint.clone();
                    if let Some(pos) = data
                        .instances
                        .iter()
                        .position(|i| i.instance_id == existing.instance_id)
                    {
                        data.instances[pos] = backfilled.clone();
                        self.save(&data)?;
                    }
                    return Ok(backfilled);
                }
                Ok(existing.clone())
            } else {
                Err(DshError::new(
                    codes::AUTH_SIGNATURE,
                    "profile plugin identity changed",
                ))
            };
        }
        let instance = SelectedInstance {
            // 稳定实例 ID：与发现侧（identity::discover_all）同一派生来源；
            // 不用 PID/端口（端口复用/重启不换身份——H02 取证与验收②）。
            instance_id: instance_id_for(kind, &profile_dir),
            kind,
            profile_dir,
            dsh_home: None,
            identity_fingerprint: fingerprint.to_owned(),
            read_evidence_fingerprint: fingerprint.to_owned(),
        };
        data.instances.push(instance.clone());
        if let Err(e) = self.save(&data) {
            data.instances.pop();
            return Err(e);
        }
        Ok(instance)
    }

    /// 候选配置登记（H05 审查 I2 阶段边界）：只记 kind + 规范化 profile，
    /// `identity_fingerprint` 置空 = **尚未受信锚定**。这是打破「登记需要指纹、
    /// 指纹需要授权、授权需要登记」循环的受限首次装配路径：候选登记后即可建
    /// 实例 auth 域与管理器（安装固定包不依赖未完成的授权），首次绑定完成后再以
    /// 探针指纹调 [`Self::register`] 落账。空指纹记录在 resolve/授权面一律不可核验
    /// 拒绝（探针无 Bound 行 → None；有 Bound 行但未锚定 → 指纹门不连续），绝不
    /// 当作已通过。
    pub fn register_candidate(
        &self,
        kind: HostKind,
        profile: &Path,
    ) -> Result<SelectedInstance, DshError> {
        self.register_candidate_with_home(kind, profile, None)
    }

    /// 将发现证明过的 home 与物理 profile 一并登记，不改变实例 ID。
    pub fn register_candidate_with_home(
        &self,
        kind: HostKind,
        profile: &Path,
        home: Option<&Path>,
    ) -> Result<SelectedInstance, DshError> {
        let profile_dir = std::fs::canonicalize(profile)
            .map_err(|e| DshError::new(codes::STATE_IO, format!("profile: {e}")))?;
        if !profile_dir.is_dir() {
            return Err(DshError::new(
                codes::PROFILE_NOT_INITIALIZED,
                "profile is not a directory",
            ));
        }
        let dsh_home = match home {
            Some(home) => {
                let home = std::fs::canonicalize(home)
                    .map_err(|e| DshError::new(codes::STATE_IO, format!("dsh home: {e}")))?;
                let logical = home.join("profiles").join(kind.as_str());
                if std::fs::canonicalize(logical).ok().as_ref() != Some(&profile_dir) {
                    return Err(DshError::new(
                        codes::INSTANCE_UNVERIFIED,
                        "home and profile do not identify the same directory",
                    ));
                }
                Some(home)
            }
            None => None,
        };
        let _lock = self.lock()?;
        let mut data = self.load()?;
        if let Some(pos) = data
            .instances
            .iter()
            .position(|i| i.kind == kind && i.profile_dir == profile_dir)
        {
            let existing = &data.instances[pos];
            if let (Some(old), Some(new)) = (&existing.dsh_home, &dsh_home) {
                if old != new {
                    return Err(DshError::new(
                        codes::INSTANCE_UNVERIFIED,
                        "profile is already associated with another home",
                    ));
                }
            }
            if existing.dsh_home.is_none() && dsh_home.is_some() {
                data.instances[pos].dsh_home = dsh_home;
                self.save(&data)?;
            }
            return Ok(data.instances[pos].clone());
        }
        let instance = SelectedInstance {
            instance_id: instance_id_for(kind, &profile_dir),
            kind,
            profile_dir,
            dsh_home,
            identity_fingerprint: String::new(),
            read_evidence_fingerprint: String::new(),
        };
        data.instances.push(instance.clone());
        if let Err(e) = self.save(&data) {
            data.instances.pop();
            return Err(e);
        }
        Ok(instance)
    }
    pub fn select(&self, id: &str) -> Result<SelectedInstance, DshError> {
        let _selection = self.selection_lock()?;
        let _lock = self.lock()?;
        let mut data = self.load()?;
        let selected = data
            .instances
            .iter()
            .find(|i| i.instance_id == id)
            .cloned()
            .ok_or_else(|| DshError::new(codes::HOST_NOT_RUNNING, "instance not registered"))?;
        if data.selected.as_deref() != Some(id) {
            data.selection_revision = data.selection_revision.checked_add(1).ok_or_else(|| {
                DshError::new(codes::STATE_IO, "instance selection revision exhausted")
            })?;
            data.selected = Some(id.to_owned());
            self.save(&data)?;
        }
        Ok(selected)
    }
    /// 连续切换原子对（销项复审【2】P2）：在同一把 `selection.lock` 内读取
    /// **实际前任**并翻转选择。此前 `selected()`（锁外）+ `select()`（锁内）
    /// 两段式在并发切换下两个 writer 都会读到同一旧选择——关错页/漏关活连接。
    ///
    /// 返回：`(实际前任 id, 新选登记, 选择租约)`。租约存活期间其他选择翻转
    /// 无法插入——「关前任控制台活连接」等翻转后置动作必须在租约存续期内
    /// 完成，保证每个 writer 关的都是自己实际离开的那一代选择。Drop 即释放锁。
    pub fn select_exclusive(
        &self,
        id: &str,
    ) -> Result<(Option<String>, SelectedInstance, SelectionLease), DshError> {
        let selection = self.selection_lock()?;
        let _lock = self.lock()?;
        let mut data = self.load()?;
        let selected = data
            .instances
            .iter()
            .find(|i| i.instance_id == id)
            .cloned()
            .ok_or_else(|| DshError::new(codes::HOST_NOT_RUNNING, "instance not registered"))?;
        let previous = data.selected.clone();
        if data.selected.as_deref() != Some(id) {
            data.selection_revision = data.selection_revision.checked_add(1).ok_or_else(|| {
                DshError::new(codes::STATE_IO, "instance selection revision exhausted")
            })?;
            data.selected = Some(id.to_owned());
            self.save(&data)?;
        }
        Ok((previous, selected, SelectionLease(selection)))
    }
    /// 同一文件锁下读取选择与版本，识别 A→B→A。
    pub fn selection_snapshot(&self) -> Result<(Option<SelectedInstance>, u64), DshError> {
        let _lock = self.lock()?;
        let data = self.load()?;
        let selected = match data.selected.as_ref() {
            Some(id) => Some(
                data.instances
                    .iter()
                    .find(|item| &item.instance_id == id)
                    .cloned()
                    .ok_or_else(|| DshError::new(codes::STATE_IO, "selected instance missing"))?,
            ),
            None => None,
        };
        Ok((selected, data.selection_revision))
    }

    /// 仅在同一选择代次仍生效时执行短小的内存操作。
    pub fn with_selection_revision<T>(
        &self,
        id: &str,
        revision: u64,
        operation: impl FnOnce() -> T,
    ) -> Result<T, DshError> {
        let _lock = self.lock()?;
        let data = self.load()?;
        if data.selected.as_deref() != Some(id) || data.selection_revision != revision {
            return Err(DshError::new(
                codes::INSTANCE_SWITCHED,
                "instance selection changed",
            ));
        }
        Ok(operation())
    }

    /// 已登记实例 ID 列表（H07 最终批 I1：绑定入口域判定消费）。
    pub fn registered_ids(&self) -> Result<Vec<String>, DshError> {
        let _lock = self.lock()?;
        let data = self.load()?;
        Ok(data
            .instances
            .iter()
            .map(|i| i.instance_id.clone())
            .collect())
    }

    pub fn selected(&self) -> Result<Option<SelectedInstance>, DshError> {
        let _lock = self.lock()?;
        let data = self.load()?;
        match &data.selected {
            None => Ok(None),
            Some(id) => data
                .instances
                .iter()
                .find(|i| &i.instance_id == id)
                .cloned()
                .map(Some)
                .ok_or_else(|| DshError::new(codes::STATE_IO, "selected instance missing")),
        }
    }

    /// 按 ID 取已登记实例（登记面只读；未登记 = None——resolve_session 对未登记
    /// 实例仍可核验（派生 ID 只能命中已核验活体），但**没有指纹可比**，指纹门不适用）。
    fn registered_opt(&self, instance_id: &str) -> Result<Option<SelectedInstance>, DshError> {
        let _lock = self.lock()?;
        let data = self.load()?;
        Ok(data
            .instances
            .iter()
            .find(|i| i.instance_id == instance_id)
            .cloned())
    }

    /// 按 ID 取已登记实例（H05 生产装配入口；未登记 = 明确错误，装配面据此
    /// fail-closed，不给未登记实例造任何授权/安装域）。
    /// 全部已登记实例（含当前未运行的）。
    ///
    /// 用途：实例列表要把「已登记但现在没跑」的条目也报出来——用户退出 DSH 之后
    /// 活体发现为空，若列表只剩活体，前端就拿不到 instanceId，「重开」入口消失。
    pub fn registered_all(&self) -> Result<Vec<SelectedInstance>, DshError> {
        let _lock = self.lock()?;
        Ok(self.load()?.instances)
    }

    pub fn registered(&self, instance_id: &str) -> Result<SelectedInstance, DshError> {
        self.registered_opt(instance_id)?.ok_or_else(|| {
            DshError::new(
                codes::HOST_NOT_RUNNING,
                format!("instance {instance_id} is not registered"),
            )
        })
    }

    /// **指纹 CAS**（M03）：在一次 `lock → load → compare → save` 内把
    /// `expected` 换成 `new`。
    ///
    /// 为什么必须是 CAS 而不是「读一下再写」：两条授权路径可能并发提交同一实例。
    /// 先读后写的形态下，两边都会看到旧值并通过自己的检查，后写者覆盖先写者——
    /// 结果是「两把不同的钥都以为自己绑上了」。CAS 把比较与写入放进同一把文件锁，
    /// 只有第一个能成功。
    ///
    /// 语义：
    /// * 登记值 != `expected` → `DSH_AUTH_SIGNATURE`（身份不连续，不覆盖）；
    /// * `expected == new` → 幂等成功（不写盘）；
    /// * `new` 为空 → 拒绝（空串不是身份）。
    pub fn compare_and_set_fingerprint(
        &self,
        instance_id: &str,
        expected: &str,
        new: &str,
    ) -> Result<SelectedInstance, DshError> {
        if new.is_empty() {
            return Err(DshError::new(
                codes::AUTH_SIGNATURE,
                "empty plugin fingerprint cannot be committed",
            ));
        }
        self.compare_and_set_fingerprint_inner(instance_id, expected, new, false, None, None)
    }

    /// 仅用于补偿本次已经成功的指纹提交；允许恢复首绑前的空值。`previous_evidence`
    /// 给出时（第九轮 P1-2 Important-1），登记只读证据一并恢复为提交前值——
    /// 锚回滚为空时证据不随 CAS 的空 new 走「保持原值」分支，失败提交推进的
    /// 新钥证据必须显式回写，否则卸载保留的历史只读证据丢失（历史 403）。
    pub(crate) fn restore_fingerprint_for_rollback(
        &self,
        instance_id: &str,
        expected: &str,
        previous: &str,
        previous_evidence: Option<&str>,
    ) -> Result<SelectedInstance, DshError> {
        self.compare_and_set_fingerprint_inner(
            instance_id,
            expected,
            previous,
            false,
            None,
            previous_evidence,
        )
    }

    pub(crate) fn compare_and_set_selected_fingerprint(
        &self,
        instance_id: &str,
        expected: &str,
        new: &str,
    ) -> Result<SelectedInstance, DshError> {
        if new.is_empty() {
            return Err(DshError::new(
                codes::AUTH_SIGNATURE,
                "empty plugin fingerprint cannot be committed",
            ));
        }
        self.compare_and_set_fingerprint_inner(instance_id, expected, new, true, None, None)
    }

    pub(crate) fn compare_and_set_intent_fingerprint(
        &self,
        instance_id: &str,
        revision: u64,
        expected: &str,
        new: &str,
    ) -> Result<SelectedInstance, DshError> {
        if new.is_empty() {
            return Err(DshError::new(
                codes::AUTH_SIGNATURE,
                "empty plugin fingerprint cannot be committed",
            ));
        }
        self.compare_and_set_fingerprint_inner(
            instance_id,
            expected,
            new,
            true,
            Some(revision),
            None,
        )
    }

    fn compare_and_set_fingerprint_inner(
        &self,
        instance_id: &str,
        expected: &str,
        new: &str,
        require_selected: bool,
        revision: Option<u64>,
        restore_evidence: Option<&str>,
    ) -> Result<SelectedInstance, DshError> {
        let _lock = self.lock()?;
        let mut data = self.load()?;
        let pos = data
            .instances
            .iter()
            .position(|i| i.instance_id == instance_id)
            .ok_or_else(|| {
                DshError::new(
                    codes::HOST_NOT_RUNNING,
                    format!("instance {instance_id} is not registered"),
                )
            })?;
        if require_selected && data.selected.as_deref() != Some(instance_id) {
            return Err(DshError::new(
                codes::AUTH_SIGNATURE,
                "selected instance changed",
            ));
        }
        if revision.is_some_and(|expected| expected != data.selection_revision) {
            return Err(DshError::new(
                codes::INSTANCE_SWITCHED,
                "instance selection revision changed",
            ));
        }
        let current = data.instances[pos].identity_fingerprint.clone();
        if current != expected {
            return Err(DshError::new(
                codes::AUTH_SIGNATURE,
                format!(
                    "instance {instance_id} fingerprint changed under us (expected [{expected}], found [{current}]); re-authorize required"
                ),
            ));
        }
        if current == new {
            // Round-8 P1-3 复审 Important 回填：升级携带记录（锚在、证据空）经幂等
            // 锚定触达时补齐证据（同锚复制，不虚构授权）。
            if data.instances[pos].read_evidence_fingerprint.is_empty() {
                data.instances[pos].read_evidence_fingerprint = new.to_owned();
                self.save(&data)?;
            }
            return Ok(data.instances[pos].clone());
        }
        data.instances[pos].identity_fingerprint = new.to_owned();
        // Round-8 P1-3：非空指纹写同步落只读证据（锚定即证据）。第九轮 P1-2
        // Important-1：补偿回滚（`restore_evidence` 在场）显式把证据恢复为提交
        // 回执保存的**提交前值**——锚回滚为空串时「保持原值」分支会让证据停留
        // 在失败提交推进的新钥上，卸载保留的历史只读证据随之丢失。
        let previous_evidence = data.instances[pos].read_evidence_fingerprint.clone();
        match restore_evidence {
            Some(evidence) => data.instances[pos].read_evidence_fingerprint = evidence.to_owned(),
            None if !new.is_empty() => {
                data.instances[pos].read_evidence_fingerprint = new.to_owned();
            }
            _ => {}
        }
        if let Err(e) = self.save(&data) {
            data.instances[pos].identity_fingerprint = current;
            data.instances[pos].read_evidence_fingerprint = previous_evidence;
            return Err(e);
        }
        Ok(data.instances[pos].clone())
    }

    /// 显式指纹重绑（H05）：重新授权完成后把**新钥**指纹登记到已登记实例——
    /// 「指纹不连续 → 重新选择/授权」这个出口的收尾步（H04 的 register 拒绝同
    /// profile 换指纹；本入口是用户显式重新授权后的登记面更新，不是自动跟随）。
    /// 未登记实例与空指纹一律拒绝。
    pub fn rebind_fingerprint(
        &self,
        instance_id: &str,
        fingerprint: &str,
    ) -> Result<SelectedInstance, DshError> {
        if fingerprint.is_empty() {
            return Err(DshError::new(
                codes::AUTH_SIGNATURE,
                "empty plugin fingerprint",
            ));
        }
        let _lock = self.lock()?;
        let mut data = self.load()?;
        let pos = data
            .instances
            .iter()
            .position(|i| i.instance_id == instance_id)
            .ok_or_else(|| {
                DshError::new(
                    codes::HOST_NOT_RUNNING,
                    format!("instance {instance_id} is not registered"),
                )
            })?;
        let previous = data.instances[pos].identity_fingerprint.clone();
        // Round-8 P1-3：重绑同步推进只读证据（证据跟随最新受信钥）。
        let previous_evidence = data.instances[pos].read_evidence_fingerprint.clone();
        data.instances[pos].identity_fingerprint = fingerprint.to_owned();
        data.instances[pos].read_evidence_fingerprint = fingerprint.to_owned();
        if let Err(e) = self.save(&data) {
            data.instances[pos].identity_fingerprint = previous;
            data.instances[pos].read_evidence_fingerprint = previous_evidence;
            return Err(e);
        }
        Ok(data.instances[pos].clone())
    }

    /// 退役实例登记的插件身份指纹（P1-2 同钥循环复装）：旧钥已随卸载/升级兼容
    /// 清理退役（身份目录已删 + 撤权墓碑拦截旧钥重连），登记锚随之清空 → 空回
    /// 「候选未锚定」态；再开启装出的全新钥经布防意图走**首绑**（TOFU），不进
    /// 重绑面板。旧钥绝不因锚清空复活（墓碑在绑定账本里，与登记面无关）。
    ///
    /// Round-8 P1-3：只清当代锚，**只读证据保留**——关闭+卸载后只读历史归属
    /// 凭「登记行保留的证据 + 绑定账本撤权墓碑」仍可证明（证据随锚定落账、
    /// 随重绑推进，独立于卸载销毁的锚与插件身份目录）。复审 Important 回填：
    /// 升级携带的锚定记录（旧构建写的行、无证据字段）在此**销锚前回填证据**
    /// （证据 := 锚）——卸载/升级兼容清理的每条退役路径都经本函数，回填点
    /// 保证不存在「锚与证据同时为空」的退役结果。
    ///
    /// 幂等收尾面（复审 Minor-2）：登记锚已空 → 原样成功；**实例未登记**（作业
    /// 中途被注销）→ 同样成功（无可清即达成目标）——退役失败不得成为卸载/兼容
    /// 清理重试的永久卡点。
    pub fn retire_identity_fingerprint(&self, instance_id: &str) -> Result<(), DshError> {
        let _lock = self.lock()?;
        let mut data = self.load()?;
        let Some(pos) = data
            .instances
            .iter()
            .position(|i| i.instance_id == instance_id)
        else {
            return Ok(()); // 未登记 = 无可清（幂等）
        };
        if data.instances[pos].identity_fingerprint.is_empty() {
            return Ok(());
        }
        let previous = data.instances[pos].identity_fingerprint.clone();
        // Round-8 P1-3 复审 Important 回填（关键点）：升级携带的锚定记录（旧构建
        // 只写锚、证据字段缺省为空）在此退役——**销毁锚之前先保全证据**（锚在 =
        // 证据缺）。没有这一步，升级用户首次关闭+卸载即复发 403（锚与证据同时
        // 为空，只读门拒绝）。锚只能由授权侧写点落账（锚定/重绑/CAS），把锚复制
        // 为证据不虚构授权；退役后只读仍须对位绑定账本绑定起源行（墓碑），硬下界
        // 不变。
        let previous_evidence = data.instances[pos].read_evidence_fingerprint.clone();
        if previous_evidence.is_empty() {
            data.instances[pos].read_evidence_fingerprint = previous.clone();
        }
        data.instances[pos].identity_fingerprint = String::new();
        if let Err(e) = self.save(&data) {
            data.instances[pos].identity_fingerprint = previous;
            data.instances[pos].read_evidence_fingerprint = previous_evidence;
            return Err(e);
        }
        Ok(())
    }

    /// 按 kind + 规范化 profile 查登记记录（不受路径拼写差影响的登记面查找）。
    /// 审查 I2 残留：唯一候选兼容路径返回前用它判断「该实例其实已登记」——
    /// 已登记则指纹门必须执行，豁免只属于真正未登记的实例。
    fn registered_for(
        &self,
        kind: HostKind,
        normalized_profile: &Path,
    ) -> Result<Option<SelectedInstance>, DshError> {
        let _lock = self.lock()?;
        let data = self.load()?;
        Ok(data
            .instances
            .iter()
            .find(|i| i.kind == kind && i.profile_dir == normalized_profile)
            .cloned())
    }

    /// 活体环境（缺省构造的登记没有 env，活体核验如实报错而非瞎猜）。
    fn live_env(&self) -> Result<&Arc<DshEnvironment>, DshError> {
        self.env.as_ref().ok_or_else(|| {
            DshError::new(
                codes::UNSUPPORTED,
                "instance registry has no live environment for verification",
            )
        })
    }

    /// 指纹门（审查 I2）：已登记实例必须把**当代受信插件指纹**与登记值核对。
    /// 探针未接线 / 当代来源不可用 → `DSH_INSTANCE_UNVERIFIED`（绝不当作通过）；
    /// 指纹变化 → `DSH_AUTH_SIGNATURE`（身份不连续，要求重新选择/授权）。
    /// 未登记实例没有登记值可比，本门不适用（身份由派生 ID + 已核验活体承担）。
    fn fingerprint_gate(&self, registered: &SelectedInstance) -> Result<(), DshError> {
        // 外层 None = 探针未接线；内层 None = 接线但当代来源不可用——两者都是
        // 「不可核验」，绝不当作通过（审查 I2 隔离环境口径）。
        let observation: Option<Option<String>> = match &self.identity_probe {
            None => None,
            Some(probe) => Some(probe.current_fingerprint(registered)?),
        };
        match observation {
            None | Some(None) => Err(DshError::new(
                codes::INSTANCE_UNVERIFIED,
                format!(
                    "instance {} plugin fingerprint unverifiable (no identity probe or no current source); re-select/re-authorize required",
                    registered.instance_id
                ),
            )),
            Some(Some(current)) if current == registered.identity_fingerprint => Ok(()),
            Some(Some(current)) => Err(DshError::new(
                codes::AUTH_SIGNATURE,
                format!(
                    "instance {} plugin fingerprint changed (registered {}, now {current}); re-select/re-authorize required",
                    registered.instance_id, registered.identity_fingerprint
                ),
            )),
        }
    }

    /// 解析已选实例的当代活体（§3.1：每次核验）。运行期 `LocalHost.epoch/authority`
    /// 由本次发现产生；选择记录不携带 Cookie。
    ///
    /// * 无选择：**唯一已核验**候选 = 该候选（单实例部署的兼容口径）；多个宿主在场
    ///   （多核验候选 / 有未核验或不可归属宿主）= `DSH_TARGET_AMBIGUOUS`（审查 I5：
    ///   多宿主必须显式选择，不取第一项也不静默吞掉）；只有未核验宿主 =
    ///   `DSH_INSTANCE_UNVERIFIED`；无宿主 = `DSH_HOST_NOT_RUNNING`。
    /// * 有选择：**已核验**候选中必须有 kind + profile 一致的活体且派生实例 ID 与
    ///   登记一致 + 当代指纹与登记值核对（审查 I2）。实例消失或身份不连续 → 明确
    ///   错误，**绝不回退到另一个候选、绝不自动切换**（spec §3.2）。
    pub fn resolve_selected(&self) -> Result<LocalHost, DshError> {
        let selected = self.selected()?;
        let env = self.live_env()?;
        let discovery = super::identity::discover_detailed(env)?;
        let verified: Vec<&InstanceCandidate> =
            discovery.candidates.iter().filter(|c| c.verified).collect();
        let Some(selected) = selected else {
            // 无选择（I5：唯一性决策在这里，发现层只报事实）。多宿主口径 =
            // 已核验候选 + 未核验候选 + 不可归属 web + 核验失败 Desktop，合计 >1 即歧义。
            let unverified_count = discovery.candidates.len() - verified.len();
            let observed_hosts = verified.len()
                + discovery.unresolved_desktop_hosts
                + discovery.unattributable_web_hosts
                + unverified_count;
            return match verified.len() {
                1 if observed_hosts == 1 => {
                    // 审查 I2 残留：兼容豁免只属于**真正未登记**的实例——候选若已
                    // 登记（kind+profile 匹配），指纹门照样执行（探针未接线/来源
                    // 不可用/指纹变化都拒绝），不得借「无保存选择」绕过。
                    let candidate = verified[0];
                    if let Some(registered) =
                        self.registered_for(candidate.kind, &candidate.profile_dir)?
                    {
                        if registered.instance_id != candidate.instance_id {
                            return Err(DshError::new(
                                codes::AUTH_SIGNATURE,
                                format!(
                                    "instance identity changed ({} -> {}); re-select required",
                                    registered.instance_id, candidate.instance_id
                                ),
                            ));
                        }
                        self.fingerprint_gate(&registered)?;
                    }
                    Ok(candidate.host.clone())
                }
                0 if observed_hosts == 0 => {
                    match discovery.desktop_error.or(discovery.web_error) {
                        Some(e) => Err(e),
                        None => Err(DshError::new(
                            codes::HOST_NOT_RUNNING,
                            "no live dsh instance and nothing selected",
                        )),
                    }
                }
                0 if observed_hosts == 1 => {
                    // 唯一观测宿主但不可解析：单进程核验错误如实上抛（比笼统歧义
                    // 更可诊断），未核验候选报未核验。
                    if let Some(e) = discovery.desktop_error.or(discovery.web_error) {
                        return Err(e);
                    }
                    Err(DshError::new(
                        codes::INSTANCE_UNVERIFIED,
                        "dsh host observed but none with verified home attribution; awaiting trusted plugin verification",
                    ))
                }
                _ => Err(DshError::new(
                    codes::TARGET_AMBIGUOUS,
                    format!(
                        "{observed_hosts} dsh host(s) observed ({} verified, {unverified_count} unverified, {} unattributable web, {} unresolved desktop); explicit selection required",
                        verified.len(),
                        discovery.unattributable_web_hosts,
                        discovery.unresolved_desktop_hosts
                    ),
                )),
            };
        };
        // 有选择：只在**已核验**候选里匹配（未核验候选 resolve 不得消费——审查 I1）。
        let live: Vec<&InstanceCandidate> = verified
            .iter()
            .copied()
            .filter(|c| {
                c.kind == selected.kind && normalize_profile(&c.profile_dir) == selected.profile_dir
            })
            .collect();
        // 审查 N1：目标实例必须对应**唯一运行活体**——两个不同 PID 携同一
        // profile 会派生同一实例 ID，取首项等于在竞争 writer 间猜边，拒绝。
        if live.len() > 1 {
            return Err(DshError::new(
                codes::TARGET_AMBIGUOUS,
                format!(
                    "instance {} has {} distinct live processes (pids {:?}); instance ambiguity",
                    selected.instance_id,
                    live.len(),
                    live.iter().map(|c| c.host.pid).collect::<Vec<_>>()
                ),
            ));
        }
        let candidate = live.into_iter().next();
        let Some(candidate) = candidate else {
            // 匹配失败先看「该 kind 的单进程核验错误」（如监听缺失），比笼统的
            // 不在场更可诊断；未核验候选在场则如实报未核验。
            let kind_error = match selected.kind {
                HostKind::Desktop => discovery.desktop_error.as_ref(),
                HostKind::Web => discovery.web_error.as_ref(),
            };
            if let Some(e) = kind_error {
                return Err(e.clone());
            }
            let unverified_match = discovery.candidates.iter().any(|c| {
                !c.verified
                    && c.kind == selected.kind
                    && normalize_profile(&c.profile_dir) == selected.profile_dir
            });
            if unverified_match {
                return Err(DshError::new(
                    codes::INSTANCE_UNVERIFIED,
                    format!(
                        "selected instance {} observed but home attribution unverified; awaiting trusted plugin verification",
                        selected.instance_id
                    ),
                ));
            }
            return Err(DshError::new(
                codes::HOST_NOT_RUNNING,
                format!(
                    "selected instance {} ({}) is not live; re-select required — no fallback",
                    selected.instance_id,
                    selected.profile_dir.display()
                ),
            ));
        };
        if candidate.instance_id != selected.instance_id {
            return Err(DshError::new(
                codes::AUTH_SIGNATURE,
                format!(
                    "instance identity changed ({} -> {}); re-select required",
                    selected.instance_id, candidate.instance_id
                ),
            ));
        }
        // 指纹门（I2）：已选择 = 已登记，当代指纹必须与登记值连续。
        self.fingerprint_gate(&selected)?;
        Ok(candidate.host.clone())
    }

    /// 读取受信实例历史：只检查登记身份与会话头，不依赖活体写入资格。
    pub fn resolve_readonly_root(
        &self,
        instance_id: &str,
        session_id: &str,
    ) -> Result<PathBuf, DshError> {
        self.resolve_readonly_root_with(
            instance_id,
            session_id,
            &READONLY_HEADER_SCAN,
            read_readonly_header,
        )
    }

    /// 只读解析的预算包裹壳（发现②加固，2026-11-16 真机证据：只读消息走
    /// generation_logs readdir 楔死 getdirentries 数分钟 → 手机面「加载中」
    /// 永不完成）：整段解析装进请求级死线（9s 口径，见
    /// [`crate::monitor::walk_budget::READONLY_ROOT_BUDGET`]）。超时/Busy →
    /// `DSH_READ_TIMEOUT`（读取超时降级；超时 ≠ 通过——只读门的判据一行不动，
    /// 任何门在超时时按「不可证明」拒绝，硬下界不放松）；卡住的遍历线程
    /// detached 保留。/m 的 session-messages 端点把它映射为 HTTP 504
    /// （「读取超时，点重试」语义）。
    fn resolve_readonly_root_with<F>(
        &self,
        instance_id: &str,
        session_id: &str,
        scan: &SessionFileScan,
        parse: F,
    ) -> Result<PathBuf, DshError>
    where
        F: Fn(&Path) -> Option<String> + Send + Sync + 'static,
    {
        self.resolve_readonly_root_in_budget(
            instance_id,
            session_id,
            scan,
            parse,
            readonly_root_budget(),
        )
    }

    /// 单测用：预算参数注入（NEEDS_FIX Important-4——取消全局预算覆写缝，预算走
    /// 参数，杜绝跨用例互抢/泄漏影响其他只读解析测试）。
    #[cfg(test)]
    fn resolve_readonly_root_with_budget_for_test<F>(
        &self,
        instance_id: &str,
        session_id: &str,
        scan: &SessionFileScan,
        parse: F,
        budget: Duration,
    ) -> Result<PathBuf, DshError>
    where
        F: Fn(&Path) -> Option<String> + Send + Sync + 'static,
    {
        self.resolve_readonly_root_in_budget(instance_id, session_id, scan, parse, budget)
    }

    /// 预算包裹壳内核（预算参数化）。
    fn resolve_readonly_root_in_budget<F>(
        &self,
        instance_id: &str,
        session_id: &str,
        scan: &SessionFileScan,
        parse: F,
        budget: Duration,
    ) -> Result<PathBuf, DshError>
    where
        F: Fn(&Path) -> Option<String> + Send + Sync + 'static,
    {
        // 克隆 = 句柄（Arc 字段共享，见 InstanceRegistry Clone 注），不复制登记数据；
        // parse/scan 按值进预算线程（SessionFileScan 克隆只复制 namespace 手柄，
        // 缓存数据在全局 registry 按 ns 共享，不损失记忆化）。parse 包 Arc 跨线程
        //（Arc<F>: Send 需要 F: Send + Sync——Arc 可克隆共享，两界都必须）。
        // 只读解析是纯读（无状态提交），取消令牌无需检查——超时后产物直接丢弃。
        let registry = self.clone();
        let instance_id = instance_id.to_string();
        let session_id = session_id.to_string();
        let scan = scan.clone();
        let parse = Arc::new(parse);
        crate::monitor::walk_budget::run_bounded("readonly-root", budget, move || {
            registry.resolve_readonly_root_unbounded(
                &instance_id,
                &session_id,
                &scan,
                parse.as_ref(),
            )
        })
        .map_err(|failure| match failure {
            // panic 保持原语义：载荷原样 resume（由调用方 catch_unwind 收敛）。
            crate::monitor::walk_budget::WalkError::Panicked(payload) => {
                std::panic::resume_unwind(payload)
            }
            other => DshError::new(
                codes::READ_TIMEOUT,
                format!("readonly root resolve not finished in budget ({other})"),
            ),
        })?
    }

    /// 只读解析体（原实现原样搬移；在预算线程内执行——预算判的就是本函数耗时）。
    fn resolve_readonly_root_unbounded<A>(
        &self,
        instance_id: &str,
        session_id: &str,
        scan: &SessionFileScan,
        parse: &A,
    ) -> Result<PathBuf, DshError>
    where
        A: Fn(&Path) -> Option<String>,
    {
        let deny = || {
            DshError::new(
                codes::SESSION_OWNERSHIP_UNPROVEN,
                "readonly session identity unproven",
            )
        };
        // 只读解析的实例候选：选定优先；无选定时的**兼容场景**（唯一登记实例，
        // 或唯一活体 compat 实例）与 auth_for_target 的回退判定同口径——
        // 身份可证明即允许读，多义仍然拒绝。
        let mut fingerprint_gate_applies = true;
        let selected = match self.selected()? {
            Some(s) => s,
            None => {
                // 兼容候选与**绑定入口同口径**：唯一登记实例优先（**离线可达**——
                // profile 取登记记录，读历史不需要活体）；多登记无选定 = 歧义拒绝
                //（用户须先选择——不猜）；零登记才允许唯一活体 compat（profile 由
                // 当代活体证明；离线 = 无身份证明，fail-closed）。
                let registered = self.registered_ids()?;
                let id = match registered.len() {
                    1 => Some(registered.first().cloned()),
                    0 => self.resolve_selected().ok().map(|h| Some(h.host_id)),
                    _ => {
                        return Err(DshError::new(
                            codes::HOST_NOT_RUNNING,
                            "multiple registered dsh instances; select one before reading",
                        ));
                    }
                };
                let Some(id) = id.filter(|id| id.as_deref() == Some(instance_id)) else {
                    return Err(deny());
                };
                let id = id.unwrap_or_default();
                match self.registered(&id).ok() {
                    Some(record) => {
                        // 已登记：profile/指纹全部来自登记记录；指纹门照常适用
                        record
                    }
                    None => {
                        // 未登记 compat 实例：无登记值可比 ⇒ 指纹门不适用（H04 语义）；
                        // profile 必须由**当代活体**证明（离线 = 无身份证明，fail-closed）
                        let live = self.resolve_selected().ok();
                        let Some(profile_dir) = live.as_ref().map(|h| h.profile_dir.clone()) else {
                            return Err(deny());
                        };
                        let kind = if profile_dir.components().any(|c| c.as_os_str() == "desktop") {
                            HostKind::Desktop
                        } else {
                            HostKind::Web
                        };
                        fingerprint_gate_applies = false;
                        SelectedInstance {
                            instance_id: id,
                            kind,
                            dsh_home: logical_home_of_profile_entry(&profile_dir),
                            profile_dir,
                            identity_fingerprint: String::new(),
                            read_evidence_fingerprint: String::new(),
                        }
                    }
                }
            }
        };
        if selected.instance_id != instance_id
            || (fingerprint_gate_applies
                && selected.identity_fingerprint.is_empty()
                && selected.read_evidence_fingerprint.is_empty())
            || instance_id_for(selected.kind, &selected.profile_dir) != instance_id
        {
            return Err(deny());
        }
        if fingerprint_gate_applies {
            // R02（复审 P1）：只读走**历史钉住规则**（与写路径的当代探针分离）。
            // 第九轮 P1-2 统一判据（卸载前后同一判断）：锚定与退役都以**绑定起源
            // 行**对位——活 Bound 行摘要 == 登记值（`pinned_fingerprint_for`），或
            // 最新起源行（Bound 或撤权墓碑——墓碑永久留存被撤钥）摘要 == 登记值
            // （`historical_fingerprint_for`）。撤权把 Bound 行翻成墓碑后、卸载
            // 退役清锚前的整个等待退出窗口内，anchored 登记不再恒 deny；仍未对位
            // （从未授权 / 指纹漂移 / 账本缺失畸形）一律 deny——入口第一道门、
            // home/profile 自洽与会话头核验照旧，所有权硬下界不放松。
            // 无 env（测试形态）：探针作为连续性预言机（Trusted → 登记锚自洽）。
            let anchored = !selected.identity_fingerprint.is_empty();
            let historical = match self.env.as_ref() {
                Some(env) => {
                    crate::services::dsh_control::auth::pinned_fingerprint_for(env, instance_id)
                        .or_else(|| {
                            crate::services::dsh_control::auth::historical_fingerprint_for(
                                env,
                                instance_id,
                            )
                        })
                }
                None if anchored => self
                    .identity_probe
                    .as_ref()
                    .and_then(|probe| probe.current_fingerprint(&selected).ok().flatten()),
                None => None,
            };
            let expected = if anchored {
                selected.identity_fingerprint.as_str()
            } else {
                selected.read_evidence_fingerprint.as_str()
            };
            if historical.as_deref() != Some(expected) {
                return Err(deny());
            }
        }
        let profile = std::fs::canonicalize(&selected.profile_dir).map_err(|_| deny())?;
        if profile != selected.profile_dir {
            return Err(deny());
        }
        // M06 统一判定：登记 home 必须可证明（登记值自洽，或经可信绑定账本
        // 唯一恢复）。只剩物理路径/证据冲突的记录一律拒绝——不从物理 profile
        // 反推 home（直连与外链在该形态下不可区分，反推可能读进另一实例）。
        // 本入口的既定拒绝语义 = `SESSION_OWNERSHIP_UNPROVEN`（历史只读保留下界）。
        let home = match resolve_registered_home(self.env.as_deref(), &selected, &profile) {
            RegisteredHomeResolution::Proven(home) => home,
            RegisteredHomeResolution::Unproven(reason) => {
                return Err(DshError::new(
                    codes::SESSION_OWNERSHIP_UNPROVEN,
                    format!(
                        "instance {} dsh home unproven ({}); start the original instance and re-select it",
                        selected.instance_id, reason
                    ),
                ));
            }
        };
        let logical_profile = home.join("profiles").join(selected.kind.as_str());
        if std::fs::canonicalize(logical_profile).ok().as_ref() != Some(&profile) {
            return Err(deny());
        }
        let root = std::fs::canonicalize(home).map_err(|_| deny())?;
        let sessions = std::fs::canonicalize(root.join("sessions")).map_err(|_| deny())?;
        if !sessions.starts_with(&root) {
            return Err(deny());
        }
        let mut count = 0;
        for project in std::fs::read_dir(&sessions).map_err(|_| deny())?.flatten() {
            let Ok(project) = std::fs::canonicalize(project.path()) else {
                continue;
            };
            if !project.starts_with(&sessions) || !project.is_dir() {
                continue;
            }
            let Ok(dirs) = std::fs::read_dir(project) else {
                continue;
            };
            for dir in dirs.flatten() {
                let Ok(dir) = std::fs::canonicalize(dir.path()) else {
                    continue;
                };
                if !dir.starts_with(&sessions) || !dir.is_dir() {
                    continue;
                }
                let Some((_, generation)) = crate::monitor::dsh::log::generation_logs(&dir).pop()
                else {
                    continue;
                };
                if !std::fs::canonicalize(&generation).is_ok_and(|p| p.starts_with(&dir)) {
                    continue;
                }
                // 先按文件身份/时间/大小验证首行缓存；未知头无法排除重复身份。
                std::fs::metadata(&generation)
                    .and_then(|m| m.modified())
                    .map_err(|_| deny())?;
                let header = cached_readonly_header(scan, &generation, parse);
                let id = header.as_ref().as_deref().ok_or_else(deny)?;
                if id == session_id {
                    count += 1;
                }
            }
        }
        if count != 1 {
            return Err(deny());
        }
        Ok(root)
    }

    /// 核验主会话归属并给出当代 Target（§3.1/§3.3；审查 I3 收紧）。
    ///
    /// 通过条件（全部满足，缺一拒绝）：
    /// ① session ID 原生形态（`session-` 前缀 + 受限字符集 + 长度界——同时阻断
    ///    路径逃逸）；② 该实例为当代活体候选且 **home 归属已核验**（未核验一律
    ///    拒绝）；③ 已登记实例的当代指纹与登记值连续（未登记无登记值可比，指纹门
    ///    不适用）；④ 会话**目录**（类型核验）存在于该实例 home 的 `sessions/` 树内
    ///    且规范化后不越界；⑤ 同一会话未被其他活体候选（含未核验者——宁拒不误归）
    ///    同时认领。
    ///
    /// 写归属的**最终**正面证明（受信插件握手 + 逐写活体核验）归 H05/H07；本入口
    /// 是 H04 能证明的硬下界，证不动就返回 `DSH_SESSION_OWNERSHIP_UNPROVEN`
    /// （历史只读保留），不猜。
    ///
    /// `Target.host_id` = 稳定实例 ID（wire 字段名不变）。
    pub fn resolve_session(&self, instance_id: &str, session_id: &str) -> Result<Target, DshError> {
        // ① 形态门：非原生形态直接拒绝（含路径逃逸向量）。
        if !is_native_session_id(session_id) {
            return Err(DshError::new(
                codes::SESSION_OWNERSHIP_UNPROVEN,
                format!(
                    "session id {session_id:?} is not a native dsh session id; ownership unproven"
                ),
            ));
        }
        let registered = self.registered_opt(instance_id)?;
        let env = self.live_env()?;
        let discovery = super::identity::discover_detailed(env)?;
        // 审查 N1：目标实例必须对应**唯一运行活体**——同 profile 双进程派生同一
        // 实例 ID 时，writer 归属在竞争者间不可证，拒绝（不取首项）。
        let same_instance: Vec<&InstanceCandidate> = discovery
            .candidates
            .iter()
            .filter(|c| c.instance_id == instance_id)
            .collect();
        if same_instance.len() > 1 {
            return Err(DshError::new(
                codes::SESSION_OWNERSHIP_UNPROVEN,
                format!(
                    "instance {instance_id} has {} distinct live processes (pids {:?}); writer ownership unproven",
                    same_instance.len(),
                    same_instance.iter().map(|c| c.host.pid).collect::<Vec<_>>()
                ),
            ));
        }
        let live = same_instance.first().copied().ok_or_else(|| {
            DshError::new(
                codes::HOST_NOT_RUNNING,
                format!("instance {instance_id} is not live; session ownership unverifiable"),
            )
        })?;
        // ③ 指纹门（已登记才有登记值可比）。
        if let Some(registered) = &registered {
            self.fingerprint_gate(registered)?;
        }
        // ② home 归属必须已核验（Web 在 H04 无正面证据 → 一律拒绝）。
        if !live.verified {
            return Err(DshError::new(
                codes::INSTANCE_UNVERIFIED,
                format!(
                    "instance {instance_id} home attribution unverified; session ownership unproven"
                ),
            ));
        }
        // ④ 活体候选保留原始 profile 入口；不能从外链后的物理目录反推 home。
        let live_home = normalize_profile(&session_home(&live.host.profile_dir));
        let home = match registered
            .as_ref()
            .and_then(|record| record.dsh_home.as_ref())
        {
            Some(saved) if normalize_profile(saved) != live_home => {
                return Err(DshError::new(
                    codes::INSTANCE_UNVERIFIED,
                    "live home differs from registered home",
                ));
            }
            Some(saved) => saved.clone(),
            None => live_home,
        };
        if !session_dir_owned(env, &home, session_id) {
            return Err(DshError::new(
                codes::SESSION_OWNERSHIP_UNPROVEN,
                format!(
                    "session {session_id} is not present as a directory under instance {instance_id} home; write ownership unproven"
                ),
            ));
        }
        // 未归属的 Web 可能持有同一会话；在证明其数据根前不能排除冲突。
        if !discovery.raw_web_hosts.is_empty() {
            return Err(DshError::new(
                codes::SESSION_OWNERSHIP_UNPROVEN,
                "an unverified Web host prevents proving unique session ownership",
            ));
        }
        // ⑤ 同一会话目录被其他活体候选同时认领 → writer 归属不可证（共享 home/
        // 克隆目录形态），拒绝而不是按 ID 猜（含未核验候选——宁拒不误归）。
        for other in discovery
            .candidates
            .iter()
            .filter(|c| c.instance_id != instance_id)
        {
            let other_home = normalize_profile(&session_home(&other.host.profile_dir));
            if session_dir_exists(env, &other_home, session_id) {
                return Err(DshError::new(
                    codes::SESSION_OWNERSHIP_UNPROVEN,
                    format!(
                        "session {session_id} exists under multiple live instances ({} and {}); write ownership unproven",
                        instance_id, other.instance_id
                    ),
                ));
            }
        }
        Ok(Target {
            host_id: instance_id.to_owned(),
            epoch: live.host.epoch,
            session_id: session_id.to_owned(),
        })
    }
}

/// DSH 原生会话 ID 形态（H02 实样 `session-9c6569fc-c781-4962-84e7-…`）：
/// `session-` 前缀 + 小写十六进制与连字符、总长有界、首尾不得为连字符。
/// 同时阻断 `..`/分隔符等路径逃逸向量（审查 I3「session ID 形态」）。
pub(crate) fn is_native_session_id(session_id: &str) -> bool {
    let Some(rest) = session_id.strip_prefix("session-") else {
        return false;
    };
    if session_id.len() > 64 || rest.len() < 8 {
        return false;
    }
    if rest.starts_with('-') || rest.ends_with('-') || rest.contains("--") {
        return false;
    }
    rest.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

/// 卡片归属钩子（审查 I4①；monitor 3s 扫描调用，生产装配在
/// `production::install_instance_attributor` 安装）。实现只回**已核验**实例 ID：
/// 同 home 唯一已核验候选 → `Some(id)`；多义 / 未核验 / 归属不可证 → `None`
/// （卡片不带实例 ID；新写由 hub 的 resolve_session 门拒绝——不猜）。
pub trait InstanceAttributor: Send + Sync {
    fn verified_instance_for_home(&self, home: &Path) -> Option<String>;
    /// R03（复审 P1）：已核验实例的扫描根枚举——(home, 宿主 pid, instance_id)。
    /// 同 home 多实例 → 明确降级不返回；宿主 pid 由 monitor 侧对 processes 做
    /// 匹配（层级解耦：本 trait 不引 adapter 类型），匹配不到 = 宿主未运行，
    /// 该 home 跳过（无卡）。默认空 = 未装配（测试）不补扫。
    fn attributed_scan_roots(&self) -> Vec<(std::path::PathBuf, u32, String)> {
        Vec::new()
    }
}

/// 实例 home = profile 目录的两级词法上溯（profile = `<home>/profiles/<kind>`）。
pub(crate) fn session_home(profile_dir: &Path) -> PathBuf {
    let text = profile_dir.to_string_lossy();
    let trimmed = text.trim_end_matches(['/', '\\']);
    // 一级上溯到 `<home>/profiles`，再一级到 home。
    match trimmed.rfind(['/', '\\']) {
        Some(idx) => {
            let profiles = &trimmed[..idx];
            match profiles.rfind(['/', '\\']) {
                Some(idx2) => PathBuf::from(&profiles[..idx2]),
                None => PathBuf::from(profiles),
            }
        }
        None => PathBuf::from(trimmed),
    }
}

/// 实例 DSH home 的**装配单源**（Round-8 P1-3）：登记核验 `dsh_home` 优先；
/// 缺失时按 profile 入口权威推导（带 `..` 的逻辑入口沿 `profiles` 祖先上溯取
/// home 并规范化推导值；否则两级词法上溯）。安装管理器装配
/// （`production::init_instance_manager` 的 env.home）与旧钥探测
/// （`switch::dead_identity_in_force` 覆写 env.home 后委托
/// `install::dead_identity_in_force`）**共用本函数**——env.home 是兔维斯侧
/// 推导值，自定义 DSH_HOME 的宿主实例两者不同；两处各推一份 = 双谓词语义
/// 漂移（round-7 教训），凡需要「该实例实际使用的 DSH home」一律经此处。
pub(crate) fn verified_instance_home(instance: &SelectedInstance) -> PathBuf {
    let home = instance.dsh_home.clone().unwrap_or_else(|| {
        if instance
            .profile_dir
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            if let Some(profiles) = instance
                .profile_dir
                .ancestors()
                .find(|part| part.file_name().is_some_and(|name| name == "profiles"))
            {
                if let Some(home) = profiles.parent() {
                    return home.to_path_buf();
                }
            }
        }
        session_home(&instance.profile_dir)
    });
    if home
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        std::fs::canonicalize(&home).unwrap_or(home)
    } else {
        home
    }
}

/// 登记实例 DSH home 的统一判定结果（M06）。
pub(crate) enum RegisteredHomeResolution {
    /// 已证明：登记值自洽，或经可信绑定账本唯一恢复。
    Proven(PathBuf),
    /// 无法证明：禁止管理写入与会话读取，需启动原实例并重新选择。
    /// 不扫描猜测 home、不回退另一实例。
    Unproven(&'static str),
}

/// 原始逻辑 profile 入口（`<home>/profiles/<kind>` 形态，可能带 `..`/符号链接
/// 组件）推导 home：取 `profiles` 祖先前缀的父目录。入口没有 `profiles` 形态
/// （如登记只存了已规范化的外链物理目录）→ `None`——记录本身无法区分直连
/// 与外链，绝不从物理 profile 反推 home。
pub(crate) fn logical_home_of_profile_entry(entry: &Path) -> Option<PathBuf> {
    entry
        .ancestors()
        .skip(1)
        .find(|ancestor| ancestor.file_name().is_some_and(|name| name == "profiles"))
        .and_then(|profiles| profiles.parent())
        .map(|home| home.to_path_buf())
}

/// home 自洽门：绝对路径，且 `<home>/profiles/<kind>` 解析后等于登记物理 profile。
pub(crate) fn proven_home_self_consistent(
    home: &Path,
    kind: HostKind,
    physical_profile: &Path,
) -> bool {
    home.is_absolute()
        && std::fs::canonicalize(home.join("profiles").join(kind.as_str()))
            .is_ok_and(|resolved| resolved == physical_profile)
}

/// 登记实例 DSH home 的统一判定（生产唯一入口；只读，不回写登记）。
///
/// 优先级：
/// ① 登记值 `dsh_home`——通过自洽门即证明；不自洽 = 登记值失效（不修正，
///    报不可证明）；
/// ② 离线可信绑定账本——同实例 Bound 行的公钥摘要与登记指纹连续，且这些行
///    绑定时记录的**原始逻辑 profile 入口**恰好唯一推导出**一个**通过自洽门
///    的 home；
/// ③ 其余（登记缺 home、入口形态不可推导、证据冲突/多义、指纹不连续、账本
///    缺失或畸形、无环境源）→ [`RegisteredHomeResolution::Unproven`]：禁止
///    管理写入与会话读取，需启动原实例并重新选择——不扫描猜测 home，也不
///    回退到另一实例。
pub(crate) fn resolve_registered_home(
    env: Option<&DshEnvironment>,
    instance: &SelectedInstance,
    physical_profile: &Path,
) -> RegisteredHomeResolution {
    if let Some(home) = instance.dsh_home.as_ref() {
        return if proven_home_self_consistent(home, instance.kind, physical_profile) {
            RegisteredHomeResolution::Proven(home.clone())
        } else {
            RegisteredHomeResolution::Unproven(
                "registered dsh home does not resolve to the registered profile",
            )
        };
    }
    let Some(env) = env else {
        return RegisteredHomeResolution::Unproven(
            "no environment source for offline home recovery",
        );
    };
    // Round-8 P1-3：对位摘要双口径——登记锚（当代）与只读证据（退役保留）。
    // 锚定登记继续只认 **Bound 行**（当代绑定在册，M06 原语义：撤权即不可证明，
    // 见 legacy_readonly_rejects_revoked_binding_as_home_evidence）；退役登记
    // （锚被卸载清空、只读证据保留）改对**绑定起源行**（含撤权墓碑）——关闭+
    // 卸载销毁锚与身份目录，销不毁墓碑账本与登记行，只读历史归属不因此断链。
    // 两条口径都要求「摘要对位 + 入口唯一 + 自洽」，硬下界不变。
    let anchored = !instance.identity_fingerprint.is_empty();
    let evidence_digest = if anchored {
        instance.identity_fingerprint.as_str()
    } else {
        instance.read_evidence_fingerprint.as_str()
    };
    if evidence_digest.is_empty() {
        return RegisteredHomeResolution::Unproven(
            "registration has no anchored fingerprint nor preserved read evidence to corroborate binding evidence",
        );
    }
    let rows = if anchored {
        super::auth::bound_rows_for(env, &instance.instance_id)
    } else {
        super::auth::historical_rows_for(env, &instance.instance_id)
    };
    let Some(rows) = rows else {
        return RegisteredHomeResolution::Unproven(
            "binding ledger unavailable for offline home recovery",
        );
    };
    let mut homes: Vec<PathBuf> = Vec::new();
    for (digest, entry) in &rows {
        if digest != evidence_digest {
            continue; // 与登记身份脱钩的行不构成证据
        }
        let Some(home) = logical_home_of_profile_entry(Path::new(entry)) else {
            continue; // 入口只剩物理形态，无法恢复原始 home
        };
        let home = if home
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            // 入口带 `..`：只规范化推导出的 home 本身（不规范化 profile——
            // 外链 profile 的规范化会跳到物理对象，丢失原始 home 形态）。
            std::fs::canonicalize(&home).unwrap_or(home)
        } else {
            home
        };
        if proven_home_self_consistent(&home, instance.kind, physical_profile)
            && !homes.contains(&home)
        {
            homes.push(home);
        }
    }
    if homes.len() != 1 {
        return RegisteredHomeResolution::Unproven(
            "binding evidence cannot uniquely prove one dsh home",
        );
    }
    RegisteredHomeResolution::Proven(homes.remove(0))
}

/// 撤权前保全卸载目标（第九轮 P1-2，关闭流程专用回填）。登记 `dsh_home` 缺失
/// 时，从同实例、同证据（锚定取登记锚 / 退役取只读证据）的**绑定账本 Bound
/// 行**（撤权前的窗口内仍在场；撤权翻墓碑后此面即空）唯一验证逻辑 home——
/// 复用 M06 的「摘要对位 + 入口唯一推导 + home 自洽」硬下界——再经登记写入口
/// （[`InstanceRegistry::register_candidate_with_home`]）的既有物理 profile
/// 对位校验回填登记。任一步不可证 → 具体原因上抛：不猜测、不扫描、不回退
/// 另一实例；调用方（关闭流程）宁可不撤（保持关闭侧可重试）也不写错卸载
/// 目标。回填是登记**证据面的保全**，不放宽任何归属判据——管理写入的 home
/// 证据判定（[`resolve_registered_home`] / 受理门）原样保留。
pub(crate) fn backfill_missing_registered_home(
    env: &DshEnvironment,
    registry: &InstanceRegistry,
    instance_id: &str,
) -> Result<(), DshError> {
    let registered = registry.registered(instance_id)?;
    if registered.dsh_home.is_some() {
        return Ok(());
    }
    let anchored = !registered.identity_fingerprint.is_empty();
    let evidence = if anchored {
        registered.identity_fingerprint.as_str()
    } else {
        registered.read_evidence_fingerprint.as_str()
    };
    if evidence.is_empty() {
        return Err(DshError::new(
            codes::INSTANCE_UNVERIFIED,
            "registration has no anchored fingerprint nor read evidence to preserve the uninstall target; start the original instance and re-select it",
        ));
    }
    let rows = super::auth::bound_rows_for(env, instance_id).ok_or_else(|| {
        DshError::new(
            codes::INSTANCE_UNVERIFIED,
            "binding ledger unavailable; cannot preserve the uninstall target before revocation",
        )
    })?;
    let physical_profile = std::fs::canonicalize(&registered.profile_dir).map_err(|_| {
        DshError::new(
            codes::INSTANCE_UNVERIFIED,
            "registered profile is missing; start the original instance and re-select it",
        )
    })?;
    let mut homes: Vec<PathBuf> = Vec::new();
    for (digest, entry) in &rows {
        if digest != evidence {
            continue; // 与登记身份脱钩的行不构成证据
        }
        let Some(home) = logical_home_of_profile_entry(Path::new(entry)) else {
            continue; // 入口只剩物理形态，无法恢复原始 home
        };
        let home = if home
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            // 入口带 `..`：只规范化推导出的 home 本身（与 resolve_registered_home
            // 同形——不规范化 profile，外链的规范化会跳到物理对象）。
            std::fs::canonicalize(&home).unwrap_or(home)
        } else {
            home
        };
        if proven_home_self_consistent(&home, registered.kind, &physical_profile)
            && !homes.contains(&home)
        {
            homes.push(home);
        }
    }
    if homes.len() != 1 {
        return Err(DshError::new(
            codes::INSTANCE_UNVERIFIED,
            "binding rows cannot uniquely prove one dsh home; start the original instance and re-select it",
        ));
    }
    let home = homes.remove(0);
    registry.register_candidate_with_home(registered.kind, &registered.profile_dir, Some(&home))?;
    let updated = registry.registered(instance_id)?;
    if updated.dsh_home.is_none() {
        return Err(DshError::new(
            codes::STATE_IO,
            "dsh home backfill did not persist to the registration",
        ));
    }
    Ok(())
}

/// 会话目录存在性（`<home>/sessions/<project>/<session_id>`；只经注入文件面做
/// 只读 stat，不打开任何会话内容）。
fn session_dir_exists(env: &DshEnvironment, home: &Path, session_id: &str) -> bool {
    let sessions_root = home.join("sessions");
    env.files
        .list_dir(&sessions_root)
        .iter()
        .any(|project| env.files.exists(&project.join(session_id)))
}

/// 带类型与范围核验的会话目录归属（审查 I3）：必须是**目录**（is_dir），且
/// 规范化后仍在该 home 的 `sessions/` 树内（防符号链接/构造路径越界）。
fn session_dir_owned(env: &DshEnvironment, home: &Path, session_id: &str) -> bool {
    let sessions_root = home.join("sessions");
    let root_normalized = normalize_profile(&sessions_root);
    env.files.list_dir(&sessions_root).iter().any(|project| {
        let candidate_dir = project.join(session_id);
        if !env.files.is_dir(&candidate_dir) {
            return false;
        }
        // 规范化（或词法回退）后必须仍在 sessions 树内。
        normalize_profile(&candidate_dir).starts_with(&root_normalized)
    })
}

/// `session_home` 的契约：profile 恒为 `<home>/profiles/<kind>`，取两级上溯。
///
/// 这是**真实生产函数**的测试（生产调用点：`production.rs:355/380/508`——
/// 其中 508 直接把它赋给 `env.home`，安装/重开都依赖它）。此前这里零覆盖，
/// 而 `production.rs` 里那条「两级上溯」用例其实是对字面量 `Path` 做算术自证、
/// 从不调用本函数——删掉生产代码它照样绿。
#[cfg(test)]
mod session_home_tests {
    use super::session_home;
    use std::path::{Path, PathBuf};

    #[test]
    fn derives_home_two_levels_up() {
        assert_eq!(
            session_home(Path::new("/Users/x/.dsh/profiles/desktop")),
            PathBuf::from("/Users/x/.dsh")
        );
        assert_eq!(
            session_home(Path::new("/home/u/.dsh/profiles/web")),
            PathBuf::from("/home/u/.dsh")
        );
    }

    /// 尾部分隔符必须被吞掉，否则会多上溯一级（得到 `/Users/x`）。
    #[test]
    fn trailing_separators_do_not_shift_the_result() {
        for profile in [
            "/Users/x/.dsh/profiles/desktop/",
            "/Users/x/.dsh/profiles/desktop//",
        ] {
            assert_eq!(
                session_home(Path::new(profile)),
                PathBuf::from("/Users/x/.dsh"),
                "profile={profile}"
            );
        }
    }

    /// Windows 风格反斜杠同样按路径分隔符处理（同一函数两平台共用）。
    #[test]
    fn windows_style_separators_are_handled() {
        assert_eq!(
            session_home(Path::new(r"C:\Users\x\.dsh\profiles\desktop")),
            PathBuf::from(r"C:\Users\x\.dsh")
        );
        assert_eq!(
            session_home(Path::new(r"C:\Users\x\.dsh\profiles\desktop\")),
            PathBuf::from(r"C:\Users\x\.dsh")
        );
    }

    /// 层级不足时保守返回能取到的部分，不 panic、不猜。
    #[test]
    fn shallow_paths_degrade_without_panicking() {
        assert_eq!(session_home(Path::new("desktop")), PathBuf::from("desktop"));
        assert_eq!(
            session_home(Path::new("profiles/desktop")),
            PathBuf::from("profiles")
        );
    }
}

/// P1-2 复审 Minor-2：身份锚退役是**幂等收尾面**——已空（候选未锚定）与
/// **未登记**（实例在作业中途被注销）都必须按「无可清」成功返回，不得让
/// 卸载/兼容清理的重试卡死在锚清空失败上。
#[cfg(test)]
mod retire_identity_fingerprint_tests {
    use super::*;

    #[test]
    fn retire_is_idempotent_for_empty_anchor_and_unregistered_instance() {
        let temp = tempfile::tempdir().unwrap();
        let registry = InstanceRegistry::open(temp.path().join("instances.json")).unwrap();
        let profile = temp.path().join("profiles/desktop");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(profile.join("package.json"), b"{}").unwrap();
        let instance = registry
            .register_candidate(HostKind::Desktop, &profile)
            .unwrap();
        // 空锚（候选未锚定）：幂等成功。
        registry
            .retire_identity_fingerprint(&instance.instance_id)
            .unwrap();
        // 未登记实例：无可清 = 成功（不是失败源——重试必须能继续推进）。
        registry
            .retire_identity_fingerprint("ffffffffffffffffffffffffffffffff")
            .unwrap();
    }

    #[test]
    fn retire_clears_anchored_fingerprint_and_keeps_record() {
        let temp = tempfile::tempdir().unwrap();
        let registry = InstanceRegistry::open(temp.path().join("instances.json")).unwrap();
        let profile = temp.path().join("profiles/desktop");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(profile.join("package.json"), b"{}").unwrap();
        let instance = registry
            .register(HostKind::Desktop, &profile, "fp-old")
            .unwrap();
        registry
            .retire_identity_fingerprint(&instance.instance_id)
            .unwrap();
        let registered = registry.registered(&instance.instance_id).unwrap();
        assert!(
            registered.identity_fingerprint.is_empty(),
            "退役必须清空登记锚"
        );
        assert_eq!(registered.instance_id, instance.instance_id, "记录保留");
    }
}

#[cfg(test)]
mod readonly_header_tests {
    use super::*;
    use crate::monitor::dsh::log::MAX_READONLY_HEADER_BYTES;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn large_unrelated_log_header_is_cached_and_stat_change_invalidates() {
        struct Trusted;
        impl InstanceIdentityProbe for Trusted {
            fn current_fingerprint(
                &self,
                i: &SelectedInstance,
            ) -> Result<Option<String>, DshError> {
                Ok(Some(i.identity_fingerprint.clone()))
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join("profiles/desktop");
        let target = temp.path().join("sessions/project/target");
        let unrelated = temp.path().join("sessions/project/unrelated");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(&unrelated).unwrap();
        let registry = InstanceRegistry::open(temp.path().join("registry.json"))
            .unwrap()
            .with_identity_probe(Arc::new(Trusted));
        let home = std::fs::canonicalize(temp.path()).unwrap();
        registry
            .register_candidate_with_home(HostKind::Desktop, &profile, Some(&home))
            .unwrap();
        let instance = registry
            .register(HostKind::Desktop, &profile, "trusted")
            .unwrap();
        registry.select(&instance.instance_id).unwrap();
        std::fs::write(
            target.join("session.jsonl"),
            b"{\"id\":\"session-target\"}\n",
        )
        .unwrap();
        let path = unrelated.join("session.v3.jsonl.zstd");
        let mut content = b"{\"id\":\"session-unrelated\"}\n".to_vec();
        content.extend(std::iter::repeat_n(b'x', 4 * 1024 * 1024));
        std::fs::write(
            &path,
            zstd::stream::encode_all(content.as_slice(), 1).unwrap(),
        )
        .unwrap();
        let scan = SessionFileScan::new("dsh-readonly-header-cache-test");
        let reads = Arc::new(AtomicUsize::new(0));
        // parse 按值进预算线程（一次一消耗），用工厂从同一 Arc 计数器重建闭包
        let make_parse = |counter: Arc<AtomicUsize>| {
            move |p: &Path| {
                counter.fetch_add(1, Ordering::SeqCst);
                read_readonly_header(p)
            }
        };
        for _ in 0..2 {
            assert!(registry
                .resolve_readonly_root_with(
                    &instance.instance_id,
                    "session-target",
                    &scan,
                    make_parse(Arc::clone(&reads)),
                )
                .is_ok());
        }
        assert_eq!(
            reads.load(Ordering::SeqCst),
            2,
            "两个头各读取一次，第二次请求全命中"
        );
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"x")
            .unwrap();
        assert!(registry
            .resolve_readonly_root_with(
                &instance.instance_id,
                "session-target",
                &scan,
                make_parse(Arc::clone(&reads)),
            )
            .is_ok());
        assert_eq!(reads.load(Ordering::SeqCst), 3, "仅变更文件被重新读取");
        std::fs::write(
            &path,
            zstd::stream::encode_all(b"{\"id\":\"session-target\"}\n".as_slice(), 1).unwrap(),
        )
        .unwrap();
        assert!(
            registry
                .resolve_readonly_root_with(
                    &instance.instance_id,
                    "session-target",
                    &scan,
                    make_parse(Arc::clone(&reads)),
                )
                .is_err(),
            "同一身份出现两次必须拒绝"
        );
    }

    #[test]
    fn readonly_resolve_times_out_with_slow_reader_within_budget() {
        // NEEDS_FIX Important-4：接线测试共用生产容量计数器 → 统一隔离锁串行
        let _walk_isolation = crate::monitor::walk_budget::walk_test_guard();
        struct Trusted;
        impl InstanceIdentityProbe for Trusted {
            fn current_fingerprint(
                &self,
                i: &SelectedInstance,
            ) -> Result<Option<String>, DshError> {
                Ok(Some(i.identity_fingerprint.clone()))
            }
        }
        // 夹具同 large_unrelated 用例：登记 + 选择 + 目标会话文件在场。
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join("profiles/desktop");
        let target = temp.path().join("sessions/project/target");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        let registry = InstanceRegistry::open(temp.path().join("registry.json"))
            .unwrap()
            .with_identity_probe(Arc::new(Trusted));
        let home = std::fs::canonicalize(temp.path()).unwrap();
        registry
            .register_candidate_with_home(HostKind::Desktop, &profile, Some(&home))
            .unwrap();
        let instance = registry
            .register(HostKind::Desktop, &profile, "trusted")
            .unwrap();
        registry.select(&instance.instance_id).unwrap();
        std::fs::write(
            target.join("session.jsonl"),
            b"{\"id\":\"session-target\"}\n",
        )
        .unwrap();
        let scan = SessionFileScan::new("dsh-readonly-budget-test");
        let parse = move |p: &Path| {
            // 慢 reader：读头前挂 2s（真机形态 = readdir/读头在内核楔死数分钟；
            // 测试用 2s 短睡眠复现「超预算」形态，预算覆写 200ms）
            std::thread::sleep(Duration::from_millis(2000));
            read_readonly_header(p)
        };
        // NEEDS_FIX Important-4：预算经**参数注入**（with_budget_for_test）——
        // 全局预算覆写缝取消后，不再影响并行跑的其他只读解析测试。
        let (tx, rx) = std::sync::mpsc::channel();
        let instance_id = instance.instance_id.clone();
        let walker = std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let result = registry.resolve_readonly_root_with_budget_for_test(
                &instance_id,
                "session-target",
                &scan,
                parse,
                Duration::from_millis(200),
            );
            let _ = tx.send((result, started.elapsed()));
        });
        // 有界收货（8s）：旧代码无预算时慢 reader 拖满 2s 才回 Ok—— RED；
        // 新代码预算 200ms 内必须以 DSH_READ_TIMEOUT 拒绝（超时 ≠ 通过，按「不可证明」拒绝）。
        let delivered = rx.recv_timeout(Duration::from_secs(8));
        let (result, elapsed) =
            delivered.expect("只读解析必须按时返回（8s 有界；超时即预算未接线）");
        let err = result.expect_err("慢 reader 超预算必须返回 DSH_READ_TIMEOUT（读取超时降级）");
        assert_eq!(err.code, "DSH_READ_TIMEOUT", "必须走读取超时码");
        assert!(
            elapsed < Duration::from_secs(1),
            "预算 200ms 必须有界返回（实际 {elapsed:?}）"
        );
        walker.join().unwrap();
    }

    #[test]
    fn missing_stat_never_opens_header() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("missing.jsonl");
        let scan = SessionFileScan::new("dsh-readonly-missing-stat-test");
        let reads = AtomicUsize::new(0);
        let value = cached_readonly_header(&scan, &path, |_| {
            reads.fetch_add(1, Ordering::SeqCst);
            Some("untrusted".to_string())
        });
        assert!(value.is_none());
        assert_eq!(reads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn compressed_oversized_header_is_unknown() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.jsonl.zstd");
        let text = format!(
            "{{\"id\":\"{}\"}}\n",
            "x".repeat(MAX_READONLY_HEADER_BYTES as usize)
        );
        std::fs::write(&path, zstd::stream::encode_all(text.as_bytes(), 1).unwrap()).unwrap();
        assert!(read_readonly_header(&path).is_none());
    }
}
