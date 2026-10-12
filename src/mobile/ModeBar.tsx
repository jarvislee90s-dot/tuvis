import { useCallback, useEffect, useState } from "react";
import { toneTokens } from "./InteractiveCard";
import {
  ApiError,
  appFormPendingCopy,
  fetchSessionMode,
  sessionModeSwitch,
  type MamMode,
  type ModeGroupId,
  type ModeGroupView,
  type SessionModeView,
} from "./api";

/** 模式栏（批次丙 T6；批次丁 T4 扩二维与回读全开）。
 *
 * 数据源 GET /session-mode。**丁T4 起后端下发结构表**（§2.6 规格表）：
 * - 二维家（codex/kimi）→ `structure="twoAxis"`，`groups` = [模式组, 权限组]，
 *   两组各自显示当前档 + 各自的按钮；
 * - 单轴家（claude/opencode）→ `structure="singleAxis"`，`groups` = [模式轴]，
 *   渲染「切换」钮 + 当前模式回显；
 * - 旧后端（无 `structure`/`groups`）→ **回落**到「单轴渲染 + 顶层 current」，
 *   调用 POST 时不带 `group`（后端按 target 归组）。
 *
 * **降级语义（T6 红线 4 + 裁5：不假装成功）**：
 * - `switchKind === "unsupported"` → 不渲染（该工具无实测切换机制）；
 * - 某组 `current === null` → 该组显示「模式未知」+「请人工核对终端」；
 * - 切档回执 `verified === false` → 显示后端下发的 `hint`（含「人工核对」语义），
 *   **不声称已切到目标档**。
 *
 * **裁7（codex 退役旧档不作可选）**：`groups[].tiers[]` 里 `selectable=false` 的档
 * 不渲染为可点按钮；`groups[].legacy[]`（untrusted/on-failure）**不渲染**（2026-10-10
 * 二版用户指令：chips 高亮已承载全部状态信息，退役说明行纯文本已删——后端仍下发，
 * 前端不消费）。
 */
export default function ModeBar({ session }: { session: { id: string } }) {
  const [view, setView] = useState<SessionModeView | null>(null);
  const [ready, setReady] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  /** 切档回执提示（成功后展示；verified=false 时是人工核对提示） */
  const [receipt, setReceipt] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    fetchSessionMode(session.id)
      .then((v) => {
        if (alive) setView(v);
      })
      .catch(() => {
        /* 拉取失败静默自隐（approve/question 同惯例） */
      })
      .finally(() => {
        if (alive) setReady(true);
      });
    return () => {
      alive = false;
    };
  }, [session.id]);

  // 后端是否下发了结构表（旧后端没有 → 前端合成的单轴视图，POST 不带 `group`）。
  // 抽成**布尔**再进依赖数组：直接依赖 `view?.groups` 会让 useCallback 的依赖
  // 每次渲染都变（数组字面量），等于没 memo（eslint react-hooks 会点名）
  const hasServerGroups = view?.groups !== undefined;

  const handleSwitch = useCallback(
    async (target: MamMode, group?: ModeGroupId) => {
      setBusy(true);
      setError(null);
      setReceipt(null);
      try {
        // 旧后端（无 groups 字段）回落出来的那组是前端**合成的**——它的 id 不是
        // 后端给的，故不带 `group` 发（后端按 target 自行归组，与旧调用逐字一致）
        const g = hasServerGroups ? group : undefined;
        const res = await sessionModeSwitch(session.id, target, g);
        if (res.status === "key_sent") {
          if (res.verified) {
            // 回读命中：回执 + 重拉一次 GET 刷新显示（**不拿回执字段去改本地状态**：
            // 回执只描述该组那一刻的观测，直接采信会把「屏读快照」当成结构表；
            // 重拉失败则保留原视图——如实，不是把旧值刷成新值）
            // T5（spec §6-T5）：hint **原样透传**——codex toggle 臂命中 = 「已切换
            // （屏读核验命中）」+ 前读申报，不能硬编码「已切换」盖掉后端口径。
            // wire 对位：observed（核验末拍屏读档）有值也不改本地状态——卡面以
            // 重拉 GET 为准。2026-10-10 用户指令后「已在目标档（零投递）」态已
            // 不存在（点切换必然发键），hint 只剩命中/不符/未生效三态。
            setReceipt(res.hint ?? "已切换");
            try {
              setView(await fetchSessionMode(session.id));
            } catch {
              /* 回读失败不清回执（切换本身已投递且已核实） */
            }
          } else {
            // 红线 4：不假装成功——原样透出后端 hint（含「预期/实际」或「请人工核对」）。
            // T5：observed 非空且 verified=false 时，后端 hint 已含「屏已切换至 X
            // （预期 Y）」——这里只透传，**不重复拼接** observed 的档名。
            setReceipt(res.hint ?? "已发送切换，请人工核对终端模式");
          }
        } else {
          setError(res.error);
        }
      } catch (e) {
        if (e instanceof ApiError) {
          if (e.status === 404 && e.data?.error === "no_session") {
            setError("会话已结束，请返回看板刷新");
          } else if (e.status === 409 && e.data?.error === "no_mechanism") {
            setError("该工具的模式切换未实测");
          } else if (e.status === 409 && e.data?.error === "blocked_by_dialog") {
            // 丁T3 §2.7 对话框在场红线（裁8/9，问题 5）：控制类注入被拒——后端
            // 屏读见编号选项对话框，切换键/斜杠命令会落进对话框（实机连点 17 次
            // 全变「选第一项」）。文案用后端下发的 reason（单一来源），缺省给同义兜底
            setError(
              typeof e.data?.reason === "string" ? e.data.reason : "终端有待决对话框，请先处理"
            );
          } else if (e.status === 409 && e.data?.error === "blocked_by_question") {
            // E3② 待决拦截（硬）：kimi 问答待决时切档注入（含回车）会误选答案——
            // 后端整条拒绝；文案用后端 reason（单一来源）
            setError(
              typeof e.data?.reason === "string"
                ? e.data.reason
                : "终端有待答的问题，请先在问答卡作答"
            );
          } else if (e.status === 409 && e.data?.error === "app_form_pending") {
            // Task 15 止血门（2026-10-12）：codex APP 会话的模式/权限写动作走协议
            // 通道（开发中）——后端在屏读与注入之前整条拒绝。取值逻辑走单点
            // [`appFormPendingCopy`]（reason 优先、常量兜底，与问答/审批侧**同一实现**）。
            // 不消费这条码的话用户只会看到「session-mode/switch 409」这种无意义串
            setError(appFormPendingCopy(e));
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
    [session.id, hasServerGroups]
  );

  // 未就绪 / 拉取失败 / 无实测机制：不渲染
  if (!ready || view === null || view.switchKind === "unsupported") return null;

  // T10：本组件是**状态条**（非「等待用户输入」交互卡，任务书 §2.2 的容器契约
  // 针对 ApproveCard/QuestionCard 两类交互卡），故保留自身的横向布局；但**取色
  // 走统一 token**（InteractiveCard 的 mode 档）——四套界面同一套设计语言
  const t = toneTokens("mode");
  const groups = modeGroups(view);
  // E3④ 待决置灰：终端问答待决时切档注入（含回车）会被问答框误消费
  // （codex 交默认答案 / kimi 误选推进待决态）→ 全部按钮禁用 + 原因文案。
  // undefined（旧后端）= 未知，不置灰（与后端「无法判定放行」同一取向）
  const questionPending = view.questionPending === true;
  return (
    <div
      data-testid="mode-bar"
      data-mode={view.current ?? "unknown"}
      data-structure={view.structure ?? "legacy"}
      data-question-pending={questionPending ? "true" : "false"}
      data-tone="mode"
      className={`flex flex-wrap items-center gap-x-3 gap-y-1 px-2 py-1 ${t.box}`}
    >
      {groups.map((g) => (
        <ModeGroupRow
          key={g.id}
          group={g}
          showGroupLabel={groups.length > 1}
          busy={busy || questionPending}
          onSwitch={(target) => handleSwitch(target, g.id)}
        />
      ))}
      {questionPending && (
        <span
          data-testid="mode-question-pending-hint"
          className="text-[11px] text-amber-700 dark:text-amber-400"
        >
          终端有待答的问题——切权限/模式的命令会误选答案，请先在问答卡作答
        </span>
      )}
      {error !== null && (
        <span data-testid="mode-error" className="text-[11px] text-rose-600 dark:text-rose-400">
          {error}
        </span>
      )}
      {receipt !== null && (
        <span
          data-testid="mode-receipt"
          className="text-[11px] text-emerald-600 dark:text-emerald-400"
        >
          {receipt}
        </span>
      )}
    </div>
  );
}

/** 视图 → 组清单。
 *
 * 丁T4 的结构表来自后端；旧后端（无 `groups`）在此**回落到单轴视图**（用顶层
 * current/readback + switchKind 合成一组）——这样渲染分支只有一套，不会出现
 * 「新老两条渲染路径」的分叉（分叉正是口径漂移的温床）。
 *
 * # 前向兼容的**如实边界**（T4 复评 M1：这不是「零损失」，是**降级**）
 *
 * 回落路径用 `step = switchKind === "shiftTab"` 决定渲染哪条分支，且 `tiers: []`
 * （旧后端不下发档位表，前端无从知道该工具有哪些档、屏显标签叫什么）。后果：
 * - `shiftTab` 的旧后端（claude / opencode，**以及 T4 之前的 kimi**）→ 渲染
 *   「切换模式」钮 + 顶层 current 回显，与旧前端行为一致（**无损失**）；
 * - `slashCommand` 的旧后端（codex）→ 旧前端渲染的是 `plan`/`bypass` **两个逐档按钮**
 *   （`/plan` 与 `/permissions` 两条有命令证据的路），而 `tiers: []` 让新前端**一个
 *   逐档按钮都渲染不出来** —— 只剩回显。**这是功能降级，不是等价兼容。**
 *
 * 为什么接受这个降级而不做「旧后端硬编码一份 codex 档位表」：那份表会立刻成为第二份
 * 真源（§2.6 的规格表 + 后端结构表已经是两份，再加前端硬编码就是三份），而后端升级
 * 到 T4 后它就变成**永不执行的死代码**——本仓既往的口径漂移大多源于这种「过渡期硬
 * 编码」。过渡期的正确做法是**如实告知**：旧后端 + codex 时 ModeBar 只回显不给切换
 * 入口，用户在终端操作或升级后端即可。 */
function modeGroups(view: SessionModeView): ModeGroupView[] {
  if (view.groups !== undefined && view.groups.length > 0) return view.groups;
  // 旧后端回落：单轴 + 顶层 current。tiers 空（旧后端不下发档位表）——
  // `slashCommand` 的旧后端（codex）因此失去逐档按钮（见上方如实边界）。
  return [
    {
      id: "mode",
      label: "模式",
      step: view.switchKind === "shiftTab",
      readback: view.readback,
      current: view.current,
      currentLabel: view.currentLabel,
      tiers: [],
      legacy: [],
    },
  ];
}

/** 组内档位 chip 的统一外观——模式组（toggle）与权限组（picker）**同款**
 *  （2026-10-10 用户指令「风格统一」三版：不高亮 = 灰底正常字体；高亮 =
 *  **黑色边缘 + 字体加粗**，无底色——与「完全信任」高亮观感一致）。
 *  当前档不压暗（disabled 是「不可点」语义不是置灰，评审 P2-1）；
 *  非当前档 disabled（含问答待决置灰）时降透明度。 */
function chipClass(isCurrent: boolean): string {
  return `rounded-full border px-2 py-0.5 text-[11px] ${
    isCurrent
      ? "border-[var(--tx)] bg-transparent font-semibold text-[var(--tx)]"
      : "border-transparent bg-[var(--cb)] text-[var(--tx)] disabled:opacity-40"
  }`;
}

/** 单组渲染：组标题（仅二维时显示）+ 切换 chips。
 *  E3④：当前档 chip **高亮**（data-current + 反色样式）；问答待决时全组禁用。
 *  2026-09-23 codex 模式切换改造 / **2026-10-10 用户指令改版（二版）**：
 *  - `layout === "toggle"`（codex 模式组）→ **两档 chips 即按钮**：当前档高亮
 *    **不可点**，另一档可点，点击 = 向终端发一次 shift+tab（**无零投递闸**：卡面
 *    档位过期时点按钮也必然动作——这正是「切换不了」的修复面；后端按前读翻转
 *    核验，卡面跟随屏读结果高亮）。档位未知（current=null）时两档都可点——
 *    点哪个都是发一次 shift+tab，落点以屏读为准（用户裁决：屏读必有解，未知态
 *    实际不存在，这里只作纵深防御）；上一版的 [◀▶]「切换模式」按钮已删；
 *  - `layout === "picker"`（codex 权限组）→ **四档 chips 常驻**（只读/默认/自动审批/
 *    完全信任，来自 tiers 里 selectable 档；生效档高亮框选中），点 chip = 走 switch
 *    端点既有 Menu 编排（数字直达 + Full Access 确认框阶段代按）；「完全信任」保留
 *    先武装二次确认条的前端防线；既有「切换权限」picker 入口与「已退役：…」说明行
 *    **已删**（用户指令：四 chips 高亮已承载全部状态信息，二级入口不再保留）；
 *  - 「完全信任」二次确认（2026-09-23 用户裁决，chips 同样遵守）：点完全信任先出
 *    确认条，确认后才发——codex 会连发两次按键（4→1）并代按终端的风险确认框；
 *  - 权限组 current 来源 = **记忆回落**（`currentSource === "memory"`，终审 P1-2：
 *    readback 全开后旧判据 `readback === false` 不可达）时标注「（上次切换）」——
 *    它是 兔维斯 的记忆，不是实时屏读（终端手改会失真，如实声明口径）；屏读来源
 *    （"screen"）是实时权威，不标注。 */

function ModeGroupRow({
  group,
  showGroupLabel,
  busy,
  onSwitch,
}: {
  group: ModeGroupView;
  showGroupLabel: boolean;
  /** E3④：busy 含问答待决置灰（父层合并——待决时全组按钮禁用） */
  busy: boolean;
  onSwitch: (target: MamMode) => void;
}) {
  const currentText = group.currentLabel ?? "模式未知";
  const unknown = group.current === null;
  // 完全信任二次确认的待确认态（只在含 bypass 档的权限组用得上）
  const [bypassArmed, setBypassArmed] = useState(false);
  const hasBypass = group.tiers.some((t) => t.mode === "bypass" && t.selectable);
  return (
    <span data-testid={`mode-group-${group.id}`} className="flex flex-wrap items-center gap-2">
      {showGroupLabel && (
        <span
          data-testid={`mode-group-label-${group.id}`}
          className="text-xs font-bold text-[var(--tx)]"
        >
          {group.label}
        </span>
      )}
      {/* 单组（无组标题）时给当前档一个「当前」前缀；二维两行组已有组标题
          （模式/权限），再叠「模式」二字会读成「模式 模式 …」（用户 2026-09-23） */}
      {/* 当前档文本（2026-10-10 用户指令）：toggle/picker 组当前态由 tabs/chips 高亮框
          承载 → 不再渲染文本；其余布局（单轴家无内联指示器）保留文本回显。
          「未知」提示与「（上次切换）」标注保留（如实申报口径不变） */}
      {group.layout !== "toggle" && group.layout !== "picker" && (
        <span
          data-testid={`mode-current-${group.id}`}
          className={`text-xs font-semibold ${
            unknown ? "text-amber-700 dark:text-amber-400" : "text-[var(--tx)]"
          }`}
        >
          {currentText}
        </span>
      )}
      {!unknown && group.id === "permission" && group.currentSource === "memory" && (
        <span
          data-testid={`mode-current-source-${group.id}`}
          className="text-[10px] text-[var(--mut)]"
        >
          （上次切换）
        </span>
      )}
      {unknown && (
        <span
          data-testid={`mode-unknown-hint-${group.id}`}
          className="text-[11px] text-amber-700 dark:text-amber-400"
        >
          请人工核对终端当前模式
        </span>
      )}
      {group.layout === "picker" ? (
        // **四档 chips 常驻**（2026-10-10 用户指令）：只读/默认/自动审批/完全信任
        // 直选——生效档（GET current，屏读→记忆→未知三级回落）**高亮框选中**；
        // 点 chip = 走 switch 端点既有 Menu 编排（/permissions → 屏读定位数字 →
        // 发数字 → Full Access 确认框阶段代按）。编号猜测仍不在前端——档位→数字
        // 的映射由后端屏读完成。「切换权限」二级入口已删（二版指令）——Menu 编排
        // 失败时走 mode-error 文案引导，不再保留面板兜底。
        <span className="flex flex-wrap items-center gap-1">
          {group.tiers
            .filter((t) => t.selectable)
            .map((t) => {
              const isCurrent = group.current !== null && group.current === t.mode;
              return (
                <button
                  key={t.mode}
                  type="button"
                  data-testid={`mode-switch-${group.id}-${t.mode}`}
                  data-current={isCurrent ? "true" : "false"}
                  disabled={busy}
                  onClick={() => {
                    if (t.mode === "bypass") {
                      setBypassArmed(true); // 不直接发——等「确认启用」（二次确认防线 chips 同守）
                    } else {
                      onSwitch(t.mode);
                    }
                  }}
                  className={chipClass(isCurrent)}
                >
                  {t.label}
                </button>
              );
            })}
        </span>
      ) : group.layout === "toggle" ? ( // codex 模式组两档 chips 即按钮（2026-10-10 二版指令）：
        // 当前档高亮不可点，另一档可点 = 发一次 shift+tab；**无零投递闸**——卡面
        // 档位过期时点按钮也必然动作，落点由后端屏读核验（前读翻转），卡面跟随
        // 屏读结果高亮。上一版三段 [计划][◀▶][操作] 的「切换模式」按钮已删。
        (() => {
          const labelFor = (m: MamMode): string =>
            group.tiers.find((x) => x.mode === m)?.label ?? (m === "plan" ? "计划" : "操作");
          return (
            <span className="flex items-center gap-1">
              {(["plan", "default"] as const).map((m) => {
                const isCurrent = group.current === m;
                return (
                  <button
                    key={m}
                    type="button"
                    data-testid={`mode-switch-${group.id}-${m}`}
                    data-current={isCurrent ? "true" : "false"}
                    // 当前档不可点（用户指令：高亮的那档禁用，点另一档=切过去）；
                    // 档位未知时两档都可点（纵深防御，落点以屏读为准）
                    disabled={busy || isCurrent}
                    title="向终端发送一次 shift+tab，切档结果以终端屏读为准（卡面跟随高亮）"
                    onClick={() => onSwitch(m)}
                    className={chipClass(isCurrent)}
                  >
                    {labelFor(m)}
                  </button>
                );
              })}
            </span>
          );
        })()
      ) : group.step ? (
        // 步进轴（claude/opencode）：单钮「切换模式」——shift+tab 一次一档，
        // 目标档由环序决定（后端按实测环序回读核对，前端不假装直达）
        <button
          type="button"
          data-testid={`mode-switch-next-${group.id}`}
          disabled={busy}
          onClick={() => onSwitch("default")}
          className="rounded-full bg-[var(--cb)] px-2 py-0.5 text-[11px] text-[var(--tx)] disabled:opacity-40"
        >
          切换模式
        </button>
      ) : (
        // 直达轴（模式组/权限组）：逐档按钮。**只渲染 selectable 的档**（裁7 的
        // 退役档根本不进 tiers）。
        // **当前档高亮**（E3④）：`group.current === t.mode` 的按钮反色 + data-current。
        // 「完全信任」特例（2026-09-23 用户裁决）：先武装二次确认条，确认后才发。
        <span className="flex flex-wrap items-center gap-1">
          {group.tiers.map((t) => {
            const isCurrent = group.current !== null && group.current === t.mode;
            return t.selectable ? (
              <button
                key={t.mode}
                type="button"
                data-testid={`mode-switch-${group.id}-${t.mode}`}
                data-current={isCurrent ? "true" : "false"}
                disabled={busy}
                onClick={() => {
                  if (t.mode === "bypass") {
                    setBypassArmed(true); // 不直接发——等「确认启用」
                  } else {
                    onSwitch(t.mode);
                  }
                }}
                className={`rounded-full px-2 py-0.5 text-[11px] disabled:opacity-40 ${
                  isCurrent
                    ? "bg-[var(--cb)] font-semibold text-[var(--tx)]"
                    : "bg-[var(--cb)] text-[var(--tx)]"
                }`}
              >
                {t.label}
              </button>
            ) : (
              // 不可选档：不渲染按钮，只给一句灰字（reason 来自后端，单一来源）
              <span
                key={t.mode}
                data-testid={`mode-tier-disabled-${group.id}-${t.mode}`}
                title={t.reason ?? undefined}
                className="rounded-full border border-dashed border-[var(--cb)]/40 px-2 py-0.5 text-[11px] text-[var(--mut)]"
              >
                {t.label}（不可用）
              </span>
            );
          })}
        </span>
      )}
      {/* 裁7 退役旧档说明行已删（2026-10-10 二版用户指令）：chips 高亮已承载全部
          状态信息，「已退役：untrusted / on-failure」纯文本不再渲染（后端仍下发
          legacy 字段，前端不消费——档位表单一真源不变） */}
      {/* 完全信任二次确认条（2026-09-23 用户裁决的前端防线；未确认不发任何请求） */}
      {hasBypass && bypassArmed && (
        <span
          data-testid="mode-bypass-confirm"
          className="flex w-full flex-wrap items-center gap-2 rounded-lg bg-rose-500/10 px-2 py-1 text-[11px] text-rose-700 dark:text-rose-300"
        >
          <span>将连发两次按键（4→1）启用完全信任：终端会弹出风险确认框，由 兔维斯 代按确认。</span>
          <button
            type="button"
            data-testid="mode-bypass-confirm-yes"
            disabled={busy}
            onClick={() => {
              setBypassArmed(false);
              onSwitch("bypass");
            }}
            className="rounded-full bg-rose-600 px-2 py-0.5 font-semibold text-white disabled:opacity-40 dark:bg-rose-500"
          >
            确认启用
          </button>
          <button
            type="button"
            data-testid="mode-bypass-confirm-no"
            disabled={busy}
            onClick={() => setBypassArmed(false)}
            className="rounded-full bg-[var(--cb)] px-2 py-0.5 text-[var(--tx)] disabled:opacity-40"
          >
            取消
          </button>
        </span>
      )}
    </span>
  );
}
