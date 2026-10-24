//! ZCode 官方远控链接的格式校验与持久化（A4，设计 §4.3）。
//!
//! # 合同
//! - 存储键 [`CONFIG_KEY`]（settings KV）；每台主机一条，配对手机共享同一份。
//! - [`validate_official_url`] 只判**格式**：标准绝对 URL、https、精确官方主机
//!   `zcode.z.ai`、版本化路径 `/remote/vN`、非空唯一 `sid`、无 userinfo、仅默认端口、
//!   长度 ≤ 4096、`sid` ≤ 128 UTF-8 字节；已知身份字段（`sid`/`mid`）不得重复，
//!   未知 query 一律保留。格式通过 ≠ 远端可用。
//! - [`get_config`] 读取历史值时**同样复验**：坏值 → [`ConfigState::Invalid`] 且
//!   `url = null`，不自动删除用户设置（设计 §4.3「可手工替换，不自动删除设置」）。
//! - 错误 [`ConfigError`] 只含码与固定文案，**绝不携带原 URL**（设计 §4.3：
//!   完整链接不写日志、公开文档或测试夹具）；保存失败不写入，旧合法值保持不变。

use rusqlite::Connection;

/// settings KV 键：本机保存的 ZCode 官方远控链接。
pub const CONFIG_KEY: &str = "remote.zcode_official_url";

/// 官方主机（精确匹配，不接受子域/相似域）。
const OFFICIAL_HOST: &str = "zcode.z.ai";
/// 链接总长度上限（字符/字节同口径：`http::Uri` 侧亦按字节，超限直接拒绝）。
const MAX_URL_BYTES: usize = 4096;
/// sid 的 UTF-8 字节上限。
const MAX_SID_BYTES: usize = 128;
/// 已知身份字段（设计 §5.2：SID／MID 只是定位信息）——它们可以各出现一次，
/// 重复即语义二义，拒绝；其余未知 query 不擅自丢弃。
const KNOWN_IDENTITY_FIELDS: [&str; 2] = ["sid", "mid"];

/// 配置状态（wire DTO 的 `configState` 取值；小写序列化）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ConfigState {
    /// 本机未配置（或历史值被清空）。
    Missing,
    /// 已保存且当前格式校验通过——**只代表格式已存，不代表已联机**。
    Saved,
    /// 已保存但当前复验不通过（历史坏值/外部改写）；`url` 恒为 null，不下发坏 URL。
    Invalid,
}

/// 官方链接配置的 wire DTO（serde camelCase：`configState` / `url`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ZcodeRemoteConfig {
    pub config_state: ConfigState,
    pub url: Option<String>,
}

impl ZcodeRemoteConfig {
    fn missing() -> Self {
        Self {
            config_state: ConfigState::Missing,
            url: None,
        }
    }

    fn saved(url: String) -> Self {
        Self {
            config_state: ConfigState::Saved,
            url: Some(url),
        }
    }
}

/// 格式校验失败原因。文案固定、带错误码前缀；**不携带原 URL**（唯一构造源是
/// [`validate_official_url`] 的静态分支）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    /// 不是可解析的标准绝对 URL。
    Invalid,
    /// 非 https（http / javascript / 其他 scheme）。
    Scheme,
    /// 主机不是精确的 `zcode.z.ai`（伪域名/子域/相似域）。
    Host,
    /// 路径不是版本化的 `/remote/vN`。
    Path,
    /// 带 userinfo（用户名/密码）。
    UserInfo,
    /// 显式非默认端口（https 默认 443）。
    Port,
    /// 总长度超过 4096 字节。
    TooLong,
    /// 缺少 sid。
    SidMissing,
    /// sid 出现多次。
    SidDuplicate,
    /// sid 为空或超过 128 UTF-8 字节。
    SidInvalid,
    /// 其他已知身份字段（如 mid）重复。
    ParamDuplicate,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Invalid => "ZCODE_URL_INVALID: 链接格式无效",
            Self::Scheme => "ZCODE_URL_SCHEME: 仅支持 https 官方链接",
            Self::Host => "ZCODE_URL_HOST: 仅支持 zcode.z.ai 官方主机",
            Self::Path => "ZCODE_URL_PATH: 官方链接路径需为 /remote/vN",
            Self::UserInfo => "ZCODE_URL_USERINFO: 链接不得包含用户名或密码",
            Self::Port => "ZCODE_URL_PORT: 仅支持默认 443 端口",
            Self::TooLong => "ZCODE_URL_TOO_LONG: 链接长度超过 4096 字节",
            Self::SidMissing => "ZCODE_URL_SID_MISSING: 链接缺少 sid 参数",
            Self::SidDuplicate => "ZCODE_URL_SID_DUPLICATE: 链接中 sid 参数重复",
            Self::SidInvalid => "ZCODE_URL_SID_INVALID: sid 不能为空且不得超过 128 字节",
            Self::ParamDuplicate => "ZCODE_URL_PARAM_DUPLICATE: 链接中身份参数重复",
        })
    }
}

impl std::error::Error for ConfigError {}

/// 校验官方远控链接（唯一校验点，前后端共用服务端口径）。
///
/// 返回：
/// - `Ok(None)`：空白输入（空白保存与删除同源）；
/// - `Ok(Some(url))`：格式合法，返回规范化后的链接（未知 query 原样保留）；
/// - `Err(ConfigError)`：拒绝，错误不含原 URL。
pub fn validate_official_url(raw: &str) -> Result<Option<String>, ConfigError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.len() > MAX_URL_BYTES {
        return Err(ConfigError::TooLong);
    }
    // 标准绝对 URL 解析（reqwest 复用 url crate）；解析失败的原串不进错误
    let url = reqwest::Url::parse(trimmed).map_err(|_| ConfigError::Invalid)?;
    if url.scheme() != "https" {
        return Err(ConfigError::Scheme);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ConfigError::UserInfo);
    }
    if url.host_str() != Some(OFFICIAL_HOST) {
        return Err(ConfigError::Host);
    }
    // https 的默认端口是 443：Url 对显式 :443 也返回 None，故 None 即「默认端口」
    if url.port().is_some() {
        return Err(ConfigError::Port);
    }
    if !is_versioned_remote_path(url.path()) {
        return Err(ConfigError::Path);
    }
    // 已知身份字段各至多一次；sid 必填且非空、≤128 UTF-8 字节。未知 query 保留。
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut sid: Option<String> = None;
    for (key, value) in url.query_pairs() {
        if !KNOWN_IDENTITY_FIELDS.contains(&key.as_ref()) {
            continue;
        }
        let key = key.into_owned();
        if !seen.insert(key.clone()) {
            return Err(if key == "sid" {
                ConfigError::SidDuplicate
            } else {
                ConfigError::ParamDuplicate
            });
        }
        if key == "sid" {
            sid = Some(value.into_owned());
        }
    }
    let Some(sid) = sid else {
        return Err(ConfigError::SidMissing);
    };
    if sid.trim().is_empty() || sid.len() > MAX_SID_BYTES {
        return Err(ConfigError::SidInvalid);
    }
    Ok(Some(url.to_string()))
}

/// `/remote/vN`（N 为十进制数字，非空）——精确两段路径，不接受尾斜杠/多余段。
fn is_versioned_remote_path(path: &str) -> bool {
    let Some(version) = path.strip_prefix("/remote/v") else {
        return false;
    };
    !version.is_empty() && version.bytes().all(|b| b.is_ascii_digit())
}

/// 读取配置（读取时复验历史值）：无记录/空白 → Missing；坏值 → Invalid + url null。
pub fn get_config(conn: &Connection) -> ZcodeRemoteConfig {
    let Some(stored) = crate::database::dao::settings::get_setting_conn(conn, CONFIG_KEY) else {
        return ZcodeRemoteConfig::missing();
    };
    match validate_official_url(&stored) {
        Ok(Some(url)) => ZcodeRemoteConfig::saved(url),
        Ok(None) => ZcodeRemoteConfig::missing(),
        // 坏值不下发（url = null），也不自动删除用户设置（可手工替换）
        Err(_) => ZcodeRemoteConfig {
            config_state: ConfigState::Invalid,
            url: None,
        },
    }
}

/// 保存配置（空白 = 删除）：校验通过才写入；失败不落库，旧合法值保持不变。
pub fn set_config(conn: &Connection, raw: &str) -> Result<ZcodeRemoteConfig, ConfigError> {
    match validate_official_url(raw)? {
        Some(url) => {
            crate::database::dao::settings::set_setting_conn(conn, CONFIG_KEY, &url);
            Ok(ZcodeRemoteConfig::saved(url))
        }
        None => {
            // 空白保存与删除同源：写空串，读取口径统一视为 Missing（复用既有 DAO，
            // 不引入行删除接口；行存在但值为空不构成配置）
            crate::database::dao::settings::set_setting_conn(conn, CONFIG_KEY, "");
            Ok(ZcodeRemoteConfig::missing())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::dao::settings::set_setting_conn;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        conn
    }

    fn missing() -> ZcodeRemoteConfig {
        ZcodeRemoteConfig {
            config_state: ConfigState::Missing,
            url: None,
        }
    }

    fn saved(url: &str) -> ZcodeRemoteConfig {
        ZcodeRemoteConfig {
            config_state: ConfigState::Saved,
            url: Some(url.to_string()),
        }
    }

    fn query_pairs(url: &str) -> Vec<(String, String)> {
        reqwest::Url::parse(url)
            .expect("已存链接应是合法 URL")
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    /// 红测 1：往返（空白删除、v4/v5、中文/编码名称、未知 query 语义保留、更新/删除一致）。
    #[test]
    fn official_url_roundtrip() {
        let conn = mem();

        // 空白保存 = 删除；读口径恒 Missing
        assert_eq!(set_config(&conn, "   ").unwrap(), missing());
        assert_eq!(get_config(&conn), missing());

        // v4 原样往返
        let v4 = "https://zcode.z.ai/remote/v4?sid=synthetic-sid-0001";
        assert_eq!(set_config(&conn, v4).unwrap(), saved(v4));
        assert_eq!(get_config(&conn), saved(v4));

        // v5 + 编码中文 + 未知 query：语义原样保留，不因新增字段/版本强制拒绝
        let v5 =
            "https://zcode.z.ai/remote/v5?sid=synthetic-sid-0002&name=%E5%85%94%E5%AD%90&region=cn";
        let stored = set_config(&conn, v5).unwrap();
        assert_eq!(stored.config_state, ConfigState::Saved);
        let stored_url = stored.url.expect("已保存链接");
        assert_eq!(get_config(&conn).url.as_deref(), Some(stored_url.as_str()));
        let pairs = query_pairs(&stored_url);
        assert!(pairs.contains(&("sid".to_string(), "synthetic-sid-0002".to_string())));
        assert!(pairs.contains(&("name".to_string(), "兔子".to_string())));
        assert!(pairs.contains(&("region".to_string(), "cn".to_string())));

        // 未编码中文 query 同样语义保留（规范化后解码一致即可）
        let raw_cn = "https://zcode.z.ai/remote/v4?sid=synthetic-sid-0003&name=兔子";
        let stored = set_config(&conn, raw_cn).unwrap().url.expect("已保存链接");
        assert!(query_pairs(&stored).contains(&("name".to_string(), "兔子".to_string())));

        // 更新覆盖
        let updated = "https://zcode.z.ai/remote/v4?sid=synthetic-sid-0004";
        assert_eq!(set_config(&conn, updated).unwrap(), saved(updated));
        assert_eq!(get_config(&conn), saved(updated));

        // 删除（空白）后读取一致
        assert_eq!(set_config(&conn, "").unwrap(), missing());
        assert_eq!(get_config(&conn), missing());
    }

    /// 红测 2：拒绝不安全/二义形态；sid 边界与未知 query 保留。
    #[test]
    fn official_url_rejects_unsafe_or_ambiguous() {
        let rejected = [
            // 伪域名 / 子域 / 相似域
            "https://zcode.z.ai.evil.example/remote/v4?sid=synthetic-sid-1001",
            "https://sub.zcode.z.ai/remote/v4?sid=synthetic-sid-1002",
            "https://zcodez.ai/remote/v4?sid=synthetic-sid-1003",
            // userinfo
            "https://user:pass@zcode.z.ai/remote/v4?sid=synthetic-sid-1004",
            "https://user@zcode.z.ai/remote/v4?sid=synthetic-sid-1005",
            // 非 https / 非 URL scheme
            "http://zcode.z.ai/remote/v4?sid=synthetic-sid-1006",
            "javascript:alert(1)",
            "file:///remote/v4?sid=synthetic-sid-1007",
            // 非 remote 路径
            "https://zcode.z.ai/remote/4?sid=synthetic-sid-1008",
            "https://zcode.z.ai/remote/v4x?sid=synthetic-sid-1009",
            "https://zcode.z.ai/remote/v4/?sid=synthetic-sid-1010",
            "https://zcode.z.ai/other/v4?sid=synthetic-sid-1011",
            "https://zcode.z.ai/remote/v4/nested?sid=synthetic-sid-1012",
            // 非默认端口
            "https://zcode.z.ai:8443/remote/v4?sid=synthetic-sid-1013",
            // 缺失 / 空 / 重复 sid
            "https://zcode.z.ai/remote/v4",
            "https://zcode.z.ai/remote/v4?sid=",
            "https://zcode.z.ai/remote/v4?sid=%20",
            "https://zcode.z.ai/remote/v4?sid=synthetic-sid-1014&sid=synthetic-sid-1015",
            // 已知身份字段重复
            "https://zcode.z.ai/remote/v4?sid=synthetic-sid-1016&mid=1&mid=2",
            // 非 URL
            "not a url",
        ];
        for bad in rejected {
            assert!(validate_official_url(bad).is_err(), "应拒绝：{bad}");
        }

        // 显式默认端口 443 允许（Url 归一化后端口等同缺省）
        let default_port = "https://zcode.z.ai:443/remote/v4?sid=synthetic-sid-1017";
        let stored = validate_official_url(default_port)
            .expect("默认端口应通过")
            .expect("非空白");
        let parsed = reqwest::Url::parse(&stored).unwrap();
        assert_eq!(parsed.port(), None, "默认端口不落库");
        assert_eq!(parsed.host_str(), Some("zcode.z.ai"));

        // 长度上限（字节）
        let too_long = format!(
            "https://zcode.z.ai/remote/v4?sid=synthetic-sid-1018&pad={}",
            "a".repeat(MAX_URL_BYTES)
        );
        assert_eq!(validate_official_url(&too_long), Err(ConfigError::TooLong));

        // sid 字节边界：128 恰好通过，129 拒绝；中文按 UTF-8 字节计
        let sid_128 = "s".repeat(MAX_SID_BYTES);
        let ok = format!("https://zcode.z.ai/remote/v4?sid={sid_128}");
        assert!(validate_official_url(&ok).unwrap().is_some());
        let sid_129 = "s".repeat(MAX_SID_BYTES + 1);
        let bad = format!("https://zcode.z.ai/remote/v4?sid={sid_129}");
        assert_eq!(validate_official_url(&bad), Err(ConfigError::SidInvalid));
        let sid_cn_ok = format!("https://zcode.z.ai/remote/v4?sid={}", "兔".repeat(42));
        assert!(
            validate_official_url(&sid_cn_ok).unwrap().is_some(),
            "126 字节应通过"
        );
        let sid_cn_bad = format!("https://zcode.z.ai/remote/v4?sid={}", "兔".repeat(43));
        assert_eq!(
            validate_official_url(&sid_cn_bad),
            Err(ConfigError::SidInvalid),
            "129 字节应拒绝"
        );

        // 未知 query 不擅自丢（含重复未知键）
        let unknown =
            "https://zcode.z.ai/remote/v4?sid=synthetic-sid-1019&future=1&future=2&mid=m-1";
        let stored = validate_official_url(unknown)
            .expect("未知 query 应通过")
            .expect("非空白");
        let pairs = query_pairs(&stored);
        assert_eq!(pairs.iter().filter(|(k, _)| k == "future").count(), 2);
        assert!(pairs.contains(&("mid".to_string(), "m-1".to_string())));

        // 错误不带原 URL 的任何片段
        let err = validate_official_url(
            "https://zcode.z.ai.evil.example/remote/v4?sid=synthetic-sid-1020",
        )
        .unwrap_err();
        assert!(!err.to_string().contains("evil.example"));
        assert!(!err.to_string().contains("synthetic-sid-1020"));
    }

    /// 红测 3：历史坏值 → Invalid/null；错误与失败保存不泄露、不覆盖旧合法值。
    #[test]
    fn stored_invalid_never_leaks() {
        let conn = mem();

        // 绕过写入接口直接注入坏值（模拟历史值/外部改写）
        set_setting_conn(
            &conn,
            CONFIG_KEY,
            "https://zcode.z.ai.evil.example/remote/v4?sid=synthetic-sid-2001",
        );
        let cfg = get_config(&conn);
        assert_eq!(cfg.config_state, ConfigState::Invalid);
        assert_eq!(cfg.url, None, "坏值不下发给消费方");

        // 错误文案不含原 URL 片段
        let err = set_config(
            &conn,
            "https://evil.example/remote/v4?sid=synthetic-sid-2002",
        )
        .unwrap_err();
        assert!(!err.to_string().contains("evil.example"));
        assert!(!err.to_string().contains("synthetic-sid-2002"));

        // 失败保存保留旧合法值
        let valid = "https://zcode.z.ai/remote/v4?sid=synthetic-sid-2003";
        assert_eq!(set_config(&conn, valid).unwrap(), saved(valid));
        assert!(set_config(&conn, "http://zcode.z.ai/remote/v4?sid=synthetic-sid-2004").is_err());
        assert_eq!(get_config(&conn), saved(valid));

        // 存储空白 = 未配置（不判坏值）
        set_setting_conn(&conn, CONFIG_KEY, "   ");
        assert_eq!(get_config(&conn).config_state, ConfigState::Missing);
    }
}
