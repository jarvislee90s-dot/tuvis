//! 指定进程的启动期环境白名单观测；不作为有效 profile 的单独证明。
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadCompleteness {
    Complete,
    Incomplete,
}
#[derive(Clone, PartialEq, Eq)]
pub enum EnvironmentField {
    Unset,
    Empty,
    Value(String),
}
impl std::fmt::Debug for EnvironmentField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unset => "Unset",
            Self::Empty => "Empty",
            Self::Value(_) => "Value(<redacted>)",
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllowlistedEnvironment {
    pub dsh_home: EnvironmentField,
    pub home: EnvironmentField,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseEnvironmentError {
    ReadIncomplete,
    MissingTerminator,
    DuplicateKey,
    MalformedRecord,
    InvalidEncoding,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Procargs2Error {
    InvalidExtent,
    Incomplete,
    MalformedFrame,
    Environment(ParseEnvironmentError),
}

pub fn parse_macos_procargs2(
    buffer: &[u8],
    bytes_written: usize,
) -> Result<AllowlistedEnvironment, Procargs2Error> {
    if bytes_written > buffer.len() {
        return Err(Procargs2Error::InvalidExtent);
    }
    if bytes_written == buffer.len() || bytes_written < 4 {
        return Err(Procargs2Error::Incomplete);
    }
    let bytes = &buffer[..bytes_written];
    let argc = i32::from_ne_bytes(
        bytes[..4]
            .try_into()
            .map_err(|_| Procargs2Error::Incomplete)?,
    );
    if argc <= 0 {
        return Err(Procargs2Error::MalformedFrame);
    }
    let mut offset = 4;
    let path_end = bytes[offset..]
        .iter()
        .position(|b| *b == 0)
        .ok_or(Procargs2Error::Incomplete)?;
    let path_len = path_end + 1;
    let padded = path_len
        .checked_add(7)
        .ok_or(Procargs2Error::InvalidExtent)?
        & !7;
    offset = offset
        .checked_add(padded)
        .ok_or(Procargs2Error::InvalidExtent)?;
    if offset > bytes.len() {
        return Err(Procargs2Error::Incomplete);
    }
    if bytes[4 + path_len..offset].iter().any(|b| *b != 0) {
        return Err(Procargs2Error::MalformedFrame);
    }
    for _ in 0..argc {
        let end = bytes[offset..]
            .iter()
            .position(|b| *b == 0)
            .ok_or(Procargs2Error::Incomplete)?;
        offset += end + 1;
    }
    if offset >= bytes.len() {
        return Err(Procargs2Error::Incomplete);
    }
    parse_allowlisted_nul(&bytes[offset..], ReadCompleteness::Complete)
        .map_err(Procargs2Error::Environment)
}

#[cfg(all(target_os = "macos", feature = "dsh-test-probes"))]
#[derive(Clone, Debug)]
pub enum EnvironmentVisibility {
    Unknown,
    FullFor(ObservedProcessIdentity),
}
#[cfg(all(target_os = "macos", feature = "dsh-test-probes"))]
pub struct Procargs2Observation {
    pub bytes_written: usize,
    pub visibility: EnvironmentVisibility,
}
#[cfg(all(target_os = "macos", feature = "dsh-test-probes"))]
pub trait MacOsProcargs2Source: Send + Sync {
    fn identity_now(&self, pid: u32) -> Result<Option<ObservedProcessIdentity>, ReadFailure>;
    fn read_procargs2_into(
        &self,
        pid: u32,
        output: &mut [u8],
    ) -> Result<Procargs2Observation, ReadFailure>;
}
#[cfg(target_os = "macos")]
#[derive(Default)]
pub struct MacOsProcessEnvironmentReader {
    #[cfg(feature = "dsh-test-probes")]
    source: Option<std::sync::Arc<dyn MacOsProcargs2Source>>,
}
#[cfg(all(target_os = "macos", feature = "dsh-test-probes"))]
impl MacOsProcessEnvironmentReader {
    pub fn with_source(source: std::sync::Arc<dyn MacOsProcargs2Source>) -> Self {
        Self {
            source: Some(source),
        }
    }
}

#[cfg(target_os = "macos")]
impl ProcessEnvironmentReader for MacOsProcessEnvironmentReader {
    fn identity_now(&self, pid: u32) -> Result<Option<ObservedProcessIdentity>, ReadFailure> {
        #[cfg(feature = "dsh-test-probes")]
        if let Some(source) = &self.source {
            return source.identity_now(pid);
        }
        use std::os::unix::ffi::OsStrExt;
        let pid_raw = i32::try_from(pid)
            .ok()
            .filter(|p| *p > 0)
            .ok_or(ReadFailure::InvalidExtent)?;
        let read_info = || -> Result<Option<libc::proc_bsdinfo>, ReadFailure> {
            let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of::<libc::proc_bsdinfo>();
            let result = unsafe {
                libc::proc_pidinfo(
                    pid_raw,
                    libc::PROC_PIDTBSDINFO,
                    0,
                    (&mut info as *mut libc::proc_bsdinfo).cast(),
                    size as i32,
                )
            };
            if result == 0 {
                return match std::io::Error::last_os_error().raw_os_error() {
                    Some(libc::ESRCH) => Ok(None),
                    Some(libc::EPERM | libc::EACCES) => Err(ReadFailure::PermissionDenied),
                    _ => Err(ReadFailure::Io),
                };
            }
            if result as usize != size || info.pbi_pid != pid || info.pbi_start_tvsec == 0 {
                return Err(ReadFailure::InvalidExtent);
            }
            Ok(Some(info))
        };
        // XNU 私有 ABI：进程唯一 ID 不随 exec 变化，pid version 随 exec 更新。
        #[repr(C)]
        struct UniqueInfo {
            uuid: [u8; 16],
            unique_id: u64,
            parent_unique_id: u64,
            pid_version: u32,
            parent_version: u32,
            reserved: [u64; 2],
        }
        let read_generation = || -> Result<NativeGeneration, ReadFailure> {
            let mut info: UniqueInfo = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of::<UniqueInfo>();
            if size != 56 {
                return Err(ReadFailure::InvalidExtent);
            }
            let result = unsafe {
                libc::proc_pidinfo(
                    pid_raw,
                    17,
                    0,
                    (&mut info as *mut UniqueInfo).cast(),
                    size as i32,
                )
            };
            if result as usize != size || info.unique_id == 0 {
                return Err(ReadFailure::Io);
            }
            Ok(NativeGeneration {
                unique_id: info.unique_id,
                pid_version: info.pid_version,
            })
        };
        let Some(before) = read_info()? else {
            return Ok(None);
        };
        let generation = read_generation()?;
        let mut path = [0u8; 4096];
        let len =
            unsafe { libc::proc_pidpath(pid_raw, path.as_mut_ptr().cast(), path.len() as u32) };
        if len <= 0 || len as usize >= path.len() {
            return Err(ReadFailure::Io);
        }
        let Some(after) = read_info()? else {
            return Ok(None);
        };
        if before.pbi_pid != after.pbi_pid
            || before.pbi_start_tvsec != after.pbi_start_tvsec
            || before.pbi_start_tvusec != after.pbi_start_tvusec
            || generation != read_generation()?
        {
            return Err(ReadFailure::Io);
        }
        let started_at_ms = i64::try_from(before.pbi_start_tvsec)
            .ok()
            .and_then(|v| v.checked_mul(1000))
            .ok_or(ReadFailure::InvalidExtent)?;
        Ok(Some(ObservedProcessIdentity {
            pid,
            started_at_ms,
            executable: std::ffi::OsStr::from_bytes(&path[..len as usize]).into(),
            native_generation: Some(generation),
        }))
    }
    fn read_initial_environment_into(
        &self,
        pid: u32,
        output: &mut [u8],
    ) -> Result<ReadExtent, ReadFailure> {
        #[cfg(feature = "dsh-test-probes")]
        if let Some(source) = &self.source {
            let before = source.identity_now(pid)?.ok_or(ReadFailure::Io)?;
            let mut raw = zeroize::Zeroizing::new(vec![0u8; 1024 * 1024]);
            let observed = source.read_procargs2_into(pid, &mut raw)?;
            let after = source.identity_now(pid)?.ok_or(ReadFailure::Io)?;
            let full = matches!(observed.visibility, EnvironmentVisibility::FullFor(ref identity) if identity == &before && before == after && before.native_generation.is_some());
            if !full {
                return Ok(ReadExtent {
                    bytes_written: 0,
                    completeness: ReadCompleteness::Incomplete,
                });
            }
            let environment = parse_macos_procargs2(&raw, observed.bytes_written)
                .map_err(|_| ReadFailure::InvalidExtent)?;
            return write_allowlisted_environment(output, environment);
        }
        // 没有可信的可见性证据时，不读取或猜测目标环境。
        let _ = (pid, output);
        Ok(ReadExtent {
            bytes_written: 0,
            completeness: ReadCompleteness::Incomplete,
        })
    }
}

/// 观测目标进程**启动环境中的白名单字段**，仅用于「正向命中」判断。
///
/// 与 [`MacOsProcessEnvironmentReader`] 的区别：这里不宣称环境完整可见，
/// 也不得据此得出「某字段未设置」的结论。调用方只能把匹配到的值当作
/// 支持证据（例如「该 PID 确实以这个 HOME/DSH_HOME 启动」），未读到时
/// 必须当作未知而不是不匹配的反证。
#[cfg(target_os = "macos")]
pub fn observed_environment(pid: u32) -> Option<AllowlistedEnvironment> {
    let pid = i32::try_from(pid).ok().filter(|p| *p > 0)?;
    let mut argmax: libc::c_int = 0;
    let mut argmax_len = std::mem::size_of_val(&argmax);
    let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            (&mut argmax as *mut libc::c_int).cast(),
            &mut argmax_len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || argmax_len != std::mem::size_of_val(&argmax) || argmax <= 0 {
        return None;
    }
    let capacity = (argmax as usize).checked_add(4)?.min(1024 * 1024);
    if capacity <= 4 {
        return None;
    }
    let mut raw = zeroize::Zeroizing::new(vec![0u8; capacity]);
    let mut written = raw.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            raw.as_mut_ptr().cast(),
            &mut written,
            std::ptr::null_mut(),
            0,
        )
    };
    // 等于容量意味着可能被截断：本次观测不足以支持任何正向结论。
    if rc != 0 || written >= raw.len() {
        return None;
    }
    parse_macos_procargs2(&raw, written).ok()
}

#[cfg(all(target_os = "macos", feature = "dsh-test-probes"))]
fn write_allowlisted_environment(
    output: &mut [u8],
    environment: AllowlistedEnvironment,
) -> Result<ReadExtent, ReadFailure> {
    let mut used = 0usize;
    for (key, value) in [
        (b"DSH_HOME".as_slice(), environment.dsh_home),
        (b"HOME".as_slice(), environment.home),
    ] {
        let value = match value {
            EnvironmentField::Unset => continue,
            EnvironmentField::Empty => String::new(),
            EnvironmentField::Value(v) => v,
        };
        let end = used
            .checked_add(key.len() + 1 + value.len() + 1)
            .ok_or(ReadFailure::InvalidExtent)?;
        if end > output.len() {
            return Err(ReadFailure::InvalidExtent);
        }
        output[used..used + key.len()].copy_from_slice(key);
        used += key.len();
        output[used] = b'=';
        used += 1;
        output[used..used + value.len()].copy_from_slice(value.as_bytes());
        used += value.len();
        output[used] = 0;
        used += 1;
    }
    Ok(ReadExtent {
        bytes_written: used,
        completeness: ReadCompleteness::Complete,
    })
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeGeneration {
    pub unique_id: u64,
    pub pid_version: u32,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedProcessIdentity {
    pub pid: u32,
    pub started_at_ms: i64,
    pub executable: PathBuf,
    pub native_generation: Option<NativeGeneration>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadFailure {
    PermissionDenied,
    Io,
    Unsupported,
    InvalidExtent,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadExtent {
    pub bytes_written: usize,
    pub completeness: ReadCompleteness,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SampleEnvironmentError {
    ProcessMissing,
    IdentityChanged,
    IdentityReadFailed(ReadFailure),
    EnvironmentReadFailed(ReadFailure),
    Parse(ParseEnvironmentError),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitialEnvironmentObservation {
    identity: ObservedProcessIdentity,
    environment: AllowlistedEnvironment,
}
impl InitialEnvironmentObservation {
    pub fn identity(&self) -> &ObservedProcessIdentity {
        &self.identity
    }
    pub fn environment(&self) -> &AllowlistedEnvironment {
        &self.environment
    }
}
/// 必须对指定 PID 新鲜读取完整身份；无法确认启动代际或可执行文件时返回错误。
/// 环境读取只能报告实际写入字节；`Complete` 表示完整读取，而非仅看到 NUL。
pub trait ProcessEnvironmentReader: Send + Sync {
    fn identity_now(&self, pid: u32) -> Result<Option<ObservedProcessIdentity>, ReadFailure>;
    fn read_initial_environment_into(
        &self,
        pid: u32,
        output: &mut [u8],
    ) -> Result<ReadExtent, ReadFailure>;
}

/// 只保留白名单字段；不完整读取不能解释为字段不存在。
pub fn parse_allowlisted_nul(
    bytes: &[u8],
    completeness: ReadCompleteness,
) -> Result<AllowlistedEnvironment, ParseEnvironmentError> {
    if completeness != ReadCompleteness::Complete {
        return Err(ParseEnvironmentError::ReadIncomplete);
    }
    if !bytes.is_empty() && !bytes.ends_with(&[0]) {
        return Err(ParseEnvironmentError::MissingTerminator);
    }
    let mut dsh_home = None;
    let mut home = None;
    for record in bytes.split(|byte| *byte == 0) {
        if record.is_empty() {
            continue;
        }
        let separator = record.iter().position(|byte| *byte == b'=');
        let key = separator.map_or(record, |pos| &record[..pos]);
        let target = match key {
            b"DSH_HOME" => &mut dsh_home,
            b"HOME" => &mut home,
            _ => continue,
        };
        let pos = separator.ok_or(ParseEnvironmentError::MalformedRecord)?;
        if target.is_some() {
            return Err(ParseEnvironmentError::DuplicateKey);
        }
        let value = std::str::from_utf8(&record[pos + 1..])
            .map_err(|_| ParseEnvironmentError::InvalidEncoding)?;
        *target = Some(if value.is_empty() {
            EnvironmentField::Empty
        } else {
            EnvironmentField::Value(value.to_owned())
        });
    }
    Ok(AllowlistedEnvironment {
        dsh_home: dsh_home.unwrap_or(EnvironmentField::Unset),
        home: home.unwrap_or(EnvironmentField::Unset),
    })
}

/// 前后读取同一进程身份，避免把换代或读取失败解释为有效环境。
pub fn sample_initial_environment(
    reader: &dyn ProcessEnvironmentReader,
    expected: &ObservedProcessIdentity,
) -> Result<InitialEnvironmentObservation, SampleEnvironmentError> {
    let observe = || {
        reader
            .identity_now(expected.pid)
            .map_err(SampleEnvironmentError::IdentityReadFailed)?
            .ok_or(SampleEnvironmentError::ProcessMissing)
    };
    if observe()? != *expected {
        return Err(SampleEnvironmentError::IdentityChanged);
    }
    let mut bytes = zeroize::Zeroizing::new(vec![0u8; 1024 * 1024]);
    let read = reader.read_initial_environment_into(expected.pid, &mut bytes);
    let after = observe();
    (|| {
        if after? != *expected {
            return Err(SampleEnvironmentError::IdentityChanged);
        }
        let extent = read.map_err(SampleEnvironmentError::EnvironmentReadFailed)?;
        if extent.bytes_written > bytes.len() {
            return Err(SampleEnvironmentError::EnvironmentReadFailed(
                ReadFailure::InvalidExtent,
            ));
        }
        let environment =
            parse_allowlisted_nul(&bytes[..extent.bytes_written], extent.completeness)
                .map_err(SampleEnvironmentError::Parse)?;
        Ok(InitialEnvironmentObservation {
            identity: expected.clone(),
            environment,
        })
    })()
}

/// 非 macOS：无已验证的启动环境观测实现。
///
/// 返回 None = **不可观测**（不是「未设置」）。调用方必须据此拒绝依赖环境的
/// 归属判定；Windows 未实现，保持 fail-closed。
#[cfg(not(target_os = "macos"))]
pub fn observed_environment(_pid: u32) -> Option<AllowlistedEnvironment> {
    None
}
