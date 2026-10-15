// LinkerService — symlink (Unix) / Junction (Windows) 管理
// 移植自 skills-manager-jw linker.rs，简化为 MVP 所需功能

use log::debug;
pub mod detector;
pub mod layer2;
pub mod layer3;
use fs2::FileExt;
use std::fs;
use std::path::{Path, PathBuf};

/// 链接健康状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkHealth {
    /// 链接存在且目标可达
    Valid,
    /// 链接存在但目标不可达（SSOT 被删/移动）
    Dangling,
    /// 路径存在但不是链接（原生目录/文件）
    NotLink,
    /// 路径不存在
    Missing,
}

/// 判定链接健康状态
pub fn check_link_health(target: &Path) -> LinkHealth {
    if !target.exists() && !target.is_symlink() {
        return LinkHealth::Missing;
    }
    if !target.is_symlink() {
        return LinkHealth::NotLink;
    }
    if fs::metadata(target).is_ok() {
        LinkHealth::Valid
    } else {
        LinkHealth::Dangling
    }
}

/// 确保全局仓库目录存在，返回路径
pub fn ensure_repo_dir() -> PathBuf {
    let repo = dirs::home_dir()
        .unwrap_or_default()
        .join(".tuvis")
        .join("skills");
    let _ = fs::create_dir_all(&repo);
    repo
}

/// 获取全局仓库中所有 skill 的名称
pub fn list_repo_skills() -> Vec<String> {
    let repo = ensure_repo_dir();
    let mut skills = Vec::new();
    if let Ok(entries) = fs::read_dir(&repo) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    skills.push(name.to_string());
                }
            }
        }
    }
    skills.sort();
    skills
}

/// 判定路径是否为链接（symlink / Windows junction）。
/// Windows 上 junction 的 is_symlink() 恒为 true（reparse point 被视作 symlink），
/// 故直接以 is_symlink() 为判据即可双平台通用
pub fn link_marker_is_present(target: &Path) -> bool {
    target.is_symlink()
}

/// 派发拍平（用户裁决 2026-09-17）：SSOT 仓库保持套件层级原样，但派发到
/// 工具的一切磁盘链接名（Layer2 / Layer3 / 工具 skill 目录）一律把嵌套
/// 规范名里的 `/` 替换为 `-`（如 superpowers/brainstorming →
/// superpowers-brainstorming）——部分工具不递归扫描技能目录，嵌套派发整组
/// 不可见。账本身份（extension_id / DB 行 / 快照项）保持嵌套规范名不变，
/// 仅磁盘链接名拍平；对账扫描比较时账本侧正向映射到此名（磁盘侧不做反解）。
/// 平铺名（无 `/`）恒等映射
pub fn dispatch_name(skill_name: &str) -> String {
    skill_name.replace('/', "-")
}

/// 派发拍平目标路径：base.join(dispatch_name(skill_name))——Layer2 / Layer3 /
/// 工具 skill 目录三处派发目标的统一构造器（source 侧仍是 repo.join(嵌套名)）
pub fn dispatch_target(base: &Path, skill_name: &str) -> PathBuf {
    base.join(dispatch_name(skill_name))
}

/// 创建链接：source（全局仓库）→ target（工具 skill 目录）
pub fn create_link(source: &Path, target: &Path) -> Result<(), String> {
    // 如果目标已存在，先移除
    if target.exists() || target.is_symlink() {
        // 安全保护：若目标经父级 symlink 穿透到 SSOT 仓库，说明已被套件链接接管，
        // 此时不应删除真实 SSOT 目录，直接返回即可。
        if !target.is_symlink() && source.canonicalize().ok() == target.canonicalize().ok() {
            return Ok(());
        }
        remove_link(target)?;
    }

    // 确保父目录存在
    if let Some(parent) = target.parent() {
        let _ = fs::create_dir_all(parent);
    }

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(source, target)
            .map_err(|e| format!("LINKER_SYMLINK_FAIL: 创建 symlink 失败: {}", e))?;
    }

    #[cfg(windows)]
    {
        // Windows: 目录用 Junction（纯 API 调用，不走 cmd，避免闪控制台窗口），文件用 copy
        if source.is_dir() {
            junction::create(source, target)
                .map_err(|e| format!("LINKER_JUNCTION_FAIL: 创建 Junction 失败: {}", e))?;
        } else {
            fs::copy(source, target)
                .map_err(|e| format!("LINKER_COPY_FAIL: 复制文件失败: {}", e))?;
        }
    }

    debug!("Link created: {:?} -> {:?}", target, source);
    Ok(())
}

/// 移除链接
pub fn remove_link(target: &Path) -> Result<(), String> {
    if target.is_symlink() {
        // 判断链接是否为目录型。关键点：不跟随链接的 symlink_metadata 在 Windows 上对
        // 目录 Junction 的 FileType::is_dir() 恒为 false（reparse point 被视作 symlink），
        // 须用 FileTypeExt::is_symlink_dir() 判定（对 dangling junction 同样返回 true）；
        // 目录型链接用 remove_file 会报"拒绝访问"（os error 5），须改用 remove_dir_all ——
        // 它对 reparse point 只删除链接本身，不会递归进目标目录。
        #[cfg(windows)]
        let link_is_dir = target
            .symlink_metadata()
            .map(|m| {
                use std::os::windows::fs::FileTypeExt;
                m.file_type().is_symlink_dir()
            })
            .unwrap_or(false);
        // Unix 上 symlink_metadata 的 is_dir() 对 symlink 恒为 false，链接本体用 remove_file 即可
        #[cfg(not(windows))]
        let link_is_dir = target
            .symlink_metadata()
            .map(|m| m.file_type().is_dir())
            .unwrap_or(false);
        if link_is_dir {
            fs::remove_dir_all(target)
                .map_err(|e| format!("LINKER_REMOVE_DIRLINK_FAIL: 移除目录链接失败: {}", e))?;
        } else {
            fs::remove_file(target)
                .map_err(|e| format!("LINKER_REMOVE_SYMLINK_FAIL: 移除 symlink 失败: {}", e))?;
        }
    } else if target.exists() {
        // 普通文件用 remove_file，目录 / Junction 用 remove_dir_all
        if target.is_dir() {
            fs::remove_dir_all(target)
                .map_err(|e| format!("LINKER_REMOVE_DIR_FAIL: 移除目录失败: {}", e))?;
        } else {
            fs::remove_file(target)
                .map_err(|e| format!("LINKER_REMOVE_FILE_FAIL: 移除文件失败: {}", e))?;
        }
    }
    Ok(())
}

/// 将原始目录替换为指向 SSOT 仓库的符号链接
///
/// 安全校验：target 必须存在且不是符号链接（防止重复清理）
/// 操作流程：删除 target 目录 → 创建符号链接 target → source
pub fn replace_with_symlink(source: &Path, target: &Path) -> Result<(), String> {
    if !target.exists() {
        return Err(format!(
            "LINKER_TARGET_MISSING: 目标路径不存在: {}",
            target.display()
        ));
    }
    if target.is_symlink() {
        return Err(format!(
            "LINKER_ALREADY_LINK: 目标已是符号链接，无需清理: {}",
            target.display()
        ));
    }
    if !source.exists() {
        return Err(format!(
            "LINKER_SOURCE_MISSING: 源路径不存在: {}",
            source.display()
        ));
    }

    // 删除原始目录
    if target.is_dir() {
        std::fs::remove_dir_all(target)
            .map_err(|e| format!("LINKER_DELETE_DIR_FAIL: 删除原始目录失败: {}", e))?;
    } else {
        std::fs::remove_file(target)
            .map_err(|e| format!("LINKER_DELETE_FILE_FAIL: 删除原始文件失败: {}", e))?;
    }

    // 创建符号链接
    create_link(source, target)
}

/// 安装 skill 到全局仓库（从源路径复制）
/// 安全检查：验证源路径不在敏感目录内（防止路径穿越）
/// 若目标同名资源已存在：overwrite=false 直接报错，overwrite=true 先按类型清理再复制
pub fn install_to_repo(source: &Path, name: &str, overwrite: bool) -> Result<(), String> {
    // 路径穿越检查：解析源路径，验证不在敏感目录内
    let canonical = source
        .canonicalize()
        .map_err(|e| format!("LINKER_RESOLVE_FAIL: 路径解析失败: {}", e))?;
    let sensitive_paths = [
        ".ssh",
        ".gnupg",
        ".aws",
        ".kube",
        ".netrc",
        ".npmrc",
        ".docker",
        ".config/gcloud",
        ".config/gh",
    ];
    let home = dirs::home_dir().unwrap_or_default();
    for sensitive in &sensitive_paths {
        let sensitive_path = home.join(sensitive);
        if canonical.starts_with(&sensitive_path) {
            return Err(format!(
                "LINKER_SENSITIVE_PATH: 安全检查失败：源路径在敏感目录内: {}",
                sensitive
            ));
        }
    }
    // 验证目标名称不含路径穿越字符
    if name.contains("..") || name.contains('\\') {
        return Err("LINKER_NAME_INVALID: Skill 名称包含非法字符".to_string());
    }

    let repo = ensure_repo_dir();
    let dest = repo.join(name);

    if dest.exists() {
        if !overwrite {
            return Err(format!("LINKER_NAME_TAKEN: 已存在同名资源: {}", name));
        }
        // 按 dest 类型清理：目录用 remove_dir_all，文件用 remove_file
        if dest.is_dir() {
            fs::remove_dir_all(&dest)
                .map_err(|e| format!("LINKER_CLEAN_DIR_FAIL: 清理旧目录失败: {}", e))?;
        } else {
            fs::remove_file(&dest)
                .map_err(|e| format!("LINKER_CLEAN_FILE_FAIL: 清理旧文件失败: {}", e))?;
        }
    }

    copy_dir_recursive(&canonical, &dest)?;
    Ok(())
}

/// 递归复制目录
/// 原子写入文件（write-to-temp + rename）
pub fn write_atomic(path: &Path, content: &str) -> Result<(), String> {
    let temp = path.with_extension("tmp");
    fs::write(&temp, content)
        .map_err(|e| format!("LINKER_WRITE_TEMP_FAIL: 写入临时文件失败: {}", e))?;
    fs::rename(&temp, path).map_err(|e| format!("LINKER_RENAME_FAIL: 重命名失败: {}", e))?;
    Ok(())
}

/// 对配置文件加排他锁后执行原子写入，防止多个 兔维斯 实例并发写同一配置
pub fn write_config_locked(path: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("RESOURCE_MKDIR_FAIL: 创建目录失败: {}", e))?;
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .open(path)
        .map_err(|e| format!("LINKER_OPEN_CONFIG_FAIL: 打开配置文件失败: {}", e))?;
    file.lock_exclusive()
        .map_err(|e| format!("LINKER_FILE_LOCK_FAIL: 获取文件锁失败: {}", e))?;
    let result = (|| {
        let temp = path.with_extension("tmp");
        fs::write(&temp, content)
            .map_err(|e| format!("LINKER_WRITE_TEMP_FAIL: 写入临时文件失败: {}", e))?;
        fs::rename(&temp, path).map_err(|e| format!("LINKER_RENAME_FAIL: 重命名失败: {}", e))?;
        Ok(())
    })();
    let _ = file.unlock();
    result
}

pub fn copy_dir_recursive(source: &Path, dest: &Path) -> Result<(), String> {
    fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    for entry in fs::read_dir(source).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dest_path)?;
        } else {
            fs::copy(&src_path, &dest_path).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// 链接 target 归一化（「单跳字面 target」语义，不触盘、不解析任何 symlink）：
/// 1) Windows `\\?\` / `\\?\UNC\` verbatim 前缀剥离（UNC 还原为 `\\server\share`）；
/// 2) 相对 target 按链接所在目录绝对化；
/// 3) 词法消解 `.` / `..`（越出根的 `..` 就地忽略）。
///
/// 供需要「字面 target 相等/前缀比较」的两处共用：§4.4 直链守卫（commands/manifest）
/// 与 §4.3 遗留链接谓词（services/resource/migration）——两侧必须同规则归一化。
/// 刻意不用 canonicalize：canonicalize 会沿链接链穿透解析到最终真实路径（破坏
/// 单跳语义），且断链 target 无法 canonicalize。
pub fn normalize_link_target(target: &Path, link_parent: &Path) -> PathBuf {
    let s = target.to_string_lossy();
    // was_extended：剥离过 `\\?\` 前缀的路径必为 Windows 绝对路径（扩展长度路径恒为
    // 绝对）——绝对性判定不能依赖 Path::is_absolute()，它在 Unix 上只认 `/` 开头，
    // 会把 `C:\...` 误判为相对
    let (stripped, was_extended) = if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        (format!(r"\\{}", rest), true)
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        (rest.to_string(), true)
    } else {
        (s.to_string(), false)
    };
    let joined = {
        let p = PathBuf::from(stripped);
        if was_extended || p.is_absolute() {
            p
        } else {
            link_parent.join(p)
        }
    };
    let mut resolved = PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other.as_os_str()),
        }
    }
    resolved
}

/// 递归比较两个目录内容是否一致（条目名集合 + 文件字节）。
/// 用于「工具侧真目录是否为 SSOT 的未修改副本」判定：一致才允许替换为链接，
/// 不一致（用户就地修改/手装异内容）必须拒绝，防止静默覆盖用户数据（review I-1）。
/// 任一侧读失败按不一致处理（保守拒绝）。
pub fn dir_contents_equal(a: &Path, b: &Path) -> bool {
    let entries_of = |dir: &Path| -> Option<Vec<std::ffi::OsString>> {
        let mut names: Vec<std::ffi::OsString> = fs::read_dir(dir)
            .ok()?
            .flatten()
            .map(|e| e.file_name())
            .collect();
        names.sort();
        Some(names)
    };
    let Some(a_names) = entries_of(a) else {
        return false;
    };
    let Some(b_names) = entries_of(b) else {
        return false;
    };
    if a_names != b_names {
        return false;
    }
    for name in a_names {
        let (pa, pb) = (a.join(&name), b.join(&name));
        // review N-3：以 symlink_metadata 判「本体类型」，链接不跟随——链接对链接比
        // 字面 target、链接对非链接直接判不一致。跟随式 is_dir() 递归会被链接环
        // （sub → .）无界递归打爆栈（与 scan_skills_recursive 的防环模式对齐）
        let (Ok(ma), Ok(mb)) = (fs::symlink_metadata(&pa), fs::symlink_metadata(&pb)) else {
            // 读失败（权限/竞态删除）→ 保守按不一致
            return false;
        };
        let (a_link, b_link) = (ma.is_symlink(), mb.is_symlink());
        if a_link || b_link {
            // 链接对链接：比单跳字面 target 的原始形式（逐字节，保守语义——
            // 平行树内同写法的相对链接判等，写法不同即判不一致，宁可拒绝不误放行）
            if !(a_link && b_link) {
                return false;
            }
            let (Ok(ta), Ok(tb)) = (fs::read_link(&pa), fs::read_link(&pb)) else {
                return false;
            };
            if ta != tb {
                return false;
            }
            continue;
        }
        match (ma.is_dir(), mb.is_dir()) {
            (true, true) => {
                if !dir_contents_equal(&pa, &pb) {
                    return false;
                }
            }
            // 目录 vs 文件 → 不一致
            (true, false) | (false, true) => return false,
            (false, false) => match (fs::read(&pa), fs::read(&pb)) {
                (Ok(ca), Ok(cb)) => {
                    if ca != cb {
                        return false;
                    }
                }
                _ => return false,
            },
        }
    }
    true
}

#[cfg(test)]
mod normalize_link_target_tests {
    use super::*;

    /// Windows `\\?\` / `\\?\UNC\` 前缀剥离（字符串级逻辑，双平台可测）
    #[test]
    fn strips_windows_verbatim_prefixes() {
        let parent = Path::new("/home/u/.agents/skills");
        assert_eq!(
            normalize_link_target(Path::new(r"\\?\C:\Users\u\.tuvis\active\codex\foo"), parent),
            PathBuf::from(r"C:\Users\u\.tuvis\active\codex\foo")
        );
        assert_eq!(
            normalize_link_target(
                Path::new(r"\\?\UNC\server\share\.tuvis\active\codex\foo"),
                parent
            ),
            PathBuf::from(r"\\server\share\.tuvis\active\codex\foo")
        );
    }

    /// 相对 target join 链接父目录 + 词法消解 `.` / `..`
    #[test]
    fn joins_relative_and_folds_lexically() {
        let parent = Path::new("/home/u/.agents/skills");
        assert_eq!(
            normalize_link_target(Path::new("../active/codex/foo"), parent),
            PathBuf::from("/home/u/.agents/active/codex/foo")
        );
        assert_eq!(
            normalize_link_target(Path::new("./local-skill"), parent),
            PathBuf::from("/home/u/.agents/skills/local-skill")
        );
        // 绝对路径原样保留（无 . / .. 时组件不变）
        assert_eq!(
            normalize_link_target(Path::new("/home/u/.tuvis/active/codex/foo"), parent),
            PathBuf::from("/home/u/.tuvis/active/codex/foo")
        );
    }
}

#[cfg(test)]
mod dir_contents_equal_tests {
    use super::*;

    #[test]
    fn identical_trees_are_equal() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        for dir in [&a, &b] {
            std::fs::create_dir_all(dir.join("sub")).unwrap();
            std::fs::write(dir.join("SKILL.md"), "hello").unwrap();
            std::fs::write(dir.join("sub/note.md"), "x".repeat(10)).unwrap();
        }
        assert!(dir_contents_equal(&a, &b));
        assert!(dir_contents_equal(&b, &a));
    }

    #[test]
    fn differing_file_bytes_or_names_are_not_equal() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        // 字节差异（用户就地修改）
        std::fs::write(a.join("SKILL.md"), "v1").unwrap();
        std::fs::write(b.join("SKILL.md"), "v2").unwrap();
        assert!(!dir_contents_equal(&a, &b));
        // 条目名差异（多余文件）
        std::fs::write(b.join("extra.md"), "v1").unwrap();
        assert!(!dir_contents_equal(&a, &b));
    }

    #[test]
    fn missing_or_type_mismatched_sides_are_not_equal() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        // 另一侧不存在 → 保守不一致
        assert!(!dir_contents_equal(&a, &b));
        // 目录 vs 文件 → 不一致
        std::fs::create_dir_all(&b).unwrap();
        std::fs::create_dir_all(a.join("sub")).unwrap();
        std::fs::write(b.join("sub"), "not-a-dir").unwrap();
        assert!(!dir_contents_equal(&a, &b));
    }

    /// review N-3 回归锁：链接环（sub → .）不得无界递归（旧实现跟随式 is_dir()
    /// 会栈溢出）；链接对链接比字面 target、链接对非链接判不一致。
    /// 三个场景各用独立目录对，避免前序用例的环污染后续断言
    #[test]
    #[cfg(unix)]
    fn symlink_loops_do_not_recurse_and_links_compare_by_target() {
        use std::os::unix::fs::symlink;

        let mk = |tmp: &tempfile::TempDir, tag: &str| {
            let dir = tmp.path().join(tag);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("SKILL.md"), "same").unwrap();
            dir
        };

        // 场景一：链接环 a/sub → .（自指）vs b/sub 真目录。旧实现此处栈溢出；
        // 新实现链接 vs 真目录 → false（不跟随、不触达环）
        let tmp1 = tempfile::tempdir().unwrap();
        let (a1, b1) = (mk(&tmp1, "a"), mk(&tmp1, "b"));
        symlink(".", a1.join("sub")).unwrap();
        std::fs::create_dir_all(b1.join("sub")).unwrap();
        assert!(!dir_contents_equal(&a1, &b1));

        // 场景二：链接对链接、字面 target 相同 → 一致（不跟随、不触达环）
        let tmp2 = tempfile::tempdir().unwrap();
        let (a2, b2) = (mk(&tmp2, "a"), mk(&tmp2, "b"));
        symlink("SKILL.md", a2.join("alias")).unwrap();
        symlink("SKILL.md", b2.join("alias")).unwrap();
        assert!(dir_contents_equal(&a2, &b2));

        // 场景三：链接对链接、字面 target 不同 → 不一致
        let tmp3 = tempfile::tempdir().unwrap();
        let (a3, b3) = (mk(&tmp3, "a"), mk(&tmp3, "b"));
        symlink("SKILL.md", a3.join("alias")).unwrap();
        symlink("other", b3.join("alias")).unwrap();
        assert!(!dir_contents_equal(&a3, &b3));
    }
}

/// 派发拍平（用户裁决 2026-09-17）的纯函数契约
#[cfg(test)]
mod dispatch_name_tests {
    use super::*;

    /// 嵌套规范名 → 拍平派发名；平铺名恒等（幂等）
    #[test]
    fn flattens_slashes_and_is_identity_for_flat_names() {
        assert_eq!(
            dispatch_name("superpowers/brainstorming"),
            "superpowers-brainstorming"
        );
        assert_eq!(dispatch_name("a/b/c"), "a-b-c");
        assert_eq!(dispatch_name("plain-skill"), "plain-skill");
    }

    /// 派发目标 = base.join(拍平名)——嵌套名不得在 base 下建子目录
    #[test]
    fn dispatch_target_joins_flat_name() {
        assert_eq!(
            dispatch_target(Path::new("/h/.claude/skills"), "suite/inner"),
            PathBuf::from("/h/.claude/skills/suite-inner")
        );
        assert_eq!(
            dispatch_target(Path::new("/h/.claude/skills"), "plain"),
            PathBuf::from("/h/.claude/skills/plain")
        );
    }
}

#[cfg(test)]
mod link_health_tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn detects_dangling_junction() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let link = tmp.path().join("lnk");
        junction::create(&src, &link).unwrap();
        assert_eq!(check_link_health(&link), LinkHealth::Valid);
        std::fs::remove_dir_all(&src).unwrap();
        assert_eq!(check_link_health(&link), LinkHealth::Dangling);
    }

    #[cfg(windows)]
    #[test]
    fn dangling_junction_removed_by_remove_link() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let link = tmp.path().join("lnk");
        junction::create(&src, &link).unwrap();
        std::fs::remove_dir_all(&src).unwrap();
        assert_eq!(check_link_health(&link), LinkHealth::Dangling);
        remove_link(&link).unwrap();
        assert!(!link.exists() && !link.is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn detects_dangling_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let link = tmp.path().join("lnk");
        std::os::unix::fs::symlink(&src, &link).unwrap();
        assert_eq!(check_link_health(&link), LinkHealth::Valid);
        std::fs::remove_dir_all(&src).unwrap();
        assert_eq!(check_link_health(&link), LinkHealth::Dangling);
    }

    #[test]
    fn missing_and_native_paths() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            check_link_health(&tmp.path().join("none")),
            LinkHealth::Missing
        );
        let native = tmp.path().join("real");
        std::fs::create_dir_all(&native).unwrap();
        assert_eq!(check_link_health(&native), LinkHealth::NotLink);
    }
}
