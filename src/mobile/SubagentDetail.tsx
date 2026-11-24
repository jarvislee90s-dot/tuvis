// 子 Agent 实时预览详情（观察台 §三）：预览区「子 Agent」sheet 的二级内容视图
// （T1 sheet 化起与文件预览同区平级，split/split-h/fullscreen 由父排版）。
// 活跃 = 打开期间 5s 一拍自动刷新逐步输出（关闭即停；hidden 暂停对齐
// SessionDetail 轮询先例）；不活跃 = 定格快照。
// 渲染复用 useMessageRenderers（与会话消息同一套视觉与交互，§三.5）。
// 仅 claude：其余工具 supported=false → 明确「暂不支持查看详情」态。
// 详情内**不重复展示时长/token**（评审 P3-8）：定格快照以清单卡冻结值为准
// （验收口径单点），对话框只承载执行过程正文。
// 页头状态点与 SubagentList 卡片点同款样式（两处 3 行重复，容忍——出现第三处
// 消费再抽 Dot 小组件，评审 P3-9）。
// T1 审查 S5 唯一实例裁决：布局切换器与关闭钮上收 SessionDetail 顶栏 sheet bar——
// 本组件页头仅保留 状态点/名字/back 钮（mode/onModeChange/onClose props 删除）。
import { useEffect, useRef, useState } from "react";
import { ChevronDown, ChevronRight } from "lucide-react";
import i18n from "@/i18n";
import { collapsedLabel, isProcessKind } from "./message-fold";
import { useMessageRenderers } from "./message-render";
import { ApiError, fetchSessionFiles, fetchSubagentMessages, type SessionMessage } from "./api";
import type { Session } from "@/types/session";

/** 自动刷新周期（观察台 §三.3「数秒一拍」）：详情页 10s 轮询的一半——
 *  逐步输出的观感优先；仅对话框打开且 running 时产生请求 */
const SUBAGENT_DETAIL_REFRESH_MS = 5_000;

/** 详情分页：legacy 面（claude 等）与父会话同口径扩窗（200/页 → 上限 1000）；
 *  cursor 面（DSH，2026-11-18 销项批）尾窗恒 200/页，更早内容由 before 游标
 *  逐页补前（后端单页预算仍 clamp 1000——游标页同样受约束） */
const SUBAGENT_PAGE_LIMIT = 200;
const SUBAGENT_MAX_LIMIT = 1000;

interface SubagentDetailProps {
  session: Session;
  subagentId: string;
  /** 显示名（来自名单快照；null = 名单未拉到 → 显示 id） */
  subagentName: string | null;
  /** 运行态（来自名单快照）：true=实时预览（自动刷新），false=定格快照 */
  running: boolean;
  statusUnknown?: boolean;
  /** 返回清单（从清单看板进入时提供 → 页头显示返回按钮；
   *  chip 直达（backToList=false）时缺省不渲染，既有行为不变） */
  onBack?: () => void;
  fontScale?: number;
  /** 正文路径点击 → 文件预览（SessionDetail 传既有 openFile——§三.5「同一套交互」：
   *  详情正文里的已知路径可点开，与消息正文行为一致） */
  openFile: (path: string) => void;
}

/** 任务原文剥壳（§三.2）：首条 user 若被 <teammate-message …> 包裹（乙.3.1
 *  teammate 形态），剥标签显内文；经典形态裸文本原样返回 */
export function extractTaskText(content: string): string {
  const open = content.indexOf(">");
  const close = content.lastIndexOf("</teammate-message>");
  if (content.startsWith("<teammate-message") && open >= 0 && close > open) {
    return content.slice(open + 1, close).trim();
  }
  return content;
}

// ==== 受限续页（2026-11-18 销项批：DSH 子详情 >1000 条的游标分页）====
//
// 两种分页面（按响应里 `olderBefore` 键的存在性判别，与 /session-messages 同法）：
// - legacy（claude 等）：无游标 → 既有「更大 limit 整页重拉」（200 → 1000 封顶）；
// - cursor（DSH）：`olderBefore` = 已展示尾窗最旧消息的**起始索引**（从最旧数
//   起）。尾窗恒 200/页 5s 轮询；「加载更多」按游标向最旧方向逐页补前端拼接
//   （起始索引在同代际追加下稳定——尾窗滚动前进不引起重复/漏读）。

/** 游标页的一条更早消息：abs = 该消息在整份转写里的起始索引（0 基，
 *  由各响应页的 olderBefore 锚定），拼接与去重的唯一键 */
export interface OlderEntry {
  abs: number;
  msg: SessionMessage;
}

/** 游标分页账本（ref 内聚，避免轮询闭包读陈旧 state；更新后经 setMessages
 *  提交合并视图） */
interface PagingState {
  /** legacy | cursor（首帧未知，按首个响应判别后锁定） */
  mode: "legacy" | "cursor";
  /** 尾窗锚：尾窗首条消息的起始索引（legacy 恒 0 且无意义） */
  tailBase: number;
  /** 尾窗消息（skip=0 的最新一页） */
  tailMsgs: SessionMessage[];
  /** 已加载的更早页（按 abs 升序、无重复） */
  older: OlderEntry[];
  /** 下一页 before 游标；null = 无续页（未启航或已到最旧） */
  cursor: number | null;
  /** 已到最旧（显式旗标：耗尽后尾窗锚前移不得重开续页链） */
  exhausted: boolean;
}

/** 游标页合并（纯函数）：按 abs 升序插入并去重（同 abs 保留先到者——
 *  同代际内起始索引唯一，重复只可能来自代际切换窗口的瞬时重叠） */
export function mergeCursorPage(older: OlderEntry[], page: OlderEntry[]): OlderEntry[] {
  const seen = new Set(older.map((e) => e.abs));
  const merged = older.concat(page.filter((e) => !seen.has(e.abs)));
  merged.sort((a, b) => a.abs - b.abs);
  return merged;
}

/** 合并视图（纯函数）：更早页 + 尾窗按文件序拼接，seq 重排为 0 基连续号
 *  （渲染 key / 展开表键；prepend 只发生在点「加载更多」时，轮询不重排） */
export function renderPagedList(paging: {
  mode: "legacy" | "cursor";
  older: OlderEntry[];
  tailMsgs: SessionMessage[];
}): SessionMessage[] {
  if (paging.mode !== "cursor" || paging.older.length === 0) return paging.tailMsgs;
  const merged = paging.older.map((e) => e.msg).concat(paging.tailMsgs);
  return merged.map((m, i) => ({ ...m, seq: i }));
}

export default function SubagentDetail({
  session,
  subagentId,
  subagentName,
  running,
  statusUnknown = false,
  onBack,
  fontScale = 1,
  openFile,
}: SubagentDetailProps) {
  const [messages, setMessages] = useState<SessionMessage[] | null>(null);
  // 截断即后端省略了更早可见消息：legacy 面与父会话同口径逐级扩窗（200 → 1000）；
  // cursor 面（DSH）尾窗恒 200/页，更早内容由游标逐页补前（见下方 pagingRef）。
  const [limit, setLimit] = useState(SUBAGENT_PAGE_LIMIT);
  const [truncated, setTruncated] = useState(false);
  const [error, setError] = useState<{ status: number | null } | null>(null);
  // supported=false：该工具暂不支持查看详情（§三.6）
  const [unsupported, setUnsupported] = useState(false);
  // 游标页加载中（加载更多按钮反馈；轮询尾窗不置位）
  const [olderLoading, setOlderLoading] = useState(false);
  // 渲染侧游标面镜像（react-hooks/refs 禁渲染期读 ref）：cursorMode = 账本已判
  // 别为游标面；cursorAvailable = 游标面且还有未消费游标。账本本体在 pagingRef，
  // 载入回调在事件上下文里同步此镜像
  const [cursorMode, setCursorMode] = useState(false);
  const [cursorAvailable, setCursorAvailable] = useState(false);
  // 错误重试信号（M3 评审修复）：bump 触发主 effect 重跑 = 单发重拉——running=false
  // 仍不轮询，定格语义不变（SessionDetail retry 的 refreshTick 同款手法）
  const [retryTick, setRetryTick] = useState(0);
  // 贴底跟随（P2-B 同款语义）：刷新前采样，贴底才跟随落底
  const areaRef = useRef<HTMLDivElement>(null);
  const followRef = useRef(true);
  // 游标分页账本（ref 内聚——轮询闭包读到的永远是最新账本；提交走 setMessages）
  const pagingRef = useRef<PagingState>({
    mode: "legacy",
    tailBase: 0,
    tailMsgs: [],
    older: [],
    cursor: null,
    exhausted: false,
  });

  useEffect(() => {
    let alive = true;
    const load = () => {
      fetchSubagentMessages(session.agentType, session.id, subagentId, limit, session.dshInstanceId)
        .then((pg) => {
          if (!alive) return;
          setUnsupported(!pg.supported);
          setTruncated(pg.truncated);
          setError(null);
          const p = pagingRef.current;
          if (pg.olderBefore !== undefined) {
            // cursor 面（DSH）：olderBefore 键在场（仅真实存在续页游标时由
            // 后端插入——键缺席的最旧页/非游标面走不到这里……最旧页键缺席，
            // 故 mode 判别只在**首个**带键响应发生；此后 exhausted 旗标防重开）
            if (p.mode !== "cursor") p.mode = "cursor";
            const base = pg.olderBefore ?? 0;
            // 尾窗锚前移吸收：上一轮尾窗里 abs < base 的条目已滚出最新尾窗，
            // 并入 older（轮询只刷新尾窗，已见内容不得凭空消失）
            if (p.tailMsgs.length > 0 && p.tailBase < base) {
              const rolled = p.tailMsgs
                .slice(0, base - p.tailBase)
                .map((m, i) => ({ abs: p.tailBase + i, msg: m }));
              p.older = mergeCursorPage(p.older, rolled);
            }
            p.tailBase = base;
            p.tailMsgs = pg.messages;
            // 续页链启航锚定：仅首帧（older 空且未启航）由尾窗锚给 cursor；
            // 启航后 cursor 由游标页驱动，耗尽后不得因尾窗锚前移而重开
            if (p.cursor === null && !p.exhausted && p.older.length === 0) {
              p.cursor = base;
            }
          } else {
            // legacy 面：整页替换（无游标，扩窗靠 limit）
            p.tailMsgs = pg.messages;
          }
          setCursorMode(p.mode === "cursor");
          setCursorAvailable(p.mode === "cursor" && p.cursor !== null);
          setMessages(renderPagedList(p));
          // 落底不在这里写（I1 评审修复）：React 19 批处理下 promise 回调先于
          // commit，scrollTop 会写进旧 DOM（首开停在顶/轮询钉在旧底）——
          // 改由下方 messages effect 在数据提交后执行
        })
        .catch((e: unknown) => {
          if (alive) setError({ status: e instanceof ApiError ? e.status : null });
        });
    };
    load();
    // 自动刷新：仅 running（§三.3）且页面可见（对齐 SessionDetail F6 hidden 暂停）。
    // visibilitychange 暂停逻辑与 SessionDetail F6 是两份独立实现（评审 P3-11：
    // 规模小，容忍——出现第三消费者再抽 useVisibleInterval hook）
    if (!running)
      return () => {
        alive = false;
      };
    const tick = () => {
      const el = areaRef.current;
      followRef.current = el === null || el.scrollHeight - el.scrollTop - el.clientHeight < 120;
      load();
    };
    let timer: ReturnType<typeof setInterval> | null =
      document.visibilityState !== "hidden" ? setInterval(tick, SUBAGENT_DETAIL_REFRESH_MS) : null;
    const onVisibility = () => {
      if (document.visibilityState === "hidden") {
        if (timer !== null) {
          clearInterval(timer);
          timer = null;
        }
      } else if (timer === null) {
        // 恢复可见：立即补刷一次再续节奏（M1 评审修复——此前只重启 interval，
        // 缺 F6 的「补刷再续拍」，注释声称的对齐并不成立）
        tick();
        timer = setInterval(tick, SUBAGENT_DETAIL_REFRESH_MS);
      }
    };
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      alive = false;
      if (timer !== null) clearInterval(timer);
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [session.agentType, session.id, session.dshInstanceId, subagentId, limit, running, retryTick]);

  // 落底跟随（I1 评审修复，镜像 SessionDetail 既有成熟模式）：数据 commit 后
  // 再写 scrollTop——首拉落底、轮询贴底跟随、定格快照开局即见尾部都在这里生效；
  // pg.messages 每次成功拉取都是新数组，effect 随每次 load 触发一次
  useEffect(() => {
    const el = areaRef.current;
    if (el && followRef.current) el.scrollTop = el.scrollHeight;
  }, [messages]);

  /** 加载更多（cursor 面专属，2026-11-18 销项批）：按游标向最旧方向取一页，
   *  拼前不重拉尾窗（running 子代理的 5s 轮询照常只刷尾窗）。单页失败不抬
   *  整页错误屏（已加载内容不遮蔽）——olderLoading 复位后按钮可再试；游标
   *  未消费即原值重试。legacy 面不走此路径（扩窗由 hasLoadMore → setLimit）。 */
  const loadOlder = () => {
    const p = pagingRef.current;
    if (p.mode !== "cursor" || p.cursor === null || olderLoading) return;
    const cursorAtClick = p.cursor;
    setOlderLoading(true);
    fetchSubagentMessages(
      session.agentType,
      session.id,
      subagentId,
      SUBAGENT_PAGE_LIMIT,
      session.dshInstanceId,
      cursorAtClick
    )
      .then((pg) => {
        const base = pg.olderBefore ?? 0;
        p.older = mergeCursorPage(
          p.older,
          pg.messages.map((m, i) => ({ abs: base + i, msg: m }))
        );
        p.cursor = pg.truncated ? base : null;
        p.exhausted = !pg.truncated;
        setCursorAvailable(p.cursor !== null);
        setMessages(renderPagedList(p));
      })
      .finally(() => setOlderLoading(false));
  };

  // 折叠覆盖表（SessionDetail expandedOverride 同款）：seq → 强制展开/折叠；
  // 缺省走 wire collapsed 语义（thinking/tool-call 默认折叠，点击展开——§三.5）
  const [expandMap, setExpandMap] = useState<Map<number, boolean>>(new Map());
  const isCollapsed = (m: SessionMessage): boolean => expandMap.get(m.seq) ?? m.collapsed;
  const toggle = (m: SessionMessage) => {
    setExpandMap((prev) => {
      const next = new Map(prev);
      next.set(m.seq, !isCollapsed(m));
      return next;
    });
  };

  // 文件链接化数据源：本会话文件表（fetchSessionFiles 既有封装；失败静默 → 空集
  // = 不链接化，消息照常渲染——增强能力不阻塞详情）
  const [files, setFiles] = useState<Set<string>>(new Set());
  useEffect(() => {
    let alive = true;
    fetchSessionFiles(session.agentType, session.id, 200, session.dshInstanceId)
      .then((pg) => {
        if (alive) setFiles(new Set(pg.files.map((f) => f.path)));
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [session.agentType, session.id, session.dshInstanceId]);

  const { renderBody } = useMessageRenderers({ openFile, files });

  const firstUser = messages?.find((m) => m.kind === "user") ?? null;
  // 只信后端标记：cursor 面看账本镜像（是否还有未消费游标——尾窗截断标记在
  // 游标面恒 true，不作判据）；legacy 面看截断标记（条数恰好等于 limit 不代表
  // 还有更早内容，1000 为后端 clamp 上限）。
  const hasLoadMore =
    messages !== null &&
    !error &&
    (cursorAvailable || (!cursorMode && truncated && limit < SUBAGENT_MAX_LIMIT));

  return (
    <section
      data-testid="subagent-detail"
      aria-label={i18n.t("mobile.subagent.detailAria")}
      className="flex h-full min-h-0 flex-col bg-[var(--cbg)]"
    >
      <header className="flex shrink-0 items-center gap-2 border-b border-[var(--cb)] px-3 py-2">
        {/* 返回清单（二级导航）：仅从清单看板进入时出现（chip 直达无） */}
        {onBack && (
          <button
            type="button"
            data-testid="subagent-back"
            aria-label={i18n.t("mobile.subagent.backToList")}
            onClick={onBack}
            className="shrink-0 rounded-full p-1 text-[var(--mut)] hover:bg-[var(--cb)] dark:hover:bg-[var(--btnp)]"
          >
            <ChevronRight size={16} className="rotate-180" />
          </button>
        )}
        <span
          data-testid="subagent-dot"
          className={`inline-block h-2 w-2 shrink-0 rounded-full ${
            running ? "bg-emerald-500" : "bg-gray-400"
          }`}
        />
        <span className="min-w-0 flex-1 truncate text-sm font-medium text-[var(--tx)]">
          {subagentName ?? subagentId}
        </span>
        {running && (
          <span className="shrink-0 text-[10px] text-[var(--mut)]">
            {i18n.t("mobile.subagent.liveTicker")}
          </span>
        )}
        {statusUnknown && (
          <span className="shrink-0 text-[10px] text-[var(--mut)]">
            {i18n.t("mobile.subagent.statusUnknown")}
          </span>
        )}
        {/* S5 唯一实例裁决：布局切换器与关闭钮在顶栏 sheet bar，本页头不再渲染 */}
      </header>

      <div
        ref={areaRef}
        data-testid="subagent-messages"
        data-font-scale={fontScale}
        className="min-h-0 flex-1 overflow-y-auto px-3 py-2"
      >
        {unsupported ? (
          <p
            data-testid="subagent-detail-unsupported"
            className="py-12 text-center text-sm text-[var(--mut)]"
          >
            {i18n.t("mobile.subagent.unsupported")}
          </p>
        ) : error ? (
          <div className="py-12 text-center">
            <p
              data-testid="subagent-detail-error"
              className="mb-3 text-sm text-rose-600 dark:text-rose-400"
            >
              {i18n.t(
                error.status === 404 ? "mobile.subagent.error404" : "mobile.subagent.errorGeneric"
              )}
            </p>
            {/* M3 评审修复：定格快照（running=false）遇到临时失败（网络闪断）此前
                是死局——补一发手动重试（bump retryTick 单发重拉，不引入轮询） */}
            <button
              type="button"
              data-testid="subagent-detail-retry"
              onClick={() => setRetryTick((t) => t + 1)}
              className="rounded-full bg-[var(--cb)] px-4 py-1.5 text-sm text-[var(--tx)]"
            >
              {i18n.t("mobile.subagent.retry")}
            </button>
          </div>
        ) : messages === null ? (
          <p className="py-12 text-center text-sm text-[var(--mut)]">
            {i18n.t("mobile.detail.loading")}
          </p>
        ) : (
          <>
            {hasLoadMore && (
              <div className="mb-2 flex justify-center">
                <button
                  type="button"
                  data-testid="subagent-load-more"
                  onClick={() => {
                    // cursor 面 = 游标向最旧补一页（不重拉尾窗）；legacy 面 =
                    // 既有扩窗重拉（200 → 1000 封顶）
                    if (pagingRef.current.mode === "cursor") loadOlder();
                    else setLimit((l) => Math.min(l + SUBAGENT_PAGE_LIMIT, SUBAGENT_MAX_LIMIT));
                  }}
                  disabled={olderLoading}
                  className="rounded-full bg-[var(--cb)] px-4 py-1.5 text-xs text-[var(--tx)] disabled:opacity-50"
                >
                  {i18n.t("mobile.detail.loadMore")}
                </button>
              </div>
            )}
            {firstUser && (
              <div data-testid="subagent-task" className="mb-2">
                <p className="mb-1 text-[11px] font-medium tracking-wide text-[var(--mut)] uppercase">
                  {i18n.t("mobile.subagent.taskOriginal")}
                </p>
                <div
                  data-testid="subagent-task-text"
                  className="rounded-lg border border-[var(--cb)] bg-[var(--bub)] p-2 text-sm text-[var(--tx)]"
                >
                  {extractTaskText(firstUser.content)}
                </div>
              </div>
            )}
            <ul className="space-y-2 pb-4">
              {messages.map((m) => {
                const toggleable = isProcessKind(m.kind);
                const collapsed = isCollapsed(m);
                return (
                  <li
                    key={m.seq}
                    data-testid={`subagent-msg-${m.seq}`}
                    data-kind={m.kind}
                    className="flex flex-col items-start"
                  >
                    <div className="w-full rounded-2xl border border-[var(--cb)] bg-[var(--cbg)] px-3 py-2">
                      {toggleable ? (
                        <>
                          <button
                            type="button"
                            data-testid={`subagent-msg-${m.seq}-toggle`}
                            aria-expanded={!collapsed}
                            onClick={() => toggle(m)}
                            className="-mx-1 flex w-[calc(100%+8px)] items-center gap-1 rounded-lg px-1 py-0.5 text-left text-xs text-[var(--mut)]"
                          >
                            {collapsed ? <ChevronRight size={13} /> : <ChevronDown size={13} />}
                            <span className="truncate">{collapsedLabel(m)}</span>
                          </button>
                          {!collapsed && <div className="mt-1">{renderBody(m)}</div>}
                        </>
                      ) : (
                        renderBody(m)
                      )}
                    </div>
                  </li>
                );
              })}
            </ul>
          </>
        )}
      </div>
    </section>
  );
}
