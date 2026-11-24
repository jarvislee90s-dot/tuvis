//! 同一物理 profile 的管理作业跨进程互斥。

use super::error::{codes, DshError};
use fs2::FileExt;
use std::path::Path;

#[cfg(target_os = "macos")]
fn require_safe_acl(file: &std::fs::File, strict: bool) -> Result<(), DshError> {
    use std::ffi::c_void;
    use std::os::fd::AsRawFd;
    #[link(name = "System")]
    unsafe extern "C" {
        fn acl_get_fd_np(fd: libc::c_int, kind: libc::c_uint) -> *mut c_void;
        fn acl_valid(acl: *mut c_void) -> libc::c_int;
        fn acl_get_entry(
            acl: *mut c_void,
            which: libc::c_int,
            entry: *mut *mut c_void,
        ) -> libc::c_int;
        fn acl_get_tag_type(entry: *mut c_void, tag: *mut libc::c_int) -> libc::c_int;
        fn acl_get_permset(entry: *mut c_void, permissions: *mut *mut c_void) -> libc::c_int;
        fn acl_get_perm_np(permissions: *mut c_void, permission: libc::c_int) -> libc::c_int;
        fn acl_get_qualifier(entry: *mut c_void) -> *mut c_void;
        fn mbr_uuid_to_id(
            uuid: *const u8,
            id: *mut libc::id_t,
            kind: *mut libc::c_int,
        ) -> libc::c_int;
        fn acl_free(object: *mut c_void) -> libc::c_int;
    }
    let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), 0x100) };
    if acl.is_null() {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ENOENT) {
            Ok(())
        } else {
            Err(DshError::new(
                codes::STATE_IO,
                format!("read ancestor ACL: {error}"),
            ))
        };
    }
    let mut safe = unsafe { acl_valid(acl) } == 0;
    let mut which = 0;
    while safe {
        let mut entry = std::ptr::null_mut();
        let result = unsafe { acl_get_entry(acl, which, &mut entry) };
        if result == -1 {
            safe = std::io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL);
            break;
        }
        if result != 0 || entry.is_null() {
            safe = false;
            break;
        }
        let mut tag = 0;
        if unsafe { acl_get_tag_type(entry, &mut tag) } != 0 {
            safe = false;
            break;
        }
        if tag == 1 {
            let mut permissions = std::ptr::null_mut();
            if unsafe { acl_get_permset(entry, &mut permissions) } != 0 || permissions.is_null() {
                safe = false;
                break;
            }
            let mut grants_write = strict;
            for permission in [
                1 << 2,
                1 << 4,
                1 << 5,
                1 << 6,
                1 << 8,
                1 << 10,
                1 << 12,
                1 << 13,
            ] {
                let value = unsafe { acl_get_perm_np(permissions, permission) };
                if value < 0 {
                    safe = false;
                    break;
                }
                grants_write |= value == 1;
            }
            if safe && grants_write {
                let euid = unsafe { libc::geteuid() };
                let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
                let mut result = std::ptr::null_mut();
                let mut buffer = vec![0u8; 16384];
                if unsafe {
                    libc::getpwuid_r(
                        euid,
                        &mut pwd,
                        buffer.as_mut_ptr().cast(),
                        buffer.len(),
                        &mut result,
                    )
                } != 0
                    || result.is_null()
                {
                    safe = false;
                    break;
                }
                let qualifier = unsafe { acl_get_qualifier(entry) };
                if qualifier.is_null() {
                    safe = false;
                    break;
                }
                let mut uuid = [0u8; 16];
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        qualifier.cast::<u8>(),
                        uuid.as_mut_ptr(),
                        uuid.len(),
                    )
                };
                let freed = unsafe { acl_free(qualifier) } == 0;
                let mut id: libc::id_t = 0;
                let mut kind = -1;
                let status = unsafe { mbr_uuid_to_id(uuid.as_ptr(), &mut id, &mut kind) };
                if !freed || status != 0 || kind != 0 || id != euid {
                    safe = false;
                }
            }
        } else if tag != 2 {
            safe = false;
        }
        which = -1;
    }
    if unsafe { acl_free(acl) } != 0 {
        safe = false;
    }
    if safe {
        Ok(())
    } else {
        Err(DshError::new(
            codes::STATE_IO,
            "profile ancestor ACL may permit replacement",
        ))
    }
}

#[cfg(target_os = "macos")]
fn require_safe_ancestor_acl(file: &std::fs::File) -> Result<(), DshError> {
    require_safe_acl(file, false)
}
#[cfg(target_os = "macos")]
fn require_empty_acl(file: &std::fs::File) -> Result<(), DshError> {
    require_safe_acl(file, true)
}
#[cfg(all(unix, not(target_os = "macos")))]
fn require_safe_ancestor_acl(_file: &std::fs::File) -> Result<(), DshError> {
    Ok(())
}
#[cfg(all(unix, not(target_os = "macos")))]
fn require_empty_acl(_file: &std::fs::File) -> Result<(), DshError> {
    Ok(())
}

#[cfg(unix)]
pub(super) struct ProfileLock {
    original: std::path::PathBuf,
    directory: std::fs::File,
    lock_dir: std::fs::File,
    lock: std::fs::File,
}

#[cfg(unix)]
fn check_ancestors(path: &Path) -> Result<(), DshError> {
    use std::collections::VecDeque;
    use std::ffi::OsString;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Component, PathBuf};
    #[derive(Debug)]
    enum Part {
        Root,
        Parent,
        Name(OsString),
    }
    fn parts(path: &Path) -> VecDeque<Part> {
        path.components()
            .filter_map(|part| match part {
                Component::RootDir => Some(Part::Root),
                Component::ParentDir => Some(Part::Parent),
                Component::Normal(name) => Some(Part::Name(name.to_owned())),
                Component::CurDir | Component::Prefix(_) => None,
            })
            .collect()
    }
    if !path.is_absolute() {
        return Err(DshError::new(
            codes::STATE_IO,
            "profile path is not absolute",
        ));
    }
    let uid = unsafe { libc::geteuid() };
    let mut current = PathBuf::from("/");
    let mut pending = parts(path);
    let mut hops = 0usize;
    while let Some(part) = pending.pop_front() {
        match part {
            Part::Root => current = PathBuf::from("/"),
            Part::Parent => {
                let directory = std::fs::File::open(&current).map_err(|error| {
                    DshError::new(
                        codes::STATE_IO,
                        format!("open traversed directory: {error}"),
                    )
                })?;
                let metadata = directory.metadata().map_err(|error| {
                    DshError::new(
                        codes::STATE_IO,
                        format!("stat traversed directory: {error}"),
                    )
                })?;
                if (metadata.uid() != 0 && metadata.uid() != uid)
                    || (metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0)
                {
                    return Err(DshError::new(
                        codes::STATE_IO,
                        "traversed profile directory permits replacement",
                    ));
                }
                require_safe_ancestor_acl(&directory)?;
                current.pop();
                if current.as_os_str().is_empty() {
                    current.push("/");
                }
            }
            Part::Name(name) => {
                let parent = std::fs::File::open(&current).map_err(|error| {
                    DshError::new(codes::STATE_IO, format!("open profile ancestor: {error}"))
                })?;
                let parent_meta = parent.metadata().map_err(|error| {
                    DshError::new(codes::STATE_IO, format!("stat profile ancestor: {error}"))
                })?;
                if (parent_meta.uid() != 0 && parent_meta.uid() != uid)
                    || (parent_meta.mode() & 0o022 != 0 && parent_meta.mode() & 0o1000 == 0)
                {
                    return Err(DshError::new(
                        codes::STATE_IO,
                        "profile ancestor permits replacement",
                    ));
                }
                require_safe_ancestor_acl(&parent)?;
                let candidate = current.join(name);
                let meta = std::fs::symlink_metadata(&candidate).map_err(|error| {
                    DshError::new(
                        codes::STATE_IO,
                        format!("stat profile path component: {error}"),
                    )
                })?;
                if meta.file_type().is_symlink() {
                    if meta.uid() != 0 && meta.uid() != uid && parent_meta.mode() & 0o022 != 0 {
                        return Err(DshError::new(
                            codes::STATE_IO,
                            "profile symlink can be replaced by another user",
                        ));
                    }
                    hops += 1;
                    if hops > 40 {
                        return Err(DshError::new(
                            codes::STATE_IO,
                            "profile symlink limit exceeded",
                        ));
                    }
                    let target = std::fs::read_link(&candidate).map_err(|error| {
                        DshError::new(codes::STATE_IO, format!("read profile symlink: {error}"))
                    })?;
                    let mut expanded = parts(&target);
                    expanded.append(&mut pending);
                    pending = expanded;
                } else {
                    current = candidate;
                }
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
impl ProfileLock {
    pub(super) fn validate_target(&self, logical_profile: &Path) -> Result<(), DshError> {
        use std::os::unix::fs::MetadataExt;
        self.validate()?;
        check_ancestors(logical_profile)?;
        let actual = std::fs::metadata(logical_profile).map_err(|error| {
            DshError::new(codes::STATE_IO, format!("stat CLI profile: {error}"))
        })?;
        let held = self.directory.metadata().map_err(|error| {
            DshError::new(codes::STATE_IO, format!("stat held profile: {error}"))
        })?;
        if actual.dev() != held.dev() || actual.ino() != held.ino() {
            return Err(DshError::new(
                codes::STATE_IO,
                "CLI profile differs from locked profile",
            ));
        }
        Ok(())
    }

    pub(super) fn validate(&self) -> Result<(), DshError> {
        use std::os::unix::fs::MetadataExt;
        check_ancestors(&self.original)?;
        let actual = std::fs::canonicalize(&self.original)
            .map_err(|e| DshError::new(codes::STATE_IO, format!("resolve profile: {e}")))?;
        let path_meta = std::fs::metadata(&actual)
            .map_err(|e| DshError::new(codes::STATE_IO, format!("stat profile: {e}")))?;
        let held = self
            .directory
            .metadata()
            .map_err(|e| DshError::new(codes::STATE_IO, format!("stat held profile: {e}")))?;
        if path_meta.dev() != held.dev()
            || path_meta.ino() != held.ino()
            || held.nlink() == 0
            || held.uid() != unsafe { libc::geteuid() }
            || held.mode() & 0o022 != 0
        {
            return Err(DshError::new(codes::STATE_IO, "profile identity changed"));
        }
        let dir = std::fs::symlink_metadata(actual.join(".tuvis-management"))
            .map_err(|e| DshError::new(codes::STATE_IO, format!("stat lock directory: {e}")))?;
        let held_dir = self.lock_dir.metadata().map_err(|e| {
            DshError::new(codes::STATE_IO, format!("stat held lock directory: {e}"))
        })?;
        let lock = std::fs::symlink_metadata(actual.join(".tuvis-management/active.lock"))
            .map_err(|e| DshError::new(codes::STATE_IO, format!("stat lock file: {e}")))?;
        let held_lock = self
            .lock
            .metadata()
            .map_err(|e| DshError::new(codes::STATE_IO, format!("stat held lock: {e}")))?;
        if !dir.is_dir()
            || !lock.is_file()
            || dir.dev() != held_dir.dev()
            || dir.ino() != held_dir.ino()
            || lock.dev() != held_lock.dev()
            || lock.ino() != held_lock.ino()
            || held_lock.nlink() != 1
            || held_dir.uid() != unsafe { libc::geteuid() }
            || held_dir.mode() & 0o077 != 0
            || held_lock.uid() != unsafe { libc::geteuid() }
            || held_lock.mode() & 0o077 != 0
        {
            return Err(DshError::new(
                codes::STATE_IO,
                "profile lock identity changed",
            ));
        }
        check_ancestors(&actual)?;
        require_empty_acl(&self.directory)?;
        require_empty_acl(&self.lock_dir)?;
        require_empty_acl(&self.lock)?;
        Ok(())
    }
}

#[cfg(unix)]
pub(super) fn acquire(profile: &Path) -> Result<ProfileLock, DshError> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    check_ancestors(profile)?;
    let actual = std::fs::canonicalize(profile)
        .map_err(|e| DshError::new(codes::STATE_IO, format!("resolve profile: {e}")))?;
    check_ancestors(&actual)?;
    let name = CString::new(actual.as_os_str().as_bytes())
        .map_err(|_| DshError::new(codes::STATE_IO, "invalid profile path"))?;
    let fd = unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(DshError::new(
            codes::STATE_IO,
            format!("open profile: {}", std::io::Error::last_os_error()),
        ));
    }
    let directory = unsafe { std::fs::File::from_raw_fd(fd) };
    use std::os::unix::fs::MetadataExt;
    let profile_meta = directory
        .metadata()
        .map_err(|e| DshError::new(codes::STATE_IO, format!("stat profile: {e}")))?;
    if profile_meta.uid() != unsafe { libc::geteuid() } || profile_meta.mode() & 0o022 != 0 {
        return Err(DshError::new(
            codes::STATE_IO,
            "profile directory has unsafe ownership or mode",
        ));
    }
    require_empty_acl(&directory)?;
    let lock_dir_name = c".tuvis-management";
    let made = unsafe { libc::mkdirat(directory.as_raw_fd(), lock_dir_name.as_ptr(), 0o700) };
    if made != 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists {
        return Err(DshError::new(
            codes::STATE_IO,
            format!(
                "create profile lock directory: {}",
                std::io::Error::last_os_error()
            ),
        ));
    }
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            lock_dir_name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(DshError::new(
            codes::STATE_IO,
            format!(
                "open profile lock directory: {}",
                std::io::Error::last_os_error()
            ),
        ));
    }
    let lock_dir = unsafe { std::fs::File::from_raw_fd(fd) };
    let meta = lock_dir
        .metadata()
        .map_err(|e| DshError::new(codes::STATE_IO, format!("stat profile lock directory: {e}")))?;
    if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
        return Err(DshError::new(
            codes::STATE_IO,
            "profile lock directory has unsafe ownership or mode",
        ));
    }
    require_empty_acl(&lock_dir)?;
    let fd = unsafe {
        libc::openat(
            lock_dir.as_raw_fd(),
            c"active.lock".as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(DshError::new(
            codes::STATE_IO,
            format!("open profile job lock: {}", std::io::Error::last_os_error()),
        ));
    }
    let lock = unsafe { std::fs::File::from_raw_fd(fd) };
    let meta = lock
        .metadata()
        .map_err(|e| DshError::new(codes::STATE_IO, format!("stat profile job lock: {e}")))?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o077 != 0
        || meta.nlink() != 1
    {
        return Err(DshError::new(
            codes::STATE_IO,
            "profile job lock has unsafe identity or permissions",
        ));
    }
    require_empty_acl(&lock)?;
    lock.try_lock_exclusive().map_err(|error| {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            DshError::new(codes::JOB_BUSY, "another management job is running")
        } else {
            DshError::new(codes::STATE_IO, format!("lock profile job: {error}"))
        }
    })?;
    let guard = ProfileLock {
        original: profile.to_path_buf(),
        directory,
        lock_dir,
        lock,
    };
    guard.validate()?;
    Ok(guard)
}
