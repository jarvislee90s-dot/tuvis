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
import { useEffect, useRef, useState, type ClipboardEvent as ReactClipboardEvent } from "react";
import i18n from "@/i18n";
import {
  isZcodeErrorCode,
  type ActionReceipt,
  type ConnectionView,
  type ControlAction,
  type SessionView,
  type ZcodeErrorCode,
  ZcodeControlApiError,
} from "@/lib/api/zcodeControl";
import useZcodeControl from "./useZcodeControl";
import ZcodePendingCard, { STATUS_KEYS, pendingDomId, zcodeReasonText } from "./ZcodePendingCard";
import ZcodeQueuePanel from "./ZcodeQueuePanel";
import {
  attachmentMarkup,
  isImageFile,
  newAttachmentId,
  type PendingAttachment,
} from "./attachments";
import { ApiError, fetchZcodeControl, postZcodeTakeover, uploadAttachment } from "./api";

interface ZcodeControlPanelProps {
  /** 会话原 ID（服务端按它绑定 ZCode 会话；手机不自报 workspace） */
  sessionId: string;
}

/**
 * 冻结原因码 → 本地化文案键。词表随待处理卡一起下沉到
 * [`ZcodePendingCard`](./ZcodePendingCard.tsx)（面板消费该组件，词表留在面板会形成
 * 循环 import）；此处按原名 re-export，既有导入方与测试不受影响。
 */
export { ZCODE_REASON_KEYS, zcodeReasonText } from "./ZcodePendingCard";

/**
 * 写面硬门：`connection.state === "paired"` **且** `session.ready === true` 才放行。
 * 返回 null = 可写；否则返回冻结原因码（陈旧投影/未配对连接一律零写）。
 * `connecting` 等配对前态不在冻结码表内 → 统一按 `not_ready` 呈现。
 */
function actionGateReason(
  connection: ConnectionView | null,
  session: SessionView | null
): string | null {
  if (connection === null) return "unknown";
  if (connection.state !== "paired") {
    if (isZcodeErrorCode(connection.reasonCode)) return connection.reasonCode;
    if (isZcodeErrorCode(connection.state)) return connection.state;
    return "not_ready";
  }
  if (session === null || !session.ready) return "not_ready";
  return null;
}

export default function ZcodeControlPanel({ sessionId }: ZcodeControlPanelProps) {
  const { connection, session, reason, submitting, draftCleared, submit, refresh } =
    useZcodeControl(sessionId);
  const [draft, setDraft] = useState("");
  const [receipt, setReceipt] = useState<ActionReceipt | null>(null);
  /** 已保存的官方链接（只读 GET /m/api/v1/official-control；读不到 → null = 不挂入口）。 */
  const [officialUrl, setOfficialUrl] = useState<string | null>(null);
  /** 同帧双击闸：一次用户动作只允许一个在途 POST（双击不产生第二个 actionId）。 */
  const inFlightRef = useRef(false);
  /** 最近一次 send 提交的原文：accepted 迟到时只清这份，不碰回执到达前续写的草稿。 */
  const submittedDraftRef = useRef<string | null>(null);
  /** 草稿即时值（effect 之外也要读最新值，避免闭包拿到旧草稿）。 */
  const draftRef = useRef(draft);
  // 三轮 C：附件态（与 MessageComposer 同一上传管线/条目形状/引用约定）。
  const [attachments, setAttachments] = useState<PendingAttachment[]>([]);
  // 渲染期写 ref 被 react-hooks 规则禁止：同步镜像改在 effect（发送读 refs 语义不变）。
  const attachmentsRef = useRef<PendingAttachment[]>([]);
  useEffect(() => {
    attachmentsRef.current = attachments;
  }, [attachments]);
  /** 上传中条目的取消柄（× 移除 = XHR abort，防白传到底）。 */
  const uploadCtrlsRef = useRef(new Map<string, AbortController>());
  /** 本次 send 提交的就绪附件 id：accepted 清的是「随本次发送」的条目，不碰续加。 */
  const submittedAttachmentIdsRef = useRef<string[] | null>(null);
  /** 附件闸提示（上传中/存在失败项时点击发送）：瞬态，随下次发送动作清除。 */
  const [attachNotice, setAttachNotice] = useState<"uploading" | "failed" | null>(null);
  const attachInputRef = useRef<HTMLInputElement | null>(null);
  /**
   * 当前展示的待处理卡（多组并存时的切组状态）。存 id 而不是下标：宿主增删卡
   * 不会把选择挪到别的请求上；id 一旦消失则回落到首张（绝不空屏丢卡）。
   */
  const [pendingTabId, setPendingTabId] = useState<string | null>(null);

  // 仅 send 的 accepted 允许清本次草稿（hook 语义）；失败/拒绝/未知一律保留。
  // 迟到回执竞态：accepted 到达时若草稿已被续写成别的内容，那是**新**输入，必须保留——
  // 只有草稿仍等于本次提交原文才清空（连点两次发送的场景由 hook 复位标记兜底）。
  useEffect(() => {
    if (!draftCleared) return;
    const submitted = submittedDraftRef.current;
    // 竞态守卫：草稿被续写 → 那是新输入，草稿保留。
    const unchanged = submitted !== null && draftRef.current === submitted;
    if (unchanged) {
      draftRef.current = "";
      setDraft("");
    }
    // 三轮评审 P2-2：附件清理**独立于草稿**——已上传并引用进已发送正文的附件属
    // 本次提交，accepted 即按提交批次 id 清除（续加的新附件保留）；文字才按是否
    // 续写决定去留。否则续写期间 accepted 会把旧附件留到下一次发送重复携带。
    const sentIds = submittedAttachmentIdsRef.current;
    if (sentIds !== null) {
      submittedAttachmentIdsRef.current = null;
      setAttachments((prev) => prev.filter((a) => !sentIds.includes(a.id)));
    }
  }, [draftCleared]);

  // 换会话清旧回执与旧草稿：上一次会话的受理状态不得贴到新会话。
  useEffect(() => {
    setReceipt(null);
    setDraft("");
    draftRef.current = "";
    submittedDraftRef.current = null;
    // 三轮 C：附件与在途上传不跨会话残留。
    setAttachments([]);
    submittedAttachmentIdsRef.current = null;
    setAttachNotice(null);
    // 切组选择不跨会话残留（旧会话的 interaction id 在新会话没有意义）。
    setPendingTabId(null);
    // 抢回面同样不跨会话残留（T8）。
    setTakeoverBusy(false);
    setTakeoverStillConflict(false);
    setTakeoverError(null);
    takeoverInFlightRef.current = false;
    // 三轮评审 P2-1：实际挂载点（SessionDetail）以会话 id 为 **key**——切会话 =
    // 卸载整个组件，本 effect 体不会执行；卸载必须与换 sessionId 同语义。cleanup
    // 在卸载与依赖变更两条路径都取消在途上传（防旧会话附件继续传输落盘）。
    return () => {
      for (const ctrl of uploadCtrlsRef.current.values()) ctrl.abort();
      uploadCtrlsRef.current.clear();
    };
  }, [sessionId]);

  // A5 基线「打开官方页面」的显式辅助入口：只读已保存链接（不自动打开、不代理点击）。
  useEffect(() => {
    let alive = true;
    setOfficialUrl(null);
    void fetchZcodeControl().then((availability) => {
      if (alive) setOfficialUrl(availability.url);
    });
    return () => {
      alive = false;
    };
  }, [sessionId]);

  // ==== T8 冲突态「断开并抢回」 ====
  /** 抢回 POST 在途（busy 态 + 同帧双击闸）。 */
  const [takeoverBusy, setTakeoverBusy] = useState(false);
  /** 服务端一次抢回后仍 conflict：只提示先关官方页面，绝不二次请求。 */
  const [takeoverStillConflict, setTakeoverStillConflict] = useState(false);
  /** 抢回失败的冻结原因码（文案按码本地化，码只进 data-reason-code）。 */
  const [takeoverError, setTakeoverError] = useState<ZcodeErrorCode | "unknown" | null>(null);
  const takeoverInFlightRef = useRef(false);

  /** 一次抢回一个在途槽：busy 期间重入一律丢弃（绝不产生第二次 POST）。 */
  const handleTakeover = () => {
    if (takeoverInFlightRef.current) return;
    takeoverInFlightRef.current = true;
    setTakeoverBusy(true);
    setTakeoverStillConflict(false);
    setTakeoverError(null);
    void postZcodeTakeover()
      .then((view) => {
        // 仍 conflict（官方页还在线）：服务端已按契约只试一次，这里只提示。
        setTakeoverStillConflict(view.state === "conflict");
        // 成功（含仍 conflict）后重拉权威连接/会话视图（现有 GET 纠偏机制）。
        void refresh();
      })
      .catch((error: unknown) => {
        // 失败显示冻结码原因：网络层（status=null）按 offline 口径，其余按服务端码。
        setTakeoverError(
          error instanceof ZcodeControlApiError
            ? (error.code ?? (error.status === null ? "offline" : "unknown"))
            : "unknown"
        );
      })
      .finally(() => {
        takeoverInFlightRef.current = false;
        setTakeoverBusy(false);
      });
  };

  const openOfficial = () => {
    if (officialUrl === null) return;
    window.open(officialUrl, "_blank", "noopener,noreferrer");
  };

  // 写面硬门：连接已配对 + 会话就绪（陈旧投影 ready=false 一律零写）。
  const gateReason = actionGateReason(connection, session);
  // 发送区原因优先取 hook 的如实连接/回执原因；连接正常但投影未就绪时显示 not_ready。
  const displayReason = reason ?? gateReason;

  /**
   * 待处理卡的展示组：默认第一张；用户切过的 id 若仍在册就继续用，被宿主撤掉则回落
   * 首张——其余卡一直挂载（视觉隐藏），所以切组不会丢任何草稿，也绝不丢请求。
   */
  const pendings = session?.pending ?? [];
  const activePendingId =
    pendings.find((pending) => pending.interactionId === pendingTabId)?.interactionId ??
    pendings[0]?.interactionId ??
    null;

  /** 一次动作一个在途槽：同帧重入直接丢弃，回执只认第一次提交。 */
  const runOnce = (action: ControlAction) => {
    if (inFlightRef.current) return;
    inFlightRef.current = true;
    // 新提交开始即清旧回执（C2）：上一次的「已受理」绝不得贴到在途/失败/未知的
    // 新动作上——accepted 越级正是本面板要防的失真。
    setReceipt(null);
    void submit(action)
      .then(setReceipt)
      .catch(() => {
        // 传输层失败：hook 已置冻结原因码；草稿保留，绝不自动重发。
      })
      .finally(() => {
        inFlightRef.current = false;
      });
  };

  // 硬门：连接已配对 + 会话就绪 + send 能力开启 + 无在途提交。空草稿不阻挡按钮
  // （按钮代表控制通道就绪），但点击时判空、绝不 POST 空动作。
  const canSend =
    gateReason === null && session !== null && session.capabilities.send.enabled && !submitting;
  // 能力关闭就地说明（I1）：paired + ready 但 send 能力关时，禁用按钮之外必须有一句
  // 可见原因（服务端 reasonCode），否则用户只会以为按钮坏了。
  const sendCapabilityReason =
    gateReason === null && session !== null && !session.capabilities.send.enabled
      ? (session.capabilities.send.reasonCode ?? "not_ready")
      : null;

  // 三轮 C：附件上传（通用通道；状态机与 MessageComposer 同形，无速率估算）。
  const addZcodeFiles = (files: File[]) => {
    for (const file of files) {
      const id = newAttachmentId();
      const isImage = isImageFile(file);
      setAttachments((prev) => [
        ...prev,
        { id, name: file.name || (isImage ? file.name : "file"), isImage, status: "uploading" },
      ]);
      const ctrl = new AbortController();
      uploadCtrlsRef.current.set(id, ctrl);
      void uploadAttachment(sessionId, file, ctrl.signal, (loaded, total) => {
        setAttachments((prev) =>
          prev.map((a) => (a.id === id ? { ...a, loaded, total: total > 0 ? total : null } : a))
        );
      })
        .then((res) => {
          if (res === null) throw new ApiError(403, i18n.t("mobile.zcodeControl.deviceInvalid"));
          setAttachments((prev) =>
            prev.map((a) => (a.id === id ? { ...a, status: "ready", path: res.path } : a))
          );
        })
        .catch((e: unknown) => {
          if (ctrl.signal.aborted) {
            // 用户取消：chip 已随 removeZcodeAttachment 移除，静默收尾。
            setAttachments((prev) => prev.filter((a) => a.id !== id));
            return;
          }
          const reason =
            e instanceof ApiError ? e.message : e instanceof Error ? e.message : String(e);
          setAttachments((prev) =>
            prev.map((a) => (a.id === id ? { ...a, status: "failed", error: reason } : a))
          );
        })
        .finally(() => {
          uploadCtrlsRef.current.delete(id);
        });
    }
  };

  const removeZcodeAttachment = (id: string) => {
    // 上传中移除 = 取消（XHR abort），与 MessageComposer 同语义。
    uploadCtrlsRef.current.get(id)?.abort();
    uploadCtrlsRef.current.delete(id);
    setAttachments((prev) => prev.filter((a) => a.id !== id));
  };

  /** 粘贴图片走同一上传链路；非图片放行默认文本行为。 */
  const handleZcodePaste = (e: ReactClipboardEvent<HTMLTextAreaElement>) => {
    const images = Array.from(e.clipboardData?.files ?? []).filter((f) => isImageFile(f));
    if (images.length === 0) return;
    e.preventDefault();
    addZcodeFiles(images);
  };

  const handleSend = () => {
    if (!canSend || session === null || inFlightRef.current) return;
    const current = attachmentsRef.current;
    // 附件闸：上传中或存在失败项时不静默丢附件发送（三轮 C 契约）。
    if (current.some((a) => a.status === "uploading")) {
      setAttachNotice("uploading");
      return;
    }
    if (current.some((a) => a.status === "failed")) {
      setAttachNotice("failed");
      return;
    }
    const markup = attachmentMarkup(current);
    const text = draft.trim();
    if (text.length === 0 && markup.length === 0) return;
    // 记下本次提交原文与随行附件：accepted 清「同一份」，失败/未知全保留。
    submittedDraftRef.current = draft;
    submittedAttachmentIdsRef.current = current
      .filter((a) => a.status === "ready")
      .map((a) => a.id);
    setAttachNotice(null);
    runOnce({
      kind: "send",
      sessionId,
      generation: session.generation,
      text: markup ? (text ? `${draft}\n${markup}` : markup) : draft, // 多行原样上行
    });
  };

  // 打断：只认新鲜 control 里唯一的 primaryTurn（多目标拒绝，禁止随取第一个）；
  // 只发 stop，绝不夹带 sendText。
  const stopTargets = (session?.control?.activeWorks ?? []).filter(
    (work) => work.kind === "primaryTurn" && work.foregroundExecutionId !== null
  );
  const stopTarget = stopTargets.length === 1 ? stopTargets[0].foregroundExecutionId : null;
  const canInterrupt =
    gateReason === null &&
    session !== null &&
    session.capabilities.interrupt.enabled &&
    stopTarget !== null &&
    !submitting;
  // 不可打断的就地原因：写面硬门 > 能力关闭 > 多目标歧义。
  const interruptReason =
    gateReason ??
    (session?.capabilities.interrupt.enabled === false
      ? session.capabilities.interrupt.reasonCode
      : stopTarget === null
        ? "target_ambiguous"
        : null);

  const handleInterrupt = () => {
    if (!canInterrupt || session === null || stopTarget === null) return;
    runOnce({
      kind: "stop",
      sessionId,
      generation: session.generation,
      expectedForegroundExecutionId: stopTarget,
    });
  };

  // 模型/档位：目录只来自 session.selection（当前 provider）；写面硬门 + 能力两闸
  // 同开才可提交，任一关闭 → 两个控件都禁用并显示可见原因。
  const selection = session?.selection ?? null;
  const currentChoice =
    selection?.models.find((choice) => choice.modelId === selection.model) ?? null;
  const selectionEnabled =
    gateReason === null &&
    session?.capabilities.model.enabled === true &&
    session.capabilities.thoughtLevel.enabled === true;
  const selectionReason =
    gateReason ??
    session?.capabilities.model.reasonCode ??
    session?.capabilities.thoughtLevel.reasonCode ??
    "unknown";
  const currentThought =
    selection?.thought != null && currentChoice?.thoughtLevels.includes(selection.thought)
      ? selection.thought
      : (currentChoice?.thoughtLevels[0] ?? "");

  const submitSelection = (model: string, thought: string) => {
    if (session === null || !selectionEnabled || thought === "") return;
    runOnce({ kind: "selection", sessionId, generation: session.generation, model, thought });
  };

  const handleModelChange = (modelId: string) => {
    const choice = selection?.models.find((candidate) => candidate.modelId === modelId);
    if (choice === undefined) return;
    // 换模型时档位落在新模型的合法值里：保持旧档位，否则取首档。
    const thought = selection?.thought;
    const kept =
      thought != null && choice.thoughtLevels.includes(thought)
        ? thought
        : (choice.thoughtLevels[0] ?? "");
    submitSelection(modelId, kept);
  };

  const handleThoughtChange = (thought: string) => {
    if (selection === null || currentChoice?.thoughtLevels.includes(thought) !== true) return;
    submitSelection(selection.model, thought);
  };

  return (
    <div
      data-testid="zcode-control-panel"
      data-connection-state={connection?.state ?? "unknown"}
      className="shrink-0 border-t border-[var(--cb)] px-3 py-2"
    >
      {displayReason !== null && (
        <p
          data-testid="zcode-control-reason"
          data-reason-code={displayReason}
          className="mb-2 text-xs text-[var(--mut)]"
        >
          {zcodeReasonText(displayReason)}
        </p>
      )}
      {/* T8 冲突态「断开并抢回」：只在 conflict 出现。确认文案常驻按钮旁（点击即
          执行一次），busy 禁用；仍 conflict 只提示先关官方页面，失败显示冻结码原因。 */}
      {connection?.state === "conflict" && (
        <div className="mb-2">
          <button
            type="button"
            data-testid="zcode-takeover"
            disabled={takeoverBusy}
            onClick={handleTakeover}
            className="rounded-full border border-[var(--cb)] px-4 py-1 text-sm text-[var(--tx)] disabled:opacity-40"
          >
            {i18n.t(
              takeoverBusy ? "mobile.zcodeControl.takeoverBusy" : "mobile.zcodeControl.takeover"
            )}
          </button>
          <p data-testid="zcode-takeover-hint" className="mt-1 text-xs text-[var(--mut)]">
            {i18n.t("mobile.zcodeControl.takeoverHint")}
          </p>
          {takeoverStillConflict && (
            <p
              data-testid="zcode-takeover-still-conflict"
              className="mt-1 text-xs text-[var(--mut)]"
            >
              {i18n.t("mobile.zcodeControl.takeoverStillConflict")}
            </p>
          )}
          {takeoverError !== null && (
            <p
              data-testid="zcode-takeover-error"
              data-reason-code={takeoverError}
              className="mt-1 text-xs text-[var(--mut)]"
            >
              {zcodeReasonText(takeoverError)}
            </p>
          )}
        </div>
      )}
      {/* A5 显式辅助入口：只打开**已保存**的官方链接（挂载不自动外跳、不代理点击）。 */}
      {officialUrl !== null && (
        <button
          type="button"
          data-testid="zcode-open-official"
          onClick={openOfficial}
          className="mb-2 rounded-full border border-[var(--cb)] px-3 py-1 text-xs text-[var(--mut)]"
        >
          {i18n.t("mobile.composer.zcodeOpenOfficial")}
        </button>
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
      {/* 多张 pending 并存：默认只展示一张「当前交互卡」，其余用紧凑小标签切换
          （「问答 1/3」「审批 2/3」）。切组不删卡——每张卡各自保留草稿与受理锁，
          绝不会把其他待处理请求静默丢掉。 */}
      {pendings.length > 1 && (
        <div
          data-testid="zcode-pending-tabs"
          role="tablist"
          aria-label={i18n.t("mobile.zcodeControl.pendingTabsAria")}
          className="mb-1 flex flex-wrap gap-1"
        >
          {pendings.map((pending, index) => {
            const selected = pending.interactionId === activePendingId;
            return (
              <button
                key={pending.interactionId}
                id={pendingDomId("zcode-pending-tab", pending.interactionId)}
                type="button"
                role="tab"
                aria-selected={selected}
                aria-controls={pendingDomId("zcode-pending-panel", pending.interactionId)}
                data-testid={`zcode-pending-tab-${pending.interactionId}`}
                data-active={selected ? "true" : "false"}
                onClick={() => setPendingTabId(pending.interactionId)}
                className={`rounded-full border px-2 py-0.5 text-xs ${
                  selected
                    ? "border-[var(--btnp)] bg-[var(--btnp)]/10 text-[var(--tx)]"
                    : "border-[var(--cb)] text-[var(--mut)]"
                }`}
              >
                {i18n.t(
                  pending.kind === "permission"
                    ? "mobile.zcodeControl.pendingTabPermission"
                    : "mobile.zcodeControl.pendingTabQuestion",
                  { position: index + 1, total: pendings.length }
                )}
              </button>
            );
          })}
        </div>
      )}
      {session !== null &&
        pendings.map((pending) => (
          // key 带代际：interaction 被替换或换代时整卡重挂（草稿/翻页/受理锁一并清空）。
          // 非当前组仍挂载但视觉隐藏：草稿与锁定各自保留，切回来即恢复。
          <div
            key={`${session.generation}:${pending.interactionId}`}
            id={pendingDomId("zcode-pending-panel", pending.interactionId)}
            // 多组并存才成组：只有那时这个容器才是某个 tab 的 tabpanel，
            // 单张卡没有 tab 指向它，不冒充 tabpanel 语义（避免悬空 aria-*）。
            role={pendings.length > 1 ? "tabpanel" : undefined}
            aria-labelledby={
              pendings.length > 1
                ? pendingDomId("zcode-pending-tab", pending.interactionId)
                : undefined
            }
            data-testid={`zcode-pending-slot-${pending.interactionId}`}
            data-active={pending.interactionId === activePendingId ? "true" : "false"}
            // hidden 与 inert 同时用：hidden 管视觉隐藏，inert 在**真实浏览器**里
            // 阻断隐藏组的 Tab/读屏/程序化激活（jsdom 未实现 inert 语义——其
            // focusing.js 里 inert 只有 TODO，故自动测试只能断属性存在；自动回环
            // 的实际隔离护栏是 hidden + 组内控件显式断言，浏览器侧语义列入实机待验）。
            // 多组卡内层 testid 同名，测试也必须 within(slot)。
            hidden={pending.interactionId !== activePendingId}
            inert={pending.interactionId !== activePendingId}
          >
            <ZcodePendingCard
              pending={pending}
              sessionId={sessionId}
              generation={session.generation}
              capabilities={session.capabilities}
              gateReason={gateReason}
              submitting={submitting}
              revision={session.revision}
              submit={submit}
            />
          </div>
        ))}
      {/* 官方输入队列（只读）：默认一行（数量 + 暂停），点开才列正文。
          放在 pending 卡之后、模型选择之前——运行中发送的排队事实比档位更贴近
          用户此刻的疑问（「我刚发的那条去哪了」）。未知/空队列不占位。 */}
      <ZcodeQueuePanel queue={session?.queue} />
      {selection !== null && (
        <div className="mb-2 flex flex-wrap items-center gap-2">
          <select
            data-testid="zcode-model-select"
            aria-label={i18n.t("mobile.zcodeControl.modelAria")}
            value={selection.model}
            disabled={!selectionEnabled}
            onChange={(e) => handleModelChange(e.target.value)}
            className="rounded-lg border border-[var(--cb)] px-2 py-1 text-sm text-[var(--tx)] disabled:opacity-40"
          >
            {selection.models.map((choice) => (
              <option key={choice.modelId} value={choice.modelId}>
                {choice.modelId}
              </option>
            ))}
          </select>
          <select
            data-testid="zcode-thought-select"
            aria-label={i18n.t("mobile.zcodeControl.thoughtAria")}
            value={currentThought}
            disabled={!selectionEnabled}
            onChange={(e) => handleThoughtChange(e.target.value)}
            className="rounded-lg border border-[var(--cb)] px-2 py-1 text-sm text-[var(--tx)] disabled:opacity-40"
          >
            {(currentChoice?.thoughtLevels ?? []).map((level) => (
              <option key={level} value={level}>
                {level}
              </option>
            ))}
          </select>
          {!selectionEnabled && (
            <p
              data-testid="zcode-selection-reason"
              data-reason-code={selectionReason}
              className="text-xs text-[var(--mut)]"
            >
              {zcodeReasonText(selectionReason)}
            </p>
          )}
        </div>
      )}
      {stopTargets.length > 0 && (
        <div className="mb-2 flex flex-wrap items-center gap-2">
          <button
            type="button"
            data-testid="zcode-interrupt"
            disabled={!canInterrupt}
            onClick={handleInterrupt}
            className="rounded-full border border-[var(--cb)] px-4 py-1 text-sm text-[var(--tx)] disabled:opacity-40"
          >
            {i18n.t("mobile.zcodeControl.interrupt")}
          </button>
          {!canInterrupt && interruptReason !== null && (
            <p
              data-testid="zcode-interrupt-reason"
              data-reason-code={interruptReason}
              className="text-xs text-[var(--mut)]"
            >
              {zcodeReasonText(interruptReason)}
            </p>
          )}
        </div>
      )}
      {attachments.length > 0 && (
        <div className="flex flex-wrap gap-2">
          {attachments.map((a) => (
            <span
              key={a.id}
              data-testid="zcode-attachment"
              data-status={a.status}
              title={a.error ?? a.path ?? undefined}
              className="max-w-full truncate rounded-md border border-[var(--cb)] px-2 py-1 text-xs text-[var(--tx)]"
            >
              {a.isImage ? "🖼" : "📎"} {a.name}
              {a.status === "uploading" && (
                <span className="ml-1 text-[var(--mut)]">
                  {typeof a.loaded === "number"
                    ? ` ${a.loaded}${a.total ? `/${a.total}` : ""}B`
                    : ""}
                </span>
              )}
              {a.status === "failed" && a.error && (
                <span className="ml-1 text-red-500">{a.error}</span>
              )}
              <button
                type="button"
                data-testid="zcode-attachment-remove"
                aria-label={i18n.t("mobile.zcodeControl.removeAttachment")}
                onClick={() => removeZcodeAttachment(a.id)}
                className="ml-1 text-[var(--mut)]"
              >
                ×
              </button>
            </span>
          ))}
        </div>
      )}
      {attachNotice !== null && (
        <p data-testid="zcode-attach-notice" className="text-xs text-amber-500">
          {i18n.t(
            attachNotice === "uploading"
              ? "mobile.zcodeControl.attachBlockedUploading"
              : "mobile.zcodeControl.attachBlockedFailed"
          )}
        </p>
      )}
      <div className="flex items-end gap-2">
        <button
          type="button"
          data-testid="zcode-attach"
          aria-label={i18n.t("mobile.zcodeControl.attachAria")}
          onClick={() => attachInputRef.current?.click()}
          className="shrink-0 rounded-full bg-[var(--btnp)]/10 px-3 py-2 text-sm text-[var(--tx)]"
        >
          +
        </button>
        <input
          ref={attachInputRef}
          data-testid="zcode-attachment-input"
          type="file"
          multiple
          className="hidden"
          onChange={(e) => {
            const files = Array.from(e.target.files ?? []);
            e.target.value = "";
            if (files.length > 0) addZcodeFiles(files);
          }}
        />
        <textarea
          data-testid="zcode-draft"
          aria-label={i18n.t("mobile.zcodeControl.draftAria")}
          value={draft}
          rows={2}
          onChange={(e) => {
            draftRef.current = e.target.value;
            setDraft(e.target.value);
          }}
          onPaste={handleZcodePaste}
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
      {sendCapabilityReason !== null && (
        <p
          data-testid="zcode-send-reason"
          data-reason-code={sendCapabilityReason}
          className="mt-1 text-xs text-[var(--mut)]"
        >
          {zcodeReasonText(sendCapabilityReason)}
        </p>
      )}
    </div>
  );
}
