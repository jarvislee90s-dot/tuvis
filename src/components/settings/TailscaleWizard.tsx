// §C2 Tailscale 首次配置引导一条龙（Task 6）+ §C3 强制可达性校验呈现（Task 7）：
// 按后端 wizard_steps 渲染纵向步骤列表，每步三态（待做 / 进行中 / 已完成）；需要人
// 的步骤突出显示用户要做什么（actAdmin 等动作文案键由后端下发）；卡住时明确显示该步
// blocked_reason——**不得只转圈**。verify 行承接只读可达性校验与 DNS 等待；
// 独立「重新开通」入口需要确认。只有实时验证通过才显示可用地址；动作失败落到
// 对应步骤行，配置/生命周期变化后丢弃旧回执。
// Windows 行如实标注「尚未实机校验」弱提示（数据源 windowsVerified）——不把未验证的
// 流程伪装成已验证。**当前 Windows 已实测整条流程**（2026-10-07 用户实机走完 兔维斯 向导
// 全程），故该提示在生产载荷下不渲染；机制保留（后端 `windowsUnverifiedSteps` 非空时
// 自动回来并点名），见下方渲染块注释。数据源 = remote_ts_probe / remote_ts_run_step
//（形状契约见 Rust 端 tailscale::wizard_status / run_step 注释）。
// 导出组件：TailscaleWizard（步骤列表本体，RemoteSection 的 tailscale 卡详情区
// 直接挂载——Task 9 卡片重排时已收敛，旧的独立入口行已撤）。tsWizard.* 两级键的
// 字面量必须只出现在本文件：RemoteSection 的 i18n 守卫测试按字面量扫描单层键，
// 两级键由本文件承担（本目录 tailscaleWizard.test.tsx 以两级感知扫描把关双语言键齐备）。
import { useCallback, useEffect, useRef, useState } from "react";
import { Check } from "lucide-react";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import {
  SETTINGS_NOTE,
  SETTINGS_REMOTE_BADGE,
  SETTINGS_REMOTE_HINT,
  SETTINGS_REMOTE_ROW,
} from "@/components/settings/typography";
import { useAppTranslation } from "@/hooks/use-app-translation";
import { formatInvokeError } from "@/lib/invokeError";
import { cn } from "@/lib/utils";
import { SetupWizardFrame } from "@/components/settings/SetupWizardFrame";
import { ProcessHint } from "@/components/settings/ProcessHint";
import {
  remoteTsProbe,
  remoteTsRunStep,
  remoteTsRevealDownloads,
  reachChecking,
  type TsStepState,
  type TsWizardProbe,
  type TsWizardStep,
} from "@/lib/api/remote";

// 步骤标题 i18n 键（显式映射而非模板串拼接——键必须字面存在，两级感知扫描才把得了关）
const STEP_KEY: Record<string, string> = {
  detect: "settings.remote.tsWizard.step_detect",
  download: "settings.remote.tsWizard.step_download",
  install: "settings.remote.tsWizard.step_install",
  sys_ext: "settings.remote.tsWizard.step_sys_ext",
  login: "settings.remote.tsWizard.step_login",
  shields_up: "settings.remote.tsWizard.step_shields_up",
  funnel: "settings.remote.tsWizard.step_funnel",
  verify: "settings.remote.tsWizard.step_verify",
  autostart: "settings.remote.tsWizard.step_autostart",
};

// 步骤 id → 本 locale 的步骤名（未知 id 原样回显）。**放模块级、t 由调用方传入**：
// 步骤行标题、I-3 弱提示的起点名、未验清单三处走**同一条映射**（M1：文案从起点派生的
// 前提）——三处各写一遍就是三份会漂移的副本。
const stepLabel = (t: ReturnType<typeof useAppTranslation>["t"], id: string) =>
  STEP_KEY[id] ? t(STEP_KEY[id]) : id;

// 可自动触发的步（id 与 Rust wizard_steps 一致）。login 不在列：兔维斯 不代登录，
// 动作是「去登录」链接；detect/sys_ext 是只读探针；verify 走下方专属「重试」按钮
//（§C3，未验/失败都要能重触发校验）；autostart 是 兔维斯 自身行为（恒已完成）
const RUNNABLE = new Set(["download", "install", "shields_up", "funnel"]);

// ④ 「在走动」反馈的适用范围（2026-10-07 用户裁决）：只有这三步会给用户一段
// **没有任何其它动静**的等待——download = 一次同步下载（Rust 侧 download_url_to 一次性
// 取回，期间零反馈）；install = 系统授权框之后到安装器返回之间（msiexec/installer 阻塞）；
// login = 「获取登录链接」的动作（后端后台发起 `tailscale login` + 有界轮询取链接，
// 最长 9.5s——这一步同样只有"进行中…"可看，而它正是用户实测"没有可点之物"的那一步，
// 更要让用户看见 兔维斯 确实去要了）。其余步（shields_up / funnel）是秒级 CLI 写，不给每步都挂噪音。
// **不确定进度（indeterminate）**：总量未知 ⇒ 不给百分比（与移动端「速率未知只显示已传
// 字节，不显示假百分比」同一条纪律），只表示「正在传输，没有卡住」。
const LONG_STEP_HINT: Record<string, string> = {
  download: "settings.remote.tsWizard.downloading",
  install: "settings.remote.tsWizard.installing",
  login: "settings.remote.tsWizard.fetchingLoginLink",
};

// **M3（2026-10-07 评审 Minor）**：线稿状态二的两行琥珀提示（wireframe 354-355：
// 「恢复中」「发布中」）**带 badge**，与状态五同形（`b-amber` = 正在进行、无需操作的
// 中间态，既不是故障 rose 也不是完成 green）；实现侧此前是纯文本 `<p>`，线稿与实现
// 各说各话。本组件内联同形徽标（**不 import RemoteSection 的 Badge**：RemoteSection
// 已 import 本组件，反向 import 会成环；字号走 `SETTINGS_REMOTE_BADGE` 这一份线稿档，
// 与那份 amber 档逐字一致）。
function AmberBadge({ children }: { children: React.ReactNode }) {
  return (
    <span
      className={cn(
        "inline-flex flex-none items-center rounded-full bg-amber-500/15 px-2 py-0.5 text-amber-600 dark:text-amber-400",
        SETTINGS_REMOTE_BADGE
      )}
    >
      {children}
    </span>
  );
}

export function TailscaleWizard({
  pin = "",
  refreshKey = "",
  remoteEnabled = true,
  configKey = null,
  channelEnabled = true,
}: {
  pin?: string;
  refreshKey?: string;
  remoteEnabled?: boolean;
  /** 父级通道快照的配置标识（enabled/running 之外的最后一路失效信号） */
  configKey?: string | null;
  /** 父级通道快照的开关位（通道 KV，非进程运行态） */
  channelEnabled?: boolean;
}) {
  const { t } = useAppTranslation();
  const [probe, setProbe] = useState<TsWizardProbe | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  // 在途步互斥：连点会并发远程命令（下载/安装竞态）
  const [busyStep, setBusyStep] = useState<string | null>(null);
  // funnel 步回执带出的批准链接（首次开通时上游要求一次浏览器批准；兔维斯 只递不代点）
  const [approvalUrl, setApprovalUrl] = useState<string | null>(null);
  // ⑤ login 步回执带出的授权链接（「获取登录链接」按钮的产物）：回执里拿到的链接**立即**
  // 上墙，不等下一次探测——探测的 authUrl 是同一字段（后端 status --json 的 AuthURL），
  // 两条路同源，回执只是把用户刚点出来的结果直接交回去（少一次"点了没反应"的空窗）。
  // **M4（2026-10-08 架构评审）：回执必须有失效机制**——它只是"过渡态"，随**下一次探测
  // 落地**作废（`receiptGen` 代数与 `probeGen` 对齐；探测是权威源，见 loginLink 的计算）。
  const [authUrl, setAuthUrl] = useState<string | null>(null);
  const [receiptGen, setReceiptGen] = useState(0);
  // 探测落地的代数（每次成功探到一轮读数 +1）：回执只对"它之后还没有新探测"的窗口有效
  const [probeGen, setProbeGen] = useState(0);
  // 动作失败的落点（Task 6 评审移交指针）：按步骤行内展示，不再 catch 吞错零反馈
  const [stepError, setStepError] = useState<{ step: string; message: string } | null>(null);
  const [installOpened, setInstallOpened] = useState(false);
  const [downloadFailed, setDownloadFailed] = useState(false);
  const [repairOpen, setRepairOpen] = useState(false);
  const [troubleshootingFor, setTroubleshootingFor] = useState<string | null>(null);
  const [clockNow, setClockNow] = useState(0);
  const lifecycle = useRef(0);
  const latestProbe = useRef(0);
  // clockNow 由探测落地/秒级 tick 播种（播种点见 load）；等待元数据首帧渲染时必有真值
  const waitingExpired = Boolean(
    probe?.waiting &&
    clockNow - probe.waiting.startedAtMs >= (probe.waiting.expectedSeconds ?? 600) * 1000
  );
  const waitingKey = probe?.waiting
    ? `${probe.configKey ?? ""}:${probe.waiting.startedAtMs}`
    : null;
  const overdueDns = Boolean(
    waitingExpired &&
    (probe?.reach.state === "record_pending" || probe?.reach.state === "verifying")
  );
  const troubleshootingOpen = Boolean(waitingKey && troubleshootingFor === waitingKey);

  const load = useCallback(async () => {
    const request = ++latestProbe.current;
    const generation = lifecycle.current;
    try {
      const next = await remoteTsProbe();
      if (request !== latestProbe.current || generation !== lifecycle.current) return;
      setProbe(next);
      // 时钟在探测落地那一刻播种（2026-10-22）：等待元数据首帧即真值——渲染期读
      // Date.now() 过不了 purity 规则，播种晚一拍又会让首帧闪「0 分钟」
      if (next.waiting) setClockNow(Date.now());
      const completed = new Set(next.states.filter((state) => state.done).map((state) => state.id));
      if (next.installerReady || completed.has("download") || completed.has("install")) {
        setDownloadFailed(false);
      }
      // 人工恢复后也以重探为准，完成态不能继续携带上一次失败的降级提示。
      setStepError((error) =>
        error && (completed.has(error.step) || (error.step === "download" && next.installerReady))
          ? null
          : error
      );
      // 探测落地 = 权威源更新一次 ⇒ 回执代数随之作废（M4）
      setProbeGen((g) => g + 1);
      setLoadError(null);
    } catch (e) {
      if (request !== latestProbe.current || generation !== lifecycle.current) return;
      setLoadError(typeof e === "string" ? e : String(e));
    }
  }, []);

  // **真失效与同配置普通刷新分离**（2026-10-22 终审 P2）：只有配置标识换代、通道被关、
  // 总开关被关才算真失效——递增生命周期（拒在途动作回执）并全清状态；其余 refreshKey
  // 变化（enabled/running/address/reach 翻转，**含开通意图持久化后的 enabled false→true**）
  // 只是同配置普通刷新：照常重探，但**保留**批准链接与在途动作。批准链接只由 funnel 回执
  // 给出、probe 不含此字段，一刀切清状态会让首次开通被自己触发的通道翻转清掉链接，
  // 用户来不及完成浏览器批准。通道/总开关按「当前是否关」判定、不区分翻转方向：关必然
  // 作废（旧回执不得点亮已关闭的通道），开（含 false→true）不。
  // seenConfigKey 以 undefined 起始 ⇒ 首跑（挂载）恒失效（清场 noop），此后只比配置。
  const seenConfigKey = useRef<string | null | undefined>(undefined);
  useEffect(() => {
    const invalidated =
      seenConfigKey.current !== configKey || channelEnabled === false || remoteEnabled === false;
    seenConfigKey.current = configKey;
    if (invalidated) {
      lifecycle.current += 1;
      setProbe(null);
      setApprovalUrl(null);
      setAuthUrl(null);
      setStepError(null);
    }
    void load();
  }, [load, refreshKey, configKey, channelEnabled, remoteEnabled]);

  // 卸载清理（与失效门分离）：作废一切在途请求。普通刷新**不能**替在途动作作废，
  // 否则「在途同配置刷新 → 回执到达」会被这里误杀。
  useEffect(() => {
    return () => {
      lifecycle.current += 1;
      latestProbe.current += 1;
    };
  }, []);

  useEffect(() => {
    if (!probe?.waiting) return;
    setClockNow(Date.now());
    const timer = window.setInterval(() => setClockNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [probe?.waiting]);

  const run = async (step: string) => {
    if (busyStep) return;
    const generation = lifecycle.current;
    latestProbe.current += 1;
    setBusyStep(step);
    setStepError(null);
    if (step === "download") setDownloadFailed(false);
    try {
      const r = await remoteTsRunStep(step);
      if (generation !== lifecycle.current) return;
      if (step === "install" && probe?.platform === "mac") setInstallOpened(true);
      if (r.approvalUrl) setApprovalUrl(r.approvalUrl);
      // ⑤ login 步回执的授权链接（后端主动取回的，兔维斯 只递不代登录）；M4：记下它到的
      // **探测代数**——紧随其后的那次重探一落地，本回执即作废（探测是权威源）
      if (r.authUrl) {
        setAuthUrl(r.authUrl);
        setReceiptGen(probeGen);
      }
      await load(); // 动作回执后重探一次（GET 时机 = 挂载/动作后，不做时刻轮询）
    } catch (e) {
      if (generation !== lifecycle.current) return;
      // 失败不弹全局 toast 抢焦点：把错误落到对应步骤行（可见错误行），重探刷新状态
      if (step === "download") setDownloadFailed(true);
      const message = formatInvokeError(e, t);
      setStepError({
        step,
        message: message.trim() || t("settings.remote.tsWizard.actionFailedUnknown"),
      });
      await load();
    } finally {
      setBusyStep(null);
    }
  };

  const stateOf = (id: string): TsStepState | undefined => probe?.states.find((s) => s.id === id);
  // §C3：头部地址门控——只有校验通过（Verified）才算可用地址；未验证/失败一律不展示
  const reachVerified = probe?.reach?.state === "verified";
  // ⑤ 登录窗口：登录步未完成时，链接可能由两条路出现（用户点「获取登录链接」、或用户在
  // 浏览器里点完了）——两条都在下面接住
  const loginDone = stateOf("login")?.done ?? false;
  const installDone = stateOf("install")?.done ?? false;
  const installed = installDone || (stateOf("detect")?.done ?? false);
  const activeStep = probe?.steps.find(
    (step) => step.id !== "detect" && !stateOf(step.id)?.done
  )?.id;
  const openDownloads = () => {
    if (!probe?.installerPath || !probe.installerReady) return;
    const generation = lifecycle.current;
    void remoteTsRevealDownloads(probe.installerPath).catch((e: unknown) => {
      if (generation !== lifecycle.current) return;
      setStepError({
        step: "download",
        message: formatInvokeError(e, t) || t("settings.remote.tsWizard.actionFailedUnknown"),
      });
    });
  };
  // **权威源仍是探测**（与全仓「UI 以重探为权威源」一致）：探测有链接就用探测的（授权链接
  // 会被 tailscaled 轮换/失效，回执里那条可能已经过期）。
  // **M4（2026-10-08 架构评审）：回执有失效机制**——它只在"还没有新探测落地"的窗口里有效
  //（`receiptGen === probeGen`，即它到的时候是第几代探测），把用户刚点出来的结果**立即**
  // 上墙、免得出现"点了没反应"的空窗；下一次探测一落地就**一律以探测为准**（哪怕探测说
  // "没有链接"——那是权威读数，不许拿旧回执硬撑）。旧实现在探测报空时仍回落到回执，
  // 与"探测是权威源"的注释自相矛盾。
  const receiptLive = authUrl !== null && receiptGen === probeGen;
  // 探测（权威源）给的链接：**空串按"没有链接"处理**（后端用空串表示暂无链接，
  // 不是"有链接"）——只有它为空串时，才轮到上面的回执顶上。
  const probedLink = probe?.authUrl ? probe.authUrl : null;
  const loginLink = probedLink ?? (receiptLive ? authUrl : null);

  // ⑤ 登录窗口的**自动复评**（用户实测：登录步曾无可点之物，见下方渲染块注释）。
  // 用户路径是「点链接 → 浏览器抢走焦点 → 在浏览器完成登录 → 切回 兔维斯」——窗口重新拿到
  // focus 时重探一次，登录步自动翻「已完成」、向导继续往下走。
  // **为什么不做定时轮询**：本仓纪律是「GET 时机 = 挂载 / 状态跃迁 / 动作后」，且这一窗口的
  // 真值变化**只可能由用户离开/回到本窗口引起**；拿不到链接时用户手上还有「获取登录链接」
  // 与「刷新状态」两条明路，不需要后台偷跑 CLI（探一次要派生 3 个进程）。
  // **M5（2026-10-08 架构评审）：作用域收窄到"链接已在屏幕上"这一窗口。** 旧判据只有
  // `!loginDone`，于是"已装未登录"的整段时间里每次聚焦都白跑一轮探测（3 个 CLI 派生）——
  // 用户切个窗口回来就付这个代价；而上面那条用户路径**必然先有链接**（没链接时他还在
  // 向导里点「获取登录链接」，不存在"去浏览器登录完切回来"这一幕）。
  useEffect(() => {
    const awaitingInstall = installOpened && probe?.platform === "mac" && !installDone;
    if (!awaitingInstall && (loginDone || !loginLink)) return;
    const onFocus = () => void load();
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, [load, loginDone, loginLink, installOpened, installDone, probe?.platform]);

  const allStepsDone = Boolean(
    probe?.steps.length && probe.steps.every((step) => stateOf(step.id)?.done)
  );
  const complete = Boolean(
    remoteEnabled &&
    probe?.configKey &&
    probe.enabled &&
    probe.running &&
    probe.reach.state === "verified" &&
    probe.boardUrl &&
    allStepsDone
  );
  const blockedReason = probe?.states.find((state) => state.blockedReason)?.blockedReason ?? null;
  const configuredOff = Boolean(probe && (!probe.enabled || !remoteEnabled));
  const issue = Boolean(
    loadError ||
    (!configuredOff && probe?.reach.state === "failed") ||
    (!configuredOff && probe?.reach.state !== "recovering" && blockedReason)
  );
  // 「自动复验在途」判据见 reachChecking（shared）——只认 unverified 会让提示在首验
  // 落进 record_pending 时中途退场（实测转圈十几秒就消失，但看板要几十秒到一分钟才通，
  // 剩下又变成无台词等待）
  const reachCheckingNow = probe ? reachChecking(probe.reach) : false;

  return (
    <SetupWizardFrame
      storageKey="remote.setup.tailscale"
      configKey={probe?.configKey ?? null}
      complete={complete}
      address={reachVerified && remoteEnabled ? (probe?.boardUrl ?? null) : null}
      pin={pin}
      offline={configuredOff}
      reverifyPending={Boolean(
        remoteEnabled && probe?.enabled && probe.running && reachCheckingNow
      )}
      issueText={
        probe?.reach.state === "failed"
          ? "reason" in probe.reach
            ? formatInvokeError(probe.reach.reason, t)
            : null
          : probe?.reach.state === "recovering"
            ? null
            : blockedReason || loadError
              ? formatInvokeError(blockedReason ?? loadError, t)
              : null
      }
      statusText={
        probe?.reach.state === "recovering" ? t("settings.remote.tsRecoveringHint") : null
      }
      issue={issue}
    >
      <div data-testid="ts-wizard">
        <div className="flex items-center justify-between gap-2">
          <p className="text-sm font-semibold">{t("settings.remote.tsWizard.title")}</p>
          <Button variant="ghost" size="sm" onClick={() => void load()}>
            {t("settings.remote.tsWizard.refresh")}
          </Button>
        </div>
        {/* Windows 行弱提示（I-3：验证位不得大于证据）——**机制保留，当前不渲染**：
          2026-10-07 用户在本机 Windows 上卸载 Tailscale 后**从零走完 兔维斯 向导全程**
          （下载 → 安装 UAC → 登录 → 关 shields-up → 开通 Funnel → 可达性校验，全程正常），
          故后端把「实测覆盖起点」前移到第一步（`wizard.rs::WINDOWS_VERIFIED_FROM`）⇒
          未验清单为空 ⇒ `windowsVerified=true` ⇒ 本行不渲染（用户实测看到的黄标就此撤下）。
          **为什么保留这一块**：它承载的是「验证位不得大于证据」这条机制的 UI 侧——将来若
          又出现未实测的段落（新增步骤 / 新平台形态），后端把起点挪回那一步，清单非空，
          本行自动回来并按 `windowsUnverifiedSteps` **点名**那几步（清单随步骤表派生，
          文案不写死）。详见 `tests/settings/tailscaleWizard.test.tsx` 的 I-3 组：
          该组喂的是「清单非空」载荷，锁的正是这条机制。
          **M1（2026-10-08 架构评审）：文案也从起点派生**——旧文案写死"只实测了**后半段**"
          （当时的起点正是 `shields_up`），一旦起点挪到别处那句话就成了**假陈述**。现在
          起点名（`windowsVerifiedFrom` → 本 locale 的步骤名）也随载荷下发、进 i18n 插值；
          字段缺失（形态不符，生产对 Windows 恒下发）时显示 `—`——**可见的**降级，
          好过一句读起来像事实的假话。 */}
        {probe?.platform === "windows" && !probe.windowsVerified && (
          <p
            data-testid="ts-windows-unverified"
            className="mt-1 rounded-lg bg-amber-50 px-2.5 py-1.5 text-xs text-amber-600 dark:bg-amber-500/10 dark:text-amber-400"
          >
            {t("settings.remote.tsWizard.windowsUnverified", {
              from: probe.windowsVerifiedFrom ? stepLabel(t, probe.windowsVerifiedFrom) : "—",
              steps: (probe.windowsUnverifiedSteps ?? []).map((id) => stepLabel(t, id)).join(" → "),
            })}
          </p>
        )}
        {/* B2：写路径未实机验证弱提示（与 Windows 行同款做法）——macOS 的
          `funnel --bg` / `reset` / 批准链接抓取从未在本机跑过，不得伪装成已验证；
          Windows 已实测三条写路径，后端位为 true 时本条自动消失。
          M-4：文案**逐平台**取（macOS 那份"只读探测已实测"只在 macOS 成立——Linux/Other
          上什么都不曾探测过，套用 macOS 的表述就是张冠李戴的谎报）。 */}
        {probe && !probe.writePathVerified && (
          <p
            data-testid="ts-writepath-unverified"
            className="mt-1 rounded-lg bg-amber-50 px-2.5 py-1.5 text-xs text-amber-600 dark:bg-amber-500/10 dark:text-amber-400"
          >
            {probe.platform === "mac"
              ? t("settings.remote.tsWizard.writePathUnverifiedMac")
              : t("settings.remote.tsWizard.writePathUnverifiedOther")}
          </p>
        )}
        {loadError !== null && (
          <p data-testid="ts-load-error" className="mt-1 text-xs text-rose-500">
            {t("settings.remote.tsWizard.loadFailed")}: {formatInvokeError(loadError, t)}
          </p>
        )}
        {probe === null && loadError === null && (
          <p className="text-muted-foreground mt-2 animate-pulse text-xs">…</p>
        )}
        {/* §C3：看板地址只在通道运行且校验通过（Verified）时展示——未验证不得当可用
          地址让用户干等（失败/待验的呈现走 verify 步行的「尚未生效 + 重试」） */}
        {probe?.running && probe.boardUrl && reachVerified && (
          <p className="text-muted-foreground mt-1 text-xs">
            <span className="mr-1 inline-block h-2 w-2 rounded-full bg-emerald-500 align-middle" />
            {t("settings.remote.tsWizard.runningBadge")} ·{" "}
            {t("settings.remote.tsWizard.boardUrlLabel")}:{" "}
            <code className={`bg-muted rounded-md px-1.5 py-0.5 font-mono ${SETTINGS_REMOTE_HINT}`}>
              {probe.boardUrl}
            </code>
          </p>
        )}
        {/* W-A：**开机恢复窗口**（后端重连中）——独立语义块，先于其他提示判定：
          既不是故障（不显示 verify 行的「尚未生效」），也不是「域名生效中」（重启后
          DNS 记录不撤销，压根不用等发布）。判据是后端下发的 reach.state=recovering，
          与 probe.running 无关（恢复窗口里快照就是 running=false）。 */}
        {probe?.reach?.state === "recovering" && (
          <p
            data-testid="ts-recovering"
            className="mt-1 flex flex-wrap items-center gap-2 rounded-lg bg-amber-50 px-2.5 py-1.5 text-xs text-amber-600 dark:bg-amber-500/10 dark:text-amber-400"
          >
            {/* M3：与线稿状态二 354 行同形（琥珀 badge + 说明） */}
            <AmberBadge>{t("settings.remote.tsWizard.tsRecoveringBadge")}</AmberBadge>
            {t("settings.remote.tsWizard.tsRecoveringHint")}
          </p>
        )}
        <ol aria-label={t("settings.remote.tsWizard.timelineLabel")} className="mt-3">
          {(probe?.steps ?? []).map((step: TsWizardStep, index: number) => {
            const st = stateOf(step.id);
            // 检测返回「未安装」也是一次已完成的检测；是否装好仍取后端 installed 判据。
            const done = step.id === "detect" || (st?.done ?? false);
            const busy = busyStep === step.id;
            const waitingForInstall =
              !installed && !done && !["detect", "download", "install"].includes(step.id);
            const blocked = waitingForInstall ? null : (st?.blockedReason ?? null);
            const sysExtStopped = Boolean(
              step.id === "sys_ext" && blocked?.includes("TSWIZ_SYS_EXT_NOT_RUNNING")
            );
            // ④ 长阻塞步的「在走动」文案键（非长阻塞步 ⇒ undefined ⇒ 不挂指示器）
            const progressKey =
              step.id === "install" && probe?.platform === "mac"
                ? "settings.remote.tsWizard.openingInstaller"
                : LONG_STEP_HINT[step.id];
            // **I3（2026-10-08 架构评审）：动作文案必须指向行内真实存在的按钮。**
            // 后端下发的 `actLogin` 是"点击「去登录」…"，而「去登录」按钮**只在 loginLink
            // 非空时渲染**（见下方 login 块）——无链接时行内只有「获取登录链接」(+兜底提示)，
            // 沿用 actLogin 就是同一行两条矛盾指令（⑤ 要消灭的"无可点之物"的降级版）。
            // 故无链接时换 `actLoginNoLink`（两个 locale 同步新增，文案里点名的正是那个真按钮）。
            const actionKey =
              step.id === "login" && !loginLink
                ? "settings.remote.tsWizard.actLoginNoLink"
                : step.humanActionKey;
            const actionText =
              step.needsHuman && actionKey && !done && !waitingForInstall && !sysExtStopped
                ? step.id === "install" && !probe?.installerReady
                  ? t("settings.remote.tsWizard.installNeedsDownload")
                  : t(actionKey)
                : "";
            return (
              <li
                key={step.id}
                data-step={step.id}
                data-done={done ? "true" : "false"}
                aria-current={activeStep === step.id ? "step" : undefined}
                className="relative flex items-start gap-3 pb-3"
              >
                {index < (probe?.steps.length ?? 0) - 1 && (
                  <span
                    aria-hidden="true"
                    className={cn(
                      "absolute top-7 bottom-0 left-3 w-px",
                      done ? "bg-emerald-300 dark:bg-emerald-800" : "bg-border"
                    )}
                  />
                )}
                <span
                  data-dot
                  aria-hidden="true"
                  className={cn(
                    "relative z-10 mt-1 flex h-6 w-6 shrink-0 items-center justify-center rounded-full text-xs font-medium",
                    done
                      ? "bg-emerald-100 text-emerald-700 dark:bg-emerald-950 dark:text-emerald-400"
                      : busy || activeStep === step.id
                        ? "bg-blue-100 text-blue-700 dark:bg-blue-950 dark:text-blue-300"
                        : "bg-muted text-muted-foreground"
                  )}
                >
                  {done ? <Check className="h-3.5 w-3.5" /> : index + 1}
                </span>
                <div
                  className={cn(
                    "min-w-0 flex-1 rounded-lg border p-3",
                    activeStep === step.id
                      ? "border-blue-300 bg-blue-50/30 dark:border-blue-800 dark:bg-blue-950/10"
                      : "border-border/60"
                  )}
                >
                  <div className="grid grid-cols-[minmax(0,1fr)_auto] items-start gap-3">
                    <div className="flex min-h-8 min-w-0 flex-wrap items-center gap-x-2 gap-y-1">
                      <span
                        className={cn(
                          SETTINGS_REMOTE_ROW,
                          done && "text-emerald-700 dark:text-emerald-400"
                        )}
                      >
                        {stepLabel(t, step.id)}
                      </span>
                      <span
                        className={cn(
                          SETTINGS_NOTE,
                          done && "text-emerald-700 dark:text-emerald-400"
                        )}
                      >
                        {done
                          ? step.id === "sys_ext"
                            ? t("settings.remote.tsWizard.stateNoAction")
                            : t("settings.remote.tsWizard.stateDone")
                          : step.id === "verify" && probe?.waiting
                            ? t("settings.remote.tsWizard.waitingDnsLabel")
                            : busy || (step.id === "verify" && probe?.reach.state === "verifying")
                              ? t("settings.remote.tsWizard.stateActive")
                              : waitingForInstall
                                ? t("settings.remote.tsWizard.waitingForInstall")
                                : t("settings.remote.tsWizard.statePending")}
                      </span>
                    </div>
                    <div
                      data-testid="ts-step-actions"
                      className="flex min-h-8 max-w-52 flex-wrap justify-end gap-2"
                    >
                      {RUNNABLE.has(step.id) && !done && (
                        <Button
                          variant={activeStep === step.id ? "default" : "outline"}
                          size="sm"
                          className="min-h-8 px-3 text-xs"
                          disabled={
                            busyStep !== null ||
                            waitingForInstall ||
                            (step.id === "install" && !probe?.installerReady)
                          }
                          onClick={() => void run(step.id)}
                        >
                          {t(
                            step.id === "download"
                              ? downloadFailed
                                ? "settings.remote.tsWizard.retryDownload"
                                : "settings.remote.tsWizard.autoDownload"
                              : step.id === "install"
                                ? "settings.remote.tsWizard.step_install"
                                : step.id === "shields_up"
                                  ? "settings.remote.tsWizard.allowIncoming"
                                  : "settings.remote.tsWizard.enableFunnel"
                          )}
                        </Button>
                      )}
                      {/* login：兔维斯 不代登录——只递授权链接。**⑤（2026-10-07 用户实测）**：
                      旧实现只在 `probe.authUrl` 非空时渲染一条文字链接，而新装机器上
                      `status --json` 是 `NeedsLogin ∧ AuthURL=""`（授权链接要**发起一次
                      交互式登录**才由尾网生成）⇒ **登录行里没有任何可点的东西**，用户
                      「没有看到有链接什么的」，只能自己去客户端手动登录。
                      现在两条路都给：
                      ① 拿到链接（回执 / 探测）→ 做成**明确的按钮**（Button asChild 包 <a>，
                         新窗口 + noreferrer noopener）；
                      ② 还没有链接 → 一个**「获取登录链接」按钮**（点它让 兔维斯 主动向后端要：
                         后端后台发起 `tailscale login` + 有界轮询取链接，见 Rust
                         `login_step_with`）+ 一句**明确的下一步**文案（不是让用户自己去
                         客户端里找）。合规红线不变：**只递链接，不代登录、不持凭据**。 */}
                      {step.id === "login" && !done && loginLink && (
                        <Button asChild size="sm" className="min-h-8 px-3 text-xs">
                          <a
                            data-auth
                            data-testid="ts-auth-link"
                            href={loginLink}
                            target="_blank"
                            rel="noreferrer noopener"
                          >
                            {t("settings.remote.tsWizard.openAuthUrl")}
                          </a>
                        </Button>
                      )}
                      {step.id === "login" && !done && (
                        <Button
                          variant={loginLink ? "ghost" : "outline"}
                          size="sm"
                          className="min-h-8 px-3 text-xs"
                          data-testid="ts-login-link-btn"
                          disabled={busyStep !== null || waitingForInstall}
                          onClick={() => void run("login")}
                        >
                          {t("settings.remote.tsWizard.getLoginLink")}
                        </Button>
                      )}
                      {/* verify 只读；记录发布等待期间后台每分钟复验，不给重复触发入口。 */}
                      {step.id === "verify" &&
                        !done &&
                        probe?.reach.state !== "record_pending" &&
                        probe?.reach.state !== "verifying" &&
                        !probe?.waiting && (
                          <Button
                            variant="outline"
                            size="sm"
                            className="min-h-8 px-3 text-xs"
                            data-testid="ts-verify-check"
                            disabled={busyStep !== null || waitingForInstall}
                            onClick={() => void run("verify")}
                          >
                            {t("settings.remote.tsWizard.checkNow")}
                          </Button>
                        )}
                      {step.id === "verify" &&
                        !done &&
                        (probe?.reach.state === "record_pending" ||
                          probe?.reach.state === "verifying") &&
                        probe.waiting &&
                        waitingExpired && (
                          <div className="space-y-2">
                            <p data-testid="ts-waiting-overdue" className="text-xs text-amber-600">
                              {t("settings.remote.tsWizard.waitingOverdue")}
                            </p>
                            <Button
                              variant="outline"
                              size="sm"
                              aria-expanded={troubleshootingOpen}
                              onClick={() =>
                                setTroubleshootingFor(troubleshootingOpen ? null : waitingKey)
                              }
                            >
                              {t("settings.remote.tsWizard.viewTroubleshooting")}
                            </Button>
                          </div>
                        )}
                      {step.id === "verify" && !done && probe?.reach.state === "failed" && (
                        <Button
                          variant="outline"
                          size="sm"
                          data-testid="ts-repair-open"
                          disabled={busyStep !== null}
                          onClick={() => setRepairOpen(true)}
                        >
                          {t("settings.remote.tsWizard.repair")}
                        </Button>
                      )}
                      {step.id === "sys_ext" && !done && (
                        <Button
                          variant="outline"
                          size="sm"
                          className="min-h-8 px-3 text-xs"
                          disabled={busyStep !== null || waitingForInstall}
                          onClick={() => void load()}
                        >
                          {t("settings.remote.tsWizard.refresh")}
                        </Button>
                      )}
                      {((step.id === "download" && probe?.installerReady) ||
                        (step.id === "install" &&
                          stepError?.step === "install" &&
                          probe?.installerReady)) && (
                        <Button
                          variant="outline"
                          size="sm"
                          className="min-h-8 px-3 text-xs"
                          disabled={busyStep !== null}
                          onClick={openDownloads}
                        >
                          {t("settings.remote.tsWizard.openDownloads")}
                        </Button>
                      )}
                    </div>
                  </div>
                  {step.id === "detect" && (
                    <p className="text-muted-foreground mt-1 text-xs">
                      {t(
                        installed
                          ? "settings.remote.tsWizard.detectFound"
                          : "settings.remote.tsWizard.detectMissing"
                      )}
                    </p>
                  )}
                  {step.id === "download" && probe?.installerReady && probe.installerPath && (
                    <div data-testid="ts-installer-download" className="mt-2 space-y-1 text-xs">
                      <p>{t("settings.remote.tsWizard.downloadLocation")}</p>
                      <p className="text-muted-foreground break-all">{probe.installerPath}</p>
                    </div>
                  )}

                  {/* ④ 长阻塞步的「在走动」反馈（用户裁决：不确定进度条即可，不要假百分比）：
                    发起即显示（busy 在本步发起时同步置上）、步骤返回即收起。
                    为什么纯前端够用（已读码验证，不是猜）：`remote_ts_run_step` 是
                    `async fn` + `tauri::async_runtime::spawn_blocking`（Rust 主线程/IPC
                    派发线程不被占用），前端 `invoke()` 返回 Promise 也**不阻塞 JS 事件
                    循环** ⇒ 下载阻塞期间本行照常渲染、`animate-spin` 照常转。
                    role=progressbar 且**不带 aria-valuenow** = 标准的「不确定进度」语义
                    （有值就是假百分比的前身）；文案也如实说"总量未知，故不给百分比"。 */}
                  {busy && progressKey && (
                    <p
                      data-testid="ts-step-progress"
                      role="progressbar"
                      aria-label={t(progressKey)}
                      className="text-muted-foreground mt-1 flex items-center gap-1.5 text-[11px]"
                    >
                      <span
                        aria-hidden="true"
                        className="inline-block h-3 w-3 flex-none animate-spin rounded-full border-2 border-blue-500 border-t-transparent"
                      />
                      {t(progressKey)}
                    </p>
                  )}
                  {step.id === "install" &&
                    probe?.platform === "mac" &&
                    installOpened &&
                    !done &&
                    !busy && (
                      <p
                        data-testid="ts-install-awaiting"
                        className="mt-1 text-xs text-amber-600 dark:text-amber-400"
                      >
                        {t("settings.remote.tsWizard.completeInInstaller")}
                      </p>
                    )}
                  {/* 需要人的步骤：动作文案突出显示（后端 humanActionKey 下发，前端只翻译）。
                    A1：可选人工步（humanOptional，目前只有 funnel 批准）额外挂一条弱提示
                    ——实测两平台各执一端（Windows 1.102.4 零批准链接、零点击；macOS 首开
                    确有一次），故**不得**把批准渲染成"等你点批准"的必经步骤，也不得据此
                    阻塞；链接真出现时由下方 approvalUrl 行给出。 */}
                  {actionText && (
                    <p className="mt-0.5 text-xs font-medium text-amber-600 dark:text-amber-400">
                      {actionText}
                    </p>
                  )}
                  {/* 2026-10-08（用户实测缺口）：Windows 走 MSI，**安装向导会让用户选安装
                    路径**（兔维斯 执行 msiexec 刻意不加 /qn——「装在哪里」的选择权本就该给
                    用户）。装到自定义路径时后端已能经「服务登记 ImagePath」找到 CLI，但
                    用户不会知道，只会一直看到「未安装」→ 又下载又安装（用户实测的死循环）。
                    故安装行常驻一条弱提示：说清 兔维斯 的**两个查找位置**，并给出「装完点
                    刷新状态」的动作（重探时机 = 挂载/动作后，不点就一直显示旧结论）。
                    macOS 的 .pkg 由 installer 固定装到 /Applications，用户无从选择
                    ⇒ 不显示（与后端 actAdminWinMsi 的逐平台分叉同口径）。 */}
                  {step.id === "install" && !done && probe?.platform === "windows" && (
                    <p
                      data-testid="ts-install-path-hint"
                      className="text-muted-foreground mt-0.5 text-[11px]"
                    >
                      {t("settings.remote.tsWizard.installPathHint")}
                    </p>
                  )}
                  {/* ⑤ 拿不到链接时的**明确下一步**（用户实测的正是这一段：行里只有动作文案、
                    没有任何可点的东西，用户只能自己去客户端里手动登录）。文案说清三件事：
                    点上面的按钮让 兔维斯 去要链接 / 兔维斯 不代登录不碰账号 / 兜底可到本机
                    Tailscale 客户端点「Log in」，或点右上「刷新状态」重试。
                    busy 期间不挂（正在要链接，别同时喊两边）。 */}
                  {step.id === "login" && !done && !loginLink && !busy && !waitingForInstall && (
                    <p
                      data-testid="ts-login-hint"
                      className="text-muted-foreground mt-0.5 text-[11px] leading-snug"
                    >
                      {t("settings.remote.tsWizard.loginLinkHint")}
                    </p>
                  )}
                  {step.needsHuman && step.humanOptional && !done && !waitingForInstall && (
                    <p
                      data-testid="ts-optional-step"
                      className="text-muted-foreground mt-0.5 text-[11px]"
                    >
                      {t("settings.remote.tsWizard.maybeNeeded")}
                    </p>
                  )}
                  {/* 卡住必须点名原因，不许只转圈（verify 行有专属「尚未生效」呈现，见下）。
                    **M4（2026-10-07 评审 Minor）**：色/语义档位随后端判据走——`amber`
                    是中间态（恢复窗口「正在进行、无需操作」，线稿 82-84 明写「既不是故障
                    （rose）也不是正常完成」）：不涂红、也不加「卡住：」前缀（措辞本就如实，
                    只是档位不对），直接给成因与"稍候自动复评"。缺字段按 rose（fail-safe）。 */}
                  {blocked && step.id !== "verify" && (
                    <p
                      data-blocked
                      data-testid="ts-blocked"
                      data-blocked-tone={st?.blockedTone ?? "rose"}
                      className={cn(
                        "mt-0.5 text-xs",
                        (st?.blockedTone ?? "rose") === "amber"
                          ? "text-amber-600 dark:text-amber-400"
                          : "text-rose-500"
                      )}
                    >
                      {(st?.blockedTone ?? "rose") === "rose" &&
                        `${t("settings.remote.tsWizard.blockedTitle")} `}
                      {formatInvokeError(blocked, t)}
                    </p>
                  )}
                  {step.id === "verify" && (
                    <>
                      {/* 等待与检查结果留在 verify 步。等待起点和预期窗口以后端元数据为准；
                        不推测发布成因，不给倒计时，也不提供重复检查入口。 */}
                      {probe?.running &&
                        !reachVerified &&
                        (probe.reach?.state === "record_pending" ||
                          probe.reach?.state === "verifying") &&
                        probe.waiting && (
                          <p
                            data-testid="ts-pending"
                            className="mt-1 flex flex-wrap items-center gap-2 rounded-lg bg-amber-50 px-2.5 py-1.5 text-xs text-amber-600 dark:bg-amber-500/10 dark:text-amber-400"
                          >
                            {t("settings.remote.tsWizard.waitingDnsHint", {
                              expected: Math.ceil((probe.waiting?.expectedSeconds ?? 600) / 60),
                              elapsed: Math.floor(
                                Math.max(0, clockNow - (probe.waiting?.startedAtMs ?? clockNow)) /
                                  60000
                              ),
                            })}
                            {probe.reach.state === "verifying" && (
                              <span>{t("settings.remote.tsWizard.verifyingNow")}</span>
                            )}
                            <span>{t("settings.remote.tsWizard.waitingLocalReady")}</span>
                            <span>{t("settings.remote.tsWizard.addressWillAppear")}</span>
                          </p>
                        )}
                      {/* 未验证 / 在验：等待时间与预计窗口由后端 waiting 元数据承载，前端不再猜时长。 */}
                      {probe?.running &&
                        !reachVerified &&
                        (probe.reach?.state === "unverified" ||
                          (probe.reach?.state === "verifying" && !probe.waiting) ||
                          (probe.reach?.state === "record_pending" && !probe.waiting)) && (
                          <p
                            data-testid="ts-pending"
                            className="mt-1 rounded-lg bg-amber-50 px-2.5 py-1.5 text-xs text-amber-600 dark:bg-amber-500/10 dark:text-amber-400"
                          >
                            {t("settings.remote.tsWizard.tsPendingChecking")}
                          </p>
                        )}
                      {overdueDns && troubleshootingOpen && (
                        <div
                          data-testid="ts-troubleshooting"
                          className="mt-2 space-y-2 rounded-md border p-3 text-xs"
                        >
                          <p>{t("settings.remote.tsWizard.troubleshootingKeepOn")}</p>
                          <p>{t("settings.remote.tsWizard.troubleshootingNetwork")}</p>
                          <p>{t("settings.remote.tsWizard.troubleshootingChecksContinue")}</p>
                          <p>{t("settings.remote.tsWizard.troubleshootingRepairOptional")}</p>
                          <Button
                            variant="outline"
                            size="sm"
                            data-testid="ts-repair-open"
                            disabled={busyStep !== null}
                            onClick={() => setRepairOpen(true)}
                          >
                            {t("settings.remote.tsWizard.repair")}
                          </Button>
                        </div>
                      )}
                    </>
                  )}
                  {/* verify §C3：校验失败 = 固定地址尚未生效——点名口径 + reason，
                    不显示成可用地址让用户干等 */}
                  {step.id === "verify" && blocked && (
                    <p data-testid="ts-not-live" className="mt-0.5 text-xs text-rose-500">
                      {t("settings.remote.tsWizard.tsNotLive")}：{formatInvokeError(blocked, t)}
                    </p>
                  )}
                  {/* 瞬时抖动引导（2026-10-13 真机定位）：「地址已解析但连不通」类失败多为
                    公网入口瞬时抖动，60s 轮询会自愈——先安抚 + 给重试门槛，避免恢复窗口内
                    狂点重试（重试在旧版还会触发 funnel reset 误伤配置）。前缀判定与
                    reach.rs 的连不通类格式串同步（reason 恒中文，不随 UI 语言漂移） */}
                  {step.id === "verify" && blocked?.startsWith("REACH_ADDR_UNREACHABLE") && (
                    <ProcessHint testId="ts-transient-hint">
                      {t("settings.remote.tsWizard.tsTransientHint")}
                    </ProcessHint>
                  )}
                  {step.id === "funnel" && approvalUrl && (
                    <p data-testid="ts-approval" className="mt-1 text-xs">
                      <span className="text-amber-600 dark:text-amber-400">
                        {t("settings.remote.tsWizard.approvalHint")}
                      </span>{" "}
                      <a
                        href={approvalUrl}
                        target="_blank"
                        rel="noreferrer"
                        className="text-blue-500 underline"
                      >
                        {approvalUrl}
                      </a>
                    </p>
                  )}
                  {/* 动作失败落行内错误位（Task 6 移交指针：不再吞错误零反馈） */}
                  {stepError?.step === step.id && (
                    <p
                      role="alert"
                      data-testid="ts-step-error"
                      className="mt-2 text-xs text-rose-500"
                    >
                      {t("settings.remote.tsWizard.actionFailed")} {stepError.message}
                    </p>
                  )}
                  {step.id === "download" &&
                    !done &&
                    downloadFailed &&
                    !probe?.installerReady &&
                    busyStep !== "download" &&
                    probe?.downloadUrl && (
                      <div
                        data-testid="ts-download-fallback"
                        className="mt-2 space-y-2 rounded-md bg-amber-50 p-2.5 text-xs dark:bg-amber-950/30"
                      >
                        <p>{t("settings.remote.tsWizard.manualInstallHint")}</p>
                        <a
                          href={probe.downloadUrl}
                          target="_blank"
                          rel="noreferrer noopener"
                          className="inline-flex min-h-8 items-center text-blue-600 underline dark:text-blue-400"
                        >
                          {t("settings.remote.tsWizard.manualDownload")}
                        </a>
                      </div>
                    )}
                  {step.id === "install" &&
                    stepError?.step === "install" &&
                    probe?.installerReady && (
                      <p className="text-muted-foreground mt-2 text-xs">
                        {t("settings.remote.tsWizard.manualOpenHint")}
                      </p>
                    )}
                </div>
              </li>
            );
          })}
        </ol>
        {/* ③ 让用户知道 Tailscale 是什么（2026-10-07 用户要求）：向导此前**没有任何地方
          解释它是什么**，而用户被要求装一个没听过的第三方软件、还要去它的官网登录一次。
          页脚恒挂一行（不随相位/平台变化——未装 / 配置中 / 恢复中 / 未运行任何挂载形态
          都要能回答「这是什么」）：① 它是什么 = 免费的个人组网工具；② **为什么需要它**
          = 它给每台设备一个固定的私有地址，兔维斯 因此**不需要用户自备域名**就能给出永久
          链接（正是本卡卖点「免域名」的原理）；③ 官网外链（新窗口 + noreferrer noopener:
          不给新窗口 opener 句柄，安全惯例——与会话内其它外链同口径）。 */}
        <p
          data-testid="ts-about-tailscale"
          className="text-muted-foreground mt-2 text-[11px] leading-snug"
        >
          {t("settings.remote.tsWizard.aboutTailscale")}{" "}
          <a
            href="https://tailscale.com/"
            target="_blank"
            rel="noreferrer noopener"
            className="text-blue-500 underline"
          >
            tailscale.com
          </a>
        </p>
      </div>
      <Dialog open={repairOpen} onOpenChange={setRepairOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>{t("settings.remote.tsWizard.repairTitle")}</DialogTitle>
            <DialogDescription>{t("settings.remote.tsWizard.repairDescription")}</DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setRepairOpen(false)}>
              {t("settings.remote.tsWizard.cancel")}
            </Button>
            <Button
              data-testid="ts-repair-confirm"
              onClick={() => {
                setRepairOpen(false);
                void run("repair");
              }}
            >
              {t("settings.remote.tsWizard.repair")}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </SetupWizardFrame>
  );
}
