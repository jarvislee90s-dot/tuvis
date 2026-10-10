// 移动端问答卡（批次乙 T8，AskUserQuestion 问答卡 · claude 先行）：挂在
// SessionDetail 正文视图 / 分屏对话列（**非结束态**；丁T1 放宽——ApproveCard 仍是
// waiting 门，key 前缀 question-* 与 approve-*/composer-* 互异防 duplicate-key）。
// - 可用性：挂载拉取一次 /session-question；**状态跃迁重拉**（丁T1 复评 F-1：
//   effect deps 含 `session.status`——详情页停留期间 Board 的既有数据通道
//   （SSE 跃迁/快照 + 降级轮询）把活会话 status 对齐进 selected，status 一变即
//   重拉一次：终端答完题（waiting → processing/idle）卡随之消失，新问题出现
//   （→ waiting）卡随之浮现；无需重进页面）；拉取失败 / 网络异常 → 静默自隐；
//   available=false（双通道未命中 / 审批标记隔离）→ 自隐（fetchApproveOptions 惯例）；
// - **问答模式不出 允许/拒绝**（ApproveCard 的映射键位对问答无意义——后端硬约束①
//   同时保证问答会话上 approve 端点不可用，红卡自隐；本卡自身也零允许/拒绝字样）；
// - 渲染：题干（header 徽标 + question 文本）+ 选项按钮（编号+label+description，
//   编号从 1 起）；「取消」钮 = Esc（探测 K3：取消/拒绝整个问题）；
// - 单选（multiSelect=false）：点选项 → POST select{index}（后端注入对应数字单键——
//   探测 K1/K2 定案：数字直接勾选并提交，无回车、严禁后补 Esc）；
// - 多选（multiSelect=true）：点选 = POST toggle{index}（后端**闭环切勾阶段机**：
//   屏读定位 → 方向键走位 → 空格 → 屏读校验翻转——2026-09-24 数字路径被用户实机
//   推翻废止，档案 2026-09-24-claude多选多题键序-用户实机取证）+ 本地勾选态
//   （回执带 `checked` 屏读真值时以它为准；缺失才盲翻）+「提交」钮 → POST submit
//   （后端三段式：走位到推进行（Submit/Next）→ enter → 屏上编号确认，探测 K10——
//   Enter 当提交是反直觉反例 K9，前端绝不自行拼 Enter）；
// - **丁T5 起三段式改为后端阶段机闭环**（§2.3 裁4）：submit 的响应带 `done`/`stage`/
//   `verified`——走完整条（提交行 → 走位 → 回车 → Review 屏 → 抄屏上编号确认 → 终态）
//   才显示完成；中途任一段屏读不符 → `failed{aborted:true, stage}`，卡片显示**中止在
//   哪一段 + 原因 + 引到终端**（不是笼统的「已发送按键」）；
// - **进行中态**（§2.3「卡片进行中态替代『已发送按键』」）：请求在途期间卡片显示
//   「进行中（走到哪一段）」——后端的段推进是同步的（一次请求内走完），故前端只需
//   一个总进行中态 + 段名文案（`QUESTION_STAGE_LABELS`）；
// - 自由文本（**丁T5 §2.4 入口 1**；支持面随批次逐步放开——2026-10-10 起
//   claude/kimi/opencode/**codex** 四家全通；codex = 0.162.1 复验复活，走位
//   Other → tab 开备注 → 文本 → 回车，序列与其三家不同族由后端编排屏蔽）：
//   卡内嵌输入框 + 「作为回答发送」→ POST freeText{text}。文本经归一
//   且**不带** `[mobile]` 签名。
//   **两个不渲染输入框的情形**（都渲染「请在终端作答」引导，**不假装能发**）：
//   ① 工具未定案（未知工具；`info.freeText !== true`，§2.8）；
//   ② **多选题**（复评 F6-3）：多选屏的自由作答行带勾选框
//      （`4. [ ] Type something`，实机截图 `C-s8-cursor-submit-*.png`），定位判据
//      （剥编号后以 `Type something` 开头）不匹配 → 后端恒拒 409；前端同步不给按钮。
//      证据与放宽前提见 `inject::question::free_text_shape_supported` 的文档；
// - 多问题（questions.length>1，批次戊 E4-E6 起）：**逐题交互卡**——单选题点数字
//   即答（终端自动推进下一题，前端同步切题）；多选题点数字=仅勾选（**不切题**，
//   2026-09-23 错位修复）+「切换题目」钮显式发 tab（仅 opencode，`advance` 旗标）；
//   全部题目翻完后出**确认卡**（提交/返回题目/取消）。未定案工具维持只读。
// - 应答分診：key_sent → 终态（按钮禁用）——阶段机动作另显 `verified` 的三态；failed
//   {error} → 错误文案可重试（`aborted:true` 时额外显示段名 + 引到终端）；ApiError
//   （409/400 带 data.error）→ 分診中文文案：no_question→「当前没有待回答的问题」、
//   multi_questions→「多个问题请回到终端完成作答」、tool_readonly→「该工具的远程作答
//   尚未实测，请在终端完成作答」、bad_index→「选项序号无效，请刷新后重试」、
//   其余显示 message。
// **已知限制（丁T1 复评 F-1，如实申报）**：重拉只由**状态跃迁**驱动，不做卡内轮询
// （轮询超 T1 范围，丁T2 另有安排）。因此同一 waiting 窗口内的非跃迁变化——例如
// 模型连续提两组问题、或用户改答但状态未变——不会自动重拉，需等下一次状态跃迁
// （或重进详情页）。`session.id` 变化会重拉（跨会话串卡防线，`key` 也随 id 强制重挂）。
// **已知限制之二（丁T1 复审 F2-3，opencode 的载荷就绪空窗）**：opencode 的 question
// part 是「part 行先落盘、题目数据后到」——`pending` 事件里 `state.input` 恒为 `{}`
// （questions 尚未写入），要到 `running` 拍才有。实测空窗 **5ms–4s**（数据源见
// `monitor::opencode_parser::pending_question_part` 文档）。影响面：
// - **红灯不受影响**：状态链用**工具名**分支判待决（`input` 为空也成立）；
// - **卡片在这一拍确实无法渲染**：端点拿不到 questions，`available=false` → 卡自隐
//   （没有任何题目文本可显示，不是逻辑错，是数据未就绪）；
// - 卡片会在下一次**状态跃迁**驱动的重拉时出现（同属上一条「非跃迁变化不自动重拉」
//   的限制族）。**不为它加占位卡或轮询**（超 T1 范围；重拉触发机制归丁T2 收口面）。
import { useCallback, useEffect, useRef, useState } from "react";
import InteractiveCard, { toneTokens } from "./InteractiveCard";
import {
  ApiError,
  fetchSessionQuestion,
  questionAnswerErrorCopy,
  sessionQuestionAnswer,
  type QuestionAnswerAction,
  type QuestionAnswerStage,
  type QuestionInfoView,
  type QuestionView,
} from "./api";

/** 阶段名 → 用户可读文案（**进行中态与中止回执共用**，两处不会漂移）。
 *  取值与后端 `remote::api::QUESTION_STAGE_*` 逐字对应（见 api.ts 的
 *  `QuestionAnswerStage` 注释）。 */
const QUESTION_STAGE_LABELS: Record<QuestionAnswerStage, string> = {
  "submit-row": "定位提交入口",
  review: "等待确认屏",
  confirm: "确认提交",
  receipt: "核对完成回执",
  "free-row": "定位自由作答行",
  "free-text": "提交回答文本",
  "toggle-row": "定位选项行并切勾",
  advance: "切换到下一题",
  select: "定位选项行并选择",
};

/** 阶段名 → 进行中文案（比 `QUESTION_STAGE_LABELS` 更像「正在做什么」——
 *  同一段在「进行中」与「中止」两个语境里的措辞不同，两套文案都在本文件内。 */
const QUESTION_STAGE_PROGRESS: Record<QuestionAnswerStage, string> = {
  "submit-row": "正在定位提交入口（屏读确认）…",
  review: "已提交勾选，正在等待确认屏…",
  confirm: "确认屏已出现，正在确认提交…",
  receipt: "正在核对完成回执…",
  "free-row": "正在定位自由作答输入行…",
  "free-text": "正在提交回答文本…",
  "toggle-row": "正在定位选项行并按空格切勾（屏读校验）…",
  advance: "正在 ←/→ 切换题目（屏读核对）…",
  select: "正在定位选项行并选择…",
};

/** 自由作答文本长度上限（与 composer 的 `MAX_SEND_CHARS` 对齐；后端同口径 400） */
const MAX_FREE_TEXT_CHARS = 10000;

/** 问答载荷的**内容指纹**（丁T1 复评 F2-1，纯函数）：题干 + 每题的选项标签与顺序
 *  + 多选标记 + 选项描述的组合摘要。同一问题重复拉取恒等；模型换题或改选项即变。
 *  为何不用 `JSON.stringify(info)` 直接比：载荷里含 `source`（"mark"/"scan"）——
 *  同一问题从标记通道落到扫描通道会翻字符串但问题并未变化，那不该重置终态。 */
/** 题干归一化（与后端 `screen_question_matches` 的 norm 同规则镜像）：剥空白 +
 *  控制台读写损失字符（U+FFFD/变体选择符/键帽/星面 emoji）——屏读快照与载荷题干
 *  的对位判据（卡面状态权威源的对位面） */
function normalizeQuestionText(s: string): string {
  return [...s]
    .filter((c) => {
      const code = c.codePointAt(0) ?? 0;
      return (
        !/\s/.test(c) &&
        c !== "�" &&
        !(code >= 0xfe00 && code <= 0xfe0f) &&
        code !== 0x20e3 &&
        code < 0x10000
      );
    })
    .join("");
}

/** 屏读快照的题干 → 载荷题下标（评审 I3 不猜纪律）：归一化后空串跳过、精确相等
 *  优先、**多个匹配 → null**（放弃同步而不是错跳题） */
function findQuestionByHeading(questions: QuestionView[], heading: string): number | null {
  const h = normalizeQuestionText(heading);
  if (h.length < 4) return null; // 判别力不足（与后端 screen_question_matches 同口径）
  const exact: number[] = [];
  const partial: number[] = [];
  questions.forEach((q, i) => {
    const pq = normalizeQuestionText(q.question);
    if (!pq) return;
    if (pq === h) exact.push(i);
    else if (pq.includes(h) || h.includes(pq)) partial.push(i);
  });
  if (exact.length === 1) return exact[0];
  if (exact.length === 0 && partial.length === 1) return partial[0];
  return null; // 0 个或多个匹配 → 不猜
}

/** codex 回执快照的**附带勾选/TS 通道守卫**（评审 F4.3）：codex 面板形状无
 *  checked/freeText 通道（快照只携带题号头计数与焦点），直读 questionIdx 对位
 *  后不调 writeSnapshotState；仅当 heading 恰好唯一命中 dest（防御：其他工具
 *  形状夹带 questionIdx 的字段膨胀）才走 writeSnapshotState 回填勾选/TS 行。 */
function landedCodexHeadingMatches(
  info: QuestionInfoView,
  dest: number,
  heading: string | undefined
): boolean {
  if (heading == null) return false;
  return findQuestionByHeading(info.questions, heading) === dest;
}

/** 确认卡摘要查询（2026-10-07 权威源切换）：题干归一键 → 终端 Review 页答案。
 *  精确键优先，长度 ≥4 的包含关系兜底（与 findQuestionByHeading 的 exact/partial
 *  纪律同构——折行归并后的键对不上精确键时仍可命中）；查不到 → undefined，
 *  调用方回落本地缓存渲染（不猜）。 */
function lookupSummaryAnswer(
  summary: Record<string, string>,
  question: string
): string | undefined {
  const key = normalizeQuestionText(question);
  if (summary[key] !== undefined) return summary[key];
  return Object.entries(summary).find(
    ([k]) => k.length >= 4 && (key.includes(k) || k.includes(key))
  )?.[1];
}

/** 屏读快照的勾选/自由作答 → 卡面状态回填内核（review 三态与 freeTextPresent
 *  三态的判定单点）：`qi` = 已对位的题下标。单题卡勾选走 `checked`，多题卡走
 *  `mqChecked`（I7）。`multiSelect` = 该题是否多选——**单选题的屏上选中态
 *  （行尾 ✓）经 checked 通道到达后回写 mqSelected**（卡面单选选项的选中渲染
 *  源，2026-10-06 活体定案）；屏上无选中 → 清本地 mqSelected（屏读为准）。
 *  返回该题的勾选 Set。 */
function writeSnapshotState(
  screen: {
    checked?: (boolean | null)[];
    freeText?: string | null;
    freeTextPresent?: boolean;
  },
  qi: number,
  singleCard: boolean,
  setters: {
    setChecked: (set: Set<number>) => void;
    setMqChecked: (
      updater: (prev: Record<number, Set<number>>) => Record<number, Set<number>>
    ) => void;
    setMqFreeText: (updater: (prev: Record<number, string>) => Record<number, string>) => void;
    setMqSelected: (updater: (prev: Record<number, number>) => Record<number, number>) => void;
    /** 输入格直接同步（2026-10-07 用户指令：刷新后输入格=终端真值，不留本地残留） */
    setFreeText?: (v: string) => void;
  },
  multiSelect = true
): Set<number> {
  const set = new Set<number>();
  (screen.checked ?? []).forEach((c, i) => c === true && set.add(i));
  // I7：单题卡的勾选渲染走 `checked`（不是 mqChecked）
  if (singleCard) {
    setters.setChecked(set);
  } else {
    setters.setMqChecked((prev) => ({ ...prev, [qi]: set }));
  }
  // 单选：屏上 ✓ 行 → mqSelected；屏上无选中 → 清（权威源=屏读）。
  // **可读性门**（2026-10-07）：仅在屏读回带**非空 checked 数组**时覆盖——kimi 单选
  // 页选中项无字符标记（高亮读不到，heading-only 快照 checked 恒空），空数组 =
  // 「读不到」≠「没有」→ 保留本地记忆（AGENTS.md 申报边界：该面快照权威源不覆盖）；
  // claude/opencode 单选页 checked 恒带逐行真值（[null×N] 起）→ 行为不变。
  if (!multiSelect && (screen.checked?.length ?? 0) > 0) {
    const picked = [...set][0];
    setters.setMqSelected((prev) => {
      if (picked !== undefined) {
        return prev[qi] === picked ? prev : { ...prev, [qi]: picked };
      }
      if (!(qi in prev)) return prev;
      const n = { ...prev };
      delete n[qi];
      return n;
    });
  }
  if (screen.freeTextPresent) {
    if (screen.freeText !== null && screen.freeText !== undefined) {
      setters.setMqFreeText((prev) => ({ ...prev, [qi]: screen.freeText as string }));
    } else {
      // 占位 = 终端已无内容 → 清本地残留（评审 I1）
      setters.setMqFreeText((prev) => {
        const n = { ...prev };
        delete n[qi];
        return n;
      });
    }
    // **输入格直接同步**（2026-10-07 用户指令）：刷新后输入格 = 终端真值
    //（有文字 → 填入；占位 → 清空），不留上一题/上一轮的本地残留
    setters.setFreeText?.(screen.freeText ?? "");
  }
  return set;
}

/** 交互回执的屏读快照 → 卡面状态回填（2026-10-03 屏读为准·交互后核对）：
 *  归属 = 当前交互的题（toggle/select 都带 questionIndex），不涉 heading 对位。 */
function applyInteractionScreen(
  screen: {
    checked?: (boolean | null)[];
    freeText?: string | null;
    freeTextPresent?: boolean;
  },
  questionIndex: number,
  setters: {
    setChecked: (set: Set<number>) => void;
    setMqChecked: (
      updater: (prev: Record<number, Set<number>>) => Record<number, Set<number>>
    ) => void;
    setMqFreeText: (updater: (prev: Record<number, string>) => Record<number, string>) => void;
    setMqSelected: (updater: (prev: Record<number, number>) => Record<number, number>) => void;
  },
  multiSelect = true
): void {
  writeSnapshotState(screen, questionIndex, false, setters, multiSelect);
}

function questionFingerprint(info: QuestionInfoView): string {
  return info.questions
    .map((q) =>
      [
        q.header,
        q.question,
        q.multiSelect ? "m" : "s",
        q.options.map((o) => `${o.label}\u0001${o.description}`).join("\u0002"),
      ].join("\u0003")
    )
    .join("\u0004");
}

interface QuestionCardProps {
  /** 会话：只消费 id（请求键）与 status（重拉触发键，丁T1 复评 F-1）。
   *  结构化类型——完整 Session 可直接传入，测试可只给这两字段 */
  session: { id: string; status?: string };
}

export default function QuestionCard({ session }: QuestionCardProps) {
  // 可用性：ready=false（加载中 / 拉取失败）→ 不渲染
  const [info, setInfo] = useState<QuestionInfoView | null>(null);
  const [ready, setReady] = useState(false);
  // 应答进行中（防连点）
  const [busy, setBusy] = useState(false);
  // **进行中态**（丁T5 §2.3）：请求在途期间显示「进行中（走到哪一段）」——
  // 阶段机动作（submit/freeText）是**一条同步请求内走完整条闭环**的，故前端拿不到
  // 中间段；这里显示的是「已发起的动作」，段名文案按动作类型取首段（`stage` 字段
  // 只有中止时才有真值）。请求返回即被终态或中止态取代。
  const [inProgress, setInProgress] = useState<QuestionAnswerStage | null>(null);
  // 多选本地勾选态（仅在 toggle 成功回执后切换——端点拒绝时本地状态不漂移）
  const [checked, setChecked] = useState<Set<number>>(() => new Set());
  // **多题卡**按题记忆的勾选态（2026-09-23 错位修复）：toggle 只作用于当前题，
  // 「切换题目/返回题目」后各题勾选态保留（与终端实际勾选一致——手机端做过的
  // 每次 toggle 都记录在案；用户在终端手动改动仍无法感知，属既有已知限制）
  const [mqChecked, setMqChecked] = useState<Record<number, Set<number>>>(() => ({}));
  // **每题已选答案**（2026-10-02 确认卡答案清单）：mqSelected=单选题选中项（0 起
  // 选项下标——select 推进时记录）；mqFreeText=自由作答文本（多选 Type something
  // 内联编辑 / 单题同源）。本地记忆渲染，终端为准（脚注申报）
  const [mqSelected, setMqSelected] = useState<Record<number, number>>({});
  const [mqFreeText, setMqFreeText] = useState<Record<number, string>>({});
  // 编辑模式（2026-10-02）：已写入后输入框只读；点「编辑」解锁，再发送走覆盖写入
  const [editingFreeText, setEditingFreeText] = useState(false);
  // 自由作答「已发送但未核验」（评审 I1）：回执无屏读真值时提示，不虚报已写入
  const [ftUnverified, setFtUnverified] = useState(false);
  // 终态：按键序列已投递（key_sent）——按钮禁用 +「已发送按键」。**toggle 不算终态**
  // （多选点选后仍需「提交」，置终态会锁死提交钮）
  const [sent, setSent] = useState(false);
  // 阶段机走完全链后的**终态回执核验**（丁T5）：true=屏读到终态锚（确认完成）；
  // false=读到屏但未见锚（不谎报，提示人工核对）；null/undefined=读屏不可用。
  // 非阶段机动作（select/toggle/cancel）恒 null（无此语义）。
  const [verified, setVerified] = useState<boolean | null>(null);
  // 失败文案（failed{error} 回执 / ApiError 分診）——非 null 展示，按钮保持可点
  const [error, setError] = useState<string | null>(null);
  // 中止的段名（丁T5：`failed{aborted:true, stage}`）——与 `error` 并存：
  // error 是后端的整句中文说明，stage 供渲染「卡在哪一段」的进度语义
  const [abortedStage, setAbortedStage] = useState<QuestionAnswerStage | null>(null);
  // 自由作答输入框内容（**仅单题卡 + info.freeText === true 时渲染**）
  const [freeText, setFreeText] = useState("");
  /** freeText 现值镜像（供 applyScreenSync 等零依赖回调读现值——不进依赖数组，
   *  避免 GET 重拉 effect 随每次按键重建） */
  const freeTextRef = useRef("");
  // 提交后同步（react-hooks/refs 禁止渲染期写 ref）——applyScreenSync 均在
  // 异步回调里读，commit 后的镜像值即现值
  useEffect(() => {
    freeTextRef.current = freeText;
  }, [freeText]);
  /** 上次 GET 屏读的题号（0 起；null = 尚无屏读）——换题检测用 */
  const lastScreenIdxRef = useRef<number | null>(null);
  // E4-E6 多题交互：当前作答到第几题（0 起；answer 成功且非末题时 +1）
  const [mqIndex, setMqIndex] = useState(0);
  // codex 面板快照的未答数（2026-10-09 设计 §3.1）——0/null 不显示
  const [unansweredHint, setUnansweredHint] = useState<number | null>(null);

  // 拉取（挂载一次 + 状态跃迁重拉，丁T1 复评 F-1）：deps 含 `session.status`——
  // 详情页停留期间 Board 数据通道把活会话 status 对齐进 selected（App.tsx
  // handleSessionsChanged），status 一变即重拉：答完题卡消失、新问题卡浮现。
  // 「非状态跃迁的变更不自动重拉」是已知限制（见文件头注释）。
  const status = session.status;
  // 上一轮载荷的**内容指纹**（丁T1 复评 F2-1）：用于判定「这一轮拿到的是不是新问题」
  const lastFingerprint = useRef<string | null>(null);
  // **屏读快照应用**（2026-10-03 屏读为准）：GET/中止后重拉 共用的纠偏入口——
  // Review 在场 → 进确认卡；题屏 → heading 对位 + writeSnapshotState 回填
  /** Review 页屏读摘要（确认卡权威源）：题干归一键 → 终端答案（2026-10-07） */
  const [confirmSummary, setConfirmSummary] = useState<Record<string, string>>({});
  const applyScreenSync = useCallback((v: QuestionInfoView) => {
    if (!v.available || !v.screen) return;
    // codex 题号对位（2026-10-09 设计 §3.1）：questionIdx 直读对位——不依赖
    // 题干文本匹配（codex 题干区可能带状态栏杂讯）。unanswered 驱动进度提示。
    // 不调 writeSnapshotState：codex 快照形状不同（无 checked/freeText）——
    // 题号对位即可，选中态维持本地乐观显示（与 kimi mqSelected 同口径，
    // 设计 §5.3 明示的边界）
    if (typeof v.screen.questionIdx === "number") {
      const qi = Math.min(v.screen.questionIdx, v.questions.length - 1);
      setMqIndex(qi);
      setUnansweredHint(v.screen.unanswered ?? null);
      // **换题即清输入**（2026-10-10 21:23 教训）：屏读题号与上次不同 = 终端已翻题
      // ——输入框里旧题残留文字会与新题 note 状态矛盾（用户实测撞形），清掉
      let clearedInput = false;
      if (lastScreenIdxRef.current !== null && lastScreenIdxRef.current !== v.screen.questionIdx) {
        setFreeText("");
        freeTextRef.current = "";
        clearedInput = true;
      }
      lastScreenIdxRef.current = v.screen.questionIdx;
      // **终端 note 文字同步进输入框**（2026-10-10 用户指令：「对话框里应该同步
      // 把字给同步出来」）——note 有字 ∧ 输入框还没打字（含刚换题清空）→ 回填
      // （用户可见、可改后覆盖写入）；用户已打字不覆盖（本地优先，避免顶掉输入）
      const nt = v.screen.noteText;
      if (nt != null && nt !== "" && (clearedInput || freeTextRef.current.trim() === "")) {
        setFreeText(nt);
      }
      return;
    }
    if (v.screen.review) {
      // **确认卡摘要权威源切换**（2026-10-07）：summary = Review 页屏读解析的
      // 逐题（题干, 答案）——覆盖本地缓存记录（终端真值优先；终端没答的题如实
      // 显示未作答）。summary 缺失（解析不出）→ 维持本地缓存渲染（不猜）。
      setMqIndex(v.questions.length);
      if (v.screen.summary) {
        setConfirmSummary(
          Object.fromEntries(v.screen.summary.map((s) => [normalizeQuestionText(s.q), s.a]))
        );
      }
      return;
    }
    if (!v.screen.heading) return;
    const qi = findQuestionByHeading(v.questions, v.screen.heading);
    if (qi === null) return;
    setMqIndex(qi);
    writeSnapshotState(
      v.screen,
      qi,
      v.questions.length === 1,
      { setChecked, setMqChecked, setMqFreeText, setMqSelected, setFreeText },
      v.questions[qi]?.multiSelect ?? true
    );
    // 已打字（屏上文本）→ 只读呈现「已写入 + 编辑」（自旧内联逻辑归并）
    if (v.screen.freeText !== null && v.screen.freeText !== undefined) {
      setEditingFreeText(false);
    }
  }, []);
  // **交互纪元**（评审 I4）：每次应答动作自增——GET 快照落地时纪元已变 = 用户已
  // 交互，快照是旧时刻的屏面，**跳过应用**（防止晚到的快照把已前进的卡拉回去）
  const interactionEpoch = useRef(0);
  /** **主动重拉触发器**（2026-10-07）：kimi freeText 保存直达 Review 后会话状态
   *  无跃迁 → GET 轮询不读屏 → 确认卡摘要不更新——回执 review_reached 时自增
   *  触发上面的载荷重拉 effect（等效补一个跃迁），Review summary 随载荷回来。 */
  const [reloadTick, setReloadTick] = useState(0);
  useEffect(() => {
    let alive = true;
    setReady(false);
    // 重拉时清掉上一轮的错误文案（陈旧「没有待回答的问题」会误导新一轮）
    setError(null);
    const epochAtFetch = interactionEpoch.current;
    fetchSessionQuestion(session.id)
      .then((v) => {
        if (!alive) return;
        // **问题内容变化才重置终态/勾选态**（F2-1，2026-09-21 复评）：
        // 同一会话内可连续多次提问（实测 rollout-2026-09-21T13-44-08：单会话连续
        // 8 次 request_user_input，两两之间无 task_complete），而 key 是
        // `question-${session.id}` → **不重挂** → `sent=true` 会残留到下一题，
        // 用户看到一张写着「已发送按键」且无按钮的**伪终态**卡。
        // 为什么不用「无条件清」：投递成功（key_sent）到状态回落之间有短暂窗口，
        // 期间 `sent` 必须保留以**防连投**（同一次问答内重复按键会二次投递终端）。
        // 故判据取「内容变了才是新问题」——指纹 = 题目结构摘要（题干 + 选项标签
        // + 多选标记），对同一问题的重复拉取稳定不变。
        //
        // 边界：**不可用载荷不参与判据**（`available=false` → 指纹视为空串）——
        // 「拉不到题」与「换了题」是两回事：opencode 的 pending 拍 input 未就绪
        // （F2-3）就会短暂 available=false，若让它算「内容变化」，会把刚投递的
        // sent 清掉 → 按钮复活 → 防连投语义被削弱。
        const fp = v.available && v.questions.length > 0 ? questionFingerprint(v) : "";
        // 只在「两次都是可用载荷且内容不同」时重置（空串一律不触发重置）
        if (
          lastFingerprint.current !== null &&
          lastFingerprint.current !== "" &&
          fp !== "" &&
          lastFingerprint.current !== fp
        ) {
          setSent(false);
          setChecked(new Set());
          // 新问题的输入框清空（旧答案不该跟着新题走）
          setFreeText("");
          setMqSelected({});
          setMqFreeText({});
          setVerified(null);
          setAbortedStage(null);
          setMqIndex(0);
          setMqChecked({});
        }
        if (fp !== "") lastFingerprint.current = fp;
        setInfo(v);
        // **屏读快照同步**（2026-10-03 卡面状态权威源）：GET 带回终端当前态——
        // 停在题屏 → 对位到载荷题并纠偏 mqIndex/勾选/输入框（兔维斯 重启、终端手动
        // 作答等漂移场景的统一解法）；停在 Review → 直接进确认卡。
        // **2026-10-07 归一到 applyScreenSync**：此前这里是旧内联逻辑，review 分支
        // 只切确认卡不落 summary → 刷新后摘要丢失、四题全显「未作答」（后端已解析
        // 回传，前端没收货）；归一后 Review 摘要经同一条入口落 confirmSummary。
        if (v.available && v.screen && interactionEpoch.current === epochAtFetch) {
          applyScreenSync(v);
        }
        setReady(true);
      })
      .catch(() => {
        if (alive) setReady(false);
      });
    return () => {
      alive = false;
    };
  }, [session.id, status, reloadTick, applyScreenSync]);

  /** 动作后重拉 GET 并应用屏读同步（2026-10-10「每次操作后都读一次屏」）：freeText
   *  提交/清空、advance 翻页、freeText 中止四条路径共用——noteText/题号随新屏回来，
   *  卡面与输入框跟随终端真值。epoch 防旧回执覆盖新交互（评审 I4 同源）。 */
  const repullAndSync = useCallback(
    (epoch: number) => {
      void fetchSessionQuestion(session.id)
        .then((v2) => {
          if (interactionEpoch.current !== epoch) return;
          setInfo(v2);
          applyScreenSync(v2);
        })
        .catch(() => {});
    },
    [session.id, applyScreenSync]
  );

  const handleAnswer = useCallback(
    async (
      action: QuestionAnswerAction,
      index?: number,
      text?: string,
      /** E4-E6 多题交互：select/toggle 作用在第几题（0 起） */
      questionIndex?: number,
      /** claude 切题方向（2026-10-02 ←/→ 双向导航；仅 advance 消费） */
      direction?: "prev" | "next",
      /** 覆盖写入（仅 freeText 消费）：退格清空旧内容再打新字 */
      overwrite?: boolean
    ) => {
      if (busy || sent) return;
      setBusy(true);
      const epoch = interactionEpoch.current; // 本动作纪元（中止重拉的新旧判定）
      setError(null);
      setAbortedStage(null);
      // **进行中态**（丁T5）：按动作显示对应的首段文案（toggle 切勾链/advance 切题链/
      // 提交链/自由作答链）
      setInProgress(
        action === "freeText"
          ? "free-row"
          : action === "toggle"
            ? "toggle-row"
            : action === "advance"
              ? "advance"
              : "submit-row"
      );
      try {
        const res = await sessionQuestionAnswer(
          session.id,
          action,
          index,
          text,
          questionIndex,
          direction,
          overwrite
        );
        if ((res as { review?: boolean }).review === true) {
          // **动作后直达 Review**（select 末题推进 / toggle 末次勾完自动汇总）——
          // 直接切确认卡（终端真值），不等刷新
          setInProgress(null);
          setMqIndex(info?.questions.length ?? 0);
        }
        if (res.status === "key_sent") {
          if (action === "toggle" && typeof index === "number") {
            // 勾选态同步（2026-09-24）：回执带 `checked`（屏读核验到的**终端真值**）
            // 时**以它为准**设置本地位——不再盲翻（旧实现「成功即翻」会在屏读真值
            // 与预期不符时把卡面漂移掉）；`checked` 缺失/为 null（旧后端 / 读不到屏
            // 无法核验）→ 回落盲翻（toggle 本就是幂等切换，一次翻动是合理近似）。
            const applyChecked = (cur: Set<number>): Set<number> => {
              if (typeof res.checked === "boolean") {
                if (res.checked === cur.has(index)) return cur;
                const next = new Set(cur);
                if (res.checked) {
                  next.add(index);
                } else {
                  next.delete(index);
                }
                return next;
              }
              const next = new Set(cur);
              if (next.has(index)) {
                next.delete(index);
              } else {
                next.add(index);
              }
              return next;
            };
            if (typeof questionIndex === "number") {
              // **多题卡**勾选切换（2026-09-23 错位修复）：按题记忆本地位；
              // toggle 只翻勾选，**不推进题目**——多选题页的切勾不切页，
              // 推进只由 advance 显式触发，两通道同步
              setMqChecked((prev) => ({
                ...prev,
                [questionIndex]: applyChecked(new Set(prev[questionIndex] ?? [])),
              }));
              // **交互后屏读核对**（2026-10-03 屏读为准）：回执带整屏快照时，
              // 勾选/TS 行内容以屏读为准（覆盖上面的 applyChecked 近似）
              if (res.screen) {
                applyInteractionScreen(
                  res.screen,
                  questionIndex,
                  { setChecked, setMqChecked, setMqFreeText, setMqSelected },
                  info?.questions[questionIndex]?.multiSelect ?? true
                );
              }
            } else {
              // 单题卡勾选切换：成功回执后同步本地位（下轮渲染高亮）；不置终态
              setChecked((prev) => applyChecked(prev));
            }
          } else if (action === "advance") {
            // **切换题目**（2026-10-02 ←/→ 双向导航）：next = 下一题/Confirm 卡；
            // prev = 上一题（确认卡上 = 回到最后一题修改）。claude 已在 Review 屏
            // 请求下一题时回执 `advanced:false`（零按键，已在终点）→ **不推进**，
            // 停在确认卡（opencode 的 Confirm 页 tab=回绕第 1 题，回执无该字段且
            // 无 direction → 维持回绕行为）
            // **翻页后读 note**（2026-10-10 用户指令）：翻页后 note 行换题了——
            // 重拉 GET 让 noteText 随新题屏回来，输入框同步新题的终端备注
            repullAndSync(epoch);
            if (res.advanced === false) {
              // 已在 Review 屏（零按键）→ 前端直接进确认卡（评审 C1 前端面）
              setInProgress(null);
              if (info !== null) setMqIndex(info.questions.length);
            } else if (res.screen && typeof res.screen.questionIdx === "number" && info !== null) {
              // **codex 回执直读对位**（2026-10-09 评审 F3）：codex 面板形状快照的
              // 题号对位主键是 questionIdx（题干区可能带状态栏杂讯，heading 归属
              // 校验不适用）——直读 + Math.min clamp（末题 ▶ 回执 questionIdx=0
              // = 环形回首题，自然覆盖）。unanswered 随回执更新（评审 F6）。
              const dest = Math.min(res.screen.questionIdx, info.questions.length - 1);
              setMqIndex(dest);
              if (typeof res.screen.unanswered === "number") {
                setUnansweredHint(res.screen.unanswered);
              }
              // codex 快照无 checked/freeText 通道——选中态维持本地乐观显示
              //（设计 §5.3 明示的边界，与 GET 路径的 questionIdx 分支同口径）
            } else {
              // **目的地计算 + 屏读快照纠偏**（2026-10-03 屏读为准）：prev echo 确认
              // = 退回上一题；next 沿用既有推进/回绕；快照随回执到达 → 新题的勾选/
              // 自由作答以屏读为准覆盖本地记忆
              const dest =
                direction === "prev" && res.direction === "prev"
                  ? Math.max(0, mqIndex - 1)
                  : info !== null && mqIndex >= info.questions.length
                    ? 0
                    : mqIndex + 1;
              setMqIndex(dest);
              // 评审 I2：快照**归属校验**——heading 必须唯一对位到 dest，且勾选数
              // 与载荷选项数一致；校验不过 → 跳过应用（本地状态不被部分/异屏读损坏）
              // **输入格快照驱动**（2026-10-07）：快照归属命中（含单选页 heading-only
              // 形态）→ free_text 覆盖输入格；缺失 → 清空（不沿用上一题的本地残留）
              setFreeText(res.screen?.freeText ?? "");
              setMqFreeText((prev) => {
                const n = { ...prev };
                if (res.screen?.freeText) {
                  n[dest] = res.screen.freeText;
                } else {
                  delete n[dest];
                }
                return n;
              });
              const snapCheckedLen = (res.screen?.checked ?? []).length;
              const headingOnly = res.screen?.heading != null && snapCheckedLen === 0;
              // **unanswered 随回执更新**（评审 F6）：快照带未答计数（codex 面板
              // 形状扩展面）→ 直读刷新进度提示
              if (typeof res.screen?.unanswered === "number") {
                setUnansweredHint(res.screen.unanswered);
              }
              if (
                res.screen &&
                res.screen.heading &&
                info !== null &&
                findQuestionByHeading(info.questions, res.screen.heading) === dest &&
                (snapCheckedLen === info.questions[dest]?.options.length ||
                  // 单选页快照：无勾选框 → checked 空——heading 唯一命中即对位
                  // （勾选态不写，如实），页面位置同步不再被 checked 长度卡死
                  (headingOnly &&
                    findQuestionByHeading(info.questions, res.screen.heading) !== null))
              ) {
                writeSnapshotState(
                  res.screen,
                  dest,
                  info.questions.length === 1,
                  { setChecked, setMqChecked, setMqFreeText, setMqSelected },
                  info.questions[dest]?.multiSelect ?? true
                );
              }
            }
          } else if (
            action === "select" &&
            typeof questionIndex === "number" &&
            info !== null &&
            questionIndex < info.questions.length
          ) {
            // E4-E6 多题逐题推进：本题数字已发（单选题终端自动推进下一题；**末题
            // 单选答完终端自动进 Review/Confirm 页** → 前端也推进到确认卡——
            // 2026-09-23 修复：旧判据 `< length - 1` 把末题单选误置终态，卡片锁死
            // 在「已发送按键」，手机端走不到提交）。**不置终态**（submit/cancel 才终态）
            setMqSelected((prev) => ({ ...prev, [questionIndex]: index ?? 0 }));
            setMqIndex(questionIndex + 1);
            // **交互后屏读核对**（2026-10-03 屏读为准）：select 回执带发后快照——
            // TS 行内容回填当前题的 mqFreeText（卡面「已写入」态以屏读为准）
            if (res.screen && typeof res.screen.questionIdx === "number") {
              // **codex 回执直读对位**（2026-10-09 评审 F4.3）：codex 面板形状的
              // questionIdx 是题号对位主键——跳过 findQuestionByHeading（codex
              // 题干区带状态栏杂讯，heading 对位不成立）。unanswered 随回执更新
              //（评审 F6）。
              const dest = Math.min(res.screen.questionIdx, info.questions.length - 1);
              if (typeof res.screen.unanswered === "number") {
                setUnansweredHint(res.screen.unanswered);
              }
              if (landedCodexHeadingMatches(info, dest, res.screen.heading)) {
                // heading 恰好唯一命中 dest（claude/opencode 形状夹带 questionIdx
                // 的防御分支）→ 勾选/TS 行照旧走 writeSnapshotState
                writeSnapshotState(
                  res.screen,
                  dest,
                  info.questions.length === 1,
                  { setChecked, setMqChecked, setMqFreeText, setMqSelected },
                  info.questions[dest]?.multiSelect ?? true
                );
              }
              // 选中态维持本地乐观显示（下方 setMqSelected 已按点击记录，不动）
              setMqIndex(dest);
            } else if (res.screen) {
              // **屏读归属按 heading 对位**（2026-10-06 修复）：快照拍的是发键后
              // **到达页**——推进工具 landed=qi+1；**停留工具**（opencode 2.0.22
              // 单选选中不推进，✓ 标记在原页，2026-10-06 活体定案）landed=qi。
              // 旧实现盲目归属 questionIndex 且无条件 mqIndex+1——到达题的占位态
              // 写进本题（清掉已存文字）、停留被误当推进（卡面漂移到确认卡）。
              // 对位失败（无 heading/多义）→ 维持旧归属（不猜纪律）。
              // **unanswered 随回执更新**（评审 F6）：快照带未答计数 → 直读刷新
              if (typeof res.screen.unanswered === "number") {
                setUnansweredHint(res.screen.unanswered);
              }
              const landed =
                res.screen.heading != null
                  ? findQuestionByHeading(info.questions, res.screen.heading)
                  : null;
              if (landed !== null) {
                writeSnapshotState(
                  res.screen,
                  landed,
                  info.questions.length === 1,
                  { setChecked, setMqChecked, setMqFreeText, setMqSelected },
                  info.questions[landed]?.multiSelect ?? true
                );
                setMqIndex(landed);
              } else {
                applyInteractionScreen(
                  res.screen,
                  questionIndex,
                  { setChecked, setMqChecked, setMqFreeText, setMqSelected },
                  info.questions[questionIndex]?.multiSelect ?? true
                );
              }
            }
          } else if (action === "freeText" && typeof questionIndex === "number") {
            // **多题卡的自由作答**（2026-10-02 多选 Type something 内联编辑）：记录
            // 文本进答案清单、**不置终态**（多题卡要继续切题/提交；打字编排零后续键，
            // 焦点留在终端的 Type something 行，切题由 ◀/▶ 接管）
            // 屏读为准：回执带回该行屏上文本（可能带此前终端侧的残留），以它为准
            // 屏读为准（评审 I1）：回执 text = 该行屏上文本；null = 收尾读屏失败
            //（未核验——不把本地发送文本虚报成已写入，保持可编辑可重试）
            if (res.review === true) {
              // **保存后 TUI 直达 Review**（末题/全部已答自动汇总，2026-10-07）——
              // 直接切确认卡，同步终端真实位置；并触发载荷重拉（Review summary
              // 随新 GET 回来，确认卡摘要以终端为准）
              setReloadTick((t) => t + 1);
              setMqFreeText((prev) => {
                const n = { ...prev };
                delete n[questionIndex];
                return n;
              });
              setFreeText("");
              setEditingFreeText(false);
              setFtUnverified(false);
              setInProgress(null);
              setMqIndex(info?.questions.length ?? questionIndex + 1);
            } else if (overwrite && (text ?? "") === "") {
              // **清空请求**：成功 = 终端 TS 行恢复占位 → 删卡面记录回「发送」初态
              setMqFreeText((prev) => {
                const n = { ...prev };
                delete n[questionIndex];
                return n;
              });
              setFreeText("");
              setEditingFreeText(false);
              setFtUnverified(false);
              setInProgress(null);
            } else if (
              res.advanced === true &&
              typeof questionIndex === "number" &&
              info !== null &&
              questionIndex < info.questions.length &&
              !info.questions[questionIndex].multiSelect
            ) {
              // **单选保存后 TUI 自动推进**（2026-10-07 kimi 定案）——卡面同步推进
              // 到下一题（多选留原页不适用本分支；review=true 已在上分支处理）
              setMqFreeText((prev) => {
                const n = { ...prev };
                delete n[questionIndex];
                return n;
              });
              setFreeText("");
              setEditingFreeText(false);
              setFtUnverified(false);
              setInProgress(null);
              setMqIndex(questionIndex + 1);
            } else if (res.text !== null && res.text !== undefined) {
              setMqFreeText((prev) => ({ ...prev, [questionIndex]: res.text as string }));
              setFreeText("");
              setEditingFreeText(false);
              setFtUnverified(false);
              // **单选题推进**（2026-10-05 用户需求）：单选打字+enter = 选即提交，
              // 终端已推进到下一题/确认页——卡面同步推进（与数字 select 一致）。
              // 多选不推进（需显式切题保存）。
              if (
                info !== null &&
                questionIndex < info.questions.length &&
                !info.questions[questionIndex].multiSelect
              ) {
                setMqIndex(questionIndex + 1);
              }
            } else {
              setFtUnverified(true);
            }
            setInProgress(null);
            // **动作后读 note**（2026-10-10 用户指令「清空了之后不应该读一下屏幕吗」）：
            // 多题卡 freeText 各成功分支（保存/清空/推进）都改变终端 note 行——重拉
            // GET 让 noteText 随新屏回来，按钮面（发送 ↔ 清空/覆盖写入）随之切换
            repullAndSync(epoch);
          } else {
            // **清空请求（单题卡）**：非终态——note 清掉即回「发送」初态（setSent
            // 会把卡标成已发送伪终态，不适用）；重拉 GET 刷新 noteText/按钮面
            const wasClear = action === "freeText" && overwrite === true && (text ?? "") === "";
            if (wasClear) {
              setFreeText("");
              repullAndSync(epoch);
            } else {
              // select（单题）/ submit / 覆盖写入：终态
              setSent(true);
              // 阶段机动作带回 verified（三态）；单键动作无该字段 → 保持 null
              setVerified(typeof res.verified === "boolean" ? res.verified : null);
              // 自由作答成功后清空输入框（已投递；留着会让用户以为没发出去）
              if (action === "freeText") setFreeText("");
              // **动作后重拉 GET**（2026-10-10 用户指令：「页面上有过操作的按键
              // 之后都做一次读屏」）——freeText 提交后 notes 行/摘要态变化，重拉
              // 把终端 noteText 顶到卡面（abort 路径既有同款重拉，评审 I6）
              if (action === "freeText") {
                void fetchSessionQuestion(session.id)
                  .then((v2) => {
                    if (interactionEpoch.current !== epoch) return;
                    setInfo(v2);
                    applyScreenSync(v2);
                  })
                  .catch(() => {});
              }
            }
          }
        } else {
          // failed：区分「阶段机中止」（aborted+stage）与普通投递失败（可重试）
          // **notes 上屏核验失败的静默调和**（2026-10-10 21:23 用户指令）：核验窗
          // 内终端恰好翻题/提交（终端侧合法动作）→ 旧题 note 必然读不到——不是需
          // 要用户处理的错误，红字只制造恐慌；不显示，靠下面的中止重拉把卡面刷到
          // 终端真值（题/notes 同步、输入框随新题重置）
          const silentReconcile =
            action === "freeText" &&
            res.aborted === true &&
            typeof res.error === "string" &&
            (res.error.includes("备注文本") || res.error.includes("备注态意外关闭"));
          if (!silentReconcile) {
            setError(res.error);
          }
          if (res.aborted === true && res.stage) {
            if (!silentReconcile) {
              setAbortedStage(res.stage);
            } else {
              // **2 秒后二次重拉**：核验窗（~1.2s）内没读到的文字可能稍后落地
              // （终端重绘慢）——立即重拉时 noteText 还没回来，再补一拍让它被
              // 读到并同步进输入框
              setTimeout(() => {
                repullAndSync(epoch);
              }, 2000);
            }
            // **中止后重拉**（评审 I6）：键可能在轮询窗尽后才被终端消费——重拉 GET
            // 用屏读快照把卡面拉回与终端一致（快照对位失败则维持现状）。
            // 静默调和同走本重拉（立即一次 + 上面 2s 补拍）；**不设 abortedStage**
            //（评审 Minor：此前无条件重设把静默调和击穿，琥珀横幅照样挂）
            repullAndSync(epoch);
          }
        }
      } catch (e) {
        if (e instanceof ApiError) {
          // 错误码 → 中文文案走**单点映射**（`questionAnswerErrorCopy`）：
          // composer 的问答转向路径用同一个函数，两条入口不会漂移（丁T6 复评抽出）
          setError(questionAnswerErrorCopy(e));
        } else {
          setError(String(e));
        }
      } finally {
        setBusy(false);
        setInProgress(null);
      }
    },
    [busy, sent, session.id, info, mqIndex, applyScreenSync, repullAndSync]
  );

  // 加载中 / 拉取失败 / info 未落地 / 不可用：不渲染（卡自隐）
  if (!ready || info === null || !info.available) return null;
  const questions = info.questions;
  if (questions.length === 0) return null;

  // T3：该工具的问答键序未实测（answerable 明确 false，如 codex——实机 0 样本，
  // 键位仅源码级）→ 只读卡 + 引导终端作答（「未验不出键」；渲染选项供阅读，
  // 但不给可点按钮）。`answerable` 缺省按 true（前向兼容旧后端）
  if (info.answerable === false) {
    const q0 = questions[0];
    return (
      <InteractiveCard
        tone="question"
        testId="question-card"
        mode="tool-readonly"
        pulsing={false}
        title="等待回答"
      >
        {q0.header && (
          <div
            data-testid="question-header"
            className="mt-1.5 inline-block rounded bg-[var(--btnp)]/10 px-1.5 py-0.5 text-[10px] font-medium text-[var(--tx)]"
          >
            {q0.header}
          </div>
        )}
        <p data-testid="question-text" className="mt-1 text-sm text-[var(--tx)]">
          {q0.question}
        </p>
        <ol className="mt-1.5 space-y-0.5">
          {q0.options.map((o, i) => (
            <li
              key={`question-ro-opt-${i}`}
              data-testid={`question-readonly-option-${i}`}
              className="text-xs text-[var(--tx)]"
            >
              <span className="mr-1 font-mono text-[var(--mut)]">{i + 1}.</span>
              {o.label}
              {o.description && <span className="ml-1 text-[var(--mut)]">— {o.description}</span>}
            </li>
          ))}
        </ol>
        <p data-testid="question-tool-readonly-hint" className="mt-1.5 text-xs text-[var(--tx)]">
          该工具的远程作答尚未实测，请在终端完成作答
        </p>
      </InteractiveCard>
    );
  }

  // 多问题（批次戊 E4-E6）：键序已实机定案的三家（multiQuestion 旗标）→ **逐题交互
  // 卡**。推进语义按题型分派（2026-09-23 错位修复的核心）：
  // - **单选题**：点数字=选中即答，终端**自动推进**下一题（kimi DigitAdvance /
  //   opencode `enter confirm` 页 / codex 数字即交）→ 前端 select 成功后同步 +1；
  // - **多选题**：点数字=仅 toggle 勾选，终端**停在原题**（opencode 多选页数字
  //   与切页键是两回事，戊探A ③）→ 前端 toggle 成功后只翻勾选态；「切换题目」钮
  //   （`advance` 旗标，仅 opencode）显式发 tab，成功后前端才切题——两通道永远
  //   同步，不再出现「手机在第 2 题、终端停在第 1 题」的错位；
  // - **确认卡**（mqIndex == questions.length）：提交（submit 阶段机）/ 返回题目
  //   （advance 回绕）/ 取消（esc dismiss）。
  const renderMultiFreeText = (qi: number) => (
    <div className="mt-2" data-testid="question-multi-freetext">
      <label
        htmlFor="question-multi-freetext-input"
        className="mb-1 block text-xs text-[var(--mut)]"
      >
        自由作答（直接输入，切题时自动保存）
      </label>
      <div className="flex gap-1.5">
        <input
          id="question-multi-freetext-input"
          data-testid="question-multi-freetext-input"
          value={freeText || (mqFreeText[qi] ?? "")}
          maxLength={MAX_FREE_TEXT_CHARS}
          disabled={busy}
          readOnly={mqFreeText[qi] !== undefined && !editingFreeText}
          onChange={(e) => setFreeText(e.target.value.slice(0, MAX_FREE_TEXT_CHARS))}
          className="min-w-0 flex-1 rounded-lg border border-[var(--cb)] bg-[var(--cbg)] px-2 py-1.5 text-xs text-[var(--tx)] placeholder:text-[var(--mut)] focus:border-[var(--btnp)] focus:outline-none disabled:opacity-60"
          placeholder="输入内容后点发送（勿在终端按回车）"
        />
        {/* codex 屏驱按钮面（2026-10-10 用户规格）：按钮跟随终端 note 行屏读——
            无字 → 「发送」（overwrite=false，tab→脚注核验→打字→上屏核验→回车）；
            有字 → 「清空」「覆盖写入」（tab 循环清空 ≤2 下每下读屏；覆盖 = 清空 +
            重开 + 打字 + 核验 + 回车）。screen.questionIdx 非空 = codex 面板形状
            （noteText 才有意义）；其他工具走下方既有能力位门控链 */}
        {info.screen?.questionIdx != null ? (
          info.screen?.noteText != null && info.screen.noteText !== "" ? (
            <>
              <button
                type="button"
                data-testid="question-multi-freetext-clear"
                disabled={busy}
                onClick={() => handleAnswer("freeText", undefined, "", qi, undefined, true)}
                className="shrink-0 rounded-full bg-rose-500/10 px-3 py-1.5 text-xs text-rose-700 hover:bg-rose-500/20 disabled:opacity-40 dark:bg-rose-400/10 dark:text-rose-300"
              >
                清空
              </button>
              <button
                type="button"
                data-testid="question-multi-freetext-overwrite"
                disabled={busy || freeText.trim() === ""}
                onClick={() => handleAnswer("freeText", undefined, freeText, qi, undefined, true)}
                className="shrink-0 rounded-full bg-[var(--btnp)] px-3 py-1.5 text-xs font-medium text-[var(--btnpt)] hover:bg-[var(--btnp)] disabled:opacity-40"
              >
                覆盖写入
              </button>
            </>
          ) : (
            <button
              type="button"
              data-testid="question-multi-freetext-send"
              disabled={busy || freeText.trim() === ""}
              onClick={() => handleAnswer("freeText", undefined, freeText, qi)}
              className="rounded-full bg-[var(--btnp)] px-3 py-1.5 text-xs font-medium text-[var(--btnpt)] hover:bg-[var(--btnp)] disabled:opacity-40"
            >
              发送
            </button>
          )
        ) : mqFreeText[qi] !== undefined && info.freeTextOverwrite !== true ? (
          <p className="mt-1.5 rounded-lg bg-slate-500/10 px-2 py-1.5 text-xs text-slate-600 dark:bg-slate-400/10 dark:text-slate-300">
            已写入。该工具的卡内覆盖/清空尚未实机验证——修改请到终端完成
          </p>
        ) : mqFreeText[qi] !== undefined && !editingFreeText ? (
          <button
            type="button"
            data-testid="question-multi-freetext-edit"
            disabled={busy}
            onClick={() => {
              setFreeText(mqFreeText[qi] ?? "");
              setEditingFreeText(true);
            }}
            className="rounded-full bg-[var(--btnp)]/10 px-3 py-1.5 text-xs text-[var(--tx)] hover:bg-[var(--btnp)]/20 disabled:opacity-40"
          >
            编辑
          </button>
        ) : (
          <button
            type="button"
            data-testid="question-multi-freetext-send"
            disabled={busy || freeText.trim() === ""}
            onClick={() =>
              handleAnswer(
                "freeText",
                undefined,
                freeText,
                qi,
                undefined,
                freeText.trim() !== "" || mqFreeText[qi] !== undefined ? true : undefined
              )
            }
            className="rounded-full bg-[var(--btnp)] px-3 py-1.5 text-xs font-medium text-[var(--btnpt)] hover:bg-[var(--btnp)] disabled:opacity-40"
          >
            {freeText.trim() !== "" || mqFreeText[qi] !== undefined ? "覆盖写入" : "发送"}
          </button>
        )}
        {/* 清空（独立动作，不依赖编辑解锁）：**输入格有内容即在场**（2026-10-06 用户
            指令：不等屏读回执——输入了就该能清）+ 已写入态；按能力位门控 */}
        {info.freeTextOverwrite === true &&
          (mqFreeText[qi] !== undefined || freeText.trim() !== "") && (
            <button
              type="button"
              data-testid="question-multi-freetext-clear"
              disabled={busy}
              onClick={() => handleAnswer("freeText", undefined, "", qi, undefined, true)}
              className="shrink-0 rounded-full bg-rose-500/10 px-3 py-1.5 text-xs text-rose-700 hover:bg-rose-500/20 disabled:opacity-40 dark:bg-rose-400/10 dark:text-rose-300"
            >
              清空
            </button>
          )}
      </div>
      {mqFreeText[qi] !== undefined && (
        <p className="mt-1 text-xs text-emerald-600 dark:text-emerald-400">
          已写入：{mqFreeText[qi]}
        </p>
      )}
      {ftUnverified && (
        <p className="mt-1 text-xs text-amber-700 dark:text-amber-400">
          已发送但未能屏读核验——请到终端核对该行内容与勾选态后重试
        </p>
      )}
    </div>
  );
  if (questions.length > 1) {
    if (info.multiQuestion !== true) {
      return (
        <InteractiveCard
          tone="question"
          testId="question-card"
          mode="readonly"
          pulsing={false}
          title={`有 ${questions.length} 个问题等待回答`}
        >
          <ol className="mt-1.5 space-y-1">
            {questions.map((q, i) => (
              <li
                key={`question-readonly-${i}`}
                data-testid={`question-readonly-${i}`}
                className="text-xs text-[var(--tx)]"
              >
                {q.header && (
                  <span className="mr-1 rounded bg-[var(--btnp)]/10 px-1 py-0.5 text-[10px] font-medium text-[var(--tx)]">
                    {q.header}
                  </span>
                )}
                {q.question}
              </li>
            ))}
          </ol>
          <p data-testid="question-readonly-hint" className="mt-1.5 text-xs text-[var(--tx)]">
            请在终端完成作答
          </p>
        </InteractiveCard>
      );
    }
    // 逐题交互：mqIndex = 当前作答的题（0 起）；**mqIndex === questions.length =
    // 确认卡**（2026-09-23 错位修复：全部题目翻完后显式确认——提交/返回/取消）

    const multiFooter = (
      <>
        {error !== null && (
          <p data-testid="question-error" className="mt-1 text-xs text-rose-600 dark:text-rose-400">
            {error}
          </p>
        )}
        {abortedStage !== null && (
          <p
            data-testid="question-aborted"
            className="mt-1 text-xs text-amber-700 dark:text-amber-400"
          >
            卡在阶段：{QUESTION_STAGE_LABELS[abortedStage] ?? abortedStage}
          </p>
        )}
        {sent && (
          <p
            data-testid="question-sent"
            className="mt-1.5 text-xs font-medium text-emerald-600 dark:text-emerald-400"
          >
            已发送按键
          </p>
        )}
      </>
    );
    if (mqIndex >= questions.length) {
      return (
        <InteractiveCard
          tone="question"
          testId="question-card"
          mode="multi-confirm"
          pulsing={false}
          title={`有 ${questions.length} 个问题等待回答（确认提交）`}
          footer={multiFooter}
        >
          <p data-testid="question-confirm-hint" className="mt-1 text-xs text-[var(--tx)]">
            全部题目已翻页完毕，终端应已停在 Confirm（Review）页——提交后模型会收到全部答案。
          </p>
          {/* **已选答案清单**（2026-10-02）：本地记忆渲染（多选=勾选项 / 单选=选中项 /
              自由作答=文本）；终端 Review 页为准——用户在终端改动不会同步到本清单 */}
          <div data-testid="question-confirm-answers" className="mt-2 space-y-1">
            {questions.map((qi, idx) => {
              const checkedSet = mqChecked[idx];
              const selectedIdx = mqSelected[idx];
              const freeTextAns = mqFreeText[idx];
              const parts: string[] = [];
              // **Review 页权威摘要优先**（2026-10-07 权威源切换定案）：summary
              // 命中的题**只显示 summary 答案**（终端 Review 页真值——不再叠加
              // 本地勾选/选中/文字，消除单选显示成多选/双份记录的误区）；
              // 未命中（GET 解析不出）→ 回落本地缓存（兜底，如实可能有偏差）
              const summaryAns = lookupSummaryAnswer(confirmSummary, qi.question);
              if (summaryAns !== undefined) {
                parts.push(summaryAns);
              } else {
                if (checkedSet !== undefined && checkedSet.size > 0) {
                  parts.push(
                    [...checkedSet]
                      .sort((a, b) => a - b)
                      .map((i) => qi.options[i]?.label ?? `#${i + 1}`)
                      .join("、")
                  );
                }
                if (typeof selectedIdx === "number") {
                  parts.push(qi.options[selectedIdx]?.label ?? `#${selectedIdx + 1}`);
                }
                if (freeTextAns !== undefined && freeTextAns !== "") {
                  parts.push(freeTextAns);
                }
              }
              return (
                <div key={idx} className="text-xs">
                  <span className="text-[var(--tx)]">
                    {idx + 1}. {qi.question}
                  </span>
                  <span
                    data-testid={`question-confirm-answer-${idx}`}
                    className={
                      parts.length > 0
                        ? "ml-1 font-medium text-emerald-600 dark:text-emerald-400"
                        : "ml-1 text-[var(--mut)]"
                    }
                  >
                    → {parts.length > 0 ? parts.join("；") : "（未作答）"}
                  </span>
                </div>
              );
            })}
            <p className="text-xs text-[var(--mut)]">以终端 Review 页为准</p>
          </div>
          {!sent && (
            <div className="mt-2 space-y-1.5">
              <button
                type="button"
                data-testid="question-confirm-submit"
                disabled={busy}
                onClick={() => handleAnswer("submit")}
                className="w-full rounded-full bg-[var(--btnp)] px-3 py-1.5 text-xs font-medium text-[var(--btnpt)] hover:bg-[var(--btnp)] disabled:opacity-40"
              >
                提交答案
              </button>
              <button
                type="button"
                data-testid="question-confirm-back"
                disabled={busy}
                onClick={() => handleAnswer("advance", undefined, undefined, undefined, "prev")}
                className="w-full rounded-full bg-[var(--btnp)]/10 px-3 py-1.5 text-xs text-[var(--tx)] hover:bg-[var(--btnp)]/20 disabled:opacity-40"
              >
                ◀ 返回上一题修改
              </button>
              <button
                type="button"
                data-testid="question-confirm-cancel"
                disabled={busy}
                onClick={() => handleAnswer("cancel")}
                className="w-full rounded-full bg-[var(--cb)] px-3 py-1.5 text-xs text-[var(--mut)] hover:bg-[var(--cb)] disabled:opacity-40"
              >
                取消回答
              </button>
            </div>
          )}
        </InteractiveCard>
      );
    }
    const q = questions[mqIndex];
    // 2026-10-02：多选题自由作答升格——claude（navBoth）多选屏的 Type something 行
    // **直接打字即内联编辑**（活体取证：勾选自动置上；回车会取消勾选 → 编排零后续
    // 键）。2026-10-04 起 opencode 以显式能力位 multiFreeText 点亮（own answer
    // toggle 双 enter 语义编排定案）；kimi/codex 多选形态未取证，两旗皆缺 → 不渲染。
    const multiFreeTextEnabled =
      info.freeText === true && (info.navBoth === true || info.multiFreeText === true);
    return (
      <InteractiveCard
        tone="question"
        testId="question-card"
        mode="multi"
        pulsing={false}
        title={`有 ${questions.length} 个问题等待回答（第 ${mqIndex + 1} 题）`}
        footer={multiFooter}
      >
        <p data-testid="question-multi-current" className="mt-1 text-xs text-[var(--tx)]">
          {q.header && (
            <span className="mr-1 rounded bg-[var(--btnp)]/10 px-1 py-0.5 text-[10px] font-medium text-[var(--tx)]">
              {q.header}
            </span>
          )}
          {q.question}
          {q.multiSelect && (
            <span
              data-testid="question-multi-multiselect-badge"
              className="ml-1 rounded bg-[var(--btnp)]/10 px-1 py-0.5 text-[10px] font-medium text-[var(--tx)]"
            >
              多选
            </span>
          )}
        </p>
        {/* codex 未答进度提示（2026-10-09 设计 §3.1）：GET/回执快照的 unanswered
            直读——0/null 不显示（全答完不制造噪音）；非 codex 快照恒 null 不渲染 */}
        {unansweredHint !== null && unansweredHint > 0 && (
          <p
            data-testid="question-unanswered-hint"
            className="mt-1 text-xs text-amber-700 dark:text-amber-400"
          >
            {unansweredHint} 题未答
          </p>
        )}
        <div className="mt-1.5 space-y-1">
          {q.options.map((o, i) => {
            // 多选题的勾选高亮：按题记忆（mqChecked），仅在 toggle 成功回执后变化；
            // **单选题的选中标记**（2026-10-06 活体定案）：终端 ✓ 经快照 checked
            // 通道回写 mqSelected（writeSnapshotState），此处渲染 ✓+高亮
            const checkedHere = q.multiSelect && (mqChecked[mqIndex]?.has(i) ?? false);
            const selectedHere = !q.multiSelect && mqSelected[mqIndex] === i;
            const activeHere = checkedHere || selectedHere;
            return (
              <button
                key={`mq-${mqIndex}-${i}`}
                type="button"
                data-testid={`question-multi-option-${i}`}
                data-checked={activeHere ? "true" : undefined}
                disabled={busy || sent}
                onClick={() =>
                  handleAnswer(q.multiSelect ? "toggle" : "select", i, undefined, mqIndex)
                }
                className={`w-full rounded-lg px-2 py-1.5 text-left text-xs hover:bg-[var(--btnp)]/20 disabled:opacity-40 dark:hover:bg-[var(--btnp)] ${
                  activeHere
                    ? "bg-[var(--btnp)]/25 text-[var(--tx)]"
                    : "bg-[var(--btnp)]/10 text-[var(--tx)]"
                }`}
              >
                <span className="mr-1.5 rounded bg-[var(--btnp)] px-1 py-0.5 font-mono text-[10px] font-semibold text-[var(--btnpt)]">
                  {i + 1}
                </span>
                {q.multiSelect && (
                  <span className="mr-1 font-mono text-[10px]">{checkedHere ? "[✓]" : "[ ]"}</span>
                )}
                {o.label}
                {!q.multiSelect && selectedHere && (
                  <span className="ml-1 font-mono text-[10px] text-emerald-600 dark:text-emerald-400">
                    ✓
                  </span>
                )}
                {o.description !== "" && (
                  <span className="ml-1 text-[10px] text-[var(--mut)]">{o.description}</span>
                )}
              </button>
            );
          })}
        </div>
        {/* **◀/→ 切题导航**（2026-10-02 ←/→ 双向；2026-10-05 opencode 接入）：
            prev/next 都发方向语义——claude=←/→ 方向键；opencode=tab 前向循环
            （next=tab×1；prev=tab×(总页数-1) 前向循环等效回退，全部已验证键）。
            第 1 题隐藏上一题、确认页隐藏下一题。`advance` 旗标未下发的工具
            （kimi/codex 切页键未验）不渲染按钮、改渲染终端引导——不假装能发。 */}
        {!sent && info.advance === true && (
          <div className="mt-2 flex gap-1.5">
            {mqIndex > 0 && (
              <button
                type="button"
                data-testid="question-nav-prev"
                disabled={busy}
                onClick={() => handleAnswer("advance", undefined, undefined, undefined, "prev")}
                className="flex-1 rounded-full bg-[var(--btnp)]/10 px-3 py-1.5 text-xs text-[var(--tx)] hover:bg-[var(--btnp)]/20 disabled:opacity-40"
              >
                ◀ 上一题
              </button>
            )}
            <button
              type="button"
              data-testid="question-multi-advance"
              disabled={busy}
              onClick={() => handleAnswer("advance", undefined, undefined, undefined, "next")}
              className="flex-1 rounded-full bg-[var(--btnp)] px-3 py-1.5 text-xs font-medium text-[var(--btnpt)] hover:bg-[var(--btnp)] disabled:opacity-40"
            >
              下一题 ▶{mqIndex < questions.length - 1 ? "" : "（进入确认页）"}
            </button>
          </div>
        )}
        {/* **多选自由作答**（2026-10-02）：内联编辑编排——打字 → 屏读核验勾选保持 →
            零后续键（回车会取消勾选）。发送后卡面记录文本，切题时终端自然保存。 */}
        {multiFreeTextEnabled && !sent && renderMultiFreeText(mqIndex)}
        {!sent && info.advance !== true && (
          <p
            data-testid="question-multi-advance-unavailable"
            className="mt-2 text-xs text-[var(--mut)]"
          >
            {q.multiSelect ? "多选题勾选后请到终端切换下一题并提交" : "请到终端切换题目"}
          </p>
        )}
      </InteractiveCard>
    );
  }

  const q = questions[0];
  // 自由作答入口的**渲染条件**（丁T5 §2.4；复评 F6-3 收紧）：
  // - `info.freeText === true`（后端按**工具**判：四家定案，2026-10-10 codex 复活）；
  // - **且题目形态是单选**（后端 `free_text_shape_supported`）——多选屏的自由作答行
  //   渲染为 `4. [ ] Type something`（带勾选框，实机截图
  //   `C-s8-cursor-submit-20260921-015844.png` 第 4 行），与「剥编号后以
  //   `Type something` 开头」的定位判据不符 → 后端已 409 拒绝。前端**同步不渲染**
  //   输入框（否则是给用户一个必然失败的按钮），改渲染「请在终端作答」引导。
  //
  // 判据来源说明：前端**不猜**这个限制，而是与后端同源——`multiSelect` 来自同一份
  // questions 载荷；`info.freeText` 由后端按工具给。两处判据合起来 = 后端的
  // `free_text_supported(tool) && free_text_shape_supported(q)`。
  const freeTextEnabled = info.freeText === true && !q.multiSelect;
  // 2026-10-03：单题**多选**卡同样提供自由作答（内联编辑编排与题数无关——活体
  // 取证补齐）；能力位 = navBoth（claude）∨ multiFreeText（2026-10-04 起 opencode
  // own answer toggle 双 enter 编排定案；kimi/codex 多选形态未取证两旗皆缺不渲染）
  const multiFreeTextEnabled =
    info.freeText === true &&
    q.multiSelect &&
    (info.navBoth === true || info.multiFreeText === true);

  return (
    <InteractiveCard
      tone="question"
      testId="question-card"
      mode={q.multiSelect ? "multi" : "single"}
      title={q.multiSelect ? "等待回答（多选）" : "等待回答"}
      titleSuffix={
        q.header ? (
          <span
            data-testid="question-header"
            className={`rounded px-1.5 py-0.5 text-xs font-medium ${toneTokens("question").badge} ${toneTokens("question").title}`}
          >
            {q.header}
          </span>
        ) : null
      }
    >
      <p data-testid="question-text" className="mt-1 text-sm text-[var(--tx)]">
        {q.question}
      </p>
      {/* **进行中态**（丁T5 §2.3）——替代「已发送按键」：提交/自由作答的整条闭环是
          一次同步请求，期间显示「正在做什么」（段名文案），请求返回后本块消失。 */}
      {busy && inProgress !== null && (
        <p
          data-testid="question-progress"
          data-stage={inProgress}
          className="mt-1.5 text-xs font-medium text-[var(--tx)]"
        >
          <span className="mr-1 inline-block h-1.5 w-1.5 animate-pulse rounded-full bg-[var(--btnp)] align-middle" />
          {QUESTION_STAGE_PROGRESS[inProgress]}
        </p>
      )}
      {error !== null && (
        <p data-testid="question-error" className="mt-1 text-xs text-rose-600 dark:text-rose-400">
          {/* 中止（阶段机）时把「卡在哪一段」放在原因之前——用户第一眼要知道停在哪 */}
          {abortedStage !== null && (
            <span data-testid="question-aborted-stage" className="font-medium">
              中止于「{QUESTION_STAGE_LABELS[abortedStage]}」段：
            </span>
          )}
          {error}
        </p>
      )}
      {abortedStage !== null && (
        <p
          data-testid="question-aborted-hint"
          className="mt-1 text-xs text-amber-700 dark:text-amber-400"
        >
          已停止投递后续按键——请到终端查看当前对话框状态后重试
        </p>
      )}
      {sent && (
        <div className="mt-1.5 text-xs font-medium text-emerald-600 dark:text-emerald-400">
          <p data-testid="question-sent">
            {/* 阶段机动作走完整条闭环 → 追加「已走完提交闭环」；单键动作与走完整条的
                都保留「已发送按键」这句主文案（既有用例与用户习惯都认它）。 */}
            已发送按键{verified !== null && "（已走完提交闭环）"}
          </p>
          {/* **终态回执核验**的三态（丁T5）：true 不额外提示；false/未核验要如实说 */}
          {verified === false && (
            <p
              data-testid="question-verified-unseen"
              className="mt-1 text-amber-700 dark:text-amber-400"
            >
              已按屏读完成提交，但未在屏上见到完成回执——请到终端确认结果
            </p>
          )}
        </div>
      )}
      {!sent && (
        <>
          <div className="mt-2 space-y-1.5">
            {q.options.map((o, i) => (
              <button
                key={`question-option-${i}`}
                type="button"
                data-testid={`question-option-${i}`}
                data-checked={q.multiSelect && checked.has(i) ? "true" : undefined}
                disabled={busy}
                onClick={() => handleAnswer(q.multiSelect ? "toggle" : "select", i)}
                className={`flex w-full items-start gap-2 rounded-lg px-2.5 py-1.5 text-left text-sm disabled:opacity-40 ${
                  q.multiSelect && checked.has(i)
                    ? "bg-[var(--btnp)]/15 text-[var(--tx)] ring-1 ring-[var(--btnp)]"
                    : "bg-[var(--btnp)]/5 text-[var(--tx)] hover:bg-[var(--btnp)]/10 dark:hover:bg-[var(--bub)]"
                }`}
              >
                <span className="mt-0.5 inline-flex h-4 w-4 shrink-0 items-center justify-center rounded bg-[var(--btnp)]/15 text-[10px] font-semibold text-[var(--tx)]">
                  {i + 1}
                </span>
                {/* 多选：勾选框字形（与终端 `[ ]`/`[✓]` 同形——2026-09-24「手机端
                    同步终端操作逻辑」；与多题卡的 checkedHere 字形同一形态） */}
                {q.multiSelect && (
                  <span className="mt-0.5 mr-0.5 font-mono text-xs text-[var(--tx)]">
                    {checked.has(i) ? "[✓]" : "[ ]"}
                  </span>
                )}
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
            <button
              type="button"
              data-testid="question-submit"
              disabled={busy || checked.size === 0}
              onClick={() => handleAnswer("submit")}
              className="mt-2 w-full rounded-full bg-[var(--btnp)] px-3 py-1.5 text-sm font-medium text-[var(--btnpt)] disabled:opacity-40"
            >
              提交勾选
            </button>
          )}
          <button
            type="button"
            data-testid="question-cancel"
            disabled={busy}
            onClick={() => handleAnswer("cancel")}
            className="mt-1.5 w-full rounded-full bg-[var(--cb)] px-3 py-1.5 text-sm text-[var(--mut)] disabled:opacity-40"
          >
            取消回答
          </button>
          {/* ===== 丁T5 §2.4：卡内自由作答输入框（入口 1；仅单题卡，本分支恒单题）===== */}
          {info.screen?.noteText != null && info.screen.noteText !== "" && (
            // **终端 notes 行屏读回显**（2026-10-10 用户指令：note 位置但凡有输入，
            // 一定要显示在远端页面上）——只读事实行，与输入框互不干扰
            <p data-testid="question-note-text" className="mt-1.5 text-xs text-[var(--mut)]">
              终端备注（屏读）：<span className="text-[var(--tx)]">{info.screen.noteText}</span>
            </p>
          )}
          {freeTextEnabled ? (
            <div className="mt-2" data-testid="question-freetext">
              <p data-testid="question-freetext-label" className="mb-1 text-xs text-[var(--mut)]">
                或直接输入回答（将作为本题的答案发送到终端）
              </p>
              {(() => {
                // **按钮面由终端 note 行屏读驱动**（2026-10-10 用户规格）：
                // note 无字 → 只有「发送」（tab→脚注核验→打字→上屏核验→回车提交）；
                // note 有字 → 「清空」「覆盖写入」两键（tab 循环清空 ≤2 下、每下读屏
                // 字消失即停；覆盖 = 清空 + 重开 + 打字 + 核验 + 回车提交）。noteText
                // 是 codex 屏读形状字段，claude 等不带 → 恒走「发送」，行为不变。
                const hasNote = info.screen?.noteText != null && info.screen.noteText !== "";
                return (
                  <div className="flex gap-1.5">
                    <input
                      data-testid="question-freetext-input"
                      aria-label="回答内容"
                      type="text"
                      value={freeText}
                      maxLength={MAX_FREE_TEXT_CHARS}
                      disabled={busy}
                      onChange={(e) => setFreeText(e.target.value.slice(0, MAX_FREE_TEXT_CHARS))}
                      placeholder={hasNote ? "终端已有备注——覆盖写入将替换它" : "输入你的回答…"}
                      className="min-w-0 flex-1 rounded-lg border border-[var(--cb)] px-2.5 py-1.5 text-sm text-[var(--tx)] placeholder:text-[var(--mut)] focus:ring-2 focus:ring-[var(--btnp)] focus:outline-none disabled:opacity-50"
                    />
                    {hasNote ? (
                      <>
                        <button
                          type="button"
                          data-testid="question-freetext-clear"
                          disabled={busy}
                          onClick={() =>
                            handleAnswer("freeText", undefined, "", undefined, undefined, true)
                          }
                          className="shrink-0 rounded-full bg-rose-500/10 px-3 py-1.5 text-sm text-rose-700 hover:bg-rose-500/20 disabled:opacity-40 dark:bg-rose-400/10 dark:text-rose-300"
                        >
                          清空
                        </button>
                        <button
                          type="button"
                          data-testid="question-freetext-overwrite"
                          disabled={busy || freeText.trim() === ""}
                          onClick={() =>
                            handleAnswer(
                              "freeText",
                              undefined,
                              freeText,
                              undefined,
                              undefined,
                              true
                            )
                          }
                          className="shrink-0 rounded-full bg-[var(--btnp)] px-3 py-1.5 text-sm font-medium text-[var(--btnpt)] disabled:opacity-40"
                        >
                          覆盖写入
                        </button>
                      </>
                    ) : (
                      <button
                        type="button"
                        data-testid="question-freetext-send"
                        disabled={busy || freeText.trim() === ""}
                        onClick={() => handleAnswer("freeText", undefined, freeText)}
                        className="shrink-0 rounded-full bg-[var(--btnp)] px-3 py-1.5 text-sm font-medium text-[var(--btnpt)] disabled:opacity-40"
                      >
                        作为回答发送
                      </button>
                    )}
                  </div>
                );
              })()}
            </div>
          ) : multiFreeTextEnabled ? (
            renderMultiFreeText(0)
          ) : (
            <p data-testid="question-freeform-hint" className="mt-1.5 text-xs text-[var(--mut)]">
              {/* 降级文案**说清是哪种限制**（两种成因用户动作相同——都去终端——但原因
                  不同，写清楚能少一次困惑）：① 多选题 → 「多选卡」限制（复评 F6-3，
                  题目形态维度）；② 工具未定案 → 「该工具尚未实测」（§2.8，工具维度）。 */}
              {q.multiSelect
                ? "需自由作答？当前工具的多选题自由作答尚未验证，请在终端作答"
                : "需自由作答？该工具的远程自由作答尚未实测，请在终端作答"}
            </p>
          )}
        </>
      )}
    </InteractiveCard>
  );
}
