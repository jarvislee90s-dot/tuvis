// 移动端会话输入区（M7 Task 7，W4）：挂在 SessionDetail 对话区之下（非预览分支）。
// - 可用性：挂载拉取一次 /session-send-info；不可注入 → 输入区禁用 + reason 展示；
//   拉取失败（网络异常等）静默不渲染——详情页正文照常（与 fetchSessionFiles
//   静默降级同一惯例），403 设备失效同语境（上层会回配对页）；
// - 发送：POST /session-send 回执 chip——delivered 绿「已送达终端」/ submitted 灰
//   「已投递至终端输入，agent 空闲后处理（未确认落盘）」（D7/T3 中性回执，验收
//   问题 #5：注入 Ok + 戳未中 + 屏读无滞留草稿 = 已被 TUI 收进内部队列，非失败
//   **不提供重试**，重试 = 双发且 TUI 那份无法撤回）/ queued 黄
//   「排队中 第 N 位」+ [立即发送][撤回] / failed 红「发送失败：…」（重按发送即重试）
//   / blocked 中性 slate（丁T3 接入②：分流拦截——无注入尝试、无重试语义，文案是指路）；
//   await 全程另有「投递中…」chip（灰3：慢消费者长文投递可达分钟级，界面不空白，
//   完成后被结果 chip 覆盖）；正文上限与后端 MAX_SEND_CHARS 对齐（10000，双保险）；
// - 多列队列表（D8，验收问题 #6）：完整渲染 /session-queue——挂载拉一次（进入会话
//   即见桌面端/他端排的既有队列）+ 复用 3s 轮询通道（有排队回执或列表非空即持续，
//   列表变空即停）；逐条 = 预览（剥 [mobile 设备名] **尾**签名、附件标记替换 [附件]、
//   截前 12 字符）+ 位次（position=0 容忍口径不变）+ 立即发送/修改/撤回三钮（按条
//   id 走既有端点，不限本端发送的条目）；底部「空闲时将按序自动发送」说明。
//   「排队中 第 N 位」chip 从回执槽移除（由列表取代）；queued 回执仍内部存在——
//   发送后到列表刷新之间的即时反馈靠行乐观 upsert，失败对账（reconcileQueued）
//   与 3s 轮询仍以它驱动；单回执槽只保留最近一次发送的送达/中性/失败终态；
// - 排队列 3s 轮询 /session-queue（unmount 清理定时器）；我方条目从队列消失
//   （已被 flush 送达 / 他端撤回）→ 回执收敛留痕 + 行随权威列表移除；
// - 插队/撤回失败对账（M9R P2-7）：忙时失败 / 网络异常先 fetchQueue 复核——条目
//   仍在 pending → 恢复排队视图（刷新队位、按钮保留，可重试）；确认不在队 →
//   中性收敛文案（gone chip，评审裁决：忙时失败的守卫方正是正在投递的 flush 循环，
//   条目不在队大概率=已送达，落 failed「可重试」会诱发重复注入）；复核自身网络
//   失败 → 保守恢复排队视图（队位沿用旧值，3s 轮询随后自愈）——拿不到「真不在队」
//   的证据就不落终态，避免按钮丢失后排队条目在 UI 上失控；
// - 撤回按联合返回值分流（评审必须1）：{ok:true} 服务端确认已撤 → 免复核直接收敛；
//   200 {status:"failed"}（忙时拒收、条目仍在队）→ 复核——不得丢弃返回值直接复核，
//   否则「忙时 + 复核也失败」双失败会让条目实际在队而 UI 永久失控；
// - sending 期间插队/撤回按钮加闸（disabled=busy||sending，评审必须3）：与发送
//   回执的 last-write-wins 竞态防线（按钮可见不可点，保持「不失联」意图）；
// - 修改重发只入队（D6，验收问题 #4）：「修改」确认出队后置 queueOnlyNext 标志，
//   下一次发送携带 queueOnly=true 强制入队——防「文件说闲、TUI 实忙」窗口把
//   修改后的重发直发出去（变相插队）；入队后的放行节奏：会话转闲跃迁后事件臂
//   即时放行，已空闲且无跃迁时由 60s 周期兜底放行（可达分钟级），行为收敛；
// - 多行原样上行（textarea 天然换行，不做回车发送），归一在服务端入队时一次完成；
// - **卡片在场分流（丁T3 接入②，§2.4 裁3 的安全面；丁T6 复评按契约补齐「转向」）**：
//   终端上有待决对话框时，自由文本被 TUI 当成键盘输入理解——多选对话框里
//   **Enter = 切换高亮项**（问题 6 实锤：composer 打自由文本 → 选项 1 被勾选/反勾）、
//   审批对话框里更可能被读成选项（kimi 误批准实锤）。本组件挂载/状态跃迁时探测两类
//   卡片的在场（与两张卡自己拉的是同两个端点），在场则按**四态**分流：
//   - **`questionFreeText`（问答在场 + answerable + 单题 + 非多选 + 工具已验）** →
//     placeholder 承诺**真发送**语义；发送**转向**卡内同一条 freeText 出口
//     （`sessionQuestionAnswer(action="freeText")`，端点自 T5 起存在）——不再是
//     拦截（§2.4 裁3「三个入口，同一个作答序列出口」；丁T3 原实现因序列未定案而
//     拦截，T5 定案后由本批合并前复评收口）。成功后与 delivered 同口径清空输入框；
//   - **`questionBlocked`（未验工具 / 多题 / 多选）** → 拦截（不假装能发：
//     未验不出键；多题只读卡 §2.3；多选屏自由作答行判据不匹配，后端恒 409）；
//   - **审批卡 / 计划待确认在场（`approve`）** → 拦截 + 「终端等待审批，请用卡片按钮」；
//   - **`none`** → 原 `sessionSend` 路径不变。
//   **转向路径不带附件标记行**（自由作答是「回答」不是「消息」，走字符通道、无
//   `[mobile]` 签名）；带附件时**拦截**而不是静默丢（静默丢用户内容是坏体验）。
//   **发送时刻再探一次**（快照可能陈旧：挂载后对话框才出现）——探针本身是前端
//   尽力而为的第二道闸，**权威守卫**在后端注入时刻的屏读（接入①覆盖控制类；
//   普通消息的屏读守卫不在本任务范围，见未覆盖面申报）。
//   探针失败（网络异常）→ 按「不在场」放行（能力缺失不阻断，与后端
//   `blocks_control_injection` 同裁决）；代价是弱网下退化为无分流，如实申报。
import { useCallback, useEffect, useRef, useState } from "react";
import { formatInvokeError } from "@/lib/invokeError";
import i18n from "@/i18n";
import type { ClipboardEvent as ReactClipboardEvent } from "react";
import { CircleHelp, Plus } from "lucide-react";
import {
  ApiError,
  fetchApproveOptions,
  fetchChannel,
  fetchQueue,
  fetchSendInfo,
  fetchSessionQuestion,
  queueJump,
  queueRetract,
  questionAnswerErrorCopy,
  sessionQuestionAnswer,
  sessionSend,
  uploadAttachment,
  type ChannelInfo,
  type QueueItemView,
  type SendInfo,
} from "./api";
import { ChannelBwNote } from "./FilePanel";
import { etaRemainingText, formatBytes, transferRateBps, type TransferSample } from "./board-logic";
// Codex APP 原生发送（Desktop IPC）：actionId 生成与回执 DTO
import {
  fetchCodexQueue,
  newActionId,
  reconcileCodexQueuedReceipt,
  resumeCodexQueue,
  type ActionReceipt,
  type CodexSuccessorSend,
} from "./codexIpcApi";
// S5：发送结果待确认面板（30 秒截止后的接管呈现；核对/人工继续出口）
import CodexSendRecoveryPanel from "./CodexSendRecovery";
// S5：统一报告入口（R5 前身）——payload 只含安全引用
import { reportIssue, reportIssueSummary, type ReportIssueRef } from "@/lib/reportIssue";

interface MessageComposerProps {
  /** 发送回执触发的队列刷新信号（2026-10-20 复审 P2）：本地队列可见性不依赖
   *  Desktop 必须产生新帧——queued / desktop_ipc 回执都会让队列视图变化。 */
  onQueueChanged?: () => void;
  /** S5：发送静态门结论上报（send-info 是 send_gate_static 的展示面单点）——
   *  SessionDetail 据此驱动能力横幅（可尝试时不再宣称「暂不能远程发送」）。
   *  判据：injectable=true，或 reasonCode 非两能力码（native_pending 等非能力
   *  拒绝不算「不能发送」的能力结论）。拉取失败不回调（横幅维持原状，不猜）。 */
  onSendGate?: (attemptable: boolean) => void;
  /** F8：send-info 外部刷新信号（单调递增 tick；0 = 尚无信号不拉）。触发源 =
   *  同页打断受理/恢复队列（队列暂停态可能翻转）与投影能力档位翻转（设置页
   *  关闭/重开自动适配）——事件驱动重拉，不做时刻轮询、不引 Tauri channel。 */
  refreshSendInfoSignal?: number;
  /** 会话（本组件只消费 id；结构化类型，完整 Session 可直接传入） */
  session: { id: string };
  /** H12：Desktop(App) 形态禁用终端审批/问答探针（App 发送期间零 TUI 请求）。 */
  disableTerminalProbes?: boolean;
}

/** 待发附件条目（组件内态）：status=uploading → ready/failed；
 *  path = 服务端落盘后的绝对路径（仅 ready 有）；
 *  loaded/total = 上行进度（Task 10 §C5，仅 uploading 且收到过进度事件才有——
 *  total 未知（事件未到/事件报 0）→ null，chip 只显示已传字节，不编假百分比） */
type PendingAttachment = {
  id: string;
  name: string;
  isImage: boolean;
  status: "uploading" | "ready" | "failed";
  path?: string;
  error?: string;
  loaded?: number;
  total?: number | null;
  /** 瞬时速率估算（字节/秒；样本不足 → null——「预计剩余」文案的开关） */
  rateBps?: number | null;
};

/** 回执条状态（与 SendResult 对应 + 网络层 ApiError 归入 failed；
 *  gone = 中性收敛（评审裁决）：条目经复核确认已离开队列——大概率已被送达，
 *  不标失败红色、不带「（可重试）」，防止用户重发造成重复注入） */
type Receipt =
  | { kind: "delivered" }
  | {
      /** D7/T3 中性回执（验收问题 #5）：已投递未确认——消息已被 TUI 收进内部
       *  队列，非失败、无重试入口（重试 = 双发且 TUI 那份无法撤回） */
      kind: "submitted";
    }
  | {
      kind: "queued";
      itemId: number;
      position: number;
      /** 排队正文（2026-09-20「修改」按钮）：发送时就在手上，随回执携带——
       *  「修改」确认出队后据此放回输入框，零额外请求。条目内容在队内不可变 */
      content: string;
    }
  | { kind: "failed"; error: string }
  | { kind: "gone"; message: string }
  | {
      /** 丁T3 接入②：**被分流拦截**（终端卡片在场，本轮未发送）。与 failed 分列
       *  的理由：拦截不是失败——没有任何注入尝试、没有「可重试」语义（重按发送
       *  在同一形态下仍会被拦），文案是**指路**（去卡片按钮 / 去卡片输入框）。
       *  渲染配色同 gone（中性 slate），避免红 chip 暗示「操作出错」。 */
      kind: "blocked";
      message: string;
    }
  | {
      /** **Desktop IPC 原生回执**（Codex APP）：按 delivery 分诊渲染。
       *  unknown_delivery 显示「投递结果未知，请在 Desktop 核对」，**无重试语义**
       *  （S5：不确定结果绝不重试；文本已清空防双投）。
       *  S5：`retriedFrom` = 本动作由「明确未送达后的重试」创建时的原动作 ID
       *  （安全引用，展示前 8 位）——重试创建关联的新动作，不永远重放原失败。 */
      kind: "codex";
      receipt: ActionReceipt;
      retriedFrom?: string | null;
    }
  | {
      /** S5：**发送结果待确认**（设计 §5.2 第五分类）——已可能写出但未取得足够
       *  证据（30 秒截止 / 网络层失败）。结束转圈、保留可见消息和原动作，由
       *  CodexSendRecoveryPanel 接管（检查是否送达 / 报告问题 / 人工继续）；
       *  **绝不**渲染普通重试（结果未知重试 = 双投窗口）。 */
      kind: "codexUnknown";
      actionId: string;
      /** 原文（人工继续的后继重发就是原消息——服务器权威核验） */
      text: string;
      /** 进入面板时的已知理由码（网络异常 → null） */
      reasonCode: string | null;
      /** true = 30 秒前台预算到期呈现 */
      deadline?: boolean;
      /** W2：本动作是原动作人工继续创建的后继时携带原动作 ID（父子关系展示，
       *  面板据此渲染「原动作 → 本后继」关联行） */
      resumedFrom?: string | null;
    }
  | {
      /** **兼容通道回执**（Codex APP 兼容发送）：后端 headless 封套原样透出。 */
      kind: "headless";
      status: "ok" | "queued" | "failed" | "cancelled";
      reason?: string;
    }
  | {
      /** **忙 Thread 留队**（宪法 R2.3）：中性回执带队位；队列查看/撤回由
       *  CodexQueuePanel 承担（本组件不重复渲染队列，也无插队入口）。
       *  actionId 是对账锚（2026-10-22 B1）：轮询对齐 codex 队列视图真值，
       *  动作离开队列（派发/撤回）→ chip 收敛清除，不再常驻误导。 */
      kind: "codexQueued";
      position: number;
      actionId: string;
      /** W2：本动作是原动作人工继续创建的后继时携带原动作 ID（父子关联展示） */
      resumedFrom?: string | null;
    }
  | null;

/** 排队列 3s 轮询间隔（毫秒）：有排队项时刷新队位（与看板轮询同量级） */
const QUEUE_POLL_MS = 3000;

/** S3 前台统一预算的前台面（30 秒）：前端本地单调计时——从点击发送起算，到期
 *  强制结束转圈（不依赖服务端时钟同步）；此后按服务端动作 stage/核对结果呈现
 *  五分类之一。只作用于 Codex APP 路径（Desktop IPC 的统一预算概念；终端注入
 *  路径的慢消费者可达分钟级，不受此约束）。 */
const FRONT_DEADLINE_MS = 30_000;

/** 正文长度上限：与后端 MAX_SEND_CHARS 对齐（服务端超限 400 拒收，前端
 *  maxLength 截断是第一道防线，onChange slice 为 jsdom/旧内核兜底的双保险） */
const MAX_SEND_CHARS = 10000;

/** gone 收敛文案定稿（评审裁决，中性、不带「可重试」）：jump 条目可能已送达
 *  （守卫方 flush 循环刚把队首投出）；retract 只需告知不在队。M5 起存 i18n 键，
 *  消费点经 i18n.t 现取（语言切换后新产生的回执跟随生效语言）。 */
const GONE_JUMP_MESSAGE = "mobile.composer.goneJump";
const GONE_RETRACT_MESSAGE = "mobile.composer.goneRetract";
/** 轮询发现条目消失（2026-09-20）：此前静默清空回执——用户实测「桌面端插队后，
 *  手机端排队提示无声消失」。队列会话级共享、无归属标记，移动端无法区分被送达
 *  还是被其他端处理，故给中性提示留痕；真实归属需后端审计回传，边缘场景不做 */
const GONE_POLL_CONSUMED_MESSAGE = "mobile.composer.gonePollConsumed";

/** D8 队列预览截断长度（字符）：逐条预览只给「前十余字符」一眼可辨的量 */
const PREVIEW_MAX_CHARS = 12;

/** 终端卡片在场态（丁T3 接入②，§2.4 裁3；丁T6 复评扩为**四态**——原三态
 *  `"none" | "question" | "approve"` 丢失了「这个问答卡能不能收自由文本」的信息，
 *  导致所有问答在场都被拦截、composer 永远无法按裁3 走「同一个作答出口」）。
 *
 *  - `approve`：审批卡或计划待确认在场（/session-approve-options 的 `available=true`，
 *    含 planPending 形态——那是「等一个计划确认」，放行自由文本在 kimi 上会被读成
 *    批准）。**拦截**（裁3 安全面）。
 *  - `questionFreeText`：问答在场 **且可远程自由作答**——即 `answerable === true`
 *    **且** `freeText === true`（E4-E6 起四家定案，见 api.ts 的 `QuestionInfoView.freeText`）
 *    **且** `questions.length === 1`（多题只读卡，§2.3）**且** `!multiSelect`
 *    （多选屏的自由作答行带勾选框，判据不匹配 → 后端恒 409，见
 *    `question::free_text_shape_supported`）。composer 发送**转向**
 *    `sessionQuestionAnswer(action="freeText")`——与卡内输入框**同一条出口**。
 *  - `questionBlocked`：问答在场但**不能**走自由作答（工具未验 / 多题 / 多选）。
 *    **拦截**（不假装能发，引导去终端或卡片按钮）。
 *  - `none`：都不在场（或探针不可用）。原 `sessionSend` 路径。
 */
type TerminalCardPresence = "none" | "questionFreeText" | "questionBlocked" | "approve";

/** 分流文案（丁T3 §2.4 裁3；丁T6 复评按四态改写）：
 *  - 问答在场**可自由作答**：placeholder 承诺**真发送**（发送即调 freeText 端点）；
 *  - 问答在场但不可自由作答（未验工具 / 多题 / 多选）：不承诺——引导终端；
 *  - 审批在场：裁3 的安全面——此类对话框**没有自由作答语义**，放行=误触选项
 *    （kimi 误批准实锤），故拦截并指路卡片按钮。
 *
 *  **成功回执不另编文案**：转向 freeText 成功后复用本组件既有的 receipt 词汇
 *  （`delivered` / `submitted`），按端点自带的 `verified` 三态分派——语义见
 *  `SessionQuestionAnswerResult.verified`（true=屏读到终态锚 / false=读到屏但未见锚
 *  / null=读屏不可用；**后两者都不是失败，是「未确认」**）。 */
const QUESTION_FREETEXT_PLACEHOLDER = "mobile.composer.questionFreePlaceholder";
const QUESTION_BLOCKED_RECEIPT = "mobile.composer.questionBlockedReceipt";
const APPROVE_PRESENT_RECEIPT = "mobile.composer.approvePresent";

/** 附件与「作为回答发送」的冲突拦截（丁T6 复评的用户裁决：**拦截而不是静默丢**）。
 *  理由：自由作答是「回答」不是「消息」——后端 freeText 走**字符通道**（不带签名、
 *  也不带附件标记行），卡内输入框同样没有附件入口。若 composer 带着附件转向
 *  freeText，那些附件**必然被静默丢弃**（用户以为发出去了，agent 看不到路径）——
 *  静默丢用户内容是坏体验（本仓「回执如实」纪律的同一面）。故拦截并说清去处。 */
const FREETEXT_WITH_ATTACHMENT_RECEIPT = "mobile.composer.freetextWithAttachment";

/** W1 来源标记（服务端 compose_injection 注入，非用户正文）：预览与「修改」放回正文
 *  时剥离——不剥则修改重发会二次叠加签名。
 *
 *  **丁T3 裁2 起签名在尾部**（原名在头部）：正则从 `^\[mobile…\]` 改为 `\s*\[mobile…\]$`
 *  ——尾部形态（`{正文} [mobile 设备名]`）。不改为尾部就会在预览里残留签名、在
 *  「修改」回填时把签名当正文带回输入框（重发即二次叠加）。
 *  容错：正文里偶然出现 `[mobile …]` 字样但**不在尾部**时不剥（与 Rust 侧
 *  `normalize::strip_mobile_signature` 同口径：只剥形态完整的尾签名）。 */
const MOBILE_SIGNATURE_RE = /\s*\[mobile[^\]]*\]$/;
/** 附件内联标记（文件池既有约定）：<image|file path="…"> → 预览中替换为 [附件]。
 *  展示标记文案走 i18n 键（M5；仅展示用，「修改」放回正文不受影响） */
const ATTACH_MARKUP_RE = /<(?:image|file)\s+path="[^"]*">/g;

/** 剥离尾部 `[mobile 设备名]` 签名（正则不命中则原样返回）：预览与修改放回共用。
 *  前后空白一并剥（服务端产出形态为 `{正文} [mobile X]`）。 */
function stripMobileSignature(content: string): string {
  return content.replace(MOBILE_SIGNATURE_RE, "");
}

/** 队列条目预览（D8）：剥尾部 [mobile …] 签名 → 附件标记替换 [附件] → 截前 12 字符
 *  （不足则全显）。仅用于展示；「修改」放回正文只剥签名、保留附件标记行 */
function queueItemPreview(content: string): string {
  const body = stripMobileSignature(content).replace(
    ATTACH_MARKUP_RE,
    i18n.t("mobile.composer.attachMark")
  );
  return body.length > PREVIEW_MAX_CHARS ? `${body.slice(0, PREVIEW_MAX_CHARS)}…` : body;
}

/** S5：NotSent 理由码（S2 枚举单源的移动端词表）→ 用户可读文案。词表内给**出口
 *  指引**（不只是「失败，请重试」）；词表外返回 null → 回落 {{reason}} 模板（不猜，
 *  绝不渲染 raw i18n 键）。 */
function notSentReasonCopy(reasonCode: string | null): string | null {
  switch (reasonCode) {
    case "queue_paused":
      return i18n.t("mobile.codex.composer.reasonQueuePaused");
    case "grant_consumed":
      return i18n.t("mobile.codex.composer.reasonGrantConsumed");
    case "native_pending":
      return i18n.t("mobile.codex.composer.reasonNativePending");
    case "capability_unverified":
      return i18n.t("mobile.codex.composer.reasonCapabilityUnverified");
    case "capability_unavailable":
      return i18n.t("mobile.codex.composer.reasonCapabilityUnavailable");
    default:
      return null;
  }
}

/** 行按 id upsert（乐观插入/更新共用的收敛入口）：命中替换、未命中尾部追加——
 *  发送入队的即时反馈与复核保守恢复都经它把行放回列表，3s 轮询随后以服务端
 *  权威列表整表覆盖 */
function upsertQueueItem(items: QueueItemView[], item: QueueItemView): QueueItemView[] {
  const idx = items.findIndex((i) => i.id === item.id);
  if (idx < 0) return [...items, item];
  const next = items.slice();
  next[idx] = item;
  return next;
}

export default function MessageComposer({
  session,
  onQueueChanged,
  onSendGate,
  refreshSendInfoSignal = 0,
  disableTerminalProbes = false,
}: MessageComposerProps) {
  // 可用性：sendInfo=null 且未就绪 → 不渲染（加载中 / 拉取失败 / 403）
  const [sendInfo, setSendInfo] = useState<SendInfo | null>(null);
  const [infoReady, setInfoReady] = useState(false);
  const [text, setText] = useState("");
  /** 输入框 ref：「修改」确认出队后把正文放回输入框时聚焦（移动端直接可改） */
  const inputRef = useRef<HTMLTextAreaElement>(null);
  /** 在途上传的中断器（id → controller）：上传中移除 chip 时中断传输（XHR abort） */
  const uploadCtrlsRef = useRef(new Map<string, AbortController>());
  /** 上行速率估算的首样本（id → 样本）：进度回调里取样（非渲染期，purity 纪律），
   *  供 chip「预计剩余」文案——Task 10 §C5 */
  const uploadFirstSampleRef = useRef(new Map<string, TransferSample>());
  /** 隐式文件选择器 ref：「+」钮 click 转发 */
  const fileInputRef = useRef<HTMLInputElement>(null);
  /** 待发附件（2026-09-20）：uploading → ready（含落盘绝对路径）/ failed；
   *  发送时仅 ready 的拼内联标记行，failed 不上送（用户裁决：不做通用美化，
   *  附件路径是给 agent 读的） */
  const [attachments, setAttachments] = useState<PendingAttachment[]>([]);
  /** 附件「?」说明展开态（2026-09-20 知情披露）：点开才显示，不平铺常驻 */
  const [attachHintOpen, setAttachHintOpen] = useState(false);
  /** 会话无项目目录（服务端 404 no_cwd 一次即知）：禁用上传钮（与 R5 禁用口径同源） */
  const [noCwd, setNoCwd] = useState(false);
  // 通道能力（Task 10 §C5 带宽横幅，装饰）：挂载拉一次，任何失败静默 → 无横幅不
  // 阻塞输入区（与 FilePanel 同一口径与端点）。纯装饰——读 Host 推断、可被伪造、
  // 永不进任何安全判定（G1 推论③）；判据与文案由 FilePanel 导出的 ChannelBwNote
  // 统一承担（复评 I-2：附件区也要出横幅，但绝不新造第二套文案）
  const [channel, setChannel] = useState<ChannelInfo | null>(null);
  useEffect(() => {
    let alive = true;
    void fetchChannel().then((c) => {
      if (alive) setChannel(c);
    });
    return () => {
      alive = false;
    };
  }, []);
  const [sending, setSending] = useState(false);
  // 插队/撤回进行中（与发送互斥，防连点）
  const [busy, setBusy] = useState(false);
  const [receipt, setReceipt] = useState<Receipt>(null);
  /** **本次用户动作的 actionId**（Codex APP 原生发送的账本锚）。
   *
   *  一次用户动作生成一次：用 ref 而非 state——双击在同一渲染帧内会两次进入
   *  handleSend，state 闭包里的旧值还是 null，两次各生成一个 ID；ref 同步读写，
   *  第二次点击必然命中第一次写入的同一个 ID（服务端按 actionId 去重 = 不双投）。
   *  生命周期：明确终态（delivered/submitted/queued/desktop_ipc/headless）即清空；
   *  unknown_delivery / 网络异常保留（结果未知的核对锚，绝不换 ID 重投）；换会话清空。 */
  const codexActionIdRef = useRef<string | null>(null);
  /** F6：核对锚对应的**逻辑发送载荷**（与 codexActionIdRef 同生命周期）——
   *  运输重试的判据从「ref 非空」收紧为「ref 非空 ∧ 载荷相同」：上一动作结果未知
   *  时，同载荷重按 = 同一次逻辑发送的运输重试（同 ID，服务端账本去重防双投）；
   *  **换了正文 = 新逻辑发送**，必须换新 ID（否则同 ID 异载荷 → 服务端
   *  action_conflict，且旧锚永不清 → 死锁）。 */
  const codexActionPayloadRef = useRef<string | null>(null);
  /** S5+F6：30 秒前台预算的**当前发送**计时器把手（{epoch, timer}；闭包内另有
   *  逐发送的本地句柄——共享 ref 只供卸载清理兜底，读写一律按 epoch 归属）。
   *  到期 = 呈现所有权移交待确认面板：此后迟到的原始回执不再覆盖面板
   *  （核对/队列视图才是权威呈现——面板自己可随时核对账本）。 */
  const deadlineTimerRef = useRef<{ epoch: number; timer: ReturnType<typeof setTimeout> } | null>(
    null
  );
  /** S5：发送代际（每次 handleSend 递增）——旧发送闭包的迟到响应/迟到回执在代际
   *  前进后一律作废（不回写新发送的回执槽；「刷新/切会话旧 promise 不回写」在
   *  发送流上的同源防线）。 */
  const sendEpochRef = useRef(0);
  /** Q2（复审 round 6）：当前恢复上下文归属快照——最近一次以「发送结果待确认」
   *  呈现的动作 {actionId, epoch, draftVersion}。人工继续的 onSuccessorResolved
   *  是**父组件回调**，不受面板按 actionId 重挂的局部状态保护：旧面板卸载后，
   *  在途的人工继续 promise 依然会调用它。回执槽建立 codexUnknown 的同一刻在此
   *  登记；回调落地先核对「当前恢复动作 + 发送代际」仍归属发起时的原动作，不一致
   *  直接作废（在**全部副作用之前**——锚释放/回执覆盖/清草稿一概不发生）。 */
  const codexRecoveryOwnerRef = useRef<{
    actionId: string;
    epoch: number;
    draftVersion: number;
  } | null>(null);
  /** Q2：草稿版本号——用户编辑正文/附件即前移。后继实际提交的是原动作的原文，
   *  只有「发起人工继续后未再编辑」的草稿（= 仍是实际提交的那份）才允许被
   *  成功/入队清理；在途编辑出的新草稿绝不代清。 */
  const draftVersionRef = useRef(0);
  /** S5：「本条先发送」一次性越队标志（队列暂停时的二选一之一；arm 后下一次
   *  发送携带 bypassPausedQueue=true，消费即清——授权失效重放会得 grant_consumed） */
  const [bypassNext, setBypassNext] = useState(false);
  /** S5：重试关联——明确未送达后用户点「重试」时记录原动作 ID（安全引用，展示前 8 位） */
  const retriedFromRef = useRef<string | null>(null);
  /** S5：报告入口的「诊断服务未开启」展开态（未部署 = sink 恒 null → 如实呈现） */
  const [reportUnavailable, setReportUnavailable] = useState(false);
  /** S5：最近一次报告请求的安全引用（复制摘要出口用；只含引用不含正文） */
  const [lastReportRef, setLastReportRef] = useState<{
    errorCode: string | null;
    actionId: string | null;
  } | null>(null);
  /** S5：恢复队列进行中（防连点） */
  const [resumingQueue, setResumingQueue] = useState(false);
  /** D8 完整队列表（/session-queue 的 items）：逐条渲染的数据源——不限本端发送
   *  的条目（桌面端/他端排的队同样可见可操作）。乐观行（发送即时反馈/复核保守
   *  恢复）与权威列表（挂载拉取/3s 轮询）都收敛到这里 */
  const [queueItems, setQueueItems] = useState<QueueItemView[]>([]);
  /** 本地队列认知最近一次翻转的时刻（T4 复评 I1/M1 时序防御）：发送乐观入队、
   *  插队送达、撤回确认/404、复核三分支任一落地即前移。tick 快照若**发起**早于
   *  该时刻，说明它反映的是翻转前的服务端视图——既不可整表覆盖（会抹乐观行 /
   *  复活已确认出队的幽灵行），也不可据其判 gone（会把「刚入队」误判成已送达，
   *  唯一条目时更会停摆轮询 → 用户重发 = 重复注入，本项目头号禁忌）。作废一份
   *  快照的代价 ≤ 3s 自愈，方向一律保守（同毫秒按更旧处理，用 <=） */
  const queueMutatedAtRef = useRef(0);
  /** codexQueued 回执最近一次设置/更换的时刻（2026-10-22 复审 Important-1，与
   *  queueMutatedAtRef 同款 I1 时序防御）：忙线程连发时新 receipt 以新 actionId
   *  原子换入，此前发起的 /codex/queue tick 快照必不含新锚——若放行会把「刚入队」
   *  误判成已离队，清掉新 chip 且停摆轮询（无自愈）。tick 快照若发起早于该时刻
   *  一律作废（同毫秒按更旧处理，用 <=），作废一份 ≤ 3s 自愈，方向保守 */
  const codexReceiptSetAtRef = useRef(0);
  /** 修改重发只入队标志（D6）：「修改」确认出队后置 true，下一次发送携带
   *  queueOnly=true 并清除（消费即清——失败重试不再带标志，回归普通发送语义）。
   *  保守语义：修改后的重发一律入队，用户手动改字不清除标志。入队后的放行节奏：
   *  会话转闲跃迁后事件臂即时放行；已空闲且无跃迁时由 60s 周期兜底放行（可达
   *  分钟级）。队列存储不携带该标志（后端仅影响入队决策，flush 循环对
   *  queueOnly 项与普通队列项同权） */
  const [queueOnlyNext, setQueueOnlyNext] = useState(false);
  /** 终端卡片在场态（丁T3 接入②）：挂载/换会话拉一次 + **发送时刻复探**（快照会
   *  陈旧——对话框可能在挂载之后才出现，而复探是发送前的最后一道前端闸）。
   *  探针失败 = 不在场（能力缺失不阻断，与后端同裁决；见文件头注释）。 */
  const [cardPresence, setCardPresence] = useState<TerminalCardPresence>("none");

  /** 探测两类卡片在场（两张卡自己拉的是同两个端点——ApproveCard 的
   *  `available`（含 planPending）/ QuestionCard 的 `available`）。二者并行，
   *  任一失败按不在场处理（单点失败不连坐另一路）。
   *
   *  **丁T6 复评：问答在场细分两态**（原实现只回 `"question"`，丢失了「能不能远程
   *  自由作答」这层信息——见 `TerminalCardPresence` 的设计注）。判据**与后端同源**
   *  （不前端自造规则）：三个字段全来自同一份 `fetchSessionQuestion` 载荷，
   *  后端 `session_question_answer` 的 `action_supported` 门用的是同一组判据
   *  （`free_text_supported(tool)` × `free_text_shape_supported(q)` × 单题）。
   *
   *  四条判据（全部满足才算 `questionFreeText`）：
   *  1. `answerable !== false`（缺省按 true——旧后端兼容，见 api.ts:610 注释）；
   *  2. `freeText === true`（批次戊 E4-E6 起四家定案：claude Type something /
   *     codex Tab 备注 / kimi Other 行 / opencode own answer——各走各家阶段机；
   *     缺省/旧后端按 false 处理，不假装能发）；
   *  3. `questions.length === 1`（多题只读卡，§2.3：翻页键序未测，后端同样 409）；
   *  4. `!questions[0].multiSelect`（多选屏的自由作答行带勾选框，定位判据不匹配 →
   *     后端恒 409，见 `question::free_text_shape_supported` 的实机证据）。 */
  const probeCardPresence = useCallback(async (): Promise<TerminalCardPresence> => {
    // H12：App 形态 = 投影/原生端点唯一来源，**零终端探针**（含发送前复探）。
    if (disableTerminalProbes) return "none";
    const [approve, question] = await Promise.all([
      fetchApproveOptions(session.id).catch(() => null),
      fetchSessionQuestion(session.id).catch(() => null),
    ]);
    // 审批优先（裁3 的安全面更重：审批框放行自由文本 = 误触选项/误批准）
    if (approve?.available === true) return "approve";
    if (question?.available !== true) return "none";
    // 防御：旧后端/异常载荷可能没有 questions 数组（类型是必填，运行时仍设防——
    // 本函数在挂载 effect 的 `.then` 里跑，抛异常会变成未处理的 rejection）
    const q = question.questions ?? [];
    const canFreeText =
      question.answerable !== false &&
      question.freeText === true &&
      q.length === 1 &&
      !q[0].multiSelect;
    return canFreeText ? "questionFreeText" : "questionBlocked";
  }, [session.id, disableTerminalProbes]);

  // 挂载拉取一次输入区可用性；任何失败静默保持隐藏
  useEffect(() => {
    let alive = true;
    setInfoReady(false);
    // 换会话即弃修改重发标志（D6）：queueOnlyNext 是上一个会话「修改」的遗愿，
    // 不得泄漏到新会话的首发（保守方向虽无害，语义上仍属错位）
    setQueueOnlyNext(false);
    // S5：越队标志/报告展开态同属上一会话的遗愿，一并清
    setBypassNext(false);
    setReportUnavailable(false);
    // actionId 同理——一次用户动作不跨会话，切换即清（下一条消息是新动作）
    codexActionIdRef.current = null;
    codexActionPayloadRef.current = null;
    retriedFromRef.current = null;
    // Q2：恢复归属快照同属上一会话的现场，一并清
    codexRecoveryOwnerRef.current = null;
    if (deadlineTimerRef.current !== null) {
      clearTimeout(deadlineTimerRef.current.timer);
      deadlineTimerRef.current = null;
    }
    fetchSendInfo(session.id)
      .then((info) => {
        if (!alive) return;
        setSendInfo(info);
        setInfoReady(info !== null);
        // S5：静态门结论上报（能力横幅的「可尝试」唯一来源）
        if (info !== null) {
          onSendGate?.(sendGateAttemptable(info));
        }
      })
      .catch(() => {
        if (alive) setInfoReady(false);
      });
    return () => {
      alive = false;
    };
  }, [session.id, onSendGate]);

  // F8：外部信号（同页打断受理 / 恢复队列 / 投影能力档位翻转）→ 事件驱动重拉
  // send-info（队列暂停态、静态门结论都可能已变）。失败静默——下一次信号或重挂
  // 自愈；403（设备失效）返回 null 跳过（全局判废通道处理）。
  useEffect(() => {
    if (refreshSendInfoSignal <= 0) return;
    let alive = true;
    fetchSendInfo(session.id)
      .then((info) => {
        if (!alive || info === null) return;
        setSendInfo(info);
        onSendGate?.(sendGateAttemptable(info));
      })
      .catch(() => {
        /* 单次刷新失败静默 */
      });
    return () => {
      alive = false;
    };
  }, [refreshSendInfoSignal, session.id, onSendGate]);

  // D8：挂载/换会话拉一次完整队列——进入会话即见既有队列（桌面端/他端排的队也要
  // 能看到）；换会话先清列表防串台。拉取失败静默（列表留空），排队回执在场的 3s
  // 轮询通道随后自愈
  useEffect(() => {
    let alive = true;
    setQueueItems([]);
    fetchQueue(session.id)
      .then((items) => {
        if (alive) setQueueItems(items);
      })
      .catch(() => {
        /* 挂载拉取失败静默：留空待轮询/复核自愈 */
      });
    return () => {
      alive = false;
    };
  }, [session.id]);

  // 丁T3 接入②：挂载/换会话探一次卡片在场（placeholder 分流的数据源）。
  // 成本申报：本组件因此每次挂载/换会话多打**两发** GET（/session-approve-options
  // 与 /session-question）——这两条请求**卡片自己也会打**（ApproveCard / QuestionCard
  // 各自挂载拉一次）。为什么不把在场态从 SessionDetail 传下来：那需要在 SessionDetail
  // 里把「两张卡内部各自的拉取结果」提升为共享状态（两卡目前是自拉自用、失败自隐的
  // 独立组件），改动面覆盖两张卡 + 详情页挂载门，超出 T3 范围；而自拉的代价只是
  // **每挂载两发轻量 GET**（后端都是短查询；且本组件只在 `available=true` 时改变
  // 行为，不会因多打而误判）。发送时刻另有复探（见 handleSend）——两发探针的成本
  // 换来「对话框出现后不必等重挂即被发现」。
  useEffect(() => {
    let alive = true;
    setCardPresence("none");
    void probeCardPresence().then((p) => {
      if (alive) setCardPresence(p);
    });
    return () => {
      alive = false;
    };
  }, [probeCardPresence]);

  // D8 排队列 3s 轮询（复用既有 3s 通道，不另起轮询）：有排队回执 **或** 列表非空
  // 即持续；列表变空（且无排队回执）→ 停轮询。依赖是布尔量而非列表本体——位次
  // 变化/整表覆盖不重建定时器。setReceipt 用函数式更新且未变时返回原引用
  const queueTracked = receipt?.kind === "queued" || queueItems.length > 0;
  useEffect(() => {
    if (!queueTracked) return;
    // I2：换会话/停轮询触发 effect 重建时置 false——此前会话的在途 tick 响应
    // 一律作废（旧会话的行不得经无条件 setQueueItems 落入新会话，与挂载 effect
    // 的 alive 守卫同款）
    let alive = true;
    const iv = setInterval(() => {
      // I1：先记快照发起时刻，再发 GET——响应到货时与本地认知翻转时刻比对
      const issuedAt = Date.now();
      void fetchQueue(session.id)
        .then((items) => {
          if (!alive) return; // I2：换会话/停轮询后的在途响应作废
          if (issuedAt <= queueMutatedAtRef.current) return; // I1/M1：早于本地认知翻转的旧快照整份作废（防御见 ref 声明处）
          setQueueItems(items);
          setReceipt((prev) => {
            if (prev?.kind !== "queued") return prev;
            const mine = items.find((i) => i.id === prev.itemId);
            if (!mine) {
              // 已被 flush 送达 / 他端插队 / 他端撤回 → 回执收敛（2026-09-20 前
              // 是静默 return null，用户实测困惑；队列会话级共享、无归属标记，
              // 移动端分不清谁触发，故落中性提示留痕）；行随权威列表一并移除
              return { kind: "gone", message: GONE_POLL_CONSUMED_MESSAGE };
            }
            return mine.position === prev.position
              ? prev
              : {
                  ...prev,
                  position: mine.position,
                };
          });
        })
        .catch(() => {
          /* 单次轮询失败忽略，下轮再试 */
        });
    }, QUEUE_POLL_MS);
    return () => {
      alive = false;
      clearInterval(iv);
    };
  }, [queueTracked, session.id]);

  // codexQueued 回执收敛（2026-10-22 B1）：通用 session-queue 不含 codex 动作，
  // 该 chip 的权威真值在 /codex/queue——单独按 actionId 对齐该视图。动作离开
  // 队列（派发/撤回）→ chip 清除（派发结果经会话轮询自然呈现，不再常驻误导）；
  // 位次变化 → 跟随。依赖布尔量而非回执本体，位次更新不重建定时器。
  const codexQueuedTracked = receipt?.kind === "codexQueued";
  useEffect(() => {
    if (!codexQueuedTracked) return;
    let alive = true;
    const iv = setInterval(() => {
      // I1（复审 Important-1，与通用队列 queueMutatedAtRef 同款）：先记快照发起
      // 时刻——响应到货时若发起早于回执设置/更换时刻，说明它不含新锚，作废
      const issuedAt = Date.now();
      void fetchCodexQueue(session.id)
        .then((view) => {
          if (!alive) return;
          if (issuedAt <= codexReceiptSetAtRef.current) return;
          // 403（设备失效）→ null：无法对账，保持现状（下轮再试）
          if (view === null) return;
          setReceipt((prev) => {
            if (prev?.kind !== "codexQueued") return prev;
            return reconcileCodexQueuedReceipt(prev, view);
          });
        })
        .catch(() => {
          /* 单次轮询失败忽略，下轮再试 */
        });
    }, QUEUE_POLL_MS);
    return () => {
      alive = false;
      clearInterval(iv);
    };
  }, [codexQueuedTracked, session.id]);

  const injectable = sendInfo !== null && sendInfo.injectable;

  /** S5：发送静态门的「可尝试」判据（send-info 展示面单点的消费；不复刻能力策略
   *  ——只区分「两能力码拒绝」与「其余一切」）。native_pending / queue_paused 等
   *  拒绝是**操作层**的（处理卡片/排队即可），不是「不能远程发送」的能力结论。 */
  function sendGateAttemptable(info: SendInfo): boolean {
    if (info.injectable) return true;
    return (
      info.reasonCode !== "capability_unverified" && info.reasonCode !== "capability_unavailable"
    );
  }

  /** S5：统一报告入口。诊断出口未接线（R5 未部署）→ 如实展开「诊断服务未开启 +
     复制摘要」，绝不显示提交成功。payload 只含安全引用（无正文/日志/路径）。 */
  const handleReportIssue = useCallback(
    (errorCode: string | null, actionId: string | null) => {
      const ref: ReportIssueRef = {
        tool: "codex",
        surface: "composer_send",
        sessionId: session.id,
        actionId,
        errorCode,
        adaptSummary: null,
      };
      if (!reportIssue(ref)) {
        setLastReportRef({ errorCode, actionId });
        setReportUnavailable(true);
        return;
      }
      setReportUnavailable(false);
    },
    [session.id]
  );

  const copyReportSummary = useCallback(async () => {
    if (lastReportRef === null) return;
    const text = reportIssueSummary({
      tool: "codex",
      surface: "composer_send",
      sessionId: session.id,
      actionId: lastReportRef.actionId,
      errorCode: lastReportRef.errorCode,
      adaptSummary: null,
    });
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      /* 剪贴板不可用：静默（复制失败不伪造成功） */
    }
  }, [lastReportRef, session.id]);

  /** S5：恢复被打断暂停的发送队列（POST /codex/queue/resume = 唯一解除出口）；
   *  成功后重拉 send-info 刷新暂停态。 */
  const resumeQueue = useCallback(async () => {
    if (resumingQueue) return;
    setResumingQueue(true);
    try {
      await resumeCodexQueue(session.id);
      const info = await fetchSendInfo(session.id);
      setSendInfo(info);
      if (info !== null) onSendGate?.(sendGateAttemptable(info));
      // F8：恢复成功 → 队列视图必然变化（暂停解除、派发接力）——通知同页队列
      // 面板刷新（不依赖 Desktop 必须产生新帧）
      onQueueChanged?.();
    } catch {
      /* 恢复失败静默：暂停态以服务端为准（send-info 重挂/轮询自愈） */
    } finally {
      setResumingQueue(false);
    }
  }, [resumingQueue, session.id, onSendGate, onQueueChanged]);

  const handleSend = useCallback(async () => {
    const body = text.trim();
    if (!body || sending || busy || sendInfo === null || !sendInfo.injectable) return;
    if (attachments.some((a) => a.status === "uploading")) return; // 上传中禁发（防消息先于落盘）
    setSending(true);
    // S5：本次发送的代际（迟到响应作废判据；见 sendEpochRef 声明处）
    const myEpoch = ++sendEpochRef.current;
    const staleEpoch = () => sendEpochRef.current !== myEpoch;
    // ===== F6：点击即建立 30 秒前台总预算（S3 前台统一预算的前台面）=====
    // 预算覆盖**整次编排**：前置卡片探测 + session-send 消费同一窗——此前计时器
    // 在探测之后才武装，探测挂起即可无限转圈。计时器/到期标记/回执写入全部按
    // **本次发送闭包**归属：旧发送的迟到收尾（成功/失败/finally）碰不到新动作的
    // 计时器、转圈与回执槽（finally 一并承担所有出口的计时器清理）。
    // 附件标记行与 actionId 在点击时一次算定（handleSend 闭包已把 text/attachments
    // 定格在点击帧——与探测后计算逐字等价）。
    const markup = attachments
      .filter((a) => a.status === "ready" && a.path)
      .map((a) => (a.isImage ? `<image path="${a.path}">` : `<file path="${a.path}">`))
      .join("\n");
    const fullText = markup ? `${text}\n${markup}` : text;
    // Q2（复审 round 7）：草稿版本在**提交时点**冻结（与 fullText 同刻定格）——
    // textarea 在发送等待期间仍可编辑，回执/计时器落地时再采样 draftVersionRef
    // 会把等待期编辑出的新稿版本记到本次动作头上，后继人工继续的版本门随之
    // 误判「草稿未变」清掉未提交的新稿。handleSend 自身不动版本号（bump 全在
    // onChange/addFiles/removeAttachment/handleEdit），此刻读到的就是提交内容
    // 对应的版本；四个 Unknown 登记点一律携带本值。
    const submittedDraftVersion = draftVersionRef.current;
    const needsActionId = sendInfo?.channels?.some((c) => c.startsWith("headless_codex_")) ?? false;
    // F6：actionId 按逻辑发送的载荷键控（见 codexActionPayloadRef 声明处）——
    // 上一动作结果未知（核对锚在 ref）时：同载荷重按 = 运输重试（同 ID）；换正文
    // = 新逻辑发送（新 ID——否则撞 action_conflict 且旧锚死锁）。
    let actionId: string | undefined;
    if (needsActionId) {
      const prevId = codexActionIdRef.current;
      if (prevId !== null && codexActionPayloadRef.current === fullText) {
        actionId = prevId;
      } else {
        actionId = newActionId();
        codexActionIdRef.current = actionId;
        codexActionPayloadRef.current = fullText;
      }
    }
    // S5：Codex 路径武装标记（catch 臂可见；null = 本次发送非 Codex 路径/未武装）
    const armedCodexActionId: string | null = actionId ?? null;
    // F6：本次发送专属的 30 秒计时器（闭包归属；共享 ref 只留卸载清理把手）
    let myExpired = false;
    let myTimer: ReturnType<typeof setTimeout> | null = null;
    const clearMyDeadline = () => {
      if (myTimer !== null) {
        clearTimeout(myTimer);
        myTimer = null;
      }
      if (deadlineTimerRef.current?.epoch === myEpoch) deadlineTimerRef.current = null;
    };
    if (actionId !== undefined) {
      myTimer = setTimeout(() => {
        myTimer = null;
        if (deadlineTimerRef.current?.epoch === myEpoch) deadlineTimerRef.current = null;
        // 代际已前进（用户已开新发送）→ 本计时器作废，不碰任何状态
        if (staleEpoch()) return;
        myExpired = true;
        setSending(false);
        setReceipt({
          kind: "codexUnknown",
          actionId,
          text: fullText,
          reasonCode: null,
          deadline: true,
        });
        // Q2：待确认现场自此归属本动作（回调落地据此核对）
        codexRecoveryOwnerRef.current = {
          actionId,
          epoch: myEpoch,
          draftVersion: submittedDraftVersion,
        };
      }, FRONT_DEADLINE_MS);
      deadlineTimerRef.current = { epoch: myEpoch, timer: myTimer };
    }
    try {
      // ===== 丁T3 接入②：发送时刻**复探**卡片在场（§2.4 裁3 的安全面）=====
      // 挂载时探到的是快照；对话框可能在挂载之后才出现（模型提问/请求批准是回合
      // 内任意时刻的事），而放行自由文本的代价是**误触选项**（多选框 Enter=切换
      // 高亮项 / 审批框被读成选择），故发送前再探一次、以此刻结论为准。
      //
      // 分流矩阵（丁T6 复评按四态落地；**回执文案与卡内路径同口径**，不自编一套）：
      // - `questionFreeText`（问答在场 + answerable + 单题 + 非多选 + 工具已验）→
      //   **转向**：调 `sessionQuestionAnswer(action="freeText")`——与卡内输入框
      //   **同一条出口**（§2.4 裁3「三个入口，同一个作答序列出口」）；
      // - `questionBlocked`（问答在场但不可自由作答：未验工具 / 多题 / 多选）→
      //   **拦截**（多题只读卡 §2.3；未验不出键）——不假装能发；
      // - `approve`（审批 / 计划待确认在场）→ **拦截**（裁3 安全面：此类对话框没有
      //   自由作答语义，放行=误触选项——kimi 误批准实锤）；
      // - `none` → 原 `sessionSend` 路径（零回归）。
      //
      // 拦截时输入框内容**保留**（与 failed 态同口径：用户可复制到卡片输入框或终端）。
      const presence = await probeCardPresence();
      // R7（F6 前端半边）：前探也是异步边界——恢复后先核**发送归属**（代际），
      // 再写 cardPresence 或分流。A 前探挂起、B 先开始后 A 迟到返回（真实可达：
      // api.rs:2589 审批扫描不排除 App，rollout 尾部 proposed_plan 经 plan_pending
      // 在 :2921 返回 available=true）→ A 的拦截结果不得驱动 B 的分流/回执。
      // 只核代际不核本发预算：A 自己超时（面板已接管呈现）后其发送照常继续——
      // 既有 F6 语义（f6_probe：探测晚到 → 发送照发 → 迟到回执不覆盖面板），
      // 本发预算的写状态闸在下游回执点（myExpired）。
      if (staleEpoch()) return;
      setCardPresence(presence);
      if (presence === "approve") {
        setReceipt({ kind: "blocked", message: APPROVE_PRESENT_RECEIPT });
        return;
      }
      if (presence === "questionBlocked") {
        setReceipt({ kind: "blocked", message: QUESTION_BLOCKED_RECEIPT });
        return;
      }
      if (presence === "questionFreeText") {
        // 附件不能随「回答」发送：freeText 走字符通道（不带附件标记行），卡内输入框
        // 也没有附件入口——带了附件转向 freeText 必然**静默丢弃**它们（用户以为发
        // 出去了）。故拦截而不是忽略（理由见常量注）。
        if (attachments.length > 0) {
          setReceipt({ kind: "blocked", message: FREETEXT_WITH_ATTACHMENT_RECEIPT });
          return;
        }
        // 转向自由作答：**不带附件标记行**（自由作答是「回答」不是「消息」——
        // 后端归一后走字符通道，不带 [mobile] 签名，见 api.ts 的
        // `sessionQuestionAnswer` 注释）。
        //
        // **非 2xx 分诊在本臂内单独处理**（不复用外层 catch）：问答端点的错误码在
        // `data.error`（`no_question`/`multi_questions`/`tool_readonly`/`bad_index`），
        // 而 `/session-send` 的失败细节在 `data.reason`——两个端点的载荷字段**不同**
        // （丁T6 复评核出：曾用同一段 `reason` 读取，问答 409 会显示成
        // 「session-question/answer 409」这种对用户无意义的串）。文案走
        // [`questionAnswerErrorCopy`]（与卡内**同一函数**，两入口不漂移）。
        let res: Awaited<ReturnType<typeof sessionQuestionAnswer>>;
        try {
          res = await sessionQuestionAnswer(session.id, "freeText", undefined, text);
        } catch (err) {
          setReceipt({
            kind: "failed",
            error: err instanceof ApiError ? questionAnswerErrorCopy(err) : String(err),
          });
          return;
        }
        // R7：应答返回同为异步边界——迟到结果先核归属（B 已开新发送时，A 的自由
        // 作答回执不得写入 B 的回执槽）；本发预算已到期移交呈现的同样不再写
        //（面板是唯一呈现，回执不得覆盖）
        if (staleEpoch() || myExpired) return;
        if (res.status === "key_sent") {
          // 回执与卡内路径**同口径**（不另编一套文案）：
          // - `verified === true`（屏读到终态锚 = 走完整条闭环）= delivered；
          // - `verified === false`（读到屏但未见终态锚）/ `null`|缺省（读屏不可用）
          //   → **中性** submitted（D7/T3 同一纪律：未确认 ≠ 失败，也**不得**冒充
          //   已完成——`false` 的语义逐字是「已按屏读完成提交，但未在屏上见到完成
          //   回执」，卡内也如实显示）。
          // 两种都算「已投递」→ 与 delivered 同口径清空输入框（留着会让用户以为
          // 没发出去）。**不提供重试语义**（TUI 那份无法撤回，重按 = 重发）。
          setText("");
          setReceipt(res.verified === true ? { kind: "delivered" } : { kind: "submitted" });
        } else {
          // failed：分诊与卡内同源——`aborted:true` 是阶段机中止（带段名，卡内
          // 渲染段名徽标；composer 无该视觉位，展示 `error` 整句原文即可，其中已
          // 含段名与原因），其余是投递失败（可重试）。**不另编文案**。
          setReceipt({ kind: "failed", error: formatInvokeError(res.error, i18n.t) });
        }
        return;
      }
      // D6：修改重发的下一次发送带 queueOnly=true 强制入队，消费即清——本次发送
      // 失败的话，用户重试走的是普通发送语义（后端失败行已退出 pending，可重发）
      const forceQueue = queueOnlyNext;
      setQueueOnlyNext(false);
      // 多行原样上行（trim 只用于判空，不改写正文——归一在服务端）。
      // **actionId / fullText 已在点击时算定**（F6：预算与载荷键控见函数头）。
      // S5：「本条先发送」一次性越队授权（队列暂停二选一之一）——arm 后消费即清；
      // 授权失效重放会得 NotSent(grant_consumed)，前端引导重选而非原样重放。
      const bypass = needsActionId && bypassNext ? true : undefined;
      if (bypass) setBypassNext(false);
      const res = await sessionSend(
        session.id,
        fullText,
        forceQueue ? true : undefined,
        actionId,
        bypass
      );
      // S5：代际已前进（新发送已开）或本次预算已到期移交面板的，本响应一律
      // 作废不回写（计时器清理在 finally，按闭包归属——碰不到新动作的计时器）
      if (staleEpoch()) return;
      if (myExpired) return;
      if (res.status === "delivered") {
        setText("");
        setAttachments([]);
        setReceipt({ kind: "delivered" });
      } else if (res.status === "submitted") {
        // D7/T3 中性回执：注入 Ok + 戳未中 + 屏读无滞留草稿 = 已被 TUI 收进内部
        // 队列。消息已离开前端 → 输入框/附件清空对齐 delivered 口径；不提供重试
        // （TUI 那份无法撤回，重按发送 = 双发——可重试语义仅属于 failed 态）
        setText("");
        setAttachments([]);
        setReceipt({ kind: "submitted" });
      } else if (res.status === "queued") {
        setText("");
        setAttachments([]);
        // I1：本地「该条在队」认知自此成立——此前发起的在途 tick 快照可能不含
        // 本条，不得据此判 gone / 整表覆盖（时序防御见 queueMutatedAtRef 声明处）
        queueMutatedAtRef.current = Date.now();
        setReceipt({
          kind: "queued",
          itemId: res.itemId,
          position: res.position,
          content: fullText,
        });
        // D8 即时反馈：行乐观 upsert（发送后到列表刷新之间不上墙等 3s）；content
        // 用本端原文（此时服务端 compose 后的带签名文本尚未回拉，3s 轮询整表覆盖）
        setQueueItems((prev) =>
          upsertQueueItem(prev, {
            id: res.itemId,
            content: fullText,
            enqueuedAt: Date.now(),
            position: res.position,
          })
        );
      } else if (res.status === "desktop_ipc") {
        // **Desktop IPC 原生回执**（S5 前台五分类）：
        // - accepted = 已受理（显示「已受理/正在执行」，不宣称本轮已完成）；
        // - unknown_delivery = 发送结果待确认 → 待确认面板接管（检查是否送达 /
        //   报告问题；绝不普通重试——结果未知重试 = 双投窗口）；
        // - not_sent/rejected = 明确未送达（保留草稿；重试 = 建新动作并展示关联）。
        if (res.receipt.delivery === "accepted") {
          setText("");
          setAttachments([]);
        }
        // 2026-10-20 复审 P2：Unknown 的「待核对动作」与「下一条新草稿」生命周期
        // 分离——保留回执里的 actionId 供核对，但**新一次发送必须生成新 ID**，
        // 不得复用旧 ID 撞 action_conflict，也绝不自动重发。
        codexActionIdRef.current = null;
        codexActionPayloadRef.current = null;
        if (res.receipt.delivery === "unknown_delivery") {
          setReceipt({
            kind: "codexUnknown",
            actionId: res.receipt.actionId,
            text: fullText,
            reasonCode: res.receipt.reasonCode,
            deadline: false,
          });
          // Q2：待确认现场自此归属本动作（回调落地据此核对）
          codexRecoveryOwnerRef.current = {
            actionId: res.receipt.actionId,
            epoch: myEpoch,
            draftVersion: submittedDraftVersion,
          };
        } else {
          const linkedFrom = retriedFromRef.current;
          retriedFromRef.current = null;
          setReceipt({ kind: "codex", receipt: res.receipt, retriedFrom: linkedFrom });
        }
        onQueueChanged?.();
      } else if (res.status === "codex_queued") {
        // 忙 Thread 留队：消息已进服务端 FIFO（账本锚 = actionId），由调度循环按序派发。
        // actionId 存入回执（B1）：轮询对齐 codex 队列视图真值的对账锚。
        // 先前移对账时刻（复审 Important-1）：在途旧 tick 快照不得清掉新 chip。
        codexReceiptSetAtRef.current = Date.now();
        setText("");
        setAttachments([]);
        codexActionIdRef.current = null;
        codexActionPayloadRef.current = null;
        setReceipt({
          kind: "codexQueued",
          position: res.position,
          actionId: res.actionId,
        });
        onQueueChanged?.();
      } else if (res.status === "headless") {
        // 兼容通道回执（每回合 spawn）：ok/queued 即消息已交给 CLI；失败如实展示原因
        if (res.receipt.status === "ok" || res.receipt.status === "queued") {
          setText("");
          setAttachments([]);
          codexActionIdRef.current = null;
          codexActionPayloadRef.current = null;
        }
        setReceipt({
          kind: "headless",
          status: res.receipt.status,
          reason: res.receipt.reason,
        });
        // 2026-10-20 复审 P2：兼容回执（含超时/失败）同样可能留下 Unknown 与
        // 屏障；revision=null 时没有新帧，必须主动刷新恢复面板，否则用户看不到
        // 「核对后继续」。
        onQueueChanged?.();
      } else {
        setReceipt({ kind: "failed", error: formatInvokeError(res.error, i18n.t) });
      }
    } catch (e) {
      // S5/F6：代际已前进（新发送已开）或本次预算已到期移交面板的迟到异常，
      // 一律作废不回写（计时器清理解释权在 finally，按闭包归属）
      if (staleEpoch()) return;
      if (myExpired) return;
      if (e instanceof ApiError) {
        // S5 分诊：Codex 路径上**非确定性失败**（网络层/5xx/408——代理可能在服务端
        // 已处理后返回错误）= 提交结果未知 → 待确认面板（沿同 ID 只读核对，绝不
        // 换 ID 重投）；确定性拒绝（4xx 状态码在响应在手）才落 failed 可重试。
        const definiteRefusal = e.status !== null && e.status < 500 && e.status !== 408;
        if (armedCodexActionId !== null && !definiteRefusal) {
          // 结果未知：核对锚保留（同载荷重按 = 运输重试，服务端账本去重防双投）
          setReceipt({
            kind: "codexUnknown",
            actionId: armedCodexActionId,
            text: fullText,
            reasonCode: null,
            deadline: false,
          });
          // Q2：待确认现场自此归属本动作（回调落地据此核对）
          codexRecoveryOwnerRef.current = {
            actionId: armedCodexActionId,
            epoch: myEpoch,
            draftVersion: submittedDraftVersion,
          };
          return;
        }
        // F6：确定性拒绝 = 零投递终局——核对锚让位（含 action_conflict 同 ID 异
        // 载荷撞库的形态），下一次发送取新 ID，不遗留死锁锚
        if (armedCodexActionId !== null) {
          codexActionIdRef.current = null;
          codexActionPayloadRef.current = null;
        }
        // 2026-10-20 复审 P7：已知 reasonCode 走本地化文案（后端 reason 只有中文，
        // en 界面不能原样显示）；未知码才回退服务端 reason/错误消息。
        const code = typeof e.data?.reasonCode === "string" ? e.data.reasonCode : null;
        const localized =
          code === "headless_disabled" ? i18n.t("mobile.composer.headlessDisabled") : null;
        const reason = typeof e.data?.reason === "string" ? e.data.reason : null;
        setReceipt({ kind: "failed", error: localized ?? reason ?? e.message });
      } else if (armedCodexActionId !== null) {
        // 非 ApiError 异常（网络层断开等）：Codex 路径提交结果未知 → 待确认面板
        //（核对锚保留：同载荷重按 = 运输重试，绝不换 ID 重投）
        setReceipt({
          kind: "codexUnknown",
          actionId: armedCodexActionId,
          text: fullText,
          reasonCode: null,
          deadline: false,
        });
        // Q2：待确认现场自此归属本动作（回调落地据此核对）
        codexRecoveryOwnerRef.current = {
          actionId: armedCodexActionId,
          epoch: myEpoch,
          draftVersion: submittedDraftVersion,
        };
      } else {
        setReceipt({ kind: "failed", error: String(e) });
      }
    } finally {
      // F6：计时器清理由 finally 单点承担（覆盖一切出口：探测拦截早退 / 响应 /
      // 异常），且只清**本次发送自己的**计时器（闭包句柄）——旧发送的迟到收尾
      // 碰不到新动作的计时器。sending 同理归当前发送所有：代际已前进的迟到
      // finally 不得关掉新动作的转圈。
      clearMyDeadline();
      if (!staleEpoch()) setSending(false);
    }
  }, [
    text,
    attachments,
    sending,
    busy,
    sendInfo,
    session.id,
    queueOnlyNext,
    bypassNext,
    probeCardPresence,
    onQueueChanged,
  ]);

  // S5：明确未送达后的「重试」——创建**关联的新动作**（新 actionId），不永远重放
  // 原失败动作。原动作 ID 记入 retriedFromRef（安全引用，随新回执展示前 8 位）。
  // 草稿保留在输入框中（用户可在重试前修改——改后发送的是新动作新内容）。
  // 核对锚先行让位：明确零投递已终局，同 ID 重放只会再次回到账本同一终态
  // （「永远重放原失败」正是本按钮要消灭的形态）。
  const retryNotSent = useCallback(
    (predecessorActionId: string) => {
      if (sending || busy) return;
      codexActionIdRef.current = null;
      codexActionPayloadRef.current = null;
      retriedFromRef.current = predecessorActionId;
      void handleSend();
    },
    [sending, busy, handleSend]
  );

  // W2（复审 round 5）：人工继续后继的真实结果按五分类接管。resumed 响应的
  // `send` 字段表明后继是否受理 / 未送达 / 未知 / 排队——**绝不凭「后继已创建」
  // 宣称已发**。原动作锚在人工继续事实成立时一律释放（此后同文重按 = 全新发送，
  // 不沿已被撤销写出权的原 ID 重投）。Accepted 由面板保留「已重新发送」呈现
  //（此处只释放锚）；无 send 信息（already_resumed / 旧后端）由面板呈现「待查」。
  const handleSuccessorResolved = useCallback(
    (info: {
      originalActionId: string;
      successorActionId: string;
      text: string;
      send: CodexSuccessorSend | null;
    }) => {
      // ===== Q2（复审 round 6）：归属守卫——全部副作用之前 =====
      // 本回调是父组件回调，不受面板按 actionId 重挂的局部状态保护：旧面板卸载
      // 后，在途的人工继续 promise 依然会调用它。落地第一件事核对「当前恢复动作
      // + 发送代际」仍归属发起时的原动作 A；不一致 = 响应迟到（用户已发送 B /
      // 恢复现场已切到别的动作）→ 一律作废：不释放锚、不覆盖回执、不清草稿。
      //（不只保护 setReceipt——Accepted/空 send 路径的锚释放同在守卫之后。）
      const owner = codexRecoveryOwnerRef.current;
      if (
        owner === null ||
        owner.actionId !== info.originalActionId ||
        owner.epoch !== sendEpochRef.current
      ) {
        return;
      }
      // Q2 草稿版本门：只有「发起人工继续后未再编辑」的草稿（= 仍是后继实际
      // 提交的那份原文）才允许被成功/入队清理；在途编辑出的新草稿绝不代清。
      const draftUnchanged = owner.draftVersion === draftVersionRef.current;
      codexActionIdRef.current = null;
      codexActionPayloadRef.current = null;
      const r = info.send?.receipt;
      if (r && (r.delivery === "not_sent" || r.delivery === "rejected")) {
        // 后继明确未送达（如 pending 门拒绝）→ 标准 NotSent 五分类：理由文案 +
        // 重试沿**后继 ID** + 报告；父子关联 = retriedFrom 展示原动作短 ID
        retriedFromRef.current = info.originalActionId;
        setReceipt({ kind: "codex", receipt: r, retriedFrom: info.originalActionId });
      } else if (info.send?.queued === true) {
        // 后继忙态留队 → 标准「已排队」五分类：位次 chip，队列对账沿后继 actionId。
        // Q2：清正文/附件只针对仍是实际提交草稿的现场（版本门）
        codexReceiptSetAtRef.current = Date.now();
        if (draftUnchanged) {
          setText("");
          setAttachments([]);
        }
        setReceipt({
          kind: "codexQueued",
          position: info.send.position ?? 0,
          actionId: info.successorActionId,
          resumedFrom: info.originalActionId,
        });
        onQueueChanged?.();
      } else if (r && r.delivery === "unknown_delivery") {
        // 后继结果未知 → 面板按**后继 ID** 重挂（F7 隔离）：核对沿后继、只核对
        // 不自动重发；父子关联行 + 理由码随后继回执
        setReceipt({
          kind: "codexUnknown",
          actionId: info.successorActionId,
          text: info.text,
          reasonCode: r.reasonCode,
          deadline: false,
          resumedFrom: info.originalActionId,
        });
        // Q2：恢复现场随面板切到后继——归属快照同刻重登记（该后继自己的人工
        // 继续/迟到响应此后沿新快照核对归属）。Q2（复审 round 7）：后继**继承
        // 原提交版本**而不是重读当前草稿版本——后继重发的就是原消息
        //（ManualResumeBody.text 契约），其提交版本即原动作提交时点冻结的
        // owner.draftVersion；重读当前值会把等待期编辑出的新稿版本记到后继
        // 头上，多层 Unknown 时版本门同样误清新稿。
        codexRecoveryOwnerRef.current = {
          actionId: info.successorActionId,
          epoch: sendEpochRef.current,
          draftVersion: owner.draftVersion,
        };
      }
      // accepted：面板保留「已重新发送」；send 无 receipt 且未排队：面板呈现
      // 「此前已创建后继…待查」——两种都不在此换回执
    },
    [onQueueChanged]
  );

  // S5/F6：卸载清理当前发送的 30 秒计时器（挂起 promise 的迟到回调经代际/闭包
  // 作废；这里只兜底清掉真实挂在事件循环上的那一个）
  useEffect(
    () => () => {
      if (deadlineTimerRef.current !== null) {
        clearTimeout(deadlineTimerRef.current.timer);
        deadlineTimerRef.current = null;
      }
    },
    []
  );

  // P2-7 失败对账（插队/撤回/修改共用）：失败后复核 /session-queue——
  // - 条目仍在 pending → 恢复排队视图（刷新队位，行同步更新，「立即发送/修改/撤回」
  //   按钮保留可重试）；
  // - 确认不在队（已被消费 / 他端撤回）→ onGone 终态（jump/retract 均为中性 gone
  //   收敛文案，评审裁决：不落 failed「可重试」，防重复注入）+ 行移除（确认不在队
  //   的行继续挂着按钮会指向必 404 的幽灵条目）；
  // - 复核自身网络失败 → 保守恢复排队视图（队位沿用旧值，3s 轮询随后自愈）——
  //   拿不到「真不在队」的证据就不落终态；行缺失时补回（含他从桌面排、本端无
  //   回执的条目），避免按钮丢失后排队条目在 UI 上失控
  const reconcileQueued = useCallback(
    async (itemId: number, prevPosition: number, prevContent: string, onGone: () => void) => {
      try {
        const items = await fetchQueue(session.id);
        const mine = items.find((i) => i.id === itemId);
        if (mine) {
          // 复核快照是点击时刻之后的服务端视图，比此前在途的 tick 更具新：落地即
          // 前移认知时刻，作废可能显示「不在队」的旧 tick（防假 gone）
          queueMutatedAtRef.current = Date.now();
          setReceipt({
            kind: "queued",
            itemId,
            position: mine.position,
            content: mine.content ?? prevContent,
          });
          setQueueItems((prev) =>
            upsertQueueItem(prev, {
              id: itemId,
              content: mine.content,
              enqueuedAt: mine.enqueuedAt,
              position: mine.position,
            })
          );
        } else {
          // 确认不在队同理前移：此后到货的旧 tick 快照仍显示在队 → 不得复活幽灵行
          queueMutatedAtRef.current = Date.now();
          setQueueItems((prev) => prev.filter((i) => i.id !== itemId));
          onGone();
        }
      } catch {
        // 保守恢复也是一次本地认知落地（「该条仍在队」）：同样作废此前在途旧快照
        queueMutatedAtRef.current = Date.now();
        setReceipt({ kind: "queued", itemId, position: prevPosition, content: prevContent });
        setQueueItems((prev) =>
          prev.some((i) => i.id === itemId)
            ? prev
            : upsertQueueItem(prev, {
                id: itemId,
                content: prevContent,
                enqueuedAt: 0,
                position: prevPosition,
              })
        );
      }
    },
    [session.id]
  );

  // D8 参数化（按条操作，不再读 receipt.itemId——桌面端/他端排的队同样可插队）。
  // busy 加闸单点保留：同一时刻至多一个排队操作在途
  const handleJump = useCallback(
    async (item: QueueItemView) => {
      if (busy) return;
      const { id: itemId, position, content } = item;
      setBusy(true);
      try {
        const j = await queueJump(session.id, itemId);
        if (j.status === "delivered") {
          // 本地认知翻转：该条已出队（时序防御见 queueMutatedAtRef 声明处）
          queueMutatedAtRef.current = Date.now();
          setReceipt({ kind: "delivered" });
          // 立即发送成功 = 条目已出队：行即时移除（3s 轮询权威列表随后兜底）
          setQueueItems((prev) => prev.filter((i) => i.id !== itemId));
        } else if (j.status === "submitted") {
          // E1 裁16 排队回执（kimi）：消息已进 TUI 内部队列（busy 直接投递=排队制，
          // 回合结束自动开新回合）——已离开 兔维斯 队列 → 行移除 + 中性 submitted 回执
          //（不谎报 delivered，也不落 failed；对账收敛会把它误报成 gone）
          queueMutatedAtRef.current = Date.now();
          setReceipt({ kind: "submitted" });
          setQueueItems((prev) => prev.filter((i) => i.id !== itemId));
        } else {
          // 非 delivered 回执先复核再定终态。queued（F1 后新语义：jump 守卫忙/
          // 条目已被消费 → 回 queued{itemId,position}，前端 reconcileQueued 对账
          // 兼容——条目仍在队恢复排队视图、确认不在队走中性 gone 收敛）与 failed
          // （注入失败，行已退出 pending）同走复核——不得落 failed「可重试」诱发
          // 重复注入（评审裁决）
          await reconcileQueued(itemId, position, content, () =>
            setReceipt({ kind: "gone", message: GONE_JUMP_MESSAGE })
          );
        }
      } catch {
        // 网络层 / 非 2xx（404 条目已不在队等）：同样复核，在队即恢复、不在队中性收敛
        await reconcileQueued(itemId, position, content, () =>
          setReceipt({ kind: "gone", message: GONE_JUMP_MESSAGE })
        );
      } finally {
        setBusy(false);
      }
    },
    [busy, session.id, reconcileQueued]
  );

  /** 撤回/修改共用的出队执行器（评审必须1 的复核收敛逻辑单点保留，两钮不复制）：
   *  调 queueRetract——{ok:true} = 服务端确认已撤（免复核直接收敛，撤回目的已达成）；
   *  忙时 failed / 404 / 网络错 → 复核对账（条目仍在队恢复排队视图可重试、确认不在队
   *  走中性 gone）。onConfirmed 仅在「确认出队」后被调：撤回用它清回执，
   *  修改用它把正文放回输入框 */
  const retractWithOutcome = useCallback(
    async (itemId: number, position: number, content: string, onConfirmed: () => void) => {
      setBusy(true);
      try {
        const r = await queueRetract(session.id, itemId);
        if ("ok" in r) {
          /* 服务端确认已撤（{ok:true}）：免复核，直接收敛（评审必须1——撤回目的已达成）；
             行同步移除（快路径不拉队列，列表即权威呈现）。本地认知翻转前移，作废
             可能仍显示在队的在途旧快照（M1：防幽灵行复活） */
          queueMutatedAtRef.current = Date.now();
          setQueueItems((prev) => prev.filter((i) => i.id !== itemId));
          onConfirmed();
        } else {
          /* 忙时 200 {status:"failed"}：条目未被撤、仍在队 → 复核对账。不得丢弃联合
             返回值直接复核——否则「忙时 + 复核也失败」双失败时条目实际在队、UI 却
             永久失控（评审必须1核心场景；reconcileQueued 复核失败保守恢复兜底） */
          await reconcileQueued(itemId, position, content, () =>
            setReceipt({ kind: "gone", message: GONE_RETRACT_MESSAGE })
          );
        }
      } catch (e) {
        if (e instanceof ApiError && e.status === 404) {
          /* 404 not_found（已送达 / 他端撤回）：条目已不在队 → 中性收敛（不作失败
             提示），行同步移除；本地认知翻转前移（同 M1 防幽灵行） */
          queueMutatedAtRef.current = Date.now();
          setQueueItems((prev) => prev.filter((i) => i.id !== itemId));
          setReceipt({ kind: "gone", message: GONE_RETRACT_MESSAGE });
        } else {
          // 网络错：同款复核——条目仍在队 → 恢复排队视图可重试；确认不在队 → 中性收敛
          await reconcileQueued(itemId, position, content, () =>
            setReceipt({ kind: "gone", message: GONE_RETRACT_MESSAGE })
          );
        }
      } finally {
        setBusy(false);
      }
    },
    [session.id, reconcileQueued]
  );

  /** 撤回（W4）：完全取消——确认出队后清回执 + 行移除，正文不保留（2026-09-20
   *  裁决：撤回=完全取消；拉回编辑走「修改」钮）。D8：按条操作，不限本端发送的
   *  条目 */
  const handleRetract = useCallback(
    async (item: QueueItemView) => {
      if (busy) return;
      await retractWithOutcome(item.id, item.position, item.content, () => setReceipt(null));
    },
    [busy, retractWithOutcome]
  );

  /** 修改（2026-09-20 裁决）：确认出队后把正文放回输入框继续编辑——与撤回共用
   *  同一后端出队动作，差别仅在是否恢复文本。D8：按条取行 content——放回前剥离
   *  [mobile …] **尾**签名（丁T3 裁2 签名后置；行 content 是服务端 compose 后的最终
   *  注入文本，不剥则重发二次叠加签名）；附件标记行保留（重发仍引用同一落盘文件）。截断到
   *  MAX_SEND_CHARS 与输入框 maxLength 对齐；恢复后聚焦输入框（移动端直接可改）。
   *  D6：确认出队后置 queueOnlyNext——修改后的重发一律入队（防「文件说闲、
   *  TUI 实忙」窗口变相插队）；忙时失败/复核失败条目仍在队（onConfirmed 不触发）
   *  则不置标志，取消「修改」意图后的排队视图照旧 */
  const handleEdit = useCallback(
    async (item: QueueItemView) => {
      if (busy) return;
      await retractWithOutcome(item.id, item.position, item.content, () => {
        // Q2：「修改」回填 = 草稿被整体替换（版本前移）
        draftVersionRef.current += 1;
        setText(stripMobileSignature(item.content).slice(0, MAX_SEND_CHARS));
        setQueueOnlyNext(true);
        setReceipt(null);
        inputRef.current?.focus();
      });
    },
    [busy, retractWithOutcome]
  );

  // 附件上传（2026-09-20）：逐个上传 → chips 状态机（uploading → ready/failed）。
  // 404 no_cwd 一次即置 noCwd（会话无项目目录，+ 钮禁用——与 R5 禁用口径同源）；
  // 403 设备失效抛 ApiError(403, "设备已失效…") → failed chip（Board 侧另有 403
  // 全局判废通道，此处不重复处理）
  const addFiles = useCallback(
    async (files: File[]) => {
      for (const f of files) {
        // Q2：附件增补也是草稿变更（版本前移）
        draftVersionRef.current += 1;
        const id = `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`;
        const isImage = f.type.startsWith("image/");
        setAttachments((prev) => [
          ...prev,
          {
            id,
            name: f.name || (isImage ? i18n.t("mobile.composer.pastedImageName") : "file"),
            isImage,
            status: "uploading",
          },
        ]);
        // AbortController：上传中可取消（× 移除 = 中断上传 + 移除 chip）——
        // 大文件（视频）上传耗时长，不可取消会被迫干等（2026-09-20 用户实测）
        const ctrl = new AbortController();
        uploadCtrlsRef.current.set(id, ctrl);
        try {
          // §C5 上行进度：onProgress 是 uploadAttachment 的**第 4 参**（第 3 参是
          // 既有 signal）。进度落 chip：已传字节 + 百分比（有 total 才有）+ 预估剩余
          const res = await uploadAttachment(session.id, f, ctrl.signal, (loaded, total) => {
            const atMs = Date.now();
            const first = uploadFirstSampleRef.current.get(id) ?? { loaded, atMs };
            uploadFirstSampleRef.current.set(id, first);
            setAttachments((prev) =>
              prev.map((a) =>
                a.id === id
                  ? {
                      ...a,
                      loaded,
                      total: total > 0 ? total : null, // 事件报 0 = 未知，不编百分比
                      rateBps: transferRateBps(first, { loaded, atMs }),
                    }
                  : a
              )
            );
          });
          uploadFirstSampleRef.current.delete(id);
          if (res === null) throw new ApiError(403, i18n.t("mobile.composer.deviceInvalid"));
          setAttachments((prev) =>
            prev.map((a) => (a.id === id ? { ...a, status: "ready", path: res.path } : a))
          );
        } catch (e) {
          uploadFirstSampleRef.current.delete(id);
          if (ctrl.signal.aborted) {
            // 用户取消：chip 已随 removeAttachment 移除，静默收尾
            setAttachments((prev) => prev.filter((a) => a.id !== id));
            continue;
          }
          const isNoCwd = e instanceof ApiError && e.status === 404 && e.message === "no_cwd";
          if (isNoCwd) setNoCwd(true);
          const reason =
            e instanceof ApiError && isNoCwd
              ? i18n.t("mobile.composer.noCwdShort")
              : formatInvokeError(e instanceof ApiError ? e.message : String(e), i18n.t);
          setAttachments((prev) =>
            prev.map((a) => (a.id === id ? { ...a, status: "failed", error: reason } : a))
          );
        } finally {
          uploadCtrlsRef.current.delete(id);
        }
      }
    },
    [session.id]
  );

  const removeAttachment = useCallback((id: string) => {
    // Q2：附件移除也是草稿变更（版本前移）
    draftVersionRef.current += 1;
    // 上传中移除 = 取消：中断在途传输（XHR abort），防止白传到底（2026-09-20 用户反馈）
    uploadCtrlsRef.current.get(id)?.abort();
    uploadCtrlsRef.current.delete(id);
    uploadFirstSampleRef.current.delete(id);
    setAttachments((prev) => prev.filter((a) => a.id !== id));
  }, []);

  /** 粘贴图片（2026-09-20）：clipboard 里的图片文件走同上传链路（桌面浏览器粘贴
   *  最顺；移动端以 + 钮文件选择为主）。非图片粘贴放行默认文本行为 */
  const handlePaste = useCallback(
    (e: ReactClipboardEvent<HTMLTextAreaElement>) => {
      const images = Array.from(e.clipboardData?.files ?? []).filter((f) =>
        f.type.startsWith("image/")
      );
      if (images.length === 0) return;
      e.preventDefault();
      void addFiles(images);
    },
    [addFiles]
  );

  // 拉取未就绪 / 失败 / 403：不渲染（详情页正文照常）
  if (!infoReady || sendInfo === null) return null;

  const canSend =
    injectable &&
    !sending &&
    !busy &&
    text.trim().length > 0 &&
    !attachments.some((a) => a.status === "uploading");

  return (
    <div data-testid="message-composer" className="shrink-0 border-t border-[var(--cb)] px-3 py-2">
      {/* 不可注入：输入区整体禁用，仅留 reason 展示（输入行保留占位但不可用） */}
      {!injectable && (
        <p data-testid="send-disabled-reason" className="mb-2 text-xs text-[var(--mut)]">
          {i18n.t("mobile.composer.sendDisabledPrefix")}
          {sendInfo.reason ?? i18n.t("mobile.composer.sendDisabledFallback")}
          {sendInfo.reasonCode
            ? i18n.t("mobile.composer.sendDisabledCode", { code: sendInfo.reasonCode })
            : ""}
        </p>
      )}
      {/* 丁T3 接入② / 丁T6 复评：终端卡片在场提示条（**输入区仍可用**——不把输入框
          整个禁用是刻意的：用户可能正想把内容复制到卡片输入框；发送按钮仍可点，
          点了会按分流走。四态各一句话：可自由作答 → 承诺「作为回答发送」；
          不可作答/审批 → 指路卡片按钮或终端）。文案与回执同源常量，两处不会漂移。 */}
      {/* questionBlocked（待答问题不可在本框作答）不在此渲染——ModeBar 的
          「终端有待回答的问题」提示 + 输入框占位语已各说一次（2026-10-10 用户
          指令：同一条指引只显示一遍，三条同义提示堆叠是噪音） */}
      {injectable && cardPresence !== "none" && cardPresence !== "questionBlocked" && (
        <p
          data-testid="composer-card-presence"
          data-presence={cardPresence}
          className="mb-2 text-xs text-amber-700 dark:text-amber-400"
        >
          {cardPresence === "approve"
            ? i18n.t("mobile.composer.presenceApprove")
            : i18n.t("mobile.composer.presenceQuestion")}
        </p>
      )}
      {/* S5：队列暂停二选一（S2：暂停**不置灰**——用户仍可选恢复队列或本条先发送）。
          恢复 = POST /codex/queue/resume（唯一解除出口）；先发送 = 一次性越队授权
         （bypassPausedQueue，消费即失效；grant_consumed 引导重选而非原样重放）。 */}
      {injectable && sendInfo?.queuePaused === true && (
        <div
          data-testid="send-queue-paused"
          className="mb-2 rounded-lg bg-amber-500/10 px-3 py-2 text-xs text-amber-700 dark:bg-amber-400/10 dark:text-amber-300"
        >
          <p>{i18n.t("mobile.codex.queue.pausedBanner")}</p>
          {bypassNext && (
            <p data-testid="queue-bypass-armed" className="mt-0.5">
              {i18n.t("mobile.codex.queue.bypassArmed")}
            </p>
          )}
          <div className="mt-1.5 flex flex-wrap gap-2">
            <button
              type="button"
              data-testid="queue-resume"
              disabled={resumingQueue || sending}
              onClick={() => {
                void resumeQueue();
              }}
              className="rounded-full bg-amber-500/20 px-2 py-0.5 disabled:opacity-40 dark:bg-amber-400/20"
            >
              {i18n.t("mobile.codex.queue.resume")}
            </button>
            <button
              type="button"
              data-testid="queue-bypass-arm"
              disabled={sending}
              onClick={() => setBypassNext((v) => !v)}
              className="rounded-full bg-amber-500/20 px-2 py-0.5 disabled:opacity-40 dark:bg-amber-400/20"
            >
              {i18n.t("mobile.codex.queue.bypass")}
            </button>
          </div>
        </div>
      )}
      {/* 投递中（灰3）：send await 全程在场——慢消费者长文投递可达分钟级，
          期间不空白；与既有回执并存（排队态的撤回/插队按钮不因发送而失联），
          完成后被结果 chip 覆盖（sending 翻转 false 即消失） */}
      {sending && (
        <div className="mb-2 flex flex-wrap items-center gap-2">
          <span
            data-testid="send-receipt-delivering"
            className="rounded-full bg-[var(--btnp)]/10 px-2 py-0.5 text-xs text-[var(--tx)]"
          >
            <span className="mr-1 inline-block h-1.5 w-1.5 animate-pulse rounded-full bg-[var(--btnp)] align-middle" />
            {i18n.t("mobile.composer.delivering")}
            <span className="ml-1 text-[var(--mut)]">
              {i18n.t("mobile.composer.deliveringHint")}
            </span>
          </span>
        </div>
      )}
      {/* 单回执槽（T4 复评 M3 语义对齐）：保留**最近一次队列操作**的终态——本端
          发送，或对任意行（含他端排队条目）的立即发送/修改/撤回：
          delivered/submitted/failed/gone。queued 态不再渲染 chip（由下方多列队
          列表取代），回执状态本身保留——乐观行即时反馈、失败对账与 3s 轮询仍以
          其驱动。codexUnknown 由待确认面板接管（下方独立块），不进 chip 容器 */}
      {receipt !== null && receipt.kind !== "queued" && receipt.kind !== "codexUnknown" && (
        <div className="mb-2 flex flex-wrap items-center gap-2">
          {receipt.kind === "delivered" && (
            <span
              data-testid="send-receipt-delivered"
              className="rounded-full bg-emerald-500/10 px-2 py-0.5 text-xs text-emerald-700 dark:bg-emerald-400/10 dark:text-emerald-400"
            >
              {i18n.t("mobile.composer.delivered")}
            </span>
          )}
          {receipt.kind === "submitted" && (
            // D7/T3 中性回执（非确认成功亦非失败）：已投递未确认——agent 空闲后
            // 处理 TUI 内部队列里的消息；配色与 gone 同族（中性 slate），不带
            // 「（可重试）」——该语义仅属于 failed 态，出现在此会诱导双发
            <span
              data-testid="send-receipt-submitted"
              className="rounded-full bg-[var(--cb)]/70 px-2 py-0.5 text-xs text-[var(--mut)]"
            >
              {i18n.t("mobile.composer.submitted")}
            </span>
          )}
          {receipt.kind === "failed" && (
            <span
              data-testid="send-receipt-failed"
              className="rounded-full bg-rose-500/10 px-2 py-0.5 text-xs text-rose-700 dark:bg-rose-400/10 dark:text-rose-400"
            >
              {i18n.t("mobile.composer.sendFailedRetry", { error: receipt.error })}
            </span>
          )}
          {receipt.kind === "gone" && (
            // 中性收敛（评审裁决）：条目经复核确认已离开队列——大概率已送达，
            // 不标失败红色、不带「（可重试）」，防重复注入。
            // message 存 i18n 键（M5），渲染期现取——语言切换后旧回执也跟随
            <span
              data-testid="send-receipt-gone"
              className="rounded-full bg-[var(--cb)]/70 px-2 py-0.5 text-xs text-[var(--mut)]"
            >
              {i18n.t(receipt.message)}
            </span>
          )}
          {receipt.kind === "blocked" && (
            // 丁T3 接入②：分流拦截（非失败——无注入尝试、无「可重试」语义）。
            // 配色同 gone（中性 slate）：红 chip 会暗示「操作出错」，而这是**安全
            // 侧的正确行为**（终端在等对话框，此刻直发会误触选项）。
            // message 存 i18n 键（M5），渲染期现取
            <span
              data-testid="send-receipt-blocked"
              className="rounded-full bg-[var(--cb)]/70 px-2 py-0.5 text-xs text-[var(--mut)]"
            >
              {i18n.t(receipt.message)}
            </span>
          )}
          {receipt.kind === "codex" && receipt.receipt.delivery === "accepted" && (
            // 已受理 ≠ Turn 完成（终态以投影为准）——措辞只说「正在执行」，
            // 绝不写「已完成/已送达」（设计 §11.1）
            <span
              data-testid="send-receipt-codex-accepted"
              className="rounded-full bg-emerald-500/10 px-2 py-0.5 text-xs text-emerald-700 dark:bg-emerald-400/10 dark:text-emerald-400"
            >
              {i18n.t("mobile.codex.composer.codexAccepted")}
            </span>
          )}
          {receipt.kind === "codex" && receipt.retriedFrom && (
            // S5：重试关联——新动作由「明确未送达后的重试」创建（安全引用，前 8 位）
            <span data-testid="codex-retried-from" className="w-full text-[10px] text-[var(--mut)]">
              {i18n.t("mobile.codex.composer.retriedFrom", {
                short: receipt.retriedFrom.slice(0, 8),
              })}
            </span>
          )}
          {receipt.kind === "codex" &&
            (receipt.receipt.delivery === "not_sent" ||
              receipt.receipt.delivery === "rejected") && (
              <>
                <span
                  data-testid="send-receipt-codex-failed"
                  className="rounded-full bg-rose-500/10 px-2 py-0.5 text-xs text-rose-700 dark:bg-rose-400/10 dark:text-rose-400"
                >
                  {/* S2 理由码专属文案（词表内）优先；词表外回落 {{reason}} 模板 */}
                  {notSentReasonCopy(receipt.receipt.reasonCode) ??
                    i18n.t("mobile.codex.composer.codexNotDelivered", {
                      reason: receipt.receipt.reasonCode ?? receipt.receipt.delivery,
                    })}
                </span>
                {/* 明确未送达（§5.2）：保留草稿（在输入框），重试 = 新动作 + 关联展示；
                    报告问题 = 统一安全引用入口。queue_paused 的重试预期再得同一拒绝
                    ——先在上方二选一条处理队列选择。 */}
                <button
                  type="button"
                  data-testid="codex-retry-notsent"
                  disabled={sending || busy}
                  onClick={() => retryNotSent(receipt.receipt.actionId)}
                  className="rounded-full bg-emerald-500/15 px-2 py-0.5 text-xs text-emerald-800 disabled:opacity-40 dark:bg-emerald-400/15 dark:text-emerald-300"
                >
                  {i18n.t("mobile.codex.composer.codexRetry")}
                </button>
                <button
                  type="button"
                  data-testid="codex-report-notsent"
                  onClick={() =>
                    handleReportIssue(receipt.receipt.reasonCode, receipt.receipt.actionId)
                  }
                  className="rounded-full bg-[var(--cb)] px-2 py-0.5 text-xs text-[var(--mut)]"
                >
                  {i18n.t("reportIssue.button")}
                </button>
              </>
            )}
          {receipt.kind === "codexQueued" && (
            // 忙 Thread 留队——中性 chip（队列查看/撤回在 CodexQueuePanel）
            <>
              <span
                data-testid="send-receipt-codex-queued"
                className="rounded-full bg-[var(--cb)]/70 px-2 py-0.5 text-xs text-[var(--mut)]"
              >
                {i18n.t("mobile.codex.composer.codexQueued", { position: receipt.position })}
              </span>
              {receipt.resumedFrom && (
                // W2：人工继续后继的父子关联（安全引用，前 8 位）
                <span
                  data-testid="codex-queued-resumed-from"
                  className="w-full text-[10px] text-[var(--mut)]"
                >
                  {i18n.t("mobile.codex.composer.retriedFrom", {
                    short: receipt.resumedFrom.slice(0, 8),
                  })}
                </span>
              )}
            </>
          )}
          {receipt.kind === "headless" && (
            <span
              data-testid="send-receipt-headless"
              data-status={receipt.status}
              className={
                receipt.status === "failed" || receipt.status === "cancelled"
                  ? "rounded-full bg-amber-500/10 px-2 py-0.5 text-xs text-amber-700 dark:bg-amber-400/10 dark:text-amber-400"
                  : "rounded-full bg-emerald-500/10 px-2 py-0.5 text-xs text-emerald-700 dark:bg-emerald-400/10 dark:text-emerald-400"
              }
            >
              {i18n.t(`mobile.codex.composer.headless_${receipt.status}`, {
                reason: receipt.reason ?? "",
              })}
            </span>
          )}
        </div>
      )}
      {/* S5：发送结果待确认（第五分类）——面板接管（结束转圈后保留消息与原动作；
          检查是否送达 / 报告问题 / 人工继续三件风险；绝不普通重试） */}
      {receipt !== null && receipt.kind === "codexUnknown" && (
        <div className="mb-2">
          {/* F7：按 actionId 键控重挂——新动作不继承旧动作的核对结论 / 确认臂 /
              busy 态；旧面板实例卸载后其核对 promise 的迟到 setState 一并作废 */}
          <CodexSendRecoveryPanel
            key={receipt.actionId}
            sessionId={session.id}
            actionId={receipt.actionId}
            text={receipt.text}
            deadlineExceeded={receipt.deadline === true}
            initialReasonCode={receipt.reasonCode}
            resumedFrom={receipt.resumedFrom}
            onRetryNewAction={retryNotSent}
            onReportIssue={(code) => handleReportIssue(code, receipt.actionId)}
            onSuccessorResolved={handleSuccessorResolved}
          />
        </div>
      )}
      {/* S5：诊断服务未部署的如实出口（不伪造上传成功）：明确「未开启」+ 复制摘要 */}
      {reportUnavailable && (
        <div
          data-testid="codex-report-unavailable"
          className="mb-2 rounded-lg bg-[var(--cb)]/70 px-3 py-2 text-xs text-[var(--mut)]"
        >
          <p>{i18n.t("reportIssue.unavailable")}</p>
          <p className="mt-0.5">{i18n.t("reportIssue.unavailableHint")}</p>
          <button
            type="button"
            data-testid="codex-report-copy"
            onClick={() => {
              void copyReportSummary();
            }}
            className="mt-1.5 rounded-full bg-[var(--cb)] px-2 py-0.5"
          >
            {i18n.t("reportIssue.copySummary")}
          </button>
        </div>
      )}
      {/* D8 多列队列表（验收问题 #6）：完整 /session-queue 渲染——逐条预览 + 位次 +
          立即发送/修改/撤回三钮（按条 id，不限本端发送的条目）；「排队中 第 N 位」
          chip 已由本列表取代。位次沿用 position=0 容忍口径（并发消费窗口值 →
          「排队中」不带数字） */}
      {queueItems.length > 0 && (
        <div className="mb-2" data-testid="queue-list">
          <ul className="space-y-1">
            {queueItems.map((item) => (
              <li
                key={item.id}
                data-testid={`queue-row-${item.id}`}
                className="flex flex-wrap items-center gap-1.5 rounded-lg bg-amber-500/10 px-2 py-1 dark:bg-amber-400/10"
              >
                <span
                  data-testid={`queue-position-${item.id}`}
                  className="shrink-0 text-xs text-amber-700 dark:text-amber-300"
                >
                  {item.position >= 1
                    ? i18n.t("mobile.codex.queue.position", { n: item.position })
                    : i18n.t("mobile.composer.queuedNoPosition")}
                </span>
                <span
                  className="min-w-0 flex-1 truncate text-xs text-[var(--tx)]"
                  title={item.content}
                >
                  {queueItemPreview(item.content)}
                </span>
                <button
                  type="button"
                  data-testid="queue-jump"
                  disabled={busy || sending}
                  onClick={() => {
                    void handleJump(item);
                  }}
                  className="rounded-full bg-amber-500/20 px-2 py-0.5 text-xs text-amber-700 disabled:opacity-40 dark:bg-amber-400/20 dark:text-amber-300"
                >
                  {i18n.t("mobile.composer.jumpNow")}
                </button>
                <button
                  type="button"
                  data-testid="queue-edit"
                  disabled={busy || sending}
                  onClick={() => {
                    void handleEdit(item);
                  }}
                  className="rounded-full bg-amber-500/20 px-2 py-0.5 text-xs text-amber-700 disabled:opacity-40 dark:bg-amber-400/20 dark:text-amber-300"
                >
                  {i18n.t("mobile.composer.editAction")}
                </button>
                <button
                  type="button"
                  data-testid="queue-retract"
                  disabled={busy || sending}
                  onClick={() => {
                    void handleRetract(item);
                  }}
                  className="rounded-full bg-[var(--cb)] px-2 py-0.5 text-xs text-[var(--mut)] disabled:opacity-40"
                >
                  {i18n.t("mobile.codex.queue.retract")}
                </button>
              </li>
            ))}
          </ul>
          <p data-testid="queue-hint" className="mt-1 text-[11px] text-[var(--mut)]">
            {i18n.t("mobile.composer.queueHint")}
          </p>
        </div>
      )}
      {/* 待发附件 chips（2026-09-20）：上传中/就绪/失败三态，可单个移除。
          上行进度（Task 10 §C5；复评 I-2 按线稿补齐进度条 + 口径说明行）：
          线稿 .upl 画块三件套 = upl-head（文字进度）+ .pbar（进度条）+ .upl-meta
          （口径说明），缺一即与线稿不一致（2026-10-07 用户裁决：以线稿为准）。
          - 文字进度：收到过进度事件才追加——已传字节 + 百分比（有 total 才显示）
            + 预估剩余（速率可估才显示）；
          - 进度条：与百分比同条件（total 已知），类串照抄下行 FilePreview 的
            .pbar 等价写法；**total 未知绝不渲染**（§C5 硬要求：不显示假百分比）；
          - 口径行：只要还有附件在传就显示（线稿逐字），放在 chips 容器下方——
            chip 是 rounded-full 胶囊，条与说明挤进去都看不清；
          「上传中：{name}」保持独立文本节点（既有测试精确匹配 + 结构稳定），
          进度细节在子 span；进度条是 chip 的兄弟节点（同在一个逐条包裹里） */}
      {attachments.length > 0 && (
        <div className="mb-1">
          <div className="flex flex-wrap items-center gap-1.5" data-testid="attachment-chips">
            {attachments.map((a) => {
              const uploading = a.status === "uploading";
              const eta = uploading
                ? etaRemainingText(a.loaded ?? 0, a.total ?? null, a.rateBps ?? null)
                : null;
              // 百分比与进度条同条件：total 已知（== null = 隧道不透传 content-length
              // 或事件报 0）→ 只显示已传字节，不编假百分比也不画条
              const pct =
                uploading && a.loaded !== undefined && a.total != null
                  ? Math.min(100, Math.floor((a.loaded / a.total) * 100))
                  : null;
              return (
                // 包裹层用 div（块级 flex 项）：chip 是 span、进度条是 div，
                // 包在 span 里属非法嵌套（span 只能含短语内容）——换 div 即合法
                <div key={a.id} className="flex flex-col gap-1">
                  <span
                    data-testid={`attach-chip-${a.id}`}
                    className={`flex items-center gap-1 rounded-full px-2 py-0.5 text-xs ${
                      a.status === "failed"
                        ? "bg-rose-500/10 text-rose-700 dark:bg-rose-400/10 dark:text-rose-400"
                        : "bg-[var(--cb)] text-[var(--mut)]"
                    }`}
                  >
                    {uploading
                      ? i18n.t("mobile.composer.uploadUploading", { name: a.name })
                      : a.status === "failed"
                        ? i18n.t("mobile.composer.uploadFailed", {
                            name: a.name,
                            error: a.error ?? "",
                          })
                        : a.name}
                    {uploading && a.loaded !== undefined && (
                      <span
                        data-testid={`attach-progress-${a.id}`}
                        className="text-[var(--mut)] tabular-nums"
                      >
                        {i18n.t("mobile.composer.uploadedBytes", {
                          bytes: formatBytes(a.loaded),
                        })}
                        {a.total != null && pct !== null && ` / ${formatBytes(a.total)} · ${pct}%`}
                        {eta && ` · ${eta}`}
                      </span>
                    )}
                    <button
                      type="button"
                      data-testid={`attach-remove-${a.id}`}
                      aria-label={i18n.t("mobile.composer.removeAttachmentAria", { name: a.name })}
                      onClick={() => removeAttachment(a.id)}
                      className="text-[var(--mut)] hover:text-[var(--mut)]"
                    >
                      ×
                    </button>
                  </span>
                  {pct !== null && (
                    <div className="h-1.5 overflow-hidden rounded-full bg-[var(--cb)]">
                      <div
                        data-testid={`attach-progress-bar-${a.id}`}
                        className="h-full rounded-full bg-[var(--btnp)]"
                        style={{ width: `${pct}%` }}
                      />
                    </div>
                  )}
                </div>
              );
            })}
          </div>
          {/* 口径说明（线稿 :364 .upl-meta 逐字）：有附件在传 = 「上传中」态的呈现
              之一；上传结束/失败即收起，不常驻 */}
          {attachments.some((a) => a.status === "uploading") && (
            <p
              data-testid="attach-upl-meta"
              className="mt-1 text-[11px] leading-4 text-[var(--mut)]"
            >
              {i18n.t("mobile.composer.uplMeta")}
            </p>
          )}
          {/* 带宽横幅（§C5 / 复评 I-2 后半）：设计说明书原文是「在**文件面板与附件区**
              显示提示」——只在 FilePanel 出横幅，会让只上传、不浏览文件的用户永远
              看不到「这是通道限制，不是故障」。判据与文案复用 FilePanel 导出的
              ChannelBwNote（同一份判据 + 同一份文案，不新造口径），est 有限正数守卫
              也在组件内统一。落点：附件区（chips 容器之下）——用户进入附件流程即
              就地出现；纯问答/看板会话不常驻噪音（与 attach-hint「不平铺常驻」同
              取向）。横幅不占通道卡片位（§C4 仍 4 卡） */}
          <ChannelBwNote channel={channel} testId="composer-bw-note" className="mt-1" />
        </div>
      )}
      <div className="flex items-end gap-2">
        {/* 附件入口（2026-09-20）：+ 选择文件；「?」知情披露（存储到用户项目目录）。
            2026-10-04 用户裁决：两钮横排占横向空间 → 改上下竖排（+ 大在上、? 小在下） */}
        <span className="flex shrink-0 flex-col items-center gap-0.5">
          <input
            ref={fileInputRef}
            type="file"
            multiple
            className="hidden"
            data-testid="attach-file-input"
            onChange={(e) => {
              const fs = Array.from(e.target.files ?? []);
              e.target.value = "";
              if (fs.length > 0) void addFiles(fs);
            }}
          />
          <button
            type="button"
            data-testid="attach-add"
            aria-label={i18n.t("mobile.composer.attachAddAria")}
            aria-expanded={attachHintOpen}
            disabled={!injectable || noCwd}
            title={
              noCwd
                ? i18n.t("mobile.composer.attachAddTitleNoCwd")
                : i18n.t("mobile.composer.attachAddTitle")
            }
            onClick={() => fileInputRef.current?.click()}
            className="shrink-0 rounded-full p-1 text-[var(--mut)] hover:bg-[var(--cb)] disabled:opacity-40 dark:hover:bg-[var(--btnp)]"
          >
            <Plus size={20} />
          </button>
          <button
            type="button"
            data-testid="attach-help"
            aria-label={i18n.t("mobile.composer.attachHelpAria")}
            aria-expanded={attachHintOpen}
            onClick={() => setAttachHintOpen((v) => !v)}
            className="shrink-0 rounded-full p-0.5 text-[10px] leading-none text-[var(--mut)] hover:bg-[var(--cb)] dark:hover:bg-[var(--btnp)]"
          >
            <CircleHelp size={10} />
          </button>
        </span>
        <textarea
          ref={inputRef}
          data-testid="composer-input"
          aria-label={i18n.t("mobile.composer.inputAria")}
          value={text}
          maxLength={MAX_SEND_CHARS}
          onChange={(e) => {
            // Q2：用户编辑即前移草稿版本（在途人工继续的迟到入队不得代清新草稿）
            draftVersionRef.current += 1;
            setText(e.target.value.slice(0, MAX_SEND_CHARS));
          }}
          onPaste={handlePaste}
          rows={2}
          disabled={!injectable}
          placeholder={
            !injectable
              ? i18n.t("mobile.composer.sendDisabledFallback")
              : cardPresence === "questionFreeText"
                ? i18n.t(QUESTION_FREETEXT_PLACEHOLDER)
                : cardPresence === "questionBlocked"
                  ? i18n.t("mobile.composer.placeholderBlocked")
                  : cardPresence === "approve"
                    ? i18n.t(APPROVE_PRESENT_RECEIPT)
                    : i18n.t("mobile.composer.placeholder")
          }
          className="min-h-0 flex-1 resize-none rounded-lg border border-[var(--cb)] px-3 py-2 text-sm text-[var(--tx)] placeholder:text-[var(--mut)] focus:ring-2 focus:ring-[var(--btnp)] focus:outline-none disabled:opacity-50"
        />
        <button
          type="button"
          data-testid="composer-send"
          disabled={!canSend}
          onClick={handleSend}
          className="shrink-0 rounded-full bg-[var(--btnp)]/10 px-4 py-2 text-sm text-[var(--tx)] disabled:opacity-40"
        >
          {i18n.t(sending ? "mobile.composer.sending" : "mobile.composer.send")}
        </button>
      </div>
      {/* 知情披露（2026-09-20 用户要求）：「?」点开才显示——附件落在用户项目目录，
          已本地 git 排除不会提交；项目收尾可整目录清理。看过即收、不平铺常驻 */}
      {attachHintOpen && (
        <p
          data-testid="attach-hint"
          className="mt-1 rounded-lg bg-[var(--cbg)] px-2 py-1.5 text-[11px] leading-4 text-[var(--mut)]"
        >
          {i18n.t("mobile.composer.attachHint")}
        </p>
      )}
    </div>
  );
}
