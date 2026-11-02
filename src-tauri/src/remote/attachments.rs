// 移动端附件上传（2026-09-20 用户裁决，方案见计划 polished-gliding-simon）：
//
// 存储 = <会话工作目录>/.tuvis-attachments/<session_id>/（**用户项目目录下**）——
// 所有工具读自己工作区内的文件都天然零审批（各工具权限模型的公共下限）；
// 附件随项目生命周期存在、收尾整目录清理。
//
// git 零污染 = 首份附件写入时幂等追加 `<cwd>/.git/info/exclude` 一行
// `.tuvis-attachments/`——**本地管理区**（非 .gitignore 跟踪文件，兔维斯 不往用户
// 仓库写跟踪文件）；非 git 项目（无 .git）整步跳过（无版本库无误提交风险）。
//
// 纯函数核（消毒 / 目录计算 / exclude 行判定）与 IO（写盘 / 追加）分离：
// 纯函数零 IO 直接测；IO 测试一律 tempdir，零真实用户目录。

use std::path::{Path, PathBuf};

/// 附件目录名（项目根下、点前缀隐藏目录）
pub const ATTACHMENT_DIR_NAME: &str = ".tuvis-attachments";
/// 单附件字节上限（20MB；端点同时显式 DefaultBodyLimit 硬兜底）
pub const MAX_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;
/// git/info/exclude 里追加的排除行（尾斜杠 = 目录整体排除）
pub const GIT_EXCLUDE_LINE: &str = ".tuvis-attachments/";
/// 消毒后文件名的字符数上限（防超长名撑爆目录项）
const MAX_FILE_NAME_CHARS: usize = 120;

/// 文件名消毒（2026-09-20 安全边界）：先取路径最后一段（客户端传名，**杜绝路径
/// 注入**），再逐字符过滤——保留字母数字（含 CJK）与 `-_. `，其余（分隔符/控制
/// 字符/符号）替换为 `_`；去首尾点与空白（防 `..` 与隐藏文件）；空 → `file`；
/// 超长按字符截断。纯函数。
pub fn sanitize_file_name(raw: &str) -> String {
    let base = Path::new(raw)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| raw.to_string());
    let filtered: String = base
        .chars()
        .map(|ch| {
            if ch.is_alphanumeric() || matches!(ch, '-' | '_' | '.' | ' ' | '(' | ')') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = filtered.trim().trim_matches('.').to_string();
    let joined = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = joined;
    if out.is_empty() {
        out = "file".to_string();
    }
    if out.chars().count() > MAX_FILE_NAME_CHARS {
        out = out.chars().take(MAX_FILE_NAME_CHARS).collect();
    }
    out
}

/// 落盘目录 = <cwd>/.tuvis-attachments/<session_id>（纯函数）
pub fn attachment_dir(cwd: &Path, session_id: &str) -> PathBuf {
    cwd.join(ATTACHMENT_DIR_NAME).join(session_id)
}

/// 只接受当前会话附件目录内的现存普通文件。
pub(crate) fn valid_attachment_path(workspace: &Path, session_id: &str, raw_path: &str) -> bool {
    use std::path::Component;
    let mut components = Path::new(session_id).components();
    if !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
        || raw_path.chars().any(|c| c == '"' || c.is_control())
    {
        return false;
    }
    let Ok(root) = workspace.canonicalize() else {
        return false;
    };
    let attachment_root = root.join(ATTACHMENT_DIR_NAME);
    let session_dir = attachment_root.join(session_id);
    if [
        &attachment_root,
        &session_dir,
        &Path::new(raw_path).to_path_buf(),
    ]
    .iter()
    .any(|path| std::fs::symlink_metadata(path).map_or(true, |m| m.file_type().is_symlink()))
    {
        return false;
    }
    let Ok(dir) = session_dir.canonicalize() else {
        return false;
    };
    let Ok(path) = Path::new(raw_path).canonicalize() else {
        return false;
    };
    dir.starts_with(&root)
        && path.parent() == Some(dir.as_path())
        && path
            .metadata()
            .is_ok_and(|m| m.is_file() && m.len() <= MAX_ATTACHMENT_BYTES as u64)
}

/// 待追加的本地排除文件路径：None = 非 git 仓库（无 .git，整步跳过）或
/// exclude 已含该行（幂等——重复上传不重复追加）。纯函数（只读）。
pub fn pending_git_exclude(cwd: &Path, line: &str) -> Option<PathBuf> {
    if !cwd.join(".git").is_dir() {
        return None;
    }
    let exclude = cwd.join(".git").join("info").join("exclude");
    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    let present = existing.lines().any(|l| l.trim() == line.trim());
    if present {
        None
    } else {
        Some(exclude)
    }
}

/// 幂等追加本地排除行（best-effort：失败仅日志，不影响附件落盘本身）。
/// 时机 = 首份附件写入的同一刻（调用方在写盘前调一次）。
pub fn ensure_git_exclude(cwd: &Path, line: &str) {
    if let Some(exclude) = pending_git_exclude(cwd, line) {
        if let Some(parent) = exclude.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                log::warn!("附件本地排除目录创建失败: {e}");
                return;
            }
        }
        let mut content = std::fs::read_to_string(&exclude).unwrap_or_default();
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(line);
        content.push('\n');
        if let Err(e) = std::fs::write(&exclude, content) {
            log::warn!("附件本地排除写入失败（不影响附件本身）: {e}");
        }
    }
}

/// 同名落盘去重（2026-09-20 用户裁决，对齐微信语义）：**保留原始文件名**——
/// 文件池按名搜索、agent 识名都依赖原始名，任何哈希/纳秒前缀都会破坏索引；
/// 同名已存在 → 追加 `(1)`、`(2)` 序号（a.txt → "a (1).txt"），不覆盖旧文件。
/// 括号在消毒白名单内，标记路径 `path="X"` 引用安全。纯函数。
fn unique_target(dir: &Path, base: &str) -> PathBuf {
    let first = dir.join(base);
    if !first.exists() {
        return first;
    }
    let stem = Path::new(base)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| base.to_string());
    let ext = Path::new(base)
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy()))
        .unwrap_or_default();
    let mut n: u32 = 1;
    loop {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

/// 落盘（IO）：建目录 → 幂等确保本地排除 → 写 **原始文件名**（同名追加序号，
/// 不覆盖）→ 返回绝对路径。会话 id 仅作子目录名（消毒后存储，各工具形态不同
/// 不做格式校验）。
pub fn write_attachment(
    cwd: &Path,
    session_id: &str,
    raw_name: &str,
    bytes: &[u8],
) -> std::io::Result<PathBuf> {
    let dir = attachment_dir(cwd, session_id);
    std::fs::create_dir_all(&dir)?;
    ensure_git_exclude(cwd, GIT_EXCLUDE_LINE);
    let path = unique_target(&dir, &sanitize_file_name(raw_name));
    std::fs::write(&path, bytes)?;
    Ok(path)
}

// ---- 数据管理（C5，桌面端「数据管理」卡片的数据源）----
//
// 附件分散在各项目目录（.tuvis-attachments/<会话>/），桌面端要列出/清理就必须
// 知道「哪些项目有附件」——索引文件 ~/.tuvis/attachments-index.json 在上传时
// 追加一条（服务端写入，客户端零路径输入），是项目发现的唯一可信来源。

/// 索引条目（~/.tuvis/attachments-index.json 数组元素）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AttachmentIndexEntry {
    /// 会话工作目录（.tuvis-attachments 所在项目）
    pub project: String,
    pub session: String,
    pub tool: String,
    /// 消毒后的落盘文件名
    pub name: String,
    /// 落盘绝对路径
    pub path: String,
    pub size: u64,
    /// 毫秒时间戳
    pub ts: i64,
}

/// 索引文件路径（home 注入，测试可 tempdir）
pub fn index_path(home: &Path) -> PathBuf {
    home.join(ATTACHMENT_DIR_NAME.trim_start_matches('.'))
        .join("attachments-index.json")
}

/// 读索引（文件缺失/损坏 → 空数组防御）
pub fn read_index(home: &Path) -> Vec<AttachmentIndexEntry> {
    let raw = std::fs::read_to_string(index_path(home)).unwrap_or_default();
    serde_json::from_str(&raw).unwrap_or_default()
}

/// 追加索引条目（IO）：目录不存在即建；写失败仅日志（索引是管理面数据源，
/// 丢失只影响桌面列出，不影响附件本身）
pub fn append_index_entry(home: &Path, entry: &AttachmentIndexEntry) {
    let mut all = read_index(home);
    all.push(entry.clone());
    let file = index_path(home);
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match serde_json::to_string_pretty(&all) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&file, json) {
                log::warn!("附件索引写入失败（不影响附件本身）: {e}");
            }
        }
        Err(e) => log::warn!("附件索引序列化失败: {e}"),
    }
}

/// 剔除某项目的全部索引条目（清理动作后调用，与磁盘删除配套）
pub fn prune_index_for_project(home: &Path, project: &str) {
    let all = read_index(home);
    let kept: Vec<_> = all
        .into_iter()
        .filter(|e| !e.project.eq_ignore_ascii_case(project))
        .collect();
    let file = index_path(home);
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match serde_json::to_string_pretty(&kept) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&file, json) {
                log::warn!("附件索引回写失败: {e}");
            }
        }
        Err(e) => log::warn!("附件索引序列化失败: {e}"),
    }
}

/// 项目附件占用统计（IO：递归走 <project>/.tuvis-attachments）；目录不存在 → (0, 0)
pub fn project_attachment_stats(project: &Path) -> (u64, u64) {
    let root = project.join(ATTACHMENT_DIR_NAME);
    let mut files: u64 = 0;
    let mut bytes: u64 = 0;
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                files += 1;
                bytes += meta.len();
            }
        }
    }
    (files, bytes)
}

/// 清理某项目的附件目录（IO：整目录删除——用户裁决：随项目生命周期、收尾清理）
pub fn clean_project_attachments(project: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(project.join(ATTACHMENT_DIR_NAME)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()), // 已清理 = 幂等成功
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{temp_dir, TestDir};

    /// 本族每个用例的临时目录：唯一性来自 `test_support`（pid + 进程内原子序号），
    /// **不再**用 `as_nanos()`——旧写法同一微秒内撞名，并行用例互相
    /// `remove_dir_all`（存量 flake 根因，见 `test_support` 模块文档）。
    fn tempdir() -> TestDir {
        temp_dir("mam-attach-test")
    }

    // ---- sanitize_file_name ----

    #[test]
    #[cfg_attr(
        not(windows),
        ignore = "断言 Windows 形态（bash 包装/反斜杠分隔符语义）；非 Windows 平台行为另测"
    )]
    fn sanitize_strips_path_traversal_and_separators() {
        // 路径注入三形态：正斜杠 / 反斜杠 / 盘符——全部只剩最后一段
        assert_eq!(sanitize_file_name("a/b/c.png"), "c.png");
        assert_eq!(sanitize_file_name("..\\..\\evil.txt"), "evil.txt");
        assert_eq!(sanitize_file_name("C:\\Users\\x\\图.png"), "图.png");
        // 纯 ".." → 无有效名 → 兜底 file
        assert_eq!(sanitize_file_name(".."), "file");
        assert_eq!(sanitize_file_name("."), "file");
        assert_eq!(sanitize_file_name(""), "file");
    }

    #[test]
    fn sanitize_keeps_cjk_and_replaces_controls() {
        // CJK 保留（附件名中文是常态）；控制字符/符号替换为 _
        assert_eq!(sanitize_file_name("设计稿 v2(终).png"), "设计稿 v2(终).png");
        assert_eq!(sanitize_file_name("a\u{0001}b<c>d*e"), "a_b_c_d_e");
    }

    #[test]
    fn sanitize_trims_dots_and_caps_length() {
        assert_eq!(sanitize_file_name("  .hidden.log.  "), "hidden.log");
        let long = "名".repeat(200);
        let out = sanitize_file_name(&long);
        assert_eq!(out.chars().count(), MAX_FILE_NAME_CHARS);
    }

    // ---- attachment_dir ----

    #[test]
    fn attachment_dir_is_cwd_dot_mam_session() {
        let dir = attachment_dir(Path::new("E:/proj"), "sess_abc");
        let s = dir.to_string_lossy().replace('\\', "/");
        assert!(s.ends_with("/.tuvis-attachments/sess_abc"), "{s}");
        assert!(s.contains("E:/proj"), "{s}");
    }

    // ---- pending_git_exclude / ensure_git_exclude ----

    #[test]
    fn exclude_skipped_for_non_git_project() {
        let root = tempdir();
        // 无 .git → None（不写任何排除文件——用户定案：非 git 项目跳过）
        assert_eq!(pending_git_exclude(&root, GIT_EXCLUDE_LINE), None);
        assert!(!root.join(".git").exists());
    }

    #[test]
    fn exclude_appended_once_then_idempotent() {
        let root = tempdir();
        std::fs::create_dir_all(root.join(".git").join("info")).unwrap();
        // 首次：待追加
        let file = pending_git_exclude(&root, GIT_EXCLUDE_LINE).expect("首份附件应待追加");
        assert!(file.ends_with("info/exclude"));
        ensure_git_exclude(&root, GIT_EXCLUDE_LINE);
        let content = std::fs::read_to_string(&file).unwrap();
        assert!(content.contains(".tuvis-attachments/"), "{content}");
        // 幂等：第二次不再追加（行数不涨）
        assert_eq!(pending_git_exclude(&root, GIT_EXCLUDE_LINE), None);
        ensure_git_exclude(&root, GIT_EXCLUDE_LINE);
        let content2 = std::fs::read_to_string(&file).unwrap();
        assert_eq!(content.matches(".tuvis-attachments/").count(), 1);
        assert_eq!(content2, content);
    }

    #[test]
    fn exclude_append_normalizes_missing_trailing_newline() {
        let root = tempdir();
        std::fs::create_dir_all(root.join(".git").join("info")).unwrap();
        // 既有 exclude 无尾换行 → 追加前补换行（不与已有行粘连）
        std::fs::write(
            root.join(".git").join("info").join("exclude"),
            "node_modules",
        )
        .unwrap();
        ensure_git_exclude(&root, GIT_EXCLUDE_LINE);
        let content =
            std::fs::read_to_string(root.join(".git").join("info").join("exclude")).unwrap();
        assert!(
            content.contains("node_modules\n.tuvis-attachments/"),
            "{content}"
        );
    }

    // ---- write_attachment ----

    #[test]
    fn write_roundtrip_preserves_original_file_name() {
        let root = tempdir();
        std::fs::create_dir_all(root.join(".git").join("info")).unwrap();
        let bytes = b"\x89PNG fake image bytes";
        let path = write_attachment(&root, "sess_abc", "截图.png", bytes).unwrap();
        let parent = path.parent().unwrap();
        assert_eq!(
            parent,
            root.join(".tuvis-attachments").join("sess_abc"),
            "落盘 = <cwd>/.tuvis-attachments/<session>/"
        );
        // 2026-09-20 用户反馈：文件名保留原名（不加哈希前缀——文件池按名搜索、
        // agent 识名都依赖原始名）
        let name = path.file_name().unwrap().to_string_lossy();
        assert_eq!(name, "截图.png");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        // 首份写入即完成本地排除（同一刻）
        let content =
            std::fs::read_to_string(root.join(".git").join("info").join("exclude")).unwrap();
        assert!(content.contains(".tuvis-attachments/"));
    }

    #[test]
    fn write_same_name_twice_no_overwrite() {
        let root = tempdir();
        let p1 = write_attachment(&root, "s", "a.txt", b"one").unwrap();
        let p2 = write_attachment(&root, "s", "a.txt", b"two").unwrap();
        assert_ne!(p1, p2, "同名二次落盘追加序号，不覆盖首份");
        // 微信语义：同名追加 (1) 尾缀，原名保留可读
        assert_eq!(
            p2.file_name().unwrap().to_string_lossy(),
            "a (1).txt",
            "同名第二次落盘 = a (1).txt"
        );
        assert_eq!(std::fs::read(&p1).unwrap(), b"one");
    }

    // ---- 索引（C5 数据管理数据源）----

    #[test]
    fn index_roundtrip_append_and_prune() {
        let home = tempdir();
        assert!(read_index(&home).is_empty(), "缺文件 → 空索引");
        let e1 = AttachmentIndexEntry {
            project: "E:/proj/a".into(),
            session: "sess_1".into(),
            tool: "claude".into(),
            name: "a-shot.png".into(),
            path: "E:/proj/a/.tuvis-attachments/sess_1/a-shot.png".into(),
            size: 9,
            ts: 1000,
        };
        append_index_entry(&home, &e1);
        append_index_entry(&home, &e1); // 重复追加（同条）也入索引——磁盘上确是两个文件
        assert_eq!(read_index(&home).len(), 2);
        prune_index_for_project(&home, "e:/proj/a"); // 大小写不敏感剔除
        assert!(read_index(&home).is_empty());
    }

    #[test]
    fn index_corrupt_file_falls_back_to_empty() {
        let home = tempdir();
        std::fs::create_dir_all(index_path(&home).parent().unwrap()).unwrap();
        std::fs::write(index_path(&home), "not-json{{").unwrap();
        assert!(read_index(&home).is_empty(), "损坏索引防御性降级为空");
    }

    #[test]
    fn project_stats_walks_dir_tree() {
        let root = tempdir();
        let att = root.join(".tuvis-attachments").join("sess_1");
        std::fs::create_dir_all(&att).unwrap();
        std::fs::write(att.join("a.png"), b"12345").unwrap();
        std::fs::create_dir_all(att.join("nested")).unwrap();
        std::fs::write(att.join("nested").join("b.txt"), b"123").unwrap();
        let (files, bytes) = project_attachment_stats(&root);
        assert_eq!((files, bytes), (2, 8));
        // 无附件目录 → (0, 0)
        let (f2, b2) = project_attachment_stats(&root.join("nope"));
        assert_eq!((f2, b2), (0, 0));
    }
}
