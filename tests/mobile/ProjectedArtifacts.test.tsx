// 任务 6：结构化附件卡片渲染与展示限额。
// 覆盖：12/13 张图片卡与单次汇总、6/7 个 fileChange chip、整卡/整 chip 点击、
// 来源标签（上传/生成/修改/查看，中英）、同回合提示词摘要与 24 字上限、
// 缺路径/fileId/data URL/错误 scheme 占位、外链仅 HTTP(S)、读取失败与解码失败占位。
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import ProjectedArtifacts, {
  MAX_FILE_CHANGE_CHIPS,
  MAX_TURN_IMAGE_CARDS,
  buildArtifactMessageContexts,
  capArtifactSummary,
  type ArtifactMessageContext,
} from "@/mobile/ProjectedArtifacts";
import useArtifactImages from "@/mobile/useArtifactImages";
import type { ProjectedArtifact, ProjectedMessage } from "@/mobile/codexIpcApi";
import { MockIntersectionObserver } from "./intersectionObserverMock";

function artifact(over: Partial<ProjectedArtifact> & Pick<ProjectedArtifact, "artifactId">) {
  return {
    kind: "image",
    source: "local_image",
    path: null,
    url: null,
    fileKey: null,
    ordinal: 1,
    imageIndexInTurn: null,
    summary: null,
    sourceStatus: null,
    changeKind: null,
    unavailableReason: null,
    ...over,
  } satisfies ProjectedArtifact;
}

function message(over: Partial<ProjectedMessage> & Pick<ProjectedMessage, "id">): ProjectedMessage {
  return {
    role: "user",
    kind: "userMessage",
    text: "",
    ts: null,
    turnId: "turn-1",
    artifacts: [],
    turnImageCount: 0,
    omittedArtifactCount: 0,
    ...over,
  };
}

function Harness({
  msg,
  context,
  openFile,
  onOpenFilePanel,
}: {
  msg: ProjectedMessage;
  context?: ArtifactMessageContext;
  openFile: (path: string) => void;
  onOpenFilePanel?: () => void;
}) {
  const images = useArtifactImages("sess-1");
  const ctx = context ?? buildArtifactMessageContexts([msg]).get(msg.id);
  return (
    <ProjectedArtifacts
      message={msg}
      context={ctx}
      openFile={openFile}
      images={images}
      onOpenFilePanel={onOpenFilePanel}
    />
  );
}

let fetchMock: ReturnType<typeof vi.fn>;
let revokeObjectURL: ReturnType<typeof vi.fn>;
let urlSeq = 0;

beforeEach(async () => {
  await i18n.changeLanguage("zh");
  MockIntersectionObserver.install();
  urlSeq = 0;
  Object.defineProperty(URL, "createObjectURL", {
    value: vi.fn(() => `blob:pa-${++urlSeq}`),
    configurable: true,
    writable: true,
  });
  revokeObjectURL = vi.fn();
  Object.defineProperty(URL, "revokeObjectURL", {
    value: revokeObjectURL,
    configurable: true,
    writable: true,
  });
  fetchMock = vi.fn(
    async () => new Response("png-bytes", { status: 200, headers: { "content-type": "image/png" } })
  );
  vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

function imageCalls(): number {
  return fetchMock.mock.calls.filter((c) => String(c[0]).includes("/m/api/v1/file?")).length;
}

describe("buildArtifactMessageContexts：同回合提示词与图片预算", () => {
  it("提示词只取同回合、附件前的非空用户消息；不借用上一回合", () => {
    const messages: ProjectedMessage[] = [
      message({ id: "u1", text: "第一回合提示词", role: "user" }),
      message({
        id: "a1",
        role: "system",
        kind: "imageGeneration",
        text: "",
        artifacts: [artifact({ artifactId: "i1", path: "/p/1.png", imageIndexInTurn: 1 })],
        turnImageCount: 1,
      }),
      message({ id: "u2", role: "user", turnId: "turn-2", text: "   " }),
      message({
        id: "a2",
        role: "system",
        kind: "imageGeneration",
        text: "",
        artifacts: [artifact({ artifactId: "i2", path: "/p/2.png", imageIndexInTurn: 1 })],
        turnImageCount: 1,
        turnId: "turn-2",
      }),
      message({
        id: "a-null",
        role: "system",
        kind: "imageView",
        turnId: null,
        text: "",
        artifacts: [artifact({ artifactId: "i3", path: "/p/3.png", imageIndexInTurn: null })],
      }),
    ];
    const ctx = buildArtifactMessageContexts(messages);
    expect(ctx.get("u1")?.turnPrompt).toBe("第一回合提示词");
    expect(ctx.get("a1")?.turnPrompt).toBe("第一回合提示词");
    // 第二回合只有空白用户消息：不得回退到第一回合提示词
    expect(ctx.get("u2")?.turnPrompt).toBeNull();
    expect(ctx.get("a2")?.turnPrompt).toBeNull();
    // 无 turnId 的独立消息不继承任何回合提示词
    expect(ctx.get("a-null")?.turnPrompt).toBeNull();
  });

  it("12 图为上限，超额只在承担汇总的消息上出现一次", () => {
    const images = Array.from({ length: 13 }, (_, i) =>
      artifact({
        artifactId: `img-${i + 1}`,
        path: `/p/${i + 1}.png`,
        imageIndexInTurn: i + 1,
      })
    );
    const m = message({ id: "m1", turnImageCount: 13, artifacts: images });
    const ctx = buildArtifactMessageContexts([m]).get("m1")!;
    expect(ctx.visibleImageIds.size).toBe(MAX_TURN_IMAGE_CARDS);
    expect(ctx.visibleImageIds.has("img-13")).toBe(false);
    expect(ctx.hiddenImageCount).toBe(1);
    expect(ctx.turnImageCount).toBe(13);
  });
});

describe("ProjectedArtifacts：来源标签与摘要", () => {
  it("上传/生成/修改/查看标签与正确文件名（zh）", () => {
    const m = message({
      id: "m1",
      role: "system",
      kind: "imageGeneration",
      text: "",
      artifacts: [
        artifact({ artifactId: "gen", source: "image_generation", path: "/p/out/gen-1.png" }),
        artifact({ artifactId: "up", source: "local_image", path: "/p/up/photo.jpg" }),
        artifact({
          artifactId: "doc",
          kind: "document",
          source: "mention",
          path: "/p/docs/spec.md",
          summary: "spec.md",
        }),
        artifact({
          artifactId: "chg",
          kind: "document",
          source: "file_change",
          path: "/p/src/app.ts",
          sourceStatus: "in_progress",
          changeKind: "modify",
        }),
        artifact({ artifactId: "view", source: "image_view", path: "/p/seen.png" }),
      ],
    });
    render(<Harness msg={m} openFile={() => {}} />);
    const labels = screen.getAllByTestId("artifact-source-label").map((el) => el.textContent);
    expect(labels).toEqual(expect.arrayContaining(["生成", "上传", "修改", "查看"]));
    expect(screen.getByText("gen-1.png")).toBeTruthy();
    expect(screen.getByText("photo.jpg")).toBeTruthy();
    expect(screen.getByText("spec.md")).toBeTruthy();
    expect(screen.getByText("app.ts")).toBeTruthy();
    expect(screen.getByText("seen.png")).toBeTruthy();
    // 未验证状态不得出现「成功/已完成」式成功语言，只保留原始标量
    const chg = screen.getByTestId("artifact-chip-chg");
    expect(chg.textContent).not.toContain("成功");
    expect(chg.textContent).not.toContain("已完成");
    expect(chg.getAttribute("data-source-status")).toBe("in_progress");
  });

  it("英文侧同样区分上传/生成/修改/查看", async () => {
    await i18n.changeLanguage("en");
    const m = message({
      id: "m1",
      role: "system",
      kind: "imageGeneration",
      artifacts: [
        artifact({ artifactId: "gen", source: "image_generation", path: "/p/gen.png" }),
        artifact({ artifactId: "up", source: "local_image", path: "/p/photo.jpg" }),
        artifact({
          artifactId: "chg",
          kind: "document",
          source: "file_change",
          path: "/p/app.ts",
        }),
        artifact({ artifactId: "view", source: "image_view", path: "/p/seen.png" }),
      ],
    });
    render(<Harness msg={m} openFile={() => {}} />);
    const labels = screen.getAllByTestId("artifact-source-label").map((el) => el.textContent);
    expect(labels).toEqual(expect.arrayContaining(["Generated", "Uploaded", "Modified", "Viewed"]));
  });

  it("摘要优先同回合提示词 → 附件摘要 → 来源标签；展示上限 24 字", () => {
    const prompt = "请根据这张照片生成一张十六比九的超宽幅电影感海报封面";
    const first = message({
      id: "m1",
      text: prompt,
      turnImageCount: 2,
      artifacts: [
        artifact({
          artifactId: "a",
          path: "/p/a.png",
          imageIndexInTurn: 1,
          summary: "附件摘要不应覆盖同回合提示词",
        }),
      ],
    });
    const second = message({
      id: "m2",
      turnId: "turn-2",
      text: "",
      artifacts: [
        artifact({
          artifactId: "b",
          path: "/p/b.png",
          imageIndexInTurn: 1,
          summary: "仅附件摘要",
        }),
        artifact({ artifactId: "c", path: "/p/c.png", imageIndexInTurn: 2 }),
      ],
    });
    const ctx = buildArtifactMessageContexts([first, second]);
    render(
      <>
        <Harness msg={first} context={ctx.get("m1")} openFile={() => {}} />
        <Harness msg={second} context={ctx.get("m2")} openFile={() => {}} />
      </>
    );
    const summaries = screen.getAllByTestId("artifact-summary").map((el) => el.textContent ?? "");
    expect(summaries[0]).toBe(capArtifactSummary(prompt));
    expect(summaries[0]?.length).toBe(25); // 24 字 + 省略号
    expect(summaries[0].endsWith("…")).toBe(true);
    expect(summaries[1]).toBe("仅附件摘要");
    // 无提示词、无附件摘要 → 来源标签兜底
    expect(summaries[2]).toBe("来源：上传");
  });
});

describe("ProjectedArtifacts：点击与展示限额", () => {
  it("图片整卡与文档整 chip 可点，均打开同一预览入口", () => {
    const openFile = vi.fn();
    const m = message({
      id: "m1",
      role: "system",
      kind: "imageGeneration",
      artifacts: [
        artifact({ artifactId: "img", path: "/p/pic.png", imageIndexInTurn: 1 }),
        artifact({
          artifactId: "doc",
          kind: "document",
          source: "mention",
          path: "/p/doc.md",
        }),
      ],
    });
    render(<Harness msg={m} openFile={openFile} />);
    fireEvent.click(screen.getByTestId("artifact-card-img"));
    expect(openFile).toHaveBeenCalledWith("/p/pic.png");
    fireEvent.click(screen.getByTestId("artifact-chip-doc"));
    expect(openFile).toHaveBeenCalledWith("/p/doc.md");
  });

  it("布局/上下文变化不重新读取已加载图片（同一路径只请求一次）", async () => {
    const m = message({
      id: "m1",
      role: "system",
      kind: "imageGeneration",
      artifacts: [artifact({ artifactId: "img", path: "/p/live.png", imageIndexInTurn: 1 })],
      turnImageCount: 1,
    });
    const { rerender } = render(<Harness msg={m} openFile={() => {}} />);
    act(() => MockIntersectionObserver.triggerAll(true));
    await screen.findByTestId("artifact-image-img");
    expect(imageCalls()).toBe(1);

    // 父级重渲染（布局/摘要上下文变化）不得让卡片重新 attach、重新读盘
    const ctx = buildArtifactMessageContexts([m]).get("m1")!;
    rerender(<Harness msg={m} context={ctx} openFile={() => {}} />);
    expect(imageCalls()).toBe(1);
    expect(screen.getByTestId("artifact-image-img")).toBeTruthy();
  });

  it("13 张图片：渲染 12 张卡、汇总只出现一次、第 13 张不请求", async () => {
    const images = Array.from({ length: 13 }, (_, i) =>
      artifact({
        artifactId: `img-${i + 1}`,
        path: `/p/${i + 1}.png`,
        imageIndexInTurn: i + 1,
      })
    );
    const m = message({ id: "m1", turnImageCount: 13, artifacts: images });
    render(<Harness msg={m} openFile={() => {}} />);
    expect(screen.getAllByTestId(/^artifact-card-/).length).toBe(12);
    expect(screen.queryByTestId("artifact-card-img-13")).toBeNull();
    const overflow = screen.getAllByTestId("artifact-image-overflow");
    expect(overflow.length).toBe(1);
    expect(overflow[0].textContent).toContain("13");
    expect(overflow[0].textContent).toContain("12");
    // omittedArtifactCount=0：只有 UI 限额汇总，不得出现服务端预算省略提示
    expect(screen.queryByTestId("artifact-server-omitted")).toBeNull();

    act(() => MockIntersectionObserver.triggerAll(true));
    await waitFor(() => expect(imageCalls()).toBe(12));
    // 超额图片既无卡片也未发出读取
    expect(imageCalls()).toBe(12);
  });

  it("7 个 fileChange：至多 6 个 chip + 一条汇总 + 文件面板入口", () => {
    const openFile = vi.fn();
    const openPanel = vi.fn();
    const changes = Array.from({ length: 7 }, (_, i) =>
      artifact({
        artifactId: `chg-${i + 1}`,
        kind: "document",
        source: "file_change",
        path: `/p/f${i + 1}.ts`,
      })
    );
    const m = message({ id: "m1", artifacts: changes });
    render(<Harness msg={m} openFile={openFile} onOpenFilePanel={openPanel} />);
    expect(screen.getAllByTestId(/^artifact-chip-chg-/).length).toBe(MAX_FILE_CHANGE_CHIPS);
    expect(screen.queryByTestId("artifact-chip-chg-7")).toBeNull();
    const overflow = screen.getByTestId("artifact-chip-overflow");
    expect(overflow.textContent).toContain("1");
    fireEvent.click(screen.getByTestId("artifact-file-panel"));
    expect(openPanel).toHaveBeenCalledTimes(1);
    // 普通文档 chip 不参与 fileChange 6 个限额
    expect(MAX_FILE_CHANGE_CHIPS).toBe(6);
  });
});

describe("ProjectedArtifacts：服务端预算省略提示", () => {
  it("omittedArtifactCount>0：每条消息只出现一次消息级提示，不混入 UI 限额", () => {
    const m = message({
      id: "m1",
      artifacts: [
        artifact({ artifactId: "doc", kind: "document", source: "mention", path: "/p/a.md" }),
      ],
      omittedArtifactCount: 3,
    });
    render(<Harness msg={m} openFile={() => {}} />);
    const notes = screen.getAllByTestId("artifact-server-omitted");
    expect(notes).toHaveLength(1);
    expect(notes[0].textContent).toContain("3");
    expect(notes[0].textContent).toContain("服务端预算");
    // 与 UI 的 12 图 / 6 chip 展示上限分开：不冒充任何一条 UI 汇总
    expect(screen.queryByTestId("artifact-image-overflow")).toBeNull();
    expect(screen.queryByTestId("artifact-chip-overflow")).toBeNull();
  });

  it("附件全部被服务端省略（无任何附件可渲染）时提示仍然可见", () => {
    const m = message({ id: "m1", artifacts: [], omittedArtifactCount: 2 });
    render(<Harness msg={m} openFile={() => {}} />);
    const note = screen.getByTestId("artifact-server-omitted");
    expect(note.textContent).toContain("2");
  });

  it("UI 12 图上限与服务端省略同时成立：两条提示各自出现一次", () => {
    const images = Array.from({ length: 13 }, (_, i) =>
      artifact({
        artifactId: `img-${i + 1}`,
        path: `/p/${i + 1}.png`,
        imageIndexInTurn: i + 1,
      })
    );
    const m = message({
      id: "m1",
      turnImageCount: 13,
      artifacts: images,
      omittedArtifactCount: 2,
    });
    render(<Harness msg={m} openFile={() => {}} />);
    expect(screen.getAllByTestId("artifact-image-overflow")).toHaveLength(1);
    const notes = screen.getAllByTestId("artifact-server-omitted");
    expect(notes).toHaveLength(1);
    expect(notes[0].textContent).toContain("2");
  });
});

describe("ProjectedArtifacts：不可用引用与外链", () => {
  it("缺路径 / fileId / data URL / 其他 scheme / 超长均给可理解占位，且不渲染 base64", () => {
    const m = message({
      id: "m1",
      artifacts: [
        artifact({ artifactId: "missing", unavailableReason: "missing_path" }),
        artifact({ artifactId: "fileid", unavailableReason: "unresolved_file_id" }),
        artifact({ artifactId: "data", unavailableReason: "data_url", url: null }),
        artifact({
          artifactId: "scheme",
          kind: "external_image",
          source: "image_url",
          url: "ftp://example.com/a.png",
          unavailableReason: "unsupported_url_scheme",
        }),
        artifact({ artifactId: "plong", unavailableReason: "path_too_long" }),
        artifact({
          artifactId: "fklong",
          kind: "document",
          source: "mention",
          path: "/p/ok.md",
          unavailableReason: "file_key_too_long",
        }),
        artifact({
          artifactId: "nourl",
          kind: "external_image",
          source: "image_url",
          unavailableReason: "missing_url",
        }),
        artifact({
          artifactId: "urllong",
          kind: "external_image",
          source: "image_url",
          url: "https://example.com/" + "a".repeat(10),
          unavailableReason: "url_too_long",
        }),
        artifact({ artifactId: "unknown", unavailableReason: "some_future_code" }),
      ],
    });
    const openFile = vi.fn();
    const { container } = render(<Harness msg={m} openFile={openFile} />);
    expect(screen.getByTestId("artifact-placeholder-missing").textContent).toContain(
      "缺少文件路径"
    );
    expect(screen.getByTestId("artifact-placeholder-fileid").textContent).toContain("文件标识");
    expect(screen.getByTestId("artifact-placeholder-data").textContent).toContain("内嵌图片");
    expect(screen.getByTestId("artifact-placeholder-scheme").textContent).toContain("HTTP(S)");
    expect(screen.getByTestId("artifact-placeholder-plong").textContent).toContain("路径过长");
    expect(screen.getByTestId("artifact-placeholder-nourl").textContent).toContain("缺少链接地址");
    expect(screen.getByTestId("artifact-placeholder-urllong").textContent).toContain("链接过长");
    expect(screen.getByTestId("artifact-placeholder-unknown").textContent).toContain("无法预览");
    expect(container.textContent).not.toContain("data:");
    // fileKey 超长但 path 仍在：仍显示 chip 且可按路径打开
    expect(screen.queryByTestId("artifact-placeholder-fklong")).toBeNull();
    fireEvent.click(screen.getByTestId("artifact-chip-fklong"));
    expect(openFile).toHaveBeenCalledWith("/p/ok.md");
  });

  it("外链仅 HTTP(S)，沿用 target=_blank + rel=noreferrer；不做任何下载", () => {
    const m = message({
      id: "m1",
      role: "system",
      kind: "userMessage",
      artifacts: [
        artifact({
          artifactId: "ext-ok",
          kind: "external_image",
          source: "image_url",
          url: "https://example.com/a.png",
        }),
      ],
    });
    render(<Harness msg={m} openFile={() => {}} />);
    const link = screen.getByTestId("artifact-external-ext-ok") as HTMLAnchorElement;
    expect(link.getAttribute("href")).toBe("https://example.com/a.png");
    expect(link.getAttribute("target")).toBe("_blank");
    expect(link.getAttribute("rel")).toBe("noreferrer");
    expect(fetchMock).not.toHaveBeenCalled();
  });
});

describe("ProjectedArtifacts：读取失败与解码失败占位", () => {
  it("敏感路径 / 文件不存在 / 超限 → 可理解占位", async () => {
    const reasons = [
      { error: "sensitive", text: "安全策略保护" },
      { error: "not_found", text: "不存在或已被移动" },
      { error: "too_large", text: "超过预览上限" },
    ];
    for (const r of reasons) {
      fetchMock.mockImplementation(
        async () =>
          new Response(JSON.stringify({ error: r.error }), {
            status: 403,
            headers: { "content-type": "application/json" },
          })
      );
      const m = message({
        id: `m-${r.error}`,
        role: "system",
        kind: "imageGeneration",
        artifacts: [
          artifact({
            artifactId: `img-${r.error}`,
            path: `/p/${r.error}.png`,
            imageIndexInTurn: 1,
          }),
        ],
        turnImageCount: 1,
      });
      const { unmount } = render(<Harness msg={m} openFile={() => {}} />);
      act(() => MockIntersectionObserver.triggerAll(true));
      const ph = await screen.findByTestId(`artifact-placeholder-img-${r.error}`);
      expect(ph.textContent).toContain(r.text);
      unmount();
      MockIntersectionObserver.reset();
      cleanup();
    }
  });

  it("图片解码失败 → 占位并回收 object URL", async () => {
    const m = message({
      id: "m1",
      role: "system",
      kind: "imageGeneration",
      artifacts: [artifact({ artifactId: "img", path: "/p/bad.png", imageIndexInTurn: 1 })],
      turnImageCount: 1,
    });
    render(<Harness msg={m} openFile={() => {}} />);
    act(() => MockIntersectionObserver.triggerAll(true));
    const img = await screen.findByTestId("artifact-image-img");
    fireEvent.error(img);
    expect(revokeObjectURL).toHaveBeenCalled();
    const ph = await screen.findByTestId("artifact-placeholder-img");
    expect(ph.textContent).toContain("无法解码");
  });
});
