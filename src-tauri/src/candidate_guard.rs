//! 候选构建的隔离启动守卫（2026-11-13 用户裁决）。
//!
//! 只对 identifier = [`CANDIDATE_IDENTIFIER`] 的构建生效：`run()` 取得 Tauri
//! Context 后、**任何日志/数据库/资源/Hook/桥接/远程恢复之前**调用
//! [`verify_candidate_startup`]，任一条件不满足即输出 [`EXIT_MESSAGE`] 并立即
//! 退出——防止候选从程序坞直开落到真实 HOME 写真实 `~/.tuvis`（两轮冒烟实录）。
//!
//! 隔离启动条件由 `scripts/run-candidate.sh` 注入：
//! - `TUVIS_CANDIDATE_ROOT`：本次启动的数据根（脚本已做 realpath 与归属标记）；
//! - `TUVIS_CANDIDATE_RUN_ID`：本次运行标识（tmux socket / 运行记录关联）；
//! - `TUVIS_CANDIDATE_APP`：候选 .app 真实路径（核对 current_exe 确在包内）。
//!
//! 已知边界（如实声明）：归一化用 `canonicalize`，要求路径已存在；不存在的路径
//! 按字面前缀与组件检查（不解析链接）。这是启动门禁的最小充分集，不替代
//! `run-candidate.sh` 的体检。

/// 候选构建的 bundle identifier（与 scripts/tauri.candidate.conf.json 同源）。
pub const CANDIDATE_IDENTIFIER: &str = "com.jarvis.tuvis.candidate";

/// 非脚本启动被拒时向用户展示的消息（stderr + macOS 弹窗尽力呈现）。
pub const EXIT_MESSAGE: &str = "请通过候选启动脚本打开";

/// 隔离启动所需的进程环境（全部可缺省——缺省即验证失败）。
pub struct CandidateStartupEnv {
    pub data_root: Option<String>,
    pub run_id: Option<String>,
    pub app_bundle: Option<String>,
    pub home: Option<std::path::PathBuf>,
    pub xdg_config: Option<std::path::PathBuf>,
    pub xdg_data: Option<std::path::PathBuf>,
    pub xdg_state: Option<std::path::PathBuf>,
}

impl CandidateStartupEnv {
    /// 从进程环境收集（release 数据目录由 HOME 锚定，TUVIS_HOME 单独设不足以
    /// 改变 release 的数据目录——故以 HOME 与 XDG 三件套为准）。
    pub fn from_process_env() -> Self {
        Self {
            data_root: std::env::var("TUVIS_CANDIDATE_ROOT").ok(),
            run_id: std::env::var("TUVIS_CANDIDATE_RUN_ID").ok(),
            app_bundle: std::env::var("TUVIS_CANDIDATE_APP").ok(),
            home: std::env::var_os("HOME").map(std::path::PathBuf::from),
            xdg_config: std::env::var_os("XDG_CONFIG_HOME").map(std::path::PathBuf::from),
            xdg_data: std::env::var_os("XDG_DATA_HOME").map(std::path::PathBuf::from),
            xdg_state: std::env::var_os("XDG_STATE_HOME").map(std::path::PathBuf::from),
        }
    }
}

fn fail(reason: &str) -> Result<(), String> {
    Err(format!("{EXIT_MESSAGE}（{reason}）"))
}

fn canonicalize_existing(path: &std::path::Path) -> Option<std::path::PathBuf> {
    std::fs::canonicalize(path).ok()
}

/// 路径是否落在 `root`（canonicalize 后）之内或与 root 相等。
/// 不存在的路径：按字面归一（去 `.`/`..`）后与前缀比较——不解析链接，
/// 启动门禁按「启动前未创建的路径只能是脚本约定形状」处理。
fn within_root(root: &std::path::Path, path: &std::path::Path) -> bool {
    let root_canon = canonicalize_existing(root);
    let path_canon = canonicalize_existing(path);
    match (root_canon, path_canon) {
        (Some(r), Some(p)) => p.starts_with(&r),
        _ => {
            // 至少一侧不存在：字面归一比较
            let normalize = |p: &std::path::Path| -> std::path::PathBuf {
                let mut out = std::path::PathBuf::new();
                for comp in p.components() {
                    match comp {
                        std::path::Component::CurDir => {}
                        std::path::Component::ParentDir => {
                            out.pop();
                        }
                        other => out.push(other.as_os_str()),
                    }
                }
                out
            };
            normalize(path).starts_with(normalize(root))
        }
    }
}

fn is_safe_run_id(run_id: &str) -> bool {
    !run_id.is_empty()
        && run_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn reject_symlink(path: &std::path::Path, what: &str) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                fail(&format!(
                    "{what} 是符号链接，疑似指向真实目录: {}",
                    path.display()
                ))
            } else {
                Ok(())
            }
        }
        // 不存在 = 本次启动尚未创建，符合启动前形状
        Err(_) => Ok(()),
    }
}

/// 磁盘核验：候选根内的关键对象不得经符号链接指向真实目录；归属标记必须在。
fn verify_paths_on_disk(root: &std::path::Path) -> Result<(), String> {
    let marker = root.join(".tuvis-candidate-owner");
    match std::fs::symlink_metadata(&marker) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => fail("归属标记不是普通文件")?,
        Err(_) => fail("缺少候选归属标记（数据根不是由候选启动脚本创建）")?,
    }
    reject_symlink(&root.join(".tuvis"), "候选 .tuvis 目录")?;
    let db = root.join(".tuvis").join("tuvis.db");
    if let Ok(meta) = std::fs::symlink_metadata(&db) {
        if meta.file_type().is_symlink() {
            fail(&format!("候选数据库是符号链接: {}", db.display()))?;
        }
        if !meta.is_file() {
            fail(&format!("候选数据库不是普通文件: {}", db.display()))?;
        }
    }
    reject_symlink(&root.join(".config"), "候选 .config 目录")?;
    reject_symlink(&root.join(".config").join("opencode"), "OpenCode 配置目录")?;
    Ok(())
}

/// 候选隔离启动核验（见模块文档）。任何失败返回含 [`EXIT_MESSAGE`] 的错误。
pub fn verify_candidate_startup(
    env: &CandidateStartupEnv,
    current_exe: Option<std::path::PathBuf>,
) -> Result<(), String> {
    // ① 数据根：存在、绝对、无 ".."、真实存在
    let Some(data_root) = env.data_root.as_deref() else {
        return fail("缺少 TUVIS_CANDIDATE_ROOT（未经理赔脚本启动）");
    };
    let root = std::path::Path::new(data_root);
    if !root.is_absolute() {
        return fail("数据根不是绝对路径");
    }
    for comp in root.components() {
        if matches!(comp, std::path::Component::ParentDir) {
            return fail("数据根路径含 \"..\" 组件");
        }
    }
    if !root.is_dir() {
        return fail("数据根不存在或不是目录");
    }

    // ② 本次运行标识
    let Some(run_id) = env.run_id.as_deref() else {
        return fail("缺少 TUVIS_CANDIDATE_RUN_ID");
    };
    if !is_safe_run_id(run_id) {
        return fail("运行标识为空或含非法字符");
    }

    // ③ 候选 .app：current_exe 必须真实落在包内（release 实际路径核对）
    let Some(app_bundle) = env.app_bundle.as_deref() else {
        return fail("缺少 TUVIS_CANDIDATE_APP");
    };
    let bundle = std::path::Path::new(app_bundle);
    if !bundle.is_absolute() {
        return fail("候选 .app 路径不是绝对路径");
    }
    let Some(exe) = current_exe else {
        return fail("无法取得当前可执行文件路径");
    };
    let Some(exe_canon) = canonicalize_existing(&exe) else {
        return fail("当前可执行文件路径无法解析");
    };
    let Some(bundle_canon) = canonicalize_existing(bundle) else {
        return fail("候选 .app 路径无法解析");
    };
    if !exe_canon.starts_with(&bundle_canon) {
        return fail(&format!(
            "当前可执行文件 {} 不在候选包 {} 内",
            exe_canon.display(),
            bundle_canon.display()
        ));
    }

    // ④ HOME 必须就是数据根（release 数据目录由 HOME 锚定）
    let Some(home) = env.home.as_ref() else {
        return fail("无法取得 HOME");
    };
    let Some(home_canon) = canonicalize_existing(home) else {
        return fail("HOME 无法解析");
    };
    let Some(root_canon) = canonicalize_existing(root) else {
        return fail("数据根无法解析");
    };
    if home_canon != root_canon {
        return fail(&format!(
            "HOME ({}) 不在隔离数据根 ({}) 内",
            home_canon.display(),
            root_canon.display()
        ));
    }

    // ⑤ OpenCode 配置/数据/状态路径必须都落在数据根内
    for (what, path) in [
        ("XDG_CONFIG_HOME", env.xdg_config.as_ref()),
        ("XDG_DATA_HOME", env.xdg_data.as_ref()),
        ("XDG_STATE_HOME", env.xdg_state.as_ref()),
    ] {
        if let Some(path) = path {
            if !within_root(root, path) {
                return fail(&format!("{what} ({}) 不在隔离数据根内", path.display()));
            }
        }
        // None：回落 home 下标准位置（home == root，等价满足）
    }

    // ⑥ 磁盘核验：无符号链接逃逸 + 归属标记
    verify_paths_on_disk(root)?;

    Ok(())
}

/// 守卫失败的标准出口：stderr + macOS 弹窗尽力呈现 + 立即退出。
/// 弹窗用 `osascript`（无新增依赖；headless/无 osascript 时静默降级为 stderr）。
pub fn exit_as_candidate(error: &str) -> ! {
    eprintln!("{error}");
    let script = format!(
        "display dialog \"{}\\n\\n{}\" with title \"Tuvis 候选\" buttons {{\"好\"}} default button 1",
        EXIT_MESSAGE,
        error.replace(['"', '\\'], "")
    );
    let _ = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    std::process::exit(1);
}
