//! 新建会话路径校验及正式提交时的本机目录准备。
//! 黑名单 = 两张表：
//! ① remote::files::SENSITIVE_DIRS（读侧凭据表）——create 域**策略扩展**为全局段匹配
//!    （读侧原语义是「仅主目录内」，见 files.rs 表注释：「黑名单仅作用于**用户主目录
//!    范围内**（凭据高价值区）；主目录之外不设路径级防线」；扩展理由：起 CLI 的目录
//!    与读文件同等敏感——会话工作目录落在凭据目录里同样不可接受）；
//! ② CREATE_SYSTEM_DIRS（create 专属）——系统关键目录，读侧从不需要（低敏）但建会话必须拦。
//!
//! 失败码词表（wire 值，端点层直出）：empty / non_ascii_path / not_local_volume /
//! bad_windows_form / not_absolute / blacklisted。

/// 路径拒绝原因：`code` 为 wire 失败码（移动端按码出文案），`message` 为诊断备注。
#[derive(Debug)]
pub struct PathReject {
    pub code: &'static str,
    pub message: String,
}

/// 系统关键目录（create 专属；windows = 段匹配：大小写折叠 + 分隔符双态归一）。
const CREATE_SYSTEM_DIRS_WIN: &[&str] = &[
    "windows",
    "program files",
    "program files (x86)",
    "programdata",
];

/// 系统关键目录（create 专属；macos = 前缀匹配，须卡段边界——`/usr` 命中 `/usr/bin`
/// 而不误伤 `/Users/...`）。
const CREATE_SYSTEM_DIRS_MAC: &[&str] = &[
    "/system", "/library", "/private", "/bin", "/sbin", "/etc", "/usr",
];

/// 新建会话路径校验（纯函数，只判不建；递归创建在状态机校验段）。
/// `platform` = "windows" | "macos"（仅识别 "windows"，其余值一律走 POSIX 形态判别
/// ——Windows 形态会以 `not_absolute` 拒、不会放行，fail-closed）。
/// **trim 契约（评审 I1）**：判定基于 `trim()` 后形态；调用方（C6 状态机）必须以
/// **同一 trim 后的路径**执行 `create_dir_all` 与起窗 cwd——不得拿原始串落盘，
/// 否则尾随空白会在盘上建出带空格的目录。
pub fn validate(path: &str, platform: &str) -> Result<(), PathReject> {
    let p = path.trim();
    if p.is_empty() {
        return Err(rej("empty", "路径为空"));
    }
    if !p.is_ascii() {
        return Err(rej("non_ascii_path", "v1 路径限纯 ASCII（CJK 路径留后批）"));
    }
    // UNC / 网络路径先拦（`\\` 与 `//` 双形态，平台无关；spec：v1 仅本地卷）
    if p.starts_with(r"\\") || p.starts_with("//") {
        return Err(rej("not_local_volume", "v1 仅本地卷，UNC/网络路径不入本批"));
    }
    if platform == "windows" {
        // 三态判别（验收 SSOT：测试逐条驱动）：
        //   `/` 开头 = POSIX 形态跑在 windows → bad_windows_form；
        //   `X:\` / `X:/` 盘符形态 → 过；
        //   其余（无盘符的相对/裸名）→ not_absolute。
        if p.starts_with('/') {
            return Err(rej(
                "bad_windows_form",
                "POSIX 形态路径不能作 Windows 绝对路径",
            ));
        }
        let b = p.as_bytes();
        let drive = b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && (b[2] == b'\\' || b[2] == b'/');
        if !drive {
            return Err(rej("not_absolute", "Windows 路径须为 X:\\ 盘符绝对形态"));
        }
    } else if !p.starts_with('/') {
        return Err(rej("not_absolute", "macOS 路径须为 / 开头绝对路径"));
    }
    if let Some(hit) = hits_blacklist(p, platform) {
        return Err(rej(
            "blacklisted",
            &format!(
                "路径命中危险目录黑名单（命中段「{}」，{}）",
                hit.segment, hit.table
            ),
        ));
    }
    Ok(())
}

/// 原生新建固定首次核验的规范目录及文件系统身份。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CreateDirectory {
    pub path: String,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl CreateDirectory {
    pub(crate) fn still_current(&self) -> bool {
        validate_existing(&self.path).is_ok_and(|current| current == *self)
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn stat_key(&self) -> String {
        format!("{}:{}", self.device, self.inode)
    }
}

/// 原生新建的预览与回读只核对既有目录。原始路径和真实落点均过 D21 校验链。
pub(crate) fn validate_existing(path: &str) -> Result<CreateDirectory, PathReject> {
    validate(path, std::env::consts::OS)?;
    let canonical = std::fs::canonicalize(path.trim()).map_err(|_| unresolved())?;
    if !canonical.is_dir() {
        return Err(unresolved());
    }
    let directory = canonical
        .to_str()
        .ok_or_else(|| rej("non_ascii_path", "ASCII project paths are required"))?;
    validate(directory, std::env::consts::OS)?;
    if directory != directory.trim() {
        return Err(unresolved());
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let path =
            std::ffi::CString::new(canonical.as_os_str().as_bytes()).map_err(|_| unresolved())?;
        let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: path 以 NUL 结尾，stat 指向可写的 statfs；成功时内核完整初始化。
        if unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
            return Err(unresolved());
        }
        if unsafe { stat.assume_init() }.f_flags & libc::MNT_LOCAL as u32 == 0 {
            return Err(rej("not_local_volume", "Only local volumes are supported"));
        }
    }
    #[cfg(unix)]
    let (device, inode) = {
        use std::os::unix::fs::MetadataExt;
        let metadata = canonical.metadata().map_err(|_| unresolved())?;
        if !metadata.is_dir() {
            return Err(unresolved());
        }
        (metadata.dev(), metadata.ino())
    };
    Ok(CreateDirectory {
        path: directory.to_owned(),
        #[cfg(unix)]
        device,
        #[cfg(unix)]
        inode,
    })
}

/// 仅在正式提交中准备合法的缺失目录；不沿符号链接创建新目录。
pub(crate) fn prepare_native_create_directory(path: &str) -> Result<CreateDirectory, PathReject> {
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CString;
        use std::fs::{self, File};
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
        use std::path::{Component, Path, PathBuf};

        let existing_error = match validate_existing(path) {
            Ok(existing) => return Ok(existing),
            Err(error) => error,
        };
        validate(path, std::env::consts::OS)?;
        let requested = Path::new(path.trim());
        // 已有目标（含链接）必须保留原有 D21 拒绝结果，不进入建目录分支。
        if !fs::symlink_metadata(requested)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        {
            return Err(existing_error);
        }
        // 在第一次落盘前确认整个路径不含上级跳转或 NUL。
        let components = requested
            .components()
            .skip(1)
            .map(|part| match part {
                Component::Normal(name) => CString::new(name.as_bytes()).map_err(|_| unresolved()),
                _ => Err(unresolved()),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut parent = File::open("/").map_err(|_| unresolved())?;
        let mut resolved = PathBuf::from("/");
        for part in components {
            let open = || {
                // SAFETY: parent fd 和 NUL 结尾的单个组件在调用期间有效。
                let fd = unsafe {
                    libc::openat(
                        parent.as_raw_fd(),
                        part.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if fd < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // SAFETY: fd 成功打开，所有权交给 File。
                Ok(unsafe { File::from_raw_fd(fd) })
            };
            let next = match open() {
                Ok(directory) => directory,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    let visible = validate_existing(resolved.to_str().ok_or_else(unresolved)?)?;
                    let opened = parent.metadata().map_err(|_| unresolved())?;
                    if visible.device != opened.dev() || visible.inode != opened.ino() {
                        return Err(unresolved());
                    }
                    // SAFETY: mkdirat 相对已经复核的父目录 fd；part 不是路径或链接。
                    let rc = unsafe { libc::mkdirat(parent.as_raw_fd(), part.as_ptr(), 0o700) };
                    if rc != 0
                        && std::io::Error::last_os_error().kind()
                            != std::io::ErrorKind::AlreadyExists
                    {
                        return Err(unresolved());
                    }
                    open().map_err(|_| unresolved())?
                }
                Err(_) => return Err(unresolved()),
            };
            let name = std::ffi::OsStr::from_bytes(part.as_bytes());
            resolved.push(name);
            parent = next;
        }
        if fs::canonicalize(requested).map_err(|_| unresolved())? != resolved {
            return Err(unresolved());
        }
        let result = validate_existing(path)?;
        let opened = parent.metadata().map_err(|_| unresolved())?;
        if result.device != opened.dev() || result.inode != opened.ino() {
            return Err(unresolved());
        }
        Ok(result)
    }
    #[cfg(not(target_os = "macos"))]
    validate_existing(path)
}

fn unresolved() -> PathReject {
    rej(
        "directory_unresolved",
        "The canonical project directory is unavailable",
    )
}

/// 黑名单命中详情（评审修复③）：命中的段名 + 所属表（凭据表/系统目录表）——
/// 用于 PathReject.message 回显，排障时直接定位命中来源。
struct BlacklistHit {
    segment: String,
    table: &'static str,
}

/// 黑名单双表命中判定（纯函数）：① 凭据表（SENSITIVE_DIRS）全局段匹配；② create
/// 专属系统目录表（按平台语义）。归一 = 小写折叠 + `/`→`\`，段匹配与平台无关。
/// 命中 → Some((命中段, 所属表))；未命中 → None。
fn hits_blacklist(p: &str, platform: &str) -> Option<BlacklistHit> {
    let seg_norm = p.to_ascii_lowercase().replace('/', "\\");
    for d in crate::remote::files::SENSITIVE_DIRS {
        let d = d.to_ascii_lowercase().replace('/', "\\");
        if seg_contains(&seg_norm, &d) {
            return Some(BlacklistHit {
                segment: d,
                table: "凭据表",
            });
        }
    }
    if platform == "windows" {
        for d in CREATE_SYSTEM_DIRS_WIN {
            let d = d.to_ascii_lowercase();
            if seg_contains(&seg_norm, &d) {
                return Some(BlacklistHit {
                    segment: d,
                    table: "系统目录表",
                });
            }
        }
    } else {
        let posix = seg_norm.replace('\\', "/");
        for pfx in CREATE_SYSTEM_DIRS_MAC {
            if posix == *pfx || posix.starts_with(&format!("{pfx}/")) {
                return Some(BlacklistHit {
                    segment: (*pfx).to_string(),
                    table: "系统目录表",
                });
            }
        }
    }
    None
}

/// 段精确命中：`\d\` 出现或 `\d` 收尾（`.ssh2` 这类前缀相似目录不误伤）。
fn seg_contains(haystack_norm: &str, needle: &str) -> bool {
    haystack_norm.contains(&format!("\\{needle}\\"))
        || haystack_norm.ends_with(&format!("\\{needle}"))
}

fn rej(code: &'static str, msg: &str) -> PathReject {
    PathReject {
        code,
        message: msg.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_empty_relative_nonascii_unc() {
        assert_eq!(validate("", "windows").err().unwrap().code, "empty");
        assert_eq!(
            validate(r"relative\path", "windows").err().unwrap().code,
            "not_absolute"
        );
        assert_eq!(
            validate(r"E:\项目\demo", "windows").err().unwrap().code,
            "non_ascii_path"
        );
        assert_eq!(
            validate("/项目/demo", "macos").err().unwrap().code,
            "non_ascii_path"
        );
        assert_eq!(
            validate(r"\\server\share\p", "windows").err().unwrap().code,
            "not_local_volume"
        );
        assert_eq!(
            validate("//server/share", "macos").err().unwrap().code,
            "not_local_volume"
        );
    }
    #[test]
    fn windows_requires_drive_form_macos_requires_slash() {
        assert_eq!(
            validate(r"/tmp/x", "windows").err().unwrap().code,
            "bad_windows_form"
        );
        assert!(validate(r"C:\Users\u\proj", "windows").is_ok());
        assert!(validate("/Users/u/proj", "macos").is_ok());
    }
    #[test]
    fn credential_blacklist_hits_globally() {
        // SENSITIVE_DIRS（读侧表）在 create 域升级为全局段匹配（策略扩展，文件头声明）
        assert_eq!(
            validate(r"D:\keys\.ssh\k", "windows").err().unwrap().code,
            "blacklisted"
        );
        assert_eq!(
            validate("/Users/u/.claude/x", "macos").err().unwrap().code,
            "blacklisted"
        );
    }
    #[test]
    fn system_dirs_blocked_via_create_table() {
        // 系统关键目录：SENSITIVE_DIRS 不含，须由 CREATE_SYSTEM_DIRS 拦
        assert_eq!(
            validate(r"C:\Windows\System32\x", "windows")
                .err()
                .unwrap()
                .code,
            "blacklisted"
        );
        assert_eq!(
            validate(r"C:\Program Files\app", "windows")
                .err()
                .unwrap()
                .code,
            "blacklisted"
        );
        assert_eq!(
            validate("/System/Volumes", "macos").err().unwrap().code,
            "blacklisted"
        );
        assert_eq!(
            validate("/usr/bin", "macos").err().unwrap().code,
            "blacklisted"
        );
        // 普通路径不误伤（用户目录名撞系统段的情况不在 v1 处理面，如实登记）
        assert!(validate(r"D:\work\proj", "windows").is_ok());
        assert!(validate("/Users/u/work/proj", "macos").is_ok());
    }
    #[test]
    fn segment_boundary_and_case_fold_locks() {
        // 敏感目录作为路径**末段**（ends_with 半支）与 mac 前缀**精确相等**支（评审 M1）
        assert_eq!(
            validate(r"C:\Users\u\.ssh", "windows").err().unwrap().code,
            "blacklisted"
        );
        assert_eq!(
            validate(r"C:\Windows", "windows").err().unwrap().code,
            "blacklisted"
        );
        assert_eq!(validate("/etc", "macos").err().unwrap().code, "blacklisted");
        // 前缀相似目录不误伤（模块 doc 明写承诺，create 侧补锁，评审 M2）
        assert!(validate(r"D:\work\.ssh2\k", "windows").is_ok());
        // 小写盘符合法（评审 M3）
        assert!(validate(r"c:\Users\u\proj", "windows").is_ok());
        // 大小写折叠：凭据段大小写变体照拦
        assert_eq!(
            validate(r"D:\KEYS\.SSH\id", "windows").err().unwrap().code,
            "blacklisted"
        );
        // macos 相对路径 → not_absolute
        assert_eq!(
            validate("relative/path", "macos").err().unwrap().code,
            "not_absolute"
        );
        // trim 契约（评审 I1）：判定基于 trim 后形态——首尾空白不改变判定结果
        assert!(validate("  /Users/u/proj  ", "macos").is_ok());
    }
    #[test]
    fn blacklisted_message_echoes_segment_and_table() {
        // 评审修复③：message 回显命中段名与所属表——凭据表 / 系统目录表两源各锁一例
        let cred = validate(r"D:\keys\.ssh\k", "windows").err().unwrap();
        assert_eq!(cred.code, "blacklisted");
        assert!(
            cred.message.contains(".ssh") && cred.message.contains("凭据表"),
            "凭据表命中回显：{}",
            cred.message
        );
        let sys = validate(r"C:\Windows\System32\x", "windows").err().unwrap();
        assert_eq!(sys.code, "blacklisted");
        assert!(
            sys.message.contains("windows") && sys.message.contains("系统目录表"),
            "系统目录表命中回显：{}",
            sys.message
        );
        let mac = validate("/usr/bin", "macos").err().unwrap();
        assert!(
            mac.message.contains("/usr") && mac.message.contains("系统目录表"),
            "mac 前缀命中回显：{}",
            mac.message
        );
    }
}
