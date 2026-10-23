import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { toast } from "sonner";
import { ArrowDownToLine, ArrowUpToLine, ChevronDown, ChevronRight } from "lucide-react";
import i18n from "@/i18n";
import {
  ApiError,
  deleteArchivedSession,
  fetchSessionMessages,
  sessionOpen,
  unhideSession,
  type ArchivedSession,
  type SessionMessage,
} from "./api";
// 复用既有导出（勿自造）：statusLabel 状态显示名（M5 Task 7 键化后现取）/ formatRelativeTime 相对时间
import { formatRelativeTime, statusLabel } from "./board-logic";
import { chipLabel } from "./archive-logic";
import { collapsedLabel, isProcessKind } from "./message-fold";
import { JUMP_SHOW_THRESHOLD_PX } from "./SessionDetail";
import { resumeUnavailableReason, resumeUnsupportedReason } from "./resume-gate";

/** 归档详情单次拉取条数（与活会话详情默认 limit 同标尺；truncated 即有更早内容未载） */
const ARCHIVE_PAGE_LIMIT = 200;
/** limit 翻倍上限（与活会话 load-more 同标尺）：到达后不再提供「加载更早消息」 */
const MAX_LIMIT = 1000;

/** 打开失败文案分诊（评审 Minor：归档语境哨兵引导）。哨兵串与后端 api.rs 的
 *  Err 哨兵对应（404 载荷 data.error）：no_session=活/档双未命中（归档记录可能
 *  已被移除/清空，引导刷新）；no_resume_command=工具未入命令表（复用 resume-gate
 *  门的文案）；其余透传后端原文。文案走 mobile.archive.* 双语键（M5；reason 段
 *  含后端/resume-gate 原文，its 本身归 ts 逻辑文件侧） */
function openFailureCopy(error: string | null | undefined): string {
  if (error === "no_session") return i18n.t("mobile.archive.gone");
  if (error === "no_resume_command")
    return i18n.t("mobile.archive.openFailed", { reason: resumeUnsupportedReason() });
  return i18n.t("mobile.archive.openFailed", {
    reason: error ?? i18n.t("mobile.archive.unknownError"),
  });
}

/** 归档详情页（spec §7.3）：只读消息（/session-messages 按 id 直读文件，零后端改动）
 *  + 唯一动作「在桌面端打开」（复用 sessionOpen）+ 次要动作「从归档移除」。
 *  交互对齐活会话（2026-09-20 体验批）：独立滚动容器 + 进入落底 + 到顶/到底浮动钮
 *  + 总结模式折叠（归档恒为死会话：过程消息默认折叠、只直显最后 assistant 总结）
 *  + 底部动作区（打开在上、移除在下）。 */
export default function ArchiveDetail({
  session,
  onBack,
  onActivated,
}: {
  session: ArchivedSession;
  onBack: () => void;
  onActivated: () => void;
}) {
  const [messages, setMessages] = useState<SessionMessage[] | null>(null);
  const [truncated, setTruncated] = useState(false);
  // 加载更早消息（体验批二，活会话同款）：limit 翻倍整页重拉（200→400→…→1000）
  const [limit, setLimit] = useState(ARCHIVE_PAGE_LIMIT);
  const [contentError, setContentError] = useState(false);
  const [opening, setOpening] = useState(false);
  const [openError, setOpenError] = useState<string | null>(null);
  const [confirmRemove, setConfirmRemove] = useState(false);
  // 移除失败行内反馈（终审 Finding 3：Promise 拒绝无 .catch → 按钮无反应 +
  // unhandled rejection）；不复用 contentError/openError（语义不同），下次点击清除
  const [removeError, setRemoveError] = useState(false);
  // 移回看板失败行内反馈（软归档活会话的动作）；下次点击清除
  const [unhideError, setUnhideError] = useState(false);
  // 折叠覆盖表：seq → 强制折叠/展开（活会话同款语义；「收起」= 清空回默认折叠态）
  const [expandedOverride, setExpandedOverride] = useState<Map<number, boolean>>(new Map());
  // 浮动钮显隐（onScroll 驱动；阈值与活会话「跳到最新」同源）
  const [showJumpTop, setShowJumpTop] = useState(false);
  const [showJumpBottom, setShowJumpBottom] = useState(false);
  const areaRef = useRef<HTMLDivElement>(null);
  // 加载更早消息的滚动锚：重拉后把视图锚回原顶部消息（不把用户甩到底部）
  const anchorSeqRef = useRef<number | null>(null);
  // 相对时长基准时钟（react-hooks/purity 禁渲染期调 Date.now，同 Board 惯例）：
  // 挂载时取一次快照（归档详情为静态只读页，无需定时走动）
  const [now] = useState(() => Date.now());
  const reason = resumeUnavailableReason(session);
  // 最后状态显示名（spec §7.3 顶部信息行要求）：lastStatus 小写串与 statusLabel
  // 六键（waiting/processing/thinking/compacting/idle/finished）精确对齐——Rust 端
  // format!("{:?}", status).to_lowercase() 产物一致；未知串回退原文（M5 Task 7：
  // 经 board-logic.statusLabel 渲染期现取 i18n，中英随语言切换）
  const lastStatusLabel = statusLabel(session.lastStatus);

  useEffect(() => {
    let alive = true;
    fetchSessionMessages(session.agentType, session.sessionId, limit)
      .then((p) => {
        if (!alive) return;
        setMessages(p.messages);
        setTruncated(p.truncated);
      })
      .catch(() => {
        if (alive) setContentError(true);
      });
    return () => {
      alive = false;
    };
  }, [session.agentType, session.sessionId, limit]);

  // 加载更早消息（活会话同款）：limit 翻倍整页重拉，锚定原顶部消息不甩屏
  const hasLoadMore =
    messages !== null &&
    !contentError &&
    (messages.length >= limit || truncated) &&
    limit < MAX_LIMIT;

  const loadMore = useCallback(() => {
    const first = messages?.[0];
    anchorSeqRef.current = first ? first.seq : null;
    setLimit((l) => Math.min(l * 2, MAX_LIMIT));
  }, [messages]);

  // 折叠判定（活会话总结模式同款，归档恒总结模式）：过程消息可折叠；
  // assistant 只直显最后一条总结（更早 assistant 折叠）
  const lastAssistantSeq = useMemo(() => {
    if (messages === null) return null;
    let last: number | null = null;
    for (const m of messages) {
      if (m.kind === "assistant") last = m.seq;
    }
    return last;
  }, [messages]);

  const isToggleable = useCallback(
    (m: SessionMessage) =>
      isProcessKind(m.kind) || (m.kind === "assistant" && m.seq !== lastAssistantSeq),
    [lastAssistantSeq]
  );

  const isCollapsed = useCallback(
    (m: SessionMessage) => {
      const forced = expandedOverride.get(m.seq);
      return forced !== undefined ? forced : isToggleable(m);
    },
    [expandedOverride, isToggleable]
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

  const toggleableMessages = useMemo(
    () => (messages !== null ? messages.filter((m) => isToggleable(m)) : []),
    [messages, isToggleable]
  );
  const collapsedCount = toggleableMessages.filter((m) => isCollapsed(m)).length;

  const expandAll = useCallback(() => {
    setExpandedOverride((prev) => {
      const next = new Map(prev);
      // 覆盖表值语义 = 强制折叠与否（false = 强制展开，见 isCollapsed）
      for (const m of toggleableMessages) next.set(m.seq, false);
      return next;
    });
  }, [toggleableMessages]);

  const collapseAll = useCallback(() => setExpandedOverride(new Map()), []);

  // 浮动钮显隐（onScroll 驱动；活会话同款阈值——贴底/贴顶阅读时是纯噪声）
  const handleAreaScroll = useCallback(() => {
    const el = areaRef.current;
    if (!el) return;
    setShowJumpTop(el.scrollTop > JUMP_SHOW_THRESHOLD_PX);
    setShowJumpBottom(el.scrollHeight - el.scrollTop - el.clientHeight > JUMP_SHOW_THRESHOLD_PX);
  }, []);

  const jumpToTop = useCallback(() => {
    const el = areaRef.current;
    if (!el) return;
    el.scrollTop = 0;
    setShowJumpTop(false);
  }, []);

  const jumpToBottom = useCallback(() => {
    const el = areaRef.current;
    if (!el) return;
    el.scrollTop = el.scrollHeight;
    setShowJumpBottom(false);
  }, []);

  // 滚动定位：有锚（加载更早后）锚回原顶部消息；无锚（首次进入）落底
  useEffect(() => {
    const el = areaRef.current;
    if (messages === null || el === null) return;
    if (anchorSeqRef.current !== null) {
      const target = el.querySelector(`[data-testid="msg-${anchorSeqRef.current}"]`);
      anchorSeqRef.current = null;
      if (target) {
        target.scrollIntoView({ block: "start" });
        return;
      }
    }
    el.scrollTop = el.scrollHeight;
  }, [messages]);

  const handleOpen = async () => {
    setOpening(true);
    setOpenError(null);
    try {
      const r = await sessionOpen(session.sessionId);
      if (r.status === "opening") {
        // T3 未信任预检（评审修复：移动端用户可见承载）：电脑侧命中未信任目录——
        // claude 将弹终端信任提示，手机注入答不了，须人工去主机应答否则会话挂起。
        // 文案与 Rust TRUST_PROMPT_REMINDER 同义（M5 双语键）
        if (r.trustPromptExpected) {
          toast(i18n.t("mobile.archive.trustPrompt"));
        }
        onActivated(); // 乐观回看板：活板 3s 内出卡，归档条目下次拉取自然消失
        return;
      }
      setOpenError(openFailureCopy(r.error));
    } catch (e) {
      // 404 哨兵在响应体 data.error（api.ts 解析进 ApiError.data）——命中哨兵走
      // 归档语境文案，其余（网络异常等）维持原 String(e) 形态
      const code = e instanceof ApiError ? (e.data?.error as string | undefined) : undefined;
      setOpenError(
        code !== undefined
          ? openFailureCopy(code)
          : i18n.t("mobile.archive.openFailed", { reason: String(e) })
      );
    } finally {
      setOpening(false);
    }
  };

  return (
    <div className="mx-auto flex h-dvh max-w-3xl flex-col px-4">
      <header className="flex shrink-0 items-center gap-2 pt-4">
        <button
          type="button"
          onClick={onBack}
          className="text-sm text-[var(--mut)]"
          aria-label={i18n.t("mobile.archive.backToListAria")}
        >
          {i18n.t("mobile.archive.back")}
        </button>
        <h1 className="text-lg font-semibold">{session.projectName}</h1>
      </header>
      <p className="mt-1 shrink-0 text-xs text-[var(--mut)]">
        {chipLabel(session.agentType)} · {session.projectPath} · {lastStatusLabel} ·{" "}
        {formatRelativeTime(session.lastSeenAt, now)}
        {i18n.t(session.hiddenAlive ? "mobile.archive.alive" : "mobile.archive.ended")}{" "}
        {session.hiddenAlive && (
          <span
            data-testid="hidden-alive-badge"
            className="rounded-full bg-green-100 px-1.5 py-0.5 text-[10px] text-green-700 dark:bg-green-900/40 dark:text-green-300"
          >
            {i18n.t("mobile.archive.aliveHidden")}
          </span>
        )}
      </p>

      {/* 窗口截断诚实提示（到顶 ≠ 全会话顶：更早内容未加载） */}
      {truncated && messages !== null && (
        <p
          data-testid="archive-truncated"
          className="mt-2 shrink-0 rounded-lg bg-amber-50 px-3 py-1.5 text-center text-xs text-amber-700 dark:bg-amber-950/40 dark:text-amber-400"
        >
          {i18n.t("mobile.archive.truncated", { count: ARCHIVE_PAGE_LIMIT })}
        </p>
      )}

      {/* 折叠提示行（活会话 Bug 8 同款）：有可折叠内容即出现（不看当前折叠数——
          全部展开后「收起」仍要可达），计数如实显示 */}
      {toggleableMessages.length > 0 && (
        <div
          data-testid="archive-fold-banner"
          className="mt-2 flex shrink-0 items-center justify-between rounded-lg bg-[var(--cbg)] px-3 py-1.5 text-xs text-[var(--mut)]"
        >
          <span>{i18n.t("mobile.archive.foldedCount", { count: collapsedCount })}</span>
          <span className="flex gap-2">
            <button
              type="button"
              data-testid="expand-all"
              className="underline"
              onClick={expandAll}
            >
              {i18n.t("mobile.archive.expandAll")}
            </button>
            <button
              type="button"
              data-testid="collapse-all"
              className="underline"
              onClick={collapseAll}
            >
              {i18n.t("mobile.archive.collapse")}
            </button>
          </span>
        </div>
      )}

      {/* 消息滚动区（活会话 MessageScrollArea 同款形态）：wrapper 持 relative，
          浮动按钮放 wrapper 才不随内容滚走 */}
      <div className="relative mt-2 min-h-0 flex-1">
        <div
          ref={areaRef}
          data-testid="message-area"
          className="h-full overflow-y-auto pr-1"
          onScroll={handleAreaScroll}
        >
          {contentError && (
            <p className="py-6 text-center text-sm text-[var(--mut)]">
              {i18n.t("mobile.archive.contentUnreadable")}
            </p>
          )}
          {messages === null && !contentError && (
            <p className="py-6 text-center text-sm text-[var(--mut)]">
              {i18n.t("mobile.detail.loading")}
            </p>
          )}
          {messages?.length === 0 && (
            <p className="py-6 text-center text-sm text-[var(--mut)]">
              {i18n.t("mobile.archive.noMessages")}
            </p>
          )}
          {/* 加载更早消息（体验批二，活会话同款）：条数达 limit 或 truncated 即提供 */}
          {hasLoadMore && (
            <button
              type="button"
              data-testid="load-more"
              onClick={loadMore}
              className="mx-auto mb-2 block rounded-lg border border-[var(--cb)] px-3 py-1.5 text-xs text-[var(--mut)] hover:bg-[var(--cbg)] dark:hover:bg-[var(--btnp)]"
            >
              {i18n.t("mobile.detail.loadMore")}
            </button>
          )}
          <ul className="flex flex-col gap-2 pb-2">
            {messages?.map((m) => {
              const toggleable = isToggleable(m);
              const collapsed = isCollapsed(m);
              return (
                <li
                  key={m.seq}
                  data-testid={`msg-${m.seq}`}
                  data-kind={m.kind}
                  className="rounded-lg bg-[var(--cbg)] px-3 py-2 text-sm"
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
                      {!collapsed && (
                        <div className="mt-1 break-words whitespace-pre-wrap">{m.content}</div>
                      )}
                    </>
                  ) : (
                    <div className="break-words whitespace-pre-wrap">
                      <span className="mr-2 text-xs text-[var(--mut)]">{m.role}</span>
                      {m.content}
                    </div>
                  )}
                </li>
              );
            })}
          </ul>
        </div>
        {showJumpTop && (
          <button
            type="button"
            data-testid="jump-to-top"
            aria-label={i18n.t("mobile.detail.jumpTop")}
            title={i18n.t("mobile.detail.jumpTop")}
            onClick={jumpToTop}
            className="absolute top-2 right-2 z-10 rounded-full border border-[var(--cb)] bg-[var(--cbg)] p-2 text-[var(--mut)] shadow-md hover:bg-[var(--cbg)] dark:hover:bg-[var(--btnp)]"
          >
            <ArrowUpToLine size={16} />
          </button>
        )}
        {showJumpBottom && (
          <button
            type="button"
            data-testid="jump-to-bottom"
            aria-label={i18n.t("mobile.archive.jumpBottom")}
            title={i18n.t("mobile.archive.jumpBottom")}
            onClick={jumpToBottom}
            className="absolute right-2 bottom-2 z-10 rounded-full border border-[var(--cb)] bg-[var(--cbg)] p-2 text-[var(--mut)] shadow-md hover:bg-[var(--cbg)] dark:hover:bg-[var(--btnp)]"
          >
            <ArrowDownToLine size={16} />
          </button>
        )}
      </div>

      {/* 底部动作区（体验批二）：软归档活会话=「移回看板」；真死归档=「打开」（主，上）
          +「移除」（次，下） */}
      <div className="shrink-0 px-1 pt-3">
        {session.hiddenAlive ? (
          <>
            <button
              type="button"
              data-testid="unhide-session"
              onClick={() => {
                setUnhideError(false);
                unhideSession(session.sessionId)
                  .then(onBack)
                  .catch(() => setUnhideError(true));
              }}
              className="w-full rounded-lg bg-[var(--btnp)] px-3 py-2 text-sm text-[var(--btnpt)]"
            >
              {i18n.t("mobile.archive.unhide")}
            </button>
            {unhideError && (
              <p className="mt-1 text-center text-xs text-red-600">
                {i18n.t("mobile.archive.manageError")}
              </p>
            )}
          </>
        ) : (
          <>
            <button
              type="button"
              data-testid="session-open"
              disabled={reason !== null || opening}
              title={reason ?? undefined}
              onClick={handleOpen}
              className="w-full rounded-lg bg-green-700 px-3 py-2 text-sm text-white enabled:hover:bg-green-800 disabled:opacity-50"
            >
              {i18n.t(opening ? "mobile.archive.opening" : "mobile.archive.open")}
            </button>
            {reason && <p className="mt-1 text-center text-xs text-[var(--mut)]">{reason}</p>}
            {openError && (
              <p data-testid="session-open-error" className="mt-1 text-center text-xs text-red-600">
                {openError}
              </p>
            )}
          </>
        )}
      </div>

      {!session.hiddenAlive && (
        <div className="shrink-0 pt-2 pb-6">
          {confirmRemove ? (
            <span className="flex gap-2">
              <button
                type="button"
                data-testid="archive-remove-confirm"
                className="flex-1 rounded-lg border border-red-300 px-3 py-1.5 text-xs text-red-600"
                onClick={() => {
                  setRemoveError(false);
                  deleteArchivedSession(session.sessionId)
                    .then(onBack)
                    .catch(() => setRemoveError(true));
                }}
              >
                {i18n.t("mobile.archive.removeConfirm")}
              </button>
              <button
                type="button"
                className="flex-1 rounded-lg border border-[var(--cb)] px-3 py-1.5 text-xs"
                onClick={() => setConfirmRemove(false)}
              >
                {i18n.t("mobile.archive.cancel")}
              </button>
            </span>
          ) : (
            <button
              type="button"
              data-testid="archive-remove"
              className="w-full rounded-lg border border-red-300 px-3 py-2 text-sm text-red-600 hover:bg-red-50 dark:border-red-900 dark:hover:bg-red-950/40"
              onClick={() => setConfirmRemove(true)}
            >
              {i18n.t("mobile.archive.remove")}
            </button>
          )}
          {removeError && (
            <p data-testid="archive-remove-error" className="mt-1 text-center text-xs text-red-600">
              {i18n.t("mobile.archive.manageError")}
            </p>
          )}
        </div>
      )}
    </div>
  );
}
