// 会话消息渲染器（2026-10-09 观察台 T6 从 SessionDetail 纯搬家）：会话详情与
// 子 agent 详情共用同一套视觉与交互（spec §三.5「不另造一套」）。纯搬家零语义
// 变化——renderBody 的 per-kind 分支（markdown/plan 卡/工具参数升格）注释随代码
// 一并迁来，原样保留。
import { useCallback } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import remarkParse from "remark-parse";
import rehypeHighlight from "rehype-highlight";
import { unified } from "unified";
import i18n from "@/i18n";
import type { SessionMessage } from "./api";

/** 已知路径按长度降序（最长优先替换：路径互为前缀时不被短路径截断）。
 *  2026-10-09 T6 随渲染器从 SessionDetail 迁入（评审 P3-10：原处删除防死代码；
 *  勿在 hook 内再内联第二份排序） */
export function sortedPaths(files: Set<string>): string[] {
  return [...files].sort((a, b) => b.length - a.length);
}

/** 链接化分段元素：文本段或命中已知路径的文件段 */
type LinkSegment = { type: "text" | "file"; value: string };

interface MarkdownNode {
  type: string;
  value?: string;
  url?: string;
  title?: string | null;
  children?: MarkdownNode[];
  [key: string]: unknown;
}

/** 纯文本分段链接化：按已知路径把正文切成文本段与文件段（thinking / tool-result 用） */
export function linkifySegments(text: string, files: string[]): LinkSegment[] {
  let segments: LinkSegment[] = [{ type: "text", value: text }];
  for (const p of files) {
    if (!p) continue;
    const next: LinkSegment[] = [];
    for (const seg of segments) {
      if (seg.type !== "text" || !seg.value.includes(p)) {
        next.push(seg);
        continue;
      }
      const parts = seg.value.split(p);
      parts.forEach((part, i) => {
        if (part) next.push({ type: "text", value: part });
        if (i < parts.length - 1) next.push({ type: "file", value: p });
      });
    }
    segments = next;
  }
  return segments;
}

/** ReactMarkdown 所用的 CommonMark parser；代码位置与可渲染文本同源。 */
const markdownParser = unified().use(remarkParse).use(remarkGfm);

/** Markdown 正文中可链接的路径段；code / inlineCode / 既有链接节点不参与。 */
export function linkifyMarkdownSegments(text: string, files: string[]): LinkSegment[] {
  const tree = markdownParser.parse(text) as unknown as MarkdownNode;
  const segments: LinkSegment[] = [];
  const visit = (node: MarkdownNode) => {
    if (
      node.type === "code" ||
      node.type === "inlineCode" ||
      node.type === "link" ||
      node.type === "linkReference"
    )
      return;
    if (node.type === "text" && typeof node.value === "string") {
      segments.push(...linkifySegments(node.value, files));
      return;
    }
    node.children?.forEach(visit);
  };
  visit(tree);
  return segments;
}

/** remark 插件：只把可见 Markdown 文本节点中的已知路径改成文件链接。 */
export function remarkFileLinks(files: string[]) {
  return () => (tree: MarkdownNode) => {
    const transform = (node: MarkdownNode): MarkdownNode[] => {
      if (
        node.type === "code" ||
        node.type === "inlineCode" ||
        node.type === "link" ||
        node.type === "linkReference"
      )
        return [node];
      if (node.type === "text" && typeof node.value === "string") {
        return linkifySegments(node.value, files).map((segment): MarkdownNode =>
          segment.type === "file"
            ? {
                type: "link",
                url: `#file:${encodeURIComponent(segment.value)}`,
                title: null,
                children: [{ type: "text", value: segment.value }],
              }
            : { type: "text", value: segment.value }
        );
      }
      if (node.children !== undefined) node.children = node.children.flatMap(transform);
      return [node];
    };
    if (tree.children !== undefined) tree.children = tree.children.flatMap(transform);
  };
}

/** 计划正文抽取（2026-09-20 用户实测：ExitPlanMode 的整篇计划在手机上是 \n 字面量汤）。
 *  后端把工具输入原封透传为 JSON 串（claude content.rs:413 / zcode content.rs:1630
 *  同为 serde_json::to_string），字符串值里的换行全是 `\n` 转义，塞进 <pre> 不可读。
 *  按形态识别：toolArgs 解析出**非空字符串 `plan` 字段** → 返回该正文（走 markdown
 *  渲染）；其余一切情况 → null（维持原样渲染）。不看 toolName——zcode 的
 *  ExitPlanMode 输入同为 {plan}（zcode.cjs：校验 e.plan.trim()），但 兔维斯 记录的
 *  是显示 title，按名字匹配会漏；形态匹配 claude/zcode 同覆盖。
 *  注意：纯 pretty-print（stringify(_,null,2)）救不了——字符串值里的 \n 依然是
 *  转义（JSON 规范）。用户裁决：其他工具的参数渲染不做通用美化。
 *  2026-10-09 T6 随 renderBody 迁入本文件并 export（评审 P1-3：renderBody 的
 *  tool-call 分支消费它——留在 SessionDetail 会循环 import；仓内无其他消费方，
 *  已核实。isPlanPending 仍留守 SessionDetail——详情页挂载门专属，renderBody
 *  不消费）。 */
export function extractPlanBody(toolArgs: string): string | null {
  try {
    const parsed = JSON.parse(toolArgs) as { plan?: unknown };
    if (typeof parsed?.plan === "string" && parsed.plan.trim() !== "") {
      return parsed.plan;
    }
    return null;
  } catch {
    return null;
  }
}

/** 消息渲染器集合（SessionDetail 与 SubagentDetail 共用）。
 *  openFile：正文路径点击回调（两页各自的预览入口）；files：已知路径集（链接化）。 */
export function useMessageRenderers({
  openFile,
  files,
}: {
  openFile: (path: string) => void;
  files: Set<string>;
}) {
  const sorted = sortedPaths(files);
  // ↓ 以下 renderMarkdown / renderLinkifiedText / renderBody 三段 useCallback =
  // SessionDetail 原文整段搬迁（含 #file: 内链拦截 / md-body 排版层 / plan 与
  // plan-file 卡 / extractPlanBody 工具参数升格——所有分支语义与注释原样保留，
  // 此处不重复罗列）。唯一改动：闭包变量 sortedFiles → 本 hook 的 sorted。
  // markdown 渲染（assistant / user 正文）：remark-gfm 表格/删除线 + rehype-highlight
  // 代码块高亮（主题色由 mobile.css 双态内联，见其注释）；#file: 内链拦截为文件按钮
  const renderMarkdown = useCallback(
    (text: string) => (
      <ReactMarkdown
        remarkPlugins={[remarkGfm, remarkFileLinks(sorted)]}
        rehypePlugins={[rehypeHighlight]}
        components={{
          a: ({ href, children }) => {
            if (href?.startsWith("#file:")) {
              const p = decodeURIComponent(href.slice("#file:".length));
              return (
                <button
                  type="button"
                  data-testid="file-link"
                  data-file-path={p}
                  className="inline text-left break-all text-[var(--tx)] underline underline-offset-2 data-[jump-node=true]:rounded data-[jump-node=true]:bg-amber-500/40"
                  onClick={() => openFile(p)}
                >
                  {children}
                </button>
              );
            }
            return (
              <a href={href} target="_blank" rel="noreferrer">
                {children}
              </a>
            );
          },
        }}
      >
        {text}
      </ReactMarkdown>
    ),
    [openFile, sorted]
  );

  // 纯文本链接化渲染（thinking / tool-result：正文常含路径，值得可点）
  const renderLinkifiedText = useCallback(
    (text: string) =>
      linkifySegments(text, sorted).map((seg, i) =>
        seg.type === "text" ? (
          <span key={i}>{seg.value}</span>
        ) : (
          <button
            key={i}
            type="button"
            data-testid="file-link"
            data-file-path={seg.value}
            className="break-all text-[var(--tx)] underline underline-offset-2 data-[jump-node=true]:rounded data-[jump-node=true]:bg-amber-500/40"
            onClick={() => openFile(seg.value)}
          >
            {seg.value}
          </button>
        )
      ),
    [openFile, sorted]
  );

  const renderBody = useCallback(
    (m: SessionMessage) => {
      switch (m.kind) {
        case "assistant":
        case "user":
          // md-body：markdown 排版层（Bug 5——preflight 拍平标题/列表的修复锚点）
          return <div className="md-body text-sm">{renderMarkdown(m.content)}</div>;
        case "thinking":
          return (
            <div className="text-xs break-words whitespace-pre-wrap text-[var(--mut)]">
              {renderLinkifiedText(m.content)}
            </div>
          );
        case "tool-result":
          return (
            <pre className="overflow-x-auto rounded-lg border border-[var(--cb)] bg-[var(--cbg)] p-2 text-xs break-words whitespace-pre-wrap text-[var(--tx)]">
              {renderLinkifiedText(m.content)}
            </pre>
          );
        case "plan": {
          // T1 一等计划卡片：后端已把 ExitPlanMode 形态（input.plan 非空）升格为
          // kind="plan"，content 即计划 markdown 本体——无需 extractPlanBody，
          // 直接渲染；恒展开（isCollapsed/isToggleable 豁免总结模式折叠）。
          // 旧存量会话里未升格的 ExitPlanMode tool-call 仍走下方 extractPlanBody 分支
          return (
            <div
              data-testid={`plan-${m.seq}`}
              className="rounded-lg border border-[var(--cb)] bg-[var(--cbg)] p-2 text-xs text-[var(--tx)]"
            >
              <p className="mb-1 text-[11px] font-medium tracking-wide text-[var(--mut)] uppercase">
                {i18n.t("mobile.msgRender.planLabel")}
              </p>
              {/* 丁T5 修复（问题 11 的真实断点）：计划卡此前**漏挂** `.md-body` 排版层
                  ——Tailwind v4 preflight 把 h1-h6 的字号/字重与 ul/ol 的 list-style
                  全部重置（见 mobile.css 排版层注释），故计划正文里的 `###` 小标题与
                  `-` 列表在这张卡上被**拍平成正文**。
                  **订正（复评 F6-1）**：实际是「普通消息卡挂了，**计划卡与工具参数升格
                  卡都没挂**」——本文件里三张用 `renderMarkdown` 的卡中只有
                  `case "assistant"` 挂了 `.md-body`；下面 `case "tool-call"` 的升格支
                  （claude ExitPlanMode 走的那条，即任务书问题 11 点名的路径）同样漏挂，
                  本次两张一起补齐。
                  真机证据：本机 rollout `~/.codex/sessions/2026/09/21/
                  rollout-2026-09-21T17-38-17-…jsonl` 行 115 的计划含 4 个 `###` 小标题
                  + 14 行列表——正是任务书问题 11 说的「markdown 不完整」。 */}
              <div className="md-body">{renderMarkdown(m.content)}</div>
            </div>
          );
        }
        case "plan-file": {
          // T7 计划文件卡：后端从工具结果/正文里识别出计划文件引用（如 kimi 的
          // "Wrote 4263 bytes to …/plans/x.md"）→ 补一张 kind="plan-file" 消息，
          // content = 文件路径。卡片显示文件名 + 「查看计划」按钮 → 走既有文件预览
          // 面板（后端豁免面已放行 kimi 的 agents/main/plans 子树）。
          const full = m.content;
          const name = full.split(/[\\/]/).pop() || full;
          return (
            <div
              data-testid={`plan-file-${m.seq}`}
              className="rounded-lg border border-emerald-500/40 bg-emerald-500/5 p-2 text-xs"
            >
              <p className="mb-1 text-[11px] font-medium tracking-wide text-emerald-700 uppercase dark:text-emerald-400">
                {i18n.t("mobile.msgRender.planFileLabel")}
              </p>
              <div className="flex items-center gap-2">
                <span
                  data-testid={`plan-file-name-${m.seq}`}
                  className="min-w-0 flex-1 truncate font-mono text-[var(--tx)]"
                  title={full}
                >
                  {name}
                </span>
                <button
                  type="button"
                  data-testid={`plan-file-open-${m.seq}`}
                  className="shrink-0 rounded-md bg-emerald-600 px-2 py-0.5 text-[11px] font-medium text-white hover:bg-emerald-700"
                  onClick={() => openFile(full)}
                >
                  {i18n.t("mobile.msgRender.openPlan")}
                </button>
              </div>
            </div>
          );
        }
        case "tool-call": {
          // 计划类工具（ExitPlanMode / zcode 同形）：plan 字段是整篇 markdown，
          // 抽出来走 markdown 渲染；其余工具维持参数 JSON 原样（用户裁决不做通用美化）
          const planBody = m.toolArgs ? extractPlanBody(m.toolArgs) : null;
          return (
            <div className="space-y-1">
              <p className="text-xs font-medium text-[var(--tx)]">
                {m.toolName
                  ? i18n.t("mobile.msgRender.toolCall", { name: m.toolName })
                  : i18n.t("mobile.msgRender.toolCallGeneric")}
              </p>
              {m.toolArgs && planBody === null && (
                <pre
                  data-testid={`tool-args-${m.seq}`}
                  className="overflow-x-auto rounded-lg bg-[var(--cbg)] p-2 text-xs text-[var(--tx)]"
                >
                  {m.toolArgs}
                </pre>
              )}
              {planBody !== null && (
                <div
                  data-testid={`tool-args-${m.seq}`}
                  className="rounded-lg border border-[var(--cb)] bg-[var(--cbg)] p-2 text-xs text-[var(--tx)]"
                >
                  <p className="mb-1 text-[11px] font-medium tracking-wide text-[var(--mut)] uppercase">
                    {i18n.t("mobile.msgRender.planLabel")}
                  </p>
                  {/* 丁T5 复评 F6-1：本支（**工具参数升格**——claude 的 ExitPlanMode 走这
                      里，任务书问题 11 点名的路径）与上面的 `case "plan"` 是同一缺陷的
                      两半：都漏挂 `.md-body`，故 `###` 标题与 `-` 列表被 preflight 拍平。
                      本次两张一起补齐。 */}
                  <div className="md-body">{renderMarkdown(planBody)}</div>
                </div>
              )}
            </div>
          );
        }
        default:
          return <div className="text-sm">{m.content}</div>;
      }
    },
    [renderLinkifiedText, renderMarkdown, openFile]
  );
  return { renderMarkdown, renderLinkifiedText, renderBody };
}
