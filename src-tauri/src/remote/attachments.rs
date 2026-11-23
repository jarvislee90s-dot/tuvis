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
/// 单附件字节上限（20MB；H07 D1 起由上传端点 handler 的限长读取单点执行，
/// 超限响应契约 JSON 413 体）
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
/// 括号在消毒白名单内，标记路径 `path="X"` 引用安全。原子独占创建并持有句柄。
#[cfg(any(test, not(unix)))]
fn unique_target(dir: &Path, base: &str) -> std::io::Result<(PathBuf, std::fs::File)> {
    let stem = Path::new(base)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    let ext = Path::new(base)
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy()))
        .unwrap_or_default();
    for n in 0..=u32::MAX {
        let candidate = dir.join(if n == 0 {
            base.to_string()
        } else {
            format!("{stem} ({n}){ext}")
        });
        #[cfg(test)]
        tests::before_create();
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::other("attachment name space exhausted"))
}

// 失败也不删除路径：其他请求只能创建新路径；避免清理与用户目录删除/重建竞争时
// unlink 到另一个上传。未完成的文件没有成功回执，不会进入手机发送意图。
#[cfg(any(test, not(unix)))]
fn write_owned(
    mut file: std::fs::File,
    path: PathBuf,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> std::io::Result<PathBuf> {
    write(&mut file)?;
    Ok(path)
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
    write_attachment_with_exclude_warning(cwd, session_id, raw_name, bytes).map(|(path, _)| path)
}

/// 同 [`write_attachment`]，但把「本地排除规则未确认追加」的原因一并返回：
/// 附件已成功落盘时调用方仍应返回成功回执，并把该警告如实告知用户。
pub fn write_attachment_with_exclude_warning(
    cwd: &Path,
    session_id: &str,
    raw_name: &str,
    bytes: &[u8],
) -> std::io::Result<(PathBuf, Option<String>)> {
    #[cfg(unix)]
    return write_attachment_unix(cwd, session_id, raw_name, bytes);
    #[cfg(not(unix))]
    {
        let dir = attachment_dir(cwd, session_id);
        std::fs::create_dir_all(&dir)?;
        ensure_git_exclude(cwd, GIT_EXCLUDE_LINE);
        use std::io::Write;
        let (path, file) = unique_target(&dir, &sanitize_file_name(raw_name))?;
        write_owned(file, path.clone(), |file| file.write_all(bytes))?;
        Ok((path, None))
    }
}

#[cfg(unix)]
fn write_attachment_unix(
    cwd: &Path,
    session_id: &str,
    raw_name: &str,
    bytes: &[u8],
) -> std::io::Result<(PathBuf, Option<String>)> {
    use std::ffi::{CStr, CString};
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    fn name(value: &str) -> std::io::Result<CString> {
        CString::new(value).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid path component")
        })
    }
    fn open_at(
        parent: &std::fs::File,
        name: &CStr,
        flags: i32,
        mode: libc::mode_t,
    ) -> std::io::Result<std::fs::File> {
        // SAFETY: parent 和 name 在调用期间有效；成功的 fd 由 File 唯一接管。
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                mode as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { std::fs::File::from_raw_fd(fd) })
    }
    /// 返回 true = 本实现创建了 exclude；false = linkat 撞 AlreadyExists
    /// （**可观察到的竞争创建**：我们刚观察到缺席，别人抢先建了文件）。
    fn create_exclude_if_absent(info: &std::fs::File) -> std::io::Result<bool> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let temp = name(&format!(
            ".tuvis-exclude-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))?;
        let final_name = name("exclude")?;
        let mut candidate = open_at(
            info,
            &temp,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )?;
        let result = (|| -> std::io::Result<bool> {
            writeln!(candidate, "{GIT_EXCLUDE_LINE}")?;
            // SAFETY: 两个名称都是固定目录 fd 下的单一路径组件。
            let rc = unsafe {
                libc::linkat(
                    info.as_raw_fd(),
                    temp.as_ptr(),
                    info.as_raw_fd(),
                    final_name.as_ptr(),
                    0,
                )
            };
            if rc != 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(error);
                }
                return Ok(false);
            }
            Ok(true)
        })();
        // SAFETY: 仅清理由本次 O_EXCL 创建的临时目录项。
        unsafe {
            libc::unlinkat(info.as_raw_fd(), temp.as_ptr(), 0);
        }
        result
    }
    fn directory(
        parent: &std::fs::File,
        part: &str,
        create: bool,
    ) -> std::io::Result<std::fs::File> {
        let component = name(part)?;
        if create {
            // SAFETY: parent 和 component 在调用期间有效。
            let result = unsafe { libc::mkdirat(parent.as_raw_fd(), component.as_ptr(), 0o700) };
            if result != 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(error);
                }
            }
        }
        open_at(parent, &component, libc::O_RDONLY | libc::O_DIRECTORY, 0)
    }
    /// 在已有 exclude 的**原 inode** 上追加本地排除规则。
    ///
    /// 协议：O_APPEND 新开同一 inode 的写句柄 → 身份复核（与只读基线句柄同 inode、
    /// 路径仍指向它、无硬链接）→ `flock(LOCK_EX|LOCK_NB)` 协作锁（只串行兔维斯自身）
    /// → 锁内重读比对基线 → **单次** write 追加（前导 LF 保证规则独立成行）→ 写后
    /// 复核（内容恰为基线+本次缓冲、身份/链接数不变）。任何一步不符都以错误显式
    /// 报告：不截断、不重写、不换文件、不回滚；副作用是否已发生如实交调用方提示。
    fn append_exclude_rule(
        info: &std::fs::File,
        baseline: &mut std::fs::File,
        exclude_path: &Path,
        baseline_bytes: &[u8],
    ) -> Result<(), String> {
        use std::os::unix::fs::MetadataExt;
        let exclude_name =
            name("exclude").map_err(|error| format!("invalid exclude name: {error}"))?;
        let mut append = match open_at(
            info,
            &exclude_name,
            libc::O_WRONLY | libc::O_APPEND | libc::O_NONBLOCK,
            0,
        ) {
            Ok(file) => file,
            Err(error) => return Err(format!("reopen git exclude for append: {error}")),
        };
        {
            let base_meta = match baseline.metadata() {
                Ok(meta) => meta,
                Err(error) => return Err(format!("stat git exclude baseline: {error}")),
            };
            let append_meta = match append.metadata() {
                Ok(meta) => meta,
                Err(error) => return Err(format!("stat git exclude append: {error}")),
            };
            if base_meta.dev() != append_meta.dev() || base_meta.ino() != append_meta.ino() {
                return Err("git exclude replaced before append".into());
            }
            let current = match std::fs::symlink_metadata(exclude_path) {
                Ok(meta) => meta,
                Err(error) => return Err(format!("stat git exclude path: {error}")),
            };
            if current.file_type().is_symlink()
                || current.dev() != append_meta.dev()
                || current.ino() != append_meta.ino()
                || append_meta.nlink() != 1
            {
                return Err("git exclude identity changed before append".into());
            }
        }
        let lock_rc = unsafe { libc::flock(append.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if lock_rc != 0 {
            return Err("git exclude busy: another upload is updating it".into());
        }
        let result = (|| -> Result<(), String> {
            use std::io::{Read, Seek, SeekFrom, Write};
            if let Err(error) = baseline.seek(SeekFrom::Start(0)) {
                return Err(format!("rewind git exclude baseline: {error}"));
            }
            let mut fresh = Vec::new();
            if let Err(error) = baseline.take(1_048_577).read_to_end(&mut fresh) {
                return Err(format!("reread git exclude: {error}"));
            }
            if fresh.as_slice() != baseline_bytes {
                return Err("git exclude changed during upload (reread mismatch)".into());
            }
            // 前导 LF 仅在基线非空且不以换行结尾时补：并发追加者留下无尾换行内容
            // 也能保证规则独立成行；基线已有尾换行则紧贴追加，不产生多余空行。
            let mut buffer = Vec::with_capacity(GIT_EXCLUDE_LINE.len() + 2);
            if !baseline_bytes.is_empty() && !baseline_bytes.ends_with(b"\n") {
                buffer.push(b'\n');
            }
            buffer.extend_from_slice(GIT_EXCLUDE_LINE.as_bytes());
            buffer.push(b'\n');
            let written = append
                .write(&buffer)
                .map_err(|error| format!("append git exclude: {error}"))?;
            if written != buffer.len() {
                return Err("git exclude partially appended".into());
            }
            // 写后复核：内容恰为基线 + 本次缓冲；身份/链接数仍成立。
            if let Err(error) = baseline.seek(SeekFrom::Start(0)) {
                return Err(format!("rewind git exclude baseline: {error}"));
            }
            let mut after = Vec::new();
            if let Err(error) = baseline.take(1_048_578).read_to_end(&mut after) {
                return Err(format!("verify git exclude: {error}"));
            }
            let mut expected = baseline_bytes.to_vec();
            expected.extend_from_slice(&buffer);
            if after != expected {
                return Err("git exclude changed after append (post-write mismatch)".into());
            }
            let metadata = append
                .metadata()
                .map_err(|error| format!("stat git exclude after append: {error}"))?;
            if metadata.nlink() != 1 {
                return Err("git exclude hardlinked after append".into());
            }
            let current = std::fs::symlink_metadata(exclude_path)
                .map_err(|error| format!("stat git exclude path after append: {error}"))?;
            if current.file_type().is_symlink()
                || current.dev() != metadata.dev()
                || current.ino() != metadata.ino()
            {
                return Err("git exclude path replaced after append".into());
            }
            Ok(())
        })();
        unsafe { libc::flock(append.as_raw_fd(), libc::LOCK_UN) };
        result
    }
    if session_id.contains('\0')
        || !matches!(Path::new(session_id).components().collect::<Vec<_>>().as_slice(), [std::path::Component::Normal(part)] if *part == std::ffi::OsStr::new(session_id))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid session id",
        ));
    }
    // 与 `write_attachment`（非 unix 路径）同口径：目标项目目录可能尚未创建
    // （新建会话/合成工作区），先幂等建目录再以 O_NOFOLLOW 打开真实目录。
    // 不做这一步会把「目录不存在」当成 500 写盘失败，而不是正常创建附件目录。
    std::fs::create_dir_all(cwd)?;
    let root = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(cwd)?;
    // 先打开已有 Git 路径；链接或其他异常在附件落盘前拒绝。
    // 前置阶段的排除维护失败也计入警告，与追加阶段统一随回执返回。
    let git_path = cwd.join(".git");
    let mut exclude_warning: Option<String> = None;
    let mut exclude = match std::fs::symlink_metadata(&git_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
        Ok(meta) if meta.file_type().is_file() => None,
        Ok(_) => {
            let git = match directory(&root, ".git", false) {
                Ok(git) => Some(git),
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                    log::warn!("附件 Git 目录不可访问: {error}");
                    exclude_warning = Some(format!("git directory not accessible: {error}"));
                    None
                }
                Err(error) => return Err(error),
            };
            let info = match git
                .as_ref()
                .map(|git| directory(git, "info", true))
                .transpose()
            {
                Ok(info) => info,
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                    log::warn!("附件本地排除目录不可写: {error}");
                    exclude_warning = Some(format!("git info directory not writable: {error}"));
                    None
                }
                Err(error) => return Err(error),
            };
            #[cfg(test)]
            tests::before_exclude_open();
            // 可观察到的竞争创建：缺席观察后他人抢先建文件（linkat EEXIST）。
            // 此时绝不追加规则，整体退避并以 excludeWarning 如实上报。
            let mut creation_competed = false;
            match info
                .as_ref()
                .map(|info| {
                    let exclude_name = name("exclude")?;
                    match open_at(info, &exclude_name, libc::O_RDONLY | libc::O_NONBLOCK, 0) {
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            #[cfg(test)]
                            tests::before_exclude_create();
                            let created = create_exclude_if_absent(info)?;
                            if !created {
                                creation_competed = true;
                            }
                            open_at(info, &exclude_name, libc::O_RDONLY | libc::O_NONBLOCK, 0)
                        }
                        result => result,
                    }
                })
                .transpose()
            {
                Ok(Some(file)) => {
                    let metadata = file.metadata()?;
                    if !metadata.is_file() || metadata.nlink() != 1 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            "unsafe git exclude",
                        ));
                    }
                    #[cfg(test)]
                    tests::after_exclude_check();
                    let same_path =
                        |handle: &std::fs::File, path: &Path| -> std::io::Result<bool> {
                            let opened = handle.metadata()?;
                            let current = std::fs::symlink_metadata(path)?;
                            Ok(!current.file_type().is_symlink()
                                && opened.dev() == current.dev()
                                && opened.ino() == current.ino())
                        };
                    let info_handle = info.as_ref().expect("exclude requires info directory");
                    if file.metadata()?.nlink() != 1
                        || !same_path(
                            git.as_ref().expect("exclude requires git directory"),
                            &git_path,
                        )?
                        || !same_path(info_handle, &git_path.join("info"))?
                        || !same_path(&file, &git_path.join("info/exclude"))?
                    {
                        return Err(std::io::Error::other("git exclude changed during upload"));
                    }
                    Some((
                        file,
                        git.expect("exclude requires git directory"),
                        info.expect("exclude requires info directory"),
                        creation_competed,
                    ))
                }
                Ok(None) => None,
                Err(error) if error.raw_os_error() == Some(libc::ELOOP) => return Err(error),
                Err(error) => {
                    log::warn!("附件本地排除文件不可写: {error}");
                    exclude_warning = Some(format!("git exclude not writable: {error}"));
                    None
                }
            }
        }
    };
    let attachment_root = directory(&root, ATTACHMENT_DIR_NAME, true)?;
    let session_dir = directory(&attachment_root, session_id, true)?;
    let base = sanitize_file_name(raw_name);
    let stem = Path::new(&base)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    let extension = Path::new(&base)
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy()))
        .unwrap_or_default();
    for n in 0..=u32::MAX {
        let filename = if n == 0 {
            base.clone()
        } else {
            format!("{stem} ({n}){extension}")
        };
        #[cfg(test)]
        tests::before_create();
        match open_at(
            &session_dir,
            &name(&filename)?,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            0o600,
        ) {
            Ok(mut file) => {
                file.write_all(bytes)?;
                #[cfg(test)]
                tests::after_attachment_write();
                let path = attachment_dir(cwd, session_id).join(&filename);
                let same = |handle: &std::fs::File, path: &Path| -> std::io::Result<bool> {
                    let opened = handle.metadata()?;
                    let current = std::fs::symlink_metadata(path)?;
                    Ok(!current.file_type().is_symlink()
                        && opened.dev() == current.dev()
                        && opened.ino() == current.ino())
                };
                if !same(&root, cwd)?
                    || !same(&attachment_root, &cwd.join(ATTACHMENT_DIR_NAME))?
                    || !same(&session_dir, &attachment_dir(cwd, session_id))?
                    || !same(&file, &path)?
                {
                    return Err(std::io::Error::other(
                        "attachment path changed during write",
                    ));
                }
                #[cfg(test)]
                tests::after_inode_check();
                if let Some((exclude, git, info, creation_competed)) = exclude.as_mut() {
                    // 附件已落盘：排除侧身份复核失败只降级为警告并跳过更新，
                    // 绝不整单报错——那会跳过索引、留下未管理文件并诱发重复上传。
                    // 附件文件自身的路径身份检查（上方）仍是硬失败。
                    let exclude_intact =
                        exclude.metadata().map(|m| m.nlink() == 1).unwrap_or(false)
                            && same(git, &git_path).unwrap_or(false)
                            && same(info, &git_path.join("info")).unwrap_or(false)
                            && same(exclude, &git_path.join("info/exclude")).unwrap_or(false);
                    let update = if !exclude_intact {
                        Err("git exclude changed during upload; rule not auto-added".to_string())
                    } else {
                        (|| -> Result<(), String> {
                            // 观察到竞争创建（缺席→他人抢先创建）：整体退避，规则不自动追加，
                            // pending 状态保留，由用户自行补写。
                            if *creation_competed {
                                return Err("git exclude appeared during upload (competing creation); rule not auto-added".into());
                            }
                            let mut content = Vec::new();
                            if let Err(error) =
                                (&mut *exclude).take(1_048_577).read_to_end(&mut content)
                            {
                                return Err(format!("read git exclude: {error}"));
                            }
                            if content.len() > 1_048_576 {
                                return Err("git exclude too large".into());
                            }
                            let text = String::from_utf8_lossy(&content);
                            if text.lines().any(|line| line.trim() == GIT_EXCLUDE_LINE) {
                                return Ok(());
                            }
                            #[cfg(test)]
                            tests::before_exclude_write();
                            // 已有 exclude 缺规则：在**原 inode** 上追加（保全内容、权限与
                            // inode 身份）。协作锁只串行兔维斯自身；锁内重读比对，冲突/
                            // 竞态显式报告为警告，绝不截断重写或换文件。
                            append_exclude_rule(
                                info,
                                exclude,
                                &git_path.join("info/exclude"),
                                &content,
                            )
                        })()
                    };
                    if let Err(warning) = update {
                        log::warn!("附件本地排除未确认追加: {warning}");
                        exclude_warning = Some(warning);
                    }
                }
                if !same(&root, cwd)?
                    || !same(&attachment_root, &cwd.join(ATTACHMENT_DIR_NAME))?
                    || !same(&session_dir, &attachment_dir(cwd, session_id))?
                    || !same(&file, &path)?
                {
                    return Err(std::io::Error::other(
                        "attachment path changed during write",
                    ));
                }
                return Ok((path, exclude_warning));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::other("attachment name space exhausted"))
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
pub(crate) mod tests {
    use super::*;

    #[cfg(unix)]
    static UPLOAD_HARDLINKS: std::sync::LazyLock<
        std::sync::Mutex<std::collections::HashMap<String, PathBuf>>,
    > = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

    #[cfg(unix)]
    pub(crate) fn register_upload_exclude_hardlink(project: &Path, external: PathBuf) {
        UPLOAD_HARDLINKS
            .lock()
            .unwrap()
            .insert(project.to_string_lossy().into_owned(), external);
    }

    #[cfg(unix)]
    pub(crate) fn arm_upload_exclude_hardlink(project: &str) {
        if let Some(external) = UPLOAD_HARDLINKS.lock().unwrap().remove(project) {
            let exclude = Path::new(project).join(".git/info/exclude");
            let saved = Path::new(project).join(".tuvis-attachments/sess_att/a.txt");
            AFTER_INODE_CHECK.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || {
                    assert_eq!(std::fs::read(&saved).unwrap(), b"payload");
                    std::fs::hard_link(exclude, external).unwrap();
                }));
            });
        }
    }
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

    // 仅测试线程局部闸：把两个上传停在首次创建前，不影响其他并行测试。
    thread_local! {
        static CREATE_BARRIER: std::cell::RefCell<Option<std::sync::Arc<std::sync::Barrier>>> = const { std::cell::RefCell::new(None) };
    }
    pub(super) fn before_create() {
        CREATE_BARRIER.with(|slot| {
            if let Some(barrier) = slot.borrow_mut().take() {
                barrier.wait();
            }
        });
    }

    #[test]
    fn concurrent_same_name_uploads_keep_distinct_bytes() {
        let root = tempdir();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let paths = std::thread::scope(|scope| {
            let workers: Vec<_> = [b"first".as_slice(), b"second".as_slice()]
                .into_iter()
                .map(|bytes| {
                    let barrier = barrier.clone();
                    let root = &root;
                    scope.spawn(move || {
                        CREATE_BARRIER.with(|slot| *slot.borrow_mut() = Some(barrier));
                        let path = write_attachment(root, "s", "same.txt", bytes).unwrap();
                        (path, bytes)
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_ne!(paths[0].0, paths[1].0, "独占创建必须给每个上传不同路径");
        for (path, expected) in paths {
            assert_eq!(std::fs::read(path).unwrap(), expected);
        }
    }

    #[test]
    fn failed_write_never_deletes_or_overwrites_another_upload() {
        use std::io::Write;
        let root = tempdir();
        let dir = attachment_dir(&root, "s");
        std::fs::create_dir_all(&dir).unwrap();
        let (path, file) = unique_target(&dir, "same.txt").unwrap();
        let other = write_attachment(&root, "s", "same.txt", b"other").unwrap();
        let result = write_owned(file, path.clone(), |file| {
            file.write_all(b"partial")?;
            Err(std::io::Error::other("injected disk write failure"))
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read(other).unwrap(), b"other");
        assert_eq!(std::fs::read(path).unwrap(), b"partial");
    }

    #[cfg(unix)]
    #[test]
    fn attachment_root_symlink_to_external_directory_is_rejected() {
        use std::os::unix::fs::symlink;
        let project = tempdir();
        let external = tempdir();
        std::fs::write(external.join("sentinel"), b"untouched").unwrap();
        symlink(&*external, project.join(ATTACHMENT_DIR_NAME)).unwrap();

        let result = write_attachment(&project, "session", "upload.txt", b"payload");
        assert_eq!(
            std::fs::read(external.join("sentinel")).unwrap(),
            b"untouched"
        );
        assert!(!external.join("session").exists(), "外部目录不得出现附件");
        assert!(result.is_err(), "链接的附件根目录必须拒绝写入");
    }

    #[cfg(unix)]
    #[test]
    fn attachment_session_symlink_to_external_directory_is_rejected() {
        use std::os::unix::fs::symlink;
        let project = tempdir();
        let external = tempdir();
        std::fs::write(external.join("sentinel"), b"untouched").unwrap();
        std::fs::create_dir(project.join(ATTACHMENT_DIR_NAME)).unwrap();
        symlink(
            &*external,
            project.join(ATTACHMENT_DIR_NAME).join("session"),
        )
        .unwrap();

        let result = write_attachment(&project, "session", "upload.txt", b"payload");
        assert_eq!(
            std::fs::read(external.join("sentinel")).unwrap(),
            b"untouched"
        );
        assert!(
            !external.join("upload.txt").exists(),
            "外部目录不得出现附件"
        );
        assert!(result.is_err(), "链接的会话目录必须拒绝写入");
    }

    #[cfg(unix)]
    #[test]
    fn attachment_git_exclude_symlink_to_external_file_is_rejected() {
        use std::os::unix::fs::symlink;
        let project = tempdir();
        let external = tempdir();
        let sentinel = external.join("sentinel");
        std::fs::write(&sentinel, b"untouched\n").unwrap();
        std::fs::create_dir_all(project.join(".git/info")).unwrap();
        symlink(&sentinel, project.join(".git/info/exclude")).unwrap();

        let result = write_attachment(&project, "session", "upload.txt", b"payload");
        assert_eq!(
            std::fs::read(&sentinel).unwrap(),
            b"untouched\n",
            "外部 exclude 哨兵不得改变"
        );
        assert!(result.is_err(), "链接的 exclude 必须拒绝上传");
        assert!(
            !project.join(ATTACHMENT_DIR_NAME).exists(),
            "拒绝时不得创建附件"
        );
    }

    #[cfg(unix)]
    thread_local! {
        static AFTER_WRITE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    }

    #[cfg(unix)]
    pub(super) fn after_attachment_write() {
        AFTER_WRITE.with(|slot| {
            if let Some(callback) = slot.borrow_mut().take() {
                callback();
            }
        });
    }

    #[cfg(unix)]
    thread_local! {
        static AFTER_EXCLUDE_CHECK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
        static AFTER_INODE_CHECK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
        static BEFORE_EXCLUDE_WRITE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
        static BEFORE_EXCLUDE_OPEN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
        static BEFORE_EXCLUDE_CREATE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    }

    #[cfg(unix)]
    pub(super) fn before_exclude_create() {
        BEFORE_EXCLUDE_CREATE.with(|slot| {
            if let Some(callback) = slot.borrow_mut().take() {
                callback();
            }
        });
    }

    #[cfg(unix)]
    pub(super) fn before_exclude_open() {
        BEFORE_EXCLUDE_OPEN.with(|slot| {
            if let Some(callback) = slot.borrow_mut().take() {
                callback();
            }
        });
    }

    #[cfg(unix)]
    pub(super) fn after_exclude_check() {
        AFTER_EXCLUDE_CHECK.with(|slot| {
            if let Some(callback) = slot.borrow_mut().take() {
                callback();
            }
        });
    }

    #[cfg(unix)]
    pub(super) fn after_inode_check() {
        AFTER_INODE_CHECK.with(|slot| {
            if let Some(callback) = slot.borrow_mut().take() {
                callback();
            }
        });
    }

    #[cfg(unix)]
    pub(super) fn before_exclude_write() {
        BEFORE_EXCLUDE_WRITE.with(|slot| {
            if let Some(callback) = slot.borrow_mut().take() {
                callback();
            }
        });
    }

    #[cfg(unix)]
    fn mutate_before_exclude_write(mutate: impl FnOnce(), root: &Path) -> std::io::Result<PathBuf> {
        let (arrived_tx, arrived_rx) = std::sync::mpsc::sync_channel(0);
        let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(0);
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                BEFORE_EXCLUDE_WRITE.with(|slot| {
                    *slot.borrow_mut() = Some(Box::new(move || {
                        arrived_tx.send(()).unwrap();
                        resume_rx
                            .recv_timeout(std::time::Duration::from_secs(10))
                            .unwrap();
                    }))
                });
                write_attachment(root, "session", "upload.txt", b"payload")
            });
            arrived_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("未到达 exclude 写入屏障");
            mutate();
            resume_tx.send(()).unwrap();
            worker.join().unwrap()
        })
    }

    #[cfg(unix)]
    fn replace_after_write(
        replace: impl FnOnce() + Send + 'static,
        root: &Path,
    ) -> std::io::Result<PathBuf> {
        let arrived = std::sync::Arc::new(std::sync::Barrier::new(2));
        let resume = std::sync::Arc::new(std::sync::Barrier::new(2));
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                let arrived = arrived.clone();
                let resume = resume.clone();
                AFTER_WRITE.with(|slot| {
                    *slot.borrow_mut() = Some(Box::new(move || {
                        arrived.wait();
                        resume.wait();
                    }))
                });
                write_attachment(root, "session", "upload.txt", b"payload")
            });
            arrived.wait();
            replace();
            resume.wait();
            worker.join().unwrap()
        })
    }

    #[cfg(unix)]
    #[test]
    fn replaced_attachment_directory_never_returns_wrong_file() {
        let project = tempdir();
        let dir = project.join(".tuvis-attachments/session");
        let moved = project.join("old-session");
        let target = dir.join("upload.txt");
        let result = replace_after_write(
            move || {
                std::fs::rename(&dir, &moved).unwrap();
                std::fs::create_dir(&dir).unwrap();
                std::fs::write(&target, b"different").unwrap();
            },
            &project,
        );
        if let Ok(path) = result {
            assert_eq!(
                std::fs::read(path).unwrap(),
                b"payload",
                "成功回执不得指向替换后的文件"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn replaced_attachment_file_never_returns_wrong_file() {
        let project = tempdir();
        let target = project.join(".tuvis-attachments/session/upload.txt");
        let result = replace_after_write(
            move || {
                std::fs::rename(&target, target.with_extension("old")).unwrap();
                std::fs::write(&target, b"different").unwrap();
            },
            &project,
        );
        if let Ok(path) = result {
            assert_eq!(
                std::fs::read(path).unwrap(),
                b"payload",
                "成功回执不得指向替换后的文件"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn exclude_hard_link_does_not_modify_external_sentinel() {
        let project = tempdir();
        let external = tempdir();
        let sentinel = external.join("sentinel");
        std::fs::write(&sentinel, b"untouched\n").unwrap();
        std::fs::create_dir_all(project.join(".git/info")).unwrap();
        std::fs::hard_link(&sentinel, project.join(".git/info/exclude")).unwrap();
        let _ = write_attachment(&project, "session", "upload.txt", b"payload");
        assert_eq!(std::fs::read(sentinel).unwrap(), b"untouched\n");
    }

    #[cfg(unix)]
    #[test]
    fn session_dot_suffix_does_not_follow_symlink() {
        assert_session_suffix_does_not_follow_symlink("session/.");
    }

    #[cfg(unix)]
    #[test]
    fn session_trailing_slash_does_not_follow_symlink() {
        assert_session_suffix_does_not_follow_symlink("session/");
    }

    #[cfg(unix)]
    fn assert_session_suffix_does_not_follow_symlink(session: &str) {
        use std::os::unix::fs::symlink;
        let project = tempdir();
        let external = tempdir();
        std::fs::create_dir(project.join(".tuvis-attachments")).unwrap();
        symlink(&*external, project.join(".tuvis-attachments/session")).unwrap();
        let result = write_attachment(&project, session, "upload.txt", b"payload");
        assert!(!external.join("upload.txt").exists(), "不得向链接目标写入");
        assert!(result.is_err(), "带分隔符的会话名必须拒绝");
    }

    #[cfg(unix)]
    #[test]
    fn exclude_read_failure_does_not_report_failure_after_attachment_written() {
        let project = tempdir();
        std::fs::create_dir_all(project.join(".git/info")).unwrap();
        std::fs::write(project.join(".git/info/exclude"), [0xff]).unwrap();
        let result = write_attachment(&project, "session", "upload.txt", b"payload");
        let path = result.expect("普通 exclude 维护失败不得丢失已写附件的成功回执");
        assert_eq!(std::fs::read(path).unwrap(), b"payload");
    }

    #[cfg(unix)]
    #[test]
    fn exclude_fifo_does_not_block_upload() {
        const CHILD: &str = "TUVIS_TEST_ATTACHMENT_FIFO_CHILD";
        if let Some(project) = std::env::var_os(CHILD) {
            use std::os::unix::ffi::OsStrExt;
            let project = PathBuf::from(project);
            std::fs::create_dir_all(project.join(".git/info")).unwrap();
            let path = project.join(".git/info/exclude");
            let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            let _ = write_attachment(&project, "session", "upload.txt", b"payload");
            return;
        }
        let project = tempdir();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "remote::attachments::tests::exclude_fifo_does_not_block_upload",
                "--nocapture",
            ])
            .env(CHILD, project.as_os_str())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "FIFO 子进程失败: {status}");
                break;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("exclude FIFO 导致上传阻塞");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn inode_checked_then_replaced_directory_must_not_return_wrong_file() {
        let project = tempdir();
        let dir = project.join(".tuvis-attachments/session");
        let moved = project.join("moved-session");
        AFTER_INODE_CHECK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                std::fs::rename(&dir, &moved).unwrap();
                std::fs::create_dir(&dir).unwrap();
                std::fs::write(dir.join("upload.txt"), b"different").unwrap();
            }))
        });
        let result = write_attachment(&project, "session", "upload.txt", b"payload");
        if let Ok(path) = result {
            assert_eq!(
                std::fs::read(path).unwrap(),
                b"payload",
                "成功回执指向了替换后的文件"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn inode_checked_then_replaced_file_must_not_return_wrong_file() {
        let project = tempdir();
        let target = project.join(".tuvis-attachments/session/upload.txt");
        AFTER_INODE_CHECK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                std::fs::rename(&target, target.with_extension("old")).unwrap();
                std::fs::write(&target, b"different").unwrap();
            }))
        });
        let result = write_attachment(&project, "session", "upload.txt", b"payload");
        if let Ok(path) = result {
            assert_eq!(
                std::fs::read(path).unwrap(),
                b"payload",
                "成功回执指向了替换后的文件"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn exclude_new_hard_link_after_check_leaves_external_file_unchanged() {
        let project = tempdir();
        let external = tempdir();
        std::fs::create_dir_all(project.join(".git/info")).unwrap();
        let exclude = project.join(".git/info/exclude");
        let sentinel = external.join("sentinel");
        std::fs::write(&exclude, b"original\n").unwrap();
        let linked = sentinel.clone();
        AFTER_EXCLUDE_CHECK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                std::fs::hard_link(exclude, linked).unwrap();
            }))
        });
        let _ = write_attachment(&project, "session", "upload.txt", b"payload");
        assert_eq!(std::fs::read(sentinel).unwrap(), b"original\n");
    }

    #[cfg(unix)]
    #[test]
    fn exclude_moved_directory_after_check_does_not_modify_external_file() {
        let project = tempdir();
        let external = tempdir();
        std::fs::create_dir_all(project.join(".git/info")).unwrap();
        let info = project.join(".git/info");
        let moved = external.join("info");
        std::fs::write(info.join("exclude"), b"original\n").unwrap();
        let moved_hook = moved.clone();
        AFTER_EXCLUDE_CHECK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                std::fs::rename(info, moved_hook).unwrap();
            }))
        });
        let _ = write_attachment(&project, "session", "upload.txt", b"payload");
        assert_eq!(std::fs::read(moved.join("exclude")).unwrap(), b"original\n");
    }

    #[cfg(unix)]
    #[test]
    fn late_exclude_identity_change_returns_saved_attachment_with_warning() {
        let project = tempdir();
        let external = tempdir();
        let exclude = project.join(".git/info/exclude");
        let sentinel = external.join("sentinel");
        std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        std::fs::write(&exclude, b"original\n").unwrap();
        let link = sentinel.clone();
        let saved = project.join(".tuvis-attachments/session/upload.txt");
        AFTER_INODE_CHECK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                assert_eq!(std::fs::read(&saved).unwrap(), b"payload");
                std::fs::hard_link(exclude, link).unwrap();
            }));
        });
        let (path, warning) =
            write_attachment_with_exclude_warning(&project, "session", "upload.txt", b"payload")
                .expect("附件已落盘，排除文件复核失败应返回成功和警告");
        assert_eq!(std::fs::read(path).unwrap(), b"payload");
        assert!(warning.is_some());
        assert_eq!(std::fs::read(sentinel).unwrap(), b"original\n");
    }

    #[cfg(unix)]
    #[test]
    fn exclude_hard_link_created_during_attachment_write_leaves_external_file_unchanged() {
        let project = tempdir();
        let external = tempdir();
        let exclude = project.join(".git/info/exclude");
        let sentinel = external.join("sentinel");
        std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        std::fs::write(&exclude, b"original\n").unwrap();
        let linked = sentinel.clone();
        let _ = replace_after_write(
            move || {
                std::fs::hard_link(exclude, linked).unwrap();
            },
            &project,
        );
        assert_eq!(
            std::fs::read(sentinel).unwrap(),
            b"original\n",
            "新增外部硬链接后不得写入 exclude"
        );
    }

    #[cfg(unix)]
    #[test]
    fn exclude_info_moved_during_attachment_write_leaves_external_file_unchanged() {
        let project = tempdir();
        let external = tempdir();
        let info = project.join(".git/info");
        let moved = external.join("info");
        std::fs::create_dir_all(&info).unwrap();
        std::fs::write(info.join("exclude"), b"original\n").unwrap();
        let destination = moved.clone();
        let _ = replace_after_write(
            move || {
                std::fs::rename(info, destination).unwrap();
            },
            &project,
        );
        assert_eq!(
            std::fs::read(moved.join("exclude")).unwrap(),
            b"original\n",
            "移出项目后的 exclude 不得被修改"
        );
    }

    #[cfg(unix)]
    #[test]
    fn exclude_final_check_then_hard_link_preserves_external_content() {
        let project = tempdir();
        let external = tempdir();
        let exclude = project.join(".git/info/exclude");
        let sentinel = external.join("sentinel");
        std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        std::fs::write(&exclude, b"original\n").unwrap();
        let _ = mutate_before_exclude_write(
            || {
                std::fs::hard_link(&exclude, &sentinel).unwrap();
            },
            &project,
        );
        assert_eq!(
            std::fs::read(&sentinel).unwrap(),
            b"original\n",
            "最终检查后新增的外部硬链接不得被修改"
        );
    }

    #[cfg(unix)]
    #[test]
    fn exclude_final_check_then_info_move_preserves_external_content() {
        let project = tempdir();
        let external = tempdir();
        let info = project.join(".git/info");
        let moved = external.join("info");
        std::fs::create_dir_all(&info).unwrap();
        std::fs::write(info.join("exclude"), b"original\n").unwrap();
        let _ = mutate_before_exclude_write(
            || {
                std::fs::rename(&info, &moved).unwrap();
            },
            &project,
        );
        assert_eq!(
            std::fs::read(moved.join("exclude")).unwrap(),
            b"original\n",
            "最终检查后移出项目的 exclude 不得被修改"
        );
    }

    #[cfg(unix)]
    #[test]
    fn inaccessible_git_directory_does_not_block_attachment_upload() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let project = tempdir();
        let git = project.join(".git");
        std::fs::create_dir(&git).unwrap();
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = write_attachment(&project, "session", "upload.txt", b"payload");
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = result.expect(".git 权限不足不得阻止附件上传");
        assert_eq!(std::fs::read(path).unwrap(), b"payload");
    }

    #[cfg(unix)]
    #[test]
    fn nul_session_id_has_no_filesystem_side_effects() {
        let project = tempdir();
        std::fs::create_dir(project.join(".git")).unwrap();
        let error =
            write_attachment(&project, "bad\0session", "upload.txt", b"payload").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(!project.join(".git/info").exists());
        assert!(!project.join(".tuvis-attachments").exists());
    }

    #[cfg(unix)]
    #[test]
    fn inaccessible_git_info_is_best_effort_for_attachment_upload() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let project = tempdir();
        let info = project.join(".git/info");
        std::fs::create_dir_all(&info).unwrap();
        std::fs::set_permissions(&info, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = write_attachment(&project, "session", "upload.txt", b"payload");
        std::fs::set_permissions(&info, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = result.expect("普通 Git 目录权限不足不得阻止附件上传");
        assert_eq!(std::fs::read(path).unwrap(), b"payload");
        assert!(
            !info.join("exclude").exists(),
            "权限不足时规则未自动添加，不能宣称 Git 零污染"
        );
        assert!(
            pending_git_exclude(&project, GIT_EXCLUDE_LINE).is_some(),
            "必须明确显示排除规则未自动添加"
        );
    }

    #[cfg(unix)]
    #[test]
    fn exclude_existing_regular_file_gets_missing_rule_without_competition() {
        let project = tempdir();
        let exclude = project.join(".git/info/exclude");
        std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        std::fs::write(&exclude, b"user-pattern\n").unwrap();

        let path = write_attachment(&project, "session", "upload.txt", b"payload").unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"payload");
        assert_eq!(
            std::fs::read(&exclude).unwrap(),
            b"user-pattern\n.tuvis-attachments/\n",
            "普通文件无竞争时应保留用户规则并添加附件排除规则"
        );
        assert!(
            pending_git_exclude(&project, GIT_EXCLUDE_LINE).is_none(),
            "规则已添加后不应继续提示待处理"
        );
    }

    #[cfg(unix)]
    #[test]
    fn exclude_existing_rule_remains_byte_identical() {
        use std::os::unix::fs::MetadataExt;
        let project = tempdir();
        let exclude = project.join(".git/info/exclude");
        std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        let original = b"# user\n.tuvis-attachments/\ncustom\n";
        std::fs::write(&exclude, original).unwrap();
        let before = std::fs::metadata(&exclude).unwrap();
        write_attachment(&project, "session", "upload.txt", b"payload").unwrap();
        let after = std::fs::metadata(&exclude).unwrap();
        assert_eq!(std::fs::read(&exclude).unwrap(), original);
        assert_eq!(
            (after.dev(), after.ino(), after.mtime(), after.mtime_nsec()),
            (
                before.dev(),
                before.ino(),
                before.mtime(),
                before.mtime_nsec()
            )
        );
    }

    #[cfg(unix)]
    #[test]
    fn exclude_concurrent_user_edit_is_preserved_without_adding_rule() {
        let project = tempdir();
        let exclude = project.join(".git/info/exclude");
        std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        std::fs::write(&exclude, b"original\n").unwrap();
        let result = mutate_before_exclude_write(
            || {
                std::fs::write(&exclude, b"user concurrent edit\n").unwrap();
            },
            &project,
        );
        assert!(result.is_ok(), "排除规则维护失败不应丢失附件回执");
        assert_eq!(
            std::fs::read(&exclude).unwrap(),
            b"user concurrent edit\n",
            "不得覆盖或追加到并发编辑"
        );
        assert!(
            pending_git_exclude(&project, GIT_EXCLUDE_LINE).is_some(),
            "必须明确显示排除规则未自动添加"
        );
    }

    #[cfg(unix)]
    #[test]
    fn exclude_competing_creation_preserves_user_file() {
        let project = tempdir();
        let exclude = project.join(".git/info/exclude");
        std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        let competing = exclude.clone();
        BEFORE_EXCLUDE_CREATE.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                std::fs::write(competing, b"user created file\n").unwrap();
            }))
        });
        let result = write_attachment(&project, "session", "upload.txt", b"payload");
        let path = result.expect("创建竞争不得阻止附件上传");
        assert_eq!(std::fs::read(path).unwrap(), b"payload");
        assert_eq!(
            std::fs::read(&exclude).unwrap(),
            b"user created file\n",
            "竞争创建的用户文件不得覆盖或追加"
        );
        assert!(
            pending_git_exclude(&project, GIT_EXCLUDE_LINE).is_some(),
            "必须明确显示排除规则未自动添加"
        );
    }

    #[cfg(unix)]
    #[test]
    fn exclude_missing_is_created_with_rule() {
        let project = tempdir();
        std::fs::create_dir_all(project.join(".git/info")).unwrap();
        write_attachment(&project, "session", "upload.txt", b"payload").unwrap();
        assert_eq!(
            std::fs::read(project.join(".git/info/exclude")).unwrap(),
            b".tuvis-attachments/\n"
        );
        assert!(pending_git_exclude(&project, GIT_EXCLUDE_LINE).is_none());
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
