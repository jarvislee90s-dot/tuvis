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
import ApproveCard from "./ApproveCard";
import PlanFeedbackBar from "./PlanFeedbackBar";
import { collapsedLabel, isProcessKind } from "./message-fold";
import { useMessageRenderers } from "./message-render";
import ModeBar from "./ModeBar";
import SubagentChips from "./SubagentChips";
import SubagentDetail from "./SubagentDetail";
import SubagentList from "./SubagentList";
import QuestionCard from "./QuestionCard";
import BookmarkBar from "./BookmarkBar";
import FilePanel from "./FilePanel";
import FilePreview from "./FilePreview";
import MessageComposer from "./MessageComposer";
import PreviewModeSwitcher, { type PreviewMode } from "./PreviewModeSwitcher";
import SplitHandle from "./SplitHandle";
import {
  ApiError,
  fetchHeadlessApproval,
  fetchSessionFiles,
  fetchSessionMessages,
  fetchSessionSubagents,
  headlessApprove,
  headlessCancel,
  type HeadlessApprovalPending,
  type HeadlessApprovalQuestion,
  type HeadlessTurn,
  type SessionFileEntry,
  type SessionMessage,
  type SubagentView,
} from "./api";
import { STATUS_DOT_COLOR, TOOL_LABELS } from "./board-logic";
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
};

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
// 无头回执卡（Task 8 / H7）：发送中 / 回执 / 失败分诊 + 取消钮
// ============================================================

/** 失败阶段 → **用户可读分诊文案**（与 Rust `inject::headless::receipt::Stage` 的 wire 词
 *  一一对应，勿漂移）。这是**分诊**不是编造结论：每条只说该阶段实际发生了什么；未知档
 *  如实显示「未分类失败」+ 后端 reason 原文，不猜成因。
 *
 *  **跨语言锁**：本表键集合必须与 `tests/fixtures/headless_stages.json` 的 `stages`
 *  逐项相等（Rust 侧 `receipt::stage_wire_names_are_pinned` 断言同一份夹具）——任一侧
 *  新增变体而另一侧没跟上，必有一侧先红。 */
export const HEADLESS_STAGE_TRIAGE: Record<string, string> = {
  spawn: "进程未能启动（安装路径 / 权限）",
  version_gate: "版本门控未通过（CLI flag 面已变，MAM 拒发以防盲发）",
  timeout: "回合超时（看门狗已终止进程树）",
  crash: "进程异常退出",
  channel_error: "通道异常（未拿到有效回执）",
  dialog: "终端对话框在场",
  workspace_busy: "工作区忙（ZCode 应用正在该工作区活动）",
  // 投递前拒绝（Task 8 复审追补）：回合**未起跑、零字节投递**——不是通道故障，
  // 与 channel_error 分列，文案必须说清「没发出去」
  refused: "投递前拒绝（未起跑、未发送）——原因见下",
};

/** 阶段分诊文案（未知/缺省 → 如实标注，不编成因） */
export function headlessStageText(stage?: string | null): string {
  if (!stage) return "未分类失败";
  return HEADLESS_STAGE_TRIAGE[stage] ?? `未分类失败（${stage}）`;
}

/** 耗时展示：<1s 走毫秒（探测级回合），≥1s 走秒（一位小数） */
export function headlessDurationText(ms: number): string {
  if (!Number.isFinite(ms) || ms < 0) return "—";
  return ms < 1000 ? `${Math.round(ms)}ms` : `${(ms / 1000).toFixed(1)}s`;
}

// ============================================================
// 无头审批 / 问答卡（Task 13 / C4：claude 双向桥的移动端面）
// ============================================================

/** claude 无头通道 wire 名（与 Rust `routing::HeadlessKind::ClaudeP.wire_name()` 同源）。
 *  **唯一用途**：回执卡据此判断「本条要不要轮询审批卡」——审批面只属于 claude 双向桥
 *  （codex queue / zcode yolo 无审批面；kimi/opencode 在非交互模式下由 CLI 自行拒绝权限
 *  请求，Task 13 实测）。别拿它做通道白名单。 */
export const HEADLESS_CLAUDE_CHANNEL = "headless_claude_p";

/** 审批卡轮询周期（**只在无头回合在飞时**轮询；不做常驻轮询）。
 *  1.5s 的取舍：审批是「回合停下来等人」的形态，用户看到卡的延迟要短；而每次轮询只是一条
 *  只读查询（进程内内存态，无 IO），代价可忽略。 */
export const HEADLESS_APPROVE_POLL_MS = 1500;

/** 一题的已答标签（**判据与核侧 `Q::answered` / `answer_set_from_entries` 同规**）。
 *
 *  - **多选**：勾选表 + 自由文本行（自由文本作为额外标签，编成 JSON 数组）；
 *  - **单选**：**至多一个标签**——自由文本行**取代**勾选（不是追加）。为什么必须取代：
 *    核侧对单选收到多个标签是**如实拒绝**（「单选题只允许一个答案」，附录 E-② 的
 *    单选值 = 标签字符串、多选值 = 数组），若前端把「勾了 A + 又写了其他」拼成两个标签，
 *    用户点提交只会拿到 400——那是**死角落**（Task 13 复审 Minor 1）。
 *    界面上的互斥由 [`HeadlessApprovalCard`] 的 onChange 保证（选选项清空自由文本、
 *    写自由文本清空选项），本函数再兜一层：无论状态怎么漂，单选**永不吐两个标签**。 */
export function headlessAnswerLabels(
  q: HeadlessApprovalQuestion,
  picked: string[],
  free: string
): string[] {
  const f = free.trim();
  if (f) {
    return q.multiSelect ? [...picked, f] : [f];
  }
  return q.multiSelect ? [...picked] : picked.slice(0, 1);
}

/** 未答题面（空 = 可提交；防静默丢题的第一道闸——第二道在核侧 `AnswerSet::new`） */
export function headlessUnanswered(
  questions: HeadlessApprovalQuestion[],
  picked: Record<number, string[]>,
  free: Record<number, string>
): string[] {
  return questions
    .map((q, i) => ({ q, i }))
    .filter(({ q, i }) => headlessAnswerLabels(q, picked[i] ?? [], free[i] ?? "").length === 0)
    .map(({ q }) => q.question);
}

/** **审批 / 问答卡**（Task 13 / C4 激活；Task 10 的接口预留位就是这里）。
 *
 *  数据来自 `GET /session-headless-approval`（父组件轮询），应答走
 *  `POST /session-headless-approve`：
 *  - **审批卡**：标题 = 工具名，主体 = **入参原文**（Bash 就是命令行——用户必须看清要批准
 *    什么），并显示**权限档**（`tier` = 审批面形态 `stdio` + `permissionMode` = CLI 内部档
 *    `default`，Task 10 的 tier 展示义务在此兑现）；两钮「允许 / 拒绝」；
 *  - **问答卡**：逐题渲染单选/多选 + 「其他」自由文本；**不全答则提交禁用**并列出未答题面
 *    （防静默丢题，附录 E-②/③）；
 *  - **弃卡 = 拒绝**（附录 E-②：allow 但未答 = 静默丢题，故关闭卡片一律发 `deny`，
 *    绝不发空答案）；
 *  - 未送达（`delivered:false`）如实回显原因并停止等待（回合可能已终结/超时）。 */
export function HeadlessApprovalCard({ pending }: { pending: HeadlessApprovalPending }) {
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<string | null>(null);
  const [picked, setPicked] = useState<Record<number, string[]>>({});
  const [free, setFree] = useState<Record<number, string>>({});
  const isQuestion = pending.kind === "question";
  const unanswered = isQuestion ? headlessUnanswered(pending.questions, picked, free) : [];
  // 题面缺失（claude 给了 control_request 但 questions 为空）：**不编题、也不放行提交**
  const noQuestions = isQuestion && pending.questions.length === 0;
  const canSubmit = isQuestion && !noQuestions && unanswered.length === 0 && !busy;

  const answer = (
    decision: "allow" | "deny" | "answer",
    answers?: { question: string; labels: string[] }[]
  ) => {
    setBusy(true);
    setNote(null);
    void headlessApprove(pending.sessionId, pending.requestId, decision, answers)
      .then((res) => {
        // 送达 = 回合已收到这一答（卡由轮询自然收走：核侧登记表已清空）
        if (res.delivered) {
          setNote("已送达——回合继续执行");
          return;
        }
        // 未送达：如实回显（不是错误，只是这一答没赶上）；下一轮轮询会据核侧状态收走卡片
        setNote(res.reason ?? "应答未送达（回合可能已结束）");
      })
      .catch((e: unknown) => {
        const reason =
          e instanceof ApiError && e.data && typeof e.data.reason === "string"
            ? e.data.reason
            : e instanceof ApiError
              ? e.message
              : String(e);
        setNote(`应答失败：${reason}`);
      })
      .finally(() => setBusy(false));
  };

  return (
    <div
      data-testid="headless-approval-card"
      data-kind={pending.kind}
      className="mx-3 mb-2 rounded-lg border border-amber-300/70 bg-amber-500/5 px-3 py-2 dark:border-amber-700/70 dark:bg-amber-400/5"
    >
      <div className="flex flex-wrap items-center gap-2 text-xs">
        <span
          data-testid="headless-approval-title"
          className="rounded-full bg-amber-500/15 px-2 py-0.5 font-medium text-amber-800 dark:bg-amber-400/15 dark:text-amber-300"
        >
          {isQuestion ? `claude 提问：${pending.toolName}` : `审批请求：${pending.toolName}`}
        </span>
        {/* 权限档展示（Task 10 的义务）：tier = 审批面形态；permissionMode = CLI 内部权限档 */}
        <span data-testid="headless-approval-tier" className="text-slate-500 dark:text-slate-400">
          审批档 {pending.tier}
          {pending.permissionMode ? ` · 权限模式 ${pending.permissionMode}` : ""}
        </span>
        <span data-testid="headless-approval-waited" className="text-slate-400 dark:text-slate-500">
          已等待 {headlessDurationText(pending.waitedMs)}
        </span>
      </div>
      {!isQuestion && (
        <pre
          data-testid="headless-approval-input"
          className="mt-1 max-h-40 overflow-auto rounded bg-slate-900/5 px-2 py-1 text-xs break-all whitespace-pre-wrap text-slate-800 dark:bg-slate-100/5 dark:text-slate-100"
        >
          {pending.input}
        </pre>
      )}
      {noQuestions && (
        <p
          data-testid="headless-approval-no-questions"
          className="mt-1 text-xs text-rose-700 dark:text-rose-300"
        >
          这一条问询没有携带任何题面（questions 为空）——如实告知，MAM 不替你编题；请拒绝并在
          终端里回答
        </p>
      )}
      {isQuestion &&
        pending.questions.map((q, i) => {
          const sel = picked[i] ?? [];
          return (
            <div key={`${q.question}-${i}`} data-testid={`headless-question-${i}`} className="mt-2">
              <p className="text-xs font-medium text-slate-700 dark:text-slate-200">
                {q.header ? `${q.header}｜` : ""}
                {q.question}
                {q.multiSelect ? "（多选）" : ""}
              </p>
              <div className="mt-1 flex flex-col gap-1">
                {q.options.map((o, j) => (
                  <label
                    key={`${o.label}-${j}`}
                    className="flex items-start gap-1 text-xs text-slate-700 dark:text-slate-200"
                  >
                    <input
                      type={q.multiSelect ? "checkbox" : "radio"}
                      name={`headless-q-${i}`}
                      data-testid={`headless-q-${i}-opt-${j}`}
                      checked={sel.includes(o.label)}
                      onChange={() => {
                        setPicked((prev) => {
                          const cur = prev[i] ?? [];
                          const next = q.multiSelect
                            ? cur.includes(o.label)
                              ? cur.filter((x) => x !== o.label)
                              : [...cur, o.label]
                            : [o.label];
                          return { ...prev, [i]: next };
                        });
                        // **单选互斥**（Task 13 复审 Minor 1）：选了选项就清掉「其他」自由文本
                        // ——单选只能有一个答案（核侧对多标签是 400，死角落必须在前端就堵死）
                        if (!q.multiSelect) {
                          setFree((prev) => ({ ...prev, [i]: "" }));
                        }
                      }}
                    />
                    <span>
                      {o.label}
                      {o.description ? (
                        <span className="text-slate-400 dark:text-slate-500">
                          （{o.description}）
                        </span>
                      ) : null}
                    </span>
                  </label>
                ))}
                <input
                  type="text"
                  data-testid={`headless-q-${i}-free`}
                  placeholder="其他（自由作答）"
                  value={free[i] ?? ""}
                  onChange={(e) => {
                    const v = e.target.value;
                    setFree((prev) => ({ ...prev, [i]: v }));
                    // 单选互斥的另一半：写了「其他」就清掉已选选项（取代，不是追加）
                    if (!q.multiSelect && v.trim() !== "") {
                      setPicked((prev) => ({ ...prev, [i]: [] }));
                    }
                  }}
                  className="rounded border border-slate-300/70 bg-transparent px-2 py-1 text-xs text-slate-800 dark:border-slate-600/70 dark:text-slate-100"
                />
              </div>
            </div>
          );
        })}
      {isQuestion && !noQuestions && unanswered.length > 0 && (
        <p
          data-testid="headless-question-incomplete"
          className="mt-2 text-xs text-rose-700 dark:text-rose-300"
        >
          还有 {unanswered.length} 题未作答，不能提交（防静默丢题）：{unanswered.join("；")}
        </p>
      )}
      <div className="mt-2 flex flex-wrap items-center gap-2">
        {isQuestion ? (
          <button
            type="button"
            data-testid="headless-question-submit"
            disabled={!canSubmit}
            onClick={() =>
              answer(
                "answer",
                pending.questions.map((q, i) => ({
                  question: q.question,
                  labels: headlessAnswerLabels(q, picked[i] ?? [], free[i] ?? ""),
                }))
              )
            }
            className="rounded-full bg-emerald-500/15 px-3 py-0.5 text-xs text-emerald-800 disabled:opacity-40 dark:bg-emerald-400/15 dark:text-emerald-300"
          >
            提交答案
          </button>
        ) : (
          pending.options.map((o) => (
            <button
              key={o.id}
              type="button"
              data-testid={`headless-approval-${o.id}`}
              disabled={busy}
              onClick={() => answer(o.id === "allow" ? "allow" : "deny")}
              className={`rounded-full px-3 py-0.5 text-xs disabled:opacity-40 ${
                o.id === "allow"
                  ? "bg-emerald-500/15 text-emerald-800 dark:bg-emerald-400/15 dark:text-emerald-300"
                  : "bg-rose-500/15 text-rose-800 dark:bg-rose-400/15 dark:text-rose-300"
              }`}
            >
              {o.label}
            </button>
          ))
        )}
        {/* 弃卡 = 拒绝（附录 E-②）：绝不发 allow 空答案（那是静默丢题） */}
        {isQuestion && (
          <button
            type="button"
            data-testid="headless-approval-dismiss"
            disabled={busy}
            onClick={() => answer("deny")}
            className="rounded-full bg-rose-500/15 px-3 py-0.5 text-xs text-rose-800 disabled:opacity-40 dark:bg-rose-400/15 dark:text-rose-300"
          >
            拒绝（关闭卡片）
          </button>
        )}
        <span className="text-xs text-slate-400 dark:text-slate-500">
          会话 {pending.sessionId} · 通道 {pending.channel}
        </span>
      </div>
      {note && (
        <p
          data-testid="headless-approval-note"
          className="mt-1 text-xs text-slate-600 dark:text-slate-300"
        >
          {note}
        </p>
      )}
    </div>
  );
}

/** 无头回执卡（Task 8 / H7）：三态——发送中（带取消钮）/ 回执 / 失败分诊。
 *
 *  **文案纪律**：assistant 摘要、token、耗时、可见性提示**全部来自后端回执**（`Visibility::note()`
 *  逐字文案）——前端只渲染不另编，与 H3 置灰同款单一措辞出口。可见性提示只在**回合真的
 *  落到工作区**（ok/cancelled）时展示：失败/排队时什么都没写进工作区，承诺「重启后可见」
 *  就是谎报。
 *
 *  **取消钮**：仅在发送中在场；点它调 `/session-headless-cancel`（**请求**语义——送达与否
 *  如实回显，回合照常以回执收尾）。`data-phase` 供测试与样式分态。 */
export function HeadlessReceiptCard({
  session,
  turn,
  onDismiss,
}: {
  session: Session;
  turn: HeadlessTurn | null;
  onDismiss: () => void;
}) {
  const [cancelling, setCancelling] = useState(false);
  const [cancelNote, setCancelNote] = useState<string | null>(null);
  // Task 13/C4：在飞期间轮询**当前待答**的审批/问答请求（只在 claude 通道上开——审批面
  // 只属于 claude 双向桥；其余通道白轮询既是浪费也会误导用户以为有审批面）
  const [approval, setApproval] = useState<HeadlessApprovalPending | null>(null);
  const claudeChannel = turn?.phase === "sending" && turn.channel === HEADLESS_CLAUDE_CHANNEL;
  // 回合态翻转即清掉上一发的取消提示（避免「已请求取消」标签挂在下一回合上）
  useEffect(() => {
    setCancelNote(null);
  }, [turn]);
  useEffect(() => {
    if (!claudeChannel) {
      setApproval(null);
      return;
    }
    let alive = true;
    const tick = () => {
      void fetchHeadlessApproval(session.id)
        .then((p) => {
          if (alive) setApproval(p);
        })
        .catch(() => {
          /* 拉取失败静默降级为「暂无卡」（回合照常跑；拿不到卡不等于出错） */
        });
    };
    tick();
    const timer = window.setInterval(tick, HEADLESS_APPROVE_POLL_MS);
    return () => {
      alive = false;
      window.clearInterval(timer);
    };
  }, [claudeChannel, session.id]);
  if (turn === null) return null;

  if (turn.phase === "sending") {
    return (
      <div
        data-testid="headless-receipt-card"
        data-phase="sending"
        className="mx-3 mb-2 rounded-lg border border-sky-300/60 bg-sky-500/5 px-3 py-2 dark:border-sky-700/60 dark:bg-sky-400/5"
      >
        <div className="flex flex-wrap items-center gap-2">
          <span
            data-testid="headless-sending"
            className="rounded-full bg-sky-500/10 px-2 py-0.5 text-xs text-sky-700 dark:bg-sky-400/10 dark:text-sky-300"
          >
            <span className="mr-1 inline-block h-1.5 w-1.5 animate-pulse rounded-full bg-sky-500 align-middle" />
            无头回合进行中…（进程级回合，可能持续数十秒）
          </span>
          <button
            type="button"
            data-testid="headless-cancel"
            disabled={cancelling}
            onClick={() => {
              setCancelling(true);
              void headlessCancel(session.id)
                .then((res) => {
                  setCancelNote(
                    res.cancelled
                      ? "已请求取消（回合会以「已取消」回执收尾）"
                      : (res.reason ?? "取消未送达（回合可能已结束）")
                  );
                })
                .catch((e: unknown) => {
                  setCancelNote(
                    e instanceof ApiError ? `取消失败：${e.message}` : `取消失败：${String(e)}`
                  );
                })
                .finally(() => setCancelling(false));
            }}
            className="rounded-full bg-rose-500/10 px-2 py-0.5 text-xs text-rose-700 disabled:opacity-40 dark:bg-rose-400/10 dark:text-rose-300"
          >
            取消
          </button>
          {cancelNote && (
            <span
              data-testid="headless-cancel-note"
              className="text-xs text-slate-500 dark:text-slate-400"
            >
              {cancelNote}
            </span>
          )}
        </div>
        {/* Task 13/C4：待答的审批/问答卡（claude 双向桥）——用户在回合在飞期间即可应答。
            卡的生命周期由轮询驱动：核侧登记表清空（答完/回合终结/超时）后自然收走 */}
        {approval && (
          <div className="mt-2">
            <HeadlessApprovalCard pending={approval} />
          </div>
        )}
      </div>
    );
  }

  const r = turn.receipt;
  const failed = r.status === "failed";
  const cancelled = r.status === "cancelled";
  // 可见性提示只在回合真的落到工作区时展示（ok / cancelled）
  const showVisibility = (r.status === "ok" || cancelled) && !!turn.visibilityNote;
  return (
    <div
      data-testid="headless-receipt-card"
      data-phase="done"
      data-status={r.status}
      className={`mx-3 mb-2 rounded-lg border px-3 py-2 ${
        failed
          ? "border-rose-300/60 bg-rose-500/5 dark:border-rose-700/60 dark:bg-rose-400/5"
          : "border-slate-300/60 bg-slate-500/5 dark:border-slate-700/60 dark:bg-slate-400/5"
      }`}
    >
      <div className="flex flex-wrap items-center gap-2 text-xs">
        {r.status === "ok" && (
          <span
            data-testid="headless-ok"
            className="rounded-full bg-emerald-500/10 px-2 py-0.5 text-emerald-700 dark:bg-emerald-400/10 dark:text-emerald-400"
          >
            无头回合完成
          </span>
        )}
        {r.status === "queued" && (
          <span
            data-testid="headless-queued"
            className="rounded-full bg-slate-200/70 px-2 py-0.5 text-slate-600 dark:bg-slate-700/60 dark:text-slate-300"
          >
            {/* **成因只由后端 reason 说话**（Task 9 / H8 起同一 queued 状态有两个成因：
                H4 全局并发名额已满〔zcode〕、H8 codex 已入队待 APP 消费）——此前的固定括注
                「全局并发名额已满」对后者是假话，故改为中性文案 + 下方 reason 原样透出 */}
            排队中（原因见下）
          </span>
        )}
        {cancelled && (
          <span
            data-testid="headless-cancelled"
            className="rounded-full bg-slate-200/70 px-2 py-0.5 text-slate-600 dark:bg-slate-700/60 dark:text-slate-300"
          >
            已取消
          </span>
        )}
        {failed && (
          <span
            data-testid="headless-failed"
            className="rounded-full bg-rose-500/10 px-2 py-0.5 text-rose-700 dark:bg-rose-400/10 dark:text-rose-400"
          >
            无头回合失败
          </span>
        )}
        {failed && (
          <span data-testid="headless-stage" className="text-rose-700 dark:text-rose-300">
            {headlessStageText(r.stage)}
          </span>
        )}
        <span data-testid="headless-duration" className="text-slate-500 dark:text-slate-400">
          耗时 {headlessDurationText(r.durationMs)}
        </span>
        {typeof r.tokens === "number" && (
          <span data-testid="headless-tokens" className="text-slate-500 dark:text-slate-400">
            {r.tokens} tokens
          </span>
        )}
        <button
          type="button"
          data-testid="headless-dismiss"
          onClick={onDismiss}
          className="ml-auto text-slate-400 hover:text-slate-600 dark:hover:text-slate-200"
        >
          收起
        </button>
      </div>
      {/* Task 10 的审批占位已**作废**（Task 13/C4 起审批卡真接线——见 `HeadlessApprovalCard`，
          在飞期间挂在本卡下方）。此处不再有任何「尚未启用」文案：那是过期声明。 */}
      {r.lastAssistant && (
        <p
          data-testid="headless-last-assistant"
          className="mt-1 text-xs break-words whitespace-pre-wrap text-slate-700 dark:text-slate-200"
        >
          {r.lastAssistant}
        </p>
      )}
      {r.reason && (
        <p
          data-testid="headless-reason"
          className="mt-1 text-xs break-words text-slate-600 dark:text-slate-300"
        >
          {r.reason}
        </p>
      )}
      {showVisibility && (
        <p
          data-testid="headless-visibility"
          className="mt-1 text-xs text-amber-700 dark:text-amber-400"
        >
          {turn.visibilityNote}
        </p>
      )}
    </div>
  );
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
          aria-label="跳到顶部"
          title="跳到顶部"
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
          aria-label="跳到最新消息"
          title="跳到最新消息"
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

export default function SessionDetail({ session, onBack }: SessionDetailProps) {
  const [messages, setMessages] = useState<SessionMessage[] | null>(null);
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
  const [fileLoading, setFileLoading] = useState(false);
  // 字号档位（默认 100%）
  const [fontScale, setFontScale] = useState<number>(1);
  // 书签表（M3+）：初值从模块级 store 读——SessionDetail 卸载重挂后仍恢复
  const [bookmarks, setBookmarks] = useState<Bookmark[]>(() => listBookmarks(session.id));
  // 跳转失败提示（书签指向更早范围，当前窗口内找不到）
  const [bookmarkJumpMiss, setBookmarkJumpMiss] = useState(false);
  const [preview, setPreview] = useState<PreviewState | null>(null);
  // **无头回执卡态**（Task 8 / H7）：由 MessageComposer 经 `onHeadlessTurn` 上报——
  // 「发送中」在 composer 发请求前上报（无头回合 = 进程生命周期，请求要在飞数十秒），
  // 终态回执在响应落地后上报。卡渲染在本页（回执卡是页面级信息：回合摘要 + 可见性提示
  // + 取消入口），composer 只负责上报，不重复渲染（单回执槽语义）。
  const [headlessTurn, setHeadlessTurn] = useState<HeadlessTurn | null>(null);
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

  /** 宽屏判定（matchMedia 防御式访问；jsdom / 隐私模式可能缺失，theme.ts 同款口径） */
  const isWideViewport = useCallback((): boolean => {
    try {
      return window.matchMedia?.("(min-width: 768px)")?.matches ?? false;
    } catch {
      return false; // matchMedia 缺失/异常 → 窄屏语义（Task 8 原裁决）
    }
  }, []);

  // 点文件链接（消息正文）→ 预览。Bug 2 顺手项（spec P9「按屏幕宽度自适应」）：
  // ≥768px 分屏（2026-09-20 裁决 + T2 决策 2 收口：宽屏默认 split-h = 左对话右文件）、
  // <768px 全屏；手动切换随时覆盖该默认值。
  // 正文进入 → files sheet 选中该文件，backToList=false（无返回列表按钮，既有行为不变）
  const openFile = useCallback(
    (path: string) => {
      setPreview({
        sheet: "files",
        open: { kind: "file", path },
        mode: isWideViewport() ? "split-h" : "fullscreen",
        backToList: false,
      });
    },
    [isWideViewport]
  );

  // 从文件看板进入单文件预览（M3+）：backToList=true → 预览页头显示返回按钮；
  // 布局沿用当前 mode（往返保持，用户裁决「布局保持」）
  const openFileFromList = useCallback((path: string) => {
    setPreview((p) => ({
      sheet: "files",
      open: { kind: "file", path },
      mode: p?.mode ?? "fullscreen",
      backToList: true,
    }));
  }, []);

  // chip 直达子 agent 详情（观察台 §二.4）：切到 subagents sheet 并选中该 agent。
  // 2026-10-09 sheet 两层级导航裁决：子 Agent sheet 不做预览区内分屏，清单与详情
  // 是两级导航——chip 直达落在详情级，backToList=true（页头返回钮回清单，与清单
  // 点卡同款，避免死端）；mode 仍按宽窄屏赋值（文件 sheet 消费；子 Agent sheet
  // 忽略 mode 恒单栏）
  const openSubagent = useCallback(
    (id: string) => {
      setPreview({
        sheet: "subagents",
        open: { kind: "subagent", id },
        mode: isWideViewport() ? "split-h" : "fullscreen",
        backToList: true,
      });
    },
    [isWideViewport]
  );

  // 清单看板点卡进入（观察台 §二.5）：沿用当前 mode（往返保持，openFileFromList 同款）
  const openSubagentFromList = useCallback((id: string) => {
    setPreview((p) => ({
      sheet: "subagents",
      open: { kind: "subagent", id },
      mode: p?.mode ?? "fullscreen",
      backToList: true,
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

  // 页头面板入口（M3+）：宽屏默认 split-h（列表是行集，右侧整列纵向空间大）、
  // 窄屏 fullscreen（用户裁决：面板默认布局口径）。
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
            mode: isWideViewport() ? "split-h" : "fullscreen",
            backToList: false,
          }
    );
  }, [isWideViewport]);

  const closePreview = useCallback(() => setPreview(null), []);

  // 从二级内容（文件预览 / 子 agent 详情）返回所在 sheet 的看板态（保持当前布局 mode）
  const backToBoard = useCallback(() => {
    setPreview((p) => (p ? { ...p, open: { kind: "none" }, backToList: false } : p));
  }, []);

  // 布局切换（顶栏 sheet bar 唯一实例，S5 裁决——预览组件内部的切换器已删）
  const changePreviewMode = useCallback((mode: PreviewMode) => {
    setPreview((p) => (p ? { ...p, mode } : p));
  }, []);

  // 消息渲染器（观察台 T6 起迁至 ./message-render 共用）：renderBody 消费本页
  // 既有 openFile（正文路径点击 → 预览）与 files（链接化路径集）
  const { renderBody } = useMessageRenderers({ openFile, files });

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
    [limit, scrollAnchorIntoView]
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
      className="flex max-h-[55vh] shrink-0 flex-col gap-2 overflow-y-auto"
    >
      {approveMounted &&
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
      {!isSummary && <QuestionCard key={`question-${session.id}`} session={session} />}
      {/* 2026-10-08 子 agent chip：与 ModeBar 同一行（flex-wrap 兄弟），生命周期
          解耦——mode 视图失败/unsupported 时 chip 仍活，反之亦然（spec §6）。
          finished 不挂载（idle 不拦：claude 后台 agent 可在主会话 idle 时仍在跑）；
          其余状态的空态裁决交给数据（list 为 null/无 running 项 → 不渲染）。
          观察台 T5：chip 改纯展示，名单由上方 subagentList 单一数据源下发
          （拉取不再自持），点击直达详情对话框（§二.4）。 */}
      <div data-testid="status-row" className="flex flex-wrap items-center gap-2">
        <ModeBar key={`mode-${session.id}`} session={session} />
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
      {/* 总结模式提示行（Bug 8）：仅总结模式且有可折叠过程消息时出现 */}
      {isSummary && toggleableMessages.length > 0 && (
        <div
          data-testid="summary-banner"
          className="mb-2 flex items-center gap-2 rounded-lg bg-[var(--btnp)]/10 px-3 py-2 text-xs text-[var(--tx)]"
        >
          <span>总结模式 · 已折叠 {collapsedCount} 条过程消息</span>
          <span className="ml-auto flex shrink-0 gap-1">
            {collapsedCount > 0 && (
              <button
                type="button"
                data-testid="expand-all"
                onClick={expandAll}
                className="rounded-full bg-[var(--btnp)]/20 px-2 py-0.5 text-xs text-[var(--tx)]"
              >
                展开全部
              </button>
            )}
            {collapsedCount < toggleableMessages.length && (
              <button
                type="button"
                data-testid="collapse-all"
                onClick={collapseAll}
                className="rounded-full bg-[var(--cb)] px-2 py-0.5 text-xs text-[var(--mut)]"
              >
                收起全部
              </button>
            )}
          </span>
        </div>
      )}
      {loading && messages === null && (
        <p className="py-16 text-center text-sm text-[var(--mut)]">加载中…</p>
      )}
      {error && (
        <div className="py-16 text-center">
          <p data-testid="detail-error" className="mb-3 text-sm text-rose-600 dark:text-rose-400">
            {error.status === 404 ? "无法读取该会话内容" : "加载失败，请检查网络后重试"}
          </p>
          <button
            type="button"
            data-testid="detail-retry"
            onClick={retry}
            className="rounded-full bg-[var(--cb)] px-4 py-1.5 text-sm text-[var(--tx)]"
          >
            重试
          </button>
        </div>
      )}
      {messages !== null && !error && messages.length === 0 && (
        <p className="py-16 text-center text-sm text-[var(--mut)]">暂无消息</p>
      )}
      {/* 书签跳转：自动加载中提示（M5 P3-c）与到顶未命中 miss 提示（M3+） */}
      {jumpLoading && (
        <p
          data-testid="bookmark-jump-loading"
          className="mb-2 rounded-lg bg-[var(--btnp)]/10 px-3 py-2 text-xs text-[var(--tx)]"
        >
          正在加载更早消息以定位书签…
        </p>
      )}
      {bookmarkJumpMiss && !jumpLoading && (
        <p
          data-testid="bookmark-jump-miss"
          className="mb-2 rounded-lg bg-amber-500/10 px-3 py-2 text-xs text-amber-700 dark:text-amber-400"
        >
          已到最早消息，未找到该书签目标
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
            {loading ? "加载中…" : "加载更早消息"}
          </button>
        </div>
      )}
      {messages !== null && messages.length > 0 && (
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
                    title={`书签：${msgBookmark.preview}`}
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
        ▤ 文件
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
          ◉ 子 Agent ({runningCount})
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
          aria-label="关闭预览"
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
        entries={fileEntries}
        truncated={fileTruncated}
        scope={fileScope}
        loading={fileLoading}
        onScopeChange={setFileScope}
        onOpenFile={openFileFromList}
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
        暂无子 Agent
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
        onBack={preview.backToList ? backToBoard : undefined}
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
        选择左侧项目查看
      </div>
    )
  ) : null;

  return (
    <div className="flex h-dvh flex-col bg-[var(--pg)] text-[var(--tx)]">
      <header className="flex shrink-0 items-center gap-2 border-b border-[var(--cb)] px-3 py-2">
        <button
          type="button"
          data-testid="detail-back"
          aria-label="返回看板"
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
            {TOOL_LABELS[session.agentType]}
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
          aria-label="文件面板"
          aria-pressed={panelOpen}
          title={panelOpen ? "收起文件面板" : "打开文件面板"}
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
          aria-label="正文字号"
          title="正文字号"
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
          aria-label="刷新消息"
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
            {/* 无头回执卡（Task 8 / H7）：紧跟发送输入区之上（发完即见回执/取消钮）；
                key 带会话号防跨会话串卡（T1 活状态流同款防线） */}
            <HeadlessReceiptCard
              key={`headless-${session.id}`}
              session={session}
              turn={headlessTurn}
              onDismiss={() => setHeadlessTurn(null)}
            />
            <MessageComposer
              key={`composer-${session.id}`}
              session={session}
              onHeadlessTurn={setHeadlessTurn}
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
              send-info 拉取失败时组件自静默，不影响对话渲染；钥匙口径同上。
              Task 8：无头回执卡同层挂载（两分支一致），composer 经 onHeadlessTurn 上报 */}
          <HeadlessReceiptCard
            key={`headless-${session.id}`}
            session={session}
            turn={headlessTurn}
            onDismiss={() => setHeadlessTurn(null)}
          />
          <MessageComposer
            key={`composer-${session.id}`}
            session={session}
            onHeadlessTurn={setHeadlessTurn}
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
