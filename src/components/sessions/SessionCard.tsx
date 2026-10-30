import { useTranslation } from "react-i18next";
import { useQueryClient } from "@tanstack/react-query";
import { Cpu, Clock, Bot, ChevronRight, X, ArrowLeftRight, ExternalLink } from "lucide-react";
import { invoke } from "@tauri-apps/api/core";
import { cn } from "@/lib/utils";
import { toast } from "sonner";
import { Card } from "@/components/ui/card";
import { StatusLight } from "@/components/sessions/StatusLight";
import { useSessionJump } from "@/hooks/useSessionJump";
import { AGENT_BADGE, getAgentLabel } from "@/lib/agentBadge";
import { sessionTitleOrUndefined } from "@/lib/sessionTitle";
import { RESUME_CAPABLE_TOOLS, sessionOpen } from "@/lib/api/session";
import type { Session } from "@/types/session";

function formatRuntime(lastActivityAt: string, t: (key: string) => string): string {
  if (!lastActivityAt || lastActivityAt === "Unknown") return "--";
  // 尝试解析 ISO 时间戳或 Claude 的时间格式
  const date = new Date(lastActivityAt);
  if (isNaN(date.getTime())) return lastActivityAt.slice(0, 19);
  const diff = Date.now() - date.getTime();
  if (diff < 0) return t("sessions.justNow");
  const mins = Math.floor(diff / 60000);
  if (mins < 1) return t("sessions.justNow");
  if (mins < 60) return `${mins}m`;
  const hours = Math.floor(mins / 60);
  return `${hours}h${mins % 60}m`;
}

// 全角/CJK 字符按 1.5 单位计入显示预算（10 个中文 = 15 单位）
const CJK_CHAR =
  /[\u2e80-\u303f\u31c0-\u31ef\u3400-\u4dbf\u4e00-\u9fff\uf900-\ufaff\ufe30-\ufe4f\uff00-\uffef]/;

/**
 * 项目名显示预算（卡片顶行）：中文封顶 10 字、英文封顶 15 字符（按 15 单位宽度
 * 预算折算，CJK 每字 1.5 单位），超出截断加省略号；完整名走 title 悬停提示。
 * CSS truncate 仅作兜底——预算保证不同卡宽下截断点可预期
 */
export function truncateProjectName(name: string, budget = 15): string {
  const chars = [...name];
  let cost = 0;
  for (let i = 0; i < chars.length; i++) {
    cost += CJK_CHAR.test(chars[i]) ? 1.5 : 1;
    if (cost > budget) return chars.slice(0, i).join("") + "…";
  }
  return name;
}

export function SessionCard({
  session,
  pairingAmbiguous = false,
}: {
  session: Session;
  /** 同工具同项目双开：卡片信息（状态/最后消息）可能互换的提示角标（issue #48） */
  pairingAmbiguous?: boolean;
}) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const badge = AGENT_BADGE[session.agentType];
  const Icon = badge.Icon;
  // 跳转共享逻辑（歧义候选窗口由 hook 状态承载，命中多个窗口时弹出选择器）
  const { candidates, setCandidates, focus, focusHwnd } = useSessionJump();

  // 手动关闭卡（X）：不触发卡片跳转。全部形态（App/CLI）均可关闭。
  // 未读卡 = 标记已读（mark_session_read，spec W4 已读信号 2）；
  // 非未读卡（黄/红/绿）= 暂离不提示（dismiss_session_card，T2）——写入进程内
  // dismiss 集合从看板与宠物隐藏，同一会话状态变化后自然重现
  const handleClose = async (e: React.MouseEvent) => {
    e.stopPropagation(); // 不触发卡片跳转
    try {
      if (session.unread) {
        await invoke("mark_session_read", {
          agentType: session.agentType,
          sessionId: session.id,
        });
      } else {
        await invoke("dismiss_session_card", {
          agentType: session.agentType,
          sessionId: session.id,
          status: session.status,
        });
      }
    } catch (err) {
      console.error("session card close failed:", err);
      return;
    }
    // F5：命令已同步补扫，失效后立刻读到不含该卡的新快照（失败不失效，不伪装成功）
    void queryClient.invalidateQueries({ queryKey: ["sessions"] });
  };

  const handleClick = async () => {
    if (!session.jumpSupported) {
      toast.info(t("sessions.jumpUnsupported"));
      return;
    }
    try {
      await focus({
        pid: session.pid,
        id: session.id,
        agentType: session.agentType,
        projectName: session.projectName,
        lastMessage: session.lastMessage ?? undefined,
        title: sessionTitleOrUndefined(session),
        unread: session.unread, // 歧义选择器点选成功后回标已读用（spec W4 已读信号 1）
        form: session.form, // review M3：CLI 会话 APP 级保底激活时的 UX 提示依据
      });
    } catch (e) {
      toast.error(t("sessions.jumpFailed", { error: e }));
    }
  };

  // R5 一键 resume（Task 11「在电脑上打开」）：无 cwd / 无映射 → 禁用 + 原因
  //（title 悬停提示）；出手成功 toast「正在电脑上打开终端…」，失败 toast 后端
  // 转译后的中文文案（命令侧哨兵串已转译）
  const resumeReason =
    !session.projectPath || !session.projectPath.trim()
      ? t("resume.noCwd")
      : !RESUME_CAPABLE_TOOLS.has(session.agentType)
        ? t("resume.noCmd")
        : null;
  const handleResumeOpen = async (e: React.MouseEvent) => {
    e.stopPropagation(); // 不触发卡片跳转
    try {
      await sessionOpen(session.id);
      toast.info(t("resume.opening"));
    } catch (err) {
      toast.error(t("resume.failed", { error: err }));
    }
  };

  return (
    <>
      <Card
        className={cn(
          // 行距 8px（用户裁决 2026-10-27）：Card 基类自带 flex flex-col gap-6（24px）
          // ——三行（标题/会话名/正文）之间的空行就是它垫出来的；改为 gap-2 统一 8px，
          // 行内 mb 全部移除，避免 margin 叠在 gap 上
          "group hover:bg-accent/50 @container relative flex cursor-pointer flex-col gap-2 border p-3 transition-colors",
          session.status === "waiting" && "border-red-500/40",
          !session.jumpSupported && "cursor-default opacity-80"
        )}
        onClick={handleClick}
        title={session.jumpSupported ? t("sessions.jumpToTerminal") : t("sessions.jumpUnsupported")}
      >
        {/* 第一行：工具标签 + 项目目录名（自然宽度优先完整显示）+ 配对歧义图标 + 分支 | 状态指示区 | 关闭 X 最右。
            会话标题主要用于跳转匹配定位而非阅读，移到第二行独立展示（灰色 mono 次文字，
            悬停 tooltip 看全文，有整行空间不再需要容器查询隐藏——实机反馈轮 2026-10-27 #1）。
            项目名 shrink-0 **绝不参与收缩**（JS 预算已限 15 字，max-w-[12rem] 只作病理宽度安全帽）：
            实测 flex-shrink 1:5 分摊仍会让短名缩 2~3px，而 truncate 溢出 1px 就把尾字换成
            省略号（「tuvis」→「tu…」）；分支改 flex-1 min-w-0 吸收剩余空间、先截。
            首组 overflow-hidden + 右侧 pr-1 保证任何溢出都裁在状态点之前，永不压到状态灯下 */}
        <div className="flex items-center justify-between gap-2">
          <div className="flex min-w-0 flex-1 items-center gap-2 overflow-hidden pr-1">
            <span
              className={cn(
                "inline-flex shrink-0 items-center gap-1 rounded border px-1.5 py-0.5 text-xs font-semibold",
                badge.className
              )}
            >
              <Icon className="h-3 w-3" />
              {getAgentLabel(session.agentType, session.form)}
            </span>
            <span
              className="max-w-[12rem] shrink-0 truncate text-sm font-medium"
              title={session.projectName}
            >
              {truncateProjectName(session.projectName)}
            </span>
            {pairingAmbiguous && (
              <span
                className="shrink-0 text-amber-600 dark:text-amber-400"
                title={t("sessions.pairingAmbiguous")}
                aria-label={t("sessions.pairingAmbiguous")}
              >
                <ArrowLeftRight className="h-3 w-3" />
              </span>
            )}
            {session.gitBranch && (
              <span
                className="text-muted-foreground min-w-0 flex-1 truncate font-mono text-xs"
                title={session.gitBranch}
              >
                {session.gitBranch}
              </span>
            )}
          </div>
          {/* 状态指示区：状态灯与未读合并为单一指示（未读时绿点加光环，不再渲染独立小绿点） */}
          <div className="flex shrink-0 items-center gap-1.5">
            <div className="relative" title={session.unread ? t("sessions.unread") : undefined}>
              <StatusLight status={session.status} size="sm" />
              {session.unread && (
                <span
                  className="pointer-events-none absolute -inset-1 rounded-full ring-2 ring-emerald-400/80"
                  aria-label={t("sessions.unread")}
                />
              )}
            </div>
            <button
              onClick={handleClose}
              className="text-muted-foreground hover:bg-muted hover:text-foreground rounded p-0.5"
              title={session.unread ? t("sessions.markRead") : t("sessions.dismissCard")}
              aria-label={session.unread ? t("sessions.markRead") : t("sessions.dismissCard")}
            >
              <X className="h-3 w-3" />
            </button>
          </div>
        </div>

        {/* 第二行：会话标题尾巴——独立整行展示，截断加省略号 + title 全文悬停。
            二轮实机反馈 ①（2026-10-27）：加粗（font-semibold）拉近（mb-2→mb-1）——
            会话名是卡片的主要可读内容，预览行距同步收紧（mb-2→mb-1.5），三行更紧凑 */}
        {(session.title || session.id || "").trim() !== "" && (
          <div
            className="text-muted-foreground truncate font-mono text-xs font-semibold"
            title={session.title || session.id.slice(0, 8)}
          >
            {session.title || session.id.slice(0, 8)}
          </div>
        )}

        {/* 中间：最后消息预览（二轮实机反馈 ①：mb-2→mb-1.5，其余不动） */}
        <p className="text-muted-foreground line-clamp-2 min-h-[2.5rem] text-xs">
          {session.lastMessage || t("sessions.noMessage")}
        </p>

        {/* 底部：CPU + PID + 运行时长 */}
        <div className="text-muted-foreground flex items-center gap-3 text-xs">
          <span className="flex items-center gap-1">
            <Cpu className="h-3 w-3" />
            {session.cpuUsage.toFixed(1)}%
          </span>
          <span className="flex items-center gap-1">
            <Clock className="h-3 w-3" />
            {formatRuntime(session.lastActivityAt, t)}
          </span>
          {session.activeSubagentCount > 0 && (
            <span className="flex items-center gap-1">
              <Bot className="h-3 w-3" />
              {t("sessions.subagents", { n: session.activeSubagentCount })}
            </span>
          )}
          {/* R5 一键 resume（Task 11）：无 cwd / 无映射工具禁用（title 给原因——
              评审 M2：去掉 disabled:pointer-events-none，悬停可见禁用原因；
              disabled 按钮本就不派发 click，卡片 onClick 不会被穿透触发） */}
          <button
            type="button"
            data-testid="session-open"
            disabled={resumeReason !== null}
            title={resumeReason ?? t("resume.open")}
            aria-label={resumeReason ?? t("resume.open")}
            onClick={handleResumeOpen}
            className="hover:bg-muted hover:text-foreground rounded p-0.5 disabled:opacity-40"
          >
            <ExternalLink className="h-3 w-3" />
          </button>
          {session.jumpSupported && (
            <ChevronRight className="ml-auto h-3 w-3 opacity-0 transition-opacity group-hover:opacity-50" />
          )}
        </div>
      </Card>
      {/* 窗口选择器：跳转歧义时由用户点选目标窗口 */}
      {candidates && (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
          onClick={() => setCandidates(null)}
        >
          <div
            className="workspace-glass w-96 rounded-lg border p-4 shadow-xl"
            onClick={(e) => e.stopPropagation()}
          >
            <p className="mb-3 text-sm font-medium">{t("sessions.pickWindow")}</p>
            <div className="flex flex-col gap-2">
              {candidates.map((w) => (
                <button
                  key={w.hwnd}
                  className={cn(
                    "hover:bg-accent truncate rounded border px-3 py-2 text-left text-xs",
                    // 零分候选（多为其他工具无人认领的窗口）视觉弱化；排序保持后端降序，
                    // 不做过滤——评分素材缺失的真目标可能恰好 0 分，弱化无误杀风险
                    w.score === 0 && w.uiaPrefix === 0 && "opacity-60"
                  )}
                  onClick={async () => {
                    setCandidates(null);
                    try {
                      await focusHwnd(w.hwnd);
                    } catch (e) {
                      toast.error(t("sessions.jumpFailed", { error: e }));
                    }
                  }}
                  title={w.title}
                >
                  {w.title || t("sessions.untitledWindow")} — {w.process}
                </button>
              ))}
            </div>
          </div>
        </div>
      )}
    </>
  );
}
