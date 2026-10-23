// 消息窗口书签条（M3+，2026-09-16 用户裁决）：消息区上方常驻横条。
//
// 功能区布局（2026-09-20 用户裁决重排）：新增/管理/清空三个功能钮**聚集在左**，
// 色点区随其后，行最右为过程折叠开关（可选）——原布局管理钮在最右端、
// 与「＋书签」相距过远，功能区块被拆散。
// - 左：「+ 书签」→ 弹 10 色调色板，已占用色置灰不可点；选色即打标签
//   （落点 = 当前视口顶部消息，由 SessionDetail 取锚后回填）；
//   其右：「管理」↔「完成」切换；管理态额外出现「清空全部」；
// - 中：一排色点——常态点即跳转；管理态变成 × 删除钮；
// - 右：过程折叠开关（可选，2026-09-20）：一键收起/展开思考与工具调用过程。
//
// 纯展示组件（数据与 store 操作全在 SessionDetail），故无内部书签状态；
// 唯一内部状态是「调色板是否展开」「是否管理态」。
import { useState } from "react";
import { FoldVertical, Plus, Settings2, UnfoldVertical, X } from "lucide-react";
import i18n from "@/i18n";
import { BOOKMARK_COLORS, type Bookmark } from "./bookmarks";

interface BookmarkBarProps {
  bookmarks: Bookmark[];
  /** 已达上限：+ 按钮 disabled（上限由 store 保证，此处只做展示） */
  atLimit: boolean;
  /** 选色打标签（color 为调色板色值） */
  onAdd: (color: string) => void;
  /** 点色点跳转（anchor 为内容指纹） */
  onJump: (anchor: string) => void;
  /** 管理态删除单条 */
  onRemove: (color: string) => void;
  /** 清空该会话全部书签 */
  onClear: () => void;
  /** 过程折叠开关（2026-09-20）：缺省不渲染。allCollapsed=true 表示当前全部
   *  处于折叠态（此时按钮动作=全部展开）；由 SessionDetail 依据
   *  collapsedCount/toggleableMessages 计算——运行态与总结态都可用 */
  processToggle?: { allCollapsed: boolean; onToggle: () => void };
}

export default function BookmarkBar({
  bookmarks,
  atLimit,
  onAdd,
  onJump,
  onRemove,
  onClear,
  processToggle,
}: BookmarkBarProps) {
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [managing, setManaging] = useState(false);
  const usedColors = new Set(bookmarks.map((b) => b.color));

  return (
    <div className="relative shrink-0 border-b border-[var(--cb)] px-3 py-1.5">
      <div className="flex items-center gap-2">
        {/* 打标签入口 */}
        <button
          type="button"
          data-testid="bookmark-add"
          aria-label={i18n.t("mobile.bookmark.addAria")}
          title={i18n.t(atLimit ? "mobile.bookmark.limitTitle" : "mobile.bookmark.addTitle")}
          aria-expanded={paletteOpen}
          disabled={atLimit}
          onClick={() => setPaletteOpen((v) => !v)}
          className={`flex shrink-0 items-center gap-0.5 rounded-full px-2 py-0.5 text-xs ${
            atLimit
              ? "cursor-not-allowed text-[var(--mut)]"
              : "text-[var(--mut)] hover:bg-[var(--cb)] dark:hover:bg-[var(--btnp)]"
          }`}
        >
          <Plus size={13} />
          {i18n.t("mobile.bookmark.add")}
        </button>

        {/* 管理 / 完成（2026-09-20 用户裁决：紧挨「书签」文字右侧，
            与新增/清空聚成功能区块——原布局被色点区隔到最右端） */}
        <button
          type="button"
          data-testid={managing ? "bookmark-manage-done" : "bookmark-manage"}
          aria-label={i18n.t(managing ? "mobile.bookmark.doneAria" : "mobile.bookmark.manageAria")}
          aria-pressed={managing}
          onClick={() => setManaging((v) => !v)}
          className={`shrink-0 rounded-full p-1 text-xs ${
            managing
              ? "bg-[var(--cb)] text-[var(--tx)]"
              : "text-[var(--mut)] hover:bg-[var(--cb)] dark:hover:bg-[var(--btnp)]"
          }`}
        >
          {managing ? i18n.t("mobile.bookmark.done") : <Settings2 size={14} />}
        </button>

        {/* 清空全部（仅管理态 + 有书签） */}
        {managing && bookmarks.length > 0 && (
          <button
            type="button"
            data-testid="bookmark-clear-all"
            aria-label={i18n.t("mobile.bookmark.clearAllAria")}
            onClick={onClear}
            className="shrink-0 rounded-full bg-rose-500/15 px-2 py-0.5 text-xs text-rose-700 dark:text-rose-400"
          >
            {i18n.t("mobile.bookmark.clearAll")}
          </button>
        )}

        {/* 色点区：常态点即跳转；管理态变大一号的 × 删除钮（同一按钮两种态） */}
        <span className="flex min-w-0 flex-1 flex-wrap items-center gap-1.5">
          {bookmarks.map((b) => (
            <button
              key={b.color}
              type="button"
              data-testid={`bookmark-${managing ? "remove" : "dot"}-${b.color}`}
              aria-label={i18n.t(
                managing ? "mobile.bookmark.removeAria" : "mobile.bookmark.jumpAria",
                { preview: b.preview }
              )}
              title={
                managing ? i18n.t("mobile.bookmark.removeTitle", { preview: b.preview }) : b.preview
              }
              onClick={() => (managing ? onRemove(b.color) : onJump(b.anchor))}
              style={{ backgroundColor: b.color }}
              className={
                managing
                  ? "flex h-5 w-5 shrink-0 items-center justify-center rounded-full text-white"
                  : "h-3.5 w-3.5 shrink-0 rounded-full transition-transform hover:scale-125"
              }
            >
              {managing && <X size={12} />}
            </button>
          ))}
        </span>

        {/* 过程折叠开关（2026-09-20）：行最右；一键收起/展开思考与工具调用。
            带文字按钮（2026-09-20 用户反馈：纯图标功能不明显）——文字跟随状态：
            展开态显示「过程折叠」（点击收起），全折叠态显示「过程展开」（点击展开） */}
        {processToggle && (
          <button
            type="button"
            data-testid="process-collapse-toggle"
            aria-label={i18n.t(
              processToggle.allCollapsed
                ? "mobile.bookmark.processExpandAria"
                : "mobile.bookmark.processCollapseAria"
            )}
            aria-pressed={!processToggle.allCollapsed}
            title={
              processToggle.allCollapsed
                ? i18n.t("mobile.bookmark.processExpandTitle")
                : i18n.t("mobile.bookmark.processCollapseTitle")
            }
            onClick={processToggle.onToggle}
            className={`flex shrink-0 items-center gap-0.5 rounded-full px-2 py-0.5 text-xs ${
              processToggle.allCollapsed
                ? "text-[var(--mut)] hover:bg-[var(--cb)] dark:hover:bg-[var(--btnp)]"
                : "bg-[var(--cb)] text-[var(--mut)] hover:bg-[var(--cb)] dark:hover:bg-[var(--cb)]"
            }`}
          >
            {processToggle.allCollapsed ? <UnfoldVertical size={13} /> : <FoldVertical size={13} />}
            {processToggle.allCollapsed
              ? i18n.t("mobile.bookmark.processExpand")
              : i18n.t("mobile.bookmark.processCollapse")}
          </button>
        )}
      </div>

      {/* 调色板（点 + 展开）：10 色，已占用置灰 */}
      {paletteOpen && (
        <div
          data-testid="bookmark-palette"
          role="dialog"
          aria-label={i18n.t("mobile.bookmark.paletteAria")}
          className="absolute top-full left-3 z-10 mt-1 flex flex-wrap gap-1.5 rounded-lg border border-[var(--cb)] bg-[var(--cbg)] p-2 shadow-lg"
        >
          {BOOKMARK_COLORS.map((c) => {
            const used = usedColors.has(c);
            return (
              <button
                key={c}
                type="button"
                data-testid={`bookmark-color-${c}`}
                aria-label={i18n.t(
                  used ? "mobile.bookmark.colorUsedAria" : "mobile.bookmark.colorAddAria",
                  { color: c }
                )}
                title={i18n.t(
                  used ? "mobile.bookmark.colorUsedTitle" : "mobile.bookmark.colorAddTitle"
                )}
                disabled={used}
                onClick={() => {
                  onAdd(c);
                  setPaletteOpen(false);
                }}
                className={`h-6 w-6 rounded-full ${
                  used ? "cursor-not-allowed opacity-25" : "hover:scale-110"
                }`}
                style={{ backgroundColor: c }}
              />
            );
          })}
        </div>
      )}
    </div>
  );
}
