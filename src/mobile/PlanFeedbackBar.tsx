// 计划反馈条（2026-10-04 计划批准卡）：claude 计划批准框选「Tell Claude what to
// change」后的**打字通道**——蓝系、与问答卡自由作答按钮组同款交互（发送/覆盖写入/清空）。
//
// 流程语义（与后端 /session-plan-feedback 契约同源）：
// - 「覆盖写入」= 清空终端编辑行后打字，**不回车**（暂存——可在输入框改错字再覆盖）；
// - 「发送」= 清空（有内容时）→ 打字 → 回车提交：Claude **留在计划模式**按反馈开启
//   一轮新的计划修改（用户裁决：选项 1/2 才切出计划）；
//   终端已有暂存内容且输入框为空时 = 仅回车（提交暂存的那份）；
// - 「清空」= 退格清空编辑行（放弃暂存内容，回到空白编辑态）。
//
// 生命周期仅前端持有（SessionDetail 的 planFeedbackActive）：审批卡随状态转黄卸载后
// 本条独立存活；发送成功提示后自隐；会话切换/页面卸载即清态。**刷新后不恢复反馈态**
// （终端直接打字即可——如实登记的已知边界，不做编辑态屏面识别）。
import { useCallback, useState } from "react";
import { formatInvokeError } from "@/lib/invokeError";
import i18n from "@/i18n";
import { ApiError, sessionPlanFeedback } from "./api";

const MAX_FEEDBACK_CHARS = 2000;
const SENT_DISMISS_MS = 1600;

export default function PlanFeedbackBar({
  sessionId,
  onDismiss,
}: {
  sessionId: string;
  /** 发送成功（自隐倒计时）或用户点收起 → 通知父级卸载本条 */
  onDismiss?: () => void;
}) {
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // 终端编辑行里已有暂存内容（覆盖写入成功后置位）——「发送」在输入框为空时
  // 仍可点（仅回车提交暂存那份）
  const [staged, setStaged] = useState(false);
  const [sentDone, setSentDone] = useState(false);

  const fail = useCallback((e: unknown) => {
    if (e instanceof ApiError) {
      const code = typeof e.data?.error === "string" ? e.data.error : null;
      if (code === "not_waiting") setError("会话不在等待状态");
      else if (code === "no_session") setError("会话已结束，请返回看板刷新");
      else setError(formatInvokeError(e.message, i18n.t));
    } else {
      setError(formatInvokeError(String(e), i18n.t));
    }
  }, []);

  const run = useCallback(
    async (action: "type" | "clear", opts: { text?: string; submit?: boolean } = {}) => {
      if (busy) return;
      setBusy(true);
      setError(null);
      try {
        const res = await sessionPlanFeedback(sessionId, action, opts);
        if (res.status === "failed") {
          setError(res.error);
          return;
        }
        if (action === "clear") {
          setStaged(false);
          setText("");
        } else if (opts.submit) {
          // 发送成功：Claude 正在按反馈修改计划——提示后自隐
          setSentDone(true);
          setStaged(false);
          setText("");
          window.setTimeout(() => onDismiss?.(), SENT_DISMISS_MS);
        } else {
          setStaged(true);
          setText("");
        }
      } catch (e) {
        fail(e);
      } finally {
        setBusy(false);
      }
    },
    [busy, sessionId, onDismiss, fail]
  );

  const canSend = !busy && !sentDone && (text.trim() !== "" || staged);

  return (
    <div
      data-testid="plan-feedback-bar"
      className="shrink-0 rounded-xl border-2 border-sky-500/60 bg-sky-500/5 px-3 py-2 dark:border-sky-400/60 dark:bg-sky-400/5"
    >
      <div className="flex flex-wrap items-center gap-1.5">
        <span className="inline-block h-2 w-2 shrink-0 rounded-full bg-sky-500" />
        <span className="text-sm font-semibold text-sky-700 dark:text-sky-400">
          告诉 Claude 要改什么
        </span>
        <button
          type="button"
          data-testid="plan-feedback-dismiss"
          disabled={busy}
          onClick={() => onDismiss?.()}
          className="ml-auto rounded-full px-2 py-0.5 text-xs text-slate-500 hover:bg-slate-500/10 dark:text-slate-400"
        >
          收起
        </button>
      </div>
      <p className="mt-1 text-xs text-sky-700/80 dark:text-sky-400/80">
        输入修改意见发送后，Claude 仍停留在计划模式并重新出计划；选上面的选项 1/2 才会开始执行
      </p>
      <textarea
        data-testid="plan-feedback-input"
        aria-label="计划修改意见"
        value={text}
        rows={2}
        maxLength={MAX_FEEDBACK_CHARS}
        disabled={busy || sentDone}
        onChange={(e) => setText(e.target.value.slice(0, MAX_FEEDBACK_CHARS))}
        className="mt-1.5 min-h-0 w-full resize-none rounded-lg border border-slate-200 px-2.5 py-1.5 text-sm text-slate-800 placeholder:text-slate-400 focus:ring-2 focus:ring-sky-500/40 focus:outline-none disabled:opacity-50 dark:border-slate-700 dark:bg-slate-900 dark:text-slate-200"
      />
      {error !== null && (
        <p
          data-testid="plan-feedback-error"
          className="mt-1 text-xs text-rose-600 dark:text-rose-400"
        >
          {error}
        </p>
      )}
      {sentDone && (
        <p
          data-testid="plan-feedback-sent"
          className="mt-1 text-xs font-medium text-emerald-600 dark:text-emerald-400"
        >
          已发送，Claude 正在修改计划…
        </p>
      )}
      <div className="mt-1.5 flex flex-wrap items-center gap-1.5">
        <button
          type="button"
          data-testid="plan-feedback-send"
          disabled={!canSend}
          onClick={() => run("type", { text, submit: true })}
          className="rounded-full bg-sky-600 px-3 py-1.5 text-xs font-medium text-white hover:bg-sky-700 disabled:opacity-40 dark:bg-sky-500"
        >
          {staged && text.trim() === "" ? "提交已输入内容" : "发送"}
        </button>
        <button
          type="button"
          data-testid="plan-feedback-overwrite"
          disabled={busy || sentDone || text.trim() === ""}
          onClick={() => run("type", { text, submit: false })}
          className="rounded-full bg-sky-500/10 px-3 py-1.5 text-xs text-sky-700 hover:bg-sky-500/20 disabled:opacity-40 dark:bg-sky-400/10 dark:text-sky-300"
        >
          覆盖写入
        </button>
        <button
          type="button"
          data-testid="plan-feedback-clear"
          disabled={busy || sentDone || (!staged && text.trim() === "")}
          onClick={() => run("clear")}
          className="rounded-full bg-slate-500/10 px-3 py-1.5 text-xs text-slate-600 hover:bg-slate-500/20 disabled:opacity-40 dark:bg-slate-400/10 dark:text-slate-300"
        >
          清空
        </button>
        {staged && (
          <span className="text-[11px] text-slate-500 dark:text-slate-400">
            终端已暂存内容（未提交）
          </span>
        )}
      </div>
    </div>
  );
}
