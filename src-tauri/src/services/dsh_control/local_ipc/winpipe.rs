//! Windows 本机命名管道。权限描述符在创建时传入，禁止远程客户端。
//!
//! 本模块尚需 Windows 实机验证；调用方不得仅凭交叉编译宣称平台已验收。

use super::*;
use std::ffi::c_void;
use std::mem::size_of;
use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{NamedPipeServer, PipeMode, ServerOptions};
use tokio::sync::{mpsc, oneshot, watch};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER,
};
use windows::Win32::System::Pipes::GetNamedPipeClientProcessId;
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

fn ipc_error(context: &str, error: impl std::fmt::Display) -> DshError {
    DshError::new(codes::IPC_STATE, format!("{context}: {error}"))
}

struct OwnedToken(HANDLE);
impl Drop for OwnedToken {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
struct Descriptor(PSECURITY_DESCRIPTOR);
impl Drop for Descriptor {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0 .0)));
        }
    }
}

fn current_sid() -> Result<String, DshError> {
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(|e| ipc_error("open current process token", e))?;
    let token = OwnedToken(token);
    let mut size = 0u32;
    let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut size) };
    if size < size_of::<TOKEN_USER>() as u32 || size > 1024 * 1024 {
        return Err(DshError::new(
            codes::IPC_STATE,
            "invalid current user token size",
        ));
    }
    let words = (size as usize).div_ceil(size_of::<usize>());
    let mut buffer = vec![0usize; words];
    unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            size,
            &mut size,
        )
    }
    .map_err(|e| ipc_error("read current user token", e))?;
    let user = unsafe { &*(buffer.as_ptr().cast::<TOKEN_USER>()) };
    let mut sid = PWSTR::null();
    unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid) }
        .map_err(|e| ipc_error("convert current user SID", e))?;
    let result = unsafe { sid.to_string() }.map_err(|e| ipc_error("decode current user SID", e));
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sid.0.cast())));
    }
    result
}

fn create_pipe(address: &str, sid: &str, first: bool) -> Result<NamedPipeServer, DshError> {
    let sddl: Vec<u16> = format!("O:{sid}D:P(D;;GA;;;NU)(A;;GA;;;{sid})")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut raw = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut raw,
            None,
        )
    }
    .map_err(|e| ipc_error("create pipe security descriptor", e))?;
    let descriptor = Descriptor(raw);
    let mut attrs = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0 .0,
        bInheritHandle: false.into(),
    };
    let mut options = ServerOptions::new();
    options
        .pipe_mode(PipeMode::Byte)
        .reject_remote_clients(true)
        .first_pipe_instance(first);
    unsafe {
        options.create_with_security_attributes_raw(
            address,
            (&mut attrs as *mut SECURITY_ATTRIBUTES).cast::<c_void>(),
        )
    }
    .map_err(|e| ipc_error("create private named pipe", e))
}

struct State {
    address: String,
    sid: String,
    pipe: Option<NamedPipeServer>,
}

pub struct WinPipeListener {
    state_dir: std::path::PathBuf,
    name: Option<String>,
    state: Arc<tokio::sync::Mutex<Option<State>>>,
    endpoint: Mutex<Option<String>>,
    stopped: watch::Sender<bool>,
}

impl WinPipeListener {
    pub fn new(state_dir: std::path::PathBuf, name: Option<String>) -> Self {
        let (stopped, _) = watch::channel(false);
        Self {
            state_dir,
            name,
            state: Arc::new(tokio::sync::Mutex::new(None)),
            endpoint: Mutex::new(None),
            stopped,
        }
    }
}

impl IpcListener for WinPipeListener {
    fn bind(&self) -> IpcFut<'_, Result<String, DshError>> {
        Box::pin(async move {
            use sha2::{Digest, Sha256};
            use std::os::windows::ffi::OsStrExt;
            let mut guard = self.state.lock().await;
            if *self.stopped.borrow() {
                *guard = None;
            }
            if let Some(state) = guard.as_ref() {
                return Ok(state.address.clone());
            }
            let sid = current_sid()?;
            let mut hash = Sha256::new();
            hash.update(sid.as_bytes());
            for c in self.state_dir.as_os_str().encode_wide() {
                hash.update(c.to_le_bytes());
            }
            if let Some(name) = &self.name {
                hash.update(name.as_bytes());
            }
            let digest = hash.finalize();
            let suffix: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
            let address = format!(r"\\.\pipe\tuvis-dsh-{suffix}");
            let pipe = create_pipe(&address, &sid, true)?;
            self.stopped.send_replace(false);
            *self.endpoint.lock().unwrap() = Some(address.clone());
            *guard = Some(State {
                address: address.clone(),
                sid,
                pipe: Some(pipe),
            });
            Ok(address)
        })
    }

    fn accept(&self) -> IpcFut<'_, Result<Box<dyn IpcClientConnection>, DshError>> {
        Box::pin(async move {
            let mut guard = self.state.lock().await;
            let state = guard
                .as_mut()
                .ok_or_else(|| DshError::new(codes::IPC_STATE, "named pipe is not bound"))?;
            let mut stopped = self.stopped.subscribe();
            if *stopped.borrow() {
                return Err(DshError::new(codes::IPC_STATE, "named pipe stopped"));
            }
            let listener = state
                .pipe
                .as_ref()
                .ok_or_else(|| DshError::new(codes::IPC_STATE, "named pipe has no listener"))?;
            tokio::select! {
                biased;
                _ = stopped.changed() => {
                    state.pipe = None;
                    return Err(DshError::new(codes::IPC_STATE, "named pipe stopped"));
                },
                result = listener.connect() => result.map_err(|e| ipc_error("accept named pipe", e))?,
            }
            if *stopped.borrow() {
                return Err(DshError::new(codes::IPC_STATE, "named pipe stopped"));
            }
            let pipe = state
                .pipe
                .take()
                .ok_or_else(|| DshError::new(codes::IPC_STATE, "named pipe has no listener"))?;
            state.pipe = match create_pipe(&state.address, &state.sid, false) {
                Ok(next) => Some(next),
                Err(error) => {
                    self.stopped.send_replace(true);
                    *self.endpoint.lock().unwrap() = None;
                    return Err(error);
                }
            };
            let mut pid = 0u32;
            unsafe { GetNamedPipeClientProcessId(HANDLE(pipe.as_raw_handle()), &mut pid) }
                .map_err(|e| ipc_error("obtain named pipe peer", e))?;
            if pid == 0 {
                return Err(DshError::new(
                    codes::PEER_MISMATCH,
                    "named pipe peer PID unavailable",
                ));
            }
            Ok(Box::new(PipeConnection::start(pipe, pid)) as Box<dyn IpcClientConnection>)
        })
    }

    fn stop(&self) {
        self.stopped.send_replace(true);
        *self.endpoint.lock().unwrap() = None;
        if let Ok(mut state) = self.state.try_lock() {
            *state = None;
        } else if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let state = Arc::clone(&self.state);
            let stopped = self.stopped.subscribe();
            runtime.spawn(async move {
                let mut state = state.lock().await;
                if *stopped.borrow() {
                    *state = None;
                }
            });
        }
    }

    fn endpoint(&self) -> Option<String> {
        self.endpoint.lock().unwrap().clone()
    }
}

type WriteRequest = (String, oneshot::Sender<Result<(), DshError>>);
struct PipeConnection {
    peer: PeerOsIdentity,
    incoming: tokio::sync::Mutex<mpsc::Receiver<Result<String, DshError>>>,
    outgoing: mpsc::Sender<WriteRequest>,
    stop: watch::Sender<bool>,
    closed: Arc<AtomicBool>,
}

impl PipeConnection {
    fn start(pipe: NamedPipeServer, pid: u32) -> Self {
        let (in_tx, incoming) = mpsc::channel(32);
        let (outgoing, mut out_rx) = mpsc::channel::<WriteRequest>(32);
        let (stop, mut read_stop) = watch::channel(false);
        let mut write_stop = stop.subscribe();
        let write_abort = stop.clone();
        let (ended, mut peer_ended) = watch::channel(false);
        let closed = Arc::new(AtomicBool::new(false));
        let read_closed = Arc::clone(&closed);
        let write_closed = Arc::clone(&closed);
        let (mut reader, mut writer) = tokio::io::split(pipe);
        tokio::spawn(async move {
            let mut pending = Vec::<u8>::new();
            let mut chunk = [0u8; 8192];
            'read: loop {
                let result = tokio::select! {
                    _ = read_stop.changed() => break,
                    result = reader.read(&mut chunk) => result,
                };
                match result {
                    Ok(0) => break,
                    Ok(n) => {
                        pending.extend_from_slice(&chunk[..n]);
                        while let Some(end) = pending.iter().position(|b| *b == b'\n') {
                            let mut line: Vec<u8> = pending.drain(..=end).collect();
                            line.pop();
                            if line.last() == Some(&b'\r') {
                                line.pop();
                            }
                            if line.len() > MAX_FRAME_BYTES {
                                break 'read;
                            }
                            if line.is_empty() {
                                continue;
                            }
                            let frame = String::from_utf8(line)
                                .map_err(|e| ipc_error("decode named pipe frame", e));
                            let sent = tokio::select! {
                                result = in_tx.send(frame) => result.is_ok(),
                                _ = read_stop.changed() => false,
                            };
                            if !sent {
                                break 'read;
                            }
                        }
                        if pending.len() > MAX_FRAME_BYTES + 1 {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            read_closed.store(true, Ordering::Release);
            ended.send_replace(true);
        });
        tokio::spawn(async move {
            loop {
                let message = tokio::select! {
                    _ = write_stop.changed() => break,
                    _ = peer_ended.changed() => break,
                    message = out_rx.recv() => message,
                };
                let Some((line, reply)) = message else { break };
                let mut bytes = line.into_bytes();
                bytes.push(b'\n');
                let result = tokio::select! {
                    result = writer.write_all(&bytes) => result.map_err(|e| ipc_error("write named pipe", e)),
                    _ = write_stop.changed() => Err(DshError::new(codes::IPC_STATE, "named pipe closed")),
                    _ = peer_ended.changed() => Err(DshError::new(codes::IPC_STATE, "named pipe peer closed")),
                };
                let failed = result.is_err();
                let _ = reply.send(result);
                if failed {
                    break;
                }
            }
            write_closed.store(true, Ordering::Release);
            if !*peer_ended.borrow() {
                write_abort.send_replace(true);
            }
        });
        Self {
            peer: PeerOsIdentity { pid, uid: None },
            incoming: tokio::sync::Mutex::new(incoming),
            outgoing,
            stop,
            closed,
        }
    }
}

impl IpcClientConnection for PipeConnection {
    fn peer_os(&self) -> PeerOsIdentity {
        self.peer.clone()
    }
    fn recv(&self) -> IpcFut<'_, Result<String, DshError>> {
        Box::pin(async move {
            let mut stop = self.stop.subscribe();
            let mut incoming = self.incoming.lock().await;
            if *stop.borrow() {
                return Err(DshError::new(codes::IPC_STATE, "named pipe closed"));
            }
            if let Ok(frame) = incoming.try_recv() {
                return frame;
            }
            if self.is_closed() {
                return Err(DshError::new(codes::IPC_STATE, "named pipe closed"));
            }
            tokio::select! {
                frame = incoming.recv() => frame.unwrap_or_else(|| Err(DshError::new(codes::IPC_STATE, "named pipe closed"))),
                _ = stop.changed() => Err(DshError::new(codes::IPC_STATE, "named pipe closed")),
            }
        })
    }
    fn send(&self, frame: &str) -> IpcFut<'_, Result<(), DshError>> {
        if frame.len() > MAX_FRAME_BYTES || frame.contains('\n') {
            return Box::pin(async {
                Err(DshError::new(codes::IPC_STATE, "invalid named pipe frame"))
            });
        }
        let frame = frame.to_owned();
        Box::pin(async move {
            if self.is_closed() {
                return Err(DshError::new(codes::IPC_STATE, "named pipe closed"));
            }
            let (reply, wait) = oneshot::channel();
            self.outgoing
                .send((frame, reply))
                .await
                .map_err(|e| ipc_error("queue named pipe frame", e))?;
            wait.await
                .map_err(|e| ipc_error("await named pipe write", e))?
        })
    }
    fn close(self: Arc<Self>) {
        self.closed.store(true, Ordering::Release);
        self.stop.send_replace(true);
    }
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}
