import { useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { LogicalSize, PhysicalPosition } from "@tauri-apps/api/dpi";
import { useTranslation } from "react-i18next";
import { X } from "lucide-react";
import { useSessionJump } from "@/hooks/useSessionJump";
import { AGENT_BADGE } from "@/lib/agentBadge";
import { cn } from "@/lib/utils";
import { Button } from "@/components/ui/button";

interface NotificationPayload {
  agentType: string;
  agentLabel: string;
  projectName: string;
  statusColor: string;
  status: string;
  lastMessage: string;
  title: string;
  pid: number;
  sessionId: string;
  /** 2A 操作卡标记：Rust 侧 Option 字段（camelCase 直达），会话卡不带 */
  action?: string;
  /** 操作卡要复制的命令文本（如 "/hooks"）——卡面展示与复制同源 */
  actionCommand?: string;
}

// 与 Rust 侧 commands::notification::CODEX_TRUST_HINT_ACTION 同值。本页面在
// 独立通知窗中加载，payload 类型即本地复刻（不引共享模块），常量同口径双端各持
const CODEX_TRUST_HINT_ACTION = "codexTrustHint";

// 自定义通知浮窗页面 — 由 Rust 侧 show_notification_window 创建的独立小窗加载，
// 收到 notification:new 事件后显示，10 秒无交互自动隐藏（悬停保留）
export default function NotificationPage() {
  const [payload, setPayload] = useState<NotificationPayload | null>(null);
  // 跳转共享逻辑（歧义候选窗口内联渲染）
  const { candidates, setCandidates, focus, focusHwnd } = useSessionJump();
  const { t } = useTranslation();
  const timerRef = useRef<number | null>(null);

  // 自动隐藏计时器（组件级，参数化时长）：通知卡片与候选列表复用
  const armTimer = (ms: number) => {
    if (timerRef.current) window.clearTimeout(timerRef.current);
    timerRef.current = window.setTimeout(() => getCurrentWindow().hide(), ms);
  };

  // 候选列表动态高度：N 个候选按 60+N*34+16 计算，上限 400（超出内部滚动）；null 还原 110。
  // 底边锚定：窗口 y 是按 110 高定位的，增高时同步上移，避免底边溢出屏幕/任务栏
  const applyHeight = async (count: number | null) => {
    const h = count === null ? 110 : Math.min(60 + count * 34 + 16, 400);
    try {
      const win = getCurrentWindow();
      const scale = await win.scaleFactor();
      const pos = await win.outerPosition();
      await win.setSize(new LogicalSize(360, h));
      await win.setPosition(new PhysicalPosition(pos.x, Math.round(pos.y - (h - 110) * scale)));
    } catch {
      // 非 Tauri 环境忽略
    }
  };

  useEffect(() => {
    const win = getCurrentWindow();
    // [Bug A 取证插桩·临时] 挂载行：浮窗页每次（重）加载记一条（H3 窗体重载判别）
    console.debug("[notifwin] mounted", win.label, Date.now());
    let unlisten: (() => void) | null = null;
    // 监听必须限定本窗口：不指定 target 时监听器目标为 Any，会收到所有槽位窗口的
    // notification:new（emit_to 定向发送被 Any 监听器全收），导致每个浮窗都显示最后一条内容
    listen<NotificationPayload>(
      "notification:new",
      (e) => {
        // [Bug A 取证插桩·临时] 出现行：每次实际弹出示必有一条（与 Rust cmd 行、
        // 前端 [notif] SHOW_WINDOW 三点对账——有弹窗无此行 = 窗体被其他路径 show）
        console.debug("[notifwin] shown", e.payload.sessionId, e.payload.status, Date.now());
        setPayload(e.payload);
        // 新通知到达时清掉旧候选列表，避免与卡片同时出现（先还原高度再显示，避免闪帧）
        setCandidates(null);
        // 操作卡（2A）不走会话卡生命周期：窗口由 Rust 侧按操作卡高度（220）建房
        // （applyHeight 会还原到 110，不做）；且不 arm 自动隐藏——常驻直到用户
        // 处置（复制/不再提醒销账，X 暂忽略）。后端 consume 每次运行只调一次，
        // 「本次运行内不再弹」由后端结构性保证，前端不额外重弹
        const isActionCard = e.payload.action !== undefined;
        void (async () => {
          if (!isActionCard) {
            await applyHeight(null);
          }
          win.show();
        })();
        if (!isActionCard) {
          armTimer(10000);
        }
      },
      { target: win.label }
    ).then((fn) => (unlisten = fn));
    return () => unlisten?.();
    // eslint-disable-next-line react-hooks/exhaustive-deps -- armTimer/applyHeight 仅依赖稳定的 timerRef
  }, []);

  if (!payload) return <div className="h-full w-full" />;

  // 操作卡分支（2A）：codex 信任门常驻提醒——复用本页面与建房管线，但不参与
  // 会话卡的自动隐藏/跳转（见 listener 处 isActionCard 分支注释）
  if (payload.action === CODEX_TRUST_HINT_ACTION) {
    return <CodexTrustHintCard command={payload.actionCommand || "/hooks"} />;
  }

  // 点击通知卡 → 隐藏浮窗并跳转到对应会话终端（与主界面卡片同一实现）
  const jump = async () => {
    getCurrentWindow().hide();
    try {
      const ambiguous = await focus({
        pid: payload.pid,
        id: payload.sessionId,
        agentType: payload.agentType,
        projectName: payload.projectName,
        lastMessage: payload.lastMessage,
        // 通知浮窗 payload.title 必填 string（Rust 侧），`|| undefined` 是该来源
        // 的正确空值语义（issue #45：非 Session 来源，不走 sessionTitleOrUndefined）
        title: payload.title || undefined,
      });
      // 多窗口歧义：在通知窗内联渲染候选，避免静默失败（先调高度再显示，避免闪帧）
      if (ambiguous) {
        await applyHeight(ambiguous.length);
        getCurrentWindow().show();
        // 候选列表不操作 15 秒自动隐藏，避免无限驻留
        armTimer(15000);
      }
    } catch {
      // 跳转失败不弹新提示（通知窗口环境无 toast 容器）
    }
  };

  return (
    <>
      {payload && !candidates && (
        <div
          className="bg-card flex h-screen w-screen cursor-pointer items-center gap-3 rounded-lg border p-3 shadow-2xl"
          onMouseEnter={() => timerRef.current && window.clearTimeout(timerRef.current)}
          onMouseLeave={() => armTimer(5000)}
          onClick={jump}
        >
          <span
            className="h-3 w-3 shrink-0 rounded-full"
            style={{ background: payload.statusColor }}
          />
          <div className="min-w-0 flex-1">
            <p className="truncate text-xs font-semibold">
              {(() => {
                const badge = AGENT_BADGE[payload.agentType];
                return badge ? (
                  <span
                    className={cn(
                      "mr-1 inline-flex items-center gap-1 rounded border px-1.5 py-0.5",
                      badge.className
                    )}
                  >
                    <badge.Icon className="h-3 w-3" />
                    {payload.agentLabel}
                  </span>
                ) : (
                  payload.agentLabel
                );
              })()}{" "}
              · {payload.projectName} ·{" "}
              {t(`sessions.statusLabels.${payload.status}`, payload.status)}
            </p>
            <p className="mt-1 line-clamp-2 text-[11px] opacity-70">{payload.lastMessage}</p>
          </div>
        </div>
      )}
      {candidates && (
        <div
          className="bg-card flex h-screen w-screen flex-col gap-1 rounded-lg border p-3 shadow-2xl"
          onMouseEnter={() => timerRef.current && window.clearTimeout(timerRef.current)}
          onMouseLeave={() => armTimer(5000)}
        >
          <p className="text-xs font-semibold">{t("sessions.pickWindow")}</p>
          <div className="max-h-full overflow-y-auto">
            {candidates.map((w) => (
              <button
                key={w.hwnd}
                className="hover:bg-accent truncate rounded border px-2 py-1.5 text-left text-[11px]"
                title={w.title}
                onClick={() => {
                  setCandidates(null);
                  void applyHeight(null);
                  getCurrentWindow().hide();
                  focusHwnd(w.hwnd).catch(() => {});
                }}
              >
                {w.title || t("sessions.untitledWindow")} — {w.process}
              </button>
            ))}
          </div>
        </div>
      )}
    </>
  );
}

// codex 信任门操作卡（2A）：常驻直到用户处置——「复制命令」/「不再提醒」销账
// （ack 命令落 KV 后关窗）；X 仅藏窗（KV pending 保留 → 下次启动再弹）。
// 剪贴板判断：focusless 窗内点击自带瞬时 user activation，WebView2/WKWebView
// 大概率放行 navigator.clipboard.writeText，但不保证——失败时卡面就地提示
// （命令文本醒目在手，可手抄后自行点「不再提醒」或 X），不关窗、不销账
function CodexTrustHintCard({ command }: { command: string }) {
  const { t } = useTranslation();
  // 卡面就地提示位：null = 无；"copyFailed" = 复制失败；"ackFailed" = 销账命令失败
  const [hint, setHint] = useState<"copyFailed" | "ackFailed" | null>(null);
  const [busy, setBusy] = useState(false);

  // 销账 + 关窗：ack 失败不关窗（KV 未动——重试/复制/X 均可，销账语义不因 IPC
  // 抖动静默丢失，下次启动仍会弹）
  const ack = async (action: "copy" | "never") => {
    setBusy(true);
    try {
      await invoke("codex_trust_notice_ack", { action });
      await getCurrentWindow().hide();
    } catch (e) {
      console.error("codex_trust_notice_ack failed:", e);
      setHint("ackFailed");
    } finally {
      setBusy(false);
    }
  };

  // 复制命令：先写剪贴板，成功才销账；失败就地提示（不关窗、不销账）
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(command);
    } catch (e) {
      console.error("clipboard write failed:", e);
      setHint("copyFailed");
      return;
    }
    await ack("copy");
  };

  return (
    <div className="bg-card relative flex h-screen w-screen flex-col gap-2 rounded-lg border p-4 shadow-2xl">
      {/* X = 暂忽略：仅藏窗不销账（KV pending 保留，下次启动再弹） */}
      <button
        type="button"
        className="text-muted-foreground hover:bg-accent hover:text-accent-foreground absolute top-1.5 right-1.5 rounded p-1"
        aria-label={t("notifications.codexTrust.close")}
        title={t("notifications.codexTrust.close")}
        onClick={() => void getCurrentWindow().hide()}
      >
        <X className="h-4 w-4" />
      </button>
      <div className="flex items-center gap-2 pr-6">
        <span className="h-3 w-3 shrink-0 rounded-full bg-amber-500" />
        <p className="text-xs font-semibold">{t("notifications.codexTrust.title")}</p>
      </div>
      <p className="text-[11px] leading-relaxed opacity-70">
        {t("notifications.codexTrust.description")}
      </p>
      {/* 醒目命令文本（等宽）：复制失败的手抄兜底——即使剪贴板不可用，用户照抄也能完成信任 */}
      <code className="bg-muted rounded-md border px-3 py-2 text-center font-mono text-base font-bold tracking-widest">
        {command}
      </code>
      {hint && (
        <p className="text-destructive text-[11px]">
          {t(
            hint === "copyFailed"
              ? "notifications.codexTrust.copyFailed"
              : "notifications.codexTrust.ackFailed"
          )}
        </p>
      )}
      <div className="mt-auto flex justify-end gap-2">
        <Button size="sm" disabled={busy} onClick={() => void copy()}>
          {t("notifications.codexTrust.copy")}
        </Button>
        <Button size="sm" variant="outline" disabled={busy} onClick={() => void ack("never")}>
          {t("notifications.codexTrust.never")}
        </Button>
      </div>
    </div>
  );
}
