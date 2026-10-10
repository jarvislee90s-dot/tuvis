// 移动端审批卡（M8 Task 12，红卡选项卡 UI）：挂在 SessionDetail 正文视图
// messageArea 上方（waiting 态；预览/分屏分支不挂——MessageComposer 同一挂载惯例）。
// - 选项可用性：挂载拉取一次 /session-approve-options；拉取失败 / 网络异常 →
//   静默自隐；available=false 且无 reason（非 Waiting / 无映射 / 未命中）→ 自隐
//   （SessionDetail 无需感知选项可用性，详情页正文照常——fetchSessionFiles 静默
//   降级同一惯例）；available=false 且带 reason（严格档）→ 渲染提示条见下；
// - 红卡视觉：红色边框卡 + 标题「等待批准」（红点呼吸对齐看板 waiting 状态点）+
//   选项按钮横排（label 渲染；响应载荷只含 id/label，键位是投递层机密不外泄 UI）；
// - drift=true → 提示条「映射待实测确认，若提示不符请用普通发送」；
// - 严格档（M9R）：available=false 且后端下发 reason（Task 10：键位未实测确认时
//   下发「键位待实测确认，请用普通发送」）→ 卡片只渲染提示条（reason 原文内联，
//   琥珀色弱化视觉、不带红卡脉冲）不渲染按键组——键位未实测防误发；
//   available=false 且无 reason（非 Waiting / 无映射 / 未命中）→ 卡自隐（原惯例）；
// - 应答：POST /session-approve——key_sent → 「已发送按键」终态（按钮禁用）；
//   failed{error} → 错误文案可重试（按钮保持可点，重按即重试）；ApiError（409/404
//   带 data.error）→ 分診中文文案：not_waiting→「会话不在等待状态」、no_mapping→
//   「该工具暂不支持审批应答，请用普通发送」、no_session→「会话已结束，请返回看板刷新」、
//   其余显示 message。
// 局限（本任务范围裁决）：mount 只拉一次选项，卡内不做轮询——红卡的出现/消失依赖
// 页面数据刷新（SSE 快照 → 详情页重挂/卸载）自然带动；SSE 驱动卡内 re-fetch 属
// Task 12 后优化，不在本任务范围。
import { useCallback, useEffect, useRef, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import InteractiveCard, { type InteractiveCardTone } from "./InteractiveCard";
import { ApiError, fetchApproveOptions, sessionApprove, type ApproveOptionsView } from "./api";

interface ApproveCardProps {
  /** 会话（本组件消费 id 与 status；结构化类型，完整 Session 可直接传入）。
   *  status = 会话当前状态（2026-10-04 使命完成自隐用）：按键已投递（sent）且
   *  会话已离开 waiting（执行已开始）→ 本卡自隐——终端在跑新回合时挂着
   *  「已发送按键」会被误读为卡死（用户裁决 2026-10-04）。status 缺省（旧调用方）
   *  → 不启用该规则（行为同旧版）。 */
  session: { id: string; status?: string };
  /** 2026-10-04 计划批准卡：反馈入口 start 成功（终端已进反馈编辑态）→ 上报父级
   *  换渲染 PlanFeedbackBar（本卡随状态转黄自然卸载，反馈条独立存活） */
  onPlanFeedbackReady?: () => void;
}

/** 计划待确认条的脚注文案（丁T2）：预期态在场但后端没读到终端对话框选项——
 *  与后端 `remote/api.rs` 的降级语义同源（终端对话框可能尚未绘制/已关闭/不在
 *  Windows 可见窗口）。前端自持文案：后端在 available=true 形态下不下发 reason
 *  （reason 是 available=false 的严格档通道），故此处由前端给同义提示。 */
const PLAN_CHECK_MISS_HINT =
  "未读到终端对话框选项——请再点一次「检查终端对话框」，或直接在终端处理该确认";

/** 裁12 折叠阈值：计划正文超过该字符数**默认折叠**（长内容不得把 composer 顶出
 *  首屏——卡体总高度上限由 max-h 封顶、长文再默认折叠，点开查看全文） */
const PLAN_COLLAPSE_CHARS = 600;

/** 审批卡的计划正文块（T8 聚合；两个渲染分支共用——裁12 布局契约单点）：
 *  - **总高度上限**：容器恒 max-h 封顶（折叠 max-h-24 / 展开 max-h-64 内滚）——
 *    任何内容量都不把 composer 顶出首屏；
 *  - **长内容默认折叠**：content 超过 [`PLAN_COLLAPSE_CHARS`] 字符默认折叠
 *    （max-h-24 + overflow-hidden + 渐隐提示），点「展开全文」切换到 max-h-64
 *    内滚视图；短内容直接完整渲染（无按钮）。 */
function PlanBody({ plan }: { plan: { content: string; isFile: boolean } }) {
  // 展开/收起默认折叠（长文）；短文 expanded 恒 true 且不渲染按钮
  const [expanded, setExpanded] = useState(false);
  const long = plan.content.length > PLAN_COLLAPSE_CHARS;
  const collapsed = long && !expanded;
  return (
    <div>
      <div
        data-testid="approve-plan"
        data-plan-file={plan.isFile ? "true" : "false"}
        data-collapsed={collapsed ? "true" : "false"}
        className={`mt-2 rounded-lg border border-[var(--btnp)] bg-[var(--cbg)]/60 p-2 text-xs text-[var(--tx)] ${
          collapsed ? "max-h-24 overflow-hidden" : "max-h-64 overflow-y-auto"
        }`}
      >
        <p className="mb-1 text-[11px] font-medium tracking-wide text-[var(--tx)]/80 uppercase">
          {plan.isFile ? "计划文件" : "计划内容"}
        </p>
        {plan.isFile ? (
          <p data-testid="approve-plan-file" className="font-mono break-all">
            {plan.content}
          </p>
        ) : (
          <div className="prose-sm max-w-none">
            <ReactMarkdown remarkPlugins={[remarkGfm]}>{plan.content}</ReactMarkdown>
          </div>
        )}
      </div>
      {long && (
        <button
          type="button"
          data-testid="approve-plan-toggle"
          onClick={() => setExpanded((v) => !v)}
          className="mt-1 text-[11px] font-medium text-[var(--tx)]/80 underline"
        >
          {expanded ? "收起计划" : "展开全文"}
        </button>
      )}
    </div>
  );
}

export default function ApproveCard({ session, onPlanFeedbackReady }: ApproveCardProps) {
  // 选项可用性：ready=false（加载中 / 拉取失败）→ 不渲染（available/reason 分诊在渲染侧）
  const [options, setOptions] = useState<ApproveOptionsView | null>(null);
  const [ready, setReady] = useState(false);
  // 应答进行中（防连点）
  const [busy, setBusy] = useState(false);
  // 终态：按键已投递（key_sent）——按钮禁用 +「已发送按键」
  const [sent, setSent] = useState(false);
  // 使命完成自隐（2026-10-04 用户裁决 + 2026-10-05 宽限修正）：按键已投递且会话
  // 已离开 waiting（执行已开始）→ 宽限 2.5s 展示「已发送按键」回执后收卡——
  // 不宽限会让回执闪没（测试与可感知性），不收卡则像卡死。status 缺省（旧调用
  // 方）→ 不启用。
  const [hideAfterSent, setHideAfterSent] = useState(false);
  const leftWaiting = session.status !== undefined && session.status !== "waiting";
  useEffect(() => {
    if (!(sent && leftWaiting)) return;
    const t = window.setTimeout(() => setHideAfterSent(true), 2500);
    return () => window.clearTimeout(t);
  }, [sent, leftWaiting]);
  // 失败文案（failed{error} 回执 / ApiError 分診）——非 null 展示，按钮保持可点（可重试）
  const [error, setError] = useState<string | null>(null);
  // 丁T2：「检查终端对话框」进行中（防连点）+ 检查后仍未读到选项的降级提示
  const [checking, setChecking] = useState(false);
  const [checkMissed, setCheckMissed] = useState(false);
  // **降级态（命中审批 ∧ 未读到终端对话框）**：二元键可能错位（2026-10-10 21:55
  // 实机——多选问题面板被误读成审批框，点「允许」真实发键）。此态下按钮禁用 +
  // 自动重读，读到选项即恢复可用（2026-10-10 用户裁决「读不到就别给钮」）。
  const degraded = typeof options?.degradedHint === "string" && options.degradedHint.trim() !== "";

  // 降级态自动重读：每 2.5s 重拉一次选项端点（现场屏读）——真审批框一旦绘制完成
  // 即读到选项、degraded 解除按钮亮起；假审批（多选面板等）永远读不到、按钮恒禁用。
  // 手动「检查终端对话框」按钮同样保留。sent 后不再轮询（终态）。handleCheck 定义
  // 在下方——经 ref 间接调用（本 effect 声明在前）。
  const handleCheckRef = useRef<(() => void) | null>(null);
  // 挂载拉取一次选项可用性；拉取失败 → 静默保持隐藏。
  // ready 只表示「载荷已落地」（available/reason 的分诊移到渲染侧——严格档
  // available=false + reason 也要渲染提示条，不能拿 available 当 ready）
  useEffect(() => {
    let alive = true;
    setReady(false);
    fetchApproveOptions(session.id)
      .then((v) => {
        if (!alive) return;
        setOptions(v);
        setReady(true);
      })
      .catch(() => {
        if (alive) setReady(false);
      });
    return () => {
      alive = false;
    };
  }, [session.id]);

  /** 「检查终端对话框」（丁T2）：重拉一次选项端点——后端在计划预期态下**现场屏读**，
   *  命中即回 N 选项（id=`dialog:<n>`），未命中回零选项 + planPending=true。
   *  与挂载那次拉取同一条数据通道（无新端点）；失败静默保留原载荷 + 显示降级提示。 */
  const handleCheck = useCallback(async () => {
    if (checking) return;
    setChecking(true);
    setCheckMissed(false);
    try {
      const v = await fetchApproveOptions(session.id);
      setOptions(v);
      // 仍未读到选项 → 明示未命中，不假装成功。两个形态：planPending 仍立且无
      // dialog（计划条）/ **降级态重拉后仍降级**（二元卡核对未命中——评审
      // Important 4 锁的行为面：用户点了检查得知道「没读到」而不是停在「核对中」）
      const stillPending = v.available === true && v.planPending === true && v.dialog !== true;
      const stillDegraded = typeof v.degradedHint === "string" && v.degradedHint.trim() !== "";
      setCheckMissed((stillPending && v.options.length === 0) || stillDegraded);
    } catch {
      setCheckMissed(true);
    } finally {
      setChecking(false);
    }
  }, [checking, session.id]);

  const handleAnswer = useCallback(
    async (optionId: string) => {
      if (busy || sent) return;
      setBusy(true);
      setError(null);
      try {
        const res = await sessionApprove(session.id, optionId);
        if (res.status === "key_sent") {
          setSent(true);
        } else {
          setError(res.error);
        }
      } catch (e) {
        if (e instanceof ApiError) {
          const code = typeof e.data?.error === "string" ? e.data.error : null;
          if (code === "not_waiting") {
            setError("会话不在等待状态");
          } else if (code === "no_mapping") {
            setError("该工具暂不支持审批应答，请用普通发送");
          } else if (code === "no_session") {
            setError("会话已结束，请返回看板刷新");
          } else {
            setError(e.message);
          }
        } else {
          setError(String(e));
        }
      } finally {
        setBusy(false);
      }
    },
    [busy, sent, session.id]
  );

  /** 2026-10-04 计划批准卡：反馈入口（feedbackOption 行）——**就是点选项 3 本身**
   *  （POST /session-approve {optionId:"dialog:3"}，后端计划框导航优先键序；探测定
   *  案 2026-10-04 §S3：选 3 = 计划被拒回空 composer、plan 模式保持）。成功即上报
   *  父级换渲染 PlanFeedbackBar；失败文案复用 error 态（按钮保持可点可重试）。 */
  const handleFeedbackStart = useCallback(async () => {
    if (busy || sent) return;
    setBusy(true);
    setError(null);
    try {
      const res = await sessionApprove(session.id, "dialog:3");
      if (res.status === "key_sent") {
        onPlanFeedbackReady?.();
      } else {
        setError(res.error);
      }
    } catch (e) {
      if (e instanceof ApiError) {
        const code = typeof e.data?.error === "string" ? e.data.error : null;
        if (code === "not_waiting") {
          setError("会话不在等待状态");
        } else if (code === "no_session") {
          setError("会话已结束，请返回看板刷新");
        } else {
          setError(e.message);
        }
      } else {
        setError(String(e));
      }
    } finally {
      setBusy(false);
    }
  }, [busy, sent, session.id, onPlanFeedbackReady]);

  // 降级态自动重读 effect（handleCheckRef 提交后同步 + 每 2.5s 重拉）——置于全部
  // useCallback 之后（React Compiler：memoized 链中间插 ref 写入会跳过其记忆化）
  useEffect(() => {
    handleCheckRef.current = handleCheck;
  }, [handleCheck]);
  useEffect(() => {
    if (!degraded || sent) return;
    const t = window.setInterval(() => {
      handleCheckRef.current?.();
    }, 2500);
    return () => window.clearInterval(t);
  }, [degraded, sent]);

  // 加载中 / 拉取失败 / options 未落地：不渲染
  if (!ready || options === null) return null;
  // 严格档（M9R）：available=false 且后端给出 reason → 只渲染提示条不渲染按键；
  // available=false 且无 reason（非 Waiting / 无映射 / 未命中）：卡自身自隐（原惯例）
  const hintOnly =
    !options.available && typeof options.reason === "string" && options.reason.trim() !== "";
  if (!options.available && !hintOnly) return null;

  // 严格档提示条模式：reason 原文内联（后端中文），琥珀色弱化（非红卡脉冲——
  // 无可操作按键，避免误导），不渲染按键组
  if (hintOnly) {
    return (
      <div
        data-testid="approve-card"
        data-mode="hint"
        className="shrink-0 rounded-xl border border-amber-500/50 bg-amber-500/5 px-3 py-2 dark:border-amber-400/50 dark:bg-amber-400/5"
      >
        <p data-testid="approve-hint" className="text-xs text-amber-700 dark:text-amber-400">
          {options.reason}
        </p>
      </div>
    );
  }

  // 使命完成自隐（2026-10-04 用户裁决，2026-10-05 宽限修正）：见顶部 hideAfterSent
  // 的宽限期注释——回执先展示 2.5s，随后整卡收起。
  if (hideAfterSent) {
    return null;
  }

  // ===== 丁T2：计划待确认条（codex/kimi 的计划确认框）=====
  //
  // 形态：`available=true ∧ planPending=true ∧ dialog≠true`（零选项是**预期**，不是错误）。
  // 为什么单独一条渲染分支（而不是复用选项卡）：
  // - 此形态下后端**刻意不下发**映射表键位（codex 的 y/esc 是补丁审批键位、kimi 的
  //   数字通道已被 R1-1 证伪不可依赖）——没有可渲染的按钮，选项卡会渲染成空组；
  // - 用户的下一步动作是**去终端看**（可能对话框没绘制/已关闭）或**点检查重试**
  //   （对话框刚绘制出来时，屏读一次就能拿到 N 选项）。
  //
  // 计划正文照常渲染（T8 聚合机制，`plan` 字段）——用户点检查前先看到计划全文。
  const planPendingOnly =
    options.available && options.planPending === true && options.dialog !== true;
  if (planPendingOnly) {
    return (
      <InteractiveCard
        tone="question"
        testId="approve-card"
        mode="plan-pending"
        title="计划待确认"
        footer={
          <>
            <button
              type="button"
              data-testid="approve-plan-check"
              disabled={checking}
              onClick={handleCheck}
              className="mt-2 rounded-full bg-[var(--btnp)] px-3 py-1.5 text-xs font-medium text-[var(--btnpt)] hover:bg-[var(--btnp)] disabled:opacity-40"
            >
              {checking ? "检查中…" : "检查终端对话框"}
            </button>
            {/* 检查未命中：明示（红线 3——不假装成功）；命中则本条整个消失（走选项卡分支） */}
            {checkMissed && (
              <p
                data-testid="approve-plan-check-miss"
                className="mt-1 text-xs text-amber-700 dark:text-amber-400"
              >
                {PLAN_CHECK_MISS_HINT}
              </p>
            )}
          </>
        }
      >
        {/* 计划待确认条：**检查未命中即清除**（任务书语义「预期态清除：下一个用户消息
            注入或检查未命中时清除」）——点过检查且屏读仍没读到选项时，本条不再显示
            （不再断言「终端正在等这个计划」——那是未经验证的声明，§2.8 不假装），
            改为下方脚注的「未读到」明示 + 检查钮可再试。用户注入下一条消息后，
            消息尾部判据（`isPlanPending`）会让整张卡不再挂载 = 预期态彻底清除。 */}
        {!checkMissed && (
          <p data-testid="approve-plan-pending" className="mt-1 text-xs text-[var(--tx)]/80">
            终端正在等待这个计划的确认——请到终端对话框选择，或点下方按钮读取选项
          </p>
        )}
        {/* 计划全文（T8 聚合；无计划消息 → 不渲染主体）——裁12 布局契约见 PlanBody */}
        {options.plan != null && options.plan.content.trim() !== "" && (
          <PlanBody plan={options.plan} />
        )}
      </InteractiveCard>
    );
  }

  // 裁11 活口收口（2026-09-24 用户裁决「完全并成一套」）：审批卡与问答卡统一蓝系
  // （sky）+ 同一**纵向编号列表**形态——两者只差选项数目。原「二元审批保留红系
  // 警示」的活口由用户本次关闭；`mode` 数据属性仍区分 binary/dialog（测试/可观测
  // 面）。警示类脚注（drift/degradedHint）保留琥珀——那是告警语义层，不是选项层。
  const cardTone: InteractiveCardTone = "question";
  return (
    <InteractiveCard
      tone={cardTone}
      testId="approve-card"
      mode={options.dialog ? (options.planDialog ? "plan-dialog" : "dialog") : "binary"}
      title={
        options.dialog ? (options.planDialog ? "计划批准" : "等待批准（终端对话框）") : "等待批准"
      }
      footer={
        <>
          {error !== null && (
            <p
              data-testid="approve-error"
              className="mt-1 text-xs text-rose-600 dark:text-rose-400"
            >
              {error}
            </p>
          )}
          {sent && (
            <p
              data-testid="approve-sent"
              className="mt-1.5 text-xs font-medium text-emerald-600 dark:text-emerald-400"
            >
              已发送按键
            </p>
          )}
          {/* **降级态（读不到终端对话框）不给出键**（2026-10-10 用户裁决「读不到就
              别给钮」）：按钮已禁用，这里给「核对中/未读到」两种状态文案 + 手动检查
              钮；自动重读每 2.5s 一次，读到选项即整卡切换为可点形态 */}
          {degraded && (
            <>
              <button
                type="button"
                data-testid="approve-degraded-check"
                disabled={checking}
                onClick={handleCheck}
                className="mt-2 rounded-full bg-[var(--btnp)] px-3 py-1.5 text-xs font-medium text-[var(--btnpt)] hover:bg-[var(--btnp)] disabled:opacity-40"
              >
                {checking ? "检查中…" : "检查终端对话框"}
              </button>
              <p
                data-testid="approve-degraded-state"
                className="mt-1 text-xs text-amber-700 dark:text-amber-400"
              >
                {checkMissed
                  ? "仍未读到终端对话框选项——上方按键保持禁用，请到终端确认当前面板"
                  : "正在核对终端对话框——读到选项后上方按键可用"}
              </p>
            </>
          )}
        </>
      }
      actions={
        <div className="space-y-1">
          {/* 纵向编号列表（与问答卡同形态——「一套，只差选项数目」）。编号徽标 =
              将注入的数字键，**只在 dialog 分支渲染**（屏读到的选项才有真实编号）；
              二元分支的键位不外泄给 UI（载荷只有 id/label——契约锚点），且各家键位
              不同（claude 允许='1' / codex 允许='y' / 拒绝=esc），编造序号或硬编码
              键名都会谎报「将按什么」——评审 I2：不渲染徽标是唯一不撒谎的形态 */}
          {options.options.map((o, i) => {
            // 2026-10-04 计划批准卡：反馈选项（feedbackOption 命中行）→ 渲染为
            // 「告诉 Claude 要改什么」入口——点击 start 进反馈编辑态（换渲染
            // PlanFeedbackBar），**不**直发按键。其余选项照常代按。
            const isFeedback = options.planDialog === true && o.id === options.feedbackOption;
            return (
              <button
                key={o.id}
                type="button"
                data-testid={isFeedback ? "approve-option-feedback" : `approve-option-${o.id}`}
                disabled={busy || sent || degraded}
                onClick={() => (isFeedback ? handleFeedbackStart() : handleAnswer(o.id))}
                className={`flex w-full items-start gap-2 rounded-lg px-2.5 py-1.5 text-left text-sm disabled:opacity-40 ${
                  // 行形态与问答卡选项行同款（2026-10-04 蓝系统一批：同内距/字号/
                  // 中性底+hover——「一套，只差选项数目」的视觉收口）
                  "bg-sky-500/5 text-slate-700 hover:bg-sky-500/10 dark:bg-sky-400/5 dark:text-slate-300 dark:hover:bg-sky-400/10"
                }`}
              >
                {options.dialog && (
                  <span className="mt-0.5 inline-flex h-4 w-4 shrink-0 items-center justify-center rounded bg-sky-500/15 text-[10px] font-semibold text-sky-700 dark:bg-sky-400/15 dark:text-sky-400">
                    {i + 1}
                  </span>
                )}
                <span className="min-w-0 flex-1 break-words">{o.label}</span>
                {isFeedback && (
                  <span className="mt-0.5 shrink-0 text-[11px] text-sky-700/70 dark:text-sky-400/70">
                    输入修改意见 →
                  </span>
                )}
              </button>
            );
          })}
        </div>
      }
    >
      {options.dialog && (
        <p data-testid="approve-dialog-label" className="mt-1 text-xs text-[var(--tx)]/80">
          {options.planDialog
            ? "以下选项读自终端对话框——点按即代你用方向键选择；反馈项会打开意见输入框"
            : "以下选项读自终端对话框，点按即代你按对应数字键"}
        </p>
      )}
      {/* T8：审批点 plan 聚合——计划确认类审批卡主体即见计划全文（不再要用户去
          消息流翻）。markdown 直出（claude/codex 的 kind="plan"）；isFile=true 时以
          路径提示呈现（全文走文件预览）。
          **kimi 恒为 null**（丁T2 复评 F3-4：任务书成文要求 kimi 审批卡不含 plan
          正文——正文由消息流的 kind="plan" 正文卡承担）；无计划（plan null）→ 不渲染。
          裁12 布局契约见 PlanBody */}
      {options.plan != null && options.plan.content.trim() !== "" && (
        <PlanBody plan={options.plan} />
      )}
      {options.drift && (
        <p data-testid="approve-drift" className="mt-1 text-xs text-amber-700 dark:text-amber-400">
          映射待实测确认，若提示不符请用普通发送
        </p>
      )}
      {/* R1-3 降级警示（计划红线 3）：命中审批但未读到终端对话框选项 → 终端可能正
          显示多选项而二元键可能错位，必须显式提示用户去终端核对（后端下发文案原文） */}
      {typeof options.degradedHint === "string" && options.degradedHint.trim() !== "" && (
        <p
          data-testid="approve-degraded-hint"
          className="mt-1 text-xs text-amber-700 dark:text-amber-400"
        >
          {options.degradedHint}
        </p>
      )}
    </InteractiveCard>
  );
}
