// 通知历史铃铛 — 未读角标 + 历史面板（点击条目跳转对应会话）
import { useEffect, useRef, useState } from "react";
import { Bell } from "lucide-react";
import { toast } from "sonner";
import { useTranslation } from "react-i18next";
import { AGENT_BADGE, getAgentLabel } from "@/lib/agentBadge";
import { useSessionJump } from "@/hooks/useSessionJump";
import { cn } from "@/lib/utils";
import {
  clearHistory,
  clearRead,
  getHistory,
  getUnreadCount,
  markAllRead,
  type HistoryEntry,
} from "@/lib/notificationHistory";

function timeAgo(at: number, t: (k: string) => string): string {
  const mins = Math.floor((Date.now() - at) / 60000);
  if (mins < 1) return t("sessions.justNow");
  if (mins < 60) return `${mins}m`;
  return `${Math.floor(mins / 60)}h`;
}

export function NotificationBell() {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [entries, setEntries] = useState<HistoryEntry[]>([]);
  const [unread, setUnread] = useState(0);
  // #5b：清空全部的两步轻量确认（首次点击进入待确认态，3 秒内再点才执行）
  const [confirmingClear, setConfirmingClear] = useState(false);
  const confirmTimerRef = useRef<number | null>(null);
  const openRef = useRef(false);
  const { candidates, setCandidates, focus, focusHwnd } = useSessionJump();

  useEffect(() => {
    const refresh = () => {
      // 面板打开期间新到的通知直接视为已读（角标不堆积）；unread>0 守卫防事件循环
      if (openRef.current && getUnreadCount() > 0) {
        markAllRead();
        return; // markAllRead 会再次触发本事件
      }
      setEntries(getHistory());
      setUnread(getUnreadCount());
    };
    refresh();
    window.addEventListener("mam-history-updated", refresh);
    return () => {
      window.removeEventListener("mam-history-updated", refresh);
      if (confirmTimerRef.current) window.clearTimeout(confirmTimerRef.current);
    };
  }, []);

  const toggle = () => {
    const next = !open;
    openRef.current = next;
    setOpen(next);
    // 面板关闭时还原清空确认态，避免下次打开残留"确认清空？"
    if (!next) {
      if (confirmTimerRef.current) window.clearTimeout(confirmTimerRef.current);
      setConfirmingClear(false);
    }
    if (next) markAllRead();
  };

  // #5b：清空全部（两步确认：首次点击进入待确认态，3 秒内再点执行；超时自动还原）
  const handleClearAll = () => {
    if (!confirmingClear) {
      setConfirmingClear(true);
      confirmTimerRef.current = window.setTimeout(() => setConfirmingClear(false), 3000);
      return;
    }
    if (confirmTimerRef.current) window.clearTimeout(confirmTimerRef.current);
    setConfirmingClear(false);
    clearHistory();
    toast.success(t("notifications.clearedToast"));
  };

  // #5b：仅清理已读（未读保留，角标不变）；同时取消"清空全部"的待确认态，
  // 避免武装态残留导致后续误触发二次清空
  const handleClearRead = () => {
    if (confirmTimerRef.current) window.clearTimeout(confirmTimerRef.current);
    setConfirmingClear(false);
    clearRead();
    toast.success(t("notifications.readClearedToast"));
  };

  const jumpTo = async (e: HistoryEntry) => {
    try {
      await focus({
        pid: e.pid,
        id: e.sessionId,
        agentType: e.agentType,
        projectName: e.projectName,
        lastMessage: e.lastMessage,
        // HistoryEntry.title 是可选字段，原样透传即正确语义（issue #45：
        // 非 Session 来源，空值由后端 title_keys 过滤，无需归一）
        title: e.title,
        // review（PR #38）：历史条目带 form 却未透传 → 铃铛跳 App 会话进不了
        // 深链第一顺位（与 P2-5 同族、换入口）。form 仅由 session.form 写入
        form: e.form as "cli" | "app" | undefined,
      });
    } catch {
      toast.error(t("notifications.jumpFailed"));
    }
  };

  return (
    <div className="relative">
      <button
        className="hover:bg-accent relative rounded p-1.5 transition-colors"
        onClick={toggle}
        title={t("notifications.historyTitle")}
      >
        <Bell className="h-4 w-4" />
        {unread > 0 && (
          // 角标几何与「清除已完成」徽章（home.tsx）完全同款：位置/字号/字重一致，
          // 只用颜色区分语义（绿=可清除已完成、红=未读通知）——2026-11-04 用户裁决
          // 「泡泡外形样式统一」。
          <span className="absolute -top-1 -right-1 flex h-4 min-w-4 items-center justify-center rounded-full bg-red-500 px-1 text-[10px] leading-none font-semibold text-white">
            {unread > 99 ? "99+" : unread}
          </span>
        )}
      </button>
      {open && (
        <div className="bg-card absolute right-0 z-50 mt-2 w-96 rounded-lg border p-2 shadow-xl">
          <div className="mb-2 flex items-center justify-between gap-2 px-1">
            <p className="text-xs font-semibold">{t("notifications.historyTitle")}</p>
            <div className="flex items-center gap-1">
              <button
                className={cn(
                  "rounded px-1.5 py-0.5 text-[10px] transition-colors",
                  confirmingClear
                    ? "bg-destructive text-white"
                    : "text-muted-foreground hover:bg-accent hover:text-foreground"
                )}
                onClick={handleClearAll}
                title={t("notifications.clearAll")}
              >
                {confirmingClear ? t("notifications.clearAllConfirm") : t("notifications.clearAll")}
              </button>
              <button
                className="text-muted-foreground hover:bg-accent hover:text-foreground rounded px-1.5 py-0.5 text-[10px] transition-colors"
                onClick={handleClearRead}
                title={t("notifications.clearRead")}
              >
                {t("notifications.clearRead")}
              </button>
            </div>
          </div>
          {candidates && candidates.length > 0 && (
            <div className="mb-2 space-y-1 rounded border p-1.5">
              <p className="px-1 text-[10px] font-semibold">{t("sessions.pickWindow")}</p>
              {candidates.map((w) => (
                <button
                  key={w.hwnd}
                  className="hover:bg-accent w-full truncate rounded px-2 py-1 text-left text-[11px]"
                  title={w.title}
                  onClick={async () => {
                    setCandidates(null);
                    try {
                      await focusHwnd(w.hwnd);
                    } catch {
                      toast.error(t("notifications.jumpFailed"));
                    }
                  }}
                >
                  {w.title || t("sessions.untitledWindow")} — {w.process}
                </button>
              ))}
            </div>
          )}
          <div className="max-h-80 overflow-y-auto">
            {entries.length === 0 && (
              <p className="text-muted-foreground p-4 text-center text-xs">
                {t("notifications.historyEmpty")}
              </p>
            )}
            {entries.map((e) => {
              const badge = AGENT_BADGE[e.agentType];
              return (
                <button
                  key={e.at + e.sessionId}
                  className="hover:bg-accent/50 flex w-full items-center gap-2 rounded px-2 py-1.5 text-left"
                  onClick={() => jumpTo(e)}
                >
                  {badge && (
                    <span
                      className={`inline-flex shrink-0 items-center gap-1 rounded border px-1.5 py-0.5 text-[9px] ${badge.className}`}
                    >
                      <badge.Icon className="h-2.5 w-2.5" />
                      {getAgentLabel(e.agentType, e.form)}
                    </span>
                  )}
                  <span className="min-w-0 flex-1 truncate text-[11px]">
                    {e.projectName} · {t(`sessions.statusLabels.${e.status}`, e.status)} —{" "}
                    {e.lastMessage || t("sessions.noMessage")}
                  </span>
                  <span className="text-muted-foreground shrink-0 text-[10px]">
                    {timeAgo(e.at, t)}
                  </span>
                </button>
              );
            })}
          </div>
        </div>
      )}
    </div>
  );
}
