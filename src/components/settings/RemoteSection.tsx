// 设置页「远程接入」分区（§C4：按线稿 v6 重排为 4 张对外通道卡，UI 唯一契约 =
// docs/superpowers/wireframes/2026-09-17-remote-settings-redesign.html）。
// 结构三段式：① 通用（总开关 / 电源保活 / 本机名称）② 通道四卡一排 + 唯一展开
// 详情区（局域网连接 / 临时隧道 / 自有域名 / 外部域名——「本机」不是
// 通道，是访问方式：走局域网卡地址，零豁免 §C4）③ 访问与安全（访问密码 / 重置设备
// / 已接入设备列表）。状态唯一数据源 = remote_status 的 channels + pin 载荷（M5 A5）；
// tailscale 卡相位数据源 = remote_ts_probe（§C2/§C3；相位枚举见 TsPhase）；命令统一走
// src/lib/api/remote.ts；Token / 本机名 / 保活写通用 set_setting。
// 排版层级统一走 components/settings/typography.ts 单一出处（2026-10-07 用户裁决 C1）：
// 卡标题取 SETTINGS_CARD_TITLE、副标题取 SETTINGS_SUBTITLE，不再内联字号字重；本文件
// 另被登记为小字号豁免面（密集数据网格），豁免理由见 typography.ts。
import {
  SETTINGS_CARD_TITLE,
  SETTINGS_REMOTE_BADGE,
  SETTINGS_SUBTITLE,
} from "@/components/settings/typography";
import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import QRCode from "qrcode";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { useAppTranslation } from "@/hooks/use-app-translation";
import { toast } from "sonner";
import { cn } from "@/lib/utils";
import { formatInvokeError } from "@/lib/invokeError";
import {
  remoteConfirmPublic,
  remoteDevices,
  remoteRevokeDevice,
  remoteStatus,
  remoteToggle,
  remoteTsProbe,
  remoteTsRunStep,
  renameDevice,
  resetDevices,
  setPin,
  toggleChannel,
  toggleHeadless,
  setHeadlessLimits,
  type RemoteDevice,
  type RemoteStatus,
  type TsServeEntry,
  type TsWizardProbe,
} from "@/lib/api/remote";
import { getSetting, setSetting } from "@/lib/api/settings";
// §C2 Tailscale 首次配置向导（Task 6；Task 9 收敛为 tailscale 卡详情区的进度渲染器）：
// 步骤表组件自带标题与全部 tsWizard.* i18n 字面量（RemoteSection 不直接引用两级键
// ——本文件的 i18n 守卫测试按单层键扫描）
import { TailscaleWizard } from "@/components/settings/TailscaleWizard";

// B1：撤销预览载荷（Rust 端 run_step("disable_preview") 的形状，见 wizard.rs 注释）
type TsDisablePreview = {
  foreign: boolean;
  wouldClear: number;
  entries: TsServeEntry[];
  /** W-A：后端重连中读不到配置形态（≠ 没有条目）——确认框必须如实说明，见对话框渲染 */
  unreadable: boolean;
};

// 本机名：与 Rust 端 remote::KEY_HOST_NAME 对齐；空串/空白原样写——后端
// display_host_name 过滤空白后回落系统名，前端不做非空校验（口径单点在后端）。
// A6：未设置时默认填系统名（remote_status.host.name），灰字占位提示删除
const HOST_NAME_KEY = "remote.host_name";
// 隧道 Token：与 Rust 端 remote::KEY_TUNNEL_TOKEN 对齐；A6 起保存走通用 set_setting
//（remote_set_channel 已下线），开关由自有域名卡片开关（remote_toggle_channel）驱动
const TUNNEL_TOKEN_KEY = "remote.tunnel_token";
// H3 一次性安全说明的**已读记忆键**：与后端同库（settings 表）持久化——同 codex
// 一次性提示（monitor/hooks.rs 的 codex_hook_notice_shown）的既有口径：确认过即写
// "true"，此后开启不再弹；换机器/清库会再弹一次（可接受：说明本就该在陌生环境重放）
const HEADLESS_ACK_KEY = "remote.headless_notice_ack";
// 电源保活：与 Rust 端 remote::power::KEY_KEEPALIVE 对齐；默认开，
// 后端 should_acquire（None/乱串 → true）是唯一口径，前端仅同步展示
const KEEPALIVE_KEY = "remote.keepalive";
// 远程消息设备签名开关（2026-10-05 用户裁决）：默认关（省 token；溯源在注入审计页）——
// 与 Rust 侧 normalize::message_signature_enabled 的 KV 键/取值逐字对齐
const SIGNATURE_KEY = "remote_message_signature";
// H4（Task 6）：无头子区两件的文档默认值——与 Rust 端
// inject::headless::{DEFAULT_TIMEOUT_MS, DEFAULT_CONCURRENCY} 同值（缺键/旧后端载荷
// 时按此渲染，不让输入框空着）；后端是唯一权威（clamp 后落库）
const DEFAULT_HEADLESS_TIMEOUT_MS = 600000;
const DEFAULT_HEADLESS_CONCURRENCY = 2;
// P7 门特征文案（与 Rust PUBLIC_ACK_REQUIRED_MSG 单点常量同源的前缀特征）：前端只做
// includes 判别分流弹既有 TLS Dialog，不复制门槛判定——文案漂移由后端常量保证
const PUBLIC_ACK_FEATURE = "对外绑定需先确认已配置 TLS 反向代理";

// 四通道标识（线稿 v6 四卡顺序固定；「本机」不是通道——零豁免 §C4 后它只是历史
// via 值，见 VIA_LABEL_KEY 的保留映射）。值域与后端 ChannelKind::parse 一致
type ChannelKind = "lan" | "quick" | "named" | "tailscale";

// P7 特征判别：invoke 错误透传形态为 string（Rust Err(String)），兜底 Error/任意值
function isPublicAckRequired(e: unknown): boolean {
  const raw = typeof e === "string" ? e : e instanceof Error ? e.message : String(e);
  return raw.includes(PUBLIC_ACK_FEATURE);
}

type TFunc = ReturnType<typeof useAppTranslation>["t"];

// 设备最近活跃相对时间（分钟/小时/天三档，<1 分钟按「刚刚」）
function lastSeenLabel(lastSeenAt: number, t: TFunc): string {
  const minutes = Math.max(0, Math.floor((Date.now() - lastSeenAt) / 60_000));
  if (minutes < 1) return t("settings.remote.rosterSeenNow");
  if (minutes < 60) return t("settings.remote.rosterSeenMin", { n: minutes });
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return t("settings.remote.rosterSeenHour", { n: hours });
  return t("settings.remote.rosterSeenDay", { n: Math.floor(hours / 24) });
}

// 徽标（线稿 .badge：green 推荐/运行中、blue 公网、gray 仅本机/局域网、violet 隧道）
function Badge({
  tone,
  children,
}: {
  // W-A（线稿六次回写）：新增 amber 档 = 「恢复中 / 发布中」这类**正在进行、无需操作**
  // 的中间态——既不能借 green（会读成已完成），也不能借 gray（会读成未运行）
  tone: "gray" | "green" | "blue" | "violet" | "amber";
  children: ReactNode;
}) {
  const tones = {
    gray: "bg-secondary text-secondary-foreground",
    green: "bg-emerald-500/15 text-emerald-600 dark:text-emerald-400",
    blue: "bg-blue-500/15 text-blue-600 dark:text-blue-400",
    violet: "bg-violet-500/15 text-violet-600 dark:text-violet-400",
    amber: "bg-amber-500/15 text-amber-600 dark:text-amber-400",
  } as const;
  return (
    <span
      className={cn(
        `inline-flex flex-none items-center rounded-full px-2 py-0.5 ${SETTINGS_REMOTE_BADGE}`,
        tones[tone]
      )}
    >
      {children}
    </span>
  );
}

// 设备行 via 徽标（装饰字段，不变量 G1 推论③：只展示，永不进安全判定）。
// quick/named/lan/tailscale 由服务端按域名/尾网推断；**local 是历史值**——2026-10-06
// 零豁免后服务端不再产生（本机不是一条通道），但历史行保留可读，故映射保留。
// 徽标文案与 §C4 卡名统一（Task 8 评审裁定）：走同一 i18n 键，改卡名自动跟随。
// 未知/缺失值不渲染——前端不猜。
const VIA_LABEL_KEY: Record<string, string> = {
  local: "settings.remote.chanLocal",
  lan: "settings.remote.chanLan",
  quick: "settings.remote.chanQuick",
  named: "settings.remote.chanNamed",
  tailscale: "settings.remote.chanTailscale",
};
function ViaBadge({ via }: { via: string | undefined }) {
  const { t } = useAppTranslation();
  if (!via) return null;
  const key = VIA_LABEL_KEY[via];
  if (!key) return null;
  // 对外隧道族（quick/named/tailscale）紫标；lan/历史 local 灰标
  const tone = via === "quick" || via === "named" || via === "tailscale" ? "violet" : "gray";
  return <Badge tone={tone}>{t(key)}</Badge>;
}

// tailscale 卡相位（**逐名枚举、不写态数**——M6 纪律：数字会随枚举漂移）：
// - notInstalled（detect 判据未命中）/ configuring（其余未齐）/ configured（全部步骤完成
//   ∧ reach=Verified ∧ 通道在运行）/ configuredIdle（**已装好并登录过、但通道没在运行**）；
// - recovering（W-A 开机恢复窗口，见下相位机注释）/ unknown（probe 未达：加载中/失败时
//   不猜相位，渲染向导本体——其自带加载脉冲/读取失败行诚实呈现）。
// 线稿 p-ext 状态一~五（未安装 / 配置中 / 已配置 / 通道未运行 / 开机恢复中）即前五者。
// I-1（评审 Important）：第四个态不是装饰——旧相位机只有三态，把「成因三档」长在
// configured 分支里，而 configured 要求 verify 步完成（后端判据 = Verified ∧ 快照
// running），于是 **running=false 与 configured 在稳态互斥** ⇒ 通道一停，成因行永远
// 渲染不出来，用户只看到「配置中」+ 向导列表（成因被吞掉，正是 B3 要收口的那件事）。
type TsPhase =
  | "notInstalled"
  | "configuring"
  | "configured"
  | "configuredIdle"
  // W-A：开机恢复窗口（后端重连 1–2 分钟）——既不是故障也不是「未运行」，见相位机注释
  | "recovering"
  | "unknown";

// 通道二维码（qrcode 生成放 effect 事件路径，不在渲染期调用；失败降级无图不阻塞页面）
function ChannelQr({ url, caption }: { url: string; caption: string }) {
  const [dataUrl, setDataUrl] = useState<string | null>(null);
  useEffect(() => {
    let alive = true;
    setDataUrl(null);
    QRCode.toDataURL(url, { width: 132, margin: 1 })
      .then((d) => {
        if (alive) setDataUrl(d);
      })
      .catch((e) => console.error("qrcode render failed:", e));
    return () => {
      alive = false;
    };
  }, [url]);
  return (
    <div className="flex flex-none flex-col items-center gap-1">
      {dataUrl ? (
        <img src={dataUrl} alt={caption} className="h-[132px] w-[132px] rounded-lg border" />
      ) : (
        <div className="bg-muted/40 h-[132px] w-[132px] animate-pulse rounded-lg border" />
      )}
      <p className="text-muted-foreground text-center text-[10.5px]">{caption}</p>
    </div>
  );
}

export function RemoteSection() {
  const { t } = useAppTranslation();
  const [status, setStatus] = useState<RemoteStatus | null>(null);
  // 电源保活开关（默认开）：受控 Switch，加载回填、切换落盘
  const [keepalive, setKeepalive] = useState(true);
  // 设备签名开关（默认关）：受控 Switch，加载回填、切换落盘
  const [signature, setSignature] = useState(false);
  // 本机名称：受控输入，加载回填、blur 落盘；未设置时默认系统名（host.name）
  const [hostName, setHostName] = useState("");
  const hostNameSavedRef = useRef(false);
  const hostNameDefaultedRef = useRef(false);
  // 访问密码：首拍 status.pin 回填一次，轮询不回读（保留编辑中值）
  const [pinInput, setPinInput] = useState("");
  const pinInitRef = useRef(false);
  // 隧道 Token：进面板回填已存值
  const [token, setToken] = useState("");
  // 教程 popover（线稿 .help：点击展开/收起，可保持展开边看边操作）
  const [helpOpen, setHelpOpen] = useState(false);
  // 唯一展开的通道详情卡（线稿 pick()：同时只显示一个，点其它卡切换）。默认局域网
  // ——本机卡已删（§C4），局域网是本机访问与局域网访问共用的第一入口
  const [selected, setSelected] = useState<ChannelKind>("lan");
  // tailscale 卡相位数据源（remote_ts_probe）与「一键配置」展开的向导本体开关：
  // 探测要 spawn CLI 子进程，只在选中该卡后发起（不偷跑）
  const [tsProbe, setTsProbe] = useState<TsWizardProbe | null>(null);
  const [tsWizardOpen, setTsWizardOpen] = useState(false);
  // B1：撤销 tailscale 前的只读预览（确认框数据源）与被清除条目的卡面呈现
  const [tsPreview, setTsPreview] = useState<TsDisablePreview | null>(null);
  const [tsCleared, setTsCleared] = useState<{ entries: TsServeEntry[] } | null>(null);
  // 设备花名册（3s 轮询）+ 行内重命名态
  const [devices, setDevices] = useState<RemoteDevice[]>([]);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [editName, setEditName] = useState("");
  // TLS 对外绑定确认弹窗：lan 开关触发（toggle_channel Err 特征文案 → 确认 → 重试）
  const [tlsOpen, setTlsOpen] = useState(false);
  // H3 无头安全说明弹窗（一次性：确认过即写 HEADLESS_ACK_KEY，此后不再弹）
  const [headlessNoticeOpen, setHeadlessNoticeOpen] = useState(false);
  // H3 说明已读记忆（进面板读一次；读失败/缺键 = 未确认 → 照常弹，宁多提示不漏提示）
  const headlessAckRef = useRef(false);
  // 重置设备二次确认弹窗
  const [resetOpen, setResetOpen] = useState(false);
  // H4（Task 6）无头配置两件：超时（毫秒）+ 全局并发上限。首个 status 到达即回填并
  // 上锁（ref 闸）——3s 轮询不 clobber 编辑中的输入框（同 hostName 的既有口径）
  const [headlessTimeout, setHeadlessTimeout] = useState(String(DEFAULT_HEADLESS_TIMEOUT_MS));
  const [headlessConcurrency, setHeadlessConcurrency] = useState(
    String(DEFAULT_HEADLESS_CONCURRENCY)
  );
  const headlessLimitsInitRef = useRef(false);
  // 开关在途互斥：连点会并发远程命令（启停竞态），与旧版 busy 语义一致
  const [busy, setBusy] = useState(false);

  // 轻量 status 刷新：只刷 status——轮询若回读 KV 会 clobber 编辑中的输入框
  const refreshStatus = useCallback(async () => {
    try {
      setStatus(await remoteStatus());
    } catch {
      /* 刷新尽力而为，下一拍自愈 */
    }
  }, []);

  // 设备表刷新：尽力而为，失败静默（下一拍轮询自愈）；null 兜底空表
  const refreshDevices = useCallback(async () => {
    try {
      setDevices((await remoteDevices()) ?? []);
    } catch {
      /* 刷新尽力而为 */
    }
  }, []);

  // tailscale 向导探测（尽力而为）：失败静默置空——卡面落 unknown 态，
  // 由向导本体的加载脉冲/读取失败行诚实呈现，前端不猜相位
  const loadTsProbe = useCallback(async () => {
    try {
      setTsProbe(await remoteTsProbe());
    } catch {
      setTsProbe(null);
    }
  }, []);

  useEffect(() => {
    void refreshStatus();
  }, [refreshStatus]);

  // 保活回填：进面板读一次；null/非 "false" → 默认开（与后端 fail-safe 口径一致）
  useEffect(() => {
    void (async () => setKeepalive((await getSetting(KEEPALIVE_KEY)) !== "false"))();
    void (async () => setSignature((await getSetting(SIGNATURE_KEY)) === "on"))();
  }, []);

  // 本机名回填：已存值优先；未设置 → 后续 effect 以系统名（status.host.name）兜底
  useEffect(() => {
    void (async () => {
      const saved = await getSetting(HOST_NAME_KEY);
      hostNameSavedRef.current = saved !== null && saved.trim() !== "";
      if (saved !== null) setHostName(saved);
    })();
  }, []);

  // 系统名兜底（A6：默认填系统用户名/主机名，去掉灰字提示）：只在「无已存值且
  // 用户尚未编辑」时填一次；ref 双闸防止轮询期间反复覆盖空输入框；
  // 函数式写入防挂载竞态——首个 status 到达前用户已敲的字符不被兜底值冲掉（A8 评审）
  useEffect(() => {
    if (hostNameSavedRef.current || hostNameDefaultedRef.current) return;
    const sys = status?.host?.name;
    if (sys) {
      hostNameDefaultedRef.current = true;
      setHostName((prev) => (prev.trim() === "" ? sys : prev));
    }
  }, [status]);

  // Token 回填：进面板读已存值，后续刷新不回读
  useEffect(() => {
    void (async () => setToken((await getSetting(TUNNEL_TOKEN_KEY)) ?? ""))();
  }, []);

  // H3 安全说明已读回填：进面板读一次（"true" = 已确认过 → 开启不再弹）
  useEffect(() => {
    void (async () => {
      try {
        headlessAckRef.current = (await getSetting(HEADLESS_ACK_KEY)) === "true";
      } catch {
        /* 读失败按未确认处理（照常弹说明） */
      }
    })();
  }, []);

  // PIN 回填：首个非空 status.pin 填一次（ref 闸），轮询不覆盖编辑中值
  useEffect(() => {
    if (pinInitRef.current) return;
    const p = status?.pin;
    if (typeof p === "string" && p.length > 0) {
      pinInitRef.current = true;
      setPinInput(p);
    }
  }, [status]);

  // M-1：撤销回看（被一并清除的条目）只在**当前这次撤销**的语境里有意义——切换卡片就
  // 收起（重新开通也收起，见 changeChannel 的 on 分支）。旧实现一旦置上永不清，用户
  // 切走再回来还挂着一条早先的「已撤销」告示。
  useEffect(() => {
    setTsCleared(null);
  }, [selected]);
  // H4 无头配置回填：首个 status 到达填一次（缺键 → 文档默认值，输入框不留空）
  useEffect(() => {
    if (headlessLimitsInitRef.current || !status) return;
    headlessLimitsInitRef.current = true;
    setHeadlessTimeout(String(status.headlessTimeoutMs ?? DEFAULT_HEADLESS_TIMEOUT_MS));
    setHeadlessConcurrency(String(status.headlessConcurrency ?? DEFAULT_HEADLESS_CONCURRENCY));
  }, [status]);

  const enabled = status?.enabled ?? false;
  // H3 无头总开关态：唯一数据源 = remote_status.headlessEnabled（缺键/旧载荷 = 关）
  const headlessEnabled = status?.headlessEnabled ?? false;

  // 花名册与状态 3s 轮询恒开（与移动端看板同节奏）。花名册是 DB 语义（已配对设备
  // 的吊销/重命名管理入口），不随远程关闭清空——Mac 报告七-6「关闭期间 0/10 而 DB
  // 9 行」的根因即旧版「disabled 清空设备表」；后端 remote_devices 本就无关开关态
  // （恒查 remote_devices 表 revoked=0），关闭态行内 online 点全灰即真实状态。
  // status 同拍轮询保持开关/通道态新鲜（PIN/本机名回填有 ref 闸，不 clobber 编辑中输入框）
  useEffect(() => {
    void refreshDevices();
    const timer = setInterval(() => {
      void refreshStatus();
      void refreshDevices();
    }, 3000);
    return () => clearInterval(timer);
  }, [refreshDevices, refreshStatus]);

  const channels = status?.channels;
  // tailscale 段（§C3 三重门后的展示快照：address 已由后端按 running∧error空∧verified
  // 收口，UI 只渲染不拼接不猜）
  const tsChannel = channels?.tailscale;
  // 设备上限（后端 KV 可改；未载荷时回落 10 = 决策 #17 默认值）
  const maxDevices = status?.maxDevices ?? 10;
  // 展示态唯一判据 = running；开关本体唯一判据 = enabled（评审 C-I1）。旧判据
  // chanOn = enabled||running 让卡片状态点与「未开启」徽标都退化成 !enabled
  //（running ⇒ enabled），于是「通道开关开着但服务没在监听」永远显示为 live——
  // 而「总开关关闭 ∧ 通道开关开着」是一键可达的默认路径（stop_server(false) 只停
  // 监听/隧道，不改通道 KV）：载荷 {enabled:true, running:false, addresses:[真实地址]}
  // 下旧版照旧给绿色推荐徽标 + 地址 + 复制 + 二维码，全是死链。
  const chanEnabled = (key: ChannelKind): boolean => Boolean(channels?.[key]?.enabled);
  const chanRunning = (key: ChannelKind): boolean => Boolean(channels?.[key]?.running);
  const lanRunning = chanRunning("lan");
  const quickRunning = chanRunning("quick");

  // !running 的成因分三档——用户必须看懂「为什么进不去」并知道「该打开哪个」：
  //   ① 总开关关 → 服务根本没在监听，通道开关开着也没用 → 未运行 + 指上方总开关；
  //   ② 总开关开 ∧ 该通道开关关 → 该通道自己的关闭文案（lan/quick 各自措辞）；
  //   ③ 两开关都开却仍未监听 → 不谎报成因（启动中/启动失败/端口占用），如实说服务没在监听。
  // 载荷未达（status = null）时不判定成因（渲染侧以 status 门控）——不猜。
  type OffCause = "master" | "channel" | "listening";
  const offCause = (key: ChannelKind): OffCause =>
    !enabled ? "master" : !chanEnabled(key) ? "channel" : "listening";
  // 徽标/文案键保持字面量：i18n 守卫测试按 settings.remote.<单层键> 扫描源码，
  // 拼接出来的键字符串会逃过该守卫（新增键必须在此以字面量出现）
  const offBadge = (key: ChannelKind): string =>
    offCause(key) === "channel"
      ? t("settings.remote.chanOffBadge")
      : t("settings.remote.chanStoppedBadge");
  // B3：四张卡共用**同一套**成因三档（总开关 / 本通道开关 / 服务没在监听）——每张卡只
  // 提供「本通道开关关」那一档的自有文案。tailscale/named 此前游离在外（前者无论哪种
  // 成因都甩一句 tsOffHint「开关已关」，后者压根没有运行态行），同族残留在此收口。
  const offHint = (key: ChannelKind): string => {
    const cause = offCause(key);
    if (cause === "master") return t("settings.remote.masterOffHint");
    if (cause === "listening") return t("settings.remote.chanNotListeningHint");
    switch (key) {
      case "lan":
        return t("settings.remote.lanOffHint");
      case "quick":
        return t("settings.remote.quickOffHint");
      case "named":
        return t("settings.remote.namedOffHint");
      default:
        return t("settings.remote.tsOffHint");
    }
  };

  // tailscale 卡相位判定（线稿 p-ext 状态一~五）：detect 判据 = 本机装有 Tailscale CLI；
  // 已配置 = 全部步骤完成 ∧ 通道载荷校验态 Verified ∧ **通道在运行**（probe 的 verify 步
  // 判据本身含 Verified∧running，此处再对齐 3s 轮询载荷，双保险不提前宣布配置完成，
  // 也保证状态三的「运行中 + 地址 + 二维码」不会与 running=false 的载荷并存）。
  const tsStates = tsProbe?.states ?? [];
  const tsStepDone = (id: string): boolean => tsStates.find((s) => s.id === id)?.done === true;
  const tsDetectDone = tsStepDone("detect");
  const tsAllStepsDone =
    tsProbe !== null && tsProbe.steps.length > 0 && tsProbe.steps.every((s) => tsStepDone(s.id));
  const tsReachVerified = tsChannel?.reach?.state === "verified";
  const tsRunning = tsChannel?.running ?? false;
  // I-1「已配置·未运行」判据（状态四）：**已装好并登录过**——除 funnel / verify 这两个
  // 依赖 Funnel 自身的步骤外，其余步骤都完成（检测/下载/安装/登录/关 shields-up）。
  // 为什么把 funnel 也排除在外：总开关关闭时 stop_all 会 `funnel reset` 掉整份配置，
  // funnel 步随之回退未完成——若把它算进「已配置」，总开关一关就退回「配置中」，
  // 状态四永远不可达（正是本条评审要修的形态）。为什么不用「本通道开关是否开着」做
  // 门控：开关关掉时同样是「未运行」，而线稿状态四的第一行正是那一档成因。
  const tsSetupSteps = tsStates.filter((s) => s.id !== "funnel" && s.id !== "verify");
  const tsSetupDoneExceptFunnel =
    tsProbe !== null && tsSetupSteps.length > 0 && tsSetupSteps.every((s) => s.done === true);
  // W-A：**开机恢复窗口**（后端重连中）——独立相位，优先级仅次于「未安装」。
  // 实测时间线（真机重启逐秒取证）：T+58s 服务已 Running 但 BackendState=NoState、
  // `funnel status` 短暂为 {}；T+73s 配置逐字段自恢复；T+89s 公网仍 TLS 失败；
  // T+2.5min 两个入口 200。这窗口内若落进「未运行」（状态四）用户会去点开关重开、
  // 落进「配置中」会被向导步骤列表骗着一步步点——都必须避免；语义只能是「恢复中」。
  // 判据取后端下发的 reach.state（不是 error 文案匹配——文案会漂移、也不该被解析）。
  // **I3（2026-10-07 评审）**：必须**先过通道开关门**（`enabled`）——竞态窗口里载荷可能
  // 带着 `enabled=false ∧ reach=recovering`（后端已在载荷侧收口，这里是前端侧纵深防御）：
  // 只看 reach.state 就会把用户刚关掉的通道渲染成「恢复中……配置与地址都不会变，无需
  // 任何操作」，与既成事实背离。enabled=false 时落下面各态（未开启/未运行），成因由
  // offCause 三档给出。
  const tsRecovering = tsChannel?.enabled === true && tsChannel?.reach?.state === "recovering";
  const tsPhase: TsPhase =
    tsProbe === null
      ? "unknown"
      : !tsDetectDone
        ? "notInstalled"
        : tsRecovering
          ? "recovering"
          : tsAllStepsDone && tsReachVerified && tsRunning
            ? "configured"
            : !tsRunning && tsSetupDoneExceptFunnel
              ? "configuredIdle"
              : "configuring";

  // 探测时机（不做时刻轮询）：① 选中 tailscale 卡；② 通道快照跃迁——3s 轮询携带的
  // running/address/reach 任一翻转（键为字符串快照，轮询同值不重探）。两路覆盖
  // 「开通 → 运行 → 校验通过」的全部跃迁，卡面随真值收敛
  const tsRefetchKey = tsChannel
    ? `${tsChannel.running}|${tsChannel.address ?? ""}|${tsChannel.reach?.state ?? ""}`
    : "";
  useEffect(() => {
    if (selected !== "tailscale") return;
    void loadTsProbe();
  }, [selected, tsRefetchKey, loadTsProbe]);

  // 有界重探（4s，仅一小窗）：未安装态且向导已打开——一键配置各步完成会推进 detect
  // 判据（装好 CLI 即翻态），而此时 running/address/reach 尚未变化，上面两路探不到；
  // 翻入下一态即自动停表
  useEffect(() => {
    if (selected !== "tailscale" || !tsWizardOpen || tsPhase !== "notInstalled") return;
    const timer = setInterval(() => void loadTsProbe(), 4000);
    return () => clearInterval(timer);
  }, [selected, tsWizardOpen, tsPhase, loadTsProbe]);

  // 二维码内容 = 通道地址 + 密码参数（线稿：链接已含密码，扫码自动填入直接进入）
  const pin = typeof status?.pin === "string" ? status.pin : "";
  const withPin = (url: string) => (pin ? `${url}#pin=${pin}` : url);
  const qrCaption = `${t("settings.remote.qrTitle")} · ${t("settings.remote.qrAutoPin")}`;

  const copy = async (text: string) => {
    try {
      await navigator.clipboard.writeText(text);
      toast.success(t("settings.remote.copied"));
    } catch (e) {
      // 复制失败给用户可见反馈（与其它错误路径同一 toast 口径；A8 评审）
      console.error("clipboard write failed:", e);
      toast.error(t("settings.remote.copyFailed"));
    }
  };

  // 总开关（线稿：显性关闭 = 断开所有设备并要求重新输入访问密码，语义在行提示中）
  const changeEnabled = async (v: boolean) => {
    if (busy) return;
    setBusy(true);
    try {
      await remoteToggle(v);
      await refreshStatus();
    } catch (e) {
      toast.error(formatInvokeError(e, t));
    } finally {
      setBusy(false);
    }
  };

  // 保活落盘：写 "true"/"false"；失败 toast 且开关回弹（受控态未变）
  const changeKeepalive = async (v: boolean) => {
    try {
      await setSetting(KEEPALIVE_KEY, v ? "true" : "false");
      setKeepalive(v);
    } catch (e) {
      toast.error(formatInvokeError(e, t));
    }
  };

  // 设备签名开关落盘：on/off 两值（Rust 侧只认 "on"，缺省关）
  const changeSignature = async (v: boolean) => {
    try {
      await setSetting(SIGNATURE_KEY, v ? "on" : "off");
      setSignature(v);
    } catch (e) {
      toast.error(formatInvokeError(e, t));
    }
  };

  // 本机名落盘：blur 触发（不逐键写库）；空串原样写（后端回落系统名）
  const changeHostName = async (value: string) => {
    try {
      await setSetting(HOST_NAME_KEY, value);
    } catch (e) {
      toast.error(formatInvokeError(e, t));
    }
  };

  // B1+M-1：撤销 tailscale 通道的**收口路径**——`funnel reset` 清的是**整份** serve 配置：
  // 叠加形态（兔维斯 那路 + 用户自建 /media 等）下普通撤销**照常放行**，用户条目会被一起
  // 清掉。故撤销前先取只读预览，只要「有非 兔维斯 条目会被清」或「普通撤销会被守卫拒绝」
  // 就弹确认框**逐条列出**，用户点头后才走 `disable_force`
  //（我们刻意不加严守卫：加严 = 兔维斯 撤不掉自己的 Funnel = 公网暴露撤不掉，代价就是
  // 叠加形态下会多清条目——所以必须让用户知情）。
  // **M-1：弹框与不弹框两条路都经向导步并消费回执**——唯一能拿到「实际被清条目」的出口是
  // run_step 回执（走 remote_toggle_channel 那条路 stop_channel() 会把 ServeResetReport
  // 丢掉）：「N 条」与逐条列表都取自回执的条目表（**同源**），窗口内配置变了也不会出现
  // 「toast 说清了 2 条、列表只列得出预览时的 1 条」这种对不上的形态。
  const revokeTailscale = async (force: boolean) => {
    try {
      const r = await remoteTsRunStep(force ? "disable_force" : "disable");
      // 回执带条目（新后端）→ 以回执为准；只有条数的旧后端 → 退回预览条目（尽力而为，
      // 如实标注来源仍是"撤销前预览"）
      const entries =
        r?.clearedEntries ??
        (r?.clearedExtraServeEntries ? (tsPreview?.entries ?? []).filter((e) => !e.ours) : []);
      if (entries.length > 0) {
        setTsCleared({ entries });
        toast.error(t("settings.remote.tsClearedToast", { count: entries.length }));
      }
      setTsPreview(null);
      await refreshStatus();
    } catch (e) {
      toast.error(formatInvokeError(e, t));
    }
  };

  // 通道开关（线稿 tg()：开关独立、stopPropagation 不触发卡片选中）。
  // P7 门唯一口径在后端：lan 开启未确认 TLS 反代 → Err 特征文案 → 弹既有 TLS Dialog
  //（确认后重试）；其余失败 toast 原样透出。后端失败自回滚 KV，开关态以 status 为准
  const changeChannel = async (kind: ChannelKind, on: boolean) => {
    if (busy) return;
    setBusy(true);
    try {
      if (kind === "tailscale") {
        // M-1：重新开通 = 上一次撤销的"被清条目"回看该收起了（它只与那一次撤销有关）
        if (on) setTsCleared(null);
        else {
          // B1：先问「这次撤销会连带清掉什么」（只读预览，零写操作）
          const p = await remoteTsRunStep("disable_preview");
          const would = p?.wouldClear ?? 0;
          // W-A：unreadable（后端重连中读不到配置形态）**照 foreign 处理**——照旧读会
          // 把"读不到"当成"没有条目"而静默直接撤销，用户的知情同意就建立在假信息上
          if (p?.foreign || would > 0 || p?.unreadable) {
            setTsPreview({
              foreign: Boolean(p?.foreign),
              wouldClear: would,
              entries: p?.entries ?? [],
              unreadable: Boolean(p?.unreadable),
            });
            return; // 等用户在确认框里点头；没点头就不撤销（开关保持原态，由 status 真值决定）
          }
          // 预览说「没有连带清除」也走同一条出口（回执照样上墙——预览与撤销之间的窗口里
          // 配置若被改过，用户仍能看到实际清了什么）
          await revokeTailscale(false);
          return;
        }
      }
      await toggleChannel(kind, on);
      await refreshStatus(); // 开关即卡关：即时刷新 live 点与地址展示
    } catch (e) {
      if (kind === "lan" && on && isPublicAckRequired(e)) {
        setTlsOpen(true);
      } else {
        // 预览失败也走这里：读不到 Funnel 状态就**不撤销**（fail-closed，不静默跳过守卫）
        toast.error(formatInvokeError(e, t));
      }
    } finally {
      setBusy(false);
    }
  };

  // TLS 确认 + 重试（仅 lan 开启路径）：先 remote_confirm_public 置位 remote.public_ack
  //（后端 P7 门据此放行），再重试 toggle_channel("lan", true)。确认失败不关弹窗
  //（可就地重试）；重试失败回落 changeChannel 既有分流（P7 已放行，异常走 toast）
  const confirmTlsAndRetryLan = async () => {
    if (busy) return;
    try {
      await remoteConfirmPublic();
    } catch (e) {
      toast.error(formatInvokeError(e, t));
      return;
    }
    setTlsOpen(false);
    await changeChannel("lan", true);
  };

  // H3 无头总开关落盘（后端 remote_toggle_headless：写 KV + 审计 + 广播）。开关态以
  // status.headlessEnabled 为准（后端是唯一数据源），失败仅 toast 不回弹本地态
  const applyHeadless = async (v: boolean) => {
    if (busy) return;
    setBusy(true);
    try {
      await toggleHeadless(v);
      await refreshStatus();
    } catch (e) {
      toast.error(formatInvokeError(e, t));
    } finally {
      setBusy(false);
    }
  };

  // 开启方向：**首次**（未确认过）先弹一次性安全说明，确认后才真正开启；已确认过
  // （HEADLESS_ACK_KEY="true"）或关闭方向：直接落盘
  const changeHeadless = async (v: boolean) => {
    if (busy) return;
    if (v && !headlessAckRef.current) {
      setHeadlessNoticeOpen(true);
      return;
    }
    await applyHeadless(v);
  };

  // 安全说明确认：先记已读（写失败不阻断开启——说明已展示过），再开启并关弹窗
  const confirmHeadlessNotice = async () => {
    headlessAckRef.current = true;
    try {
      await setSetting(HEADLESS_ACK_KEY, "true");
    } catch {
      /* 记忆写失败不阻断开启（最坏下次再弹一次说明，不谎报已读以外的事） */
    }
    setHeadlessNoticeOpen(false);
    await applyHeadless(true);
  };

  // Token 保存（A6 落点：通用 set_setting）；开关由自有域名卡片开关驱动，后端
  // start_channel 对空 Token 拒启并写快照错误（「自有域名缺少 Tunnel Token」）
  const saveToken = async () => {
    try {
      await setSetting(TUNNEL_TOKEN_KEY, token);
      toast.success(t("settings.remote.tokenSaved"));
    } catch (e) {
      toast.error(formatInvokeError(e, t));
    }
  };

  // H4（Task 6）无头配置保存：后端 remote_set_headless_limits（越界 clamp 后落 KV +
  // 审计 + 广播 remote-changed）。失败仅 toast、不回弹本地值（后端未变，可再点一次）；
  // 空/非法输入交给后端 clamp（前端只做数字框约束，口径单点在后端）
  const saveHeadlessLimits = async () => {
    if (busy) return;
    setBusy(true);
    try {
      await setHeadlessLimits(
        Number(headlessTimeout) || DEFAULT_HEADLESS_TIMEOUT_MS,
        Number(headlessConcurrency) || DEFAULT_HEADLESS_CONCURRENCY
      );
      toast.success(t("settings.remote.tokenSaved"));
      await refreshStatus();
    } catch (e) {
      toast.error(formatInvokeError(e, t));
    } finally {
      setBusy(false);
    }
  };

  // 随机密码（线稿 randPin：1000-9999 四位数字）
  const randomPin = () => {
    setPinInput(String(Math.floor(1000 + Math.random() * 9000)));
  };

  // 密码保存：改值 = 后端全设备吊销 + 断连（「修改后所有设备需重新输入」语义）
  const savePin = async () => {
    try {
      await setPin(pinInput);
      toast.success(t("settings.remote.pinSaved"));
      await refreshStatus();
    } catch (e) {
      toast.error(formatInvokeError(e, t));
    }
  };

  // 重置设备（二次确认后）：吊销全部 + 断连，不改 PIN
  const doResetDevices = async () => {
    try {
      await resetDevices();
      setResetOpen(false);
      await refreshDevices();
    } catch (e) {
      toast.error(formatInvokeError(e, t));
    }
  };

  const startRename = (d: RemoteDevice) => {
    setEditingId(d.id);
    setEditName(d.name ?? "");
  };

  const saveRename = async (id: string) => {
    try {
      await renameDevice(id, editName);
      setEditingId(null);
      await refreshDevices();
    } catch (e) {
      toast.error(formatInvokeError(e, t));
    }
  };

  // 踢下线（线稿口径的单设备吊销）
  const kick = async (id: string) => {
    try {
      await remoteRevokeDevice(id);
      await refreshDevices();
    } catch (e) {
      toast.error(formatInvokeError(e, t));
    }
  };

  // 详情区数据快照（缺失键安全兜底——旧后端/异常载荷不致渲染崩溃）。
  // 「本机」不再有地址行（零豁免 §C4：本机访问走局域网卡地址，local 段不进 UI）
  const lanAddrs = channels?.lan?.addresses ?? [];
  const quickAddr = channels?.quick?.address ?? null;
  const quickErr = channels?.quick?.error ?? null;
  const namedAddr = channels?.named?.address ?? null;
  const namedErr = channels?.named?.error ?? null;
  const namedRunning = chanRunning("named");
  // M5 P2-c：手填地址（已保存值）与自动记忆的上次地址（解析失败时的显示兜底）
  const namedLastAddr = channels?.named?.lastAddr ?? null;
  // 命名卡片展示优先级：解析地址 > 手填 > 上次地址（三者皆无 = 从未配置）
  const namedDisplayAddr = namedAddr ?? namedLastAddr;
  const tsAddr = tsChannel?.address ?? null;
  const tsErr = tsChannel?.error ?? null;

  // 通道卡定义（线稿 v6 顺序：局域网连接 / 临时隧道 / 自有域名 / 外部域名；
  // 「本机」不再是卡——它是访问方式，走局域网卡地址，见 lan 展开区的未开启提示）
  const cardDefs: Array<{ key: ChannelKind; name: string; desc: string }> = [
    { key: "lan", name: t("settings.remote.chanLan"), desc: t("settings.remote.chanLanDesc") },
    {
      key: "quick",
      name: t("settings.remote.chanQuick"),
      desc: t("settings.remote.chanQuickDesc"),
    },
    {
      key: "named",
      name: t("settings.remote.chanNamed"),
      desc: t("settings.remote.chanNamedDesc"),
    },
    {
      key: "tailscale",
      name: t("settings.remote.chanTailscale"),
      desc: t("settings.remote.chanTailscaleDesc"),
    },
  ];

  return (
    <div>
      <h2 className={SETTINGS_CARD_TITLE}>{t("settings.remote.title")}</h2>
      <p className={`mb-1 ${SETTINGS_SUBTITLE}`}>{t("settings.remote.desc")}</p>

      {/* ① 通用 */}
      <div className="text-muted-foreground mt-4 text-[12.5px] font-semibold">
        {t("settings.remote.groupGeneral")}
      </div>
      <div className="flex items-center justify-between gap-4 py-3">
        <div className="flex-1">
          <label className="text-sm font-semibold">{t("settings.remote.enable")}</label>
          <p className="text-muted-foreground mt-0.5 text-xs">
            {t("settings.remote.enableCloseHint")}
          </p>
        </div>
        <Switch
          aria-label={t("settings.remote.enable")}
          checked={enabled}
          disabled={busy || !status}
          onCheckedChange={(v) => void changeEnabled(v)}
        />
      </div>
      <div className="flex items-center justify-between gap-4 py-3">
        <div className="flex-1">
          <label className="text-sm font-semibold">{t("settings.remote.keepalive")}</label>
          <p className="text-muted-foreground mt-0.5 text-xs">
            {t("settings.remote.keepaliveHint")}
          </p>
        </div>
        <Switch
          aria-label={t("settings.remote.keepalive")}
          checked={keepalive}
          disabled={!status}
          onCheckedChange={(v) => void changeKeepalive(v)}
        />
      </div>
      <div className="flex items-center justify-between gap-4 py-3">
        <div className="flex-1">
          <label className="text-sm font-semibold">{t("settings.remote.signature")}</label>
          <p className="text-muted-foreground mt-0.5 text-xs">
            {t("settings.remote.signatureHint")}
          </p>
        </div>
        <Switch
          aria-label={t("settings.remote.signature")}
          checked={signature}
          onCheckedChange={(v) => void changeSignature(v)}
        />
      </div>
      <div className="flex items-center justify-between gap-4 py-3">
        <label htmlFor="remote-host-name" className="flex-none text-sm font-semibold">
          {t("settings.remote.hostName")}
        </label>
        <Input
          id="remote-host-name"
          value={hostName}
          className="max-w-64"
          onChange={(e) => setHostName(e.target.value)}
          onBlur={(e) => void changeHostName(e.target.value)}
        />
      </div>

      {/* ② 通道（开关各自独立，可同时开启；点卡片在下方展开详情） */}
      <div className="text-muted-foreground mt-4 text-[12.5px] font-semibold">
        {t("settings.remote.groupChannels")}
      </div>
      <div className="mt-2 grid grid-cols-4 gap-2">
        {cardDefs.map((c) => {
          const sel = selected === c.key;
          return (
            <div
              key={c.key}
              data-card={c.key}
              role="button"
              tabIndex={0}
              aria-pressed={sel}
              onClick={() => setSelected(c.key)}
              onKeyDown={(e) => {
                if (e.key === "Enter" || e.key === " ") setSelected(c.key);
              }}
              className={cn(
                "cursor-pointer rounded-[10px] border-[1.5px] p-2.5 transition-colors outline-none",
                sel
                  ? "border-foreground shadow-[0_0_0_2px_rgba(15,23,42,0.07)] dark:shadow-[0_0_0_2px_rgba(255,255,255,0.09)]"
                  : "border-border hover:border-muted-foreground/60"
              )}
            >
              <div className="flex items-center justify-between gap-1.5">
                <span className="flex min-w-0 items-center gap-1 text-[13px] font-bold">
                  <span
                    data-live
                    className={cn(
                      "inline-block h-[7px] w-[7px] flex-none rounded-full",
                      chanRunning(c.key) ? "bg-emerald-500" : "bg-gray-300"
                    )}
                  />
                  {/* ①（2026-10-07 用户实测 + 裁决）：卡位标题可用宽只有约 94px（880 窗口 −
                      侧栏 160 − p-4 − 4 列 gap − 卡内边距 − 开关 32 − 点与间隙 11），
                      `truncate` 会把它切掉——用户实测当时 zh「外部域名（免域名）」被切成
                      「外部域名（免…」。用户裁决 =「那个括号不用，这 4 个字就挺好的，其他
                      注释写在下面」⇒ 卡名收成 4 字（i18n chanTailscale），免域名那层信息
                      由下方备注 chanTailscaleDesc 承载。`title` 是兜底：将来某语言（或更窄
                      的窗口）仍超宽时，悬停可见全名——兜底不等于可以把标题写长。 */}
                  <span className="truncate" title={c.name}>
                    {c.name}
                  </span>
                </span>
                {/* 开关独立于卡片选中（线稿 stopPropagation）；四卡同式，启停唯一
                    入口 = remote_toggle_channel（后端幂等 + 失败回滚）。开关态只认
                    enabled——它与状态点（running）刻意可以不同：开关 ON + 点灰 =
                    「已开通道但服务没在监听」，这正是用户要看见的成因（评审 C-I1） */}
                <span className="flex-none" onClick={(e) => e.stopPropagation()}>
                  <Switch
                    checked={chanEnabled(c.key)}
                    disabled={busy || !status}
                    aria-label={c.name}
                    onCheckedChange={(v) => void changeChannel(c.key, v)}
                  />
                </span>
              </div>
              <p className="text-muted-foreground mt-1 min-h-[33px] text-[11px] leading-snug">
                {c.desc}
              </p>
            </div>
          );
        })}
      </div>

      {/* 唯一展开详情区（线稿 pick()：同一时刻只显示一个；「本机」没有展开块——
          零豁免 §C4 后本机访问走局域网卡地址） */}
      <div data-expand={selected} className="bg-muted/30 mt-2.5 rounded-xl border p-4 text-sm">
        {selected === "lan" && (
          <>
            {/* 地址/复制/推荐徽标只在 running 时宣称（评审 C-I1 + 线稿状态二/三：
                「地址与二维码此时不渲染」）。后端 lan.addresses 不受 running/总开关
                约束（channels_payload 原样透传 lan_urls_for），故门必须在前端——
                服务没在监听时给了地址就是死链 */}
            {lanRunning && lanAddrs.length > 0 && (
              <div className="flex flex-col gap-1.5">
                {lanAddrs.map((u, i) => (
                  <div key={u} className="flex flex-wrap items-center gap-2">
                    {i === 0 && <Badge tone="green">{t("settings.remote.badgeRecommended")}</Badge>}
                    <code className="bg-muted rounded-md px-2 py-0.5 font-mono text-[12.5px] break-all">
                      {u}
                    </code>
                    <Button variant="outline" size="sm" onClick={() => void copy(u)}>
                      {t("settings.remote.copy")}
                    </Button>
                  </div>
                ))}
              </div>
            )}
            {/* !running 一律显式呈现成因（本机/局域网都进不去的原因要可见，零豁免
                §C4）：徽标区分「未开启（通道开关关）」与「未运行（服务没在监听）」，
                文案指向上方总开关或本通道开关（评审 C-I1 两因分述）。
                status 未达时不判定成因——载荷未知，不猜 */}
            {status && !lanRunning && (
              <div className="mt-1 flex flex-wrap items-center gap-2">
                <Badge tone="gray">{offBadge("lan")}</Badge>
                <span className="text-muted-foreground text-xs">{offHint("lan")}</span>
              </div>
            )}
            {/* 本机访问口径（线稿 lan 展开区提示行，恒在） */}
            <p className="text-muted-foreground mt-2 text-xs">
              {t("settings.remote.lanLocalHint")}
            </p>
            <div className="mt-3 flex items-start gap-6">
              {/* 二维码与地址同门：!running 不渲染（死链不宣称） */}
              {lanRunning && lanAddrs[0] && (
                <ChannelQr url={withPin(lanAddrs[0])} caption={qrCaption} />
              )}
              <p className="text-muted-foreground flex-1 text-xs">
                {t("settings.remote.lanDetailHint")}
              </p>
            </div>
          </>
        )}
        {selected === "quick" && (
          <>
            <div className="flex flex-wrap items-center gap-2">
              <Badge tone="blue">{t("settings.remote.badgePublic")}</Badge>
              {/* 地址与复制只在 running 时宣称（与 lan 同门：载荷契约本身也以 running
                  为门，前端再挡一道，避免毫秒级陈旧快照被当活链展示） */}
              {quickRunning && quickAddr ? (
                <>
                  <code className="bg-muted rounded-md px-2 py-0.5 font-mono text-[12.5px] break-all">
                    {quickAddr}
                  </code>
                  <Button variant="outline" size="sm" onClick={() => void copy(quickAddr)}>
                    {t("settings.remote.copy")}
                  </Button>
                </>
              ) : // 运行中但地址尚未解析（cloudflared 启动/重试窗口）：占位提示而非
              // 空白或旧值——脏/旧地址 + 「已获取」toast 的误导组合已在解析器侧治理
              quickRunning && !quickErr ? (
                <span
                  data-testid="quick-fetching"
                  className="text-muted-foreground animate-pulse text-xs"
                >
                  {t("settings.remote.quickFetching")}
                </span>
              ) : null}
            </div>
            <div className="mt-2 flex flex-wrap items-center gap-2">
              {quickRunning ? (
                <>
                  <Badge tone="green">{t("settings.remote.badgeRunning")}</Badge>
                  <span className="text-muted-foreground text-xs">
                    {t("settings.remote.autoReconnect")}
                  </span>
                </>
              ) : (
                // 评审 C-I1：enabled 但未运行时旧版只剩一个「公网」徽标、无地址无说明
                // ——用户看不出为什么进不去。成因分档文案与 lan 同一套
                status && (
                  <>
                    <Badge tone="gray">{offBadge("quick")}</Badge>
                    <span className="text-muted-foreground text-xs">{offHint("quick")}</span>
                  </>
                )
              )}
              {quickErr && <span className="text-xs text-amber-500">{quickErr}</span>}
            </div>
            {/* 换址警告块（线稿 .warn，逐字） */}
            <p className="mt-2 rounded-lg bg-amber-50 px-2.5 py-1.5 text-xs text-amber-600 dark:bg-amber-500/10 dark:text-amber-400">
              {t("settings.remote.quickWarn")}
            </p>
            <div className="mt-3 flex items-start gap-6">
              {quickRunning && quickAddr && (
                <ChannelQr url={withPin(quickAddr)} caption={qrCaption} />
              )}
              <p className="text-muted-foreground flex-1 text-xs">
                {t("settings.remote.quickDetailHint")}
              </p>
            </div>
          </>
        )}
        {selected === "named" && (
          <>
            <div className="flex items-start justify-between gap-4">
              <div className="min-w-0 flex-1">
                <div className="text-[12.5px] font-semibold">
                  {t("settings.remote.tunnelToken")}{" "}
                  {/* 教程 popover（线稿 .help：点击展开/收起，可保持展开边看边操作） */}
                  <span className="relative inline-block align-baseline">
                    <span
                      role="button"
                      tabIndex={0}
                      aria-expanded={helpOpen}
                      data-help={helpOpen ? "open" : "closed"}
                      className="cursor-pointer border-b border-dotted border-blue-500 text-xs text-blue-500"
                      onClick={() => setHelpOpen((v) => !v)}
                      onKeyDown={(e) => {
                        if (e.key === "Enter" || e.key === " ") setHelpOpen((v) => !v);
                      }}
                    >
                      {t("settings.remote.tutorialToggle")}
                    </span>
                    {helpOpen && (
                      <div className="bg-background absolute top-6 left-[-40px] z-30 w-[420px] rounded-xl border p-3.5 text-xs shadow-lg">
                        <p className="font-semibold">{t("settings.remote.tutorialPre")}</p>
                        <ol className="mt-1.5 list-decimal space-y-1.5 pl-4">
                          <li>{t("settings.remote.tutorialStep1")}</li>
                          <li>{t("settings.remote.tutorialStep2")}</li>
                          <li>{t("settings.remote.tutorialStep3")}</li>
                          <li>{t("settings.remote.tutorialStep4")}</li>
                          <li>{t("settings.remote.tutorialStep5")}</li>
                          <li>{t("settings.remote.tutorialStep6")}</li>
                        </ol>
                        <p className="text-muted-foreground mt-2">
                          {t("settings.remote.tutorialPost")}
                        </p>
                        {/* 官方图文兜底：Cloudflare 面板 UI 迭代快，应用内步骤以简版
                            为主，细节引导到官方文档（2026-09-20 用户裁决） */}
                        <a
                          href="https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/get-started/create-remote-tunnel/"
                          target="_blank"
                          rel="noreferrer"
                          className="mt-1.5 inline-block text-xs text-blue-500 underline"
                        >
                          {t("settings.remote.tutorialDocs")}
                        </a>
                      </div>
                    )}
                  </span>
                </div>
                <p className="text-muted-foreground mt-0.5 text-xs">
                  {t("settings.remote.tokenHint")}
                </p>
              </div>
              <div className="flex max-w-[300px] flex-none gap-2">
                <Input
                  id="remote-tunnel-token"
                  type="password"
                  value={token}
                  aria-label={t("settings.remote.tunnelToken")}
                  className="flex-1"
                  onChange={(e) => setToken(e.target.value)}
                />
                <Button size="sm" onClick={() => void saveToken()}>
                  {t("settings.remote.save")}
                </Button>
              </div>
            </div>
            <div className="mt-3 flex flex-wrap items-center gap-2">
              <Badge tone="blue">{t("settings.remote.badgePublic")}</Badge>
              {/* M5 P2-c：显示优先级 解析地址 > 手填 > 上次地址（灰字标注来源）。
                  B3 判据原则：**用户自己的/永久的地址保留显示**（未运行也留着，用户要能
                  复制存书签）——但下面必须紧跟运行态徽标，绝不宣称可用 */}
              {namedDisplayAddr ? (
                <code className="bg-muted rounded-md px-2 py-0.5 font-mono text-[12.5px] break-all">
                  {namedDisplayAddr}
                  {!namedAddr && namedDisplayAddr !== namedAddr && (
                    <span className="text-muted-foreground ml-1 font-sans text-[10px]">
                      {t("settings.remote.addrFallback")}
                    </span>
                  )}
                </code>
              ) : null}
              {namedDisplayAddr && (
                <Button variant="outline" size="sm" onClick={() => void copy(namedDisplayAddr)}>
                  {t("settings.remote.copy")}
                </Button>
              )}
              {namedErr && <span className="text-xs text-amber-500">{namedErr}</span>}
            </div>
            {/* 运行态行（B3，与 lan/quick 同一套 offCause 口径）：永久地址卡也必须有 */}
            <div className="mt-2 flex flex-wrap items-center gap-2">
              {namedRunning ? (
                <>
                  <Badge tone="green">{t("settings.remote.badgeRunning")}</Badge>
                  <span className="text-muted-foreground text-xs">
                    {t("settings.remote.namedRunningHint")}
                  </span>
                </>
              ) : (
                status && (
                  <>
                    <Badge tone="gray">{offBadge("named")}</Badge>
                    <span className="text-muted-foreground text-xs">{offHint("named")}</span>
                  </>
                )
              )}
            </div>
            {/* 二维码与「扫码即可用」同门：未运行不渲染（死链不宣称；地址本身保留） */}
            {namedRunning && namedDisplayAddr && (
              <div className="mt-3">
                <ChannelQr url={withPin(namedDisplayAddr)} caption={qrCaption} />
              </div>
            )}
          </>
        )}
        {selected === "tailscale" && (
          <div data-testid="ts-phase" data-phase={tsPhase}>
            {/* B1 卡面呈现：撤销时被一并清除的非 兔维斯 条目（不静默——用户必须能回看
                「我的 /media 那次是被谁清的」） */}
            {tsCleared && (
              <div
                data-testid="ts-cleared-notice"
                className="mb-2 rounded-lg bg-amber-50 px-2.5 py-1.5 text-xs text-amber-600 dark:bg-amber-500/10 dark:text-amber-400"
              >
                <p>{t("settings.remote.tsClearedNotice", { count: tsCleared.entries.length })}</p>
                {tsCleared.entries.length > 0 && (
                  <ul className="mt-1 list-disc space-y-0.5 pl-4">
                    {tsCleared.entries.map((e) => (
                      <li key={e.label}>
                        <code className="font-mono">{e.label}</code>
                      </li>
                    ))}
                  </ul>
                )}
              </div>
            )}
            {tsPhase === "notInstalled" && (
              <>
                <div className="flex flex-wrap items-center gap-2">
                  <Badge tone="gray">{t("settings.remote.tsStateNotInstalled")}</Badge>
                </div>
                {/* 价值主张 + 一键配置（线稿 p-ext 状态一；线稿把状态一~五同屏陈列是示意，
                    实现按 probe 真值单态呈现） */}
                <p className="mt-2 text-[12.5px]">{t("settings.remote.tsPitch")}</p>
                <div className="mt-2 flex flex-wrap items-center gap-2">
                  <Button
                    size="sm"
                    data-testid="ts-one-click"
                    onClick={() => setTsWizardOpen(true)}
                  >
                    {t("settings.remote.tsOneClick")}
                  </Button>
                  <span className="text-muted-foreground text-xs">
                    {t("settings.remote.tsWizardPath")}
                  </span>
                </div>
                {tsWizardOpen && (
                  <div className="bg-background mt-3 rounded-lg border p-3">
                    <TailscaleWizard />
                  </div>
                )}
              </>
            )}
            {/* W-A：开机恢复中（状态五，线稿 p-ext 六次回写）——**既不是故障也不是
                「未运行」**：重启后 tailscaled 有 1–2 分钟恢复窗口（实测 T+58s 服务已
                Running 但后端 NoState、T+73s 配置逐字段自恢复、T+2.5min 公网 200）。
                地址与二维码不渲染（§C3 三门：未验证不宣称可用），安抚句恒在
                （「地址固定不变」——本功能的核心承诺）。**不给「重开」入口**：
                配置没丢，重开只会打断 tailscaled 自己的恢复。 */}
            {tsPhase === "recovering" && (
              <>
                <div
                  data-testid="ts-recovering"
                  className="flex flex-wrap items-center gap-2 rounded-lg bg-amber-50 px-2.5 py-1.5 dark:bg-amber-500/10"
                >
                  <Badge tone="amber">{t("settings.remote.tsRecoveringBadge")}</Badge>
                  <span className="text-xs text-amber-600 dark:text-amber-400">
                    {t("settings.remote.tsRecoveringHint")}
                  </span>
                </div>
                <p data-testid="ts-addr-permanent" className="text-muted-foreground mt-1 text-xs">
                  {t("settings.remote.tsAddrPermanentHint")}
                </p>
              </>
            )}
            {tsPhase === "configuring" && (
              <>
                <div className="flex flex-wrap items-center gap-2">
                  <Badge tone="violet">{t("settings.remote.tsStateConfiguring")}</Badge>
                  <span className="text-muted-foreground text-xs">
                    {t("settings.remote.tsProgressHint")}
                  </span>
                </div>
                {/* 向导进度 = remote_ts_probe 的平台步骤表数据（每步三态 + 卡在哪一步
                    + blocked_reason），§C2 步骤表是数据不写死——不照抄线稿 5 行示意 */}
                <div className="mt-2">
                  <TailscaleWizard />
                </div>
              </>
            )}
            {tsPhase === "configured" && (
              <>
                <div className="flex flex-wrap items-center gap-2">
                  <Badge tone="green">{t("settings.remote.tsStateConfigured")}</Badge>
                  {/* 地址行 = badge + code.url + 复制（线稿 .addr 统一形态）；
                      reach 已由相位判定门控为 Verified，address 仍只在载荷宣称时渲染 */}
                  {tsAddr && (
                    <>
                      <Badge tone="blue">{t("settings.remote.badgePublic")}</Badge>
                      <code className="bg-muted rounded-md px-2 py-0.5 font-mono text-[12.5px] break-all">
                        {tsAddr}
                      </code>
                      <Button variant="outline" size="sm" onClick={() => void copy(tsAddr)}>
                        {t("settings.remote.copy")}
                      </Button>
                    </>
                  )}
                </div>
                <div className="mt-2 flex flex-wrap items-center gap-2">
                  {/* 状态三 = 通道在运行（相位机已把 tsRunning 收进 configured 判据），
                      故这里恒为「运行中」——不再在同一个块里留一条「未运行 + 成因」的
                      分支：那条分支在旧相位机下永远渲染不到（I-1），成因行已移到下面
                      configuredIdle（状态四）里 */}
                  <Badge tone="green">{t("settings.remote.badgeRunning")}</Badge>
                  <span className="text-muted-foreground text-xs">
                    {t("settings.remote.tsRunningHint")}
                  </span>
                  {tsErr && <span className="text-xs text-amber-500">{tsErr}</span>}
                </div>
                {tsAddr && (
                  <div className="mt-3">
                    <ChannelQr url={withPin(tsAddr)} caption={qrCaption} />
                  </div>
                )}
              </>
            )}
            {tsPhase === "configuredIdle" && (
              <>
                {/* 状态四（线稿 p-ext 第四块「通道未运行」）：成因三档（与 lan/quick/named
                    同一套 offCause）+ 快照错误如实透出；**地址与二维码不渲染**（§C3 三门：
                    未运行就不宣称可用；线稿对 ts 卡写的也是这个例外——永久地址的「保留
                    显示」以「拿得到地址」为前提）。status 未达时不判定成因（不猜） */}
                {status && (
                  <div className="flex flex-wrap items-center gap-2">
                    <Badge tone="gray">{offBadge("tailscale")}</Badge>
                    <span className="text-muted-foreground text-xs">{offHint("tailscale")}</span>
                  </div>
                )}
                {tsErr && (
                  <div className="mt-1 flex flex-wrap items-center gap-2">
                    <span className="text-xs text-amber-500">{tsErr}</span>
                    {/* **M1（2026-10-07 评审 Minor）升级路径**：宽限窗过后仍连不通 /
                        后端持续未就绪（有界降级为 Failed，见 reach::BACKEND_INIT_GRACE）
                        时，本态必须给出**向导重试入口**——线稿状态五第 2 行承诺
                        「宽限窗过后 → 回到状态四，由向导的『看板可达性校验』行如实点名
                        成因 + 给出重试」。只有真的带 error 时才挂（无 error 的纯「未运行」
                        不给这个入口，避免把正常停机渲染成故障待修） */}
                    <Button
                      variant="outline"
                      size="sm"
                      className="h-6 px-2 text-xs"
                      data-testid="ts-wizard-retry-entry"
                      onClick={() => setTsWizardOpen(true)}
                    >
                      {t("settings.remote.tsRetryEntry")}
                    </Button>
                  </div>
                )}
                {tsWizardOpen && tsErr && (
                  <div className="bg-background mt-3 rounded-lg border p-3">
                    <TailscaleWizard />
                  </div>
                )}
                {/* M-5（用户裁决：**只加文案，不改逻辑**）：本态不渲染地址（§C3 三门），
                    而本功能的核心承诺是「获得一个固定的链接」——地址一消失，用户就会怀疑
                    「这链接到底是不是固定的、下次开还是不是这个」。故补一句安抚，恒在
                    （与上面成因档位无关）；运行中态不挂这句（地址已上墙，那一态由
                    tsRunningHint 承担），多挂就是噪音。线稿 p-ext 状态四已同步（五次回写） */}
                <p data-testid="ts-addr-permanent" className="text-muted-foreground mt-1 text-xs">
                  {t("settings.remote.tsAddrPermanentHint")}
                </p>
              </>
            )}
            {/* probe 未达（加载中/失败）：不猜相位——向导本体自带加载脉冲/读取失败行 */}
            {tsPhase === "unknown" && <TailscaleWizard />}
          </div>
        )}
      </div>

      {/* ③ 访问与安全 */}
      <div className="text-muted-foreground mt-4 text-[12.5px] font-semibold">
        {t("settings.remote.groupSecurity")}
      </div>
      <div className="flex items-center justify-between gap-4 py-3">
        <div className="flex-1">
          <label htmlFor="remote-pin" className="text-sm font-semibold">
            {t("settings.remote.pinTitle")}
          </label>
          <p className="text-muted-foreground mt-0.5 text-xs">{t("settings.remote.pinHint")}</p>
        </div>
        <div className="flex flex-none items-center gap-2.5">
          <Input
            id="remote-pin"
            inputMode="numeric"
            maxLength={4}
            placeholder="0000"
            value={pinInput}
            onChange={(e) => setPinInput(e.target.value.replace(/\D/g, "").slice(0, 4))}
            className="w-28 text-center font-mono text-lg font-bold tracking-[0.3em]"
          />
          <Button variant="outline" size="sm" onClick={randomPin}>
            {t("settings.remote.pinRandom")}
          </Button>
          <Button size="sm" onClick={() => void savePin()} disabled={!/^\d{4}$/.test(pinInput)}>
            {t("settings.remote.save")}
          </Button>
          <Button
            variant="ghost"
            size="sm"
            className="text-rose-600 hover:text-rose-600"
            onClick={() => setResetOpen(true)}
          >
            {t("settings.remote.resetDevices")}
          </Button>
        </div>
      </div>
      <div className="py-3">
        <div className="flex items-center gap-2">
          <span className="text-sm font-semibold">{t("settings.remote.devicesTitle")}</span>
          {/* 上限徽标：上限来自 remote_status.maxDevices（KV 可改，默认 10），不再硬编码 */}
          <Badge tone="gray">{`${devices.length} / ${maxDevices}`}</Badge>
        </div>
        <p className="text-muted-foreground mt-0.5 text-xs">{t("settings.remote.devicesHint")}</p>
        {devices.length === 0 ? (
          <p className="text-muted-foreground mt-2 text-xs">{t("settings.remote.rosterEmpty")}</p>
        ) : (
          <ul className="mt-1">
            {devices.map((d) => (
              <li
                key={d.id}
                className="flex flex-wrap items-center gap-2.5 py-2 text-[13px] [&:not(:last-child)]:border-b [&:not(:last-child)]:border-dashed"
              >
                <span
                  className={cn(
                    "h-2 w-2 flex-none rounded-full",
                    d.online ? "bg-green-500" : "bg-gray-300"
                  )}
                />
                {editingId === d.id ? (
                  <>
                    <Input
                      aria-label={t("settings.remote.rename")}
                      value={editName}
                      onChange={(e) => setEditName(e.target.value)}
                      className="h-7 w-44 px-2 text-xs"
                    />
                    <Button size="sm" onClick={() => void saveRename(d.id)}>
                      {t("settings.remote.save")}
                    </Button>
                  </>
                ) : (
                  <>
                    <span className="max-w-48 truncate">{d.name || d.id.slice(0, 8)}</span>
                    <ViaBadge via={d.via} />
                    <span className="text-muted-foreground text-xs">
                      {d.online && (
                        <span className="text-emerald-600">
                          {t("settings.remote.rosterOnline")}
                        </span>
                      )}
                      {d.online ? " · " : ""}
                      {lastSeenLabel(d.lastSeenAt, t)}
                    </span>
                    <Button variant="outline" size="sm" onClick={() => startRename(d)}>
                      {t("settings.remote.rename")}
                    </Button>
                  </>
                )}
                <Button
                  variant="outline"
                  size="sm"
                  className="ml-auto text-rose-600 hover:text-rose-600"
                  onClick={() => void kick(d.id)}
                >
                  {t("settings.remote.kick")}
                </Button>
              </li>
            ))}
          </ul>
        )}
      </div>
      {/* 底部安全警示（线稿：地址+密码即钥匙勿外传；连错 5 次锁 10 分钟） */}
      <p className="mt-1 rounded-lg bg-amber-50 px-2.5 py-1.5 text-xs text-amber-600 dark:bg-amber-500/10 dark:text-amber-400">
        {t("settings.remote.notice")}
      </p>

      {/* ④ 无头注入（H3 / 裁决 9-10）：**单一总开关**（不做每工具分开关），默认关；
          开启前弹一次性安全说明。spec H4 的 watchdog 超时 / 并发上限两件随 Task 6 的
          「无头」子区控件一并落在本分组内（本任务只放开关） */}
      <div className="text-muted-foreground mt-4 text-[12.5px] font-semibold">
        {t("settings.remote.groupHeadless")}
      </div>
      <div className="flex items-center justify-between gap-4 py-3">
        <div className="flex-1">
          <label className="text-sm font-semibold">{t("settings.remote.headlessTitle")}</label>
          <p className="text-muted-foreground mt-0.5 text-xs">
            {t("settings.remote.headlessHint")}
          </p>
        </div>
        <Switch
          aria-label={t("settings.remote.headlessTitle")}
          checked={headlessEnabled}
          disabled={busy || !status}
          onCheckedChange={(v) => void changeHeadless(v)}
        />
      </div>

      {/* H4（Task 6）：「无头」子区三件套之另两件——watchdog 超时 + 全局并发上限
          （spec H4「配置落点」：三件同住本分组，不另开页面）。数据源 = remote_status
          的同名两键（后端 clamp 后落 KV + 审计 + 广播），旧后端缺键 → 文档默认值 */}
      <div
        data-headless-limits
        className="flex flex-wrap items-start gap-4 border-t border-dashed py-3"
      >
        <div className="min-w-[190px] flex-1">
          <label htmlFor="headless-timeout" className="text-sm font-semibold">
            {t("settings.remote.headlessTimeoutLabel")}
          </label>
          <Input
            id="headless-timeout"
            type="number"
            min={1000}
            max={3600000}
            step={1000}
            value={headlessTimeout}
            onChange={(e) => setHeadlessTimeout(e.target.value)}
            className="mt-1"
          />
          <p className="text-muted-foreground mt-0.5 text-xs">
            {t("settings.remote.headlessTimeoutHint")}
          </p>
        </div>
        <div className="min-w-[160px] flex-1">
          <label htmlFor="headless-concurrency" className="text-sm font-semibold">
            {t("settings.remote.headlessConcurrencyLabel")}
          </label>
          <Input
            id="headless-concurrency"
            type="number"
            min={1}
            max={8}
            step={1}
            value={headlessConcurrency}
            onChange={(e) => setHeadlessConcurrency(e.target.value)}
            className="mt-1"
          />
          <p className="text-muted-foreground mt-0.5 text-xs">
            {t("settings.remote.headlessConcurrencyHint")}
          </p>
        </div>
        <Button
          className="mt-6"
          aria-label={t("settings.remote.headlessLimitsSave")}
          disabled={busy || !status}
          onClick={() => void saveHeadlessLimits()}
        >
          {t("settings.remote.save")}
        </Button>
      </div>

      {/* H3 一次性安全说明（开启动作首次触发；确认 = 记已读 + 开启，取消仅关弹窗、
          不调后端——同 TLS Dialog 的既有交互口径） */}
      <Dialog open={headlessNoticeOpen} onOpenChange={setHeadlessNoticeOpen}>
        <DialogContent className="max-w-md">
          <DialogHeader>
            <DialogTitle>{t("settings.remote.headlessTitle")}</DialogTitle>
            <DialogDescription>{t("settings.remote.headlessConfirm")}</DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setHeadlessNoticeOpen(false)}>
              {t("settings.remote.cancel")}
            </Button>
            <Button onClick={() => void confirmHeadlessNotice()} disabled={busy}>
              {t("settings.remote.tlsDialogConfirm")}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* 重置设备二次确认弹窗（对齐仓库既有 Dialog 组件） */}
      <Dialog open={resetOpen} onOpenChange={setResetOpen}>
        <DialogContent className="max-w-md">
          <DialogHeader>
            <DialogTitle>{t("settings.remote.resetTitle")}</DialogTitle>
            <DialogDescription>{t("settings.remote.resetDesc")}</DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setResetOpen(false)}>
              {t("settings.remote.cancel")}
            </Button>
            <Button variant="destructive" onClick={() => void doResetDevices()}>
              {t("settings.remote.resetConfirm")}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* B1 撤销确认弹窗：逐条列出将随 `funnel reset` 一并清除的 serve 条目，
          用户确认后才走 disable_force。刻意不加严后端守卫（加严 = 公开暴露撤不掉），
          代价就是叠加形态下会多清条目——所以知情同意必须在这里做实。 */}
      <Dialog
        open={tsPreview !== null}
        onOpenChange={(o) => {
          if (!o) setTsPreview(null);
        }}
      >
        <DialogContent className="max-w-md" data-testid="ts-disable-confirm">
          <DialogHeader>
            <DialogTitle>{t("settings.remote.tsDisableTitle")}</DialogTitle>
            <DialogDescription>
              {tsPreview?.unreadable
                ? t("settings.remote.tsDisableDescUnreadable")
                : tsPreview?.foreign
                  ? t("settings.remote.tsDisableDescForeign")
                  : t("settings.remote.tsDisableDescMixed", {
                      count: tsPreview?.wouldClear ?? 0,
                    })}
            </DialogDescription>
          </DialogHeader>
          <ul data-testid="ts-disable-entries" className="space-y-1 text-xs">
            {(tsPreview?.entries ?? []).map((e) => (
              <li key={e.label} className="flex items-start gap-2">
                <Badge tone={e.ours ? "blue" : "gray"}>
                  {e.ours ? t("settings.remote.tsEntryOurs") : t("settings.remote.tsEntryOther")}
                </Badge>
                <code className="font-mono break-all">{e.label}</code>
              </li>
            ))}
          </ul>
          <DialogFooter>
            <Button variant="outline" onClick={() => setTsPreview(null)}>
              {t("settings.remote.cancel")}
            </Button>
            <Button
              variant="destructive"
              disabled={busy}
              onClick={() => void revokeTailscale(true)}
            >
              {t("settings.remote.tsDisableConfirm")}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* TLS 对外绑定确认弹窗（既有 P7 门槛 UI）：lan 开关触发，确认 = 置位
          remote.public_ack（长期生效）并重试开启 lan；取消仅关弹窗、不调后端 */}
      <Dialog open={tlsOpen} onOpenChange={setTlsOpen}>
        <DialogContent className="max-w-md">
          <DialogHeader>
            <DialogTitle>{t("settings.remote.tlsDialogTitle")}</DialogTitle>
            <DialogDescription>{t("settings.remote.tlsDialogDesc")}</DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setTlsOpen(false)}>
              {t("settings.remote.cancel")}
            </Button>
            <Button onClick={() => void confirmTlsAndRetryLan()} disabled={busy}>
              {t("settings.remote.tlsDialogConfirm")}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
