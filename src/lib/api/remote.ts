import { invoke } from "@tauri-apps/api/core";

// Rust 端 remote_status 的返回（serde_json::json! 裸值，键名原样，无 camelCase 重命名）。
// M5 A6：设置页切到 channels + pin 新载荷；M5 A8 回归裁决：M3 起的 legacy 键
// （bind/port/url/lanUrls/channel/tunnelUrl/tunnelError/addresses）后端已删——
// 前端唯一数据源是 enabled + maxDevices + channels + pin + host 载荷
export type RemoteStatus = {
  enabled: boolean;
  /** 设备上限（线稿「已接入设备 N / 上限」徽标；KV 可改，未设置默认 10） */
  maxDevices: number;
  /** H3：无头注入总开关（KV remote.headless_enabled；缺键 = 默认关）。
   *  可选——旧后端载荷无此键，前端按 undefined = 关渲染（不谎报开） */
  headlessEnabled?: boolean;
  // M5 A5：四通道状态 + 当前访问密码（设置页卡片与详情区的唯一数据源；
  // 形状契约见 Rust 端 channels_payload 注释——A6 卡片与 A7 移动端消费同一形状）
  channels: RemoteChannels;
  pin: string | null;
  // M3 起 host 载荷（P8a/P8b）：name = display_host_name（已存命名回落系统主机名）
  // ——A6 本机名称输入框的默认值来源
  host?: { name: string; platform?: string; version?: string; bootId?: string };
  enabledTools?: string[];
};

// M5 A5 四通道状态载荷（Rust 端 channels_payload 注释即唯一契约）：
// enabled = 三通道 KV 开关（local 无此键——随总开关常驻）；running = 运行态；
// 隧道 address 错误态或已停不宣称 → null；lan.addresses = 完整可直达 URL 列表
export type RemoteChannels = {
  local: { running: boolean; address: string };
  lan: { enabled: boolean; running: boolean; addresses: string[] };
  quick: { enabled: boolean; running: boolean; address: string | null; error: string | null };
  named: {
    enabled: boolean;
    running: boolean;
    address: string | null;
    error: string | null;
    /** M5 P2-c：最近一次解析成功的地址（KV remote.named_addr_last） */
    lastAddr?: string | null;
  };
  // §C1 Tailscale 固定网址通道（Task 5 只加类型，卡片在 Task 9）：
  // 形状与隧道通道同构；address = 机器域名看板地址（https://<机器名>.<尾网>.ts.net/m）；
  // §C3（Task 7）地址三门 = running ∧ error 为空 ∧ reach.state="verified"。
  // W-B（真机实测三档时长）：未验证/在验 = 带预期时长的口径（首开 5–6 分钟）；
  // record_pending / recovering 各有各的口径（见下 reach 联合类型）；
  // Failed 时 error = 具体 reason，reach 随段透出
  tailscale: {
    enabled: boolean;
    running: boolean;
    address: string | null;
    error: string | null;
    reach: TsReachability;
  };
};

// §C3（Task 7）+ W-A/W-B 可达性态（Rust tailscale::Reachability：serde tag=state snake_case）。
// 只有 verified 才算「固定地址可用」：
// - failed：具体故障（拦截页 / 连不通 / 解析服务不可用），reason 供「尚未生效 + 重试」；
// - record_pending：**公网 DNS 记录尚未发布**（正常发布延迟，不是故障）——`republish`
//   分辨两档实测时长（true = 本进程内刚重新开通 ⇒ ≈30–49 秒；false = 首开档 ⇒ ≈5–6 分钟）；
// - recovering：**开机恢复窗口**（后端重连 1–2 分钟，记录不撤销、不用等 DNS）——
//   不是故障、也不是「域名生效中」，卡面与向导都给「恢复中」语义。
export type TsReachability =
  | { state: "unverified" }
  | { state: "verifying" }
  | { state: "verified" }
  | { state: "record_pending"; republish: boolean }
  | { state: "recovering" }
  | { state: "failed"; reason: string };

// 命令定义于 src-tauri/src/remote/mod.rs（M5 A1-A5 已落地）
export async function remoteStatus(): Promise<RemoteStatus> {
  return await invoke<RemoteStatus>("remote_status");
}
export async function remoteToggle(enabled: boolean): Promise<void> {
  return await invoke("remote_toggle", { enabled });
}
// 四通道独立开关（M5 A5 三通道 + §C1 tailscale）：channel ∈ "lan" | "quick" | "named" |
// "tailscale"（本机常驻无命令；tailscale on/off = Funnel 起停，与隧道互斥触达由后端
// 保证）。lan 开启未确认 TLS 反代时 Err 特征文案（PUBLIC_ACK_REQUIRED_MSG，含「对外
// 绑定需先确认已配置 TLS 反向代理」），前端据此弹确认 Dialog；幂等（重复同向调用 no-op）
export async function toggleChannel(channel: string, on: boolean): Promise<void> {
  return await invoke("remote_toggle_channel", { channel, on });
}
// 设置访问密码（M5 A4）：首次只落库；改值 = 全部设备吊销 + 断连；非法 PIN Err
export async function setPin(pin: string): Promise<void> {
  return await invoke("remote_set_pin", { pin });
}
// 重置设备（M5 A4，线稿「重置设备」按钮口径）：吊销全部设备 + SSE 断连，不改 PIN
export async function resetDevices(): Promise<void> {
  return await invoke("remote_reset_devices");
}
// 设备重命名（M5 A4）：空名拒绝；超 40 字由后端 DAO 截断
export async function renameDevice(id: string, name: string): Promise<void> {
  return await invoke("remote_rename_device", { id, name });
}
// TLS 前置确认（P7 安全门）：置位 remote.public_ack，解锁对外绑定
export async function remoteConfirmPublic(): Promise<void> {
  return await invoke("remote_confirm_public");
}
// H3 无头注入总开关（默认关，显式开启；裁决 9 单一总开关）：写 KV remote.headless_enabled
// + 审计 + 广播 remote-changed。无头绑定会话在关闭态被后端 403 headless_disabled 拦下
// （移动端则由 /session-send-info 的 injectable=false + reason 置灰——门在前）
export async function toggleHeadless(enabled: boolean): Promise<void> {
  return await invoke("remote_toggle_headless", { enabled });
}

// ============================================================
// 设备花名册（已配对设备行；M5 A3 起配对仅 /pair/pin，审批/直通命令已下线）
// ============================================================

// 已配对设备（花名册行）：online 口径在后端（SSE 注册 ∨ 30s 过闸），前端只渲染。
// via = 配对时刻接入通道（local/quick/named/lan，A1 起落库）；旧载荷无此键 →
// undefined 不渲染徽标（前端不猜）
export type RemoteDevice = {
  id: string;
  name: string;
  firstPairedAt: number;
  lastSeenAt: number;
  online: boolean;
  via?: string;
};
// 设备花名册（已吊销项后端已过滤）
export async function remoteDevices(): Promise<RemoteDevice[]> {
  return await invoke<RemoteDevice[]>("remote_devices");
}
// 踢下线（线稿口径）：DB 置位 + SSE 即时断连
export async function remoteRevokeDevice(id: string): Promise<void> {
  return await invoke("remote_revoke_device", { id });
}
// 全部吊销（A6 UI 已改用 resetDevices；命令仍在后端，去留由 A8 裁决，契约保留）
export async function remoteRevokeAllDevices(): Promise<void> {
  return await invoke("remote_revoke_all_devices");
}

// ============================================================
// §C2 Tailscale 首次配置引导一条龙（Task 6）：向导探测 + 单步触发
// 载荷形状契约 = Rust 端 tailscale::wizard_status / run_step 注释
// ============================================================

// 向导步骤（静态平台数据；id 稳定机器标识，UI 文案走 i18n 键）
export type TsWizardStep = {
  id: string;
  needsHuman: boolean;
  /** A1：该人工步**可能不出现**（上游按尾网策略决定要不要人点）——目前只有 funnel
   *  批准步。true 时前端渲染成「若出现才需点」并给弱提示，**绝不渲染成"等你点批准"** */
  humanOptional: boolean;
  /** 需要人时的动作文案键（i18n 全路径，如 settings.remote.tsWizard.actAdmin） */
  humanActionKey: string;
};

// 单步完成态：done = 判据命中；blockedReason = 卡住原因（null = 单纯待做）。
// **M4（2026-10-07 评审）**：blockedTone = 卡点的语义档位——rose = 真故障（红「卡住：」），
// amber = 中间态（恢复窗口「正在进行、无需操作」，线稿 82-84：既不是故障也不是完成）。
// 可选：载荷缺该字段时前端按 rose 处理（fail-safe 落到"要人看"那一档，不当成正常）。
export type TsStepState = {
  id: string;
  done: boolean;
  blockedReason: string | null;
  blockedTone?: "rose" | "amber";
};

// remote_ts_probe 载荷：platform 三值；windowsVerified=false 时向导顶部显示
// Windows 弱提示（不许把未验证流程伪装成已验证）。**I-3：Windows 验证位按覆盖面拆细**——
// windowsVerified = **整条** Windows 流程是否都实机跑过（2026-10-07 用户卸载后从零走完
// 兔维斯 向导全程 ⇒ 后端把实测覆盖起点前移到第一步 ⇒ **当前为 true**，弱提示随之撤下；
// 机制保留：将来又有未实测段落时起点后移，清单非空、提示自动回来）；
// windowsVerifiedFrom = 实测覆盖从哪一步起（当前 = 第一步 "detect"）；
// windowsUnverifiedSteps = 没被端到端实机跑过的步骤（前端据此点名，不写死清单文案）。
// authUrl = 待登录授权链接（login 步「去登录」按钮，兔维斯 不代登录）；
// reach = 可达性态（**逐名枚举见上 TsReachability，不写死态数**——M6 纪律；
// 前端据此门控头部地址展示：只有 Verified 才显示）
export type TsWizardProbe = {
  platform: "mac" | "windows" | "other";
  windowsVerified: boolean;
  /** I-3：实测覆盖从哪一步起（非 Windows 平台为 null） */
  windowsVerifiedFrom?: string | null;
  /** I-3：没被端到端实机跑过的步骤 id（非 Windows 平台为空表） */
  windowsUnverifiedSteps?: string[];
  /** B2：写路径（funnel --bg / reset / 批准链接抓取）实机验证位（逐平台）——
   *  false 时向导顶部给弱提示，**不得把未验证的写路径伪装成已验证** */
  writePathVerified: boolean;
  /** A1：人工步骤数（必需 / 可能不出现），由后端步骤表派生 */
  humanSteps?: { required: number; optional: number };
  steps: TsWizardStep[];
  states: TsStepState[];
  authUrl: string;
  running: boolean;
  boardUrl: string | null;
  reach: TsReachability;
};

// serve 配置里的一条 handler（B1 撤销预览/确认框用；ours = 是否 兔维斯 那一路）
export type TsServeEntry = { ours: boolean; label: string };

// remote_ts_run_step 回执（按步取用，字段可选；verify 步带最终校验态；
// disable_preview 步带撤销预览：foreign = 普通撤销会被守卫拒绝，
// wouldClear = 会随 `funnel reset` 一并被清除的非 兔维斯 条目数，
// entries = 逐条清单——**撤销前的知情同意靠它**）
export type TsStepResult = {
  ok: boolean;
  approvalUrl?: string | null;
  /** **login 步的授权链接**（⑤，2026-10-07 用户实测）：新装机器上 `status --json` 的
   *  `AuthURL` 是**空串**（授权链接要**发起一次交互式登录**才由尾网生成），所以登录步
   *  不再是"等链接自己出现"——`remote_ts_run_step("login")` 会**后台发起**一次
   *  `tailscale login`（只 spawn、不等待；argv = `login --timeout 15s`，见 Rust
   *  `wizard::LOGIN_ARGS`）并**有界轮询**（≤9.5s）把 `AuthURL` 取回来，链接随本字段
   *  交回前端做成按钮。**兔维斯 只递链接：不代登录、不持凭据。**
   *  已登录（Running）时为空串（无需链接）；始终拿不到时也为空串——前端据空串给
   *  「去本机客户端点 Log in / 稍候重试」的兜底文案（走静态 i18n 键，不渲染 `note`）。
   *  **M4（2026-10-08 架构评审）：前端把本字段当"过渡回执"用**——立即上墙，但**随下一次
   *  探测落地作废**（`TailscaleWizard` 的 `receiptGen`/`probeGen`）：权威源永远只有探测
   *  （`TsWizardProbe.authUrl`），探测说没有链接时不许拿旧回执硬撑（链接会被 tailscaled
   *  轮换/失效）。 */
  authUrl?: string;
  /** 本次 login 步**是否发起过**登录尝试（true = 已让尾网去生成链接；false/缺省 =
   *  已有链接或已登录，未发起）。幂等语义：已有链接时不发起（用户可能正拿着它在浏览器操作）。 */
  triggered?: boolean;
  /** Tailscale 后端状态（`sys_ext` / `login` 步回读；Deferred 回执里点明「卡在哪个态」） */
  backendState?: string;
  path?: string;
  skipped?: string;
  done?: boolean;
  /** W-A：后端重连/初始化窗口内这一步**没做**（后端拒绝写入，或不判定既有配置形态）——
   *  `true` **不是失败**（回执 `ok: true`），配套 `backendState` + `note` 说明成因；
   *  期望态已记（如 Funnel 的 DESIRED），后端就绪后由轮询自动复评/补开通。
   *  **前端只声明不消费**（2026-10-07 判断，非疏漏）：动作回执后必重探
   *  （TailscaleWizard.run 的 `await load()`），重探载荷的 `states[]` 已把**同一事实**
   *  如实呈现成 amber「后端正在重连…」而非「待做 / 卡住」（后端 `probe_steps_from` 的
   *  shields_up / funnel 两处 `blocked_amber` + 前端 blockedTone 分支），用户可见的
   *  诚实性不缺；再引入回执派生的第二份状态源要额外管生命周期（探测定稿后残留的
   *  「本次已推迟」会变成假话），且回执里的 `note` 是中文、上屏会绕过 i18n。
   *  故：**只补类型**，UI 仍以重探为权威源。 */
  deferred?: boolean;
  /** 后端下发的**成因说明**（中文人话；Deferred / unreadable 回执里带）。
   *  **前端当前零消费者**（全仓无 `.note` 读取点，2026-10-07 核实）：用户可见的成因说明
   *  一律走**静态 i18n 键**（如 `settings.remote.tsDisableDescUnreadable`，与后端文案同义，
   *  英文界面出英文）；本字段的消费方目前只有**后端自己的测试锚点**
   *  （`tailscale/mod.rs` 的 I2/W-A 用例逐字断言它含「重连」）。
   *  保留声明 = 如实描述载荷形状（日志 / 移动端将来可用）；**要渲染它先做 i18n 化**——
   *  后端串是中文硬编码，直接上屏会绕过 i18n。 */
  note?: string;
  reach?: TsReachability;
  foreign?: boolean;
  wouldClear?: number;
  entries?: TsServeEntry[];
  /** W-A（第三处判据点）：后端重连中读不到现有 serve 配置形态——**不是「没有条目」**。
   *  前端必须照 foreign 处理（先弹确认框并说明读不到），绝不静默直接撤销。
   *  成因说明在 **UI 侧**走等效的静态 i18n 键 `settings.remote.tsDisableDescUnreadable`
   *  （RemoteSection 按本字段分支渲染），**不读**回执的 `note`（消费方见上方 `note` 注释） */
  unreadable?: boolean;
  clearedExtraServeEntries?: number;
  /** M-1：随本次 `funnel reset` **实际**被一并清除的非 兔维斯 条目（与
   *  clearedExtraServeEntries 同源）——卡面「N 条」与逐条列表都取它，不再出现
   *  「确认框列预览条目、toast 报撤销时条数」两处对不上 */
  clearedEntries?: TsServeEntry[];
  forced?: boolean;
};

export async function remoteTsProbe(): Promise<TsWizardProbe> {
  return await invoke<TsWizardProbe>("remote_ts_probe");
}

// 幂等；需要人的步骤只做「触发+回报」，不代点。Err（如外来 serve 配置守卫拒绝、
// 安装包校验失败）以字符串 reject 透传——调用方（TailscaleWizard）把它落到对应
// 步骤行内的可见错误行（组件内展示，**无全局 toast**；Task 6 评审移交指针修正）
export async function remoteTsRunStep(step: string): Promise<TsStepResult> {
  return await invoke<TsStepResult>("remote_ts_run_step", { step });
}
