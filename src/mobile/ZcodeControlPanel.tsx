// ZCode 统一控制面板（T7.2 Slice B1a）：手机端最小发送面。
//
// 纪律（与 T7.2 计划 / useZcodeControl 对齐）：
// - 连接态只来自 hook 的官方 GET；未确认一律 `unknown`，已保存链接绝不当成已连接
//   （文案里也不出现「已连接 / 已配对」这类断言性措辞）。
// - 发送只走官方 tagged send（hook.submit → POST /m/api/v1/zcode/actions），
//   **不**回退旧 `session-send`、无头 CLI 或终端按键。
// - 回执如实：accepted=已受理（≠ 已生效），confirmed 才显示已生效，unknown 保持中性；
//   send_failed/rejected/unknown 保留草稿、不自动重发。就地原因只展示本地化文案，
//   `data-reason-code` 才是冻结错误码本身。
import { useEffect, useState } from "react";
import i18n from "@/i18n";
import type { ActionReceipt } from "@/lib/api/zcodeControl";
import useZcodeControl from "./useZcodeControl";

interface ZcodeControlPanelProps {
  /** 会话原 ID（服务端按它绑定 ZCode 会话；手机不自报 workspace） */
  sessionId: string;
}

/** 冻结原因码 → 本地化文案键；表外码统一中性兜底（码本身只进 data-reason-code） */
const REASON_KEYS: Record<string, string> = {
  needs_config: "mobile.zcodeControl.reasonNeedsConfig",
  needs_auth: "mobile.zcodeControl.reasonNeedsAuth",
  offline: "mobile.zcodeControl.reasonOffline",
  conflict: "mobile.zcodeControl.reasonConflict",
  unknown: "mobile.zcodeControl.reasonUnknown",
};

/** 回执状态 → 本地化文案键：accepted/confirmed/unknown 三种语义互不冒充 */
const STATUS_KEYS: Record<string, string> = {
  accepted: "mobile.zcodeControl.statusAccepted",
  confirmed: "mobile.zcodeControl.statusConfirmed",
  noop: "mobile.zcodeControl.statusNoop",
  rejected: "mobile.zcodeControl.statusRejected",
  send_failed: "mobile.zcodeControl.statusSendFailed",
  unknown: "mobile.zcodeControl.statusUnknown",
};

export default function ZcodeControlPanel({ sessionId }: ZcodeControlPanelProps) {
  const { connection, session, reason, submitting, draftCleared, submit } =
    useZcodeControl(sessionId);
  const [draft, setDraft] = useState("");
  const [receipt, setReceipt] = useState<ActionReceipt | null>(null);

  // 仅 send 的 accepted 允许清本次草稿（hook 语义）；失败/拒绝/未知一律保留。
  useEffect(() => {
    if (draftCleared) setDraft("");
  }, [draftCleared]);

  // 换会话清旧回执与旧草稿：上一次会话的受理状态不得贴到新会话。
  useEffect(() => {
    setReceipt(null);
    setDraft("");
  }, [sessionId]);

  // 硬门：连接已配对 + 会话就绪 + send 能力开启 + 无在途提交。空草稿不阻挡按钮
  // （按钮代表控制通道就绪），但点击时判空、绝不 POST 空动作。
  const canSend =
    connection?.state === "paired" &&
    session !== null &&
    session.ready &&
    session.capabilities.send.enabled &&
    !submitting;

  const handleSend = async () => {
    if (!canSend || session === null || draft.trim().length === 0) return;
    try {
      const result = await submit({
        kind: "send",
        sessionId,
        generation: session.generation,
        text: draft, // 多行原样上行；trim 只用于判空
      });
      setReceipt(result);
    } catch {
      // 传输层失败：hook 已置冻结原因码；草稿保留，绝不自动重发。
    }
  };

  return (
    <div
      data-testid="zcode-control-panel"
      data-connection-state={connection?.state ?? "unknown"}
      className="shrink-0 border-t border-[var(--cb)] px-3 py-2"
    >
      {reason !== null && (
        <p
          data-testid="zcode-control-reason"
          data-reason-code={reason}
          className="mb-2 text-xs text-[var(--mut)]"
        >
          {i18n.t(REASON_KEYS[reason] ?? "mobile.zcodeControl.reasonUnknown")}
        </p>
      )}
      {receipt !== null && (
        <p
          data-testid="zcode-action-status"
          data-action-state={receipt.state}
          className="mb-2 text-xs text-[var(--mut)]"
        >
          {i18n.t(STATUS_KEYS[receipt.state] ?? "mobile.zcodeControl.statusUnknown")}
        </p>
      )}
      <div className="flex items-end gap-2">
        <textarea
          data-testid="zcode-draft"
          aria-label={i18n.t("mobile.zcodeControl.draftAria")}
          value={draft}
          rows={2}
          onChange={(e) => setDraft(e.target.value)}
          placeholder={i18n.t("mobile.zcodeControl.draftPlaceholder")}
          className="min-h-0 flex-1 resize-none rounded-lg border border-[var(--cb)] px-3 py-2 text-sm text-[var(--tx)] placeholder:text-[var(--mut)] focus:ring-2 focus:ring-[var(--btnp)] focus:outline-none"
        />
        <button
          type="button"
          data-testid="zcode-send"
          disabled={!canSend}
          onClick={() => void handleSend()}
          className="shrink-0 rounded-full bg-[var(--btnp)]/10 px-4 py-2 text-sm text-[var(--tx)] disabled:opacity-40"
        >
          {i18n.t(submitting ? "mobile.zcodeControl.sending" : "mobile.zcodeControl.send")}
        </button>
      </div>
    </div>
  );
}
