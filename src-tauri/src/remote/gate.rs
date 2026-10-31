// gate 中间件：/m/api/* 全量过闸（403）——放行名单仅 /pair/pin（换 cookie 的入口）
// （2026-10-06 零豁免：回环豁免已整块删除，鉴权只认设备凭据，不变量 G1）
// cookie 解析手写（不加 cookie 依赖）：mam_device=<hex>
//
// 路径语义（重要，勿按直觉改）：本中间件由 `server.rs` 作为 **nest("/m/api/v1") 的内层 layer**
// 挂载，axum 的 nest 会**剥掉前缀**——这里 `req.uri().path()` 看到的是 `/pair/pin`、
// `/sessions`、`/nope`，而**不是** `/m/api/v1/pair/pin`。放行名单必须用相对路径，写成
// 绝对路径会导致 pair 被 403（无法换 cookie）或未知路径漏放，两种错法都不报编译错。
// （Task 7 追加静态路由不经过本中间件：那些路由注册在 nest 之外的外层 Router。）

use axum::{
    extract::{Request, State},
    http::header,
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::Arc;

use super::server::RemoteState;

/// Host 头归一（纯函数）：小写 + 剥端口（Host 可带端口 `host:9420`，split(':') 取首段）。
/// 隧道域名恒为裸域名无端口；IPv6 字面量 Host（`[::1]:9420`）剥后失真只会更严格
/// （剥不出域名 → 不匹配隧道名单 → 按"非隧道 Host"判定，方向安全）
pub fn normalize_host(host: &str) -> String {
    host.trim()
        .to_ascii_lowercase()
        .split(':')
        .next()
        .unwrap_or("")
        .to_string()
}

/// 配对来源标注（**纯装饰**，不变量 G1 推论③）：按 Host 命中哪条通道的域名名单，
/// 返回 `quick` / `named` / `tailscale`（§C1 固定网址）；未命中一律 `lan`。
///
/// **不得用于任何安全判定**——它读的是请求头（Host），**可被伪造**；这里只为在设备
/// 花名册上给用户一个"从哪条通道进来的"提示，允许不准确。
/// 2026-10-06（设计说明书 §G3）：本函数不再接受来源地址、不再返回 `local`——本机
/// 不是一条通道。历史数据里已存的 `local` 标注前端仍可渲染，但服务端不再产生。
/// **tailscale 分支为何必须存在**（§C1 地图点名的坑）：`.ts.net` 机器域名不在
/// quick/named 名单里，缺这个分支会把 Tailscale 通道的连接误标成「局域网」。
pub fn classify_via(
    host: &str,
    quick_hosts: &[String],
    named_hosts: &[String],
    tailscale_hosts: &[String],
) -> &'static str {
    let h = normalize_host(host);
    if h.is_empty() {
        return "lan";
    }
    if quick_hosts.iter().any(|t| normalize_host(t) == h) {
        "quick"
    } else if named_hosts.iter().any(|t| normalize_host(t) == h) {
        "named"
    } else if tailscale_hosts.iter().any(|t| normalize_host(t) == h) {
        "tailscale"
    } else {
        "lan"
    }
}

/// Cloudflare 系通道的权威来源头（**实测唯一可信**，2026-10-06 经 quick tunnel 回显
/// 全部请求头）：伪造它 → Cloudflare 边缘直接 403，整个请求被拒（比"覆盖"更强）。
/// - `X-Forwarded-For`：真实值被**追加在后**（`伪造值,真实值`），前缀可注入 → 不可裸信；
/// - `True-Client-IP`：**原样透传**，可伪造 → 绝不可采信；
/// - `X-Real-IP`：被边缘丢弃。
pub const CF_AUTHORITATIVE_HEADER: &str = "cf-connecting-ip";

/// 限速信任通道的**声明项**（评审 A-I2 确立的结构性单一声明处）：一条通道 =
/// 通道名 + 该通道**已登记域名** + 该通道**静态声明的权威来源头**。
///
/// 不变量：**未出现在声明表里的通道一律回落全局桶**——这是结构事实而非注释纪律：
/// [`rate_bucket_key`] 只能取声明表里写明的头，取不到（无声明 / Host 不命中 / 头缺失）
/// 即 fail-closed 回落。新通道接入必须显式新增声明**并附实测证据**（如"伪造该头被
/// 边缘拒绝"），注释纪律不再承担这条。
#[derive(Debug, PartialEq, Eq)]
pub struct RateBucketChannel {
    /// 通道名（审计与测试断言用；生产 = `quick` / `named`）
    pub name: &'static str,
    /// 该通道已登记域名（比较前经 [`normalize_host`] 归一）
    pub hosts: Vec<String>,
    /// 该通道的权威来源头（小写头名）
    pub authoritative_header: &'static str,
}

impl RateBucketChannel {
    /// Cloudflare 系通道声明（**唯一构造入口**：权威头在此钉死为实测背书的
    /// [`CF_AUTHORITATIVE_HEADER`]，调用方无法"顺手"换成别的头）
    pub fn cloudflare(name: &'static str, hosts: Vec<String>) -> Self {
        Self {
            name,
            hosts,
            authoritative_header: CF_AUTHORITATIVE_HEADER,
        }
    }

    /// Host 是否命中本通道（归一后比较；空 Host 恒不命中）
    fn matches_host(&self, normalized_host: &str) -> bool {
        !normalized_host.is_empty()
            && self
                .hosts
                .iter()
                .any(|t| normalize_host(t) == normalized_host)
    }
}

/// Cloudflare 两路（临时隧道 / 自有域名）的声明表（**生产唯一构造点**）：本仓库只有
/// 这两条通道被实测背书。tailscale（§C1 第三路）**刻意不进表**——Funnel 没有
/// Cloudflare 边缘，同名头在其上可被任意伪造（旧实现在 api.rs 用特例注释排除它，
/// 现在由「未声明 ⇒ 回落全局桶」结构性承担，特例注释一并删除）。
pub fn cloudflare_rate_channels(quick: Vec<String>, named: Vec<String>) -> Vec<RateBucketChannel> {
    vec![
        RateBucketChannel::cloudflare("quick", quick),
        RateBucketChannel::cloudflare("named", named),
    ]
}

/// 限速信任声明源（`RemoteState` 字段类型）：每次请求现算声明表（快照可变）；
/// 空表 = 无任何通道被信任 → 回环来源一律全局桶（fail-closed）。
pub type RateBucketChannelsSource = dyn Fn() -> Vec<RateBucketChannel> + Send + Sync;

/// 权威来源取值（信任判定的**唯一实现点**，私有）：Host 命中**已声明**通道 **且**
/// 该通道声明的权威头在场、可转 str、trim 后非空 → Some(值)；否则 None（fail-closed）。
///
/// **"Host 命中"能证明什么（如实口径，2026-10-07 修订）**——原注释写"等价于确实经它
/// 进来"，**只对真的过了 Cloudflare 边缘的流量成立**：
/// ① 过边缘的流量：伪造 Host 会被边缘直接 403（2026-10-06 同日实测两次），故对这种流量
///    "Host 命中该通道已登记域名"确实等价于"确实经它进来"（那是**闭源边缘行为**，证据
///    台账已注明"不得作为设计依赖"，本注释同样不把它当保证）；
/// ② **同机转发不受这条保护**：同机反向代理（或任何能触达回环端口的转发）可以**同时**
///    伪造 `Host`（填成登记域名）与 `CF-Connecting-IP`（填成任意值）⇒ 攻击者**自选限速
///    分桶**。"Host 命中"因此**不等于**"经边缘进来"，本函数的结论只在①成立时可靠；
/// ③ 影响**有界且只限分桶**：放行判定只看设备凭据（见本模块 [`gate`] 与不变量 §G1——
///    任何请求头**进不了鉴权**，多一个头也改不了它）；失效的只是限速——每次失败仍记
///    全局桶（`api::pair_pin` ③），于是单个攻击者的可用额度从「每来源 5 次 / 窗口」
///    **退化为**「全局 50 次 / 窗口」（[`crate::remote::pin::MAX_FAILURES`] /
///    `GLOBAL_MAX_FAILURES`，窗口同为 10 分钟）：4 位 PIN 全量 10⁴ 组合的最坏耗时从
///    ≈2×10⁴ 分钟缩到 ≈2×10³ 分钟（同一比值：10 倍）；且单个伪造者能把全局桶填满 ⇒
///    `/pair/pin` 对**所有人**锁 10 分钟（可按窗口循环重放）——配对入口的有界可用性
///    代价，已配对设备的读写不经该桶、不受影响；
/// ④ 为何**不**立刻加固成"回环来源一律不信 Host"：cloudflared / tailscaled 都是**本机
///    进程**，隧道流量到达本地端口时 TCP 对端**恒为回环地址**（2026-10-06 实测；§G5
///    问题陈述同源）——一刀切会让**真隧道流量也全落全局桶**，正好退回本设计要修的
///    "隧道下所有配对尝试共用一个桶、陌生人几次输错就锁住所有人"。故这条 Host 检查
///    本身就是该路径存在的理由；真修法需要**更强的来源证据**（例如让通道在本机侧注入
///    一个只有它知道、别的本机转发者拿不到的共享密钥头——需先核实连接器是否提供该
///    能力，属新的实现面与新的待验项），成本与风险须重新评估后由控制方裁决，**勿顺手改**；
/// ⑤ 威胁模型缓和（为何可接受）：利用②**必须先能触达回环端口**（即已在机器上——那时
///    配对 PIN 就显示在应用里），收益只是"限速分桶自选 + 有界的配对入口 DoS"。
///
/// 为何必须走声明表：权威头**按通道静态声明**，**不得**在运行时按请求内容推断
/// "该信哪个头"。
fn declared_public_source<'a>(
    host: &str,
    headers: &'a axum::http::HeaderMap,
    channels: &[RateBucketChannel],
) -> Option<&'a str> {
    let normalized = normalize_host(host);
    let ch = channels.iter().find(|c| c.matches_host(&normalized))?;
    headers
        .get(ch.authoritative_header)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// 配对限速的**分桶键**推导（纯函数，安全关键）：按来源「可证性」分档，
/// **不可证即 fail-closed**（设计说明书 §G5；信任边界由**声明表**给定，评审 A-I2）。
///
/// 分档规则：
/// 1. 来源**非回环** → 直接用来源地址。真实且不可伪造，覆盖局域网与一切直连来源；
/// 2. 来源**回环**（流量经本机连接器进来）→ 只有**同时**满足「`Host` 命中声明表里某条
///    通道的已登记域名」**且**「该通道声明的权威头在场」时，才用该头；
/// 3. 其余一切情况（含**未声明的通道**、**直连域名 + 同机反代**、声明表为空）→
///    **回落全局桶**。宁可更严，**绝不**回落成"每请求一个新桶"（那等于取消限速）。
///
/// **不得用于放行判定**——它只进限速与审计（不变量 §G1 推论③的同源纪律）。
/// 它可能返回内部哨兵 [`crate::remote::pin::GLOBAL_BUCKET`]，故**不得**写进设备指纹 /
/// 花名册（那是 [`display_origin`] 的职责，评审 A-M3）。
pub fn rate_bucket_key(
    source: std::net::IpAddr,
    host: &str,
    headers: &axum::http::HeaderMap,
    channels: &[RateBucketChannel],
) -> String {
    // 档 1：非回环来源，真实不可伪造
    if !source.is_loopback() {
        return source.to_string();
    }
    // 档 2 / 3：回环来源需 Host 命中已声明通道 + 该通道权威头在场；否则全局桶
    declared_public_source(host, headers, channels)
        .map(str::to_string)
        .unwrap_or_else(|| crate::remote::pin::GLOBAL_BUCKET.to_string())
}

/// 展示 / 指纹用的**来源**（评审 A-M3；§G5「来源记录必须能区分公网来源」）：
/// 与 [`rate_bucket_key`] 共用同一套信任判定（单一实现点 [`declared_public_source`]），
/// 但**不可证时回落真实 TCP 对端地址**（回环即 `127.0.0.1`）而非内部哨兵——
/// 写进 `remote_devices.origin_ip` 的值必须是来源地址，**绝不含 NUL 哨兵**。
///
/// 两者**不得混用**：分桶键可以含哨兵（限速内部态），展示来源不行。
/// **本函数值不参与设备指纹**（评审 B 追加项把两个职责分开）：它只进
/// `remote_devices.origin_ip` 展示列（花名册/审计看得到真实来源）；指纹输入由调用方
/// 显式给定——生产配对路径传**原始 TCP 对端**（`persist_device_with_fingerprint` 的
/// `fingerprint_origin`），故"同一浏览器换网络重新配对"是覆盖原行、不新增、不占名额
/// （分离理由见 `pairing::persist_device_with_fingerprint` 文档）。
pub fn display_origin(
    source: std::net::IpAddr,
    host: &str,
    headers: &axum::http::HeaderMap,
    channels: &[RateBucketChannel],
) -> String {
    if !source.is_loopback() {
        return source.to_string();
    }
    declared_public_source(host, headers, channels)
        .map(str::to_string)
        .unwrap_or_else(|| source.to_string())
}

/// 设备 cookie 名（唯一来源：此处解析与 api::pair_pin 下发都引用它，避免两处字面量漂移）
pub const COOKIE_NAME: &str = "mam_device";

/// 从 Cookie 头解析设备 id；无 / 空值 → None
pub fn extract_device(headers: &axum::http::HeaderMap) -> Option<String> {
    let prefix = format!("{COOKIE_NAME}=");
    let raw = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(';'))
        .map(str::trim)
        .find_map(|s| s.strip_prefix(prefix.as_str()))?;
    let v = raw.trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// 门禁：`/pair/pin` 放行（PIN 即凭据）+ 其余请求必须带有效设备凭据，否则 403。
///
/// **不变量 G1（2026-10-06 零豁免，勿退化）**：任何请求头、来源地址、进程归属
/// **不得参与鉴权判定**。判据只能来自服务端自身状态——**本中间件只查设备表**
/// （配对入口另有服务端侧的速率限制，属 `pin` 模块，不来自请求数据）。
/// 历史上曾有一条「回环 + Host 不在隧道名单 → 免密」的本机豁免：它以**黑名单**
/// 为判据（fail-open），且在 macOS 上没有第二道防线——任何原样透传 Host 的入站
/// 通道都能把它打成免密全读写。该豁免已整块删除；**新增任何通道不需要在此处
/// 新增代码、配置或名单**。
///
/// 路径语义（重要，勿按直觉改）：本中间件由 `server.rs` 作为 **nest("/m/api/v1")
/// 的内层 layer** 挂载，axum 的 nest 会**剥掉前缀**——这里 `req.uri().path()` 看到的是
/// `/pair/pin`、`/sessions`、`/nope`，而**不是** `/m/api/v1/pair/pin`。放行名单必须用
/// 相对路径，写成绝对路径会导致 pair 被 403。
pub async fn gate(State(state): State<Arc<RemoteState>>, req: Request, next: Next) -> Response {
    if req.uri().path() == "/pair/pin" {
        return next.run(req).await; // PIN 即凭据的换 cookie 入口放行
    }
    let Some(device) = extract_device(req.headers()) else {
        return unauthorized();
    };
    let now = chrono::Utc::now().timestamp_millis();
    // 有效判定与滑动续期合并进**同一个** with 闭包（`with` 持锁不可重入，且两段之间
    // 存在 revoke 插入的 TOCTOU 窗口）
    let ok = state.store.with(|c| {
        let valid = crate::remote::pairing::device_valid(c, &device, now);
        if valid {
            crate::remote::pairing::touch_device(c, &device, now);
        }
        valid
    });
    if ok {
        // 远控在线心跳（Task 4 评审修订）：**只记已认证请求**——本层原先挂在
        // `api_router` 最外层（gate 之前），端口扫描 / 未知路径等未认证命中也会把
        // 节奏升到 Idle 10s 档并持续 60s。移到过闸成功之后：鉴权判定与既有行为
        // 完全不变，只是心跳不再采信未认证流量（/pair/pin 放行分支同样不记——
        // 换 cookie 不算「有人在看」，与线上口径一致）
        crate::remote::note_api_request();
        next.run(req).await
    } else {
        unauthorized()
    }
}

/// 统一拒绝响应：403 且响应体为空——不泄露"未配对 / 设备失效"的区分
fn unauthorized() -> Response {
    axum::http::StatusCode::FORBIDDEN.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    /// Host 归一：小写 + 剥端口；空白 trim
    #[test]
    fn normalize_host_lowercases_and_strips_port() {
        assert_eq!(normalize_host("Mam.Example.COM:9420"), "mam.example.com");
        assert_eq!(normalize_host("mam.example.com"), "mam.example.com");
        assert_eq!(normalize_host("  LocalHost:80 "), "localhost");
        assert_eq!(normalize_host(""), "");
    }

    /// via 四分支（装饰字段，§C1 起四值）：quick 命中 → quick；named 命中 → named；
    /// tailscale 机器域名命中 → tailscale；其余（含空 Host）→ lan。
    /// **不再有 local 分支**（本机不是通道，2026-10-06 §G3）。
    #[test]
    fn classify_via_four_branches() {
        let quick = vec!["q-test.trycloudflare.com".to_string()];
        let named = vec!["mam.example.com".to_string()];
        let ts = vec!["JarvisMac-Mini.example-tailnet.ts.net".to_string()];
        assert_eq!(
            classify_via("q-test.trycloudflare.com", &quick, &named, &ts),
            "quick"
        );
        assert_eq!(
            classify_via("mam.example.com:443", &quick, &named, &ts),
            "named",
            "带端口归一后匹配"
        );
        assert_eq!(
            classify_via("jarvismac-mini.example-tailnet.ts.net", &quick, &named, &ts),
            "tailscale",
            "机器域名命中第四分支（大小写归一后匹配）"
        );
        assert_eq!(
            classify_via("其他机器.example-tailnet.ts.net", &quick, &named, &ts),
            "lan",
            "同尾网的其他机器不在名单 → lan（只认登记过的本机名）"
        );
        assert_eq!(classify_via("192.168.1.9:9420", &quick, &named, &ts), "lan");
        assert_eq!(
            classify_via("", &quick, &named, &ts),
            "lan",
            "空 Host → lan"
        );
        assert_eq!(
            classify_via("localhost:9420", &quick, &named, &ts),
            "lan",
            "本机形态不再是 local"
        );
    }

    /// 请求头夹具
    fn headers(pairs: &[(&str, &str)]) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    /// 声明表夹具：走**生产同一构造点** `cloudflare_rate_channels`（防"测试声明的"
    /// 与"生产声明的"漂移）
    fn cf_channels(quick: &[&str], named: &[&str]) -> Vec<RateBucketChannel> {
        cloudflare_rate_channels(
            quick.iter().map(|s| s.to_string()).collect(),
            named.iter().map(|s| s.to_string()).collect(),
        )
    }

    /// 分桶键真值表（安全关键，逐档锁定）：**声明表驱动的信任边界**（§G5，评审 A-I2）。
    /// 实测依据（2026-10-06，经 quick tunnel 回显全部请求头）：伪造 `CF-Connecting-IP`
    /// → 边缘 403；`X-Forwarded-For` 真实值被追加在后；`True-Client-IP` 原样透传
    /// （可伪造）；`X-Real-IP` 被边缘丢弃。
    #[test]
    fn rate_bucket_key_truth_table() {
        use crate::remote::pin::GLOBAL_BUCKET;
        let ts_host = "jarvismac-mini.example-tailnet.ts.net";
        let channels = cf_channels(&["mam-test.trycloudflare.com"], &["mam.example.com"]);
        let lb: IpAddr = "127.0.0.1".parse().unwrap();
        let lan: IpAddr = "192.168.1.9".parse().unwrap();
        let cf = headers(&[("cf-connecting-ip", "1.2.3.4")]);

        // ① 非回环 → 一律来源地址（真实不可伪造），与头、与声明表无关
        assert_eq!(
            rate_bucket_key(lan, "mam-test.trycloudflare.com", &cf, &channels),
            "192.168.1.9"
        );
        assert_eq!(
            rate_bucket_key(lan, "evil.example.com", &cf, &[]),
            "192.168.1.9",
            "非回环连空声明表都不影响（来源地址本身可证）"
        );
        // ② 回环 + 已声明通道 + 该通道声明的权威头 → 头值（quick / named 两路等价）
        assert_eq!(
            rate_bucket_key(lb, "mam-test.trycloudflare.com", &cf, &channels),
            "1.2.3.4"
        );
        assert_eq!(
            rate_bucket_key(lb, "mam.example.com:443", &cf, &channels),
            "1.2.3.4",
            "带端口归一后命中 named 声明"
        );
        // ③ **未声明的通道**（tailscale Funnel：无 CF 边缘背书，同名头可被任意伪造）
        //    → 全局桶。变异锚点：把 tailscale 并进声明表、或改成"Host 不在名单也信头"，
        //    本档必红（2026-10-06 实测：Funnel 上同名头可任意伪造）。
        assert_eq!(
            rate_bucket_key(lb, ts_host, &cf, &channels),
            GLOBAL_BUCKET,
            "未声明通道一律回落全局桶（结构性事实，不靠注释纪律）"
        );
        // ④ 声明命中但权威头缺失 / 空白 → 全局桶（fail-closed）
        assert_eq!(
            rate_bucket_key(lb, "mam-test.trycloudflare.com", &headers(&[]), &channels),
            GLOBAL_BUCKET
        );
        assert_eq!(
            rate_bucket_key(
                lb,
                "mam-test.trycloudflare.com",
                &headers(&[("cf-connecting-ip", "   ")]),
                &channels
            ),
            GLOBAL_BUCKET,
            "空白头不采信"
        );
        // ⑤ 空声明表（快照不可信 / 未接线）→ 全局桶
        assert_eq!(
            rate_bucket_key(lb, "mam-test.trycloudflare.com", &cf, &[]),
            GLOBAL_BUCKET
        );
        // ⑥ Host 不命中任何声明（含空 Host）→ 全局桶（直连域名 + 同机反代即此类）
        assert_eq!(
            rate_bucket_key(lb, "evil.example.com", &cf, &channels),
            GLOBAL_BUCKET
        );
        assert_eq!(rate_bucket_key(lb, "", &cf, &channels), GLOBAL_BUCKET);
        // ⑦ **绝不**采信可伪造的 True-Client-IP、**绝不**裸信 X-Forwarded-For：
        //    声明表里没有它们 → 全局桶（勿"顺手支持一下"）
        let bad = headers(&[
            ("true-client-ip", "9.9.9.9"),
            ("x-forwarded-for", "5.6.7.8"),
        ]);
        assert_eq!(
            rate_bucket_key(lb, "mam-test.trycloudflare.com", &bad, &channels),
            GLOBAL_BUCKET
        );
        // ⑧ 声明驱动（不是硬编码 cf-connecting-ip）：通道声明谁，才信谁——
        //    证明"权威头按通道静态声明"这一结构真的生效
        let custom = vec![RateBucketChannel {
            name: "custom",
            hosts: vec!["mam.internal".to_string()],
            authoritative_header: "x-forwarded-for",
        }];
        assert_eq!(
            rate_bucket_key(lb, "mam.internal", &bad, &custom),
            "5.6.7.8",
            "通道声明的权威头才被取用"
        );
    }

    /// 生产声明表（`cloudflare_rate_channels`）恒为 quick / named 两路、权威头恒为实测
    /// 背书的 `CF-Connecting-IP`——**这就是"通道→权威头"的唯一声明处**（评审 A-I2）：
    /// 新通道接入必须显式改这里并附实测证据，未改即**结构性**回落全局桶。
    #[test]
    fn cloudflare_rate_channels_declare_only_measured_channels() {
        let chans = cf_channels(&["q.trycloudflare.com"], &["mam.example.com"]);
        assert_eq!(
            chans.len(),
            2,
            "只有 Cloudflare 系两路进表（tailscale 不在其列）"
        );
        assert_eq!(chans[0].name, "quick");
        assert_eq!(chans[0].hosts, vec!["q.trycloudflare.com".to_string()]);
        assert_eq!(chans[1].name, "named");
        for c in &chans {
            assert_eq!(c.authoritative_header, CF_AUTHORITATIVE_HEADER);
        }
        assert_eq!(
            CF_AUTHORITATIVE_HEADER, "cf-connecting-ip",
            "实测唯一可信：伪造即被 Cloudflare 边缘 403"
        );
        // 域名清单为空也仍是两路声明（空名单 ⇒ 不命中任何 Host，fail-closed）
        assert_eq!(cf_channels(&[], &[]).len(), 2);
    }

    /// 展示/指纹来源（评审 A-M3）：与分桶键**分开推导**——可证 → 权威头值
    /// （§G5 要求"来源记录能区分公网来源"）；不可证 → **真实 TCP 对端地址**。
    /// **永不含内部哨兵**（GLOBAL_BUCKET 含 NUL，写进 remote_devices.origin_ip 是内部
    /// 状态外泄；它也不是来源地址）。
    #[test]
    fn display_origin_never_leaks_the_global_sentinel() {
        use crate::remote::pin::GLOBAL_BUCKET;
        let channels = cf_channels(&["mam-test.trycloudflare.com"], &[]);
        let lb: IpAddr = "127.0.0.1".parse().unwrap();
        let lan: IpAddr = "192.168.1.9".parse().unwrap();
        let cf = headers(&[("cf-connecting-ip", "1.2.3.4")]);
        // 可证（回环 + 已声明通道 + 权威头）→ 权威头值：来源记录恢复区分度（§G5）
        assert_eq!(
            display_origin(lb, "mam-test.trycloudflare.com", &cf, &channels),
            "1.2.3.4"
        );
        // 非回环 → 对端地址（真实不可伪造，不看头）
        assert_eq!(
            display_origin(lan, "mam-test.trycloudflare.com", &cf, &channels),
            "192.168.1.9"
        );
        // 不可证三形态 → 真实对端地址，且与分桶键（哨兵）**不同值**
        for (host, hs, why) in [
            ("mam-test.trycloudflare.com", headers(&[]), "权威头缺失"),
            ("evil.example.com", cf.clone(), "Host 不命中声明"),
            (
                "jarvismac-mini.example-tailnet.ts.net",
                cf.clone(),
                "未声明通道",
            ),
        ] {
            let bucket = rate_bucket_key(lb, host, &hs, &channels);
            let shown = display_origin(lb, host, &hs, &channels);
            assert_eq!(bucket, GLOBAL_BUCKET, "{why}：分桶键回落全局桶");
            assert_eq!(shown, "127.0.0.1", "{why}：展示来源 = 真实对端地址");
            assert_ne!(shown, GLOBAL_BUCKET, "{why}：展示来源不得是内部哨兵");
            assert!(!shown.contains('\u{0}'), "{why}：展示来源绝不含 NUL");
        }
    }
}
