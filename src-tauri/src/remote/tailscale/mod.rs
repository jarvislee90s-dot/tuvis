//! Tailscale 通道（§C1「固定网址 · 免域名」）：驱动**用户本机已安装的** Tailscale CLI
//! 开通 Funnel，把 `https://<机器名>.<尾网名>.ts.net/m` 作为永久看板地址。
//!
//! **与 tunnel.rs 的架构差异（勿照抄）**：cloudflared 是前台子进程（spawn + kill_on_drop +
//! wait 守护）；`tailscale funnel --bg` **立即返回**，配置由常驻 tailscaled 服务持有，
//! **没有子进程可守**。故本模块的守护模型是**周期轮询**，运行态由 CLI 状态现算。
//!
//! **不持有任何用户凭据**：登录由用户在浏览器完成，本模块只把 `AuthURL` 递出去。
//!
//! **模块拆分（评审「问题 14」，纯移动零行为变化）**：四种关注点分开，风险画像不同：
//! - [`status`]：通道本体与运行态——CLI 探测/调用、快照、期望态与轮询守护、serve 守卫、
//!   起停生命周期（**运行态**，常驻线程与 CLI 派生都在这里）；
//! - [`wizard`]：首次配置引导（§C2）——平台步骤表、**安装包下载与特权安装触发**
//!   （**供应链**，与运行态分开）、逐步探测与 `run_step` 单入口；
//! - [`reach`]：强制可达性校验与自愈（§C3）——DoH 解析、钉 IP 探测、兔维斯 特征判据、自愈；
//! - 本文件：模块文档、**跨子模块再导出**（crate 内消费方只用 `tailscale::X` 这一条路）
//!   与**测试缝/测试模块**（零网络零进程纪律的集中登记处）。
//!
//! **可见性收敛**：子模块对彼此与 mod.rs 用 `pub(super)`，crate 内消费方（`remote::mod`
//! 等）用 `pub(crate)` 再导出——原先的 `pub` 一律收窄，`tailscale` 之外无人能触达内部件。

mod reach;
mod status;
mod wizard;

// crate 内消费方（remote::mod / server.rs / 测试）的唯一入口：**只有这些名字可见**
pub(crate) use reach::{
    reachability, record_pending_hint, Reachability, RECORD_PENDING_HINT, RECOVERING_HINT,
};
// 升级检查的 DoH 钉 IP 兜底（commands/updater.rs）：直连/代理失败后经生产探针解析
// api.github.com 真实地址——2026-10-08 实测本机 DNS 污染（假证书），钉 IP 直连 200
pub(crate) use reach::real_probe;
#[allow(unused_imports)]
// remote/named and remote/mod consume these in the parallel wizard work.
pub(crate) use reach::{config_key_snapshot, observe_port, probe_public_http, waiting_status};
// 重开档常量：生产路径经 `record_pending_hint(republish)` 取文案（不直接引用常量），
// crate 内只有**测试**要按名断言两档不同 ⇒ 仅测试构建再导出（与 set_reachability 同纪律）
#[cfg(test)]
pub(crate) use reach::RECORD_REPUBLISH_HINT;
// 生产校验经代际提交；测试需要直接注入夹具，仅测试构建再导出。
#[cfg(test)]
pub(crate) use reach::set_reachability;
pub(crate) use status::{set_ts_snapshot, start_channel, stop_all, stop_channel, ts_snapshot};
// I1（2026-10-08 架构评审）：应用退出钩子要能把 `tailscale login` 的等待者一起收掉
// （kill + wait，不留孤儿进程）——`lib.rs` 的 RunEvent::Exit 是唯一的生产消费方。
pub(crate) use wizard::cancel_login_attempt;
pub(crate) use wizard::{run_step, wizard_status};

// 测试模块需要看见三个子模块的内部件（`use super::*` 从 tests 取用）；仅测试构建存在。
// **glob 导入不引入任何新的可见性**：只有子模块里 `pub(super)` 以上的件会被带进来
#[cfg(test)]
use reach::*;
#[cfg(test)]
use status::*;
#[cfg(test)]
use wizard::*;

// ============================================================
// 测试缝（**全局单槽 + TEST_LOCK 串行**，零网络零进程纪律的集中登记处）
// ------------------------------------------------------------
// 这些件被三个子模块内的生产函数在 `cfg(test)` 下读取（`super::XXX_OVERRIDE`）：
// run_cli 的 CLI 替身、两条网络探测的替身。放这里而不是各自的子模块：
// ① 纪律只有一份（谁要新增测试缝，先看这一节）；② 子模块保持"只有生产逻辑"。
// **用后必须还原**（见 tests::teardown_globals），且必须持 TEST_LOCK 串行
// ============================================================

/// （修复轮 1）测试缝：run_cli 的行为替换（仅测试触碰）。Some(f) 时 run_cli 全部转发
/// 给 f——「守卫专测」靠它注入 funnel status 输出、记录 reset 调用，零网络零进程。
/// **必须持 `TEST_LOCK` 串行**（全局单槽），用后 `set_run_cli_override(None)` 还原
/// （clippy type_complexity 收敛别名，对齐 [`super::server::ViaHostsSource`] 先例）
#[cfg(test)]
type RunCliFake = dyn Fn(&[&str]) -> Result<String, String> + Send;

#[cfg(test)]
static RUN_CLI_OVERRIDE: once_cell::sync::Lazy<std::sync::Mutex<Option<Box<RunCliFake>>>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(None));

/// tailscale 全局态（RUN_CLI_OVERRIDE / DESIRED / TS_SNAPSHOT）的测试串行锁
/// （与 tunnel::test_sync::TUNNEL_GLOBALS / queue::LOOP_HANDLE_TEST_LOCK 同纪律）。
/// 获取用 `test_lock()`（毒化容忍）：一条测试断言失败不得让同锁兄弟测试连环毒崩
#[cfg(test)]
static TEST_LOCK: once_cell::sync::Lazy<std::sync::Mutex<()>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(()));

#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// 换入 run_cli 行为替身（None = 还原真身）
#[cfg(test)]
fn set_run_cli_override(f: Option<Box<RunCliFake>>) {
    *RUN_CLI_OVERRIDE.lock().unwrap() = f;
}

#[cfg(test)]
mod tests {
    use super::*;
    // 发现链测试要构造候选路径（`super::*` 只带进 tailscale 模块自己的名字）
    use std::path::PathBuf;

    /// 实测样本（2026-10-06，macOS 1.102.4 真机 `status --json` 截取关键字段）。
    /// 注意 `Self.DNSName` **带结尾点**——这是解析必须处理的真实形态。
    const STATUS_RUNNING: &str = r#"{
        "BackendState": "Running",
        "AuthURL": "",
        "Self": { "DNSName": "jarvismac-mini.example-tailnet.ts.net." },
        "CertDomains": ["jarvismac-mini.example-tailnet.ts.net"]
    }"#;

    /// 待登录形态：AuthURL 带链接（兔维斯 要把它做成按钮）
    const STATUS_NEEDS_LOGIN: &str = r#"{
        "BackendState": "NeedsLogin",
        "AuthURL": "https://login.tailscale.com/a/abc123",
        "Self": { "DNSName": "" },
        "CertDomains": []
    }"#;

    /// **⑤ 新装未发起过登录的形态**（2026-10-07 诊断所得，用户实测场景）：
    /// `NeedsLogin` **但 AuthURL 为空**——授权链接由尾网在**发起一次交互式登录**时才生成
    /// （`tailscale login` / GUI 的「Log in」按钮），兔维斯 此前从不发起 ⇒ 链接永远不出现，
    /// 而前端只在 `probe.authUrl` 非空时才渲染「去登录」，于是登录步成为**死胡同**：
    /// 行里有"需要你操作：去浏览器登录"的文案，却没有任何可点的东西。
    const STATUS_NEEDS_LOGIN_NO_URL: &str = r#"{
        "BackendState": "NeedsLogin",
        "AuthURL": "",
        "Self": { "DNSName": "" },
        "CertDomains": []
    }"#;

    #[test]
    fn parse_status_reads_backend_state_and_auth_url() {
        let s = parse_status(STATUS_RUNNING).expect("运行态应可解析");
        assert_eq!(s.backend_state, "Running");
        assert!(s.auth_url.is_empty(), "已登录时 AuthURL 应为空串");
        assert_eq!(
            s.dns_name, "jarvismac-mini.example-tailnet.ts.net.",
            "实测 DNSName 带结尾点——解析必须原样保留（归一是使用方的职责）"
        );
        assert_eq!(
            s.cert_domains,
            vec!["jarvismac-mini.example-tailnet.ts.net".to_string()],
            "CertDomains 是不带点的同值域名"
        );

        let s = parse_status(STATUS_NEEDS_LOGIN).expect("待登录态应可解析");
        assert_eq!(s.backend_state, "NeedsLogin");
        assert_eq!(
            s.auth_url, "https://login.tailscale.com/a/abc123",
            "授权链接必须能取到（做成按钮用）"
        );
    }

    #[test]
    fn parse_status_is_total_on_garbage() {
        // 不 panic、不猜：坏 JSON / 缺字段一律 Err（引导要如实报"读不到状态"）
        assert!(parse_status("not json").is_err());
        assert!(
            parse_status("{}").is_err(),
            "缺 BackendState 应 Err 而非默认 Running"
        );
    }

    #[test]
    fn funnel_active_only_when_config_non_empty() {
        assert!(!funnel_active("{}"), "空对象 = 未伺服（实测形态）");
        assert!(!funnel_active(""));
        assert!(
            !funnel_active("not json"),
            "解析不了按未激活处理（fail-closed）"
        );
        assert!(funnel_active(
            r#"{"TCP":{"443":{"HTTPS":true}},"Web":{"x.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:9420"}}}}}"#
        ));
    }

    #[test]
    fn board_url_from_dns_name_strips_trailing_dot_and_appends_m() {
        // 契约（与既有通道一致）：快照 url 恒为含 /m 的看板完整地址
        assert_eq!(
            board_url_from_dns_name("jarvismac-mini.example-tailnet.ts.net."),
            "https://jarvismac-mini.example-tailnet.ts.net/m"
        );
        assert_eq!(
            board_url_from_dns_name("jarvismac-mini.example-tailnet.ts.net"),
            "https://jarvismac-mini.example-tailnet.ts.net/m",
            "不带点的形态也要能处理（CertDomains 就不带点）"
        );
    }

    #[test]
    fn board_url_from_dns_name_is_empty_safe() {
        assert_eq!(
            board_url_from_dns_name(""),
            "",
            "无名字时给空串，不要造出 https:///m"
        );
        assert_eq!(board_url_from_dns_name("."), "");
    }

    /// 「不覆盖用户既有 serve 配置」守卫：空表（未开通）与 兔维斯 自己那份（指向本机
    /// 兔维斯 端口）放行；指向别的端口的活配置 = 外来，拒绝开通——`funnel reset` 会
    /// 清掉整份 serve 配置，静默清掉用户自建配置是不可接受的副作用
    #[test]
    fn foreign_serve_config_flags_only_foreign_active_configs() {
        const OURS: &str =
            r#"{"Web":{"x.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:9420"}}}}}"#;
        const OTHER_PORT: &str =
            r#"{"Web":{"x.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8080"}}}}}"#;
        // 本来就没有配置 → 不算外来
        assert!(!foreign_serve_config("{}", 9420));
        // 兔维斯 自己的配置（指向本机 兔维斯 端口）→ 不算外来
        assert!(!foreign_serve_config(OURS, 9420));
        // 指向别的端口 → 外来（哪怕同在 127.0.0.1）
        assert!(foreign_serve_config(OTHER_PORT, 9420), "端口不同即外来");
        assert!(!foreign_serve_config(OTHER_PORT, 8080), "端口对上即自己人");
        // 空 JSON（未激活）在任意端口都不算外来
        assert!(!foreign_serve_config("{}", 8080));
        // 修复轮 2 指针①：非空但解析不了 → 保守视为外来（fail-closed）——
        // 旧实现先经 funnel_active 提前判「不算外来」，fail-closed 分支不可达
        assert!(foreign_serve_config("not json", 9420));
        assert!(
            foreign_serve_config(r#"{"Web": {"x.ts.net:443""#, 9420),
            "截断 JSON 也是「非空但解析不了」"
        );
        // 空串（run_cli 读失败兜底形态）不算外来——没有配置可覆盖
        assert!(!foreign_serve_config("", 9420));
    }

    /// B-M4/B-I5 **变异锚点**：判据从「序列化后**包含** 兔维斯 那路」改为「逐条 handler 的
    /// Proxy **精确**指向本机 兔维斯 端口」。两个已实测隐患：
    /// ① 端口前缀撞车——兔维斯 在 80、外来配置指向 8080 时旧判据（子串）判成自己人，
    ///    于是 `funnel reset` 会清掉别人的配置；
    /// ② 叠加形态（兔维斯 那路 + 用户自建）——旧判据也判成自己人。**本实现仍放行**（加严会让
    ///    公开暴露撤不掉，失败模式更糟，评审确认权衡成立），但把「将一并被清除的非 兔维斯
    ///    条目数」如实算出来供回执/日志明示（B-I5 ②）。
    /// 变异自证：判据退回子串包含 → ①档必红；Mixed 改判 Foreign → ②档必红。
    #[test]
    fn serve_ownership_is_precise_and_reports_mixed_extras() {
        const OURS: &str =
            r#"{"Web":{"x.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:9420"}}}}}"#;
        const PROXY_8080: &str =
            r#"{"Web":{"x.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8080"}}}}}"#;
        // ① 端口前缀撞车：兔维斯 在 808，外来配置指向 8080 —— 不得判成自己人
        assert!(
            foreign_serve_config(PROXY_8080, 808),
            "端口前缀不得撞车（旧子串判据在此判成自己人）"
        );
        // ② 叠加形态：兔维斯 那路 + 用户自建 /media → 归类 Mixed，**不拒绝**（见测试注释）
        const MIXED: &str = r#"{"Web":{"x.ts.net:443":{"Handlers":{
            "/":{"Proxy":"http://127.0.0.1:9420"},
            "/media":{"Proxy":"http://127.0.0.1:7000"}}}}}"#;
        assert_eq!(
            serve_ownership(MIXED, 9420),
            ServeOwnership::Mixed { others: 1 },
            "叠加形态要如实数出「将被一并清除」的非 兔维斯 条目"
        );
        assert!(
            !foreign_serve_config(MIXED, 9420),
            "叠加形态不拒绝（拒绝 = 公开暴露撤不掉，失败模式更糟）"
        );
        assert_eq!(serve_ownership(MIXED, 9420).cleared_others(), 1);
        // ③ 空表 / 只有自己那路 / 只有别人那路 / 读不懂
        assert_eq!(serve_ownership("{}", 9420), ServeOwnership::Absent);
        assert_eq!(serve_ownership("", 9420), ServeOwnership::Absent);
        assert_eq!(serve_ownership(OURS, 9420), ServeOwnership::Ours);
        assert_eq!(
            serve_ownership(PROXY_8080, 9420),
            ServeOwnership::Foreign { others: 1 }
        );
        // 非空但读不懂 → 保守视为外来（fail-closed，归属口径与旧版同）；**条目数由 0 改 1**
        // （B1）：读不懂的配置同样会被 `funnel reset` 整份清掉——把它说成「0 条」等于
        // 告诉用户「没有你的东西会被清」，那是少报；确认框要逐条列出，就必须有这一条。
        assert_eq!(
            serve_ownership("not json", 9420),
            ServeOwnership::Foreign { others: 1 },
            "非空但读不懂 → 外来且**如实计一条**（会被清掉的东西不能报 0）"
        );
        let unknown = serve_entries("not json", 9420);
        assert_eq!(unknown.len(), 1, "读不懂的配置同样要有一条可展示的条目");
        assert!(!unknown[0].ours, "读不懂的条目绝不算自己那路");
        assert!(
            unknown[0].label.contains("无法识别"),
            "条目描述要如实说『读不懂』: {}",
            unknown[0].label
        );
        // ④ 回环的其他拼写也认（tailscale 当前写 127.0.0.1，但不把拼写当契约）
        let localhost =
            r#"{"Web":{"x.ts.net:443":{"Handlers":{"/":{"Proxy":"http://localhost:9420"}}}}}"#;
        assert_eq!(serve_ownership(localhost, 9420), ServeOwnership::Ours);
        // ⑤ 无 Proxy 的 handler（Text 等形态）= 别人的条目，不得当成自己
        let non_proxy = r#"{"Web":{"x.ts.net:443":{"Handlers":{"/":{"Text":"hello"}}}}}"#;
        assert_eq!(
            serve_ownership(non_proxy, 9420),
            ServeOwnership::Foreign { others: 1 }
        );
    }

    /// 轮询单轮的**状态映射**真值表（守护循环唯一可测内核，周期逻辑不测——报告注明）：
    /// 每个状态都**如实**落进 {running, url, error}，绝不把失败伪装成运行
    #[test]
    fn map_status_maps_each_state_honestly() {
        use crate::remote::tunnel::ChannelStatus;
        let running_ts = || TsStatus {
            backend_state: "Running".into(),
            auth_url: String::new(),
            dns_name: "jarvismac-mini.example-tailnet.ts.net.".into(),
            cert_domains: vec!["jarvismac-mini.example-tailnet.ts.net".into()],
        };
        let active = r#"{"TCP":{"443":{"HTTPS":true}},"Web":{"x.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:9420"}}}}}"#;
        let foreign =
            r#"{"Web":{"x.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8080"}}}}}"#;

        // status 读不到（未装 / 调用失败）→ error 如实上墙
        let s = map_status(Err("未检测到 Tailscale".into()), Ok("{}".into()), 9420);
        assert!(!s.running);
        assert_eq!(s.url, None);
        assert!(s.error.unwrap().contains("未检测到 Tailscale"));

        // 未登录（AuthURL 在场）→ 点名「待登录」（Task 6 向导据此引导）
        let s = map_status(
            Ok(TsStatus {
                backend_state: "NeedsLogin".into(),
                auth_url: "https://login.tailscale.com/a/abc".into(),
                dns_name: String::new(),
                cert_domains: vec![],
            }),
            Ok("{}".into()),
            9420,
        );
        assert!(!s.running);
        assert!(s.error.unwrap().contains("待登录"));

        // 登录了但 Funnel 未开（实测 `{}`）→ 不运行且**无错误**（引导态 ≠ 故障态）
        let s = map_status(Ok(running_ts()), Ok("{}".into()), 9420);
        assert!(!s.running);
        assert_eq!(s.url, None);
        assert_eq!(s.error, None, "未开 Funnel 是引导态，不是故障");

        // Funnel 状态读不到 → error（不猜）
        let s = map_status(Ok(running_ts()), Err("调用失败".into()), 9420);
        assert!(!s.running);
        assert!(s.error.unwrap().contains("Funnel"));

        // 活配置不指向本机 兔维斯 端口 → 他人配置，不宣称地址
        let s = map_status(Ok(running_ts()), Ok(foreign.into()), 9420);
        assert!(!s.running);
        assert_eq!(s.url, None);
        assert!(s.error.unwrap().contains("占用"));

        // 正常态：结尾点剥离 + 补 /m（看板地址契约）
        let s = map_status(Ok(running_ts()), Ok(active.into()), 9420);
        assert!(s.running);
        assert_eq!(
            s.url.as_deref(),
            Some("https://jarvismac-mini.example-tailnet.ts.net/m")
        );
        assert_eq!(s.error, None);

        // DNSName 缺失（异常形态）→ 回落 CertDomains[0]（不带点，同值域名）
        let no_dns = TsStatus {
            dns_name: String::new(),
            ..running_ts()
        };
        let s = map_status(Ok(no_dns), Ok(active.into()), 9420);
        assert!(s.running);
        assert_eq!(
            s.url.as_deref(),
            Some("https://jarvismac-mini.example-tailnet.ts.net/m")
        );
        // 双双缺失 → 不造 `https:///m`，如实报错
        let no_name_at_all = TsStatus {
            dns_name: String::new(),
            cert_domains: vec![],
            ..running_ts()
        };
        let s = map_status(Ok(no_name_at_all), Ok(active.into()), 9420);
        assert!(!s.running);
        assert_eq!(s.url, None);
        assert!(s.error.unwrap().contains("域名"));
        let _ = ChannelStatus::default();
    }

    // ==== 修复轮 1：stop / disable 路径的「不覆盖用户既有 serve 配置」守卫 ====
    // 零网络纪律：run_cli 经 RUN_CLI_OVERRIDE 注入；全局态（override/DESIRED/快照）
    // 持 TEST_LOCK 串行 + 用后即还

    /// 换入 CLI 行为替身并返回调用记录（["funnel","status","--json"] 等命令逐次入账；
    /// 未显式应答的命令按违约 Err 记录入账后返回）
    fn spy_run_cli(
        status_reply: Result<String, String>,
    ) -> std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>> {
        let calls: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>> = Default::default();
        let c = calls.clone();
        set_run_cli_override(Some(Box::new(move |args: &[&str]| {
            let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            c.lock().unwrap().push(v.clone());
            if v == ["funnel", "status", "--json"] {
                return status_reply.clone();
            }
            if v == ["funnel", "reset"] {
                return Ok(String::new());
            }
            Err(format!("测试未应答的 CLI 调用: {v:?}"))
        })));
        calls
    }

    // ==== I-4：run_cli 的有界等待（超时 + kill）====

    /// I-4 **变异锚点**：CLI 等待上限**逐调用如实分档**——只读命令给够（后端忙时不误杀），
    /// **写路径（`funnel --bg` / `funnel reset` / `set`）用更短的窗**：A6 已证明这条 CLI
    /// 不加 `--bg` 时会**永久阻塞终端**，而 macOS 首开在设计中确有一次浏览器批准，
    /// `--bg` 在 macOS GUI standalone build 上是否立即返回**属未验证**（write_path_verified
    /// (Mac)=false）——若它等批准才返回，向导「开通」步的 spawn_blocking 线程会永久挂住
    /// （按钮永远转圈、无错误、无出路）。写路径必须更早放弃并**如实报超时**。
    /// 变异：把 `cli_timeout_for` 改成恒返回同一个值（不分档）→ 本测试必红。
    #[test]
    fn cli_timeout_is_shorter_for_write_paths_than_for_reads() {
        let read = cli_timeout_for(&["status", "--json"]);
        let read_get = cli_timeout_for(&["get", "--json"]);
        let read_funnel = cli_timeout_for(&["funnel", "status", "--json"]);
        assert_eq!(read, read_get, "只读命令同档");
        assert_eq!(
            read, read_funnel,
            "`funnel status` 是只读查询，与 status 同档"
        );
        for write in [
            vec!["funnel", "--bg", "9420"],
            vec!["funnel", "reset"],
            vec!["set", "--shields-up=false"],
        ] {
            let d = cli_timeout_for(&write);
            assert!(
                d < read,
                "写路径 {write:?} 必须比只读命令更早放弃（否则开通/自愈线程可能永久挂住）: {d:?} vs {read:?}"
            );
            assert!(
                d >= std::time::Duration::from_secs(1),
                "窗太短会把正常的慢调用误杀: {d:?}"
            );
        }
    }

    /// I-4 **变异锚点**（有界等待本体，真子进程）：
    /// ① 超时 → **杀进程 + 收尸**（不是无限等），错误文案如实说「调用 Tailscale 超时」；
    /// ② 输出大于管道缓冲（~64KB）的正常子进程**不得假性超时**——旧范式「先 wait 后读」
    ///    会在子进程写满管道时把它卡住（approve.rs 已如实登记该边界），本实现在 wait 前
    ///    就把两条管道交给读线程，故 200KB 输出照常收全。
    /// 变异：把 wait_child_bounded 的超时分支改成 `child.wait()` 无限等 → ①档永久挂住；
    /// 去掉读线程（改成先 wait 后读）→ ②档必红（假性超时）。
    #[cfg(unix)]
    #[test]
    fn run_cli_bounded_wait_kills_on_timeout_and_drains_large_output() {
        use std::process::{Command, Stdio};
        // ① 超时：sleeping 子进程必须在窗口到点后被杀掉并报「超时」
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("sleep 30")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sh -c sleep 30");
        let t0 = std::time::Instant::now();
        let e = wait_child_bounded(&mut child, std::time::Duration::from_millis(200))
            .expect_err("超时必须 Err（不是静默挂着）");
        assert!(
            e.contains("超时") && e.contains("调用 Tailscale"),
            "超时文案必须如实点名（写快照用同一条）: {e}"
        );
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(5),
            "超时窗必须被真正执行（不得等子进程自己结束）: {:?}",
            t0.elapsed()
        );
        assert!(
            matches!(child.try_wait(), Ok(Some(_))),
            "超时分支必须 kill + 收尸（防僵尸进程）"
        );

        // ② 大输出（200KB > 管道缓冲）不得假性超时
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("yes x | head -c 200000")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn 大输出子进程");
        let (status, out, _err) =
            wait_child_bounded(&mut child, std::time::Duration::from_secs(20)).expect("正常退出");
        assert!(status.success());
        assert_eq!(
            out.len(),
            200_000,
            "管道必须被并发排空（先 wait 后读会假性超时）"
        );
    }

    /// **I-4 的边界：超时分支不得被「孙进程持有管道」拖住**（2026-10-07 Linux CI 抓获）。
    ///
    /// 机制（诊断结论）：`wait_child_bounded` 在 wait 前把两条管道交给读线程，读线程要等
    /// **管道 EOF**；而 EOF 只由「所有持有写端的进程都退出」触发。若直接子进程 fork 出了
    /// 继承 stdout/stderr 的孙进程，kill 掉直接子进程后孙进程仍持有写端 ⇒ 读线程要等孙
    /// 进程自己退出 —— **有界等待被打回无界**（实测：Linux 上 `sh -c "sleep 30"` 的 200ms
    /// 超时窗耗了 30.0037s，恰等于子进程自然结束的时间）。
    ///
    /// **为什么 macOS 上没暴露**：`/bin/sh`（bash 3.2）对**单条命令**会 exec 成直接子进程
    /// （实测：spawn 出的 pid 本身就是 `sleep`，子进程数 0），没有孙进程持管；而 Linux 的
    /// `/bin/sh` 在此 fork。故**本缺陷不属于 Linux、属于我们的实现**——凡 fork 的写法都能
    /// 触发。本用例用 `&`（强制 fork，不给 shell exec 优化的机会）在**任何平台**复现该形状，
    /// 因此在 macOS 上也能跑红。
    ///
    /// 变异：把超时分支的收线程改回无界 `JoinHandle::join` → 本用例必红（等满 sleep）。
    #[cfg(unix)]
    #[test]
    fn bounded_wait_timeout_survives_forked_grandchild_holding_pipe() {
        use std::process::{Command, Stdio};
        // `&` 保证 shell 不把它 exec 成直接子进程 —— 孙进程继承管道写端
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("sleep 20 & wait")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sh -c 'sleep 20 & wait'");
        let t0 = std::time::Instant::now();
        let e = wait_child_bounded(&mut child, std::time::Duration::from_millis(200))
            .expect_err("超时必须 Err（不是静默挂着）");
        assert!(e.contains("超时"), "超时文案必须如实点名: {e}");
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(5),
            "孙进程持有管道时，超时窗仍必须被真正执行（不得等管道 EOF）: {:?}",
            t0.elapsed()
        );
        assert!(
            matches!(child.try_wait(), Ok(Some(_))),
            "超时分支必须 kill + 收尸（防僵尸进程）"
        );
    }

    /// I-4（用户可见行为）：**超时不是静默失败**——它经既有快照口径上墙（轮询轮
    /// `refresh_once` → `map_status`；开关路径 `record_ts_failure` 同一处 error 字段），
    /// 用户在卡面读得到「调用 Tailscale 超时」；同时 running 撤下（不宣称运行）。
    /// 变异：让 wait_child_bounded 超时后返回 Ok（静默）→ 本测试必红。
    #[test]
    fn cli_timeout_surfaces_as_snapshot_error_not_a_silent_hang() {
        let _g = test_lock();
        *DESIRED.lock().unwrap() = Some(9420);
        set_ts_snapshot(|s| {
            s.running = true;
            s.url = Some("https://x.example-tailnet.ts.net/m".into());
        });
        set_run_cli_override(Some(Box::new(|_a: &[&str]| {
            Err(
                "调用 Tailscale 超时（10 秒未返回，已终止该子进程；该命令可能正在等待人工确认）"
                    .to_string(),
            )
        })));
        refresh_once(9420);
        let s = ts_snapshot();
        assert!(!s.running, "超时不得继续宣称运行");
        assert_eq!(s.url, None, "超时不得继续宣称地址");
        let e = s.error.unwrap_or_default();
        assert!(
            e.contains("超时"),
            "超时必须如实写成快照 error（用户可见），不得静默: {e}"
        );
        teardown_globals();
    }

    /// 用后即还（override / DESIRED / 快照 / 校验态 / 两个校验探针缝 六全局一并归零）
    fn teardown_globals() {
        set_run_cli_override(None);
        *DESIRED.lock().unwrap() = None;
        set_ts_snapshot(|s| *s = crate::remote::tunnel::ChannelStatus::default());
        set_reachability(Reachability::Unverified);
        set_verify_probe_override(None);
        set_http_probe_override(None);
        // I3 起留痕也会被「通道关着」的用例读到（不写快照但照记），必须一并归零
        set_deferred_open_error(None);
        // W-B：档位/恢复窗口记账量与两个注入缝一并归零（顺序无关，都是置位）
        reset_wb_globals();
    }

    /// 守卫专测（Finding 1）：配置已是外来时 stop_channel 必须拒绝 `funnel reset`
    /// （它清整份 serve 配置——静默清掉用户自建配置不可接受），错误口径与 start_channel
    /// 同款；但**本地宣称先收口**（DESIRED 清空 + 快照复位，卡面绝不继续撒谎）。
    /// 变异锚点：去掉 stop_inner 的守卫分支 → reset 被发出 → 本测试必红
    #[test]
    fn stop_channel_refuses_to_reset_foreign_serve_config() {
        const FOREIGN: &str =
            r#"{"Web":{"x.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8080"}}}}}"#;
        let _g = test_lock();
        *DESIRED.lock().unwrap() = Some(9420);
        set_ts_snapshot(|s| {
            s.running = true;
            s.url = Some("https://x.example-tailnet.ts.net/m".into());
        });
        let calls = spy_run_cli(Ok(FOREIGN.into()));

        let r = stop_channel();
        let e = r.unwrap_err();
        assert!(
            e.contains("检测到非 兔维斯") && e.contains("funnel reset"),
            "拒绝文案须沿用 start_channel 守卫口径（说清被拒原因与手动出口）: {e}"
        );
        // 本地宣称先收口（无论守卫结果如何，兔维斯 不再宣称运行）
        assert_eq!(*DESIRED.lock().unwrap(), None, "期望态必须已清空");
        let s = ts_snapshot();
        assert!(!s.running, "快照必须已复位（running 不再宣称）");
        assert_eq!(s.url, None, "快照必须已复位（地址不再宣称）");
        // 守卫拒绝后不得发出任何撤配置调用（记录里只允许有守卫自己的 status 探针）
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .all(|c| c == &["funnel", "status", "--json"]),
            "外来配置在先不得发出 funnel reset: {:?}",
            calls.lock().unwrap()
        );
        teardown_globals();
    }

    /// 守卫专测（Finding 1）：run_step("disable") 与 stop_channel 走**同一守卫**——
    /// 端口取向导步参数（hint），拒绝时同样不清配置；且 DESIRED 同步收口
    /// （向导停 = 完整停止语义，与 stop_all 一致——评审核对口径后接线的前提）
    #[test]
    fn run_step_disable_goes_through_same_foreign_guard() {
        const FOREIGN: &str =
            r#"{"Web":{"x.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8080"}}}}}"#;
        let _g = test_lock();
        *DESIRED.lock().unwrap() = Some(9420);
        let calls = spy_run_cli(Ok(FOREIGN.into()));

        let r = run_step("disable", 9420);
        let e = r.unwrap_err();
        assert!(e.contains("检测到非 兔维斯"), "向导撤销同样拒外来配置: {e}");
        assert_eq!(*DESIRED.lock().unwrap(), None, "向导停同样收口期望态");
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .all(|c| c == &["funnel", "status", "--json"]),
            "向导撤销同样不得发出 funnel reset: {:?}",
            calls.lock().unwrap()
        );
        teardown_globals();
    }

    /// B-I5 ② + B-M4 **变异锚点**：
    /// ① 叠加形态（兔维斯 那路 + 用户自建）撤销**不拒绝**，但回执必须如实报出「另有 N 条
    ///    非 兔维斯 条目将一并被清除」——这是用户唯一能看到"你的 /media 也没了"的地方；
    /// ② 完全外来（含端口被改后 兔维斯 旧配置变"外来"的死局）给一条**显式强制撤销**出口：
    ///    `disable_force` 跳过归属守卫但仍如实回报条目数（不静默）。没有这条出口，用户
    ///    会遇到「Funnel 留在公网且 UI 无出路」。
    /// 变异自证：把 Mixed 也判 Foreign → ①档 Err 必红；删掉 disable_force 分支 → ②档必红。
    #[test]
    fn disable_reports_extras_and_force_bypasses_the_guard() {
        let _g = test_lock();
        const MIXED: &str = r#"{"Web":{"x.ts.net:443":{"Handlers":{
            "/":{"Proxy":"http://127.0.0.1:9420"},
            "/media":{"Proxy":"http://127.0.0.1:7000"}}}}}"#;
        // ① 叠加形态：放行 + 回执报数
        *DESIRED.lock().unwrap() = Some(9420);
        let calls = spy_run_cli(Ok(MIXED.into()));
        let r = run_step("disable", 9420).expect("叠加形态不得拒绝撤销（否则暴露撤不掉）");
        assert_eq!(r["ok"], true);
        assert_eq!(
            r["clearedExtraServeEntries"], 1,
            "必须明示「另有 1 条非 兔维斯 条目被一并清除」: {r}"
        );
        // M-1：回执要带上**被清除的条目本身**（不只是条数）——前端卡面「N 条」与列表
        // 由此**同源**（都来自这次撤销的实际条目表），不再「确认框列预览条目、toast 报
        // 撤销时的条数」两处对不上
        assert_eq!(
            r["clearedEntries"],
            serde_json::json!([{ "ours": false, "label": "x.ts.net:443 /media → http://127.0.0.1:7000" }]),
            "回执必须逐条报出被一并清除的非 兔维斯 条目（与条数同源）: {r}"
        );
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c == &["funnel".to_string(), "reset".to_string()]),
            "叠加形态照常撤销: {:?}",
            calls.lock().unwrap()
        );
        teardown_globals();

        // ② 完全外来：普通撤销拒绝；disable_force 是显式确认后的出口
        const FOREIGN: &str =
            r#"{"Web":{"x.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8080"}}}}}"#;
        *DESIRED.lock().unwrap() = Some(9420);
        let calls = spy_run_cli(Ok(FOREIGN.into()));
        assert!(
            run_step("disable", 9420).is_err(),
            "外来配置在先，普通撤销必须拒绝"
        );
        assert!(
            !calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c == &["funnel".to_string(), "reset".to_string()]),
            "拒绝时不得发出 reset: {:?}",
            calls.lock().unwrap()
        );
        // force：跳过归属守卫（用户已显式确认），仍如实回报被清条目数
        *DESIRED.lock().unwrap() = Some(9420);
        let calls = spy_run_cli(Ok(FOREIGN.into()));
        let r = run_step("disable_force", 9420).expect("强制撤销是死局的唯一出口");
        assert_eq!(r["ok"], true);
        assert_eq!(r["clearedExtraServeEntries"], 1, "强制撤销同样如实报数");
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c == &["funnel".to_string(), "reset".to_string()]),
            "强制撤销必须真的发出 reset: {:?}",
            calls.lock().unwrap()
        );
        assert_eq!(*DESIRED.lock().unwrap(), None, "强制撤销同样收口期望态");
        teardown_globals();
    }

    /// **C-1 防线（Critical）**：撤销 tailscale 的**第二条入口**（向导 disable /
    /// disable_force 步——前端确认框路径走它，不经 remote_toggle_channel）成功后必须
    /// **落通道开关位**。不落位的三连后果：
    /// ① `enabled` 仍为 true ⇒ 卡面开关保持 ON（开关与既成事实背离）；
    /// ② 成因行落到 `offCause='listening'` ⇒ 对刚被用户撤销的通道谎报「服务没在监听」；
    /// ③ **重启（或总开关重新打开）→ restore_enabled_tunnels 逐通道读 KV → start_channel
    ///    → `funnel --bg` ⇒ 用户刚撤销的公网暴露被自动重新打开**。
    /// 本测试跑完整链条（撤销步 → KV → 恢复），③ 即本测试的真实防线。
    /// 变异：删掉 disable_step_with 里的 clear_chan_flag() 调用 → 本测试必红
    /// （restore 会把 tailscale 重新 ensure 一遍）。
    #[test]
    fn revoke_step_lands_the_channel_flag_so_restart_cannot_reopen_the_funnel() {
        let _g = test_lock();
        // 撤销前 KV：tailscale 开着（用户此刻正是从"开着"撤销的）
        let mut kv = crate::remote::ChannelFlags {
            tailscale: true,
            ..Default::default()
        };
        let r = disable_step_with(
            9420,
            true,
            |port, force| {
                assert_eq!(
                    port,
                    Some(9420),
                    "向导步参数（端口 hint）必须透传给停止内核"
                );
                assert!(force, "disable_force 的强制语义必须透传");
                Ok(ServeResetReport {
                    cleared_others: 0,
                    forced: false,
                    cleared_entries: Vec::new(),
                })
            },
            // KV 写入替身（生产 = remote::clear_tailscale_chan_flag → write_chan_flag）
            || kv.tailscale = false,
            // 审计出口替身（生产 = remote::events::audit；本测试只锁开关位，审计另有专测）
            |_, _| {},
            // 事件出口替身（生产 = remote::events::emit_ui；本测试只锁开关位，事件另有专测）
            |_, _| {},
        )
        .expect("撤销成功");
        assert_eq!(r["ok"], true);
        assert!(
            !kv.tailscale,
            "撤销成功后通道开关位必须落关（KV 单写入者写口不变）"
        );

        // ③ 重启 / 总开关重新打开：逐通道恢复读同一份 KV —— 不得再 ensure tailscale
        let mut ensured: Vec<&'static str> = Vec::new();
        crate::remote::restore_tunnels_core(kv, |k| ensured.push(k.as_str()));
        assert!(
            ensured.is_empty(),
            "撤销落 KV 之后，重启恢复绝不能再开通该通道（否则 funnel --bg 把用户刚撤销的\
             公网暴露自动重新打开）: {ensured:?}"
        );
    }

    /// C-1 反面（失败不撒谎）：撤销**没成功**（守卫拒绝 / CLI 失败）时不得落开关位——
    /// 落了位就是「开关说已关、实际公网还开着」，且下次启动不再恢复，用户以为撤掉了。
    /// 变异：把 disable_step_with 的 clear_chan_flag() 提到 stop() 之前 → 本测试必红。
    #[test]
    fn failed_revoke_does_not_land_the_channel_flag() {
        let _g = test_lock();
        let r = disable_step_with(
            9420,
            false,
            |_, _| {
                Err("检测到非 兔维斯 的 Tailscale serve/Funnel 配置，为避免覆盖已中止撤销".into())
            },
            || panic!("撤销失败不得落开关位（否则就是『开关说已关、实际公网还开着』）"),
            |_, _| panic!("撤销失败不得发审计（专测见 failed_revoke_emits_no_audit）"),
            |_, _| {
                panic!("撤销失败不得广播 remote-changed（专测见 failed_revoke_does_not_broadcast_remote_changed）")
            },
        );
        assert!(r.is_err(), "失败的撤销必须原样 Err 上抛");
    }

    /// **撤销路径的审计留痕（B1 补账）**：撤销 tailscale 有两条入口——① 开关命令
    /// `remote_toggle_channel`（`remote/mod.rs` 发 `channel_toggled` 审计）；② 向导步
    /// `disable` / `disable_force`（前端撤销确认框走这条，**不经**①）。B1 把撤销整体挪到
    /// 向导步时顺手丢了后者的审计（不是老问题）——**不对称的审计比没有更糟**：事后只看到
    /// 一串「开」记录、找不到对应的「关」记录，「这个 Funnel 什么时候关的 / 关没关」无从
    /// 回答；而撤销路径还可能连带清掉用户自己的 serve 条目（刻意接受的取舍）、本身就是撤下
    /// 公网暴露的安全动作，与暴露出去同量级——更必须留痕。
    /// 本测试锁三件事：① 成功后**恰有一条**审计；② 动作名与开通路径**同源**
    /// （`channel_toggled`，`channel=tailscale` 一把 grep 就可拉出完整开/关序列）；
    /// ③ 明细带上 `forced`（是否走了强制撤销）与 `cleared_others`（连带清掉几条非 兔维斯
    /// 条目）两个事实字段——「强制撤销」与「顺带清了用户配置」正是最需要留痕的两种情况。
    /// **变异锚点：删掉 `disable_step_with` 里的 audit 调用 → 本测试必红。**
    #[test]
    fn revoke_step_audits_success_with_forced_and_cleared_counts() {
        let _g = test_lock();
        let lines: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>> = Default::default();
        let sink = lines.clone();
        let r = disable_step_with(
            9420,
            true,
            |_, _| {
                Ok(ServeResetReport {
                    cleared_others: 2,
                    forced: true,
                    cleared_entries: Vec::new(),
                })
            },
            || {},
            move |action: &str, detail: &str| {
                sink.lock()
                    .unwrap()
                    .push((action.to_string(), detail.to_string()));
            },
            // 事件出口替身（生产 = remote::events::emit_ui）：审计专测只锁审计，事件另有专测
            |_, _| {},
        )
        .expect("撤销成功");
        assert_eq!(r["ok"], true);
        let lines = lines.lock().unwrap();
        assert_eq!(
            lines.len(),
            1,
            "撤销成功后必须留一条审计（不多不少）: {lines:?}"
        );
        let (action, detail) = &lines[0];
        assert_eq!(
            action, "channel_toggled",
            "动作名与开通路径同源——开/关记录必须成对可 grep（正是本条要修的「不对称」）: {action}"
        );
        assert!(
            detail.contains("channel=tailscale") && detail.contains("on=false"),
            "明细要点名通道与「关」方向: {detail}"
        );
        assert!(
            detail.contains("forced=true"),
            "是否强制撤销必须入账（跳过归属守卫的情况最需要留痕）: {detail}"
        );
        assert!(
            detail.contains("cleared_others=2"),
            "连带清掉几条非 兔维斯 条目必须入账（清用户配置最需要留痕）: {detail}"
        );
    }

    /// **失败不撒谎（审计版）**：守卫拒绝 / CLI 失败时**不得**发「已撤销」的审计。
    /// 假审计比没有审计更糟——事后按日志追溯会得出「Funnel 已关」的错误结论，而真值是
    /// 配置还在公网、暴露还在。与 `failed_revoke_does_not_land_the_channel_flag`
    /// （开关位版）是同一条纪律的两个面。
    /// **变异锚点：把 `disable_step_with` 的 audit 调用提到 `stop()` 之前 → 本测试必红。**
    #[test]
    fn failed_revoke_emits_no_audit() {
        let _g = test_lock();
        let lines: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>> = Default::default();
        let sink = lines.clone();
        let r = disable_step_with(
            9420,
            false,
            |_, _| {
                Err("检测到非 兔维斯 的 Tailscale serve/Funnel 配置，为避免覆盖已中止撤销".into())
            },
            || {
                panic!(
                    "撤销失败不得落开关位（防线见 failed_revoke_does_not_land_the_channel_flag）"
                )
            },
            move |action: &str, detail: &str| {
                sink.lock()
                    .unwrap()
                    .push((action.to_string(), detail.to_string()));
            },
            // 事件出口替身（生产 = remote::events::emit_ui）：本测试只锁「失败不发审计」
            |_, _| {},
        );
        assert!(r.is_err(), "失败的撤销必须原样 Err 上抛");
        assert!(
            lines.lock().unwrap().is_empty(),
            "撤销失败不得发「已撤销」审计（假审计会让人误以为公网暴露已撤下）: {:?}",
            lines.lock().unwrap()
        );
    }

    /// **撤销成功必须广播 `remote-changed`（向导路撤销 → 托盘「远程地址」回落真值）**：
    /// `remote-changed` 全仓只有 1 个消费方（`src/pages/home.tsx` → `initTrayMenu()` →
    /// `refresh_tray`），而托盘的「复制远程地址」项取值依赖 tailscale 快照
    /// （`remote::tray_display().1` → `tray_url_from`）。向导路撤销会复位快照 + 作废校验态
    /// （`stop_inner` 本地宣称先收口），**真值**已回落 cloudflared/局域网地址——但托盘只在
    /// 挂载与事件时重建（`system_tray.rs` 无定时器；3 秒轮询是设置卡的），不发事件托盘就会
    /// 继续显示那个**已经打不开**的固定地址。本模块的核心纪律是「绝不把打不开的地址呈现成
    /// 可用」，故撤销成功后必须广播（与开通路径 `remote_toggle_channel` 同一条惯例）。
    /// 本测试锁三件事：① 成功 → **恰一次**广播；② 事件名 = `remote-changed`（托盘监听的就是
    /// 它）；③ 载荷与开关命令既有形状同源（`{channel, on}`）。
    /// **变异锚点：删掉 `disable_step_with` 的 emit 调用 → 本测试必红。**
    #[test]
    fn revoke_step_broadcasts_remote_changed_on_success() {
        let _g = test_lock();
        let events: std::sync::Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>> =
            Default::default();
        let sink = events.clone();
        let r = disable_step_with(
            9420,
            false,
            |_, _| Ok(ServeResetReport::default()),
            || {},
            |_, _| {},
            move |event: &str, payload: serde_json::Value| {
                sink.lock().unwrap().push((event.to_string(), payload));
            },
        )
        .expect("撤销成功");
        assert_eq!(r["ok"], true);
        let events = events.lock().unwrap();
        assert_eq!(
            events.len(),
            1,
            "撤销成功必须**恰**广播一次 remote-changed（托盘据此重建、地址回落真值）: {events:?}"
        );
        assert_eq!(
            events[0].0, "remote-changed",
            "事件名必须与开通路径同源（托盘监听的就是它）: {events:?}"
        );
        assert_eq!(
            events[0].1,
            serde_json::json!({ "channel": "tailscale", "on": false }),
            "载荷与开关命令的既有形状同源（channel + on）: {events:?}"
        );
    }

    /// **失败不撒谎（事件版）**：守卫拒绝 / CLI 失败时不得广播 `remote-changed`——撤销没成功，
    /// 固定地址**仍然可用**，托盘那条地址就该保持原样（此时广播会让托盘重建、把一条仍然
    /// 有效的固定地址换成次优地址，同样是"呈现与现实不符"）。与审计版的
    /// `failed_revoke_emits_no_audit` 是同一条纪律的两个面。
    /// **变异锚点：把 `disable_step_with` 的 emit 调用提到 `stop()` 之前 → 本测试必红。**
    #[test]
    fn failed_revoke_does_not_broadcast_remote_changed() {
        let _g = test_lock();
        let r = disable_step_with(
            9420,
            false,
            |_, _| {
                Err("检测到非 兔维斯 的 Tailscale serve/Funnel 配置，为避免覆盖已中止撤销".into())
            },
            || {
                panic!(
                    "撤销失败不得落开关位（防线见 failed_revoke_does_not_land_the_channel_flag）"
                )
            },
            |_, _| panic!("撤销失败不得发「已撤销」审计（见 failed_revoke_emits_no_audit）"),
            |_, _| panic!("撤销失败不得广播 remote-changed（固定地址仍可用，托盘该保持原样）"),
        );
        assert!(r.is_err(), "失败的撤销必须原样 Err 上抛");
    }

    /// **总开关关闭路径的撤销审计（B1 补账 · 第二条：补掉最后一处不对称）**：撤销 tailscale
    /// 三处入口此前只有两处留痕——① 开关命令 `remote_toggle_channel`（有）；② 向导确认框
    /// `disable` / `disable_force` 步（上一批补上）；③ **总开关关闭**（`remote_toggle(false)`
    /// → `stop_server_explicit_close` → `stop_server` → `tailscale::stop_all`）**没有**——而
    /// 这恰是用户最可能用来撤掉公网暴露的动作：漏了它，审计在最主要的路径上失效（事后只看到
    /// 一串「开」记录，找不到「关」）。
    /// 本测试锁四件事：① 真的拆掉（`funnel reset` 成功返回）→ **恰一条**撤销审计；
    /// ② 动作名与字段同源（`channel_toggled` + `channel=tailscale` `on=false`——一把
    /// `channel=tailscale` 的 grep 就能拉出完整开/关序列）；③ `cleared_others` 如实入账
    /// （叠加形态下「顺带清了用户自己几条配置」正是最需要留痕的事；两档不同取值同时证明
    /// 该字段来自本次拆卸报告而非硬编常量）；④ 本路径**不写** `forced`（这条路的 `force`
    /// 恒为 false，写一个恒假常量是硬编噪声、不是事实——是否强制只有向导步那条路有）。
    /// **变异锚点：删掉 `stop_all_with` 里的 audit 调用 → 本测试必红。**
    #[test]
    fn master_off_audits_revoke_when_funnel_was_really_torn_down() {
        let _g = test_lock();
        // ① 我们自己那份（`{}` 实测形态）：干净撤销，无连带
        *DESIRED.lock().unwrap() = Some(9420);
        set_ts_snapshot(|s| {
            s.running = true;
            s.url = Some("https://x.example-tailnet.ts.net/m".into());
        });
        let calls = spy_run_cli(Ok("{}".into()));
        let lines: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>> = Default::default();
        let sink = lines.clone();
        stop_all_with(move |action: &str, detail: &str| {
            sink.lock()
                .unwrap()
                .push((action.to_string(), detail.to_string()));
        });
        {
            let lines = lines.lock().unwrap();
            assert_eq!(
                lines.len(),
                1,
                "总开关关闭真的拆掉 Funnel 时必须**恰**留一条撤销审计: {lines:?}"
            );
            let (action, detail) = &lines[0];
            assert_eq!(
                action, "channel_toggled",
                "动作名与开通路径/向导撤销步同源（开/关记录必须成对可 grep）: {action}"
            );
            assert!(
                detail.contains("channel=tailscale") && detail.contains("on=false"),
                "明细要点名通道与「关」方向: {detail}"
            );
            assert!(
                detail.contains("cleared_others=0"),
                "连带清掉的非 兔维斯 条目数如实入账（无连带记 0）: {detail}"
            );
            assert!(
                !detail.contains("forced"),
                "本路径 force 恒 false（从不强制撤销）——不得硬编一个恒假字段冒充事实: {detail}"
            );
        }
        // 留痕不得替代真拆：reset 必须真的发出（否则就是「记了撤销、实际没撤」）
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c == &["funnel".to_string(), "reset".to_string()]),
            "留痕的前提是拆卸真的发生: {:?}",
            calls.lock().unwrap()
        );
        // 既有停机口径不变：本地宣称先收口（DESIRED 清空 + 快照复位）
        assert_eq!(*DESIRED.lock().unwrap(), None, "期望态必须已清空");
        assert!(!ts_snapshot().running, "快照必须已复位（不再宣称运行）");
        teardown_globals();

        // ② 叠加形态（兔维斯 那路 + 用户自建）：连带清掉的条数如实入账
        const MIXED: &str = r#"{"Web":{"x.ts.net:443":{"Handlers":{
            "/":{"Proxy":"http://127.0.0.1:9420"},
            "/media":{"Proxy":"http://127.0.0.1:7000"}}}}}"#;
        *DESIRED.lock().unwrap() = Some(9420);
        let _calls = spy_run_cli(Ok(MIXED.into()));
        let lines2: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let sink2 = lines2.clone();
        stop_all_with(move |_a: &str, d: &str| sink2.lock().unwrap().push(d.to_string()));
        let lines2 = lines2.lock().unwrap();
        assert_eq!(lines2.len(), 1, "叠加形态同样恰一条: {lines2:?}");
        assert!(
            lines2[0].contains("cleared_others=1"),
            "连带清掉 1 条非 兔维斯 条目必须如实入账（同字段同口径）: {lines2:?}"
        );
        drop(lines2);
        teardown_globals();
    }

    /// **失败不撒谎（总开关关闭版）· 防线一：本就没开 → 零条**：`stop_inner` 的「无事可做」
    /// 早退（端口 hint 与期望态皆无）不得被当成一次「撤销」留痕——本就没开，哪来的撤销？
    /// 与既有「零 CLI 早退」断言同源（同一替身纪律：CLI 替身 panic 证明零调用）。
    /// **变异锚点：把 `stop_all_with` 的 audit 调用移出 `Ok(Some(_))` 分支（例如 `match` 之后
    /// 无条件调用）→ 本测试必红。**
    #[test]
    fn master_off_without_running_channel_emits_no_audit() {
        let _g = test_lock();
        set_run_cli_override(Some(Box::new(|args: &[&str]| {
            panic!("本就没开不得调 CLI: {args:?}")
        })));
        let lines: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let sink = lines.clone();
        stop_all_with(move |a: &str, d: &str| sink.lock().unwrap().push(format!("{a} {d}")));
        assert!(
            lines.lock().unwrap().is_empty(),
            "本就没开不得发「已撤销」审计（无撤销可言）: {:?}",
            lines.lock().unwrap()
        );
        teardown_globals();
    }

    /// **失败不撒谎（总开关关闭版）· 防线二：拆卸失败 → 零条**：守卫拒绝（配置外来）或 CLI
    /// 失败时 Funnel 还在公网、暴露还在——此时发「已撤销」是**假审计**，比没有审计更糟：
    /// 事后按日志追溯会得出「已撤下」的错误结论。与向导撤销步的 `failed_revoke_emits_no_audit`
    /// 是同一条纪律的两个入口。
    /// **变异锚点：把 `stop_all_with` 的 audit 调用提到结果判定之前（或提到 `stop_inner` 之前）
    /// → 本测试必红。**
    #[test]
    fn failed_master_off_revoke_emits_no_audit() {
        let _g = test_lock();
        // ① CLI 失败（`funnel status --json` 读不到）：fail-closed 拒绝拆卸 → 不得留痕
        *DESIRED.lock().unwrap() = Some(9420);
        let calls = spy_run_cli(Err("CLI 失败（测试注入）".into()));
        let lines: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let sink = lines.clone();
        stop_all_with(move |a: &str, d: &str| sink.lock().unwrap().push(format!("{a} {d}")));
        assert!(
            lines.lock().unwrap().is_empty(),
            "拆卸失败（读不到配置）不得发「已撤销」审计: {:?}",
            lines.lock().unwrap()
        );
        assert!(
            !calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c == &["funnel".to_string(), "reset".to_string()]),
            "读不到配置就不得发出 reset（fail-closed）: {:?}",
            calls.lock().unwrap()
        );
        teardown_globals();

        // ② 守卫拒绝（配置完全外来）：同样拒绝拆卸 → 不得留痕
        const FOREIGN: &str =
            r#"{"Web":{"x.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8080"}}}}}"#;
        *DESIRED.lock().unwrap() = Some(9420);
        let calls = spy_run_cli(Ok(FOREIGN.into()));
        let lines2: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let sink2 = lines2.clone();
        stop_all_with(move |a: &str, d: &str| sink2.lock().unwrap().push(format!("{a} {d}")));
        assert!(
            lines2.lock().unwrap().is_empty(),
            "守卫拒绝（配置外来）不得发「已撤销」审计: {:?}",
            lines2.lock().unwrap()
        );
        assert!(
            !calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c == &["funnel".to_string(), "reset".to_string()]),
            "守卫拒绝不得发出 reset: {:?}",
            calls.lock().unwrap()
        );
        teardown_globals();
    }

    /// 守卫正路：配置为空（`{}` 实测形态）或本就是 兔维斯 自己那份 → 正常撤销
    /// （`funnel reset` 恰好发出一次）；DESIRED 为 None（本就没开）时 stop_all 零 CLI
    /// 调用直接返回——三处停机路径（修复轮 2 接线）依赖这条「无事可做不悬挂」
    #[test]
    fn stop_channel_resets_when_config_is_ours_or_absent() {
        let _g = test_lock();
        // 空配置：正常撤，status 探针 + reset 恰各一次
        *DESIRED.lock().unwrap() = Some(9420);
        let calls = spy_run_cli(Ok("{}".into()));
        assert!(stop_channel().is_ok());
        assert_eq!(
            *calls.lock().unwrap(),
            [
                vec![
                    "funnel".to_string(),
                    "status".to_string(),
                    "--json".to_string()
                ],
                vec!["funnel".to_string(), "reset".to_string()],
            ],
            "非外来配置 → funnel reset 恰好一次（status 探针在前）"
        );
        teardown_globals();

        // 本就没开（DESIRED=None）：stop_all 早退，零 CLI 调用（panic 替身证明）
        set_run_cli_override(Some(Box::new(|args: &[&str]| {
            panic!("本就没开不得调 CLI: {args:?}")
        })));
        stop_all();
        set_run_cli_override(None);
    }

    // ==== Task 6：首次配置引导（§C2）——步骤表 / 资产表 / 下载校验复用 ====

    /// B-M5 **变异锚点**：写路径（`funnel --bg` / `funnel reset` / 批准链接抓取）的
    /// 实机验证位必须**逐平台如实**——**macOS 零实机验证（false）**；**Windows 已于
    /// 2026-10-07 实测三条写路径（true）**（探测第 3/7/8 节：`set --shields-up=false`
    /// 写后回读、`funnel --bg` 退出码 0/0.1 秒返回、`funnel reset` 后回 `{}`）。
    /// 谁在没有实机证据的情况下动这两个位，本测试必红（真做了实测回填时，同步改本测试
    /// + 设计文档追记）。「只读命令已实测」与「写路径已验证」是两件事。
    #[test]
    fn write_path_verified_flag_is_honest_per_platform() {
        assert!(
            !write_path_verified(Platform::Mac),
            "macOS 写路径零实机验证，不得标 true"
        );
        assert!(
            write_path_verified(Platform::Windows),
            "Windows 写路径 2026-10-07 已实测（三条命令跑过并记录退出码）"
        );
        assert!(
            !write_path_verified(Platform::Other),
            "无资产无探测的平台不得标 true"
        );
        // 载荷透出同一位（前端弱提示的数据源）
        let _g = test_lock();
        set_run_cli_override(Some(Box::new(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_RUNNING.into()),
            ["get", "--json"] => Ok(r#"{"shields-up": false}"#.into()),
            ["funnel", "status", "--json"] => Ok("{}".into()),
            other => Err(format!("测试未应答的 CLI 调用: {other:?}")),
        })));
        let v = wizard_status_with(9420, true);
        assert_eq!(
            v["writePathVerified"],
            write_path_verified(current_platform()),
            "载荷必须如实透出本平台的写路径验证位"
        );
        teardown_globals();
    }

    /// 平台差异**只有一个**：macOS 多一步「批准系统扩展」。这条锁死「Windows = macOS - 1 步」。
    #[test]
    fn windows_has_exactly_one_fewer_step_than_macos() {
        let mac = wizard_steps(Platform::Mac);
        let win = wizard_steps(Platform::Windows);
        assert_eq!(mac.len(), 9, "macOS 九步（已实测）");
        assert_eq!(win.len(), 8, "Windows 八步（2026-10-07 实机校验回填）");
        assert!(
            mac.iter().any(|s| s.id == "sys_ext"),
            "macOS 必须有系统扩展步"
        );
        assert!(
            !win.iter().any(|s| s.id == "sys_ext"),
            "Windows 不得有系统扩展步"
        );
        // 其余八步 id 序列必须完全一致（防止两平台各自漂移）
        let strip = |v: &[WizardStep]| {
            v.iter()
                .filter(|s| s.id != "sys_ext")
                .map(|s| s.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(strip(&mac), strip(&win));
    }

    /// 每个需要人的步骤都必须带一个可展示的动作文案键——否则引导会变成"卡住但不说为什么"。
    #[test]
    fn human_steps_all_carry_an_action_key() {
        for p in [Platform::Mac, Platform::Windows] {
            for s in wizard_steps(p) {
                if s.needs_human {
                    assert!(
                        !s.human_action_key.is_empty(),
                        "{} 的需要人步骤缺文案键",
                        s.id
                    );
                }
            }
        }
    }

    /// **2026-10-08 缺口锚点（用户实测）**：Windows 走 MSI，**安装向导会让用户选安装路径**
    /// （兔维斯 执行 `msiexec /i` **刻意不加 /qn**——选择权本就该给用户）⇒ 安装步的动作文案
    /// 必须**逐平台分叉**：Windows 那份要事前点明「用默认路径最省事 / 装到别处也可以，
    /// 请记住那个路径」；macOS 那份**不得夹带**（.pkg 由 `installer` 固定装到
    /// /Applications，用户根本无从选择，套 Windows 的话就是张冠李戴的谎报）。
    /// 变异：把 install 步的文案键改回两平台同值 → 本测试必红。
    #[test]
    fn install_step_action_key_warns_windows_users_about_the_install_path() {
        let install_key = |p: Platform| {
            wizard_steps(p)
                .into_iter()
                .find(|s| s.id == "install")
                .expect("两平台都必有 install 步")
                .human_action_key
        };
        assert_eq!(
            install_key(Platform::Windows),
            "settings.remote.tsWizard.actAdminWinMsi",
            "Windows 的 MSI 会让用户选安装路径 ⇒ 动作文案必须点明（默认路径最省事）"
        );
        assert_eq!(
            install_key(Platform::Mac),
            "settings.remote.tsWizard.actAdmin",
            "macOS 的 .pkg 固定装到 /Applications，不得套用 Windows 特有的路径提示"
        );
        assert_ne!(
            install_key(Platform::Mac),
            install_key(Platform::Windows),
            "两平台文案必须分叉（同值 = 又把 Windows 特有的话塞给 mac 用户）"
        );
    }

    /// A1（2026-10-07 Windows 实测）**变异锚点**：Funnel 首次开通**不必然**要求浏览器
    /// 批准——Windows 11 家庭版 / Tailscale 1.102.4 MSI 实测 `funnel --bg 19999` 退出码 0、
    /// 0.1 秒返回、**零批准链接、零人工点击**（推测该尾网 ACL 已允许本节点 Funnel）；
    /// 而 macOS 实测首开**确有**一次浏览器批准。故批准是**分支**不是必经步骤：
    /// ① 步骤表必须能表达「本平台/本尾网可能无需批准」（funnel = 可选人工步）；
    /// ② 本次探测范围（第 6–9 步：shields_up / funnel / verify / autostart）内
    ///    **必需**人工步 = 0——实测零点击，登记为数据而非注释。
    /// 变异自证：把 funnel 步的「可选」标记去掉 → ①档与②档同时必红。
    #[test]
    fn funnel_approval_is_optional_not_a_mandatory_step() {
        for p in [Platform::Mac, Platform::Windows] {
            let funnel = wizard_steps(p)
                .into_iter()
                .find(|s| s.id == "funnel")
                .expect("两平台都必有 funnel 步");
            assert!(
                funnel.needs_human,
                "批准若出现仍需人点——needs_human 保持 true（{p:?}）"
            );
            assert!(
                funnel.human_optional,
                "批准步必须标记为「可能不出现」（{p:?}）——实测两平台各执一端"
            );
            assert!(
                !funnel.human_action_key.is_empty(),
                "可选人工步同样要能说清要人做什么（{p:?}）"
            );
        }
        // 探测范围内零『必需』人工点击（Windows 实测）
        let probe_scope = ["shields_up", "funnel", "verify", "autostart"];
        let mandatory: Vec<&str> = wizard_steps(Platform::Windows)
            .iter()
            .filter(|s| probe_scope.contains(&s.id) && s.needs_human && !s.human_optional)
            .map(|s| s.id)
            .collect();
        assert!(
            mandatory.is_empty(),
            "Windows 实测探测范围内零次人工点击，不得登记为必需人工步: {mandatory:?}"
        );
        // 人工确认数按平台如实登记 + 仍满足 §C2 出口标准（macOS ≤4 / Windows ≤3）
        assert_eq!(
            human_step_counts(Platform::Mac),
            (3, 1),
            "macOS：系统扩展/安装/登录 3 步必需 + funnel 批准 1 步可选（实测出现过）"
        );
        assert_eq!(
            human_step_counts(Platform::Windows),
            (2, 1),
            "Windows：安装(UAC)/登录 2 步必需 + funnel 批准 1 步可选（实测未出现）"
        );
        for (p, bound) in [(Platform::Mac, 4usize), (Platform::Windows, 3usize)] {
            let (req, opt) = human_step_counts(p);
            assert!(
                req + opt <= bound,
                "{p:?} 人工确认数 {req}+{opt} 超出 §C2 出口标准 {bound}"
            );
        }
    }

    /// 安装包资产表：平台齐备且 sha256 是 64 位十六进制（防手滑粘错）。
    #[test]
    fn installer_asset_table_is_complete_and_well_formed() {
        for p in [Platform::Mac, Platform::Windows] {
            let asset = ts_asset_name(p);
            let url = ts_download_url(p);
            assert!(
                url.starts_with("https://pkgs.tailscale.com/stable/"),
                "只允许官方源"
            );
            assert!(
                url.ends_with(asset),
                "URL 末段必须等于资产名（与 tunnel::download_url_for 同约定）"
            );
            assert!(
                url.ends_with(".pkg") || url.ends_with(".msi"),
                "macOS=.pkg / Windows=.msi"
            );
            // 实测锚点：照索引页顶部"最新版"(1.102.5) 拼 URL 会 404——那是静态 Linux 包的版本
            assert!(!url.contains("1.102.5"), "Windows 1.102.5 实测 404");
            let h = ts_expected_sha256(asset).expect("必须有固定校验值");
            assert_eq!(h.len(), 64, "sha256 必须 64 位");
            assert!(
                h.chars().all(|c| c.is_ascii_hexdigit()),
                "sha256 必须是十六进制"
            );
        }
        // 未知资产必须 Err（不返回空串——那是 fail-open）
        assert!(ts_expected_sha256("nope.msi").is_err());
    }

    /// 下载/校验复用（不新写校验）：错误的哈希必须被 `tunnel::verify_sha256` 拒绝
    /// （沿用 tunnel.rs HELLO_SHA 手法——校验单点在 tunnel，本模块只调用）
    #[test]
    fn installer_hash_rejects_wrong_bytes() {
        assert!(crate::remote::tunnel::verify_sha256(b"hello", "00".repeat(32).as_str()).is_err());
    }

    /// 修复轮 2 指针②：CLI 失败且 stderr 为空 → 错误文案必须带命令上下文与退出码，
    /// 绝不 `Err("")`（前端只见空 toast 无从排查）；stderr 非空原样透出
    #[test]
    fn cli_error_falls_back_to_exit_code_and_args_when_stderr_blank() {
        assert_eq!(cli_error(&["status"], "boom\n", Some(1)), "boom");
        let e = cli_error(&["funnel", "--bg", "9420"], "  \n", Some(2));
        assert!(
            e.contains("funnel --bg 9420") && e.contains("2"),
            "空 stderr 必须回落命令上下文 + 退出码: {e}"
        );
        let e = cli_error(&["get", "--json"], "", None);
        assert!(
            e.contains("get --json") && e.contains("未知"),
            "退出码取不到时如实写「未知」: {e}"
        );
    }

    /// 批准链接捕获（纯函数）：取首个「https:// 开头且含 tailscale.com」的词并剥
    /// 句尾标点；无关域名与空文本不误报
    #[test]
    fn extract_approval_url_finds_tailscale_link_and_trims_punctuation() {
        assert_eq!(
            extract_approval_url(
                "Funnel is pending approval.\nTo approve, visit: https://login.tailscale.com/a/xyz."
            )
            .as_deref(),
            Some("https://login.tailscale.com/a/xyz")
        );
        // 非 tailscale.com 的 https 词被跳过，命中第一个满足条件者
        assert_eq!(
            extract_approval_url("see https://example.com/foo then https://docs.tailscale.com/kb)")
                .as_deref(),
            Some("https://docs.tailscale.com/kb")
        );
        assert_eq!(extract_approval_url("https://example.com/only"), None);
        assert_eq!(extract_approval_url(""), None);
        assert_eq!(extract_approval_url("no links here"), None);
    }

    /// shields-up 写入回读确认（写入侧尚未实测，探测提示词第 6 步）：写后 `get
    /// --json` 回读——false 才算成功；仍为开 / 读不到字段 / get 失败一律如实 Err，
    /// 绝不把「写没写进去不知道」伪装成成功
    #[test]
    fn disable_shields_up_verifies_by_read_back() {
        let _g = test_lock();
        // 回读 false → Ok
        set_run_cli_override(Some(Box::new(|args: &[&str]| match args {
            ["set", "--shields-up=false"] => Ok(String::new()),
            ["get", "--json"] => Ok(r#"{"shields-up": false}"#.into()),
            other => Err(format!("测试未应答的 CLI 调用: {other:?}")),
        })));
        assert!(disable_shields_up().is_ok());
        // 回读仍为 true → Err「写入未生效」
        set_run_cli_override(Some(Box::new(|args: &[&str]| match args {
            ["set", "--shields-up=false"] => Ok(String::new()),
            ["get", "--json"] => Ok(r#"{"shields-up": true}"#.into()),
            other => Err(format!("测试未应答的 CLI 调用: {other:?}")),
        })));
        let e = disable_shields_up().unwrap_err();
        assert!(e.contains("仍为开启"), "{e}");
        // 回读不到字段 → Err（不猜）
        set_run_cli_override(Some(Box::new(|args: &[&str]| match args {
            ["set", "--shields-up=false"] => Ok(String::new()),
            ["get", "--json"] => Ok(r#"{}"#.into()),
            other => Err(format!("测试未应答的 CLI 调用: {other:?}")),
        })));
        let e = disable_shields_up().unwrap_err();
        assert!(e.contains("shields-up"), "{e}");
        set_run_cli_override(None);
    }

    /// 逐步探测真值表（判据表 §C2）：每步都要能说出「怎么算完成」——全部就绪时
    /// 除 verify（Task 7 占位）外全 done；CLI 读不到时 fail-closed + blocked_reason
    /// 如实说明，绝不把读不到伪装成完成或卡死
    ///
    /// **平台门控（2026-10-07 存量债清理，Linux CI 门禁）**：本用例的期望步骤表
    /// **平台感知地取自 `wizard_steps(current_platform())`**——2026-10-13 修：原硬编码
    /// 期望清单无条件含 `sys_ext`（macOS 独有步），与「Windows 步骤表恰少该步」相矛盾
    /// （Windows 上必以「缺步骤 sys_ext」失败）；现与生产及
    /// `windows_has_exactly_one_fewer_step_than_macos` 用例同源，不再另立第三份清单。
    /// 一键配置向导只对 macOS/Windows 实现（产品只发这两个平台），而
    /// `current_platform()` 在 Linux 返回 `Platform::Other`，不在支持面内，故按平台门控。
    /// **不是静默跳过**：Linux 上它在 `cargo test` 里以 "N ignored" 可见，
    /// 并可用 `--ignored` 显式跑。
    #[cfg_attr(
        not(any(target_os = "macos", target_os = "windows")),
        ignore = "平台不适用：步骤表按平台给，sys_ext 只对 macOS 存在（current_platform() 在 Linux = Platform::Other）"
    )]
    #[test]
    fn probe_steps_marks_each_criterion_honestly() {
        let _g = test_lock();
        let state_of = |v: &[StepState], id: &str| -> StepState {
            v.iter()
                .find(|s| s.id == id)
                .unwrap_or_else(|| panic!("缺步骤 {id}"))
                .clone()
        };
        // 场景一：全部就绪（Running + shields 关 + Funnel 活且指向本机 兔维斯 端口）
        // 校验态是全局：先钉回 Unverified，隔离兄弟用例崩溃时泄漏的状态
        set_reachability(Reachability::Unverified);
        const OURS: &str =
            r#"{"Web":{"x.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:9420"}}}}}"#;
        set_run_cli_override(Some(Box::new(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_RUNNING.into()),
            ["get", "--json"] => Ok(r#"{"shields-up": false}"#.into()),
            ["funnel", "status", "--json"] => Ok(OURS.into()),
            other => Err(format!("测试未应答的 CLI 调用: {other:?}")),
        })));
        let v = probe_steps_from(9420, true, &read_cli_readings());
        for id in [
            "detect",
            "install",
            "download",
            "login",
            "shields_up",
            "funnel",
        ] {
            let s = state_of(&v, id);
            assert!(s.done, "{id} 应判完成: {s:?}");
            assert_eq!(s.blocked_reason, None, "{id} 就绪时不得报卡住: {s:?}");
        }
        // verify = Task 7 占位：恒未完成（不谎报），且非卡住（无 reason）
        let verify = state_of(&v, "verify");
        assert!(
            !verify.done && verify.blocked_reason.is_none(),
            "{verify:?}"
        );
        // autostart = 兔维斯 自身行为，恒 done
        assert!(state_of(&v, "autostart").done);
        // macOS：sys_ext 判据 = 可解析且 BackendState != Stopped → Running 命中
        #[cfg(target_os = "macos")]
        assert!(state_of(&v, "sys_ext").done);
        teardown_globals();

        // 场景二：CLI 完全读不到（未安装）→ fail-closed：各步未完成且带 blocked_reason，
        // 「待做」与「卡住」可区分（login/shields 等给出原因而非静默 false）
        set_run_cli_override(Some(Box::new(|args: &[&str]| {
            Err(format!("未检测到 Tailscale（尚未安装）: {args:?}"))
        })));
        let v = probe_steps_from(9420, false, &read_cli_readings());
        for id in ["detect", "install"] {
            let s = state_of(&v, id);
            assert!(!s.done, "{id} 未装不得判完成");
            assert_eq!(s.blocked_reason, None, "{id} 是待做不是卡住: {s:?}");
        }
        // 读不到须如实报「卡住 + 原因」的候选步里，期望清单按平台从 wizard_steps 取
        // （单一事实源）：Windows 比 macOS 恰少 sys_ext 一步（与
        // windows_has_exactly_one_fewer_step_than_macos 锁死的平台差异一致），不在此
        // 再立第三份硬编码清单
        let cli_blind: Vec<&str> = wizard_steps(current_platform())
            .iter()
            .map(|s| s.id)
            .filter(|id| ["sys_ext", "login", "shields_up", "funnel"].contains(id))
            .collect();
        for id in cli_blind {
            let s = state_of(&v, id);
            assert!(!s.done, "{id} 读不到状态不得判完成");
            assert!(
                s.blocked_reason
                    .as_deref()
                    .unwrap_or_default()
                    .contains("读不到"),
                "{id} 必须如实说明读不到: {s:?}"
            );
        }
        teardown_globals();

        // 场景三：待登录（NeedsLogin）→ login 未完成但**不报卡住**（等用户动作，
        // 动作文案上墙）；场景四：外来 serve 配置占用 → funnel 未完成且 blocked 点名占用
        set_run_cli_override(Some(Box::new(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_NEEDS_LOGIN.into()),
            ["get", "--json"] => Ok(r#"{"shields-up": false}"#.into()),
            ["funnel", "status", "--json"] => Ok("{}".into()),
            other => Err(format!("测试未应答的 CLI 调用: {other:?}")),
        })));
        let v = probe_steps_from(9420, true, &read_cli_readings());
        let login = state_of(&v, "login");
        assert!(!login.done && login.blocked_reason.is_none(), "{login:?}");
        teardown_globals();

        const FOREIGN: &str =
            r#"{"Web":{"x.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8080"}}}}}"#;
        set_run_cli_override(Some(Box::new(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_RUNNING.into()),
            ["get", "--json"] => Ok(r#"{"shields-up": false}"#.into()),
            ["funnel", "status", "--json"] => Ok(FOREIGN.into()),
            other => Err(format!("测试未应答的 CLI 调用: {other:?}")),
        })));
        let v = probe_steps_from(9420, true, &read_cli_readings());
        let funnel = state_of(&v, "funnel");
        assert!(
            !funnel.done
                && funnel
                    .blocked_reason
                    .as_deref()
                    .unwrap_or_default()
                    .contains("占用"),
            "外来配置在先：funnel 对 兔维斯 不算完成且须点名占用: {funnel:?}"
        );
        teardown_globals();
    }

    /// 向导载荷装配：platform/windowsVerified/steps/states 形状契约 + steps 与 states
    /// 按 id 一一对齐 + authUrl 透出（NeedsLogin 形态）
    #[test]
    fn wizard_status_payload_aligns_steps_with_states() {
        let _g = test_lock();
        set_run_cli_override(Some(Box::new(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_NEEDS_LOGIN.into()),
            ["get", "--json"] => Ok(r#"{"shields-up": true}"#.into()),
            ["funnel", "status", "--json"] => Ok("{}".into()),
            other => Err(format!("测试未应答的 CLI 调用: {other:?}")),
        })));
        let p = wizard_status_with(9420, true);
        assert!(
            p["enabled"].is_boolean(),
            "通道意图开关须随 probe 返回: {p}"
        );
        assert!(p["configKey"]
            .as_str()
            .unwrap_or_default()
            .starts_with("sha256:"));
        assert!(p["waiting"].is_null(), "没有 DNS 等待时返回 null: {p}");
        let expected_platform = if cfg!(target_os = "macos") {
            "mac"
        } else if cfg!(target_os = "windows") {
            "windows"
        } else {
            "other"
        };
        assert_eq!(p["platform"], expected_platform);
        // I-3 → ②（2026-10-07 用户实机确认）：Windows 验证位仍**按覆盖面如实派生**，
        // 但覆盖面已经**从第一步起**了——用户在本机 Windows 上卸载 Tailscale 后**从零
        // 走完 兔维斯 向导全程**（下载 → 安装 UAC → 登录 → 关 shields-up → 开通 Funnel →
        // 可达性校验），全程正常，故 detect/download/install/login 四步的**流程**已被
        // 端到端实机跑过（此前记录的 shields_up 起点是更早一次、从第 6 步起的探测）。
        // 验证位仍然由清单派生（不是硬编码 true）——机制与派生关系见下方
        // `windows_verification_list_is_derived_from_coverage_start`。
        let win = windows_verification_for(Platform::Windows);
        assert_eq!(
            win["windowsVerified"], true,
            "用户 2026-10-07 实机走完 兔维斯 向导全程 ⇒ 整条 Windows 流程已实测: {win}"
        );
        assert_eq!(
            win["windowsVerifiedFrom"], "detect",
            "实测覆盖起点 = 步骤表第一步（清单因此为空）: {win}"
        );
        assert_eq!(
            win["windowsUnverifiedSteps"],
            serde_json::json!([]),
            "已无未实测步骤（清单随起点派生，表变了跟着变）: {win}"
        );
        // 本机平台（测试机上 = mac）：无 Windows 行 → 不声称任何 Windows 覆盖
        let here = windows_verification_for(current_platform());
        assert_eq!(p["windowsVerified"], here["windowsVerified"]);
        assert_eq!(p["windowsVerifiedFrom"], here["windowsVerifiedFrom"]);
        assert_eq!(p["windowsUnverifiedSteps"], here["windowsUnverifiedSteps"]);
        let steps = p["steps"].as_array().unwrap();
        let states = p["states"].as_array().unwrap();
        assert_eq!(steps.len(), states.len(), "steps 与 states 必须一一对应");
        for (s, st) in steps.iter().zip(states.iter()) {
            assert_eq!(s["id"], st["id"], "id 对齐: {s} vs {st}");
        }
        // A1：可选人工步必须随载荷透出（前端据此把批准渲染成「若出现才需点」）
        let funnel = steps
            .iter()
            .find(|s| s["id"] == "funnel")
            .expect("必有 funnel 步");
        assert_eq!(funnel["needsHuman"], true);
        assert_eq!(
            funnel["humanOptional"], true,
            "批准步必须标记为『可能不出现』——不得让前端以为在等批准"
        );
        let autostart = steps
            .iter()
            .find(|s| s["id"] == "autostart")
            .expect("必有 autostart 步");
        assert_eq!(
            autostart["humanOptional"], false,
            "自动步不得标成可选人工步"
        );
        // A1：人工步骤数随载荷透出（由步骤表派生，不写死）——平台注册值可被断言
        let (req, opt) = human_step_counts(current_platform());
        assert_eq!(p["humanSteps"]["required"], req);
        assert_eq!(p["humanSteps"]["optional"], opt);
        assert_eq!(steps.len(), wizard_steps(current_platform()).len());
        // NeedsLogin 形态：authUrl 透传给前端「去登录」按钮
        assert_eq!(
            p["authUrl"], "https://login.tailscale.com/a/abc123",
            "待登录授权链接必须透出（兔维斯 不代登录，只递链接）"
        );
        // M4（2026-10-07 评审）：每个 state 都带 **blockedTone**（rose|amber）——非恢复窗口
        // 的一切卡点都是真故障档
        for st in states {
            if st["blockedReason"].is_null() {
                continue;
            }
            assert_eq!(
                st["blockedTone"], "rose",
                "非恢复窗口的卡点必须是故障档（rose）: {st}"
            );
        }
        teardown_globals();
    }

    /// **② 诚实标注的机制仍在，且三位都由「实测覆盖起点」派生**（2026-10-07 用户实机
    /// 确认后：起点前移到第一步 ⇒ 清单为空 ⇒ `windowsVerified = true`，黄标随之撤下）。
    ///
    /// 为什么要有这条：本轮改的是**事实**（Windows 全流程已被实机跑过），不是**机制**
    /// ——`8e10e12` 那批「诚实标注」的资产（起点常量 + 派生清单 + 两个位 + 前端弱提示）
    /// 必须留着，将来若又出现未实测的段落（新步骤 / 新平台形态），把起点挪回那一步即可
    /// （清单与两个位自动跟着变）。本测试用**起点参数化**证明机制活着：
    /// 变异：① 把 `windows_unverified_steps` 改成恒返回空表（删掉派生）→ 下方
    /// `from("shields_up", …)` 的清单断言必红；② 把 `windowsVerified` 改成硬编码 true
    /// → 同一条断言必红（清单非空却声称整条验过）；③ 把 `WINDOWS_VERIFIED_FROM` 改成
    /// 非第一步而仍声称整条流程已验 → 最后一条断言必红。
    #[test]
    fn windows_verification_list_is_derived_from_coverage_start() {
        // 起点一旦后移，那四步自动回到未验清单——正是 8e10e12 建立的机制
        let past = windows_verification_for_from("shields_up", Platform::Windows);
        assert_eq!(
            past["windowsUnverifiedSteps"],
            serde_json::json!(["detect", "download", "install", "login"]),
            "起点后移 ⇒ 起点之前的步骤自动点名（机制必须仍然生效）: {past}"
        );
        assert_eq!(
            past["windowsVerified"], false,
            "清单非空 ⇒ 整条流程不算验过（位由清单派生，不得硬编码 true）: {past}"
        );
        // 生产起点 = 步骤表第一步 ⇒ 起点之前没有任何步骤 ⇒ 清单为空 ⇒ 位为 true
        assert_eq!(
            WINDOWS_VERIFIED_FROM,
            wizard_steps(Platform::Windows)
                .first()
                .expect("Windows 步骤表非空")
                .id,
            "「整条 Windows 流程已实测」的充要条件 = 覆盖起点就是步骤表第一步"
        );
        assert_eq!(
            windows_unverified_steps(Platform::Windows),
            Vec::<&str>::new(),
            "本次生产口径：无未实测步骤"
        );
        // 非 Windows 平台：清单恒空、起点恒 null（不得张冠李戴）
        let other = windows_verification_for_from("shields_up", Platform::Other);
        assert_eq!(other["windowsUnverifiedSteps"], serde_json::json!([]));
        assert!(other["windowsVerifiedFrom"].is_null(), "{other}");
    }

    // ============================================================
    // ⑤ 登录步：兔维斯 不代登录，但**必须主动把链接取来**（2026-10-07 用户实测诊断）
    // ============================================================
    //
    // 根因（读码 + 注入缝复现，见 STATUS_NEEDS_LOGIN_NO_URL 的注释）：新装机器上
    // `status --json` 是 `NeedsLogin ∧ AuthURL=""`——授权链接要**发起一次交互式登录**
    // 才由尾网生成，而全仓 `run_cli` 调用点里**从来没有 `up` / `login`**（只有
    // status / get / funnel status / funnel --bg / set / funnel reset），兔维斯 从不发起
    // ⇒ 链接永远不出现；前端又只在 `probe.authUrl` 非空时才渲染链接 ⇒ 登录步无可点之物。
    // 修法（**不越合规红线**：只递链接、不代登录、不持凭据）：后台发起 `tailscale login`
    //（与 GUI「Log in」按钮同义，**只让尾网生成授权链接**）+ 有界轮询 status 取 AuthURL。
    //
    // 注入缝 = (读状态, 发起登录, 等待)：测试零进程零睡眠，且能把「有界」断言到底。

    /// 读状态序列（测试替身：按调用次数吐预置序列，越界后一直吐最后一个）
    fn status_reader_seq(seq: Vec<&'static str>) -> impl Fn() -> Result<TsStatus, String> {
        let idx = std::cell::Cell::new(0usize);
        move || {
            let i = idx.get();
            idx.set(i + 1);
            let raw = seq.get(i).copied().unwrap_or_else(|| *seq.last().unwrap());
            parse_status(raw)
        }
    }

    /// 空 URL 的待登录态（新装形态）→ 一次发起 → 轮询拿到链接：**这就是用户实测的那条路**
    #[test]
    fn login_step_actively_requests_link_and_returns_it() {
        let reads = std::cell::Cell::new(0usize);
        let seq = status_reader_seq(vec![
            STATUS_NEEDS_LOGIN_NO_URL,
            STATUS_NEEDS_LOGIN_NO_URL,
            STATUS_NEEDS_LOGIN,
        ]);
        let triggered = std::cell::Cell::new(0usize);
        let waited = std::cell::RefCell::new(Vec::new());
        let r = login_step_with(
            || {
                reads.set(reads.get() + 1);
                seq()
            },
            || {
                triggered.set(triggered.get() + 1);
                Ok(())
            },
            |d| waited.borrow_mut().push(d),
            |_| {},
        )
        .expect("登录步应成功返回");
        assert_eq!(
            triggered.get(),
            1,
            "AuthURL 为空时必须**发起一次**登录尝试（否则链接永远不出现）: {r}"
        );
        assert_eq!(
            r["authUrl"], "https://login.tailscale.com/a/abc123",
            "拿到链接必须原样递出（兔维斯 只递链接，不代登录）: {r}"
        );
        assert_eq!(r["done"], false, "还没登录完，不得报 done: {r}");
        assert!(
            reads.get() >= 3,
            "必须轮询（第一次读到空 → 发起 → 继续读），实际读了 {} 次: {r}",
            reads.get()
        );
        // 有界：注入的等待总时长不得超过轮询窗上限
        let total: std::time::Duration = waited.borrow().iter().sum();
        assert!(
            total <= LOGIN_LINK_POLL_WINDOW,
            "轮询必须有界（≤{:?}），实际累计等了 {total:?}",
            LOGIN_LINK_POLL_WINDOW
        );
    }

    /// 幂等：已经有链接时**不得**再发起登录尝试（用户可能正拿着那条链接在浏览器里操作）
    #[test]
    fn login_step_does_not_trigger_when_link_is_already_present() {
        let triggered = std::cell::Cell::new(0usize);
        let r = login_step_with(
            status_reader_seq(vec![STATUS_NEEDS_LOGIN]),
            || {
                triggered.set(triggered.get() + 1);
                Ok(())
            },
            |_| {},
            |_| {},
        )
        .expect("登录步应成功返回");
        assert_eq!(triggered.get(), 0, "已有链接 → 不发起（幂等）: {r}");
        assert_eq!(r["authUrl"], "https://login.tailscale.com/a/abc123");
    }

    /// 已登录（Running）：无需链接、不发起任何东西
    #[test]
    fn login_step_reports_done_without_triggering_when_already_running() {
        let reads = std::cell::Cell::new(0usize);
        let triggered = std::cell::Cell::new(0usize);
        let r = login_step_with(
            || {
                reads.set(reads.get() + 1);
                parse_status(STATUS_RUNNING)
            },
            || {
                triggered.set(triggered.get() + 1);
                Ok(())
            },
            |_| {},
            |_| {},
        )
        .expect("登录步应成功返回");
        assert_eq!(r["done"], true, "Running = 已登录完成: {r}");
        assert_eq!(r["authUrl"], "");
        assert_eq!(reads.get(), 1, "已登录 ⇒ 读一次就够，不该轮询: {r}");
        assert_eq!(triggered.get(), 0);
    }

    /// 链接始终不出现：**有界放弃**并如实回报（不许无限轮询、不许谎报有链接）
    #[test]
    fn login_step_gives_up_bounded_when_link_never_appears() {
        let reads = std::cell::Cell::new(0usize);
        let triggered = std::cell::Cell::new(0usize);
        let waited = std::cell::RefCell::new(Vec::new());
        let r = login_step_with(
            || {
                reads.set(reads.get() + 1);
                parse_status(STATUS_NEEDS_LOGIN_NO_URL)
            },
            || {
                triggered.set(triggered.get() + 1);
                Ok(())
            },
            |d| waited.borrow_mut().push(d),
            |_| {},
        )
        .expect("登录步应成功返回（拿不到链接不是错误，是「还没生成」）");
        assert_eq!(triggered.get(), 1, "仍要发起（下一次可能就拿到了）: {r}");
        assert_eq!(r["authUrl"], "", "拿不到就如实给空串，不编链接: {r}");
        assert_eq!(r["triggered"], true, "回执要如实说明「已发起」: {r}");
        // 有界：读次数 = 1（首次）+ 轮询步数；等待累计 = 轮询窗
        assert_eq!(
            reads.get(),
            1 + LOGIN_LINK_POLL_DELAYS_MS.len(),
            "轮询次数必须固定（有界），实际读了 {} 次",
            reads.get()
        );
        let total: std::time::Duration = waited.borrow().iter().sum();
        assert!(
            total <= LOGIN_LINK_POLL_WINDOW,
            "累计等待 {total:?} 超过窗口 {:?}",
            LOGIN_LINK_POLL_WINDOW
        );
        // note 如实说明成因（中文硬编码、前端不渲染——消费方约定见 src/lib/api/remote.ts）
        assert!(
            r["note"].as_str().unwrap_or_default().contains("没有拿到"),
            "必须给出成因说明: {r}"
        );
    }

    /// 读状态失败 = Err（fail-closed：绝不拿"读不到"当"已登录/已拿到链接"）
    #[test]
    fn login_step_fails_closed_when_status_unreadable() {
        let triggered = std::cell::Cell::new(0usize);
        let e = login_step_with(
            || Err("未检测到 Tailscale（尚未安装）".to_string()),
            || {
                triggered.set(triggered.get() + 1);
                Ok(())
            },
            |_| {},
            |_| {},
        )
        .expect_err("读不到状态必须 Err");
        assert!(e.contains("尚未安装"), "{e}");
        assert_eq!(triggered.get(), 0, "状态都读不到就不该去发起登录: {e}");
    }

    /// 发起登录失败（CLI 调不起来）= 如实 Err（不静默吞、也不谎报有链接）
    #[test]
    fn login_step_reports_trigger_failure() {
        let e = login_step_with(
            status_reader_seq(vec![STATUS_NEEDS_LOGIN_NO_URL]),
            || Err("启动 tailscale login 失败: 权限不足".to_string()),
            |_| {},
            |_| {},
        )
        .expect_err("发起失败必须 Err");
        assert!(e.contains("权限不足"), "{e}");
    }

    // ============================================================
    // I1（2026-10-08 架构评审）：登录子进程的**生命周期**——有界 + 单飞 + 窗尽/退出收尸
    // ============================================================
    //
    // 评审三条：① 只 spawn + 收尸线程 ⇒ 每次点击泄漏 1 进程 + 1 阻塞线程；② `login` 的
    // `--timeout` 默认 0s = 一直等 ⇒ 用户不登录它就一直活着；③ 轮询失败后无清理 ⇒ 兔维斯
    // 退出后成孤儿进程（本仓纪律：不留孤儿进程）。修法 = 评审给的 a+b：`--timeout 15s`
    // 有界 + 模块级 `Mutex<Option<Child>>` 单飞 + 窗尽/应用退出 kill + wait。
    //
    // **杀与 AuthURL 的关系待真机复核**（本机无法验证 Windows 行为，见 LoginCleanup 的
    // 注释）：故杀法**保守**——只在"轮询窗已尽**且拿不到链接**"时杀，拿到链接一律不杀。

    /// 窗尽收尾的决策表（纯函数）——保守口径逐格锁死
    #[test]
    fn login_cleanup_decision_is_conservative() {
        assert_eq!(
            login_cleanup_decision(true, false),
            LoginCleanup::Keep,
            "已登录（Running）：CLI 自己的等待条件已满足，它会自然退出——不需要我们杀"
        );
        assert_eq!(
            login_cleanup_decision(false, true),
            LoginCleanup::Keep,
            "**拿到链接 ⇒ 绝不杀**：杀掉等待者会不会让已生成的 AuthURL 失效，本机无法验证\n\
             Windows 行为 ⇒ 保守处理（登记为待真机复核）"
        );
        assert_eq!(
            login_cleanup_decision(false, false),
            LoginCleanup::Kill,
            "窗尽且没拿到链接：再等下去也不会有链接（默认 0s 会一直等）⇒ 必须杀 + 收尸，\n\
             否则 兔维斯 退出后就是一个孤儿进程"
        );
    }

    /// 收尾决策**接进登录步**（不是只写在抽屉里的纯函数）：拿不到链接 → Kill；
    /// 拿到链接 → Keep。变异：把 login_step_with 尾部的 cleanup 调用删掉/改成恒 Keep → 必红。
    #[test]
    fn login_step_settles_waiter_conservatively() {
        let decisions = std::cell::RefCell::new(Vec::new());
        // ① 轮询窗耗尽、始终没有链接 → Kill
        login_step_with(
            status_reader_seq(vec![STATUS_NEEDS_LOGIN_NO_URL]),
            || Ok(()),
            |_| {},
            |d| decisions.borrow_mut().push(d),
        )
        .expect("拿不到链接不是错误");
        assert_eq!(
            decisions.borrow().as_slice(),
            [LoginCleanup::Kill],
            "窗尽且无链接必须收掉等待者（否则它按默认 0s 一直等）"
        );
        // ② 轮询中拿到链接 → Keep（保守：不杀）
        decisions.borrow_mut().clear();
        login_step_with(
            status_reader_seq(vec![STATUS_NEEDS_LOGIN_NO_URL, STATUS_NEEDS_LOGIN]),
            || Ok(()),
            |_| {},
            |d| decisions.borrow_mut().push(d),
        )
        .expect("拿到链接应成功返回");
        assert_eq!(
            decisions.borrow().as_slice(),
            [LoginCleanup::Keep],
            "拿到链接 ⇒ 不杀（AuthURL 是否随之失效待真机复核）"
        );
    }

    /// **合规红线自查清单**（凭据类 + 偏好类旗标前缀）：登录发起**一个都不许带**。
    /// 两个测试共用同一份清单——常量内容白名单（`login_attempt_args_are_a_closed_whitelist`）
    /// 与**实际 argv** 行为断言（`login_attempt_command_carries_only_the_whitelist_argv`）
    /// 各查一遍，避免两处清单漂移。
    const LOGIN_FORBIDDEN_FLAGS: &[&str] = &[
        "--auth-key",
        "--client-secret",
        "--id-token",
        "--shields-up",
        "--advertise-routes",
        "--advertise-exit-node",
        "--accept-routes",
        "--exit-node",
        "--hostname",
        "--login-server",
        "--operator",
        "--reset",
    ];

    /// **一切"往命令里加参数"的调用形态**（方法态 + 关联函数态）——形态针靠它把
    /// "argv 只能从白名单来"从偏好变成可执行约束。
    ///
    /// 为什么逐个列全、而不是只写旧针的 `.arg(`：Windows `CommandExt` 的 `.raw_arg(` 与
    /// `Command::args(&mut cmd, …)` 这类**关联函数形态**都不含子串 `.arg(`，旧针漏掉它们
    /// （2026-10-08 收口轮实测：在调用点补 `.args(&["--auth-key", …])` 旧针**不红**）。
    const ARGV_ADDING_PATTERNS: &[&str] = &[
        ".arg(",
        ".args(",
        ".arg_os(",
        ".args_os(",
        ".raw_arg(",
        "::arg(",
        "::args(",
        "::arg_os(",
        "::args_os(",
        "::raw_arg(",
    ];

    /// **I2（2026-10-08 架构评审）：argv 是封闭白名单**——红线是「**不带任何凭据参数、
    /// 不碰偏好**」，而旧形态针只禁等待类调用、**没锁参数**（往 `login` 后面加
    /// `--auth-key` / `--shields-up=false` 不会变红）。本测试把白名单本身断言到底：
    /// - **被测对象是 `login_argv()`**（生产实际取参的那个纯函数），不是常量本身——
    ///   `assert_eq!(login_argv(), LOGIN_ARGS)` 把两者钉成同一个，改任何一边都红；
    /// - 除 `login` 外只允许 `--timeout <有界秒数>`（默认 0s = 一直等，正是 I1 要修的）；
    /// - 任何凭据 / 偏好旗标（`--auth-key` / `--shields-up` / `--advertise-*` / …）出现即红。
    ///
    /// 变异：往 LOGIN_ARGS（或 `login_argv()`）里加任何一个参数 → 必红。
    #[test]
    fn login_attempt_args_are_a_closed_whitelist() {
        let argv = login_argv();
        assert_eq!(
            argv, LOGIN_ARGS,
            "`login_argv()` 必须是白名单常量本身（argv 的唯一出处，两边不许分叉）"
        );
        assert_eq!(
            argv.len(),
            3,
            "argv 只允许 `login --timeout <n>s` 三条：{argv:?}"
        );
        assert_eq!(argv[0], "login", "只允许 login 子命令（up 语义不同）");
        assert_eq!(argv[1], "--timeout", "第二条只允许有界等待参数");
        // 有界：0s = 「blocks forever」（1.102.4 `login --help` 原文）⇒ 必须是有限秒数
        let secs: u64 = argv[2]
            .strip_suffix('s')
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("--timeout 必须是有界秒数（如 15s）: {argv:?}"));
        assert!(
            (1..=60).contains(&secs),
            "等待上限必须小而有界（1..=60 秒），实际 {secs}s"
        );
        // 红线自查（与上面的长度断言同源，但把「不许带什么」写成可读的清单）
        for &forbidden in LOGIN_FORBIDDEN_FLAGS {
            assert!(
                !argv.iter().any(|a| a.starts_with(forbidden)),
                "登录发起**不得**带 {forbidden}（合规红线：不带任何凭据参数、不碰偏好）"
            );
        }
    }

    /// **argv 的行为断言（2026-10-08 收口轮补强）**：直读生产**真正会 spawn 的那个
    /// `Command` 对象**的 `get_args()`——不是扫源码字符串。
    ///
    /// 起因（旧针被实测证伪）：旧形态针只断言 `body.contains(".args(LOGIN_ARGS)")` 且
    /// `!body.contains(".arg(")`，于是**在调用点再补一行 `.args(&["--auth-key", …])`
    /// 两个断言都不会红**（`.args(` 不含子串 `.arg(`；白名单测试只管常量本身、不管调用点）。
    /// 实测：加完变异后 `login_attempt_spawns_detached_and_never_waits` +
    /// `login_attempt_args_are_a_closed_whitelist` **双双通过**（4 passed / 0 failed）。
    ///
    /// 本测试查三层：① `login_argv()` == 白名单常量；② `login_command()` 造出的命令其
    /// **实际 argv 逐条等于**白名单（长度 + 顺序 + 内容，多一条即红）；③ 红线自查落在
    /// **实际 argv** 上（任何凭据/偏好旗标前缀出现即红）——"注释里说安全"变成"argv 被断言"。
    /// 变异：在 `login_command` 的 `.args(login_argv())` 之后再补
    /// `.args(&["--auth-key", "x"])` → ②③ 必红（实测报错见提交说明）。
    #[test]
    fn login_attempt_command_carries_only_the_whitelist_argv() {
        assert_eq!(
            login_argv(),
            LOGIN_ARGS,
            "argv 的唯一出处必须是白名单常量本身"
        );
        let bin = std::path::Path::new("tailscale"); // 只是占位路径：本测试不 spawn
        let cmd = login_command(bin);
        assert_eq!(
            cmd.get_program(),
            bin.as_os_str(),
            "程序名必须是被注入的 CLI 路径（不许换成别的可执行）"
        );
        let got: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let want: Vec<String> = LOGIN_ARGS.iter().map(|a| (*a).to_string()).collect();
        assert_eq!(
            got, want,
            "生产命令的**实际 argv** 必须逐条等于白名单（长度/顺序/内容）——多一条即触红线"
        );
        for &forbidden in LOGIN_FORBIDDEN_FLAGS {
            assert!(
                !got.iter().any(|a| a.starts_with(forbidden)),
                "生产命令的**实际 argv** 里出现了 {forbidden}\
                 （合规红线：不带任何凭据参数、不碰偏好）: {got:?}"
            );
        }
    }

    /// **argv 只有一个出处**（形态针，与上面的行为断言配对）：唯一拼装点 `login_command`
    /// 体内 argv 调用**恰好一处**，且必须是 `.args(login_argv())`——加第二处（哪怕加空数组）
    /// 即红；`.raw_arg(` 之类的关联/扩展形态也一并禁掉。
    /// 变异：在 `login_command` 里再加一行 `.args(...)` → 必红。
    #[test]
    fn login_command_assembles_argv_only_from_the_whitelist_source() {
        let src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/remote/tailscale/wizard.rs"
        ))
        .expect("读 wizard.rs");
        let start = src
            .find("fn login_command(")
            .expect("argv 的唯一拼装点必须存在（spawn_login_attempt 调它）");
        let body = &src[start..];
        let end = body.find("\n}\n").expect("函数体结束");
        let body = &body[..end];
        assert!(
            body.contains(".args(login_argv())"),
            "唯一拼装点必须从 `login_argv()`（白名单纯函数）**整条**取参: {body}"
        );
        for &pat in ARGV_ADDING_PATTERNS {
            // 只允许 `.args(login_argv())` 那一处；其余形态一次都不许出现
            let expected = usize::from(pat == ".args(");
            assert_eq!(
                body.matches(pat).count(),
                expected,
                "拼装点里 `{pat}` 的出现次数不对（只允许 `.args(login_argv())` 这一处）——\
                 多一处就可能就地拼参数（合规红线）: {body}"
            );
        }
    }

    /// **不许同步阻塞**（本项目刚修过 `wait_child_bounded` 那类「命令阻塞把 UI 挂死」）：
    /// 生产发起入口 `spawn_login_attempt` 只能 spawn，**不得**出现任何等待/收取输出的调用。
    /// 形态针（与既有 `run_cli(&["funnel", "reset"])` 调用形态针同一手法）：
    /// 变异：把 `spawn_login_attempt` 里的 `spawn()` 换成 `output()` / 加 `wait()` / 改走
    /// `run_cli`（内部会 `wait_child_bounded`）→ 本测试必红。
    ///
    /// **I2 追加（2026-10-08 架构评审）：形态针必须连 argv 一起锁。** 旧针只禁等待类调用，
    /// 往登录命令后面加 `--auth-key` / `--shields-up=false` **不会变红**——而这条命令的合规
    /// 红线恰恰是「不带任何凭据参数、不碰偏好」。
    ///
    /// **2026-10-08 收口轮补强（旧针再次被实测证伪）**：旧针的 argv 判据是
    /// `body.contains(".args(LOGIN_ARGS)")` 且 `!body.contains(".arg(")`——**在调用点再补一行
    /// `.args(&["--auth-key", "x"])` 两个断言都不会红**（`.args(` 不含子串 `.arg(`；白名单
    /// 测试只管常量本身、不管调用点）。实测：加完变异后本测试 + 白名单测试 **4 passed**。
    /// 现在改成"调用点不许自己拼 argv"：
    /// - spawn 点必须**逐字**调用唯一拼装点 `login_command(&bin)`（签名/实参一变即不匹配）；
    /// - 本函数体内**不得出现任何 argv 拼装形态**（[`ARGV_ADDING_PATTERNS`]：`.arg(` /
    ///   `.args(` / `.args_os(` / `.raw_arg(` 及关联函数形态）——旧判据 `.arg(` 是其中之一，
    ///   **没有削弱**；
    /// - argv 的**内容**由行为断言 `login_attempt_command_carries_only_the_whitelist_argv`
    ///   直读真实 `Command::get_args()` 锁死（不再依赖源码扫描）。
    ///
    /// 变异：在 `spawn_login_attempt` 里加 `.args(&["--auth-key", "x"])` → 必红（实测报错见
    /// 提交说明）；把 `login_command(&bin)` 换成任何别的东西 → 必红。
    ///
    /// **2026-10-08 评审 Important #2（TOCTOU）**：单飞门从「reap / 判空 / 登记三次各自
    /// 取锁」收紧为**一锁到底**——判空与 spawn/登记之间不许再有放锁窗口（旧形态两个并发
    /// `run_step("login")` 可同时通过判空 ⇒ 双 spawn、第二次登记覆盖第一次句柄）。判据
    /// 同步改为：体内取 `login_child_lock()` 守卫、判空看 `slot.is_some()`、登记走
    /// `adopt_login_child_locked(&mut slot, child)`（同一守卫）。变异：把判空改回免锁版
    /// `login_child_slot_is_empty()`、或把登记改回独立取锁 → 必红。入口接线由
    /// [`login_arm_dispatches_only_through_spawn_login_attempt`] 补锁（评审 Important #1）。
    #[test]
    fn login_attempt_spawns_detached_and_never_waits() {
        let src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/remote/tailscale/wizard.rs"
        ))
        .expect("读 wizard.rs");
        let start = src
            .find("fn spawn_login_attempt()")
            .expect("生产发起入口必须存在（run_step 的 login 臂传它）");
        let body = &src[start..];
        let end = body.find("\n}\n").expect("函数体结束");
        let body = &body[..end];
        assert!(
            body.contains(".spawn()"),
            "必须用 spawn 起进程（不等待）: {body}"
        );
        for forbidden in [
            ".output()",
            ".status()",
            "wait_child_bounded",
            "wait_timeout",
            "run_cli(",
        ] {
            assert!(
                !body.contains(forbidden),
                "发起登录**不得**同步阻塞（出现 {forbidden}）——tailscale login 默认会一直\n\
                 等用户在浏览器完成；同步等它=把向导挂死: {body}"
            );
        }
        // I1：spawn 出来的子进程必须**登记进单飞槽**（否则窗尽/退出时无从 kill + wait，
        // 就成了评审说的"每次点击泄漏 1 进程 + 1 阻塞线程、退出后成孤儿"）。
        // TOCTOU 修复：登记必须是**持同一守卫**的带锁版——登记与 spawn 同临界区
        // （旧形态 spawn 后放开锁、二次取锁再登记，两次取锁之间可被并发发起插队，
        // 第二次登记覆盖第一次的句柄）。
        assert!(
            body.contains("adopt_login_child_locked(&mut slot, child)"),
            "spawn 出来的 wait 者必须在**同一临界区**登记进单飞槽（I1 + TOCTOU：\n\
             窗尽/应用退出要能 kill + wait，且并发发起不得互相覆盖句柄）: {body}"
        );
        // I1：单飞门——「取锁 → 判空 → spawn → 登记」必须**一锁到底**（TOCTOU 修复）。
        // 旧形态三次取锁（reap/判空/登记各自取放），判空与登记之间放锁做 IO + spawn
        // ⇒ 两个并发发起可同时通过判空、双 spawn、第二次登记覆盖第一次句柄。
        // 变异：把判空改回免锁版 `login_child_slot_is_empty()`（放锁读）→ 本断言红。
        assert!(
            body.contains("let mut slot = login_child_lock();") && body.contains("slot.is_some()"),
            "单飞门必须持锁判空（`login_child_lock()` 取守卫后，在同一守卫上看 \
             `slot.is_some()`）——同一时刻至多一个 `tailscale login`: {body}"
        );
        // I2（2026-10-08 收口轮补强）：argv 一律经**唯一拼装点** `login_command`。
        // 调用逐字锁死——多传/少传实参、换成别的构造都会让这一行不匹配。
        assert!(
            body.contains("login_command(&bin)"),
            "spawn 点必须逐字调用 `login_command(&bin)`（argv 的唯一拼装点，\
             内容由 login_attempt_command_carries_only_the_whitelist_argv 断言）: {body}"
        );
        // 本函数体内**不得出现任何 argv 拼装**：旧判据 `.arg(` 只是其中一种形态，
        // 这里把方法态与关联函数态一并禁掉——`.args(&["--auth-key", …])` 这类追加在旧针下
        // 不红，正是本轮补上的缺口（实测见本测试文档）。
        for &pat in ARGV_ADDING_PATTERNS {
            assert!(
                !body.contains(pat),
                "spawn 点**不得**就地拼 argv（出现 {pat}）——argv 只能经 `login_command` / \
                 `login_argv` 从白名单来（合规红线：不带任何凭据参数、不碰偏好）: {body}"
            );
        }
        // 命令必须是 login（不是 up）：up 与 login 是两条不同的上游命令（up 是"连上网络
        // 并按需登录"，login 是"发起一次交互式登录"），本步语义只要后者 —— 事实版理由见
        // LOGIN_ARGS 与 spawn_login_attempt 的注释
        assert!(
            login_argv().contains(&"login"),
            "必须调 `tailscale login`（只发起交互式登录、让尾网生成授权链接）: {:?}",
            login_argv()
        );
    }

    // ============================================================
    // 红线守卫链的最后一环（2026-10-08 评审 Important #1）：入口接线
    // ============================================================

    /// **login 臂只能经 [`spawn_login_attempt`] 发起**：argv 三层守卫（常量内容断言 /
    /// 唯一拼装点形态针 / `get_args()` 行为断言）锁的全是**对象内部**（`login_command` /
    /// `spawn_login_attempt` 两个函数体），而消费它们的入口——`run_step` 的 login 臂——
    /// 此前没有任何测试锁。实测变异：把臂里的 `spawn_login_attempt` 换成就地拼装的
    /// `Command::new(bin).args(&["login","--timeout","15s","--auth-key",…]).spawn()`，
    /// 定向 138 个测试**全绿**（同时静默绕掉单飞槽、kill 路径与退出钩子——孤儿进程
    /// 回归也不红）。本形态针补上这一环：臂内不得出现任何 argv 拼装形态 / `Command::new` /
    /// 裸 `.spawn(`，发起只能以函数值 `spawn_login_attempt` 交给 `login_step_with` 编排；
    /// 臂内唯一允许的 CLI 调用是状态读（`run_cli` 是同步等待的，经它跑 `login` 是另一条
    /// 逃逸路——同步挂死向导）。
    /// 变异：把臂的 trigger 换成就地拼 argv + 裸 spawn → 必红（`.args(` / `Command::new` /
    /// `.spawn(` 任一出现即红）。
    #[test]
    fn login_arm_dispatches_only_through_spawn_login_attempt() {
        let src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/remote/tailscale/wizard.rs"
        ))
        .expect("读 wizard.rs");
        let start = src
            .find("\"login\" => login_step_with(")
            .expect("run_step 的 login 臂必须存在，且直接经 login_step_with 编排");
        let rest = &src[start..];
        let end = rest
            .find("\"shields_up\" =>")
            .expect("login 臂之后应衔接 shields_up 臂（截取臂体用）");
        let arm = &rest[..end];
        assert!(
            arm.contains("spawn_login_attempt,"),
            "login 臂的发起口只能是生产入口 `spawn_login_attempt`（函数值直传）——argv \
             白名单 / 单飞槽 / kill 路径全挂在那条链上，绕过即失控: {arm}"
        );
        // 读侧一并锁形：臂内恰好一条 run_cli 调用，且就是状态读
        assert_eq!(
            arm.matches("run_cli(&[").count(),
            1,
            "login 臂内 run_cli 调用必须恰好一条（状态读）——多一条就可能经它跑别的命令: {arm}"
        );
        assert!(
            arm.contains("run_cli(&[\"status\", \"--json\"])"),
            "login 臂内的 CLI 读必须是 `status --json`（别的命令走这条口就是逃逸）: {arm}"
        );
        for forbidden in ["Command::new", "std::process::", ".spawn("] {
            assert!(
                !arm.contains(forbidden),
                "login 臂**不得**就地构造/派生进程（出现 {forbidden}）——发起一律经 \
                 `spawn_login_attempt`: {arm}"
            );
        }
        for &pat in ARGV_ADDING_PATTERNS {
            assert!(
                !arm.contains(pat),
                "login 臂**不得**就地拼 argv（出现 {pat}）——argv 只能经 `login_command` / \
                 `login_argv` 从白名单来（合规红线：不带任何凭据参数、不碰偏好）: {arm}"
            );
        }
    }

    // ============================================================
    // I1（2026-10-08 架构评审）：退出钩子必须把登录等待者一起收
    // ============================================================

    /// **应用退出不留孤儿**：`lib.rs` 的 `RunEvent::Exit` 钩子此前只 `tunnel::stop_all` +
    /// `tailscale::stop_all`（隧道/通道），而 `tailscale login` 的等待者**不是通道**——
    /// 它是本模块唯一的长期子进程（`--timeout 15s` 有界，但 15 秒内 兔维斯 退出就是孤儿）。
    /// 形态针：退出钩子里必须出现 `tailscale::cancel_login_attempt()`。
    /// 变异：删掉 lib.rs 里那一行 → 必红。
    #[test]
    fn app_exit_hook_reaps_login_attempt() {
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs"))
            .expect("读 lib.rs");
        let start = src
            .find("RunEvent::Exit")
            .expect("退出钩子必须存在（M4：应用退出清理子进程）");
        let hook = &src[start..];
        assert!(
            hook.contains("tailscale::cancel_login_attempt()"),
            "退出钩子必须收掉 `tailscale login` 的等待者（kill + wait）——\n\
             否则 兔维斯 退出后它就是一个孤儿进程: {hook}"
        );
    }

    /// 槽空时收尾口必须是**无副作用的安全调用**（退出钩子会在任何状态下调用它：
    /// 用户可能一次都没点过「获取登录链接」）。本用例不派生任何进程。
    #[test]
    fn cancel_login_attempt_is_safe_without_a_child() {
        cancel_login_attempt(); // 槽空 → no-op，不得 panic
        reap_login_attempt();
        assert!(
            login_child_slot_is_empty(),
            "槽空时收尾口不得凭空变出一个子进程句柄"
        );
    }

    /// **M4（2026-10-07 评审 Minor）恢复窗口内的卡点必须是琥珀档（amber）**：
    /// 线稿 82-84 明写琥珀「既不是故障（rose）也不是正常完成（green）」——每次开机的
    /// 正常恢复过程不得经通用行渲染涂成红色「卡住」。档位由后端判据给出，前端不按文案猜。
    /// 变异：把 probe_steps_from 两个恢复窗口分支的 `BlockedTone::Amber` 改回 `Rose`
    /// （或删掉载荷的 blockedTone 字段）→ 本测试必红。
    #[test]
    fn init_window_blocked_steps_are_amber_not_rose() {
        let _g = test_lock();
        set_run_cli_override(Some(Box::new(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_BACKEND_INIT.into()),
            ["get", "--json"] => Err("backend not ready".into()),
            ["funnel", "status", "--json"] => Ok("{}".into()),
            other => Err(format!("恢复窗口内不该有这条调用: {other:?}")),
        })));
        let p = wizard_status_with(9420, true);
        let states = p["states"].as_array().unwrap();
        for id in ["funnel", "shields_up"] {
            let st = states
                .iter()
                .find(|s| s["id"] == id)
                .unwrap_or_else(|| panic!("必有 {id} 步"));
            assert!(
                st["blockedReason"].is_string(),
                "恢复窗口内该步必须如实说明（不是待做诱导用户去点）: {st}"
            );
            assert_eq!(
                st["blockedTone"], "amber",
                "{id} 在恢复窗口内是**中间态**，不得涂成红色故障: {st}"
            );
        }
        // 对照：同一时刻其他卡点仍是 rose（不得一律加琥珀，否则档位失去意义）
        let detect = states
            .iter()
            .find(|s| s["id"] == "detect")
            .expect("必有 detect 步");
        assert_eq!(detect["done"], true, "夹具自检：装好了");
        teardown_globals();
    }

    /// 修复轮 2 指针③：run_step("enable") / run_step("funnel") 必须走与 start_channel
    /// 一致的完整生命周期（守卫 → 开通 → 置 DESIRED → 现算 → 起轮询），不得直调
    /// enable_funnel 原语留下一份「开了 Funnel 但轮询不守护」的悬空态。
    /// 变异锚点：把分派改回 `enable_funnel(port)` 直调 → DESIRED 断言必红
    #[test]
    fn run_step_enable_and_funnel_follow_full_start_lifecycle() {
        let _g = test_lock();
        let calls: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>> = Default::default();
        let c = calls.clone();
        set_run_cli_override(Some(Box::new(move |args: &[&str]| {
            let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            c.lock().unwrap().push(v.clone());
            match args {
                ["funnel", "status", "--json"] => Ok("{}".into()), // 守卫：无既有配置
                ["set", "--shields-up=false"] => Ok(String::new()),
                ["get", "--json"] => Ok(r#"{"shields-up": false}"#.into()),
                ["funnel", "--bg", "9420"] => Ok(String::new()),
                ["funnel", "status"] => Ok(String::new()),
                ["status", "--json"] => Ok(STATUS_RUNNING.into()),
                other => Err(format!("测试未应答的 CLI 调用: {other:?}")),
            }
        })));
        let r = run_step("enable", 9420).expect("enable 步应成功");
        assert_eq!(r["ok"], true);
        assert_eq!(
            *DESIRED.lock().unwrap(),
            Some(9420),
            "enable 必须置期望态（轮询守护起走）——直调原语则此处必红"
        );
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c == &["funnel".to_string(), "--bg".to_string(), "9420".to_string()]),
            "开通命令必须已发出: {:?}",
            calls.lock().unwrap()
        );
        teardown_globals();

        // funnel 步：同一生命周期 + 批准链接（若有）随回执递出（兔维斯 不代点）
        let c2 = calls.clone();
        set_run_cli_override(Some(Box::new(move |args: &[&str]| {
            let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            c2.lock().unwrap().push(v.clone());
            match args {
                ["funnel", "status", "--json"] => Ok("{}".into()),
                ["set", "--shields-up=false"] => Ok(String::new()),
                ["get", "--json"] => Ok(r#"{"shields-up": false}"#.into()),
                ["funnel", "--bg", "9420"] => Ok(String::new()),
                ["funnel", "status"] => {
                    Ok("To approve, visit: https://login.tailscale.com/funnel/abc.".into())
                }
                ["status", "--json"] => Ok(STATUS_RUNNING.into()),
                other => Err(format!("测试未应答的 CLI 调用: {other:?}")),
            }
        })));
        let r = run_step("funnel", 9420).expect("funnel 步应成功");
        assert_eq!(
            r["approvalUrl"], "https://login.tailscale.com/funnel/abc",
            "批准链接须剥句尾标点后递出"
        );
        assert_eq!(*DESIRED.lock().unwrap(), Some(9420));
        teardown_globals();
    }

    #[test]
    fn first_funnel_step_persists_intent_and_can_complete_the_wizard() {
        let _g = test_lock();
        use crate::database::dao::settings::{get_setting, set_setting};
        let old_enabled = get_setting(crate::remote::KEY_ENABLED).unwrap_or_else(|| "false".into());
        let old_channel =
            get_setting(crate::remote::KEY_CHAN_TAILSCALE).unwrap_or_else(|| "0".into());
        set_setting(crate::remote::KEY_ENABLED, "true");
        set_setting(crate::remote::KEY_CHAN_TAILSCALE, "0");
        let active = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cli_active = active.clone();
        set_run_cli_override(Some(Box::new(move |args| {
            match args {
            ["status", "--json"] => Ok(STATUS_RUNNING.into()),
            ["funnel", "status", "--json"] if cli_active.load(std::sync::atomic::Ordering::SeqCst) => Ok(r#"{"Web":{"x.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:9420"}}}}}"#.into()),
            ["funnel", "status", "--json"] => Ok("{}".into()),
            ["set", "--shields-up=false"] => Ok(String::new()),
            ["get", "--json"] => Ok(r#"{"shields-up":false}"#.into()),
            ["funnel", "--bg", "9420"] => { cli_active.store(true, std::sync::atomic::Ordering::SeqCst); Ok(String::new()) },
            ["funnel", "status"] => Ok(String::new()),
            other => Err(format!("测试未应答: {other:?}")),
        }
        })));
        let probe_active = active.clone();
        set_verify_probe_override(Some(Box::new(move |_| {
            Ok(if probe_active.load(std::sync::atomic::Ordering::SeqCst) {
                vec!["203.0.113.10".into()]
            } else {
                vec![]
            })
        })));
        set_http_probe_override(Some(Box::new(|_, _| Ok(()))));
        assert_eq!(wizard_status_with(9420, true)["enabled"], false);
        assert_eq!(
            run_step("verify", 9420).unwrap()["reach"]["state"],
            "record_pending"
        );
        assert_eq!(
            get_setting(crate::remote::KEY_CHAN_TAILSCALE).as_deref(),
            Some("0")
        );
        let opened = run_step("funnel", 9420).unwrap();
        let verified = run_step("verify", 9420).unwrap();
        let probe = wizard_status_with(9420, true);
        let persisted = get_setting(crate::remote::KEY_CHAN_TAILSCALE);
        set_setting(crate::remote::KEY_ENABLED, &old_enabled);
        set_setting(crate::remote::KEY_CHAN_TAILSCALE, &old_channel);
        teardown_globals();
        assert_eq!(opened["ok"], true);
        assert_eq!(verified["done"], true);
        assert_eq!(persisted.as_deref(), Some("1"));
        assert_eq!(probe["enabled"], true);
        assert_eq!(probe["reach"]["state"], "verified");
        assert!(probe["running"].as_bool().unwrap());
        assert!(probe["states"]
            .as_array()
            .unwrap()
            .iter()
            .all(|state| state["done"] == true));
    }

    #[test]
    fn failed_first_funnel_step_keeps_channel_intent_off() {
        let _g = test_lock();
        use crate::database::dao::settings::{get_setting, set_setting};
        let old_channel =
            get_setting(crate::remote::KEY_CHAN_TAILSCALE).unwrap_or_else(|| "0".into());
        set_setting(crate::remote::KEY_CHAN_TAILSCALE, "0");
        let calls = spy_cli(|args| {
            match args {
            ["status", "--json"] => Ok(STATUS_RUNNING.into()),
            ["funnel", "status", "--json"] => Ok(r#"{"Web":{"foreign.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8080"}}}}}"#.into()),
            ["get", "--json"] => Ok(r#"{"shields-up":false}"#.into()),
            other => Err(format!("归属守卫拒绝时不得执行: {other:?}")),
        }
        });
        let result = run_step("funnel", 9420);
        let persisted = get_setting(crate::remote::KEY_CHAN_TAILSCALE);
        let probe = wizard_status_with(9420, true);
        set_setting(crate::remote::KEY_CHAN_TAILSCALE, &old_channel);
        let calls = calls.lock().unwrap().clone();
        teardown_globals();
        assert!(result.unwrap_err().contains("TS_FOREIGN_SERVE"));
        assert_eq!(persisted.as_deref(), Some("0"));
        assert_eq!(probe["enabled"], false);
        assert_eq!(probe["running"], false);
        assert_no_write_calls(&calls);
    }

    /// A1 **变异锚点**（2026-10-07 Windows 实测）：`funnel --bg` **成功即算开通**——
    /// 没有批准链接是**正常成功路径**（Windows 实测零批准链接），不是「等待批准」也不是错误。
    /// 代码若改成「等批准」「无链接即报错/卡住」，本测试必红。
    /// 开通成功后的下一步是**等公网解析**（§C3）：verify 步此刻是「待做」（等 60s 重验窗口
    /// 覆盖记录发布），**不得**是 blocked（那会变成"卡在批准"的假象）。
    #[test]
    fn funnel_step_succeeds_without_approval_link() {
        let _g = test_lock();
        const OURS: &str =
            r#"{"Web":{"x.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:9420"}}}}}"#;
        set_run_cli_override(Some(Box::new(|args: &[&str]| match args {
            ["funnel", "status", "--json"] => Ok(OURS.into()), // 重开幂等：配置已是我们那路
            ["set", "--shields-up=false"] => Ok(String::new()),
            ["get", "--json"] => Ok(r#"{"shields-up": false}"#.into()),
            // 实测形态：funnel --bg 立即返回，无任何批准链接
            ["funnel", "--bg", "9420"] => {
                Ok("Funnel started and running in the background.\n".into())
            }
            ["funnel", "status"] => Ok("Funnel started and running in the background.\n".into()),
            ["status", "--json"] => Ok(STATUS_RUNNING.into()),
            other => Err(format!("测试未应答的 CLI 调用: {other:?}")),
        })));
        set_reachability(Reachability::Unverified);
        let r = run_step("funnel", 9420).expect("无批准链接必须照常成功——不得等批准、不得报错");
        assert_eq!(r["ok"], true);
        assert!(
            r["approvalUrl"].is_null(),
            "无批准链接时如实为 null（不编造、不阻塞）: {r}"
        );
        assert_eq!(
            *DESIRED.lock().unwrap(),
            Some(9420),
            "开通成功即进入运行态（轮询守护 + 等公网解析），不得停在『等批准』"
        );
        // 等公网解析：verify 步 = 待做（无 blocked_reason），不是卡住
        let steps = probe_steps_from(9420, true, &read_cli_readings());
        let verify = steps
            .iter()
            .find(|s| s.id == "verify")
            .expect("必有 verify 步");
        assert!(
            !verify.done && verify.blocked_reason.is_none(),
            "开通后下一步是等公网解析（verify 待做），不得报成卡在批准: {verify:?}"
        );
        teardown_globals();
    }

    /// verify 步实装（Task 7）：全链假件（CLI 登录态 + DoH 假探针有记录 + 钉 IP 探测通）
    /// → done=true、reach=verified 写全局。零网络：DoH/HTTP 两个缝全注入
    #[test]
    fn run_step_verify_checks_and_stores_reachability() {
        let _g = test_lock();
        set_run_cli_override(Some(Box::new(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_RUNNING.into()),
            other => Err(format!("测试未应答的 CLI 调用: {other:?}")),
        })));
        set_verify_probe_override(Some(Box::new(|_h| Ok(vec!["203.0.113.10".into()]))));
        set_http_probe_override(Some(Box::new(|_h, _ip| Ok(()))));
        set_reachability(Reachability::Unverified);

        let r = run_step("verify", 9420).expect("verify 应 Ok");
        assert_eq!(r["done"], true, "记录在且链路通 → 完成: {r}");
        assert_eq!(r["reach"]["state"], "verified");
        assert_eq!(reachability(), Reachability::Verified, "结果必须写全局");
        teardown_globals();
    }

    #[test]
    fn verify_is_read_only_even_when_channel_is_enabled_and_record_is_pending() {
        let _g = test_lock();
        *DESIRED.lock().unwrap() = Some(9420);
        let calls = spy_cli(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_RUNNING.into()),
            other => Err(format!("普通 verify 不应执行写入或守卫命令: {other:?}")),
        });
        set_verify_probe_override(Some(Box::new(|_h: &str| Ok(vec![]))));

        let result = run_step("verify", 9420).expect("只读 verify 应返回检查结果");
        assert_eq!(result["reach"]["state"], "record_pending");
        assert_eq!(
            reachability(),
            Reachability::RecordPending { republish: false }
        );
        assert!(calls
            .lock()
            .unwrap()
            .iter()
            .all(|args| args == &["status".to_string(), "--json".to_string()]));
        teardown_globals();
    }

    #[test]
    fn repair_does_not_reset_funnel_when_channel_intent_is_off() {
        let _g = test_lock();
        *DESIRED.lock().unwrap() = None;
        let calls = spy_cli(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_RUNNING.into()),
            other => Err(format!("关闭状态下 repair 不得执行配置写入: {other:?}")),
        });
        set_verify_probe_override(Some(Box::new(|_h: &str| Ok(vec![]))));

        let result = run_step("repair", 9420).expect("关闭状态下 repair 仍返回只读结论");
        assert_eq!(result["reach"]["state"], "record_pending");
        assert!(calls
            .lock()
            .unwrap()
            .iter()
            .all(|args| args == &["status".to_string(), "--json".to_string()]));
        teardown_globals();
    }

    #[test]
    fn repair_aborts_before_reset_when_channel_closes_during_guard_read() {
        let _g = test_lock();
        *DESIRED.lock().unwrap() = Some(9420);
        let calls = spy_cli(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_RUNNING.into()),
            ["funnel", "status", "--json"] => {
                *DESIRED.lock().unwrap() = None;
                super::reach::invalidate_verification_lifecycle();
                Ok("{}".into())
            }
            other => Err(format!("关闭期间不应继续 repair: {other:?}")),
        });
        set_verify_probe_override(Some(Box::new(|_h: &str| Ok(vec![]))));

        let result = run_step("repair", 9420).expect("过期 repair 回执应正常返回");
        assert_eq!(result["stale"], true);
        assert!(calls
            .lock()
            .unwrap()
            .iter()
            .all(|args| args != &["funnel".to_string(), "reset".to_string()]));
        teardown_globals();
    }

    #[test]
    fn repair_does_not_reenable_after_lifecycle_changes_during_reset() {
        let _g = test_lock();
        *DESIRED.lock().unwrap() = Some(9420);
        let calls = spy_cli(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_RUNNING.into()),
            ["funnel", "status", "--json"] => Ok("{}".into()),
            ["funnel", "reset"] => {
                super::reach::advance_lifecycle_epoch();
                Ok(String::new())
            }
            ["set", "--shields-up=false"] => Ok(String::new()),
            ["get", "--json"] => Ok(r#"{"shields-up": false}"#.into()),
            ["funnel", "--bg", "9420"] => Ok(String::new()),
            ["funnel", "status"] => Ok(String::new()),
            other => Err(format!("关闭后的 repair 不应继续写: {other:?}")),
        });
        set_verify_probe_override(Some(Box::new(|_h: &str| Ok(vec![]))));

        let result = run_step("repair", 9420).expect("过期 repair 回执应正常返回");
        assert_eq!(result["stale"], true);
        assert!(calls.lock().unwrap().iter().all(|args| {
            args != &["set".to_string(), "--shields-up=false".to_string()]
                && args != &["funnel".to_string(), "--bg".to_string(), "9420".to_string()]
        }));
        teardown_globals();
    }

    #[test]
    fn repair_rechecks_lifecycle_after_reopen_reads_and_between_writes() {
        let _g = test_lock();
        for interrupt_on in ["reopen_guard", "shields_write"] {
            *DESIRED.lock().unwrap() = Some(9420);
            let guard_reads = std::sync::atomic::AtomicUsize::new(0);
            let calls = spy_cli(move |args: &[&str]| match args {
                ["status", "--json"] => Ok(STATUS_RUNNING.into()),
                ["funnel", "status", "--json"] => {
                    if guard_reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1
                        && interrupt_on == "reopen_guard"
                    {
                        super::reach::advance_lifecycle_epoch();
                    }
                    Ok("{}".into())
                }
                ["funnel", "reset"] => Ok(String::new()),
                ["set", "--shields-up=false"] => {
                    if interrupt_on == "shields_write" {
                        super::reach::advance_lifecycle_epoch();
                    }
                    Ok(String::new())
                }
                ["get", "--json"] => Ok(r#"{"shields-up":false}"#.into()),
                ["funnel", "--bg", "9420"] | ["funnel", "status"] => Ok(String::new()),
                other => Err(format!("测试未应答: {other:?}")),
            });
            set_verify_probe_override(Some(Box::new(|_| Ok(vec![]))));
            let result = run_step("repair", 9420).unwrap();
            assert_eq!(result["stale"], true);
            let calls = calls.lock().unwrap();
            assert!(
                !calls.iter().any(|args| args == &["funnel", "--bg", "9420"]),
                "{interrupt_on}: 关闭请求已发生，不得继续公开端口: {calls:?}"
            );
            if interrupt_on == "reopen_guard" {
                assert!(!calls
                    .iter()
                    .any(|args| args == &["set", "--shields-up=false"]));
            }
            drop(calls);
            teardown_globals();
        }
    }

    #[test]
    fn verify_and_repair_return_missing_dns_host_reason_instead_of_stale_unverified() {
        let _g = test_lock();
        set_run_cli_override(Some(Box::new(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_NEEDS_LOGIN_NO_URL.into()),
            other => Err(format!("缺域名路径不应调用其他 CLI: {other:?}")),
        })));

        for step in ["verify", "repair"] {
            let result = run_step(step, 9420).expect("缺域名应返回校验结果");
            assert_eq!(result["stale"], false, "{step}: {result}");
            assert_eq!(result["reach"]["state"], "failed", "{step}: {result}");
            assert!(result["reach"]["reason"]
                .as_str()
                .unwrap_or_default()
                .starts_with("TSWIZ_VERIFY_HOST_MISSING:"));
        }
        teardown_globals();
    }

    /// repair 自愈正路：第一次校验解析不到（记录未发布）→ 通道开着（DESIRED=Some）→
    /// 自动 reset + 重开 → 记录「重新发布」→ 重验 Verified。
    /// **变异锚点**：把 verify_with_heal 的自愈分支删掉 → reset/bg 不入账、done 断言红
    #[test]
    fn run_step_repair_failed_heals_via_reset_and_reenable() {
        let _g = test_lock();
        *DESIRED.lock().unwrap() = Some(9420); // 通道开着 → 允许自愈
        let calls: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>> = Default::default();
        let published = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let c = calls.clone();
        let p = published.clone();
        set_run_cli_override(Some(Box::new(move |args: &[&str]| {
            let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            c.lock().unwrap().push(v.clone());
            match args {
                ["status", "--json"] => Ok(STATUS_RUNNING.into()),
                // 守卫读数：无外来配置（实测空对象 `{}`）
                ["funnel", "status", "--json"] => Ok("{}".into()),
                // reset 触发记录重新发布（自愈机理的测试建模）
                ["funnel", "reset"] => {
                    p.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(String::new())
                }
                ["set", "--shields-up=false"] => Ok(String::new()),
                ["get", "--json"] => Ok(r#"{"shields-up": false}"#.into()),
                ["funnel", "--bg", "9420"] => Ok(String::new()),
                ["funnel", "status"] => Ok(String::new()),
                other => Err(format!("测试未应答的 CLI 调用: {other:?}")),
            }
        })));
        let p2 = published.clone();
        set_verify_probe_override(Some(Box::new(move |_h: &str| {
            if p2.load(std::sync::atomic::Ordering::SeqCst) {
                Ok(vec!["203.0.113.10".into()])
            } else {
                Ok(vec![]) // 权威否定：记录尚未发布
            }
        })));
        set_http_probe_override(Some(Box::new(|_h, _ip| Ok(()))));

        let r = run_step("repair", 9420).expect("repair 应 Ok");
        assert_eq!(r["done"], true, "自愈后记录发布 → 重验通过: {r}");
        assert_eq!(reachability(), Reachability::Verified);
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c == &["funnel".to_string(), "reset".to_string()]),
            "自愈必须发出 funnel reset: {:?}",
            calls.lock().unwrap()
        );
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c == &["funnel".to_string(), "--bg".to_string(), "9420".to_string()]),
            "自愈必须重开 Funnel: {:?}",
            calls.lock().unwrap()
        );
        teardown_globals();
    }

    /// 连不通类失败**不自愈**（2026-10-13 用户裁决）：「地址已解析但连不通」说明 DNS
    /// 记录在、配置在，成因是到公网中继入口的链路瞬时抖动（真机实测 60s 轮询自愈）——
    /// reset 只会把完好配置清掉再重发布 30–49 秒。verify 步必须原样返回该 Failed、
    /// 一个写动作都不发。
    /// 变异锚点：删掉 verify_with_heal 的 ADDR_UNREACHABLE_PREFIX 拦截分支 → ②③必红。
    #[test]
    fn run_step_verify_unreachable_class_does_not_heal() {
        let _g = test_lock();
        *DESIRED.lock().unwrap() = Some(9420); // 通道开着——旧实现正是在这态才自愈
        let calls = spy_cli(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_RUNNING.into()),
            other => Err(format!("测试未应答的 CLI 调用: {other:?}")),
        });
        // 解析得到记录 + 钉 IP 逐条连不通 → 「地址已解析但连不通」类 Failed
        set_verify_probe_override(Some(Box::new(|_h: &str| {
            Ok(vec!["203.0.113.1".into(), "203.0.113.2".into()])
        })));
        set_http_probe_override(Some(Box::new(|_h, _ip| {
            Err("error sending request for url (https://a.ts.net/m)".into())
        })));

        run_step("verify", 9420).expect("verify 应 Ok");
        // ① 结果如实是连不通类 Failed（前缀 + 逐条失败明细）
        match reachability() {
            Reachability::Failed { reason } => {
                assert!(
                    reason.starts_with(ADDR_UNREACHABLE_PREFIX) && reason.contains("203.0.113.1"),
                    "{reason}"
                );
            }
            other => panic!("应 Failed: {other:?}"),
        }
        // ②③ 一个写动作都不发：不 reset、不重开、连自愈守卫读数都不该有
        let calls = calls.lock().unwrap().clone();
        assert!(
            !calls
                .iter()
                .any(|c| c == &["funnel".to_string(), "reset".to_string()]),
            "连不通类不得 funnel reset: {calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|c| c.starts_with(&["funnel".to_string(), "--bg".to_string()])),
            "连不通类不得重开 Funnel: {calls:?}"
        );
        assert!(
            !calls.iter().any(|c| c
                == &[
                    "funnel".to_string(),
                    "status".to_string(),
                    "--json".to_string()
                ]),
            "连不通类不得进入自愈（守卫读数都不发生）: {calls:?}"
        );
        teardown_globals();
    }

    /// 对照上一条：**其他**失败（DoH 全挂 → 无法验证）仍走自愈——reset+重开的意义在
    /// 「真故障时触发记录重发布」（2026-10-06 实测机理），2026-10-13 裁决只收窄连不通类。
    #[test]
    fn run_step_repair_other_failure_still_heals() {
        let _g = test_lock();
        *DESIRED.lock().unwrap() = Some(9420);
        let calls = spy_cli(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_RUNNING.into()),
            ["funnel", "status", "--json"] => Ok("{}".into()),
            ["funnel", "reset"] => Ok(String::new()),
            ["set", "--shields-up=false"] => Ok(String::new()),
            ["get", "--json"] => Ok(r#"{"shields-up": false}"#.into()),
            ["funnel", "--bg", "9420"] => Ok(String::new()),
            ["funnel", "status"] => Ok(String::new()),
            other => Err(format!("测试未应答的 CLI 调用: {other:?}")),
        });
        // DoH 全挂 → Failed「无法验证地址…」（不是连不通类）
        set_verify_probe_override(Some(Box::new(|_h: &str| {
            Err("3 个 DoH 源均无有效应答: 223.5.5.5、223.6.6.6、1.12.12.12".into())
        })));

        run_step("repair", 9420).expect("repair 应 Ok");
        assert!(
            matches!(reachability(), Reachability::Failed { ref reason } if reason.contains("无法验证")),
            "自愈后仍验不过 → 如实 Failed: {:?}",
            reachability()
        );
        let calls = calls.lock().unwrap().clone();
        assert!(
            calls
                .iter()
                .any(|c| c == &["funnel".to_string(), "reset".to_string()]),
            "其他失败仍须 funnel reset（自愈正路）: {calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| c == &["funnel".to_string(), "--bg".to_string(), "9420".to_string()]),
            "其他失败仍须重开 Funnel: {calls:?}"
        );
        teardown_globals();
    }

    /// [`reach::error_with_source`]：reqwest 顶层 Display 只给「error sending request」，
    /// source 链首层才是真实成因（tcp 超时 / TLS / 重置）——2026-10-13 实机定位的
    /// 盲区补齐。纯函数：有 source 拼「（底层: …）」，无 source 原样返回。
    #[test]
    fn error_with_source_appends_first_source_layer() {
        #[derive(Debug)]
        struct Inner(&'static str);
        impl std::fmt::Display for Inner {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
        impl std::error::Error for Inner {}
        #[derive(Debug)]
        struct Outer(&'static str, Option<Inner>);
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                self.1.as_ref().map(|i| i as _)
            }
        }
        assert_eq!(
            error_with_source(&Outer(
                "error sending request",
                Some(Inner("tcp connect timeout"))
            )),
            "error sending request（底层: tcp connect timeout）"
        );
        assert_eq!(
            error_with_source(&Outer("error sending request", None)),
            "error sending request"
        );
    }

    /// **I1（2026-10-07 评审）自愈的 `funnel reset` 必须记账**：自愈动作就是
    /// 「重置 Funnel 后重新开启」（§C3 要求 3），它**正是最该走「重新开通」档**的场景
    /// （reset 后重发布实测 30–49 秒）。旧实现直接 `let _ = run_cli(&["funnel","reset"])`
    /// 绕过 [`super::status::disable_funnel`]，于是 `FUNNEL_RESET_SEEN` 不置位 ⇒ 随后落
    /// `RecordPending{republish:false}` ⇒ 用户看到首开档「首次开通约需 5–6 分钟」（把
    /// 30 秒说成 5 分钟）。规格 §C3 要求 6 三档表写「`disable_funnel` 是 reset 的唯一
    /// 调用点 ⇒ 这是事实不是猜测」——本用例把这个可证事实钉在**自愈路径**上。
    /// 变异：把 `disable_funnel()` 换回 `let _ = run_cli(&["funnel", "reset"]);` → ②③ 必红。
    #[test]
    fn heal_reset_is_accounted_so_record_pending_uses_republish_tier() {
        let _g = test_lock();
        *DESIRED.lock().unwrap() = Some(9420);
        let calls = spy_cli(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_RUNNING.into()),
            ["funnel", "status", "--json"] => Ok("{}".into()),
            ["funnel", "reset"] => Ok(String::new()),
            ["set", "--shields-up=false"] => Ok(String::new()),
            ["get", "--json"] => Ok(r#"{"shields-up": false}"#.into()),
            ["funnel", "--bg", "9420"] => Ok(String::new()),
            ["funnel", "status"] => Ok(String::new()),
            other => Err(format!("测试未应答的 CLI 调用: {other:?}")),
        });
        // 记录始终未发布（权威否定）：自愈前 → 触发自愈；自愈后 → 落「记录尚未发布」
        set_verify_probe_override(Some(Box::new(|_h: &str| Ok(vec![]))));
        assert!(
            !reset_seen_raw(),
            "起点：本进程还没 reset 过（判据未置位前，档位必须不是「重开」）"
        );

        let r = run_step("repair", 9420).expect("repair 应 Ok");
        // ① 自愈确实发生了（reset + 重开）
        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.as_slice() == ["funnel".to_string(), "reset".to_string()])
                .count(),
            1,
            "自愈必须恰好发一次 funnel reset: {:?}",
            calls.lock().unwrap()
        );
        // ② 记账：`disable_funnel` 是唯一 reset 调用点（自愈也经它）
        assert!(
            reset_seen_raw() && pending_republish(),
            "自愈的 reset 必须经 disable_funnel 记账（否则「本进程重开过」在最该成立的场景恒假）"
        );
        // ③ 回执落「重新开通」档，文案取重开档（30 秒～1 分钟），不是首开那句 5–6 分钟
        assert_eq!(
            r["reach"]["state"], "record_pending",
            "记录未发布是正常发布延迟: {r}"
        );
        assert_eq!(
            r["reach"]["republish"], true,
            "自愈 reset 后重开必须走重新开通档: {r}"
        );
        assert_eq!(
            record_pending_hint(true),
            RECORD_REPUBLISH_HINT,
            "重开档文案必须是 30 秒～1 分钟那句"
        );
        assert_ne!(
            record_pending_hint(true),
            record_pending_hint(false),
            "两档不得冒充（把 30 秒说成 5 分钟正是本条要修的形态）"
        );
        teardown_globals();
    }

    /// **I1（2026-10-07 评审）自愈失败不得吞掉**：`funnel reset` 失败时**不发**
    /// `funnel --bg`（把「半程自愈」当成功），如实报
    /// `Failed{reason:"自愈失败（重置 Funnel 未成功）…"}`。
    /// 变异：把 `disable_funnel()?` 换回 `let _ = run_cli(&["funnel", "reset"]);`（吞掉错误）
    /// → `set`/`--bg` 会被发出，②③ 必红。
    #[test]
    fn heal_reports_failure_and_skips_reenable_when_reset_fails() {
        let _g = test_lock();
        *DESIRED.lock().unwrap() = Some(9420);
        let calls = spy_cli(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_RUNNING.into()),
            ["funnel", "status", "--json"] => Ok("{}".into()),
            ["funnel", "reset"] => Err("permission denied".into()),
            other => Err(format!("reset 失败后不该有这条调用: {other:?}")),
        });
        set_verify_probe_override(Some(Box::new(|_h: &str| Ok(vec![]))));

        let r = run_step("repair", 9420).expect("repair 应 Ok");
        // ① 如实报失败（点名「重置 Funnel 未成功」与真原因）
        assert_eq!(r["done"], false);
        let reason = r["reach"]["reason"].as_str().unwrap_or_default();
        assert!(
            reason.contains("自愈失败")
                && reason.contains("重置 Funnel 未成功")
                && reason.contains("permission denied"),
            "reset 失败必须如实上墙（成因 + 真错误）: {reason}"
        );
        assert!(
            matches!(reachability(), Reachability::Failed { .. }),
            "失败原因必须写进校验态: {:?}",
            reachability()
        );
        // ② 半程自愈不算成功：不发 set / --bg（配置已被 reset 清掉，但绝不谎称已重开）
        let snapshot = calls.lock().unwrap().clone();
        let writes: Vec<&Vec<String>> = snapshot
            .iter()
            .filter(|c| {
                c.first().map(String::as_str) == Some("set") || c.contains(&"--bg".to_string())
            })
            .collect();
        assert!(
            writes.is_empty(),
            "reset 失败后不得继续 shields-up / funnel --bg（半程自愈当成功 = 谎报）: {writes:?}"
        );
        teardown_globals();
    }

    /// **I1 源码锁（2026-10-07 评审）**：`funnel reset` 的调用点必须**唯一**
    /// （[`super::status::disable_funnel`]）——这是「重新开通档判据可证」的前提，
    /// 也是「撤销/自愈都过守卫与记账」的前提。自愈曾自己再发一条 reset 绕过记账
    /// （把实测 30–49 秒说成 5–6 分钟），故本锁扫**全部生产文件**，不是只扫一个。
    /// 变异：在 reach.rs / wizard.rs / mod.rs 任一处再写一条 reset 调用 → 必红。
    #[test]
    fn funnel_reset_has_exactly_one_call_site() {
        // 运行时拼出针（避免本测试自己的字面量被自己扫到）
        // 调用形态针：只在 `run_cli(&["funnel", "reset"])` 这种**调用**上命中
        let call = ["run_cli(&[", "\"funnel\"", ", ", "\"reset\"", "])"].concat();
        // 宽松针：命令参数对，模式匹配（`["funnel", "reset", ..]` 超时分档）也会命中
        let pair = ["\"funnel\"", "\"reset\""].join(", ");
        let hits = |src: &str, needle: &str| src.matches(needle).count();
        let reach = include_str!("reach.rs");
        let wizard = include_str!("wizard.rs");
        let modrs = include_str!("mod.rs");
        let status = include_str!("status.rs");
        // 只扫**生产段**（mod.rs 的测试替身里有大量 `["funnel","reset"] => Ok(..)` 应答臂，
        // 那是夹具不是调用点；`#[cfg(test)] mod tests` 之后的内容整体排除）
        fn prod(src: &str) -> &str {
            let marker = ["#[cfg(test)]", "mod tests {"].join("\n");
            match src.find(&marker) {
                Some(i) => &src[..i],
                None => src,
            }
        }
        assert_eq!(
            hits(prod(status), &call),
            1,
            "status.rs 必须恰好一条 `funnel reset` 调用（disable_funnel 内）"
        );
        assert_eq!(
            hits(prod(status), &pair),
            2,
            "status.rs 里除那一条调用外，只允许 CLI 超时分档的模式匹配（新增第三处即红）"
        );
        for (name, src) in [
            ("reach.rs", reach),
            ("wizard.rs", wizard),
            ("mod.rs", modrs),
        ] {
            assert_eq!(
                hits(prod(src), &pair),
                0,
                "{name} 不得再发 `funnel reset`——必须经 status::disable_funnel（守卫 + 记账 + 失败如实上抛）"
            );
        }
    }

    /// 自愈守卫：配置是外来的 → **绝不 funnel reset**（清整份 serve 配置不可接受），
    /// 守卫拒绝如实写进校验态与回执（§C3 要求 3「仍失败才报错」不掩盖）。
    /// 变异锚点：去掉 heal_and_reverify 的守卫先行分支 → reset 入账、本测试红
    #[test]
    fn heal_refuses_to_reset_foreign_serve_config() {
        let _g = test_lock();
        *DESIRED.lock().unwrap() = Some(9420);
        const FOREIGN: &str =
            r#"{"Web":{"x.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8080"}}}}}"#;
        // status 正常（域名可解析、首轮校验真实走通）+ funnel status 外来占用
        let calls: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>> = Default::default();
        let c = calls.clone();
        set_run_cli_override(Some(Box::new(move |args: &[&str]| {
            let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            c.lock().unwrap().push(v.clone());
            match args {
                ["status", "--json"] => Ok(STATUS_RUNNING.into()),
                ["funnel", "status", "--json"] => Ok(FOREIGN.into()),
                other => Err(format!("测试未应答的 CLI 调用: {other:?}")),
            }
        })));
        set_verify_probe_override(Some(Box::new(|_h| Ok(vec![])))); // 解析不到 → Failed → 走自愈
        set_http_probe_override(Some(Box::new(|_h, _ip| Ok(()))));

        let r = run_step("repair", 9420).expect("repair 应 Ok");
        assert_eq!(r["done"], false);
        let reason = r["reach"]["reason"].as_str().unwrap_or_default();
        assert!(
            reason.contains("检测到非 兔维斯"),
            "守卫拒绝必须如实上墙: {reason}"
        );
        assert!(
            matches!(reachability(), Reachability::Failed { .. }),
            "拒绝原因必须写进校验态"
        );
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .all(|c| c == &["funnel", "status", "--json"] || c == &["status", "--json"]),
            "外来配置在先不得发出 funnel reset: {:?}",
            calls.lock().unwrap()
        );
        teardown_globals();
    }

    /// 通道没开（DESIRED=None）时校验失败**不得自愈**——开通只属于 funnel 步/开关，
    /// verify 绝不借自愈偷开 Funnel（fail-closed：如实报未生效）
    #[test]
    fn verify_does_not_heal_when_channel_is_off() {
        let _g = test_lock();
        let calls: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>> = Default::default();
        let c = calls.clone();
        set_run_cli_override(Some(Box::new(move |args: &[&str]| {
            let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            c.lock().unwrap().push(v.clone());
            match args {
                ["status", "--json"] => Ok(STATUS_RUNNING.into()),
                other => Err(format!("测试未应答的 CLI 调用: {other:?}")),
            }
        })));
        set_verify_probe_override(Some(Box::new(|_h| Ok(vec![]))));
        set_http_probe_override(Some(Box::new(|_h, _ip| Ok(()))));

        let r = run_step("verify", 9420).expect("verify 应 Ok");
        assert_eq!(r["done"], false);
        assert!(
            matches!(reachability(), Reachability::RecordPending { .. }),
            "未生效如实落全局（W-B：记录尚未发布是正常发布延迟，不是故障）"
        );
        assert!(
            !calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c[0] == "funnel" && c[1] != "status"),
            "通道没开不得发任何 funnel 动作: {:?}",
            calls.lock().unwrap()
        );
        teardown_globals();
    }

    /// 向导 verify 判据随校验态联动（Task 6 判据表 Task 7 实装后的真值表）：
    /// 未验 = 待做；Failed = blocked 带 reason；Verified + 通道在运行 = done；
    /// Verified 但通道停了 = 回退待做（地址已随快照撤下，步子不能还挂着绿）
    #[test]
    fn verify_step_reflects_reachability() {
        let _g = test_lock();
        set_run_cli_override(Some(Box::new(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_RUNNING.into()),
            other => Err(format!("测试未应答的 CLI 调用: {other:?}")),
        })));
        set_ts_snapshot(|s| {
            s.running = true;
            s.url = Some("https://jarvismac-mini.example-tailnet.ts.net/m".into());
        });
        let state_of = |v: &[StepState]| v.iter().find(|s| s.id == "verify").unwrap().clone();

        set_reachability(Reachability::Unverified);
        let s = state_of(&probe_steps_from(9420, true, &read_cli_readings()));
        assert!(!s.done && s.blocked_reason.is_none(), "未验 = 待做: {s:?}");

        set_reachability(Reachability::Failed {
            reason: "公网解析不到该地址".into(),
        });
        let s = state_of(&probe_steps_from(9420, true, &read_cli_readings()));
        assert!(
            !s.done && s.blocked_reason.as_deref() == Some("公网解析不到该地址"),
            "Failed = blocked 带 reason: {s:?}"
        );

        set_reachability(Reachability::Verified);
        assert!(state_of(&probe_steps_from(9420, true, &read_cli_readings())).done);

        // 通道停了（快照复位）：即便校验态还挂着 Verified 也回退待做
        set_ts_snapshot(|s| *s = crate::remote::tunnel::ChannelStatus::default());
        let s = state_of(&probe_steps_from(9420, true, &read_cli_readings()));
        assert!(!s.done && s.blocked_reason.is_none(), "{s:?}");
        teardown_globals();
    }

    /// 重验到期判定（纯函数）：从未重验立即到期；60s 窗内不重复；窗外到期
    #[test]
    fn reverify_due_gates_on_sixty_second_window() {
        let now = std::time::Instant::now();
        assert!(reverify_due(None, now), "从未重验过 → 立即到期");
        let fresh = now - std::time::Duration::from_secs(30);
        assert!(!reverify_due(Some(fresh), now), "30s 前重验过 → 未到期");
        let stale = now - std::time::Duration::from_secs(61);
        assert!(reverify_due(Some(stale), now), "61s 前重验过 → 到期");
    }

    /// A2 ③ + M-6 **变异锚点**（用户要求「确认 60s 重验循环能覆盖这个窗口，要测」）：
    /// 实测首次开通后公网记录 **5–6 分钟**才发布。本测试用**虚拟时钟驱动轮询线程真正走
    /// 的那一轮**（[`poller_tick`]，判据与动作只在那一处）：`refresh_once`（现算运行态）
    /// → `next_poll_secs`（同结果退避到 60s）→ `reverify_due`（60s 窗）→ `should_reverify`
    /// （只要在运行就校验，不分当前态）→ `reverify_with`（真探测）；断言「记录在第 330 秒
    /// 发布」这个实测形态能在窗口内被发现。
    ///
    /// **M-6（评审判定）**：旧用例**自己重写了一遍循环模型**（直接调 should_reverify /
    /// reverify_due / reverify_with）——把 `should_reverify` 的调用从线程体里删掉，用例
    /// **照样绿**（它测的是自己那份判据）。现在用例驱动 `poller_tick`，判据只此一处。
    /// 变异：① 把 `poller_tick` 里的 `should_reverify` 调用删掉 → 本测试必红；
    /// ② 把 should_reverify 改回「仅 Verified 才重验」→ 首轮 Failed 后再不重试，本测试必红
    ///（这正是首轮评审纠正过的旧口径）。
    #[test]
    fn sixty_second_reverify_cycle_covers_the_five_minute_publish_delay() {
        let _g = test_lock();
        const PUBLISH_AT_SECS: u64 = 330; // 实测：约 5 分钟后公网才出现 A 记录
                                          // 我们那路的 serve 配置（refresh_once 现算出的快照必须 running=true，
                                          // 否则 should_reverify 的第一参恒假，重验根本不会发起）
        const ACTIVE_OURS: &str =
            r#"{"Web":{"x.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:9420"}}}}}"#;
        let t0 = std::time::Instant::now();
        // 虚拟时钟：闭包要求 'static（ProbeFn 是 dyn ... + 'static），故用函数内静态量；
        // 本测试持 TEST_LOCK 串行，开头显式归零
        static CLOCK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        CLOCK.store(0, std::sync::atomic::Ordering::Relaxed);
        let probe = |_h: &str| {
            if CLOCK.load(std::sync::atomic::Ordering::Relaxed) >= PUBLISH_AT_SECS {
                Ok(vec!["203.0.113.10".to_string()])
            } else {
                Ok(vec![]) // 权威否定：记录尚未发布（实测连续 14 次 NODATA）
            }
        };
        // 通道开着 + CLI 替身让 refresh_once 现算出「在运行」；校验态/时钟经
        // invalidate_reachability 归零（= 重开后首个轮询窗即校验）
        *DESIRED.lock().unwrap() = Some(9420);
        set_run_cli_override(Some(Box::new(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_RUNNING.into()),
            ["funnel", "status", "--json"] => Ok(ACTIVE_OURS.into()),
            other => Err(format!("测试未应答的 CLI 调用: {other:?}")),
        })));
        set_http_probe_override(Some(Box::new(|_h, _ip| Ok(()))));
        invalidate_reachability();

        let mut st = PollerState::default(); // interval=POLL_SECS / last=None
        let mut elapsed = 0u64;
        let mut attempts = 0usize;
        let mut verified_at: Option<u64> = None;
        for _ in 0..64 {
            let slept = st.interval; // 本轮睡多久（线程里 sleep 用的是上一轮更新后的值）
            elapsed += slept;
            let now = t0 + std::time::Duration::from_secs(elapsed);
            CLOCK.store(elapsed, std::sync::atomic::Ordering::Relaxed);
            let retired = poller_tick(&mut st, now, &mut |h, port| {
                attempts += 1;
                Some(reverify_with(h, port, &probe))
            });
            assert!(!retired, "通道开着，轮询线程不得退役");
            if matches!(reachability(), Reachability::Verified) {
                verified_at = Some(elapsed);
                break;
            }
            assert!(elapsed < 900, "重验循环未能在合理时间内发现记录发布");
        }
        let at = verified_at.expect("记录发布后必须被 60s 重验循环发现（不得恒不重试）");
        assert!(
            attempts >= 6,
            "5–6 分钟窗口内必须发起多次重验（实际 {attempts} 次）"
        );
        assert!(
            at - PUBLISH_AT_SECS <= REVERIFY_SECS,
            "记录发布后应在**一个重验窗**内被发现：发布 {PUBLISH_AT_SECS}s、发现 {at}s"
        );
        teardown_globals();
    }

    /// §C3 长期化核心断言：**「先通后不通」状态可回退**——假探针先给记录（通）后撤
    /// 记录，重验一路必须 Verified → RecordPending 并覆写校验态全局（地址随载荷三门撤下，
    /// 载荷级断言见 mod.rs channels_payload_hides_address_until_reachability_verified）。
    /// **W-B 补一支（变异锚点）**：记录被撤 = **地址从可用变不可用**，用户必须收到
    /// 事件——把"记录未发布"从 Failed 拆出来后，若不补进 [`failure_event_reason`]，
    /// 这次翻转就会静默（重构不得顺走既有能力）。
    /// 变异：把 `failure_event_reason` 的 RecordPending 分支删掉 → ②档必红。
    #[test]
    fn reverify_with_reverts_verified_to_record_pending_when_probe_breaks() {
        let _g = test_lock();
        set_http_probe_override(Some(Box::new(|_h, _ip| Ok(()))));
        set_reachability(Reachability::Unverified);
        // 先通：Verified 写全局（零捕获闭包 Send+Sync，可直接按 &ProbeFn 用）
        let r = reverify_with("a.ts.net", 9420, &|_h| Ok(vec!["203.0.113.10".into()]));
        assert_eq!(r, Reachability::Verified);
        assert_eq!(reachability(), Reachability::Verified);
        // 后不通（解析记录被撤）：RecordPending 覆写全局
        let r = reverify_with("a.ts.net", 9420, &|_h| Ok(vec![]));
        assert!(matches!(r, Reachability::RecordPending { .. }));
        assert!(
            matches!(reachability(), Reachability::RecordPending { .. }),
            "回退后的状态必须落全局（载荷三门据此撤地址）"
        );
        // ① 事件判据：Verified → Failed 与 Verified → RecordPending 都发
        assert!(
            failure_event_reason(
                &Reachability::Verified,
                &Reachability::Failed {
                    reason: "拦截页".into()
                }
            )
            .is_some(),
            "Verified → Failed 必须发事件（原口径）"
        );
        let pending_evt = failure_event_reason(
            &Reachability::Verified,
            &Reachability::RecordPending { republish: false },
        )
        .expect("Verified → 记录被撤 也必须发事件（地址从可用变不可用）");
        assert!(
            pending_evt.contains("分钟"),
            "事件文案取该档口径（带时长），不谎称成因: {pending_evt}"
        );
        // ② 防刷屏：没有"从可用跌落"的形态一律不发
        for (prev, next) in [
            (
                Reachability::Unverified,
                Reachability::RecordPending { republish: false },
            ),
            (Reachability::Unverified, Reachability::Verified),
            (Reachability::Verified, Reachability::Verified),
        ] {
            assert!(
                failure_event_reason(&prev, &next).is_none(),
                "非「从可用跌落」不得发事件（防刷屏）: {prev:?} → {next:?}"
            );
        }
        teardown_globals();
    }

    /// **M8（2026-10-07 评审 · 控制方裁决：做）**：事件判据纳入 `Recovering`。
    /// 规格原写「从可用跌落 = `Verified → Failed` ∨ `Verified → RecordPending`」，而
    /// **中年期后端抖动**（tailscaled 服务重启 / `tailscale down/up`）会把**已验证的
    /// 地址静默撤下 1–2 分钟**：只有卡面变化、没有 `remote-tunnel-error` ⇒ 用户手里
    /// 失效的地址没有任何提示。开机场景不会刷屏（新进程起点恒 `Unverified`，生命周期
    /// 入口还会作废），故纳入不违背防刷屏红线（②即该红线）。
    /// 变异：删掉 `failure_event_reason` 的 `Recovering` 臂 → ①必红。
    #[test]
    fn failure_event_includes_recovering_but_not_from_boot_start() {
        let _g = test_lock();
        // ① 从可用跌落（Verified → Recovering）必须发：文案取「后端重连」档（带成因与时长）
        let reason = failure_event_reason(&Reachability::Verified, &Reachability::Recovering)
            .expect("Verified → Recovering 必须发事件（已验证的地址被撤下不能静默）");
        assert_eq!(reason, RECOVERING_HINT, "文案取恢复档口径，不谎称成因");
        assert!(
            reason.contains("重连") && reason.contains("1–2 分钟"),
            "必须点明成因（后端重连）与实测时长（1–2 分钟）: {reason}"
        );
        // ② 开机路径（Unverified → Recovering）**不发**：防刷屏红线
        assert!(
            failure_event_reason(&Reachability::Unverified, &Reachability::Recovering).is_none(),
            "开机恢复不是「从可用跌落」（起点就是 Unverified），不得发事件"
        );
        // ③ 接线（生产判定路 reverify_with）：解析得到但连不通 + 恢复窗口内 ⇒ Recovering
        //    写全局（事件由同一函数在同一处发出）
        set_reachability(Reachability::Verified);
        set_boot_recovery_override(Some(true));
        set_http_probe_override(Some(Box::new(|_h, _ip| Err("TLS 失败".into()))));
        let r = reverify_with("a.ts.net", 9420, &|_h| Ok(vec!["203.0.113.10".into()]));
        assert_eq!(r, Reachability::Recovering, "窗口内应落恢复中: {r:?}");
        assert_eq!(
            reachability(),
            Reachability::Recovering,
            "状态必须落全局（载荷三门据此撤下地址）"
        );
        set_boot_recovery_override(None);
        teardown_globals();
    }

    /// 修复轮 1（评审 Important）：install 步的 sha 门禁——install 可从 UI/IPC 独立
    /// 触发，落位路径上被替换/陈旧的安装包**不得直达提权安装框**（用户随后要输
    /// 管理员密码/UAC，固定 sha256 的端到端保管链不能在最后一跳断裂）。
    /// 错误必须点名「校验失败」并指引重新走 download 步（幂等语义随之做实）。
    /// **变异锚点**：删掉 install 步的 `installer_file_ok` 分支 → panic 替身爆红
    /// （零进程红线：门禁拦截时安装器绝不被触达）
    ///
    /// **平台门控（2026-10-07 存量债清理，Linux CI 门禁）**：`ts_asset_name(Platform::Other)`
    /// 返回**空串** ⇒ `dir.path().join("")` 就是临时目录**本身**，`std::fs::write` 撞
    /// `Os 21 IsADirectory` 而 unwrap 恐慌（Linux CI 实测）。本用例考的是 sha 门禁，前提是
    /// 「能在临时目录里落一个安装包文件」，而该前提只在有真实产物名的平台成立。
    #[cfg_attr(
        not(any(target_os = "macos", target_os = "windows")),
        ignore = "平台不适用：ts_asset_name(Platform::Other) 为空串，join(\"\") 落到临时目录本身，fs::write 撞 IsADirectory"
    )]
    #[test]
    fn install_step_refuses_tampered_installer_without_spawning() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join(ts_asset_name(current_platform()));
        std::fs::write(&pkg, b"tampered bytes").unwrap(); // 内容 sha 必不匹配固定表
        let r = install_step_with(current_platform(), &pkg, |_| {
            panic!("sha 门禁必须拦下坏包——不得触达 installer/msiexec")
        });
        let e = r.unwrap_err();
        assert!(e.contains("校验失败"), "必须点名 sha 校验失败: {e}");
        assert!(
            e.contains("下载安装包"),
            "必须指引重新执行 download 步: {e}"
        );
    }

    /// install 步缺失包（exists 门禁在前）：文案指引先走 download 步，同样零触发
    ///
    /// **平台门控（2026-10-07 存量债清理，Linux CI 门禁）**：本用例期望「未就位 +
    /// 下载安装包」文案，而 Linux 上 `current_platform()` = `Platform::Other`，**平台门
    /// 先于 exists 门** ⇒ 返回的是「当前平台暂不支持一键配置」——那正是本模块在非产品
    /// 平台上的如实行为，不是缺陷。
    #[cfg_attr(
        not(any(target_os = "macos", target_os = "windows")),
        ignore = "平台不适用：平台门先于 exists 门，Linux 上如实返回「当前平台暂不支持一键配置」"
    )]
    #[test]
    fn install_step_refuses_when_installer_absent() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("never-downloaded.pkg");
        let r = install_step_with(current_platform(), &pkg, |_| panic!("缺包不得触达安装器"));
        let e = r.unwrap_err();
        assert!(e.contains("未就位") && e.contains("下载安装包"), "{e}");
    }

    // ==== Task 7：强制可达性校验 + 自愈（§C3）====

    /// 外部解析：只认 A / AAAA 记录（CNAME 不算"已发布"）。
    /// 样本形态取自阿里 DNS 的 JSON 接口（实测可用：`https://223.5.5.5/resolve?name=X&type=A`）。
    #[test]
    fn parse_doh_answer_extracts_addresses_only() {
        let ok = r#"{"Status":0,"Answer":[{"name":"a.ts.net.","type":1,"data":"203.0.113.10"},{"name":"a.ts.net.","type":5,"data":"b.ts.net."}]}"#;
        assert_eq!(
            parse_doh_answer(ok),
            vec!["203.0.113.10"],
            "CNAME(type 5) 不得当作已发布"
        );
        let none = r#"{"Status":0}"#;
        assert!(
            parse_doh_answer(none).is_empty(),
            "无 Answer = 记录尚未发布"
        );
        assert!(
            parse_doh_answer("not json").is_empty(),
            "解析不了按未发布处理"
        );
    }

    /// 判定门：**没有公网记录就绝不能是 Verified**（这是 §C3 的核心断言）。
    /// W-B 后该形态落在 [`Reachability::RecordPending`]（正常发布延迟，不是故障）——
    /// 门本身不变：**不是可用**。
    #[test]
    fn empty_answer_never_verifies() {
        let probe = |_host: &str| Ok(vec![]); // 假探针：权威否定（真的没有记录）
        let r = verify_reachability("a.ts.net", &probe);
        assert!(
            matches!(r, Reachability::RecordPending { .. }),
            "权威否定 = 记录尚未发布（正常发布延迟，两档时长），不是 Verified: {r:?}"
        );
        assert_ne!(r, Reachability::Verified, "没有公网记录绝不可是 Verified");
    }

    /// A2 **变异锚点**（2026-10-07 Windows 实测 + 用户裁决）：解析不到记录时的
    /// user-facing 口径必须**说明预期时长**——实测首次开通后公网 DNS 记录**约 5–6 分钟**
    /// 才发布（DoH 连续 14 次 NODATA 后才出现两条 A 记录），而这期间本机检查全绿、
    /// 用户唯一的感受就是"连不上"。文案不给时长 = 让用户在正常现象里干等。
    /// W-B：时长**分两档**（首开 5–6 分钟 / reset 后重开 30–49 秒），档位判据见
    /// `record_pending_tier_distinguishes_republish_from_first_open`。
    /// 变异：把文案改回「公网解析不到该地址」（无时长）→ 本测试必红。
    #[test]
    fn pending_record_reason_states_the_expected_delay() {
        let probe = |_host: &str| Ok(vec![]);
        match verify_reachability("a.ts.net", &probe) {
            Reachability::RecordPending { republish } => {
                let reason = record_pending_hint(republish);
                assert!(
                    reason.contains("分钟"),
                    "必须告知预期时长（否则用户以为故障）: {reason}"
                );
                assert!(
                    reason.contains("尚未发布") || reason.contains("重新发布"),
                    "必须说清这是「记录尚未发布」这一类正常现象: {reason}"
                );
            }
            other => panic!("解析不到必须是「记录尚未发布」态: {other:?}"),
        }
    }

    /// A2 防回归（用户裁决 + 2026-10-07 实测）：**可达性检查只准走 DoH(443) 与钉 IP
    /// 直连，禁止任何走系统 DNS 栈的解析**。实测教训：Tailscale 的 WFP 规则接管 53 端口，
    /// `Resolve-DnsName -Server 223.5.5.5` **也被本机劫持**（恒返回 MagicDNS 值
    /// 100.90.14.111、TTL=5）——**任何走系统 DNS 栈的检查在本机永远显示"通"**，正是
    /// §C3 事故的形态。两道锁：
    /// ① 探针发出的**每个**网络目标，主机段必须是 **IP 字面量**（URL 里没有域名 ⇒
    ///    连接器不会去解析，等价于"绕开系统解析器"）；
    /// ② 源码里不得出现系统解析 API 与域名形态的解析器地址。
    /// 变异：把 DoH 源换成 `https://dns.alidns.com/resolve`（域名形态）→ ①档必红。
    #[test]
    fn reachability_targets_ip_literals_only_and_never_the_system_resolver() {
        let urls: std::cell::RefCell<Vec<String>> = Default::default();
        let _ = probe_addresses("a.ts.net", |u| {
            urls.borrow_mut().push(u.to_string());
            "not a doh response".into() // 每个源都失败 → 走完全部源/类型，全部目标入账
        });
        assert!(!urls.borrow().is_empty(), "探针必须真的发出查询");
        for u in urls.borrow().iter() {
            let rest = u.split_once("://").expect("必须是 https 目标").1;
            let host = rest.split(['/', '?']).next().unwrap_or("");
            assert!(
                host.parse::<std::net::IpAddr>().is_ok(),
                "可达性检查的目标必须是 IP 字面量（域名形态会走系统 DNS 栈，本机被劫持）: {u}"
            );
        }
        // 源码级锁：出现系统解析 API / 域名形态的 DoH 源即红
        // **⚠ 本锁的边界（M-6 评审）**：只扫 `reach.rs` 一个文件。`status.rs` / `wizard.rs`
        // 里新加一处走系统解析器的调用（或域名形态的目标）**不会被本锁抓到**——那里出现
        // 新的网络/解析代码时，必须把文件加进本锁的扫描面或另立同款锁，别以为本锁管全局。
        let src = include_str!("reach.rs");
        for bad in [
            "lookup_host",
            "to_socket_addrs",
            "getaddrinfo",
            "dns.google",
            "dns.alidns.com",
            "cloudflare-dns.com",
        ] {
            assert!(!src.contains(bad), "不得引入走系统 DNS 栈的解析: {bad}");
        }
        // 钉 IP 直连 + 禁系统代理是在位不变量（不能只靠注释宣称）
        assert!(
            src.contains(".no_proxy()") && src.contains(".resolve("),
            "钉 IP（resolve）与禁用系统代理（no_proxy）必须留在实现里"
        );
    }

    /// B1 **变异锚点**：撤销前的**只读预览**必须能逐条说出「这次撤销会连带清掉哪些
    /// 条目」——它是前端确认框的唯一数据源；没有它，「用户明确确认后才走 disable_force」
    /// 就无从谈起（叠加形态下用户会莫名少掉自己的 serve 条目）。
    /// 预览必须**零写操作**：只发 `funnel status --json` 这一条读命令。
    /// 变异：删掉 disable_preview 分支 → 本测试必红（步不存在）；
    /// 预览里顺手发 reset → 零写操作断言必红。
    #[test]
    fn disable_preview_lists_would_be_cleared_entries_and_writes_nothing() {
        let _g = test_lock();
        const MIXED: &str = r#"{"Web":{"x.ts.net:443":{"Handlers":{
            "/":{"Proxy":"http://127.0.0.1:9420"},
            "/media":{"Proxy":"http://127.0.0.1:7000"}}}}}"#;
        *DESIRED.lock().unwrap() = Some(9420);
        // W-A 第三处判据点后预览多读一次 `status --json`（后端门）——夹具照真机形态应答
        let calls = spy_cli(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_RUNNING.into()),
            ["funnel", "status", "--json"] => Ok(MIXED.into()),
            other => Err(format!("预览不该有这条调用: {other:?}")),
        });

        let r = run_step("disable_preview", 9420).expect("预览步必须存在（确认框的数据源）");
        assert_eq!(r["ok"], true);
        assert_eq!(
            r["foreign"], false,
            "叠加形态不是外来配置（普通撤销本可放行）"
        );
        assert_eq!(
            r["wouldClear"], 1,
            "必须如实报出会一并被清除的非 兔维斯 条目数: {r}"
        );
        let entries = r["entries"].as_array().expect("必须逐条列出条目");
        assert_eq!(entries.len(), 2, "两条 handler 都要列出: {r}");
        let ours: Vec<&serde_json::Value> = entries.iter().filter(|e| e["ours"] == true).collect();
        assert_eq!(ours.len(), 1, "本机 兔维斯 那一路标 ours: {r}");
        let others: Vec<&serde_json::Value> =
            entries.iter().filter(|e| e["ours"] == false).collect();
        assert_eq!(others.len(), 1, "用户自建那一路标非 ours: {r}");
        // 逐条可读：主机 + 路径 + 目标（用户要能认出「这是我的 /media」）
        let label = others[0]["label"].as_str().unwrap_or_default();
        assert!(
            label.contains("/media") && label.contains("7000"),
            "条目描述必须能让人认出是哪一条: {label}"
        );
        // 零写操作：预览只读（W-A 后允许两条**读**命令：status 与 funnel status）
        assert!(
            calls.lock().unwrap().iter().all(|c| c
                == &[
                    "funnel".to_string(),
                    "status".to_string(),
                    "--json".to_string()
                ]
                || c == &["status".to_string(), "--json".to_string()]),
            "预览不得发出任何写命令: {:?}",
            calls.lock().unwrap()
        );
        assert_eq!(*DESIRED.lock().unwrap(), Some(9420), "预览不得动期望态");
        teardown_globals();
    }

    /// A6 **变异锚点**（2026-10-07 Windows 实测）：**`--bg` 是必需的**——不加 `--bg` 时
    /// `tailscale funnel <port>` 会打印横幅 + `Press Ctrl+C to exit.` 后**一直阻塞终端**。
    /// 兔维斯 是 GUI 应用，任何阻塞调用线程的形态都不可接受，故本模块**一律带 `--bg`**。
    /// 本测试把整条起停 + 自愈链路跑一遍，逐条核对 CLI 调用形状：凡 `funnel` 且非
    /// status/reset 的调用必须带 `--bg`，且绝不出现 `["funnel", "<端口>"]` 这种会阻塞的形态。
    /// 变异：把 enable_funnel 的 `["funnel", "--bg", port]` 改成 `["funnel", port]` → 必红。
    #[test]
    fn every_funnel_enable_call_passes_bg_or_it_would_block_the_terminal() {
        let _g = test_lock();
        let calls: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>> = Default::default();
        let c = calls.clone();
        set_run_cli_override(Some(Box::new(move |args: &[&str]| {
            let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            c.lock().unwrap().push(v.clone());
            match args {
                ["funnel", "status", "--json"] => Ok("{}".into()),
                ["funnel", "status"] => Ok(String::new()),
                ["set", "--shields-up=false"] => Ok(String::new()),
                ["get", "--json"] => Ok(r#"{"shields-up": false}"#.into()),
                ["funnel", "reset"] => Ok(String::new()),
                ["status", "--json"] => Ok(STATUS_RUNNING.into()),
                _ if args.first() == Some(&"funnel") && args.contains(&"--bg") => Ok(String::new()),
                other => Err(format!("测试未应答的 CLI 调用: {other:?}")),
            }
        })));
        set_reachability(Reachability::Unverified);
        start_channel_inner(9420).expect("起通道（走 enable_funnel 原语）");
        stop_channel().expect("停通道");
        // 自愈路径同走一条原语（reset + 重开），一并纳入核对
        *DESIRED.lock().unwrap() = Some(9420);
        let token = begin_verification("jarvismac-mini.example-tailnet.ts.net", 9420);
        let _ = heal_and_reverify(9420, &token);
        let recorded = calls.lock().unwrap().clone();
        let is_enable = |c: &Vec<String>| {
            c.first().map(String::as_str) == Some("funnel")
                && !matches!(c.get(1).map(String::as_str), Some("status") | Some("reset"))
        };
        let enables: Vec<&Vec<String>> = recorded.iter().filter(|c| is_enable(c)).collect();
        assert!(
            !enables.is_empty(),
            "本测试必须真的观察到开通调用（否则等于没锁）: {recorded:?}"
        );
        for c in enables {
            assert!(
                c.iter().any(|a| a == "--bg"),
                "funnel 开通必须带 --bg（实测不加会阻塞终端）: {c:?}"
            );
        }
        assert!(
            !recorded.iter().any(|c| c.len() == 2
                && c[0] == "funnel"
                && c[1].chars().all(|ch| ch.is_ascii_digit())),
            "绝不出现 `funnel <端口>` 裸形态（会阻塞终端）: {recorded:?}"
        );
        teardown_globals();
    }

    /// A5 **变异锚点**（2026-10-07 Windows 实测）：CLI 只在**完整安装路径**上找——
    /// `Get-Command tailscale` 为空（不在 PATH）、卸载登记项 `InstallLocation=（空）`
    /// （`WindowsInstaller=1` + `UninstallString=MsiExec.exe /X{...}` = MSI 版）。
    /// 故候选表只装标准安装位置的完整路径：**不得**回落 PATH 查找，**不得**读
    /// InstallLocation。变异：Windows 候选改成裸命令名 "tailscale"（PATH 查找）→ 本测试必红。
    ///
    /// ⚠️ 2026-10-08 补：本测试锁的是**这张静态表**（仍为"逐平台固定完整路径"）。候选链
    /// 自本日起多了一段**动态**来源（Windows = 服务登记 `ImagePath`，见
    /// [`status::discovered_cli`]）——动态来源**不进这张表**（它依赖运行时注册表读数，
    /// 塞进 `&'static [&'static str]` 既装不下、也会把"表 = 事实"这个前提弄脏）；
    /// 顺序/fail-closed 契约由 `default_install_path_still_wins_and_second_source_stays_lazy`
    /// 与 `cli_discovery_fails_closed_when_no_source_matches` 单独锁。
    #[test]
    fn cli_candidates_are_full_paths_only() {
        assert_eq!(
            cli_candidates(Platform::Windows),
            &[r"C:\Program Files\Tailscale\tailscale.exe"],
            "Windows 候选必须是标准安装位置的完整路径"
        );
        for p in [Platform::Mac, Platform::Windows, Platform::Other] {
            for c in cli_candidates(p) {
                assert!(
                    c.starts_with('/') || c.starts_with(r"C:\"),
                    "{p:?} 候选必须是完整路径（裸命令名 = PATH 查找，实测找不到）: {c}"
                );
            }
        }
        // 表非空（空表 = 永远「未安装」，那是另一种谎报）
        for p in [Platform::Mac, Platform::Windows, Platform::Other] {
            assert!(!cli_candidates(p).is_empty(), "{p:?} 候选表不得为空");
        }
    }

    // ==== 2026-10-08：CLI 发现链补「服务登记」第二来源（治本）====
    //
    // **缺口（用户实测）**：Windows MSI 让用户自选安装路径（兔维斯 执行 `msiexec /i <包>`，
    // **刻意不加 /qn**——选择权本就该给用户），而 find_cli 只在
    // `C:\Program Files\Tailscale` 找。用户装到 `D:\软件\Tailscale`（带中文）⇒ 找不到
    // ⇒ detect 判「没装」⇒ 向导又下载又安装 ⇒ 装完还是找不到（可能死循环）。
    // 讽刺之处：路径选择框是 兔维斯 自己弹出来的，用户照做之后 兔维斯 就瞎了。
    //
    // **治本判据**：服务登记 `HKLM\SYSTEM\CurrentControlSet\Services\Tailscale\ImagePath`
    // 指向**真实安装位置**，与盘符/目录名/中文都无关。**为什么选注册表而不是 `sc qc`**：
    // 见 `status.rs::service_image_path` 的文档（`sc qc` 的字段名随系统显示语言本地化，
    // 中文机上根本不是 `BINARY_PATH_NAME`；且它要派生子进程）。卸载登记的
    // `InstallLocation` 实测为**空**，那条路已被 2026-10-07 真机排除。
    //
    // **本机（macOS）无法验证 Windows 注册表值的真实形态** ⇒ 解析写成**注入式纯函数**
    // （存在性判据由调用方注入），全部形态在本组注入测试里锁死；唯一接触注册表的
    // `service_image_path`（`#[cfg(windows)]`）只能由真机复核。

    /// **形态锚点**：注册表子键路径逐字锁死（改路径 = 找不到服务 = 本测试必红）。
    /// 末段 `Tailscale` 即 MSI 装出来的服务名（改了同样必红）。
    #[test]
    fn service_image_path_registry_key_is_the_real_one() {
        assert_eq!(
            TS_SERVICE_REG_PATH, r"SYSTEM\CurrentControlSet\Services\Tailscale",
            "服务登记的注册表子键（HKLM 下），末段是服务名"
        );
    }

    /// 带引号 + 带参数（MSI 装出来的常态形态）：取**第一对引号内**的内容——路径里的
    /// 空格因此不构成歧义；**且不得为此触碰文件系统**（引号形态零歧义，注入的存在性
    /// 判据一旦被调用就该炸）。
    #[test]
    fn image_path_quoted_with_args_parses_exe_without_fs_probe() {
        let raw = r#""C:\Program Files\Tailscale\tailscaled.exe" --state=C:\ProgramData\Tailscale\tailscaled.state --port=0"#;
        let got = parse_service_image_path_with(raw, |_| {
            panic!("带引号形态无歧义，解析不得触碰文件系统")
        });
        assert_eq!(
            got.as_deref(),
            Some(r"C:\Program Files\Tailscale\tailscaled.exe"),
            "引号内即 exe 路径，参数（含路径形态的参数）一概不吃进来"
        );
    }

    /// 带引号 + 不带参数：同样只取引号内内容。
    #[test]
    fn image_path_quoted_without_args_parses_exe() {
        let raw = r#""D:\软件\Tailscale\tailscaled.exe""#;
        let got = parse_service_image_path_with(raw, |_| panic!("带引号形态不得触碰文件系统"));
        assert_eq!(
            got.as_deref(),
            Some(r"D:\软件\Tailscale\tailscaled.exe"),
            "用户实测的非默认路径（带中文）必须原样解析出来"
        );
    }

    /// 不带引号 + 带参数（路径无空格）：按 Windows 的 `CreateProcess` 语义在**空白处**
    /// 逐个加长成候选，取**真实存在**者——参数不得混进 exe 路径。
    #[test]
    fn image_path_unquoted_with_args_parses_exe() {
        let raw = r"C:\Tailscale\tailscaled.exe --port=0";
        let exists = |p: &str| p == r"C:\Tailscale\tailscaled.exe";
        assert_eq!(
            parse_service_image_path_with(raw, exists).as_deref(),
            Some(r"C:\Tailscale\tailscaled.exe"),
            "无引号时必须靠「存在性」把参数切掉"
        );
    }

    /// 不带引号 + **路径含空格** + 不带参数（Windows 的经典歧义形态）：整串就是那个
    /// 存在的文件 ⇒ 必须解析出完整路径，不能截成 `C:\Program`。
    #[test]
    fn image_path_unquoted_path_with_spaces_parses_whole_exe() {
        let raw = r"C:\Program Files\Tailscale\tailscaled.exe";
        let exists = |p: &str| p == r"C:\Program Files\Tailscale\tailscaled.exe";
        assert_eq!(
            parse_service_image_path_with(raw, exists).as_deref(),
            Some(r"C:\Program Files\Tailscale\tailscaled.exe"),
            "含空格的整串存在 ⇒ 整串就是 exe（不得截成第一个空白前的段）"
        );
    }

    /// 不带引号 + **路径含空格** + 带参数：最长存在候选 = exe。
    #[test]
    fn image_path_unquoted_path_with_spaces_and_args_parses_exe() {
        let raw = r"C:\Program Files\Tailscale\tailscaled.exe --state=C:\ProgramData\Tailscale\tailscaled.state";
        let exists = |p: &str| p == r"C:\Program Files\Tailscale\tailscaled.exe";
        assert_eq!(
            parse_service_image_path_with(raw, exists).as_deref(),
            Some(r"C:\Program Files\Tailscale\tailscaled.exe"),
            "含空格 + 带参数：取「存在且最长」的候选"
        );
    }

    /// 前后空白（含 CRLF / Tab）：注册表值实测可能带尾随空格 ⇒ 一律先 trim。
    #[test]
    fn image_path_trims_surrounding_whitespace() {
        let raw = "  \t\"C:\\Program Files\\Tailscale\\tailscaled.exe\" --port=0\r\n ";
        assert_eq!(
            parse_service_image_path_with(raw, |_| panic!("trim 后仍是引号形态，不得触碰 FS"))
                .as_deref(),
            Some(r"C:\Program Files\Tailscale\tailscaled.exe"),
            "首尾空白（空格/Tab/CRLF）必须先剥掉"
        );
    }

    /// 大小写：解析**原样保留**大小写（Windows 路径大小写不敏感，但不得擅自改写用户机器上
    /// 的真实拼写——路径要拿去原样启动进程）。
    #[test]
    fn image_path_keeps_original_case() {
        let raw = r#""C:\PROGRAM FILES\TAILSCALE\TAILSCALED.EXE" --State=X"#;
        assert_eq!(
            parse_service_image_path_with(raw, |_| panic!("引号形态不得触碰 FS")).as_deref(),
            Some(r"C:\PROGRAM FILES\TAILSCALE\TAILSCALED.EXE"),
            "全大写形态必须原样解析（不得小写化/改写）"
        );
    }

    /// **fail-closed**：形态不可解析 / 无存在者一律 `None`——**绝不返回一个猜的路径**
    /// （返回猜的路径 = 把「没装」谎报成「装了」，随后 CLI 调用会以莫名错误失败）。
    #[test]
    fn image_path_unparsable_forms_yield_none() {
        let never = |_: &str| false;
        for raw in [
            "",                                           // 空值
            "   \r\n ",                                   // 全空白
            "\"\"",                                       // 只有一对空引号
            "\"   \"",                                    // 引号内全空白
            r#""C:\Tailscale\tailscaled.exe"#,            // 只有开引号（形态残缺）
            r"C:\Nope\tailscaled.exe --port=0",           // 无引号且候选都不存在
            r"C:\Program Files\Tailscale\tailscaled.exe", // 无引号、含空格、整串不存在
        ] {
            assert_eq!(
                parse_service_image_path_with(raw, never),
                None,
                "不可解析/无存在者的形态必须如实 None：{raw:?}"
            );
        }
    }

    /// **M6（2026-10-08 架构评审）：`REG_EXPAND_SZ` 必须展开 `%VAR%`。** 旧实现按字符串读
    /// **但不展开** —— 于是 `ImagePath` 写成 `%ProgramFiles%\Tailscale\...` 的机器上，这条
    /// "第二来源"会**静默失效**（fail-closed 成"未安装"：安全，但治不了本）。展开语义对齐
    /// `ExpandEnvironmentStringsW`，**查找函数注入** ⇒ 任何宿主可测（本机只有 macOS）。
    #[test]
    fn image_path_expand_sz_is_expanded() {
        let lookup = |n: &str| match n {
            "ProgramFiles" => Some(r"C:\Program Files".to_string()),
            _ => None,
        };
        let expanded = image_path_value(r"%ProgramFiles%\Tailscale\tailscaled.exe", true, lookup);
        assert_eq!(
            expanded, r"C:\Program Files\Tailscale\tailscaled.exe",
            "REG_EXPAND_SZ ⇒ 必须展开成真实路径，否则第二来源静默失效"
        );
        // 与后面的解析链串起来：展开后的串必须还能推出同目录的 CLI
        let exe = parse_service_image_path_with(&format!("\"{expanded}\" --port=0"), |_| {
            panic!("带引号形态不得触碰 FS")
        })
        .expect("展开后的形态应当可解析");
        assert_eq!(
            cli_beside_service_exe(&exe).as_deref(),
            Some(r"C:\Program Files\Tailscale\tailscale.exe"),
            "展开 → 解析 exe → 同目录 CLI：整条第二来源在 REG_EXPAND_SZ 形态下必须仍然成立"
        );
    }

    /// **按值的真实类型决定是否展开**（不是"凡串皆展开"）：`REG_SZ` 里合法出现的 `%`
    /// 不得被改写——注入的查找函数一旦被调用就该炸。
    #[test]
    fn image_path_sz_is_not_expanded() {
        let raw = r"C:\100%\Tailscale\tailscaled.exe";
        assert_eq!(
            image_path_value(raw, false, |_| panic!("REG_SZ 不得做变量查找")),
            raw,
            "值类型是 REG_SZ ⇒ 原样返回（不展开、不查环境变量）"
        );
    }

    /// 展开的**边界语义**（对齐 `ExpandEnvironmentStringsW`：查不到 / 形态残缺一律
    /// **原样保留**，绝不吞字符、绝不落空）：未知变量、缺配对的单个 `%`、空名 `%%`、
    /// 无 `%` —— 四种都逐字保留；已知变量才替换。
    #[test]
    fn env_expansion_keeps_unknown_and_malformed_refs_verbatim() {
        let lookup = |n: &str| (n == "Known").then(|| "V".to_string());
        for raw in [
            r"%Unknown%\x.exe",
            r"C:\50%\x.exe",
            r"%%\x.exe",
            r"C:\Tailscale\tailscaled.exe",
        ] {
            assert_eq!(
                image_path_value(raw, true, lookup),
                raw,
                "查不到/形态残缺的引用必须原样保留（Windows 语义）：{raw:?}"
            );
        }
        assert_eq!(
            image_path_value(r"%Known%\x.exe", true, lookup),
            r"V\x.exe",
            "已知变量必须替换"
        );
        // 多个引用 + 变量值里再带 `%`（替换结果不得被二次展开——单趟扫描）
        assert_eq!(
            image_path_value(r"%Known%\%Known%\x.exe", true, |_| Some("K".into())),
            r"K\K\x.exe"
        );
    }

    /// 形态针：唯一接触注册表的 `service_image_path`（Windows-only，本机跑不到）必须
    /// **读原始值拿 vtype** 并**经纯函数展开**——接线断了上面那些纯函数测试就白测了。
    /// 变异：退回 `get_value::<String,_>`（拿不到类型信息）/ 恒不展开 / 不接
    /// `image_path_value` → 必红。
    #[test]
    fn registry_reader_wires_expand_sz_through_the_pure_expander() {
        let src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/remote/tailscale/status.rs"
        ))
        .expect("读 status.rs");
        let start = src
            .find("fn service_image_path()")
            .expect("注册表读取点必须存在（第二来源的第一跳）");
        let body = &src[start..];
        let end = body.find("\n}\n").expect("函数体结束");
        let body = &body[..end];
        assert!(
            body.contains("get_raw_value("),
            "必须读**原始值**才拿得到 vtype（`get_value::<String,_>` 把 REG_SZ / \
             REG_EXPAND_SZ 归一成同一个 String，类型信息就丢了）: {body}"
        );
        assert!(
            body.contains("REG_EXPAND_SZ"),
            "必须认出 REG_EXPAND_SZ（形态门）: {body}"
        );
        assert!(
            body.contains("image_path_value("),
            "展开必须经纯函数 image_path_value（否则上面几条纯函数测试与生产脱钩）: {body}"
        );
    }

    /// 由服务本体的 exe 推 CLI 路径（纯函数）：Tailscale 的服务跑的是 **`tailscaled.exe`**，
    /// CLI 是**同目录**下的 `tailscale.exe` 兄弟文件 ⇒ 取同目录 + 换名。
    /// **不用 `std::path::Path`**：本函数处理的是 Windows 形态路径串（反斜杠分隔），而
    /// `Path` 的分隔符语义**跟随编译宿主**——在 macOS/Linux 上 `\` 不是分隔符，
    /// `parent()` 会返回空串，测试根本跑不了。故自己按 `\` / `/` 取最后一段分隔符。
    #[test]
    fn cli_beside_service_exe_uses_same_directory() {
        assert_eq!(
            cli_beside_service_exe(r"C:\Program Files\Tailscale\tailscaled.exe").as_deref(),
            Some(r"C:\Program Files\Tailscale\tailscale.exe"),
            "服务本体是同目录的 tailscaled.exe ⇒ CLI 是它的同目录兄弟"
        );
        // 用户实测场景：非默认盘符 + 中文目录名
        assert_eq!(
            cli_beside_service_exe(r"D:\软件\Tailscale\tailscaled.exe").as_deref(),
            Some(r"D:\软件\Tailscale\tailscale.exe"),
            "非默认路径 + 中文目录（用户实测形态）必须推到同目录 CLI"
        );
        // 服务若直接跑 CLI 本体（上游变更），同目录 + 换名仍得到同一条路径
        assert_eq!(
            cli_beside_service_exe(r"D:\软件\Tailscale\tailscale.exe").as_deref(),
            Some(r"D:\软件\Tailscale\tailscale.exe"),
            "服务跑的就是 CLI 本体时结果不变（对上游变更免疫）"
        );
    }

    /// 大小写 + 正斜杠 + 根目录：同目录推导对形态不敏感。
    #[test]
    fn cli_beside_service_exe_handles_case_slashes_and_root() {
        assert_eq!(
            cli_beside_service_exe(r"C:\TAILSCALE\TAILSCALED.EXE").as_deref(),
            Some(r"C:\TAILSCALE\tailscale.exe"),
            "大写目录名必须原样保留（CLI 名按上游固定小写）"
        );
        assert_eq!(
            cli_beside_service_exe("C:/Tailscale/tailscaled.exe").as_deref(),
            Some("C:/Tailscale/tailscale.exe"),
            "正斜杠形态（等价写法）同样要能推到同目录"
        );
        assert_eq!(
            cli_beside_service_exe(r"C:\tailscaled.exe").as_deref(),
            Some(r"C:\tailscale.exe"),
            "盘根目录下的服务本体"
        );
    }

    /// **fail-closed**：没有目录成分（裸文件名）= 无从知道装在哪 ⇒ `None`（不猜，
    /// 绝不返回一个相对路径让调用方在当前工作目录里瞎找）。
    #[test]
    fn cli_beside_service_exe_rejects_bare_filename() {
        assert_eq!(cli_beside_service_exe("tailscaled.exe"), None);
        assert_eq!(cli_beside_service_exe(""), None);
    }

    /// **候选链顺序锚点（只增不改语义）**：默认位置**仍最优先**，且第二来源必须**惰性**
    /// ——默认位置命中时**连读都不许去读**服务登记（`panic` 替身证明：一旦提前求值就炸）。
    /// 变异：把服务登记排到默认位置之前 → 本测试立即红。
    #[test]
    fn default_install_path_still_wins_and_second_source_stays_lazy() {
        let got = find_cli_with(
            &[r"C:\Program Files\Tailscale\tailscale.exe"],
            || panic!("默认位置已命中，不得求值第二来源（顺序锚点）"),
            |p| p == std::path::Path::new(r"C:\Program Files\Tailscale\tailscale.exe"),
        );
        assert_eq!(
            got,
            Some(PathBuf::from(r"C:\Program Files\Tailscale\tailscale.exe")),
            "默认位置命中时必须原样返回默认位置（第二来源只在前面全落空时才用）"
        );
    }

    /// **治本**：默认位置落空时启用服务登记推出来的路径——这正是用户实测场景
    /// （装到 `D:\软件\Tailscale`）。
    #[test]
    fn service_registration_is_used_when_default_is_missing() {
        let service_cli = r"D:\软件\Tailscale\tailscale.exe";
        let got = find_cli_with(
            &[r"C:\Program Files\Tailscale\tailscale.exe"],
            || Some(PathBuf::from(service_cli)),
            |p| p == std::path::Path::new(service_cli),
        );
        assert_eq!(
            got,
            Some(PathBuf::from(service_cli)),
            "默认位置落空 ⇒ 必须启用服务登记来源（否则向导会陷入又下载又安装的死循环）"
        );
    }

    /// **fail-closed 收口**：第二来源给出路径后**仍要过存在性门**；两处都落空 ⇒ `None`。
    /// 绝不把「猜的路径」交给调用方（随后 CLI 调用会以莫名错误失败，比如实报「没装」更坏）。
    #[test]
    fn cli_discovery_fails_closed_when_no_source_matches() {
        // ① 服务读不到 / 形态不可解析 ⇒ 第二来源 None
        let got = find_cli_with(
            &[r"C:\Program Files\Tailscale\tailscale.exe"],
            || None,
            |_| false,
        );
        assert_eq!(got, None, "两处都没有必须如实 None");
        // ② 第二来源给出路径但该文件不存在 ⇒ 同样 None（存在性门在链尾统一收口）
        let got = find_cli_with(
            &[r"C:\Program Files\Tailscale\tailscale.exe"],
            || Some(PathBuf::from(r"D:\软件\Tailscale\tailscale.exe")),
            |_| false,
        );
        assert_eq!(
            got, None,
            "第二来源给出的路径也必须过存在性门，不得直接采信"
        );
    }

    /// **平台门控**：本机（非 Windows）第二来源恒 `None` ⇒ `find_cli` 的行为与改动前
    /// **逐字相同**（macOS 是固定 .app 位置、Linux 走包管理器路径，不存在"用户自选路径"
    /// 这个问题；也不该在非 Windows 上做任何多余 IO）。
    #[cfg(not(windows))]
    #[test]
    fn second_source_is_absent_off_windows() {
        assert_eq!(
            discovered_cli(),
            None,
            "非 Windows 不得启用服务登记来源（发现链与改动前行为一致）"
        );
    }

    /// A4 定性（二选一，**选 ②「保留以备上游变更」并写明实测依据**）。
    ///
    /// 2026-10-07 实测：**Funnel 始终只发布 A 记录，AAAA 不发布**——记录发布后 DoH 返回
    /// 两条 A（两个入口 IP），同一域名的 AAAA 查询**始终 NODATA**（14 次轮询无一例外）。
    /// 所以 A→AAAA 回落**当前永远不会产出地址**。**为什么不删**：它是上游形态变更的
    /// **廉价保险**（若 ts.net 某天改发 AAAA，我们不会把「有地址」误判成「记录未发布」
    /// 而让用户白等），成本 = 仅在 A 被权威否定后才多发一次请求（常态路径零额外开销）；
    /// **为什么必须写在这里**：不留"不说话的死代码"——已实测无用就明说无用，并锁住它的
    /// 真实边界（不是能救场的分支，是保险）。
    ///
    /// 本测试锁两件事：① A 有记录时**绝不多问** AAAA（常态路径零开销）；
    /// ② A 被权威否定后**确实**会问 AAAA（保险在位，不是被悄悄删掉）。
    /// 变异：删掉 AAAA 回落（只查 A）→ ②档必红。
    #[test]
    fn aaaa_fallback_is_kept_as_upstream_change_insurance() {
        // ① 实测常态：A 有记录 → 一次查询即结束，不碰 AAAA
        let urls: std::cell::RefCell<Vec<String>> = Default::default();
        let addrs = probe_addresses("a.ts.net", |u| {
            urls.borrow_mut().push(u.to_string());
            r#"{"Status":0,"Answer":[{"type":1,"data":"203.0.113.10"},{"type":1,"data":"203.0.113.11"}]}"#
                .into()
        });
        assert_eq!(
            addrs.unwrap(),
            vec!["203.0.113.10".to_string(), "203.0.113.11".to_string()],
            "实测形态：发布后是两个入口 A 记录（两条都收）"
        );
        assert_eq!(urls.borrow().len(), 1, "A 有记录时绝不多问 AAAA");
        assert!(urls.borrow()[0].contains("type=A"));
        // ①b 实测否定面：同一域名 AAAA 恒 NODATA（权威否定，不是"验证不了"）
        let aaaa_nodata = probe_addresses("a.ts.net", |u| {
            if u.contains("type=AAAA") {
                r#"{"Status":0,"Question":{}}"#.into() // 实测：AAAA 无记录
            } else {
                r#"{"Status":0,"Answer":[{"type":1,"data":"203.0.113.10"}]}"#.into()
            }
        });
        assert_eq!(
            aaaa_nodata.unwrap(),
            vec!["203.0.113.10".to_string()],
            "AAAA 不发布不影响 A 的结论"
        );
        // ② 保险在位：A 权威否定 + AAAA 有记录（上游改发 AAAA 的假想形态）仍能解析到
        let urls2: std::cell::RefCell<Vec<String>> = Default::default();
        let addrs2 = probe_addresses("a.ts.net", |u| {
            urls2.borrow_mut().push(u.to_string());
            if u.contains("type=AAAA") {
                r#"{"Status":0,"Answer":[{"type":28,"data":"2001:db8::1"}]}"#.into()
            } else {
                r#"{"Status":0}"#.into() // A 权威否定
            }
        });
        assert_eq!(
            addrs2.unwrap(),
            vec!["2001:db8::1".to_string()],
            "AAAA 回落是上游变更的保险（不是能救场的常态分支，但不能没有）"
        );
        assert!(
            urls2.borrow().iter().any(|u| u.contains("type=AAAA")),
            "A 权威否定后必须真的问 AAAA（否则保险形同虚设）: {:?}",
            urls2.borrow()
        );
    }

    /// A3 **变异锚点**（2026-10-07 复测）：DoH 必须**真的多源**——按序尝试，前一个
    /// 失败/超时/非权威应答就换下一个。实测本网络下 `dns.google`(8.8.8.8) 的 DoH
    /// **无响应**，只有 223.5.5.5 可用 ⇒ 单一源 = 该源一挂就永远「未验证」。
    /// 变异：把 probe_addresses 改回只问 DOH_SOURCES[0] → 本测试必红（记录在次源上）。
    #[test]
    fn doh_falls_over_to_the_second_source_when_the_first_fails() {
        let urls: std::cell::RefCell<Vec<String>> = Default::default();
        let addrs = probe_addresses("a.ts.net", |u| {
            urls.borrow_mut().push(u.to_string());
            if u.contains("223.5.5.5") {
                String::new() // 首选源不可用（超时/无应答）
            } else {
                r#"{"Status":0,"Answer":[{"type":1,"data":"203.0.113.7"}]}"#.into()
            }
        });
        assert_eq!(
            addrs.expect("次源可用即算解析成功"),
            vec!["203.0.113.7".to_string()],
            "首选源失败时必须切换到次源并采信其次源结果"
        );
        assert!(
            urls.borrow().iter().any(|u| u.contains("223.5.5.5")),
            "必须先试首选源（实测可用者优先）: {:?}",
            urls.borrow()
        );
        assert!(
            urls.borrow().iter().any(|u| u.contains("223.6.6.6")),
            "首源失败必须切到次源（2026-10-07 复测实测可达的那一个）: {:?}",
            urls.borrow()
        );
        assert_eq!(
            urls.borrow().len(),
            2,
            "A 查询即命中：两源各问一次，不做多余请求"
        );
    }

    /// I-2 **变异锚点 + 源码锁**（评审 2026-10-07 复测）：**表里不得有纸面源**——
    /// 每个源都必须是本网络**实测可达**的 IP 字面量。旧表的次源 `1.1.1.1` 自陈「本网络
    /// 未实测」，复测 `nc -z 1.1.1.1 443` 不通 ⇒ 冗余只是名义上的，A3 要修的那个失败
    /// 模式（单源挂掉就永远未验证）原样存在。
    ///
    /// 三条锁：
    /// ① 实测可达者必须在表里；② 复测**明确不可达**者不得在表里（纸面源即缺陷）；
    /// ③ 每条源在源码注释里必须有**带日期的实测记录**（host 与「实测 … 200」同行）——
    ///    新增一个源却不留下实测依据，本锁必红。
    /// 逐源连通性冒烟（打一次，非单测；零网络红线不给单测做网络请求）：
    /// `scripts/check-doh-sources.sh`
    /// 变异：把次源换回 `https://1.1.1.1/dns-query` → ②③ 档必红。
    #[test]
    fn doh_source_table_carries_measured_evidence_and_no_paper_source() {
        // ① 2026-10-07 复测可达（200）者必须在表里
        for ip in ["223.5.5.5", "223.6.6.6", "1.12.12.12"] {
            assert!(
                DOH_SOURCES.iter().any(|s| s.contains(ip)),
                "实测可达的源必须在表里: {ip} / {DOH_SOURCES:?}"
            );
        }
        // ② 复测明确不可达者（nc -z <ip> 443 不通 / curl 000）不得留在表里
        for dead in [
            "1.1.1.1",
            "8.8.8.8",
            "119.29.29.29",
            "180.76.76.76",
            "9.9.9.9",
            "94.140.14.14",
            "120.53.53.53",
        ] {
            assert!(
                !DOH_SOURCES.iter().any(|s| s.contains(dead)),
                "不可达的源不得留在表里（纸面源 = A3 要修的失败模式本身）: {dead} / {DOH_SOURCES:?}"
            );
        }
        // ③ 每条源都要有带日期的实测注释（源码锁：纸面源进表即红）
        let src = include_str!("reach.rs");
        for s in DOH_SOURCES {
            let host = s
                .split_once("://")
                .map(|(_, r)| r)
                .unwrap_or(s)
                .split(['/', '?'])
                .next()
                .unwrap_or(s);
            assert!(
                src.lines().any(|l| l.trim_start().starts_with("//")
                    && l.contains(host)
                    && l.contains("实测")
                    && l.contains("200")),
                "每个 DoH 源都必须在 reach.rs 注释里留下带日期的实测记录（host + 实测 + 200 同行）: {host}"
            );
        }
        // ④ 冒烟脚本必须在（换网络/加源时的实测出口；删掉它这条纪律就没有落地工具了）
        assert!(
            std::path::Path::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../scripts/check-doh-sources.sh"
            ))
            .exists(),
            "逐源连通性冒烟脚本不得缺失: scripts/check-doh-sources.sh"
        );
    }

    /// **I-5 反例①（发布传播期）变异锚点**：一个解析器已返回 A、另一个权威 NODATA
    /// 在记录传播期很常见——旧探针在 `NoRecord` 上直接 `break`，**不再问次源** ⇒ 报
    /// 「记录尚未发布」并把**已经可用**的地址继续藏起来（红线①「绝不把打不开的地址报成
    /// 可用」的镜像面：也绝不把能用的地址藏成「尚未发布」）。
    /// 变异：把 `NoRecord` 分支改回 `break`（不再问后续源）→ 本测试必红。
    #[test]
    fn nodata_from_one_source_must_not_stop_asking_the_next_source() {
        let urls: std::cell::RefCell<Vec<String>> = Default::default();
        let addrs = probe_addresses("a.ts.net", |u| {
            urls.borrow_mut().push(u.to_string());
            if u.contains("223.5.5.5") {
                r#"{"Status":0}"#.into() // 首选源：NOERROR 无记录（NODATA）
            } else {
                // 次源：记录已发布（传播期两源不一致的实测形态）
                r#"{"Status":0,"Answer":[{"type":1,"data":"203.0.113.9"}]}"#.into()
            }
        });
        assert_eq!(
            addrs.expect("次源已有记录就必须采信（不得因首源 NODATA 就不再问）"),
            vec!["203.0.113.9".to_string()]
        );
        assert!(
            urls.borrow().iter().any(|u| u.contains("223.6.6.6")),
            "首源权威否定后必须继续问次源: {:?}",
            urls.borrow()
        );
    }

    /// **I-5 反例② 变异锚点（成因不得错认）**：A 在**所有源**都 `Unusable`（解析服务故障）、
    /// AAAA 在某源拿到权威否定 ⇒ 旧实现仍返回 `Ok(空)` ⇒ 判「记录尚未发布」，而 A
    /// **从未得到过权威答复**——正是模块头注释承诺「不把自家解析故障说成上游的发布延迟」
    /// 的那一类（方向 fail-closed 没错，但**成因**说错了，用户会白等 5 分钟）。
    /// 新语义：A / AAAA **分别记账**，只有**被问到的类型都拿到权威否定**才 `Ok(空)`。
    /// 变异：退回「任一类型权威否定即 Ok(空)」→ 本测试必红。
    #[test]
    fn one_type_denied_while_the_other_is_unusable_is_not_a_published_record_verdict() {
        // A：全源不可用（空串 = 该源 Unusable）；AAAA：权威否定。
        // 用具名 fn（零捕获、'static）——ProbeFn 是 'static 的 trait object
        fn fetch(u: &str) -> String {
            if u.ends_with("type=A") {
                String::new()
            } else {
                r#"{"Status":0}"#.into()
            }
        }
        let e = probe_addresses("a.ts.net", fetch)
            .expect_err("A 从未得到权威答复 ⇒ 必须报「无法验证」，不得报「记录尚未发布」");
        assert!(e.contains("无有效应答"), "必须如实说源不可用: {e}");
        // 端到端：成因文字不得指向「记录尚未发布 / 重开发布中」
        let probe = |h: &str| probe_addresses(h, fetch);
        match verify_reachability("a.ts.net", &probe) {
            Reachability::Failed { reason } => {
                assert!(
                    !reason.contains("尚未发布")
                        && !reason.contains(RECORD_PENDING_HINT)
                        && !reason.contains("生效中"),
                    "不得把自家解析故障谎报成上游发布延迟: {reason}"
                );
            }
            other => panic!("全源不可用必须 fail-closed 成 Failed: {other:?}"),
        }
    }

    /// A3 要求：**全部源都失败 ⇒ 如实报「未验证」**（fail-closed），
    /// **不得**把「源不可用」谎报成「记录尚未发布」——后者含义完全不同（是正常发布延迟），
    /// 会让用户白等 5 分钟而实际问题在解析服务上。
    /// 变异：让全源失败返回 Ok(空) → 本测试必红（成因被谎报成"记录未发布"）。
    #[test]
    fn all_doh_sources_failing_reports_unverified_not_a_false_nodata_claim() {
        // 源表本身：至少两源，且都是 IP 字面量（域名形态会走系统 DNS 栈，实测被劫持）
        assert!(
            DOH_SOURCES.len() >= 2,
            "必须真的多源（单源挂掉就永远未验证）: {DOH_SOURCES:?}"
        );
        for src in DOH_SOURCES {
            let host = src.split_once("://").expect("https 源").1;
            let host = host.split(['/', '?']).next().unwrap_or("");
            assert!(
                host.parse::<std::net::IpAddr>().is_ok(),
                "DoH 源必须是 IP 字面量（不得走系统 DNS 栈）: {src}"
            );
        }
        assert!(
            DOH_SOURCES.iter().any(|s| s.contains("223.5.5.5")),
            "实测可用的首选源必须在表里"
        );
        // 全部源不可用 → Err（而不是「没有记录」）
        let r = probe_addresses("a.ts.net", |_u| String::new());
        assert!(
            r.is_err(),
            "全部源不可用必须如实报错（不得当成『记录尚未发布』）: {r:?}"
        );
        // 端到端：verify_reachability 的成因不得指向「记录未发布」
        let probe = |h: &str| probe_addresses(h, |_u| String::new());
        match verify_reachability("a.ts.net", &probe) {
            Reachability::Failed { reason } => {
                assert!(
                    reason.contains("无法验证") || reason.contains("不可用"),
                    "必须如实说『验证不了』: {reason}"
                );
                assert!(
                    !reason.contains("记录尚未发布"),
                    "全源失败 ≠ 记录未发布（不得谎报成因）: {reason}"
                );
            }
            other => panic!("全源失败必须 fail-closed 成 Failed: {other:?}"),
        }
    }

    /// B-M7 **变异锚点**：① 解析到多条地址时**逐条尝试**——首条陈旧/不可达不得直接
    /// 误报 Failed（旧实现只探 `addrs[0]`）；② A 查询无记录时必须回落查 AAAA
    /// （A4 定性：实测 Funnel 只发 A，故这条回落当前不产出地址，保留为上游变更的保险
    /// ——边界与依据见 `aaaa_fallback_is_kept_as_upstream_change_insurance` 与
    /// `reach::probe_addresses` 的 A4 注释）。
    /// 变异自证：verify_reachability 改回只看 addrs[0] → ①档红；probe_addresses 去掉
    /// AAAA 回落 → ②档红。
    #[test]
    fn reachability_tries_multiple_addresses_and_falls_back_to_aaaa() {
        let _g = test_lock();
        let multi = |_h: &str| {
            Ok(vec![
                "198.51.100.1".to_string(),
                "203.0.113.9".to_string(),
                "203.0.113.10".to_string(),
            ])
        };
        // ① 首条与次条不可达、第三条通 → Verified
        set_http_probe_override(Some(Box::new(|_h, ip| {
            if ip == "203.0.113.10" {
                Ok(())
            } else {
                Err(format!("连不通 {ip}"))
            }
        })));
        assert_eq!(
            verify_reachability("a.ts.net", &multi),
            Reachability::Verified,
            "多条地址逐条尝试（首条陈旧不得误报 Failed）"
        );
        // ①b 全部不可达 → Failed 且原因带失败明细（可排查）
        set_http_probe_override(Some(Box::new(|_h, ip| Err(format!("连不通 {ip}")))));
        match verify_reachability("a.ts.net", &multi) {
            Reachability::Failed { reason } => {
                assert!(
                    reason.contains("连不通") && reason.contains("3 条"),
                    "{reason}"
                )
            }
            other => panic!("应 Failed: {other:?}"),
        }
        // ② DoH 取数的 A→AAAA 回落（注入假 fetch，零网络）。A3：查询地址由
        // doh_url_at(源, 名, 类型) 构成，逐源逐类型，全部是 IP 字面量源
        assert!(doh_url_at(DOH_SOURCES[0], "a.ts.net", "AAAA").contains("type=AAAA"));
        assert!(
            doh_url_at(DOH_SOURCES[0], "a.ts.net", "A").contains("type=A"),
            "A 查询语义不变"
        );
        let urls: std::cell::RefCell<Vec<String>> = Default::default();
        let addrs = probe_addresses("a.ts.net", |u| {
            urls.borrow_mut().push(u.to_string());
            r#"{"Status":0}"#.into() // 权威否定（NOERROR 无记录）
        });
        assert_eq!(
            addrs.expect("全源权威否定 = Ok(空)，不是「验证不了」"),
            Vec::<String>::new(),
            "两查皆空（A 与 AAAA 都拿到全源权威否定）才是「解析不到」"
        );
        // I-5 后计数口径：`NoRecord` **不再提前 break**（传播期两源不一致很常见，提前收手
        // 会把已可用的地址误报成未发布）⇒ 每个类型都要问完整个源表：
        // A 全源否定（3 次）+ AAAA 全源否定（3 次）= 6 次
        assert_eq!(
            urls.borrow().len(),
            DOH_SOURCES.len() * 2,
            "A 为空必须回落查 AAAA，且每个类型都要问完全源: {:?}",
            urls.borrow()
        );
        assert!(
            urls.borrow()[..DOH_SOURCES.len()]
                .iter()
                .all(|u| u.ends_with("type=A")),
            "先问完 A 的全部源: {:?}",
            urls.borrow()
        );
        assert!(
            urls.borrow()[DOH_SOURCES.len()..]
                .iter()
                .all(|u| u.ends_with("type=AAAA")),
            "A 没拿到记录才回落问 AAAA: {:?}",
            urls.borrow()
        );
        let urls2: std::cell::RefCell<Vec<String>> = Default::default();
        let addrs = probe_addresses("a.ts.net", |u| {
            urls2.borrow_mut().push(u.to_string());
            r#"{"Status":0,"Answer":[{"type":1,"data":"203.0.113.10"}]}"#.into()
        });
        assert_eq!(addrs.unwrap(), vec!["203.0.113.10".to_string()]);
        assert_eq!(urls2.borrow().len(), 1, "A 有记录不再多查一次 AAAA");
        teardown_globals();
    }

    /// B-M8：`host_from_board_url` 是 [`current_dns_host`] 的**活回落路径**
    /// （status 读不到时用快照 url 抽 host 做校验目标），此前零测试。
    /// 纯函数边界：scheme 可有可无、带端口保留、无 host 段给 None（绝不造空域名）
    #[test]
    fn host_from_board_url_extracts_host_from_snapshot_url() {
        assert_eq!(
            host_from_board_url("https://x.example-tailnet.ts.net/m").as_deref(),
            Some("x.example-tailnet.ts.net")
        );
        assert_eq!(
            host_from_board_url("https://x.ts.net").as_deref(),
            Some("x.ts.net")
        );
        assert_eq!(
            host_from_board_url("x.ts.net/m").as_deref(),
            Some("x.ts.net"),
            "无 scheme 也要能抽（回落路径的输入不必是完整 URL）"
        );
        assert_eq!(
            host_from_board_url("https://x.ts.net:8443/m").as_deref(),
            Some("x.ts.net:8443"),
            "端口保留"
        );
        assert_eq!(host_from_board_url(""), None);
        assert_eq!(host_from_board_url("/m"), None, "没有 host 段 → None");
        assert_eq!(host_from_board_url("https:///m"), None);
    }

    /// B-M8 组合：回落路径**真的被走通**——status 读不到时，校验目标取自快照 url 的
    /// host 段；status 可读时 DNSName 优先（剥结尾点）
    #[test]
    fn current_dns_host_falls_back_to_snapshot_url() {
        let _g = test_lock();
        set_run_cli_override(Some(Box::new(|_args: &[&str]| Err("CLI 挂了".into()))));
        set_ts_snapshot(|s| {
            s.running = true;
            s.url = Some("https://y.example-tailnet.ts.net/m".into());
        });
        assert_eq!(
            current_dns_host().as_deref(),
            Some("y.example-tailnet.ts.net"),
            "status 读不到 → 用快照 url 抽 host（活回落路径）"
        );
        set_run_cli_override(Some(Box::new(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_RUNNING.into()),
            other => Err(format!("测试未应答的 CLI 调用: {other:?}")),
        })));
        assert_eq!(
            current_dns_host().as_deref(),
            Some("jarvismac-mini.example-tailnet.ts.net"),
            "status 可读 → DNSName 优先（剥结尾点）"
        );
        teardown_globals();
    }

    /// B-M6 **变异锚点**（安装包校验）：
    /// ① 校验走**流式**哈希（.pkg/.msi 数十 MB，旧实现每次探测把整包读进内存做哈希，
    ///    download 又整读一次）——流式路径与既有切片路径必须同判据；
    /// ② `Platform::Other`（无线程资产）不得先落到「sha256 校验失败」的误导文案，
    ///    应如实说「当前平台不支持一键配置」。
    /// 变异自证：把 install_step_with 的平台门去掉 → ②档红（文案变成 sha 失败）。
    ///
    /// **平台门控（2026-10-07 存量债清理，Linux CI 门禁）**：①档（流式/切片校验同判据）本身
    /// 跨平台，但②档要先在临时目录落一个**带真实产物名**的包——`ts_asset_name(Platform::Other)`
    /// 返回空串 ⇒ `join("")` 落到临时目录本身，`fs::write` 撞 `Os 21 IsADirectory`（Linux CI
    /// 实测）。且②档期望的「不支持」文案正是非产品平台上的如实行为。
    #[cfg_attr(
        not(any(target_os = "macos", target_os = "windows")),
        ignore = "平台不适用：②档落包路径的 ts_asset_name(Platform::Other) 为空串 ⇒ 撞 IsADirectory；且平台门文案本就是非产品平台的如实行为"
    )]
    #[test]
    fn installer_hash_streams_and_platform_gate_says_unsupported() {
        use sha2::Digest;
        let dir = tempfile::tempdir().unwrap();
        // ① 流式校验与切片校验同判据（同一份内容两条路径结论一致）
        let blob = dir.path().join("blob.bin");
        let bytes = b"hello".to_vec();
        std::fs::write(&blob, &bytes).unwrap();
        let real: String = sha2::Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(
            crate::remote::tunnel::verify_sha256_file(&blob, &real).is_ok(),
            "流式校验：内容对得上必须通过"
        );
        assert!(
            crate::remote::tunnel::verify_sha256(&bytes, &real).is_ok(),
            "切片校验（既有单点）：同判据"
        );
        let wrong = "00".repeat(32);
        assert!(crate::remote::tunnel::verify_sha256_file(&blob, &wrong).is_err());
        assert!(
            crate::remote::tunnel::verify_sha256_file(&dir.path().join("absent.bin"), &real)
                .is_err()
        );
        // 坏内容不得因「不读整包」而放行（installer_file_ok 走流式）
        let pkg = dir.path().join(ts_asset_name(current_platform()));
        std::fs::write(&pkg, b"tampered bytes").unwrap();
        assert!(
            !installer_file_ok(current_platform(), &pkg),
            "篡改包必须判不可信"
        );

        // ② 平台门在 sha 门之前：Other 如实说不支持
        let r = install_step_with(
            Platform::Other,
            std::path::Path::new("/nonexistent-pkg"),
            |_| panic!("不支持平台不得触达安装器"),
        );
        let e = r.unwrap_err();
        assert!(
            e.contains("TSWIZ_PLATFORM_UNSUPPORTED: 当前平台暂不支持一键配置"),
            "Other 平台必须如实说「不支持」而不是「sha256 校验失败」: {e}"
        );
        assert!(!e.contains("sha256"), "不得落到误导文案: {e}");
    }

    /// B-M3 **变异锚点**：轮询退避——状态与上轮**逐字相同**（含连续失败）时间隔翻倍到
    /// 上限，任何变化立刻回到基准周期。固定 5s × 2 次 CLI/轮 ≈ 24 次进程派生/分钟，
    /// 持续失败（未登录 / 后端常年忙）也照打；退避后稳定态降到 1–2 次/分钟。
    /// 变异自证：`next_poll_secs` 改回恒返回基准周期 → 本测试必红。
    #[test]
    fn poll_interval_backs_off_on_unchanged_and_resets_on_change() {
        assert_eq!(
            next_poll_secs(POLL_SECS, true),
            POLL_SECS,
            "有变化：卡面跟手"
        );
        assert_eq!(next_poll_secs(POLL_SECS, false), POLL_SECS * 2);
        assert_eq!(next_poll_secs(POLL_SECS * 2, false), POLL_SECS * 4);
        assert_eq!(
            next_poll_secs(POLL_MAX_SECS, false),
            POLL_MAX_SECS,
            "封顶（不再无限翻倍）"
        );
        assert_eq!(
            next_poll_secs(POLL_MAX_SECS, true),
            POLL_SECS,
            "任何变化立刻回到基准周期（起停/上下线必须尽快反映）"
        );
        // 同结果判据看得到错误文案的变化（连续失败但原因变了 = 变化，不视为稳定态）
        let a = crate::remote::tunnel::ChannelStatus {
            running: false,
            url: None,
            error: Some("Tailscale 待登录".into()),
        };
        let b = crate::remote::tunnel::ChannelStatus {
            error: Some("读取 Tailscale 状态失败: 后端忙".into()),
            ..a.clone()
        };
        assert!(same_snapshot(&a, &a.clone()));
        assert!(!same_snapshot(&a, &b), "错误原因变化不得当成同结果");
    }

    /// B-M11 **变异锚点**：单次 `remote_ts_probe`（`wizard_status_with` 内核）只读**一次**
    /// `status --json`——原实现 `probe_steps_from` 读一次、为 authUrl 又读一次
    /// （合计 status×2 + get + funnel status = 4 次进程派生）。本测试同时断言这一次读数
    /// **真的被两个消费方共用**（login 步判据与 authUrl 都来自它）。
    /// 变异自证：authUrl 改回独立 `run_cli(["status","--json"])` → 计数必红。
    #[test]
    fn wizard_status_reads_status_json_exactly_once() {
        let _g = test_lock();
        let calls: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>> = Default::default();
        let c = calls.clone();
        set_run_cli_override(Some(Box::new(move |args: &[&str]| {
            let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            c.lock().unwrap().push(v.clone());
            match args {
                ["status", "--json"] => Ok(STATUS_RUNNING.into()),
                ["get", "--json"] => Ok(r#"{"shields-up": false}"#.into()),
                ["funnel", "status", "--json"] => Ok("{}".into()),
                other => Err(format!("测试未应答的 CLI 调用: {other:?}")),
            }
        })));
        let v = wizard_status_with(9420, true);
        let status_calls = calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.as_slice() == ["status", "--json"])
            .count();
        assert_eq!(
            status_calls,
            1,
            "status --json 必须只读一次（B-M11）: {:?}",
            calls.lock().unwrap()
        );
        // 同一次读数被两个消费方共用：login 步判据 + authUrl
        let login_done = v["states"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == "login")
            .unwrap()["done"]
            .as_bool()
            .unwrap();
        assert!(login_done, "login 判据取自同一次读数（Running → done）");
        assert_eq!(v["authUrl"], "", "authUrl 取自同一次读数（Running → 空串）");
        teardown_globals();
    }

    /// B-I6 **变异锚点**：探针判据从「拿到**任意** HTTP 响应即算通」收紧为「响应带
    /// **兔维斯 特征**」——TUN / 透明重定向式 MITM 返回的拦截页（用户把自签 CA 装进系统
    /// 信任库时 TLS 仍"成功"）必须判失败。§C3 的事故正是「本地信号全绿」型误判。
    /// 变异自证：把 `mam_response_ok` 改回「只看状态码/只看能否拿到响应」→ 本测试必红。
    #[test]
    fn non_mam_response_never_verifies() {
        let _g = test_lock();
        let marker = crate::remote::server::MAM_REACH_VALUE;
        // 判据层真值表
        assert!(
            mam_response_ok(200, Some(marker)).is_ok(),
            "200 + 兔维斯 特征头 = 通"
        );
        assert!(
            mam_response_ok(200, None).is_err(),
            "无特征头 = 不是 兔维斯 答的"
        );
        assert!(
            mam_response_ok(200, Some("nginx/1.25")).is_err(),
            "外来特征值同样不是 兔维斯 答的"
        );
        assert!(
            mam_response_ok(403, Some(marker)).is_err(),
            "非 200 不算通（旧口径连 403 都算通）"
        );
        // 整链层：假响应 = 「MITM 拦截页」→ 解析得到地址也绝不 Verified
        set_http_probe_override(Some(Box::new(|_h, _ip| {
            mam_response_ok(200, Some("Blocked by proxy"))
        })));
        let r = verify_reachability("a.ts.net", &|_h| Ok(vec!["203.0.113.10".into()]));
        assert!(
            matches!(r, Reachability::Failed { .. }),
            "拦截页不得判可用（§C3 残余风险）: {r:?}"
        );
        // 真 兔维斯 特征 → 通（成功路径不受影响）
        set_http_probe_override(Some(Box::new(|_h, _ip| mam_response_ok(200, Some(marker)))));
        assert_eq!(
            verify_reachability("a.ts.net", &|_h| Ok(vec!["203.0.113.10".into()])),
            Reachability::Verified
        );
        teardown_globals();
    }

    // ==== 修复轮 2（独立评审 B 段）：生命周期与校验态 ====

    /// B-I4 **变异锚点**：守卫读数失败必须 **fail-closed**——读不到 `funnel status`
    /// 就绝不执行 `funnel --bg`（开通）或 `funnel reset`（撤销/自愈）。旧实现
    /// `unwrap_or_default()` → 空串 → `foreign_serve_config("")` = 「没有配置」→ 放行：
    /// CLI 一时失败（后端忙 / 版本差异）就会覆盖用户自建配置，与本模块自己的
    /// fail-closed 纪律（map_status / 步骤判据表都「读不到就报错」）自相矛盾。
    /// 变异自证：把 `match run_cli(...) { Err(e) => return Err(...) }` 换回
    /// `unwrap_or_default()` → 本测试两档必红（既有配置被覆盖、命令照发）。
    #[test]
    fn guards_refuse_when_funnel_status_is_unreadable() {
        let _g = test_lock();
        // ① 开通路径：守卫读数失败 → Err，且绝不出 `funnel --bg` / `set`
        *DESIRED.lock().unwrap() = None;
        let calls: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>> = Default::default();
        let c = calls.clone();
        set_run_cli_override(Some(Box::new(move |args: &[&str]| {
            let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            c.lock().unwrap().push(v.clone());
            match args {
                ["funnel", "status", "--json"] => Err("后端忙，读不到".into()),
                ["status", "--json"] => Ok(STATUS_RUNNING.into()),
                other => Err(format!("测试未应答的 CLI 调用: {other:?}")),
            }
        })));
        let e = run_step("enable", 9420).unwrap_err();
        assert!(
            e.contains("读不到 Funnel 状态"),
            "读不到配置必须如实拒绝（不是「没有配置」）: {e}"
        );
        assert!(
            !calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c[0] == "funnel" && c[1] != "status"),
            "守卫拒绝后不得发出任何 funnel 动作: {:?}",
            calls.lock().unwrap()
        );
        teardown_globals();

        // ② 自愈路径：校验失败后自愈前同样要读守卫 —— 读不到 → 拒绝 reset/重开
        *DESIRED.lock().unwrap() = Some(9420);
        let calls2: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>> = Default::default();
        let c2 = calls2.clone();
        set_run_cli_override(Some(Box::new(move |args: &[&str]| {
            let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            c2.lock().unwrap().push(v.clone());
            match args {
                ["status", "--json"] => Ok(STATUS_RUNNING.into()),
                ["funnel", "status", "--json"] => Err("后端忙，读不到".into()),
                other => Err(format!("测试未应答的 CLI 调用: {other:?}")),
            }
        })));
        set_verify_probe_override(Some(Box::new(|_h| Ok(vec![])))); // 解析不到 → Failed → 走自愈
        set_http_probe_override(Some(Box::new(|_h, _ip| Ok(()))));
        let r = run_step("repair", 9420).expect("repair 应 Ok（失败如实落态）");
        assert_eq!(r["done"], false);
        let reason = r["reach"]["reason"].as_str().unwrap_or_default();
        assert!(
            reason.contains("读不到 Funnel 状态"),
            "自愈守卫读数失败必须如实上墙: {reason}"
        );
        assert!(
            !calls2
                .lock()
                .unwrap()
                .iter()
                .any(|c| c[0] == "funnel" && c[1] != "status"),
            "读不到配置时不得发出 funnel reset / 重开: {:?}",
            calls2.lock().unwrap()
        );
        teardown_globals();
    }

    /// B-I3 **变异锚点**：轮询线程读到「期望态已清空」后必须在**同一把锁内复查**再清零。
    /// 场景（µs 级但后果永久）：线程读到 `None` 后被抢占，主线程趁隙 `start_channel_inner`
    /// （置 `DESIRED=Some` → `refresh_once` 写好运行快照 → `ensure_poller` 见旧线程
    /// `!is_finished()` 故**不新起**），旧线程恢复后无条件清零 → 通道永远显示未运行、
    /// **无自愈路径**（只有再切一次开关才恢复）。变异自证：删掉复查 → 本测试必红。
    #[test]
    fn retire_recheck_keeps_snapshot_when_channel_was_rearmed_midflight() {
        let _g = test_lock();
        // 模拟「主线程已趁隙重新开启」：期望态 Some + 快照已被 refresh_once 写好
        *DESIRED.lock().unwrap() = Some(9420);
        set_ts_snapshot(|s| {
            s.running = true;
            s.url = Some("https://x.example-tailnet.ts.net/m".into());
        });
        let mut d = DESIRED.lock().unwrap();
        assert!(
            !retire_if_still_off(&mut d),
            "期望态已被重新置 Some → 本轮不得退役"
        );
        drop(d);
        let s = ts_snapshot();
        assert!(
            s.running && s.url.is_some(),
            "在途清零不得擦掉刚写好的运行快照: running={} url={:?}",
            s.running,
            s.url
        );
        teardown_globals();
    }

    /// B-I3 第二半：退役清零**不得擦掉调用方写入的诊断**——开关路径（`mod.rs`）在守卫
    /// 拒绝 / CLI 失败时把原因写进 `error` 供卡面展示；旧线程随后无条件清零，用户就只
    /// 看到「尚未生效」而非「外来配置占用」（评审实测的第二半后果）。
    /// 变异自证：把条件清零改回无条件清零 → 本测试必红。
    #[test]
    fn retire_preserves_caller_written_diagnostic() {
        let _g = test_lock();
        *DESIRED.lock().unwrap() = None;
        set_ts_snapshot(|s| {
            s.running = false;
            s.error = Some("检测到非 兔维斯 的 serve 配置".into());
        });
        let mut d = DESIRED.lock().unwrap();
        assert!(retire_if_still_off(&mut d), "确实还关着 → 可以退役");
        drop(d);
        assert_eq!(
            ts_snapshot().error.as_deref(),
            Some("检测到非 兔维斯 的 serve 配置"),
            "调用方写入的诊断（守卫拒绝/CLI 失败）不得被退役清零擦掉"
        );
        teardown_globals();
    }

    /// B-I2 **变异锚点**：重验触发条件**不分当前态**——只要求「通道在运行 + 窗口到期」。
    /// 旧口径（要求当前已是 Verified）有两个实测后果：① 翻 Failed 后条件恒假 → 再无自动
    /// 重试；② 每次重启校验态复位 Unverified → 恢复路径永不校验 → 地址恒不显示（与 §C1
    /// 「重启后地址不变」直接冲突）。本测试在 Unverified 态下断言「该校验」——谁把
    /// 「当前必须是 Verified」加回来，这里必红。
    #[test]
    fn should_reverify_covers_running_channel_regardless_of_state() {
        let _g = test_lock();
        set_reachability(Reachability::Unverified);
        assert!(
            should_reverify(true, true),
            "通道在运行且窗口到期 → 必须校验（不分当前态；Unverified 更要校验）"
        );
        assert!(!should_reverify(false, true), "通道没运行不校验");
        assert!(!should_reverify(true, false), "窗口未到期不重复校验");
        teardown_globals();
    }

    /// B-I2 **事件防刷屏锚点**（W-B 扩展口径 + M8 再扩展）：错误事件只在**「从可用
    /// 跌落」**时发（[`failure_event_reason`]）——上一轮态就是写入前的全局校验态（每轮
    /// 写完即成为下一轮的「上一轮」）。三种跌落形态：`Verified → Failed`、
    /// `Verified → RecordPending`（记录被撤，用户刚才还能用的地址现在打不开）、
    /// `Verified → Recovering`（**M8**：中年期后端抖动把已验证地址撤下 1–2 分钟，
    /// 静默撤下就等于用户手里的失效地址没有任何提示）；其余一律不发：
    /// Unverified → Failed（重启后首轮就没通过）/ Unverified → Recovering（**开机路径**，
    /// 起点就不是"可用"）/ Failed → Failed（持续失败）/ Verified → Verified（成功路径）/
    /// Unverified → Verified（正常成功路径）。
    /// 变异：把 `failure_event_reason` 的 `prev != Verified` 早退删掉 → ②③ 必红；
    /// 删掉 `Recovering` 臂 → ①必红（见 `failure_event_includes_recovering_but_not_from_boot_start`）。
    #[test]
    fn failure_event_only_fires_on_drop_from_verified() {
        let failed = Reachability::Failed {
            reason: "解析不到".into(),
        };
        let pending = Reachability::RecordPending { republish: false };
        assert!(
            failure_event_reason(&Reachability::Verified, &failed).is_some(),
            "Verified → Failed 该发"
        );
        assert!(
            failure_event_reason(&Reachability::Verified, &pending).is_some(),
            "Verified → 记录被撤（地址从可用变不可用）也该发"
        );
        assert!(
            failure_event_reason(&Reachability::Verified, &Reachability::Recovering).is_some(),
            "**M8**：Verified → 恢复中（后端抖动把已验证地址撤下）必须发——否则静默撤下"
        );
        assert!(
            failure_event_reason(&Reachability::Unverified, &failed).is_none(),
            "Unverified → Failed（重启首轮失败）不得发事件（否则每次重启都刷一条）"
        );
        assert!(
            failure_event_reason(&Reachability::Unverified, &Reachability::Recovering).is_none(),
            "**M8 防刷屏红线**：开机路径（Unverified → Recovering）不得发——每次开机都刷一条"
        );
        assert!(
            failure_event_reason(&failed, &failed).is_none(),
            "持续失败不得每轮刷屏"
        );
        assert!(
            failure_event_reason(&Reachability::Verified, &Reachability::Verified).is_none(),
            "成功路径不发错误事件"
        );
        assert!(
            failure_event_reason(&Reachability::Unverified, &Reachability::Verified).is_none(),
            "Unverified → Verified 是正常成功路径"
        );
    }

    /// B-I2 用户可见后果：**重启后恢复路径只 refresh 不校验**（reachability 复位
    /// Unverified）→ 轮询入口的首轮校验必须能把地址点亮（否则载荷恒报「尚未生效」、
    /// 地址恒不显示，直到用户找到向导手点重试）。
    #[test]
    fn poll_reverify_lifts_restarted_unverified_to_verified() {
        let _g = test_lock();
        *DESIRED.lock().unwrap() = Some(9420);
        set_run_cli_override(Some(Box::new(|_| {
            Ok(STATUS_RUNNING.replace("jarvismac-mini.example-tailnet.ts.net", "a.ts.net"))
        })));
        set_reachability(Reachability::Unverified);
        set_verify_probe_override(Some(Box::new(|_h| Ok(vec!["203.0.113.10".into()]))));
        set_http_probe_override(Some(Box::new(|_h, _ip| Ok(()))));
        let r = poll_reverify("a.ts.net", 9420).expect("测试已装假探针 → 轮询入口应真的校验");
        assert_eq!(r, Reachability::Verified);
        assert_eq!(
            reachability(),
            Reachability::Verified,
            "重启后首轮校验必须把地址点亮（§C1「重启后地址不变」的实现前提）"
        );
        teardown_globals();
    }

    /// 零网络红线：**测试构建下轮询入口没有假探针就不发起真实探测**——轮询线程可能
    /// 在测试结束后才醒（5s），届时撞上别的测试留下的全局态会打出真实网络请求。
    /// 生产构建恒走 [`real_probe`]（本闸只在 cfg(test) 存在）。
    #[test]
    fn poll_reverify_skips_real_network_without_fake_probe() {
        let _g = test_lock();
        set_verify_probe_override(None);
        set_reachability(Reachability::Verified);
        assert!(
            poll_reverify("a.ts.net", 9420).is_none(),
            "无假探针 → 测试构建必须跳过（零网络红线）"
        );
        assert_eq!(
            reachability(),
            Reachability::Verified,
            "跳过时不得改动校验态"
        );
        teardown_globals();
    }

    #[test]
    fn poll_reverify_discards_results_invalidated_during_the_http_probe() {
        let _g = test_lock();
        *DESIRED.lock().unwrap() = Some(9420);
        set_run_cli_override(Some(Box::new(|_| {
            Ok(STATUS_RUNNING.replace("jarvismac-mini.example-tailnet.ts.net", "a.ts.net"))
        })));
        set_verify_probe_override(Some(Box::new(|_host| {
            invalidate_verification_lifecycle();
            Ok(vec!["203.0.113.10".into()])
        })));
        set_http_probe_override(Some(Box::new(|_host, _ip| Ok(()))));
        set_reachability(Reachability::Unverified);

        let result = poll_reverify("a.ts.net", 9420).expect("假探针应走后台入口");

        assert_eq!(result, Reachability::Unverified);
        assert_eq!(reachability(), Reachability::Unverified);
        teardown_globals();
    }

    #[test]
    fn poll_reverify_preserves_new_configuration_failure_after_old_probe_returns() {
        let _g = test_lock();
        *DESIRED.lock().unwrap() = Some(9420);
        set_run_cli_override(Some(Box::new(|_| {
            Ok(STATUS_RUNNING.replace("jarvismac-mini.example-tailnet.ts.net", "a.ts.net"))
        })));
        set_verify_probe_override(Some(Box::new(|_host| {
            super::reach::observe_config("new.ts.net", 9430);
            set_reachability(Reachability::Failed {
                reason: "new configuration failure".into(),
            });
            Ok(vec!["203.0.113.10".into()])
        })));
        set_http_probe_override(Some(Box::new(|_host, _ip| Ok(()))));
        let result = poll_reverify("a.ts.net", 9420).unwrap();
        assert_eq!(
            result,
            Reachability::Failed {
                reason: "new configuration failure".into()
            }
        );
        assert_eq!(reachability(), result);
        assert_eq!(waiting_status(), serde_json::Value::Null);
        teardown_globals();
    }

    #[test]
    fn poller_reverify_does_not_start_after_channel_closes_during_hostname_read() {
        let _g = test_lock();
        *LAST_REVERIFY.lock().unwrap() = None;
        *DESIRED.lock().unwrap() = Some(9420);
        let reads = std::sync::atomic::AtomicUsize::new(0);
        set_run_cli_override(Some(Box::new(move |args| match args {
            ["status", "--json"] => {
                if reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
                    *DESIRED.lock().unwrap() = None;
                    invalidate_verification_lifecycle();
                }
                Ok(STATUS_RUNNING.into())
            }
            ["funnel", "status", "--json"] => Ok(
                r#"{"Web":{"x.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:9420"}}}}}"#
                    .into(),
            ),
            other => Err(format!("后台复验不得写配置: {other:?}")),
        })));
        set_verify_probe_override(Some(Box::new(|_| Ok(vec![]))));
        let mut st = PollerState::default();
        assert!(!poller_tick(
            &mut st,
            std::time::Instant::now(),
            &mut poll_reverify
        ));
        assert_eq!(reachability(), Reachability::Unverified);
        assert_eq!(waiting_status(), serde_json::Value::Null);
        teardown_globals();
    }

    #[test]
    fn poller_reverify_discards_http_result_after_real_stop_or_port_change() {
        let _g = test_lock();
        for close in [true, false] {
            *LAST_REVERIFY.lock().unwrap() = None;
            *DESIRED.lock().unwrap() = Some(9420);
            set_run_cli_override(Some(Box::new(|args| {
                match args {
                ["status", "--json"] => Ok(STATUS_RUNNING.into()),
                ["funnel", "status", "--json"] => Ok(r#"{"Web":{"x.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:9420"}}}}}"#.into()),
                ["funnel", "reset"] => Ok(String::new()),
                other => Err(format!("测试未应答: {other:?}")),
            }
            })));
            set_verify_probe_override(Some(Box::new(|_| Ok(vec!["203.0.113.10".into()]))));
            set_http_probe_override(Some(Box::new(move |_, _| {
                if close {
                    stop_channel().expect("关闭走真实生命周期，CLI 使用 fake");
                } else {
                    *DESIRED.lock().unwrap() = Some(9430);
                    super::reach::observe_port(9430);
                    set_reachability(Reachability::Failed {
                        reason: "new port failure".into(),
                    });
                }
                Ok(())
            })));
            let mut st = PollerState::default();
            assert!(!poller_tick(
                &mut st,
                std::time::Instant::now(),
                &mut poll_reverify
            ));
            let expected = if close {
                Reachability::Unverified
            } else {
                Reachability::Failed {
                    reason: "new port failure".into(),
                }
            };
            assert_eq!(reachability(), expected);
            assert_eq!(waiting_status(), serde_json::Value::Null);
            teardown_globals();
        }
    }

    /// B-I1（§C3 要求 7「校验态必须随生命周期失效」）**变异锚点**：stop / start 之后
    /// 旧的 `Verified` 不得存活。`funnel reset` 恰恰是 §C3 实测事故里「记录未发布」的
    /// 触发动作——校验态跨它存活，界面就会把一个**刚被自己撤掉**的地址报成可用，
    /// 向导 verify 步还挂着「已完成」，用户没有任何再验提示（「晚上关、早上开」场景）。
    /// 变异自证：把 stop_inner / start_channel_inner 的作废调用删掉 → 本测试两档必红。
    /// 同时锁定**重验时钟随生命周期清零**：否则旧线程跨 stop→start 存活时会把上一周期
    /// 的时刻带进新周期，重开后要等满 60s 才校验、地址跟着不显示。
    #[test]
    fn lifecycle_invalidates_verified_reachability() {
        let _g = test_lock();
        // ① stop 路径：Verified + 运行快照 → 停之后校验态必须作废
        *DESIRED.lock().unwrap() = Some(9420);
        set_reachability(Reachability::Verified);
        *LAST_REVERIFY.lock().unwrap() = Some(std::time::Instant::now());
        set_ts_snapshot(|s| {
            s.running = true;
            s.url = Some("https://x.example-tailnet.ts.net/m".into());
        });
        let _calls = spy_run_cli(Ok("{}".into()));
        assert!(stop_channel().is_ok());
        assert_eq!(
            reachability(),
            Reachability::Unverified,
            "stop 后旧 Verified 不得存活（否则把刚 reset 掉的地址报成可用）"
        );
        assert!(
            LAST_REVERIFY.lock().unwrap().is_none(),
            "stop 必须清零重验时钟"
        );
        teardown_globals();

        // ② start 路径：先挂一个旧 Verified（模拟「晚上关、早上开」）→ 重开必须作废
        set_reachability(Reachability::Verified);
        *LAST_REVERIFY.lock().unwrap() = Some(std::time::Instant::now());
        set_run_cli_override(Some(Box::new(|args: &[&str]| match args {
            ["funnel", "status", "--json"] => Ok("{}".into()),
            ["set", "--shields-up=false"] => Ok(String::new()),
            ["get", "--json"] => Ok(r#"{"shields-up": false}"#.into()),
            ["funnel", "--bg", "9420"] => Ok(String::new()),
            ["funnel", "status"] => Ok(String::new()),
            ["status", "--json"] => Ok(STATUS_RUNNING.into()),
            other => Err(format!("测试未应答的 CLI 调用: {other:?}")),
        })));
        assert!(start_channel(9420).is_ok());
        assert_eq!(
            reachability(),
            Reachability::Unverified,
            "start 后旧 Verified 不得存活（重开必须重新校验）"
        );
        assert!(
            LAST_REVERIFY.lock().unwrap().is_none(),
            "start 必须清零重验时钟（重开后首个轮询窗即校验，不等满 60s）"
        );
        teardown_globals();
    }

    /// 本地宣称收口优先于 CLI 结果：status 读取失败（如 CLI 被卸载）→ Err 上抛
    /// 给调用方展示，但 DESIRED/快照已先收口——绝不因 CLI 失败而继续宣称运行
    #[test]
    fn stop_channel_surfaces_cli_failure_after_local_reclaim() {
        let _g = test_lock();
        *DESIRED.lock().unwrap() = Some(9420);
        set_ts_snapshot(|s| {
            s.running = true;
            s.url = Some("https://x.example-tailnet.ts.net/m".into());
        });
        let _calls = spy_run_cli(Err("未检测到 Tailscale（尚未安装）".into()));

        let r = stop_channel();
        assert!(r.is_err(), "CLI 失败如实上抛");
        assert_eq!(*DESIRED.lock().unwrap(), None, "期望态先收口");
        let s = ts_snapshot();
        assert!(!s.running && s.url.is_none(), "快照先收口");
        teardown_globals();
    }

    // ============================================================
    // W-A：开机恢复窗口（2026-10-07 真机重启逐秒实测）
    // ------------------------------------------------------------
    // 实测时间线（重启时刻 T）：
    //   T+58s  服务已 Running（Automatic），但 **BackendState=NoState**、
    //          `funnel status --json` **短暂为 `{}`**（后端仍在初始化）
    //   T+73s  BackendState=Running；Funnel 配置**逐字段原样恢复**（307 字节）
    //   T+89s  DoH 记录仍在，但强制入口测试 TLS 失败（curl 35）——公网恢复窗口
    //   T+2.5m 两个入口 IP 均 HTTP 200
    //
    // 纪律（本组用例压的就是它）：**判定「Funnel 配置是否存在/丢失」之前必须先确认
    // BackendState=Running**；`NoState`/初始化中一律不得判「配置丢失」，更不得因此重开
    // （`funnel --bg`）。
    // ============================================================

    /// 实测形态（真机重启 T+58s）：Windows 服务已 Running，但后端仍在初始化
    const STATUS_BACKEND_INIT: &str = r#"{
        "BackendState": "NoState",
        "AuthURL": "",
        "Self": { "DNSName": "" },
        "CertDomains": []
    }"#;

    /// 实测形态（真机重启 T+73s）：tailscaled 把 Funnel 配置**逐字段原样恢复**——
    /// 兔维斯 什么都不必做，更不该重开。
    const FUNNEL_RESTORED_AFTER_REBOOT: &str = r#"{"Web":{"jarvismac-mini.example-tailnet.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:9420"}}}}}"#;

    /// 记录 CLI 调用的替身（逐命令应答，便于断言"哪条命令**没有**被发出"）
    fn spy_cli(
        reply: impl Fn(&[&str]) -> Result<String, String> + Send + 'static,
    ) -> std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>> {
        let calls: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>> = Default::default();
        let c = calls.clone();
        set_run_cli_override(Some(Box::new(move |args: &[&str]| {
            c.lock()
                .unwrap()
                .push(args.iter().map(|s| s.to_string()).collect());
            reply(args)
        })));
        calls
    }

    /// 恢复窗口内**未发出任何写命令**（`funnel --bg` / `funnel reset` / `set`）的共用断言
    fn assert_no_write_calls(calls: &[Vec<String>]) {
        for c in calls {
            let head = c.first().map(String::as_str).unwrap_or("");
            let writes = head == "set"
                || c.contains(&"--bg".to_string())
                || c == &["funnel".to_string(), "reset".to_string()];
            assert!(
                !writes,
                "恢复窗口内绝不发起写命令（判配置/重开都会把 tailscaled 正在恢复的配置搞乱）: {c:?}\n全部调用: {calls:?}"
            );
        }
    }

    /// **W-A 防线①（变异锚点）**：开机恢复窗口内启动通道（服务 Running 但
    /// `BackendState=NoState` + `funnel status --json` 为 `{}`）→
    /// **不得**判「配置丢失」、**不得**发 `funnel --bg`（连 `set --shields-up=false`
    /// 都不该发：后端没就绪时回读必然失败，会写出「无法确认已关闭」的**假故障**）。
    /// 期望态照记——后端就绪后由轮询复评（配置在 = 直接恢复；确实缺 = 补开通）。
    /// 变异：删掉 enable_funnel 的 BackendState 门 → `funnel --bg` 被发出，本测试必红。
    #[test]
    fn start_channel_defers_while_backend_is_initializing_and_never_reopens() {
        let _g = test_lock();
        let calls = spy_cli(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_BACKEND_INIT.into()),
            // 恢复窗口内的实测形态：配置读成空——**这不是「配置丢失」，是后端还没加载完**
            ["funnel", "status", "--json"] => Ok("{}".into()),
            other => Err(format!("恢复窗口内不该有这条调用: {other:?}")),
        });
        let r = start_channel(9420);
        assert!(r.is_ok(), "恢复窗口不是错误（用户什么都没做错）: {r:?}");
        assert_no_write_calls(&calls.lock().unwrap());
        assert_eq!(
            *DESIRED.lock().unwrap(),
            Some(9420),
            "期望态必须记住：后端就绪后由轮询复评/补开通（否则「开机零维护」在真丢配置时失效）"
        );
        let s = ts_snapshot();
        assert!(
            !s.running && s.url.is_none(),
            "不得宣称运行/地址: running={} url={:?}",
            s.running,
            s.url
        );
        assert!(
            s.error.is_none(),
            "恢复窗口**不是故障态**（旧实现写「Tailscale 未就绪（NoState）」= 把正常开机过程报成故障）: {:?}",
            s.error
        );
        teardown_globals();
    }

    /// **W-A 防线②（变异锚点）**：轮询在恢复窗口内同样**不得**补开通——即使
    /// `funnel status --json` 读成 `{}`（实测形态）也一样，判据是 `BackendState` 未就绪。
    ///
    /// **I4a（2026-10-07 评审 · 测试保真度）**：旧用例只断言「没有写命令」，而
    /// `ready_streak` 记账与 `enable_funnel` 的第二道门**互为兜底**——留任意一道该用例
    /// 都绿 ⇒ **轮询侧那道门无测试可证**（旧注释写的变异「去掉 Running 门」实测不红）。
    /// 现改为**直接压判据本身**：窗口内 `deferred_open_due(..) == false` ∧
    /// `ready_streak == 0`（这才是轮询侧的 Running 门）；「零写命令」退为第二道门
    /// （`enable_funnel` 的 ⓪ 门）的覆盖。
    /// 变异（**实际能红的那个**）：把 `deferred_open_due` 里的
    /// `let ready = matches!(&tick.status, Ok(s) if backend_phase(s) == BackendPhase::Ready);`
    /// 改成 `let ready = true;`（= 去掉轮询侧 Running 门）→ 第 2 轮 `ready_streak` 涨到 2、
    /// 判据翻 true，本测试必红。
    /// **如实登记**：最终 `if` 里的 `!ready ||` 子句与 streak 记账**重复**，单独删它
    /// 不可观测（非就绪轮的 streak 恒 0 已足以拦住）——留作纵深防御，不宣称它可证。
    #[test]
    fn poller_never_reopens_while_backend_is_initializing() {
        let _g = test_lock();
        *DESIRED.lock().unwrap() = Some(9420);
        let calls = spy_cli(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_BACKEND_INIT.into()),
            ["funnel", "status", "--json"] => Ok("{}".into()),
            other => Err(format!("恢复窗口内不该有这条调用: {other:?}")),
        });
        let mut st = PollerState::default();
        let now = std::time::Instant::now();
        for i in 0..4 {
            // ① 判据本身（轮询侧 Running 门）：同一轮读数喂进去，必须恒 false
            let tick = read_tick(9420);
            assert!(
                !deferred_open_due(&mut st, 9420, &tick),
                "第 {i} 轮：恢复窗口内补开通判据必须恒 false（后端未确认就绪）"
            );
            // ② 记账本身：非就绪轮不得累计 streak，否则第 2 轮就会把 `{}` 当「确认缺失」
            assert_eq!(
                st.ready_streak, 0,
                "第 {i} 轮：非就绪轮不得累计 ready_streak（「连续 2 轮就绪」的确认语义）"
            );
            let retired = poller_tick(&mut st, now, &mut |_, _| None);
            assert!(!retired, "通道开着，轮询线程不得退役");
            assert_eq!(st.ready_streak, 0, "第 {i} 轮（走完整轮）：streak 仍须为 0");
            // ③ 第二道门（enable_funnel ⓪）的覆盖：整轮里零写命令
            assert_no_write_calls(&calls.lock().unwrap());
            assert!(
                ts_snapshot().error.is_none(),
                "第 {i} 轮不得把恢复窗口报成故障"
            );
        }
        teardown_globals();
    }

    /// **I2（2026-10-07 评审 Important）恢复窗口内点 shields_up 步：零写命令**。
    /// 规格要求 7 明文「初始化中一律 Deferred——**不写 shields-up**」；而
    /// `run_step("shields_up")` 此前**没过这道门**：恢复窗口里向导该行显示「待做 +
    /// 执行」，用户一点就是窗口内写入，而 `set --shields-up=false` 的回读在初始化期
    /// 必然失败 ⇒ UI 收到「无法确认已关闭」的**假故障**（真机现象）。
    /// 与 `enable` 步同形回执（`deferred` + `backendState` + 成因说明）。
    /// 变异：删掉 run_step("shields_up") 的后端门 → `set`/`get` 被发出，本测试必红。
    #[test]
    fn run_step_shields_up_defers_while_backend_is_initializing() {
        let _g = test_lock();
        let calls = spy_cli(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_BACKEND_INIT.into()),
            // 恢复窗口内**连读都不该读**（读到的 None 会被当成"偏好读不到"的假故障）
            other => Err(format!("恢复窗口内不该有这条调用: {other:?}")),
        });
        let r = run_step("shields_up", 9420).expect("恢复窗口不是错误（用户什么都没做错）");
        assert_eq!(r["deferred"], true, "必须如实回 deferred: {r}");
        assert_eq!(r["backendState"], "NoState");
        assert!(
            r["note"].as_str().unwrap_or("").contains("重连"),
            "说明必须点明成因（后端重连中）: {r}"
        );
        assert_no_write_calls(&calls.lock().unwrap());
        assert_eq!(
            calls.lock().unwrap().len(),
            1,
            "恢复窗口内除 status 外不读不写（shields-up 读取同样会落成假故障）: {:?}",
            calls.lock().unwrap()
        );
        teardown_globals();
    }

    /// **I2 同一条门的探测侧**：`probe_steps_from` 的 `shields_up` 分支必须在初始化
    /// 窗口内如实说「后端重连中——恢复窗口内不写 shields-up」，而不是把
    /// `get --json` 读不到（`None`）报成「读不到 Tailscale 偏好」的**故障**，
    /// 也不是显示成「待做」诱导用户去点。
    /// 变异：删掉 probe_steps_from 的 shields_up 初始化分支 → 落「读不到偏好」故障口径，本测试必红。
    #[test]
    fn shields_up_step_reports_reconnect_while_backend_is_initializing() {
        let _g = test_lock();
        set_run_cli_override(Some(Box::new(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_BACKEND_INIT.into()),
            // 真机形态：初始化期 get --json 读不到偏好（回读失败的同一根因）
            ["get", "--json"] => Err("backend not ready".into()),
            ["funnel", "status", "--json"] => Ok("{}".into()),
            other => Err(format!("恢复窗口内不该有这条调用: {other:?}")),
        })));
        let s = probe_steps_from(9420, true, &read_cli_readings())
            .into_iter()
            .find(|s| s.id == "shields_up")
            .expect("步骤表必须含 shields_up");
        assert!(!s.done, "恢复窗口内不判完成: {s:?}");
        let reason = s.blocked_reason.unwrap_or_default();
        assert!(
            reason.contains("后端正在重连") && reason.contains("不写 shields-up"),
            "必须如实说「恢复窗口内不写 shields-up」（不是「读不到偏好」的假故障）: {reason}"
        );
        teardown_globals();
    }

    /// **I3（2026-10-07 评审 Important）**：`store_tick` 的后端相位→可达性写入必须与
    /// **快照同临界区**（同一个 `DESIRED` 复查内）。竞态（窗口窄、后果滞留）：
    /// 轮询线程读完 `status`（NoState）后、写可达性之前，用户关掉通道 →
    /// `stop_inner_outcome` 先 `invalidate_reachability()` 清成 `Unverified`，随后轮询把
    /// 全局写成 `Recovering`；`DESIRED` 复查失败 ⇒ 快照不写，但**可达性留在 Recovering**。
    /// 载荷于是 `enabled=false ∧ error=null ∧ reach=recovering` ⇒ 卡面落「状态五·恢复中」，
    /// 对用户说「配置与地址都不会变，无需任何操作」——**而通道是他刚关掉的**。且
    /// `DESIRED=None` 期间再无 `store_tick`、退役分支也不碰可达性 ⇒ 错态滞留到再次开启。
    /// 变异：把 `note_backend_initializing/ready` 挪回 `if *desired == Some(port)` 之外 → 必红。
    #[test]
    fn store_tick_does_not_write_reachability_after_desired_is_cleared() {
        let _g = test_lock();
        let calls = spy_cli(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_BACKEND_INIT.into()),
            ["funnel", "status", "--json"] => Ok("{}".into()),
            other => Err(format!("不该有这条调用: {other:?}")),
        });
        // 竞态复现：轮询已读完这一轮读数，但此刻 DESIRED 已被 stop 清空、
        // 可达性已被 invalidate_reachability 作废成 Unverified
        let tick = read_tick(9420);
        set_reachability(Reachability::Unverified);
        assert_eq!(*DESIRED.lock().unwrap(), None, "前置：通道此刻确实已关");

        store_tick(9420, &tick);

        assert_eq!(
            reachability(),
            Reachability::Unverified,
            "DESIRED 复查失败 ⇒ 可达性不得被写成 Recovering（否则刚关掉的通道被渲染成\
             「恢复中……无需任何操作」，且 DESIRED=None 期间无 store_tick 可纠正 ⇒ 滞留）"
        );
        let s = ts_snapshot();
        assert!(
            !s.running && s.error.is_none(),
            "快照同样不得写（既有关闭口径: 默认态）: running={} error={:?}",
            s.running,
            s.error
        );
        assert!(
            !calls.lock().unwrap().is_empty(),
            "夹具自检：本轮确实读了 status（复现的是竞态，不是空转）"
        );
        teardown_globals();
    }

    /// **I3 同族第二处**：`record_deferred_open_failure`（延迟补开通失败留痕）写快照
    /// 也必须过 `DESIRED` 复查——否则停机后仍能把「开机后自动补开通失败」写进一个
    /// 已关闭通道的快照（用户看到的是刚关掉的通道在报错）。
    /// 变异：把 `record_deferred_open_failure` 的快照写入挪到 `DESIRED` 复查之外 → 必红。
    #[test]
    fn record_deferred_open_failure_respects_desired() {
        let _g = test_lock();
        set_ts_snapshot(|s| *s = crate::remote::tunnel::ChannelStatus::default());
        assert_eq!(*DESIRED.lock().unwrap(), None, "前置：通道关着");
        record_deferred_open_failure(9420, "开机后自动补开通 Funnel 失败: ACL 不允许".into());
        let s = ts_snapshot();
        assert!(
            s.error.is_none() && !s.running,
            "通道已关时不得把补开通失败写进快照（它是「开着通道」的失败）: error={:?}",
            s.error
        );
        assert!(
            deferred_open_error().is_some(),
            "留痕照记（下次开启时仍要能看到这次失败），只是不写已关闭通道的快照"
        );
        teardown_globals();
    }

    /// **I4b（2026-10-07 评审 · 测试保真度）**：`note_backend_ready` 只清
    /// `Recovering`、**不动 `Verified/Verifying`** 是写进注释的**承重不变式**——每轮
    /// `Running` 都会调它，若顺手清 `Verified`，地址会在每个轮询窗闪一次（假的"掉线"）。
    /// 此前把它改成无条件 `*g = Unverified` 后 `remote::tailscale::tests` 全绿
    /// ⇒ 该不变式**零防守**。本用例把注释里的不变式**落成测试**（含 `store_tick` 集成路）。
    /// 变异：把 `if matches!(*g, Reachability::Recovering)` 改成无条件 `*g = Unverified`
    /// → ①（store_tick 集成）与 ② 必红。
    #[test]
    fn note_backend_ready_only_clears_recovering() {
        let _g = test_lock();
        *DESIRED.lock().unwrap() = Some(9420);
        let _calls = spy_cli(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_RUNNING.into()),
            ["funnel", "status", "--json"] => Ok(FUNNEL_RESTORED_AFTER_REBOOT.into()),
            other => Err(format!("不该有这条调用: {other:?}")),
        });
        let now = std::time::Instant::now();
        // ① 已 Verified：走**生产路**（store_tick → note_backend_ready）——不得被抢写
        super::reach::observe_config("jarvismac-mini.example-tailnet.ts.net", 9420);
        set_reachability(Reachability::Verified);
        let tick = read_tick(9420);
        store_tick(9420, &tick);
        assert_eq!(
            reachability(),
            Reachability::Verified,
            "已 Verified 不得被就绪轮抢写（每轮 Running 都调本函数；抢写 = 地址每轮闪一次）"
        );
        assert!(
            ts_snapshot().running,
            "夹具自检：配置在 + 后端就绪 ⇒ 快照宣称运行（走的确实是 Ready 那一支）"
        );
        // ② Verifying：用户刚点下的那次校验，结论由它自己写
        set_reachability(Reachability::Verifying);
        note_backend_ready(now);
        assert_eq!(
            reachability(),
            Reachability::Verifying,
            "在验态不得被就绪轮抢写（结论只能由那次校验自己写）"
        );
        // ③ Recovering → Unverified：清掉「恢复中」，交回正常校验流程（这条必须真的发生）
        set_reachability(Reachability::Recovering);
        note_backend_ready(now);
        assert_eq!(
            reachability(),
            Reachability::Unverified,
            "Recovering 必须被清掉（否则「恢复中」滞留，地址永不回到校验流程）"
        );
        teardown_globals();
    }

    /// **W-A 防线②的另一面**：后端转 Running 后配置**确实在** → 正常恢复、**不重开**
    /// （真机 T+73s 实测：tailscaled 自己就把 307 字节配置逐字段恢复了）。
    #[test]
    fn poller_does_not_reopen_when_config_restored_and_snapshot_recovers() {
        let _g = test_lock();
        *DESIRED.lock().unwrap() = Some(9420);
        let calls = spy_cli(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_RUNNING.into()),
            ["funnel", "status", "--json"] => Ok(FUNNEL_RESTORED_AFTER_REBOOT.into()),
            other => Err(format!("配置已在时不该有这条调用: {other:?}")),
        });
        let mut st = PollerState::default();
        let now = std::time::Instant::now();
        for _ in 0..3 {
            assert!(!poller_tick(&mut st, now, &mut |_, _| None));
            assert_no_write_calls(&calls.lock().unwrap());
        }
        let s = ts_snapshot();
        assert!(
            s.running,
            "配置恢复 + 后端就绪 = 正常恢复: running={} error={:?}",
            s.running, s.error
        );
        assert_eq!(
            s.url.as_deref(),
            Some("https://jarvismac-mini.example-tailnet.ts.net/m"),
            "地址一字不差（实测：地址不变）"
        );
        teardown_globals();
    }

    /// **M2（2026-10-07 评审 Minor）**：后端相位跃迁必须**重置轮询退避**。
    /// 恢复窗口内快照恒为 `default`（`map_status` 对初始化的干净口径），与「配置真缺失」
    /// 的快照**逐字相同** ⇒ `changed=false` ⇒ 间隔按 5→10→20→40→60 退避；而补开通要先
    /// 「连续 2 轮就绪」，于是最坏在就绪后 ~2 个窗（~2 分钟）才发生（注释里原写「多等
    /// 一轮 5s 量级」与代码不符）。修法：把**后端相位跃迁本身**当 `changed` 信号。
    /// 变异：把 `phase_changed` 从 `changed` 判据里去掉 → 相位跃迁后间隔仍是退避值，必红。
    #[test]
    fn backend_phase_transition_resets_poll_backoff() {
        let _g = test_lock();
        *DESIRED.lock().unwrap() = Some(9420);
        let round = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let r = round.clone();
        let _calls = spy_cli(move |args: &[&str]| match args {
            ["status", "--json"] => {
                // 每轮一次 status 派生（funnel 那条走下面的分支，不计数）；前 3 轮
                // NoState，其后 Running
                let n = r.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(if n < 3 {
                    STATUS_BACKEND_INIT.into()
                } else {
                    STATUS_RUNNING.into()
                })
            }
            // 配置**始终缺失**（`{}`）：Running 轮的快照与 NoState 轮**逐字相同**，
            // 这样「有没有变化」就只剩相位跃迁一个信号（否则快照自己变也会重置退避）
            ["funnel", "status", "--json"] => Ok("{}".into()),
            other => Err(format!("不该有这条调用: {other:?}")),
        });
        let mut st = PollerState::default();
        let now = std::time::Instant::now();
        for _ in 0..3 {
            assert!(!poller_tick(&mut st, now, &mut |_, _| None));
        }
        assert_eq!(
            st.interval,
            POLL_SECS * 4,
            "恢复窗口内连续同结果（快照逐字相同）→ 退避翻倍: {}",
            st.interval
        );
        // 后端转 Running：**只有相位变了**（快照仍逐字相同）→ 间隔必须回落到 POLL_SECS
        assert!(!poller_tick(&mut st, now, &mut |_, _| None));
        assert_eq!(
            st.interval, POLL_SECS,
            "后端相位跃迁必须重置退避（否则补开通最坏要等 ~2 个退避窗 ≈2 分钟）"
        );
        teardown_globals();
    }

    /// W-A 的**收尾半步（变异锚点）**：后端连续 Running 且配置**确认缺失**
    /// （读数成功 ∧ 归属 Absent）时才补开通——恢复窗口内不判不写，就靠这一步在窗口
    /// 结束后把「确实丢了配置」的通道拉回来；且**一次缺失期只试一次**（失败不无限重试）。
    /// 变异：把延迟补开通分支删掉 → 本测试的 `--bg` 断言必红。
    #[test]
    fn poller_reopens_once_backend_ready_and_config_confirmed_absent() {
        let _g = test_lock();
        *DESIRED.lock().unwrap() = Some(9420);
        let calls = spy_cli(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_RUNNING.into()),
            // 后端就绪但配置确实不在（模拟「真的丢了」——与恢复窗口的 `{}` 形态同形，
            // 区别只在 BackendState 已确认 Running）
            ["funnel", "status", "--json"] => Ok("{}".into()),
            ["set", "--shields-up=false"] => Ok(String::new()),
            ["get", "--json"] => Ok(r#"{"shields-up": false}"#.into()),
            ["funnel", "--bg", "9420"] => Ok(String::new()),
            ["funnel", "status"] => Ok(String::new()),
            other => Err(format!("不该有这条调用: {other:?}")),
        });
        let mut st = PollerState::default();
        let now = std::time::Instant::now();
        let bg_count = |calls: &[Vec<String>]| {
            calls
                .iter()
                .filter(|c| c.contains(&"--bg".to_string()))
                .count()
        };
        // 第 1 轮：后端第一眼 Running 不算「确认」（实测 T+73s 配置才逐字段回来）
        assert!(!poller_tick(&mut st, now, &mut |_, _| None));
        assert_eq!(
            bg_count(&calls.lock().unwrap()),
            0,
            "第一眼 Running 不得立刻判缺失"
        );
        // 第 2 轮：连续 Running + 配置确认缺失 → 补开通恰好一次
        assert!(!poller_tick(&mut st, now, &mut |_, _| None));
        assert_eq!(bg_count(&calls.lock().unwrap()), 1, "确认缺失后必须补开通");
        // 第 3 轮：仍缺失也不重复轰炸（一次缺失期只试一次）
        assert!(!poller_tick(&mut st, now, &mut |_, _| None));
        assert_eq!(bg_count(&calls.lock().unwrap()), 1, "同一次缺失只试一次");
        teardown_globals();
    }

    /// **W-A 第三处判据点（变异锚点）**：撤销预览的职责是"这次撤销会连带清掉什么"，
    /// 而恢复窗口内 `funnel status --json` 会短暂为 `{}`——照旧读就会把"读不到"报成
    /// "什么都没有"（`wouldClear=0`、无 foreign），用户的知情同意因此建立在假信息上。
    /// 后端必须如实回 `unreadable`（**不拒绝撤销**——"公开暴露撤不掉"是更糟的失败模式，
    /// 与 Mixed 放行的权衡同源）。
    /// 变异：删掉 disable_preview 的后端门 → 本测试必红。
    #[test]
    fn disable_preview_reports_unreadable_while_backend_is_initializing() {
        let _g = test_lock();
        let _calls = spy_cli(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_BACKEND_INIT.into()),
            ["funnel", "status", "--json"] => Ok("{}".into()),
            other => Err(format!("恢复窗口内不该有这条调用: {other:?}")),
        });
        let r = run_step("disable_preview", 9420).expect("预览步应 Ok（只读）");
        assert_eq!(
            r["unreadable"], true,
            "后端未就绪时必须如实说「读不到配置形态」，不得报成没有条目: {r}"
        );
        assert!(
            r["note"].as_str().unwrap_or("").contains("重连"),
            "说明文字必须点明成因（后端重连中）: {r}"
        );
    }

    /// **M1 上界判据（纯函数）**：3 分钟是「正常开机恢复」的实测上界（T+73s 后端就绪 /
    /// T+2.5min 公网 200）；**没有起点不算超窗**（不编判据）；降级文案点名后端状态与
    /// 已等分钟数，并给出升级路径（向导重试）。
    /// 变异：把 BACKEND_INIT_GRACE 调大/删掉 overdue 判据 → 本用例必红。
    #[test]
    fn backend_init_overdue_bound_and_reason_are_named() {
        let t0 = std::time::Instant::now();
        assert!(
            !backend_init_overdue(Some(t0), t0 + std::time::Duration::from_secs(179)),
            "179 秒仍在宽限窗内（实测 1–2 分钟是常态）"
        );
        assert!(
            backend_init_overdue(Some(t0), t0 + BACKEND_INIT_GRACE),
            "到达上界即超窗"
        );
        assert!(
            !backend_init_overdue(None, t0),
            "没有起点不算超窗（不编判据：没见过未就绪就不许说「持续未就绪」）"
        );
        let r = backend_init_overdue_reason("NoState", std::time::Duration::from_secs(301));
        assert!(r.contains("NoState") && r.contains("5 分钟"), "{r}");
        assert!(
            r.contains("向导") && r.contains("重试"),
            "必须给出升级路径: {r}"
        );
        assert_ne!(r, RECOVERING_HINT, "降级文案不得等于「恢复中」口径: {r}");
    }

    /// **M1（2026-10-07 评审 Minor）「恢复中」必须有界**：后端 `NoState` 长期不就绪时，
    /// 旧实现是**无界承诺**——`note_backend_initializing` 没有时间上界、`map_status`
    /// 恒返回干净快照（无 error）、卡面恒说「通常 1–2 分钟」且无重试/升级路径；
    /// 而 180 秒的界此前只加在「解析得到但连不通」那一路。线稿状态五第 2 行自己承诺
    /// 「宽限窗过后…如实点名成因」。修法：进程内累计未就绪超 [`BACKEND_INIT_GRACE`]
    /// → 降级为 `Failed{reason:"后端持续未就绪（NoState，已 X 分钟）…"}` + 快照 error
    /// 如实点名；后端就绪后交回正常校验流程（降级位清零）。
    /// 变异：去掉 `store_tick` 里的 overdue 分支（回到只 note_backend_initializing）→ ②必红。
    #[test]
    fn backend_init_degrades_after_grace_with_named_cause() {
        let _g = test_lock();
        *DESIRED.lock().unwrap() = Some(9420);
        let ready = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let r = ready.clone();
        let _calls = spy_cli(move |args: &[&str]| match args {
            ["status", "--json"] => Ok(if r.load(std::sync::atomic::Ordering::SeqCst) {
                STATUS_RUNNING.into()
            } else {
                STATUS_BACKEND_INIT.into()
            }),
            ["funnel", "status", "--json"] => Ok(if r.load(std::sync::atomic::Ordering::SeqCst) {
                // 配置由 tailscaled 自己恢复（真机 T+73s 形态）
                r#"{"Web":{"jarvismac-mini.example-tailnet.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:9420"}}}}}"#.into()
            } else {
                "{}".into()
            }),
            other => Err(format!("不该有这条调用: {other:?}")),
        });
        let now = std::time::Instant::now();
        // ① 窗口内（刚见到初始化）：干净快照 + Recovering（现状不变——不是故障）
        set_backend_init_since(Some(now));
        refresh_once(9420);
        assert!(
            ts_snapshot().error.is_none(),
            "窗口内不报故障: {:?}",
            ts_snapshot().error
        );
        assert_eq!(reachability(), Reachability::Recovering);

        // ② 超过上界：降级——如实点名「后端持续未就绪（NoState，已 X 分钟）」
        set_backend_init_since(Some(
            now - BACKEND_INIT_GRACE - std::time::Duration::from_secs(30),
        ));
        refresh_once(9420);
        let e = ts_snapshot().error.clone().unwrap_or_default();
        assert!(
            e.contains("持续未就绪") && e.contains("NoState") && e.contains("分钟"),
            "超窗后必须如实点名成因与已等时长（不再是「通常 1–2 分钟」的无界承诺）: {e:?}"
        );
        assert!(
            e != RECOVERING_HINT && !e.contains("无需任何操作"),
            "降级文案不得继续套用「恢复中，无需任何操作」的无界安抚: {e:?}"
        );
        assert!(
            matches!(reachability(), Reachability::Failed { .. }),
            "超窗后不得继续宣称「恢复中」（无界承诺 = 掩盖真故障）: {:?}",
            reachability()
        );

        // ③ 后端就绪：降级位清零 → 交回正常校验流程（不是一直钉在 Failed）
        ready.store(true, std::sync::atomic::Ordering::SeqCst);
        refresh_once(9420);
        assert_eq!(
            reachability(),
            Reachability::Unverified,
            "后端就绪必须交回正常校验流程（否则降级态滞留，地址直到下次重验才回来）"
        );
        let s = ts_snapshot();
        assert!(
            s.running && s.error.is_none(),
            "配置在 + 后端就绪 = 正常运行: running={} error={:?}",
            s.running,
            s.error
        );
        teardown_globals();
    }

    /// W-A 同一条纪律的**第二处判据点（自愈）**：恢复窗口内点「重试」也**不得**
    /// `funnel reset` + 重开——那正好把 tailscaled 正在恢复的配置清掉（实测 T+73s 它
    /// 自己就回来了）。如实报「恢复中」，等窗口过去。
    /// 变异：把 heal_and_reverify 的 BackendState 门删掉 → reset 被发出，本测试必红。
    #[test]
    fn heal_never_resets_funnel_while_backend_is_initializing() {
        let _g = test_lock();
        *DESIRED.lock().unwrap() = Some(9420);
        let calls = spy_cli(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_BACKEND_INIT.into()),
            ["funnel", "status", "--json"] => Ok("{}".into()),
            other => Err(format!("恢复窗口内不该有这条调用: {other:?}")),
        });
        let token = begin_verification("node.tailnet.ts.net", 9420);
        let _r = heal_and_reverify(9420, &token);
        assert_no_write_calls(&calls.lock().unwrap());
        teardown_globals();
    }

    /// W-A 收尾半步的**诚实性**：延迟补开通**失败**必须上墙且**留得住**
    /// （`map_status` 每轮重算会把快照 error 覆盖掉——只写快照就只闪一个轮询窗，
    /// 用户看到的是"没反应"）。停机/重新开启时留痕翻页（不得在下次开启时"复活"）。
    /// 变异：删掉 `store_tick` 的留痕合并（`snap.error = deferred_open_error()`）→ 第 3 轮必红。
    #[test]
    fn deferred_open_failure_is_sticky_on_snapshot_and_cleared_on_stop() {
        let _g = test_lock();
        *DESIRED.lock().unwrap() = Some(9420);
        let _calls = spy_cli(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_RUNNING.into()),
            ["funnel", "status", "--json"] => Ok("{}".into()),
            ["set", "--shields-up=false"] => Ok(String::new()),
            ["get", "--json"] => Ok(r#"{"shields-up": false}"#.into()),
            // 补开通失败（真机可能形态：尾网 ACL 不允许 / CLI 一时失败）
            ["funnel", "--bg", "9420"] => Err("ACL 不允许 Funnel".into()),
            ["funnel", "reset"] => Ok(String::new()), // 停机撤销
            other => Err(format!("不该有这条调用: {other:?}")),
        });
        let mut st = PollerState::default();
        let now = std::time::Instant::now();
        // 两轮让判据成立并触发补开通（失败）
        assert!(!poller_tick(&mut st, now, &mut |_, _| None));
        assert!(!poller_tick(&mut st, now, &mut |_, _| None));
        let e = ts_snapshot().error.unwrap_or_default();
        assert!(
            e.contains("补开通") && e.contains("ACL 不允许 Funnel"),
            "补开通失败必须如实上墙（带真原因）: {e:?}"
        );
        // 再推一轮：map_status 重算会把快照 error 清成 None——留痕必须把它顶回来
        assert!(!poller_tick(&mut st, now, &mut |_, _| None));
        let e = ts_snapshot().error.unwrap_or_default();
        assert!(
            e.contains("补开通"),
            "失败留痕必须 sticky（否则只闪一个轮询窗 = 用户看到「没反应」）: {e:?}"
        );
        assert_eq!(
            *DESIRED.lock().unwrap(),
            Some(9420),
            "补开通失败不改期望态（通道仍开着，用户可重试）"
        );
        // 停机 → 留痕翻页；重新开启时不得"复活"成当前故障
        assert!(stop_channel().is_ok());
        assert!(deferred_open_error().is_none(), "停机必须清掉留痕");
        teardown_globals();
    }

    // ============================================================
    // W-B：可达性口径分三档（首次开通 / 重新开通 / 开机恢复）
    // ------------------------------------------------------------
    // 实测三档（2026-10-07 真机）：**首次开通**记录发布 ≈5–6 分钟；**reset 后重开**
    // ≈30–49 秒；**重启恢复**压根不用等 DNS（记录不撤销），只需等后端重连 1–2 分钟。
    // 旧口径固定一句「域名生效中，通常需要 5 分钟左右」——对重启恢复是**误导**
    //（把 1–2 分钟说成 5 分钟，还指错成因：不是"域名发布"而是"后端重连"）。
    //
    // 判据必须可证（不编判据）：
    //  ① 开机恢复 = 后端初始化窗口（W-A 已埋点：见过 `NoState/Starting` → 首个 Running
    //     后 3 分钟宽限窗内"解析得到但连不通"）；
    //  ② 重新开通 = **本进程内 兔维斯 自己执行过 `funnel reset`**（`disable_funnel` 是
    //     reset 的唯一调用点，故这个事实可证），且验过（Verified）后翻页；
    //  ③ 其余（默认）= 记录尚未发布：文案**两界都给**（首次实测 5–6 分钟 / 此前开通过
    //     通常 1 分钟内）——"是不是首次"在「兔维斯 关着时被人手动 reset」的情形下无法证实，
    //     宁可给区间也不编判据。
    // ============================================================

    /// **W-B 分档（变异锚点）**：`reset` 后重开与首次开通必须是**两个可分辨的档**。
    /// 判据 = 本进程内是否执行过 `funnel reset`；验过（Verified）后翻页——记录又发布了，
    /// 下一次"没记录"不该继续算重开档。
    /// 变异：删掉 `disable_funnel` 里的 `note_funnel_reset()` → ②档必红。
    #[test]
    fn record_pending_tier_distinguishes_republish_from_first_open() {
        let _g = test_lock();
        let absent = |_h: &str| Ok(vec![]); // 权威否定：记录尚未发布
                                            // ① 首次开通档（本进程没有 reset 过）
        set_reachability(Reachability::Verified); // 生产规则：验过 = 翻页
        let r = reverify_with("a.ts.net", 9420, &absent);
        let j = serde_json::to_value(&r).expect("可达性可序列化");
        assert_eq!(
            j["state"], "record_pending",
            "「记录尚未发布」是**正常发布延迟**，不是故障（旧实现报 Failed）: {j}"
        );
        assert_eq!(j["republish"], false, "没 reset 过 = 首次开通档: {j}");

        assert!(
            !reset_seen_raw(),
            "起点：本进程还没 reset 过（生产规则：Verified 翻页）"
        );
        // ② 重新开通档：本进程内真的执行过 `funnel reset`（生产唯一写点）
        let _calls = spy_run_cli(Ok("{}".into()));
        disable_funnel().expect("撤销原语应成功");
        assert!(
            reset_seen_raw(),
            "`disable_funnel` 是 `funnel reset` 的唯一调用点——必须记下这个可证事实"
        );
        let r = reverify_with("a.ts.net", 9420, &absent);
        let j = serde_json::to_value(&r).expect("可达性可序列化");
        assert_eq!(j["republish"], true, "reset 后重开 = 重新开通档: {j}");

        // ③ 验过之后翻页（记录又发布了）：下一次"没记录"回到首次开通档
        set_reachability(Reachability::Verified);
        assert!(
            !reset_seen_raw(),
            "Verified 必须翻页（否则一次 reset 会把整个进程的档位永久钉死）"
        );
        let r = reverify_with("a.ts.net", 9420, &absent);
        let j = serde_json::to_value(&r).expect("可达性可序列化");
        assert_eq!(
            j["republish"], false,
            "Verified 必须翻页（否则一次 reset 会把整个进程的档位永久钉在「重开」档）: {j}"
        );
        teardown_globals();
    }

    /// **W-B 文案分档（变异锚点）**：两档文案必须各自点明**成因与实测时长**，且互不冒充。
    /// 变异：把 `record_pending_hint` 改成恒返回一个常量 → 本测试必红。
    #[test]
    fn record_pending_hints_state_their_own_cause_and_duration() {
        let first = record_pending_hint(false);
        let again = record_pending_hint(true);
        assert_ne!(first, again, "两档必须是两句不同的话");
        assert!(
            first.contains("尚未发布") && first.contains("5"),
            "首次开通档必须给出实测时长（≈5–6 分钟）: {first}"
        );
        assert!(
            first.contains("1 分钟"),
            "同时给出「此前开通过」的下界（兔维斯 关着时被手动 reset 的情形无法证实，给区间不编判据）: {first}"
        );
        assert!(
            again.contains("重新开通") && again.contains("1 分钟"),
            "重新开通档必须点明成因（刚重新开通）+ 实测时长（30 秒～1 分钟）: {again}"
        );
        assert!(
            !again.contains("5"),
            "重新开通档不得再挂「5 分钟」（实测 30–49 秒，说 5 分钟就是误导）: {again}"
        );
    }

    /// **W-B 开机恢复档（变异锚点）**：连接失败**只在开机恢复窗口内**算「恢复中」；
    /// 窗口外仍是如实失败（带每条地址的错误）——恢复中不得变成掩盖真故障的挡箭牌。
    /// 变异：把 `verify_reachability` 的 boot 判据删掉 → ①档必红（会报 Failed）。
    #[test]
    fn connect_failure_is_recovering_only_inside_boot_window() {
        let _g = test_lock();
        // 记录解析得到（DoH 有两条 A），但钉 IP 请求全失败（实测 T+89s：TLS 失败 curl 35）
        let probe = |_h: &str| Ok(vec!["203.0.113.10".to_string(), "203.0.113.11".to_string()]);
        set_http_probe_override(Some(Box::new(|_h, _ip| {
            Err("error sending request: invalid peer certificate".into())
        })));
        // ① 开机恢复窗口内（本进程刚见过后端初始化）：是「恢复中」，不是故障
        set_boot_recovery_override(Some(true));
        let r = verify_reachability("a.ts.net", &probe);
        assert_eq!(
            r,
            Reachability::Recovering,
            "开机恢复窗口内连不通 = 后端/公网路径还没起来，不是故障: {r:?}"
        );
        // ② 窗口外（或本进程没见过初始化窗口）：如实失败，带每条地址的错误
        set_boot_recovery_override(Some(false));
        let r = verify_reachability("a.ts.net", &probe);
        match r {
            Reachability::Failed { reason } => {
                assert!(
                    reason.contains("连不通") && reason.contains("203.0.113.1"),
                    "窗口外必须如实报「解析得到但连不通」并逐条给地址: {reason}"
                );
            }
            other => panic!("窗口外的连接失败必须是 Failed: {other:?}"),
        }
        teardown_globals();
    }

    /// 开机恢复窗口判据（纯函数）：**必须有证据**（本进程见过后端初始化窗口）**且有界**
    /// （首个 Running 后 3 分钟）——没有证据就不许说"正在恢复"（那会把常年连不通的
    /// 故障说成"再等等"）。
    #[test]
    fn boot_recovery_window_requires_evidence_and_is_bounded() {
        let t0 = std::time::Instant::now();
        let secs = |s: u64| t0 + std::time::Duration::from_secs(s);
        assert!(
            boot_recovery_window(true, Some(t0), secs(10)),
            "见过初始化窗口 + 刚 Running = 恢复窗口内（实测 T+89s 仍连不通）"
        );
        assert!(
            boot_recovery_window(true, Some(t0), secs(179)),
            "窗口上界内仍算恢复中"
        );
        assert!(
            !boot_recovery_window(true, Some(t0), secs(181)),
            "超过上界必须交回如实失败（恢复中不得成为掩盖真故障的挡箭牌）"
        );
        assert!(
            !boot_recovery_window(false, Some(t0), secs(10)),
            "没见过初始化窗口（= 不是开机恢复）= 不许说「正在恢复」"
        );
        assert!(
            !boot_recovery_window(true, None, secs(10)),
            "还没见过 Running（窗口起点未知）不算恢复窗口"
        );
    }

    /// W-B 埋点接线（W-A store_tick 的记账面）：后端初始化 → 记证据 + 可达性置恢复中；
    /// 转 Running → 记首个就绪时刻（窗口起点）。
    /// 变异：删掉 store_tick 的 backend 分支 → 本测试必红。
    #[test]
    fn backend_phase_notes_boot_evidence_for_recovery_window() {
        let _g = test_lock();
        *DESIRED.lock().unwrap() = Some(9420);
        let _calls = spy_cli(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_BACKEND_INIT.into()),
            ["funnel", "status", "--json"] => Ok("{}".into()),
            other => Err(format!("不该有这条调用: {other:?}")),
        });
        assert!(!backend_init_seen_raw(), "测试起点：不得有残留证据");
        store_tick(9420, &read_tick(9420));
        assert_eq!(
            reachability(),
            Reachability::Recovering,
            "后端初始化 → 可达性必须置「恢复中」（恢复窗口的对外语义）"
        );
        assert!(
            backend_init_seen_raw(),
            "必须记下「见过初始化窗口」这条证据"
        );
        assert!(
            first_backend_ready_raw().is_none(),
            "还没 Running：窗口起点未知"
        );
        // 后端转 Running → 记首个就绪时刻
        let _calls = spy_cli(|args: &[&str]| match args {
            ["status", "--json"] => Ok(STATUS_RUNNING.into()),
            ["funnel", "status", "--json"] => Ok(FUNNEL_RESTORED_AFTER_REBOOT.into()),
            other => Err(format!("不该有这条调用: {other:?}")),
        });
        store_tick(9420, &read_tick(9420));
        assert!(
            first_backend_ready_raw().is_some(),
            "Running 必须记下窗口起点（宽限窗从它开始算）"
        );
        assert_eq!(
            reachability(),
            Reachability::Unverified,
            "就绪后不得停在「恢复中」（交回正常校验流程，否则地址永远不上墙）"
        );
        teardown_globals();
    }
}
