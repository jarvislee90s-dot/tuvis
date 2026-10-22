// 任务 7：文件身份关联与五页逆向定位的纯函数矩阵。
//
// 覆盖：
// - `findLastFileReference`：单页内从新到旧、同消息取最后一个节点；结构化附件
//   （fileKey）与既有链接器可识别的正文链接（经已知条目映射 fileKey）都算引用；
//   不从正文抽新路径、不按 basename 猜身份、外链不参与本地匹配。
// - `buildArtifactFileView`：按 fileKey（旧载荷退化为 path）合并后端条目与缺失
//   附件；已有条目来源优先级与 modified 原样保留（弱来源不覆盖、写标志不撤销）；
//   缺失条目按 spec §4.4 映射来源且绝不谎报 modified；外链不进本地池；排序按
//   最近引用倒序、无引用条目保持后端相对顺序；不把 IPC 顺序写成 lastSeq。
import { describe, expect, it } from "vitest";
import { buildArtifactFileView, findLastFileReference } from "@/mobile/artifact-links";
import { capArtifactSummary } from "@/mobile/ProjectedArtifacts";
import type { SessionFileEntry } from "@/mobile/api";
import type { ProjectedArtifact, ProjectedMessage } from "@/mobile/codexIpcApi";

function entry(path: string, over: Partial<SessionFileEntry> = {}): SessionFileEntry {
  return {
    path,
    fileKey: `key:${path}`,
    lastSeq: 1,
    lastTs: null,
    hits: 1,
    modified: false,
    origin: "user",
    ...over,
  };
}

function artifact(
  over: Partial<ProjectedArtifact> & Pick<ProjectedArtifact, "artifactId">
): ProjectedArtifact {
  return {
    kind: "document",
    source: "mention",
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
  };
}

function msg(id: string, over: Partial<ProjectedMessage> = {}): ProjectedMessage {
  return { id, role: "user", kind: "userMessage", text: "", ts: null, ...over };
}

function target(path: string, fileKey: string | null = null) {
  return { path, fileKey };
}

describe("findLastFileReference：单页逆序匹配", () => {
  it("仅正文链接（无结构化附件）也能命中", () => {
    const entries = [entry("/tmp/proj/notes.md")];
    const page = [msg("m1", { text: "请查看 /tmp/proj/notes.md 的内容" })];
    expect(
      findLastFileReference(page, target("/tmp/proj/notes.md", "key:/tmp/proj/notes.md"), entries)
    ).toEqual({
      messageId: "m1",
      via: "body",
      artifactId: null,
      path: "/tmp/proj/notes.md",
      nodeIndex: 0,
    });
  });

  it("同文件多次引用：取最近消息（页内从新到旧）", () => {
    const entries = [entry("/tmp/proj/a.md")];
    const page = [
      msg("m1", { text: "/tmp/proj/a.md" }),
      msg("m2", { text: "无关内容" }),
      msg("m3", { text: "再看 /tmp/proj/a.md" }),
    ];
    expect(findLastFileReference(page, target("/tmp/proj/a.md"), entries)?.messageId).toBe("m3");
  });

  it("代码中的路径不是可点击正文引用：行内和围栏代码不能遮蔽较早命中", () => {
    const entries = [entry("/tmp/proj/a.md")];
    const page = [
      msg("m1", { text: "请打开 /tmp/proj/a.md" }),
      msg("m2", {
        text: "示例：`/tmp/proj/a.md`\n\n```text\n/tmp/proj/a.md\n```",
      }),
    ];
    expect(findLastFileReference(page, target("/tmp/proj/a.md"), entries)?.messageId).toBe("m1");
  });

  it("仅出现在 Markdown 代码里的路径不构成正文引用", () => {
    const entries = [entry("/tmp/proj/a.md")];
    const page = [
      msg("m1", {
        text: "行内：`/tmp/proj/a.md`\n\n~~~text\n/tmp/proj/a.md\n~~~",
      }),
    ];
    expect(findLastFileReference(page, target("/tmp/proj/a.md"), entries)).toBeNull();
  });

  it("非 Markdown 消息仍按纯文本链接规则识别反引号与缩进内容", () => {
    const entries = [entry("/tmp/proj/a.md")];
    const page = [msg("m1", { role: "system", text: "路径：`/tmp/proj/a.md`" })];
    expect(findLastFileReference(page, target("/tmp/proj/a.md"), entries)?.messageId).toBe("m1");
  });

  it("代码跨度中的反斜杠不转义其闭合反引号", () => {
    const entries = [entry("/tmp/proj/a.md")];
    const inlineCode = "`/tmp/proj/a.md\\`";
    expect(
      findLastFileReference([msg("m1", { text: inlineCode })], target("/tmp/proj/a.md"), entries)
    ).toBeNull();
  });

  it("未闭合反引号不能跨段落吞掉真实路径", () => {
    const entries = [entry("/tmp/proj/a.md")];
    const page = [msg("m1", { text: "`未闭合\n\n/tmp/proj/a.md\n\n`之后" })];
    expect(findLastFileReference(page, target("/tmp/proj/a.md"), entries)?.messageId).toBe("m1");
  });

  it("引用块内较长闭合围栏中的路径仍不是可点击引用", () => {
    const entries = [entry("/tmp/proj/a.md")];
    const text = "> ```text\n> /tmp/proj/a.md\n> ````";
    expect(
      findLastFileReference([msg("m1", { text })], target("/tmp/proj/a.md"), entries)
    ).toBeNull();
  });

  it("引用式 Markdown 外链中的文件名不是文件预览链接", () => {
    const entries = [entry("/tmp/proj/a.md")];
    const text = "[/tmp/proj/a.md][doc]\n\n[doc]: https://example.com";
    expect(
      findLastFileReference([msg("m1", { text })], target("/tmp/proj/a.md"), entries)
    ).toBeNull();
  });

  it("同消息多处正文引用：取最后一个对应节点", () => {
    const entries = [entry("/tmp/proj/a.md"), entry("/tmp/proj/b.md")];
    const page = [msg("m1", { text: "/tmp/proj/a.md 与 /tmp/proj/b.md，再看 /tmp/proj/a.md" })];
    const hit = findLastFileReference(page, target("/tmp/proj/a.md"), entries);
    expect(hit?.messageId).toBe("m1");
    expect(hit?.nodeIndex).toBe(2); // 第三个引用节点（a、b、a）
  });

  it("结构化附件按 fileKey 命中（正文无路径）", () => {
    const entries = [entry("/tmp/proj/abs.md", { fileKey: "fk-1" })];
    const page = [
      msg("m1", {
        text: "",
        artifacts: [
          artifact({
            artifactId: "doc-1",
            kind: "document",
            source: "mention",
            path: "../proj/abs.md",
            fileKey: "fk-1",
          }),
        ],
      }),
    ];
    const hit = findLastFileReference(page, target("/tmp/proj/abs.md", "fk-1"), entries);
    expect(hit).toEqual({
      messageId: "m1",
      via: "artifact",
      artifactId: "doc-1",
      path: "../proj/abs.md",
      nodeIndex: 0,
    });
  });

  it("同消息既有正文链接又有匹配附件：附件节点在后，取附件", () => {
    const entries = [entry("/tmp/proj/a.md", { fileKey: "fk-a" })];
    const page = [
      msg("m1", {
        text: "看 /tmp/proj/a.md",
        artifacts: [
          artifact({
            artifactId: "doc-a",
            kind: "document",
            source: "mention",
            path: "/tmp/proj/a.md",
            fileKey: "fk-a",
          }),
        ],
      }),
    ];
    const hit = findLastFileReference(page, target("/tmp/proj/a.md", "fk-a"), entries);
    expect(hit?.via).toBe("artifact");
    expect(hit?.artifactId).toBe("doc-a");
    expect(hit?.nodeIndex).toBeGreaterThan(0);
  });

  it("旧载荷条目缺 fileKey：退化为 path 匹配（不猜别名）", () => {
    const entries = [{ ...entry("/tmp/proj/legacy.md"), fileKey: undefined }];
    const page = [msg("m1", { text: "/tmp/proj/legacy.md" })];
    const hit = findLastFileReference(page, target("/tmp/proj/legacy.md", "fk-new"), entries);
    expect(hit).toMatchObject({ messageId: "m1", via: "body", path: "/tmp/proj/legacy.md" });
  });

  it("不按 basename 推断：同名不同目录不命中", () => {
    const entries = [entry("/tmp/proj/notes.md")];
    const page = [msg("m1", { text: "/tmp/other/notes.md" })];
    expect(findLastFileReference(page, target("/tmp/proj/notes.md"), entries)).toBeNull();
  });

  it("外链附件不参与本地匹配", () => {
    const entries = [entry("/tmp/proj/a.png")];
    const page = [
      msg("m1", {
        artifacts: [
          artifact({
            artifactId: "ext-1",
            kind: "external_image",
            source: "image_url",
            path: "/tmp/proj/a.png",
            url: "https://example.com/a.png",
            fileKey: "key:/tmp/proj/a.png",
          }),
        ],
      }),
    ];
    expect(findLastFileReference(page, target("/tmp/proj/a.png"), entries)).toBeNull();
  });

  it("目标不在已知条目且正文只出现路径：不从正文抽新文件 → null", () => {
    const entries = [entry("/tmp/proj/known.md")];
    const page = [msg("m1", { text: "/tmp/proj/unknown.md" })];
    expect(findLastFileReference(page, target("/tmp/proj/unknown.md"), entries)).toBeNull();
  });

  it("空页 / 无引用 → null", () => {
    expect(findLastFileReference([], target("/tmp/proj/a.md"), [])).toBeNull();
    expect(
      findLastFileReference([msg("m1", { text: "无关" })], target("/tmp/proj/a.md"), [])
    ).toBeNull();
  });
});

describe("buildArtifactFileView：来源合并", () => {
  it("同一 fileKey 的附件与既有条目只显示一次，已有来源原样保留", () => {
    const entries = [entry("/tmp/proj/a.md", { origin: "user", modified: false })];
    const messages = [
      msg("m1", {
        artifacts: [
          artifact({
            artifactId: "doc-1",
            kind: "document",
            source: "mention",
            path: "/tmp/proj/a.md",
            fileKey: "key:/tmp/proj/a.md",
          }),
        ],
      }),
    ];
    const view = buildArtifactFileView(entries, messages);
    expect(view).toHaveLength(1);
    expect(view[0].path).toBe("/tmp/proj/a.md");
    expect(view[0].origin).toBe("user");
    expect(view[0].modified).toBe(false);
    expect(view[0].artifactSource).toBe("mention");
    expect(view[0].lastReference?.messageId).toBe("m1");
  });

  it("已有条目：file_change 不提升写来源、不撤销已修改标志", () => {
    const entries = [
      entry("/tmp/proj/read.md", { origin: "tool_read", modified: false }),
      entry("/tmp/proj/written.md", { origin: "tool_write", modified: true }),
      entry("/tmp/proj/uploaded.md", { origin: "user", modified: false }),
    ];
    const messages = [
      msg("m1", {
        artifacts: [
          artifact({
            artifactId: "chg-1",
            source: "file_change",
            path: "/tmp/proj/read.md",
            fileKey: "key:/tmp/proj/read.md",
            sourceStatus: "in_progress",
            changeKind: "modify",
          }),
          artifact({
            artifactId: "view-1",
            source: "image_view",
            path: "/tmp/proj/written.md",
            fileKey: "key:/tmp/proj/written.md",
          }),
          artifact({
            artifactId: "chg-2",
            source: "file_change",
            path: "/tmp/proj/uploaded.md",
            fileKey: "key:/tmp/proj/uploaded.md",
            sourceStatus: "failed",
          }),
        ],
      }),
    ];
    const view = buildArtifactFileView(entries, messages);
    const read = view.find((e) => e.path === "/tmp/proj/read.md");
    const written = view.find((e) => e.path === "/tmp/proj/written.md");
    const uploaded = view.find((e) => e.path === "/tmp/proj/uploaded.md");
    expect(read?.origin).toBe("tool_read");
    expect(read?.modified).toBe(false);
    // 原始状态标签保留（不翻译、不当成成功）
    expect(read?.sourceStatus).toBe("in_progress");
    expect(read?.changeKind).toBe("modify");
    expect(written?.origin).toBe("tool_write");
    expect(written?.modified).toBe(true);
    // 既有 user 条目：未经验证的 file_change 引用不升级写来源、不置 modified
    expect(uploaded?.origin).toBe("user");
    expect(uploaded?.modified).toBe(false);
    expect(uploaded?.sourceStatus).toBe("failed");
  });

  it("既有 tool_write/modified 条目：未验证的 file_change 引用不得撤销已修改标志", () => {
    const entries = [entry("/tmp/proj/written.md", { origin: "tool_write", modified: true })];
    const messages = [
      msg("m1", {
        artifacts: [
          artifact({
            artifactId: "chg-failed",
            source: "file_change",
            path: "/tmp/proj/written.md",
            fileKey: "key:/tmp/proj/written.md",
            sourceStatus: "failed",
            changeKind: "modify",
          }),
        ],
      }),
    ];
    const view = buildArtifactFileView(entries, messages);
    expect(view).toHaveLength(1);
    expect(view[0].origin).toBe("tool_write");
    expect(view[0].modified).toBe(true); // 后端既有证据不被前端撤销
    expect(view[0].sourceStatus).toBe("failed");
    expect(view[0].changeKind).toBe("modify");
  });

  it("失败的修改引用落在中性来源、modified=false（不进入工具读写池）", () => {
    const messages = [
      msg("m1", {
        artifacts: [
          artifact({
            artifactId: "chg-failed",
            source: "file_change",
            path: "/tmp/proj/exec-1.png",
            fileKey: "fk-exec",
            sourceStatus: "failed",
          }),
        ],
      }),
    ];
    const view = buildArtifactFileView([], messages);
    expect(view).toHaveLength(1);
    // 批准计划任务 7：只有完成证据才提升写来源——failed 不得进入「工具读写」池
    expect(view[0].origin).toBe("session_artifact");
    expect(view[0].modified).toBe(false);
    // 原始状态如实展示，信息不丢
    expect(view[0].artifactSource).toBe("file_change");
    expect(view[0].sourceStatus).toBe("failed");
  });

  it("补充条目：来源映射；未验证的 file_change 不标已修改（不凭来源虚构成功）", () => {
    const messages = [
      msg("m1", {
        artifacts: [
          artifact({
            artifactId: "up-1",
            kind: "image",
            source: "local_image",
            path: "/tmp/proj/up.png",
            fileKey: "fk-up",
          }),
          artifact({
            artifactId: "view-1",
            source: "image_view",
            path: "/tmp/proj/view.md",
            fileKey: "fk-view",
          }),
          artifact({
            artifactId: "chg-1",
            source: "file_change",
            path: "/tmp/proj/edit.md",
            fileKey: "fk-edit",
            sourceStatus: "failed",
            changeKind: "add",
          }),
          artifact({
            artifactId: "gen-1",
            kind: "image",
            source: "image_generation",
            path: "/tmp/proj/gen.png",
            fileKey: "fk-gen",
          }),
        ],
      }),
    ];
    const view = buildArtifactFileView([], messages);
    const byPath = new Map(view.map((e) => [e.path, e]));
    expect(byPath.get("/tmp/proj/up.png")?.origin).toBe("user");
    expect(byPath.get("/tmp/proj/view.md")?.origin).toBe("tool_read");
    // 修改类引用没有完成证据 → 中性来源（不是 tool_write），modified 不变量仍成立
    expect(byPath.get("/tmp/proj/edit.md")?.origin).toBe("session_artifact");
    expect(byPath.get("/tmp/proj/gen.png")?.origin).toBe("session_artifact");
    // 状态/变更类型仍按原始标量展示（failed 不当成成功）
    expect(byPath.get("/tmp/proj/edit.md")?.modified).toBe(false);
    for (const path of ["up.png", "view.md", "gen.png"]) {
      expect(byPath.get(`/tmp/proj/${path}`)?.modified).toBe(false);
    }
    expect(byPath.get("/tmp/proj/edit.md")?.sourceStatus).toBe("failed");
    expect(byPath.get("/tmp/proj/edit.md")?.changeKind).toBe("add");
  });

  it("外链（image_url / external_image）不进入本地文件池", () => {
    const messages = [
      msg("m1", {
        artifacts: [
          artifact({
            artifactId: "ext-1",
            kind: "external_image",
            source: "image_url",
            url: "https://example.com/a.png",
          }),
          artifact({
            artifactId: "ext-2",
            kind: "external_image",
            source: "image_url",
            path: "/tmp/proj/should-not-enter.png",
            url: "https://example.com/b.png",
          }),
        ],
      }),
    ];
    expect(buildArtifactFileView([], messages)).toHaveLength(0);
  });

  it("弱来源不覆盖强来源：同一缺失文件的多次引用取最强**有证据**来源", () => {
    const messages = [
      msg("m1", {
        artifacts: [
          artifact({
            artifactId: "v1",
            source: "image_view",
            path: "/tmp/proj/x.md",
            fileKey: "fk-x",
          }),
        ],
      }),
      msg("m2", {
        artifacts: [
          artifact({
            artifactId: "c1",
            source: "file_change",
            path: "/tmp/proj/x.md",
            fileKey: "fk-x",
          }),
        ],
      }),
      msg("m3", {
        artifacts: [
          artifact({
            artifactId: "v2",
            source: "image_view",
            path: "/tmp/proj/x.md",
            fileKey: "fk-x",
          }),
        ],
      }),
    ];
    const view = buildArtifactFileView([], messages);
    expect(view).toHaveLength(1);
    // file_change 无完成证据 → 不参与来源池竞争；查看引用（tool_read）胜出
    expect(view[0].origin).toBe("tool_read");
    expect(view[0].modified).toBe(false);
    // 来源标签跟随最近引用（m3 的查看）
    expect(view[0].artifactSource).toBe("image_view");
    expect(view[0].lastReference?.messageId).toBe("m3");
  });

  it("混合键：较新的无键引用不抹掉组内已知 fileKey（MINOR 1）", () => {
    const messages = [
      msg("m1", {
        artifacts: [
          artifact({
            artifactId: "doc-keyed",
            source: "mention",
            path: "/tmp/proj/mixed.md",
            fileKey: "fk-known",
          }),
        ],
      }),
      msg("m2", {
        artifacts: [
          artifact({
            artifactId: "doc-keyless",
            source: "mention",
            path: "/tmp/proj/mixed.md",
            fileKey: null,
          }),
        ],
      }),
    ];
    const view = buildArtifactFileView([], messages);
    expect(view).toHaveLength(1);
    expect(view[0].path).toBe("/tmp/proj/mixed.md");
    expect(view[0].fileKey).toBe("fk-known"); // 不因最新引用缺键而降级为 path-only
    expect(view[0].lastReference?.messageId).toBe("m2");
  });

  it("旧载荷无 fileKey 时按 path 合并；不同目录同名不合并", () => {
    const entries = [{ ...entry("/tmp/proj/a.md"), fileKey: undefined }];
    const messages = [
      msg("m1", {
        artifacts: [
          artifact({ artifactId: "doc-1", path: "/tmp/proj/a.md", fileKey: null }),
          artifact({ artifactId: "doc-2", path: "/tmp/other/a.md", fileKey: null }),
        ],
      }),
    ];
    const view = buildArtifactFileView(entries, messages);
    expect(view.map((e) => e.path)).toEqual(["/tmp/other/a.md", "/tmp/proj/a.md"]);
  });

  it("旧载荷条目缺 key：带 key 的附件按同一 path 并入该条目（不新增重复行）", () => {
    const entries = [{ ...entry("/tmp/proj/a.md"), fileKey: undefined }];
    const messages = [
      msg("m1", {
        artifacts: [
          artifact({
            artifactId: "doc-1",
            source: "image_view",
            path: "/tmp/proj/a.md",
            fileKey: "fk-new",
          }),
        ],
      }),
    ];
    const view = buildArtifactFileView(entries, messages);
    expect(view).toHaveLength(1);
    expect(view[0].origin).toBe("user"); // 既有来源保持
    expect(view[0].artifactSource).toBe("image_view");
  });

  it("同一 path 但两侧 fileKey 不同：不猜测同一身份（分别成行）", () => {
    const entries = [entry("/tmp/proj/x.md", { fileKey: "fk-a" })];
    const messages = [
      msg("m1", {
        artifacts: [
          artifact({
            artifactId: "doc-1",
            source: "mention",
            path: "/tmp/proj/x.md",
            fileKey: "fk-b",
          }),
        ],
      }),
    ];
    const view = buildArtifactFileView(entries, messages);
    expect(view).toHaveLength(2);
    expect(view.map((e) => e.fileKey).sort()).toEqual(["fk-a", "fk-b"]);
  });
});

describe("buildArtifactFileView：来源摘要（R5）", () => {
  const prompt = "请根据这张照片生成一张十六比九的超宽幅电影感海报封面";

  it("同回合、附件前的最近非空用户消息优先，并截断到 24 字", () => {
    const messages = [
      msg("u1", { text: "更早的同回合提示词", turnId: "t1" }),
      msg("u2", { text: "   ", turnId: "t1" }),
      msg("u3", { text: prompt, turnId: "t1" }),
      msg("a1", {
        role: "system",
        kind: "imageGeneration",
        turnId: "t1",
        artifacts: [
          artifact({
            artifactId: "gen-1",
            kind: "image",
            source: "image_generation",
            path: "/tmp/proj/exec-1.png",
            fileKey: "fk-gen",
            summary: "附件摘要不应覆盖同回合提示词",
          }),
        ],
      }),
    ];
    const view = buildArtifactFileView([], messages);
    expect(view).toHaveLength(1);
    expect(view[0].summary).toBe(capArtifactSummary(prompt));
    expect(Array.from(view[0].summary ?? "")).toHaveLength(25); // 24 字 + 省略号
    expect(view[0].summary?.endsWith("…")).toBe(true);
  });

  it("同回合没有非空用户消息：回退附件自身摘要（也受 24 字上限）", () => {
    const longSummary = "附件自身的来源摘要也可能超过二十四个字符需要被截断";
    const messages = [
      msg("a1", {
        role: "system",
        kind: "imageView",
        turnId: "t1",
        artifacts: [
          artifact({
            artifactId: "view-1",
            source: "image_view",
            path: "/tmp/proj/seen.md",
            fileKey: "fk-view",
            summary: longSummary,
          }),
        ],
      }),
    ];
    const view = buildArtifactFileView([], messages);
    expect(view[0].summary).toBe(capArtifactSummary(longSummary));
    expect(Array.from(view[0].summary ?? "")).toHaveLength(25); // 24 字 + 省略号
  });

  it("不借下一回合的提示词：后续用户消息不影响已出现的附件", () => {
    const messages = [
      msg("a1", {
        role: "system",
        kind: "imageGeneration",
        turnId: "t1",
        artifacts: [
          artifact({
            artifactId: "gen-1",
            kind: "image",
            source: "image_generation",
            path: "/tmp/proj/exec-1.png",
            fileKey: "fk-gen",
            summary: "仅附件摘要",
          }),
        ],
      }),
      msg("u2", { text: "下一回合提示词", turnId: "t2" }),
    ];
    expect(buildArtifactFileView([], messages)[0].summary).toBe("仅附件摘要");
  });

  it("无引用证据的后端行保持 summary=null", () => {
    const view = buildArtifactFileView([entry("/tmp/proj/a.md")], []);
    expect(view[0].summary).toBeNull();
  });
});

describe("buildArtifactFileView：排序与 lastSeq", () => {
  it("本次范围内有引用证据的条目按最近引用倒序，其余保持后端相对顺序", () => {
    const entries = [
      entry("/tmp/proj/a.md"),
      entry("/tmp/proj/b.md"),
      entry("/tmp/proj/c.md"),
      entry("/tmp/proj/no-ref.md"),
    ];
    const messages = [
      msg("m1", { text: "/tmp/proj/a.md" }),
      msg("m2", { text: "/tmp/proj/b.md" }),
      msg("m3", { text: "/tmp/proj/c.md" }),
    ];
    const view = buildArtifactFileView(entries, messages);
    expect(view.map((e) => e.path)).toEqual([
      "/tmp/proj/c.md",
      "/tmp/proj/b.md",
      "/tmp/proj/a.md",
      "/tmp/proj/no-ref.md",
    ]);
  });

  it("补充条目参与引用排序；同日消息内按节点顺序（附件在后）", () => {
    const entries = [entry("/tmp/proj/backend.md")];
    const messages = [
      msg("m1", { text: "/tmp/proj/backend.md" }),
      msg("m2", {
        text: "看 /tmp/proj/backend.md",
        artifacts: [
          artifact({
            artifactId: "doc-a",
            source: "mention",
            path: "/tmp/proj/added.md",
            fileKey: "fk-added",
          }),
        ],
      }),
    ];
    const view = buildArtifactFileView(entries, messages);
    // m2 同时引用 backend（正文）与 added（附件节点在后）→ added 最近
    expect(view.map((e) => e.path)).toEqual(["/tmp/proj/added.md", "/tmp/proj/backend.md"]);
  });

  it("不把 IPC 消息顺序编码成 lastSeq，也不用负数占位", () => {
    const entries = [entry("/tmp/proj/backend.md", { lastSeq: 42 })];
    const messages = [msg("m1"), msg("m2"), msg("m3")];
    const messagesWithArtifact = [
      ...messages,
      msg("m4", {
        artifacts: [
          artifact({
            artifactId: "doc-new",
            source: "mention",
            path: "/tmp/proj/added.md",
            fileKey: "fk-added",
          }),
        ],
      }),
    ];
    const view = buildArtifactFileView(entries, messagesWithArtifact);
    const added = view.find((e) => e.path === "/tmp/proj/added.md");
    expect(added).toBeTruthy();
    expect(added?.lastSeq).toBe(0);
    expect(added?.lastSeq).toBeGreaterThanOrEqual(0);
    // 既有条目 lastSeq 不被改写
    expect(view.find((e) => e.path === "/tmp/proj/backend.md")?.lastSeq).toBe(42);
  });

  it("空消息范围：条目原样、相对顺序不变（其他工具/无引用场景零变化）", () => {
    const entries = [entry("/p/a.md"), entry("/p/b.md"), entry("/p/c.md")];
    const view = buildArtifactFileView(entries, []);
    expect(view.map((e) => e.path)).toEqual(["/p/a.md", "/p/b.md", "/p/c.md"]);
    expect(view.every((e) => e.lastReference === null)).toBe(true);
  });
});
