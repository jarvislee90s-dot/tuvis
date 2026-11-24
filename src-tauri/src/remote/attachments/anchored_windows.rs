use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind};
use std::mem::{offset_of, size_of};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;

use windows::core::PWSTR;
use windows::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows::Wdk::Storage::FileSystem::{
    FileRenameInformation, NtCreateFile, NtSetInformationFile, FILE_CREATE, FILE_DIRECTORY_FILE,
    FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF, FILE_OPEN_REPARSE_POINT,
    FILE_RENAME_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT,
};
use windows::Win32::Foundation::{
    RtlNtStatusToDosError, HANDLE, NTSTATUS, OBJ_CASE_INSENSITIVE, UNICODE_STRING,
};
use windows::Win32::Storage::FileSystem::{
    FileDispositionInfo, GetFileInformationByHandle, SetFileInformationByHandle,
    BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ACCESS_RIGHTS, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_WRITE, FILE_READ_ATTRIBUTES, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_TRAVERSE, SYNCHRONIZE,
};
use windows::Win32::System::IO::IO_STATUS_BLOCK;

pub(super) fn open_root(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .access_mode((FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE).0)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
        .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
        .open(path)?;
    information(&file)?;
    Ok(file)
}

pub(super) fn open_dir(parent: &File, name: &str, create: bool) -> io::Result<File> {
    open_relative(
        parent,
        name,
        true,
        create,
        FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
    )
}

pub(super) fn same_file(left: &File, right: &File) -> io::Result<bool> {
    let identity = |file| {
        let info = information(file)?;
        Ok::<_, io::Error>((
            info.dwVolumeSerialNumber,
            info.nFileIndexHigh,
            info.nFileIndexLow,
        ))
    };
    Ok(identity(left)? == identity(right)?)
}

pub(super) fn create_staged(parent: &File, name: &str) -> io::Result<File> {
    open_relative(
        parent,
        name,
        false,
        true,
        FILE_GENERIC_WRITE | FILE_READ_ATTRIBUTES | DELETE | SYNCHRONIZE,
    )
}

pub(super) fn publish(
    _staging: &File,
    _temporary: &str,
    file: &File,
    destination: &File,
    name: &str,
) -> io::Result<bool> {
    let wide = component(name)?;
    let name_bytes = wide.len() * size_of::<u16>();
    let offset = offset_of!(FILE_RENAME_INFORMATION, FileName);
    let length = (offset + name_bytes).max(size_of::<FILE_RENAME_INFORMATION>());
    let mut storage = vec![0usize; length.div_ceil(size_of::<usize>())];
    let record = storage.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    let mut status = IO_STATUS_BLOCK::default();
    // SAFETY: storage is aligned and sized for the C record and its inline UTF-16 name.
    // The source and destination handles remain live throughout this synchronous call.
    let result = unsafe {
        (*record).RootDirectory = handle(destination);
        (*record).FileNameLength = name_bytes as u32;
        std::ptr::copy_nonoverlapping(wide.as_ptr(), (*record).FileName.as_mut_ptr(), wide.len());
        NtSetInformationFile(
            handle(file),
            &mut status,
            record.cast(),
            length as u32,
            FileRenameInformation,
        )
    };
    nt_result(result)?;
    Ok(true) // A no-replace rename also removes the staging name.
}

pub(super) fn cleanup(_parent: &File, _name: &str, file: &File) -> io::Result<()> {
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: a live file handle with DELETE access and a correctly sized C record.
    unsafe {
        SetFileInformationByHandle(
            handle(file),
            FileDispositionInfo,
            (&raw const disposition).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    }
    .map_err(|_| io::Error::last_os_error())
}

fn information(file: &File) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: handle and output record are valid for the duration of the call.
    unsafe { GetFileInformationByHandle(handle(file), &mut info) }
        .map_err(|_| io::Error::last_os_error())?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "attachment reparse point",
        ));
    }
    Ok(info)
}

fn open_relative(
    parent: &File,
    name: &str,
    directory: bool,
    create: bool,
    access: FILE_ACCESS_RIGHTS,
) -> io::Result<File> {
    let mut wide = component(name)?;
    let length = (wide.len() * size_of::<u16>()) as u16;
    let mut object_name = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: PWSTR(wide.as_mut_ptr()),
    };
    let attrs = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: handle(parent),
        ObjectName: &mut object_name,
        Attributes: OBJ_CASE_INSENSITIVE,
        ..Default::default()
    };
    let kind = if directory {
        FILE_DIRECTORY_FILE
    } else {
        FILE_NON_DIRECTORY_FILE
    };
    let disposition = match (directory, create) {
        (true, true) => FILE_OPEN_IF,
        (false, true) => FILE_CREATE,
        (_, false) => FILE_OPEN,
    };
    let mut result = HANDLE::default();
    let mut status = IO_STATUS_BLOCK::default();
    // SAFETY: the relative name and attribute records outlive the synchronous call.
    // A successful call returns an owned handle. Omit share-delete to pin each directory.
    nt_result(unsafe {
        NtCreateFile(
            &mut result,
            access,
            &attrs,
            &mut status,
            None,
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            disposition,
            kind | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            None,
            0,
        )
    })?;
    let file = unsafe { File::from_raw_handle(result.0) };
    information(&file)?;
    Ok(file)
}

fn component(name: &str) -> io::Result<Vec<u16>> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['\\', '/', ':', '\0']) {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "invalid attachment component",
        ));
    }
    let wide: Vec<u16> = name.encode_utf16().collect();
    if wide.len() > 255 {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "attachment component is too long",
        ));
    }
    Ok(wide)
}

fn handle(file: &File) -> HANDLE {
    HANDLE(file.as_raw_handle())
}

fn nt_result(status: NTSTATUS) -> io::Result<()> {
    if status.is_ok() {
        return Ok(());
    }
    // SAFETY: any NTSTATUS can be converted to a Win32 error code.
    Err(io::Error::from_raw_os_error(
        unsafe { RtlNtStatusToDosError(status) } as i32,
    ))
}
