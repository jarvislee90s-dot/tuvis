//! 只读服务发现与会话绑定（计划任务 2）。
//!
//! 只消费**默认后台 service** 的登记文件（`$XDG_STATE_HOME/opencode/service.json`）与
//! 调用方已取得的 `/api/info`、原生会话 JSON：本模块不 spawn 进程、不发 HTTP、不写文件。
//! 未登记（`--standalone` / 私有 TUI 形态）即不可接入，**不回退**去猜端口。
//!
//! 绑定按「卡片 id + realpath 目录 + 服务代际」三重核对，任一不成立即拒绝：绑错会话
//! 就是往错误的会话写消息，宁可拒绝。

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::session::Session;

use super::types::{
    codes, DiscoveryInput, NativeError, NativeTarget, RuntimeKey, ServiceDescriptor,
};

/// 登记文件里本端实际消费的字段。多余字段（如 `registeredAt`）忽略，缺必需字段即非法。
#[derive(Deserialize)]
struct Registration {
    id: String,
    version: String,
    url: String,
    pid: u32,
    password: String,
}

fn invalid_registration() -> NativeError {
    NativeError::new(
        codes::OC_SERVICE_REGISTRATION_INVALID,
        "opencode service registration is invalid",
    )
}

/// 登记摘要（F4 恢复链消费）：恢复载荷需要端点与密码来构造显式连接形态
/// （`--server <endpoint>` + `OPENCODE_PASSWORD`）。与 [`Registration`] 同一形状
/// 校验（id/version 非空、password 非空），只取恢复所需字段。G2：`pid` 一并对出，
/// 供恢复链「登记进程身份核验」消费（登记无 lstart 字段，等价强核验 = 进程事实
/// 可读性，见 resume.rs 起窗前三核对）。
pub(crate) struct RegistrationSummary {
    pub endpoint: String,
    pub password: String,
    pub pid: u32,
}

/// 解析登记字节为 [`RegistrationSummary`]：形状非法（缺必需字段/空值）→ None，
/// 调用方回落自托管形态（与 [`discover`] 的 fail-closed 同义，不猜端点）。
pub(crate) fn parse_registration_summary(bytes: &[u8]) -> Option<RegistrationSummary> {
    let registration: Registration = serde_json::from_slice(bytes).ok()?;
    if registration.id.trim().is_empty()
        || registration.version.trim().is_empty()
        || registration.password.is_empty()
    {
        return None;
    }
    // 端点最小合法性（与 discover 的 URL 规则同源：http(s) + 有端口）；回环判定
    // 交由消费方（G2 起窗前三核对在恢复链前置同一规则，观察面用 discover 全链）。
    let endpoint = reqwest::Url::parse(&registration.url).ok()?;
    if !matches!(endpoint.scheme(), "http" | "https") || endpoint.port().is_none() {
        return None;
    }
    Some(RegistrationSummary {
        endpoint: registration.url,
        password: registration.password,
        pid: registration.pid,
    })
}

fn invalid_endpoint() -> NativeError {
    NativeError::new(
        codes::OC_ENDPOINT_INVALID,
        "opencode service endpoint is malformed",
    )
}

fn not_loopback() -> NativeError {
    NativeError::new(
        codes::OC_ENDPOINT_NOT_LOOPBACK,
        "opencode service endpoint is not loopback",
    )
}

fn generation_mismatch() -> NativeError {
    NativeError::new(
        codes::OC_GENERATION_MISMATCH,
        "opencode service generation does not match registration",
    )
}

fn unresolved_directory() -> NativeError {
    NativeError::new(
        codes::OC_DIRECTORY_UNRESOLVED,
        "session directory cannot be resolved",
    )
}

/// 从登记文件读出端点与凭据，并结合进程启动时间形成服务代际。
///
/// 只读：仅消费注入的 `read_file`（且只读 `input.state_file`）；不启动、不重启、不探测
/// 任何端口。拿不到 `process_fact(pid)` 的启动时间即拒绝——用 0 或当前时间会让重启后的
/// 旧绑定复活。
pub fn discover(
    input: &DiscoveryInput,
    read_file: &dyn Fn(&Path) -> Result<Vec<u8>, NativeError>,
    process_fact: &dyn Fn(u32) -> Option<i64>,
) -> Result<ServiceDescriptor, NativeError> {
    let bytes = read_file(&input.state_file).map_err(|_| {
        NativeError::new(
            codes::OC_SERVICE_NOT_REGISTERED,
            "opencode background service is not registered",
        )
    })?;
    let registration: Registration =
        serde_json::from_slice(&bytes).map_err(|_| invalid_registration())?;
    if registration.id.trim().is_empty()
        || registration.version.trim().is_empty()
        || registration.password.is_empty()
    {
        return Err(invalid_registration());
    }

    let endpoint = reqwest::Url::parse(&registration.url).map_err(|_| invalid_endpoint())?;
    if !matches!(endpoint.scheme(), "http" | "https") || endpoint.port().is_none() {
        return Err(invalid_endpoint());
    }
    // 端点内嵌认证信息（`http://user:pass@...`）一律拒绝：`ServiceDescriptor` 的 Debug 会
    // 打印 endpoint，内嵌凭据会随日志外泄；凭据只允许来自登记文件的 password 字段。
    if !endpoint.username().is_empty() || endpoint.password().is_some() {
        return Err(invalid_endpoint());
    }
    if !is_loopback(&endpoint) {
        return Err(not_loopback());
    }

    let started_at_ms = process_fact(registration.pid).ok_or_else(|| {
        NativeError::new(
            codes::OC_RUNTIME_UNVERIFIED,
            "opencode service process start time is unavailable",
        )
    })?;

    Ok(ServiceDescriptor {
        runtime: RuntimeKey {
            service_id: registration.id,
            pid: registration.pid,
            started_at_ms,
        },
        endpoint,
        password: registration.password,
        version: registration.version,
        explicit: false,
    })
}

/// 显式连接形态发现（R0 §4d 单列，I3）：证据 = 目标会话的桌面进程 argv
/// （可执行名 `opencode` + `--server <url>` + `--session <sid>`，生产扫描还要求到
/// server 端口的 ESTABLISHED 实连）+ 进程事实（启动时间）+ 进程环境
/// `OPENCODE_PASSWORD`。**不读登记文件**（该形态不写登记）。
///
/// 显式形态的**客户端绑定证据**（R0 §4d 单列）：argv 指认的端点 + 客户端进程
/// 环境里的凭据。`client_pid` 只是诊断信息——**绝不**进入服务 `RuntimeKey`：
/// 显式连接是「独立 TUI 客户端 → 独立服务」，客户端 PID ≠ 服务 PID。
#[derive(Clone)]
pub struct ExplicitClientEvidence {
    pub endpoint: reqwest::Url,
    pub password: String,
    /// argv 匹配到的**客户端**进程 pid（不作为服务身份；仅诊断/日志用途）。
    pub client_pid: u32,
}

impl std::fmt::Debug for ExplicitClientEvidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExplicitClientEvidence")
            .field("endpoint", &self.endpoint)
            .field("client_pid", &self.client_pid)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// 显式形态 argv 扫描缝：目标会话 → [(客户端 pid, 完整 argv)]。
pub type ExplicitArgvScan<'a> = &'a dyn Fn(&str) -> Vec<(u32, Vec<String>)>;
/// 显式形态进程环境读取缝（客户端进程环境）。
pub type ExplicitEnvRead<'a> = &'a dyn Fn(u32, &str) -> Option<String>;

/// 显式形态第 1 步：从**客户端**进程 argv/环境取绑定证据（端点 + 凭据）。
///
/// 0 个候选 = 该会话没有显式实例（`OC_SERVICE_NOT_REGISTERED`）；多个候选 =
/// 无法证明固定执行者（`OC_INSTANCE_AMBIGUOUS`）；凭据缺失 = fail-closed。
/// 端点沿用登记 URL 同一套规则（http(s) + 有端口 + 回环 + 拒内嵌凭据）。
pub fn discover_explicit_client(
    session_id: &str,
    scan: ExplicitArgvScan<'_>,
    read_env: ExplicitEnvRead<'_>,
) -> Result<ExplicitClientEvidence, NativeError> {
    let mut candidates = scan(session_id);
    let (client_pid, argv) = match candidates.len() {
        0 => {
            return Err(NativeError::new(
                codes::OC_SERVICE_NOT_REGISTERED,
                "no explicit opencode desktop instance is bound to this session",
            ))
        }
        1 => candidates.remove(0),
        _ => {
            return Err(NativeError::new(
                codes::OC_INSTANCE_AMBIGUOUS,
                "multiple explicit opencode desktop instances target this session",
            ))
        }
    };
    let server = argv
        .iter()
        .position(|arg| arg == "--server")
        .and_then(|index| argv.get(index + 1))
        .ok_or_else(invalid_endpoint)?;
    let endpoint = reqwest::Url::parse(server).map_err(|_| invalid_endpoint())?;
    if !matches!(endpoint.scheme(), "http" | "https") || endpoint.port().is_none() {
        return Err(invalid_endpoint());
    }
    if !endpoint.username().is_empty() || endpoint.password().is_some() {
        return Err(invalid_endpoint());
    }
    if !is_loopback(&endpoint) {
        return Err(not_loopback());
    }
    let password = read_env(client_pid, "OPENCODE_PASSWORD")
        .filter(|password| !password.is_empty())
        .ok_or_else(|| {
            NativeError::new(
                codes::OC_RUNTIME_UNVERIFIED,
                "explicit opencode credential is unavailable",
            )
        })?;
    Ok(ExplicitClientEvidence {
        endpoint,
        password,
        client_pid,
    })
}

/// 显式形态的服务身份锚（第 2 步的核对规则）：服务自身 `/api/info` 必须报告
/// pid 与非空 version，且 `urls` 含 argv 指认端点的 origin；返回 `(服务 pid, version)`。
///
/// **客户端 PID 不参与**：服务代际 = 服务自身 pid +（调用方补的）进程启动时间 +
/// 端点。version 来自服务自报（无登记锚，[`verify_service_info`] 跳过版本互证），
/// 但 pid/urls 规则与普通形态相同——后续读取与本次锚不一致即按代际拒绝。
pub fn explicit_service_identity(
    info: &serde_json::Value,
    endpoint: &reqwest::Url,
) -> Result<(u32, String), NativeError> {
    let service_pid = info
        .get("pid")
        .and_then(|value| value.as_u64())
        .filter(|pid| *pid > 0 && *pid <= u64::from(u32::MAX))
        .ok_or_else(generation_mismatch)? as u32;
    let version = info
        .get("version")
        .and_then(|value| value.as_str())
        .map(str::to_owned)
        .filter(|version| !version.trim().is_empty())
        .ok_or_else(|| {
            NativeError::new(
                codes::OC_VERSION_MISMATCH,
                "explicit opencode service version is unavailable",
            )
        })?;
    if !info_lists_endpoint(info, endpoint) {
        return Err(generation_mismatch());
    }
    Ok((service_pid, version))
}

/// 生产路径解析：`XDG_STATE_HOME` 优先，否则 `$HOME/.local/state`（XDG 规范：空串视为未设）。
pub fn state_file_for(home: &Path, xdg_state_home: Option<&Path>) -> PathBuf {
    let base = match xdg_state_home {
        Some(path) if !path.as_os_str().is_empty() => path.to_path_buf(),
        _ => home.join(".local").join("state"),
    };
    base.join("opencode").join("service.json")
}

/// 生产发现输入：只读环境变量解析路径，不改进程环境（测试显式构造 `DiscoveryInput`）。
pub fn production_input() -> DiscoveryInput {
    production_input_from(&|key| std::env::var_os(key))
}

/// 环境管线的可测缝：`lookup` 注入环境读取（生产传 [`std::env::var_os`] 的包装）。
///
/// 拆出来的理由：`state_file_for` 的纯函数分支已被单测覆盖，但**环境管线**
/// （读平台用户目录 / `XDG_STATE_HOME`、空串按未设处理、用户目录缺失时的行为）
/// 只能靠注入读取器验证——真实 `std::env` 在测试里改不得（并行测试会互相污染）。
pub fn production_input_from(
    lookup: &dyn Fn(&str) -> Option<std::ffi::OsString>,
) -> DiscoveryInput {
    production_input_for_platform(lookup, cfg!(windows))
}

fn production_input_for_platform(
    lookup: &dyn Fn(&str) -> Option<std::ffi::OsString>,
    windows: bool,
) -> DiscoveryInput {
    // Windows 的上游用户目录来自 USERPROFILE；Git Bash HOME 可能指向另一服务。
    let home_key = if windows { "USERPROFILE" } else { "HOME" };
    let home = lookup(home_key).map(PathBuf::from).unwrap_or_default();
    let xdg_state_home = lookup("XDG_STATE_HOME").map(PathBuf::from);
    DiscoveryInput {
        state_file: state_file_for(&home, xdg_state_home.as_deref()),
        home,
    }
}

/// 端点必须是本机回环：`127.0.0.0/8`、`::1`、`localhost`。
/// G2：恢复链起窗前三核对复用同一判据（pub(crate)，不再各写一份）。
pub(crate) fn is_loopback(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    match bare.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => host.eq_ignore_ascii_case("localhost"),
    }
}

/// `/api/info` 与服务代际的三重互证：pid 逐字一致、version 与登记一致、urls 含登记
/// 端点 origin。bind_session 与 claim 前的写通道重核共用同一规则（C1）。
///
/// `expect_version = false`（显式连接形态，无登记版本锚）时跳过 version 互证——
/// 该形态下 version 只来自 `/api/info` 自报，没有第二个来源可比。
pub(crate) fn verify_service_info(
    info: &serde_json::Value,
    descriptor: &ServiceDescriptor,
) -> Result<(), NativeError> {
    let info_pid = info
        .get("pid")
        .and_then(|value| value.as_u64())
        .ok_or_else(generation_mismatch)?;
    if info_pid != u64::from(descriptor.runtime.pid) {
        return Err(generation_mismatch());
    }
    let info_version = info
        .get("version")
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            NativeError::new(
                codes::OC_VERSION_MISMATCH,
                "opencode service version is unavailable",
            )
        })?;
    if !descriptor.explicit && info_version != descriptor.version {
        return Err(NativeError::new(
            codes::OC_VERSION_MISMATCH,
            "opencode service version does not match registration",
        ));
    }
    if !info_lists_endpoint(info, &descriptor.endpoint) {
        return Err(generation_mismatch());
    }
    Ok(())
}

/// `/api/info.urls` 是否含登记端点的 origin（scheme + host + port）。
fn info_lists_endpoint(info: &serde_json::Value, endpoint: &reqwest::Url) -> bool {
    let Some(urls) = info.get("urls").and_then(|value| value.as_array()) else {
        return false;
    };
    let expected = endpoint.origin();
    urls.iter()
        .filter_map(|value| value.as_str())
        .filter_map(|raw| reqwest::Url::parse(raw).ok())
        .any(|url| url.origin() == expected)
}

fn canonical_directory(raw: &str) -> Result<PathBuf, NativeError> {
    if raw.is_empty() {
        return Err(unresolved_directory());
    }
    let directory = Path::new(raw)
        .canonicalize()
        .map_err(|_| unresolved_directory())?;
    if !directory.is_dir() {
        return Err(unresolved_directory());
    }
    Ok(directory)
}

/// 把兔维斯会话卡绑定到原生会话。
///
/// 三重核对全部成立才返回：服务代际（pid + 版本 + origin）、卡片 id 逐字相等、
/// 两侧目录 realpath 后相等。不以 active/viewed 之类的活跃度字段选目标。
pub fn bind_session(
    session: &Session,
    descriptor: &ServiceDescriptor,
    info: &serde_json::Value,
    native_session: &serde_json::Value,
) -> Result<NativeTarget, NativeError> {
    if !is_loopback(&descriptor.endpoint) {
        return Err(not_loopback());
    }

    verify_service_info(info, descriptor)?;

    // 真实 2.0.22 的 `GET /api/session/{id}` 把会话对象包在 `data` 里；旧夹具/回放是
    // 裸对象。与 `contract::parse_form` 相同的容忍规则：只有 `data` 是对象才解包。
    let native = native_session
        .get("data")
        .filter(|inner| inner.is_object())
        .unwrap_or(native_session);

    let native_id = native
        .get("id")
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            NativeError::new(
                codes::OC_SESSION_MISMATCH,
                "native session id does not match the card",
            )
        })?;
    if native_id != session.id {
        return Err(NativeError::new(
            codes::OC_SESSION_MISMATCH,
            "native session id does not match the card",
        ));
    }

    let card_directory = canonical_directory(&session.project_path)?;
    let native_directory = native
        .pointer("/location/directory")
        .and_then(|value| value.as_str())
        .ok_or_else(unresolved_directory)?;
    let native_directory = canonical_directory(native_directory)?;
    if card_directory != native_directory {
        return Err(NativeError::new(
            codes::OC_DIRECTORY_MISMATCH,
            "native session directory does not match the card",
        ));
    }

    Ok(NativeTarget {
        runtime: descriptor.runtime.clone(),
        session_id: session.id.clone(),
        directory: card_directory,
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::services::opencode_native::types::DiscoveryInput;
    use crate::session::{AgentType, ProcessForm, SessionStatus};

    /// 测试专用凭据：任何 Debug/错误文案里出现即失败。
    const PASSWORD: &str = "s3cret-credential-value";
    const SERVICE_ID: &str = "87bd7e45-27bf-490d-bced-58d79c108910";
    const PID: u32 = 60541;
    const PORT: u16 = 49617;
    const VERSION: &str = "2.0.22";
    const STARTED_AT_MS: i64 = 1_793_289_345_000;

    fn registration(id: &str, pid: u32, url: &str, version: &str) -> String {
        format!(
            r#"{{"id":"{id}","version":"{version}","url":"{url}","pid":{pid},"password":"{PASSWORD}"}}"#
        )
    }

    fn loopback_registration(id: &str, pid: u32, port: u16, version: &str) -> String {
        registration(id, pid, &format!("http://127.0.0.1:{port}"), version)
    }

    fn api_info(pid: u32, port: u16, version: &str) -> serde_json::Value {
        serde_json::json!({
            "version": version,
            "pid": pid,
            "urls": [format!("http://127.0.0.1:{port}")],
        })
    }

    fn native_session(id: &str, directory: &Path) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "location": { "directory": directory.to_string_lossy() },
        })
    }

    /// 真实 OpenCode 2.0.22 的 `GET /api/session/{id}` 形态：单会话对象在 `data` 里。
    fn enveloped_native_session(id: &str, directory: &Path) -> serde_json::Value {
        serde_json::json!({ "data": native_session(id, directory) })
    }

    /// 卡片：`Session.id` 即 `session_v2.id`（`monitor/opencode_parser.rs` 直接取值）。
    fn card(id: &str, directory: &Path) -> Session {
        Session {
            id: id.to_string(),
            agent_type: AgentType::OpenCode,
            project_name: "project".to_string(),
            project_path: directory.to_string_lossy().into_owned(),
            title: Some("BOK".to_string()),
            git_branch: None,
            github_url: None,
            status: SessionStatus::Idle,
            last_message: None,
            last_message_role: None,
            last_message_subagent_report: false,
            flap_from_subagent_activity: false,
            last_activity_at: "2026-10-29T23:36:43Z".to_string(),
            pid: 12345,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: ProcessForm::Cli,
            jump_supported: true,
            unread: false,
            unread_turned_green_at_ms: None,
        }
    }

    fn input_for(dir: &Path) -> DiscoveryInput {
        DiscoveryInput {
            state_file: dir.join("state/opencode/service.json"),
            home: dir.join("home"),
        }
    }

    /// 任务 0 的脱敏夹具原文（与任务 1 共用同一目录，只保留发现所需字段）。
    fn fixture_raw(name: &str) -> String {
        let path = format!(
            "{}/tests/fixtures/opencode-native/{}",
            env!("CARGO_MANIFEST_DIR"),
            name
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
    }

    fn fixture_json(name: &str) -> serde_json::Value {
        let raw = fixture_raw(name);
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {name}: {e}"))
    }

    /// 记录型假读源：断言只读给定 state_file、且只读一次。
    struct FakeSource {
        body: Result<String, NativeError>,
        reads: RefCell<Vec<PathBuf>>,
    }

    impl FakeSource {
        fn ok(body: String) -> Self {
            Self {
                body: Ok(body),
                reads: RefCell::new(Vec::new()),
            }
        }

        fn missing() -> Self {
            Self {
                body: Err(NativeError::new("OC_IO", "no such file or directory")),
                reads: RefCell::new(Vec::new()),
            }
        }

        fn read(&self, path: &Path) -> Result<Vec<u8>, NativeError> {
            self.reads.borrow_mut().push(path.to_path_buf());
            match &self.body {
                Ok(body) => Ok(body.clone().into_bytes()),
                Err(err) => Err(err.clone()),
            }
        }

        fn paths(&self) -> Vec<PathBuf> {
            self.reads.borrow().clone()
        }
    }

    fn discover_ok(input: &DiscoveryInput, body: String) -> (ServiceDescriptor, FakeSource) {
        let source = FakeSource::ok(body);
        let read = |path: &Path| source.read(path);
        let descriptor = discover(input, &read, &|_pid| Some(STARTED_AT_MS))
            .expect("登记文件合法时必须发现成功");
        (descriptor, source)
    }

    /// 同目录的另一张卡不得被选中：id 逐字相等是唯一依据，目录相同不足以绑定。
    #[test]
    fn same_directory_does_not_select_another_session() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let input = input_for(tmp.path());
        // 用任务 0 脱敏夹具（含 `x-evidence` 等无关字段）：只校验实际消费字段。
        let (descriptor, _source) = discover_ok(&input, fixture_raw("service.registration.json"));
        assert_eq!(descriptor.runtime.service_id, SERVICE_ID);
        assert_eq!(descriptor.runtime.pid, PID);
        assert_eq!(descriptor.version, VERSION);
        let info = fixture_json("api.info.json");

        // 正向对照：id 一致时绑定成功，证明拒绝来自 id 而不是目录。
        let bound = bind_session(
            &card("ses_A", &project),
            &descriptor,
            &info,
            &native_session("ses_A", &project),
        )
        .expect("id/目录/代际一致时必须绑定成功");
        assert_eq!(bound.session_id, "ses_A");
        assert_eq!(bound.runtime, descriptor.runtime);

        // 反例：同目录、同服务代际，但卡片是 A、原生响应是 B。
        let err = bind_session(
            &card("ses_A", &project),
            &descriptor,
            &info,
            &native_session("ses_B", &project),
        )
        .expect_err("同目录不同 id 必须拒绝");
        assert_eq!(err.code, codes::OC_SESSION_MISMATCH);
        assert!(!err.message.contains("ses_A") && !err.message.contains("ses_B"));
    }

    /// 真实 2.0.22 单会话读是 `{"data":{...}}`：绑定必须从 envelope 根取 id 与目录，
    /// 且返回目录是归一后的 realpath（不是原始字符串）。
    #[test]
    fn real_service_envelope_binds_and_canonicalizes_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let input = input_for(tmp.path());
        let (descriptor, _source) = discover_ok(
            &input,
            loopback_registration(SERVICE_ID, PID, PORT, VERSION),
        );
        let info = api_info(PID, PORT, VERSION);
        let expected = project.canonicalize().unwrap();

        let target = bind_session(
            &card("ses_x", &project),
            &descriptor,
            &info,
            &enveloped_native_session("ses_x", &project),
        )
        .expect("真实 data envelope 必须绑定成功");
        assert_eq!(target.session_id, "ses_x");
        assert_eq!(target.directory, expected, "目录必须是归一后的 realpath");
        assert_eq!(target.runtime, descriptor.runtime);
    }

    /// envelope 内的 id 与卡片不一致：仍按 id 不符拒绝，不能因为包了一层就放行。
    #[test]
    fn envelope_with_different_id_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let input = input_for(tmp.path());
        let (descriptor, _source) = discover_ok(
            &input,
            loopback_registration(SERVICE_ID, PID, PORT, VERSION),
        );
        let info = api_info(PID, PORT, VERSION);

        let err = bind_session(
            &card("ses_A", &project),
            &descriptor,
            &info,
            &enveloped_native_session("ses_B", &project),
        )
        .expect_err("envelope 内同目录不同 id 必须拒绝");
        assert_eq!(err.code, codes::OC_SESSION_MISMATCH);
        assert!(!err.message.contains("ses_A") && !err.message.contains("ses_B"));
    }

    /// envelope 内 location 错误或缺失：分别保持目录不符与目录不可解析的既有错误码。
    #[test]
    fn envelope_location_mismatch_or_missing_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let other = tmp.path().join("other");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let input = input_for(tmp.path());
        let (descriptor, _source) = discover_ok(
            &input,
            loopback_registration(SERVICE_ID, PID, PORT, VERSION),
        );
        let info = api_info(PID, PORT, VERSION);

        let err = bind_session(
            &card("ses_A", &project),
            &descriptor,
            &info,
            &enveloped_native_session("ses_A", &other),
        )
        .expect_err("envelope 内同 id 不同目录必须拒绝");
        assert_eq!(err.code, codes::OC_DIRECTORY_MISMATCH);

        // 缺整个 location，以及 location 存在但缺 directory：都不可解析，不默认落到 HOME。
        for body in [
            serde_json::json!({ "data": { "id": "ses_A" } }),
            serde_json::json!({ "data": { "id": "ses_A", "location": {} } }),
        ] {
            let err = bind_session(&card("ses_A", &project), &descriptor, &info, &body)
                .expect_err("envelope 缺原生目录必须拒绝");
            assert_eq!(err.code, codes::OC_DIRECTORY_UNRESOLVED, "body={body}");
        }
    }

    /// 旧夹具的无 envelope 形态与 `data` 非对象的形态都必须继续可用：
    /// 只有 `data` 是对象时才解包，否则按原 body 读取。
    #[test]
    fn unwrapped_and_non_object_data_shapes_still_bind() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let input = input_for(tmp.path());
        let (descriptor, _source) = discover_ok(
            &input,
            loopback_registration(SERVICE_ID, PID, PORT, VERSION),
        );
        let info = api_info(PID, PORT, VERSION);

        for body in [
            native_session("ses_A", &project),
            serde_json::json!({
                "id": "ses_A",
                "location": { "directory": project.to_string_lossy() },
                "data": [],
            }),
        ] {
            let target = bind_session(&card("ses_A", &project), &descriptor, &info, &body)
                .expect("无 envelope / data 非对象时必须按原 body 绑定");
            assert_eq!(target.session_id, "ses_A");
        }
    }

    /// 同端口新代际：service id / pid / 启动时间任一变化都必须换 RuntimeKey，旧绑定失效。
    #[test]
    fn same_port_new_generation_invalidates_binding() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let input = input_for(tmp.path());

        let old = FakeSource::ok(loopback_registration(SERVICE_ID, PID, PORT, VERSION));
        let old_read = |path: &Path| old.read(path);
        let before = discover(&input, &old_read, &|pid| {
            (pid == PID).then_some(STARTED_AT_MS)
        })
        .expect("旧代际登记可发现");

        // 同端口、新 service id、新 pid、新启动时间（密码轮换不影响代际判定）。
        let new = FakeSource::ok(loopback_registration(
            "3f1c8a6e-0000-4a2b-9c11-77aa55bb11cc",
            PID + 9_000,
            PORT,
            VERSION,
        ));
        let new_read = |path: &Path| new.read(path);
        let after = discover(&input, &new_read, &|pid| {
            (pid == PID + 9_000).then_some(STARTED_AT_MS + 60_000)
        })
        .expect("新代际登记可发现");

        assert_eq!(before.endpoint, after.endpoint, "端口相同");
        assert_ne!(before.runtime, after.runtime, "同端口不同代际必须可区分");
        assert_ne!(before.runtime.service_id, after.runtime.service_id);
        assert_ne!(before.runtime.pid, after.runtime.pid);
        assert_ne!(before.runtime.started_at_ms, after.runtime.started_at_ms);

        // 旧描述符 + 新代际 /api/info：拒绝，旧绑定不得复活。
        let err = bind_session(
            &card("ses_A", &project),
            &before,
            &api_info(PID + 9_000, PORT, VERSION),
            &native_session("ses_A", &project),
        )
        .expect_err("旧描述符遇到新 pid 必须拒绝");
        assert_eq!(err.code, codes::OC_GENERATION_MISMATCH);

        // 同登记同启动时间重复发现必须稳定（不引入随机性）。
        let again = FakeSource::ok(loopback_registration(SERVICE_ID, PID, PORT, VERSION));
        let again_read = |path: &Path| again.read(path);
        let repeated = discover(&input, &again_read, &|pid| {
            (pid == PID).then_some(STARTED_AT_MS)
        })
        .expect("重复发现成功");
        assert_eq!(repeated.runtime, before.runtime);
    }

    /// 非回环端点一律拒绝：原生服务只允许本机回环。
    #[test]
    fn non_loopback_descriptor_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let input = input_for(tmp.path());

        for url in [
            "http://10.0.0.5:49617",
            "http://192.168.1.20:49617",
            "http://example.com:49617",
        ] {
            let source = FakeSource::ok(registration(SERVICE_ID, PID, url, VERSION));
            let read = |path: &Path| source.read(path);
            let err = discover(&input, &read, &|_pid| Some(STARTED_AT_MS))
                .expect_err("非回环端点必须拒绝");
            assert_eq!(err.code, codes::OC_ENDPOINT_NOT_LOOPBACK, "url={url}");
            assert!(!err.message.contains(PASSWORD));
        }

        // 手工构造的非回环描述符在绑定时同样被拒（不只依赖 discover）。
        let foreign = ServiceDescriptor {
            runtime: crate::services::opencode_native::types::RuntimeKey {
                service_id: SERVICE_ID.to_string(),
                pid: PID,
                started_at_ms: STARTED_AT_MS,
            },
            endpoint: reqwest::Url::parse("http://10.0.0.5:49617").unwrap(),
            password: PASSWORD.to_string(),
            version: VERSION.to_string(),
            explicit: false,
        };
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let err = bind_session(
            &card("ses_A", &project),
            &foreign,
            &api_info(PID, PORT, VERSION),
            &native_session("ses_A", &project),
        )
        .expect_err("非回环描述符不得绑定");
        assert_eq!(err.code, codes::OC_ENDPOINT_NOT_LOOPBACK);
    }

    /// 登记缺失 = 默认 service 未运行：不得回退去猜私有 TUI 端口。
    #[test]
    fn missing_registration_is_not_private_tui_attachment() {
        let tmp = tempfile::tempdir().unwrap();
        let input = input_for(tmp.path());
        let source = FakeSource::missing();
        let read = |path: &Path| source.read(path);

        let err = discover(&input, &read, &|_pid| Some(STARTED_AT_MS))
            .expect_err("登记缺失必须失败，不能自行找端口");
        assert_eq!(err.code, codes::OC_SERVICE_NOT_REGISTERED);
        assert!(
            !err.message.to_lowercase().contains("standalone")
                && !err.message.to_lowercase().contains("tui")
                && !err.message.to_lowercase().contains("port"),
            "错误文案不得暗示私有 TUI 接入：{}",
            err.message
        );
        // 只读给定 state_file，没有第二次尝试别的路径。
        assert_eq!(source.paths(), vec![input.state_file.clone()]);
    }

    /// 合成拒读输入，不修改文件权限或 ACL，且不可转读 config 文件/其它服务。
    #[test]
    fn t12_registration_access_denied_does_not_try_another_service() {
        let tmp = tempfile::tempdir().unwrap();
        let input = input_for(tmp.path());
        let reads = RefCell::new(Vec::new());
        let err = discover(
            &input,
            &|path| {
                reads.borrow_mut().push(path.to_path_buf());
                Err(NativeError::new("OC_IO", "permission denied"))
            },
            &|_| panic!("拒读后不查询进程"),
        )
        .unwrap_err();
        assert_eq!(err.code, codes::OC_SERVICE_NOT_REGISTERED);
        assert_eq!(*reads.borrow(), vec![input.state_file]);
    }

    #[test]
    fn t12_unregistered_server_cannot_borrow_registered_endpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let input = input_for(tmp.path());
        let (descriptor, _) = discover_ok(
            &input,
            loopback_registration(SERVICE_ID, PID, PORT, VERSION),
        );
        let project = tmp.path();
        // 合成：server/standalone 指向另一监听，即使 session ID 和目录相同也不能绑。
        let err = bind_session(
            &card("ses_A", project),
            &descriptor,
            &api_info(PID + 1, PORT + 1, VERSION),
            &native_session("ses_A", project),
        )
        .unwrap_err();
        assert_eq!(err.code, codes::OC_GENERATION_MISMATCH);
    }

    /// 凭据永不进 Debug / 错误文案。
    #[test]
    fn credential_never_serializes() {
        let tmp = tempfile::tempdir().unwrap();
        let input = input_for(tmp.path());
        let (descriptor, _source) = discover_ok(
            &input,
            loopback_registration(SERVICE_ID, PID, PORT, VERSION),
        );

        // `ServiceDescriptor` 未派生 Serialize/Debug：只能手写 Debug，且打码。
        let rendered = format!("{descriptor:?}");
        assert!(!rendered.contains(PASSWORD), "Debug 泄露凭据：{rendered}");
        assert!(rendered.contains("redacted"));
        assert!(rendered.contains(SERVICE_ID), "非敏感字段保留可诊断性");

        // 所有失败路径的文案都不得回显密码或原始登记内容。
        let broken = format!(r#"{{"id":"{SERVICE_ID}","password":"{PASSWORD}","#);
        let source = FakeSource::ok(broken);
        let read = |path: &Path| source.read(path);
        let err = discover(&input, &read, &|_pid| Some(STARTED_AT_MS)).unwrap_err();
        assert_eq!(err.code, codes::OC_SERVICE_REGISTRATION_INVALID);
        assert!(!err.message.contains(PASSWORD), "{}", err.message);

        let source = FakeSource::ok(registration(
            SERVICE_ID,
            PID,
            "http://10.0.0.5:49617",
            VERSION,
        ));
        let read = |path: &Path| source.read(path);
        let err = discover(&input, &read, &|_pid| Some(STARTED_AT_MS)).unwrap_err();
        assert!(!err.message.contains(PASSWORD), "{}", err.message);

        // 服务代际无法核验时也不回显。
        let source = FakeSource::ok(loopback_registration(SERVICE_ID, PID, PORT, VERSION));
        let read = |path: &Path| source.read(path);
        let err = discover(&input, &read, &|_pid| None).unwrap_err();
        assert_eq!(err.code, codes::OC_RUNTIME_UNVERIFIED);
        assert!(!err.message.contains(PASSWORD), "{}", err.message);
    }

    /// 目录 realpath 失败即拒绝，不拿原样字符串凑合。
    #[test]
    fn canonicalize_failure_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let input = input_for(tmp.path());
        let (descriptor, _source) = discover_ok(
            &input,
            loopback_registration(SERVICE_ID, PID, PORT, VERSION),
        );
        let info = api_info(PID, PORT, VERSION);
        let absent = tmp.path().join("does-not-exist");

        let err = bind_session(
            &card("ses_A", &project),
            &descriptor,
            &info,
            &native_session("ses_A", &absent),
        )
        .expect_err("原生目录不可归一必须拒绝");
        assert_eq!(err.code, codes::OC_DIRECTORY_UNRESOLVED);

        let err = bind_session(
            &card("ses_A", &absent),
            &descriptor,
            &info,
            &native_session("ses_A", &project),
        )
        .expect_err("卡片目录不可归一必须拒绝");
        assert_eq!(err.code, codes::OC_DIRECTORY_UNRESOLVED);

        // 缺 location.directory 同样拒绝，不默认落到 HOME。
        let err = bind_session(
            &card("ses_A", &project),
            &descriptor,
            &info,
            &serde_json::json!({ "id": "ses_A" }),
        )
        .expect_err("缺原生目录必须拒绝");
        assert_eq!(err.code, codes::OC_DIRECTORY_UNRESOLVED);
    }

    /// 目录这一重必须真的参与判定：同 id、两个各自存在的真实目录，归一后不等即拒绝。
    #[test]
    fn distinct_directories_are_refused_despite_same_id() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let input = input_for(tmp.path());
        let (descriptor, _source) = discover_ok(
            &input,
            loopback_registration(SERVICE_ID, PID, PORT, VERSION),
        );
        let info = api_info(PID, PORT, VERSION);

        // 正向对照：目录一致时绑定成功，证明拒绝来自目录而不是 id。
        let bound = bind_session(
            &card("ses_A", &first),
            &descriptor,
            &info,
            &native_session("ses_A", &first),
        )
        .expect("同 id 同目录必须绑定成功");
        assert_eq!(bound.session_id, "ses_A");

        // 两个方向都要拒（实现若只比一侧，单方向用例会漏掉）。
        for (card_dir, native_dir) in [(&first, &second), (&second, &first)] {
            let err = bind_session(
                &card("ses_A", card_dir),
                &descriptor,
                &info,
                &native_session("ses_A", native_dir),
            )
            .expect_err("同 id 不同目录必须拒绝");
            assert_eq!(err.code, codes::OC_DIRECTORY_MISMATCH);
            assert!(
                !err.message.contains(&*tmp.path().to_string_lossy()),
                "错误文案不得回显路径：{}",
                err.message
            );
        }
    }

    /// 目录比较必须走 realpath：符号链接与真实路径字面不同但指向同一目录时仍须绑定成功。
    ///
    /// 对应任务 0 的 `g1.binding.json` 场景——卡片记的是字面路径，原生报的是另一写法
    /// （macOS 上 `/tmp/x` 与 `/private/tmp/x` 即此类前缀差异）。
    #[test]
    fn realpath_equivalent_directories_bind_successfully() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        // 字面不同、realpath 相同：`link` 是指向 `project` 的符号链接。
        let link = tmp.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&project, &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&project, &link).unwrap();
        assert_ne!(link, project, "两条路径字面必须不同");

        let input = input_for(tmp.path());
        let (descriptor, _source) = discover_ok(
            &input,
            loopback_registration(SERVICE_ID, PID, PORT, VERSION),
        );
        let info = api_info(PID, PORT, VERSION);
        let expected = project.canonicalize().unwrap();

        // 卡片侧符号链接、原生侧真实路径。
        let target = bind_session(
            &card("ses_A", &link),
            &descriptor,
            &info,
            &native_session("ses_A", &project),
        )
        .expect("realpath 相同的两条路径必须绑定成功");
        assert_eq!(target.session_id, "ses_A");
        assert_eq!(
            target.directory, expected,
            "返回目录必须是归一后的 realpath"
        );

        // 反向同样成立：实现不得只归一其中一侧。
        let reversed = bind_session(
            &card("ses_A", &project),
            &descriptor,
            &info,
            &native_session("ses_A", &link),
        )
        .expect("反向也必须绑定成功");
        assert_eq!(reversed.directory, expected);
    }

    /// `/api/info.urls` 必须包含登记端点 origin，否则视为换了代际。
    #[test]
    fn info_url_mismatch_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let input = input_for(tmp.path());
        let (descriptor, _source) = discover_ok(
            &input,
            loopback_registration(SERVICE_ID, PID, PORT, VERSION),
        );

        let other_port = serde_json::json!({
            "version": VERSION,
            "pid": PID,
            "urls": [format!("http://127.0.0.1:{}", PORT + 1)],
        });
        let err = bind_session(
            &card("ses_A", &project),
            &descriptor,
            &other_port,
            &native_session("ses_A", &project),
        )
        .expect_err("urls 不含登记端点必须拒绝");
        assert_eq!(err.code, codes::OC_GENERATION_MISMATCH);

        let missing_urls = serde_json::json!({ "version": VERSION, "pid": PID });
        let err = bind_session(
            &card("ses_A", &project),
            &descriptor,
            &missing_urls,
            &native_session("ses_A", &project),
        )
        .expect_err("缺 urls 必须拒绝");
        assert_eq!(err.code, codes::OC_GENERATION_MISMATCH);
    }

    /// 版本只做两侧互证，不按具体版本号放行/拒绝。
    #[test]
    fn version_mismatch_is_refused_without_pinning_a_version() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let input = input_for(tmp.path());
        let (descriptor, _source) = discover_ok(
            &input,
            loopback_registration(SERVICE_ID, PID, PORT, "9.9.9"),
        );

        let err = bind_session(
            &card("ses_A", &project),
            &descriptor,
            &api_info(PID, PORT, "9.9.8"),
            &native_session("ses_A", &project),
        )
        .expect_err("两侧版本不一致必须拒绝");
        assert_eq!(err.code, codes::OC_VERSION_MISMATCH);

        // 任意版本号，只要两侧一致即可绑定。
        let target = bind_session(
            &card("ses_A", &project),
            &descriptor,
            &api_info(PID, PORT, "9.9.9"),
            &native_session("ses_A", &project),
        )
        .expect("两侧一致即可绑定");
        assert_eq!(target.session_id, "ses_A");
    }

    /// 拿不到进程启动时间：fail-closed，不用 0 或当前时间凑合。
    #[test]
    fn unverified_process_start_time_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let input = input_for(tmp.path());
        let source = FakeSource::ok(loopback_registration(SERVICE_ID, PID, PORT, VERSION));
        let read = |path: &Path| source.read(path);

        let err = discover(&input, &read, &|_pid| None).expect_err("无启动时间必须拒绝");
        assert_eq!(err.code, codes::OC_RUNTIME_UNVERIFIED);
        assert_eq!(source.paths(), vec![input.state_file.clone()]);
    }

    /// 登记文件形态错误与「未登记」区分开。
    #[test]
    fn invalid_registration_is_distinguished_from_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let input = input_for(tmp.path());

        for body in [
            "not json".to_string(),
            // 缺 pid
            format!(
                r#"{{"id":"{SERVICE_ID}","version":"{VERSION}","url":"http://127.0.0.1:{PORT}","password":"{PASSWORD}"}}"#
            ),
            // 缺 password
            format!(
                r#"{{"id":"{SERVICE_ID}","version":"{VERSION}","url":"http://127.0.0.1:{PORT}","pid":{PID}}}"#
            ),
            // 空 id
            loopback_registration("", PID, PORT, VERSION),
        ] {
            let source = FakeSource::ok(body.clone());
            let read = |path: &Path| source.read(path);
            let err =
                discover(&input, &read, &|_pid| Some(STARTED_AT_MS)).expect_err("非法登记必须失败");
            assert_eq!(
                err.code,
                codes::OC_SERVICE_REGISTRATION_INVALID,
                "body={body}"
            );
        }

        // 字段齐全但端点缺端口：端点形态错误，与「登记非法」分开报。
        let source = FakeSource::ok(registration(SERVICE_ID, PID, "http://127.0.0.1", VERSION));
        let read = |path: &Path| source.read(path);
        let err = discover(&input, &read, &|_pid| Some(STARTED_AT_MS)).expect_err("缺端口必须失败");
        assert_eq!(err.code, codes::OC_ENDPOINT_INVALID);

        // 端点内嵌认证信息：拒绝，否则 Debug 打印 endpoint 时会把凭据带进日志。
        let source = FakeSource::ok(registration(
            SERVICE_ID,
            PID,
            &format!("http://user:{PASSWORD}@127.0.0.1:{PORT}"),
            VERSION,
        ));
        let read = |path: &Path| source.read(path);
        let err = discover(&input, &read, &|_pid| Some(STARTED_AT_MS))
            .expect_err("内嵌凭据的端点必须拒绝");
        assert_eq!(err.code, codes::OC_ENDPOINT_INVALID);
        assert!(!err.message.contains(PASSWORD), "{}", err.message);
    }

    /// 只读：只碰给定 state_file，不改进程 HOME、不探测真实用户目录。
    #[test]
    fn discovery_reads_only_the_given_state_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut input = input_for(tmp.path());
        // home 指向不存在的位置：实现若去读 home 就会失败。
        input.home = tmp.path().join("no-such-home");
        let source = FakeSource::ok(loopback_registration(SERVICE_ID, PID, PORT, VERSION));
        let read = |path: &Path| source.read(path);

        let descriptor = discover(&input, &read, &|pid| {
            assert_eq!(pid, PID, "只核验登记里的 pid");
            Some(STARTED_AT_MS)
        })
        .expect("发现成功");

        assert_eq!(source.paths(), vec![input.state_file.clone()]);
        assert_eq!(descriptor.runtime.pid, PID);
        assert_eq!(descriptor.runtime.service_id, SERVICE_ID);
        assert_eq!(descriptor.runtime.started_at_ms, STARTED_AT_MS);
        assert_eq!(descriptor.endpoint.as_str(), "http://127.0.0.1:49617/");
        assert_eq!(descriptor.version, VERSION);
        assert_eq!(descriptor.password, PASSWORD);
    }

    /// 生产路径解析：XDG_STATE_HOME 优先，否则 `$HOME/.local/state`（纯函数，不读环境）。
    #[test]
    fn production_state_file_follows_xdg_then_home() {
        let home = PathBuf::from("/Users/example");
        assert_eq!(
            state_file_for(&home, Some(Path::new("/tmp/xdg-state"))),
            PathBuf::from("/tmp/xdg-state/opencode/service.json")
        );
        assert_eq!(
            state_file_for(&home, None),
            PathBuf::from("/Users/example/.local/state/opencode/service.json")
        );
        // 空串 XDG 视为未设置（XDG 规范）。
        assert_eq!(
            state_file_for(&home, Some(Path::new(""))),
            PathBuf::from("/Users/example/.local/state/opencode/service.json")
        );
    }

    // 合成环境值：不改 HOME/USERPROFILE，不据此声称 Windows 实机通过。
    #[test]
    fn t12_windows_profile_is_not_git_bash_home() {
        let input = production_input_for_platform(
            &|key| match key {
                "USERPROFILE" => Some(OsString::from("C:/Users/中文 User")),
                "HOME" => Some(OsString::from("C:/git-bash-home")),
                _ => None,
            },
            true,
        );
        let home = PathBuf::from("C:/Users/中文 User");
        assert_eq!(input.home, home);
        assert_eq!(
            input.state_file,
            home.join(".local/state/opencode/service.json")
        );
    }

    #[test]
    fn t12_windows_xdg_and_other_platform_do_not_borrow_homes() {
        let lookup = |key: &str| match key {
            "USERPROFILE" => Some(OsString::from("C:/Users/中文 User")),
            "HOME" => Some(OsString::from("/home/unix")),
            "XDG_STATE_HOME" => Some(OsString::from("D:/隔离 state")),
            _ => None,
        };
        let windows = production_input_for_platform(&lookup, true);
        assert_eq!(windows.home, PathBuf::from("C:/Users/中文 User"));
        assert_eq!(
            windows.state_file,
            PathBuf::from("D:/隔离 state/opencode/service.json")
        );
        let unix = production_input_for_platform(&lookup, false);
        assert_eq!(unix.home, PathBuf::from("/home/unix"));
    }

    /// 环境管线（不是纯函数 `state_file_for`）：`XDG_STATE_HOME` 已设 → 用它。
    #[test]
    fn production_input_prefers_xdg_state_home() {
        let input = production_input_for_platform(
            &|key| match key {
                "HOME" => Some(OsString::from("/Users/example")),
                "XDG_STATE_HOME" => Some(OsString::from("/tmp/xdg-state")),
                _ => None,
            },
            false,
        );
        assert_eq!(input.home, PathBuf::from("/Users/example"));
        assert_eq!(
            input.state_file,
            PathBuf::from("/tmp/xdg-state/opencode/service.json")
        );
    }

    /// 环境管线：`XDG_STATE_HOME` 未设（`None`）→ 落 `$HOME/.local/state`。
    #[test]
    fn production_input_falls_back_to_home_state() {
        let input = production_input_for_platform(
            &|key| match key {
                "HOME" => Some(OsString::from("/Users/example")),
                _ => None,
            },
            false,
        );
        assert_eq!(
            input.state_file,
            PathBuf::from("/Users/example/.local/state/opencode/service.json")
        );
    }

    /// 环境管线：`XDG_STATE_HOME` 为空串 → 与未设同（XDG 规范），不是 `"/opencode"`。
    #[test]
    fn production_input_treats_empty_xdg_as_unset() {
        let input = production_input_for_platform(
            &|key| match key {
                "HOME" => Some(OsString::from("/Users/example")),
                "XDG_STATE_HOME" => Some(OsString::new()),
                _ => None,
            },
            false,
        );
        assert_eq!(
            input.state_file,
            PathBuf::from("/Users/example/.local/state/opencode/service.json")
        );
    }

    /// 环境管线：`HOME` 未设 → 保持既有行为（空 home → 相对路径），不 panic、不猜别处。
    #[test]
    fn production_input_without_home_keeps_relative_default() {
        let input = production_input_for_platform(&|_| None, false);
        assert_eq!(input.home, PathBuf::new());
        assert_eq!(
            input.state_file,
            PathBuf::from(".local/state/opencode/service.json")
        );
    }

    /// 生产入口本身必须与注入入口同源：只读环境、不改进程环境。
    #[test]
    fn production_input_reads_the_process_environment_only() {
        let before_home = std::env::var_os("HOME");
        let input = production_input();
        assert_eq!(std::env::var_os("HOME"), before_home, "不得改进程环境");
        let expected = state_file_for(
            &std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                .map(PathBuf::from)
                .unwrap_or_default(),
            std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .as_deref(),
        );
        assert_eq!(input.state_file, expected);
    }
}
