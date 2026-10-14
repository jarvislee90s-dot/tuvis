//! 强制可达性校验 + 自愈（§C3）：外部 DoH 解析（**多源、纯 IP 字面量**）→ **钉住 IP**
//! 直连（绕开本机 DNS 与本机代理）→ **兔维斯 特征判据**（HTTP 200 + 特征头，不是"任意响应
//! 即算通"）→ 失败自愈。校验态（[`Reachability`]）经载荷三门门控地址展示，并经轮询守护
//! 按窗口重验。
//!
//! **本机自检不可信**（§C3 实测事故：本地六项信号全绿而公网打不开）——原因与机制见
//! 文件内的 §C3 段头注释；探针的诚实边界见 [`mam_response_ok`]。
//!
//! **A2/A3 实测纪律**：① 可达性检查**只走 DoH(443) 与钉 IP 直连**，不得引入任何走系统
//! DNS 栈的解析（实测本机 53 端口被 Tailscale WFP 规则接管，连 `Resolve-DnsName -Server
//! 223.5.5.5` 都被劫持、恒返回 MagicDNS 值）；② DoH **真多源**按序尝试；③ 源全挂如实报
//! 「未验证」（fail-closed），**不谎报**成「记录尚未发布」。

use super::status::{
    disable_funnel, enable_funnel, foreign_serve_error, funnel_status_unreadable, parse_status,
    run_cli, serve_ownership, ts_snapshot, ServeOwnership, DESIRED,
};

// ============================================================
// 强制可达性校验 + 自愈（§C3）
// ------------------------------------------------------------
// **机制主次（设计说明书 §C3 要求 5）**：以外部解析 / 探测为主。本机不能自测——
// ① 本机是尾网成员，该名字被 MagicDNS 接管，压根不走公网路径；② 本机可能装有
// 网络加速/代理工具（实测机器上就有——它用自签 CA 做本地 MITM；实测正是被这点
// 骗过：本地六项信号全绿而公网根本打不开）。故校验 = 公共 DoH 解析 + 解析结果
// **钉住 IP** 直连，两者都绕开本机 DNS 与本机代理。**响应还必须是 兔维斯 自己的服务答的**
//（HTTP 200 + 兔维斯 特征头，B-I6）——钉 IP + 禁系统代理破不了 TUN / 透明重定向式 MITM，
// 故在「有没有响应」之上再收紧一层（残余风险的诚实边界见 [`mam_response_ok`]）。
// 外部解析服务不可用时按 **Failed（验证不了）** 处理（fail-closed）：宁可报「无法验证」，
// 也不谎报「已生效」；成因三态（全源不可用 / 权威否定 / 有记录）由 [`probe_addresses`] 的
// 返回值区分，**不把自家解析故障说成上游的发布延迟**。
// **M-3 注释纠偏（2026-10-07 评审）**：本段此前写「按**未验证**处理」，而代码返回的是
// [`Reachability::Failed`]（带可排查的 reason）——状态名与注释必须一致：Failed 才是这里的
// 事实（UI 同样按 Failed 透出具体原因，而不是笼统一句「尚未验证」；fail-closed 的方向不变）。
// ============================================================

/// 可达性态（**逐名枚举，不写数字**：`Unverified` / `Verifying` / `Verified` /
/// `RecordPending` / `Recovering` / `Failed`，共六个变体——M6（2026-10-07 评审）：
/// 本行与 `wizard_status` / `channels_payload` 两处契约注释此前各写一个数字（"四态"/
/// "五态"）且都与枚举不符，故一律改为枚举名字）。
/// **地址只有在 `Verified` 时才算可用**（§C3 要求 2：
/// 校验不通过就不许把地址当可用呈现）。
///
/// W-B 分档（2026-10-07 真机实测三档时长）把「还没生效」拆成**两种正常现象**：
/// [`Reachability::RecordPending`]（记录尚未发布：首开 ≈5–6 分钟 / reset 后重开 ≈30–49 秒）
/// 与 [`Reachability::Recovering`]（开机恢复：后端重连 1–2 分钟，**不用等 DNS**）。
/// 二者都**不是故障**——旧实现把它们都塞进 `Failed`，于是 UI 只能给一句笼统的
/// 「尚未生效」或固定 5 分钟，既不准确也指错成因。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum Reachability {
    Unverified,
    Verifying,
    Verified,
    /// 公网 DNS 记录**尚未发布**（正常发布延迟，不是故障）。
    /// `republish` 分辨两档：`true` = 本进程内 兔维斯 执行过 `funnel reset`（重开档，
    /// 实测 30–49 秒）；`false` = 其余（默认按首次开通档给 5–6 分钟，并同时给出
    /// "此前开通过 1 分钟内"的下界——见 [`record_pending_hint`]）。
    RecordPending {
        republish: bool,
    },
    /// **开机恢复窗口**（W-A，2026-10-07 真机重启实测）：后端重连中 / 公网路径尚未恢复。
    /// **不是故障、也不是「域名生效中」**——成因完全不同（重启后 DNS 记录不撤销，
    /// 压根不用等发布；实测 T+89s 记录仍在而 TLS 打不通，T+2.5min 才 200）。
    /// 地址照旧不上墙（三门不放行），但用户该看到的语义是"正在自己恢复，无需操作"。
    Recovering,
    Failed {
        reason: String,
    },
}

/// 校验态全局（载荷地址三门的第三道门 + 向导 verify 步判据的数据源）。
/// 写入点：三处经 [`set_reachability`]（verify 步 / 轮询守护的 60s 重验 / 生命周期作废），
/// 后端相位跃迁（[`note_backend_initializing`] / [`note_backend_ready`]）直接写本 static
static REACHABILITY: once_cell::sync::Lazy<std::sync::Mutex<Reachability>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(Reachability::Unverified));

/// 当前校验态（remote_status / remote_ts_probe 消费）
pub(crate) fn reachability() -> Reachability {
    REACHABILITY.lock().unwrap().clone()
}

/// 写入校验态（生产写入点全在本模块内：verify 步、轮询重验、生命周期作废；测试用它注入夹具）。
/// **副作用（W-B 翻页规则）**：写入 `Verified` 时清掉「重开档」标记——记录又发布了，
/// 下一次"没记录"不该继续算重开档（否则一次 reset 会把整个进程的档位永久钉死）。
pub(crate) fn set_reachability(r: Reachability) {
    if matches!(r, Reachability::Verified) {
        clear_funnel_reset_seen();
    }
    *REACHABILITY.lock().unwrap() = r;
}

// ============================================================
// W-B 档位判据（两档都必须是**可证事实**，不编判据）
// ============================================================

/// 本进程内是否执行过 `funnel reset`（**"重新开通"档的唯一判据**）。
/// 生产唯一写点 = [`super::status::disable_funnel`]，而它**是 `funnel reset` 的唯一
/// 调用点**（撤销 / 自愈 / 重开三条路都经它）——所以这是个可证事实，不是猜测。
/// **I1（2026-10-07 评审）**：这句话此前**不成立**——自愈曾自己直接发一条 reset 绕过
/// [`super::status::disable_funnel`]（不记账），于是最该走"重开档"的场景恒落首开档
/// （把实测 30–49 秒说成 5–6 分钟）。现已收口，并由两条锁把这个可证事实钉住：
/// 用例 `heal_reset_is_accounted_so_record_pending_uses_republish_tier`（自愈路径记账）
/// 与源码锁 `funnel_reset_has_exactly_one_call_site`（调用点唯一）。
static FUNNEL_RESET_SEEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 记「本进程执行过 funnel reset」（[`super::status::disable_funnel`] 调用）
pub(super) fn note_funnel_reset() {
    FUNNEL_RESET_SEEN.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// 翻页：验过（记录又发布）后清掉重开档（[`set_reachability`] 的副作用）
fn clear_funnel_reset_seen() {
    FUNNEL_RESET_SEEN.store(false, std::sync::atomic::Ordering::SeqCst);
}

/// 原始读数（接线测试用——分类判据在测试构建下读注入缝，见下）
#[cfg(test)]
pub(super) fn reset_seen_raw() -> bool {
    FUNNEL_RESET_SEEN.load(std::sync::atomic::Ordering::SeqCst)
}

/// 分类判据读数（生产 = 原始读数；测试构建 = 注入缝，默认 false）。
///
/// **为什么测试构建要留缝**：`FUNNEL_RESET_SEEN` 是进程级单调量，并行的既有用例
/// （大量"记录尚未发布"夹具）会被别的用例改写档位 → 判据漂移。缝的纪律与
/// `RUN_CLI_OVERRIDE` / `VERIFY_PROBE_OVERRIDE` 同源：**显式注入、用后还原**；
/// 生产接线另有接线测试（`reset_seen_raw` 的置位/翻页）。
pub(super) fn pending_republish() -> bool {
    #[cfg(test)]
    if let Some(v) = *RESET_SEEN_OVERRIDE.lock().unwrap() {
        return v;
    }
    FUNNEL_RESET_SEEN.load(std::sync::atomic::Ordering::SeqCst)
}

#[cfg(test)]
static RESET_SEEN_OVERRIDE: once_cell::sync::Lazy<std::sync::Mutex<Option<bool>>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(None));

/// 注入「重开档」判据（None = 还原；仅测试触碰，须持 TEST_LOCK）
#[cfg(test)]
pub(super) fn set_reset_seen_override(v: Option<bool>) {
    *RESET_SEEN_OVERRIDE.lock().unwrap() = v;
}

/// 本进程是否**见过后端初始化窗口**（开机恢复的**证据**）。
/// 没有这条证据就不许说"正在恢复"——那会把常年连不通的故障说成"再等等"。
static BACKEND_INIT_SEEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// **M1（2026-10-07 评审 Minor）**：「后端持续未就绪」的**有界降级**上界。
/// 与 [`BOOT_RECOVERY_GRACE`] 同量级（3 分钟）：实测开机 T+73s 后端就绪、T+2.5min
/// 公网 200 ⇒ 3 分钟仍不就绪已不是"正常开机恢复"，必须**如实点名成因与已等时长**
/// （无界地说「通常 1–2 分钟、无需任何操作」就是掩盖真故障）。
pub(super) const BACKEND_INIT_GRACE: std::time::Duration = std::time::Duration::from_secs(180);

/// 本进程内**首个**「后端未就绪」观测时刻（本次恢复期的起点；后端就绪后清零）。
static FIRST_BACKEND_INIT: once_cell::sync::Lazy<std::sync::Mutex<Option<std::time::Instant>>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(None));

/// 是否已因超上界而降级（[`note_backend_ready`] 据此把降级态交回正常校验流程，
/// 而不是把 `Failed` 一直钉住：`Verified/Verifying` 仍不被触碰）。
static BACKEND_INIT_DEGRADED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 上界判定（纯函数，可测）：**要有起点**（没见过未就绪就不算超窗，不编判据）
/// 且已等待 ≥ [`BACKEND_INIT_GRACE`]。
pub(super) fn backend_init_overdue(
    init_since: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    match init_since {
        Some(t0) => now.saturating_duration_since(t0) >= BACKEND_INIT_GRACE,
        None => false,
    }
}

/// 降级文案（纯函数，可测）：**点名后端状态与已等时长 + 给出升级路径**，
/// 并如实说明 兔维斯 在这个窗口里**没有**改配置（不判定、不重开）——用户据此判断
/// 是继续等还是去查 Tailscale 服务。
pub(super) fn backend_init_overdue_reason(state: &str, elapsed: std::time::Duration) -> String {
    let mins = (elapsed.as_secs() / 60).max(1);
    format!(
        "Tailscale 后端持续未就绪（{state}，已 {mins} 分钟）——已超出开机恢复的宽限窗；\
         可在配置向导中重试，或检查 Tailscale 服务是否正常\
         （宽限窗内 兔维斯 未判定 Funnel 配置、未重开，配置与地址不会被改动）"
    )
}

/// 测试缝：注入「后端未就绪」起点（`None` = 归零/还原；仅测试触碰，须持 TEST_LOCK）。
/// 生产起点由 [`note_backend_initializing`] 首次观测时自己记；[`reset_wb_globals`] 用它归零。
#[cfg(test)]
pub(super) fn set_backend_init_since(t: Option<std::time::Instant>) {
    *FIRST_BACKEND_INIT.lock().unwrap() = t;
    BACKEND_INIT_DEGRADED.store(false, std::sync::atomic::Ordering::SeqCst);
}

/// 本进程内后端**首个 Running 时刻**（宽限窗起点；未见 Running 则 None）
static FIRST_BACKEND_READY: once_cell::sync::Lazy<std::sync::Mutex<Option<std::time::Instant>>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(None));

/// 开机恢复宽限窗上界。实测：T+73s 后端 Running、T+89s 记录在而 TLS 打不通、
/// **T+2.5min 两个入口 200** ⇒ 3 分钟足够覆盖实测并留余量；超窗即交回如实失败
/// （恢复中绝不成为掩盖真故障的挡箭牌）。
pub(super) const BOOT_RECOVERY_GRACE: std::time::Duration = std::time::Duration::from_secs(180);

/// 开机恢复窗口判据（纯函数）：**要证据**（见过初始化窗口）**且有界**（首个 Running 后
/// 宽限窗内）。`first_ready = None`（还没见过 Running，窗口起点未知）不算。
pub(super) fn boot_recovery_window(
    seen_init: bool,
    first_ready: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    let Some(t0) = first_ready else {
        return false;
    };
    seen_init && now.duration_since(t0) < BOOT_RECOVERY_GRACE
}

/// 当前是否处于开机恢复窗口（生产 = 读全局；测试构建 = 注入缝，默认 false）。
/// 缝的理由同 [`pending_republish`]：全局量会被并行用例改写，判据必须可注入。
pub(super) fn boot_recovery_active(now: std::time::Instant) -> bool {
    #[cfg(test)]
    if let Some(v) = *BOOT_RECOVERY_OVERRIDE.lock().unwrap() {
        return v;
    }
    boot_recovery_window(
        BACKEND_INIT_SEEN.load(std::sync::atomic::Ordering::SeqCst),
        *FIRST_BACKEND_READY.lock().unwrap(),
        now,
    )
}

#[cfg(test)]
static BOOT_RECOVERY_OVERRIDE: once_cell::sync::Lazy<std::sync::Mutex<Option<bool>>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(None));

/// 注入开机恢复窗口判据（None = 还原；仅测试触碰，须持 TEST_LOCK）
#[cfg(test)]
pub(super) fn set_boot_recovery_override(v: Option<bool>) {
    *BOOT_RECOVERY_OVERRIDE.lock().unwrap() = v;
}

/// 原始读数：是否见过后端初始化窗口（接线测试用）
#[cfg(test)]
pub(super) fn backend_init_seen_raw() -> bool {
    BACKEND_INIT_SEEN.load(std::sync::atomic::Ordering::SeqCst)
}

/// 原始读数：首个 Running 时刻（接线测试用）
#[cfg(test)]
pub(super) fn first_backend_ready_raw() -> Option<std::time::Instant> {
    *FIRST_BACKEND_READY.lock().unwrap()
}

/// 测试收尾：把 W-B 的三个进程级记账量归零（teardown_globals 调用）
#[cfg(test)]
pub(super) fn reset_wb_globals() {
    FUNNEL_RESET_SEEN.store(false, std::sync::atomic::Ordering::SeqCst);
    BACKEND_INIT_SEEN.store(false, std::sync::atomic::Ordering::SeqCst);
    *FIRST_BACKEND_READY.lock().unwrap() = None;
    // M1：「后端持续未就绪」起点与降级位
    set_backend_init_since(None);
    set_reset_seen_override(None);
    set_boot_recovery_override(None);
}

/// 后端进入初始化窗口（开机恢复）→ 可达性置 [`Reachability::Recovering`] **并记下证据**
/// （[`boot_recovery_window`] 的"见过初始化窗口"那一半）。
/// **唯一调用方 = `status::store_tick`**（轮询/起停的现算点，与快照同源同轮）。
/// 在验中（[`Reachability::Verifying`]）不抢写：那是用户刚点下的那次校验，结论由它自己写。
/// 旧的 `Verified` 会被覆盖——后端都不在 Running 了还宣称地址可用就违反 §C3 三门。
///
/// **M1（2026-10-07 评审 Minor）有界降级**：本函数**不再无界承诺**。返回值 = `Some(原因)`
/// 表示本进程累计未就绪已超 [`BACKEND_INIT_GRACE`]：此时可达性置
/// `Failed{该原因}`（**如实点名**「后端持续未就绪（NoState，已 X 分钟）」+ 升级路径），
/// 调用方（[`super::status::store_tick`]）把它一并写进快照 error；后端就绪后由
/// [`note_backend_ready`] 清零降级位、交回正常校验流程。
pub(super) fn note_backend_initializing(state: &str, now: std::time::Instant) -> Option<String> {
    BACKEND_INIT_SEEN.store(true, std::sync::atomic::Ordering::SeqCst);
    // 起点只记第一次（本次恢复期；后端就绪即清零）——这样"已 X 分钟"是真实的累计等待
    let since = {
        let mut t = FIRST_BACKEND_INIT.lock().unwrap();
        *t.get_or_insert(now)
    };
    if !backend_init_overdue(Some(since), now) {
        let mut g = REACHABILITY.lock().unwrap();
        if matches!(*g, Reachability::Verifying) {
            return None;
        }
        *g = Reachability::Recovering;
        return None;
    }
    // 超上界：降级（不再说"正在恢复、无需任何操作"，如实点名成因与已等时长）
    BACKEND_INIT_DEGRADED.store(true, std::sync::atomic::Ordering::SeqCst);
    let reason = backend_init_overdue_reason(state, now.saturating_duration_since(since));
    let mut g = REACHABILITY.lock().unwrap();
    if !matches!(*g, Reachability::Verifying) {
        *g = Reachability::Failed {
            reason: reason.clone(),
        };
    }
    Some(reason)
}

/// 后端已就绪 → ① 记下**首个**就绪时刻（宽限窗起点，只记第一次）
/// ② 清掉「恢复中」（交回正常校验流程：轮询重验窗口会在本轮内发起校验）。
/// **只清 Recovering，不动 Verified/Verifying**——每轮 Running 都会调本函数，
/// 若顺手清 Verified 就会让地址每个轮询窗闪一次（假的"掉线"）。
///
/// **M1**：同时清零「后端未就绪」起点与降级位；若当前是**因降级而写的 Failed**
/// （`BACKEND_INIT_DEGRADED`）也交回 `Unverified`——否则降级态会一直钉着，直到下一次
/// 重验才恢复（`Verified/Verifying` 仍不被触碰：那条不变式是承重的，见 I4b 用例）。
pub(super) fn note_backend_ready(now: std::time::Instant) {
    {
        let mut t = FIRST_BACKEND_READY.lock().unwrap();
        if t.is_none() {
            *t = Some(now);
        }
    }
    *FIRST_BACKEND_INIT.lock().unwrap() = None;
    let degraded = BACKEND_INIT_DEGRADED.swap(false, std::sync::atomic::Ordering::SeqCst);
    let mut g = REACHABILITY.lock().unwrap();
    let degraded_failure = degraded && matches!(*g, Reachability::Failed { .. });
    if matches!(*g, Reachability::Recovering) || degraded_failure {
        *g = Reachability::Unverified;
    }
}

/// **DoH 源表（真多源，按序尝试）**——A3 实测修正：旧实现注释宣称「Cloudflare（1.1.1.1）
/// 作为备选」，**代码里根本没有备选**（硬编码 223.5.5.5 单源）——注释与代码不符即缺陷。
///
/// 纪律三条：
/// 1. **按序尝试**：前一个源失败 / 超时 / 非 DoH 应答 / 非权威应答（SERVFAIL 等）就换下一个；
/// 2. **主机必须是 IP 字面量**：`https://<域名>/...` 会让连接器走**系统解析器**，而实测
///    本机 53 端口被 Tailscale 的 WFP 规则接管（`Resolve-DnsName -Server 223.5.5.5` 也
///    被劫持，恒返回 MagicDNS 值）——用域名形态等于把「本机永远显示通」请回来；
/// 3. **绝不使用系统解析器**（防回归测试见 `reachability_targets_ip_literals_only_...`）；
/// 4. **不得有纸面源**（I-2，评审 2026-10-07 复测）：表里每个源都必须是本网络**实测可达**
///    的 IP 字面量，并在下面留一行带日期的实测记录——源码锁测试
///    `doh_source_table_carries_measured_evidence_and_no_paper_source` 按这条把关；
///    逐源连通性冒烟（打一次，非单测）：`scripts/check-doh-sources.sh`。
///
/// **2026-10-07 逐源实测记录**（本网络；`curl -s https://<ip>/resolve?name=example.com&type=A`
/// 的 HTTP 码，`nc -z <ip> 443` 交叉核对）：
/// - `223.5.5.5/resolve`：**实测 200**（阿里公共 DNS，首选；JSON 形状 `Status` + `Answer`）；
/// - `223.6.6.6/resolve`：**实测 200**（阿里公共 DNS 的第二 IP）——**与首选同运营商**，
///   只防「单个 IP 被封/单点故障」，**防不了运营商级故障**（两家一起挂就退回未验证）；
/// - `1.12.12.12/resolve`：**实测 200**（腾讯 DNSPod）——**独立运营商**，补上一条独立链路；
/// - 复测**不可达**（故**不入表**，纸面源即缺陷）：1.1.1.1（`nc -z 1.1.1.1 443` 不通）、
///   8.8.8.8、119.29.29.29、180.76.76.76、9.9.9.9、94.140.14.14、101.101.101.101；
///   另测得 `120.53.53.53/resolve` **200**（DNSPod 第二 IP）——与 1.12.12.12 同运营商，
///   不入表以收敛「源数 × 类型数 × 超时」的最坏用时（冒烟脚本仍可单独打它）。
///
/// 覆盖面的诚实边界：三源里两家运营商（阿里 ×2 + 腾讯 ×1）。两家同时不可用时按
/// 「无法验证」fail-closed 报出（**不谎报成上游的发布延迟**，见 [`probe_addresses`]）。
pub(super) const DOH_SOURCES: &[&str] = &[
    "https://223.5.5.5/resolve",  // 阿里公共 DNS：2026-10-07 实测 200（首选）
    "https://223.6.6.6/resolve", // 阿里公共 DNS 第二 IP：2026-10-07 实测 200（同运营商，只防单 IP 封锁）
    "https://1.12.12.12/resolve", // 腾讯 DNSPod：2026-10-07 实测 200（独立运营商）
];

/// 源表里的第 n 个源的查询 URL（`https://<ip>/<path>?name=&type=`，两种接口同形）
pub(super) fn doh_url_at(source: &str, name: &str, rtype: &str) -> String {
    format!("{source}?name={name}&type={rtype}")
}

/// 解析到多条地址时最多尝试的条数（B-M7）：解析结果可能是 A + AAAA / 轮换记录，
/// 首条陈旧或不可达时不得直接误报 Failed；全试一遍又太贵（每条一次 HTTPS 探测，
/// 超时 10s），取前 3 条足够覆盖「首条陈旧」这一实际形态
const PROBE_MAX_ADDRS: usize = 3;

/// 从 DoH JSON 里取 A/AAAA 记录（type 1 / 28）。**CNAME(5) 不算已发布**——
/// 记录没发布时接口返回空 Answer（实测形态）。
pub(super) fn parse_doh_answer(json: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    v.get("Answer")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|r| matches!(r.get("type").and_then(|t| t.as_u64()), Some(1) | Some(28)))
                .filter_map(|r| r.get("data").and_then(|d| d.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// 单次 DoH 查询的**定性**（A3：必须区分「没有记录」与「这个源不能用」——
/// 前者是正常发布延迟，后者是解析服务故障，二者混为一谈就是谎报成因）
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DohResponse {
    /// 该源给出了 A/AAAA 记录
    Records(Vec<String>),
    /// 该源给出了**权威否定**（DNS 层答复：NOERROR 无记录 / NXDOMAIN）——真「没有记录」
    NoRecord,
    /// 该源不可用（无应答 / 非 JSON / 非权威应答如 SERVFAIL、REFUSED）→ 换下一个源
    Unusable,
}

/// 解析一次 DoH 应答并定性（纯函数，可测）。判据：
/// - 非 JSON / JSON 里没有 `Status` / `Status` 非 0 与非 3 → `Unusable`（换源）；
/// - `Status == 0`（NOERROR）或 `3`（NXDOMAIN）且无 A/AAAA → `NoRecord`（权威否定）；
/// - 有 A(type 1)/AAAA(type 28) 记录 → `Records`（CNAME 不算已发布，见 [`parse_doh_answer`]）。
pub(super) fn parse_doh_response(json: &str) -> DohResponse {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return DohResponse::Unusable;
    };
    let records = parse_doh_answer(json);
    if !records.is_empty() {
        return DohResponse::Records(records);
    }
    match v.get("Status").and_then(|s| s.as_u64()) {
        Some(0) | Some(3) => DohResponse::NoRecord,
        // 含缺 Status（例如门户页/拦截页里的 JSON）——不可信，换下一个源
        _ => DohResponse::Unusable,
    }
}

/// 探针缝（测试注入假实现，零网络）：给定主机名返回其**公网地址**或**验证不了**。
/// `Ok(vec![])` = 至少一个源给出了权威否定（真·没有记录）；`Err(原因)` = **所有源都
/// 不可用**——语义上必须分开（A3）：前者是正常发布延迟，后者是解析服务故障，
/// 后者按「未验证」处理（fail-closed），**不得**谎报成「记录尚未发布」。
pub(super) type ProbeFn = dyn Fn(&str) -> Result<Vec<String>, String> + Send + Sync;

/// 用户可见的「记录尚未发布」口径——**首次开通档**（**必须带预期时长**）。
///
/// **为什么文案里要有分钟数**（2026-10-07 Windows 实测 + 用户裁决）：首次开通 Funnel
/// 后，本机直连 `https://<host>/` **立刻** HTTP 200（走 100.x 本地路径），而公网 DNS
/// 记录**约 5–6 分钟**才发布（实测 DoH 连续 14 次 NODATA；两个入口 A 记录随后出现；
/// AAAA 始终不发布）。**这 5–6 分钟里用户唯一的感受就是"连不上"**——只写「尚未生效」
/// 会让他以为是故障；写清预期时长才是如实告知（§C3 要求 2 与 §5「诚实」）。
///
/// **两界都给（W-B）**：兔维斯 关着时被人手动 `funnel reset` 的情形**无法证实**是不是首次，
/// 故本档同时给出"此前开通过通常 1 分钟内"的下界——给区间，不编判据。
pub(crate) const RECORD_PENDING_HINT: &str =
    "公网 DNS 记录尚未发布——首次开通实测约需 5–6 分钟（此前开通过则通常 1 分钟内）；\
     地址要等记录发布后才从外网可达";

/// 用户可见的「记录尚未发布」口径——**重新开通档**（W-B，2026-10-07 真机实测）：
/// `reset` 后重开的记录重发布**≈30–49 秒**。判据可证：本进程内 兔维斯 自己执行过
/// `funnel reset`（[`pending_republish`]）。旧实现套用首开那句 5 分钟 = 把 30 秒说成
/// 5 分钟（用户白等，且成因指向错误）。
pub(crate) const RECORD_REPUBLISH_HINT: &str =
    "Funnel 刚重新开通，公网 DNS 记录正在重新发布——通常 30 秒～1 分钟；地址不会变";

/// 档位 → 文案（纯函数，可测）：**两档各自点明成因与实测时长**，互不冒充
pub(crate) fn record_pending_hint(republish: bool) -> &'static str {
    if republish {
        RECORD_REPUBLISH_HINT
    } else {
        RECORD_PENDING_HINT
    }
}

/// 用户可见的**开机恢复**口径（W-A/W-B，2026-10-07 真机重启逐秒实测）：
/// 重启后既不是故障、也不是「域名生效中」——**成因是后端重连**。实测时间线：
/// T+58s 服务已 Running 但 `BackendState=NoState`、`funnel status --json` 短暂为 `{}`；
/// T+73s 后端 Running、Funnel 配置逐字段原样恢复；T+89s 记录仍在而 TLS 打不通；
/// **T+2.5min 两个入口 IP 均 HTTP 200**。故口径必须点明"后端重连 + 1–2 分钟"，
/// 并安抚"配置与地址都不变"（这条通道的核心承诺），且明确**无需操作**。
pub(crate) const RECOVERING_HINT: &str =
    "Tailscale 后端正在重连（开机后通常 1–2 分钟）——配置与地址都不会变，无需任何操作";

/// 「地址已解析但连不通」类失败的 reason 前缀（[`verify_reachability`] 尾部格式串）。
/// **2026-10-13 用户裁决**：这类失败**不再触发 funnel reset 自愈**——它说明 DNS 记录在、
/// 配置在，成因是到公网中继入口的链路瞬时抖动（真机实测：两个入口同设施、常一起失败，
/// 60s 轮询复验自愈），reset 只会把完好配置清掉再重发布 30–49 秒。前端
/// tsTransientHint 引导文案锚定同一前缀（reason 恒中文），两处需同步维护。
pub(super) const ADDR_UNREACHABLE_PREFIX: &str = "REACH_ADDR_UNREACHABLE: 地址已解析但连不通";

/// 每轮校验结论落一行日志——2026-10-13 真机定位的盲区：remote 模块原先不写任何日志，
/// 可达性失败没有时间线留痕，故障窗口只能事后观测反推。INFO 记常态、Failed 用 WARN
/// 突出（测试构建无 logger，log 宏为 no-op，不产生测试噪音）。
fn log_reach(cycle: &str, host: &str, r: &Reachability) {
    match r {
        Reachability::Failed { reason } => log::warn!("{cycle}（{host}）: failed —— {reason}"),
        other => log::info!("{cycle}（{host}）: {other:?}"),
    }
}

/// 尾网之外的可达性校验：**先看公网有没有记录，再试着接通**。
/// 两者缺一即失败——只有记录但连不通，说明上游还没ready。
/// 解析层三态各有各的成因（A3 + W-B 分档）：全源不可用 → 「无法验证」（不是"记录未发布"）；
/// 权威否定 → [`Reachability::RecordPending`]（正常发布延迟，两档时长）；有记录 →
/// 逐条钉 IP 接通，全连不通时**开机恢复窗口内**报 [`Reachability::Recovering`]、
/// 窗口外才是 Failed（恢复中不得成为掩盖真故障的挡箭牌）。
pub(super) fn verify_reachability(host: &str, probe: &ProbeFn) -> Reachability {
    let addrs = match probe(host) {
        Ok(a) => a,
        // 外部服务不可用 → **未验证**（fail-closed）：宁可报「尚未验证」，也不谎报
        // 「已生效」，更不把自己的故障说成上游的发布延迟
        Err(e) => {
            return Reachability::Failed {
                reason: format!("无法验证地址（公共解析服务不可用）: {e}"),
            }
        }
    };
    if addrs.is_empty() {
        // 权威否定 = **记录尚未发布**（正常发布延迟，不是故障）：两档时长由判据给出
        // （重开档 = 本进程执行过 funnel reset；否则首开档并附"此前开通过"下界）
        return Reachability::RecordPending {
            republish: pending_republish(),
        };
    }
    // 用解析到的地址**钉住 IP** 去请求（绕过本机 DNS），**逐条尝试**（B-M7）：
    // 解析结果可能有多条，只看首条会让陈旧记录/不可达的一条直接误报 Failed
    let mut errs: Vec<String> = Vec::new();
    for ip in addrs.iter().take(PROBE_MAX_ADDRS) {
        match probe_http(host, ip) {
            Ok(()) => return Reachability::Verified,
            Err(e) => errs.push(format!("{ip}: {e}")),
        }
    }
    // 记录解析得到但逐条都连不通——**分两种情况**（W-A 实测 T+89s 正是前者）：
    // ① 开机恢复窗口内（本进程见过后端初始化窗口 + 首个 Running 后 3 分钟内）：
    //    公网路径还没起来（实测 T+2.5min 才 200），语义是「恢复中」而非故障；
    // ② 窗口外：如实失败，逐条给出地址与错误（拦截页 / 证书问题 / 真连不通）。
    if boot_recovery_active(std::time::Instant::now()) {
        return Reachability::Recovering;
    }
    Reachability::Failed {
        reason: format!(
            "{ADDR_UNREACHABLE_PREFIX}（试了 {} 条）: {}",
            errs.len(),
            errs.join("; ")
        ),
    }
}

/// 响应是否**兔维斯 自己的服务答的**（纯函数，可测——B-I6 判据）：HTTP 200 +
/// [`crate::remote::server::MAM_REACH_HEADER`] 特征头命中。
///
/// **为什么必须加这道判据**：旧口径「拿到**任意** HTTP 响应即算通」挡不住 TUN / 透明
/// 重定向式 MITM——用户把自签 CA 装进系统信任库后，请求被解成拦截页而 TLS 依然"成功"，
/// 错误页被读成「已验证」。§C3 的事故正是「本地信号全绿」型误判，故本模块的可达性判定
/// 不能只看"有没有响应"。
///
/// **诚实边界（不要把它说成鉴权）**：这不是密码学证明——知道特征值的 MITM 仍可伪造。
/// 它的定位是**廉价判据**：把「任意响应」收紧为「带 兔维斯 特征」，挡掉最常见的透明错误页
/// （代理拦截页 / 门户页 / 自签 CA 的错误页都不带这个头）。
pub(super) fn mam_response_ok(status: u16, marker: Option<&str>) -> Result<(), String> {
    if status != 200 {
        return Err(format!(
            "HTTP {status}（兔维斯 看板入口应为 200；疑似链路上的拦截/错误页）"
        ));
    }
    match marker {
        Some(v) if v == crate::remote::server::MAM_REACH_VALUE => Ok(()),
        Some(other) => Err(format!(
            "响应特征头为 {other:?}，不是 兔维斯 服务答的（疑似链路上的拦截页）"
        )),
        None => {
            Err("响应缺少 兔维斯 特征标记，不是 兔维斯 服务答的（疑似链路上的拦截/错误页）".into())
        }
    }
}

/// 错误串 + source 链首层（纯函数，可测）。reqwest 的 `error sending request for url (…)`
/// 顶层 Display 压着 hyper/rustls 的真实成因（tcp 连接超时 / TLS 握手失败 / 连接被重置），
/// 只留顶层就无法区分「超时」与「被掐」——2026-10-13 实机定位时正是这个盲区。
pub(super) fn error_with_source(e: &dyn std::error::Error) -> String {
    match e.source() {
        Some(src) => format!("{e}（底层: {src}）"),
        None => e.to_string(),
    }
}

/// 用解析到的公网地址**钉住 IP** 去请求 兔维斯 看板入口（绕过本机 DNS 与本地代理），
/// 并要求响应带 **兔维斯 特征**（[`mam_response_ok`]）——「链路通了」不等于「兔维斯 答了」。
/// **不校验响应体内容**（那是前端产物的事，且会随构建漂移）；判据只用服务端写死的特征头
fn probe_http(host: &str, ip: &str) -> Result<(), String> {
    #[cfg(test)]
    if let Some(f) = HTTP_PROBE_OVERRIDE.lock().unwrap().as_ref() {
        return f(host, ip); // 测试缝：零网络红线（与 RUN_CLI_OVERRIDE 同纪律）
    }
    let addr: std::net::IpAddr = ip.parse().map_err(|_| format!("地址非法: {ip}"))?;
    let client = reqwest::blocking::Client::builder()
        // **禁用系统代理**（no_proxy）：本机代理可能做本地 MITM——§C3 实测教训
        //（本地全绿而公网打不开）。钉 IP 的意义就在于同时绕开本机 DNS 与本机代理
        .no_proxy()
        .resolve(host, std::net::SocketAddr::new(addr, 443))
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| error_with_source(&e))?;
    // 请求**看板地址**（`/m`）而不是根路径：特征头由 entry_response 写在那里，
    // 而根路径在 兔维斯 侧本就没有入口（非 /m 前缀 → 404）
    let resp = client
        .get(format!("https://{host}/m"))
        .send()
        .map_err(|e| error_with_source(&e))?;
    let status = resp.status().as_u16();
    let marker = resp
        .headers()
        .get(crate::remote::server::MAM_REACH_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    mam_response_ok(status, marker.as_deref())
}

/// 生产探针（[`ProbeFn`] 的真实现）：**逐源逐类型**问 DoH JSON →
/// [`parse_doh_response`] 定性。**同样禁用系统代理**——经本地 MITM 的 DoH 应答不可信；
/// **目标全是 IP 字面量**（[`DOH_SOURCES`]）→ 不触发任何系统 DNS 解析。
/// 每个源的超时 = [`DOH_TIMEOUT`]（单源单类型；最坏 = 源数 × 类型数 × 超时）。
/// `pub(crate)`（而非本模块惯例的 pub(super)）：经 `tailscale::real_probe` 再导出，
/// 升级检查的 DoH 钉 IP 兜底（commands/updater.rs）是 tailscale 之外的第二消费方。
pub(crate) fn real_probe() -> &'static ProbeFn {
    static PROBE: std::sync::OnceLock<Box<ProbeFn>> = std::sync::OnceLock::new();
    // get_or_init 给 &Box<ProbeFn>，一步 deref+unsize 到 &ProbeFn
    let probe: &ProbeFn = PROBE.get_or_init(|| {
        Box::new(|host: &str| {
            probe_addresses(host, |url| {
                reqwest::blocking::Client::builder()
                    .no_proxy()
                    .timeout(DOH_TIMEOUT)
                    .build()
                    .and_then(|c| c.get(url).send().and_then(|r| r.text()))
                    .unwrap_or_default() // 取不到 = 该源 Unusable（换下一个源）
            })
        })
    });
    probe
}

/// 单源单类型查询的超时。DoH 应答是几百字节的 JSON，5s 足够；调大只会让「源全挂」
/// 时把 IPC 线程占更久（重验在轮询线程、verify 步在 spawn_blocking，都不占主线程）
const DOH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// 源表里的第 i 个源的主机段（失败清单用，便于用户/日志定位是哪个源挂了）
fn source_host(source: &str) -> &str {
    let rest = source.split_once("://").map(|(_, r)| r).unwrap_or(source);
    rest.split(['/', '?']).next().unwrap_or(source)
}

/// DoH 取数内核（**fetch 注入**，可测——A3 多源 + B-M7 A→AAAA 回落）。
///
/// 顺序：**先 A 后 AAAA**（同一类型内**按源表顺序**逐源尝试）。每个源的应答分三类
/// （[`DohResponse`]）：`Records` → 直接采信返回；`NoRecord`（权威否定）→ 本类型不必
/// 再问别的源，换下一个记录类型；`Unusable` → 换下一个源。
///
/// 返回三态（对应 [`ProbeFn`] 的契约）：
/// - `Ok(记录)` = 至少一个源给了地址；
/// - `Ok(空)` = **A 与 AAAA 都拿到「全源权威否定」** ⇒ 真的「没有记录」
///   （实测形态：刚开通、记录尚未发布）；
/// - `Err(原因)` = 其余一切（全源不可用 / 只有部分源给了权威否定）⇒ 无法验证（fail-closed）。
///
/// **I-5（2026-10-07 评审复测：两处把「自家/单源拿不到结论」误报成「上游还没发布」）**：
/// ① **`NoRecord` 不 `break`**：记录传播期「一个解析器已返回 A、另一个权威 NODATA」很常见，
///    旧的 `break` 会**不再问次源** ⇒ 报「记录尚未发布」并把**已经可用**的地址继续藏起来。
///    现在只有**所有源**（源表问完）都给出权威否定，该类型才算「权威否定」；否则记入 unusable。
/// ② **A 与 AAAA 分别记账**：旧实现把两个类型合进一个 `authoritative_no_record` 布尔
///    ⇒ 「A 全源不可用（自家解析故障）+ AAAA 权威否定」也会 `Ok(空)`、报「记录尚未发布」，
///    而 A **从未拿到权威答复**——正是模块头注释承诺「不把自家解析故障说成上游的发布延迟」
///    的那一类。现在只有**被问到的类型都拿到权威否定**才 `Ok(空)`。
///
/// **A4 定性（二选一，选②：保留以备上游变更）**：2026-10-07 实测确认 **Funnel 始终只
/// 发布 A 记录，AAAA 不发布**——记录发布后 DoH 返回两条 A（两个入口 IP），同域名的 AAAA
/// 查询**始终 NODATA**（连续 14 次轮询无一例外）。故 `AAAA` 这一轮**当前永远不产出地址**。
/// **留着它不是因为有用，而是因为便宜**：仅在 A 没拿到记录时才问 AAAA（常态路径零额外
/// 开销）。**I-5 后的成本口径更新**：传播期（A 被全源权威否定）那一轮要多问「源数」次
/// （3 源 = 3 次；每源单类型超时 5s 上界），换来的是「上游某天改发 AAAA 时不会把有地址
/// 误判成未发布」的保险，以及「部分源 NODATA、部分源有记录」时不再误报未发布。
/// 已实测无用就**明说无用**（本注释 + 同名特性锁测试
/// `aaaa_fallback_is_kept_as_upstream_change_insurance`），不留不说话的死代码。
pub(super) fn probe_addresses(
    name: &str,
    fetch: impl Fn(&str) -> String,
) -> Result<Vec<String>, String> {
    let mut unusable: Vec<String> = Vec::new();
    // 拿到「**全源**权威否定」的类型数（I-5②：A 与 AAAA 分别记账，两个都拿到才 Ok(空)）
    let mut denied_types = 0usize;
    // 顺序固定：A 优先（实测 Funnel 只发 A），A 没拿到记录才问 AAAA（保险，见上）
    for rtype in ["A", "AAAA"] {
        let mut all_sources_denied = true; // 该类型：**每个**源都给了权威否定？
        let mut any_denial = false; // 至少一个源给了权威否定（源表为空时不算「全源否定」）
        for source in DOH_SOURCES {
            let url = doh_url_at(source, name, rtype);
            match parse_doh_response(&fetch(&url)) {
                DohResponse::Records(addrs) => return Ok(addrs),
                DohResponse::NoRecord => {
                    // I-5①：**不 break**——继续问后续源。传播期两源不一致很常见，
                    // 一旦 break 就会把已经可用的地址误报成「尚未发布」
                    any_denial = true;
                }
                DohResponse::Unusable => {
                    // 该源给不出结论 ⇒ 本类型**不算**权威否定（成因不得错认）
                    all_sources_denied = false;
                    unusable.push(format!("{}（{rtype}）", source_host(source)));
                }
            }
        }
        if all_sources_denied && any_denial {
            denied_types += 1;
        }
    }
    // 只有被问到的两个类型**都**拿到权威否定，才是「真的没有记录」（正常发布延迟）
    if denied_types == 2 {
        Ok(Vec::new())
    } else {
        Err(format!(
            "{} 个 DoH 源均无有效应答: {}",
            unusable.len(),
            unusable.join("、")
        ))
    }
}

// ---- 校验的测试缝（仅测试触碰，用后还原——与 RUN_CLI_OVERRIDE 同纪律）----

/// 校验入口单点（生产 = [`real_probe`]；测试经 `VERIFY_PROBE_OVERRIDE` 注入假探针，
/// 使 run_step("verify") / heal 全链可零网络测试）
fn do_verify(host: &str) -> Reachability {
    #[cfg(test)]
    if let Some(f) = VERIFY_PROBE_OVERRIDE.lock().unwrap().as_ref() {
        let r = verify_reachability(host, f);
        log_reach("手动校验", host, &r);
        return r;
    }
    let r = verify_reachability(host, real_probe());
    log_reach("手动校验", host, &r);
    r
}

/// 钉 IP 探测的行为替身类型（clippy type_complexity 收敛别名）
#[cfg(test)]
type HttpProbeFake = dyn Fn(&str, &str) -> Result<(), String> + Send;

#[cfg(test)]
pub(super) static VERIFY_PROBE_OVERRIDE: once_cell::sync::Lazy<
    std::sync::Mutex<Option<Box<ProbeFn>>>,
> = once_cell::sync::Lazy::new(|| std::sync::Mutex::new(None));

#[cfg(test)]
pub(super) static HTTP_PROBE_OVERRIDE: once_cell::sync::Lazy<
    std::sync::Mutex<Option<Box<HttpProbeFake>>>,
> = once_cell::sync::Lazy::new(|| std::sync::Mutex::new(None));

#[cfg(test)]
pub(super) fn set_verify_probe_override(f: Option<Box<ProbeFn>>) {
    *VERIFY_PROBE_OVERRIDE.lock().unwrap() = f;
}

#[cfg(test)]
pub(super) fn set_http_probe_override(f: Option<Box<HttpProbeFake>>) {
    *HTTP_PROBE_OVERRIDE.lock().unwrap() = f;
}

/// 当前机器域名（校验目标）：status 的 DNSName 优先（剥结尾点），回落
/// CertDomains[0]，再回落通道快照 url 的 host 段；全缺 → None（无法校验，如实失败）
pub(super) fn current_dns_host() -> Option<String> {
    let from_status = run_cli(&["status", "--json"])
        .ok()
        .and_then(|j| parse_status(&j).ok())
        .and_then(|s| {
            let dns = s.dns_name.trim().trim_end_matches('.').to_string();
            if !dns.is_empty() {
                return Some(dns);
            }
            s.cert_domains.into_iter().next().filter(|c| !c.is_empty())
        });
    from_status.or_else(|| ts_snapshot().url.as_deref().and_then(host_from_board_url))
}

/// 从看板 url 抽 host（纯函数）：scheme 后、首个 / 前的 host 段
pub(super) fn host_from_board_url(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    rest.split('/')
        .next()
        .map(str::to_string)
        .filter(|h| !h.is_empty())
}

/// verify 步内核：现算可达性；失败**且用户开着本通道**（DESIRED=Some）才自愈。
/// 通道没开绝不借 verify 偷开 Funnel——开通只属于 funnel 步/开关的职责
pub(super) fn verify_with_heal(port: u16) -> Reachability {
    let Some(host) = current_dns_host() else {
        return Reachability::Failed {
            reason: "读不到机器域名，无法校验（先完成登录/开通）".into(),
        };
    };
    let r = do_verify(&host);
    // 恢复窗口**不自愈**（W-A）：此刻配置极可能安然无恙（实测 T+73s tailscaled 自己
    // 就恢复了），reset+重开只会把它清掉；`Recovering` 也不是"失败"，轮询窗口会继续复验。
    if matches!(r, Reachability::Verified | Reachability::Recovering)
        || DESIRED.lock().unwrap().is_none()
    {
        return r;
    }
    // 连不通类同样**不自愈**（2026-10-13 用户裁决，判据见 [`ADDR_UNREACHABLE_PREFIX`]）：
    // 记录在、配置在，成因是公网链路瞬时抖动——reset 只会把好配置清掉再重发布 30–49 秒；
    // 60s 轮询复验自会恢复（前端同场景挂 tsTransientHint 引导，重试不再变成配置重置）。
    if matches!(&r, Reachability::Failed { reason } if reason.starts_with(ADDR_UNREACHABLE_PREFIX))
    {
        return r;
    }
    heal_and_reverify(port)
}

/// 失败时自动尝试自愈：**重置 Funnel 后重新开启**——2026-10-06 实测这一招能
/// 触发解析记录重新发布。仍失败才报错（§C3 要求 3）。
/// **2026-10-13 收窄**：连不通类失败（记录在、配置在，成因是公网链路抖动）已被
/// [`verify_with_heal`] 拦在门外——能走到这里的失败 = 记录缺失 / 读不到域名 /
/// 后端就绪后的其他真故障，reset 才有意义。
/// **守卫先行（与 stop_inner 同红线）**：配置是外来的绝不 `funnel reset`——它清
/// 整份 serve 配置；拒绝时如实报错，不掩盖（简报草稿里 reset 在守卫前，此处修正：
/// 否则 reset 先把外来配置清掉，守卫形同虚设）。
pub(super) fn heal_and_reverify(port: u16) -> Reachability {
    // **W-A 后端门（2026-10-07 真机重启实测）**：判配置归属与 `funnel reset` **之前**
    // 必须先确认 `BackendState=Running`——恢复窗口里 `funnel status --json` 会短暂为 `{}`，
    // 而 T+73s 实测 tailscaled 自己就把配置逐字段恢复了；此时 reset + 重开等于把
    // 一个完好的配置清掉（还白等一轮重发布）。初始化中如实报「恢复中」，不写任何东西。
    match run_cli(&["status", "--json"]).and_then(|j| parse_status(&j)) {
        Ok(s) if s.backend_state == "Running" => {}
        Ok(s) if super::status::backend_initializing(&s.backend_state) => {
            return Reachability::Recovering
        }
        Ok(s) => {
            return Reachability::Failed {
                reason: format!(
                    "Tailscale 未就绪（{}）——未重置 Funnel 配置（后端就绪后可重试）",
                    s.backend_state
                ),
            }
        }
        Err(e) => {
            return Reachability::Failed {
                reason: format!("读取 Tailscale 状态失败，未重置 Funnel 配置: {e}"),
            }
        }
    }
    // B-I4：守卫读数失败 → 不 reset、不重开（fail-closed）；旧实现在这里 unwrap_or_default()
    let before = match run_cli(&["funnel", "status", "--json"]) {
        Ok(j) => j,
        Err(e) => {
            return Reachability::Failed {
                reason: funnel_status_unreadable("自愈", &e),
            }
        }
    };
    if let ServeOwnership::Foreign { others } = serve_ownership(&before, port) {
        return Reachability::Failed {
            reason: foreign_serve_error("自愈", others),
        };
    }
    // **I1（2026-10-07 评审）**：reset **必须经 [`super::status::disable_funnel`]**
    // （唯一 reset 调用点）——① 它记账「本进程重开过」，否则随后的「记录尚未发布」
    // 会落首开档（把实测 30–49 秒说成 5–6 分钟，正是自愈最该走重开档的场景）；
    // ② 失败必须**如实返回**，绝不吞掉：reset 没成功还继续 `--bg` 等于把「半程自愈」
    // 当成功（配置已被清掉、地址却继续被宣称在路上）。
    if let Err(e) = disable_funnel() {
        return Reachability::Failed {
            reason: format!("自愈失败（重置 Funnel 未成功）: {e}"),
        };
    }
    if let Err(e) = enable_funnel(port) {
        return Reachability::Failed {
            reason: format!("自愈失败: {e}"),
        };
    }
    // 记录发布有延迟，给一个短退避再验（不要立刻断言失败）；测试零等待
    std::thread::sleep(heal_backoff());
    let Some(host) = current_dns_host() else {
        return Reachability::Failed {
            reason: "自愈后读不到机器域名".into(),
        };
    };
    do_verify(&host)
}

/// 自愈后的重验退避（实测记录发布需数秒）；测试置零（零等待，不拖慢套件）
fn heal_backoff() -> std::time::Duration {
    if cfg!(test) {
        std::time::Duration::ZERO
    } else {
        std::time::Duration::from_secs(5)
    }
}

/// 该发事件时的文案（纯函数，可测）：**判据是"地址从可用变不可用"**，与成因是不是
/// "正常现象"无关——所以 `Verified → Failed`（W-B 前唯一形态）、
/// `Verified → RecordPending`（**W-B 新增形态**：记录被撤，用户刚才还能用的地址
/// 现在打不开了）与 `Verified → Recovering`（**M8 新增形态**：中年期后端抖动——
/// tailscaled 重启 / `tailscale down/up`——把已验证地址撤下 1–2 分钟）都要发；
/// 其余（含各种"本来就没通过"）不发，防刷屏。
///
/// **为什么必须把 RecordPending 纳进来**：把"记录未发布"从 Failed 拆成独立态之后，
/// 若不在这里补一支，`Verified → 记录被撤` 就会**静默**——用户手里失效的地址没有任何
/// 提示（重构不得顺走既有能力）。文案取该档自己的口径（带时长），不谎称成因。
///
/// **M8（2026-10-07 评审 · 控制方裁决：做）为什么 Recovering 也要发**：规格原判据
/// 只列了 Failed / RecordPending，于是后端抖动时**地址被静默撤下**（只有卡面变化、
/// 没有 remote-tunnel-error）。开机场景不会因此刷屏：新进程起点恒 `Unverified`
/// （不是"从可用跌落"），生命周期入口还会把校验态作废——用例
/// `failure_event_includes_recovering_but_not_from_boot_start` 两面都钉住。
pub(super) fn failure_event_reason(prev: &Reachability, next: &Reachability) -> Option<String> {
    if !matches!(prev, Reachability::Verified) {
        return None;
    }
    match next {
        Reachability::Failed { reason } => Some(reason.clone()),
        Reachability::RecordPending { republish } => Some(record_pending_hint(*republish).into()),
        Reachability::Recovering => Some(RECOVERING_HINT.into()),
        _ => None,
    }
}

/// 重验一轮（注入缝版，可测）：现算新态 → 写校验态全局 → 仅 [`failure_event_reason`]
/// 判定要发时发既有 remote-tunnel-error 事件（复用 tunnel.rs 放弃守护时的形状
/// `{"error", "channel"}`），地址由载荷三门随之撤下。**通道无关（宪法原则 2）**：只写
/// tailscale 自己的校验态，不触碰隧道两路
pub(super) fn reverify_with(host: &str, probe: &ProbeFn) -> Reachability {
    let prev = reachability();
    let r = verify_reachability(host, probe);
    log_reach("轮询复验", host, &r);
    set_reachability(r.clone());
    if let Some(reason) = failure_event_reason(&prev, &r) {
        crate::remote::events::emit_ui(
            "remote-tunnel-error",
            serde_json::json!({ "error": reason, "channel": "tailscale" }),
        );
    }
    r
}

/// 轮询线程的重验入口（生产）：探针与用户手点 verify 同源（[`real_probe`]）
#[cfg(not(test))]
pub(super) fn poll_reverify(host: &str) -> Option<Reachability> {
    Some(reverify_with(host, real_probe()))
}

/// 轮询线程的重验入口（测试构建）：假探针优先；**没有假探针就不探测**（返回 None）。
/// 为什么要有这道闸：`start_channel` 的测试会 spawn 真实轮询线程，线程可能在测试结束后
/// 才醒（5s）——届时若撞上别的测试留下的全局态（DESIRED=Some + 报活配置的 CLI 替身），
/// 真探针会发起**真实网络请求**，违反零网络红线。故测试构建下探测必须显式注入
/// （与 run_cli 的 RUN_CLI_OVERRIDE 同纪律）；生产构建没有这个分支
#[cfg(test)]
pub(super) fn poll_reverify(host: &str) -> Option<Reachability> {
    let g = VERIFY_PROBE_OVERRIDE.lock().unwrap();
    g.as_ref().map(|f| reverify_with(host, f))
}
