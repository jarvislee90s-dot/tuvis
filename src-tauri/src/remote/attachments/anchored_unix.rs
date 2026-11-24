use std::ffi::{CStr, CString};
use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

const DIRECTORY_FLAGS: libc::c_int = libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

pub(super) fn open_root(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(DIRECTORY_FLAGS)
        .open(path)
}

pub(super) fn open_dir(parent: &File, name: &str, create: bool) -> io::Result<File> {
    let name = component(name)?;
    if create {
        // SAFETY: name is one NUL-terminated component of the live parent fd.
        let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
        if result != 0 {
            let error = io::Error::last_os_error();
            if error.kind() != ErrorKind::AlreadyExists {
                return Err(error);
            }
        }
    }
    open_relative(parent, &name, libc::O_RDONLY | DIRECTORY_FLAGS, 0)
}

pub(super) fn same_file(left: &File, right: &File) -> io::Result<bool> {
    let left = left.metadata()?;
    let right = right.metadata()?;
    Ok((left.dev(), left.ino()) == (right.dev(), right.ino()))
}

pub(super) fn check_staging_permissions(staging: &File) -> io::Result<()> {
    let metadata = staging.metadata()?;
    // SAFETY: geteuid has no arguments and returns the effective Unix user ID.
    let uid = unsafe { libc::geteuid() };
    if metadata.uid() != uid || metadata.mode() & 0o022 != 0 {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "attachment staging directory is writable by another user",
        ));
    }
    Ok(())
}

pub(super) fn verify_git_head(parent: &File) -> io::Result<()> {
    let file = open_relative(
        parent,
        &component("HEAD")?,
        libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "invalid Git HEAD",
        ));
    }
    Ok(())
}

pub(super) fn append_git_exclude(parent: &File, line: &str) -> io::Result<()> {
    use std::io::{Read, Write};
    const MAX_EXCLUDE_BYTES: u64 = 1024 * 1024;
    static EXCLUDE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    let _guard = EXCLUDE_LOCK
        .lock()
        .map_err(|_| io::Error::other("Git exclude lock poisoned"))?;
    let mut file = open_relative(
        parent,
        &component("exclude")?,
        libc::O_RDWR
            | libc::O_APPEND
            | libc::O_CREAT
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | libc::O_CLOEXEC,
        0o600,
    )?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.len() > MAX_EXCLUDE_BYTES {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "unverified Git exclude file",
        ));
    }
    let mut existing = String::new();
    (&file)
        .take(MAX_EXCLUDE_BYTES + 1)
        .read_to_string(&mut existing)?;
    if existing.len() as u64 > MAX_EXCLUDE_BYTES
        || existing.lines().any(|entry| entry.trim() == line)
    {
        return Ok(());
    }
    let newline = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    file.write_all(format!("{newline}{line}\n").as_bytes())
}

pub(super) fn create_staged(parent: &File, name: &str) -> io::Result<File> {
    open_relative(
        parent,
        &component(name)?,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0o600,
    )
}

pub(super) fn publish(
    staging: &File,
    temporary: &str,
    file: &File,
    destination: &File,
    name: &str,
) -> io::Result<bool> {
    let temporary = component(temporary)?;
    let name = component(name)?;
    let named_file = open_relative(
        staging,
        &temporary,
        libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )?;
    if !same_file(&named_file, file)? {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "staged file was replaced",
        ));
    }
    // SAFETY: both names are single NUL-terminated components relative to live directory fds.
    #[cfg(target_vendor = "apple")]
    let result = unsafe {
        libc::renameatx_np(
            staging.as_raw_fd(),
            temporary.as_ptr(),
            destination.as_raw_fd(),
            name.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    let result = unsafe {
        libc::renameat2(
            staging.as_raw_fd(),
            temporary.as_ptr(),
            destination.as_raw_fd(),
            name.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(not(any(target_vendor = "apple", all(target_os = "linux", target_env = "gnu"))))]
    let result = unsafe {
        libc::linkat(
            staging.as_raw_fd(),
            temporary.as_ptr(),
            destination.as_raw_fd(),
            name.as_ptr(),
            0,
        )
    };
    if result == 0 {
        return Ok(cfg!(any(
            target_vendor = "apple",
            all(target_os = "linux", target_env = "gnu")
        )));
    }
    Err(io::Error::last_os_error())
}

pub(super) fn cleanup(parent: &File, name: &str, file: &File) -> io::Result<()> {
    let name = component(name)?;
    let named_file = match open_relative(
        parent,
        &name,
        libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    ) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !same_file(&named_file, file)? {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "staged file was replaced",
        ));
    }
    // SAFETY: the live parent fd and single component identify the temporary entry.
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } == 0 {
        return Ok(());
    }
    Err(io::Error::last_os_error())
}

fn component(name: &str) -> io::Result<CString> {
    CString::new(name).map_err(|error| io::Error::new(ErrorKind::InvalidInput, error))
}

fn open_relative(
    parent: &File,
    name: &CStr,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> io::Result<File> {
    // SAFETY: name is a valid C string, and a successful call returns a new owned fd.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags,
            libc::c_uint::from(mode),
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
