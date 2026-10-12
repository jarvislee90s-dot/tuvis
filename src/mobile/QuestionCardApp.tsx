// 移动端问答卡 · **APP 形态专属**（Task 8 新建，裁决 28b 降级补投语义）。
// 与 CLI 版（QuestionCardCli）双套分置、零共享可变状态：CLI 版的屏读权威源机制
//（screen 快照、checked 翻转核验、heading 对位、noteText 回填）与九段键序阶段机
// 在 APP 会话上没有控制台可用——本件的作答语义是**补投**（repost）：
// - 读侧照旧：GET /session-question（题目来自 rollout 事件流，读链不动）；APP 面
//   `screen` 恒 null（无屏读快照）——本件不消费任何快照通道；
// - 写侧：有作答文本的动作（select/toggle/freeText）经 H8 queue 通道**补投为消息**
//  （模板由后端组装：单选=「选择第 N 项：<文本>」/ 多选逐项=「勾选第 N 项：<文本>」/
//   自由作答=原文）——自然语言补投**不保证**选项翻转，回执只说「已补投为消息，
//   请回 APP 核对题面」+ `screen:null`（设计 §3.3 红线：永不声称已作答、不显示翻转）；
// - 无作答文本的动作（切题/提交/中止）后端如实 409 `app_form_no_repost`
//  （reason 含「清空作答」说明）——本件不提供这三类入口，错误码到达时如实透出。
// **APP 词汇表**（设计 §3.7(a)）：渲染输出禁「终端/屏读/按键」。
import { useCallback, useEffect, useRef, useState } from "react";
import InteractiveCard from "./InteractiveCard";
// 换题指纹判据与 CLI 套**同源**（收口 I-1）：questionFingerprint 是纯函数
//（题干 + 选项摘要），抽共享不复制第二份（两处各抄一份迟早漂移——CLI 套的
// 同款注释引 rollout-2026-09-21 实机事故）
import { questionFingerprint } from "./QuestionCardCli";
import {
  ApiError,
  fetchSessionQuestion,
  questionAnswerErrorCopy,
  sessionQuestionAnswerRepost,
  type QuestionAnswerAction,
  type QuestionInfoView,
  type QuestionView,
} from "./api";

/** 自由作答长度上限（与 CLI 套 MAX_FREE_TEXT_CHARS / composer MAX_SEND_CHARS 同口径） */
const MAX_FREE_TEXT_CHARS = 10000;

/** 回执引导缺省文案（后端 note 的钉句——Task 6b `CODEX_APP_REPOST_NOTE`；仅在
 *  后端缺字段时兜底，不另编第二套话） */
const REPOST_NOTE_FALLBACK = "已补投为消息，请回 APP 核对题面";

/** 补投文本的**展示镜像**（纯展示，不参与请求组装——请求只发 action/index/text，
 *  模板由后端 `codex_app_answer_repost_text` 单点组装；本函数仅镜像同一模板供
 *  用户预览「将要补投什么」，词表漂移时以后端为准）。 */
function repostPreviewText(
  action: QuestionAnswerAction,
  index: number | undefined,
  text: string | undefined,
  q: QuestionView | undefined
): string {
  if (action === "select" || action === "toggle") {
    const i = index ?? 0;
    const label = q?.options[i]?.label ?? `第 ${i + 1} 项`;
    return `${action === "select" ? "选择" : "勾选"}第 ${i + 1} 项：${label}`;
  }
  return text ?? "";
}

export default function QuestionCardApp({
  session,
}: {
  /** 会话：只消费 id（请求键）与 status（状态跃迁重拉触发键——CLI 套同惯例） */
  session: { id: string; status?: string };
}) {
  const [info, setInfo] = useState<QuestionInfoView | null>(null);
  const [ready, setReady] = useState(false);
  const [busy, setBusy] = useState(false);
  /** 最近一次补投的展示镜像（busy=「正在补投」/ 空闲=「最近补投」——持续在场，
   *  补投语义下没有「终态」概念，用户随时可再投） */
  const [postedPreview, setPostedPreview] = useState<string | null>(null);
  /** 回执引导（receipt.status ∈ ok|queued 时后端 note 原样透出） */
  const [note, setNote] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  /** 逐题自由作答草稿（补投成功即清——已投出；失败保留可重试） */
  const [freeTexts, setFreeTexts] = useState<Record<number, string>>({});

  const status = session.status;
  // 上一轮载荷的**内容指纹**（收口 I-1）：换题检测判据，与 CLI 套 F2-1 同源
  const lastFingerprint = useRef<string | null>(null);
  useEffect(() => {
    let alive = true;
    setReady(false);
    setError(null);
    fetchSessionQuestion(session.id)
      .then((v) => {
        if (!alive) return;
        // **换题指纹重置**（收口 I-1）：重拉 effect 此前只清 error——载荷换成新题时
        // 旧 note 谎称「已补投」（那是旧题的回执）、旧草稿灌进新题输入框、旧「最近
        // 补投」预览留痕。判据与 CLI 套同款（questionFingerprint；rollout-2026-09-21
        // 实机事故同一族）。**空载荷不参与判据**：「拉不到题」≠「换了题」（CLI 套
        // 同款边界——available=false 的空窗不该把本轮痕迹误清）。
        const fp = v.available && v.questions.length > 0 ? questionFingerprint(v) : "";
        if (
          lastFingerprint.current !== null &&
          lastFingerprint.current !== "" &&
          fp !== "" &&
          lastFingerprint.current !== fp
        ) {
          setNote(null);
          setPostedPreview(null);
          setFreeTexts({});
        }
        if (fp !== "") lastFingerprint.current = fp;
        setInfo(v);
        setReady(true);
      })
      .catch(() => {
        if (alive) setReady(false);
      });
    return () => {
      alive = false;
    };
  }, [session.id, status]);

  const repost = useCallback(
    async (
      action: QuestionAnswerAction,
      index: number | undefined,
      text: string | undefined,
      questionIndex: number
    ) => {
      if (busy || info === null) return;
      setBusy(true);
      setError(null);
      setNote(null);
      setPostedPreview(repostPreviewText(action, index, text, info.questions[questionIndex]));
      try {
        const res = await sessionQuestionAnswerRepost(
          session.id,
          action,
          index,
          text,
          questionIndex
        );
        if (res.status === "headless") {
          // APP 补投回执 = 无头封套（Task 6b）：ok/queued 附 note 引导核对；
          // failed/cancelled 只带原因（不带 note——不谎报送达）。`screen` 恒 null，
          // 本件不消费（永不声称已作答——红线）
          const r = res.receipt;
          if (r.status === "ok" || r.status === "queued") {
            setNote(res.note ?? REPOST_NOTE_FALLBACK);
            if (action === "freeText") {
              setFreeTexts((prev) => ({ ...prev, [questionIndex]: "" }));
            }
          } else {
            setError(r.reason ?? "补投未成功，请重试或在 APP 内操作");
          }
        } else if (res.status === "failed") {
          // 防御分支：APP 面不应到达（后端 6b 已翻转）——如实透出
          setError(res.error);
        } else {
          // key_sent：CLI 面回执词在 APP 会话上不可达（防御分支）——**不得按补投成功
          // 处理**（spec 审 m-1：note 会谎称已补投送达）；如实报未知形态并引导核对
          setError("收到未知回执形态，请回 APP 核对");
        }
      } catch (e) {
        if (e instanceof ApiError) {
          // app_form_no_repost：无作答文本动作的如实拒（reason 含「清空作答」口径，
          // 单点来自后端）；其余错误码走与 CLI 套同一份 questionAnswerErrorCopy
          const reason = e.data?.reason;
          if (typeof reason === "string") {
            setError(reason);
          } else {
            setError(questionAnswerErrorCopy(e));
          }
        } else {
          setError(String(e));
        }
      } finally {
        setBusy(false);
      }
    },
    [busy, session.id, info]
  );

  // 加载中 / 拉取失败 / 不可用 / 无题目：不渲染（CLI 套同惯例的自隐契约）
  if (!ready || info === null || !info.available) return null;
  const questions = info.questions;
  if (questions.length === 0) return null;

  return (
    <InteractiveCard
      tone="question"
      testId="question-card-app"
      mode={questions.some((q) => q.multiSelect) ? "multi" : "single"}
      pulsing={false}
      title={questions.length > 1 ? `有 ${questions.length} 个问题等待回答` : "等待回答"}
    >
      <p data-testid="question-app-intro" className="mt-1 text-[11px] text-[var(--mut)]">
        作答以「补投为消息」送达会话——是否生效请回 APP 核对题面
      </p>
      {questions.map((q, qi) => (
        <div key={`question-app-${qi}`} data-testid={`question-app-q-${qi}`} className="mt-2">
          {q.header && (
            <span
              data-testid={`question-app-header-${qi}`}
              className="mr-1 rounded bg-[var(--btnp)]/10 px-1.5 py-0.5 text-[10px] font-medium text-[var(--tx)]"
            >
              {q.header}
            </span>
          )}
          <span data-testid={`question-app-text-${qi}`} className="text-sm text-[var(--tx)]">
            {q.question}
            {q.multiSelect && (
              <span className="ml-1 rounded bg-[var(--btnp)]/10 px-1 py-0.5 text-[10px] font-medium text-[var(--tx)]">
                多选
              </span>
            )}
          </span>
          <div className="mt-1.5 space-y-1">
            {q.options.map((o, i) => (
              <button
                key={`question-app-opt-${qi}-${i}`}
                type="button"
                data-testid={`question-app-option-${qi}-${i}`}
                disabled={busy}
                onClick={() => repost(q.multiSelect ? "toggle" : "select", i, undefined, qi)}
                className="flex w-full items-start gap-2 rounded-lg bg-[var(--btnp)]/5 px-2.5 py-1.5 text-left text-sm text-[var(--tx)] hover:bg-[var(--btnp)]/10 disabled:opacity-40 dark:hover:bg-[var(--bub)]"
              >
                <span className="mt-0.5 inline-flex h-4 w-4 shrink-0 items-center justify-center rounded bg-[var(--btnp)]/15 text-[10px] font-semibold text-[var(--tx)]">
                  {i + 1}
                </span>
                <span className="min-w-0">
                  <span className="block font-medium">{o.label}</span>
                  {o.description && (
                    <span className="mt-0.5 block text-xs text-[var(--mut)]">{o.description}</span>
                  )}
                </span>
              </button>
            ))}
          </div>
          {q.multiSelect && (
            <p
              data-testid={`question-app-uncheck-hint-${qi}`}
              className="mt-1 text-[11px] text-[var(--mut)]"
            >
              多选逐项补投勾选；取消勾选请在 APP 内操作
            </p>
          )}
          {/* 自由作答：原文补投（后端不加模板包装） */}
          <div className="mt-1.5 flex gap-1.5" data-testid={`question-app-freetext-${qi}`}>
            <input
              data-testid={`question-app-freetext-input-${qi}`}
              aria-label="自由作答内容"
              type="text"
              maxLength={MAX_FREE_TEXT_CHARS}
              disabled={busy}
              value={freeTexts[qi] ?? ""}
              onChange={(e) =>
                setFreeTexts((prev) => ({
                  ...prev,
                  [qi]: e.target.value.slice(0, MAX_FREE_TEXT_CHARS),
                }))
              }
              placeholder="输入内容后补投为消息"
              className="min-w-0 flex-1 rounded-lg border border-[var(--cb)] px-2.5 py-1.5 text-sm text-[var(--tx)] placeholder:text-[var(--mut)] focus:ring-2 focus:ring-[var(--btnp)] focus:outline-none disabled:opacity-50"
            />
            <button
              type="button"
              data-testid={`question-app-freetext-send-${qi}`}
              disabled={busy || (freeTexts[qi] ?? "").trim() === ""}
              onClick={() => repost("freeText", undefined, freeTexts[qi] ?? "", qi)}
              className="shrink-0 rounded-full bg-[var(--btnp)] px-3 py-1.5 text-sm font-medium text-[var(--btnpt)] disabled:opacity-40"
            >
              补投作答
            </button>
          </div>
        </div>
      ))}
      {postedPreview !== null && (
        <p data-testid="question-app-progress" className="mt-1.5 text-xs text-[var(--tx)]">
          {/* M-4：上一拍失败时「最近补投」会谎称投出——改「最近尝试补投」（error
              在场即最近一次补投未成功；下一拍成功后 label 回落「最近补投」） */}
          {busy ? "正在补投：" : error !== null ? "最近尝试补投：" : "最近补投："}
          <span className="font-medium">{postedPreview}</span>
        </p>
      )}
      {note !== null && (
        <p
          data-testid="question-app-note"
          className="mt-1 text-xs font-medium text-emerald-600 dark:text-emerald-400"
        >
          {note}
        </p>
      )}
      {error !== null && (
        <p
          data-testid="question-app-error"
          className="mt-1 text-xs text-rose-600 dark:text-rose-400"
        >
          {error}
        </p>
      )}
    </InteractiveCard>
  );
}
