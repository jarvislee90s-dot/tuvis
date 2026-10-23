// 文件面板（M3+ 计划 Task 3）：会话工具调用涉及文件的聚合列表。
// 纯展示（用户裁决 4）——排序/去重/计数全部后端完成（files.rs FileEntry），
// 本组件的筛选只是**视图层筛选**已有结果。
//
// T1 sheet 化（2026-10-09）：本组件是预览区「文件」sheet 的看板——
// - 子 Agent 卡区迁出为独立 sheet 看板（subagents/onOpenSubagent props 删除）；
// - 布局切换器/关闭钮上收 SessionDetail 顶栏 sheet bar（唯一实例裁决，
//   mode/onModeChange/onClose props 删除，布局本身由父组件承担）；
// - 筛选条重排（决策 6）：单行三组下拉（追溯范围 ┃ 文件来源 ┃ 文件类型）+
//   文件名搜索框，组间竖分隔符——kind/origin/scope/search 四状态与叠加过滤
//   逻辑零变化（同一批 useState 换控件形态），「代码文件仅在类型=全部可见」
//   口径保留，搜索纪律（S3：点「搜索」或回车才执行）保留。
import { useEffect, useMemo, useState } from "react";
import { Crosshair, FileText, Image as ImageIcon, X } from "lucide-react";
import i18n from "@/i18n";
import { formatRelativeTime } from "./board-logic";
import { fetchChannel, type ChannelInfo, type SessionFileEntry } from "./api";
import { artifactSourceLabel } from "./ProjectedArtifacts";
import type { ArtifactSource } from "./codexIpcApi";

/** 带宽耗时预估的参考附件大小（§C5，MB）：面板只知路径不知大小，按线稿口径用
 *  20 MB 参考量给出「量级感受」（20MB×8bit / 1.8Mbps ≈ 1.5 分钟 / ÷0.8 ≈ 3.3 分钟） */
const BW_REFERENCE_MB = 20;

/** 带宽受限横幅（§C5：如实告知 + 满速升级指引）。**文件面板与附件区共用同一份
 *  判据与文案**（评审 I-2 后半：附件区也要出横幅——只上传、不浏览文件的用户
 *  不然永远看不到「这是通道限制，不是故障」；抽成一个组件而不是各写一份，是为了
 *  杜绝两处文案漂移，改口径只改这里）。
 *  - 判据：仅受限通道（隧道）就地出现；局域网直连与拉取失败都不渲染；
 *  - 耗时式 = 20MB×8 / 实测速率 / 60（分钟，一位小数）；
 *  - **有限正数守卫**（评审 M-3 / I-2）：畸形载荷（limited=true 但 est=0/NaN/
 *    Infinity）时耗时式会得「Infinity 分钟」这类假数字——速率非有限正数一律
 *    不渲染（无数据不如不提示）；
 *  - testId 由调用方给（panel-bw-note / composer-bw-note），className 供落点间距。 */
export function ChannelBwNote({
  channel,
  testId,
  className = "",
}: {
  channel: ChannelInfo | null;
  testId: string;
  className?: string;
}) {
  if (!channel?.limited) return null;
  // 速率要能算出有意义的耗时：非有限正数（0 / NaN / Infinity）一律不渲染横幅
  const isUsableRate = (mbps: number) => Number.isFinite(mbps) && mbps > 0;
  if (!isUsableRate(channel.estMbpsDown) || !isUsableRate(channel.estMbpsUp)) return null;
  // 参考量按实测速率折算的耗时（分钟，一位小数）
  const minutesFor = (mbps: number) => ((BW_REFERENCE_MB * 8) / mbps / 60).toFixed(1);
  return (
    <div
      data-testid={testId}
      className={`rounded-lg bg-[var(--cbg)] px-2 py-1.5 text-[11px] leading-4 text-[var(--mut)] ${className}`}
    >
      {i18n.t("mobile.file.bwNote", {
        down: channel.estMbpsDown,
        up: channel.estMbpsUp,
        ref: BW_REFERENCE_MB,
        downMinutes: minutesFor(channel.estMbpsDown),
        upMinutes: minutesFor(channel.estMbpsUp),
      })}
    </div>
  );
}

/** 追溯档位（用户裁决 3）：顶档 = SessionDetail.MAX_LIMIT（1000 到顶） */
export const FILE_SCOPES = [200, 500, 1000] as const;

/** 类型过滤档（用户裁决 5）：全部 / 文档 / 图片；代码文件仅在「全部」可见 */
export type FileKindFilter = "all" | "doc" | "image";

/** 来源过滤档（M5 决策 10 / 线稿三池 + 会话产物兜底池）：全部来源 / 我上传的 /
 *  工具读取 / 工具读写 / 会话产物。值与后端 FileEntry.origin（snake_case 序列化）一致 */
export type FileOriginFilter = "all" | "user" | "tool_read" | "tool_write" | "session_artifact";

/** 文档扩展名（用户裁决 5：md/markdown/txt） */
const DOC_EXTS = ["md", "markdown", "txt"];
/** 图片扩展名（用户裁决 5：png/jpg/jpeg/gif/webp/svg/bmp） */
const IMAGE_EXTS = ["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp"];

/** 扩展名 → 类型档（纯函数，小写判定；无扩展名/未知 → other） */
export function fileKindOf(path: string): "doc" | "image" | "other" {
  const ext = path.split(/[\\/]/).pop()?.split(".").pop()?.toLowerCase() ?? "";
  if (DOC_EXTS.includes(ext)) return "doc";
  if (IMAGE_EXTS.includes(ext)) return "image";
  return "other";
}

/** 路径末段文件名（列表主行） */
export function fileBaseName(path: string): string {
  return path.split(/[\\/]/).pop() || path;
}

/** 路径的目录前缀（列表次行；无目录时为空串） */
export function fileDirPrefix(path: string): string {
  const parts = path.split(/[\\/]/);
  parts.pop();
  return parts.join("/");
}

/** 组间竖分隔符（决策 6 单行三组筛选条的分组线；wireframe .vsep） */
function FilterDivider() {
  return (
    <span
      data-testid="file-filter-divider"
      role="separator"
      aria-orientation="vertical"
      className="h-4 w-px shrink-0 bg-[var(--cb)]"
    />
  );
}

/** 下拉组的统一控件样式（三组同款，仅尺寸紧凑） */
const FILTER_SELECT_CLS =
  "rounded-md border border-[var(--cb)] bg-[var(--cbg)] px-1 py-0.5 text-xs text-[var(--tx)]";

/** 文件面板条目形态：后端 `FileEntry` + Task 7 合并视图的可选补充字段。
 *  后端原始条目（无引用证据）可缺席这些字段——渲染退回既有行为。 */
export interface FilePanelEntry extends SessionFileEntry {
  /** 最近一次结构化引用的来源（只作行为标签：生成/修改/查看/上传）。 */
  artifactSource?: ArtifactSource | null;
  /** 原始状态标量（不翻译、不写成成功/失败）。 */
  sourceStatus?: string | null;
  /** 原始变更类型标量（不猜 add/modify/delete）。 */
  changeKind?: string | null;
  /** 来源摘要（同回合提示词优先，≤24 字；无引用证据的后端行为 null）。 */
  summary?: string | null;
}

interface FilePanelProps {
  /** 已按 lastSeq 降序（后端契约，本组件不再排序；Task 7 合并视图已按最近引用排好） */
  entries: FilePanelEntry[];
  /** 该档位下还有更早文件未纳入（档位提示依据） */
  truncated: boolean;
  scope: number;
  loading: boolean;
  onScopeChange: (n: number) => void;
  /** 主行文件名点击 → 进入单文件预览 */
  onOpenFile: (path: string) => void;
  /** 来源按钮（Task 7）：在正文中定位该文件的最近引用（与预览页头同一查找流程）。
   *  缺省不渲染定位按钮——其他工具的文件池行为不变。 */
  onLocate?: (entry: FilePanelEntry) => void;
  /** 字号档位（2026-09-16 用户裁决）：作用于列表内容区（筛选条不受影响） */
  fontScale?: number;
}

export default function FilePanel({
  entries,
  truncated,
  scope,
  loading,
  onScopeChange,
  onOpenFile,
  onLocate,
  fontScale = 1,
}: FilePanelProps) {
  const [kind, setKind] = useState<FileKindFilter>("all");
  // 来源筛选（M5 B3）：默认全部来源；undefined origin 的旧载荷条目只在「全部」可见
  const [origin, setOrigin] = useState<FileOriginFilter>("all");
  // 文件名搜索（M5 决策 10）：**点「搜索」（或回车）才执行**——输入框只是草稿态，
  // activeSearch 才参与过滤。两态分离是裁决的执行点，勿合并成受控即时过滤
  const [searchDraft, setSearchDraft] = useState("");
  const [activeSearch, setActiveSearch] = useState("");
  // 目录路径浮窗（2026-09-16 用户裁决）：次行目录被截断时点击查看全路径
  // （手机与电脑逻辑一致——同一组件两板共用）
  const [pathPopover, setPathPopover] = useState<string | null>(null);
  // 相对时间的基准时钟（Board 同款模式：渲染期不得调 Date.now——react-hooks/purity）
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), 30_000);
    return () => clearInterval(id);
  }, []);

  // 通道能力（Task 10 §C5，装饰）：挂载拉一次，任何失败静默 → 无提示不阻塞面板。
  // **纯装饰**——读 Host 推断、可被伪造、允许不准、不进任何安全判定（G1 推论③）
  const [channel, setChannel] = useState<ChannelInfo | null>(null);
  useEffect(() => {
    let alive = true;
    void fetchChannel().then((c) => {
      if (alive) setChannel(c);
    });
    return () => {
      alive = false;
    };
  }, []);

  /** 执行搜索：草稿 trim 后生效（空串 = 清除搜索） */
  const runSearch = () => setActiveSearch(searchDraft.trim());

  // 视图层三层叠加过滤（决策 10：与追溯范围、类型、来源叠加；不再排序：
  // 顺序即后端 lastSeq 降序，用户裁决 1）
  const visible = useMemo(() => {
    let list = kind === "all" ? entries : entries.filter((e) => fileKindOf(e.path) === kind);
    if (origin !== "all") {
      list = list.filter((e) => e.origin === origin);
    }
    if (activeSearch) {
      const q = activeSearch.toLowerCase();
      list = list.filter((e) => fileBaseName(e.path).toLowerCase().includes(q));
    }
    return list;
  }, [entries, kind, origin, activeSearch]);

  // 到顶判定（用户裁决 3）：1000 = MAX_LIMIT，到顶后不再提示"还有更早文件"
  const atTop = scope >= FILE_SCOPES[FILE_SCOPES.length - 1];
  // 类型下拉档（原 chips 收编为「文件类型」下拉，决策 6；文案 M5 双语键现取）
  const kindOptions: Array<{ key: FileKindFilter; label: string }> = [
    { key: "all", label: i18n.t("mobile.file.kindAll") },
    { key: "doc", label: i18n.t("mobile.file.kindDoc") },
    { key: "image", label: i18n.t("mobile.file.kindImage") },
  ];
  // 来源下拉档（原 origin chips 收编为「文件来源」下拉；键 = 后端 origin 值域）
  const originOptions: Array<{ key: FileOriginFilter; label: string }> = [
    { key: "all", label: i18n.t("mobile.file.originAll") },
    { key: "user", label: i18n.t("mobile.file.originUser") },
    { key: "tool_read", label: i18n.t("mobile.file.originToolRead") },
    { key: "tool_write", label: i18n.t("mobile.file.originToolWrite") },
    { key: "session_artifact", label: i18n.t("mobile.file.originArtifact") },
  ];
  // 行内来源徽标文案（undefined = 旧载荷，不渲染）
  const originLabel: Record<string, string> = {
    user: i18n.t("mobile.file.originUser"),
    tool_read: i18n.t("mobile.file.originToolRead"),
    tool_write: i18n.t("mobile.file.originToolWrite"),
    session_artifact: i18n.t("mobile.file.originArtifact"),
  };

  return (
    <section
      data-testid="file-panel"
      aria-label={i18n.t("mobile.file.panelAria")}
      className="flex h-full min-h-0 flex-col bg-[var(--cbg)]"
    >
      {/* 筛选条（决策 6 单行三组）：追溯范围 ┃ 文件来源 ┃ 文件类型 ┃ 文件名搜索。
          四状态与叠加过滤逻辑零变化；「代码文件仅在类型=全部可见」口径保留 */}
      <div
        data-testid="file-filter-bar"
        className="flex shrink-0 flex-wrap items-center gap-x-2 gap-y-1 border-b border-[var(--cb)] px-3 py-2 text-xs"
      >
        <label className="flex items-center gap-1 text-[var(--mut)]">
          <span data-testid="file-scope-hint" title={i18n.t("mobile.file.scopeHint")}>
            {i18n.t("mobile.file.scopeLabel")}
          </span>
          <select
            data-testid="file-scope-select"
            aria-label={i18n.t("mobile.file.scopeAria")}
            value={String(scope)}
            onChange={(e) => onScopeChange(Number(e.target.value))}
            className={FILTER_SELECT_CLS}
          >
            {FILE_SCOPES.map((s) => (
              <option key={s} value={String(s)}>
                {s}
              </option>
            ))}
          </select>
        </label>
        <FilterDivider />
        <label className="flex items-center gap-1 text-[var(--mut)]">
          <span>{i18n.t("mobile.file.originLabel")}</span>
          <select
            data-testid="file-origin-select"
            aria-label={i18n.t("mobile.file.originLabel")}
            value={origin}
            onChange={(e) => setOrigin(e.target.value as FileOriginFilter)}
            className={FILTER_SELECT_CLS}
          >
            {originOptions.map((o) => (
              <option key={o.key} value={o.key}>
                {o.label}
              </option>
            ))}
          </select>
        </label>
        <FilterDivider />
        <label className="flex items-center gap-1 text-[var(--mut)]">
          <span>{i18n.t("mobile.file.kindLabel")}</span>
          <select
            data-testid="file-kind-select"
            aria-label={i18n.t("mobile.file.kindLabel")}
            value={kind}
            onChange={(e) => setKind(e.target.value as FileKindFilter)}
            className={FILTER_SELECT_CLS}
          >
            {kindOptions.map((o) => (
              <option key={o.key} value={o.key}>
                {o.label}
              </option>
            ))}
          </select>
        </label>
        <FilterDivider />
        {/* 文件名搜索（M5 决策 10）：点「搜索」（或回车）才执行——两态分离纪律不变 */}
        <form
          role="search"
          aria-label={i18n.t("mobile.file.searchAria")}
          className="ml-auto flex items-center"
          onSubmit={(e) => {
            e.preventDefault();
            runSearch();
          }}
        >
          <input
            value={searchDraft}
            onChange={(e) => setSearchDraft(e.target.value)}
            placeholder={i18n.t("mobile.file.searchPlaceholder")}
            data-testid="file-search-input"
            aria-label={i18n.t("mobile.file.searchPlaceholder")}
            className="w-28 rounded-l-lg border border-r-0 border-[var(--cb)] bg-[var(--cbg)] px-2 py-1 text-xs outline-none focus:border-[var(--cb)]"
          />
          <button
            type="submit"
            data-testid="file-search-run"
            className="rounded-r-lg border border-[var(--cb)] bg-[var(--cbg)] px-2 py-1 text-xs text-[var(--mut)] hover:bg-[var(--cb)]"
          >
            {i18n.t("mobile.file.search")}
          </button>
        </form>
        {loading && (
          <span className="text-xs text-[var(--mut)]">{i18n.t("mobile.detail.loading")}</span>
        )}
      </div>

      <div
        data-testid="panel-content"
        data-font-scale={fontScale}
        className="min-h-0 flex-1 overflow-y-auto px-3 py-2"
      >
        {visible.length === 0 && !loading && (
          <p data-testid="panel-empty" className="py-12 text-center text-sm text-[var(--mut)]">
            {i18n.t(
              activeSearch || origin !== "all" ? "mobile.file.noMatch" : "mobile.file.emptyInRange"
            )}
          </p>
        )}
        <ul className={`space-y-1 ${loading && entries.length > 0 ? "opacity-60" : ""}`}>
          {visible.map((e, i) => (
            <li
              key={`${e.path}-${i}`}
              data-testid={`file-row-${i}`}
              data-change-kind={e.changeKind ?? undefined}
              className="rounded-lg px-2 py-1.5 hover:bg-[var(--cbg)] dark:hover:bg-[var(--btnp)]"
            >
              {/* 主行 = 末段文件名（打开预览）＋ 来源按钮（在正文中定位最近引用）。
                  **两个热区互不触发**：文件名只走 onOpenFile，来源按钮只走 onLocate；
                  未知来源（无 origin / 无结构化引用）也保留定位入口。 */}
              <div className="flex items-center gap-2">
                <button
                  type="button"
                  data-testid={`file-row-${i}-open`}
                  onClick={() => onOpenFile(e.path)}
                  className={`block min-w-0 flex-1 truncate text-left text-sm hover:underline ${
                    e.modified ? "font-semibold text-[var(--tx)]" : "font-medium text-[var(--mut)]"
                  }`}
                >
                  {fileKindOf(e.path) === "image" ? (
                    <ImageIcon size={12} className="mr-1 inline shrink-0" />
                  ) : (
                    <FileText size={12} className="mr-1 inline shrink-0" />
                  )}
                  {fileBaseName(e.path)}
                </button>
                {onLocate && (
                  <button
                    type="button"
                    data-testid={`file-row-${i}-locate`}
                    aria-label={i18n.t("mobile.artifacts.locate.aria", {
                      name: fileBaseName(e.path),
                    })}
                    title={i18n.t("mobile.artifacts.locate.action")}
                    onClick={() => onLocate(e)}
                    className="flex shrink-0 items-center gap-0.5 rounded-full border border-[var(--cb)] px-1.5 py-px text-[10px] text-[var(--mut)] hover:bg-[var(--cb)] hover:text-[var(--tx)] dark:hover:bg-[var(--btnp)]"
                  >
                    <Crosshair size={10} className="shrink-0" />
                    {e.artifactSource
                      ? artifactSourceLabel(e.artifactSource)
                      : i18n.t("mobile.artifacts.locate.short")}
                  </button>
                )}
              </div>
              {/* 来源摘要（Task 7 R5）：仅结构化引用有证据时出现；24 字已在合并视图
                  截断，title 带完整摘要供长按/悬停查看。无摘要的行布局不变。 */}
              {typeof e.summary === "string" && e.summary !== "" && (
                <p
                  data-testid={`file-row-${i}-summary`}
                  title={e.summary}
                  className="mt-0.5 truncate pl-5 text-xs text-[var(--tx)]"
                >
                  {e.summary}
                </p>
              )}
              <div className="flex items-center gap-2 text-xs text-[var(--mut)]">
                {/* 来源徽标（M5 线稿 .from pill）：undefined（旧载荷）不渲染 */}
                {e.origin && (
                  <span
                    data-testid={`file-row-${i}-origin`}
                    className="flex-none rounded-full border border-[var(--cb)] px-1.5 py-px text-[10px] text-[var(--mut)]"
                  >
                    {originLabel[e.origin] ?? e.origin}
                  </span>
                )}
                {/* 原始状态标量（Task 7）：只原样展示，绝不改写成“已写入/成功” */}
                {typeof e.sourceStatus === "string" && e.sourceStatus !== "" && (
                  <span
                    data-testid={`file-row-${i}-status`}
                    data-status={e.sourceStatus}
                    className="flex-none rounded-full bg-[var(--cb)] px-1.5 py-px font-mono text-[10px] text-[var(--mut)]"
                  >
                    {e.sourceStatus}
                  </span>
                )}
                {fileDirPrefix(e.path) ? (
                  <button
                    type="button"
                    data-testid={`file-row-${i}-dir`}
                    aria-label={i18n.t("mobile.file.viewFullPath")}
                    title={i18n.t("mobile.file.viewFullPathTitle")}
                    onClick={() =>
                      setPathPopover((p) =>
                        p === fileDirPrefix(e.path) ? null : fileDirPrefix(e.path)
                      )
                    }
                    className="min-w-0 flex-1 truncate text-left hover:text-[var(--tx)] hover:underline dark:hover:text-[var(--mut)]"
                  >
                    {fileDirPrefix(e.path)}
                  </button>
                ) : (
                  <span className="min-w-0 flex-1" />
                )}
                {/* 相对时间（lastTs null → 占位符） */}
                <span className="shrink-0">
                  {e.lastTs === null
                    ? "—"
                    : formatRelativeTime(new Date(e.lastTs).toISOString(), now)}
                </span>
              </div>
            </li>
          ))}
        </ul>
        {/* 路径浮窗（2026-09-16 用户裁决）：完整目录路径可读可复制（长按/选中） */}
        {pathPopover !== null && (
          <div
            data-testid="path-popover"
            role="dialog"
            aria-label={i18n.t("mobile.file.fullPathAria")}
            className="sticky bottom-0 mt-2 flex items-start gap-2 rounded-lg border border-[var(--cb)] bg-[var(--cbg)] p-2 shadow-lg"
          >
            <code className="min-w-0 flex-1 text-xs break-all text-[var(--tx)]">{pathPopover}</code>
            <button
              type="button"
              data-testid="path-popover-close"
              aria-label={i18n.t("mobile.file.closePathPopover")}
              onClick={() => setPathPopover(null)}
              className="shrink-0 rounded-full p-0.5 text-[var(--mut)] hover:bg-[var(--cb)] dark:hover:bg-[var(--btnp)]"
            >
              <X size={14} />
            </button>
          </div>
        )}
        {/* 档位提示：还有更早文件且未到顶（用户裁决 3） */}
        {truncated && !atTop && (
          <p
            data-testid="panel-truncated-hint"
            className="mt-2 rounded-lg bg-amber-500/10 px-3 py-2 text-xs text-amber-700 dark:text-amber-400"
          >
            {i18n.t("mobile.file.truncatedHint")}
          </p>
        )}
        {/* 可预览说明（M5 P2-a：类型与上限前置告知，配合 403 原因细分排障） */}
        <p
          data-testid="panel-preview-help"
          className="mt-3 border-t border-[var(--cb)] pt-2 text-[10px] leading-relaxed text-[var(--mut)]"
        >
          {i18n.t("mobile.file.previewHelp")}
        </p>
        {/* 带宽受限提示 + 满速升级指引（Task 10 §C5）：仅受限通道（隧道）就地出现，
            局域网直连与拉取失败都不渲染——升级指引不占通道卡片位（§C4 4 卡不变），
            只在用户真会遇到大文件传输时就地告知。判据/文案/有限正数守卫见
            ChannelBwNote（附件区复用同一份，评审 I-2） */}
        <ChannelBwNote channel={channel} testId="panel-bw-note" className="mt-3" />
      </div>
    </section>
  );
}
