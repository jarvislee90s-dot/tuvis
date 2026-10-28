# 文件预览敏感黑名单生产失效修复 · 实施计划

> **状态：已实施（2026-09-18）**，分支 `fix/file-preview-sensitive-blacklist`，commits `e906468` → `178a3df` → `c3d7236` → `5662a27`（未 push）。
> 实施偏差记录：① Task 3 增加了第 4 个 commit（生产接线探针）——独立验收复核发现「生产 `STATE` 接线无测试覆盖」盲区（把接线改 `|| None` 时全量测试全绿），已照本模块 `tunnel_hosts_from_snapshot` 惯例提取 `real_home_dir()` 具名函数并补探针闭合；② Task 3 的 Step 2 突变验证由独立复核子代理执行并额外覆盖了两种形态（值错 / 生产接线错）。
> 操作日志（含复验命令）：`.superpowers/sdd/2026-09-18-file-preview-fix/OPERATION-LOG.md`（未入库）。

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修复 `/m/api/v1/file` 端点上「主目录敏感目录黑名单整段失效」的生产回归——配对设备当前可读取 `~/.ssh/id_rsa`、`~/Library/Keychains`、浏览器 profile 等凭据文件；修复后黑名单在真实调用链上生效，并有端到端回归锁。

> **两类失效（2026-09-18 复核补充）**：① 端点传 `None` 致黑名单整段跳过（本计划 Task 1-3 修复）；② **macOS APFS firmlink 别名旁路**——`/System/Volumes/Data<home>/...` 与 `<home>/...` 同 inode，归属性字符串前缀比较把别名误判「主目录外」而跳过黑名单（实测泄露 194 字节），由 `8ec0545` 以 `fold_data_volume_alias()` 修复并加回归锁。**两类修完后**「主目录内黑名单在 macOS 主平台生效」的宣称才成立；残余风险见 §7。

**Architecture:** 两处改动合围。① `remote/files.rs::read_file_safe` 的 `home` 参数语义明确化：基准可用 → 仅主目录内路径过敏感段匹配（保持 M5 P2-a「主目录外不设路径级防线」裁决原样）；基准不可用（未注入 / 无法 canonicalize）→ **全段保守匹配**（fail-closed，与项目既有「None = fail-closed 哨兵」惯例一致）。② `RemoteState` 新增 `home_source` 注入缝（照 session_source / host_source / pin_source 既有惯例），生产接线 `dirs::home_dir()`，端点消费它——根因正是 3d22e2e 把生产调用改传 `None`，而单元测试全部显式传 `Some(home)`，测试全绿、生产裸奔。

**Tech Stack:** Rust（Tauri 2 侧）、axum、tempfile、dirs。前端无需改动（403 原因码 `sensitive` 的文案分诊已在 `src/mobile/FilePreview.tsx:191` 就位）。

**Spec:** 本修复无独立 spec。裁决出处 = `src-tauri/src/remote/files.rs:127-137` 与 `src-tauri/src/remote/api.rs:362-365` 的文档注释（2026-09-18 用户裁决「预览全盘放开；黑名单仅作用于主目录内，主目录外不设路径级防线」）+ commit `3d22e2e`（引入回归；其自身注释与实现自相矛盾处即本修复靶心）。注意：M5 计划 `docs/engineering/plans/2026-09-17-m5-access-pin-and-file-pool.md` §十「已知边界」仍留有「工作区外文件预览被 403 拒绝」的过期描述——已被本次盘点在文档中追记更正，以代码与实际行为为准。

## Global Constraints

- **裁决边界不得扩大**：主目录**之外**仍不设路径级防线（2026-09-18 用户裁决原样保留）——本修复只让「主目录内的黑名单」真正生效；全段兜底仅在「主目录基准不可用」的退化分支，那是 fail-closed 防注入缺失，不是新增日常防线。
- **测试零真实用户数据目录**（M5 红线）：全部 fixture 走 `tempfile::tempdir()`；注入 home 为 tmpdir，零接触真实 `~/.ssh` 或用户主目录。
- **测试零网络**；门禁逐项：`cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check`；`cargo test` 前先 `pnpm build:mobile`（rust-embed debug 直读，M5 惯例）；前端零改动但 `pnpm test` 仍须全绿。
- **`read_file_safe` 公开签名不变**（`session_cwd, path, home: Option<&str>`）——`home` 语义从「可选边界」明确为「敏感黑名单的主目录基准」，既有测试调用点语义不变。
- 不新增 Cargo 依赖（`dirs` 5.0 已在依赖内，`src-tauri/Cargo.toml:50`）。

---

### Task 1: `read_file_safe` 的黑名单基准不可用分支改为全段保守匹配

**Files:**
- Modify: `src-tauri/src/remote/files.rs:148-166`（敏感黑名单块）
- Test: `src-tauri/src/remote/files.rs`（`mod tests` 内，紧邻既有 `read_file_safe_rejects_sensitive_dirs_inside_home`）

**Interfaces:**
- Consumes: `is_sensitive_path(child: &str, home: &str, windows: bool) -> bool`（`files.rs:190`，私有；`home="/"` 时相对段 = 全路径段，天然实现全段匹配）、`path_within(child: &Path, ancestor: &Path) -> bool`（`files.rs:235`）。
- Produces: 无新公开符号；`read_file_safe` 在「home 为 None」与「home 无法 canonicalize」两种退化情形下，敏感段照拒（原先前者完全跳过检查、后者返回 NotFound 误报）。

- [ ] **Step 1: 写失败测试**

在 `src-tauri/src/remote/files.rs` 的 `mod tests` 内、`read_file_safe_rejects_sensitive_dirs_inside_home`（`files.rs:900`）之后追加：

```rust
    /// 黑名单基准不可用时的保守语义：敏感段照拒——凭据防护不得因基准缺失而
    /// 失效（M5 P2-a 追记回归：生产曾传 None 致黑名单整段跳过，配对设备可读
    /// ~/.ssh）；且坏基准不得把一个普通文件读成 NotFound 错误（不打死预览）
    #[test]
    fn sensitive_check_still_applies_when_home_base_unusable() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let cwd = proj.to_str().unwrap();

        // tmpdir 下造敏感段路径（不触真实主目录）
        let secret = tmp.path().join(".ssh").join("id_rsa");
        std::fs::create_dir_all(secret.parent().unwrap()).unwrap();
        std::fs::write(&secret, "PRIVATE").unwrap();
        let ok = tmp.path().join("notes.txt");
        std::fs::write(&ok, "ok").unwrap();

        // (1) home 未注入 → 敏感段必须拒（fail-closed）
        assert_eq!(
            read_file_safe(cwd, secret.to_str().unwrap(), None).unwrap_err(),
            FileRejectReason::Sensitive,
            "home 未知时敏感段必须照拒（fail-closed）"
        );
        // (2) home 给了但不存在（canonicalize 失败）→ 同样拒，且不得误报 NotFound
        assert_eq!(
            read_file_safe(cwd, secret.to_str().unwrap(), Some("/no-such-mam-home"))
                .unwrap_err(),
            FileRejectReason::Sensitive,
            "坏基准下敏感段仍须拒（不得退化为 NotFound 跳过检查）"
        );
        // (3) 两者下普通文件仍可读（不误伤全盘语义、不打死预览）
        assert!(
            read_file_safe(cwd, ok.to_str().unwrap(), None).is_ok(),
            "基准不可用不得妨碍普通文件（全盘语义不变）"
        );
        assert!(
            read_file_safe(cwd, ok.to_str().unwrap(), Some("/no-such-mam-home")).is_ok(),
            "坏基准不得让普通文件读取整体失败"
        );
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test sensitive_check_still_applies_when_home_base_unusable -- --nocapture`
Expected: FAIL —— (1) 断言 `left: Ok(...)` 对 `right: Sensitive`（当前 `None` 分支整段跳过，密钥被读出）；(2) 得到 `NotFound` 而非 `Sensitive`。

- [ ] **Step 3: 实现最小修改**

把 `src-tauri/src/remote/files.rs:150-166` 的整块：

```rust
    // 敏感黑名单（仅主目录范围内）：任意相对段命中 SENSITIVE_DIRS 即拒——
    // 凭据防护的主体是 ~/.ssh、AppData 浏览器 profile、~/Library/Keychains 等。
    // 主目录之外不再设路径级防线（低价值 + 误伤面大），访问门槛收敛到认证层
    if let Some(home_s) = home {
        let home_canon = Path::new(home_s)
            .canonicalize()
            .map_err(|_| FileRejectReason::NotFound)?;
        if path_within(&canon, &home_canon)
            && is_sensitive_path(
                &canon.to_string_lossy(),
                &home_canon.to_string_lossy(),
                cfg!(windows),
            )
        {
            return Err(FileRejectReason::Sensitive);
        }
    }
```

替换为：

```rust
    // 敏感黑名单（凭据防护，威胁主体 = 隧道/局域网上的配对设备读密钥）：
    // - 主目录基准可用 → 仅主目录内的路径过段匹配（相对主目录段；主目录之外
    //   不设路径级防线——M5 P2-a 2026-09-18 裁决原样保留）；
    // - 基准不可用（未注入 / 无法 canonicalize）→ **全段保守匹配**（fail-closed：
    //   宁可少读一个文件，凭据防护不因基准缺失而失效；也不让一个坏基准把全部
    //   预览打成 NotFound——根因回归锁，生产曾在 3d22e2e 传 None 致黑名单整段跳过）
    match home.and_then(|h| Path::new(h).canonicalize().ok()) {
        Some(home_canon) => {
            if path_within(&canon, &home_canon)
                && is_sensitive_path(
                    &canon.to_string_lossy(),
                    &home_canon.to_string_lossy(),
                    cfg!(windows),
                )
            {
                return Err(FileRejectReason::Sensitive);
            }
        }
        // home="/" ⇒ is_sensitive_path 的 strip_prefix 归零、相对段 = 全路径段
        None => {
            if is_sensitive_path(&canon.to_string_lossy(), "/", cfg!(windows)) {
                return Err(FileRejectReason::Sensitive);
            }
        }
    }
```

- [ ] **Step 4: 运行测试确认通过（连同既有邻居）**

Run: `cd src-tauri && cargo test --lib remote::files::tests`
Expected: PASS —— 新测试通过；既有 `read_file_safe_rejects_sensitive_dirs_inside_home`（传 `Some(home)`）、`read_file_safe_allows_outside_project_and_home`、`sensitive_path_check_covers_mac_and_windows_forms` 全部保持绿。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/remote/files.rs
git commit -m "fix(m5-files): 黑名单基准不可用时全段保守匹配（fail-closed）"
```

---

### Task 2: `RemoteState` 加 `home_source` 注入缝，生产接线真实 home

**Files:**
- Modify: `src-tauri/src/remote/server.rs:236-276`（`RemoteState` 字段）
- Modify: `src-tauri/src/remote/mod.rs:212-237`（生产构造）
- Modify: `src-tauri/src/remote/api.rs:393-397`（`read_file` 消费点）
- Test: 编译期覆盖——`RemoteState` 字面量共 8 处（`mod.rs:212` 生产 + `server.rs` 7 处测试：`370/902/1267/1351/1543/1816/2360`），Rust 强制全部补字段

**Interfaces:**
- Consumes: Task 1 的 `read_file_safe`（基准不可用已 fail-closed）。
- Produces: `RemoteState.home_source: Box<dyn Fn() -> Option<String> + Send + Sync>`——生产 = 真实 home 字符串；测试注入固定值（零真实主目录接触）；Task 3 用它做接线探针。

- [ ] **Step 1: 加字段并接线**

在 `server.rs` 的 `RemoteState` 结构体内、`via_hosts_source` 字段（`server.rs:275`）之后追加：

```rust
    /// 敏感黑名单主目录基准注入缝（M5 P2-a 追记）：生产 = `dirs::home_dir()`；
    /// 测试注入 tempdir home（零接触真实主目录）。**端点必须消费它**——
    /// 3d22e2e 曾传 None 使 ~/.ssh 等黑名单整段失效（单元测试全绿而生产裸奔）
    pub home_source: Box<dyn Fn() -> Option<String> + Send + Sync>,
```

在 `mod.rs` 的生产构造（`via_hosts_source: Box::new(via_hosts_from_snapshot),` 之后）追加：

```rust
        // M5 P2-a 追记：敏感黑名单的主目录基准（真实 home；取不到时 read_file_safe
        // 走全段保守匹配分支）
        home_source: Box::new(|| dirs::home_dir().and_then(|h| h.to_str().map(str::to_string))),
```

在 `api.rs` 的 `read_file` 里，把（`api.rs:396`）：

```rust
        match crate::remote::files::read_file_safe(&session.project_path, &path, None) {
```

改为：

```rust
        let home = (st.home_source)();
        match crate::remote::files::read_file_safe(&session.project_path, &path, home.as_deref()) {
```

- [ ] **Step 2: 补齐全部测试构造点（编译驱动）**

Run: `cd src-tauri && cargo check --all-targets --features marker-helper`
Expected: FAIL —— 编译器逐一点名剩余 7 处 `RemoteState` 字面量缺字段（`missing field home_source`）。

对全部 7 处，各在 `via_hosts_source: ...` 行后追加：

```rust
            home_source: Box::new(|| None),
```

（7 处统一注入 `None` 即可：Task 1 的全段保守分支对其夹具无影响——现有夹具均不含敏感段。Task 3 的接线探针是**新增**独立测试，不改动 `file_endpoints_are_gated_and_safe`。）

Run: `cd src-tauri && cargo check --all-targets --features marker-helper`
Expected: PASS（零错误）。

- [ ] **Step 3: 既有测试全跑（确认接线无行为回退）**

Run: `cd src-tauri && cargo test --lib remote::`
Expected: PASS —— 既有 file 端点测试此时注入 `None`，走全段保守分支；其夹具 `hello.txt` / `pic.png` / `big.txt` / `icon.svg` / `with space.png` 与 `/no-such-mam-fixture/nope.txt` 均不含敏感段，断言维持不变。

- [ ] **Step 4: 提交**

```bash
git add src-tauri/src/remote/server.rs src-tauri/src/remote/mod.rs src-tauri/src/remote/api.rs
git commit -m "fix(m5-files): RemoteState 加 home_source 注入缝，/file 端点接线真实 home"
```

---

### Task 3: 端到端回归锁——接线探针 + 敏感路径 403

**Files:**
- Modify: `src-tauri/src/remote/server.rs`（`file_endpoints_are_gated_and_safe` 之后新增独立测试）
- Test: 新增 `file_endpoint_rejects_sensitive_paths_inside_home`

**Interfaces:**
- Consumes: Task 2 的 `home_source`；既有测试装置 `persist_device(&state, "fe")`（`server.rs:1579`）、`router` / `req` / `body_string` / `uri_encode` 助手（`server.rs:361/532/402/1775`）。
- Produces: 生产调用链（端点 → home_source → read_file_safe）的行为锁——同类接线回归（改传 `None`、漏接字段）今后必红。

- [ ] **Step 1: 写失败测试**

在 `server.rs` 的 `file_endpoints_are_gated_and_safe` 测试之后追加：

```rust
    /// 敏感黑名单端到端回归锁（M5 P2-a 追记）：/file 端点必须**消费** home_source。
    /// 3d22e2e 曾把端点调用改传 None，使主目录内黑名单在生产链路上整段失效——
    /// 当时单元测试全绿（read_file_safe 每个用例都显式传 Some(home)，无从暴露
    /// 接线缺失），本锁补上这一层：探针 = 调用标记（照
    /// session_messages_endpoint_is_gated_and_shaped 的捕获注入先例），端点漏接
    /// home_source 时 hit 恒 false，断言直接变红。
    #[tokio::test]
    async fn file_endpoint_rejects_sensitive_paths_inside_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let proj = home.join("Desktop").join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let ok_file = proj.join("ok.txt");
        std::fs::write(&ok_file, "fine").unwrap();
        let secret = home.join(".ssh").join("id_rsa");
        std::fs::create_dir_all(secret.parent().unwrap()).unwrap();
        std::fs::write(&secret, "PRIVATE").unwrap();
        let home_s = home.to_str().unwrap().to_string();

        let session = crate::session::Session {
            id: "sess_home".into(),
            agent_type: crate::session::AgentType::Claude,
            project_name: "proj".into(),
            project_path: proj.to_str().unwrap().to_string(),
            title: None,
            git_branch: None,
            github_url: None,
            status: crate::session::SessionStatus::Idle,
            last_message: None,
            last_message_role: None,
            last_activity_at: "2026-09-15T00:00:00Z".into(),
            pid: 1,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: crate::session::ProcessForm::Cli,
            jump_supported: false,
            unread: false,
        };
        let hit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let h = hit.clone();
        let state = Arc::new(RemoteState {
            session_source: Box::new(move || crate::session::SessionsResponse {
                sessions: vec![session.clone()],
                total_count: 1,
                waiting_count: 0,
            }),
            store: crate::remote::pairing::DeviceStore::memory(),
            host_source: Box::new(|| serde_json::Value::Null),
            message_source: Box::new(|_, _, _| Err("测试桩：未注入内容源".to_string())),
            path_source: Box::new(|_, _, _| (Vec::new(), false)),
            watcher_tx: tokio::sync::broadcast::channel(64).0,
            sse_registry: Arc::new(SseRegistry::default()),
            max_devices_source: Box::new(|| 3),
            pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::new()),
            pin_source: Box::new(|| Some("1234".to_string())),
            now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
            tunnel_hosts_source: Box::new(|| Some(Vec::new())),
            via_hosts_source: Box::new(|| None),
            // 探针 + 注入 tmpdir home（零接触真实主目录）
            home_source: Box::new(move || {
                h.store(true, std::sync::atomic::Ordering::SeqCst);
                Some(home_s.clone())
            }),
        });
        let app = router(state.clone());
        persist_device(&state, "fe");

        // (1) 主目录内凭据 → 403 + 原因码 sensitive
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                &format!(
                    "/m/api/v1/file?session_id=sess_home&path={}",
                    uri_encode(&secret.to_string_lossy())
                ),
                Some("mam_device=fe"),
                None,
            ))
            .await
            .unwrap();
        assert!(
            hit.load(std::sync::atomic::Ordering::SeqCst),
            "/file 必须消费 home_source（漏接 = 生产黑名单失效，3d22e2e 回归形态）"
        );
        assert_eq!(r.status(), 403, "主目录内凭据必须拒绝");
        let b = body_string(r).await;
        assert!(b.contains("sensitive"), "403 体必须带原因码 sensitive: {b}");

        // (2) 主目录内普通文件 → 200（黑名单不误伤；同时证明 (1) 不是全盘拒绝）
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                &format!(
                    "/m/api/v1/file?session_id=sess_home&path={}",
                    uri_encode(&ok_file.to_string_lossy())
                ),
                Some("mam_device=fe"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "主目录内普通文件必须放行");
    }
```

- [ ] **Step 2: 运行测试确认失败（验证锁的有效性）**

先临时把 `api.rs` 的消费点改回 `read_file_safe(&session.project_path, &path, None)`，然后：

Run: `cd src-tauri && cargo test file_endpoint_rejects_sensitive_paths_inside_home -- --nocapture`
Expected: FAIL —— 接线探针断言 `hit` 为 false（端点未消费 home_source）。**验完立即还原为 `home.as_deref()`**。

- [ ] **Step 3: 运行测试确认通过（还原后的正确接线）**

Run: `cd src-tauri && cargo test file_endpoint_rejects_sensitive_paths_inside_home -- --nocapture`
Expected: PASS —— 探针 hit=true、(1) 403 sensitive、(2) 200。

- [ ] **Step 4: 全量门禁**

```bash
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
pnpm test
```

Expected: 全绿（Rust 全量较修复前 +2 测试；前端零改动）。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/remote/server.rs
git commit -m "test(m5-files): 黑名单端到端回归锁——home_source 接线探针 + 凭据 403"
```

---

## 附：手工验收（修复后，用户执行）

1. 桌面开远程接入 → 手机配对进入文件面板。用浏览器直接访问
   `http://<地址>/m/api/v1/file?session_id=<会话id>&path=<URL 编码的 ~/.ssh/id_rsa 绝对路径>`
   → **应 403 且响应体含 `{"error":"sensitive"}`**；预览页显示「该路径受安全策略保护，无法预览」。
2. 同法改 `~/Desktop/<某图>.png`（主目录内非敏感）→ 应正常预览（确认未误伤）。
3. 项目目录外的文件（如微信临时目录附件）→ 仍可预览（2026-09-18 全盘裁决未被本修复收窄）。
4. **别名形态**（macOS，2026-09-18 复核补充）：把第 1 步的路径换成
   `/System/Volumes/Data/Users/<u>/.ssh/id_rsa` → **同样应 403 sensitive**
   （修复前此形态返回 200 + 密钥内容）。

## 七、已知边界（残余风险）

- **firmlink 折叠按构造覆盖全部 19 条**（`/usr/share/firmlinks` 右列全部落在 Data 卷下，单一前缀折叠即全覆盖，无需逐条枚举）。真正的残余类别是**非 firmlink 别名**：如 `/.vol/<dev>/<ino>`——canonicalize 解析失败 → `NotFound`，目前被「偶然」挡住（不是机制性防线）；若将来有工具生成这类路径，需另行评估。
- **macOS 大小写**（2026-09-18 复核新发现，修复前 base 即存在）：`is_sensitive_path` 仅在 Windows 语义下做小写归一；若磁盘上目录**真名**即为大写（如手工/同步工具创建的 `.SSH`），段匹配会漏——复核实测可读 7 字节。对照：磁盘为小写 `.gnupg` 而以 `.GNUPG` 访问则正常拒（canonicalize 返回磁盘真名）。CLI 生成的凭据目录出现大写真名不现实；如需收口可让 darwin 分支也做小写归一（语义变更，另立裁决）。
- **未采用身份判定方案**（`(dev, ino)` 祖先遍历）：更稳但涉及 `path_within` 语义与 Windows 分支，超出本次范围；如需推进另立计划。
- 主目录**之外**仍不设路径级防线（2026-09-18 裁决）；黑名单只在主目录内生效（基准不可用时除外——fail-closed 全段匹配）。
