use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::remote::tailscale::Reachability;

pub(crate) const KEY_NAMED_ADDR: &str = "remote.named_addr";

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NamedWizardProbe {
    pub config_key: String,
    pub has_token: bool,
    pub enabled: bool,
    pub running: bool,
    pub connected: bool,
    pub origin_url: String,
    pub address: Option<String>,
    pub last_address: Option<String>,
    pub(crate) reach: Reachability,
    pub waiting: serde_json::Value,
}

pub(crate) fn available_address(probe: &NamedWizardProbe) -> Option<String> {
    (probe.enabled
        && probe.running
        && probe.connected
        && matches!(probe.reach, Reachability::Verified))
    .then(|| probe.address.clone())
    .flatten()
}

#[derive(Clone)]
struct Verification {
    config_key: Option<String>,
    generation: Option<crate::remote::tunnel::NamedConnectionIdentity>,
    reach: Reachability,
    waiting_since_ms: Option<u64>,
    transport_error: Option<String>,
}

static VERIFICATION: once_cell::sync::Lazy<std::sync::Mutex<Verification>> =
    once_cell::sync::Lazy::new(|| {
        std::sync::Mutex::new(Verification {
            config_key: None,
            generation: None,
            reach: Reachability::Unverified,
            waiting_since_ms: None,
            transport_error: None,
        })
    });
static WATCHDOG_STARTED: AtomicBool = AtomicBool::new(false);

fn config_key(token: &str, address: &str, port: u16) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{}\n{}\n{port}", token.trim(), address).as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

fn connector_key(token: &str, port: u16) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{}\n{port}", token.trim()).as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

fn current_address() -> String {
    let configured =
        crate::database::dao::settings::get_setting(KEY_NAMED_ADDR).unwrap_or_default();
    let parsed = crate::remote::tunnel::snapshot()
        .named
        .url
        .unwrap_or_default();
    effective_address(&configured, &parsed)
}

pub(crate) fn current_connector_key(port: u16) -> String {
    let token = crate::database::dao::settings::get_setting(crate::remote::KEY_TUNNEL_TOKEN)
        .unwrap_or_default();
    connector_key_for_token(&token, port)
}

pub(crate) fn current_connector_key_for_settings() -> String {
    let port = crate::remote::bind_and_port()
        .map(|(_, port)| port)
        .unwrap_or(crate::remote::DEFAULT_PORT);
    current_connector_key(port)
}

pub(super) fn connector_key_for_token(token: &str, port: u16) -> String {
    connector_key(token, port)
}

fn current_settings() -> (String, String, u16) {
    let token = crate::database::dao::settings::get_setting(crate::remote::KEY_TUNNEL_TOKEN)
        .unwrap_or_default();
    let address = current_address();
    let port = crate::remote::bind_and_port()
        .map(|(_, port)| port)
        .unwrap_or(crate::remote::DEFAULT_PORT);
    (token, address, port)
}

fn effective_address(configured: &str, parsed: &str) -> String {
    if configured.trim().is_empty() {
        parsed.to_string()
    } else {
        configured.to_string()
    }
}

fn waiting_value(state: &Verification) -> serde_json::Value {
    state.waiting_since_ms.map_or(serde_json::Value::Null, |started_at_ms| {
        serde_json::json!({ "startedAtMs": started_at_ms, "expectedSeconds": null })
    })
}

pub fn probe() -> NamedWizardProbe {
    let (token, configured_address, port) = current_settings();
    let key = config_key(&token, &configured_address, port);
    let tun = crate::remote::tunnel::snapshot();
    let running = tun.named.running;
    let connection_key = connector_key(&token, port);
    let connected_generation = crate::remote::tunnel::named_connection_generation(&connection_key);
    let connected = running && connected_generation.is_some();
    let mut state = VERIFICATION.lock().unwrap();
    reconcile_verification(&mut state, &key, connected_generation, connected);
    if let Some(reason) = tun.named.error.as_ref() {
        state.transport_error = Some(reason.clone());
        state.reach = reach_with_transport_error(&state.reach, Some(reason));
    } else if let Some(previous_error) = state.transport_error.take() {
        if matches!(&state.reach, Reachability::Failed { reason } if reason == &previous_error) {
            state.reach = Reachability::Unverified;
        }
    }
    let reach = state.reach.clone();
    let waiting = waiting_value(&state);
    drop(state);
    let last_address =
        crate::database::dao::settings::get_setting(crate::remote::KEY_NAMED_ADDR_LAST);
    let parsed = tun.named.url;
    let configured = if configured_address.trim().is_empty() {
        parsed.clone()
    } else {
        Some(configured_address)
    };
    NamedWizardProbe {
        config_key: key,
        has_token: !token.trim().is_empty(),
        enabled: crate::remote::read_channels().named,
        running,
        connected,
        origin_url: format!("http://127.0.0.1:{port}"),
        address: configured,
        last_address,
        reach,
        waiting,
    }
}

pub(crate) fn invalidate_verification() {
    let mut state = VERIFICATION.lock().unwrap();
    state.generation = None;
    state.reach = Reachability::Unverified;
    state.waiting_since_ms = None;
    state.transport_error = None;
}

fn apply_result(
    key: &str,
    generation: crate::remote::tunnel::NamedConnectionIdentity,
    reach: Reachability,
) -> bool {
    let current = probe();
    let current_generation =
        crate::remote::tunnel::named_connection_generation(&current_connector_key_for_settings());
    if !current.connected
        || !result_is_current(key, generation, &current.config_key, current_generation)
    {
        return false;
    }
    let mut state = VERIFICATION.lock().unwrap();
    apply_result_to_state(
        &mut state,
        key,
        generation,
        &current.config_key,
        current_generation,
        reach,
    )
}

fn reconcile_verification(
    state: &mut Verification,
    config_key: &str,
    identity: Option<crate::remote::tunnel::NamedConnectionIdentity>,
    connected: bool,
) {
    if state.config_key.as_deref() != Some(config_key) || state.generation != identity {
        state.config_key = Some(config_key.to_string());
        state.generation = identity;
        state.reach = Reachability::Unverified;
        state.waiting_since_ms = None;
        state.transport_error = None;
    }
    if !connected {
        state.reach = Reachability::Unverified;
        state.waiting_since_ms = None;
    }
}

fn apply_result_to_state(
    state: &mut Verification,
    key: &str,
    identity: crate::remote::tunnel::NamedConnectionIdentity,
    current_key: &str,
    current_identity: Option<crate::remote::tunnel::NamedConnectionIdentity>,
    reach: Reachability,
) -> bool {
    if state.config_key.as_deref() != Some(key)
        || state.generation != Some(identity)
        || !result_is_current(key, identity, current_key, current_identity)
    {
        return false;
    }
    match reach {
        Reachability::RecordPending { .. } => {
            if state.waiting_since_ms.is_none() {
                state.waiting_since_ms = Some(unix_millis());
            }
        }
        _ => state.waiting_since_ms = None,
    }
    state.reach = reach;
    true
}

fn result_is_current(
    expected_key: &str,
    expected_generation: crate::remote::tunnel::NamedConnectionIdentity,
    current_key: &str,
    current_generation: Option<crate::remote::tunnel::NamedConnectionIdentity>,
) -> bool {
    expected_key == current_key && current_generation == Some(expected_generation)
}

fn reach_with_transport_error(reach: &Reachability, error: Option<&str>) -> Reachability {
    error.map_or_else(
        || reach.clone(),
        |reason| Reachability::Failed {
            reason: reason.to_string(),
        },
    )
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn verify_sync(host: String) -> Reachability {
    let addresses = match crate::remote::tailscale::real_probe()(host.as_str()) {
        Ok(addresses) => addresses,
        Err(error) => {
            return Reachability::Failed {
                reason: format!("REMOTE_NAMED_DNS_FAIL: {error}"),
            }
        }
    };
    if addresses.is_empty() {
        return Reachability::RecordPending { republish: false };
    }
    let public_addresses: Vec<String> = addresses
        .into_iter()
        .filter_map(|value| value.parse::<std::net::IpAddr>().ok())
        .filter(|ip| is_public_ip(*ip))
        .map(|ip| ip.to_string())
        .take(3)
        .collect();
    if public_addresses.is_empty() {
        return Reachability::Failed {
            reason: "REMOTE_NAMED_DNS_FAIL: 公共 DNS 未返回可用于公网探测的 IP".into(),
        };
    }
    let mut errors = Vec::new();
    for ip in public_addresses {
        match crate::remote::tailscale::probe_public_http(&host, &ip) {
            Ok(()) => return Reachability::Verified,
            Err(error) => errors.push(format!("{ip}: {error}")),
        }
    }
    Reachability::Failed {
        reason: format!(
            "REMOTE_NAMED_VERIFY_FAIL: 公网 HTTP 特征核验失败（{}）",
            errors.join("；")
        ),
    }
}

#[tauri::command]
pub fn remote_named_probe() -> NamedWizardProbe {
    probe()
}

#[tauri::command]
pub fn remote_named_set_address(address: &str) -> Result<NamedWizardProbe, String> {
    let normalized = normalize_address(address)?;
    let current = crate::database::dao::settings::get_setting(KEY_NAMED_ADDR).unwrap_or_default();
    let old_effective = current_address();
    if current != normalized {
        crate::database::dao::settings::set_setting(KEY_NAMED_ADDR, &normalized);
        if old_effective != normalized {
            invalidate_verification();
        }
        crate::remote::events::emit_ui("remote-changed", serde_json::json!({ "channel": "named" }));
    }
    Ok(probe())
}

#[tauri::command]
pub async fn remote_named_verify() -> Result<NamedWizardProbe, String> {
    verify_current().await
}

async fn verify_current() -> Result<NamedWizardProbe, String> {
    let before = probe();
    if !before.has_token {
        return Err("REMOTE_NAMED_TOKEN_MISSING: 请先保存 Cloudflare Tunnel Token".into());
    }
    if !before.connected {
        invalidate_verification();
        return Err("REMOTE_NAMED_NOT_CONNECTED: 尚未观察到已注册的 Cloudflare 边缘连接".into());
    }
    let generation = crate::remote::tunnel::named_connection_generation(
        &current_connector_key_for_settings(),
    )
    .ok_or_else(|| "REMOTE_NAMED_NOT_CONNECTED: Cloudflare 隧道当前没有注册连接".to_string())?;
    let address = before.address.as_deref().ok_or_else(|| {
        "REMOTE_NAMED_ADDRESS_INVALID: 请先填写已在 Cloudflare 配置的公开域名".to_string()
    })?;
    let host = reqwest::Url::parse(address)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .ok_or_else(|| "REMOTE_NAMED_ADDRESS_INVALID: 域名格式无效".to_string())?;
    {
        let mut state = VERIFICATION.lock().unwrap();
        state.config_key = Some(before.config_key.clone());
        state.generation = Some(generation);
        state.reach = Reachability::Verifying;
    }
    let check = tokio::task::spawn_blocking(move || verify_sync(host))
        .await
        .map_err(|error| format!("REMOTE_NAMED_VERIFY_FAIL: 公网核验任务异常: {error}"))?;
    apply_result(&before.config_key, generation, check);
    Ok(probe())
}

pub fn start_watchdog() {
    if WATCHDOG_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    tauri::async_runtime::spawn(async {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            let status = probe();
            if status.enabled && status.connected && status.has_token {
                let _ = verify_current().await;
            }
        }
    });
}

fn normalize_address(input: &str) -> Result<String, String> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err("REMOTE_NAMED_ADDRESS_INVALID: 请输入公开域名".into());
    }
    let candidate = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("https://{raw}")
    };
    let url = reqwest::Url::parse(&candidate)
        .map_err(|_| "REMOTE_NAMED_ADDRESS_INVALID: 域名格式无效".to_string())?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some_and(|p| p != 443)
        || !matches!(url.path(), "" | "/" | "/m" | "/m/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("REMOTE_NAMED_ADDRESS_INVALID: 仅支持 HTTPS 公共域名根路径或 /m".into());
    }
    let raw_host = url
        .host_str()
        .ok_or_else(|| "REMOTE_NAMED_ADDRESS_INVALID: 域名无效".to_string())?;
    let host = raw_host;
    if host.is_empty()
        || host.ends_with('.')
        || host.parse::<std::net::IpAddr>().is_ok()
        || host.split('.').count() < 2
        || !host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
    {
        return Err("REMOTE_NAMED_ADDRESS_INVALID: 必须填写公开 DNS 域名，不能填写 IP".into());
    }
    Ok(format!("https://{}/m", host.to_ascii_lowercase()))
}

fn is_public_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => {
            let [a, b, c, d] = ip.octets();
            !(a == 0
                || a == 10
                || a == 127
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && b == 168)
                || (a == 192 && b == 0 && (c == 0 || c == 2))
                || (a == 192 && b == 88 && c == 99)
                || (a == 198 && (b == 18 || b == 19 || b == 51 && c == 100))
                || (a == 203 && b == 0 && c == 113)
                || a >= 224
                || (a == 255 && b == 255 && c == 255 && d == 255))
        }
        std::net::IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return is_public_ip(v4.into());
            }
            let segments = ip.segments();
            segments[0] & 0xe000 == 0x2000
                && !(segments[0] == 0x2001 && (segments[1] < 0x0200 || segments[1] == 0x0db8))
        }
    }
}

pub(super) fn update_connection_index(indexes: &mut BTreeSet<u32>, line: &str) -> bool {
    let Some(index) = log_field(line, "connIndex").and_then(|v| v.parse::<u32>().ok()) else {
        return false;
    };
    if log_field(line, "event") == Some("0") && line.contains("Registered tunnel connection") {
        return indexes.insert(index);
    }
    if line.contains("Connection terminated")
        || line.contains("Unregistered tunnel connection")
        || line.contains("Lost connection with the edge")
    {
        return indexes.remove(&index);
    }
    false
}

pub(super) fn is_auth_failure_line(line: &str) -> bool {
    line.to_ascii_lowercase().contains("unauthorized")
}

fn log_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_ascii_whitespace().find_map(|part| {
        let (name, value) = part.split_once('=')?;
        (name == key).then_some(value)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_only_public_https_origin_with_optional_m_path() {
        assert_eq!(
            normalize_address(" Example.COM ").unwrap(),
            "https://example.com/m"
        );
        assert_eq!(
            normalize_address("https://sub.example.com/m").unwrap(),
            "https://sub.example.com/m"
        );
        assert_eq!(
            normalize_address("https://sub.example.com/").unwrap(),
            "https://sub.example.com/m"
        );
    }

    #[test]
    fn rejects_non_public_or_non_origin_addresses() {
        for invalid in [
            "http://example.com",
            "https://user:pass@example.com",
            "https://example.com:8443",
            "https://example.com/settings",
            "https://127.0.0.1/m",
            "https://192.168.1.2/m",
            "https://example.com./m",
        ] {
            assert!(normalize_address(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn registered_connection_requires_registration_event_and_termination_index() {
        let mut indexes = std::collections::BTreeSet::new();
        assert!(update_connection_index(
            &mut indexes,
            "INF Registered tunnel connection connIndex=0 event=0"
        ));
        assert_eq!(indexes, [0].into_iter().collect());
        assert!(update_connection_index(
            &mut indexes,
            "INF Connection terminated connIndex=0"
        ));
        assert!(indexes.is_empty());
        assert!(!update_connection_index(
            &mut indexes,
            "INF Registered tunnel connection connIndex=x event=0"
        ));
        assert!(!update_connection_index(
            &mut indexes,
            "INF Registered tunnel connection connIndex=1 event=1"
        ));
    }

    #[test]
    fn public_ip_filter_rejects_non_routable_ranges() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "100.64.0.1",
            "169.254.1.1",
            "192.168.1.2",
            "198.51.100.1",
            "224.0.0.1",
            "::1",
            "fc00::1",
            "2001:db8::1",
            "2001:20::1",
        ] {
            assert!(!is_public_ip(ip.parse().unwrap()), "accepted {ip}");
        }
        for ip in ["1.1.1.1", "2606:4700:4700::1111"] {
            assert!(is_public_ip(ip.parse().unwrap()), "rejected {ip}");
        }
    }

    #[test]
    fn recognizes_only_explicit_cloudflare_unauthorized_text_for_auth_failure() {
        assert!(is_auth_failure_line(
            "ERR Register tunnel error: Unauthorized"
        ));
        assert!(!is_auth_failure_line("ERR connection failed: timeout"));
    }

    #[test]
    fn config_key_changes_with_token_domain_or_local_port() {
        let base = config_key("secret-a", "https://board.example.com/m", 9420);
        assert_eq!(
            base,
            config_key("secret-a", "https://board.example.com/m", 9420)
        );
        assert_ne!(
            base,
            config_key("secret-b", "https://board.example.com/m", 9420)
        );
        assert_ne!(
            base,
            config_key("secret-a", "https://other.example.com/m", 9420)
        );
        assert_ne!(
            base,
            config_key("secret-a", "https://board.example.com/m", 9430)
        );
        assert!(!base.contains("secret-a"));
    }

    #[test]
    fn parsed_hostname_is_part_of_config_key_when_manual_address_is_empty() {
        let a = effective_address("  ", "https://a.example.com/m");
        let b = effective_address("  ", "https://b.example.com/m");
        assert_ne!(
            config_key("same-token", &a, 9420),
            config_key("same-token", &b, 9420),
            "迟到核验不能因日志识别域名变化继续沿用旧配置键"
        );
        assert_eq!(
            effective_address("https://fixed.example.com/m", &b),
            "https://fixed.example.com/m"
        );
    }

    #[test]
    fn old_verification_cannot_match_new_config_or_process_generation() {
        let first = crate::remote::tunnel::NamedConnectionIdentity {
            process_generation: 7,
            verification_epoch: 4,
        };
        let reconnected = crate::remote::tunnel::NamedConnectionIdentity {
            process_generation: 7,
            verification_epoch: 5,
        };
        let restarted = crate::remote::tunnel::NamedConnectionIdentity {
            process_generation: 8,
            verification_epoch: 6,
        };
        assert!(result_is_current("key-a", first, "key-a", Some(first)));
        assert!(!result_is_current("key-a", first, "key-b", Some(first)));
        assert!(!result_is_current(
            "key-a",
            first,
            "key-a",
            Some(reconnected)
        ));
        assert!(!result_is_current("key-a", first, "key-a", Some(restarted)));
        assert!(!result_is_current("key-a", first, "key-a", None));
    }

    #[test]
    fn same_process_reconnect_invalidates_verified_state_and_rejects_late_success() {
        use crate::remote::tunnel::{named_connection_generation, test_sync};
        let _serial = test_sync::TUNNEL_GLOBALS.lock().unwrap();
        let key = "same-token-port";
        let generation = test_sync::begin_named(key);
        test_sync::named_log(
            generation,
            key,
            "INF Registered tunnel connection connIndex=0 event=0",
        );
        let old = named_connection_generation(key).unwrap();
        let mut state = Verification {
            config_key: Some("config-a".into()),
            generation: Some(old),
            reach: Reachability::Verified,
            waiting_since_ms: None,
            transport_error: None,
        };

        // 屏蔽中间 probe：最后连接断开后，同进程直接重连。
        test_sync::named_log(generation, key, "INF Connection terminated connIndex=0");
        test_sync::named_log(
            generation,
            key,
            "INF Registered tunnel connection connIndex=0 event=0",
        );
        let reconnected = named_connection_generation(key).unwrap();
        assert_eq!(old.process_generation, reconnected.process_generation);
        assert_ne!(old.verification_epoch, reconnected.verification_epoch);
        // 尚未来得及 reconcile 时，旧请求在新连接上完成也必须拒收。
        assert!(!apply_result_to_state(
            &mut state,
            "config-a",
            old,
            "config-a",
            Some(reconnected),
            Reachability::Verified,
        ));
        reconcile_verification(&mut state, "config-a", Some(reconnected), true);
        assert_eq!(state.reach, Reachability::Unverified);
        test_sync::reset_named();
    }

    #[test]
    fn named_probe_reach_exposes_terminal_authentication_error() {
        let auth = "REMOTE_NAMED_AUTH_FAIL: Cloudflare rejected the Tunnel Token";
        assert_eq!(
            reach_with_transport_error(&Reachability::Unverified, Some(auth)),
            Reachability::Failed {
                reason: auth.to_string()
            }
        );
    }

    #[test]
    fn remote_available_address_requires_enabled_running_connected_and_verified() {
        let verified = NamedWizardProbe {
            config_key: "key".into(),
            has_token: true,
            enabled: true,
            running: true,
            connected: true,
            origin_url: "http://127.0.0.1:9420".into(),
            address: Some("https://board.example.com/m".into()),
            last_address: None,
            reach: Reachability::Verified,
            waiting: serde_json::Value::Null,
        };
        assert_eq!(
            available_address(&verified).as_deref(),
            Some("https://board.example.com/m")
        );
        let mut not_verified = verified.clone();
        not_verified.reach = Reachability::Unverified;
        assert_eq!(available_address(&not_verified), None);
        let mut disconnected = verified.clone();
        disconnected.connected = false;
        assert_eq!(available_address(&disconnected), None);
        let mut disabled = verified;
        disabled.enabled = false;
        assert_eq!(available_address(&disabled), None);
    }
}
