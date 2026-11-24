//! 手写 HTTP/1.1 一元传输（目标 2 写入边界）。
//!
//! reqwest 不暴露逐写观察者——「交付沿 = 请求体交出 HTTP 栈」只覆盖到缓冲
//! 交接，底层 flush 完成前存在残余窗（销项复审 Important-6 降级申报）。本
//! 模块对回环一元面（`POST /api/{method}`、HTTP/1.1、JSON、body ≤2MiB）
//! 改走 TokioTcpStream 直连直写：
//!
//! - **交付沿 = `write_all(整帧)` 完成**——tokio 的 TcpStream 无用户态写
//!   缓冲，`write_all` 返回即字节已交内核；回环下宿主 socket 立即可见
//!   （flush 语义成立，交付沿残余窗**归零**）。
//! - **取消 = drop 连接**——写沿前的任务 drop 直接关闭 socket（零字节出门
//!   可证）；写沿后 drop 不可能（交付沿即函数返回）。部分写后失败/取消：
//!   连接关闭，宿主按 Content-Length 不齐丢弃请求体，业务零执行。
//! - **每请求一条连接**（`Connection: close`）——串行槽位下回环建连成本可
//!   忽略；不存在池化连接的「残留字节续写到下一请求」面。
//! - 响应解析覆盖 Content-Length / chunked / Connection: close 三种帧型，
//!   体上限对齐 [`crate::services::dsh_control::codec::MAX_JSON_FRAME_BYTES`]。
//!
//! `/connect` 兑换与 mux WebSocket 升级不经本模块（仍走既有 reqwest /
//! tokio-tungstenite 面）。

use std::time::Duration;

use tokio::io::AsyncReadExt;
#[cfg(test)]
use tokio::io::AsyncWriteExt;

use super::{codec, error::codes, DshError};

/// 响应头缓冲上限（状态行 + 头；远超合理范围即视为对端异常）。
const MAX_HEAD_BYTES: usize = 16 * 1024;

/// 一元 HTTP/1.1 响应（已读全；供 [`super::transport`] 分类）。
#[derive(Debug, Clone)]
pub struct RawUnaryResponse {
    pub status: u16,
    pub content_type: String,
    /// 3xx 的 Location 头原值（authority 钉定面记录用；无则 None）。
    pub location: Option<String>,
    pub body: Vec<u8>,
}

/// 请求头编码（纯函数，可测）：POST + 精确 Content-Length + Connection:
/// close。body 不在本函数内——调用方把返回值与 body 拼成单一 wire 缓冲，
/// `write_all` 一次出门（交付沿语义的前提）。
pub fn build_request_head(target: &str, authority: &str, cookie: &str, body_len: usize) -> Vec<u8> {
    format!(
        "POST {target} HTTP/1.1\r\nHost: {authority}\r\nCookie: {cookie}\r\nContent-Type: application/json\r\nContent-Length: {body_len}\r\nConnection: close\r\n\r\n"
    )
    .into_bytes()
}

/// 写沿后的在途响应。`finish` 恰好消费一次；任务已在独立 tokio task 推进
/// （写沿返回后调用方去排空提交许可，不阻塞响应读取）。
pub struct Http1Flight {
    response: Pin<Box<dyn std::future::Future<Output = Result<RawUnaryResponse, DshError>> + Send>>,
}

impl Http1Flight {
    pub async fn finish(self) -> Result<RawUnaryResponse, DshError> {
        (self.response).await
    }
}

use std::pin::Pin;

/// start 失败相位（目标 2 分类面）：调用方据此分流 NotExecuted（建连段 =
/// 零字节出门可证）与 DeliveryUnknown（写段 = 部分写出可能，保守未知）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartPhase {
    /// 建连段失败：socket 未建立，**零字节出门**。
    Connect,
    /// 写出段失败：连接已建立，部分写出可能——结局未知。
    Write,
}

/// [`start`] 的失败（相位 + 已映射错误码）。
#[derive(Debug)]
pub struct StartError {
    pub phase: StartPhase,
    pub error: DshError,
}

/// 连接并写出一元请求；返回于**交付沿**（整帧 `write_all` 完成 = 字节交
/// 内核）。失败带相位（[`StartPhase`]）。
#[cfg(test)]
pub async fn start(
    authority: &str,
    target: &str,
    cookie: &str,
    body: Vec<u8>,
    connect_timeout: Duration,
    unary_timeout: Duration,
) -> Result<Http1Flight, StartError> {
    start_checked(
        authority,
        target,
        cookie,
        body,
        [connect_timeout, unary_timeout],
        WriteGuard {
            rpc_id: "",
            acceptance: None,
            first_write_check: None,
            device: None,
            closed: &std::sync::atomic::AtomicBool::new(false),
        },
    )
    .await
}

pub(crate) struct WriteGuard<'a> {
    pub rpc_id: &'a str,
    pub acceptance: Option<(&'a str, &'a super::production::SendAcceptance)>,
    pub closed: &'a std::sync::atomic::AtomicBool,
    pub first_write_check: Option<(super::model::Target, super::actions::PreSubmitGate)>,
    pub device: Option<(&'a crate::remote::pairing::DeviceStore, &'a str)>,
}

pub(crate) async fn start_checked(
    authority: &str,
    target: &str,
    cookie: &str,
    body: Vec<u8>,
    [connect_timeout, unary_timeout]: [Duration; 2],
    guard: WriteGuard<'_>,
) -> Result<Http1Flight, StartError> {
    let mut head = build_request_head(target, authority, cookie, body.len());
    head.extend_from_slice(&body);
    #[cfg(any(test, feature = "dsh-test-probes"))]
    unary_pause::pause(guard.rpc_id, unary_pause::Stage::BeforeConnect).await;
    #[cfg(not(any(test, feature = "dsh-test-probes")))]
    let _ = guard.rpc_id;
    // ---- 建连（超时包住；失败 = 零字节出门，可证未执行）----
    let connect = tokio::time::timeout(connect_timeout, tokio::net::TcpStream::connect(authority))
        .await
        .map_err(|_| StartError {
            phase: StartPhase::Connect,
            error: DshError::new(
                codes::WRITE_TIMEOUT,
                "unary connect timed out; zero bytes written; not executed",
            ),
        })?
        .map_err(|e| StartError {
            phase: StartPhase::Connect,
            error: DshError::new(
                codes::CONNECT_FAILED,
                format!("unary connect failed; zero bytes written: {e}"),
            ),
        })?;
    let set_nodelay_result = connect.set_nodelay(true);
    let sock = connect;
    set_nodelay_result.map_err(|e| StartError {
        phase: StartPhase::Connect,
        error: DshError::new(codes::CONNECT_FAILED, format!("unary set_nodelay: {e}")),
    })?;
    let write_deadline = tokio::time::Instant::now() + unary_timeout;
    let mut written = 0;
    while written < head.len() {
        let phase = if written == 0 {
            StartPhase::Connect
        } else {
            StartPhase::Write
        };
        tokio::time::timeout_at(write_deadline, sock.writable())
            .await
            .map_err(|_| StartError {
                phase,
                error: DshError::new(codes::WRITE_TIMEOUT, "unary write timed out"),
            })?
            .map_err(|error| write_failed(phase, error))?;
        let bytes = &head[written..];
        #[cfg(any(test, feature = "dsh-test-probes"))]
        let bytes = if written == 0
            && unary_pause::armed(guard.rpc_id, unary_pause::Stage::AfterPartialWrite)
        {
            &bytes[..1]
        } else {
            bytes
        };
        let write_entry = || {
            if guard.closed.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "authorized peer closed",
                ));
            }
            sock.try_write(bytes)
        };
        let result = if let Some((instance, acceptance)) = guard.acceptance {
            let permit = if let (0, Some((target, check))) = (written, &guard.first_write_check) {
                tokio::time::timeout_at(write_deadline, check(target.clone()))
                    .await
                    .map_err(|_| StartError {
                        phase,
                        error: DshError::new(
                            codes::WRITE_TIMEOUT,
                            "unary pre-write check timed out",
                        ),
                    })?
                    .map_err(|error| StartError { phase, error })?;
                // 检查后只接受即时许可；不持全局许可做磁盘 I/O，也不在检查后等换代。
                super::production::try_commit_permit().ok_or_else(|| StartError {
                    phase,
                    error: DshError::new(
                        codes::INSTANCE_SWITCHED,
                        "instance is switching at stop commit",
                    ),
                })?
            } else {
                tokio::time::timeout_at(write_deadline, super::production::commit_permit_enter())
                    .await
                    .map_err(|_| StartError {
                        phase,
                        error: DshError::new(codes::WRITE_TIMEOUT, "write commit timed out"),
                    })?
            };
            let result = super::production::with_send_acceptance(instance, acceptance, || {
                if let (0, Some((store, device))) = (written, guard.device) {
                    return store
                        .try_with(|conn| {
                            if !crate::remote::pairing::device_valid(
                                conn,
                                device,
                                chrono::Utc::now().timestamp_millis(),
                            ) {
                                return Err(DshError::new(
                                    codes::ACTION_WRITE_GATED,
                                    "device authorization is no longer valid",
                                ));
                            }
                            Ok(write_entry())
                        })
                        .ok_or_else(|| {
                            DshError::new(codes::ACTION_WRITE_GATED, "device authorization is busy")
                        })?;
                }
                Ok(write_entry())
            })
            .and_then(|result| result)
            .map_err(|error| StartError { phase, error });
            drop(permit);
            result?
        } else {
            write_entry()
        };
        match result {
            Ok(0) => return Err(write_failed(phase, std::io::ErrorKind::WriteZero.into())),
            Ok(count) => written += count,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(error) => return Err(write_failed(phase, error)),
        }
        #[cfg(any(test, feature = "dsh-test-probes"))]
        if written < head.len() {
            unary_pause::pause(guard.rpc_id, unary_pause::Stage::AfterPartialWrite).await;
        }
    }
    // ================= 交付沿 =================
    let deadline = tokio::time::Instant::now() + unary_timeout;
    let handle = tokio::spawn(read_response(sock, deadline));
    Ok(Http1Flight {
        response: Box::pin(async move {
            handle.await.map_err(|e| {
                DshError::new(
                    codes::CONNECT_FAILED,
                    format!("unary response task failed: {e}"),
                )
            })?
        }),
    })
}

fn write_failed(phase: StartPhase, error: std::io::Error) -> StartError {
    StartError {
        phase,
        error: DshError::new(
            codes::CONNECTION_CLOSED,
            format!("unary write failed: {error}"),
        ),
    }
}

#[cfg(any(test, feature = "dsh-test-probes"))]
pub mod unary_pause {
    use std::sync::Mutex;

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub enum Stage {
        BeforeConnect,
        AfterPartialWrite,
    }

    struct Pause {
        id: String,
        stage: Stage,
        reached: bool,
    }
    static STATE: Mutex<Option<Pause>> = Mutex::new(None);

    pub fn arm(rpc_id: &str, stage: Stage) {
        *STATE.lock().unwrap() = Some(Pause {
            id: rpc_id.to_owned(),
            stage,
            reached: false,
        });
    }

    pub fn reached() -> bool {
        STATE
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|pause| pause.reached)
    }

    pub fn release() {
        STATE.lock().unwrap().take();
    }

    pub(super) fn armed(rpc_id: &str, stage: Stage) -> bool {
        STATE
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|pause| pause.id == rpc_id && pause.stage == stage)
    }

    pub(super) async fn pause(rpc_id: &str, stage: Stage) {
        {
            let mut state = STATE.lock().unwrap();
            let Some(pause) = state
                .as_mut()
                .filter(|pause| pause.id == rpc_id && pause.stage == stage)
            else {
                return;
            };
            pause.reached = true;
        }
        while armed(rpc_id, stage) {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    }
}

/// 读取并解析响应（写沿后任务）：头（≤16KiB）→ 按 Content-Length / chunked
/// / close 帧型读体（≤2MiB）。任何截断/畸形/超限 = 结局未知（请求已达宿主
/// ——保守，不自动重投）。总期限 = `unary_timeout`（自交付沿起算，与旧实现
/// 的「逐请求超时」同界）。
async fn read_response(
    mut sock: tokio::net::TcpStream,
    deadline: tokio::time::Instant,
) -> Result<RawUnaryResponse, DshError> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    // ---- 头 ----
    let head_len = loop {
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            if pos + 4 > MAX_HEAD_BYTES {
                return Err(unknown("response head over cap"));
            }
            break pos + 4;
        }
        if buf.len() > MAX_HEAD_BYTES {
            return Err(unknown("response head over cap"));
        }
        let n = tokio::time::timeout_at(deadline, sock.read(&mut chunk))
            .await
            .map_err(|_| timeout_unknown("response head"))?
            .map_err(|e| unknown(format!("unary response read failed: {e}")))?;
        if n == 0 {
            return Err(unknown("response head truncated (EOF)"));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head_text = std::str::from_utf8(&buf[..head_len])
        .map_err(|_| unknown("unary response head is not utf-8"))?;
    let mut lines = head_text.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| unknown(format!("unary status line malformed: {status_line}")))?;
    let mut content_type = "application/json".to_string();
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    let mut location: Option<String> = None;
    for line in lines.take_while(|line| !line.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .filter(|(name, _)| valid_field_name(name.as_bytes()))
            .ok_or_else(|| unknown("malformed response header"))?;
        let name = name.to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "content-type" => content_type = value.to_string(),
            "content-length" => {
                for length in value.split(',').map(str::trim) {
                    let parsed = length
                        .parse::<usize>()
                        .ok()
                        .filter(|_| length.bytes().all(|byte| byte.is_ascii_digit()))
                        .ok_or_else(|| unknown("invalid content-length"))?;
                    if content_length.is_some_and(|known| known != parsed) {
                        return Err(unknown("conflicting content-length values"));
                    }
                    content_length = Some(parsed);
                }
            }
            "transfer-encoding" => {
                if chunked || !value.eq_ignore_ascii_case("chunked") {
                    return Err(unknown("unsupported transfer-encoding"));
                }
                chunked = true;
            }
            "location" => location = Some(value.to_string()),
            _ => {}
        }
    }
    if chunked && content_length.is_some() {
        return Err(unknown("transfer-encoding conflicts with content-length"));
    }
    let mut body = buf.split_off(head_len);
    // ---- 体 ----
    if chunked {
        let mut decoder = ChunkDecoder::default();
        loop {
            if body.len() > codec::MAX_JSON_FRAME_BYTES * 2 {
                return Err(oversize_unknown("chunked body"));
            }
            if decoder.consume(&body, codec::MAX_JSON_FRAME_BYTES)? {
                body = decoder.body;
                break;
            }
            let n = tokio::time::timeout_at(deadline, sock.read(&mut chunk))
                .await
                .map_err(|_| timeout_unknown("chunked body"))?
                .map_err(|e| unknown(format!("unary chunked body read failed: {e}")))?;
            if n == 0 {
                return Err(unknown("chunked body truncated (EOF)"));
            }
            body.extend_from_slice(&chunk[..n]);
        }
    } else if let Some(len) = content_length {
        if len > codec::MAX_JSON_FRAME_BYTES {
            return Err(oversize_unknown("content-length"));
        }
        while body.len() < len {
            let n = tokio::time::timeout_at(deadline, sock.read(&mut chunk))
                .await
                .map_err(|_| timeout_unknown("content-length body"))?
                .map_err(|e| unknown(format!("unary body read failed: {e}")))?;
            if n == 0 {
                return Err(unknown("body truncated (EOF before content-length)"));
            }
            body.extend_from_slice(&chunk[..n]);
        }
        body.truncate(len);
    } else {
        // 无 CL 无 chunked：Connection: close 语义——读到 EOF。
        loop {
            let n = tokio::time::timeout_at(deadline, sock.read(&mut chunk))
                .await
                .map_err(|_| timeout_unknown("close-framed body"))?
                .map_err(|e| unknown(format!("unary close-framed body read failed: {e}")))?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
            if body.len() > codec::MAX_JSON_FRAME_BYTES {
                return Err(oversize_unknown("close-framed body"));
            }
        }
    }
    Ok(RawUnaryResponse {
        status,
        content_type,
        location,
        body,
    })
}

/// 只消费完整帧。输入追加时从上次位置继续，避免逐包重解码已读完的块。
#[derive(Default)]
struct ChunkDecoder {
    body: Vec<u8>,
    pos: usize,
    pending_size: Option<usize>,
    trailer_start: Option<usize>,
}

impl ChunkDecoder {
    fn consume(&mut self, input: &[u8], cap: usize) -> Result<bool, DshError> {
        loop {
            let remaining = &input[self.pos..];
            if let Some(size) = self.pending_size {
                if remaining.len() < size + 2 {
                    return Ok(false);
                }
                if &remaining[size..size + 2] != b"\r\n" {
                    return Err(unknown("chunked block missing CRLF"));
                }
                self.body.extend_from_slice(&remaining[..size]);
                self.pos += size + 2;
                self.pending_size = None;
                continue;
            }

            let line_budget = self.trailer_start.map_or(MAX_HEAD_BYTES, |start| {
                MAX_HEAD_BYTES.saturating_sub(self.pos - start)
            });
            let Some(line_end) = find(remaining, b"\r\n") else {
                if remaining.len() >= line_budget {
                    return Err(oversize_unknown("chunked metadata"));
                }
                return Ok(false);
            };
            if line_end + 2 > line_budget {
                return Err(oversize_unknown("chunked metadata"));
            }
            let line = &remaining[..line_end];
            self.pos += line_end + 2;
            if self.trailer_start.is_some() {
                if line.is_empty() {
                    return Ok(true);
                }
                let name = line.splitn(2, |&byte| byte == b':').collect::<Vec<_>>();
                if name.len() != 2
                    || !valid_field_name(name[0])
                    || name[0].eq_ignore_ascii_case(b"content-length")
                    || name[0].eq_ignore_ascii_case(b"transfer-encoding")
                    || name[1]
                        .iter()
                        .any(|&byte| byte.is_ascii_control() && byte != b'\t')
                {
                    return Err(unknown("invalid chunked trailer"));
                }
                continue;
            }
            let size_text = line.split(|&byte| byte == b';').next().unwrap_or_default();
            if size_text.is_empty() || !size_text.iter().all(u8::is_ascii_hexdigit) {
                return Err(unknown("chunked size line malformed"));
            }
            let size =
                usize::from_str_radix(std::str::from_utf8(size_text).expect("hex is ASCII"), 16)
                    .map_err(|_| oversize_unknown("chunked body"))?;
            if size > cap.saturating_sub(self.body.len()) {
                return Err(oversize_unknown("chunked body"));
            }
            if size == 0 {
                self.trailer_start = Some(self.pos);
            } else {
                self.pending_size = Some(size);
            }
        }
    }
}

fn valid_field_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(byte))
}

#[cfg(test)]
fn try_decode_chunked(input: &[u8], cap: usize) -> Option<Result<Vec<u8>, DshError>> {
    let mut decoder = ChunkDecoder::default();
    match decoder.consume(input, cap) {
        Ok(true) => Some(Ok(decoder.body)),
        Ok(false) => None,
        Err(error) => Some(Err(error)),
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn unknown(detail: impl Into<String>) -> DshError {
    DshError::new(
        codes::CONNECT_FAILED,
        format!(
            "unary response unusable; delivery unknown: {}",
            detail.into()
        ),
    )
}

/// 响应阶段超时（旧 reqwest 总超时映射）：单请求超时 → `DSH_WRITE_TIMEOUT`
/// （送达未知，不自动重投——与写段超时同码族）。
fn timeout_unknown(detail: &str) -> DshError {
    DshError::new(
        codes::WRITE_TIMEOUT,
        format!("unary response timed out; delivery unknown: {detail}"),
    )
}

/// 体超限（旧 R11 累计限额映射）：读过程中一过 2MiB 即 `DSH_FRAME_OVERSIZE`。
fn oversize_unknown(detail: &str) -> DshError {
    DshError::new(
        codes::FRAME_OVERSIZE,
        format!("unary body over cap; delivery unknown: {detail}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::{future::Future, task::Poll};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::{timeout, Instant};

    const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
    const CHUNKED_HEAD: &[u8] = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";

    async fn loopback_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let authority = listener.local_addr().unwrap();
        let (client, server) = timeout(RESPONSE_TIMEOUT, async {
            tokio::try_join!(TcpStream::connect(authority), listener.accept())
        })
        .await
        .expect("回环连接超时")
        .unwrap();
        (client, server.0)
    }

    #[tokio::test]
    async fn revoked_device_cannot_write_the_first_settings_byte() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let authority = listener.local_addr().unwrap().to_string();
        let device = "settings-test-device";
        let store = crate::remote::pairing::DeviceStore::memory();
        let instance = "settings-test-isolated";
        let admission = super::super::production::capture_send_acceptance(instance, None);
        let closed = std::sync::atomic::AtomicBool::new(false);
        let first_write = start_checked(
            &authority,
            "/api/session/selectModel",
            "fixture",
            b"{}".to_vec(),
            [RESPONSE_TIMEOUT, RESPONSE_TIMEOUT],
            WriteGuard {
                rpc_id: "settings-first-byte",
                acceptance: Some((instance, &admission)),
                closed: &closed,
                first_write_check: None,
                device: Some((&store, device)),
            },
        )
        .await;
        let Err(error) = first_write else {
            panic!("unauthorized device crossed the write boundary");
        };
        assert_eq!(error.error.code, codes::ACTION_WRITE_GATED);
        assert!(matches!(error.phase, StartPhase::Connect));
        let (socket, _) = listener.accept().await.unwrap();
        let mut buffer = [0u8; 1];
        assert_eq!(
            timeout(RESPONSE_TIMEOUT, socket.peek(&mut buffer))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }

    async fn read_through_eof(wire: &[u8]) -> Result<RawUnaryResponse, DshError> {
        let (client, mut server) = loopback_pair().await;
        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        timeout(RESPONSE_TIMEOUT, async {
            server.write_all(wire).await.unwrap();
            server.shutdown().await.unwrap();
            read_response(client, deadline).await
        })
        .await
        .expect("回环响应测试超时")
    }

    async fn read_bytewise_chunked(wire: &[u8]) -> RawUnaryResponse {
        let (client, mut server) = loopback_pair().await;
        server.set_nodelay(true).unwrap();
        let client = client.into_std().unwrap();
        let observer = TcpStream::from_std(client.try_clone().unwrap()).unwrap();
        let client = TcpStream::from_std(client).unwrap();
        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        let mut response = std::pin::pin!(read_response(client, deadline));

        timeout(RESPONSE_TIMEOUT, async {
            let mut received = [0u8; 1];
            let bytes = CHUNKED_HEAD.iter().chain(wire).copied();
            let last = CHUNKED_HEAD.len() + wire.len() - 1;
            for (offset, byte) in bytes.enumerate() {
                server.write_all(&[byte]).await.unwrap();
                // 先窥读确认字节已到内核。推进解析器后才发送下一字节，禁止合并。
                assert_eq!(observer.peek(&mut received).await.unwrap(), 1);
                assert_eq!(received[0], byte);
                if offset == last {
                    return response.await.expect("完整 chunked 帧应解析成功");
                }
                // 排除 Tokio 的协作让步，避免把它和接收队列清空混淆。
                tokio::task::unconstrained(std::future::poll_fn(|cx| {
                    let state = response.as_mut().poll(cx);
                    assert!(
                        state.is_pending(),
                        "响应在字节 {offset} 后提前结束: {state:?}"
                    );
                    // 两个描述符的就绪通知可能先后到达。只有接收队列清空才放行下一字节。
                    let mut unread = tokio::io::ReadBuf::new(&mut received);
                    match observer.poll_peek(cx, &mut unread) {
                        Poll::Pending => Poll::Ready(()),
                        Poll::Ready(result) => {
                            assert_eq!(result.unwrap(), 1);
                            Poll::Pending
                        }
                    }
                }))
                .await;
            }
            unreachable!("响应头至少有一个字节")
        })
        .await
        .expect("逐字节回环响应测试超时")
    }

    #[tokio::test]
    async fn chunked_response_waits_across_split_sizes_and_data_tails() {
        let response = read_bytewise_chunked(b"3;ext=1\r\nabc\r\n2\r\nde\r\n0\r\n\r\n").await;
        assert_eq!(response.body, b"abcde");
    }

    #[tokio::test]
    async fn chunked_response_waits_for_trailer_terminator() {
        let response = read_bytewise_chunked(b"0\r\nX-Fixture: done\r\n\r\n").await;
        assert!(response.body.is_empty());
    }

    #[tokio::test]
    async fn chunked_response_rejects_truncated_frames_at_eof() {
        for body in [
            &b"3\r"[..],
            &b"3\r\nab"[..],
            &b"3\r\nabc\r"[..],
            &b"0\r\n"[..],
            &b"0\r\nX-Fixture: done\r\n"[..],
        ] {
            let response = read_through_eof(&[CHUNKED_HEAD, body].concat()).await;
            let error = response.expect_err("EOF 前必须收到完整的 chunked 帧");
            assert_eq!(error.code, codes::CONNECT_FAILED, "截断帧: {body:?}");
        }
    }

    #[tokio::test]
    async fn response_rejects_transfer_encoding_with_content_length() {
        let wire = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 12\r\n\r\n2\r\n{}\r\n0\r\n\r\n";
        let error = read_through_eof(wire).await.expect_err("TE 与 CL 不可并存");
        assert_eq!(error.code, codes::CONNECT_FAILED);
    }

    #[tokio::test]
    async fn response_rejects_conflicting_content_lengths() {
        let wire = b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n{}";
        let error = read_through_eof(wire).await.expect_err("两个 CL 不可冲突");
        assert_eq!(error.code, codes::CONNECT_FAILED);
    }

    #[tokio::test]
    async fn chunked_response_rejects_chunk_size_over_body_budget() {
        let size_line = format!("{:x}\r\n", codec::MAX_JSON_FRAME_BYTES + 1);
        let wire = [CHUNKED_HEAD, size_line.as_bytes()].concat();
        let error = read_through_eof(&wire).await.expect_err("块声明超过体预算");
        assert_eq!(error.code, codes::FRAME_OVERSIZE);
    }

    #[tokio::test]
    async fn response_reads_legal_content_length_body() {
        let wire = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
        let response = read_through_eof(wire).await.unwrap();
        assert_eq!(response.body, b"{}");
    }

    #[tokio::test]
    async fn partial_write_disconnect_is_write_phase() {
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let authority = listener.local_addr().unwrap().to_string();
        let host = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut prefix = [0; 12];
            socket.read_exact(&mut prefix).await.unwrap();
            assert_eq!(&prefix, b"POST /rpc HT");
            // 未读完的请求使关闭产生 TCP reset。请求体大于环回连接的发送缓冲，
            // 因而不可能在宿主读完前缀并关闭之前整帧交付。
        });
        let result = start(
            &authority,
            "/rpc",
            "fixture=cookie",
            vec![b'x'; 32 * 1024 * 1024],
            Duration::from_secs(3),
            Duration::from_secs(3),
        )
        .await;
        let error = result.err().expect("部分写后断连必须使交付失败");
        assert_eq!(error.phase, StartPhase::Write);
        assert_eq!(error.error.code, codes::CONNECTION_CLOSED);
        host.await.unwrap();
    }

    #[tokio::test]
    async fn refused_connection_is_connect_phase() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let authority = listener.local_addr().unwrap().to_string();
        // macOS 上仅 bind 的 socket 可能让 connect 等到超时。释放动态端口，
        // 在同一任务的下一步即建连，以实际 ECONNREFUSED 覆盖分类。
        drop(listener);
        let error = start(
            &authority,
            "/rpc",
            "fixture=cookie",
            b"{}".to_vec(),
            Duration::from_secs(3),
            Duration::from_secs(3),
        )
        .await
        .err()
        .expect("无人监听必须在建连阶段失败");
        assert_eq!(error.phase, StartPhase::Connect);
        assert_eq!(error.error.code, codes::CONNECT_FAILED);
    }

    #[test]
    fn request_head_has_exact_content_length() {
        let head = build_request_head("/api/send", "127.0.0.1:1", "c=1", 5);
        let text = String::from_utf8(head).unwrap();
        assert!(text.starts_with("POST /api/send HTTP/1.1\r\n"));
        assert!(text.contains("Content-Length: 5\r\n"));
        assert!(text.contains("Connection: close\r\n"));
        assert!(text.ends_with("\r\n\r\n"));
    }

    #[test]
    fn chunked_decode_roundtrip() {
        let body = b"hello";
        let wire = format!(
            "{:x}\r\n{}\r\n0\r\n\r\n",
            body.len(),
            std::str::from_utf8(body).unwrap()
        );
        let decoded = try_decode_chunked(wire.as_bytes(), 1024).unwrap().unwrap();
        assert_eq!(decoded, body);
    }

    #[test]
    fn chunked_decode_multi_and_extensions() {
        let wire = b"3;ext=1\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        let decoded = try_decode_chunked(wire, 1024).unwrap().unwrap();
        assert_eq!(decoded, b"abcde");
    }

    #[test]
    fn chunked_decode_needs_more_then_completes() {
        let wire = b"5\r\nhello\r\n0\r\n\r\n";
        assert!(try_decode_chunked(&wire[..6], 1024).is_none());
        let decoded = try_decode_chunked(wire, 1024).unwrap().unwrap();
        assert_eq!(decoded, b"hello");
    }

    #[test]
    fn chunked_decode_rejects_over_cap_and_malformed() {
        let wire = b"ff\r\nx\r\n0\r\n\r\n";
        assert!(try_decode_chunked(wire, 4).unwrap().is_err());
        let bad = b"zz\r\nx\r\n0\r\n\r\n";
        assert!(try_decode_chunked(bad, 1024).unwrap().is_err());
    }

    #[test]
    fn find_subslice() {
        assert_eq!(find(b"abc\r\n\r\nx", b"\r\n\r\n"), Some(3));
        assert_eq!(find(b"abc", b"\r\n\r\n"), None);
    }
}
