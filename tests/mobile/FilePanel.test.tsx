// M3+ 文件面板（计划 Task 3）：纯函数 + 组件行为矩阵。
// 零真实数据：条目夹具全部合成；纯展示组件，零网络（点击回调由测试断言）。
// T1 sheet 化适配（审查 S4）：筛选条重构为单行三组下拉 + 文件名搜索——原
// file-chip-* / file-scope-* / file-origin-* 按钮交互全部改为下拉形态断言，
// kind/origin/scope/search 四状态与叠加过滤语义零变化；布局切换器与关闭钮
// 上收 SessionDetail 顶栏（唯一实例裁决），本组件头部操作用例随之删除。
import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import FilePanel, { FILE_SCOPES, fileKindOf, type FilePanelEntry } from "@/mobile/FilePanel";
import type { SessionFileEntry } from "@/mobile/api";

beforeEach(async () => {
  // Task 7 新文案走 i18n（来源标签/定位入口）；既有硬编码中文断言与 zh 一致
  await i18n.changeLanguage("zh");
});

afterEach(cleanup);

/** 条目夹具（后端 FileEntry camelCase 契约） */
function entry(path: string, over: Partial<SessionFileEntry> = {}): SessionFileEntry {
  return { path, lastSeq: 1, lastTs: 1_700_000_000_000, hits: 1, modified: true, ...over };
}

/** 渲染面板（默认回调全部注入 mock；mode/onModeChange/onClose 等顶栏收编 props 已删） */
function renderPanel(
  entries: FilePanelEntry[],
  over: {
    truncated?: boolean;
    scope?: number;
    loading?: boolean;
    onLocate?: (entry: FilePanelEntry) => void;
  } = {}
) {
  const onOpenFile = vi.fn();
  const onScopeChange = vi.fn();
  render(
    <FilePanel
      entries={entries}
      truncated={over.truncated ?? false}
      scope={over.scope ?? 200}
      loading={over.loading ?? false}
      onScopeChange={onScopeChange}
      onOpenFile={onOpenFile}
      onLocate={over.onLocate}
    />
  );
  return { onOpenFile, onScopeChange };
}

/** 下拉选档（下拉形态的「点击」） */
function pick(testId: string, value: string) {
  fireEvent.change(screen.getByTestId(testId), { target: { value } });
}

describe("FilePanel 纯函数：fileKindOf 扩展名判定", () => {
  it("文档档：md / markdown / txt（大小写不敏感）", () => {
    expect(fileKindOf("/p/README.md")).toBe("doc");
    expect(fileKindOf("/p/notes.markdown")).toBe("doc");
    expect(fileKindOf("/p/a.txt")).toBe("doc");
    expect(fileKindOf("/p/UPPER.MD")).toBe("doc");
  });

  it("图片档：png/jpg/jpeg/gif/webp/svg/bmp（用户裁决 5 白名单）", () => {
    for (const ext of ["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp"]) {
      expect(fileKindOf(`/p/x.${ext}`)).toBe("image");
      expect(fileKindOf(`/p/x.${ext.toUpperCase()}`)).toBe("image");
    }
  });

  it("代码与无扩展名归 other（代码不在专属档，仅在「全部」可见——用户裁决 5）", () => {
    expect(fileKindOf("/p/main.rs")).toBe("other");
    expect(fileKindOf("/p/app.tsx")).toBe("other");
    expect(fileKindOf("/p/Makefile")).toBe("other");
    expect(fileKindOf("/p/.gitignore")).toBe("other"); // 点开头无扩展名
    expect(fileKindOf("D:\\proj\\src\\m.rs")).toBe("other"); // Windows 分隔符
  });

  it("档位常量：三档且顶档 1000（= SessionDetail.MAX_LIMIT）", () => {
    expect([...FILE_SCOPES]).toEqual([200, 500, 1000]);
  });
});

describe("FilePanel 列表渲染", () => {
  it("渲染顺序 = entries 顺序（后端已按 lastSeq 降序，前端不重排）", () => {
    renderPanel([
      entry("/p/late.rs", { lastSeq: 9 }),
      entry("/p/mid.rs", { lastSeq: 5 }),
      entry("/p/early.rs", { lastSeq: 1 }),
    ]);
    const names = [0, 1, 2].map(
      (i) => within(screen.getByTestId(`file-row-${i}`)).getAllByRole("button")[0].textContent
    );
    expect(names[0]).toContain("late.rs");
    expect(names[1]).toContain("mid.rs");
    expect(names[2]).toContain("early.rs");
  });

  it("行信息齐全：文件名（主行）+ 目录前缀 + 相对时间", () => {
    renderPanel([entry("/tmp/proj/src/app.rs", { hits: 3 })]);
    const row = screen.getByTestId("file-row-0");
    expect(within(row).getByTestId("file-row-0-open").textContent).toContain("app.rs");
    expect(row.textContent).toContain("/tmp/proj/src");
  });

  it("hits 徽标已移除（2026-09-16 用户裁决：次数信息用户不在意）", () => {
    // 无论 hits 多少都不出现 × 徽标；lastTs=null 仍显示占位符 —
    renderPanel([
      entry("/p/a.rs", { hits: 7 }),
      entry("/p/b.rs", { lastTs: null, lastSeq: 3, hits: 4 }),
    ]);
    expect(screen.getByTestId("file-row-0").textContent).not.toContain("×");
    expect(screen.getByTestId("file-row-1").textContent).not.toContain("×");
    expect(screen.getByTestId("file-row-1").textContent).toContain("—");
  });

  it("主行点击 → onOpenFile 收到完整 path", () => {
    const { onOpenFile } = renderPanel([entry("/tmp/proj/src/app.rs")]);
    fireEvent.click(screen.getByTestId("file-row-0-open"));
    expect(onOpenFile).toHaveBeenCalledWith("/tmp/proj/src/app.rs");
  });

  it("空态：该范围内未发现文件", () => {
    renderPanel([]);
    expect(screen.getByTestId("panel-empty").textContent).toContain("该范围内未发现文件");
  });

  it("加载态：有旧条目时保留列表（半透明），不闪空态", () => {
    renderPanel([entry("/p/a.rs")], { loading: true });
    expect(screen.getByTestId("file-row-0")).toBeTruthy();
    expect(screen.queryByTestId("panel-empty")).toBeNull();
    expect(screen.getByText("加载中…")).toBeTruthy();
  });
});

describe("FilePanel 筛选条：单行三组下拉（决策 6 / 审查 S4 结构断言）", () => {
  it("三组下拉存在、默认值 200/全部/全部、组间竖分隔符 ×3、搜索框+搜索钮在列", () => {
    renderPanel([]);
    const scopeSel = screen.getByTestId("file-scope-select") as HTMLSelectElement;
    const originSel = screen.getByTestId("file-origin-select") as HTMLSelectElement;
    const kindSel = screen.getByTestId("file-kind-select") as HTMLSelectElement;
    expect(scopeSel.value).toBe("200");
    expect([...scopeSel.options].map((o) => o.text)).toEqual(["200", "500", "1000"]);
    expect(originSel.value).toBe("all");
    expect([...originSel.options].map((o) => o.text)).toEqual([
      "全部",
      "我上传的",
      "工具读取",
      "工具读写",
      "会话产物",
    ]);
    expect(kindSel.value).toBe("all");
    expect([...kindSel.options].map((o) => o.text)).toEqual(["全部", "文档", "图片"]);
    expect(screen.getAllByTestId("file-filter-divider")).toHaveLength(3);
    expect(screen.getByTestId("file-search-input")).toBeTruthy();
    expect(screen.getByTestId("file-search-run").textContent).toBe("搜索");
  });

  it("组标签齐备：追溯范围 / 文件来源 / 文件类型", () => {
    renderPanel([]);
    const bar = screen.getByTestId("file-filter-bar");
    expect(bar.textContent).toContain("追溯范围");
    expect(bar.textContent).toContain("文件来源");
    expect(bar.textContent).toContain("文件类型");
  });

  it("旧控件形态已删：类型 tabs（file-chip-*）与来源/scope pills（file-scope-*/file-origin-*）不存在", () => {
    renderPanel([entry("/p/a.rs")]);
    expect(screen.queryByTestId("file-chip-all")).toBeNull();
    expect(screen.queryByTestId("file-scope-200")).toBeNull();
    expect(screen.queryByTestId("file-origin-user")).toBeNull();
    // 布局切换器/关闭钮已上收顶栏（唯一实例裁决），面板内不再出现
    expect(screen.queryByTestId("preview-toggle-split")).toBeNull();
    expect(screen.queryByTestId("panel-close")).toBeNull();
  });
});

describe("FilePanel 追溯范围下拉（用户裁决 3，下拉形态）", () => {
  it("当前档位回显；切档回调 onScopeChange", () => {
    const { onScopeChange } = renderPanel([entry("/p/a.rs")], { scope: 500 });
    const sel = screen.getByTestId("file-scope-select") as HTMLSelectElement;
    expect(sel.value).toBe("500");
    pick("file-scope-select", "1000");
    expect(onScopeChange).toHaveBeenCalledWith(1000);
  });

  it("档位带单位注释（title）：说明 200/500/1000 是消息条数", () => {
    renderPanel([entry("/p/a.rs")]);
    expect(screen.getByTestId("file-scope-hint").getAttribute("title")).toContain("消息");
  });

  it("truncated 且未到顶 → 显示提示", () => {
    renderPanel([entry("/p/a.rs")], { truncated: true, scope: 200 });
    expect(screen.getByTestId("panel-truncated-hint").textContent).toContain("还有更早文件");
  });

  it("scope=1000 到顶 → 不显示提示（即使 truncated 为真）", () => {
    renderPanel([entry("/p/a.rs")], { truncated: true, scope: 1000 });
    expect(screen.queryByTestId("panel-truncated-hint")).toBeNull();
  });

  it("未 truncated → 不显示提示", () => {
    renderPanel([entry("/p/a.rs")], { truncated: false, scope: 200 });
    expect(screen.queryByTestId("panel-truncated-hint")).toBeNull();
  });
});

describe("FilePanel 文件类型下拉（用户裁决 5，原 chips 改下拉）", () => {
  const mixed = [
    entry("/p/doc.md", { lastSeq: 5 }),
    entry("/p/pic.png", { lastSeq: 4 }),
    entry("/p/code.rs", { lastSeq: 3 }),
  ];

  it("默认「全部」：三类都可见（代码文件仅在全部）", () => {
    renderPanel(mixed);
    expect(screen.getByText("doc.md")).toBeTruthy();
    expect(screen.getByText("pic.png")).toBeTruthy();
    expect(screen.getByText("code.rs")).toBeTruthy();
  });

  it("切「文档」：仅 md 可见，图片与代码隐藏", () => {
    renderPanel(mixed);
    pick("file-kind-select", "doc");
    expect(screen.getByText("doc.md")).toBeTruthy();
    expect(screen.queryByText("pic.png")).toBeNull();
    expect(screen.queryByText("code.rs")).toBeNull();
  });

  it("切「图片」：仅 png 可见", () => {
    renderPanel(mixed);
    pick("file-kind-select", "image");
    expect(screen.getByText("pic.png")).toBeTruthy();
    expect(screen.queryByText("doc.md")).toBeNull();
    expect(screen.queryByText("code.rs")).toBeNull();
  });

  it("切回「全部」：代码文件重新可见（代码仅在类型=全部可见的口径保留）", () => {
    renderPanel(mixed);
    pick("file-kind-select", "doc");
    expect(screen.queryByText("code.rs")).toBeNull();
    pick("file-kind-select", "all");
    expect(screen.getByText("code.rs")).toBeTruthy();
  });

  it("过滤后无匹配 → 空态（不与「范围内无文件」混淆）", () => {
    renderPanel([entry("/p/code.rs")]);
    pick("file-kind-select", "image");
    expect(screen.getByTestId("panel-empty")).toBeTruthy();
  });
});

describe("FilePanel 已改写 / 仅读过 的视觉区分（2026-09-16 用户裁决）", () => {
  it("已改写的文件名保持强调（主文字+加粗）；仅读过的用弱化文字色（两者都是超链接）", () => {
    renderPanel([
      entry("/p/edited.rs", { lastSeq: 9, modified: true }),
      entry("/p/readonly.rs", { lastSeq: 5, modified: false }),
    ]);
    const edited = screen.getByTestId("file-row-0-open");
    const readonly = screen.getByTestId("file-row-1-open");
    // 两者都是可点按钮（超链接语义一致）
    expect(edited.tagName).toBe("BUTTON");
    expect(readonly.tagName).toBe("BUTTON");
    // 已改写：主文字 + 加粗强调；仅读过：弱化文字色，不加粗（2026-10-05 Token 化：
    // 强调/弱化改由 --tx/--mut 表达，不再使用 sky/slate 双态类）
    expect(edited.className).toContain("font-semibold");
    expect(edited.className).toContain("text-[var(--tx)]");
    expect(readonly.className).toContain("text-[var(--mut)]");
    expect(readonly.className).not.toContain("font-semibold");
  });
});

describe("FilePanel 目录路径点击弹全路径（2026-09-16 用户裁决）", () => {
  const longPath = "/very/long/directory/prefix/that/gets/truncated/in/the/list/view/sub";

  it("点次行目录 → 弹层显示完整路径（手机与电脑逻辑一致）", () => {
    renderPanel([entry(`${longPath}/app.rs`)]);
    // 弹层初始不存在
    expect(screen.queryByTestId("path-popover")).toBeNull();
    fireEvent.click(screen.getByTestId("file-row-0-dir"));
    const pop = screen.getByTestId("path-popover");
    expect(pop.textContent).toContain(longPath);
  });

  it("弹层可关闭（再点目录 / 点关闭按钮）", () => {
    renderPanel([entry(`${longPath}/app.rs`)]);
    fireEvent.click(screen.getByTestId("file-row-0-dir"));
    expect(screen.getByTestId("path-popover")).toBeTruthy();
    fireEvent.click(screen.getByTestId("path-popover-close"));
    expect(screen.queryByTestId("path-popover")).toBeNull();
  });

  it("无目录前缀的行不该有可点目录按钮（避免空弹层）", () => {
    renderPanel([entry("app.rs")]);
    expect(screen.queryByTestId("file-row-0-dir")).toBeNull();
  });
});

// ==== M5 B3：来源筛选 + 文件名搜索（决策 10 / 线稿三池；来源改下拉形态）====

describe("FilePanel 来源筛选（M5 B3，下拉形态）", () => {
  const entries = [
    entry("/p/upload.png", { lastSeq: 6, origin: "user" }),
    entry("/p/read-only.rs", { lastSeq: 5, origin: "tool_read" }),
    entry("/p/edited.rs", { lastSeq: 4, origin: "tool_write" }),
    entry("/p/generated-report.md", { lastSeq: 3, origin: "session_artifact" }),
    entry("/p/legacy.rs", { lastSeq: 2 }), // 旧载荷无 origin
  ];

  it("默认「全部来源」：五条（含无 origin 的旧载荷条目）都可见", () => {
    renderPanel(entries);
    expect(screen.getByText("upload.png")).toBeTruthy();
    expect(screen.getByText("legacy.rs")).toBeTruthy();
  });

  it("切「我上传的」：仅 origin=user 可见，旧载荷条目隐藏", () => {
    renderPanel(entries);
    pick("file-origin-select", "user");
    expect(screen.getByText("upload.png")).toBeTruthy();
    expect(screen.queryByText("read-only.rs")).toBeNull();
    expect(screen.queryByText("legacy.rs")).toBeNull();
  });

  it("切「工具读取」与「工具读写」各自精确过滤", () => {
    renderPanel(entries);
    pick("file-origin-select", "tool_read");
    expect(screen.getByText("read-only.rs")).toBeTruthy();
    expect(screen.queryByText("edited.rs")).toBeNull();
    pick("file-origin-select", "tool_write");
    expect(screen.getByText("edited.rs")).toBeTruthy();
    expect(screen.queryByText("read-only.rs")).toBeNull();
  });

  it("切「会话产物」：仅 origin=session_artifact 可见（第四池兜底，脚本产物上墙）", () => {
    renderPanel(entries);
    pick("file-origin-select", "session_artifact");
    expect(screen.getByText("generated-report.md")).toBeTruthy();
    expect(screen.queryByText("edited.rs")).toBeNull();
    expect(screen.queryByText("upload.png")).toBeNull();
  });

  it("行内来源徽标：四值中文文案，旧载荷不渲染徽标", () => {
    renderPanel(entries);
    // 徽标按行内 testid 定位（筛选条里也有同文案，不能全局查文本）
    expect(screen.getByTestId("file-row-0-origin").textContent).toBe("我上传的");
    expect(screen.getByTestId("file-row-1-origin").textContent).toBe("工具读取");
    expect(screen.getByTestId("file-row-2-origin").textContent).toBe("工具读写");
    expect(screen.getByTestId("file-row-3-origin").textContent).toBe("会话产物");
    expect(screen.queryByTestId("file-row-4-origin")).toBeNull();
  });

  it("行内来源徽标含会话产物（session_artifact → 会话产物）", () => {
    renderPanel(entries);
    expect(screen.getByTestId("file-row-3-origin").textContent).toBe("会话产物");
  });

  it("来源筛选无匹配 → 「无匹配文件」空态", () => {
    renderPanel([entry("/p/a.rs", { origin: "user" })]);
    pick("file-origin-select", "tool_write");
    expect(screen.getByText("无匹配文件")).toBeTruthy();
  });
});

describe("FilePanel 文件名搜索（M5 决策 10：点「搜索」或回车才执行）", () => {
  const entries = [
    entry("/p/行程示意图.png", { lastSeq: 3 }),
    entry("/p/retrace_labels.py", { lastSeq: 2 }),
    entry("/p/report-draft.md", { lastSeq: 1 }),
  ];

  it("仅输入不执行：草稿不改变列表，点「搜索」才过滤（大小写不敏感、按文件名模糊匹配）", () => {
    renderPanel(entries);
    fireEvent.change(screen.getByTestId("file-search-input"), { target: { value: "REPORT" } });
    // 草稿态：三行都还在
    expect(screen.getByText("行程示意图.png")).toBeTruthy();
    fireEvent.click(screen.getByTestId("file-search-run"));
    expect(screen.getByText("report-draft.md")).toBeTruthy();
    expect(screen.queryByText("行程示意图.png")).toBeNull();
    expect(screen.queryByText("retrace_labels.py")).toBeNull();
  });

  it("回车提交表单等价于点「搜索」", () => {
    renderPanel(entries);
    const input = screen.getByTestId("file-search-input");
    fireEvent.change(input, { target: { value: "行程" } });
    fireEvent.submit(input.closest("form")!);
    expect(screen.getByText("行程示意图.png")).toBeTruthy();
    expect(screen.queryByText("retrace_labels.py")).toBeNull();
  });

  it("清空草稿再搜索 → 清除过滤，恢复全列表", () => {
    renderPanel(entries);
    fireEvent.change(screen.getByTestId("file-search-input"), { target: { value: "行程" } });
    fireEvent.click(screen.getByTestId("file-search-run"));
    expect(screen.queryByText("retrace_labels.py")).toBeNull();
    fireEvent.change(screen.getByTestId("file-search-input"), { target: { value: "" } });
    fireEvent.click(screen.getByTestId("file-search-run"));
    expect(screen.getByText("retrace_labels.py")).toBeTruthy();
  });

  it("搜索与来源筛选、类型档叠加生效", () => {
    const mixed = [
      entry("/p/upload.png", { lastSeq: 4, origin: "user" }),
      entry("/p/tool.png", { lastSeq: 3, origin: "tool_read" }),
      entry("/p/upload.md", { lastSeq: 2, origin: "user" }),
    ];
    renderPanel(mixed);
    // 类型=图片
    pick("file-kind-select", "image");
    // 来源=我上传的
    pick("file-origin-select", "user");
    expect(screen.getByText("upload.png")).toBeTruthy();
    expect(screen.queryByText("tool.png")).toBeNull();
    expect(screen.queryByText("upload.md")).toBeNull();
    // 再叠搜索 "upload"：upload.png 同时满足三重过滤仍在列；其余两条被滤掉
    fireEvent.change(screen.getByTestId("file-search-input"), { target: { value: "upload" } });
    fireEvent.click(screen.getByTestId("file-search-run"));
    expect(screen.getByText("upload.png")).toBeTruthy();
    expect(screen.queryByTestId("panel-empty")).toBeNull();
    expect(screen.queryByText("tool.png")).toBeNull();
    expect(screen.queryByText("upload.md")).toBeNull();
  });

  it("搜索无匹配 → 「无匹配文件」空态", () => {
    renderPanel(entries);
    fireEvent.change(screen.getByTestId("file-search-input"), { target: { value: "不存在" } });
    fireEvent.click(screen.getByTestId("file-search-run"));
    expect(screen.getByText("无匹配文件")).toBeTruthy();
  });
});

describe("FilePanel 可预览说明（M5 P2-a）", () => {
  it("面板底部展示类型与上限说明（文本 500KB / 图片 5MB）", () => {
    renderPanel([]);
    expect(screen.getByTestId("panel-preview-help").textContent).toContain("500KB");
    expect(screen.getByTestId("panel-preview-help").textContent).toContain("5MB");
  });
});

// ==== Task 7：来源按钮（定位最近引用）与文件名预览是两个独立热区 ====

describe("FilePanel 定位最近引用（Task 7）", () => {
  /** 合并视图条目：后端字段 + 结构化来源/状态标量 */
  function viewEntry(over: Partial<FilePanelEntry> = {}): FilePanelEntry {
    return {
      path: "/p/a.md",
      lastSeq: 1,
      lastTs: 1_700_000_000_000,
      hits: 1,
      modified: false,
      origin: "tool_read",
      ...over,
    } as FilePanelEntry;
  }

  it("来源按钮：展示最近引用的行为来源并触发 onLocate，不打开预览", () => {
    const onLocate = vi.fn();
    const { onOpenFile } = renderPanel(
      [viewEntry({ path: "/p/edited.md", artifactSource: "file_change" })],
      { onLocate }
    );
    const locate = screen.getByTestId("file-row-0-locate");
    expect(locate.textContent).toBe("修改");
    fireEvent.click(locate);
    expect(onLocate).toHaveBeenCalledTimes(1);
    expect(onLocate.mock.calls[0][0].path).toBe("/p/edited.md");
    expect(onOpenFile).not.toHaveBeenCalled();
  });

  it("文件名预览热区不触发定位（两个热区互不触发）", () => {
    const onLocate = vi.fn();
    const { onOpenFile } = renderPanel(
      [viewEntry({ path: "/p/a.md", artifactSource: "image_view" })],
      { onLocate }
    );
    fireEvent.click(screen.getByTestId("file-row-0-open"));
    expect(onOpenFile).toHaveBeenCalledWith("/p/a.md");
    expect(onLocate).not.toHaveBeenCalled();
  });

  it("未知来源仍保留定位入口（不依赖 origin / 结构化来源，也不预造消息）", () => {
    const onLocate = vi.fn();
    renderPanel([{ path: "/p/legacy.rs", lastSeq: 1, lastTs: null, hits: 1, modified: false }], {
      onLocate,
    });
    const locate = screen.getByTestId("file-row-0-locate");
    expect(locate.textContent).toBe("定位");
    fireEvent.click(locate);
    expect(onLocate).toHaveBeenCalledTimes(1);
    expect(onLocate.mock.calls[0][0].path).toBe("/p/legacy.rs");
  });

  it("fileChange 原始状态原样展示（不写成成功写盘）", () => {
    renderPanel(
      [
        viewEntry({
          path: "/p/edit.md",
          origin: "tool_write",
          artifactSource: "file_change",
          sourceStatus: "in_progress",
          changeKind: "modify",
        }),
      ],
      { onLocate: vi.fn() }
    );
    const status = screen.getByTestId("file-row-0-status");
    expect(status.textContent).toBe("in_progress");
    expect(status.getAttribute("data-status")).toBe("in_progress");
    // 池中没有任何“已写入/成功”的成功文案（状态只作原始标量展示）
    expect(screen.getByTestId("file-panel").textContent).not.toMatch(/已写入|写入成功|success/i);
  });

  it("来源摘要：仅在有摘要时渲染一行（truncate + 完整 title），后端行布局不变", () => {
    renderPanel(
      [
        viewEntry({
          path: "/p/exec-1.png",
          artifactSource: "image_generation",
          summary: "请根据这张照片生成一张十六比九的",
        }),
        viewEntry({ path: "/p/backend.md" }),
      ],
      { onLocate: vi.fn() }
    );
    const summary = screen.getByTestId("file-row-0-summary");
    expect(summary.textContent).toBe("请根据这张照片生成一张十六比九的");
    expect(summary.getAttribute("title")).toBe("请根据这张照片生成一张十六比九的");
    expect(summary.className).toContain("truncate");
    // 无引用证据的后端行没有摘要行，原布局（文件名/来源/目录/时间）不变
    expect(screen.queryByTestId("file-row-1-summary")).toBeNull();
    expect(screen.getByTestId("file-row-1-open").textContent).toContain("backend.md");
    expect(screen.getByTestId("file-row-1-origin").textContent).toBe("工具读取");
  });

  it("未验证的合成写来源行（tool_write + modified=false）不加粗，状态仍原样展示", () => {
    renderPanel(
      [
        viewEntry({
          path: "/p/exec-1.png",
          origin: "tool_write",
          modified: false,
          artifactSource: "file_change",
          sourceStatus: "failed",
        }),
      ],
      { onLocate: vi.fn() }
    );
    expect(screen.getByTestId("file-row-0-origin").textContent).toBe("工具读写");
    expect(screen.getByTestId("file-row-0-open").className).not.toContain("font-semibold");
    expect(screen.getByTestId("file-row-0-status").textContent).toBe("failed");
  });

  it("未提供 onLocate（非 Codex 会话）不渲染定位按钮——其他工具文件池不变", () => {
    renderPanel([viewEntry({ path: "/p/a.md", origin: "user" })]);
    expect(screen.queryByTestId("file-row-0-locate")).toBeNull();
    expect(screen.getByTestId("file-row-0-origin").textContent).toBe("我上传的");
  });
});
