import { useCallback, useEffect, useState } from "react";
import { toneTokens } from "./InteractiveCard";
import {
  ApiError,
  fetchSessionMode,
  sessionModeSwitchApp,
  type AppPolicyMode,
  type ModeGroupView,
  type SessionModeView,
} from "./api";

/** 模式栏 · **APP 形态专属**（Task 8 新建，设计 §3.3/§3.6）。
 *
 * 与 CLI 版（ModeBarCli）的关系：**双套分置、零共享可变状态**——CLI 版的整体逻辑
 * （shift+tab / 菜单键序 / 屏读回读 / 待决守卫）在 APP 会话上没有控制台可用，本件
 * 走**协议读写**（后端 codex_app_form 单点分派），不复用 CLI 件的任何交互分支。
 *
 * 数据源（GET /session-mode，`surface:"app"` 载荷——Task 6a 下发）：
 * - 权限组 = 协议三档 chips（never/on-request/untrusted，标签 全自动/默认/安全 来自
 *   后端 tiers）；当前档 = **rollout 协议读回**（currentSource="rollout"，T0.5 定案：
 *   RPC 全族不暴露档位，读回走 rollout 尾部 turn_context.approval_policy）；
 *   GET 重拉即核对（卡面权威源 = 协议读回，不是本地记忆）；
 * - 模式组 = 整组灰置（无协议载体，T0.2 只定案了 approvalPolicy 载体——裁决 31b
 *   形态级暂缺：灰置 + 一句原因，不渲染可点按钮）。
 *
 * 切档（POST /session-mode/switch 协议臂，sessionModeSwitchApp 出口）：
 * - 回执三态 `verify`（hit/mismatch/ineffective）——**三态都不拿回执改本地状态**
 *   （设计 §3.6：卡面以重拉 GET 的权威面为准；mismatch 的实读串已并入后端 hint，
 *   前端原样透出不重拼）；
 * - hit → hint 透传（回执色）+ 重拉 GET；mismatch/ineffective → hint 透传（注意色）
 *   + 重拉 GET——「未确认」不是失败，是如实申报；
 * - 活跃线程拒绝（code="active_thread_write_unavailable"）→ **灰置 + 钉句**
 *   （T0.1 定案：APP 活跃会话在 0.162.1 无任何写通道——钉句来自后端 Display 单点，
 *   置灰直到卡片重挂，不自动试探）。
 *
 * 降级链（设计 §3.3）：`degraded` 非空（Task 7 ProbeCache 探测失败）→ 只读 chips +
 * 原因串原样透出（协议面不可用时不裸失败、也不假装可写）。
 *
 * **APP 词汇表**（设计 §3.7(a)）：本件渲染输出禁「终端/屏读/按键」——统一「APP/
 * 会话/协议通道」措辞（SessionDetail 有用例级禁词扫描）。 */
export default function ModeBarApp({ session }: { session: { id: string } }) {
  const [view, setView] = useState<AppModeView | null>(null);
  const [ready, setReady] = useState(false);
  const [busy, setBusy] = useState(false);
  /** 切档回执（hit：后端 hint 原样透传，回执色） */
  const [receipt, setReceipt] = useState<string | null>(null);
  /** 未确认提示（mismatch/ineffective：后端 hint 原样透传，注意色） */
  const [notice, setNotice] = useState<string | null>(null);
  /** 活跃线程拒绝钉句（灰置原因；置位后 sticky——APP 关闭前重试必然同拒） */
  const [refusal, setRefusal] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    fetchSessionMode(session.id)
      .then((v) => {
        if (alive) setView(ensureAppView(v));
      })
      .catch(() => {
        /* 拉取失败静默自隐（CLI 套同惯例） */
      })
      .finally(() => {
        if (alive) setReady(true);
      });
    return () => {
      alive = false;
    };
  }, [session.id]);

  const handleSwitch = useCallback(
    async (target: AppPolicyMode) => {
      setBusy(true);
      setReceipt(null);
      setNotice(null);
      try {
        const res = await sessionModeSwitchApp(session.id, target);
        if (res.status === "app_switch") {
          // 三态共同纪律：卡面以重拉 GET 为准——回执只用于提示，不写本地档位
          if (res.verify === "hit") {
            setReceipt(res.hint ?? "已切换（协议读回核对一致）");
          } else {
            // mismatch（读回不符）/ ineffective（含 resumeOnly 的「下一回合生效」语义
            // 性未生效）——后端 hint 已含完整口径，原样透出（不重拼、不掩饰）
            setNotice(res.hint ?? "已投递但未确认生效，请在 APP 内核对当前档位");
          }
          try {
            setView(ensureAppView(await fetchSessionMode(session.id)));
          } catch {
            /* 重拉失败保留原视图（提示已如实，不额外报错） */
          }
        } else if (res.code === "active_thread_write_unavailable") {
          // 活跃线程如实拒：钉句单点透出 + 灰置（sticky——见 state 注释）
          setRefusal(res.error);
        } else {
          setNotice(res.error);
        }
      } catch (e) {
        if (e instanceof ApiError) {
          // H3 同门（403 headless_disabled）与其余非 2xx：如实透出（无禁词）
          setNotice(e.message);
        } else {
          setNotice(String(e));
        }
      } finally {
        setBusy(false);
      }
    },
    [session.id]
  );

  // 未就绪 / 拉取失败 / 无机制：不渲染（CLI 套同惯例）
  if (!ready || view === null || view.switchKind === "unsupported") return null;

  // 降级链 + 活跃拒绝：任一在场 → 只读（协议面不可用/无写通道时不假装可写）
  const degraded = typeof view.degraded === "string" && view.degraded !== "" ? view.degraded : null;
  const readonly = degraded !== null || refusal !== null;
  const t = toneTokens("mode");
  return (
    <div
      data-testid="mode-bar-app"
      data-surface="app"
      data-degraded={degraded ?? undefined}
      data-tone="mode"
      className={`flex flex-wrap items-center gap-x-3 gap-y-1 px-2 py-1 ${t.box}`}
    >
      {view.groups.map((g) => (
        <AppGroupRow
          key={g.id}
          group={g}
          disabled={busy || readonly}
          onSwitch={handleSwitch}
          onUnknownMode={(mode) => setNotice(`未知档位词 ${mode}——请刷新或回 APP 内操作`)}
        />
      ))}
      {degraded !== null && (
        <span
          data-testid="mode-app-degraded"
          className="text-[11px] text-amber-700 dark:text-amber-400"
        >
          协议通道暂不可用（{degraded}）——只读展示，请稍后重试或在 APP 内操作
        </span>
      )}
      {refusal !== null && (
        <span
          data-testid="mode-app-refusal"
          className="text-[11px] text-rose-600 dark:text-rose-400"
        >
          {refusal}
        </span>
      )}
      {notice !== null && (
        <span
          data-testid="mode-app-notice"
          className="text-[11px] text-amber-700 dark:text-amber-400"
        >
          {notice}
        </span>
      )}
      {receipt !== null && (
        <span
          data-testid="mode-app-receipt"
          className="text-[11px] text-emerald-600 dark:text-emerald-400"
        >
          {receipt}
        </span>
      )}
    </div>
  );
}

/** APP 面视图类型（groups 必在——ensureAppView 兜底后的不变量） */
type AppModeView = SessionModeView & { groups: ModeGroupView[] };

/** 旧后端兼容：无 groups 的载荷合成空组列表（APP 面恒有 groups；防御性回落——
 *  空列表 = 只渲染降级/提示行，不渲染可点按钮，与「不猜」纪律同向） */
function ensureAppView(v: SessionModeView): AppModeView {
  return { ...v, groups: v.groups ?? [] };
}

/** chip 外观（与 CLI 套 chipClass 同款视觉语言——2026-10-10「风格统一」口径：
 *  高亮 = 黑边 + 加粗，无底色；非当前档灰底） */
function chipClass(isCurrent: boolean): string {
  return `rounded-full border px-2 py-0.5 text-[11px] ${
    isCurrent
      ? "border-[var(--tx)] bg-transparent font-semibold text-[var(--tx)]"
      : "border-transparent bg-[var(--cb)] text-[var(--tx)] disabled:opacity-40"
  }`;
}

/** 单组渲染（APP 面）：**不消费 layout**——协议面没有 toggle/picker 键序语义，
 *  统一为「可点 chips（selectable）/ 灰置 chips（不可选）」两种形态。
 *  - 灰置组的 reason **可见**（裁决 31b「灰置 + 一句原因」——CLI 套只放 title，
 *    APP 面的暂缺是常态性形态限制，明说优于悬停）；
 *  - current=null 且组有可选档 → 「模式未知，请人工核对 APP」（T0.5 回填定案文案：
 *    rollout 无 approval_policy 记录的旧会话——保守降级不猜默认档）。 */
function AppGroupRow({
  group,
  disabled,
  onSwitch,
  onUnknownMode,
}: {
  group: ModeGroupView;
  /** busy（切档在途）或整条只读（降级/活跃拒绝） */
  disabled: boolean;
  onSwitch: (target: AppPolicyMode) => void;
  /** selectable 档的词不在协议三档词表（后端词表漂移的防御路径，spec 审 m-3）——
   *  如实提示，不静默吞点击 */
  onUnknownMode: (mode: string) => void;
}) {
  const hasChannel = group.tiers.some((tier) => tier.selectable);
  const unknown = group.current === null && hasChannel;
  const firstDisabledReason = group.tiers.find((tier) => !tier.selectable)?.reason ?? null;
  return (
    <span data-testid={`mode-group-app-${group.id}`} className="flex flex-wrap items-center gap-2">
      <span
        data-testid={`mode-group-app-label-${group.id}`}
        className="text-xs font-bold text-[var(--tx)]"
      >
        {group.label}
      </span>
      {unknown && (
        <span
          data-testid={`mode-app-unknown-hint-${group.id}`}
          className="text-[11px] text-amber-700 dark:text-amber-400"
        >
          模式未知，请人工核对 APP
        </span>
      )}
      <span className="flex flex-wrap items-center gap-1">
        {group.tiers.map((tier) =>
          tier.selectable ? (
            <button
              key={tier.mode}
              type="button"
              data-testid={`mode-app-switch-${group.id}-${tier.mode}`}
              data-current={group.current === tier.mode ? "true" : "false"}
              disabled={disabled}
              title="经协议通道切换档位，结果以读回核对为准"
              onClick={() => {
                if (isAppPolicy(tier.mode)) {
                  onSwitch(tier.mode);
                } else {
                  onUnknownMode(tier.mode);
                }
              }}
              className={chipClass(group.current === tier.mode)}
            >
              {tier.label}
            </button>
          ) : (
            // 灰置档（形态级暂缺）：虚线框 + 「不可用」——不给可点语义
            <span
              key={tier.mode}
              data-testid={`mode-app-tier-disabled-${group.id}-${tier.mode}`}
              title={tier.reason ?? undefined}
              className="rounded-full border border-dashed border-[var(--cb)]/40 px-2 py-0.5 text-[11px] text-[var(--mut)]"
            >
              {tier.label}（不可用）
            </span>
          )
        )}
      </span>
      {!hasChannel && firstDisabledReason !== null && (
        <span
          data-testid={`mode-app-tier-reason-${group.id}`}
          className="text-[11px] text-[var(--mut)]"
        >
          {firstDisabledReason}
        </span>
      )}
    </span>
  );
}

/** 档位词收窄（运行时防御——后端 APP 面权限组 tiers 恒为协议三档词；其它组
 *  selectable 恒 false 不会走到 onSwitch。双重保险防词表漂移时误发 MamMode 词） */
function isAppPolicy(mode: string): mode is AppPolicyMode {
  return mode === "never" || mode === "on-request" || mode === "untrusted";
}
