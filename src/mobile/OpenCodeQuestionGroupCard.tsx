// OpenCode 原生整组问答卡（计划任务 10）：组内页内存草稿 + 一次确认提交整组答案。
//
// 纪律（逐条对应 shared-contract 与任务 10 简报）：
// - **草稿只在页内存**（React state，键 = 会话 + 表单 id）：选择 / 输入 / 前后翻题 / 清除 /
//   放弃**零请求**——不发终端按键，也不需要有原生清字接口；
// - **合法跳过 = 省略键**：清空（或没选）就是 `Skip`，绝不用空串顶替（空串对必答是 400）；
// - **必答 = `required === true`**：缺项当场列出并禁用提交，不必先打一次 400；
// - **Unsupported 整组只读**：任一字段规则未覆盖就整组不可写，并给出具体字段（用题面标题，
//   不是内部 key），绝不挑可答字段先写一次（那会把一次整组提交拆成两次真写）；
// - **选项漂移**：这一题的内容变了（类型/选项集/是否自定义），旧答案标成「待核对」并保留
//   可见，提交键关闭，直到用户显式复核（不静默沿用，也不静默丢弃）。判据是**逐字段内容
//   版本**而不是整表单 token：只删了一个字段不该把幸存题也锁住；
// - **孤儿字段**：草稿里多出当前表单没有的键（字段集缩小）⇒ 提交时省略、界面上一行告知，
//   绝不因此锁死整组；
// - **冻结**：点击帧捕获的草稿快照就是这一次提交的全部内容；迟到回执只在「草稿仍与快照
//   同一对象」时才清空，用户之后输入的文字一律不受影响；
// - **结果只以原生为准**：`confirmed`/`settled`/409 都只说「以原生投影为准」，
//   不判断是哪一端先完成的；未确认只给只读核对，**没有重发入口**；
// - 动作/表单/会话/代际串味一律不互改状态（`epochRef` 闸）。
import { useCallback, useLayoutEffect, useMemo, useRef, useState, type ReactElement } from "react";
import i18n from "@/i18n";
import InteractiveCard, { toneTokens } from "./InteractiveCard";
import QuestionChoiceRow from "./QuestionChoiceRow";
import { ApiError } from "./api";
import {
  addActionAnchor,
  hasUnresolvedAction,
  rememberActionBinding,
  removeActionAnchor,
} from "./opencodeActionAnchors";
import {
  isConclusiveState,
  isDefiniteRefusal,
  nativeErrorCode,
  nativeErrorKey,
  newNativeActionId,
  submitOpenCodeAction,
  verifyOpenCodeActionWithTimeout,
  type ActionView,
  type DraftAnswer,
  type FormField,
} from "./opencodeNativeApi";
import {
  DraftRefusal,
  applyDraftAnswer,
  buildDraftSubmission,
  draftIsEmpty,
  fieldIsRequired,
  fieldIsActive,
  fieldSignature,
  isFormAnswerable,
  missingRequiredFields,
  orphanAnswerKeys,
  reaffirmDraft,
  staleAnswerKeys,
  unsupportedFieldReason,
  type PendingForm,
  type StoredDraft,
} from "./opencodeQuestionDraft";

/** 空草稿值（模块级常量：保持引用稳定，避免每次渲染产生新对象）。 */
const SKIP: DraftAnswer = { kind: "skip" };
const NO_VALUES: Record<string, DraftAnswer> = {};

/** 提交回执（互斥槽：只保留最近一次用户动作的结论）。 */
type Receipt =
  | { kind: "confirmed" }
  | { kind: "settled" }
  | { kind: "unknown"; actionId: string; code: string | null }
  | { kind: "notSent"; code: string | null }
  | { kind: "notAccepted"; code: string | null }
  | { kind: "rejected"; text: string };

export interface OpenCodeQuestionGroupCardProps {
  sessionId: string;
  bindingToken: string;
  form: PendingForm;
  /** 权威视图重读（提交结论 / 409 后让原生投影说话）。 */
  onChanged: () => void;
  /** 表单定案信号（confirmed / settled / 409）：面板据此按 formId **只读回读**官方
   *  状态/答案（R5b）。本卡不保存答案、不把回执当结果；回调只是「该去读了」。 */
  onFormSettled?: (formId: string) => void;
  /** R3 复审 F1：会话级连接阻断（绑定未确认）时**只锁写不卸载**——草稿与题进度
   *  都在本卡本地状态里，卸载重建会丢答案。默认 false。 */
  writeBlocked?: boolean;
}

/** 受控码 → 用户文案（三级回落，任何一级都不上屏机器串）：与面板同一套判据。 */
function errorCopy(e: ApiError): string {
  const key = nativeErrorKey(nativeErrorCode(e));
  if (key !== null) return i18n.t(key);
  const reason = e.data?.reason;
  if (typeof reason === "string" && reason.trim() !== "") return reason;
  return i18n.t("mobile.openCodeNative.errors.generic");
}

/** 草稿拒绝码 → 当场可说的理由（缺必答单独由 `missingRequiredFields` 列名）。
 *  孤儿键不在此列：它是合法跳过，只在界面上做一行告知。 */
function refusalCopy(refusal: DraftRefusal): string {
  switch (refusal.code) {
    case "kind_mismatch":
      return i18n.t("mobile.openCodeNative.questions.refused.kindMismatch", {
        key: refusal.fieldKey ?? "",
      });
    case "not_in_options":
      return i18n.t("mobile.openCodeNative.questions.refused.notInOptions", {
        key: refusal.fieldKey ?? "",
      });
    case "label_instead_of_value":
      return i18n.t("mobile.openCodeNative.questions.refused.labelInsteadOfValue", {
        key: refusal.fieldKey ?? "",
      });
    case "missing_required":
      return i18n.t("mobile.openCodeNative.questions.refused.missingRequired", {
        key: refusal.fieldKey ?? "",
      });
    case "unsupported_form":
      return i18n.t("mobile.openCodeNative.questions.unsupportedReason");
  }
}

export default function OpenCodeQuestionGroupCard({
  sessionId,
  bindingToken,
  form,
  onChanged,
  onFormSettled,
  writeBlocked = false,
}: OpenCodeQuestionGroupCardProps): ReactElement {
  /** 每组草稿按「会话 + 表单 id」分开存：新组不污染旧组，同一会话里多组互不串味。 */
  const [drafts, setDrafts] = useState<Record<string, StoredDraft>>({});
  const [index, setIndex] = useState(0);
  const [submitting, setSubmitting] = useState(false);
  const [verifying, setVerifying] = useState(false);
  const [receipt, setReceipt] = useState<Receipt | null>(null);
  const [abandoned, setAbandoned] = useState(false);
  /** 同步在途闸：同一帧连点两次都进 handler 时 state 闭包还是旧值。 */
  const submittingRef = useRef(false);
  const verifyingRef = useRef(false);
  /** 代际：换会话 / 换表单 / 换绑定（token 轮换）都 +1；迟到回执按代际作废。 */
  const epochRef = useRef(0);

  const draftKey = `${sessionId}\u0000${bindingToken}\u0000${form.id}`;
  const stored = drafts[draftKey];
  const values = stored?.values ?? NO_VALUES;
  const token = form.context_token;

  /** 同一待答项按绑定隔离；旧动作由面板的 action ID 恢复锚继续核对。 */
  const itemIdentity = `${sessionId}\u0000${bindingToken}\u0000${form.id}`;
  /** 内容 token 变化也推进代际，在途回执一律作废。 */
  const identity = `${itemIdentity}\u0000${token}`;

  useLayoutEffect(() => {
    setReceipt(null);
    setAbandoned(false);
    setIndex(0);
  }, [itemIdentity]);

  useLayoutEffect(() => {
    epochRef.current += 1;
    submittingRef.current = false;
    verifyingRef.current = false;
    setSubmitting(false);
    setVerifying(false);
    // 注意：**不**清回执。绑定/内容换代后，同一待答项上的「本次零写出（原因是…）」
    // 仍是对这一项的有效说明；清掉它会让用户只看到一次刷新、看不到为什么失败。
  }, [identity]);

  const staleKeys = useMemo(() => staleAnswerKeys(form, stored), [form, stored]);
  const staleSet = useMemo(() => new Set(staleKeys), [staleKeys]);
  const orphans = useMemo(() => orphanAnswerKeys(form, stored), [form, stored]);
  const missing = useMemo(() => missingRequiredFields(form, values), [form, values]);

  /** 整组编码（渲染期算一次：能不能提交、以及真要提交时的那一份答案都用它）。 */
  const check = useMemo(() => {
    try {
      return { ok: true as const, answers: buildDraftSubmission(form, values) };
    } catch (e) {
      return {
        ok: false as const,
        refusal: e instanceof DraftRefusal ? e : new DraftRefusal("unsupported_form", null),
      };
    }
  }, [form, values]);

  /** 写入口已撤下的判据：结论已定（confirmed/settled）或结果未确认（unknown）。 */
  const locked =
    receipt !== null &&
    (receipt.kind === "confirmed" || receipt.kind === "settled" || receipt.kind === "unknown");
  const canSubmit =
    isFormAnswerable(form) &&
    !locked &&
    !submitting &&
    !writeBlocked &&
    staleKeys.length === 0 &&
    check.ok &&
    !abandoned;

  /** 写入一个字段：把**该字段当前的内容版本**记进 `touched`（漂移判据是逐字段的）。 */
  const updateDraft = useCallback(
    (field: FormField, value: DraftAnswer) => {
      setAbandoned(false);
      setDrafts((prev) => ({
        ...prev,
        [draftKey]: applyDraftAnswer(prev[draftKey], fieldSignature(field), field.key, value),
      }));
    },
    [draftKey]
  );

  /** 结论落地（提交与只读核对共用）：**永不重发**。
   *  `frozen` 只在提交那一帧给出；只有「草稿对象仍是那一份」才清空（迟到回执不得改新输入）。 */
  const settle = useCallback(
    (action: ActionView, actionId: string, frozen: Record<string, DraftAnswer> | null) => {
      // 恢复锚（G3）：结论性状态（confirmed/settled/notSent/notAccepted）才删；
      // acknowledged/dispatching/unconfirmed 保留，供卡片卸载后从面板继续只读核对。
      if (isConclusiveState(action.state)) removeActionAnchor(sessionId, actionId);
      switch (action.state) {
        case "confirmed":
          setReceipt({ kind: "confirmed" });
          break;
        case "settled":
          setReceipt({ kind: "settled" });
          break;
        case "not_sent":
          setReceipt({ kind: "notSent", code: action.reasonCode ?? null });
          break;
        case "not_accepted":
          setReceipt({ kind: "notAccepted", code: action.reasonCode ?? null });
          break;
        default:
          // acknowledged / dispatching / unconfirmed：只给核对
          setReceipt({ kind: "unknown", actionId, code: action.reasonCode ?? null });
          break;
      }
      if (isConclusiveState(action.state)) onChanged(); // 让权威投影说话（原生待答项是否消失）
      // 已定案：让面板按原 formId 只读回读官方状态/答案（R5b）。答案不经过本组件。
      if (action.state === "confirmed" || action.state === "settled") {
        onFormSettled?.(form.id);
      }
      if (frozen !== null && (action.state === "confirmed" || action.state === "settled")) {
        // 冻结：草稿在提交之后被改过（新对象）就保留，绝不覆盖用户后来输入的文字
        setDrafts((prev) => {
          const cur = prev[draftKey];
          if (cur === undefined || cur.values !== frozen) return prev;
          const next = { ...prev };
          delete next[draftKey];
          return next;
        });
      }
    },
    [draftKey, form.id, onChanged, onFormSettled, sessionId]
  );

  /** 提交（唯一写入口；一次点击 = 一个 actionId = 最多一次派发）。 */
  const handleSubmit = useCallback(async () => {
    if (writeBlocked) return; // 连接阻断：连一帧内的竞态点击也零写出
    if (
      !canSubmit ||
      submittingRef.current ||
      !check.ok ||
      hasUnresolvedAction(sessionId, "answer", form.id, bindingToken)
    )
      return;
    submittingRef.current = true;
    const frozen = values; // 点击帧冻结：这份快照就是本次提交的全部内容
    const answers = check.answers;
    const actionId = newNativeActionId();
    // 恢复锚：**派发前**就落（只含 id），结果未知时刷新/卸载后仍有只读核对入口
    rememberActionBinding(actionId, bindingToken);
    addActionAnchor(sessionId, { actionId, kind: "answer", targetId: form.id });
    const myEpoch = epochRef.current;
    setSubmitting(true);
    setReceipt(null);
    try {
      const action = await submitOpenCodeAction({
        actionId,
        sessionId,
        bindingToken,
        operation: {
          kind: "answer",
          formId: form.id,
          contextToken: token,
          answers,
        },
      });
      if (epochRef.current !== myEpoch) return; // 迟到回执：不落地
      settle(action, actionId, frozen);
    } catch (e) {
      if (epochRef.current !== myEpoch) return;
      const err = e instanceof ApiError ? e : new ApiError(null, String(e));
      const code = nativeErrorCode(err);
      if (err.status === 409 && code === "OC_REQUEST_SETTLED") {
        // 原生侧已定案：只呈现原生结果（重读投影），不归因、不重发
        removeActionAnchor(sessionId, actionId);
        setReceipt({ kind: "settled" });
        onChanged();
        // 409 = 表单已定案：按原 formId 只读回读官方状态/答案（R5b 的「409 后展示原生结果」）。
        onFormSettled?.(form.id);
        return;
      }
      if (err.status !== null && isDefiniteRefusal(code)) {
        // 派发前受控拒绝：确定零写出 → 保留草稿、恢复可写。
        // 同时重读视图：`OC_BINDING_STALE` / `OC_ROUTE_CHANGED` 说明手上这份
        // bindingToken / contextToken 已经过期，不刷新的话用户只会拿同一份旧凭据再失败一次。
        removeActionAnchor(sessionId, actionId); // 零写出：锚已无意义
        setReceipt({ kind: "rejected", text: errorCopy(err) });
        onChanged();
        return;
      }
      // 其余一律「结果未确认」（含传输类 409、网络层异常、未收录码）：锁写，只给核对
      setReceipt({ kind: "unknown", actionId, code });
      if (err.status === 409) {
        onChanged();
        // 409 说明该表单在原生侧的结果需要回读（是否定案以 detail 为准）。
        onFormSettled?.(form.id);
      }
    } finally {
      if (epochRef.current === myEpoch) {
        submittingRef.current = false;
        setSubmitting(false);
      }
    }
  }, [
    bindingToken,
    canSubmit,
    check,
    form.id,
    onChanged,
    onFormSettled,
    sessionId,
    settle,
    token,
    values,
    writeBlocked,
  ]);

  /** 只读核对（不是重发）：结论按权威账本收敛；核对永远不动草稿。
   *  15s 超时/中止/404 都只是「没读到」：保持「未确认」、清在途闸、允许重试，
   *  绝不折成零写出（404 在首次派发 reserve 之前是正常形态）。 */
  const handleVerify = useCallback(async () => {
    if (receipt === null || receipt.kind !== "unknown" || verifyingRef.current) return;
    const actionId = receipt.actionId;
    verifyingRef.current = true;
    const myEpoch = epochRef.current;
    setVerifying(true);
    try {
      const action = await verifyOpenCodeActionWithTimeout(actionId);
      if (epochRef.current !== myEpoch) return;
      settle(action, actionId, null);
    } catch (e) {
      if (epochRef.current !== myEpoch) return;
      const err = e instanceof ApiError ? e : new ApiError(null, String(e));
      // 核对失败/超时/中止/404 都不是动作结论：保持「未确认」（不清草稿、不放开写）
      setReceipt({ kind: "unknown", actionId, code: nativeErrorCode(err) });
    } finally {
      verifyingRef.current = false;
      setVerifying(false);
    }
  }, [receipt, settle]);

  /** 明确放弃本组草稿：丢弃本组的页内存答案（不产生任何请求）。 */
  const handleAbandon = useCallback(() => {
    if (submittingRef.current) return;
    epochRef.current += 1;
    setDrafts((prev) => {
      const next = { ...prev };
      delete next[draftKey];
      return next;
    });
    setReceipt(null);
    setAbandoned(true);
  }, [draftKey]);

  // ===== Unsupported：整组只读（不进入任何可写分支）=====
  if (!isFormAnswerable(form)) {
    return (
      <InteractiveCard
        tone="question"
        testId="opencode-question-card"
        mode="unsupported"
        pulsing={false}
        title={i18n.t("mobile.openCodeNative.pending.title")}
        titleSuffix={
          <span data-testid="opencode-q-form-title" className="text-xs text-[var(--mut)]">
            {form.title}
          </span>
        }
        dataAttributes={{ "form-id": form.id, readonly: "true" }}
      >
        <p
          data-testid="opencode-q-unsupported"
          className="mt-1 text-xs text-amber-700 dark:text-amber-300"
        >
          {i18n.t("mobile.openCodeNative.questions.unsupported", {
            fields: unsupportedList(form),
          })}
        </p>
      </InteractiveCard>
    );
  }

  // ===== 空表单：没有可作答字段就不提交空答案（零写）=====
  if (form.fields.length === 0) {
    return (
      <InteractiveCard
        tone="question"
        testId="opencode-question-card"
        mode="empty"
        pulsing={false}
        title={i18n.t("mobile.openCodeNative.pending.title")}
        titleSuffix={
          <span data-testid="opencode-q-form-title" className="text-xs text-[var(--mut)]">
            {form.title}
          </span>
        }
        dataAttributes={{ "form-id": form.id, readonly: "true" }}
      >
        <p data-testid="opencode-q-empty" className="mt-1 text-xs text-[var(--mut)]">
          {i18n.t("mobile.openCodeNative.questions.emptyForm")}
        </p>
      </InteractiveCard>
    );
  }

  const fields = form.fields.filter((field) => fieldIsActive(field, values));
  const current = Math.max(0, Math.min(index, fields.length - 1));
  const field = fields[current];
  const draft = values[field.key] ?? SKIP;

  const receiptCopy = receipt === null ? null : receiptText(receipt);

  // 视觉沿用既有蓝色问答卡（InteractiveCard tone="question" + QuestionChoiceRow）：
  // 题干、选项与主按钮的层级和 QuestionCard 一致，只是内容来自原生整组表单。
  const tone = toneTokens("question");
  return (
    <InteractiveCard
      tone="question"
      testId="opencode-question-card"
      mode="group"
      title={i18n.t("mobile.openCodeNative.pending.title")}
      titleSuffix={
        <>
          <span data-testid="opencode-q-form-title" className="text-xs text-[var(--mut)]">
            {form.title}
          </span>
          <span data-testid="opencode-q-progress" className="ml-1 text-xs text-[var(--mut)]">
            {i18n.t("mobile.openCodeNative.questions.progress", {
              current: current + 1,
              total: fields.length,
            })}
          </span>
        </>
      }
      dataAttributes={{ "form-id": form.id }}
    >
      {/* 组内跳题（纯本地：只改 index） */}
      <div className="mt-1 flex flex-wrap gap-1">
        {fields.map((item, i) => (
          <button
            key={item.key}
            type="button"
            data-testid="opencode-q-jump"
            data-key={item.key}
            data-answered={draftIsEmpty(values[item.key]) ? "false" : "true"}
            data-current={i === current ? "true" : "false"}
            data-stale={staleSet.has(item.key) ? "true" : "false"}
            onClick={() => setIndex(i)}
            aria-label={item.title}
            className={
              i === current
                ? "rounded-full bg-[var(--btnp)]/25 px-1.5 py-0.5 text-[var(--tx)]"
                : "rounded-full bg-[var(--cd)] px-1.5 py-0.5 text-[var(--mut)]"
            }
          >
            {i + 1}
            {staleSet.has(item.key) ? " •" : ""}
          </button>
        ))}
      </div>

      {/* 旧字段（孤儿）：只告知，不阻塞——提交时省略它们 */}
      {orphans.length > 0 && (
        <p data-testid="opencode-q-orphans" className="mt-1 text-[var(--mut)]">
          {i18n.t("mobile.openCodeNative.questions.orphans", { count: orphans.length })}
        </p>
      )}

      {/* 选项漂移：旧答案待核对（可见、不可提交）；下方每题另有逐字段标记 */}
      {staleKeys.length > 0 && (
        <div className="mt-1 rounded-lg bg-amber-500/10 px-2 py-1">
          <p data-testid="opencode-q-stale" className="text-amber-700 dark:text-amber-300">
            {i18n.t("mobile.openCodeNative.questions.needsReview")}
          </p>
          <button
            type="button"
            data-testid="opencode-q-reaffirm"
            onClick={() => {
              const cur = drafts[draftKey];
              if (cur === undefined) return;
              setDrafts((prev) => ({ ...prev, [draftKey]: reaffirmDraft(form, cur) }));
            }}
            className={SMALL_BUTTON}
          >
            {i18n.t("mobile.openCodeNative.questions.reaffirm")}
          </button>
        </div>
      )}

      {/* 当前题：题面 + 必答标记 */}
      <p data-testid="opencode-q-title" className="mt-1 text-[var(--tx)]">
        {field.title}
        <span data-testid="opencode-q-required" className="ml-1 text-[var(--mut)]">
          {fieldIsRequired(field)
            ? i18n.t("mobile.openCodeNative.questions.required")
            : i18n.t("mobile.openCodeNative.questions.optional")}
        </span>
      </p>

      {/* 题干正文（官方 `description`）：`title` 只是短标题，真正的提问在这里。
          空白视同没有（不渲染空行）；老载荷缺键时整块不出现。 */}
      {field.description !== undefined &&
        field.description !== null &&
        field.description.trim() !== "" && (
          <p data-testid="opencode-q-description" className="mt-0.5 text-[var(--tx)]">
            {field.description}
          </p>
        )}

      {/* 逐字段漂移标记（M-3）：告诉用户**哪一题**变了，而不是只有一条整组黄条 */}
      {staleSet.has(field.key) && (
        <p data-testid="opencode-q-stale-field" className="text-amber-700 dark:text-amber-300">
          {i18n.t("mobile.openCodeNative.questions.staleField")}
        </p>
      )}

      <FieldEditor field={field} draft={draft} onChange={(v) => updateDraft(field, v)} />

      {legacyValues(field, draft).length > 0 && (
        <p data-testid="opencode-q-legacy" className="mt-0.5 text-[var(--mut)]">
          {i18n.t("mobile.openCodeNative.questions.legacyValue", {
            values: legacyValues(field, draft).join("、"),
          })}
        </p>
      )}

      <div className="mt-1 flex flex-wrap gap-2">
        <button
          type="button"
          data-testid="opencode-q-prev"
          disabled={current === 0}
          onClick={() => setIndex((i) => Math.max(0, Math.min(i, fields.length - 1) - 1))}
          className={SMALL_BUTTON}
        >
          {i18n.t("mobile.openCodeNative.questions.prev")}
        </button>
        <button
          type="button"
          data-testid="opencode-q-next"
          disabled={current >= fields.length - 1}
          onClick={() => setIndex((i) => Math.min(fields.length - 1, i + 1))}
          className={SMALL_BUTTON}
        >
          {i18n.t("mobile.openCodeNative.questions.next")}
        </button>
        <button
          type="button"
          data-testid="opencode-q-clear"
          onClick={() => updateDraft(field, SKIP)}
          className={SMALL_BUTTON}
        >
          {i18n.t("mobile.openCodeNative.questions.clear")}
        </button>
      </div>

      {/* 缺必答：当场列出（不产生请求） */}
      {missing.length > 0 && (
        <p data-testid="opencode-q-missing" className="mt-1 text-amber-700 dark:text-amber-300">
          {i18n.t("mobile.openCodeNative.questions.missingRequired", {
            titles: missing.map((item) => item.title).join("、"),
          })}
        </p>
      )}
      {!check.ok && check.refusal.code !== "missing_required" && (
        <p data-testid="opencode-q-blocked" className="mt-1 text-rose-600 dark:text-rose-400">
          {refusalCopy(check.refusal)}
        </p>
      )}

      {abandoned && (
        <p data-testid="opencode-q-abandoned" className="mt-1 text-[var(--mut)]">
          {i18n.t("mobile.openCodeNative.questions.abandoned")}
        </p>
      )}

      {/* 回执槽（提交 / 核对结论） */}
      {receipt !== null && receiptCopy !== null && (
        <div className="mt-1">
          <p data-testid="opencode-q-receipt" data-kind={receipt.kind} className={RECEIPT_CLASS}>
            {receiptCopy}
          </p>
          {"code" in receipt && receipt.code !== null && (
            <p data-testid="opencode-q-receipt-code" className="text-[var(--mut)]">
              {i18n.t("mobile.openCodeNative.questions.receipt.code", { code: receipt.code })}
            </p>
          )}
          {receipt.kind === "unknown" && (
            <button
              type="button"
              data-testid="opencode-q-verify"
              disabled={verifying}
              onClick={() => void handleVerify()}
              className={SMALL_BUTTON}
            >
              {verifying
                ? i18n.t("mobile.openCodeNative.questions.receipt.verifying")
                : i18n.t("mobile.openCodeNative.questions.receipt.verify")}
            </button>
          )}
        </div>
      )}

      <div className="mt-1 flex flex-wrap gap-2">
        {/* 已定案/未确认：**不渲染**提交键（不是禁用），写入口真的撤下；
            未确认时唯一的出路是上面的只读核对，这里没有重发。 */}
        {!locked && (
          <button
            type="button"
            data-testid="opencode-q-submit"
            disabled={!canSubmit}
            onClick={() => void handleSubmit()}
            className={`w-full rounded-full px-3 py-1.5 text-xs font-medium disabled:opacity-40 ${tone.action}`}
          >
            {submitting
              ? i18n.t("mobile.openCodeNative.questions.submitting")
              : i18n.t("mobile.openCodeNative.questions.submit")}
          </button>
        )}
        <button
          type="button"
          data-testid="opencode-q-abandon"
          onClick={handleAbandon}
          className={SMALL_BUTTON}
        >
          {i18n.t("mobile.openCodeNative.questions.abandon")}
        </button>
      </div>
    </InteractiveCard>
  );
}

/** Unsupported 字段清单（`题面标题（理由）`；标题为空时退回内部 key）。
 *  用 title 而不是 key：用户看到的是题面，key 是协议里的东西（评审 M-9）。 */
function unsupportedList(form: PendingForm): string {
  return form.fields
    .filter((field) => unsupportedFieldReason(field) !== null)
    .map((field) => {
      const label = field.title.trim() === "" ? field.key : field.title;
      return `${label}（${i18n.t("mobile.openCodeNative.questions.unsupportedReason")}）`;
    })
    .join("、");
}

/** 当前草稿里已不在选项集内的值（旧答案：仍要可见，供用户核对）。
 *
 *  只在**选项集封闭**（`custom:false` 且选项非空）时成立：`custom:true` 或纯文本框下，
 *  任意自由文本本来就是合法答案，不能当成陈旧值（否则用户自己的输入会被当成旧答案藏起来）。 */
function legacyValues(field: FormField, draft: DraftAnswer): string[] {
  const kind = field.kind;
  if (kind.kind === "unsupported") return [];
  if (kind.custom || kind.options.length === 0) return [];
  const known = new Set(kind.options.map((option) => option.value));
  if (draft.kind === "text")
    return draft.value !== "" && !known.has(draft.value) ? [draft.value] : [];
  if (draft.kind === "multi") return draft.value.filter((value) => !known.has(value));
  return [];
}

/** 单题编辑器：显示 `label`、提交 `value`（两者永不互换）。 */
function FieldEditor({
  field,
  draft,
  onChange,
}: {
  field: FormField;
  draft: DraftAnswer;
  onChange: (value: DraftAnswer) => void;
}): ReactElement | null {
  const kind = field.kind;
  if (kind.kind === "unsupported") return null;

  const legacy = new Set(legacyValues(field, draft));
  const selectedText = draft.kind === "text" && !legacy.has(draft.value) ? draft.value : "";
  const selectedMulti = draft.kind === "multi" ? draft.value.filter((v) => !legacy.has(v)) : [];

  if (kind.kind === "multi") {
    return (
      <MultiFieldEditor
        key={`${field.key}:${fieldSignature(field)}`}
        kind={kind}
        fieldKey={field.key}
        draft={draft}
        values={selectedMulti}
        onChange={onChange}
      />
    );
  }

  const showInput = kind.custom || kind.options.length === 0;
  // 选了选项时，自由文本框回显**用户点过的 label**（不是原生 value）：提交仍用 value，
  // 但不能把 `q0-red` 这种协议值摆在用户面前（评审 M-11）。
  const pickedOption = kind.options.find(
    (option) => draft.kind === "text" && draft.value === option.value
  );
  const inputValue = pickedOption !== undefined ? pickedOption.label : selectedText;
  return (
    <div className="mt-1">
      {/* 选项行沿用既有蓝色问答卡的选项组件（编号徽标 + 粗标题 + 描述 + 自动换行）；
          data-* 钩子挂在包裹层上，保持不变。再点已选项 = 清除（编码为 Skip，不是空串）。 */}
      <div className="flex flex-col gap-1">
        {kind.options.map((option, index) => {
          const on = draft.kind === "text" && draft.value === option.value;
          return (
            <QuestionChoiceRow
              key={option.value}
              index={index}
              label={option.label}
              description={option.description ?? undefined}
              checked={on}
              testId="opencode-q-option"
              dataAttributes={{
                "data-key": field.key,
                "data-value": option.value,
                "data-selected": on ? "true" : "false",
              }}
              onClick={() => onChange(on ? SKIP : { kind: "text", value: option.value })}
            />
          );
        })}
      </div>
      {showInput && (
        <textarea
          data-testid="opencode-q-text"
          value={inputValue}
          rows={kind.options.length === 0 ? 2 : 1}
          onChange={(e) =>
            onChange(e.target.value === "" ? SKIP : { kind: "text", value: e.target.value })
          }
          placeholder={i18n.t("mobile.openCodeNative.questions.freeText")}
          className="mt-1 w-full resize-none rounded-lg bg-[var(--cd)] px-2 py-1 text-xs text-[var(--tx)] outline-none"
        />
      )}
    </div>
  );
}

interface MultiEditorRow {
  id: number;
  value: string;
  custom: boolean;
}

/** 输入行身份独立于原生值：输入前缀碰巧等于选项时也不能换行或吞字。
 * 每次编辑仍同步到父组件的唯一提交草稿；这里仅保留行身份和输入来源。 */
function MultiFieldEditor({
  kind,
  fieldKey,
  draft,
  values,
  onChange,
}: {
  kind: Extract<FormField["kind"], { kind: "multi" }>;
  fieldKey: string;
  draft: DraftAnswer;
  values: string[];
  onChange: (value: DraftAnswer) => void;
}): ReactElement | null {
  const initialize = () => ({
    source: draft,
    nextId: values.length + 1,
    rows: [
      ...values.map((value, id) => ({
        id,
        value,
        custom: !kind.options.some((option) => option.value === value),
      })),
      { id: values.length, value: "", custom: true },
    ],
  });
  const [editor, setEditor] = useState(initialize);
  // 清空、换草稿或正式回执后的外部变化必须同步，不能由局部编辑状态复活旧答案。
  if (editor.source !== draft) {
    setEditor(initialize());
    return null;
  }
  const update = (rows: MultiEditorRow[]) => {
    let nextId = Math.max(editor.nextId, ...rows.map((row) => row.id + 1));
    if (!rows.some((row) => row.custom && row.value === "")) {
      rows = [...rows, { id: nextId++, value: "", custom: true }];
    }
    const selected = rows.filter((row) => row.value !== "").map((row) => row.value);
    const source: DraftAnswer = selected.length === 0 ? SKIP : { kind: "multi", value: selected };
    setEditor({ source, rows, nextId });
    onChange(source);
  };
  return (
    <div className="mt-1 flex flex-col gap-1">
      {kind.options.map((option, index) => {
        const on = editor.rows.some((row) => !row.custom && row.value === option.value);
        return (
          <QuestionChoiceRow
            key={option.value}
            index={index}
            label={option.label}
            description={option.description ?? undefined}
            checked={on}
            multiSelect
            testId="opencode-q-option"
            dataAttributes={{
              "data-key": fieldKey,
              "data-value": option.value,
              "data-selected": on ? "true" : "false",
            }}
            onClick={() => {
              if (on) update(editor.rows.filter((row) => row.custom || row.value !== option.value));
              else
                update([...editor.rows, { id: editor.nextId, value: option.value, custom: false }]);
            }}
          />
        );
      })}
      {(kind.custom || kind.options.length === 0) &&
        editor.rows
          .filter((row) => row.custom)
          .map((row, index) => (
            <textarea
              key={row.id}
              data-testid="opencode-q-multi-custom"
              aria-label={i18n.t("mobile.openCodeNative.questions.customItem", {
                index: index + 1,
              })}
              value={row.value}
              rows={1}
              placeholder={i18n.t("mobile.openCodeNative.questions.freeText")}
              onChange={(event) =>
                update(
                  editor.rows.map((item) =>
                    item.id === row.id ? { ...item, value: event.target.value } : item
                  )
                )
              }
              className="mt-1 w-full resize-none rounded-lg bg-[var(--cd)] px-2 py-1 text-xs text-[var(--tx)] outline-none"
            />
          ))}
    </div>
  );
}

/** 回执文案（一态一句，全部只说「以原生投影为准」；不出现「哪一端获胜」的断言）。 */
function receiptText(receipt: Receipt): string {
  switch (receipt.kind) {
    case "confirmed":
      return i18n.t("mobile.openCodeNative.questions.receipt.confirmed");
    case "settled":
      return i18n.t("mobile.openCodeNative.questions.receipt.settled");
    case "unknown":
      return i18n.t("mobile.openCodeNative.questions.receipt.unconfirmed");
    case "notSent":
      return i18n.t("mobile.openCodeNative.questions.receipt.notSent");
    case "notAccepted":
      return i18n.t("mobile.openCodeNative.questions.receipt.notAccepted");
    case "rejected":
      return receipt.text;
  }
}

const SMALL_BUTTON =
  "rounded-full bg-[var(--cd)] px-2 py-0.5 text-xs text-[var(--tx)] disabled:opacity-40";
const RECEIPT_CLASS = "text-[var(--tx)]";
