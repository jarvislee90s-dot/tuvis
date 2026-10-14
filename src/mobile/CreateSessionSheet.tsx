// 新建会话 UI（Phase C C11）：表单（工具四选 + 目录候选/手填 + 首句）→ 提交 →
// 2s 轮询进度 → done 看板见新卡后复用既有导航跳详情。全屏浮层形态对齐
// FilePreview 全屏态（fixed inset-0 z-50），文案硬编码中文（移动页无 i18n 契约，
// PairPage 同口径）。数据源三方法均为 C10 客户端（api.ts「Phase C C10」段）：
// fetchCreateProjects(7) / createSession（400/409 走返回值，非异常）/
// fetchCreateStatus（404 走类型化 no_task）。样式消费 mobile.css 语义 token
// （--pg/--cbg/--cb/--tx/--mut/--bub/--btnp/--btnpt/--rr/--font-ui）：日/夜与
// 六皮肤/字体/圆角随桌面外观配置器生效；状态语义色（红/黄/绿/sky）按仓内
// 「状态色不随皮肤」口径保留裸 Tailwind 色。
import { useCallback, useEffect, useRef, useState } from "react";
import {
  ApiError,
  createSession,
  fetchCreateProjects,
  fetchCreateStatus,
  type CreateProjectView,
} from "./api";
import { formatRelativeTime, TOOL_LABELS } from "./board-logic";
import NewSessionForm from "./NewSessionForm";
import type { AgentType, Session } from "@/types/session";

/** 新建会话 v1 工具面（计划 §C11）：固定四选；其余工具不在本入口提供 */
const CREATE_TOOLS = ["claude", "codex", "kimi", "opencode"] as const;

/** 最近项目快捷 chips 上限：chips 是加速器非信息面（候选列表才是全量信息面），
 *  后端已按最近活跃降序，只取前 8 个铺顶部横滚行 */
const CREATE_RECENT_CHIPS_MAX = 8;

/** 进度轮询节奏（计划权威：2s） */
const CREATE_POLL_MS = 2000;

/** 停滞提示阈值（评审 P1-8，2026-10-03）：后端管线总预算 90s，前端取 120s
 * （预算 + 轮询/网络裕量）仍未到终态 → 如实提示「可能异常」；**只提示不停拍**——
 * 主机进程死亡（非重启）时收不到后端 failed，靠这条把无限轮询的悬置态说破 */
const CREATE_STALLED_HINT_MS = 120_000;

/** done 后等待看板快照出现新卡的时限：超时不再等，如实提示去看板查看
 *  （不伪造 Session 字段强行跳转——快照没有就不装作有） */
const WAIT_BOARD_MS = 15_000;

/** 任务相 → 中文（六值全覆盖，wire 未知值回落原串展示——不渲染 undefined，
 *  formatTransition 同口径） */
export const CREATE_PHASE_LABELS: Record<string, string> = {
  opening_terminal: "开终端",
  dialog_handling: "处置弹窗",
  injecting_first: "注入首句",
  waiting_materialize: "等待上板",
  done: "完成",
  failed: "失败",
};

/** POST /session-create 400 的 reasonCode → 中文分診文案（后端
 *  create_path::PathReject.code + 端点层三码全集（tool_unavailable /
 *  path_too_long / mkdir_failed），C10 注释登记；码不在表内 → 调用侧通用兜底文案） */
export const CREATE_REJECT_LABELS: Record<string, string> = {
  tool_unavailable: "工具未启用或未安装",
  empty: "路径为空",
  not_absolute: "路径须为绝对路径",
  non_ascii_path: "v1 路径限纯 ASCII",
  not_local_volume: "仅支持本地盘",
  bad_windows_form: "Windows 路径须为 X:\\ 形态",
  blacklisted: "路径命中危险目录黑名单",
  mkdir_failed: "目录创建失败",
  path_too_long: "路径超长（上限 10000 字符）",
};

/** 黄字（表单与回执共用文案，≥1 语义）：选中/创建的工具在该目录已有活跃会话，
 *  多实例下后续消息路由可能混淆——只提示不拦截（与配对不确定信号分层） */
const ACTIVE_HINT_TEXT = "该项目已有该工具的活跃会话，多实例下后续消息路由可能混淆";

/** Windows 手填形态提示判据：盘符前缀但非 `X:\` 形态（如 C:/ 或 C:）。
 *  仅提示不拦截——服务端权威校验（400 bad_windows_form） */
function needsWindowsFormHint(path: string): boolean {
  return /^[a-zA-Z]:(?!\\)/.test(path);
}

/** chips 两态类（镜像 Board 的 chipCls 口径；仓内惯例组件内联同款，不建共享模块） */
const recentChipCls = (on: boolean) =>
  on
    ? "shrink-0 rounded-full bg-[var(--btnp)] px-3 py-1 text-xs font-medium text-[var(--btnpt)]"
    : "shrink-0 rounded-full border border-[var(--cb)] bg-[var(--cbg)] px-3 py-1 text-xs text-[var(--mut)]";

/** chips 展示名 = 路径末段（\ 与 / 双分隔切，兜底原串）；完整路径放 title/aria-label */
function recentChipLabel(path: string): string {
  return path.split(/[\\/]/).filter(Boolean).pop() ?? path;
}

/** 最近项目快捷 chips（纯展示）：置顶加速器行，点一下即选中候选/灌入手填框 */
function RecentProjectChips({
  projects,
  selectedPath,
  manualMode,
  onPick,
}: {
  projects: CreateProjectView[];
  selectedPath: string | null;
  manualMode: boolean;
  onPick: (p: CreateProjectView) => void;
}) {
  return (
    <div
      role="group"
      aria-label="最近项目"
      data-testid="create-recent-chips"
      className="mb-4 flex gap-2 overflow-x-auto pb-1"
    >
      {projects.map((p, i) => {
        // 选中权威在候选选中态：手填态一律不亮（避免与输入框双高亮歧义）
        const on = !manualMode && selectedPath === p.path;
        return (
          <button
            key={p.path}
            type="button"
            data-testid={`create-recent-chip-${i}`}
            title={p.path}
            aria-label={p.path}
            aria-pressed={on}
            onClick={() => onPick(p)}
            className={`${recentChipCls(on)} max-w-[12rem] truncate`}
          >
            {recentChipLabel(p.path)}
          </button>
        );
      })}
    </div>
  );
}

interface CreateSessionSheetProps {
  /** P8d 受管工具名单（Board host 载荷透传）：null = host 未到（不猜全量，
   *  四工具全可点——对齐 Board chips 的竞态口径） */
  enabledTools: Set<string> | null;
  /** 安装探测名单（P1-9，2026-10-03）：null = host 未到（同 enabledTools 竞态
   *  口径不猜）；非 null 时未安装工具置灰并标「未安装」——服务端 400
   *  tool_unavailable 兜底仍在（两道口径同源 tool_installed，不会漂移） */
  installedTools: Set<string> | null;
  /** 看板快照（Board data.sessions）：done 后等新卡上板再跳转的数据源；
   *  Board 的 SSE/轮询驱动它更新，本组件零额外轮询 */
  boardSessions: Session[];
  /** 既有导航复用（App.setSelected）：done + 看板见新卡时以真实 Session 调用，
   *  不新造路由。缺省（Board 测试等旧用法）时只关闭不跳转 */
  onOpenSession?: (session: Session) => void;
  onClose: () => void;
}

export default function CreateSessionSheet({
  enabledTools,
  installedTools,
  boardSessions,
  onOpenSession,
  onClose,
}: CreateSessionSheetProps) {
  // ---- 表单态（重试/取消回表单时全部保留——重填成本高的项不清） ----
  const [tool, setTool] = useState<AgentType | null>(null);
  // null = 候选拉取中；[] + projectsFailed = 拉取失败/403（静默降级手填，不阻塞表单）
  const [projects, setProjects] = useState<CreateProjectView[] | null>(null);
  const [projectsFailed, setProjectsFailed] = useState(false);
  const [selectedPath, setSelectedPath] = useState<string | null>(null);
  const [manualMode, setManualMode] = useState(false);
  const [manualPath, setManualPath] = useState("");
  const [firstMessage, setFirstMessage] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);
  // 中性提示（取消跟踪等），与红色错误分列——用户须能区分「出错」与「如实告知」
  const [notice, setNotice] = useState<string | null>(null);
  // 相对时长基准时钟：面板为瞬态界面，挂载时取一次即可（react-hooks/purity：
  // 禁止渲染期直接调 Date.now，Board 同模式）
  const [now] = useState(() => Date.now());

  // ---- 进度态（taskId 非 null 即进度视图；终态/取消/失效回表单 = 置 null） ----
  const [taskId, setTaskId] = useState<number | null>(null);
  const [receiptYellow, setReceiptYellow] = useState(false); // 回执 hasActiveSession 黄字
  const [phase, setPhase] = useState<string | null>(null);
  const [detail, setDetail] = useState<string | null>(null);
  const [sessionId, setSessionId] = useState<string | null>(null);
  const [noTask, setNoTask] = useState(false);
  // 设备失效（评审修复④）：进度轮询/提交遇 403 = 设备 cookie 已判废——停拍并
  // 如实告知重新配对（此前只依赖 Board 通道判废，面板内无反馈）
  const [deviceInvalid, setDeviceInvalid] = useState(false);
  const [waitHint, setWaitHint] = useState(false);
  // 停滞提示（评审 P1-8）：跟踪满 CREATE_STALLED_HINT_MS 仍无终态时亮起——
  // 后端 90s 预算无前端镜像，主机进程死亡场景收不到 failed，靠此提示说破
  const [stalledHint, setStalledHint] = useState(false);
  // 本轮任务的跟踪起点（Date.now，提交成功时记）——停滞判定基准
  const startedAtRef = useRef<number | null>(null);
  // 跳转一次性闩：防 boardSessions 每拍更新期间重复触发 onOpenSession
  const navigatedRef = useRef(false);

  const effectivePath = manualMode ? manualPath.trim() : selectedPath;
  // 表单黄字数据源：仅候选路径有 activeTools；手填路径无数据 → 不显示（计划权威）
  const activeProject =
    !manualMode && selectedPath !== null
      ? projects?.find((p) => p.path === selectedPath)
      : undefined;
  const formYellow =
    tool !== null && activeProject !== undefined && activeProject.activeTools.includes(tool);
  const canSubmit = tool !== null && !!effectivePath && !submitting;

  // 挂载拉目录候选（days=7 计划权威）。失败口径：403（null）/网络异常/非 2xx
  // 一律静默降级为手填 + 灰字提示，不阻塞表单（fetchCreateProjects 文档约定）
  useEffect(() => {
    let alive = true;
    fetchCreateProjects(7)
      .then((p) => {
        if (!alive) return;
        if (p === null) setProjectsFailed(true); // 403 设备失效：回配对页由 Board 通道处理
        setProjects(p ? p.projects : []);
      })
      .catch(() => {
        if (!alive) return;
        setProjectsFailed(true);
        setProjects([]);
      });
    return () => {
      alive = false;
    };
  }, []);

  /** 快捷标签点击：非手填态 = 直接选中该候选（与候选项点击同口径，黄字/已选
   *  回显天然复用）；手填态 = 只把路径灌入输入框、不切态（以候选为起点可再
   *  微调）。不清 formError/notice——点 chip 是路径操作，不该抹错误反馈 */
  const pickRecent = useCallback(
    (p: CreateProjectView) => {
      if (manualMode) {
        setManualPath(p.path);
      } else {
        setManualMode(false);
        setSelectedPath(p.path);
      }
    },
    [manualMode]
  );

  const handleSubmit = useCallback(async () => {
    if (tool === null || !effectivePath) return;
    // 首句预检（评审修复⑥）：与后端 MAX_SEND_CHARS 同标尺（10k）——后端超长回裸
    // bad_request（无 reasonCode），预检给可读提示且免一次必然失败的往返
    if (firstMessage.trim().length > 10000) {
      setFormError("首句超长（上限 10000 字符），请精简后重试");
      return;
    }
    setSubmitting(true);
    setFormError(null);
    setNotice(null);
    try {
      const r = await createSession({
        tool,
        projectPath: effectivePath,
        // 首句留空 → 省键（JSON.stringify 跳过 undefined），后端缺省探针 hi
        ...(firstMessage.trim() ? { firstMessage: firstMessage.trim() } : {}),
      });
      if ("kind" in r) {
        // 400/409 是类型化返回值（业务流，非异常）——分診文案后留在表单可改可重试
        if (r.kind === "conflict") {
          setFormError("已有创建任务进行中");
        } else {
          // 后端 reason（定位详情，如黑名单命中段+所属表）优先于本地码表——
          // 修复批 I1：定位信息直达用户，缺省再按码表/兜底（评审建议形态）
          setFormError(
            r.reasonCode
              ? (r.reason ??
                  CREATE_REJECT_LABELS[r.reasonCode] ??
                  `参数校验失败（${r.reasonCode}），请修改后重试`)
              : "参数校验失败，请修改后重试"
          );
        }
        return;
      }
      // 200：进入进度态（重置上一轮终态残留）
      setReceiptYellow(r.hasActiveSession);
      setPhase(null);
      setDetail(null);
      setSessionId(null);
      setNoTask(false);
      setWaitHint(false);
      setStalledHint(false);
      setDeviceInvalid(false);
      navigatedRef.current = false;
      startedAtRef.current = Date.now();
      setTaskId(r.taskId);
    } catch (e) {
      if (e instanceof ApiError && e.status === 403) {
        // 评审修复④：设备失效在本面板可辨（cookie 判废）——如实指向重新配对
        setFormError("设备已失效，请重新配对");
      } else {
        // ApiError（5xx/网络）：通用文案（Board 会话通道会统一判废 403 会话态）
        setFormError("创建请求失败（网络或服务异常），请稍后重试");
      }
    } finally {
      setSubmitting(false);
    }
  }, [tool, effectivePath, firstMessage]);

  // 进度轮询（2s，挂入即拍）：终态（done/failed）与失效（no_task）即停；
  // 单拍网络异常不终止——任务在主机侧继续跑，兔维斯 重启会以 404 no_task 判定，
  // 不因一次抖动放弃跟踪（fetchSessionMessages「本层无状态」同纪律）
  useEffect(() => {
    if (taskId === null) return;
    let alive = true;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const poll = async () => {
      let scheduleNext = true;
      try {
        const r = await fetchCreateStatus(taskId);
        if (!alive) return;
        if ("kind" in r) {
          scheduleNext = false;
          setNoTask(true);
        } else {
          setPhase(r.phase);
          setDetail(r.detail);
          setSessionId(r.sessionId);
          if (r.phase === "done" || r.phase === "failed") scheduleNext = false;
        }
      } catch (e) {
        if (e instanceof ApiError && e.status === 403) {
          // 评审修复④：设备失效——停拍并置失效态（继续轮询只会 403 空转）
          scheduleNext = false;
          setDeviceInvalid(true);
        }
        /* 其余单拍失败：下一拍再试（不终止轮询） */
      }
      // 停滞判定（评审 P1-8）：仍在拍且已过 120s 无终态 → 亮提示（不停拍——
      // 后端管线 90s 预算 + 裕量；主机进程死亡收不到 failed 的场景靠此说破）
      if (
        alive &&
        scheduleNext &&
        startedAtRef.current !== null &&
        Date.now() - startedAtRef.current > CREATE_STALLED_HINT_MS
      ) {
        setStalledHint(true);
      }
      if (alive && scheduleNext) timer = setTimeout(() => void poll(), CREATE_POLL_MS);
    };
    void poll();
    return () => {
      alive = false;
      if (timer) clearTimeout(timer);
    };
  }, [taskId]);

  // done + sessionId → 看板快照见新卡才跳：以快照里的真实 Session 对象复用既有
  // 导航（onOpenSession = App.setSelected），不新造路由、不伪造字段。快照未到时
  // 等 Board 的 SSE/轮询把 boardSessions 推新（本组件零额外轮询），至多 WAIT_BOARD_MS
  useEffect(() => {
    if (phase !== "done" || sessionId === null || navigatedRef.current) return;
    const hit = boardSessions.find((s) => s.agentType === tool && s.id === sessionId);
    if (!hit) return;
    navigatedRef.current = true;
    onOpenSession?.(hit);
    onClose();
  }, [phase, sessionId, boardSessions, tool, onOpenSession, onClose]);

  // done 后的上板等待时限：超时置如实提示（不阻断——上板晚到仍走上方 effect 跳转）
  useEffect(() => {
    if (phase !== "done" || sessionId === null) return;
    setWaitHint(false);
    const t = setTimeout(() => setWaitHint(true), WAIT_BOARD_MS);
    return () => clearTimeout(t);
  }, [phase, sessionId]);

  /** 终态/失效/取消 → 回表单（已填项保留）；msg 为中性提示（取消跟踪用） */
  const backToForm = useCallback((msg?: string) => {
    setTaskId(null);
    setPhase(null);
    setDetail(null);
    setSessionId(null);
    setNoTask(false);
    setWaitHint(false);
    setStalledHint(false);
    setDeviceInvalid(false);
    setReceiptYellow(false);
    if (msg !== undefined) {
      setFormError(null);
      setNotice(msg);
    }
  }, []);

  const activeHint = (
    <p
      data-testid="create-active-hint"
      className="mb-3 rounded-[var(--rr)] bg-amber-500/10 px-3 py-2 text-xs text-amber-700 dark:text-amber-400"
    >
      {ACTIVE_HINT_TEXT}
    </p>
  );

  return (
    <div
      data-testid="create-sheet"
      className="fixed inset-0 z-50 flex flex-col bg-[var(--pg)] [font-family:var(--font-ui)] text-[var(--tx)]"
    >
      <header className="flex items-center justify-between border-b border-[var(--cb)] px-4 py-3">
        <h2 className="text-base font-semibold">新建会话</h2>
        <button
          type="button"
          data-testid="create-close"
          aria-label="关闭"
          onClick={onClose}
          className="rounded-full p-1 text-[var(--mut)] hover:bg-[var(--cb)]"
        >
          ✕
        </button>
      </header>
      <div className="flex-1 overflow-y-auto px-4 py-4">
        {taskId === null ? (
          <>
            {/* 最近项目快捷 chips：页面最顶的加速器行（进度态不渲染；候选为空
                整节不渲染——候选区已有加载/空态文案，chips 再占位属重复） */}
            {projects !== null && projects.length > 0 && (
              <RecentProjectChips
                projects={projects.slice(0, CREATE_RECENT_CHIPS_MAX)}
                selectedPath={selectedPath}
                manualMode={manualMode}
                onPick={pickRecent}
              />
            )}
            {notice && (
              <p
                data-testid="create-notice"
                className="mb-3 rounded-[var(--rr)] bg-sky-500/10 px-3 py-2 text-xs text-sky-700 dark:text-sky-400"
              >
                {notice}
              </p>
            )}
            {formError && (
              <p
                data-testid="create-error"
                className="mb-3 rounded-[var(--rr)] bg-red-500/10 px-3 py-2 text-xs text-red-700 dark:text-red-400"
              >
                {formError}
              </p>
            )}

            {/* 工具四选：受管名单之外置灰标「未启用」（host.enabledTools，Board P8d
                同源）；安装探测之外置灰标「未安装」（host.installedTools，P1-9）——
                两名单分列，文案可辨（服务端 400 tool_unavailable 兜底统一覆盖） */}
            <section className="mb-4">
              <h3 className="mb-2 text-sm font-medium">工具</h3>
              <div className="flex flex-wrap gap-2">
                {CREATE_TOOLS.map((t) => {
                  const notEnabled = enabledTools !== null && !enabledTools.has(t);
                  const notInstalled = installedTools !== null && !installedTools.has(t);
                  const disabled = notEnabled || notInstalled;
                  const disabledLabel = notInstalled ? "未安装" : "未启用";
                  const selected = tool === t;
                  return (
                    <button
                      key={t}
                      type="button"
                      data-testid={`create-tool-${t}`}
                      disabled={disabled}
                      aria-pressed={selected}
                      onClick={() => setTool(t)}
                      className={
                        selected
                          ? "rounded-full bg-[var(--btnp)] px-3 py-1 text-xs font-medium text-[var(--btnpt)]"
                          : disabled
                            ? "rounded-full bg-[var(--cb)] px-3 py-1 text-xs text-[var(--mut)] opacity-50"
                            : "rounded-full bg-[var(--cb)] px-3 py-1 text-xs text-[var(--mut)]"
                      }
                    >
                      {TOOL_LABELS[t]}
                      {disabled && <span className="ml-1">{disabledLabel}</span>}
                    </button>
                  );
                })}
                {/* H10（Task 12，D1 汇流）：zcode 无头新建——不走 session-create
                    管线（终端起窗），走 /session-create-zcode 无头管线；不受
                    enabledTools/installedTools 门（无头通道与受管终端工具不同源）。
                    选中 = 表单区整体换为内嵌 NewSessionForm（返回即回到工具选择） */}
                <button
                  type="button"
                  data-testid="create-tool-zcode"
                  aria-pressed={tool === "zcode"}
                  onClick={() => setTool(tool === "zcode" ? null : "zcode")}
                  className={
                    tool === "zcode"
                      ? "rounded-full bg-[var(--btnp)] px-3 py-1 text-xs font-medium text-[var(--btnpt)]"
                      : "rounded-full bg-[var(--cb)] px-3 py-1 text-xs text-[var(--mut)]"
                  }
                >
                  ZCode（无头）
                </button>
              </div>
            </section>

            {/* zcode 无头表单（H10）：内嵌组件接管目录/首句/提交（候选源 =
                recentProjects ∪ 看板快照，与四家 CLI 的 create-projects 管线独立）；
                onBack 回到本 sheet 的工具选择（D1：本 sheet 是唯一新建入口壳） */}
            {tool === "zcode" && (
              <section className="mb-4">
                <NewSessionForm onBack={() => setTool(null)} />
              </section>
            )}

            {tool !== "zcode" && (
              <>
                {/* 目录：候选列表（path + 相对活跃时间 + 工具名）或手填切换。
                  候选是「后端已滤除不可用路径」的短名单（CreateProjectView 契约） */}
                <section className="mb-4">
                  <h3 className="mb-2 text-sm font-medium">目录</h3>
                  {projects === null ? (
                    <p className="text-xs text-[var(--mut)]">目录候选加载中…</p>
                  ) : projects.length > 0 ? (
                    <ul data-testid="create-project-list" className="mb-2 space-y-1">
                      {projects.map((p, i) => (
                        <li key={p.path}>
                          <button
                            type="button"
                            data-testid={`create-project-${i}`}
                            aria-pressed={!manualMode && selectedPath === p.path}
                            onClick={() => {
                              setManualMode(false);
                              setSelectedPath(p.path);
                            }}
                            className={`w-full rounded-[var(--rr)] border px-3 py-2 text-left ${
                              !manualMode && selectedPath === p.path
                                ? "border-[var(--btnp)] bg-[var(--bub)]"
                                : "border-[var(--cb)] bg-[var(--cbg)]"
                            }`}
                          >
                            <span className="block truncate text-sm">{p.path}</span>
                            <span className="block text-xs text-[var(--mut)]">
                              {formatRelativeTime(p.lastActiveAt, now)} ·{" "}
                              {p.tools.map((t) => TOOL_LABELS[t as AgentType] ?? t).join("、")}
                            </span>
                          </button>
                        </li>
                      ))}
                    </ul>
                  ) : (
                    <p className="mb-2 text-xs text-[var(--mut)]">
                      {projectsFailed
                        ? "目录候选不可用，请手动输入路径"
                        : "近 7 天无活跃项目，请手动输入路径"}
                    </p>
                  )}
                  <button
                    type="button"
                    data-testid="create-manual-toggle"
                    aria-pressed={manualMode}
                    onClick={() => setManualMode((v) => !v)}
                    className={`rounded-full px-3 py-1 text-xs ${
                      manualMode
                        ? "bg-[var(--btnp)] text-[var(--btnpt)]"
                        : "bg-[var(--cb)] text-[var(--mut)]"
                    }`}
                  >
                    手动输入路径
                  </button>
                  {manualMode && (
                    <>
                      <input
                        type="text"
                        data-testid="create-manual-input"
                        value={manualPath}
                        onChange={(e) => setManualPath(e.target.value)}
                        placeholder="X:\path\to\project"
                        className="mt-2 w-full rounded-[var(--rr)] border border-[var(--cb)] bg-[var(--cbg)] px-3 py-2 text-sm text-[var(--tx)] placeholder:text-[var(--mut)]"
                      />
                      {needsWindowsFormHint(manualPath) && (
                        <p
                          data-testid="create-win-hint"
                          className="mt-1 text-xs text-amber-600 dark:text-amber-400"
                        >
                          Windows 路径须为 X:\ 形态（以服务端校验为准）
                        </p>
                      )}
                    </>
                  )}
                  {!manualMode && selectedPath !== null && (
                    <p
                      data-testid="create-selected-path"
                      className="mt-1 text-xs text-[var(--mut)]"
                    >
                      已选：{selectedPath}
                    </p>
                  )}
                </section>

                {/* 首句：占位 hi；留空提交 → 请求体省 firstMessage 键（后端缺省探针 hi） */}
                <section className="mb-4">
                  <h3 className="mb-2 text-sm font-medium">首句</h3>
                  <input
                    type="text"
                    data-testid="create-first-message"
                    placeholder="hi"
                    value={firstMessage}
                    onChange={(e) => setFirstMessage(e.target.value)}
                    className="w-full rounded-[var(--rr)] border border-[var(--cb)] bg-[var(--cbg)] px-3 py-2 text-sm text-[var(--tx)] placeholder:text-[var(--mut)]"
                  />
                  <p className="mt-1 text-xs text-[var(--mut)]">留空将发送默认问候「hi」</p>
                </section>

                {formYellow && activeHint}

                <button
                  type="button"
                  data-testid="create-submit"
                  disabled={!canSubmit}
                  onClick={() => void handleSubmit()}
                  className="w-full rounded-[var(--rr)] bg-[var(--btnp)] px-3 py-2 text-sm font-medium text-[var(--btnpt)] disabled:opacity-40"
                >
                  {submitting ? "提交中…" : "开始创建"}
                </button>
              </>
            )}
          </>
        ) : deviceInvalid ? (
          // 评审修复④：进度轮询/提交遇 403 = 设备 cookie 判废——停拍并如实指向
          // 重新配对（与 no_task 的「主机重启」语义分列）
          <>
            <p
              data-testid="create-error"
              className="mb-3 rounded-[var(--rr)] bg-red-500/10 px-3 py-2 text-xs text-red-700 dark:text-red-400"
            >
              设备已失效，请重新配对
            </p>
            <div className="flex gap-2">
              <button
                type="button"
                data-testid="create-retry"
                onClick={() => backToForm()}
                className="flex-1 rounded-[var(--rr)] bg-[var(--btnp)] px-3 py-2 text-sm font-medium text-[var(--btnpt)]"
              >
                返回重试
              </button>
              <button
                type="button"
                onClick={onClose}
                className="flex-1 rounded-[var(--rr)] border border-[var(--cb)] bg-[var(--cbg)] px-3 py-2 text-sm"
              >
                关闭
              </button>
            </div>
          </>
        ) : noTask ? (
          // 404 no_task：兔维斯 重启丢内存任务簿（或 taskId 非法）——如实告知 + 回表单重试
          <>
            <p
              data-testid="create-error"
              className="mb-3 rounded-[var(--rr)] bg-red-500/10 px-3 py-2 text-xs text-red-700 dark:text-red-400"
            >
              任务已失效（主机可能重启），请重试
            </p>
            <div className="flex gap-2">
              <button
                type="button"
                data-testid="create-retry"
                onClick={() => backToForm()}
                className="flex-1 rounded-[var(--rr)] bg-[var(--btnp)] px-3 py-2 text-sm font-medium text-[var(--btnpt)]"
              >
                返回重试
              </button>
              <button
                type="button"
                onClick={onClose}
                className="flex-1 rounded-[var(--rr)] border border-[var(--cb)] bg-[var(--cbg)] px-3 py-2 text-sm"
              >
                关闭
              </button>
            </div>
          </>
        ) : phase === "failed" ? (
          // 失败：后端 detail 即分阶段中文现场（api.rs fail_create_task 逐码产出中文，
          // 终端窗口保留时附「保留供查看现场」减压指引）——展示原文，不二次转译
          <>
            <p
              data-testid="create-phase"
              className="mb-2 text-sm font-medium text-red-600 dark:text-red-400"
            >
              {CREATE_PHASE_LABELS.failed}
            </p>
            <p
              data-testid="create-detail"
              className="mb-4 rounded-[var(--rr)] bg-red-500/10 px-3 py-2 text-xs text-red-700 dark:text-red-400"
            >
              {detail ?? "创建失败，请重试"}
            </p>
            <div className="flex gap-2">
              <button
                type="button"
                data-testid="create-retry"
                onClick={() => backToForm()}
                className="flex-1 rounded-[var(--rr)] bg-[var(--btnp)] px-3 py-2 text-sm font-medium text-[var(--btnpt)]"
              >
                重试
              </button>
              <button
                type="button"
                onClick={onClose}
                className="flex-1 rounded-[var(--rr)] border border-[var(--cb)] bg-[var(--cbg)] px-3 py-2 text-sm"
              >
                关闭
              </button>
            </div>
          </>
        ) : phase === "done" ? (
          // 完成：sessionId 在场 → 等看板见新卡自动跳（导航 effect）；不在场/超时 →
          // 如实提示去看板查看。detail（codex hooks 信任提示）在 done 相也要展示
          <>
            <p
              data-testid="create-phase"
              className="mb-2 text-sm font-medium text-green-600 dark:text-green-400"
            >
              {CREATE_PHASE_LABELS.done}
            </p>
            {detail && (
              <p
                data-testid="create-detail"
                className="mb-3 rounded-[var(--rr)] bg-sky-500/10 px-3 py-2 text-xs text-sky-700 dark:text-sky-400"
              >
                {detail}
              </p>
            )}
            {sessionId === null ? (
              <p className="text-sm text-[var(--mut)]">会话已创建，请到看板查看</p>
            ) : waitHint ? (
              <>
                <p
                  data-testid="create-wait-hint"
                  className="mb-3 text-sm text-amber-600 dark:text-amber-400"
                >
                  会话已创建，但尚未在看板出现——请稍后在看板中查看
                </p>
                <button
                  type="button"
                  onClick={onClose}
                  className="w-full rounded-[var(--rr)] border border-[var(--cb)] bg-[var(--cbg)] px-3 py-2 text-sm"
                >
                  关闭
                </button>
              </>
            ) : (
              <p className="text-sm text-[var(--mut)]">等待会话上板后自动打开…</p>
            )}
          </>
        ) : (
          // 进行中：相名中文映射 + detail（有值即展示）+ 可取消（如实告知后端继续跑）
          <>
            {receiptYellow && activeHint}
            <p data-testid="create-phase" className="mb-2 text-sm font-medium">
              {phase === null ? "正在获取进度…" : (CREATE_PHASE_LABELS[phase] ?? phase)}
            </p>
            {detail && (
              <p data-testid="create-detail" className="mb-3 text-xs text-[var(--mut)]">
                {detail}
              </p>
            )}
            {stalledHint && (
              <p
                data-testid="create-stalled-hint"
                className="mb-3 rounded-[var(--rr)] bg-amber-500/10 px-3 py-2 text-xs text-amber-700 dark:text-amber-400"
              >
                创建耗时已超 2 分钟仍未完成——主机侧可能异常（断连/停摆），可稍后在
                看板确认结果，或返回重试
              </p>
            )}
            <button
              type="button"
              data-testid="create-cancel"
              onClick={() =>
                backToForm("已取消跟踪进度；主机侧创建仍将继续至终态，完成后会话将出现在看板")
              }
              className="w-full rounded-[var(--rr)] border border-[var(--cb)] bg-[var(--cbg)] px-3 py-2 text-sm"
            >
              取消跟踪
            </button>
            <p className="mt-2 text-xs text-[var(--mut)]">
              取消仅停止本页进度跟踪，主机侧创建仍将继续至终态
            </p>
          </>
        )}
      </div>
    </div>
  );
}
