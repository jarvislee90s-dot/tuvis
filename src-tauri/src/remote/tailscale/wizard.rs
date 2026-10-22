//! 首次配置引导（§C2「一条龙」）与**安装包供应链**：平台步骤表、安装包资产表（固定版本 +
//! 硬编码 sha256）、**下载并触发特权安装**、逐步探测与 `run_step` 单入口。
//!
//! **为什么单独成文件（评审「问题 14」）**：本文件是唯一接触"下载并运行第三方安装包"
//! 的地方（触发系统授权框，不静默提权），与 [`super::status`] 的通道运行态风险画像不同——
//! 审供应链改动时不必读轮询线程，反之亦然。

use super::reach::{
    apply_reachability, begin_verification, config_key, discard_verification, do_verify,
    invalidate_verification_lifecycle, observe_config, reachability, verify_with_heal,
    waiting_status_for, Reachability,
};
use super::status::{
    backend_initializing, disable_shields_up, find_cli, foreign_serve_config, funnel_active,
    funnel_status_unreadable, parse_status, run_cli, serve_entries, serve_ownership,
    start_channel_inner, stop_inner, ts_snapshot, FunnelOutcome, ServeResetReport, TsStatus,
};
use crate::remote::NoWindow;

const TAILSCALE_SYSTEM_EXTENSION_BUNDLE_ID: &str = "io.tailscale.ipn.macsys.network-extension";

/// `systemextensionsctl list` 中只认明确的 Tailscale `[activated enabled]` 证据。
fn system_extension_enabled_from(output: &str) -> Option<bool> {
    output
        .lines()
        .any(|line| {
            line.contains(TAILSCALE_SYSTEM_EXTENSION_BUNDLE_ID)
                && line.contains("[activated enabled]")
        })
        .then_some(true)
}

#[cfg(test)]
static SYSTEM_EXTENSION_LIST_OVERRIDE: once_cell::sync::Lazy<std::sync::Mutex<Option<String>>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(None));

#[cfg(test)]
fn set_system_extension_list_override(output: Option<&str>) {
    *SYSTEM_EXTENSION_LIST_OVERRIDE.lock().unwrap() = output.map(str::to_string);
}

fn system_extension_enabled() -> Option<bool> {
    if current_platform() != Platform::Mac {
        return None;
    }
    #[cfg(test)]
    {
        return SYSTEM_EXTENSION_LIST_OVERRIDE
            .lock()
            .unwrap()
            .as_deref()
            .and_then(system_extension_enabled_from);
    }
    #[cfg(not(test))]
    {
        let output = std::process::Command::new("systemextensionsctl")
            .arg("list")
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        system_extension_enabled_from(&String::from_utf8_lossy(&output.stdout))
    }
}

/// 安装器参数独立传递，不经 shell 解析下载路径。
fn installer_command(p: Platform, pkg: &std::path::Path) -> Result<std::process::Command, String> {
    let mut cmd = match p {
        Platform::Mac => {
            let mut cmd = std::process::Command::new("/usr/bin/open");
            cmd.args(["-a", "/System/Library/CoreServices/Installer.app"]);
            cmd
        }
        Platform::Windows => {
            let mut cmd = std::process::Command::new("msiexec");
            cmd.no_window().arg("/i");
            cmd
        }
        Platform::Other => {
            return Err("TSWIZ_PLATFORM_UNSUPPORTED: 当前平台暂不支持一键配置".into())
        }
    };
    cmd.arg(pkg);
    Ok(cmd)
}

fn installer_exit_error(label: &str, code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> String {
    let err = String::from_utf8_lossy(stderr);
    let out = String::from_utf8_lossy(stdout);
    let detail = if !err.trim().is_empty() {
        err.trim()
    } else if !out.trim().is_empty() {
        out.trim()
    } else {
        "no output"
    };
    format!("TSWIZ_INSTALLER_FAIL: {label} 退出码 {code:?}: {detail}")
}

/// 先在同目录临时落盘并校验，原子改名后才允许用户双击正式安装包。
fn stage_installer_with(
    dest: &std::path::Path,
    legacy: &std::path::Path,
    expected: &str,
    download: impl FnOnce(&std::path::Path) -> Result<(), String>,
) -> Result<(), String> {
    use crate::remote::tunnel::verify_sha256_file;
    if verify_sha256_file(dest, expected).is_ok() {
        return Ok(());
    }
    let parent = dest
        .parent()
        .ok_or("TSWIZ_DOWNLOAD_DIR_INVALID: 安装包下载目录无效")?;
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("TSWIZ_DOWNLOAD_DIR_FAIL: 创建下载目录失败: {e}"))?;
    let part = parent.join(format!(".tailscale-{:032x}.part", rand::random::<u128>()));
    let result = (|| {
        if verify_sha256_file(legacy, expected).is_ok() {
            std::fs::copy(legacy, &part)
                .map_err(|e| format!("TSWIZ_INSTALLER_COPY_FAIL: 复制已有安装包失败: {e}"))?;
        } else {
            download(&part)?;
        }
        verify_sha256_file(&part, expected)?;
        std::fs::rename(&part, dest)
            .map_err(|e| format!("TSWIZ_INSTALLER_SAVE_FAIL: 安装包保存失败: {e}"))
    })();
    if result.is_err() && part.exists() {
        if let Err(e) = std::fs::remove_file(&part) {
            log::warn!("清理未完成的 Tailscale 安装包失败: {e}");
        }
    }
    result
}

#[cfg(test)]
mod onboarding_tests {
    use super::*;
    const HELLO_SHA: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    #[test]
    fn mac_opens_system_installer_with_one_literal_package_argument() {
        let pkg = std::path::Path::new("/Users/a b/Downloads/a'$(echo bad).pkg");
        let cmd = installer_command(Platform::Mac, pkg).unwrap();
        assert_eq!(cmd.get_program(), "/usr/bin/open");
        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            vec![
                "-a",
                "/System/Library/CoreServices/Installer.app",
                pkg.to_str().unwrap()
            ]
        );
        let win = installer_command(Platform::Windows, pkg).unwrap();
        assert_eq!(win.get_program(), "msiexec");
        assert_eq!(
            win.get_args().collect::<Vec<_>>(),
            vec!["/i", pkg.to_str().unwrap()]
        );
    }

    #[test]
    fn installer_errors_include_exit_code_and_fall_back_to_stdout() {
        let e = installer_exit_error("installer", Some(1), b"Must be run as root", b"");
        assert!(e.contains("Must be run as root") && e.contains('1'), "{e}");
        let e = installer_exit_error("installer", None, b"", b"");
        assert!(!e.trim().is_empty());
        assert!(installer_exit_error("msiexec", Some(2), b"out", b"err").contains("err"));
    }

    #[test]
    fn download_is_staged_and_verified_before_package_becomes_visible() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("Downloads/Tailscale.pkg");
        let legacy = dir.path().join("missing.pkg");
        stage_installer_with(&dest, &legacy, HELLO_SHA, |part| {
            assert!(!dest.exists());
            assert_eq!(part.extension().unwrap(), "part");
            std::fs::write(part, b"hello").map_err(|e| e.to_string())
        })
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
        stage_installer_with(&dest, &legacy, HELLO_SHA, |_| panic!("有效包不得重下")).unwrap();
    }

    #[test]
    fn valid_legacy_package_is_copied_and_bad_download_never_promoted() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("Downloads/Tailscale.pkg");
        let legacy = dir.path().join("old.pkg");
        std::fs::write(&legacy, b"hello").unwrap();
        stage_installer_with(&dest, &legacy, HELLO_SHA, |_| panic!("应复用旧包")).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
        std::fs::remove_file(&dest).unwrap();
        std::fs::remove_file(&legacy).unwrap();
        assert!(stage_installer_with(&dest, &legacy, HELLO_SHA, |part| {
            std::fs::write(part, b"corrupt").map_err(|e| e.to_string())
        })
        .is_err());
        assert!(!dest.exists());
        assert_eq!(
            std::fs::read_dir(dest.parent().unwrap()).unwrap().count(),
            0
        );
        assert!(
            stage_installer_with(&dest, &legacy, HELLO_SHA, |_| Err("网络失败".into())).is_err()
        );
        assert!(!dest.exists());
    }

    #[test]
    fn system_extension_stopped_only_prompts_to_check_the_tailscale_client() {
        if current_platform() != Platform::Mac {
            return;
        }
        let readings = |backend_state: &str| CliReadings {
            status: Some(TsStatus {
                backend_state: backend_state.into(),
                auth_url: String::new(),
                dns_name: String::new(),
                cert_domains: Vec::new(),
            }),
            shields: None,
            funnel_json: Err("unused".into()),
            system_extension_enabled: None,
        };
        let running = probe_steps_from(9420, false, &readings("Running"));
        let running = running.iter().find(|s| s.id == "sys_ext").unwrap();
        assert!(running.done, "Running 已直接证明客户端可用，无需额外操作");

        let stopped = probe_steps_from(9420, false, &readings("Stopped"));
        let stopped = stopped.iter().find(|s| s.id == "sys_ext").unwrap();
        let reason = stopped.blocked_reason.as_deref().unwrap();
        assert!(reason.contains("请检查客户端"), "{reason}");
        assert!(
            reason.contains("无法据此判断系统扩展是否已批准"),
            "{reason}"
        );
        assert!(
            !reason.contains("批准系统扩展"),
            "不得把 Stopped 推断为未批准: {reason}"
        );
    }

    #[test]
    fn activated_system_extension_completes_before_login_without_marking_login_done() {
        if current_platform() != Platform::Mac {
            return;
        }
        let _serial = super::super::test_lock();
        let extension_row = "*\t*\tW5364U7YZB\tio.tailscale.ipn.macsys.network-extension (1.102.4/101.102.4)\tTailscale Network Extension\t[activated enabled]";
        let readings = CliReadings {
            status: Some(TsStatus {
                backend_state: "NeedsLogin".into(),
                auth_url: "https://login.tailscale.com/a/test".into(),
                dns_name: String::new(),
                cert_domains: Vec::new(),
            }),
            shields: None,
            funnel_json: Err("unused".into()),
            system_extension_enabled: Some(true),
        };
        let states = probe_steps_from(9420, true, &readings);
        assert!(states.iter().find(|s| s.id == "sys_ext").unwrap().done);
        assert!(!states.iter().find(|s| s.id == "login").unwrap().done);
        assert_eq!(system_extension_enabled_from(extension_row), Some(true));
        assert_eq!(
            system_extension_enabled_from(
                "*\t*\tW5364U7YZB\tio.tailscale.ipn.macsys.network-extension\tTailscale Network Extension\t[terminated waiting to uninstall on reboot]"
            ),
            None
        );

        // 状态装配链：CLI status=NeedsLogin + OS 已激活证据时，第4步完成、第5步仍待登录。
        set_system_extension_list_override(Some(extension_row));
        super::super::set_run_cli_override(Some(Box::new(|args: &[&str]| {
            match args {
            ["status", "--json"] => Ok(r#"{"BackendState":"NeedsLogin","AuthURL":"https://login.tailscale.com/a/test","Self":{"DNSName":""},"CertDomains":[]}"#.into()),
            ["get", "--json"] => Ok(r#"{"shields-up":true}"#.into()),
            ["funnel", "status", "--json"] => Ok("{}".into()),
            other => Err(format!("测试未应答的 CLI 调用: {other:?}")),
        }
        })));
        let probe = wizard_status_with(9420, true);
        let states = probe["states"].as_array().unwrap();
        assert!(
            states.iter().find(|s| s["id"] == "sys_ext").unwrap()["done"]
                .as_bool()
                .unwrap()
        );
        assert!(!states.iter().find(|s| s["id"] == "login").unwrap()["done"]
            .as_bool()
            .unwrap());
        set_system_extension_list_override(None);
        super::super::set_run_cli_override(None);
    }
}

// ============================================================
// 首次配置引导（§C2 一条龙）：平台步骤表 + 安装包资产表 + 逐步探测
// ============================================================

/// 目标平台（三值；`Other` = 不支持一键配置，向导只读探测如实报错）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Platform {
    Mac,
    Windows,
    Other,
}

pub(super) fn current_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::Mac
    } else if cfg!(target_os = "windows") {
        Platform::Windows
    } else {
        Platform::Other
    }
}

/// **Windows 探测的实测覆盖起点**（I-3：验证位不得大于证据）。
///
/// - **2026-10-07 第一次探测（历史）**：环境 = Windows 11 家庭版 / Tailscale 1.102.4
///   （MSI 安装）。当时照一份**一次性探测任务书**执行（该任务书已在探测完成后删除，
///   不在库内——2026-10-08 架构评审指出本注释还在引用它，故此处不再给失效路径），
///   **从第 6 步 `shields_up` 开始**（安装与登录此前已完成）——故当时的起点是
///   `shields_up`，之前的 detect / download / install(UAC) / login 四步**流程没被端到端
///   跑过**，只有字段形态被顺带核对过。
/// - **2026-10-07 用户实机确认（本轮依据，起点据此前移到第一步）**：用户在本机 Windows 上
///   **卸载 Tailscale 后从零走完 兔维斯 向导全程**（下载 → 安装 UAC → 登录 → 关 shields-up →
///   开通 Funnel → 可达性校验），**全程正常**，故上述四步的**流程**这一次是真的被端到端
///   跑过了。用户原话：「走的是 兔维斯 向导」「反正我感觉是足以指导没做过的用户一步一步点
///   下去了」。⇒ 起点前移到步骤表第一步（`detect`）：起点之前没有任何步骤 ⇒ 未验清单为
///   空 ⇒ `windowsVerified = true`，前端那条「Windows 只实测了后半段」的黄标随之撤下。
///   ⚠️ 这是一条**有据**的改动（用户亲口确认 + 实机走完），不是为了让提示消失而翻的位；
///   后人若要把起点再往前挪（或又发现某步没实测），**必须同样在提交里写明依据**。
///   **证据记录**：`docs/release-notes/windows-wizard-acceptance-2026-10-07.md`——如实标注为
///   **当事人陈述（不是机器可复核的产物）**，并登记了未覆盖/待真机复核的面（`tailscale login`
///   主动取链接、`--timeout 15s`、kill 等待者对 `AuthURL` 的影响）。引用本起点前先读那份记录。
///
/// **机制保留（不许因为"现在全绿了"就删）**：旧实现是一个**整行**布尔
/// `WINDOWS_VERIFIED = true`，UI 效果是整条提示随位撤下 ⇒ **验证位大于证据**。现结构是
/// 「实测覆盖从哪一步起（本常量）+ 没跑过的步骤逐条点名（[`unverified_before`] 派生）+
/// 前端弱提示（i18n `tsWizard.windowsUnverified`）」——本轮只改**事实**（起点前移），
/// **机制一字不动**：将来若又出现未实测的段落（新增步骤 / 新平台形态），把起点挪回那一步，
/// 清单与两个位自动跟着变（`windows_verification_list_is_derived_from_coverage_start` 锁）。
pub(super) const WINDOWS_VERIFIED_FROM: &str = "detect";

/// 没被端到端实机跑过的步骤 id（**按步骤表顺序派生**——表变了清单跟着变；非 Windows 恒空）。
/// 判据 = 步骤表里排在「实测覆盖起点」之前的那些步骤，单点常量，不另立会漂移的第二张清单。
///
/// **起点参数化**（[`unverified_before`]）：生产的起点是 [`WINDOWS_VERIFIED_FROM`]，
/// 而把起点当参数传是**为了让「机制仍在」可断言**——本轮的改动是**事实**（Windows 全流程
/// 已被实机跑过）而不是**机制**：起点后移时清单/两个位必须照旧跟着变（测试
/// `windows_verification_list_is_derived_from_coverage_start` 锁这条），将来若又出现
/// 未实测的段落，把起点挪回那一步即可。
fn unverified_before(start: &str, p: Platform) -> Vec<&'static str> {
    if p != Platform::Windows {
        return Vec::new();
    }
    wizard_steps(p)
        .into_iter()
        .map(|s| s.id)
        .take_while(|id| *id != start)
        .collect()
}

/// 生产口径：起点 = [`WINDOWS_VERIFIED_FROM`]
pub(super) fn windows_unverified_steps(p: Platform) -> Vec<&'static str> {
    unverified_before(WINDOWS_VERIFIED_FROM, p)
}

/// Windows 验证位载荷装配（纯函数：起点 + 未验清单 + 平台 → JSON）。三个字段**同源派生**，
/// 位由清单算出（`windowsVerified = unverified.is_empty()`）——所以「起点一挪，清单与两个位
/// 一起变」，不存在两处漂移。
fn verification_payload(
    start: &str,
    unverified: Vec<&'static str>,
    p: Platform,
) -> serde_json::Value {
    serde_json::json!({
        "windowsVerified": unverified.is_empty(),
        "windowsVerifiedFrom": if p == Platform::Windows {
            serde_json::json!(start)
        } else {
            serde_json::Value::Null
        },
        "windowsUnverifiedSteps": unverified,
    })
}

/// 生产口径：起点 = [`WINDOWS_VERIFIED_FROM`]（清单走具名的生产入口
/// [`windows_unverified_steps`]，保证生产与测试断言的是同一条派生）。
///
/// 载荷字段含义（纯函数、平台注入——测试可直接对 Windows 行断言）：
/// - `windowsVerified` = **整条** Windows 流程是否都实测过（起点必须是第一步才允许 true）；
/// - `windowsVerifiedFrom` = 实测覆盖从哪一步起（非 Windows 平台 null）；
/// - `windowsUnverifiedSteps` = 没被端到端实机跑过的步骤（前端据此点名，不写死清单文案）。
///
/// **谁在没有实机证据的情况下把 `windowsVerified` 弄成 true，测试必红**
/// （`wizard_status_payload_aligns_steps_with_states` +
/// `windows_verification_list_is_derived_from_coverage_start`）——真把整条流程跑过了才允许改。
pub(super) fn windows_verification_for(p: Platform) -> serde_json::Value {
    verification_payload(WINDOWS_VERIFIED_FROM, windows_unverified_steps(p), p)
}

/// 起点参数化版（[`unverified_before`] 的同一条理由：让「起点一挪清单就变」可断言）。
///
/// **`#[cfg(test)]` 是刻意的**：生产路径**只有** [`WINDOWS_VERIFIED_FROM`] 一个起点
/// （[`windows_verification_for`]），不存在"运行时换起点"这种可能——本函数是**测试缝**，
/// 用来证明派生机制仍然活着（变异：把 `unverified_before` 改成恒空表 / 把
/// `windowsVerified` 硬编码 true → `windows_verification_list_is_derived_from_coverage_start` 必红）。
#[cfg(test)]
pub(super) fn windows_verification_for_from(start: &str, p: Platform) -> serde_json::Value {
    verification_payload(start, unverified_before(start, p), p)
}

/// **写路径（动作）实机验证位**（B-M5，与 [`windows_verification_for`] 同纪律：**不得把
/// 未验证的写路径伪装成已验证**）。
///
/// - **macOS = false（零实机验证）**：本机只跑过**只读命令**（1.102.4，2026-10-06：
///   `status --json` / `funnel status --json` / `get --json`）；`funnel --bg`、
///   `funnel reset`、批准链接抓取**都没在本机跑过**。
/// - **Windows = true（2026-10-07 实测）**：探测提示词第 3/7/8 节三条写路径全部实机跑过
///   ——`set --shields-up=false`（写后回读为 false）、`funnel --bg 19999`（**退出码 0、
///   0.1 秒返回**，不加 `--bg` 则打印横幅并阻塞终端——A6）、`funnel reset`（退出码 0，
///   撤销后 `funnel status --json` 回到 `{}`）。
/// - **Other = false**：无资产、无探测。
///
/// 回填后逐平台置 true 并追记设计文档；`writePathVerified` 随向导载荷透出（前端据此
/// 决定是否显示弱提示）。
pub(super) fn write_path_verified(p: Platform) -> bool {
    match p {
        Platform::Mac => false,
        Platform::Windows => true,
        Platform::Other => false,
    }
}

/// 引导步骤（**平台数据，不是写死的流程**）。`id` 是稳定的机器标识，UI 文案走 i18n。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct WizardStep {
    pub id: &'static str,
    pub needs_human: bool,
    /// 该人工步**可能不出现**（上游按尾网策略/版本决定要不要人点）。
    /// 目前只有 `funnel` 一步：批准与否取决于该尾网 ACL 与 Tailscale 版本的默认策略，
    /// **不是本平台固定的必经步骤**（实测两平台各执一端，见 [`wizard_steps`]）。
    /// 语义：`needs_human && !human_optional` = 必需人工步；`needs_human && human_optional`
    /// = 「若出现则需人点，但可能压根不出现」——**向导不得据此等待**。
    pub human_optional: bool,
    /// 需要人时展示的动作文案键（i18n），例如 "settings.remote.tsWizard.actAdmin"
    pub human_action_key: &'static str,
}

/// macOS 九步（**已实测**）/ Windows 八步（**2026-10-07 实机校验回填**，见
/// [`WINDOWS_VERIFIED_FROM`]——起点已于同日由用户实机走完 兔维斯 向导全程后前移到第一步，
/// 即 Windows 整条流程已实测；字段核对表 7/7 与 macOS 一致）。
///
/// **人工步分「必需 / 可选」两类**（A1，2026-10-07 Windows 实测）：
/// - 必需 = 物理上必须由人做（安装授权框、macOS 系统扩展、浏览器登录）；
/// - 可选 = **可能不出现**的 `funnel` 批准步：Windows 11 家庭版 / Tailscale 1.102.4 MSI
///   实测 `funnel --bg` 退出码 0、0.1 秒返回、**零批准链接、零人工点击**（推测该尾网 ACL
///   已允许本节点 Funnel，或 1.102 默认策略变化）；macOS 1.102.4 实测首开**确有一次**
///   浏览器批准。**两平台各执一端 → 只能建模成「可能出现的分支」，不能建模成必经步骤。**
pub(super) fn wizard_steps(p: Platform) -> Vec<WizardStep> {
    let mut v = vec![
        WizardStep {
            id: "detect",
            needs_human: false,
            human_optional: false,
            human_action_key: "",
        },
        WizardStep {
            id: "download",
            needs_human: false,
            human_optional: false,
            human_action_key: "",
        },
        WizardStep {
            id: "install",
            needs_human: true,
            human_optional: false,
            // **2026-10-08（用户实测缺口）**：动作文案**逐平台分叉**。Windows 走的是 MSI，
            // 而 兔维斯 执行 `msiexec /i <包>` **刻意不加 /qn**——「装在哪里」的选择权本就该
            // 给用户（用户裁决：「你不能限制用户安装在哪里」）。既然路径由用户定，就必须
            // **事前**说清「用默认路径最省事；装到别处也可以，请记住那个路径」。
            // macOS 的 .pkg 由 `installer` 固定装到 /Applications，用户无从选择 ⇒ 保持
            // 原文案（把 Windows 特有的话塞给 mac 用户就是张冠李戴的谎报）。
            human_action_key: if p == Platform::Windows {
                "settings.remote.tsWizard.actAdminWinMsi"
            } else {
                "settings.remote.tsWizard.actAdmin"
            },
        },
    ];
    if p == Platform::Mac {
        // macOS 独有：系统扩展需去「系统设置」点一次允许（Apple 规定只有 MDM 能预批准，
        // 而 MDM 描述文件又禁止用户自行安装）
        v.push(WizardStep {
            id: "sys_ext",
            needs_human: true,
            human_optional: false,
            human_action_key: "settings.remote.tsWizard.actSysExt",
        });
    }
    v.extend([
        WizardStep {
            id: "login",
            needs_human: true,
            human_optional: false,
            human_action_key: "settings.remote.tsWizard.actLogin",
        },
        WizardStep {
            id: "shields_up",
            needs_human: false,
            human_optional: false,
            human_action_key: "",
        },
        WizardStep {
            id: "funnel",
            needs_human: true,
            // ← 唯一可选人工步：批准链接若出现才需人点（见函数文档的两平台实测）
            human_optional: true,
            human_action_key: "settings.remote.tsWizard.actFunnel",
        },
        WizardStep {
            id: "verify",
            needs_human: false,
            human_optional: false,
            human_action_key: "",
        },
        WizardStep {
            id: "autostart",
            needs_human: false,
            human_optional: false,
            human_action_key: "",
        },
    ]);
    v
}

/// 人工确认数（**必需, 可选**）——由步骤表派生，**不写死**（写死就会与平台数据漂移）。
/// §C2 出口标准：macOS ≤4 次 / Windows ≤3 次，本函数的值随之可被断言。
/// 2026-10-07 实测口径：macOS = 3 必需（安装/系统扩展/登录）+ 1 可选（批准，实测出现）；
/// Windows = 2 必需（安装 UAC/登录）+ 1 可选（批准，实测**未**出现，故本次探测范围内
/// 人工点击为 0）。
pub(super) fn human_step_counts(p: Platform) -> (usize, usize) {
    let steps = wizard_steps(p);
    let required = steps
        .iter()
        .filter(|s| s.needs_human && !s.human_optional)
        .count();
    let optional = steps
        .iter()
        .filter(|s| s.needs_human && s.human_optional)
        .count();
    (required, optional)
}

// ------------------------------------------------------------
// 安装包资产表（固定版本 + 硬编码 sha256，照 tunnel.rs:42-47 既有范式——
// 不去抓官方索引页取「最新版」，那是 TOFU，比固定版本弱）
// ------------------------------------------------------------

/// **固定版本**纪律：版本号内嵌在资产名常量里，升级 = 改常量 + 重取 sha256 表 +
/// 设计文档追记。
/// ⚠️ **实测陷阱（2026-10-06）**：官方索引页**顶部**那串「latest 1.102.5 1.102.4 …」
/// **指的是静态 Linux 包，不是 macOS/Windows 包**——照它去拼 Windows MSI 会 **404**。
/// **必须读页面对应 OS 段落里的文件名**。（当时 macOS 与 Windows 恰好都是 1.102.4。）
const TS_ASSET_MAC: &str = "Tailscale-1.102.4-macos.pkg";
/// MSI **分架构**——选错架构会装不上
const TS_ASSET_WIN_AMD64: &str = "tailscale-setup-1.102.4-amd64.msi";
const TS_ASSET_WIN_ARM64: &str = "tailscale-setup-1.102.4-arm64.msi";
const TS_ASSET_WIN_X86: &str = "tailscale-setup-1.102.4-x86.msi";

/// 编译目标架构段（纯函数）：arm64 / x86 / amd64 三值
fn win_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86") {
        "x86"
    } else {
        "amd64"
    }
}

/// 资产名（**与 `tunnel::asset_name()` 同形**：一个平台的安装包文件名）
pub(super) fn ts_asset_name(p: Platform) -> &'static str {
    match p {
        Platform::Mac => TS_ASSET_MAC,
        Platform::Windows => match win_arch() {
            "arm64" => TS_ASSET_WIN_ARM64,
            "x86" => TS_ASSET_WIN_X86,
            _ => TS_ASSET_WIN_AMD64,
        },
        Platform::Other => "",
    }
}

/// 下载地址（**与 `tunnel::download_url_for()` 同形**：只允许官方源）
pub(super) fn ts_download_url(p: Platform) -> String {
    format!("https://pkgs.tailscale.com/stable/{}", ts_asset_name(p))
}

/// 固定校验值。**全部于 2026-10-06 实测取自官方 `<文件URL>.sha256`**（官方明示 URL 后
/// 追加 `.sha256` 即得校验值；实测对 `.pkg` 与 `.msi` **均有效**；四个文件的 HTTP
/// 可达性也已实测——macOS .pkg 与三个 MSI 全部 200）。
/// 与 `tunnel.rs` 的 `expected_sha256` 表同一纪律：**改版本必须同步改这里**。
const SHA_MAC: &str = "b40b733af76233fd1e4af7acaeb325268e55e6818c15c6e9aa9e78f427245c5b";
const SHA_WIN_AMD64: &str = "80eb007e39dfebe17299fa1a09c79a8e1d934f76e0246c0817ebe3af675b7ef6";
const SHA_WIN_ARM64: &str = "b7dd1c03bf2e2c430f1fffc4e47ef92829c86d5190febbd9c025dcada5f410b6";
const SHA_WIN_X86: &str = "a8bda9fb254374bb13d46ebf02b6ffba4ed009a739580be511aa7afa8dddd42d";

/// 按资产名查固定校验值（**与 `tunnel::expected_sha256(asset)` 同形**：
/// 未知资产返回 Err，不返回空串——那是 fail-open）。
/// 查表键直接用资产名常量——资产名与校验值的对应关系由编译期保证，不会各改各的
pub(super) fn ts_expected_sha256(asset: &str) -> Result<&'static str, String> {
    match asset {
        TS_ASSET_MAC => Ok(SHA_MAC),
        TS_ASSET_WIN_AMD64 => Ok(SHA_WIN_AMD64),
        TS_ASSET_WIN_ARM64 => Ok(SHA_WIN_ARM64),
        TS_ASSET_WIN_X86 => Ok(SHA_WIN_X86),
        other => Err(format!("TSWIZ_ASSET_UNKNOWN: 未知安装包资产: {other}")),
    }
}

/// 与导出共用系统下载目录解析及 reveal 白名单，支持 Windows 下载目录重定向。
pub(super) fn ts_installer_path() -> std::path::PathBuf {
    crate::commands::export::exports_dir().join(ts_asset_name(current_platform()))
}

/// 仅下载动作复用旧包；只读探测不搬文件。
fn legacy_installer_path() -> std::path::PathBuf {
    crate::database::connection::app_data_home()
        .join(".tuvis/downloads")
        .join(ts_asset_name(current_platform()))
}

/// 安装包文件可信（纯 FS 判定）：文件在且 sha256 对得上才算——半截下载/被篡改的包
/// 不得进安装步。平台无资产（Other）恒 false。
/// **流式哈希**（B-M6）：与 `tunnel::verify_sha256` 同一比对单点，但不把整包读进内存
/// （数十 MB 的 .pkg/.msi 每次探测都整读会白占内存与 IO）
pub(super) fn installer_file_ok(p: Platform, path: &std::path::Path) -> bool {
    let Ok(expected) = ts_expected_sha256(ts_asset_name(p)) else {
        return false;
    };
    crate::remote::tunnel::verify_sha256_file(path, expected).is_ok()
}

/// macOS 打开系统安装界面；Windows 保持交互式 MSI。授权由用户在系统界面完成。
pub(super) fn run_installer(pkg: &std::path::Path) -> Result<(), String> {
    let mut cmd = installer_command(current_platform(), pkg)?;
    let label = cmd.get_program().to_string_lossy().into_owned();
    let out = cmd.output().map_err(|e| {
        let error = format!("TSWIZ_INSTALLER_FAIL: 打开 {label} 失败: {e}");
        log::warn!("{error}");
        error
    })?;
    if !out.status.success() {
        let error = installer_exit_error(&label, out.status.code(), &out.stdout, &out.stderr);
        log::warn!("{error}");
        return Err(error);
    }
    Ok(())
}

/// install 步内核（可测，路径与触发器注入；run_step 传生产值 `ts_installer_path()` +
/// `run_installer`）：① exists 门禁（缺包指引先走 download 步）→ ② **sha256 门禁**
/// （修复轮 1，评审 Important）：install 可从 UI/IPC 独立触发，落位路径上被替换/
/// 陈旧的安装包不得直达提权安装框（用户随后要输管理员密码/UAC）——固定 sha256 的
/// 端到端保管链在最后一跳收口；校验不过如实 Err 并指引重新下载（幂等语义随之做实）
/// → ③ 触发。触发闭包注入是零进程变异锚点：门禁被删时测试的 panic 替身立即爆红
pub(super) fn install_step_with(
    p: Platform,
    path: &std::path::Path,
    trigger: impl FnOnce(&std::path::Path) -> Result<(), String>,
) -> Result<serde_json::Value, String> {
    // B-M6②：平台门在 sha 门之前——Other 平台没有资产（资产名是空串），旧顺序会先落到
    // 「sha256 校验失败」的误导文案；如实说「当前平台不支持一键配置」
    if ts_asset_name(p).is_empty() {
        return Err("TSWIZ_PLATFORM_UNSUPPORTED: 当前平台暂不支持一键配置".into());
    }
    if !path.exists() {
        return Err("TSWIZ_INSTALLER_NOT_READY: 安装包未就位——请先执行「下载安装包」一步".into());
    }
    if !installer_file_ok(p, path) {
        return Err(
            "安装包 sha256 校验失败（可能被替换或下载不完整）——请重新执行「下载安装包」一步".into(),
        );
    }
    trigger(path)?;
    // 「触发成功」≠「安装完成」：是否装上以 probe 的 detect/install 判据为准
    Ok(serde_json::json!({ "ok": true, "triggered": true }))
}

/// 从 CLI 输出里捞批准链接（纯函数，可测）：取首个 https:// 且含 tailscale.com 的词，
/// 剥掉句尾标点。兔维斯 只把链接递出去，**不代点**
pub(super) fn extract_approval_url(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|w| w.starts_with("https://") && w.contains("tailscale.com"))
        .map(|w| w.trim_end_matches(['.', ',', ')']).to_string())
}

// ============================================================
// ⑤ 登录步（兔维斯 不代登录——只递链接；但**必须主动把链接取来**）
// ============================================================

/// 登录步轮询窗上限（注入 `wait` 的累计时长必须 ≤ 它——测试逐条断言，变异即红）
pub(super) const LOGIN_LINK_POLL_WINDOW: std::time::Duration =
    std::time::Duration::from_millis(9500);

/// 轮询节奏（首次读**立即**发生，之后按本表的间隔重读）。累加 = 9500ms = 上面那个窗：
/// `300+500+700+1000×8`——授权链接通常在 1 秒内就生成（只等尾网一次往返），
/// 这个节奏既能让常见情形一次就拿到，又不会在异常情形下无限轮询。
pub(super) const LOGIN_LINK_POLL_DELAYS_MS: &[u64] = &[
    300, 500, 700, 1000, 1000, 1000, 1000, 1000, 1000, 1000, 1000,
];

/// 登录步内核（可测；注入 = 读状态 / 发起登录 / 等待）。
///
/// **为什么必须主动发起**（2026-10-07 用户实测诊断）：新装机器上
/// `tailscale status --json` 是 `NeedsLogin ∧ AuthURL=""`——授权链接由尾网在
/// **发起一次交互式登录**时才生成（`tailscale login`，与 Windows GUI 的「Log in」
/// 按钮同义）。兔维斯 此前从不发起（全仓 `run_cli` 调用点无 `up`/`login`），于是
/// `AuthURL` 永远是空串，而前端只在 `authUrl` 非空时才渲染「去登录」⇒ **登录步是死胡同**
/// （行里写着"需要你操作：去浏览器登录"，却没有任何可点的东西）。
///
/// **合规红线（一字不让）**：兔维斯 **不代登录、不持有任何用户凭据**（模块头注释即此条）。
/// 本步只做两件事：① 让尾网**生成**一个授权链接（发起登录尝试，不带任何凭据参数）；
/// ② 把链接**原样递出去**。登录本身始终由用户在浏览器完成。
///
/// **有界**：发起是后台的（`trigger` 只 spawn 不等待，见 [`spawn_login_attempt`] 的形态针），
/// 轮询按 [`LOGIN_LINK_POLL_DELAYS_MS`] 走、窗上限 [`LOGIN_LINK_POLL_WINDOW`]；拿不到就
/// 如实回空串 + 成因（前端给"去客户端点登录"的兜底文案），**绝不无限等**。
///
/// **等待者的生命周期（I1，2026-10-08 架构评审；注入口 = `cleanup`）**：子进程由
/// [`spawn_login_attempt`] 登记进单飞槽，本函数**每次成功返回前**都经 `cleanup` 交代它的去向
/// （[`finish_login_attempt`]）——窗尽且没拿到链接 ⇒ [`LoginCleanup::Kill`]（否则它会按
/// 默认 0s 一直等，兔维斯 退出后成孤儿）；拿到链接 / 已登录 ⇒ [`LoginCleanup::Keep`]
/// （**保守**：杀是否让已生成的 AuthURL 失效**待真机复核**，见 [`LoginCleanup`] 的注释）。
/// 把收尾做成注入口（而不是在本函数里就地杀）是为了让"什么时候杀"成为**可断言的行为**
/// 而不是一句注释：`login_step_settles_waiter_conservatively` 锁死两条路各自的决策。
///
/// 幂等：已 `Running` → 不发起、直接 `done`；已有链接 → 不发起、直接递出
/// （用户可能正拿着那条链接在浏览器里操作）。
pub(super) fn login_step_with(
    read: impl Fn() -> Result<TsStatus, String>,
    trigger: impl FnOnce() -> Result<(), String>,
    wait: impl Fn(std::time::Duration),
    cleanup: impl Fn(LoginCleanup),
) -> Result<serde_json::Value, String> {
    let first = read()?;
    // ① 已登录（Running）：本步已完成，链接无意义（已登录时 AuthURL 本就是空串）
    if first.backend_state == "Running" {
        cleanup(LoginCleanup::Keep); // 只收尸：前一轮若有等待者，它的等待条件也已满足
        return Ok(serde_json::json!({
            "ok": true,
            "done": true,
            "backendState": first.backend_state,
            "authUrl": "",
        }));
    }
    // ② 已有链接：直接递出去（**幂等**——用户可能正拿着它那条在浏览器里操作，不得再发起）
    if !first.auth_url.is_empty() {
        cleanup(LoginCleanup::Keep); // 有链接 ⇒ 不杀（保守口径见 LoginCleanup）
        return Ok(serde_json::json!({
            "ok": true,
            "done": false,
            "backendState": first.backend_state,
            "authUrl": first.auth_url,
        }));
    }
    // ③ 链接还没生成（新装形态）：后台发起一次登录尝试 → 有界轮询把它取回来
    trigger()?;
    let mut last_state = first.backend_state;
    for delay in LOGIN_LINK_POLL_DELAYS_MS {
        wait(std::time::Duration::from_millis(*delay));
        let s = read()?;
        // 轮询期间用户在浏览器里点完了 → 直接报完成（前端随后重探也会看到 Running）
        if s.backend_state == "Running" {
            cleanup(LoginCleanup::Keep); // 已登录：CLI 的等待条件已满足，会自己退
            return Ok(serde_json::json!({
                "ok": true,
                "done": true,
                "backendState": s.backend_state,
                "authUrl": "",
                "triggered": true,
            }));
        }
        if !s.auth_url.is_empty() {
            cleanup(LoginCleanup::Keep); // **拿到链接一律不杀**（待真机复核，见 LoginCleanup）
            return Ok(serde_json::json!({
                "ok": true,
                "done": false,
                "backendState": s.backend_state,
                "authUrl": s.auth_url,
                "triggered": true,
            }));
        }
        last_state = s.backend_state;
    }
    // 有界放弃：如实回空串 + 成因（**不编链接、不谎报**），并**收掉那个还在等的子进程**
    // （I1：窗尽且没拿到链接 ⇒ 再等下去不会有链接，却会按默认 0s 一直等、兔维斯 退出后成孤儿）
    cleanup(login_cleanup_decision(false, false));
    // 前端据 authUrl 为空给出「去客户端点 Log in / 稍候重试」的兜底文案（i18n 静态键，
    // 不渲染本 note——中文串直接上屏会绕过 i18n，消费方约定见 src/lib/api/remote.ts 的
    // `note` 注释）。
    Ok(serde_json::json!({
        "ok": true,
        "done": false,
        "backendState": last_state,
        "authUrl": "",
        "triggered": true,
        "note": format!(
            "已发起登录，但在 {:?} 内没有拿到授权链接（后端状态 {last_state}）——可在开始菜单\
             打开 Tailscale 客户端点「Log in」，或稍候重试；兔维斯 只递链接、不代登录",
            LOGIN_LINK_POLL_WINDOW
        ),
    }))
    // **已知边界（2026-10-08 收口轮登记）**：上面的 `?` 错误返回（首次读状态 / 发起登录 /
    // 轮询读状态，共三条）**不经 `cleanup`**——故本文档的措辞是"每次**成功**返回前"，不是
    // "每次返回前"。那三条路径下的槽要么还是空的（尚未 spawn），要么留一个会在
    // `--timeout 15s` 内自然退出、且由应用退出钩子 `cancel_login_attempt` kill + wait 的句柄
    // ——不是无界泄漏（I1 的出口条件仍然成立）。
}

/// 有界等待参数（I1）：`login` 的 `--timeout` **默认 0s = 一直等**（1.102.4 `login --help`
/// 原文「default (0s) blocks forever」）——用户不登录，那个进程就一直活着。15s 的口径 =
/// 轮询窗（[`LOGIN_LINK_POLL_WINDOW`] 9.5s）+ 一次尾网往返的余量：常见情形 1 秒内就拿到
/// 链接，15s 是"用户在浏览器里慢慢点"与"绝不留一个永久等待者"之间的取舍。
pub(super) const LOGIN_TIMEOUT_ARG: &str = "15s";

/// **登录发起的完整 argv（唯一出处，I2）**——合规红线「**不带任何凭据参数、不碰偏好**」
/// 靠这条白名单从注释变成可执行约束（`login_attempt_args_are_a_closed_whitelist` 断言其
/// 内容封闭、`login_attempt_spawns_detached_and_never_waits` 断言 spawn 只从这里取参数）。
///
/// **为什么"零旗标"才是真正让本步安全的东西**（2026-10-08 架构评审更正：旧注释的理由与
/// 本机 CLI 自述不符）：
/// - 本机 1.102.4 `up --help` 原文：「**With no flags, "tailscale up" brings the network
///   online without changing any settings.**」——不存在"`up` 会按默认值改偏好"这回事；
///   有旗标时上游反而**拒绝**静默改设置（原文：若某个未显式给出的默认值会导致设置变化，
///   直接返回错误，除非带 `--reset`）；
/// - `login --help` 暴露的是**与 `up` 同一套偏好旗标**（`--shields-up` / `--advertise-*` /
///   `--accept-*` / `--exit-node` …）**以及凭据旗标**（`--auth-key` / `--client-secret` /
///   `--id-token`）——所以"用 login 就天然不碰偏好/凭据"是**错的**：安全来自我们
///   **一个旗标都不带**（除有界的 `--timeout`），而不是来自子命令名。
/// - ⚠️ **`login` 自标 alpha**（原文「This command is currently in alpha and may change in
///   the future.」）——上游随时可能改语义/旗标集；后人动本步前**先跑 `login --help` 复核**，
///   别照抄旧结论。
pub(super) const LOGIN_ARGS: &[&str] = &["login", "--timeout", LOGIN_TIMEOUT_ARG];

/// **登录 argv 的唯一出处（纯函数，可断言；2026-10-08 收口轮补强）**：返回 [`LOGIN_ARGS`] 本身。
///
/// 为什么要有这一层：旧的合规红线守卫是**扫字符串**（`body.contains(".args(LOGIN_ARGS)")`
/// 且 `!body.contains(".arg(")`），而**在调用点再补一行 `.args(&["--auth-key", …])` 两个断言
/// 都不会红**——`.args(` 不含子串 `.arg(`，白名单测试又只管常量本身、不管调用点。兔维斯 绝不持
/// 凭据是**合规红线**，不能靠一条可绕过的扫描守着。现在 argv 的拼装被收进唯一一处
/// （[`login_command`]），并由**行为断言**直接读真实 `Command` 的 `get_args()`
/// （`login_attempt_command_carries_only_the_whitelist_argv`）——任何就地追加的参数都必红。
///
/// 恒等断言 `login_argv() == LOGIN_ARGS` 把"生产实际用的那条 argv"与"被内容白名单锁死的
/// 常量"钉在一起（`login_attempt_args_are_a_closed_whitelist`）：改任何一边都红。
pub(super) fn login_argv() -> &'static [&'static str] {
    LOGIN_ARGS
}

/// 轮询收尾时对等待者的处置（I1）。
///
/// **决策从哪来（2026-10-08 收口轮如实更正）**：生产里**只有「窗尽且没拿到链接」那 1 条**
/// 走决策表（[`login_cleanup_decision`]），其余 **4 条 Ok 返回处是硬编码 [`LoginCleanup::Keep`]**
/// （已登录 / 已有链接 / 轮询中已登录 / 轮询中拿到链接）——今天两条口径同值，**无行为差异**；
/// 旧文档写"纯函数算出来"，是对生产形态的以偏概全。
///
/// ⚠️ **漂移风险（登记在案）**：将来若真机复核后调整决策表（例如把 `has_link` 也判 `Kill`），
/// **那 4 处硬编码不会跟着变**——`login_step_settles_waiter_conservatively` 抓不到这种漂移
/// （它注入的 `cleanup` 只看两条路各自的决策，看不到"另外 4 处没走决策表"）。动保守口径时
/// **必须同时人工复核那 4 个调用点**。控制方判定：为这 4 处引入位置布尔/枚举**不划算**，
/// 本轮只登记、不改代码。
///
/// ⚠️ **待真机复核（本机无法验证 Windows 行为）**：杀掉等待者会不会让**已经生成的
/// AuthURL** 失效？在拿到答案之前，杀法取**保守口径**——只有"轮询窗已尽**且拿不到链接**"
/// 才杀（那种情形下没有任何链接可被影响），**拿到链接一律不杀**，交给 `--timeout 15s`
/// 自己退（有界）。真机复核项：① `tailscale login --timeout 15s` 超时退出后，
/// `status --json` 的 `AuthURL` 是否仍在；② kill 掉等待者后 AuthURL 是否仍在。两条任一
/// 为"失效"⇒ 需要重新设计（例如把登录等待者交给 GUI 客户端）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LoginCleanup {
    /// 杀 + 收尸（窗尽且没拿到链接：等下去没有意义）
    Kill,
    /// 只做无阻塞收尸（自然退出清槽）；**不杀**
    Keep,
}

/// 收尾决策（纯函数、可测）：`done` = 后端已 Running；`has_link` = 本轮拿到了授权链接。
/// 变异：把 `has_link` 也判成 `Kill`（"杀了一了百了"）→
/// `login_cleanup_decision_is_conservative` 必红——那是**没有真机证据就改保守口径**。
pub(super) fn login_cleanup_decision(done: bool, has_link: bool) -> LoginCleanup {
    if done || has_link {
        LoginCleanup::Keep
    } else {
        LoginCleanup::Kill
    }
}

/// **登录等待者的单飞槽（I1）**：同一时刻至多一个 `tailscale login`。
///
/// 为什么是 `Mutex<Option<Child>>` 而不是"spawn 完起个收尸线程"（旧实现）：旧形态下子进程
/// 的所有权在收尸线程手里，**外面谁也杀不掉它** ⇒ 轮询失败后没有任何清理路径，兔维斯 退出后
/// 就是一个孤儿进程（评审 I1 三条中的第 3 条）。把句柄放在模块级槽里，窗尽/应用退出才
/// 有"可杀之物"；顺带把每次点击泄漏的**一个阻塞线程**去掉（旧收尸线程）。
///
/// 收尸纪律：自然退出的子进程在**下一次**发起/收尾时被清出槽位（发起走持守卫的
/// `reap_login_attempt_locked`，收尾走其免锁包装 [`reap_login_attempt`]；都是
/// `try_wait`，无阻塞）——"槽里至多留一个『已退出但未收尸』的句柄，不是一个活着的进程"
/// 这句话**只对"自然退出之后"成立**（2026-10-08 收口轮限定语境：旧措辞漏了这个前提）。
///
/// ⚠️ **单飞设计的常态恰恰是槽里有一个『活着』的等待者**：拿到授权链接后走
/// [`LoginCleanup::Keep`] 就是**有意让它继续活着**——直到 `--timeout 15s` 自然退出，
/// 或应用退出钩子 [`cancel_login_attempt`] 把它 kill + wait。故"槽里有一个活着的进程"
/// 是**正常状态**而非异常（单飞门正是为它而设：拦掉第二个等待者）；
/// 应用退出路径 [`cancel_login_attempt`] 一律 kill + wait。
static LOGIN_CHILD: once_cell::sync::Lazy<std::sync::Mutex<Option<std::process::Child>>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(None));

/// 取槽锁（毒化容忍：本静态被**退出钩子**（[`cancel_login_attempt`]）触碰，释放路径绝不
/// 允许因一次 panic 毒化而连环失败；槽里装的只是进程句柄，没有需要保持一致性的不变式）
fn login_child_lock() -> std::sync::MutexGuard<'static, Option<std::process::Child>> {
    LOGIN_CHILD.lock().unwrap_or_else(|e| e.into_inner())
}

/// 登记刚 spawn 的等待者（**单飞槽的唯一写入口**，带守卫版）：在 [`spawn_login_attempt`]
/// 的临界区内调用——**登记与 spawn 同临界区**正是 TOCTOU 修复的核心。旧形态（spawn 之后
/// 放开锁、由独立取锁的 `adopt_login_child` 登记）里，并发的第二次发起可以在两次取锁
/// 之间插队，第二次登记**覆盖**第一次的句柄（第一个进程 15 秒内无人可杀）。
fn adopt_login_child_locked(
    slot: &mut std::sync::MutexGuard<'static, Option<std::process::Child>>,
    child: std::process::Child,
) {
    **slot = Some(child);
}

/// 槽是否为空（**单飞不变式的读侧**，测试断言用；不含任何进程操作）。
///
/// 2026-10-08 TOCTOU 修复：生产判空**移进了临界区**——[`spawn_login_attempt`] 持锁后
/// 直接看 `slot.is_some()`（判空与 spawn/登记之间不再有放锁窗口）；本函数保留为测试
/// 可见的免锁快照，读的是同一个槽（仅测试构建存在，与 `reach::set_reachability`
/// 的再导出纪律同款）。
#[cfg(test)]
pub(super) fn login_child_slot_is_empty() -> bool {
    login_child_lock().is_none()
}

/// 无阻塞收尸（带守卫版，临界区内复用同一守卫、不二次取锁）：
/// 已自然退出的子进程清出槽位（不阻塞、不杀）
fn reap_login_attempt_locked(
    slot: &mut std::sync::MutexGuard<'static, Option<std::process::Child>>,
) {
    if let Some(child) = slot.as_mut() {
        if matches!(child.try_wait(), Ok(Some(_))) {
            **slot = None;
        }
    }
}

/// 无阻塞收尸：已自然退出的子进程清出槽位（不阻塞、不杀）
pub(super) fn reap_login_attempt() {
    let mut slot = login_child_lock();
    reap_login_attempt_locked(&mut slot);
}

/// **收尾口（应用退出 / 窗尽）**：把还在等待的 `tailscale login` kill + wait，不留孤儿。
/// 槽空 = no-op（退出钩子在"用户一次都没点过"的形态下也会调它）。
pub(crate) fn cancel_login_attempt() {
    let child = login_child_lock().take();
    if let Some(mut child) = child {
        let _ = child.kill();
        let _ = child.wait(); // 收尸（防僵尸）；kill 之后 wait 是有界的
    }
}

/// 轮询收尾的生产口径：`Kill` → 杀 + 收尸；`Keep` → 只收尸（自然退出的清槽）
pub(super) fn finish_login_attempt(decision: LoginCleanup) {
    match decision {
        LoginCleanup::Kill => cancel_login_attempt(),
        LoginCleanup::Keep => reap_login_attempt(),
    }
}

/// **登录命令的唯一拼装点**（纯构造：不 spawn、不等待、不碰管道；2026-10-08 收口轮补强）。
///
/// 抽出来的理由是**让红线可断言**：测试拿到的是生产**真正会 spawn 的那个 `Command` 对象**
/// （`Command::get_args()` 直读），于是"在调用点再拼一个参数"（例如
/// `.args(&["--auth-key", "x"])`）**必红**——不再依赖扫源码字符串。
///
/// 生产调用点 [`spawn_login_attempt`] 体内**不得出现任何 argv 拼装**（形态针
/// `login_attempt_spawns_detached_and_never_waits` 连 `.arg(` / `.args(` / `.args_os(` /
/// `.raw_arg(` 及其关联函数形态一起禁），argv 只能经本函数来。
pub(super) fn login_command(bin: &std::path::Path) -> std::process::Command {
    use std::process::Stdio;
    let mut cmd = std::process::Command::new(bin);
    cmd.args(login_argv()) // 唯一 argv 出处（I2 白名单）
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // 防闪窗：login 等待者最长存活 15s，漏加 = 一个常驻黑窗（比轮询闪窗更显眼）
    cmd.no_window();
    // 与 run_cli 同一条 macOS 适配（GUI standalone build 才认这条 CLI 入口）
    #[cfg(target_os = "macos")]
    cmd.env("TAILSCALE_BE_CLI", "1");
    cmd
}

/// 生产发起入口：**后台**起一次 `tailscale login`（只让尾网生成授权链接，不带任何旗标）。
///
/// **为什么是 `login` 而不是 `up`，以及为什么"安全"来自零旗标**：事实版理由与上游原文见
/// [`LOGIN_ARGS`]（旧注释写"`up` 会连带按默认值改偏好"，与本机 1.102.4 的 `up --help`
/// 自述不符，已更正；顺带标注 `login` 自标 alpha）。
///
/// **为什么必须后台（不能同步等）**：`tailscale login` 的 `--timeout` 默认 0s = **一直等**
/// （实测帮助原文「default (0s) blocks forever」）——同步等它就是把向导挂死（本项目刚修过
/// `wait_child_bounded` 那类「命令阻塞把 UI 挂死」的缺陷）。故只 `spawn`：三条标准流全给
/// null（不占管道、不因管道写满而假死），**不另起收尸线程**——句柄登记进单飞槽
/// （[`adopt_login_child`]），窗尽/应用退出由 [`finish_login_attempt`] /
/// [`cancel_login_attempt`] 收（旧实现的收尸线程既杀不掉子进程，又每次泄漏一个阻塞线程）。
/// 形态针见 `login_attempt_spawns_detached_and_never_waits`（出现任何等待/收取输出的调用
/// 即红，argv 不许就地拼）；argv 本身的行为由 [`login_command`] 一处拼装、由其行为测试锁死。
///
/// **单飞（I1）**：槽里还有活着的等待者就**不再起第二个**——用户连点两次不该派生两个进程
/// （第二个也没有额外信息：链接由 tailscaled 持有，轮询照旧能取到）。判空与 spawn/登记
/// 在**同一临界区**（2026-10-08 评审 Important #2 的 TOCTOU 修复，见函数体内注释）。
///
/// **不代登录**：argv 里没有任何凭据参数、不读任何凭据、不解析登录结果——只让尾网生成链接。
pub(super) fn spawn_login_attempt() -> Result<(), String> {
    // CLI 发现（IO）在临界区外做——临界区里只留「判空 → spawn → 登记」三件事
    let bin = find_cli().ok_or_else(|| "未检测到 Tailscale（尚未安装）".to_string())?;
    // **一锁到底（TOCTOU 修复）**：旧形态里 reap / 判空 / 登记三次各自取锁，中间还放开锁
    // 做 find_cli 的 IO 与 spawn——两个并发的 `run_step("login")`（向导重挂后旧步复位再点
    // 一次等场景）可同时通过判空 ⇒ 双 spawn，第二次登记**覆盖**第一个句柄：第一个进程
    // 15 秒内无人可杀（`--timeout 15s` 有界，但 兔维斯 退出时 [`cancel_login_attempt`] 只够到
    // 槽里那个）。判空+spawn+登记并入同一临界区后，「同一时刻至多一个等待者」才真正成立。
    // 持锁跨 spawn（毫秒级系统调用，无 await、无其它锁序）不会长阻塞；本函数跑在
    // spawn_blocking 线程上，短暂等锁不等用户。
    let mut slot = login_child_lock();
    reap_login_attempt_locked(&mut slot); // 先收掉已经自然退出的（单飞判据只看活着的）
    if slot.is_some() {
        return Ok(()); // 单飞：已有等待者在跑，不再派生第二个
    }
    // 命令与 argv 一律由 login_command 一处拼装（本函数体内不得出现 argv 拼装，形态针锁死）
    let mut cmd = login_command(&bin);
    let child = cmd
        .spawn()
        .map_err(|e| format!("TSWIZ_LOGIN_FAIL: 启动 `tailscale login` 失败: {e}"))?;
    adopt_login_child_locked(&mut slot, child);
    Ok(())
}

// ------------------------------------------------------------
// 逐步探测（remote_ts_probe 的内核）：每步都要能说出「怎么算完成」
// ------------------------------------------------------------

/// 卡住的**语义档位**（M4，2026-10-07 评审 Minor）：线稿 82-84 明写琥珀档
/// 「**既不是故障（rose）也不是正常完成（green）**，是"正在进行、无需操作"的中间态」
/// ——开机恢复窗口里的 `funnel` / `shields_up` 行属于这一档，不得涂成红色「卡住」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BlockedTone {
    /// 真故障（默认）：要人排查
    Rose,
    /// 中间态（恢复窗口/正在进行）：措辞已如实，只是档位不是"故障"
    Amber,
}

/// 单步完成态：`done` = 判据命中；`blocked_reason` = 卡住原因（Some = 卡住要人看，
/// None = 单纯待做）。**fail-closed**：读不到 = 未完成 + 如实说明，绝不伪装成完成。
/// `blocked_tone` = 该原因的语义档位（M4：只有恢复窗口那两处是 [`BlockedTone::Amber`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StepState {
    pub id: &'static str,
    pub done: bool,
    pub blocked_reason: Option<String>,
    pub blocked_tone: BlockedTone,
}

/// 逐步判据表（[`probe_steps_from`]）用的构造入口：把「结论是什么」与四个字段的
/// 样板分开——表里每一臂只剩下判据本身，读起来不用先越过五行结构体字面量。
impl StepState {
    /// 完成态由外部判据算出（detect / install / download 三步）
    fn done_if(id: &'static str, done: bool) -> Self {
        Self {
            id,
            done,
            blocked_reason: None,
            blocked_tone: BlockedTone::Rose,
        }
    }

    /// 判据命中（等价 [`Self::done_if`]`(id, true)`）
    fn done(id: &'static str) -> Self {
        Self::done_if(id, true)
    }

    /// 待做 / 待复评（等价 [`Self::done_if`]`(id, false)`）：**不是卡点**，
    /// blocked_reason 留空，前端据此区分「待做」与「卡住」
    fn pending(id: &'static str) -> Self {
        Self::done_if(id, false)
    }

    /// 卡住（真故障，rose）：reason 如实说明成因
    fn blocked(id: &'static str, reason: String) -> Self {
        Self {
            id,
            done: false,
            blocked_reason: Some(reason),
            blocked_tone: BlockedTone::Rose,
        }
    }

    /// 卡住（中间态，amber）：措辞已如实，只是档位不是「故障」——M4（2026-10-07 评审
    /// Minor）/ 线稿 82-84：恢复窗口是「既不是故障（rose）也不是正常完成」的中间态，
    /// 不得经通用行渲染涂成红色「卡住」（本表两处：shields_up / funnel 的恢复窗口臂）
    fn blocked_amber(id: &'static str, reason: String) -> Self {
        Self {
            id,
            done: false,
            blocked_reason: Some(reason),
            blocked_tone: BlockedTone::Amber,
        }
    }
}

/// 向导逐步完成态（生产入口）：`installed` 来自 find_cli 的真实 FS 探测。
/// 判据表（§C2）：
/// - `detect` / `install`：`find_cli().is_some()`
/// - `download`：已装（无需下载）或安装包已在落位路径且 sha256 对得上
/// - `sys_ext`（仅 macOS）：`Running` = 后端可用、直接完成；`Stopped` 只提示检查客户端，
///   不推断系统扩展未批准
/// - `login`：`BackendState == "Running"`
/// - `shields_up`：`get --json` 的 `shields-up == false`。
///   **W-A/I2 门**：`BackendState` 仍在初始化（`NoState`/`Starting`）时**不判该步**
///   （读不到偏好在初始化期是常态，照判会报成「读不到偏好」的假故障）——如实说
///   「恢复窗口内不写 shields-up，稍候自动复评」
/// - `funnel`：`funnel_active(funnel status --json)` 为真**且不是外来配置**
///   （外来占用时该步对 兔维斯 不算完成——如实 blocked，与 map_status 同口径）
/// - `verify`：§C3 可达性校验（Task 7）——判据 = 校验态 `Verified` **且通道在运行**
///   （快照停了地址已撤，校验步随之回退为待做）；`Failed` 如实 blocked（带 reason），
///   未验/在验 = 待做
/// - `autostart`：兔维斯 自身行为（启动时自动恢复，restore_tunnels_core 保证），恒 done
///
/// 单轮 CLI 读数（B-M11：**一次读数多处复用**）：`status --json` / `get --json` /
/// `funnel status --json` 各派生一次进程，由调用方按需复用（读不到 = None / Err，
/// 语义仍是 fail-closed：绝不把读不到当完成）
pub(super) struct CliReadings {
    status: Option<TsStatus>,
    shields: Option<bool>,
    funnel_json: Result<String, String>,
    system_extension_enabled: Option<bool>,
}

/// 取一轮读数（唯一的进程派生点，便于计数与替换：run_cli 的测试缝覆盖它）
pub(super) fn read_cli_readings() -> CliReadings {
    CliReadings {
        status: run_cli(&["status", "--json"])
            .ok()
            .and_then(|j| parse_status(&j).ok()),
        shields: run_cli(&["get", "--json"])
            .ok()
            .and_then(|j| serde_json::from_str::<serde_json::Value>(&j).ok())
            .and_then(|v| v.get("shields-up").and_then(|x| x.as_bool())),
        funnel_json: run_cli(&["funnel", "status", "--json"]),
        system_extension_enabled: system_extension_enabled(),
    }
}

/// 逐步探测内核（读数由调用方提供，B-M11）：`wizard_status_with` 与测试共用同一份读数——原实现各自读一次 `status --json`，单次 `remote_ts_probe` 合计
/// 4 次进程派生（status×2 + get + funnel status），现为 3 次
pub(super) fn probe_steps_from(port: u16, installed: bool, r: &CliReadings) -> Vec<StepState> {
    let status = &r.status;
    let shields = r.shields;
    let funnel_json = &r.funnel_json;

    wizard_steps(current_platform())
        .into_iter()
        .map(|ws| {
            let st = match ws.id {
                "detect" | "install" => StepState::done_if(ws.id, installed),
                "download" => StepState::done_if(
                    ws.id,
                    installed || installer_file_ok(current_platform(), &ts_installer_path()),
                ),
                "sys_ext" if r.system_extension_enabled == Some(true) => StepState::done(ws.id),
                "sys_ext" => match &status {
                    None => StepState::blocked(
                        ws.id,
                        "读不到 Tailscale 状态（CLI 未安装或调用失败）".into(),
                    ),
                    Some(s) if s.backend_state == "Running" => StepState::done(ws.id),
                    Some(s) if s.backend_state == "Stopped" => StepState::blocked(
                        ws.id,
                        "TSWIZ_SYS_EXT_NOT_RUNNING: Tailscale 后端未处于运行状态——\
                         请检查客户端；无法据此判断系统扩展是否已批准"
                            .into(),
                    ),
                    Some(_) => StepState::blocked(
                        ws.id,
                        "TSWIZ_SYS_EXT_NOT_RUNNING: Tailscale 后端未处于运行状态——\
                         请检查客户端；无法据此判断系统扩展是否已批准"
                            .into(),
                    ),
                },
                "login" => match &status {
                    None => StepState::blocked(
                        ws.id,
                        "读不到 Tailscale 状态（CLI 未安装或调用失败）".into(),
                    ),
                    Some(s) if s.backend_state == "Running" => StepState::done(ws.id),
                    // NeedsLogin = 等用户去登录（needs_human 步，动作文案已上墙），
                    // 不是「卡住」：blocked_reason 留空，前端据此区分待做与卡死
                    Some(_) => StepState::pending(ws.id),
                },
                // **W-A/I2**：后端仍在初始化时**不判** shields_up——`get --json` 在初始化
                // 期读不到（回读失败的同一根因），照旧判就会把"读不到"报成
                // 「读不到 Tailscale 偏好」的**假故障**，或显示成"待做"诱导用户去点
                // （点了也只会被推迟，见 run_step 的 Deferred 回执）。与下方 funnel 分支同形。
                "shields_up"
                    if status
                        .as_ref()
                        .is_some_and(|s| backend_initializing(&s.backend_state)) =>
                {
                    StepState::blocked_amber(
                        ws.id,
                        format!(
                            "Tailscale 后端正在重连（{}）——恢复窗口内不写 shields-up，\
                             稍候自动复评",
                            status
                                .as_ref()
                                .map(|s| s.backend_state.as_str())
                                .unwrap_or("")
                        ),
                    )
                }
                "shields_up" => match shields {
                    Some(false) => StepState::done(ws.id),
                    None => StepState::blocked(
                        ws.id,
                        "TSWIZ_PREFS_UNREADABLE: 读不到 Tailscale 偏好（shields-up）".into(),
                    ),
                    Some(true) => StepState::pending(ws.id),
                },
                // **W-A**：后端仍在初始化时 `funnel status --json` 会短暂为 `{}`——
                // 那个 `{}` **不是**「未开通」（真机 T+73s 配置逐字段自己回来了），
                // 故本步必须如实说"恢复窗口内不判定配置"，而不是显示成"待做"诱导用户
                // 去点开通（点了也只会被推迟，见 run_step 的 Deferred 回执）
                "funnel"
                    if status
                        .as_ref()
                        .is_some_and(|s| backend_initializing(&s.backend_state)) =>
                {
                    StepState::blocked_amber(
                        ws.id,
                        format!(
                            "Tailscale 后端正在重连（{}）——恢复窗口内不判定 Funnel 配置，\
                             稍候自动复评",
                            status
                                .as_ref()
                                .map(|s| s.backend_state.as_str())
                                .unwrap_or("")
                        ),
                    )
                }
                "funnel" => match &funnel_json {
                    Err(e) => StepState::blocked(
                        ws.id,
                        format!("TSWIZ_FUNNEL_STATUS_FAIL: 读不到 Funnel 状态: {e}"),
                    ),
                    Ok(j) if !funnel_active(j) => StepState::pending(ws.id),
                    Ok(j) if foreign_serve_config(j, port) => StepState::blocked(
                        ws.id,
                        "Funnel 已被其他 serve 配置占用（非 兔维斯）——为避免覆盖已停止开通;\
                         如确认可放弃，请在终端执行 `tailscale funnel reset` 后重试"
                            .into(),
                    ),
                    Ok(_) => StepState::done(ws.id),
                },
                "verify" => {
                    // §C3 实装（Task 7）：判据 = 校验态 Verified 且通道在运行；
                    // Failed 如实 blocked（带 reason），未验/在验 = 待做。
                    // 校验态/快照是全局（测试持 TEST_LOCK 串行）
                    let reach = reachability();
                    if matches!(reach, Reachability::Verified) && ts_snapshot().running {
                        StepState::done(ws.id)
                    } else if let Reachability::Failed { reason } = reach {
                        StepState::blocked(ws.id, reason)
                    } else {
                        StepState::pending(ws.id)
                    }
                }
                "autostart" => StepState::done(ws.id),
                other => {
                    StepState::blocked(other, format!("TSWIZ_STEP_UNKNOWN: 未知步骤: {other}"))
                }
            };
            st
        })
        .collect()
}

/// 向导载荷装配（remote_ts_probe 的内核，可测；JSON 形状即前后端契约）：
/// `{ platform, windowsVerified, windowsVerifiedFrom, windowsUnverifiedSteps,
///    writePathVerified, steps[], states[], authUrl, enabled, running, boardUrl,
///    configKey, waiting, reach }`
/// （reach = 可达性态（与 [`Reachability`] 的 serde 形状同源，共**六个**变体：
///   `unverified` / `verifying` / `verified` / `record_pending`{republish} /
///   `recovering` / `failed`{reason}——不写死数字，避免与枚举漂移），形状 =
///    "reason"（仅 failed）}`，见下方 Reachability 的 serde 形状）。
/// steps = 静态平台步骤表（needsHuman/humanActionKey 原样透出，i18n 键由前端翻译）；
/// states = probe_steps_from 现算；authUrl = 待登录授权链接（login 步「去登录」按钮数据源，
/// 兔维斯 不代登录）；running/boardUrl = Task 5 通道快照（向导头部展示用）；
/// enabled = 总开关与 Tailscale 通道开关均开启；waiting 是同一主机与端口的连续 DNS 等待信息。
/// **I-3**：Windows 验证位三项同源派生（[`windows_verification_for`]）——`windowsVerified`
/// 表示**整条**流程都实测过（2026-10-07 用户实机走完 兔维斯 向导全程后**当前为 true**，依据见
/// [`WINDOWS_VERIFIED_FROM`]），`windowsVerifiedFrom`/`windowsUnverifiedSteps`
/// 把「验到哪一步为止」说清，前端弱提示据此收窄到精确范围（清单为空时该提示不渲染，
/// 但**提示与派生机制保留**——将来又有未实测段落时把起点挪回去即可）。
pub(crate) fn wizard_status(port: u16) -> serde_json::Value {
    wizard_status_with(port, find_cli().is_some())
}

/// 注入版（测试缝）：installed 由调用方提供（find_cli 真实 FS 探测在测试不可控）
pub(super) fn wizard_status_with(port: u16, installed: bool) -> serde_json::Value {
    // B-M11：整份载荷只取**一轮读数**，逐步判据与 authUrl 共用它——原实现
    // probe_steps_from 读一次 status、本函数为 authUrl 又读一次（4 次进程派生）
    let readings = read_cli_readings();
    let states = probe_steps_from(port, installed, &readings);
    let auth_url = readings
        .status
        .as_ref()
        .map(|s| s.auth_url.clone())
        .unwrap_or_default();
    let snap = ts_snapshot();
    let dns_host = readings
        .status
        .as_ref()
        .and_then(|s| {
            let host = s.dns_name.trim().trim_end_matches('.');
            if !host.is_empty() {
                Some(host.to_string())
            } else {
                s.cert_domains.first().filter(|h| !h.is_empty()).cloned()
            }
        })
        .or_else(|| {
            snap.url
                .as_deref()
                .and_then(super::reach::host_from_board_url)
        });
    let host_for_key = dns_host.as_deref().unwrap_or("");
    let probe_config_key = config_key(host_for_key, port);
    observe_config(host_for_key, port);
    let waiting = dns_host
        .as_deref()
        .and_then(|host| waiting_status_for(host, port));
    let enabled = crate::database::dao::settings::get_setting(crate::remote::KEY_ENABLED)
        .as_deref()
        == Some("true")
        && crate::database::dao::settings::get_setting(crate::remote::KEY_CHAN_TAILSCALE)
            .as_deref()
            == Some("1");
    let platform = match current_platform() {
        Platform::Mac => "mac",
        Platform::Windows => "windows",
        Platform::Other => "other",
    };
    // A1：人工步骤数按平台如实登记并随载荷透出（required = 必需人工步，optional =
    // 可能不出现的人工步）。数字由步骤表派生（[`human_step_counts`]），不写死——
    // 表变了数字自己跟着变，不写死就不会漂移。
    let (human_required, human_optional) = human_step_counts(current_platform());
    // I-3：Windows 验证位三项（整条流程是否验过 / 实测覆盖起点 / 没跑过的步骤）
    let win = windows_verification_for(current_platform());
    serde_json::json!({
        "installerPath": if ts_asset_name(current_platform()).is_empty() { None } else { Some(ts_installer_path().display().to_string()) },
        "downloadUrl": if ts_asset_name(current_platform()).is_empty() { None } else { Some(ts_download_url(current_platform())) },
        "installerReady": installer_file_ok(current_platform(), &ts_installer_path()),
        "platform": platform,
        "windowsVerified": win["windowsVerified"],
        "windowsVerifiedFrom": win["windowsVerifiedFrom"],
        "windowsUnverifiedSteps": win["windowsUnverifiedSteps"],
        // B-M5：写路径（funnel --bg / reset / 批准链接抓取）实机验证位——逐平台如实
        "writePathVerified": write_path_verified(current_platform()),
        "humanSteps": { "required": human_required, "optional": human_optional },
        "steps": wizard_steps(current_platform())
            .iter()
            .map(|s| serde_json::json!({
                "id": s.id,
                "needsHuman": s.needs_human,
                // A1：人工步分「必需 / 可能不出现」——前端据此把批准步渲染成
                // 「若弹出链接才需点」，而不是"等你点批准"的必经步骤
                "humanOptional": s.human_optional,
                "humanActionKey": s.human_action_key,
            }))
            .collect::<Vec<_>>(),
        "states": states
            .iter()
            .map(|s| serde_json::json!({
                "id": s.id,
                "done": s.done,
                "blockedReason": s.blocked_reason,
                // M4（2026-10-07 评审 Minor）：卡点的**语义档位**——`rose` = 真故障（前端红
                // 「卡住：」），`amber` = 中间态（恢复窗口「正在进行、无需操作」，不得涂红）。
                // 档位由后端判据给出（前端不按文案猜），线稿 82-84 即该契约。
                "blockedTone": match s.blocked_tone {
                    BlockedTone::Amber => "amber",
                    BlockedTone::Rose => "rose",
                },
            }))
            .collect::<Vec<_>>(),
        "authUrl": auth_url,
        "running": snap.running,
        "enabled": enabled,
        "configKey": probe_config_key,
        "waiting": waiting,
        "boardUrl": snap.url,
        // §C3（Task 7）：校验态随载荷透出——前端据此门控头部地址展示（Verified 才
        // 显示地址），raw boardUrl 保持 Task 5 语义不变（门控是展示层职责，见前端）
        "reach": reachability(),
    })
}

/// 撤销步内核（可测：停止内核 / **通道开关位写入** / 审计出口 / 事件出口全部注入——
/// C-1、B1 补账与托盘地址回落）。
///
/// **为什么撤销必须落通道开关位（C-1，Critical）**：`remote_ts_run_step("disable")` 是撤销
/// tailscale 的**第二条入口**（前端确认框路径走它，不经 `remote_toggle_channel`——后者才是
/// 「开关即写 KV」的那条）。只撤配置不落位的三连后果：
/// ① `enabled` 仍是 "1" ⇒ 卡面开关保持 ON，与既成事实背离；
/// ② 成因行落到 `offCause='listening'` ⇒ 对刚被用户撤销的通道谎报「服务没在监听」；
/// ③ **下次启动 / 总开关重新打开 → `restore_enabled_tunnels` 逐通道读 KV → `start_channel`
///    → `funnel --bg`** ⇒ 用户刚撤销的公网暴露被自动重新打开。
///
/// **只在撤销成功后落位**（失败不撒谎）：守卫拒绝 / CLI 失败时开关位保持原样，卡面照旧
/// 显示 ON——那才是事实（配置还在公网），用户也能重试；落了位反而变成「开关说已关、
/// 实际还开着，且重启不再恢复」。
///
/// **写口纪律不变**：真正的 KV 写入仍只有 `remote::write_chan_flag` 一处，本内核只收一个
/// 注入的收口闭包（生产 = `remote::clear_tailscale_chan_flag`）。
///
/// **审计同一条「失败不撒谎」纪律（B1 补账）**：B1 把撤销整体挪到向导步时丢了审计——开关
/// 命令那条路（`remote_toggle_channel`）有 `channel_toggled`，向导步这条路没有，于是事后
/// 只看到一串「开」记录、「这个 Funnel 什么时候关的 / 关没关」无从回答。审计出口同样注入
/// （生产 = `remote::events::audit`），**只在 `stop()` 成功返回后发**：守卫拒绝 / CLI 失败
/// 时上面 `?` 已提前返回，绝不发「已撤销」的假审计（假审计会让人误以为公网暴露已撤下）。
///
/// **事件出口（托盘地址回落真值）**：撤销成功还要广播 `remote-changed`，托盘菜单才会重建、
/// 「复制远程地址」项才从**已作废的固定地址**回落到 cloudflared/局域网真值。出口同样注入
/// （生产 = `remote::events::emit_ui`），**同样只在成功后发**：失败时固定地址仍然可用，
/// 广播会让托盘把一条有效地址换成次优地址（同样是"呈现与现实不符"）。
pub(super) fn disable_step_with(
    port: u16,
    force: bool,
    stop: impl FnOnce(Option<u16>, bool) -> Result<ServeResetReport, String>,
    clear_chan_flag: impl FnOnce(),
    audit: impl FnOnce(&str, &str),
    emit: impl FnOnce(&str, serde_json::Value),
) -> Result<serde_json::Value, String> {
    let r = stop(Some(port), force)?;
    clear_chan_flag();
    // 动作名 `channel_toggled` 与开通路径（`remote/mod.rs` 的 toggle_channel）**刻意同源**：
    // 一把 `channel=tailscale` 的 grep 就能拉出完整的开/关序列，正是本条要修的「不对称」。
    // 明细的两个事实字段 = forced（是否走了强制撤销，即跳过了归属守卫）与 cleared_others
    // （本次 `funnel reset` 连带清掉的非 兔维斯 条目数）——「强制撤销」与「顺带清了用户配置」
    // 正是最需要留痕的两种情况；条目本身随回执 `clearedEntries` 交回前端逐条呈现。
    // 既有的 `tailscale_serve_reset_cleared_extras`（status.rs，仅 cleared_others>0 时发）
    // 记的是**副作用**，本条记的是**撤销动作本身**，两者并存、各司其职。
    audit(
        "channel_toggled",
        &format!(
            "channel=tailscale on=false forced={} cleared_others={}",
            r.forced, r.cleared_others
        ),
    );
    // 撤销成功 → 广播 `remote-changed`（与开通路径 `remote_toggle_channel` 同一条惯例：
    // 事件名 + `{channel, on}` 载荷）。**为什么必须发**：`remote-changed` 全仓只有 1 个
    // 消费方（`home.tsx` → `initTrayMenu()` → `refresh_tray`），而托盘的「复制远程地址」项
    // 取值依赖 tailscale 快照（`remote::tray_display().1` → `tray_url_from`）——上面
    // `stop()` 已复位快照 + 作废校验态（真值回落 cloudflared/局域网地址），但托盘只在挂载
    // 与事件时重建（`system_tray.rs` 无定时器；3 秒轮询是设置卡的，对托盘不成立）：不发事件，
    // 托盘会一直显示那个**已经打不开**的固定地址，正撞本模块「绝不把打不开的地址呈现成可用」
    // 的纪律。**只在成功后发**（与本函数落开关位/发审计同一条「失败不撒谎」纪律）：失败时
    // 固定地址仍然可用，广播反而会把一条有效地址换成次优地址。
    emit(
        "remote-changed",
        serde_json::json!({ "channel": "tailscale", "on": false }),
    );
    Ok(serde_json::json!({
        "ok": true,
        "clearedExtraServeEntries": r.cleared_others,
        // M-1：条目随回执交出去——卡面「N 条」与逐条列表**同源**（都出自这次撤销的条目表），
        // 不再「确认框列的是预览条目、toast 报的是撤销时条数」两处对不上
        "clearedEntries": r
            .cleared_entries
            .iter()
            .map(|e| serde_json::json!({ "ours": e.ours, "label": e.label }))
            .collect::<Vec<_>>(),
        "forced": r.forced,
    }))
}

/// 向导单入口（Task 6 的步骤表逐步走这一个口，**不逐个暴露底层原语**——
/// enable_funnel / disable_funnel / disable_shields_up / run_installer 均为模块私有
/// 或经本口收口）。step 值域：
/// - 静态探针：`status`（安装/登录/域名/Funnel 概览，Task 5 既有）
/// - 向导步（id 与 [`wizard_steps`] 一致）：`detect` / `download` / `install` /
///   `sys_ext` / `login` / `shields_up` / `funnel` / `verify` / `autostart`
/// - 独立修复：`repair`（仅用户明确触发；通道意图开启时可执行原自愈流程）
/// - 通道动作：`enable`（= `funnel` 的同义旧名）| `disable`（完整停止语义：走
///   stop_inner 同一守卫并同步收口 DESIRED——修复轮 1 起与 stop_channel/stop_all
///   单点共用）
///
/// 需要人的步骤只做「触发+回报」，**不代点**（登录只递 AuthURL、安装只触发系统
/// 授权框、批准链接只递出去）。`verify` 是 §C3 强制只读可达性校验；只有 `repair`
/// 在通道意图开启时可触发 reset + 重开，结果写校验态全局并随回执透出。
pub(crate) fn run_step(step: &str, port: u16) -> Result<serde_json::Value, String> {
    match step {
        "status" => {
            let installed = find_cli().is_some();
            let status = run_cli(&["status", "--json"]).and_then(|j| parse_status(&j));
            let funnel = run_cli(&["funnel", "status", "--json"]).map(|j| funnel_active(&j));
            match status {
                Err(e) => Ok(serde_json::json!({ "installed": installed, "error": e })),
                Ok(s) => Ok(serde_json::json!({
                    "installed": installed,
                    "backendState": s.backend_state,
                    // AuthURL 非空 = 待登录，向导做成「去登录」按钮（兔维斯 不自己登录）
                    "authUrl": s.auth_url,
                    "dnsName": s.dns_name.trim_end_matches('.'),
                    "certDomains": s.cert_domains,
                    "funnelActive": funnel.unwrap_or(false),
                })),
            }
        }
        // detect：只读探针（无动作）；与步骤判据表的 detect 同源（find_cli）
        "detect" => Ok(serde_json::json!({ "ok": true, "installed": find_cli().is_some() })),
        // download：下载安装包到落位路径 + sha256 校验（复用 tunnel 下载器与校验单点，
        // 不新写）；幂等——已装好或包已就位（sha 对得上）直接短路
        "download" => {
            let p = current_platform();
            if p == Platform::Other {
                return Err("TSWIZ_PLATFORM_UNSUPPORTED: 当前平台暂不支持一键配置".into());
            }
            if find_cli().is_some() {
                return Ok(serde_json::json!({ "ok": true, "skipped": "已安装，无需下载" }));
            }
            let dest = ts_installer_path();
            if installer_file_ok(p, &dest) {
                return Ok(serde_json::json!({
                    "ok": true, "skipped": "安装包已就位",
                    "path": dest.display().to_string(),
                }));
            }
            let expected = ts_expected_sha256(ts_asset_name(p))
                .map_err(|e| format!("TSWIZ_CHECKSUM_MISSING: 安装包无校验值，拒绝使用: {e}"))?;
            stage_installer_with(&dest, &legacy_installer_path(), expected, |part| {
                crate::remote::tunnel::download_url_to(&ts_download_url(p), part)
            })?;
            Ok(serde_json::json!({
                "ok": true,
                "path": dest.display().to_string(),
            }))
        }
        // install：触发系统安装器（授权框由系统弹——不静默提权）；幂等需安装包
        // 先就位**且 sha256 对得上**（修复轮 1：门禁见 install_step_with）
        "install" => {
            let path = ts_installer_path();
            install_step_with(current_platform(), &path, run_installer)
        }
        // sys_ext（macOS）：系统扩展批准只能人去点——本步只回报后端状态供向导核对
        "sys_ext" => {
            let s = run_cli(&["status", "--json"]).and_then(|j| parse_status(&j))?;
            Ok(serde_json::json!({ "ok": true, "backendState": s.backend_state }))
        }
        // login：兔维斯 不代登录——只把授权链接递出去（用户在浏览器完成）。**⑤（2026-10-07
        // 用户实测）**：链接**不是"等它自己出现"**——新装机器上 `AuthURL` 是空串（要发起
        // 一次交互式登录才由尾网生成），故本步**主动让尾网生成**（后台 `tailscale login`，
        // 只 spawn 不等待）+ 有界轮询取回。长阻塞注意：本臂最长 LOGIN_LINK_POLL_WINDOW
        //（9.5s），命令经 spawn_blocking 调（remote_ts_run_step），不占 IPC/主线程；
        // 发起本身是后台的（[`spawn_login_attempt`]）——**绝不**同步等 tailscale login。
        // argv 白名单（I2）：`login --timeout 15s`（零凭据旗标、零偏好旗标）；`--timeout`
        // 把"默认 0s 一直等"变成有界（I1）。
        //
        // **为什么本臂不需要 W-A 初始化门（I4，2026-10-08 架构评审）**：规格的「写恰好五处
        // + 初始化中一律 Deferred」点名的是会**改 tailscaled 配置/偏好**的四条写命令
        //（`set --shields-up=false` / `funnel --bg` / `funnel reset` / 撤销）；`login` 是
        // 第五个会碰 tailscaled 状态的动作（发起一次交互式登录），但它**不写任何设置**，
        // 而且**上游 CLI 自带 Running 门**——`--timeout` 的语义就是「等待 tailscaled 进入
        // Running 状态的最长时间」（默认 0s 一直等），即恢复窗口内这条命令只会**等**、
        // 不会在未就绪的后端上落任何配置 ⇒ 等价于"Deferred 下沉到 CLI 自己"。
        // 故本臂**有意不过门**（也不过 W-A 的 Deferred 回执：本步产物是一个链接，不是配置）。
        // ⚠️ **残余与待裁决**：恢复窗口内 `probe_steps_from` 的 login 臂会呈现成"待做"
        //（判据是 `BackendState == "Running"`），前端据此渲染「获取登录链接」按钮——对一台
        // **其实已登录、只是后端在重连**的机器，这是一个"假待做"。危害有界（点下去只是
        // 一次有界等待、不改配置），但要不要给 login 也补一道 W-A 门（或把该窗口内的 login
        // 步呈现成 amber"恢复中"）属**设计裁决**，已上报控制方（本轮只登记，不擅自改语义）。
        "login" => login_step_with(
            || run_cli(&["status", "--json"]).and_then(|j| parse_status(&j)),
            spawn_login_attempt,
            std::thread::sleep,
            finish_login_attempt,
        ),
        // shields_up：关「阻止传入连接」（写入后回读确认，见 disable_shields_up）。
        // **W-A 同一道后端门（I2，2026-10-07 评审）**：规格要求 7 明文「初始化中一律
        // Deferred——不写 shields-up」；本步此前没过门，恢复窗口里向导显示「待做 + 执行」，
        // 用户一点就在窗口内写入，而 `set --shields-up=false` 的回读在初始化期必然失败
        // ⇒ UI 收到「无法确认已关闭」的**假故障**（与 enable_funnel ⓪ 门的真机现象同源）。
        // 回执与 funnel/enable 步**同形**（deferred + backendState + 成因说明）。
        "shields_up" => {
            // 读不到状态 = Err（fail-closed：绝不拿"读不到"当"可以写"）
            let st = run_cli(&["status", "--json"]).and_then(|j| parse_status(&j))?;
            if backend_initializing(&st.backend_state) {
                // 本回执的 `deferred` / `backendState` / `note`：**前端只声明不消费**
                // （2026-10-07 核实）。语义 =「这一步**没做**（后端在恢复窗口内拒绝写入），
                // **不是失败**」；向导动作后必重探，probe_steps_from 的 shields_up 分支
                // 把同一事实呈现成 amber「后端正在重连…」（既不是"待做"也不是"卡住"）——
                // 故前端不消费的**判断与理由**见 src/lib/api/remote.ts 的
                // TsStepResult.deferred 注释；`note` 的消费方核实结论见 disable_preview
                // 分支的同名说明。
                return Ok(serde_json::json!({
                    "ok": true,
                    "deferred": true,
                    "backendState": st.backend_state,
                    "note": format!(
                        "Tailscale 后端正在重连（{}）——开机恢复窗口内不写 shields-up\
                         （此刻写入的回读必然失败，会被报成假故障）；就绪后可重试",
                        st.backend_state
                    ),
                }));
            }
            disable_shields_up().map(|_| serde_json::json!({ "ok": true }))
        }
        // funnel / enable（修复轮 2 指针③）：走 start_channel 同一生命周期——
        // 守卫 → 开通 → 置 DESIRED → 立即现算 → 起轮询；批准链接（若有）随回执递出。
        // **W-A**：后端还在初始化时回执必须如实说"推迟"（`deferred` + `backendState` +
        // 成因说明），**不得**报成 `ok:true` 让用户以为已经开通了——期望态已记，
        // 后端就绪后由轮询自动补开通（见 status::deferred_open_due）
        "funnel" | "enable" => {
            let outcome = start_channel_inner(port)?;
            // 用户明确开通成功（含已登记的延迟恢复）后，持久化通道意图；失败不落位。
            crate::remote::write_chan_flag(crate::remote::ChannelKind::Tailscale, true);
            crate::remote::events::audit("channel_toggled", "channel=tailscale on=true");
            crate::remote::events::emit_ui(
                "remote-changed",
                serde_json::json!({ "channel": "tailscale", "on": true }),
            );
            Ok(match outcome {
                FunnelOutcome::Opened(approval) => {
                    serde_json::json!({ "ok": true, "approvalUrl": approval })
                }
                // 同 shields_up：`deferred` 语义 =「这一步没做，**不是失败**」——此处**期望态
                // 已记**（DESIRED），后端就绪后由 `status::deferred_open_due` 自动复评/补开通。
                // 前端只声明不消费（理由同 shields_up 处注释：动作后重探的载荷已如实呈现，
                // 不引入第二份回执派生的状态源）。
                FunnelOutcome::Deferred { backend_state } => serde_json::json!({
                    "ok": true,
                    "deferred": true,
                    "backendState": backend_state,
                    "note": format!(
                        "Tailscale 后端尚未就绪（{backend_state}）——开机恢复窗口内不判定 Funnel \
                         配置、不重开；通道已记为期望开启，后端就绪后会自动复评/补开通"
                    ),
                }),
            })
        }
        // verify（§C3 实装）：只读执行外部解析 + 钉 IP 探测；失败不改 Funnel 配置。
        // 结果写校验态全局（载荷三门/向导 verify 判据随之联动），回执带最终状态。
        // **长阻塞注意**：公网探测最长包含 DoH 与 HTTP 超时——remote_ts_run_step
        // 命令经 spawn_blocking 调本函数，绝不占 IPC/主线程
        "verify" => {
            let host = super::reach::current_dns_host();
            let token = begin_verification(host.as_deref().unwrap_or(""), port);
            let r = if let Some(host) = host.as_deref() {
                do_verify(host)
            } else {
                Reachability::Failed {
                    reason: "TSWIZ_VERIFY_HOST_MISSING: 读不到机器域名，无法执行公网校验或修复（先完成登录/开通）".into(),
                }
            };
            let same_target = super::reach::current_dns_host() == host;
            let applied = same_target && apply_reachability(&token, r.clone());
            if !same_target {
                discard_verification(&token);
            }
            let final_reach = if applied { r } else { reachability() };
            Ok(serde_json::json!({
                "ok": true,
                "done": matches!(final_reach, Reachability::Verified),
                "stale": !applied,
                "reach": serde_json::to_value(&final_reach)
                    .unwrap_or_else(|_| serde_json::json!({ "state": "unverified" })),
            }))
        }
        // 独立的显式修复动作：只有用户明确触发 repair 才调用原 reset + 重开 + 重验。
        "repair" => {
            invalidate_verification_lifecycle();
            let host = super::reach::current_dns_host();
            let token = begin_verification(host.as_deref().unwrap_or(""), port);
            let r = if host.is_none() {
                Reachability::Failed {
                    reason: "TSWIZ_VERIFY_HOST_MISSING: 读不到机器域名，无法执行公网校验或修复（先完成登录/开通）".into(),
                }
            } else {
                verify_with_heal(port, &token)
            };
            let same_target = super::reach::current_dns_host() == host;
            let applied = same_target && apply_reachability(&token, r.clone());
            if !same_target {
                discard_verification(&token);
            }
            let final_reach = if applied { r } else { reachability() };
            Ok(serde_json::json!({
                "ok": true,
                "done": matches!(final_reach, Reachability::Verified),
                "stale": !applied,
                "reach": serde_json::to_value(&final_reach)
                    .unwrap_or_else(|_| serde_json::json!({ "state": "unverified" })),
            }))
        }
        // autostart：兔维斯 自身行为（启动时 restore_tunnels_core 自动恢复），无需动作
        "autostart" => Ok(serde_json::json!({ "ok": true, "done": true })),
        // disable_preview（B1）：**只读**预览「这次撤销会连带清掉哪些条目」——
        // 撤销前确认框的唯一数据源。为什么不复用 disable 的回执：`funnel reset` 清的是
        // **整份** serve 配置，Mixed（我们那路 + 用户自建）形态下普通撤销**照常放行**，
        // 事后才知道清掉了什么就已经晚了——知情必须在动手之前。
        // 本步零写操作、零状态变更（只发 `status` 与 `funnel status --json` 两条读命令）。
        // 载荷：{ ok, foreign（普通撤销是否会被守卫拒绝）, wouldClear,
        //         entries: [{ ours, label }], unreadable?, note? }
        //
        // **W-A 第三处判据点（2026-10-07 真机重启实测）**：后端仍在初始化时
        // `funnel status --json` 会短暂为 `{}`——照旧读就把"读不到"报成"什么都没有"
        // （wouldClear=0、无 foreign），用户点下的同意建立在假信息上。此时如实回
        // `unreadable=true` + 成因说明，**不拒绝撤销**（"公开暴露撤不掉"是更糟的失败
        // 模式，与 Mixed 放行的权衡同源）；前端照 foreign 处理（弹确认框 + 说明读不到）。
        // `status` 读不到 = Err（fail-closed，与原口径一致，不因本门放宽）。
        "disable_preview" => {
            let st = run_cli(&["status", "--json"])
                .and_then(|j| parse_status(&j))
                .map_err(|e| {
                    format!("TSWIZ_PREVIEW_RESET_UNREADABLE: 读不到 Tailscale 状态，拒绝预览撤销（无法确认会清掉什么）: {e}")
                })?;
            if backend_initializing(&st.backend_state) {
                // **本 `note` 的消费方（2026-10-07 核实）**：前端**不读**本字段——撤销
                // 确认框的成因说明走等效的**静态 i18n 键**
                // `settings.remote.tsDisableDescUnreadable`（RemoteSection 按 `unreadable`
                // 分支渲染；后端串是中文硬编码，直接上屏会绕过 i18n）。本字段当前只被本
                // 模块的测试逐字断言（`tailscale/mod.rs`
                // `disable_preview_reports_unreadable_while_backend_is_initializing`）。
                // 保留 = 回执形态如实（日志 / 移动端将来可用）；**要渲染先 i18n 化**。
                return Ok(serde_json::json!({
                    "ok": true,
                    "unreadable": true,
                    "foreign": false,
                    "wouldClear": 0,
                    "entries": [],
                    "note": format!(
                        "Tailscale 后端正在重连（{}）——此刻读不到现有 serve 配置形态，\
                         无法列出会被一并清除的条目；这次撤销仍会清除整份 serve 配置，\
                         稍候再试可先看清清单",
                        st.backend_state
                    ),
                }));
            }
            let j = run_cli(&["funnel", "status", "--json"])
                .map_err(|e| funnel_status_unreadable("预览撤销", &e))?;
            let ownership = serve_ownership(&j, port);
            let entries = serve_entries(&j, port);
            Ok(serde_json::json!({
                "ok": true,
                "foreign": ownership.is_foreign(),
                "wouldClear": ownership.cleared_others(),
                "entries": entries
                    .iter()
                    .map(|e| serde_json::json!({ "ours": e.ours, "label": e.label }))
                    .collect::<Vec<_>>(),
            }))
        }
        // disable：完整停止语义（走 stop_inner 同一守卫并同步收口 DESIRED）
        // disable_force（B-M4 出口）：端口被改/配置完全外来导致普通撤销被拒时的唯一出路。
        // **step 名本身即确认语义**——调用方（前端确认框/用户手动 IPC）必须先取得用户明确
        // 同意；本步只跳过归属守卫，仍如实回报被一并清除的条目数（不静默）。
        // B1 接线：前端在撤销前先调 disable_preview，只要 `foreign` 或 `wouldClear > 0`
        // 就弹确认框逐条列出，用户确认后才走本步——**Mixed 形态也必须先问**
        //（普通 disable 会照常放行并把用户条目一起清掉，事后再报就晚了）
        "disable" | "disable_force" => {
            let force = step == "disable_force";
            // C-1：撤销成功后落通道开关位（内核与理由见 [`disable_step_with`]）——
            // 写口仍是 remote::write_chan_flag 单点，这里只透传生产收口
            disable_step_with(
                port,
                force,
                stop_inner,
                crate::remote::clear_tailscale_chan_flag,
                crate::remote::events::audit,
                crate::remote::events::emit_ui,
            )
        }
        other => Err(format!("未知向导步骤: {other}")),
    }
}
