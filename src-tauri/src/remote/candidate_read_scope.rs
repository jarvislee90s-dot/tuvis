//! 候选实例的**授权读取范围**（2026-11-26 用户裁决）。
//!
//! 候选（identifier = `com.jarvis.tuvis.candidate`）只允许读一次明确批准的
//! 数据根 + 会话清单；仅靠改写 `HOME` 或不同 bundle ID 不足以证明读取范围。
//!
//! 生效条件（全部满足才启用；任一不满足一律 fail-closed，不回退通用读源）：
//! - `cfg!(debug_assertions)`（只在候选 debug 构建启用，release 恒不读环境）；
//! - `TUVIS_CANDIDATE_ROOT` 在场（证明由 [`crate::candidate_guard`] 的启动脚本拉起）；
//! - `TUVIS_CANDIDATE_READ_ROOT`：批准的绝对数据根（通常 = 候选根）；
//! - `TUVIS_CANDIDATE_READ_SESSIONS`：逗号分隔的会话 ID 白名单。
//!
//! 出口只有三个纯判定：
//! - [`scope_home`]：读链使用的 home（scope 生效时恒为批准的根）；
//! - [`scope_allows_session`]：agent/session 是否在批准范围内；
//! - [`scope_filters_sessions`]：扫描快照在 scope 生效时按白名单过滤。

static IS_CANDIDATE_BUILD: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 由启动守卫在确认 `CANDIDATE_IDENTIFIER` 后调用，使策略判定与启动守卫共用同一候选身份。
pub fn mark_candidate_build() {
    IS_CANDIDATE_BUILD.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// 与批准会话绑定的已验证项目目录身份（批准时通过受控打开固定 `(dev, ino)`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedDir {
    pub path: std::path::PathBuf,
    pub dev: u64,
    pub ino: u64,
}

/// 候选读取范围（scope 生效时非 None）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateReadScope {
    /// 批准的数据根（绝对路径，已通过校验）。
    pub root: std::path::PathBuf,
    /// 批准时固定的数据根对象身份 `(dev, ino)`；Unix 下缺失或失效一律 fail-closed。
    pub root_identity: Option<(u64, u64)>,
    /// 批准的会话 ID 白名单（非空；大小写敏感精确匹配）。
    pub sessions: Vec<String>,
    /// 与批准会话绑定的已验证项目目录（按 `session_id` 索引；未绑定则不得将 cwd 升格为授权根）。
    pub session_projects: std::collections::BTreeMap<String, VerifiedDir>,
}

fn verify_dir_identity(
    path: &std::path::Path,
) -> Result<(std::path::PathBuf, Option<(u64, u64)>), String> {
    #[cfg(unix)]
    {
        let (canon, dev, ino) = super::files::open_verified_dir_identity(path)?;
        Ok((canon, Some((dev, ino))))
    }
    #[cfg(not(unix))]
    {
        let canon = path.canonicalize().map_err(|e| {
            format!(
                "CANDIDATE_READ_SCOPE_INVALID: 目录不可解析 {}: {e}",
                path.display()
            )
        })?;
        if !canon.is_dir() {
            return Err(format!(
                "CANDIDATE_READ_SCOPE_INVALID: 路径不是目录: {}",
                canon.display()
            ));
        }
        Ok((canon, None))
    }
}

impl CandidateReadScope {
    /// 构造并固定批准根的目录对象身份。
    pub fn new(root: std::path::PathBuf, sessions: Vec<String>) -> Result<Self, String> {
        let (canon, root_identity) = verify_dir_identity(&root)?;
        Ok(Self {
            root: canon,
            root_identity,
            sessions,
            session_projects: std::collections::BTreeMap::new(),
        })
    }

    /// 为已批准会话绑定一个已验证的项目目录句柄身份。
    pub fn with_session_project(
        mut self,
        session_id: &str,
        project: &std::path::Path,
    ) -> Result<Self, String> {
        let (canon, identity) = verify_dir_identity(project)?;
        let (dev, ino) = identity.ok_or_else(|| {
            "CANDIDATE_READ_SCOPE_INVALID: 非 Unix 平台不支持目录对象身份绑定".to_string()
        })?;
        self.session_projects.insert(
            session_id.to_string(),
            VerifiedDir {
                path: canon,
                dev,
                ino,
            },
        );
        Ok(self)
    }
}

fn parse_sessions(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// 从一组显式值解析范围（纯判定内核，测试可注入；不读进程环境）。
/// `candidate_root` = `TUVIS_CANDIDATE_ROOT`，`read_root` = `TUVIS_CANDIDATE_READ_ROOT`，
/// `read_sessions` = `TUVIS_CANDIDATE_READ_SESSIONS`。
pub fn scope_from_values(
    candidate_confirmed: bool,
    candidate_root: Option<&str>,
    read_root: Option<&str>,
    read_sessions: Option<&str>,
) -> Result<Option<CandidateReadScope>, String> {
    scope_from_values_with_projects(
        candidate_confirmed,
        candidate_root,
        read_root,
        read_sessions,
        None,
    )
}

/// 带可选 `TUVIS_CANDIDATE_READ_PROJECTS`（JSON 对象：`session_id -> absolute_project_path`）
/// 的范围解析内核。
pub fn scope_from_values_with_projects(
    candidate_confirmed: bool,
    candidate_root: Option<&str>,
    read_root: Option<&str>,
    read_sessions: Option<&str>,
    read_projects: Option<&str>,
) -> Result<Option<CandidateReadScope>, String> {
    let nonempty = |v: Option<&str>| {
        v.map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    if !candidate_confirmed {
        return Ok(None);
    }
    if nonempty(candidate_root).is_none() {
        return Err(
            "CANDIDATE_READ_SCOPE_MISSING: 候选启动未声明 TUVIS_CANDIDATE_ROOT".to_string(),
        );
    }
    let read_root = nonempty(read_root).ok_or_else(|| {
        "CANDIDATE_READ_SCOPE_MISSING: 候选启动未声明 TUVIS_CANDIDATE_READ_ROOT".to_string()
    })?;
    let root = std::path::Path::new(&read_root);
    if !root.is_absolute() {
        return Err(format!(
            "CANDIDATE_READ_SCOPE_INVALID: 批准数据根不是绝对路径: {read_root}"
        ));
    }
    if root
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(format!(
            "CANDIDATE_READ_SCOPE_INVALID: 批准数据根含 \"..\": {read_root}"
        ));
    }
    let (root, root_identity) =
        verify_dir_identity(root).map_err(|e| format!("CANDIDATE_READ_SCOPE_INVALID: {e}"))?;
    // 候选根必须与批准根一致（同一 root 语义，防脚本误配第二处）
    if let Some(candidate_root) = nonempty(candidate_root) {
        let Ok(candidate_canon) = std::path::Path::new(&candidate_root).canonicalize() else {
            return Err(format!(
                "CANDIDATE_READ_SCOPE_INVALID: 候选根不可解析: {candidate_root}"
            ));
        };
        if candidate_canon != root {
            return Err(format!(
                "CANDIDATE_READ_SCOPE_INVALID: 批准数据根 {} 与候选根 {} 不一致",
                root.display(),
                candidate_canon.display()
            ));
        }
    }
    let sessions = parse_sessions(&nonempty(read_sessions).ok_or_else(|| {
        "CANDIDATE_READ_SCOPE_MISSING: 候选启动未声明 TUVIS_CANDIDATE_READ_SESSIONS".to_string()
    })?);
    if sessions.is_empty() {
        return Err("CANDIDATE_READ_SCOPE_INVALID: 会话白名单为空（不读任何会话）".to_string());
    }
    let mut session_projects = std::collections::BTreeMap::new();
    if let Some(raw_projects) = nonempty(read_projects) {
        let parsed: serde_json::Value = serde_json::from_str(&raw_projects).map_err(|_| {
            "CANDIDATE_READ_SCOPE_INVALID: TUVIS_CANDIDATE_READ_PROJECTS 不是合法 JSON".to_string()
        })?;
        let Some(map) = parsed.as_object() else {
            return Err(
                "CANDIDATE_READ_SCOPE_INVALID: TUVIS_CANDIDATE_READ_PROJECTS 必须是 JSON 对象"
                    .to_string(),
            );
        };
        for (sid, val) in map {
            let sid = sid.trim();
            if sid.is_empty() || !sessions.iter().any(|s| s == sid) {
                return Err(format!(
                    "CANDIDATE_READ_SCOPE_INVALID: 项目映射包含未批准会话: {sid}"
                ));
            }
            let Some(raw_path) = val.as_str().map(str::trim).filter(|s| !s.is_empty()) else {
                return Err(format!(
                    "CANDIDATE_READ_SCOPE_INVALID: 会话 {sid} 的批准项目路径必须为非空字符串"
                ));
            };
            let proj_path = std::path::Path::new(raw_path);
            if !proj_path.is_absolute()
                || proj_path
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err(format!(
                    "CANDIDATE_READ_SCOPE_INVALID: 会话 {sid} 的批准项目路径必须是无 .. 的绝对路径"
                ));
            }
            let (canon, identity) = verify_dir_identity(proj_path).map_err(|_| {
                format!("CANDIDATE_READ_SCOPE_INVALID: 会话 {sid} 的批准项目目录不可验证")
            })?;
            let (dev, ino) = identity.ok_or_else(|| {
                "CANDIDATE_READ_SCOPE_INVALID: 非 Unix 平台不支持目录对象身份绑定".to_string()
            })?;
            session_projects.insert(
                sid.to_string(),
                VerifiedDir {
                    path: canon,
                    dev,
                    ino,
                },
            );
        }
    }
    Ok(Some(CandidateReadScope {
        root,
        root_identity,
        sessions,
        session_projects,
    }))
}

/// 按可注入环境变量源解析范围（供生产薄壳与真实环境变量测试共用）。
pub fn scope_from_env_with<F>(
    candidate_confirmed: bool,
    get_var: F,
) -> Result<Option<CandidateReadScope>, String>
where
    F: Fn(&str) -> Option<String>,
{
    scope_from_values_with_projects(
        candidate_confirmed,
        get_var("TUVIS_CANDIDATE_ROOT").as_deref(),
        get_var("TUVIS_CANDIDATE_READ_ROOT").as_deref(),
        get_var("TUVIS_CANDIDATE_READ_SESSIONS").as_deref(),
        get_var("TUVIS_CANDIDATE_READ_PROJECTS").as_deref(),
    )
}

/// 从进程环境解析范围（生产薄壳）：只认启动守卫 `mark_candidate_build` 确认的候选身份，
/// 非候选 debug 构建即使继承环境变量也恒返回 `Ok(None)`（`Absent`）。
pub fn scope_from_process_env() -> Result<Option<CandidateReadScope>, String> {
    let is_candidate = IS_CANDIDATE_BUILD.load(std::sync::atomic::Ordering::SeqCst);
    scope_from_env_with(is_candidate, |k| std::env::var(k).ok())
}

impl CandidateReadScope {
    /// 该 agent/session 是否在批准范围内。非 codex 一律不在范围内（本批只批准
    /// Codex Desktop 原线程的只读观察）。
    pub fn allows(&self, agent_type: &str, session_id: &str) -> bool {
        agent_type == "codex" && self.sessions.iter().any(|s| s == session_id)
    }

    /// 读链 home：恒为批准的根（绝不回落到真实 home）。
    pub fn home_string(&self) -> Option<String> {
        self.root.to_str().map(str::to_string)
    }
}

/// 生产读链的三态策略（`OnceLock`：进程内只解析一次，运行期不变）：
/// - `Active`：候选且配置合法——只允许批准根 + 批准会话；
/// - `Absent`：非候选（日用/其它构建）——原行为；
/// - `Invalid`：候选但配置非法——**读链全拒**（含快照一律过滤为空），
///   绝不等同于 `Absent`。
#[derive(Debug, Clone)]
pub enum CandidatePolicy {
    Active(CandidateReadScope),
    Absent,
    Invalid(String),
}

pub fn policy() -> &'static CandidatePolicy {
    static POLICY: std::sync::OnceLock<CandidatePolicy> = std::sync::OnceLock::new();
    POLICY.get_or_init(|| match scope_from_process_env() {
        Ok(Some(scope)) => CandidatePolicy::Active(scope),
        Ok(None) => CandidatePolicy::Absent,
        Err(error) => {
            log::error!("候选读取范围配置非法，读链按全拒策略运行: {error}");
            CandidatePolicy::Invalid(error)
        }
    })
}

/// 候选策略生效中（Active 或 Invalid）——快照/明细等读面据此收紧。
pub fn enforced() -> bool {
    !matches!(policy(), CandidatePolicy::Absent)
}

/// 生产读链的 scope 快照（仅 Active 返回 Some；Invalid 恒 None 且由
/// [`scope_config_error`] 让调用方显式拒绝）。
pub fn active_scope() -> Option<&'static CandidateReadScope> {
    match policy() {
        CandidatePolicy::Active(scope) => Some(scope),
        _ => None,
    }
}

/// 配置是否非法（用于让读链显式拒绝，而不是当作「无 scope」放行）。
pub fn scope_config_error() -> Option<String> {
    match policy() {
        CandidatePolicy::Invalid(error) => Some(error.clone()),
        _ => None,
    }
}

/// 仅供启动日志门控消费：只检查已固定的候选策略引用，日用分支传入 `None` 绝不触发 `policy()`。
pub fn fixed_invalid_error(candidate_policy: Option<&CandidatePolicy>) -> Option<&str> {
    match candidate_policy {
        Some(CandidatePolicy::Invalid(error)) => Some(error.as_str()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approved_root() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn non_candidate_build_never_scopes() {
        let dir = approved_root();
        let root = dir.path().to_str().unwrap();
        assert_eq!(
            scope_from_values(false, Some(root), Some(root), Some("s-1")),
            Ok(None),
            "release/日用构建即使环境在场也不启用 scope"
        );
    }

    #[test]
    fn candidate_without_read_root_or_sessions_fails_closed() {
        let dir = approved_root();
        let root = dir.path().to_str().unwrap();
        let err = scope_from_values(true, Some(root), None, Some("s-1")).unwrap_err();
        assert!(err.contains("CANDIDATE_READ_SCOPE_MISSING"), "{err}");
        let err = scope_from_values(true, Some(root), Some(root), None).unwrap_err();
        assert!(err.contains("CANDIDATE_READ_SCOPE_MISSING"), "{err}");
        let err = scope_from_values(true, Some(root), Some(root), Some(" , ")).unwrap_err();
        assert!(err.contains("CANDIDATE_READ_SCOPE_INVALID"), "{err}");
    }

    #[test]
    fn candidate_read_root_must_equal_candidate_root() {
        let a = approved_root();
        let b = approved_root();
        let err = scope_from_values(
            true,
            Some(a.path().to_str().unwrap()),
            Some(b.path().to_str().unwrap()),
            Some("s-1"),
        )
        .unwrap_err();
        assert!(err.contains("不一致"), "{err}");
    }

    #[test]
    fn relative_or_parent_root_rejected() {
        let dir = approved_root();
        let root = dir.path().to_str().unwrap();
        let err =
            scope_from_values(true, Some(root), Some("relative/path"), Some("s-1")).unwrap_err();
        assert!(err.contains("不是绝对路径"), "{err}");
        let dotted = format!("{root}/../x");
        let err = scope_from_values(true, Some(root), Some(&dotted), Some("s-1")).unwrap_err();
        assert!(err.contains("\"..\""), "{err}");
    }

    #[test]
    fn scope_allows_only_listed_codex_session() {
        let dir = approved_root();
        let root = dir.path().to_str().unwrap();
        let scope = scope_from_values(true, Some(root), Some(root), Some("s-a, s-b,s-a"))
            .unwrap()
            .expect("scope enabled");
        assert_eq!(scope.sessions, vec!["s-a".to_string(), "s-b".to_string()]);
        assert!(scope.allows("codex", "s-a"));
        assert!(scope.allows("codex", "s-b"));
        assert!(!scope.allows("codex", "s-c"), "未列入白名单必须拒绝");
        assert!(!scope.allows("claude", "s-a"), "非 codex 一律不在本批范围");
        // home 必须是 canonical 后的批准根（/private 前缀由 canonicalize 归一）
        assert_eq!(
            scope.home_string(),
            Some(scope.root.to_str().unwrap().to_string())
        );
    }

    #[test]
    fn non_candidate_debug_ignores_inherited_candidate_env() {
        let root_dir = approved_root();
        let proj_dir = approved_root();
        let root = root_dir.path().canonicalize().unwrap();
        let proj = proj_dir.path().canonicalize().unwrap();
        let projects_json = serde_json::json!({ "s-1": proj }).to_string();
        let env = |key: &str| -> Option<String> {
            match key {
                "TUVIS_CANDIDATE_ROOT" | "TUVIS_CANDIDATE_READ_ROOT" => {
                    Some(root.to_str().unwrap().to_string())
                }
                "TUVIS_CANDIDATE_READ_SESSIONS" => Some("s-1".to_string()),
                "TUVIS_CANDIDATE_READ_PROJECTS" => Some(projects_json.clone()),
                _ => None,
            }
        };
        // 非候选构建（未确认身份）即使完整继承 TUVIS_CANDIDATE_* 也必须返回 Absent (None)
        assert_eq!(scope_from_env_with(false, env), Ok(None));
        // 已确认候选身份且同一环境在场 → Active
        let active = scope_from_env_with(true, env)
            .unwrap()
            .expect("confirmed candidate must be Active");
        assert!(active.allows("codex", "s-1"));
        assert!(active.session_projects.contains_key("s-1"));
        // 已确认候选但缺失 TUVIS_CANDIDATE_* → Invalid (Err)
        let err = scope_from_env_with(true, |_| None).unwrap_err();
        assert!(err.contains("CANDIDATE_READ_SCOPE_MISSING"), "{err}");
    }

    #[test]
    fn daily_startup_gate_never_initializes_candidate_policy() {
        assert_eq!(fixed_invalid_error(None), None);
        let absent = CandidatePolicy::Absent;
        assert_eq!(fixed_invalid_error(Some(&absent)), None);
        let invalid = CandidatePolicy::Invalid("CANDIDATE_READ_SCOPE_INVALID: test".into());
        assert_eq!(
            fixed_invalid_error(Some(&invalid)),
            Some("CANDIDATE_READ_SCOPE_INVALID: test")
        );
    }

    #[test]
    fn scope_from_values_with_projects_binds_and_validates_projects() {
        let root_dir = approved_root();
        let proj_dir = approved_root();
        let root = root_dir.path().to_str().unwrap();
        let proj = proj_dir.path().canonicalize().unwrap();
        let projects_json = serde_json::json!({ "s-a": proj }).to_string();
        let scope = scope_from_values_with_projects(
            true,
            Some(root),
            Some(root),
            Some("s-a"),
            Some(&projects_json),
        )
        .unwrap()
        .expect("scope enabled");
        let bound = scope.session_projects.get("s-a").expect("bound project");
        assert_eq!(bound.path, proj);

        // 未批准会话、相对路径、含 .. 均必须 fail-closed
        let bad_sid = serde_json::json!({ "s-unapproved": proj }).to_string();
        assert!(scope_from_values_with_projects(
            true,
            Some(root),
            Some(root),
            Some("s-a"),
            Some(&bad_sid)
        )
        .unwrap_err()
        .contains("未批准会话"));
        let bad_rel = serde_json::json!({ "s-a": "relative/proj" }).to_string();
        assert!(scope_from_values_with_projects(
            true,
            Some(root),
            Some(root),
            Some("s-a"),
            Some(&bad_rel)
        )
        .unwrap_err()
        .contains("绝对路径"));
        assert!(scope_from_values_with_projects(
            true,
            Some(root),
            Some(root),
            Some("s-a"),
            Some("not-json")
        )
        .unwrap_err()
        .contains("合法 JSON"));
    }
}
