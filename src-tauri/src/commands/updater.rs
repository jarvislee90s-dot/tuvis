// 升级检查（prerelease 渠道，2026-10-07 设计定案）：
// 1. 发现层走 GitHub Releases API——`releases/latest` 永远跳过 prerelease，
//    静态端点推不出 beta 版本；API 按 semver 挑最大者（含 prerelease）。
// 2. 弹窗正文以 release 页 body 为准（单一事实源，避免与 docs/release-notes 漂移）；
//    外链用 html_url（具体 tag 页，绝不用 /releases/latest）。
// 3. 安装层复用 tauri-plugin-updater（签名校验/平台安装行为不变），
//    仅把端点动态指向目标 release 的 latest.json 资产。
// 4. Windows 残留清理：updater 插件把安装包写进
//    `{app_name}-{version}-updater-*` 临时目录后直接 exit(0)，目录永久留在
//    %TEMP%（macOS 走 TempDir 自动清理无此问题）——启动时 best-effort 扫除。

use serde::{Deserialize, Serialize};
use tauri::Emitter;
use tauri_plugin_updater::UpdaterExt;

const GITHUB_REPO: &str = "jarvislee90s-dot/tuvis";
/// 钉 IP 直连的域名（resolve 只对**直连**生效；兜底客户端必须 no_proxy，见下）
const GITHUB_API_HOST: &str = "api.github.com";
/// GitHub API 强制要求 User-Agent，否则 403
const USER_AGENT: &str = "tuvis-updater";
/// 进度事件名（沿用 `mam-` 前缀惯例）
pub const PROGRESS_EVENT: &str = "tuvis-updater-progress";
/// 兜底直连最多试几个 DoH 地址（同 reach.rs B-M7 口径：首条陈旧时还有得试，
/// 全试一遍又太贵）
const PINNED_MAX_ADDRS: usize = 3;

// —— 发现层：GitHub API 响应的最小字段子集（draft 对匿名请求本就不可见，过滤作纵深防御）——

#[derive(Debug, Clone, Deserialize)]
pub struct GithubAsset {
    pub name: String,
    pub browser_download_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GithubRelease {
    pub tag_name: String,
    pub draft: bool,
    pub prerelease: bool,
    pub body: Option<String>,
    pub html_url: String,
    pub published_at: Option<String>,
    pub assets: Option<Vec<GithubAsset>>,
}

impl GithubRelease {
    /// 该 release 是否带 updater 清单资产（缺 latest.json 的手工 tag 无法走应用内升级）
    pub fn has_latest_json(&self) -> bool {
        self.assets
            .as_ref()
            .is_some_and(|list| list.iter().any(|a| a.name == "latest.json"))
    }

    pub fn latest_json_url(&self) -> Option<&str> {
        self.assets
            .as_ref()
            .and_then(|list| list.iter().find(|a| a.name == "latest.json"))
            .map(|a| a.browser_download_url.as_str())
    }
}

/// tag → semver（容错 `v` 前缀；非语义化 tag 返回 None）
pub fn parse_tag_version(tag: &str) -> Option<semver::Version> {
    let stripped = tag.strip_prefix('v').unwrap_or(tag);
    semver::Version::parse(stripped).ok()
}

/// 应用内弹窗的 notes 只该是「更新内容」本体：release body 的头段（版本标题 +
/// 「下载」清单）是给手动下载的人看的——整段塞进弹窗限高滚动框，第一屏就是与
/// 场景无关的下载清单，真正的更新内容全在折叠线以下（0.5.2 用户实测「没头没尾」）。
/// 以 CI 拼装 body 的固定分节符 `### 更新内容` 为界取其后；无分节符（旧版结构 /
/// 手工 body）原样返回。
fn extract_release_notes(body: &str) -> &str {
    const SECTION: &str = "\n### 更新内容\n";
    match body.find(SECTION) {
        Some(i) => body[i + SECTION.len()..].trim_start(),
        None => body.trim(),
    }
}

/// 在 release 列表里挑 semver 最大的**可升级目标**（含 prerelease；draft 与
/// 缺 latest.json 的 release 排除）。发布顺序与版本顺序不一致时以 semver 为准。
pub fn pick_latest_updatable(releases: &[GithubRelease]) -> Option<&GithubRelease> {
    releases
        .iter()
        .filter(|r| !r.draft)
        .filter(|r| r.has_latest_json())
        .filter_map(|r| parse_tag_version(&r.tag_name).map(|v| (r, v)))
        .max_by(|a, b| a.1.cmp(&b.1))
        .map(|(r, _)| r)
}

// —— IPC 返回类型（与前端 lib/updater.ts 的 UpdateCheckResult 对齐，camelCase）——
// 注意：internally-tagged enum 的 container 属性只作用于 variant 名；variant
// 字段名要 camelCase 必须用 rename_all_fields（默认蛇形，前端读不到）

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateAvailable {
    pub version: String,
    pub prerelease: bool,
    /// 弹窗正文 = release body 的「更新内容」节（`extract_release_notes` 裁掉
    /// 版本标题与下载清单头段——那两段是给手动下载的人看的，完整 body 走
    /// 「在 GitHub 查看」链接）
    pub notes: String,
    /// 具体 tag 页（如 releases/tag/v0.5.0-beta.1），不是 /releases/latest
    pub html_url: String,
    pub published_at: Option<String>,
    pub tag: String,
    /// 该 release 的 updater 清单地址，install 命令直接消费
    pub latest_json_url: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all_fields = "camelCase")]
pub enum CheckUpdateStatus {
    #[serde(rename = "available")]
    Available {
        current_version: String,
        update: UpdateAvailable,
    },
    #[serde(rename = "up-to-date")]
    UpToDate {
        current_version: String,
        latest_version: String,
    },
    #[serde(rename = "error")]
    Error { message: String },
}

/// 把错误的**完整源链**拍平成一行。reqwest 的 Display 只有顶层一句
/// （「error sending request for url」），定位成因的关键信息全在 source 链里
/// （「invalid peer certificate」= DNS 污染假证书 /「连接被拒绝」= 代理没在听 /
/// 超时 = 弱网）——0.5.1 的 toast 只插值顶层，用户照旧分不清，违背「失败原因
/// 第一眼可定位」的初衷（420f48a）。
fn flatten_error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![e.to_string()];
    let mut cur = e.source();
    while let Some(s) = cur {
        parts.push(format!("[{}]", s));
        cur = s.source();
    }
    parts.join(" ")
}

/// 默认管道一次请求（reqwest 默认读环境变量与 Windows/macOS 系统代理——挂 VPN
/// 时走的就是它；hyper-util 注册表读取链路 2026-10-08 独立探针实证可用）
/// Release 发现：仓库已一次性更名（2026-10，用户裁决取消分阶段过渡——存量用户
/// 极少且接受手动重装），直接打新仓名。
async fn fetch_releases_with_client(client: reqwest::Client) -> Result<Vec<GithubRelease>, String> {
    let resp = client
        .get(format!(
            "https://api.github.com/repos/{GITHUB_REPO}/releases?per_page=30"
        ))
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| {
            format!(
                "UPDATER_GITHUB_API_FAIL: GitHub API 请求失败: {}",
                flatten_error_chain(&e)
            )
        })?;
    if !resp.status().is_success() {
        return Err(format!(
            "UPDATER_GITHUB_STATUS: GitHub API 状态异常: {}",
            resp.status()
        ));
    }
    resp.json::<Vec<GithubRelease>>()
        .await
        .map_err(|e| format!("UPDATER_GITHUB_PARSE: GitHub API 响应解析失败: {e}"))
}

/// DoH 解析 + 钉 IP 直连兜底：**绕开本地 DNS 污染与失效代理**。
/// 2026-10-08 实测（本机，VPN 关）：本地解析被污染（TCP 通、TLS 假证书
/// 「invalid peer certificate: UnknownIssuer」），经阿里 DoH 拿真实地址后钉 IP
/// 直连 HTTP 200——GitHub 真实 IP 本身没被封，坏的只是域名解析。
async fn fetch_releases_via_doh_pinned() -> Result<Vec<GithubRelease>, String> {
    // DoH 解析复用 reach.rs 的生产探针（多源、IP 字面量、禁系统代理、有界超时）；
    // 内部是 reqwest::blocking——async 上下文直接调会 panic，必须 spawn_blocking
    //（与 reach verify 步同纪律）
    let addrs = tauri::async_runtime::spawn_blocking(|| {
        crate::remote::tailscale::real_probe()("api.github.com")
    })
    .await
    .map_err(|e| format!("UPDATER_DOH_TASK_FAIL: DoH 解析任务失败: {e}"))?
    .map_err(|e| format!("UPDATER_DOH_ALL_FAIL: DoH 解析失败（所有源都不可用）: {e}"))?;

    // 钉 IP 直连：**必须 no_proxy**——经 HTTP 代理的 CONNECT 隧道把域名交给代理
    // 自己解析，resolve() 只对直连生效；这条路径的意义就是绕开一切本地干扰
    let mut last_err = "DoH 没有给出可用地址".to_string();
    for ip in addrs.iter().take(PINNED_MAX_ADDRS) {
        let Ok(addr) = ip.parse::<std::net::IpAddr>() else {
            continue;
        };
        let client = match reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .no_proxy()
            .resolve(GITHUB_API_HOST, std::net::SocketAddr::new(addr, 443))
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(20))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                last_err = format!("钉 IP {addr} 客户端构建失败: {e}");
                continue;
            }
        };
        match fetch_releases_with_client(client).await {
            Ok(list) => return Ok(list),
            Err(e) => last_err = format!("UPDATER_PIN_FAIL: 钉 IP {addr} 直连失败: {e}"),
        }
    }
    Err(last_err)
}

/// 检查更新取数的两层编排：① 默认管道（环境变量 + 系统代理——挂 VPN 的正路）；
/// ② 失败后 DoH 钉 IP 直连兜底（VPN 关/代理失效/DNS 污染都覆盖）。两层各自的
/// 失败原因都如实上报，不给一句笼统的「网络错误」。
async fn fetch_github_releases() -> Result<Vec<GithubRelease>, String> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| format!("HTTP_CLIENT_BUILD_FAIL: HTTP 客户端构建失败: {e}"))?;
    match fetch_releases_with_client(client).await {
        Ok(list) => Ok(list),
        Err(via_default) => match fetch_releases_via_doh_pinned().await {
            Ok(list) => Ok(list),
            Err(via_pinned) => Err(format!(
                "{via_default}；DoH 钉 IP 直连兜底也失败: {via_pinned}"
            )),
        },
    }
}

/// 启动/手动触发时检查更新：GitHub API 挑最新（含 prerelease），与当前版本比 semver。
/// 网络失败静默返回 error（前端不弹窗、不亮徽标；手动检查时 toast）。
#[tauri::command]
pub async fn check_for_github_update(app: tauri::AppHandle) -> CheckUpdateStatus {
    let current = app.package_info().version.clone();
    let releases = match fetch_github_releases().await {
        Ok(list) => list,
        Err(msg) => return CheckUpdateStatus::Error { message: msg },
    };
    let Some(latest) = pick_latest_updatable(&releases) else {
        return CheckUpdateStatus::Error {
            message: "未找到带升级资产的 release".into(),
        };
    };
    let latest_version = parse_tag_version(&latest.tag_name).expect("pick 已过滤无法解析的 tag");
    if latest_version <= current {
        return CheckUpdateStatus::UpToDate {
            current_version: current.to_string(),
            latest_version: latest_version.to_string(),
        };
    }
    let Some(latest_json_url) = latest.latest_json_url() else {
        return CheckUpdateStatus::Error {
            message: "release 缺少 latest.json 资产".into(),
        };
    };
    CheckUpdateStatus::Available {
        current_version: current.to_string(),
        update: UpdateAvailable {
            version: latest_version.to_string(),
            prerelease: latest.prerelease,
            notes: latest
                .body
                .as_deref()
                .map(|b| extract_release_notes(b).to_string())
                .unwrap_or_default(),
            html_url: latest.html_url.clone(),
            published_at: latest.published_at.clone(),
            tag: latest.tag_name.clone(),
            latest_json_url: latest_json_url.to_string(),
        },
    }
}

/// 校验升级清单地址：只接受本仓库 release 资产下的 latest.json
/// （前端传参理论只有自家 webview，纵深防御；latest.json 进插件后另有
/// pinned 公钥签名校验兜底）。抽纯函数以便单测（评审 M8）。
pub fn validate_latest_json_url(url: &str) -> Result<(), String> {
    let expected_prefix = format!("https://github.com/{GITHUB_REPO}/releases/download/");
    if !url.starts_with(&expected_prefix) {
        return Err(format!(
            "UPDATER_MANIFEST_PREFIX: 非法的升级清单地址（前缀不符）: {url}"
        ));
    }
    if !url.ends_with("latest.json") {
        return Err(format!(
            "UPDATER_MANIFEST_FILENAME: 非法的升级清单地址（非 latest.json）: {url}"
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProgressPayload<'a> {
    event: &'a str,
    downloaded: u64,
    content_length: Option<u64>,
}

/// 一键升级：动态端点指向目标 release 的 latest.json，复用插件的
/// 签名校验与平台安装（Windows NSIS passive 后插件自行退出重启；
/// macOS 换壳成功后由本命令重启）。进度经 `tuvis-updater-progress` 事件广播。
#[tauri::command]
pub async fn install_github_update(
    app: tauri::AppHandle,
    latest_json_url: String,
) -> Result<(), String> {
    // 插件仅在 release 构建注册（lib.rs）；debug 构建下 updater_builder 会因
    // state 未托管而 panic，先降级提示（评审 I3：运行时 env 门已随 lib.rs 移除）
    if cfg!(debug_assertions) {
        return Err(
            "此构建未启用应用内升级（仅 release 构建支持），请到 GitHub release 页下载".into(),
        );
    }
    validate_latest_json_url(&latest_json_url)?;
    let url: tauri::Url = latest_json_url
        .parse()
        .map_err(|e| format!("UPDATER_MANIFEST_URL_INVALID: 升级清单地址无效: {e}"))?;

    let updater = app
        .updater_builder()
        .endpoints(vec![url])
        .map_err(|e| format!("UPDATER_ENDPOINT_INVALID: 升级端点无效: {e}"))?
        // 评审 I1：Windows 安装路径插件内部直接 exit(0)，不走 RunEvent::Exit——
        // 挂共享退出清理，保证隧道/电源锁不孤儿化（macOS 走 restart→Exit，钩子冗余无害）
        .on_before_exit(crate::exit_cleanup)
        .build()
        .map_err(|e| format!("UPDATER_BUILDER_FAIL: 升级器构建失败: {e}"))?;
    let update = updater
        .check()
        .await
        .map_err(|e| format!("UPDATER_MANIFEST_VERIFY: 升级清单校验失败: {e}"))?
        .ok_or("远端清单不包含可用更新")?;

    let emit_progress = |event: &str, downloaded: u64, content_length: Option<u64>| {
        let _ = app.emit(
            PROGRESS_EVENT,
            ProgressPayload {
                event,
                downloaded,
                content_length,
            },
        );
    };

    let mut started = false;
    // 两个回调都要读 downloaded：FnMut 回调可变捕获 + FnOnce 回调不可变捕获会撞借用，
    // 共享原子计数绕开（async 命令要求 Future 为 Send，Cell 不可用）
    let downloaded = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let downloaded_chunk = downloaded.clone();
    update
        .download_and_install(
            |chunk, content_length| {
                if !started {
                    started = true;
                    emit_progress("Started", 0, content_length);
                }
                downloaded_chunk.fetch_add(chunk as u64, std::sync::atomic::Ordering::Relaxed);
                emit_progress(
                    "Progress",
                    downloaded_chunk.load(std::sync::atomic::Ordering::Relaxed),
                    content_length,
                );
            },
            || {
                emit_progress(
                    "Finished",
                    downloaded.load(std::sync::atomic::Ordering::Relaxed),
                    None,
                )
            },
        )
        .await
        .map_err(|e| format!("UPDATER_INSTALL_FAIL: 下载或安装失败: {e}"))?;

    // Windows：插件在 install 内部已 exit(0)（由 NSIS /UPDATE 装完重启），走不到这里；
    // macOS：换壳完成，重启进新版本（restart 返回 !，下方 Ok 仅非 macOS 路径可达）
    #[cfg(target_os = "macos")]
    app.restart();

    #[cfg(not(target_os = "macos"))]
    Ok(())
}

/// 扫除 base 目录下 `{app_name}-*-updater-*` 残留（仅 Windows 升级流产生；
/// 单实例保证不会有并发安装）。best-effort：单条失败跳过，绝不向上抛错。
pub fn cleanup_updater_temp_dirs_in(base: &std::path::Path, app_name: &str) {
    let prefix = format!("{app_name}-");
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if name.starts_with(&prefix) && name.contains("-updater-") {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// 启动钩子入口：清理系统临时目录里的 updater 残留
pub fn cleanup_updater_temp_dirs(app: &tauri::AppHandle) {
    cleanup_updater_temp_dirs_in(&std::env::temp_dir(), &app.package_info().name);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, prerelease: bool, with_asset: bool) -> GithubRelease {
        GithubRelease {
            tag_name: tag.to_string(),
            draft: false,
            prerelease,
            body: Some(format!("notes of {tag}")),
            html_url: format!("https://github.com/r/releases/tag/{tag}"),
            published_at: Some("2026-10-07T00:00:00Z".into()),
            assets: Some(if with_asset {
                vec![
                    GithubAsset {
                        name: "app.dmg".into(),
                        browser_download_url: format!("https://github.com/r/dl/{tag}/app.dmg"),
                    },
                    GithubAsset {
                        name: "latest.json".into(),
                        browser_download_url: format!("https://github.com/r/dl/{tag}/latest.json"),
                    },
                ]
            } else {
                vec![GithubAsset {
                    name: "app.dmg".into(),
                    browser_download_url: format!("https://github.com/r/dl/{tag}/app.dmg"),
                }]
            }),
        }
    }

    #[test]
    fn parses_v_prefixed_and_bare_tags() {
        let expected: semver::Version = "0.5.0-beta.1".parse().unwrap();
        assert_eq!(parse_tag_version("v0.5.0-beta.1").unwrap(), expected);
        assert!(parse_tag_version("0.4.1").is_some());
        assert!(parse_tag_version("not-a-version").is_none());
    }

    #[test]
    fn ipc_shape_serializes_variant_fields_camel_case() {
        // IPC 契约形状锁（2026-10-08 评审 Important）：internally-tagged enum 的
        // container 属性只作用于 variant 名，variant 字段默认蛇形输出——前端
        // lib/updater.ts 读 currentVersion，回退蛇形即运行时恒 undefined
        let available = serde_json::to_value(CheckUpdateStatus::Available {
            current_version: "0.4.1".into(),
            update: UpdateAvailable {
                version: "0.5.0-beta.1".into(),
                prerelease: true,
                notes: "notes".into(),
                html_url: "https://github.com/r/releases/tag/v0.5.0-beta.1".into(),
                published_at: None,
                tag: "v0.5.0-beta.1".into(),
                latest_json_url: "https://github.com/r/dl/v0.5.0-beta.1/latest.json".into(),
            },
        })
        .unwrap();
        assert_eq!(available["status"], "available");
        assert_eq!(available["currentVersion"], "0.4.1");
        assert!(available.get("current_version").is_none());
        assert_eq!(
            available["update"]["htmlUrl"],
            "https://github.com/r/releases/tag/v0.5.0-beta.1"
        );

        let up_to_date = serde_json::to_value(CheckUpdateStatus::UpToDate {
            current_version: "0.5.0-beta.1".into(),
            latest_version: "0.5.0-beta.1".into(),
        })
        .unwrap();
        assert_eq!(up_to_date["status"], "up-to-date");
        assert_eq!(up_to_date["currentVersion"], "0.5.0-beta.1");
        assert_eq!(up_to_date["latestVersion"], "0.5.0-beta.1");
        assert!(up_to_date.get("latest_version").is_none());

        let error = serde_json::to_value(CheckUpdateStatus::Error {
            message: "boom".into(),
        })
        .unwrap();
        assert_eq!(error["status"], "error");
        assert_eq!(error["message"], "boom");
    }

    #[test]
    fn picks_max_semver_including_prerelease() {
        // 发布顺序乱序：beta.2 先于 beta.1 记录（API 按 created_at 排序），semver 为准
        let list = vec![
            release("v0.5.0-beta.1", true, true),
            release("v0.4.1", false, true),
            release("v0.5.0-beta.2", true, true),
        ];
        assert_eq!(
            pick_latest_updatable(&list).unwrap().tag_name,
            "v0.5.0-beta.2"
        );
    }

    #[test]
    fn prerelease_beats_older_stable() {
        // 用户裁决：单通道，prerelease 也是合法升级目标
        let list = vec![
            release("v0.4.1", false, true),
            release("v0.5.0-beta.1", true, true),
        ];
        assert_eq!(
            pick_latest_updatable(&list).unwrap().tag_name,
            "v0.5.0-beta.1"
        );
    }

    #[test]
    fn stable_beats_same_minor_prerelease() {
        // semver 规则：0.5.0 > 0.5.0-beta.1
        let list = vec![
            release("v0.5.0-beta.1", true, true),
            release("v0.5.0", false, true),
        ];
        assert_eq!(pick_latest_updatable(&list).unwrap().tag_name, "v0.5.0");
    }

    #[test]
    fn skips_draft_and_assetless_releases() {
        let mut draft = release("v0.9.0", false, true);
        draft.draft = true;
        let assetless = release("v0.6.0", false, false);
        let normal = release("v0.5.0", false, true);
        let list = vec![draft, assetless, normal];
        assert_eq!(pick_latest_updatable(&list).unwrap().tag_name, "v0.5.0");
    }

    #[test]
    fn returns_none_when_nothing_updatable() {
        assert!(pick_latest_updatable(&[]).is_none());
        let list = vec![release("v0.6.0", false, false)];
        assert!(pick_latest_updatable(&list).is_none());
    }

    #[test]
    fn cleans_only_updater_dirs_matching_app_prefix() {
        let base =
            std::env::temp_dir().join(format!("mam-updater-cleanup-test-{}", std::process::id()));
        std::fs::create_dir_all(base.join("MyApp-1.2.3-updater-abc")).unwrap();
        std::fs::create_dir_all(base.join("MyApp-1.2.3-updater-xyz")).unwrap();
        std::fs::create_dir_all(base.join("OtherApp-9.9.9-updater-abc")).unwrap();
        std::fs::create_dir_all(base.join("MyApp-ordinary-dir")).unwrap();

        cleanup_updater_temp_dirs_in(&base, "MyApp");

        assert!(!base.join("MyApp-1.2.3-updater-abc").exists());
        assert!(!base.join("MyApp-1.2.3-updater-xyz").exists());
        assert!(base.join("OtherApp-9.9.9-updater-abc").exists());
        assert!(base.join("MyApp-ordinary-dir").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn validates_latest_json_url_prefix_and_suffix() {
        // 合法：本仓库 release 资产下的 latest.json
        assert!(validate_latest_json_url(
            "https://github.com/jarvislee90s-dot/tuvis/releases/download/v0.5.0-beta.1/latest.json"
        )
        .is_ok());
        // 他仓库前缀
        assert!(validate_latest_json_url(
            "https://github.com/evil/repo/releases/download/v9.9.9/latest.json"
        )
        .is_err());
        // 明文 http
        assert!(validate_latest_json_url(
            "http://github.com/jarvislee90s-dot/tuvis/releases/download/v0.5.0/latest.json"
        )
        .is_err());
        // 非 latest.json 资产
        assert!(validate_latest_json_url(
            "https://github.com/jarvislee90s-dot/tuvis/releases/download/v0.5.0/evil.exe"
        )
        .is_err());
    }

    // 真机联网用例（#[ignore]：CI 与常规回归不跑）——锁「配置了代理就拉得到 releases」
    // 不回归（用户挂代理场景）。reqwest 启用了 system-proxy（Cargo.toml M4 T1d），
    // 默认客户端读环境变量与 Windows 注册表 / macOS 系统代理；注册表层无法在测试里
    // 模拟，环境变量路径由本用例覆盖（同一套 Proxy::system 管道）：
    //   HTTPS_PROXY=http://127.0.0.1:<port> cargo test --lib \
    //     commands::updater::tests::fetches_releases_through_proxy -- --ignored --nocapture
    #[test]
    #[ignore = "需真机联网、GitHub API 可达且已设置代理环境变量；验证代理链路时手动运行"]
    fn fetches_releases_through_proxy() {
        // 前置断言：直连可达 GitHub 时本用例不带代理也会绿，测试名承诺的「经代理」
        // 就锁空了——缺代理环境变量的误跑必须快速失败，不给假阳性
        let has_proxy_env = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
            .iter()
            .any(|k| std::env::var(k).is_ok());
        assert!(
            has_proxy_env,
            "请先设置 HTTPS_PROXY（或 ALL_PROXY）指向可达 GitHub 的代理，再运行本用例"
        );
        let releases = tauri::async_runtime::block_on(fetch_github_releases())
            .expect("经代理拉取 GitHub releases 失败");
        assert!(!releases.is_empty(), "releases 列表不应为空");
    }

    #[test]
    fn extracts_notes_after_ci_section_marker() {
        // CI 拼装的 body 形态：标题 + 下载清单 + --- + ### 更新内容 + 说明本体。
        // 弹窗只该拿分节符之后的本体（0.5.2 用户实测：整段塞弹窗 = 第一屏全是
        // 与场景无关的下载清单，真内容在折叠线下，「没头没尾」）
        let body = "## Tuvis v0.5.2\n\
                    \n\
                    ### 下载\n\
                    \n\
                    - **macOS**: `Tuvis-0.5.2-macOS.dmg`\n\
                    \n\
                    ---\n\
                    \n\
                    ### 更新内容\n\
                    \n\
                    # 🔧 v0.5.2 更新 — 两处修复\n\
                    \n\
                    本次主要做两件事。";
        let notes = extract_release_notes(body);
        assert!(
            notes.starts_with("# 🔧 v0.5.2 更新"),
            "应从说明本体开头起：{notes}"
        );
        assert!(!notes.contains("下载"), "下载清单不得进弹窗：{notes}");
    }

    #[test]
    fn notes_without_marker_falls_back_to_whole_body() {
        // 旧版结构 / 手工 body 没有分节符 → 原样返回（不猜、不裁错）
        assert_eq!(extract_release_notes("# 手写说明"), "# 手写说明");
        assert_eq!(extract_release_notes(""), "");
    }

    #[test]
    fn flattens_error_source_chain_into_one_line() {
        // toast 的「失败原因第一眼可定位」依赖 source 链——拍平器必须逐层带上
        #[derive(Debug)]
        struct Leaf;
        impl std::fmt::Display for Leaf {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "invalid peer certificate: UnknownIssuer")
            }
        }
        impl std::error::Error for Leaf {}
        #[derive(Debug)]
        struct Top {
            cause: Leaf,
        }
        impl std::fmt::Display for Top {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "error sending request for url (https://api.github.com)")
            }
        }
        impl std::error::Error for Top {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.cause)
            }
        }

        let flat = flatten_error_chain(&Top { cause: Leaf });
        assert!(flat.contains("error sending request"), "顶层信息必须保留");
        assert!(
            flat.contains("invalid peer certificate"),
            "source 链的成因（DNS 污染假证书）必须带出：{flat}"
        );
    }

    #[test]
    #[ignore = "需真机联网（不需要代理）——DoH 钉 IP 直连兜底的实链路验证；\
                本机 DNS 污染/VPN 关闭环境恰是它的目标场景"]
    fn fetches_releases_via_doh_pinned_fallback() {
        let releases = tauri::async_runtime::block_on(fetch_releases_via_doh_pinned())
            .expect("DoH 钉 IP 直连拉取 releases 失败");
        assert!(!releases.is_empty(), "releases 列表不应为空");
    }
}
