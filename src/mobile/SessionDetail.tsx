// ZCode 式会话详情页（M3 Task 8，P9 前端）：
// - 运行中：thinking / tool-call 默认折叠可展开（wire collapsed 字段语义），
//   assistant 正文直接渲染（markdown + 代码高亮）；
// - 会话 status ∈ idle/finished（总结模式）：过程消息（thinking / tool-call /
//   tool-result）与非最终 assistant 一律自动折叠，只显最后一条 assistant 总结，
//   均可手动展开；
// - 消息正文中的已知文件路径（/session-files 提取结果）渲染为可点链接 → 文件预览；
// - 「加载更早消息」按钮以更大 limit 整页重拉（Task 8 裁决：M3 用按钮替代无限
//   滚动，YAGNI——避免滚动位置管理复杂度）；
// - SSE transition 仍不驱动详情页的**消息内容**（M3 范围裁决不变；内容走 10s
//   轮询），但会话**状态**随既有看板轮询数据保持同步（T1 活状态流：App 的
//   selected 按 id 对齐 Board 数据——红卡与总结模式随状态自动切换，无需重进页面）；
//   页面可见时每 10s 静默轮询
//   刷新会话内容（F6 评审裁决：hidden 暂停、恢复可见立即补刷，页头刷新按钮保留）。
//   轮询刷新仅在「贴底」（距底 <120px）时自动跟随落底，上翻阅读历史不被动拽回
//   （P2-B 评审修复）；首次加载与手动刷新仍无条件落底。
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { ReactNode, Ref } from "react";
import {
  ArrowDownToLine,
  ArrowLeft,
  ArrowUpToLine,
  ChevronDown,
  ChevronRight,
  PanelLeft,
  RotateCw,
  X,
} from "lucide-react";
import i18n from "@/i18n";
import ApproveCard from "./ApproveCard";
import PlanFeedbackBar from "./PlanFeedbackBar";
import { collapsedLabel, isProcessKind } from "./message-fold";
import { useMessageRenderers } from "./message-render";
import ModeBar from "./ModeBar";
import SubagentChips from "./SubagentChips";
import SubagentDetail from "./SubagentDetail";
import SubagentList from "./SubagentList";
import QuestionCard from "./QuestionCard";
// Codex Desktop IPC：原生投影接管内容源与交互卡（Codex APP 会话）
import CodexQueuePanel from "./CodexQueuePanel";
import PendingRequestCard, { CodexAvailabilityBanner } from "./PendingRequestCard";
import AsyncQuestionGroupCard from "./AsyncQuestionGroupCard";
import CodexOperationToolbar from "./CodexOperationToolbar";
import CodexAdaptSheet from "./CodexAdaptSheet";
import {
  fetchCodexAdaptStatus,
  updateCodexAdaptSettings,
  type CodexAsyncQuestionView,
} from "./codexIpcApi";
import {
  retainCapabilityDisplay,
  sameDisplayCache,
  type CapabilityDisplayCache,
} from "./codexCapabilityDisplay";
import AnswerBubble from "./AnswerBubble";
import ProjectedArtifacts, { buildArtifactMessageContexts } from "./ProjectedArtifacts";
import useArtifactImages from "./useArtifactImages";
import useCodexThread from "./useCodexThread";
import useDelayedUnsupported from "./useDelayedUnsupported";
import { useViewportKind } from "./useViewportKind";
import type { ViewportKind } from "./viewport";
import useArtifactJump, {
  ARTIFACT_JUMP_MAX_PAGES,
  type ArtifactJumpFailure,
  type ArtifactJumpHit,
  type ArtifactJumpState,
} from "./useArtifactJump";
import { buildArtifactFileView, type ArtifactJumpTarget } from "./artifact-links";
import { boundedCodexHistoryWindow } from "./codex-history";
import BookmarkBar from "./BookmarkBar";
import FilePanel, { type FilePanelEntry } from "./FilePanel";
import FilePreview from "./FilePreview";
import MessageComposer, { type ComposerDraftRequest } from "./MessageComposer";
// T05：页面共享停止控制器（composer 主按钮/紧凑入口与队列面板打断共用同一实例）
import { useCodexTurnControl } from "./useCodexTurnControl";
import PreviewModeSwitcher, { type PreviewMode } from "./PreviewModeSwitcher";
import SplitHandle from "./SplitHandle";
import {
  ApiError,
  fetchSessionFiles,
  fetchSessionMessages,
  fetchSessionSubagents,
  type SessionFileEntry,
  type SessionMessage,
  type SubagentView,
} from "./api";
import { STATUS_DOT_COLOR, TOOL_LABEL_KEYS } from "./board-logic";
import {
  addBookmark,
  bookmarkPreview,
  clearBookmarks,
  ensureBootId,
  listBookmarks,
  messageAnchor,
  removeBookmark,
  restoreBookmarks,
  BOOKMARK_LIMIT,
  type Bookmark,
} from "./bookmarks";
import type { Session } from "@/types/session";

/** 单次拉取条数（与后端 session-messages 默认 limit 一致） */
const PAGE_LIMIT = 200;
/** 后端 limit clamp 上限：到达后不再提供「加载更早消息」 */
const MAX_LIMIT = 1000;
/** F6：详情页轮询周期（页面可见时每 10s 静默刷新会话内容） */
const DETAIL_REFRESH_MS = 10_000;
/** P2-B（评审修复批）：轮询「贴底跟随」判定阈值——数据落地前采样，距底小于该值
 *  （px）视为贴底，轮询刷新才自动跟随落底；上翻阅读（距底 ≥ 阈值）时轮询刷新
 *  不改变滚动位置（首次加载 / 手动刷新不受此阈值约束，仍无条件落底） */
const POLL_FOLLOW_THRESHOLD_PX = 120;

/** 竖屏分屏（split）对话列最小高度保护（2026-09-20 用户裁决）：换位后对话列
 *  在底部、composer 占其底端——没有下限的话文件栏拖到 85% 时消息区会被压没。
 *  文件栏侧同步加 maxHeight = 100% - 该值，两处同源（常量单点） */
const SPLIT_CONVERSATION_MIN_PX = 120;

interface SessionDetailProps {
  /** 完整会话对象（Task 8 裁决：详情页需要 status 判定自动折叠、projectName 页头、
   *  agentType；比传 id+agentType 再反查简单） */
  session: Session;
  /** 返回看板 */
  onBack: () => void;
}

/** 预览区状态（T1 sheet 化，2026-10-09：原 list/file/subagent 三形联合的等价重构
 *  ——三形收敛为「看板 sheet + sheet 内选中态」）：
 *  - sheet：当前看板（文件 / 子 Agent 两看板平级，顶栏 sheet 钮切换）；
 *  - open：sheet 内选中态——none = 看板态；file = 单文件预览（files sheet 的二级）；
 *    subagent = 子 agent 详情（subagents sheet 的二级，活跃=实时预览自动刷新 /
 *    不活跃=定格快照）；
 *  - mode：预览布局三态（唯一切换器在顶栏 sheet bar，S5 裁决）；
 *  - backToList：open 项来源标记（从看板进入 → 内容页头显示返回按钮，
 *    从消息正文链接 / chip 直达进入 → 无返回按钮，既有行为不变） */
type PreviewState = {
  sheet: "files" | "subagents";
  open: { kind: "none" } | { kind: "file"; path: string } | { kind: "subagent"; id: string };
  mode: PreviewMode;
  backToList: boolean;
  /** 显式打开计数（Task 6）：每次用户显式打开文件 +1，作为 FilePreview 的刷新键
   *  ——同一路径内容已变、预览仍开着时再点一次也会重读当前文件。宽窄屏/布局切换
   *  不是新的打开，不递增。 */
  openSeq: number;
};

/** 该方向的**默认布局**（2026-10-27 用户裁决 U2/U3）：
 *  - 横屏（宽 > 高 × 1.2）→ 左消息 + 右文件窗格，即 files sheet 看板态 + 左右分屏；
 *  - 竖屏 → 聊天框（null = 不渲染任何预览窗格，与既有默认一致）。
 *  纯函数、每次返回新字面量：内部不发请求也不订阅，可在 useState 的 lazy initializer
 *  里安全地跑两次（StrictMode 双调）。openSeq 归零——旋转不是「显式打开」。 */
function defaultPreviewFor(kind: ViewportKind): PreviewState | null {
  if (kind !== "landscape") return null;
  return {
    sheet: "files",
    open: { kind: "none" },
    mode: "split-h",
    backToList: false,
    openSeq: 0,
  };
}

/** 面板默认追溯档位（首屏数据源，与详情页默认 limit 同标尺） */
const FILE_DEFAULT_SCOPE = 200;

/** 字号档位（2026-09-16 用户裁决）：只作用于消息正文与文件预览内容，
 *  UI 与页头不受影响（mobile.css 的 [data-font-scale] 变量覆写） */
const FONT_SCALES = [0.5, 0.75, 1, 1.25] as const;

/** 拉取失败态：status=null 表示网络层异常（无 HTTP 状态可读） */
interface LoadError {
  status: number | null;
}

// ============================================================
// 纯函数小件（组件外，独立可测）
// ============================================================

// collapsedLabel / isProcessKind 迁至 ./message-fold（2026-09-20 归档详情对齐批，
// 纯搬家零语义变化——归档页共用同一套摘要文案与过程 kind 判定）

// sortedPaths / linkifyMarkdown / linkifySegments / extractPlanBody 与渲染器
// （renderMarkdown / renderLinkifiedText / renderBody）迁至 ./message-render
// （2026-10-09 观察台 T6，纯搬家零语义变化——子 agent 详情共用同一套渲染）

/** 贴底判定（P2-B）：距底距离（scrollHeight - scrollTop - clientHeight）落在
 *  阈值窗口内即贴底。轮询 tick 刷新前对消息滚动容器采样一次，作为该次刷新
 *  数据落地后是否跟随落底的依据 */
function isNearBottom(el: HTMLDivElement): boolean {
  return el.scrollHeight - el.scrollTop - el.clientHeight < POLL_FOLLOW_THRESHOLD_PX;
}

/** 「跳到最新」按钮显隐阈值：距底超过该值才出现——贴底阅读时它是纯噪声 */
export const JUMP_SHOW_THRESHOLD_PX = 240;

/** Task 7：命中高亮时长（毫秒）——命中消息与节点短暂高亮约两秒后清理。 */
export const ARTIFACT_JUMP_HIGHLIGHT_MS = 2000;
/** Task 7：命中后等待目标 DOM 可用的上限（更早页合并后列表重渲染的短窗口）。 */
export const ARTIFACT_JUMP_DOM_WAIT_MS = 1000;

/** 命中节点在消息内的 DOM 定位：附件按 artifactId、正文按 data-file-path，
 *  同消息多处命中取**最后一个**节点（与纯函数 findLastFileReference 的语义一致）。 */
function jumpHitNode(messageEl: HTMLElement, hit: ArtifactJumpHit): HTMLElement | null {
  let attribute: "data-artifact-id" | "data-file-path";
  let value: string;
  if (hit.via === "artifact" && hit.artifactId !== null) {
    attribute = "data-artifact-id";
    value = hit.artifactId;
  } else if (hit.path !== null) {
    attribute = "data-file-path";
    value = hit.path;
  } else {
    return null;
  }
  let found: HTMLElement | null = null;
  for (const el of messageEl.querySelectorAll<HTMLElement>(`[${attribute}]`)) {
    if (el.getAttribute(attribute) === value) found = el;
  }
  return found;
}

/** 定位状态文案（搜索/命中/范围内未命中/到头/失败各自成文；失败不伪装未命中）。 */
function artifactJumpText(state: ArtifactJumpState): string {
  switch (state.phase) {
    case "searching":
      return i18n.t("mobile.artifacts.locate.searching", {
        pages: state.pages,
        max: ARTIFACT_JUMP_MAX_PAGES,
      });
    case "hit":
      return i18n.t("mobile.artifacts.locate.hit", { page: state.hit?.page ?? state.pages });
    case "miss":
      return i18n.t("mobile.artifacts.locate.miss");
    case "end":
      return i18n.t("mobile.artifacts.locate.end");
    case "failed":
      return artifactJumpFailureText(state.failure);
    default:
      return "";
  }
}

function artifactJumpFailureText(failure: ArtifactJumpFailure | null): string {
  switch (failure?.kind) {
    case "history_incomplete":
      return i18n.t("mobile.artifacts.locate.failure.historyIncomplete");
    case "cursor_invalid":
      return i18n.t("mobile.artifacts.locate.failure.cursorInvalid");
    case "generation_invalid":
      return i18n.t("mobile.artifacts.locate.failure.generationInvalid");
    case "load_failed":
    default:
      return i18n.t("mobile.artifacts.locate.failure.loadFailed");
  }
}

/** 消息滚动区（两个布局分支共用，2026-09-20 抽取）：滚动容器 + 右下角
 *  「跳到最新」浮动按钮。对话一长，手翻到最新要很久（用户实测）；
 *  点击瞬时落底并立即恢复轮询跟随（P2-B 采样语义不变——跳底本就是「我要贴底」）。
 *  「跳到顶部」（2026-09-20 归档对齐批）：右上角镜像钮，距顶超阈值浮现，点击落 0
 *  （到顶 = 本次加载窗口的顶——更早内容靠「加载更早消息」分页）。
 *  wrapper 持 relative 定位、滚动容器在内层：浮动按钮若放进滚动容器内部
 *  会随内容滚走，放 wrapper 上才能常驻边角 */
function MessageScrollArea({
  ref: areaRef,
  fontScale,
  showJump,
  showJumpTop,
  onScroll,
  onJump,
  onJumpTop,
  children,
}: {
  /** 滚动容器 ref（React 19 ref-prop 通道；自建 areaRef prop 触发 react-hooks/refs） */
  ref?: Ref<HTMLDivElement>;
  fontScale: number;
  showJump: boolean;
  showJumpTop: boolean;
  onScroll: () => void;
  onJump: () => void;
  onJumpTop: () => void;
  children: ReactNode;
}) {
  return (
    <div className="relative min-h-0 min-w-0 flex-1">
      <div
        ref={areaRef}
        data-testid="message-area"
        data-font-scale={fontScale}
        className="h-full overflow-y-auto px-3 pt-3"
        onScroll={onScroll}
      >
        {children}
      </div>
      {showJumpTop && (
        <button
          type="button"
          data-testid="jump-to-top"
          aria-label={i18n.t("mobile.detail.jumpTop")}
          title={i18n.t("mobile.detail.jumpTop")}
          onClick={onJumpTop}
          className="absolute top-3 right-3 z-10 rounded-full border border-[var(--cb)] bg-[var(--cbg)] p-2 text-[var(--mut)] shadow-md hover:bg-[var(--cbg)] dark:hover:bg-[var(--btnp)]"
        >
          <ArrowUpToLine size={16} />
        </button>
      )}
      {showJump && (
        <button
          type="button"
          data-testid="jump-to-bottom"
          aria-label={i18n.t("mobile.detail.jumpLatest")}
          title={i18n.t("mobile.detail.jumpLatest")}
          onClick={onJump}
          className="absolute right-3 bottom-3 z-10 rounded-full border border-[var(--cb)] bg-[var(--cbg)] p-2 text-[var(--mut)] shadow-md hover:bg-[var(--cbg)] dark:hover:bg-[var(--btnp)]"
        >
          <ArrowDownToLine size={16} />
        </button>
      )}
    </div>
  );
}

/** **计划待确认预期态**（丁T2，纯函数，镜像后端 `remote::api::plan_pending_tail_index`）：
 *  消息尾部存在计划类消息（`kind="plan"` / `"plan-file"`）且其后**无**用户消息、无工具
 *  事件 → 终端正在等这个计划的确认。
 *
 *  **为什么要在前端也判一份**（后端已判、字段为 `planPending`）：ApproveCard 的挂载门是
 *  刻意的红灯门（丁T1 裁决），而 codex 的计划提案**不落 Waiting**（`codex_parser` 的兜底红
 *  已废）——没有这一判据，卡根本不会挂载，后端就算把 `available=true` 算出来也没有消费方。
 *  前端这份的作用是**决定要不要去打那一发 GET**（挂载门），后端那份才是**权威判定**
 *  （`available` / `planPending` 载荷）；两者同源同判据，前端**只放宽门，不做可用性裁决**
 *  ——`available=false`（非码族工具 / 计划已消费 / 后端判定不同）时卡片照常自隐。
 *
 *  `tool` 收窄到计划对话框族（codex/kimi——镜像后端 `plan_dialog_family`）；**claude
 *  2026-10-04 起纳入**：其计划批准等待经状态层修复已落 Waiting（红灯门本就开），纳入
 *  纯为与后端判据同源（后端扫描器的 claude 计划预期态补位见 `remote::api` 同日注），
 *  并覆盖「终端对话框未绘制、载荷走 planPending 形态」时的挂载面。当年排除 claude 的
 *  依据「走既有 waiting 门（Waiting + detect）就够」已被实况证伪：detect 对原生 UI
 *  文案恒 miss（详见后端注释的取证链）。
 *
 *  **未覆盖面（丁T2 复审 N4，与后端判据同款，如实申报）**：三条清除信号（user /
 *  tool-call / tool-result）并非穷尽——codex 选「No, stay in Plan mode」后既不注入用户
 *  消息也不必然产工具事件，预期态可能长挂（提示条持续显示）。真机 203 条 rollout 未
 *  观察到该样本，标为待取证（详见后端 `plan_pending_tail_index` 文档的同名小节）。 */
export function isPlanPending(
  messages: SessionMessage[] | null,
  tool: string | null | undefined
): boolean {
  if (messages === null || messages.length === 0) return false;
  if (tool !== "codex" && tool !== "kimi" && tool !== "claude") return false;
  let last = -1;
  for (let i = messages.length - 1; i >= 0; i -= 1) {
    const k = messages[i].kind;
    if (k === "plan" || k === "plan-file") {
      last = i;
      break;
    }
  }
  if (last < 0) return false;
  for (let i = last + 1; i < messages.length; i += 1) {
    const k = messages[i].kind;
    if (k === "user" || k === "tool-call" || k === "tool-result") return false;
  }
  return true;
}

// ============================================================
// 组件
// ============================================================

type CodexQuestionGroup = {
  key: string;
  members: CodexAsyncQuestionView[];
  live: CodexAsyncQuestionView[];
  allAnswered: boolean;
};

/** 按来源 turn/item 分组，供停靠区与已答锚清理共用（去重 + 题号排序）。 */
function groupCodexQuestions(questions: CodexAsyncQuestionView[]): CodexQuestionGroup[] {
  const unique = questions.filter(
    (question, index, all) =>
      all.findIndex((other) => other.questionKey === question.questionKey) === index
  );
  const grouped = new Map<string, CodexAsyncQuestionView[]>();
  for (const question of unique) {
    const key = `${question.sourceTurnId}|${question.sourceItemId}`;
    grouped.set(key, [...(grouped.get(key) ?? []), question]);
  }
  return [...grouped.entries()].map(([key, members]) => {
    const sorted = [...members].sort((a, b) => a.questionIndex - b.questionIndex);
    return {
      key,
      members: sorted,
      live: sorted.filter(
        (question) => question.state === "pending" || question.state === "temporarily_blocked"
      ),
      allAnswered: sorted.length > 0 && sorted.every((question) => question.state === "answered"),
    };
  });
}

export default function SessionDetail({ session, onBack }: SessionDetailProps) {
  const [messages, setMessages] = useState<SessionMessage[] | null>(null);
  // ===== Codex Desktop IPC 投影（A snapshot）=====
  // 仅 Codex 会话订阅（其余工具 enabled=false 零开销）。可用性按服务端数据渲染：
  // - controllable / send_only / read_only：投影**替换** Codex 会话实时内容源（不与
  //   本地读取简单混排——同一历史两份源各说各话是骗人），原生 pending 出卡（只读档
  //   卡面按 actionable=false 只读展示）、队列面板挂载；read_only = #172 解耦新增的
  //   只读档（读链已取证、写未开放）；
  // - compat_only / unavailable：不出投影正文与原生卡——保持旧 QuestionCard 路径，
  //   能力横幅仍按服务端档位诚实展示；
  // - state=null（无 IPC / 拉取中）：旧路径照旧，零回归。
  // stale：保留最后视图但禁写。
  // H12：**来源随形态**——只有 Desktop(App) 形态的 codex 会话才订阅投影；
  // CLI/TUI 同 thread 即使被 Desktop 认识，也必须走终端卡路径，不能取得投影。
  const codex = useCodexThread(session.id, session.agentType === "codex" && session.form === "app");
  /** 附件图片资源控制器（Task 6）：整页共用一份，保证同会话消费者共享请求与
   *  object URL、全局最多两个在途；换会话时 hook 内部销毁旧 store。 */
  const artifactImages = useArtifactImages(session.id);
  // T05：页面共享停止控制器——**单例在本层持有**，composer 主按钮/紧凑入口与
  // 队列面板打断确认共用（stopping 互斥闸防多按钮各发一次破坏性打断；结果分诊、
  // 30 秒预算、Unknown 纪律都在 hook 内）。非 App 形态投影恒 null → 惰性控制器。
  const codexTurnControl = useCodexTurnControl(codex.state, session.id);
  /** 每条消息的附件上下文（同回合提示词 / 每回合 12 图预算）：以 canonical 合并顺序
   *  的 loadedMessages 为准，翻页合并后仍只按真实回合取提示词，不借上一回合。 */
  const artifactContexts = useMemo(
    () => buildArtifactMessageContexts(codex.loadedMessages),
    [codex.loadedMessages]
  );
  /** 队列刷新独立信号（2026-10-20 复审 P2）：不依赖 Desktop 必须产生新帧——
   *  投影 revision 变化与发送回执各推进一次；旧会话的回调随组件卸载丢弃。 */
  const [queueTick, setQueueTick] = useState(0);
  /** 底部只展开一组；其余有效组收成一行切换（卡片仍挂载保草稿）。 */
  const [activeGroupKey, setActiveGroupKey] = useState<string | null>(null);
  // T04（spec §3.2）：其余有效组收进唯一「另有 N 组」入口的展开态（切换组后自动收起）。
  const [otherGroupsOpen, setOtherGroupsOpen] = useState(false);
  // F08：适配详情/开关（移动端页内 sheet；只读展示 + 与桌面同源开关）。
  const [codexAdaptOpen, setCodexAdaptOpen] = useState(false);
  // R16：首次进入 App 会话的**轻提示**（一次；不要求用户先打开详情）。
  const [adaptHintVisible, setAdaptHintVisible] = useState(false);
  // S5：发送静态门「可尝试」结论（唯一来源 = MessageComposer 拉取的 send-info——
  // send_gate_static 的展示面单点；本组件不另发请求不另判）。能力横幅据此在
  // 可尝试时不再宣称「暂不能远程发送」（设计 §3.4）。换会话由 composer 重报。
  const [codexSendGate, setCodexSendGate] = useState<boolean | undefined>(undefined);
  useEffect(() => {
    // 换会话即失效（等新 composer 的 send-info 重报；undefined = 不猜）
    setCodexSendGate(undefined);
  }, [session.id]);
  // F8/R6：send-info 外部刷新信号——三类**后端真实状态变化**都推进此 tick，
  // composer 据此事件驱动重拉（不做时刻轮询、不引 Tauri channel）：
  // ① 同页打断受理/恢复队列（CodexQueuePanel 的 onQueueStateChanged）；
  // ② 能力档位翻转（设置页关闭/重开自动适配——R6 后端 republish 生产帧，
  //    availability 随 caps 活门翻转，SSE 下游真收到）；
  // ③ 投影帧到达（跨设备起源）。R-T4 起投影帧自带 queuePaused，且暂停变化点
  //    （打断前置落行/撤回/resume/settle）各自 republish——**只翻 queuePaused** 的
  //    帧（revision 不动，Desktop 零新帧）正是 B 打断 RPC 超时 UnknownDelivery 时
  //    A 能看到暂停的唯一通道；send-info 仍是暂停门的唯一权威读取口。③ 走去抖
  //    合并（流式输出每 ~100ms 推进 revision，若每次都重拉会打爆 send-info；自
  //    首个未处理触发起超过上限即立即重拉——与 CodexQueuePanel 的 refreshSignal
  //    同款节奏）。
  const [sendInfoTick, setSendInfoTick] = useState(0);
  // 草稿正文和待回填队列归详情页持有，布局切换重挂 composer 不丢稿、不重放。
  const [editDrafts, setEditDrafts] = useState<ComposerDraftRequest[]>([]);
  const [composerDrafts, setComposerDrafts] = useState<Record<string, string>>({});
  const editDraft = editDrafts.find((draft) => draft.sessionId === session.id);
  const enqueueEditDraft = useCallback((draft: ComposerDraftRequest) => {
    setEditDrafts((current) => [...current, draft]);
  }, []);
  const consumeEditDraft = useCallback(
    (actionId: string) => {
      setEditDrafts((current) =>
        current.filter((draft) => draft.sessionId !== session.id || draft.actionId !== actionId)
      );
    },
    [session.id]
  );
  const changeDraftText = useCallback(
    (value: import("react").SetStateAction<string>) => {
      setComposerDrafts((current) => ({
        ...current,
        [session.id]: typeof value === "function" ? value(current[session.id] ?? "") : value,
      }));
    },
    [session.id]
  );
  const bumpSendInfo = useCallback(() => setSendInfoTick((t) => t + 1), []);
  // R02：消息进入 codex 队列后也必须重拉 send-info——空暂停在发送前自隐，
  // 但新消息留队后暂停会冻结接力，必须立即显示“队列已暂停，恢复后发送”。
  const bumpQueue = useCallback(() => {
    setQueueTick((t) => t + 1);
    bumpSendInfo();
  }, [bumpSendInfo]);
  // T05 刷新通知：停止落地（任何结果——含不确定）→ 队列与 send-info 各刷新一次
  // （回合终态、屏障释放、打断后的持久暂停都可能已变化；与队列面板 F8 同一链路）
  useEffect(() => {
    if (codexTurnControl.result === null) return;
    bumpQueue();
  }, [codexTurnControl.result, bumpQueue]);
  const codexAvailability = codex.state?.availability ?? null;
  const codexRuntime = codex.state?.runtimeStatus ?? null;
  const codexPendingCount = codex.state?.pending.length ?? -1;
  const codexRevisionNow = codex.state?.revision ?? null;
  // R-T4：队列暂停事实（旧载荷缺省 undefined → 与 false 同签——只有真实翻转才触发）
  const codexQueuePaused = codex.state?.queuePaused === true;
  const prevFrameSigRef = useRef<string | null>(null);
  const sendInfoDebounceRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const sendInfoFirstPendingAtRef = useRef<number | null>(null);
  useEffect(() => {
    // 帧签名：档位 / 运行态 / pending 数 / revision / 队列暂停任一变化 = 一帧事件。
    // 首拍（null → 首帧）不触发——composer 挂载时本来就会拉一次。
    // Gate F 修正：挂载渲染（codex.state=null）**不是帧**——不写哨兵。此前哨兵在
    // 挂载即被写成 null 态签名字符串，导致首帧（snapshot）被误判为变化：去抖
    // 250ms 后必然多拉一次 send-info（浪费请求；对照测试里会拉到过渡期响应）。
    // availability=null ⇔ codex.state=null（投影未达），据此判定「非帧」。
    const sig =
      codexAvailability === null
        ? null
        : `${codexAvailability}|${codexRuntime}|${codexPendingCount}|${codexRevisionNow}|${codexQueuePaused ? "paused" : "resumed"}`;
    if (sig === null) return;
    const prev = prevFrameSigRef.current;
    prevFrameSigRef.current = sig;
    // 首帧（prev=null = 哨兵未置）只记录不触发——composer 挂载时本来就拉一次；
    // 此后只有签名真变化（availability/runtimeStatus/pending 数/revision/queuePaused）
    // 才触发。
    if (prev === null || prev === sig) return;
    // 去抖 + 上限等待（CodexQueuePanel 同款：合并流式帧、不被流式饿死）
    if (sendInfoDebounceRef.current !== null) clearTimeout(sendInfoDebounceRef.current);
    const now = Date.now();
    if (sendInfoFirstPendingAtRef.current === null) sendInfoFirstPendingAtRef.current = now;
    const elapsed = now - sendInfoFirstPendingAtRef.current;
    const delay = Math.max(0, Math.min(250, 2000 - elapsed));
    sendInfoDebounceRef.current = setTimeout(() => {
      sendInfoDebounceRef.current = null;
      sendInfoFirstPendingAtRef.current = null;
      bumpSendInfo();
    }, delay);
  }, [
    codexAvailability,
    codexRuntime,
    codexPendingCount,
    codexRevisionNow,
    codexQueuePaused,
    bumpSendInfo,
  ]);
  useEffect(
    () => () => {
      if (sendInfoDebounceRef.current !== null) {
        clearTimeout(sendInfoDebounceRef.current);
        sendInfoDebounceRef.current = null;
      }
    },
    []
  );
  // R16：**稳定来源路由**——Codex App 会话只走投影+新控件，断链也不回旧 TUI 卡；
  // CLI 会话保持旧的终端卡路径。
  const appForm = session.agentType === "codex" && session.form === "app";
  useEffect(() => {
    if (!appForm) return;
    let alive = true;
    void (async () => {
      try {
        const details = await fetchCodexAdaptStatus(session.id);
        if (alive) setAdaptHintVisible(!details.hintSeen);
      } catch {
        // 读不到详情不打扰用户（提示只是辅助）。
      }
    })();
    return () => {
      alive = false;
    };
  }, [appForm, session.id]);
  const dismissAdaptHint = useCallback(() => {
    setAdaptHintVisible(false);
    void updateCodexAdaptSettings({ sessionId: session.id, action: "dismiss_hint" }).catch(
      () => undefined
    );
  }, [session.id]);
  const codexReady = codex.state !== null;
  const codexRevision = codex.state?.revision ?? null;
  useEffect(() => {
    if (codexReady) bumpQueue();
  }, [codexRevision, codexReady, bumpQueue]);
  const codexState = codex.state;
  // 有效卡：当前可答组，或本机已发出过动作（sessionStorage 锚）的待核对组。
  // 历史已答/无锚 unknown 一律不渲染（2026-10-28 用户裁决）。
  const inbox = useMemo(() => {
    const pending = (codexState?.pending ?? []).filter(
      (item, index, all) => all.findIndex((other) => other.requestKey === item.requestKey) === index
    );
    const groups = groupCodexQuestions(codexState?.asyncQuestions ?? []).map((group) => ({
      ...group,
      hasAnchor: Boolean(
        window.sessionStorage.getItem(
          `codex:group:${session.id}:${group.members[0]?.sourceTurnId}:${group.members[0]?.sourceItemId}`
        )
      ),
    }));
    const visibleGroups = groups.filter(
      (group) => group.live.length > 0 || (group.hasAnchor && !group.allAnswered)
    );
    return {
      pending,
      groups: visibleGroups,
      count: pending.length + visibleGroups.length,
    };
  }, [codexState, session.id, queueTick]);

  const activeGroup =
    inbox.groups.find((group) => group.key === activeGroupKey) ??
    inbox.groups.find((group) => group.live.length > 0) ??
    inbox.groups[0] ??
    null;
  // T04（spec §3.2）：其余**有效**组（已答组已被 inbox 过滤，不计数）。
  const otherGroups = inbox.groups.filter((group) => group.key !== activeGroup?.key);
  useEffect(() => {
    if (activeGroupKey !== null && !inbox.groups.some((group) => group.key === activeGroupKey)) {
      setActiveGroupKey(null);
    }
  }, [activeGroupKey, inbox.groups]);

  // 已答组的旧 sessionStorage 锚只用于核对，不应跨组留存；在数据更新后清理。
  useEffect(() => {
    for (const group of groupCodexQuestions(codexState?.asyncQuestions ?? [])) {
      if (!group.allAnswered) continue;
      const first = group.members[0];
      window.sessionStorage.removeItem(
        `codex:group:${session.id}:${first.sourceTurnId}:${first.sourceItemId}`
      );
    }
  }, [codexState, session.id]);
  const codexProjectionActive =
    codexState !== null &&
    (codexState.availability === "controllable" ||
      codexState.availability === "send_only" ||
      codexState.availability === "read_only");
  // R07（能力事实同源 + 真实重连展示）：页面同版 `effectiveOperations` 是工具栏与
  // 适配详情**共同**的事实源。展示缓存按 `capabilityIdentity` 键控——同一完整目标
  // 在真实重连（stale、证据槽换代失配）期间沿用上次支持展示；identity 变化即清。
  // 纪律：展示缓存只补 `support`，`allowed`/`evidence`/`reasonCode` 一字不动。
  //
  // 为什么用 state（而不是 ref）：React 19 的 react-hooks/refs 禁止渲染期读 ref。
  // 缓存本身是"上一帧的结果"，用 state 承载 + 内容相等短路（避免自激重渲染）；
  // 会话归属随缓存一起记，换会话即视为无缓存（上一目标的展示事实不跨会话沿用）。
  const [codexDisplayCache, setCodexDisplayCache] = useState<{
    sessionId: string;
    cache: CapabilityDisplayCache;
  } | null>(null);
  const codexDisplay = useMemo(() => {
    const usable = codexDisplayCache?.sessionId === session.id ? codexDisplayCache.cache : null;
    return retainCapabilityDisplay(usable, codexState?.effectiveOperations, {
      stale: codexState?.stale === true,
    });
  }, [codexDisplayCache, codexState?.effectiveOperations, codexState?.stale, session.id]);
  useEffect(() => {
    const next =
      codexDisplay.cache === null ? null : { sessionId: session.id, cache: codexDisplay.cache };
    if (sameDisplayCache(codexDisplayCache, next)) return;
    setCodexDisplayCache(next);
  }, [codexDisplay, codexDisplayCache, session.id]);
  // Task 7：瞬态 Unsupported 只延迟**前端错误提示**（后端 contentState 即时、禁写/
  // stale/屏障不去抖）——过渡 164–382ms 不闪烁，持续异常 500ms 后如实显示。
  const showUnsupported = useDelayedUnsupported(
    session.id,
    codexProjectionActive && codexState?.contentState === "unsupported"
  );
  // 头部截断标记（Bug 1，M3 验收）：后端字节窗切掉文件头时为 true——
  // 即使本页条数 < limit 也存在更早内容，按钮必须可见
  const [truncated, setTruncated] = useState(false);
  const [error, setError] = useState<LoadError | null>(null);
  const [loading, setLoading] = useState(true);
  // 「加载更早消息」：limit 递增整页重拉（后端取文件序尾部 limit 条）
  const [limit, setLimit] = useState(PAGE_LIMIT);
  // 手动刷新信号（页头刷新按钮/错误重试）；F6 轮询同样 bump 此信号复用重拉链路
  const [refreshTick, setRefreshTick] = useState(0);
  // 折叠覆盖表：seq → 强制折叠/展开；缺省走默认折叠语义（见 isCollapsed）
  const [expandedOverride, setExpandedOverride] = useState<Map<number, boolean>>(new Map());
  // 过程一键折叠模式位（2026-10-04 修复「点了没反应」）：true = 全部过程消息
  // （thinking/tool-call/tool-result）**默认收起**——包括折叠之后新到达的消息
  // （sticky，不随 10s 轮询翻回）。单条 override 优先级仍最高。
  const [processAllCollapsed, setProcessAllCollapsed] = useState(false);
  // 计划反馈态（2026-10-04 计划批准卡）：审批卡反馈入口 start 成功（终端已进
  // 反馈编辑态）→ 置位，dock 里换渲染 PlanFeedbackBar（审批卡随状态转黄自然
  // 卸载，反馈条独立存活；发送成功/收起即复位）。仅前端持有——会话切换即清，
  // 刷新后不恢复（终端直接打字即可，如实登记的已知边界）。
  const [planFeedbackActive, setPlanFeedbackActive] = useState(false);
  // 计划反馈态随会话切换复位（跨会话串态防线——与卡片 key 重挂同口径）
  useEffect(() => {
    setPlanFeedbackActive(false);
  }, [session.id]);
  // 该会话涉及的文件（/session-files 提取结果，M3+）：一份数据两用——
  // fileEntries 驱动文件面板列表，派生 Set 驱动正文路径链接化
  const [fileEntries, setFileEntries] = useState<SessionFileEntry[]>([]);
  const [fileTruncated, setFileTruncated] = useState(false);
  // 子 agent 全量名单（观察台 §二）：单一数据源供三处消费——chip（过滤 running）/
  // 文件面板卡区（全量绿灰点）/ 详情对话框（status 查询）。拉取 effect 在下方
  // 文件表拉取旁；**finished 只付首拉一次**（tick 冻结为 0，见 effect 注）；
  // 失败静默保留上一份（不闪断），首拉失败维持 null（卡区/chip 不渲染）
  const [subagentList, setSubagentList] = useState<SubagentView[] | null>(null);
  const subagentTick = session.status === "finished" ? 0 : refreshTick;
  // 追溯档位（M3+ 用户裁决 3）：面板三档 200/500/1000，切档重拉
  const [fileScope, setFileScope] = useState<number>(FILE_DEFAULT_SCOPE);
  // 初值 true（2026-10-27 横竖屏自适应配套）：横屏进详情会**立即挂载文件看板**，
  // 初值 false 会先闪一帧「该范围内未发现文件」（FilePanel 的 panel-empty 判据是
  // entries 为空 ∧ 非 loading）。下方拉取 effect 挂载即置 true、finally 置 false，
  // 改初值只消除首帧空态，不改变任何时序。顺带修掉手动开面板的同款闪帧。
  const [fileLoading, setFileLoading] = useState(true);
  // ===== Task 7：文件池合并与五页反向定位 =====
  // 引用范围 = 当前显示窗口（尾窗 + 已滚出的保留缓冲）+ Task 5 预算内更早页：
  // `boundedCodexHistoryWindow` 在这里只做边界收敛的便利——保留缓冲必须并入，
  // 否则接缝消息在列表里看得见、却不参与来源排序；第六页及更早即使被注入缓存
  // 也不参与。
  const fileReferenceRange = useMemo(
    () =>
      boundedCodexHistoryWindow(
        codex.state?.messages ?? [],
        codex.historyPages,
        codex.state?.historyToken ?? "",
        codex.retained
      ),
    [codex.state, codex.historyPages, codex.retained]
  );
  /** 文件面板视图：后端条目 + 缺失结构化引用；来源优先级/已修改标志保持后端口径，
   *  排序按本次范围内的最近引用倒序（其余条目保持后端相对顺序）。 */
  const fileView = useMemo(
    () => buildArtifactFileView(fileEntries, fileReferenceRange),
    [fileEntries, fileReferenceRange]
  );
  /** 五页定位状态机（纯逻辑在 useArtifactJump；本组件只接线、切布局与高亮）。 */
  const artifactJump = useArtifactJump({
    sessionId: session.id,
    state: codex.state,
    historyPages: codex.historyPages,
    retained: codex.retained,
    loadEarlier: codex.loadEarlier,
    fileEntries,
  });
  // 字号档位（默认 100%）
  const [fontScale, setFontScale] = useState<number>(1);
  // 书签表（M3+）：初值从模块级 store 读——SessionDetail 卸载重挂后仍恢复
  const [bookmarks, setBookmarks] = useState<Bookmark[]>(() => listBookmarks(session.id));
  // 跳转失败提示（书签指向更早范围，当前窗口内找不到）
  const [bookmarkJumpMiss, setBookmarkJumpMiss] = useState(false);
  // 方向判定（2026-10-27 用户裁决 U1：宽 > 高 × 1.2 = 横屏，其余竖屏）：默认布局与
  // 分辨率变化时的重置都以它为唯一依据；变化监听见下方「方向变化」effect
  const viewportKind = useViewportKind();
  // 预览态初值按当前方向给默认（U2/U3）：横屏 = 右栏文件看板 + 左右分屏；竖屏 = null = 聊天框。
  // 用 lazy initializer 而非「先 null 再 effect 补齐」——后者会先画一帧纯聊天再弹出右栏
  const [preview, setPreview] = useState<PreviewState | null>(() =>
    defaultPreviewFor(viewportKind)
  );
  /** Task 7：定位动作在窄屏关闭的全屏预览快照——失败/结束后可原样返回；
   *  宽屏分屏下预览仍在，快照为 null（不需要返回入口）。 */
  const [jumpReturnPreview, setJumpReturnPreview] = useState<PreviewState | null>(null);
  /** Task 7：命中高亮的计时器与已标记 DOM 节点（清理时逐个摘属性）。 */
  const jumpHighlightTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const jumpHighlightNodesRef = useRef<HTMLElement[]>([]);
  // 文件栏占比（可拖分隔条，需求 2026-09-16）：两形态各自保留用户拖出的比例，
  // 初值 0.5（对半分，与旧版 h-1/2 / w-1/2 观感一致）
  const [fileRatioV, setFileRatioV] = useState(0.5);
  const [fileRatioH, setFileRatioH] = useState(0.5);
  // 分屏容器 ref：拖动换算的尺寸来源（SplitHandle 内取 getBoundingClientRect）
  const splitRef = useRef<HTMLDivElement>(null);
  // 消息区 ref（滚动到底 / 加载更早的位置锚定）
  const messageAreaRef = useRef<HTMLDivElement>(null);
  // 待回补的滚动锚（加载更早前记录；非 null 表示下次数据落地要做位置补偿）
  const pendingScrollRef = useRef<{ prevHeight: number; prevTop: number } | null>(null);
  // 「跳到最新」按钮显隐（2026-09-20）：距底超过阈值才出现。由滚动容器的
  // onScroll 驱动（此前消息区没有 onScroll 监听）；不动 pollFollowRef——
  // P2-B 的轮询前采样语义保持原样
  const [showJump, setShowJump] = useState(false);
  // 「跳到顶部」浮动钮显隐（距顶超阈值；2026-09-20 归档对齐批与归档页同款交互）
  const [showJumpTop, setShowJumpTop] = useState(false);
  // P2-B：下一次数据落地是否「跟随落底」的信号（等价于落底函数的 follow 参数）——
  // 轮询 tick 刷新前采样贴底状态写入；手动刷新（retry）置 true 无条件落底；
  // 初值 true 使首次加载落底。ref 而非 state：纯信号不驱动渲染
  const pollFollowRef = useRef(true);
  // 当前形态对应的占比与写回口（横向/纵向各记一份，来回切换不丢用户拖出的比例）
  const fileRatio = preview?.mode === "split-h" ? fileRatioH : fileRatioV;
  const setFileRatio = preview?.mode === "split-h" ? setFileRatioH : setFileRatioV;

  // 总结模式（P9）：会话已结束/空闲 → 只显最后 assistant 总结，过程消息自动折叠
  const isSummary = session.status === "idle" || session.status === "finished";

  // 丁T2：**计划待确认预期态**（详情页挂载门的数据源）——codex/kimi 的计划提案不落
  // Waiting，仅靠 `status === "waiting"` 门会让审批卡永不挂载（问题 4 的挂载面根因）。
  // 判据与后端 `remote::api::plan_pending_tail_index` 同源同口径（见上方 isPlanPending
  // 注释）；此处**只放宽门**，可用性仍由后端载荷的 `available` 裁决（卡自隐兜底）。
  //
  // **只排除 `finished`，不用 isSummary**（丁T2 复评 F3-1，Critical 修复）：
  // **codex 计划提案后的真机状态是 Idle**（`assistant(<proposed_plan>)` → `task_complete`
  // → `TurnEnd → Idle`；codex 的兜底红已废，`codex_parser` 不落 Waiting）。而 `isSummary`
  // （`:360`）把 idle 也算「已聊完」→ 首版实现下 `planPending` 恒 false →
  // **审批卡永不挂载，后端算出的 `available=true` 没有消费方**。
  //
  // 为什么 `finished` 仍排除：那是**会话真的结束**（进程退出/归档），重进详情不必再打
  // GET；`idle` 只是「当前无回合在跑」——恰恰是「终端在等一个计划确认」的常态（模型停下
  // 来等用户），必须挂载。
  //
  // **不动 `isSummary` 本身**（它还管总结模式的消息折叠，既有行为一概保持）——
  // 本处只在挂载门这一处换判据。
  //
  // **窗口差异（复评 M1）**：前端吃的是详情页**已拉取**的整页消息（200/1000 条），
  // 后端 `approve_options_scan` 读的是 40 条尾部窗口（性能面——审批端点不该拉整页）。
  // 判据同源同形，仅窗口不同。**两种窗口差异后果各异，如实申报**：
  // - 前端窗口**更长**时可能多挂一次卡（后端看不到那条计划 → 回 `available=false`）
  //   → 卡片按自隐契约立刻消失，代价是**一次多余 GET**，不是错误界面；
  // - 前端窗口**更短**时（用户点过「加载更早」会到 1000 条，反之首屏 200 > 40）
  //   实际不可能短于后端——前端首屏就是 200 条 > 40。
  // 即差异只会造成「多一次 GET」，不会造成「卡片挂着但点了报错」（后者由 POST 门
  // 与 GET 同口径保证）。
  const planPending = useMemo(
    () => session.status !== "finished" && isPlanPending(messages, session.agentType),
    [session.status, messages, session.agentType]
  );
  // ApproveCard 挂载门：红灯 ∨ 计划预期态（两个**并列**的门，不互相削弱——
  // waiting 门仍是审批红灯的入口，T1 裁决未动；预期态门是 codex/kimi 计划确认的
  // 唯一入口，两者都经同一张卡的 `available` 数据门做最终裁决）
  const approveMounted = session.status === "waiting" || planPending;

  // 消息流拉取：挂载 / limit 变化 / 手动刷新时重拉（整页替换；M3 不做增量追加
  // 与滚动位置保持——「加载更多」按钮替代无限滚动的裁决即含此简化）
  useEffect(() => {
    let alive = true;
    setLoading(true);
    setError(null);
    fetchSessionMessages(session.agentType, session.id, limit)
      .then((pg) => {
        if (!alive) return;
        setMessages(pg.messages);
        setTruncated(pg.truncated);
      })
      .catch((e: unknown) => {
        if (alive) setError({ status: e instanceof ApiError ? e.status : null });
      })
      .finally(() => {
        if (alive) setLoading(false);
      });
    return () => {
      alive = false;
    };
  }, [session.agentType, session.id, limit, refreshTick]);

  // F6：页面可见期间每 10s 静默轮询刷新会话内容。复用 refreshTick 信号走上方既有
  // 重拉链路（拉取 / alive 清理 / 错误态单点），手动刷新与看板 SSE 均零改动。
  // 节奏形态：interval 挂载即装但首拍在 +10s（不立即触发）——与挂载首拉天然错开，
  // 无双触发；hidden 时暂停（清 interval），visibilitychange 恢复可见先立即补刷一次
  // （追回隐藏期间错过的更新）再重启 10s 节奏。卸载清理 interval + 监听，无泄漏。
  useEffect(() => {
    const tick = () => {
      // P2-B：刷新前对消息滚动容器采样贴底状态——贴底（距底 <120px）该次刷新
      // 数据落地后跟随落底；上翻阅读时刷新不改变滚动位置（ref 为 null 时按
      // 贴底处理，保留旧版无条件落底行为）
      const el = messageAreaRef.current;
      pollFollowRef.current = el === null || isNearBottom(el);
      setRefreshTick((t) => t + 1);
    };
    let timer: ReturnType<typeof setInterval> | null = null;
    const stop = () => {
      if (timer !== null) {
        clearInterval(timer);
        timer = null;
      }
    };
    const onVisibility = () => {
      if (document.visibilityState === "hidden") {
        stop(); // 隐藏：暂停轮询
      } else {
        tick(); // 恢复可见：立即补刷一次再续节奏
        if (timer === null) timer = setInterval(tick, DETAIL_REFRESH_MS);
      }
    };
    if (document.visibilityState !== "hidden") {
      timer = setInterval(tick, DETAIL_REFRESH_MS);
    }
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      stop();
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [session.agentType, session.id]);

  // 文件表拉取（M3+）：挂载 / 切档时重拉。挂载那次（scope=200）即面板首次打开
  // 复用的数据（一次拉取两用，用户裁决 3 的附带口径）；失败已由 api 层静默降级
  useEffect(() => {
    let alive = true;
    setFileLoading(true);
    fetchSessionFiles(session.agentType, session.id, fileScope)
      .then((pg) => {
        if (!alive) return;
        setFileEntries(pg.files);
        setFileTruncated(pg.truncated);
      })
      .finally(() => {
        if (alive) setFileLoading(false);
      });
    return () => {
      alive = false;
    };
  }, [session.agentType, session.id, fileScope]);

  // 子 agent 全量名单拉取（观察台 §二，state 见上方 subagentList）：挂载 +
  // subagentTick 变化时重拉。subagentTick = refreshTick（10s 轮询 + 手动刷新
  // 免费继承 hidden 暂停/恢复补刷），但 **finished 冻结为 0**（评审 P2-1：清单要
  // 历史回溯 §二.6，而 finished 后磁盘数据不再变化——不再随 refreshTick 轮询；
  // processing→finished 翻转时 tick N→0 恰好触发最后一次重拉刷新名单终态，之后
  // 停拍；chip 的 finished 挂载门仍在 cardDock）
  useEffect(() => {
    let alive = true;
    fetchSessionSubagents(session.agentType, session.id)
      .then((v) => {
        if (alive) setSubagentList(v);
      })
      .catch(() => {
        /* 静默：保留上一份；首拉失败维持 null */
      });
    return () => {
      alive = false;
    };
  }, [session.agentType, session.id, subagentTick]);

  // 链接化 Set（派生自面板条目，单一数据源）：正文路径匹配用
  const files = useMemo(() => new Set(fileEntries.map((e) => e.path)), [fileEntries]);

  // 总结模式下的「最后 assistant 总结」：倒数第一条 assistant
  const lastAssistantSeq = useMemo(() => {
    if (!messages) return null;
    for (let i = messages.length - 1; i >= 0; i -= 1) {
      if (messages[i].kind === "assistant") return messages[i].seq;
    }
    return null;
  }, [messages]);

  const isCollapsed = useCallback(
    (m: SessionMessage): boolean => {
      const forced = expandedOverride.get(m.seq);
      if (forced !== undefined) return forced; // 手动展开/再折叠优先
      // 过程一键折叠模式位（2026-10-04）：开着时过程消息一律收起——含折叠后
      // 新到达的条目（wire 默认 tool-result 展开，缺这层「过程折叠」永远够不到
      // 它们，按钮在运行态是 no-op，见 collapseAll 注释）
      if (processAllCollapsed && isProcessKind(m.kind)) return true;
      // T1：plan 一等卡片恒展开——运行态与总结态都不折叠（豁免总结模式折叠）
      // T7：plan-file 计划文件卡同为入口卡，恒展开（与 plan 同款豁免）
      if (m.kind === "plan" || m.kind === "plan-file") return false;
      if (isSummary) {
        // 总结模式：user 直显；assistant 只显最后总结；过程消息全折叠
        if (m.kind === "user") return false;
        if (m.kind === "assistant") return m.seq !== lastAssistantSeq;
        return true;
      }
      // 运行中：保持 wire collapsed 字段语义（thinking / tool-call 恒折叠）
      return m.collapsed;
    },
    [expandedOverride, processAllCollapsed, isSummary, lastAssistantSeq]
  );

  const toggleCollapsed = useCallback(
    (m: SessionMessage) => {
      setExpandedOverride((prev) => {
        const next = new Map(prev);
        next.set(m.seq, !isCollapsed(m));
        return next;
      });
    },
    [isCollapsed]
  );

  /** 宽屏判定（matchMedia 防御式访问；jsdom / 隐私模式可能缺失，theme.ts 同款口径）。
   *  **仅作竖屏下「点文件默认布局」的回退口径**（2026-10-27 用户裁决 U5：竖屏手动点文件
   *  的默认布局不改）——方向判定本身一律走 viewportKind（纯宽高比），不看这个阈值 */
  const isWideViewport = useCallback((): boolean => {
    try {
      return window.matchMedia?.("(min-width: 768px)")?.matches ?? false;
    } catch {
      return false; // matchMedia 缺失/异常 → 窄屏语义（Task 8 原裁决）
    }
  }, []);

  /** 打开预览时的默认布局：横屏恒 split-h（U3 的延伸——横屏点文件不该掉回全屏浮层）；
   *  竖屏沿用既有 ≥768px 口径（U5 明确不改），与旋转默认互不干扰 */
  const defaultPreviewMode = useCallback(
    (): PreviewMode =>
      viewportKind === "landscape" || isWideViewport() ? "split-h" : "fullscreen",
    [viewportKind, isWideViewport]
  );

  // 点文件链接（消息正文）→ 预览。Bug 2 顺手项（spec P9「按屏幕宽度自适应」）：
  // ≥768px 分屏（2026-09-20 裁决 + T2 决策 2 收口：宽屏默认 split-h = 左对话右文件）、
  // <768px 全屏；手动切换随时覆盖该默认值。2026-10-27 起横屏恒 split-h（见 defaultPreviewMode）。
  // 正文进入 → files sheet 选中该文件，backToList=false（无返回列表按钮，既有行为不变）
  const openFile = useCallback(
    (path: string) => {
      setPreview((p) => ({
        sheet: "files",
        open: { kind: "file", path },
        mode: defaultPreviewMode(),
        backToList: false,
        // 每次显式打开都推进刷新键（同一路径再点一次也重读当前内容）
        openSeq: (p?.openSeq ?? 0) + 1,
      }));
    },
    [defaultPreviewMode]
  );

  // 从文件看板进入单文件预览（M3+）：backToList=true → 预览页头显示返回按钮；
  // 布局沿用当前 mode（往返保持，用户裁决「布局保持」）
  const openFileFromList = useCallback((path: string) => {
    setPreview((p) => ({
      sheet: "files",
      open: { kind: "file", path },
      mode: p?.mode ?? "fullscreen",
      backToList: true,
      openSeq: (p?.openSeq ?? 0) + 1,
    }));
  }, []);

  // chip 直达子 agent 详情（观察台 §二.4）：切到 subagents sheet 并选中该 agent。
  // 2026-10-09 sheet 两层级导航裁决：子 Agent sheet 不做预览区内分屏，清单与详情
  // 是两级导航——chip 直达落在详情级，backToList=true（页头返回钮回清单，与清单
  // 点卡同款，避免死端）；mode 仍按宽窄屏赋值（文件 sheet 消费；子 Agent sheet
  // 忽略 mode 恒单栏）
  const openSubagent = useCallback(
    (id: string) => {
      setPreview((p) => ({
        sheet: "subagents",
        open: { kind: "subagent", id },
        mode: defaultPreviewMode(),
        backToList: true,
        openSeq: p?.openSeq ?? 0,
      }));
    },
    [defaultPreviewMode]
  );

  // 清单看板点卡进入（观察台 §二.5）：沿用当前 mode（往返保持，openFileFromList 同款）
  const openSubagentFromList = useCallback((id: string) => {
    setPreview((p) => ({
      sheet: "subagents",
      open: { kind: "subagent", id },
      mode: p?.mode ?? "fullscreen",
      backToList: true,
      openSeq: p?.openSeq ?? 0,
    }));
  }, []);

  // 顶栏 sheet 钮切换（T1 sheet 化）：两看板平级互切；跨 sheet 的选中态不携带
  // （file 选中在 subagents sheet 无从显示，反之亦然）→ 一并收回看板态
  const switchSheet = useCallback((sheet: "files" | "subagents") => {
    setPreview((p) =>
      p === null || p.sheet === sheet
        ? p
        : { ...p, sheet, open: { kind: "none" }, backToList: false }
    );
  }, []);

  // 页头面板入口（M3+）：默认布局见 defaultPreviewMode（横屏恒 split-h；竖屏沿用
  // ≥768px 口径）。2026-10-27 起横屏进详情时面板本就开着，此钮即「收起右栏」。
  // 面板开启态 = files sheet 看板态（页头按钮高亮依据；文件预览/子 agent 详情
  // 不算——那是从看板或正文进入的下一层）；再点收回（ZCode 式排版，2026-09-16 用户裁决）
  const panelOpen = preview !== null && preview.sheet === "files" && preview.open.kind === "none";
  const togglePanel = useCallback(() => {
    setPreview((p) =>
      p !== null && p.sheet === "files" && p.open.kind === "none"
        ? null
        : {
            sheet: "files",
            open: { kind: "none" },
            mode: defaultPreviewMode(),
            backToList: false,
            openSeq: p?.openSeq ?? 0,
          }
    );
  }, [defaultPreviewMode]);

  const closePreview = useCallback(() => setPreview(null), []);

  /** 文件池入口（Task 6：fileChange chip 超限汇总行）：已开着文件面板则保持，
   *  否则切到文件面板（沿用当前布局口径；文件选中态收回一级看板）。 */
  const showFilePanel = useCallback(() => {
    setPreview((p) =>
      p !== null && p.sheet === "files" && p.open.kind === "none"
        ? p
        : {
            sheet: "files",
            open: { kind: "none" },
            mode: defaultPreviewMode(),
            backToList: false,
            openSeq: p?.openSeq ?? 0,
          }
    );
  }, [defaultPreviewMode]);

  // 从二级内容（文件预览 / 子 agent 详情）返回所在 sheet 的看板态（保持当前布局 mode）
  const backToBoard = useCallback(() => {
    setPreview((p) => (p ? { ...p, open: { kind: "none" }, backToList: false } : p));
  }, []);

  // 布局切换（顶栏 sheet bar 唯一实例，S5 裁决——预览组件内部的切换器已删）
  const changePreviewMode = useCallback((mode: PreviewMode) => {
    setPreview((p) => (p ? { ...p, mode } : p));
  }, []);

  // ===== Task 7：定位动作接线（窄屏先切回消息区）与命中高亮 =====

  /** 摘除命中高亮属性并清掉两秒计时器（结束/换会话/新动作/卸载共用）。 */
  const clearJumpHighlight = useCallback(() => {
    if (jumpHighlightTimerRef.current !== null) {
      clearTimeout(jumpHighlightTimerRef.current);
      jumpHighlightTimerRef.current = null;
    }
    for (const el of jumpHighlightNodesRef.current) {
      el.removeAttribute("data-jump-hit");
      el.removeAttribute("data-jump-node");
    }
    jumpHighlightNodesRef.current = [];
  }, []);

  /** 发起一次定位（新点击 = 新动作）：窄屏（全屏浮层）先切回消息区并保留返回快照，
   *  宽屏分屏下预览保持原位。查找本身由 useArtifactJump 执行（当前页 + 最多四页）。 */
  const startArtifactJump = useCallback(
    (target: ArtifactJumpTarget) => {
      const current = preview;
      if (current?.mode === "fullscreen") {
        setJumpReturnPreview(current);
        setPreview(null);
      } else {
        setJumpReturnPreview(null);
      }
      clearJumpHighlight();
      void artifactJump.jump(target);
    },
    [preview, artifactJump, clearJumpHighlight]
  );

  /** 文件池来源按钮：按条目身份（fileKey 优先，未知来源退化 path）发起查找。 */
  const locateFromPanel = useCallback(
    (entry: FilePanelEntry) => {
      startArtifactJump({ fileKey: entry.fileKey ?? null, path: entry.path });
    },
    [startArtifactJump]
  );

  /** 预览页头定位：用当前打开路径解析身份；找不到条目时 fileKey 为 null（不预造
   *  messageId，也不猜别名），仍走同一有界查找。 */
  const locateFileInMessages = useCallback(
    (path: string) => {
      const matched = fileView.find((entry) => entry.path === path);
      // 从附件卡直达预览时，打开的可能是附件的展示原文（与后端展示路径不同形态）：
      // 只用**同一 path** 的结构化附件身份键补足，不猜别名。
      const fromArtifact =
        fileReferenceRange
          .flatMap((message) => message.artifacts ?? [])
          .find((artifact) => artifact.path === path)?.fileKey ?? null;
      startArtifactJump({ fileKey: matched?.fileKey ?? fromArtifact, path });
    },
    [fileView, fileReferenceRange, startArtifactJump]
  );

  /** 返回定位前被关闭的全屏预览（失败/结束后仍能回到原文件预览）。 */
  const returnToJumpPreview = useCallback(() => {
    if (jumpReturnPreview === null) return;
    setPreview(jumpReturnPreview);
    setJumpReturnPreview(null);
  }, [jumpReturnPreview]);

  /** 关闭定位提示（结束本次动作；高亮与返回快照一并清理）。 */
  const dismissJumpBanner = useCallback(() => {
    clearJumpHighlight();
    setJumpReturnPreview(null);
    artifactJump.reset();
  }, [artifactJump, clearJumpHighlight]);

  // 命中：先等目标 DOM（更早页合并是异步渲染），再滚动 + 高亮约两秒。
  // 新动作（phase 离开 hit）与卸载都会清掉上一次高亮，不留下悬挂计时器。
  useEffect(() => {
    const { phase, hit } = artifactJump.state;
    if (phase !== "hit" || hit === null) {
      clearJumpHighlight();
      return;
    }
    let cancelled = false;
    let raf: number | null = null;
    const deadline = Date.now() + ARTIFACT_JUMP_DOM_WAIT_MS;
    const attempt = () => {
      if (cancelled) return;
      const area = messageAreaRef.current;
      let messageEl: HTMLElement | null = null;
      if (area !== null) {
        for (const el of area.querySelectorAll<HTMLElement>("[data-message-id]")) {
          if (el.getAttribute("data-message-id") === hit.messageId) {
            messageEl = el;
            break;
          }
        }
      }
      if (messageEl !== null) {
        const node = jumpHitNode(messageEl, hit);
        messageEl.setAttribute("data-jump-hit", "true");
        const marked = [messageEl];
        if (node !== null && node !== messageEl) {
          node.setAttribute("data-jump-node", "true");
          marked.push(node);
        }
        jumpHighlightNodesRef.current = marked;
        const scrollTarget = node ?? messageEl;
        if (typeof scrollTarget.scrollIntoView === "function") {
          scrollTarget.scrollIntoView({ block: "center" });
        }
        jumpHighlightTimerRef.current = setTimeout(() => {
          jumpHighlightTimerRef.current = null;
          clearJumpHighlight();
        }, ARTIFACT_JUMP_HIGHLIGHT_MS);
        return;
      }
      if (Date.now() >= deadline) return; // 目标未出现：不滚错位置（结果条仍如实显示）
      raf = requestAnimationFrame(attempt);
    };
    attempt();
    return () => {
      cancelled = true;
      if (raf !== null) cancelAnimationFrame(raf);
    };
  }, [artifactJump.state, clearJumpHighlight]);

  // 卸载清理高亮计时器；换会话清理高亮与返回快照
  useEffect(() => () => clearJumpHighlight(), [clearJumpHighlight]);
  useEffect(() => {
    clearJumpHighlight();
    setJumpReturnPreview(null);
  }, [session.id, clearJumpHighlight]);

  // 方向 / 分辨率变化（2026-10-27 用户裁决 U4：变化时检查一次，回到该方向默认；
  // 2026-10-28 U7 追加例外：**全屏阅读文件时一律维持全屏**）。
  // **只依赖 viewportKind**——若把 preview 读进 effect 或写进 deps，每次布局变动都会
  // 重跑：横屏手切全屏被立刻拽回 split-h、竖屏手动打开预览被立刻关掉，切换器与页头
  // 📁 钮全哑。故用函数式 updater 读旧值、用 prevKindRef 判首次（不用布尔门闩：
  // StrictMode 双挂载会多跑一次重置）。
  // 两向都清返回快照：竖屏「定位」留下的 fullscreen 快照若跨旋转存活，横屏点「返回
  // 文件预览」会在横屏弹出竖屏时代的全屏浮层。清快照必须写在 updater 之外（updater
  // 须是纯函数，StrictMode 下可能执行两次）。
  const prevKindRef = useRef(viewportKind);
  useEffect(() => {
    if (prevKindRef.current === viewportKind) return;
    prevKindRef.current = viewportKind;
    setJumpReturnPreview(null);
    setPreview((p) => {
      // 例外（U7）：当前处于全屏阅读态时原样维持。判据是**此刻的状态**（mode），
      // 不是「用户是否手动切过全屏」——竖屏窄屏点文件默认就是全屏，此刻同样算
      // 「正在全屏阅读」，旋转一并豁免。左右分屏 / 上下分屏则照常按方向重排。
      if (p?.mode === "fullscreen") return p;
      if (viewportKind === "landscape") {
        // 竖→横：聊天框状态下打开右栏看板；已有预览则保留内容、只改布局
        return p === null ? defaultPreviewFor("landscape") : { ...p, mode: "split-h" };
      }
      return null; // 横→竖：回聊天框（U4 直译：不保留预览）
    });
  }, [viewportKind]);

  // 消息渲染器（观察台 T6 起迁至 ./message-render 共用）：renderBody 消费本页
  // 既有 openFile（正文路径点击 → 预览）与 files（链接化路径集）
  const { renderBody, renderMarkdown, renderLinkifiedText } = useMessageRenderers({
    openFile,
    files,
  });

  const retry = useCallback(() => {
    setError(null);
    // P2-B：手动刷新（页头刷新按钮 / 错误重试）无条件落底，保留既有行为
    pollFollowRef.current = true;
    setRefreshTick((t) => t + 1);
  }, []);

  // ---- R5 一键 resume（在电脑上打开，Task 11）----
  // 2026-09-20 归档区裁决 2（spec 2026-09-20-mobile-archive-history，验收 E-6）：
  // 活会话详情页不再提供「在电脑上打开」——resume 能力归 ArchiveDetail（历史页
  // 「在桌面端打开」），可用性门与错误文案在 ./resume-gate.ts 共享。

  // ---- 书签（M3+，2026-09-16 用户裁决）----
  // 恢复：拿到 兔维斯 进程 bootId 后从 localStorage 种回内存单例（刷新页面/
  // 卸载重挂均走此路径）；bootId 不一致（兔维斯 已重启）由 restore 内部清空
  useEffect(() => {
    let alive = true;
    void ensureBootId().then((boot) => {
      if (!alive || boot === null) return;
      restoreBookmarks(boot);
      setBookmarks(listBookmarks(session.id));
    });
    return () => {
      alive = false;
    };
  }, [session.id]);

  // 取锚：当前视口顶部可见的那条消息。消息 li 挂 data-seq，取第一个
  // 「底边越过容器顶边」的条目即视口首条
  const topVisibleMessage = useCallback((): SessionMessage | null => {
    const el = messageAreaRef.current;
    if (!el || messages === null) return null;
    const areaTop = el.getBoundingClientRect().top;
    const items = el.querySelectorAll<HTMLElement>("[data-seq]");
    for (const it of items) {
      if (it.getBoundingClientRect().bottom > areaTop) {
        const seq = Number(it.dataset.seq);
        return messages.find((m) => m.seq === seq) ?? null;
      }
    }
    return null;
  }, [messages]);

  const handleAddBookmark = useCallback(
    (color: string) => {
      const m = topVisibleMessage();
      if (m === null) return;
      addBookmark(session.id, {
        color,
        seq: m.seq,
        anchor: messageAnchor(m),
        preview: bookmarkPreview(m.content),
      });
      setBookmarks(listBookmarks(session.id));
      setBookmarkJumpMiss(false);
    },
    [session.id, topVisibleMessage]
  );

  const handleRemoveBookmark = useCallback(
    (color: string) => {
      removeBookmark(session.id, color);
      setBookmarks(listBookmarks(session.id));
    },
    [session.id]
  );

  const handleClearBookmarks = useCallback(() => {
    clearBookmarks(session.id);
    setBookmarks([]);
  }, [session.id]);

  // 跳转：按指纹在当前窗口查回消息 → seq → scrollIntoView（block:start 落在视口顶部）。
  // 查不到（书签指向更早的未加载消息）→ **自动逐级「加载更早」**直到命中或到顶
  // （M5 P3-c：刷新后窗口只剩尾部 200 条，书签目标在更早分页——旧实现只提示手动
  // 加载，跳转等于失效）；到顶仍未命中 → miss 横幅
  const pendingJumpRef = useRef<{ anchor: string; nextLimit: number } | null>(null);
  const [jumpLoading, setJumpLoading] = useState(false);

  const scrollAnchorIntoView = useCallback(
    (anchor: string): boolean => {
      if (messages === null) return false;
      const target = messages.find((m) => messageAnchor(m) === anchor);
      if (!target) return false;
      messageAreaRef.current
        ?.querySelector<HTMLElement>(`[data-seq="${target.seq}"]`)
        ?.scrollIntoView({ block: "start" });
      return true;
    },
    [messages]
  );

  const handleJumpBookmark = useCallback(
    (anchor: string) => {
      setBookmarkJumpMiss(false);
      if (scrollAnchorIntoView(anchor)) {
        return;
      }
      // 目标不在当前窗口：能扩则登记自动扩窗（逐级 200，至多 1000），否则 miss。
      // 扩窗经由 pendingJumpRef + setLimit——不直接依赖 limit，避免 setLimit 渲染
      // （messages 未变）触发的中间态误判成「仍找不到」而连锁扩到顶
      if (limit < MAX_LIMIT) {
        pendingJumpRef.current = {
          anchor,
          nextLimit: Math.min(limit + PAGE_LIMIT, MAX_LIMIT),
        };
        setJumpLoading(true);
        setLimit(Math.min(limit + PAGE_LIMIT, MAX_LIMIT));
      } else {
        setBookmarkJumpMiss(true);
      }
    },
    // setLimit 是 useState 稳定 setter（恒等），列入 deps 只为如实申报引用，
    // 不改变本回调随 limit/scrollAnchorIntoView 变化的重建时机。
    [limit, scrollAnchorIntoView, setLimit]
  );

  // 自动加载跳转的续查：**仅随 messages 变化重查**（deps 不含 limit）——
  // setLimit 引起的中间渲染不会误触发；命中滚动收尾；未命中且还能扩继续扩；
  // 到顶仍未命中 → miss。声明在滚动对齐 effect 之后：跳转滚动覆盖「落底」对齐
  useEffect(() => {
    if (messages === null) return;
    const pj = pendingJumpRef.current;
    if (pj === null) return;
    if (scrollAnchorIntoView(pj.anchor)) {
      pendingJumpRef.current = null;
      setJumpLoading(false);
      return;
    }
    if (pj.nextLimit < MAX_LIMIT) {
      const next = Math.min(pj.nextLimit + PAGE_LIMIT, MAX_LIMIT);
      pendingJumpRef.current = { anchor: pj.anchor, nextLimit: next };
      setLimit(next);
      return;
    }
    pendingJumpRef.current = null;
    setJumpLoading(false);
    setBookmarkJumpMiss(true);
  }, [messages, scrollAnchorIntoView]);

  // 进入详情默认滚到最底部（最新消息在下方，用户裁决 2026-09-16）；且
  // 「加载更早消息」重拉后**保持原阅读位置**——记录重拉前的滚动高度差，
  // 新内容（更早消息）插在顶部后把差值补回去，视线不跳。
  // P2-B：轮询刷新（F6）数据落地时，仅在刷新前采样为贴底（距底 <120px）才
  // 跟随落底——上翻阅读历史时不被每 10s 拽回底部；首次加载与手动刷新
  // （pollFollowRef 初值 true / retry 置 true）仍无条件落底。
  // 依赖 messages（而非 limit）：只在数据真正落地后执行一次对齐
  useEffect(() => {
    const el = messageAreaRef.current;
    if (!el || messages === null) return;
    const pending = pendingScrollRef.current;
    if (pending !== null) {
      // 回补：新高度 - 旧高度 = 顶部插入量，往下偏移同等距离保持视线
      const inserted = el.scrollHeight - pending.prevHeight;
      el.scrollTop = pending.prevTop + Math.max(0, inserted);
      pendingScrollRef.current = null;
      return;
    }
    if (!pollFollowRef.current) return; // P2-B：上翻阅读中的轮询刷新不动滚动位置
    el.scrollTop = el.scrollHeight; // 首次（或手动刷新后）落到底部
  }, [messages]);

  // 是否渲染折叠切换头：过程消息 + 总结模式下的「更早 assistant」；
  // 最终 assistant 总结直显正文（不给「更早的回复」头）。
  // T1：plan 不在列 → 无折叠头（常驻计划卡片，不可折——与 isCollapsed 恒展开配套；
  // 因此 toggleableMessages/总结横幅折叠计数天然不含 plan）
  const isToggleable = (m: SessionMessage) =>
    isProcessKind(m.kind) || (isSummary && m.kind === "assistant" && m.seq !== lastAssistantSeq);

  // Bug 8（M3 验收）：总结模式折叠提示。折叠数 = 当前被折叠的可折叠条数——
  // 70 条过程消息被静默折叠会被误读为「内容被截」，顶部提示行 + 展开/收起全部
  // 消除歧义（折叠本身是正确行为，不改动折叠语义）。
  // useMemo 保持引用稳定（expandAll 依赖它，逐渲染新建会让 useCallback 每拍失效）
  const toggleableMessages = useMemo(
    () => (messages !== null ? messages.filter(isToggleable) : []),
    // eslint-disable-next-line react-hooks/exhaustive-deps -- isToggleable 是渲染期纯函数（依赖 isSummary/lastAssistantSeq，已在下行列出）
    [messages, isSummary, lastAssistantSeq]
  );
  const collapsedCount = toggleableMessages.filter((m) => isCollapsed(m)).length;

  const expandAll = useCallback(() => {
    // 模式位复位 + 覆盖表强制展开（false = 强制展开）：只复位模式位不够——
    // wire 默认 thinking/tool-call 是折叠的，复位后它们会缩回去
    setProcessAllCollapsed(false);
    setExpandedOverride((prev) => {
      const next = new Map(prev);
      // 覆盖表值语义 = 强制折叠与否（false = 强制展开，见 isCollapsed）
      for (const m of toggleableMessages) next.set(m.seq, false);
      return next;
    });
  }, [toggleableMessages]);

  const collapseAll = useCallback(() => {
    // 修复（2026-10-04）：旧实现只清空覆盖表 = 回落 wire 默认，而运行态默认
    // tool-result 展开 → 未手动折叠过任何条时点击是 no-op，且「全折叠」在运行态
    // 不可达。改为**置模式位**（isCollapsed 消费）：覆盖既有条目与后续新条目
    // （sticky）；同时清覆盖表，甩掉历史单条展开的残留。
    setProcessAllCollapsed(true);
    setExpandedOverride(new Map());
  }, []);

  // Bug 1（M3 验收）：条数到顶（length >= limit）**或**后端报告头部截断（truncated，
  // 胖单行吃满字节窗的会话条数恒小于 limit）任一成立即提供「加载更早消息」
  const hasLoadMore =
    messages !== null && !error && (messages.length >= limit || truncated) && limit < MAX_LIMIT;

  // 锚 → 书签查找表（消息角标用；渲染期 O(1) 直读，避免每条消息 find）
  const bookmarkByAnchor = useMemo(() => new Map(bookmarks.map((b) => [b.anchor, b])), [bookmarks]);

  // 「跳到最新」：距底超阈值时显示（onScroll 驱动）；点击瞬时落底并立即恢复
  // 轮询跟随（P2-B 采样语义不变——跳底本就是「我要贴底」的明确意图）。
  // 「跳到顶部」（2026-09-20 归档对齐批）：距顶超阈值时显示，点击落 0——
  // 上滑离开顶部后一键回顶，不触碰轮询跟随语义（落顶即视为上翻阅读中）
  const handleAreaScroll = useCallback(() => {
    const el = messageAreaRef.current;
    if (!el) return;
    setShowJump(el.scrollHeight - el.scrollTop - el.clientHeight > JUMP_SHOW_THRESHOLD_PX);
    setShowJumpTop(el.scrollTop > JUMP_SHOW_THRESHOLD_PX);
  }, []);

  const jumpToLatest = useCallback(() => {
    const el = messageAreaRef.current;
    if (!el) return;
    pollFollowRef.current = true;
    el.scrollTop = el.scrollHeight;
    setShowJump(false);
  }, []);

  const jumpToTop = useCallback(() => {
    const el = messageAreaRef.current;
    if (!el) return;
    el.scrollTop = 0;
    setShowJumpTop(false);
  }, []);

  // 书签条（两个布局分支共用同一份 JSX）。processToggle：过程一键折叠开关
  // （2026-09-20）——运行态与总结态都可用（与 summary-banner 的差别就在不看 isSummary）；
  // 无可折叠过程消息时不渲染。allCollapsed = 当前全部折叠 → 按钮动作变为全部展开
  // （2026-10-04 起运行态真正可达：collapseAll 置模式位，见其注释）
  const bookmarkBar = (
    <BookmarkBar
      bookmarks={bookmarks}
      atLimit={bookmarks.length >= BOOKMARK_LIMIT}
      onAdd={handleAddBookmark}
      onJump={handleJumpBookmark}
      onRemove={handleRemoveBookmark}
      onClear={handleClearBookmarks}
      processToggle={
        toggleableMessages.length > 0
          ? {
              allCollapsed: collapsedCount === toggleableMessages.length,
              onToggle: collapsedCount === toggleableMessages.length ? expandAll : collapseAll,
            }
          : undefined
      }
    />
  );

  // ===== Task 7：定位状态条 =====
  // 搜索中/命中/范围内未命中/真正到头/失败各自文案；失败提供「重试」（新动作），
  // 被窄屏关闭的预览提供「返回文件预览」；**未命中不提供任何“继续加载”入口**。
  const jumpBanner =
    artifactJump.state.phase === "idle" ? null : (
      <div
        data-testid="artifact-jump-banner"
        data-phase={artifactJump.state.phase}
        data-failure-kind={artifactJump.state.failure?.kind ?? undefined}
        className={`mb-2 flex shrink-0 items-start gap-2 rounded-lg px-3 py-2 text-xs ${
          artifactJump.state.phase === "failed"
            ? "bg-rose-500/10 text-rose-700 dark:text-rose-400"
            : "bg-[var(--btnp)]/10 text-[var(--tx)]"
        }`}
      >
        <div className="min-w-0 flex-1">
          <p data-testid="artifact-jump-text">{artifactJumpText(artifactJump.state)}</p>
          {artifactJump.state.phase === "miss" && (
            <p data-testid="artifact-jump-hint" className="mt-0.5 text-[10px] opacity-80">
              {i18n.t("mobile.artifacts.locate.missHint")}
            </p>
          )}
        </div>
        {artifactJump.state.phase === "failed" && (
          <button
            type="button"
            data-testid="artifact-jump-retry"
            onClick={() => void artifactJump.retry()}
            className="shrink-0 rounded-full bg-[var(--cb)] px-2 py-0.5 text-xs text-[var(--tx)]"
          >
            {i18n.t("mobile.artifacts.locate.retry")}
          </button>
        )}
        {jumpReturnPreview !== null && artifactJump.state.phase !== "searching" && (
          <button
            type="button"
            data-testid="artifact-jump-return-preview"
            onClick={returnToJumpPreview}
            className="shrink-0 rounded-full bg-[var(--cb)] px-2 py-0.5 text-xs text-[var(--tx)]"
          >
            {jumpReturnPreview.open.kind === "file"
              ? i18n.t("mobile.artifacts.locate.returnPreview")
              : i18n.t("mobile.artifacts.locate.returnPanel")}
          </button>
        )}
        <button
          type="button"
          data-testid="artifact-jump-dismiss"
          aria-label={i18n.t("mobile.artifacts.locate.dismiss")}
          onClick={dismissJumpBanner}
          className="shrink-0 rounded-full p-0.5 text-[var(--mut)] hover:bg-[var(--cb)] dark:hover:bg-[var(--btnp)]"
        >
          <X size={12} />
        </button>
      </div>
    );

  // ===== 卡片停靠区（2026-10-04 用户裁决「问答弹窗出现在页面下半部」）=====
  // 审批/问答/模式三卡从「消息区上方」移到「消息区下方、composer 之上」——
  // InteractiveCard 的设计注释本就写明「交互只发生在底部交互卡，消息流只读」，
  // 顶部挂载是历史偏差，本批归位。长卡 max-h 内滚不把消息区挤没；composer 的
  // presence 提示（「请用上方卡片按钮应答」）与卡的相对位置保持成立。
  //
  // 挂载语义原样保留（只挪位置不改门）：
  // - ApproveCard：waiting ∨ 计划预期态（`approveMounted`，丁T1/T2 双门）；刷新
  //   靠 App 的 selected 同步（SSE/轮询），status 翻转即挂/卸，不重进页面；
  // - QuestionCard：**非结束态**挂载（丁T1 放宽），可用性由卡内 available 自隐兜底；
  // - ModeBar：常驻信息，不依赖 waiting 态。
  //
  // 组件钥匙防线（T1 活状态流）：三卡与 composer 同层且都按会话强制重挂
  // （M9R P3），key 必须互异（approve-/question-/mode-/composer- 前缀）——同 key
  // 兄弟在红卡「停留期间插入/卸载」时会让 React 同 key 复用错乱（duplicate key
  // 警告 + 红卡卸不掉）。
  const cardDock = (
    <div
      data-testid="card-dock"
      className="flex max-h-[35dvh] shrink-0 flex-col gap-2 overflow-y-auto"
    >
      {/* T09/T10：Desktop 会话的**原生卡优先**——投影里已有原生 pending（命令/文件/
          权限/问答）时不再挂旧 TUI 屏读卡，避免同一事件两张卡（一张永远读不到
          终端选项）。终端会话（codexState === null）保持原 ApproveCard 行为。 */}
      {approveMounted &&
        !appForm &&
        !(
          codexState !== null &&
          codexProjectionActive &&
          (codexState.pending.length > 0 || (codexState.asyncQuestions?.length ?? 0) > 0)
        ) &&
        (planFeedbackActive ? null : (
          <ApproveCard
            key={`approve-${session.id}`}
            session={session}
            onPlanFeedbackReady={() => setPlanFeedbackActive(true)}
          />
        ))}
      {planFeedbackActive && (
        <PlanFeedbackBar
          key={`plan-fb-${session.id}`}
          sessionId={session.id}
          onDismiss={() => setPlanFeedbackActive(false)}
        />
      )}
      {/* 2026-10-20 复审 P2：Codex 控件**不跟随**旧 `isSummary`（idle/finished 时
          QuestionCard 的总结态隐藏条件）；能力、pending、屏障与队列按投影真实状态
          展示。QuestionCard 仍保持原总结态门（终端卡行为不变）。 */}
      {codexState !== null ? (
        <>
          {/* R16：App 会话不再挂"总能力徽标"（逐操作工具栏已承担解释职责）；
              CLI/兼容会话仍保留横幅（它承载 stale/探测骨架语义）。 */}
          {!appForm ? (
            <CodexAvailabilityBanner
              key={`codex-banner-${session.id}`}
              availability={codexState.availability}
              stale={codexState.stale}
              probeStatus={codexState.probeStatus}
              sendAttemptable={codexSendGate}
            />
          ) : (
            // R16：App 会话去掉"总能力徽标"，但**stale（断链/换代）必须如实显示**。
            codexState.stale && (
              <p
                data-testid="codex-stale-note"
                className="text-xs text-amber-700 dark:text-amber-400"
              >
                {i18n.t("mobile.codex.staleBanner")}
              </p>
            )
          )}
          {appForm && adaptHintVisible && (
            <div
              data-testid="codex-adapt-hint-inline"
              className="rounded-xl border border-[var(--bd)] bg-[var(--cd)] px-3 py-2 text-xs text-[var(--mut)]"
            >
              <p>{i18n.t("mobile.codex.adapt.hint")}</p>
              <button
                type="button"
                data-testid="codex-adapt-hint-dismiss"
                onClick={dismissAdaptHint}
                className="mt-1 underline"
              >
                {i18n.t("mobile.codex.adapt.hintOk")}
              </button>
            </div>
          )}
          <CodexOperationToolbar
            key={`codex-ops-${session.id}`}
            operations={codexDisplay.operations}
            reconnecting={codexDisplay.reconnecting}
            identityPending={codexDisplay.identityPending}
            onOpenDetails={() => setCodexAdaptOpen(true)}
          />
          {codexAdaptOpen && (
            <CodexAdaptSheet
              key={`codex-adapt-${session.id}`}
              sessionId={session.id}
              open={codexAdaptOpen}
              onClose={() => setCodexAdaptOpen(false)}
              // R07：能力表消费**页面同版**事实（含展示缓存补过的 support），
              // 不读独立 GET 的旧快照。
              effectiveOperations={codexDisplay.operations}
            />
          )}
          {/* 2026-10-28 用户裁决：只展示当前有效交互卡——当前可答组，或本机
              已发出过动作的待核对组；已答/无锚 unknown 不再有历史入口。 */}
          {codexProjectionActive && (
            <div data-testid="codex-dock-cards" className="contents">
              {inbox.pending.map((p) => (
                <PendingRequestCard
                  key={`codex-pending-${session.id}-${p.requestKey}`}
                  sessionId={session.id}
                  request={p}
                  writeLocked={codexState.stale}
                />
              ))}
              {/* T04（spec §3.2）：其余有效组收进**唯一**「另有 N 组」入口（不常驻 N 行）；
                  展开后可切任意组；已答组不计数（inbox 已过滤）。组卡片仍全部挂载
                  （hidden 控制），切组/收起/展开不卸载组件——草稿与在途状态不丢。 */}
              {otherGroups.length > 0 && (
                <div>
                  <button
                    type="button"
                    data-testid="codex-other-groups"
                    aria-expanded={otherGroupsOpen}
                    onClick={() => setOtherGroupsOpen((open) => !open)}
                    className="flex w-full items-center justify-between rounded-xl border border-[var(--bd)] bg-[var(--cd)] px-3 py-1.5 text-left text-xs text-[var(--mut)]"
                  >
                    <span>
                      {i18n.t("mobile.codex.async.otherGroupsCount", {
                        count: otherGroups.length,
                      })}
                    </span>
                    <span className="underline">
                      {i18n.t(
                        otherGroupsOpen
                          ? "mobile.codex.async.dismiss"
                          : "mobile.codex.async.historyExpand"
                      )}
                    </span>
                  </button>
                  {otherGroupsOpen &&
                    otherGroups.map((group) => (
                      <button
                        key={`codex-group-item-${session.id}-${group.key}`}
                        type="button"
                        data-testid="codex-group-item"
                        data-group-key={group.key}
                        onClick={() => {
                          setActiveGroupKey(group.key);
                          setOtherGroupsOpen(false);
                        }}
                        className="mt-1 flex w-full items-center justify-between rounded-xl border border-[var(--bd)] bg-[var(--cd)] px-3 py-1.5 text-left text-xs text-[var(--mut)]"
                      >
                        <span>
                          {group.live.length > 0
                            ? i18n.t("mobile.codex.async.otherGroup", {
                                count: group.live.length,
                              })
                            : i18n.t("mobile.codex.async.verifyGroup", {
                                count: group.members.length,
                              })}
                        </span>
                        <span className="underline">{i18n.t("mobile.codex.async.openGroup")}</span>
                      </button>
                    ))}
                </div>
              )}
              {inbox.groups.map((group) => (
                <div
                  key={`codex-group-panel-${session.id}-${group.key}`}
                  data-testid={`codex-group-panel-${group.key}`}
                  hidden={group.key !== activeGroup?.key}
                >
                  <AsyncQuestionGroupCard
                    sessionId={session.id}
                    questions={group.members}
                    fontScale={fontScale}
                    onChanged={() => setQueueTick((tick) => tick + 1)}
                  />
                </div>
              ))}
            </div>
          )}
          <CodexQueuePanel
            key={`codex-queue-${session.id}`}
            sessionId={session.id}
            refreshSignal={queueTick}
            onQueueStateChanged={bumpSendInfo}
            onEditDraft={enqueueEditDraft}
            turnControl={appForm ? codexTurnControl : undefined}
          />
          {!codexProjectionActive && !isSummary && !appForm && (
            <QuestionCard key={`question-${session.id}`} session={session} fontScale={fontScale} />
          )}
        </>
      ) : appForm ? (
        // App 会话尚无投影：如实显示"连接中"，**不回退终端屏读卡**。
        <p
          data-testid="codex-connecting"
          className="rounded-xl border border-[var(--bd)] bg-[var(--cd)] px-3 py-2 text-xs text-[var(--mut)]"
        >
          {i18n.t("mobile.codex.ops.connecting")}
        </p>
      ) : (
        !isSummary && (
          <QuestionCard key={`question-${session.id}`} session={session} fontScale={fontScale} />
        )
      )}
      {/* 2026-10-08 子 agent chip：与 ModeBar 同一行（flex-wrap 兄弟），生命周期
          解耦——mode 视图失败/unsupported 时 chip 仍活，反之亦然（spec §6）。
          finished 不挂载（idle 不拦：claude 后台 agent 可在主会话 idle 时仍在跑）；
          其余状态的空态裁决交给数据（list 为 null/无 running 项 → 不渲染）。
          观察台 T5：chip 改纯展示，名单由上方 subagentList 单一数据源下发
          （拉取不再自持），点击直达详情对话框（§二.4）。 */}
      <div data-testid="status-row" className="flex flex-wrap items-center gap-2">
        {!appForm && <ModeBar key={`mode-${session.id}`} session={session} />}
        {session.status !== "finished" && (
          <SubagentChips
            key={`subagents-${session.id}`}
            list={subagentList}
            onOpenDetail={openSubagent}
          />
        )}
      </div>
    </div>
  );

  // 消息区（split 布局复用同一份 JSX）
  const messageArea = (
    <>
      {codexProjectionActive && codexState !== null && (
        <div
          data-testid="codex-projection"
          data-availability={codexState.availability}
          data-stale={codexState.stale ? "true" : "false"}
        >
          {codexState.truncated && (
            <p
              data-testid="codex-projection-truncated"
              className="mb-2 rounded-lg bg-amber-500/10 px-3 py-2 text-xs text-amber-700 dark:text-amber-400"
            >
              {i18n.t("mobile.detail.projectionTruncated")}
            </p>
          )}
          {codexState.contentState !== "ready" &&
            (codexState.contentState !== "unsupported" || showUnsupported) && (
              <p
                data-testid="codex-projection-health"
                data-state={codexState.contentState}
                className="mb-2 rounded-lg bg-amber-500/10 px-3 py-2 text-xs text-amber-700 dark:text-amber-400"
              >
                {codexState.contentState === "loading"
                  ? i18n.t("mobile.detail.stateLoading")
                  : codexState.contentState === "partial"
                    ? i18n.t("mobile.detail.statePartial")
                    : i18n.t("mobile.detail.stateUnsupported")}
              </p>
            )}
          {codex.loadedMessages.length === 0 ? (
            codexState.contentState === "ready" ? (
              <p className="py-16 text-center text-sm text-[var(--mut)]">
                {i18n.t("mobile.detail.noMessages")}
              </p>
            ) : null
          ) : (
            // 消息正文走既有 renderMarkdown / renderLinkifiedText：文件池已知路径
            // 继续可点（不新增 basename 推断）；附件卡片复用同一个 openFile。
            // 列表以 loadedMessages 为准（Task 5：尾窗 + 已合并更早页，最新在最后），
            // 翻页合并后自然出现，不在这里另造无界「加载更多」循环（Task 7 拥有
            // 有界查找，且 Task 5 没有终点信号）。
            // 注意（Task 6 复审 I1）：split/split-h 与 fullscreen 是不同 React 子树，
            // 切换布局会卸载并重建本消息列——图片资源随之释放并在重建后重新读取，
            // FilePreview 也会重挂载再读一次。这是既有布局结构的行为，本任务按
            // 「沿用现状」保留；消除需布局级提升资源，已登记为后续项。
            <ul className="space-y-2 pb-4">
              {codex.loadedMessages.map((m) => {
                const ctx = artifactContexts.get(m.id);
                const hasArtifacts = (m.artifacts?.length ?? 0) > 0;
                // 服务端预算可能把附件全部省略：此时仍需渲染消息级提示（组件内判空）
                const hasOmittedArtifacts = (m.omittedArtifactCount ?? 0) > 0;
                const isUser = m.role === "user";
                return (
                  <li
                    key={m.id}
                    data-testid={`codex-msg-${m.id}`}
                    data-message-id={m.id}
                    data-role={m.role}
                    data-kind={m.kind}
                    className={`${
                      isUser ? "flex flex-col items-end" : "flex flex-col items-start"
                    } data-[jump-hit=true]:rounded-xl data-[jump-hit=true]:bg-amber-500/10 data-[jump-hit=true]:ring-2 data-[jump-hit=true]:ring-amber-500/70`}
                  >
                    {m.interactionAnswer ? (
                      <AnswerBubble answer={m.interactionAnswer} messageId={m.id} />
                    ) : null}
                    {m.text !== "" && (
                      <div
                        className={
                          isUser
                            ? "max-w-[85%] rounded-2xl bg-[var(--btnp)]/15 px-3 py-2 text-sm text-[var(--tx)]"
                            : "max-w-[85%] rounded-2xl bg-[var(--cb)]/60 px-3 py-2 text-sm text-[var(--tx)]"
                        }
                      >
                        {m.role === "user" || m.role === "assistant" ? (
                          <div className="md-body">{renderMarkdown(m.text)}</div>
                        ) : (
                          <div className="whitespace-pre-wrap">{renderLinkifiedText(m.text)}</div>
                        )}
                      </div>
                    )}
                    {(hasArtifacts || hasOmittedArtifacts) && ctx !== undefined && (
                      <div className="mt-1 w-full">
                        <ProjectedArtifacts
                          message={m}
                          context={ctx}
                          openFile={openFile}
                          images={artifactImages}
                          onOpenFilePanel={showFilePanel}
                        />
                      </div>
                    )}
                  </li>
                );
              })}
            </ul>
          )}
        </div>
      )}
      {/* 总结模式提示行（Bug 8）：仅总结模式且有可折叠过程消息时出现 */}
      {isSummary && toggleableMessages.length > 0 && (
        <div
          data-testid="summary-banner"
          className="mb-2 flex items-center gap-2 rounded-lg bg-[var(--btnp)]/10 px-3 py-2 text-xs text-[var(--tx)]"
        >
          <span>{i18n.t("mobile.detail.summaryBanner", { count: collapsedCount })}</span>
          <span className="ml-auto flex shrink-0 gap-1">
            {collapsedCount > 0 && (
              <button
                type="button"
                data-testid="expand-all"
                onClick={expandAll}
                className="rounded-full bg-[var(--btnp)]/20 px-2 py-0.5 text-xs text-[var(--tx)]"
              >
                {i18n.t("mobile.detail.expandAll")}
              </button>
            )}
            {collapsedCount < toggleableMessages.length && (
              <button
                type="button"
                data-testid="collapse-all"
                onClick={collapseAll}
                className="rounded-full bg-[var(--cb)] px-2 py-0.5 text-xs text-[var(--mut)]"
              >
                {i18n.t("mobile.detail.collapseAll")}
              </button>
            )}
          </span>
        </div>
      )}
      {loading && messages === null && (
        <p className="py-16 text-center text-sm text-[var(--mut)]">
          {i18n.t("mobile.detail.loading")}
        </p>
      )}
      {error && (
        <div className="py-16 text-center">
          <p data-testid="detail-error" className="mb-3 text-sm text-rose-600 dark:text-rose-400">
            {i18n.t(error.status === 404 ? "mobile.detail.error404" : "mobile.detail.errorGeneric")}
          </p>
          <button
            type="button"
            data-testid="detail-retry"
            onClick={retry}
            className="rounded-full bg-[var(--cb)] px-4 py-1.5 text-sm text-[var(--tx)]"
          >
            {i18n.t("mobile.detail.retry")}
          </button>
        </div>
      )}
      {!codexProjectionActive && messages !== null && !error && messages.length === 0 && (
        <p className="py-16 text-center text-sm text-[var(--mut)]">
          {i18n.t("mobile.detail.noMessages")}
        </p>
      )}
      {/* 书签跳转：自动加载中提示（M5 P3-c）与到顶未命中 miss 提示（M3+） */}
      {jumpLoading && (
        <p
          data-testid="bookmark-jump-loading"
          className="mb-2 rounded-lg bg-[var(--btnp)]/10 px-3 py-2 text-xs text-[var(--tx)]"
        >
          {i18n.t("mobile.detail.bookmarkJumpLoading")}
        </p>
      )}
      {bookmarkJumpMiss && !jumpLoading && (
        <p
          data-testid="bookmark-jump-miss"
          className="mb-2 rounded-lg bg-amber-500/10 px-3 py-2 text-xs text-amber-700 dark:text-amber-400"
        >
          {i18n.t("mobile.detail.bookmarkJumpMiss")}
        </p>
      )}
      {/* 「加载更早消息」置于列表**最上方**（2026-09-16 用户裁决）：语义是
          「往前翻到头再加一段更早的」，与阅读方向一致；此前放在列表末尾，
          与「更早」的空间直觉相反 */}
      {hasLoadMore && (
        <div className="pt-1 pb-3 text-center">
          <button
            type="button"
            data-testid="load-more"
            onClick={() => {
              const el = messageAreaRef.current;
              if (el) {
                pendingScrollRef.current = { prevHeight: el.scrollHeight, prevTop: el.scrollTop };
              }
              setLimit((l) => Math.min(l + PAGE_LIMIT, MAX_LIMIT));
            }}
            className="rounded-full bg-[var(--cb)] px-4 py-1.5 text-xs text-[var(--tx)]"
          >
            {i18n.t(loading ? "mobile.detail.loading" : "mobile.detail.loadMore")}
          </button>
        </div>
      )}
      {!codexProjectionActive && messages !== null && messages.length > 0 && (
        <ul className="space-y-2 pb-4">
          {messages.map((m) => {
            const collapsed = isCollapsed(m);
            const toggleable = isToggleable(m);
            const isUser = m.kind === "user";
            const msgBookmark = bookmarkByAnchor.get(messageAnchor(m));
            return (
              <li
                key={m.seq}
                data-testid={`msg-${m.seq}`}
                data-kind={m.kind}
                data-seq={m.seq}
                className={
                  isUser ? "relative flex flex-col items-end" : "relative flex flex-col items-start"
                }
              >
                {/* 书签角标（M3+）：该消息命中书签时在气泡左上显示色点 */}
                {msgBookmark && (
                  <span
                    data-testid={`msg-bookmark-${m.seq}`}
                    title={i18n.t("mobile.detail.bookmarkTitle", { preview: msgBookmark.preview })}
                    className="absolute -top-1 -left-1 h-2.5 w-2.5 rounded-full ring-2 ring-[var(--pg)]"
                    style={{ backgroundColor: msgBookmark.color }}
                  />
                )}
                <div
                  className={
                    isUser
                      ? "max-w-[85%] rounded-2xl rounded-br-sm border border-[var(--cb)] bg-[var(--bub)] px-3 py-2"
                      : "w-full rounded-2xl rounded-bl-sm border border-[var(--cb)] bg-[var(--cbg)] px-3 py-2"
                  }
                >
                  {toggleable ? (
                    <>
                      <button
                        type="button"
                        data-testid={`msg-${m.seq}-toggle`}
                        aria-expanded={!collapsed}
                        onClick={() => toggleCollapsed(m)}
                        className="-mx-1 flex w-[calc(100%+8px)] items-center gap-1 rounded-lg px-1 py-0.5 text-left text-xs text-[var(--mut)] hover:bg-[var(--cb)]/60 dark:hover:bg-[var(--btnp)]"
                      >
                        {collapsed ? <ChevronRight size={13} /> : <ChevronDown size={13} />}
                        <span className="truncate">{collapsedLabel(m)}</span>
                      </button>
                      {!collapsed && <div className="mt-1">{renderBody(m)}</div>}
                    </>
                  ) : (
                    renderBody(m)
                  )}
                </div>
              </li>
            );
          })}
        </ul>
      )}
    </>
  );

  // ===== 预览区 sheet 化（T1，2026-10-09）=====
  // 子 Agent sheet 钮的挂载门（决策 7）：名单为 null/空 → 钮不渲染（sheet 不可达）
  const subagentsShown = subagentList !== null && subagentList.length > 0;
  // 顶栏「子 Agent」钮的运行数徽标（n = running 数，非总数）
  const runningCount = subagentList?.filter((s) => s.status === "running").length ?? 0;
  // 当前选中的子 agent（subagents sheet 二级详情在显；null = 看板态/文件选中）。
  // 先解出裸 id：闭包（.find 回调）里 TS 不保留 preview.open 的属性路径收窄
  const subagentDetailId = preview?.open.kind === "subagent" ? preview.open.id : null;
  // Task 7：当前打开文件路径（闭包里 TS 不保留 preview.open 的属性路径收窄）
  const openFilePath = preview?.open.kind === "file" ? preview.open.path : null;

  // 顶栏 sheet bar（审查 S5 唯一实例裁决）：sheet 两钮 + 布局切换器 + 关闭。
  // 原 FilePanel / FilePreview / SubagentDetail 内部的切换器/关闭钮全部不再渲染。
  // split/split-h 时位于预览窗格顶部、fullscreen 时位于浮层顶部——同一份 JSX
  // 两处落点，任一时刻只渲染一处（唯一实例不变）
  const sheetBar = preview ? (
    <div
      data-testid="sheet-bar"
      className="flex shrink-0 items-center gap-2 border-b border-[var(--cb)] bg-[var(--cbg)] px-3 py-2"
    >
      <button
        type="button"
        data-testid="sheet-tab-files"
        aria-pressed={preview.sheet === "files"}
        onClick={() => switchSheet("files")}
        className={`shrink-0 rounded-full px-2.5 py-1 text-xs ${
          preview.sheet === "files"
            ? "bg-violet-600 text-white dark:bg-violet-400"
            : "bg-[var(--cb)] text-[var(--mut)] hover:text-[var(--tx)]"
        }`}
      >
        {i18n.t("mobile.detail.tabFiles")}
      </button>
      {subagentsShown && (
        <button
          type="button"
          data-testid="sheet-tab-subagents"
          aria-pressed={preview.sheet === "subagents"}
          onClick={() => switchSheet("subagents")}
          className={`shrink-0 rounded-full px-2.5 py-1 text-xs ${
            preview.sheet === "subagents"
              ? "bg-violet-600 text-white dark:bg-violet-400"
              : "bg-[var(--cb)] text-[var(--mut)] hover:text-[var(--tx)]"
          }`}
        >
          {i18n.t("mobile.detail.tabSubagents", { count: runningCount })}
        </button>
      )}
      {/* 布局切换器：仅「文件」sheet 显示——子 Agent sheet 是两层级导航（清单 →
          详情），无分屏布局可切（2026-10-09 用户裁决）；关闭钮两 sheet 恒有 */}
      <span className="ml-auto flex shrink-0 items-center gap-1">
        {preview.sheet === "files" && (
          <PreviewModeSwitcher
            mode={preview.mode}
            onChange={changePreviewMode}
            testIdPrefix="preview-toggle"
          />
        )}
        <button
          type="button"
          data-testid="preview-close"
          aria-label={i18n.t("mobile.detail.closePreview")}
          onClick={closePreview}
          className="shrink-0 rounded-full p-1 text-[var(--mut)] hover:bg-[var(--cb)] dark:hover:bg-[var(--btnp)]"
        >
          <X size={16} />
        </button>
      </span>
    </div>
  ) : null;

  // sheet 看板（左栏）：文件面板 / 子 Agent 清单（分屏主从布局下选中卡高亮）
  const sheetBoard = preview ? (
    preview.sheet === "files" ? (
      <FilePanel
        entries={fileView}
        truncated={fileTruncated}
        scope={fileScope}
        loading={fileLoading}
        onScopeChange={setFileScope}
        onOpenFile={openFileFromList}
        onLocate={codexProjectionActive ? locateFromPanel : undefined}
        fontScale={fontScale}
      />
    ) : subagentsShown ? (
      <div data-font-scale={fontScale} className="min-h-0 flex-1 overflow-y-auto px-3 py-2">
        <SubagentList
          list={subagentList}
          onOpen={openSubagentFromList}
          selectedId={subagentDetailId}
        />
      </div>
    ) : (
      <p
        data-testid="subagent-board-empty"
        className="flex-1 py-12 text-center text-sm text-[var(--mut)]"
      >
        {i18n.t("mobile.detail.noSubagents")}
      </p>
    )
  ) : null;

  // sheet 二级内容（右栏）：文件预览 / 子 agent 详情 / 空态提示
  const sheetContent = preview ? (
    preview.open.kind === "file" ? (
      <FilePreview
        session={session}
        filePath={preview.open.path}
        mode={preview.mode}
        fontScale={fontScale}
        refreshKey={preview.openSeq}
        onBack={preview.backToList ? backToBoard : undefined}
        onLocate={
          codexProjectionActive && openFilePath !== null
            ? () => locateFileInMessages(openFilePath)
            : undefined
        }
      />
    ) : subagentDetailId !== null ? (
      <SubagentDetail
        key={`subagent-detail-${session.id}-${subagentDetailId}`}
        session={session}
        subagentId={subagentDetailId}
        subagentName={subagentList?.find((s) => s.id === subagentDetailId)?.name ?? null}
        running={subagentList?.find((s) => s.id === subagentDetailId)?.status === "running"}
        onBack={preview.backToList ? backToBoard : undefined}
        openFile={openFile}
        fontScale={fontScale}
      />
    ) : (
      <div
        data-testid="preview-empty"
        className="flex h-full items-center justify-center px-4 text-sm text-[var(--mut)]"
      >
        {i18n.t("mobile.detail.selectLeft")}
      </div>
    )
  ) : null;

  return (
    <div
      data-viewport={viewportKind}
      className="flex h-dvh flex-col bg-[var(--pg)] text-[var(--tx)]"
    >
      <header className="flex shrink-0 items-center gap-2 border-b border-[var(--cb)] px-3 py-2">
        <button
          type="button"
          data-testid="detail-back"
          aria-label={i18n.t("mobile.detail.backToBoard")}
          onClick={onBack}
          className="shrink-0 rounded-full p-1 text-[var(--mut)] hover:bg-[var(--cb)] dark:hover:bg-[var(--btnp)]"
        >
          <ArrowLeft size={18} />
        </button>
        <span className="min-w-0 flex-1">
          <span className="block truncate text-sm font-semibold text-[var(--tx)]">
            {session.projectName}
          </span>
          <span className="block truncate text-xs text-[var(--mut)]">
            {i18n.t(TOOL_LABEL_KEYS[session.agentType])}
            {session.title ? ` · ${session.title}` : ""}
          </span>
        </span>
        {/* 状态点（与看板卡片同语义：waiting 附加呼吸动画） */}
        <span
          className={`inline-block h-2.5 w-2.5 shrink-0 rounded-full ${STATUS_DOT_COLOR[session.status]} ${
            session.status === "waiting" ? "animate-pulse" : ""
          }`}
        />
        {/* 文件面板入口（M3+；ZCode 式排版，2026-09-16 用户裁决）：页头恒可见
            （不依赖预览是否打开）；开启态高亮 + tooltip 随状态切换 + 再点收回 */}
        <button
          type="button"
          data-testid="file-panel-button"
          aria-label={i18n.t("mobile.detail.filePanel")}
          aria-pressed={panelOpen}
          title={i18n.t(
            panelOpen ? "mobile.detail.collapseFilePanel" : "mobile.detail.openFilePanel"
          )}
          onClick={togglePanel}
          className={`shrink-0 rounded-full p-1 ${
            panelOpen
              ? "bg-[var(--cb)] text-[var(--tx)]"
              : "text-[var(--mut)] hover:bg-[var(--cb)] dark:hover:bg-[var(--btnp)]"
          }`}
        >
          <PanelLeft size={16} />
        </button>
        {/* 字号档位（2026-09-16 用户裁决）：面板按钮旁，作用于正文与预览内容 */}
        <select
          data-testid="font-scale-select"
          aria-label={i18n.t("mobile.detail.fontScale")}
          title={i18n.t("mobile.detail.fontScale")}
          value={String(fontScale)}
          onChange={(e) => setFontScale(Number(e.target.value))}
          className="shrink-0 rounded-md border border-[var(--cb)] bg-transparent px-1 py-0.5 text-xs text-[var(--mut)]"
        >
          {FONT_SCALES.map((v) => (
            <option key={v} value={String(v)}>
              {Math.round(v * 100)}%
            </option>
          ))}
        </select>
        {/* 布局切换器唯一实例已上收顶栏 sheet bar（T1 审查 S5 裁决：预览区 sheet 化后
            顶栏持有唯一切换器/关闭钮，预览页头不再各自挂载） */}
        <button
          type="button"
          data-testid="detail-refresh"
          aria-label={i18n.t("mobile.detail.refresh")}
          onClick={retry}
          className="shrink-0 rounded-full p-1 text-[var(--mut)] hover:bg-[var(--cb)] dark:hover:bg-[var(--btnp)]"
        >
          <RotateCw size={15} />
        </button>
      </header>

      {/* 预览区（T1 sheet 化）：split/split-h 内联分屏（可拖分隔条）——右/下窗格 =
          顶栏 sheet bar + [看板 ┃ 内容] 主从两栏；fullscreen 全屏浮层——顶栏在浮层
          顶部，看板与内容互斥（既有语义保留）。看板二选一：FilePanel（files sheet）/
          SubagentList（subagents sheet）；内容三选一：FilePreview / SubagentDetail /
          空态提示 */}
      {preview?.mode === "split" || preview?.mode === "split-h" ? (
        <div
          ref={splitRef}
          data-testid="split-container"
          data-split={preview.mode}
          className={`flex min-h-0 flex-1 ${preview.mode === "split-h" ? "flex-row" : "flex-col"}`}
        >
          {/* 对话列：分屏 = 对话列 + 文件列的并列布局，**对话列须保有完整对话能力**
              （2026-09-19 用户裁决）——审批红卡与发送输入框在本分支同样挂载。
              原实现把两者排除在分屏外（仅正文视图挂载），致分屏看文件时无法发消息、
              看不到待审批红卡；该行为无设计依据、系实现越权，已修。
              红卡刷新机制、组件钥匙口径与下方正文分支一致（见该分支注释）。
              **竖屏换位（2026-09-20 用户裁决）**：split 视觉顺序 = 文件在上、对话在下
              （输入框贴底，竖屏顺手）；用 CSS order 视觉换位而非调 DOM 顺序——
              DOM/a11y 顺序与 split-h 保持一致（对话优先）。minHeight = 对话列
              最小高度保护（防 composer 压没消息区），与文件栏 maxHeight 同源常量 */}
          <div
            className={`flex min-h-0 min-w-0 flex-1 flex-col ${
              preview.mode === "split" ? "order-3" : ""
            }`}
            style={preview.mode === "split" ? { minHeight: SPLIT_CONVERSATION_MIN_PX } : undefined}
          >
            {bookmarkBar}
            {/* Task 7 定位状态条（搜索/命中/未命中/失败；失败可重试、可返回原预览） */}
            {jumpBanner}
            {/* 审批/问答/模式三卡 2026-10-04 起停靠在消息区下方（cardDock）——
                原「消息区上方」挂载位置连同挂载门语义注释一并迁移至 cardDock 定义处 */}
            <MessageScrollArea
              fontScale={fontScale}
              showJump={showJump}
              showJumpTop={showJumpTop}
              onScroll={handleAreaScroll}
              onJump={jumpToLatest}
              onJumpTop={jumpToTop}
              ref={messageAreaRef}
            >
              {messageArea}
            </MessageScrollArea>
            {cardDock}
            <MessageComposer
              editDraft={editDraft}
              onEditDraftConsumed={consumeEditDraft}
              draftText={composerDrafts[session.id] ?? ""}
              onDraftTextChange={changeDraftText}
              key={`composer-${session.id}`}
              session={session}
              onQueueChanged={bumpQueue}
              onSendGate={setCodexSendGate}
              refreshSendInfoSignal={sendInfoTick}
              disableTerminalProbes={appForm}
              turnControl={appForm ? codexTurnControl : undefined}
            />
          </div>
          <SplitHandle
            orientation={preview.mode === "split-h" ? "horizontal" : "vertical"}
            ratio={fileRatio}
            onRatioChange={setFileRatio}
            containerRef={splitRef}
            ratioPane={preview.mode === "split" ? "before" : "after"}
            className={preview.mode === "split" ? "order-2" : ""}
          />
          <div
            data-testid="split-file-pane"
            className={`shrink-0 overflow-hidden ${preview.mode === "split" ? "order-1" : ""}`}
            style={
              preview.mode === "split-h"
                ? { width: `${fileRatio * 100}%` }
                : {
                    height: `${fileRatio * 100}%`,
                    maxHeight: `calc(100% - ${SPLIT_CONVERSATION_MIN_PX}px)`,
                  }
            }
          >
            <div
              data-testid="preview-shell"
              data-sheet={preview.sheet}
              data-open={preview.open.kind}
              data-mode={preview.mode}
              className="flex h-full min-h-0 flex-col"
            >
              {/* 顶栏 sheet bar（唯一实例：sheet 钮 + 布局切换器 + 关闭）。
                布局切换器控制预览窗格的停靠方式（dock 在对话旁 / 全屏覆盖）；
                窗格内部两个 sheet 均为**两层级导航**（2026-10-09 用户裁决：看板与
                详情/预览互斥单栏，不做窗格内分屏——46/54 双栏太窄） */}
              {sheetBar}
              {/* 两层级导航（两个 sheet 同构）：open=none → 看板（一级）；
                open 命中 → 详情/预览（二级），页头返回钮回看板 */}
              {preview.open.kind === "none" ? (
                <div data-testid="sheet-board" className="flex min-h-0 min-w-0 flex-1 flex-col">
                  {sheetBoard}
                </div>
              ) : (
                <div data-testid="sheet-content" className="flex min-h-0 min-w-0 flex-1 flex-col">
                  {sheetContent}
                </div>
              )}
            </div>
          </div>
        </div>
      ) : (
        <div className="flex min-h-0 flex-1 flex-col">
          {bookmarkBar}
          {/* Task 7 定位状态条（与分屏分支同构） */}
          {jumpBanner}
          {/* 审批/问答/模式三卡 2026-10-04 起停靠在消息区下方（cardDock，与分屏
              分支同构）——原「紧贴 messageArea 上方」的挂载注释（M8 Task 12 红卡
              刷新机制 / M9R P3 钥匙防线 / 丁T1-T2 挂载门）一并迁移至 cardDock 定义处 */}
          <MessageScrollArea
            fontScale={fontScale}
            showJump={showJump}
            showJumpTop={showJumpTop}
            onScroll={handleAreaScroll}
            onJump={jumpToLatest}
            onJumpTop={jumpToTop}
            ref={messageAreaRef}
          >
            {messageArea}
          </MessageScrollArea>
          {cardDock}
          {/* 发送输入区（M7 Task 7，W4）：**全布局态挂载**（正文 / split / split-h，
              2026-09-19 用户裁决）——分屏时对话列同样可发消息；
              send-info 拉取失败时组件自静默，不影响对话渲染；钥匙口径同上 */}
          <MessageComposer
            editDraft={editDraft}
            onEditDraftConsumed={consumeEditDraft}
            draftText={composerDrafts[session.id] ?? ""}
            onDraftTextChange={changeDraftText}
            key={`composer-${session.id}`}
            session={session}
            onQueueChanged={bumpQueue}
            onSendGate={setCodexSendGate}
            refreshSendInfoSignal={sendInfoTick}
            disableTerminalProbes={appForm}
            turnControl={appForm ? codexTurnControl : undefined}
          />
        </div>
      )}

      {/* fullscreen = 全屏浮层（覆盖对话，关闭回到原位——sheet 状态由本组件持有）。
          顶栏 sheet bar 在浮层顶部（S5 唯一实例在全屏态的可达位）；看板与内容互斥
          单栏整宽（既有互斥语义保留） */}
      {preview?.mode === "fullscreen" && (
        <div className="fixed inset-0 z-50 flex flex-col bg-[var(--cbg)]">
          <div
            data-testid="preview-shell"
            data-sheet={preview.sheet}
            data-open={preview.open.kind}
            data-mode="fullscreen"
            className="flex min-h-0 flex-1 flex-col"
          >
            {sheetBar}
            {preview.open.kind === "none" ? (
              <div
                data-testid="sheet-board"
                className="flex min-h-0 flex-1 flex-col overflow-hidden"
              >
                {sheetBoard}
              </div>
            ) : (
              <div data-testid="sheet-content" className="min-h-0 min-w-0 flex-1">
                {sheetContent}
              </div>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
