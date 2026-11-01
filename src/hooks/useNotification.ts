import { useEffect, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  sendNotification,
  isPermissionGranted,
  requestPermission,
  onAction,
  registerActionTypes,
} from "@tauri-apps/plugin-notification";
import { useSessionStore } from "@/stores/sessionStore";
import { playCompletionSound } from "@/lib/audio";
import { getAgentLabel } from "@/lib/agentBadge";
import { addHistory } from "@/lib/notificationHistory";
import { sessionTitleOrUndefined } from "@/lib/sessionTitle";
import { petSoundTakeover, petSuppressPopup } from "@/components/pet/petConfig";
import type { Session } from "@/types/session";
import {
  directionKey,
  isSameDirectionThrottled,
  sessionNotifyKey,
} from "@/lib/notification-throttle";

const STATUS_LABELS: Record<string, string> = {
  waiting: "等待操作",
  processing: "运行中",
  thinking: "思考中",
  compacting: "压缩中",
  idle: "空闲",
  finished: "已结束",
};

// 通知去重：同一会话同一状态 5 秒内不重复
// 状态 → 颜色映射（三色：红/黄/绿）
function statusToColor(status: string): string {
  switch (status) {
    case "waiting":
      return "red";
    case "processing":
    case "thinking":
    case "compacting":
      return "yellow";
    case "idle":
    case "finished":
      return "green";
    default:
      return "gray";
  }
}

// 首见未读卡补发通知的新鲜度门控（review F5）：重启/补偿场景下，未读池的
// turnedGreenAt 距今超过 2 分钟的老卡静默显示，不重放历史通知。
// 不能再用 lastActivityAt：DB-only 卡保留的是业务活动时间，补偿完成时可能已很旧。
// 时间无法解析（空串/异常）时保守静默
export const FIRST_SEEN_UNREAD_FRESH_MS = 2 * 60 * 1000;

// 绿灯稳定窗（2026-09-19）：翻绿不立即播报，持续 GREEN_STABLE_MS 仍绿才播。
// 防「瞬态绿」误报——如 claude thinking-only 行每落一条就闪一次绿
// （实测 5 分钟 6 次、每次仅持续 1~2s）。真实完成是永久绿，只多等这 3 秒。
// 全工具统一生效（8 家共用本通知通道）；各工具的停更降级阈值在上游状态判定层，
// 与本窗分层无竞态。翻回非绿即取消，不改状态、不改卡片颜色。
export const GREEN_STABLE_MS = 3000;

// F2a 同方向跃迁节流窗（终审发现 C / 决策 4）：同会话同一 from→to 颜色对的跃迁
// 60s 内不重复通知。teammate 子 agent 运行期主会话合法地在黄↔绿间抖动，每次抖边
// 都会弹浮窗/响铃——按方向分存记账（交替抖动的两个边各自压 60s），红/waiting 豁免

export function isFreshFirstSeenUnread(
  session: Pick<Session, "unread" | "status" | "lastActivityAt" | "unreadTurnedGreenAtMs">,
  nowMs: number = Date.now()
): boolean {
  if (!session.unread || statusToColor(session.status) !== "green") {
    return false;
  }
  const greenAt =
    typeof session.unreadTurnedGreenAtMs === "number" &&
    Number.isFinite(session.unreadTurnedGreenAtMs)
      ? session.unreadTurnedGreenAtMs
      : Date.parse(session.lastActivityAt);
  if (Number.isNaN(greenAt)) {
    return false;
  }
  return nowMs - greenAt <= FIRST_SEEN_UNREAD_FRESH_MS;
}

export function useNotification() {
  const sessions = useSessionStore((s) => s.sessions);
  const prevStatuses = useRef<Map<string, { status: string; color: string; at: number }>>(
    new Map()
  );
  // 上次实际通知记录：同会话 5 秒内重复翻转到同一目标颜色只弹一次（兜底状态抖动）
  const lastNotified = useRef<Map<string, { color: string; at: number }>>(new Map());
  // F2a 同方向记账：会话 id → (from>to 方向键 → 上次实际通知时刻)。按方向分存——
  // 交替抖动（黄→绿↔绿→黄）的两个边各自压 60s，单条「上一跳方向」记账压不住交替
  const lastDirectionNotify = useRef<Map<string, Map<string, number>>>(new Map());
  // 绿灯稳定窗定时器：转绿后延迟 GREEN_STABLE_MS 播报，期间翻回非绿即取消
  const greenTimers = useRef<Map<string, ReturnType<typeof setTimeout>>>(new Map());
  const permissionGranted = useRef(false);
  const notificationsEnabled = useRef(true);
  // 子 Agent 回报提醒开关（观察台 §四）：与 notificationsEnabled 同模式，每拍刷新
  const subagentReportEnabled = useRef(true);
  // F2b 子 agent 活动打标静默开关（终审发现 C）：默认 true=静默（缺省/读取失败沿用默认）
  const silenceSubagentFlap = useRef(true);

  // 卸载时清掉所有未到点的稳定窗定时器（应用生命周期内本 hook 不卸载，防御性收尾）
  useEffect(() => {
    // 卸载清理读的是快照（cleanup 执行时 ref 可能已被替换——react-hooks 警告正解）
    const timers = greenTimers.current;
    return () => {
      timers.forEach((t) => clearTimeout(t));
      timers.clear();
    };
  }, []);

  // 初始化：请求通知权限 + 读取设置
  useEffect(() => {
    const init = async () => {
      // 渠道统一（spec 014）：清理旧系统通知开关的遗留键
      localStorage.removeItem("mam.useSystemNotification");
      try {
        let granted = await isPermissionGranted();
        if (!granted) {
          const permission = await requestPermission();
          granted = permission === "granted";
        }
        permissionGranted.current = granted;
      } catch (e) {
        console.error("Notification permission error:", e);
      }

      // 读取通知开关设置
      try {
        const enabled = await invoke<string | null>("get_setting", {
          key: "notifications_enabled",
        });
        notificationsEnabled.current = enabled !== "false";
      } catch {
        notificationsEnabled.current = true;
      }

      // 注册"查看会话"通知 action + 监听点击（满足 FR-2 #12）
      try {
        await registerActionTypes([
          {
            id: "focus-session",
            actions: [{ id: "focus", title: "查看会话" }],
          },
          {
            // M4 T2a：配对请求通知动作（点击直达设置页审批）。通知动作类型注册的
            // 全局单点在此——useRemoteEvents 只发送 actionTypeId，不得二次注册
            id: "open-pairing",
            actions: [{ id: "open", title: "前往审批" }],
          },
        ]);
        await onAction(async (notification) => {
          // M4 T2a：配对请求通知点击直达设置页（spec「点击直达配对面板」）——
          // pathname 路由（main.tsx pageMap），整页跳转
          if (notification.actionTypeId === "open-pairing") {
            window.location.assign("/settings");
            return;
          }
          if (notification.actionTypeId !== "focus-session") return;
          // P2-5（issue #34）：放开 pid>0 门并透传 form——App 形态未读卡 pid=0，
          // 原门直接吞掉点击致"查看会话"死路；缺 form 则 Windows 深链分支永不可达。
          // pid 失效/为 0 时后端自有兜底链（pid_dead → reactivate / activate_agent_app）
          try {
            await invoke("focus_session", {
              pid: (notification.extra?.pid as number) ?? 0,
              sessionId: (notification.extra?.sessionId as string) ?? undefined,
              agentType: (notification.extra?.agentType as string) ?? undefined,
              projectName: (notification.extra?.projectName as string) ?? undefined,
              lastMessage: (notification.extra?.lastMessage as string) ?? undefined,
              // issue #45 附带发现：系统通知点击跳转此前漏传 title，标题匹配层对该
              // 入口永远跳过（安全回落既有层，但增强无效）。extra 与 onAction 成对透传
              title: (notification.extra?.title as string) || undefined,
              form: (notification.extra?.form as string) ?? undefined,
            });
          } catch (e) {
            console.error("focus_session failed:", e);
          }
        });
      } catch (e) {
        console.error("register action types failed:", e);
      }
    };
    init();
  }, []);

  useEffect(() => {
    (async () => {
      // 统一通知流：开关刷新 → 历史记录 → 提示音 → 浮窗/系统降级
      // 供两处调用：常规颜色变化 + 首见未读绿卡补偿通知
      const notifyCompletion = async (session: Session, fromColor = "") => {
        // [Bug A 取证插桩·临时] 入口对账行：notifyCompletion 每次被调必现（含被门拦下的）
        console.debug(
          "[notif] notifyCompletion",
          session.id,
          `from=${fromColor || "?"}->to=${statusToColor(session.status)}`,
          Date.now()
        );
        // 每次通知前刷新通知开关设置（支持运行时切换）
        try {
          const val = await invoke<string | null>("get_setting", { key: "notifications_enabled" });
          notificationsEnabled.current = val !== "false";
        } catch {
          // 忽略错误
        }
        if (!notificationsEnabled.current) {
          console.debug("[notif] gate:notificationsOff", session.id, Date.now());
          return;
        }

        // 子 Agent 回报提醒开关（观察台 §四）：仅滤「子 agent 回报导致」的提醒；
        // 判定信号 = 后端判据层打标（Session.lastMessageSubagentReport 布尔），
        // 与修复项完成识别同源——不在此处匹配 lastMessage 文案（§四.3）
        try {
          const v = await invoke<string | null>("get_setting", {
            key: "notify_subagent_report",
          });
          subagentReportEnabled.current = v !== "false"; // 缺省/读取失败 = 开
        } catch {
          subagentReportEnabled.current = true;
        }
        if (!subagentReportEnabled.current && session.lastMessageSubagentReport) {
          console.debug("[notif] gate:subagentReportOff", session.id, Date.now());
          return;
        }

        // F2b 子 agent 活动打标静默（终审发现 C / 决策 4）：判定在后端
        // （活跃子 agent 在场 ∧ 最新 hook ∈ PostToolUse 族 ∧ < 30s TTL），前端只消费
        // 布尔、永不匹配文案。开关缺省/读取失败 = 静默（"true"）；命中 → 整条通知流
        // 静默（含历史），早退语义与 §四 开关一致
        try {
          const v = await invoke<string | null>("get_setting", {
            key: "silence_subagent_activity_flap",
          });
          silenceSubagentFlap.current = v !== "false";
        } catch {
          silenceSubagentFlap.current = true;
        }
        if (silenceSubagentFlap.current && session.flapFromSubagentActivity) {
          console.debug("[notif] gate:silenceFlap", session.id, Date.now());
          return;
        }

        const currColor = statusToColor(session.status);

        // 记录到通知历史（spec 014）
        addHistory({
          agentType: session.agentType,
          form: session.form,
          projectName: session.projectName,
          status: session.status,
          lastMessage: session.lastMessage ?? "",
          title: sessionTitleOrUndefined(session),
          pid: session.pid,
          sessionId: session.id,
          at: Date.now(),
        });

        // 方向过滤：仅变为绿（任务完成）时按工具播放提示音
        // 宠物开启即接管完成提示音（静音则整体静默，spec D3）
        if (currColor === "green" && !petSoundTakeover()) playCompletionSound(session.agentType);
        // 记录本次通知用于时间去重
        lastNotified.current.set(sessionNotifyKey(session.agentType, session.id), {
          color: currColor,
          at: Date.now(),
        });
        // F2a 方向记账（只在真实通知时落账）：同方向 60s 节流的判定依据。
        // fromColor 未知（首见未读补发）不落账——那不是一次「跃迁通知」
        if (fromColor) {
          const dirMap =
            lastDirectionNotify.current.get(sessionNotifyKey(session.agentType, session.id)) ??
            new Map<string, number>();
          dirMap.set(directionKey(fromColor, currColor), Date.now());
          lastDirectionNotify.current.set(sessionNotifyKey(session.agentType, session.id), dirMap);
        }

        // 发送通知：应用内浮窗为主路径，失败降级系统 toast（两者都在宠物压制守卫内）
        // 宠物可见时全部静默：头顶气泡是唯一通知面（spec W1）
        if (!petSuppressPopup()) {
          const toolLabel = getAgentLabel(session.agentType, session.form);
          const statusLabel = STATUS_LABELS[session.status] ?? session.status;
          // [Bug A 取证插桩·临时] 浮窗主路径发射点：与 Rust cmd 行、[notifwin] shown 对账
          console.debug("[notif] SHOW_WINDOW", session.id, session.status, Date.now());
          try {
            await invoke("show_notification_window", {
              payload: {
                agentType: session.agentType,
                agentLabel: toolLabel,
                projectName: session.projectName,
                statusColor: currColor,
                status: session.status,
                lastMessage: session.lastMessage ?? "",
                // Rust NotificationPayload.title: String 必填——与可选来源不同，
                // `?? ""` 兜底是正确语义（issue #45：口径有意保留，非待收敛项）
                title: session.title ?? "",
                pid: session.pid,
                sessionId: session.id,
              },
            });
          } catch (e) {
            // 浮窗失败降级系统 toast，保证通知不丢（spec 008 错误处理要求）
            console.error("show_notification_window failed:", e);
            if (permissionGranted.current) {
              sendNotification({
                title: `${toolLabel} — ${session.projectName}`,
                body: `${statusLabel}${session.lastMessage ? ": " + session.lastMessage.slice(0, 80) : ""}`,
                actionTypeId: "focus-session",
                extra: {
                  pid: session.pid,
                  sessionId: session.id,
                  agentType: session.agentType,
                  // P2-5（issue #34）：降级系统通知补带 form，点击跳转才能走对链路
                  form: session.form,
                  projectName: session.projectName,
                  lastMessage: session.lastMessage ?? "",
                  // issue #45 附带发现：extra 此前漏传 title → 点击跳转的标题匹配层
                  // 对该入口永远跳过。空串兜底（后端 title_keys 过滤空键，无假命中面）
                  title: session.title ?? "",
                },
              });
            }
          }
        }
      };

      // 取消未到点的绿稳定窗定时器（翻回非绿/会话消失时调用）
      const clearGreenTimer = (id: string) => {
        const timer = greenTimers.current.get(id);
        if (timer) {
          clearTimeout(timer);
          greenTimers.current.delete(id);
        }
      };

      // 绿灯稳定窗调度：到点后从 store 取最新会话复核仍为绿才播报；
      // 翻回非绿的会话在主循环里会被 clearGreenTimer 取消，此处双保险再验一次。
      // fromColor 随调度传入：5s 同色去重与 F2a 同方向节流都在最终通知点（稳定窗后）判定
      const scheduleGreenNotify = (session: Session, fromColor = "") => {
        clearGreenTimer(session.id);
        const timer = setTimeout(() => {
          greenTimers.current.delete(session.id);
          const latest = useSessionStore.getState().sessions.find((s) => s.id === session.id);
          if (!latest || statusToColor(latest.status) !== "green") {
            // [Bug A 取证插桩·临时] 稳定窗到点复核失败：到点时已不绿/会话消失
            console.debug(
              "[notif] gate:greenUnstable",
              session.id,
              latest?.status ?? "gone",
              Date.now()
            );
            return;
          }
          const notified = lastNotified.current.get(
            sessionNotifyKey(session.agentType, session.id)
          );
          if (notified && notified.color === "green" && Date.now() - notified.at < 5000) {
            console.debug("[notif] gate:dedup5s:greenWin", session.id, Date.now());
            return;
          }
          // F2a：同方向（from→绿）60s 内已弹过 → 节流（红边天然豁免——红不进稳定窗）
          if (
            isSameDirectionThrottled(
              lastDirectionNotify.current.get(sessionNotifyKey(session.agentType, session.id)),
              fromColor,
              "green"
            )
          ) {
            console.debug("[notif] gate:throttle60s:greenWin", session.id, fromColor, Date.now());
            return;
          }
          void notifyCompletion(latest, fromColor);
        }, GREEN_STABLE_MS);
        greenTimers.current.set(session.id, timer);
      };

      for (const session of sessions) {
        const prev = prevStatuses.current.get(sessionNotifyKey(session.agentType, session.id));
        const currColor = statusToColor(session.status);

        // 首次加载不通知——除非是「未读绿卡」：补偿/重启场景下它从未被观测过转绿，
        // 需补一次完成通知（spec W4）。绿播报统一过稳定窗（与常规翻色同路径）；
        // fromColor 未知（首见）→ F2a 方向记账不落账、节流不命中
        if (!prev) {
          prevStatuses.current.set(sessionNotifyKey(session.agentType, session.id), {
            status: session.status,
            color: currColor,
            at: Date.now(),
          });
          // review F5：新鲜度门控——老未读卡（转绿 > 2 分钟）静默显示，不重放通知
          if (isFreshFirstSeenUnread(session)) {
            scheduleGreenNotify(session);
          }
          continue;
        }

        prevStatuses.current.set(sessionNotifyKey(session.agentType, session.id), {
          status: session.status,
          color: currColor,
          at: Date.now(),
        });

        // 颜色未变 → 不通知（即使状态变了，如 Processing → Thinking 都是黄色；
        // 绿→绿时稳定窗定时器维持原状，不重复调度）
        if (prev.color === currColor) continue;

        if (currColor === "green") {
          // 转绿统一过稳定窗：持续 GREEN_STABLE_MS 仍绿才播报（F2a 节流在窗后判定）
          scheduleGreenNotify(session, prev.color);
          continue;
        }

        // 翻离绿/非绿互转：取消未到点的绿播报，其余转换维持即时播报
        clearGreenTimer(session.id);

        // 时间去重：5 秒内同目标颜色不重复弹（兜底状态抖动）
        const notified = lastNotified.current.get(sessionNotifyKey(session.agentType, session.id));
        if (notified && notified.color === currColor && Date.now() - notified.at < 5000) {
          console.debug("[notif] gate:dedup5s", session.id, currColor, Date.now());
          continue;
        }

        // F2a 同方向节流：该会话该方向 60s 内已实际弹过 → 不再弹
        // （红/waiting 双向豁免在 isSameDirectionThrottled 内）
        if (
          isSameDirectionThrottled(
            lastDirectionNotify.current.get(sessionNotifyKey(session.agentType, session.id)),
            prev.color,
            currColor
          )
        ) {
          console.debug(
            "[notif] gate:throttle60s",
            session.id,
            `${prev.color}>${currColor}`,
            Date.now()
          );
          continue;
        }

        // 颜色变化 → 走统一通知流（携带前色供 F2a 方向记账）
        await notifyCompletion(session, prev.color);
      }

      // 清理已消失的会话（含未到点的绿稳定窗定时器）
      const activeIds = new Set(sessions.map((s) => `${s.agentType}-${s.id}`));
      for (const id of prevStatuses.current.keys()) {
        if (!activeIds.has(id)) {
          prevStatuses.current.delete(id);
          lastNotified.current.delete(id);
          lastDirectionNotify.current.delete(id);
          clearGreenTimer(id);
        }
      }
    })();
  }, [sessions]);
}
