# 文件面板「会话产物」第四来源批次 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 远程端文件面板新增第四来源分区「会话产物」（`session_artifact`）——脚本/命令生成的产物路径只出现在对话正文（markdown 链接 / 行内反引号）里、tool-call 参数没有它时，也能上墙并可预览。

**Architecture:** 后端 `files.rs` 新增正文候选扫描器（纯函数）→ `EntryMerger` 兜底合并（rank 最低，既有三池零变化）→ 存在性过滤（注入谓词）；`content.rs` 的 `MessagesPage` 增加可选 `cwd` 字段（codex rollout 首行 session_meta 已有，零额外 IO），供相对路径解析。前端仅 `api.ts` 类型 + `FilePanel.tsx` 两处文案数组 + 测试断言。

**Tech Stack:** Rust（serde / 纯函数 + 注入缝测试纪律）/ React 19 + vitest + testing-library。

**已定案裁决（2026-10-13 grilling，不得偏离）：**
1. 提取形态 = markdown 链接 `[文字](路径)` + 行内反引号 `` `路径` ``；三反引号代码围栏**不收**；assistant 与 user 正文都扫。
2. 路径范围 = 绝对路径 + 相对会话 cwd 的相对路径。
3. 类型 = 可预览白名单（mime 表已知扩展 + 补注册 html/htm）；pdf/xlsx 等暂不上。
4. 来源 = 新池 `session_artifact`（中文「会话产物」），**rank 最低只兜底**——工具触达过的文件永远显示原标签，且 hits/lastSeq 不被正文提及干扰。
5. 新池条目**提取时校验存在**（不存在的丢弃）；工具触达条目维持不校验。
6. 既有三池合并结果零变化（回归红线）。

**黄金案例（测试形态来源，不得触真实数据目录——测试一律用合成等价形态）：** codex 会话 01a13353 中，报告 md 由 `exec_command` 跑 python 脚本生成（参数无路径），事后 assistant 正文以 `[报告](C:\…md)` 与 `` `C:\…md` `` 两种形态给出路径；SKILL.md 由 apply_patch 写（补丁头行带路径，既有提取面）。

---

### Task 1: mime 表补 html/htm + 产物扩展名白名单

**Files:**
- Modify: `src-tauri/src/remote/files.rs`（`mime_from_ext` 约 :444；`PATH_KEYS` 常量区附近加白名单）

- [ ] **Step 1: 写失败测试**

在 `files.rs` 的 `mod tests` 中既有 `mime_from_ext_covers_brief_list_and_falls_back` 用例内追加两行断言，并新增白名单纯函数测试：

```rust
        // 会话产物批（2026-10-13）：html/htm 注册 text/html（前端预览器已有
        // html 沙箱渲染分支，后端 mime 表此前漏注册）
        assert_eq!(mime_from_ext("html"), "text/html");
        assert_eq!(mime_from_ext("htm"), "text/html");
```

```rust
    /// 产物扩展名白名单（纯函数）：可预览类放行、二进制交换格式与无扩展名拒绝、
    /// 大小写不敏感（Windows 实测形态 .MD）
    #[test]
    fn artifact_ext_allowlist_filters_previewable_kinds_only() {
        assert!(artifact_ext_allowed("C:/u/report.md"));
        assert!(artifact_ext_allowed("C:/u/chart.svg"));
        assert!(artifact_ext_allowed("C:/u/data.csv"));
        assert!(artifact_ext_allowed("C:/u/page.HTML"), "大小写不敏感");
        assert!(!artifact_ext_allowed("C:/u/report.pdf"), "二进制交换格式暂不上");
        assert!(!artifact_ext_allowed("C:/u/archive.xlsx"));
        assert!(!artifact_ext_allowed("C:/u/noext"), "无扩展名不是产物候选");
        assert!(!artifact_ext_allowed("https://x.com/a.md"), "URL 候选由 is_path_candidate 排除，白名单双保险");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test --lib remote::files`
Expected: FAIL（`mime_from_ext("html")` 返回 `text/plain`；`artifact_ext_allowed` 未定义编译错）

- [ ] **Step 3: 最小实现**

`mime_from_ext` 的 match 臂在 `"md"` 后加：

```rust
        "html" | "htm" => "text/html",
```

`PATH_KEYS` 常量后新增：

```rust
/// 「会话产物」上墙扩展名白名单（2026-10-13 用户裁决：可预览优先——后端 mime
/// 表可判定的文本/图片类 + html；pdf/xlsx/docx 等二进制交换格式暂不上墙，
/// 列为后续可选）。仅作用于会话产物池；工具触达条目不做类型过滤
const ARTIFACT_EXT_ALLOWLIST: &[&str] = &[
    "md", "markdown", "txt", "log", "json", "csv", "tsv", "yaml", "yml", "toml", "xml",
    "html", "htm",
    "rs", "ts", "tsx", "js", "jsx", "py", "go", "java", "c", "h", "cpp", "css", "sh",
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "svg", "ico",
];

/// 产物候选的扩展名白名单判定（纯函数）：末段扩展名（大小写不敏感）命中即放行
fn artifact_ext_allowed(path: &str) -> bool {
    let last = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match last.rsplit_once('.') {
        Some((stem, ext)) => {
            !stem.is_empty() && ARTIFACT_EXT_ALLOWLIST.contains(&ext.to_ascii_lowercase().as_str())
        }
        None => false,
    }
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test --lib remote::files`
Expected: PASS（全部既有用例不红）

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/remote/files.rs
git commit -m "feat(remote-files): mime 表补 html/htm + 会话产物扩展名白名单"
```

**验收条件：** `cargo test --lib remote::files` 全绿；`artifact_ext_allowed` 对 md/svg/csv/大写扩展放行、pdf/xlsx/无扩展名/URL 拒绝；`mime_from_ext("html") == "text/html"`。

---

### Task 2: 正文产物候选扫描器（纯函数）

**Files:**
- Modify: `src-tauri/src/remote/files.rs`（`extract_inline_markup_paths` 之后加扫描器）

- [ ] **Step 1: 写失败测试**

```rust
    /// 会话产物候选扫描（黄金案例合成等价形态）：① markdown 链接与行内反引号
    /// 两种定界形态都收（assistant 与 user 正文同权）；② 三反引号围栏内不收；
    /// ③ URL/锚点/白名单外扩展排除；④ tool-call 消息不参与（其路径走既有六键面）
    #[test]
    fn artifact_candidates_scan_links_inline_backticks_and_skip_fences() {
        // ① markdown 链接（绝对路径）
        let link = text_msg("assistant", "文件在 [报告](C:/Users/u/Desktop/公司/报告.md) 里。");
        // ② 行内反引号
        let tick = text_msg("assistant", "在这里：`C:/Users/u/Desktop/公司/报告.md` 打开即可。");
        // 围栏内路径样串不收（同一条消息里围栏外的反引号路径照收）
        let fenced = text_msg(
            "assistant",
            "命令如下：\n```bash\ncat /tmp/proj/leak.md\n```\n产物在 `out/summary.json`。",
        );
        // URL 与锚点排除
        let url = text_msg("assistant", "见 [文档](https://example.com/a.md) 与 [章节](#intro)");
        // 白名单外扩展
        let pdf = text_msg("assistant", "报告在 [原件](C:/u/原件.pdf)");
        // user 正文同权
        let user = text_msg("user", "结果在 [输出](./out/data.json)");
        // tool-call 消息不参与
        let tool = tool_call(r#"{"cmd":"cat /tmp/proj/leak.md"}"#);

        let paths = |msgs: &[SessionMessage]| -> Vec<String> {
            extract_artifact_candidates(msgs)
                .into_iter()
                .map(|(p, _, _)| p)
                .collect()
        };
        assert_eq!(
            paths(std::slice::from_ref(&link)),
            vec!["C:/Users/u/Desktop/公司/报告.md".to_string()]
        );
        assert_eq!(
            paths(std::slice::from_ref(&tick)),
            vec!["C:/Users/u/Desktop/公司/报告.md".to_string()]
        );
        assert_eq!(
            paths(std::slice::from_ref(&fenced)),
            vec!["out/summary.json".to_string()],
            "围栏内不收、围栏外行内反引号照收"
        );
        assert!(paths(std::slice::from_ref(&url)).is_empty(), "URL 与锚点排除");
        assert!(paths(std::slice::from_ref(&pdf)).is_empty(), "白名单外扩展排除");
        assert_eq!(
            paths(std::slice::from_ref(&user)),
            vec!["./out/data.json".to_string()],
            "user 正文同权（cwd 相对路径）"
        );
        assert!(paths(std::slice::from_ref(&tool)).is_empty(), "tool-call 不参与");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test --lib remote::files::tests::artifact_candidates`
Expected: FAIL（`extract_artifact_candidates` 未定义）

- [ ] **Step 3: 最小实现**

`extract_inline_markup_paths` 函数后新增（纯字符串扫描，无正则依赖——与该函数同纪律）：

```rust
/// 对话正文里的「会话产物」候选（纯函数，2026-10-13 用户裁决）：assistant/user
/// 正文中 ① markdown 链接 `[文字](路径)` ② 行内反引号 `` `路径` `` 两种定界形态；
/// 三反引号代码围栏内**不收**（围栏是命令与路径样串密集区，误收温床）。候选还须
/// 过 [`is_path_candidate`]（URL/换行/空串排除）与 [`artifact_ext_allowed`]。
/// 相对路径合法（按会话 cwd 解析，存在性校验在 [`filter_artifacts_by_existence`]）。
/// 返回 (path, seq, ts) 按消息出现序；单条消息内去重，跨消息去重归 EntryMerger。
pub(crate) fn extract_artifact_candidates(
    msgs: &[SessionMessage],
) -> Vec<(String, i64, Option<i64>)> {
    let mut out = Vec::new();
    for m in msgs {
        if m.kind != "user" && m.kind != "assistant" {
            continue;
        }
        let mut seen: HashSet<String> = HashSet::new();
        for path in scan_prose_paths(&m.content) {
            if is_path_candidate(&path)
                && artifact_ext_allowed(&path)
                && seen.insert(path.clone())
            {
                out.push((path, m.seq, m.ts));
            }
        }
    }
    out
}

/// 单条正文的两形态扫描（纯字符串）：① markdown 链接逐个 `](` 定界、读到下一个
/// `)`（路径含 `(` 的罕见形态会截断，截断串过不了候选判定，宁缺勿错）；`#` 开头
/// 为页内锚点跳过；② 行内反引号按行扫描，``` 围栏行切换围栏态（围栏内整行跳过），
/// 非围栏行内逐对 `...` 取内容。
fn scan_prose_paths(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = content[from..].find("](") {
        let start = from + i + 2;
        let Some(end_rel) = content[start..].find(')') else {
            break;
        };
        let cand = content[start..start + end_rel].trim();
        if !cand.is_empty() && !cand.starts_with('#') {
            out.push(cand.to_string());
        }
        from = start + end_rel;
    }
    let mut fenced = false;
    for line in content.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let mut rest = line;
        while let Some(a) = rest.find('`') {
            let after = &rest[a + 1..];
            let Some(b) = after.find('`') else {
                break;
            };
            let inner = after[..b].trim();
            if !inner.is_empty() {
                out.push(inner.to_string());
            }
            rest = &after[b + 1..];
        }
    }
    out
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test --lib remote::files`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/remote/files.rs
git commit -m "feat(remote-files): 对话正文产物候选扫描器（markdown 链接 + 行内反引号，围栏不收）"
```

**验收条件：** 链接/反引号两形态（assistant+user）都产出候选；围栏内、URL、锚点、`.pdf`、tool-call 消息均不产出；`cargo test --lib remote::files` 全绿。

---

### Task 3: `FileOrigin::SessionArtifact` 第四池 + 兜底合并

**Files:**
- Modify: `src-tauri/src/remote/files.rs`（`FileOrigin` 枚举约 :595、`rank` 约 :603、`EntryMerger` 约 :704）

- [ ] **Step 1: 写失败测试**

```rust
    /// 兜底合并（2026-10-13 用户裁决）：正文候选命中既有池的键 → **整体跳过**
    /// （不改标、不涨 hits、不挪 lastSeq——既有三池合并结果零变化）；
    /// 未命中的落 SessionArtifact（rank 最低，modified=false）
    #[test]
    fn merge_artifact_fallback_skips_existing_keys_and_fills_new_ones() {
        let mut merger = EntryMerger::new(false);
        // 既有池：工具写 + 用户上传
        merger.absorb_messages(&[
            tool_call(r#"{"file_path":"/p/edited.rs"}"#),
            {
                let mut m = text_msg("user", "上传了 `<file path=\"/p/upload.png\">`");
                m.content = "<file path=\"/p/upload.png\">".into();
                m
            },
        ]);
        let before: Vec<(String, usize)> = merger
            .out
            .iter()
            .map(|e| (e.path.clone(), e.hits))
            .collect();
        // 正文候选：一个既有文件（跳过）+ 一个纯正文路径（落新池）
        merger.merge_artifact_fallback(&[
            ("/p/edited.rs".to_string(), 9, Some(900)),
            ("/p/generated-report.md".to_string(), 9, Some(900)),
        ]);
        // 既有条目零变化（hits/lastSeq/origin）
        let after: Vec<(String, usize)> =
            merger.out.iter().map(|e| (e.path.clone(), e.hits)).collect();
        assert_eq!(before, after, "命中既有键的候选整体跳过（hits 不涨）");
        let edited = merger.out.iter().find(|e| e.path == "/p/edited.rs").unwrap();
        assert_eq!(edited.origin, FileOrigin::ToolWrite, "工具亲手写过不改标");
        // 新路径落会话产物池
        let artifact = merger
            .out
            .iter()
            .find(|e| e.path == "/p/generated-report.md")
            .unwrap();
        assert_eq!(artifact.origin, FileOrigin::SessionArtifact);
        assert_eq!(artifact.last_seq, 9);
        assert!(!artifact.modified, "产物条目不是工具改写");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test --lib remote::files::tests::merge_artifact_fallback`
Expected: FAIL（`SessionArtifact` / `merge_artifact_fallback` 未定义）

- [ ] **Step 3: 最小实现**

`FileOrigin` 枚举加变体并重排 rank（数值仅内部相对序，序列化不受影响——serde 走 rename_all）：

```rust
pub enum FileOrigin {
    /// 会话产物（2026-10-13 用户裁决）：路径出现在对话正文（markdown 链接 /
    /// 行内反引号）且文件真实存在——脚本/命令生成的产物常只在正文里露路径
    /// （tool-call 参数没有它）。rank 最低只兜底：三个既有池命中的文件不改标
    SessionArtifact,
    User,
    ToolRead,
    ToolWrite,
}
```

```rust
    fn rank(self) -> u8 {
        match self {
            FileOrigin::SessionArtifact => 0,
            FileOrigin::User => 1,
            FileOrigin::ToolRead => 2,
            FileOrigin::ToolWrite => 3,
        }
    }
```

`EntryMerger` impl 内新增：

```rust
    /// 会话产物兜底合并：候选命中既有键（任一池）→ 整体跳过（不改标 / 不涨
    /// hits / 不挪 lastSeq——「既有三池合并结果零变化」红线）；未命中才以
    /// SessionArtifact 落池
    pub(crate) fn merge_artifact_fallback(
        &mut self,
        candidates: &[(String, i64, Option<i64>)],
    ) {
        for (path, seq, ts) in candidates {
            let key = normalize_key(path, self.windows);
            if self.index.contains_key(&key) {
                continue;
            }
            self.merge(path, *seq, *ts, FileOrigin::SessionArtifact);
        }
    }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test --lib remote::files`
Expected: PASS（既有三池用例全绿——rank 数值重排不影响相对序）

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/remote/files.rs
git commit -m "feat(remote-files): FileOrigin::SessionArtifact 第四池 + 兜底合并（命中既有键整体跳过）"
```

**验收条件：** 命中既有键的候选不改标/不涨 hits/不挪 lastSeq；新路径落 `session_artifact`；既有三池全部用例不红。

---

### Task 4: `MessagesPage.cwd`（codex rollout 提供会话 cwd）

**Files:**
- Modify: `src-tauri/src/remote/content.rs`（`MessagesPage` 约 :261、`page()` 约 :366、`rollout_session_id` 约 :1114、`read_codex_rollout_with` 尾部约 :1166）

- [ ] **Step 1: 写失败测试**

在 content.rs 测试区**既有 codex rollout 夹具用例**（构造 `session_meta` 含 `"cwd":"/w"` 的那支，约 :3244 的夹具字符串所在用例）末尾追加断言（`pg` 为该用例已读取的 MessagesPage，变量名以实际为准）：

```rust
        assert_eq!(
            pg.cwd.as_deref(),
            Some("/w"),
            "rollout 首行 session_meta.payload.cwd 必须透传（会话产物相对路径的解析基准）"
        );
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test --lib remote::content`
Expected: FAIL（`cwd` 字段不存在，编译错）

- [ ] **Step 3: 最小实现**

`MessagesPage` 加字段：

```rust
pub struct MessagesPage {
    pub messages: Vec<SessionMessage>,
    pub truncated: bool,
    /// 会话工作目录（可选，2026-10-13 会话产物批）：codex rollout（首行
    /// session_meta.payload.cwd）已填；其余工具暂 None——文件面板「会话产物」
    /// 相对路径的存在性校验基准。skip 序列化：/session-messages 载荷零变化
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}
```

`page()` 加 `cwd: None`：

```rust
fn page(msgs: Vec<SessionMessage>, limit: usize, truncated: bool) -> MessagesPage {
    MessagesPage {
        messages: finalize(msgs, limit),
        truncated,
        cwd: None,
    }
}
```

`rollout_session_id` 旁加同款函数：

```rust
/// rollout 首行 session_meta → 会话工作目录（payload.cwd；缺失/损坏 None）
fn rollout_session_cwd(path: &Path) -> Option<String> {
    let first = crate::monitor::jsonl::read_first_lines(path, 1)
        .into_iter()
        .next()?;
    let v: serde_json::Value = serde_json::from_str(&first).ok()?;
    v.pointer("/payload/cwd")
        .and_then(|c| c.as_str())
        .map(String::from)
        .filter(|s| !s.is_empty())
}
```

`read_codex_rollout_with` 尾部（现 :1166 `Ok(page(...))`）改为：

```rust
    let mut pg = page(map_codex_lines(&lines), limit, truncated);
    pg.cwd = rollout_session_cwd(&f);
    Ok(pg)
```

若编译报其他 `MessagesPage` 字面构造点缺字段（测试桩等）：一律补 `cwd: None`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test --lib remote::content`
Expected: PASS（全绿；若 api/server 测试有 MessagesPage 字面构造，补 `cwd: None` 后复跑）

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/remote/content.rs src-tauri/src/remote/api.rs src-tauri/src/remote/server.rs
git commit -m "feat(remote-content): MessagesPage.cwd 可选字段——codex rollout session_meta 透传会话 cwd"
```

**验收条件：** codex rollout 夹具用例断言 `cwd == Some("/w")`；`/session-messages` 载荷不含新字段（skip_serializing_if）；`cargo test --lib remote::` 全绿。

**已知边界（如实申报，写入 Task 7 文档）：** 本批仅 codex（rollout 线路）提供 cwd；其余工具 cwd=None → 会话产物的**相对**路径候选一律丢弃（保守），绝对路径不受影响。

---

### Task 5: 存在性过滤 + 提取总接线

**Files:**
- Modify: `src-tauri/src/remote/files.rs`（`extract_file_paths_with_env` 约 :520；新过滤函数放 `EntryMerger` impl 之后）

- [ ] **Step 1: 写失败测试**

```rust
    /// 会话产物存在性过滤（2026-10-13 用户裁决）：工具触达条目不校验；产物条目
    /// 绝对路径直接 stat、相对路径按 cwd 解析、cwd 不可得则丢弃（保守）。
    /// `exists` 注入缝：零真实盘接触
    #[test]
    fn filter_artifacts_by_existence_keeps_tool_entries_and_resolves_cwd_relative() {
        let mk = |path: &str, origin: FileOrigin| FileEntry {
            path: path.into(),
            last_seq: 1,
            last_ts: None,
            hits: 1,
            modified: origin == FileOrigin::ToolWrite,
            origin,
        };
        let mut entries = vec![
            mk("/p/edited.rs", FileOrigin::ToolWrite),      // 工具触达：不校验（假路径也留）
            mk("/p/live.md", FileOrigin::SessionArtifact),  // 绝对路径：exists=true 留
            mk("/p/gone.md", FileOrigin::SessionArtifact),  // 绝对路径：exists=false 丢
            mk("out/data.json", FileOrigin::SessionArtifact), // 相对路径：cwd 拼接后 exists
            mk("out/lost.json", FileOrigin::SessionArtifact), // 相对路径：cwd=None 丢
        ];
        let exists = |p: &std::path::Path| {
            let s = p.to_string_lossy();
            s == "/p/live.md" || s == "/w/out/data.json"
        };
        filter_artifacts_by_existence(&mut entries, Some("/w"), &exists);
        let kept: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            kept,
            vec!["/p/edited.rs", "/p/live.md", "out/data.json"],
            "工具触达不校验；产物按绝对/cwd 相对判定；cwd=None 的相对产物丢弃"
        );
        // cwd 不可得：相对产物全丢，其余不变
        let mut entries = vec![
            mk("/p/live.md", FileOrigin::SessionArtifact),
            mk("out/data.json", FileOrigin::SessionArtifact),
        ];
        filter_artifacts_by_existence(&mut entries, None, &|_| true);
        let kept: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(kept, vec!["/p/live.md"], "cwd 不可得 → 相对产物保守丢弃");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test --lib remote::files::tests::filter_artifacts`
Expected: FAIL（`filter_artifacts_by_existence` 未定义）

- [ ] **Step 3: 最小实现（过滤函数 + 总接线）**

`EntryMerger` impl 块之后新增：

```rust
/// 「会话产物」条目存在性过滤（2026-10-13 用户裁决：提取时校验存在）：工具触达
/// 条目不校验（既有行为不变）；产物条目——绝对路径直接判定，相对路径按会话 cwd
/// 解析后判定，cwd 不可得（工具未提供）则丢弃（保守：宁可少上墙）。
/// `exists` 注入缝：测试零真实盘接触
pub(crate) fn filter_artifacts_by_existence(
    entries: &mut Vec<FileEntry>,
    cwd: Option<&str>,
    exists: &dyn Fn(&std::path::Path) -> bool,
) {
    entries.retain(|e| {
        if e.origin != FileOrigin::SessionArtifact {
            return true;
        }
        let p = std::path::Path::new(e.path.trim());
        if p.is_absolute() {
            return exists(p);
        }
        match cwd {
            Some(c) => exists(&std::path::Path::new(c).join(p)),
            None => false,
        }
    });
}
```

`extract_file_paths_with_env` 的 `Ok(pg)` 分支（现 :538-562）加两行接线：

```rust
        Ok(pg) => {
            let mut merger = EntryMerger::new(cfg!(windows));
            merger.absorb_messages(&pg.messages);
            // …（zcode 附件第二抽取源，原样不动）…
            // 会话产物兜底（2026-10-13）：正文链接/行内反引号路径，命中既有池跳过
            merger.merge_artifact_fallback(&extract_artifact_candidates(&pg.messages));
            let mut out = merger.finish();
            // 产物条目提取时校验存在；相对路径按会话 cwd 解析（codex rollout 提供）
            filter_artifacts_by_existence(&mut out, pg.cwd.as_deref(), &|p| p.exists());
            (out, pg.truncated)
        }
```

- [ ] **Step 4: 跑测试确认通过（含接线回归）**

Run: `cd src-tauri && cargo test --lib remote::`
Expected: PASS（若既有端到端提取用例因产物过滤产生变化，按「合成正文路径不该存在」原则修夹具——正文提及的路径一律配 `exists` 可命中或改绝对路径形态）

- [ ] **Step 5: 质量门禁**

Run: `cd src-tauri && cargo fmt && cargo clippy --all-targets 2>&1 | tail -3`
Expected: 无 error；fmt 零 diff

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/remote/files.rs
git commit -m "feat(remote-files): 会话产物总接线——正文候选兜底合并 + 存在性过滤（相对路径按会话 cwd 解析）"
```

**验收条件：** 过滤纯函数三态（工具触达不校验 / 绝对判定 / cwd 相对判定与 cwd=None 丢弃）全绿；`extract_file_paths_with_env` 接线后 `cargo test --lib remote::` 全绿；clippy/fmt 干净。

---

### Task 6: 前端第四来源（类型 + 筛选器 + 徽标 + 测试）

**Files:**
- Modify: `src/mobile/api.ts:217`
- Modify: `src/mobile/FilePanel.tsx`（:68 类型、:190-195 options、:197-201 labels）
- Test: `tests/mobile/FilePanel.test.tsx`（:133-138 options 断言、:286-334 来源筛选 describe）

- [ ] **Step 1: 写失败测试**

`FilePanel.test.tsx` :133-138 的 options 断言改为：

```tsx
    expect([...originSel.options].map((o) => o.text)).toEqual([
      "全部",
      "我上传的",
      "工具读取",
      "工具读写",
      "会话产物",
    ]);
```

来源筛选 describe（:288 起）的 `entries` 数组加一条，并新增两个用例：

```tsx
  const entries = [
    entry("/p/upload.png", { lastSeq: 6, origin: "user" }),
    entry("/p/read-only.rs", { lastSeq: 5, origin: "tool_read" }),
    entry("/p/edited.rs", { lastSeq: 4, origin: "tool_write" }),
    entry("/p/generated-report.md", { lastSeq: 3, origin: "session_artifact" }),
    entry("/p/legacy.rs", { lastSeq: 2 }), // 旧载荷无 origin
  ];
```

（既有用例的索引断言 `file-row-0/1/2/3-origin` 需随新条目位置同步：`upload.png`=row-0、`read-only.rs`=row-1、`edited.rs`=row-2、`generated-report.md`=row-3、`legacy.rs`=row-4——旧载荷断言改 `file-row-4-origin`。）

```tsx
  it("切「会话产物」：仅 origin=session_artifact 可见（第四池兜底，脚本产物上墙）", () => {
    renderPanel(entries);
    pick("file-origin-select", "session_artifact");
    expect(screen.getByText("generated-report.md")).toBeTruthy();
    expect(screen.queryByText("edited.rs")).toBeNull();
    expect(screen.queryByText("upload.png")).toBeNull();
  });

  it("行内来源徽标含会话产物（session_artifact → 会话产物）", () => {
    renderPanel(entries);
    expect(screen.getByTestId("file-row-3-origin").textContent).toBe("会话产物");
  });
```

- [ ] **Step 2: 跑测试确认失败**

Run: `pnpm vitest run tests/mobile/FilePanel.test.tsx`
Expected: FAIL（options 数组缺「会话产物」；`session_artifact` 徽标 fallback 显示原始 key）

- [ ] **Step 3: 最小实现**

`src/mobile/api.ts:217`：

```ts
  origin?: "user" | "tool_read" | "tool_write" | "session_artifact";
```

（同函数上方注释「来源三池」补一句：`session_artifact = 会话产物（正文链接/反引号路径 + 存在性校验，2026-10-13）。`）

`src/mobile/FilePanel.tsx` 三处：

```tsx
// :68
type FileOriginFilter = "all" | "user" | "tool_read" | "tool_write" | "session_artifact";
```

```tsx
// :190-195 originOptions 末尾加
    { key: "session_artifact", label: "会话产物" },
```

```tsx
// :197-201 originLabel 加
    session_artifact: "会话产物",
```

- [ ] **Step 4: 跑测试确认通过**

Run: `pnpm vitest run tests/mobile/FilePanel.test.tsx`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add src/mobile/api.ts src/mobile/FilePanel.tsx tests/mobile/FilePanel.test.tsx
git commit -m "feat(mobile): 文件面板第四来源「会话产物」——筛选器/徽标/类型契约"
```

**验收条件：** 下拉五档；`session_artifact` 精确过滤；徽标中文「会话产物」；`pnpm vitest run tests/mobile/FilePanel.test.tsx` 全绿（i18n 无涉及——移动端硬编码中文）。

---

### Task 7: 手册口径 + 全量门禁 + 收尾

**Files:**
- Modify: `docs/product/manual/zh/08-mobile-remote.md`（「文件面板」相关段落）

- [ ] **Step 1: 手册补口径说明**

在 08 章文件面板介绍处（grep `文件` 定位段落）补一句：

```markdown
来源四档：「我上传的」「工具读取」「工具读写」之外还有「会话产物」——对话里以链接或反引号给出过路径、且文件真实存在的生成物（脚本/命令产出的报告、图表、数据等，md/图片/html/json 等可预览类型）；工具亲手改写过的文件仍显示「工具读写」。
```

- [ ] **Step 2: 全量门禁**

```bash
cd src-tauri && cargo test && cargo clippy --all-targets 2>&1 | tail -3
cd .. && pnpm test 2>&1 | tail -4 && pnpm check 2>&1 | tail -3
```

Expected: 全绿（Rust 全量 + 前端全量 + format/lint/build）。已知无关失败：Windows 真机环境债 `probe_steps_marks_each_criterion_honestly`（干净树同挂、CI Linux ignore，2026-10-13 已登记）——如挂且仅此一支，不算本批回归。

- [ ] **Step 3: 提交 + 推送**

```bash
git add docs/product/manual/zh/08-mobile-remote.md
git commit -m "docs(user-manual): 08 章文件面板补「会话产物」来源口径"
git push origin main
```

（push docs 命中 notify-website 路径 → 官网手册自动重建，2026-10-13 已验证链路。）

**验收条件：** 全量门禁绿（除已登记环境债）；手册上线后官网 /docs 08 章含「会话产物」口径；真机验收——打开 codex 黄金案例会话的文件面板，报告 md 以「会话产物」出现在列表且可预览（此项需用户实机确认）。

---

## Self-Review 记录

- 裁决覆盖：六项裁决各对到 Task 1（类型白名单+html）、Task 2（形态）、Task 3（兜底/rank）、Task 4（cwd）、Task 5（存在性）、Task 6（命名/前端）✓
- 占位符扫描：Task 4 Step 3 的 `pg` 变量名、Task 6 的 row 索引随条目移位，均已给出判定规则与目标值 ✓
- 类型一致性：`extract_artifact_candidates(msgs)` 返回 `Vec<(String, i64, Option<i64>)>`；`merge_artifact_fallback(&[(String, i64, Option<i64>)])`；`filter_artifacts_by_existence(&mut Vec<FileEntry>, Option<&str>, &dyn Fn(&Path)->bool)`——三处签名互相对齐 ✓

---

## 实施追记（2026-10-13 执行）

> 本节为执行完成后回填，**不在原计划内**。上文各 Task 的步骤与代码块**原文未改**（作为历史记录），故其中「零额外 IO」「/session-file 载荷」等旧口径**一律以本节为准**。本计划文档此前未入库，随本批次一并登记（`docs/engineering/plans/` 入库惯例）。

### 一、实际提交链（14 笔）

`git log --oneline df7cfc54..HEAD`（旧 → 新；`e33e8b9d^ == df7cfc54` 已核）：

| # | 任务 | 提交 | 一句话 |
|---|------|------|--------|
| 1 | Task 1 | `e33e8b9d` | mime 表补 html/htm + 产物扩展名白名单 |
| 2 | Task 1 修复轮 | `797b1ab0` | 白名单补 `mjs`/`cjs`/`cc`/`hpp`（34 → 38 项）+ `markdown` 并入 `md` 臂 + 漂移回归锁 |
| 3 | Task 2 | `b5c5aab2` | 正文产物候选扫描器（markdown 链接 + 行内反引号，三反引号围栏不收） |
| 4 | Task 2 修复轮 | `5bbe1453` | 围栏约束**对 markdown 链接形态同样生效** + 围栏/闸门牙齿断言 |
| 5 | Task 3 | `1b171bc9` | `FileOrigin::SessionArtifact` 第四池 + 兜底合并（命中既有键整体跳过） |
| 6 | Task 3 修复轮 | `75a1ed07` | 兜底合并改为只对**调用前已存在**的键短路（同路径重复取最后一次出现）+ 四池文档与断言补强 |
| 7 | Task 4 | `dac0ccd5` | `MessagesPage` 加可选 `cwd` + codex rollout 透传会话工作目录 |
| 8 | Task 5 | `f8ffbf36` | 产物总接线：正文候选兜底合并 + 存在性过滤（相对路径按会话 cwd 解析） |
| 9 | Task 5 修复轮 ① | `e9a348a9` | 去重键按会话 cwd 绝对化——工具绝对路径与正文相对链接不再出两行 |
| 10 | Task 5 修复轮 ② | `b4db293e` | 去重键的 `.` 段处理：内部 `.` 与重复分隔符由 std `components()` 归一，显式 CurDir 过滤额外覆盖**行首** `.`（cwd 不可得时的键稳定）+ 边界断言 |
| 11 | Task 5 修复轮 ③ | `41119b78` | 短路集合与候选键**同口径**——工具相对路径 + 正文相对链接不再双行（回归修复） |
| 12 | Task 6 | `e6c7b64f` | 前端第四来源：筛选器 / 徽标 / 类型契约 |
| 13 | Task 7 ① | `18376e45` | 注释口径修正：rollout cwd 额外 IO 如实申报 / 围栏错位源改 CRLF / 预览路由名与 mime 因果更正 |
| 14 | Task 7 ② | 本追记所在提交 | 用户手册 08 章补「会话产物」来源口径 + 本节实施追记（计划文档随本批入库） |

Task 1 / 2 / 3 各 1 轮修复，**Task 5 三轮修复**（去重键三连）；Task 4 / 6 一笔即成。Task 7 = 手册口径 + 注释失真修正 + 本节追记（两笔 docs 提交）；**全量门禁由控制者统一执行**，实施者仅跑模块级测试。

### 二、执行期用户裁决（4 条，均为原计划未覆盖面）

1. **白名单补 4 项**：会话产物白名单在原计划 34 项基础上补 `mjs` / `cjs` / `cc` / `hpp`（对齐裁决 3「mime 表已知扩展」口径，共 38 项）——原清单漏了 mime 表已注册的这 4 项，脚本产物 `.mjs` / `.hpp` 会被白名单误拦。
2. **mime 表 `md` 臂并入 `markdown`**：否则 `.markdown` 产物虽在白名单内，却按纯文本渲染。
3. **围栏约束对 markdown 链接形态同样生效**：原计划实现只在反引号那一趟维护围栏态 → 围栏内的 markdown 链接会误上墙。
4. **产物池自身的重复提及按最后一次出现**：原计划「命中 index 即跳过」会把产物条目的 `lastSeq` / `hits` 锁在首次；改为**只对调用前已存在的键**短路（本趟自建的产物条目不受短路影响）。同批确立**同一文件相对 / 绝对两形态只出一行**：去重键按 cwd 绝对化 + `Path::components()` 词法归一。

### 三、与计划的偏离（如实）

1. **Task 1 函数体加了 URL 守卫**：计划给的 `artifact_ext_allowed` 实现（末段扩展名命中即放行）**过不了它自己的 URL 断言**——`https://x.com/a.md` 末段 `a.md` 会命中而误放行；实现加 `path.contains("://") → false`，白名单作第二道闸、不依赖调用方先过滤。
2. **Task 3 的「既有条目零变化」断言改写**：原写法全量比对既有条目，而兜底合并**必然追加新条目** → 断言假红；改为只比对**合并前已存在**的键，并加三轴锁（展示形态 / `lastSeq` / `ts`）。
3. **Task 5 过滤用例夹具改平台自适**：Windows 上 `/p/x` **不是**绝对路径（只有盘符 / UNC 前缀算），原夹具的绝对 / 相对对照在 Windows 不成立；改用 `if cfg!(windows) { "C:/w" } else { "/w" }` 形态。
4. **Task 6 额外同步一行注释**：`FilePanel.tsx` 的 `FileOriginFilter` 类型上方注释由「线稿三池」改为「线稿三池 + 会话产物兜底池」（计划只要求改类型与文案数组）。

### 四、已知残留与边界（勿美化）

- **相对路径产物仅 codex rollout 线路可用**：会话 cwd 本批唯一来源是 codex rollout 文件首行 `session_meta.payload.cwd`；**codex APP（thread_history）线路与其余七工具本批未取 cwd** → 这些会话里相对路径的产物不上墙（**绝对路径产物不受影响**）。口径是「**本批未取**」而非「工具不可得」：codex APP 线路实际可从 `state_*.sqlite` 的 `threads.cwd` 取到，只是本批没做。
- **`..` 段不折叠**：词法归一仅滤 `.`（`CurDir`），保留 `..`（`ParentDir`）→ `out/../out/x.md` 与 `out/x.md` 仍可能出两行。
- **围栏判定不全**：`~~~` 波浪围栏与 4 空格缩进代码块**未**纳入围栏判定（用户裁决只提三反引号，不外扩）。
- **markdown 语法漏收**：带标题的链接（`[x](a.md "标题")`）与双反引号跨度漏收。
- **plan 卡与 thinking 条目不参与扫描**（合并评审批次补记）：`content.rs` 把含 `<proposed_plan>` 的 assistant 正文升格为 kind=`plan`（thinking 同理独立 kind），而产物扫描只认 user/assistant 两种 kind → **计划文档里引用的产物路径不上墙**。方向保守（宁缺勿错），修法候选：plan 卡并入扫描（前端本就把 plan 卡渲染为 assistant 正文一等卡）。
- **行内 code span 内的链接字面量会收**（合并评审批次补记）：`` `[x](out/span.md)` `` 产出候选——裁决只排除三反引号围栏，code span 属正文不违约，且存在性校验兜底；「讲解链接写法的示例」被当产物属误收温床，知悉即可。
- **`.ico` 前后端档位不一致**（合并评审批次补记）：后端白名单与 mime 表含 ico，前端 `IMAGE_EXTS`（FilePanel.tsx）不含 → `.ico` 产物只在「全部」可见、进不了「图片」档，随 originLabel 类型收紧同批处理。
- **Windows MSYS 风格路径按相对解析**：Windows 上只有盘符 / UNC 前缀算绝对路径，`/p/x.md` 会按相对 cwd 解析（通常判不存在而丢弃）。
- **产物条目只做存在性校验**：列表侧**不做**敏感路径过滤（预览侧 `read_file_safe` 仍拦）。
- 残留方向统一**保守**：宁可少上墙 / 多一行，不误标既有标签、不误合不同文件。
- **产物相对路径的「判存在基准」与「预览解析基准」是两条推导链（未消除）**：列表侧按 `pg.cwd`（content 层现读 codex rollout 首行 `session_meta.payload.cwd`），预览侧按 `session.project_path`（会话快照，api.rs 的 `read_file_safe`）。今天 codex CLI 二者同源（monitor 侧同样取 rollout 首行 `payload.cwd`），但 rollout 选取规则不同（content 层未排序取「文件名含 id 的第一个」/ monitor 侧取「匹配进程 cwd 中 mtime 最新」）→ 同一 session id 出现两份 rollout 时会分叉，表现为**列表可见但点开预览到另一目录同名文件或 403**（静默方向）。修法候选：提取期把相对产物解析为绝对路径随条目带出（展示仍原文）/ 两侧选取规则一致化。**真机验收请重点观察「相对路径产物点开预览」**。

### 五、环境债与后续项

**环境债（本机 Windows 实测；均非本批回归）**

- `remote::tailscale::tests::probe_steps_marks_each_criterion_honestly` 在 Windows 上**确定性失败**——与 Windows 步骤表断言自相矛盾（干净树同挂、CI 上 Linux ignore）。
- `cargo test --lib` 另有 3 条既有失败：`%TEMP%` 落真实 home 之下 ×2、建符号链接需特权 ×1。
- 本机 clippy 偶发 `0xC0000409` 栈溢出，`RUST_MIN_STACK=67108864` 可绕。

**后续项**

- codex APP 线路补 `threads.cwd`（`state_*.sqlite`）→ 该线路的相对路径产物即可上墙。
- 前端 `originLabel` 类型收紧为 `Record<NonNullable<SessionFileEntry["origin"]>, string>`，并与平行维护面收敛为同批设计：`originOptions`（`FilePanel.tsx:190`）/ `originLabel`（`:198`）/ 筛选档类型（`:68`）/ 保序断言（`FilePanel.test.tsx:133`）。
- `IMAGE_EXTS`（`FilePanel.tsx:73`）是否补 `ico`——后端白名单与 mime 表均已含 `ico`，前端类型过滤未计入图片档。
- 产物候选 `..` 折叠评估。
- `files.rs` 测试模块外移（现与实现同文件，已 3000+ 行）。
- `scan_prose_paths` 的 `in_fence` 线性扫描改 `partition_point`：现为每候选 `fences.iter().any(...)`（`O(C·F)`），对抗性输入实测 1MB 正文 / 64000 围栏达 10s 级；真实正文无感（非轮询路径）。

### 口径基准（覆盖上文旧表述）

正文（assistant / user）里 **markdown 链接**与**行内反引号**两形态抽「会话产物」候选，**三反引号围栏内不收**（两形态同受约束）；候选须命中可预览扩展名白名单（38 项）**且文件真实存在**才上墙；**相对路径**按会话工作目录（cwd）解析；第四来源池 `session_artifact`（中文「会话产物」）**rank 最低只兜底**——工具触达过（读取 / 改写）的文件永远显示原标签，正文提及不改其 `hits` / 最后出现序；同一文件相对 / 绝对形态按 cwd 绝对化后同键，**只出一行**（保留工具侧标签）。

### 六、全量门禁（控制者统一执行，2026-10-13）

**前端**：`pnpm test` → **136 文件 / 1406 passed / 5 skipped**，exit 0 ✅；`pnpm check`（format + lint + build）→ exit 0 ✅。

**Rust**：`cargo clippy --all-targets` → exit 0、**零 warning** ✅（本机 clippy 偶发 `0xC0000409` 栈溢出，`RUST_MIN_STACK=67108864` 可绕）。

`cargo test -j 1 --no-fail-fast` → 4 个目标有失败，**全部为环境类、且全部落在本批未改动的模块**：

| 目标 | 结果 | 失败项与根因 |
|------|------|--------------|
| lib 单测 | 2348 passed / **4 failed** / 43 ignored | ① `database::connection::tests::{real_entry_point_is_isolated_in_this_process, tests_never_resolve_the_real_users_home_when_mam_home_is_unset}`——`%TEMP%` 自身位于真实 home 之下（`C:\Users\bunny\AppData\Local\Temp`），「临时目录不得落在真实 home 下」结构性误报；② `services::usage::project::tests::raw_path_is_preserved_and_realpath_is_stored_separately`——建符号链接需特权（os error 1314）；③ `remote::tailscale::tests::probe_steps_marks_each_criterion_honestly`——计划已登记（Windows 步骤表与 macOS 断言自相矛盾） |
| `preset_v2_test` | 35 passed / **4 failed** | **仓库既有环境基线**（既有口径「preset_v2 5 败＝360 环境基线」「4 名恒定 + 1 名自抖」）：2 条为建 junction / 符号链接 `PermissionDenied (os error 5)`，2 条为 registry / reveal 白名单断言 |
| `usage_ipc_test` | 7 passed / **1 failed** | `usage_collect_runs_seven_sources_and_pairs_ok_with_error_code` 卡在**安全前提断言**：`HOME 必须是 support::setup() 的临时目录，实际 C:\Users\bunny`——Windows 上 `dirs::home_dir()` **忽略 `HOME` 环境变量**（`tests/support.rs:12` 注释原文即已言明，靠 `TUVIS_HOME` 只能重定向应用数据目录，改不了 `dirs::home_dir()`） |
| `usage_two_round_incremental_test` | 0 passed / **1 failed** | 同一根因：夹具 home 未生效 → 采集扫到**真实用户数据**（首轮 106375 条；claude parsed_files=210、zcode 72016 条、workbuddy 12 个库文件），`parsed_files == 1` 的夹具断言必然不成立 |
| 其余目标（bin / linker / dao / reconcile / real_db_isolation / usage_ledger / usage_export / usage_concurrency / usage_polling_budget / support / m9r_e2e / create_e2e / doc-tests） | 全绿或按既有 `#[ignore]` | ✅（其中 `create_e2e` / `m9r_e2e` 是**真正消费 `remote::` 的两个 e2e 目标**，本批改动未破坏它们） |

**归属判据**：上述失败所在的测试文件与其被测模块（`database/`、`services/usage/`、`services/preset/`、`remote/tailscale/`、`tests/`）在本批 `git diff --name-only df7cfc54..HEAD` 的 10 个文件中**一个都没有**。故均为**先于本批存在的 Windows 真机环境债**，不构成本批回归。

**构建环境提示**：首次全量 `cargo test` 因 `os error 1455（页面文件太小）` 在编译超大 crate 时被 OOM 打断，产生无效元数据并级联 `E0786 / E0460 / E0463`（表现为「依赖 crate 版本不一致」的假象）；改用 `-j 1` 后构建成功。本机全量 Rust 门禁建议默认 `-j 1`。

**真机验收（本轮未做，需用户实机确认）**：打开 codex 黄金案例会话的文件面板，确认报告 md 以「会话产物」出现在列表且可预览。另：`en/` 该章为占位桩（仅 5 行、无正文），无内容可同步，待整章翻译批次统一处理。

