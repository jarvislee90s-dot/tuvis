use std::fs::File;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

use super::{ATTACHMENT_DIR_NAME, STAGING_DIR_NAME};

#[cfg(unix)]
#[path = "anchored_unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "anchored_windows.rs"]
mod platform;

pub(super) struct Directories {
    path: PathBuf,
    root: File,
    parent: File,
    staging: File,
    destination: File,
    session: String,
}

impl Directories {
    pub(super) fn open(path: &Path, session: &str) -> io::Result<Self> {
        if session.eq_ignore_ascii_case(STAGING_DIR_NAME) {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "reserved attachment directory",
            ));
        }
        let root = platform::open_root(path)?;
        let parent = platform::open_dir(&root, ATTACHMENT_DIR_NAME, true)?;
        let staging = platform::open_dir(&parent, STAGING_DIR_NAME, true)?;
        #[cfg(unix)]
        platform::check_staging_permissions(&staging)?;
        let destination = platform::open_dir(&parent, session, true)?;
        let dirs = Self {
            path: path.to_owned(),
            root,
            parent,
            staging,
            destination,
            session: session.to_owned(),
        };
        dirs.verify()?;
        Ok(dirs)
    }

    pub(super) fn verify(&self) -> io::Result<()> {
        let root = platform::open_root(&self.path)?;
        let parent = platform::open_dir(&root, ATTACHMENT_DIR_NAME, false)?;
        let staging = platform::open_dir(&parent, STAGING_DIR_NAME, false)?;
        let destination = platform::open_dir(&parent, &self.session, false)?;
        if !platform::same_file(&root, &self.root)?
            || !platform::same_file(&parent, &self.parent)?
            || !platform::same_file(&staging, &self.staging)?
            || !platform::same_file(&destination, &self.destination)?
        {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                "attachment directory changed",
            ));
        }
        Ok(())
    }

    #[cfg(unix)]
    pub(super) fn append_git_exclude(&self, line: &str) -> io::Result<()> {
        let root = platform::open_root(&self.path)?;
        if !platform::same_file(&root, &self.root)? {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                "project directory changed",
            ));
        }
        let git = platform::open_dir(&root, ".git", false)?;
        platform::verify_git_head(&git)?;
        let info = platform::open_dir(&git, "info", false)?;
        platform::append_git_exclude(&info, line)
    }

    pub(super) fn stage(&self) -> io::Result<Staged<'_>> {
        let name = format!("{:032x}.att", rand::random::<u128>());
        let file = platform::create_staged(&self.staging, &name)?;
        Ok(Staged {
            dirs: self,
            file,
            name,
            pending: true,
        })
    }
}

pub(super) struct Staged<'a> {
    dirs: &'a Directories,
    pub(super) file: File,
    name: String,
    pending: bool,
}

impl Staged<'_> {
    pub(super) fn publish(&mut self, name: &str) -> io::Result<()> {
        let renamed = platform::publish(
            &self.dirs.staging,
            &self.name,
            &self.file,
            &self.dirs.destination,
            name,
        )?;
        self.pending = !renamed;
        Ok(())
    }

    pub(super) fn cleanup(&mut self) -> io::Result<()> {
        if self.pending {
            platform::cleanup(&self.dirs.staging, &self.name, &self.file)?;
            self.pending = false;
        }
        Ok(())
    }
}

impl Drop for Staged<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            log::warn!("attachment staging cleanup failed: {error}");
        }
    }
}
