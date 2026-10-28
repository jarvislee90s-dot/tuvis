# 远程接入 · 门禁零豁免 + 固定网址通道 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** （第一部分）把 MAM 远程接入的鉴权豁免从根上取消，使「请求头 / 来源地址 / 进程归属」永久退出安全路径，并修正配对限速的来源分桶；（第二部分）新增「固定网址 · 免域名」通道并重排通道区为 4 张卡。

**Architecture（第一部分）:** 四处改动，按依赖顺序：（1）门禁零豁免——删除 `gate` 里的回环豁免块与 `is_local_access`，鉴权只认设备凭据，`classify_via` 降为三值装饰字段；（2）连接归属判据退役——判据失去唯一安全用途后整块删除（含两套系统级实现与 macOS 空实现）；（3）来源分桶修正——限速键从「TCP 对端」改为「按来源可证性分档」的可信来源，不可证即回落全局桶；（4）前端文案口径——本机也需访问密码。

**Tech Stack:** Rust（axum 0.7 + Tauri 2）、React 19 + TypeScript、Vitest；`cargo test` / `cargo clippy` / `cargo fmt`、`pnpm test` / `pnpm build` / `pnpm lint` / `pnpm format:check`。

**Spec:** `docs/engineering/specs/2026-10-06-remote-fixed-address-and-gate-design.md` —— 本计划覆盖**全部两大部分**：第一部分 §G1–§G5（Task 1–4）、第二部分 §C1–§C6（Task 5–12）。第二部分以第一部分全部通过为前置。

## Global Constraints

以下每条为项目级硬约束，**每个 Task 的要求都隐含包含本节**：

- **不变量 §G1**：`/m/api/v1/*` 的鉴权只有一条规则——**携带有效设备凭据**；唯一例外是 `/pair/pin`。**任何请求头、来源地址、进程归属不得参与鉴权判定**；判据只能来自服务端自身状态。
- **403 响应体保持为空**：不泄露「未配对 / 设备失效」的区分。
- **不放宽任何既有限速语义**：连续失败上限 **5** 次、锁定 **10** 分钟、防预言机语义均不变（§G5 只改「按什么分桶」）。
- **不采信 `True-Client-IP`**；**不裸信 `X-Forwarded-For`**（实测：前者被边缘原样透传可伪造，后者真实值被追加在后、前缀可注入）。
- **展示字段与鉴权解耦**：`via` 只进设备花名册展示，**永不得**被安全逻辑消费；相关代码注释必须显式写明。
- **历史数据兼容**：`via="local"` 的历史行**保留可读**（前端徽标仍渲染），但服务端不再产生该值；不做数据迁移。
- **门禁全绿才提交**（逐段执行，管道 `| grep/tail` 会吞退出码）：
  ```bash
  cd src-tauri && cargo fmt --check
  cd src-tauri && cargo clippy --all-targets -- -D warnings
  cd src-tauri && cargo test
  pnpm test
  pnpm build
  pnpm format:check
  pnpm lint
  ```

---

## File Structure

| 文件                                        | 职责                                        | 本计划中的改动                                                                            |
| ------------------------------------------- | ------------------------------------------- | ----------------------------------------------------------------------------------------- |
| `src-tauri/src/remote/gate.rs`              | `/m/api/v1/*` 的鉴权中间件 + `via` 装饰标签 | 删豁免块；删 `is_local_access` 与其单测；`classify_via` 去 `local` 分支、签名去掉来源地址 |
| `src-tauri/src/remote/conn_owner.rs`        | 连接归属判据（反查系统连接表拿 PID）        | **整文件删除**                                                                            |
| `src-tauri/src/remote/pin.rs`               | 配对限速状态机                              | 参数化阈值（供全局桶用更宽的阈值）；`ip` 参数更名 `key`                                   |
| `src-tauri/src/remote/api.rs`               | 移动 API 端点                               | `/pair/pin` 改用来源分桶键；`via` 取值去掉归属判据分支                                    |
| `src-tauri/src/remote/tunnel.rs`            | 隧道子进程生命周期                          | 删 `owned_channel_pids()` 与 `TunnelHandle.child_pid`                                     |
| `src-tauri/src/remote/mod.rs`               | 模块声明与服务器启停                        | 删 `pub mod conn_owner;`                                                                  |
| `src-tauri/src/remote/server.rs`            | 路由装配 + `RemoteState` + 端点测试         | 新增 `global_pin_limiter` 字段与装配；穿透矩阵改写；测试夹具补字段                        |
| `src/i18n/locales/zh.json` / `en.json`      | 中英文案                                    | 本机也需访问密码的口径                                                                    |
| `src/components/settings/RemoteSection.tsx` | 设置页通道区                                | 本机行的口径文案                                                                          |

---

## Spec 覆盖对照（每条需求 → 落到哪个 Task）

| Spec 位置        | 需求                                           | 落到                                                                                       |
| ---------------- | ---------------------------------------------- | ------------------------------------------------------------------------------------------ |
| §G1              | 鉴权零豁免：只认设备凭据，唯一例外 `/pair/pin` | **Task 1**                                                                                 |
| §G1 推论②        | 新增通道零门禁接线（无名单、无配置）           | **Task 1**（结构性收益；验收 = Task 2 Step 6 的 grep 零命中 + 新增通道时不必改 `gate.rs`） |
| §G2              | 连接归属判据整块退役                           | **Task 2**                                                                                 |
| §G3              | 展示字段与鉴权解耦（服务端不再产生 `local`）   | **Task 1** Step 5–7                                                                        |
| §G3              | 历史 `local` 标注**保留可读**                  | **Task 4** Step 1                                                                          |
| §G5              | 限速按来源可证性分桶 + 全局桶兜底              | **Task 3**                                                                                 |
| §1.3 范围表      | 前端文案口径（本机也需访问密码）               | **Task 4**                                                                                 |
| §6 里程碑 1      | 零豁免 + 判据退役 + 展示解耦                   | Task 1 + Task 2                                                                            |
| §6 里程碑 2      | 同机反代免密洞回归锚点由红转绿                 | **Task 1** Step 1 / Step 4（工作区已有那条红测试）                                         |
| §6 里程碑 3      | 文案口径                                       | **Task 4**                                                                                 |
| §6 里程碑 4      | 限速分桶修正 + 全局上限                        | **Task 3**                                                                                 |
| §5 非功能 · 性能 | 删除每回环请求的连接表扫描开销                 | **Task 2**（随判据退役一并消失）                                                           |
| §5 非功能 · 兼容 | 已配对设备凭据继续有效、历史展示数据可读       | Global Constraints + Task 4 Step 1                                                         |
| §5 非功能 · 安全 | 403 空体、不放宽限速语义、来源不进放行判定     | Global Constraints + Task 1 / Task 3                                                       |

**第二部分（§C1–§C6）**：

| Spec 位置    | 需求                                                | 落到                                         |
| ------------ | --------------------------------------------------- | -------------------------------------------- |
| §C1          | 通道本体：固定网址 · 免域名                         | **Task 5**                                   |
| §C1          | 地址固定性（重启不变）、中继不可解密                | Task 5（+ 验收清单 #2 人工）                 |
| §C2          | 首次配置引导一条龙（**一等需求**）                  | **Task 6**                                   |
| §C2          | 平台步骤表（macOS 实测 / Windows 预填）+ 探测提示词 | Task 6（步骤表）+ **Task 12**（实测回填）    |
| §C2          | 合规红线：只从官方源下载、不打包不再分发            | Task 6（Global Constraints + 固定哈希表）    |
| §C3          | 配置后强制可达性校验（**外部解析为主**）            | **Task 7**                                   |
| §C3          | 未验证不得自称可用 + 自愈 + 长期重验                | Task 7（Step 4/5/6）                         |
| §C4          | 4 张卡片 + 本机地址整行删除 + 卡片三态              | **Task 8**（线稿，前置）+ **Task 9**（实现） |
| §C4          | 线稿硬契约（实现与线稿不一致即缺陷）                | Task 8 → Task 9 的顺序 + 验收清单 #6         |
| §C5          | 带宽如实告知 + **进度可见** + 满速升级指引          | **Task 10**                                  |
| §C5          | 方法论教训（禁用服务端口径测带宽）                  | Task 10 Step 5 注释                          |
| §C6          | 外部设备体验零变化 / 新增通道零门禁接线             | Task 1（不变量）+ 验收清单 #13               |
| §6 里程碑 11 | 文档收口                                            | **Task 11**                                  |

**不在本计划范围（如实申报）**：§8 各条为约束与申报（非实施项）；§9 备选与否决为决策记录；上传侧类型白名单缺失（Task 10 测绘发现，见第二部分已知边界 #6）。

## 自查记录（写作期发现并已修正的问题）

| #   | 问题                                                                                                                                           | 处置                                                                                                                                                                                                                                                    |
| --- | ---------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1   | **类型不一致**：测试写 `Some(&tunnels)`，而签名是 `Option<&[String]>`——`Option<&Vec<T>>` 不会自动 deref，编译不过                              | 改为 `let ts = Some(tunnels.as_slice());` 并加注释说明                                                                                                                                                                                                  |
| 2   | **依赖顺序错**：`rate_bucket_key` 引用 `pin::GLOBAL_BUCKET`，但该常量原计划在后一步才定义，中间那步必然编译失败                                | 交换 Task 3 的 Step 3/4/5：先定义常量并参数化限速器，再实现 `rate_bucket_key`                                                                                                                                                                           |
| 3   | **未按真实代码写**：Task 3 Step 7 原为凭印象的片段                                                                                             | 已核对 `api.rs::pair_pin` 真实实现，改为逐段 before/after，保留既有约定（`RateDecision` 先绑定出锁、audit 不持锁、`record_failure` 与 `remaining_attempts` 同临界区、既有 audit 事件名 `pair_pin_locked` / `pair_pin_wrong` / `pair_pin_rejected_cap`） |
| 4   | **Spec 缺口**：§G3 要求历史 `local` 保留可读，原计划无任何 Task 承接                                                                           | 补 Task 4 Step 1（`ViaBadge` 的 `local` 映射**保留**、注释更新、加回归测试）                                                                                                                                                                            |
| 5   | **编号重复**：Task 4 插入新步骤后出现两个 Step 4                                                                                               | Task 4 全节重排为 8 步                                                                                                                                                                                                                                  |
| 6   | **测试助手签名**：已核对 `http_req` 为 7 参（`method, uri, addr, host, ua, cookie, body`）、`pin_post` 为 4 参（第 3 参是 **ua** 不是 cookie） | 计划中的调用与之逐参对齐（已验）                                                                                                                                                                                                                        |

---

# 第一部分 · 门禁零豁免（§G1–§G5）

### Task 1: 门禁零豁免（§G1 + §G3）

**Files:**

- Modify: `src-tauri/src/remote/gate.rs`
- Modify: `src-tauri/src/remote/api.rs`（`classify_via` 调用签名）
- Test: `src-tauri/src/remote/server.rs`（穿透矩阵改写）
- Test: `src-tauri/src/remote/gate.rs`（删 `is_local_access_truth_table`、改 `classify_via_four_branches`）

**Interfaces:**

- Consumes: 无（本计划第一个 Task）
- Produces:
  - `gate::gate(State<Arc<RemoteState>>, Request, Next) -> Response`（签名不变，**行为变为零豁免**）
  - `gate::classify_via(host: &str, quick_hosts: &[String], named_hosts: &[String]) -> &'static str`（**签名变更**：去掉第一个 `source_ip: IpAddr` 参数；返回 `"quick" | "named" | "lan"`，不再返回 `"local"`）
  - `gate::normalize_host(host: &str) -> String`（不变，仍被 `classify_via` 使用）

> **工作区已有资产**：`src-tauri/src/remote/server.rs` 里已有一条**红测试** `gate_local_exempt_must_not_cover_same_host_reverse_proxy`（2026-10-06 写于设计阶段，跑真 `router` + 真 `gate`，当前三形态实测全 200）。它断言修复后的正确行为（403），**不要重写它**——本 Task 的目标之一就是让它由红转绿。它按「每 commit 门禁全绿」纪律**不单独提交**，随本 Task 一起进历史。

- [ ] **Step 1: 把穿透矩阵改写成"任意 Host 一律不免密"（红）**

在 `src-tauri/src/remote/server.rs` 的测试模块内，找到 `gate_local_exempt_requires_loopback_and_local_host`，**整条替换**为下面两条测试（原测试的第 2、4 号断言在零豁免后不再成立）：

```rust
    /// 零豁免矩阵（安全关键，本 Task 的变异锚点）：
    /// **回环来源 + 任意 Host + 无 cookie → 一律 403。**
    /// 变异自证：谁把「回环 + Host 不在隧道名单 → 免密」加回来，本测试必红。
    /// 覆盖：本地形态 Host / 隧道域名 Host / 伪造回环 Host / 空 Host / 用户自有域名 Host。
    #[tokio::test]
    async fn gate_never_exempts_loopback_regardless_of_host() {
        let (state, _t) = a3_state(Some("1234"), 3, &["mam-test.trycloudflare.com"], &[], false);
        let app = router(state);

        for host in [
            Some("localhost:9420"),                  // 本地形态
            Some("127.0.0.1:9420"),                  // 伪造回环形态（历史漏洞入口）
            Some("mam-test.trycloudflare.com"),      // 隧道域名
            Some("mam.example.com"),                 // 用户自有域名（直连反代形态）
            Some("evil.example.com"),                // 完全外部域名
            None,                                    // 空 Host
        ] {
            let r = app
                .clone()
                .oneshot(http_req(
                    "GET",
                    "/m/api/v1/sessions",
                    "127.0.0.1:40000",
                    host,
                    None,
                    None,
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(
                r.status(),
                403,
                "回环来源 + Host={host:?} + 无凭据必须 403（不变量 G1：请求头不得影响放行）"
            );
        }
    }

    /// 配对入口仍是唯一例外：`/pair/pin` 无凭据可达（否则无法换取凭据）。
    /// 与上一条配对使用，锁死「放行名单只有一个」这个边界。
    #[tokio::test]
    async fn gate_still_allows_pair_pin_only() {
        let (state, _t) = a3_state(Some("1234"), 3, &[], &[], false);
        let app = router(state);

        // /pair/pin 无凭据可达（PIN 即凭据）——非 403 即可（本测试只断言不被门禁拦）
        let r = app
            .clone()
            .oneshot(pin_post("127.0.0.1:40100", Some("localhost:9420"), None, r#"{"pin":"1234"}"#))
            .await
            .unwrap();
        assert_ne!(r.status(), 403, "/pair/pin 是换凭据入口，不得被门禁拦");

        // 其余端点无凭据一律 403
        let r = app
            .oneshot(http_req(
                "GET",
                "/m/api/v1/host",
                "127.0.0.1:40101",
                Some("localhost:9420"),
                None,
                None,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "非配对端点无凭据必须 403");
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd src-tauri && cargo test -- gate_never_exempts_loopback_regardless_of_host gate_still_allows_pair_pin_only gate_local_exempt_must_not_cover_same_host_reverse_proxy 2>&1 | tail -30
```

Expected: **全部 FAIL**（`gate_never_exempts_...` 在 `localhost:9420` 上返回 200；`gate_local_exempt_must_not_cover_...` 报 `left: 200, right: 403`）

- [ ] **Step 3: 删除 gate 里的豁免块（绿）**

在 `src-tauri/src/remote/gate.rs` 中，把 `gate` 函数体里**从**「M5 A3 本机免密」注释块**开始、到** `let Some(device) = extract_device(...)` 之前**的整段（含 `if let Some(ci) = req.extensions()...` 整个 if 块）删除，并把函数文档注释替换为：

```rust
/// 门禁：`/pair/pin` 放行（PIN 即凭据）+ 其余请求必须带有效设备凭据，否则 403。
///
/// **不变量 G1（2026-10-06 零豁免，勿退化）**：任何请求头、来源地址、进程归属
/// **不得参与鉴权判定**。判据只能来自服务端自身状态（设备表 + 限速器）。
/// 历史上曾有一条「回环 + Host 不在隧道名单 → 免密」的本机豁免：它以**黑名单**
/// 为判据（fail-open），且在 macOS 上没有第二道防线——任何原样透传 Host 的入站
/// 通道都能把它打成免密全读写。该豁免已整块删除；**新增任何通道不需要在此处
/// 新增代码、配置或名单**。
///
/// 路径语义（重要，勿按直觉改）：本中间件由 `server.rs` 作为 **nest("/m/api/v1")
/// 的内层 layer** 挂载，axum 的 nest 会**剥掉前缀**——这里 `req.uri().path()` 看到的是
/// `/pair/pin`、`/sessions`、`/nope`，而**不是** `/m/api/v1/pair/pin`。放行名单必须用
/// 相对路径，写成绝对路径会导致 pair 被 403。
pub async fn gate(State(state): State<Arc<RemoteState>>, req: Request, next: Next) -> Response {
    if req.uri().path() == "/pair/pin" {
        return next.run(req).await; // PIN 即凭据的换 cookie 入口放行
    }
    let Some(device) = extract_device(req.headers()) else {
        return unauthorized();
    };
    let now = chrono::Utc::now().timestamp_millis();
    // 有效判定与滑动续期合并进**同一个** with 闭包（`with` 持锁不可重入，且两段之间
    // 存在 revoke 插入的 TOCTOU 窗口）
    let ok = state.store.with(|c| {
        let valid = crate::remote::pairing::device_valid(c, &device, now);
        if valid {
            crate::remote::pairing::touch_device(c, &device, now);
        }
        valid
    });
    if ok {
        next.run(req).await
    } else {
        unauthorized()
    }
}
```

- [ ] **Step 4: 跑测试确认两条新测试通过**

```bash
cd src-tauri && cargo test -- gate_never_exempts_loopback_regardless_of_host gate_still_allows_pair_pin_only gate_local_exempt_must_not_cover_same_host_reverse_proxy 2>&1 | tail -20
```

Expected: **PASS**（含工作区那条红测试——它由红转绿，这就是 §G1 的验收判据）

- [ ] **Step 5: 删除 `is_local_access` 并改 `classify_via`（编译收口）**

在 `gate.rs` 中：

1. **删除** `pub fn is_local_access(...)` 整个函数。
2. **替换** `classify_via` 为：

```rust
/// 配对来源标注（**纯装饰**，不变量 G1 推论③）：按 Host 命中哪条通道的域名名单，
/// 返回 `quick` / `named`；未命中一律 `lan`。
///
/// **不得用于任何安全判定**——它读的是请求头（Host），**可被伪造**；这里只为在设备
/// 花名册上给用户一个"从哪条通道进来的"提示，允许不准确。
/// 2026-10-06（设计说明书 §G3）：本函数不再接受来源地址、不再返回 `local`——本机
/// 不是一条通道。历史数据里已存的 `local` 标注前端仍可渲染，但服务端不再产生。
pub fn classify_via(host: &str, quick_hosts: &[String], named_hosts: &[String]) -> &'static str {
    let h = normalize_host(host);
    if h.is_empty() {
        return "lan";
    }
    if quick_hosts.iter().any(|t| normalize_host(t) == h) {
        "quick"
    } else if named_hosts.iter().any(|t| normalize_host(t) == h) {
        "named"
    } else {
        "lan"
    }
}
```

3. 清理 `gate.rs` 顶部不再需要的 import（`ConnectInfo` 与 `IpAddr` 若无其他消费者则删；`header` 仍被 cookie 解析使用）。

- [ ] **Step 6: 改 `api.rs` 的 `classify_via` 调用**

在 `src-tauri/src/remote/api.rs` 里把 `classify_via` 的调用改成新签名（**暂时保留外层连接归属判据的 match，Task 2 再删**）：

```rust
                super::gate::classify_via(host, &quick_hosts, &named_hosts)
```

- [ ] **Step 7: 删除/改写 gate.rs 内的旧单测**

- **删除** `mod tests` 里的 `is_local_access_truth_table`（函数本体已删）。
- **替换** `classify_via_four_branches` 为三分支版本：

```rust
    /// via 三分支（装饰字段）：quick 命中 → quick；named 命中 → named；其余（含空 Host）→ lan。
    /// **不再有 local 分支**（本机不是通道，2026-10-06 §G3）。
    #[test]
    fn classify_via_three_branches() {
        let quick = vec!["q-test.trycloudflare.com".to_string()];
        let named = vec!["mam.example.com".to_string()];
        assert_eq!(classify_via("q-test.trycloudflare.com", &quick, &named), "quick");
        assert_eq!(classify_via("mam.example.com:443", &quick, &named), "named", "带端口归一后匹配");
        assert_eq!(classify_via("192.168.1.9:9420", &quick, &named), "lan");
        assert_eq!(classify_via("", &quick, &named), "lan", "空 Host → lan");
        assert_eq!(classify_via("localhost:9420", &quick, &named), "lan", "本机形态不再是 local");
    }
```

- [ ] **Step 8: 跑 Rust 全量测试**

```bash
cd src-tauri && cargo test 2>&1 | tail -20
```

Expected: 全绿（若仍有引用 `is_local_access` / 旧 `classify_via` 签名的测试，按新口径改掉）

- [ ] **Step 9: 门禁全绿**

```bash
cd src-tauri && cargo fmt --check && cargo clippy --all-targets -- -D warnings
```

Expected: 无输出、退出码 0

- [ ] **Step 10: 提交**

```bash
git add src-tauri/src/remote/gate.rs src-tauri/src/remote/api.rs src-tauri/src/remote/server.rs
git commit -m "fix(remote): 门禁零豁免——删回环豁免与 is_local_access，via 降为三值装饰

不变量 G1：任何请求头/来源地址/进程归属不得参与鉴权判定；鉴权只认设备凭据，
唯一例外 /pair/pin。含同机反代免密洞的回归锚点（三形态由 200 转 403）。"
```

---

### Task 2: 连接归属判据退役（§G2）

**Files:**

- Delete: `src-tauri/src/remote/conn_owner.rs`
- Modify: `src-tauri/src/remote/mod.rs`（删模块声明）
- Modify: `src-tauri/src/remote/api.rs`（删 `via` 取值里的归属判据分支）
- Modify: `src-tauri/src/remote/tunnel.rs`（删 `owned_channel_pids()` 与 `TunnelHandle.child_pid`）

**Interfaces:**

- Consumes: Task 1 产出的 `classify_via(host, quick_hosts, named_hosts)`
- Produces: 无新接口（纯删除）；`api.rs` 的 `via` 取值变为「查 `via_hosts_source` → `classify_via`，None 时 `lan`」

- [ ] **Step 1: 删掉 api.rs 的归属判据分支**

在 `src-tauri/src/remote/api.rs` 中，把 `let via = match super::conn_owner::tunnel_channel_for_conn(...) { Some(kind) => kind, None => match (st.via_hosts_source)() { ... } };` 整段替换为：

```rust
    // via 是**纯装饰**标注（设备花名册展示用）：只按域名推断，**不得用于任何安全判定**
    // （不变量 G1 推论③）。连接归属判据已随零豁免一并退役（2026-10-06 §G2）。
    let via = match (st.via_hosts_source)() {
        None => "lan",
        Some((quick_hosts, named_hosts)) => {
            super::gate::classify_via(host, &quick_hosts, &named_hosts)
        }
    };
```

- [ ] **Step 2: 跑测试确认 `via` 相关断言仍通过**

```bash
cd src-tauri && cargo test via 2>&1 | tail -20
```

Expected: PASS（若有断言期待 `via == "local"`，按「本机不再是取值」的新口径改掉）

- [ ] **Step 3: 删文件与模块声明**

```bash
git rm src-tauri/src/remote/conn_owner.rs
```

并在 `src-tauri/src/remote/mod.rs` 中删除 `pub mod conn_owner;` 一行。

- [ ] **Step 4: 删 tunnel.rs 里的 PID 账本**

在 `src-tauri/src/remote/tunnel.rs` 中：

1. 删除 `pub(crate) fn owned_channel_pids() -> Vec<(u32, &'static str)>` 整个函数。
2. 删除 `struct TunnelHandle` 里的 `child_pid: Arc<AtomicU32>` 字段及其文档注释。
3. 删除所有对 `child_pid` 的写入点（`supervise` 里 spawn 后写 PID 的语句）与构造点（创建 `TunnelHandle` 时的初始化）。

- [ ] **Step 5: 退役 `tunnel_hosts_source`（它只为豁免判定存在）**

`tunnel_hosts_source` 的**唯一消费者**是 Task 1 删掉的豁免判定；本 Task 退役 `conn_owner` 后它彻底没有消费者。留着会留下一条**带 fail-closed 语义却无人消费**的配置缝，误导后人以为它还在保护什么。

1. 删除 `RemoteState.tunnel_hosts_source` 字段（`src-tauri/src/remote/server.rs:332`）与其文档注释。
2. 删除 `mod.rs` 的派生与接线：`tunnel_hosts_from_snapshot()`（`mod.rs:378-380`）、`tunnel_hosts_from_status()`（`mod.rs:354-376`）、接线处（`mod.rs:311`）。
   **保留** `channel_hosts` / `host_of_board_url` / `named_extra_hosts`——`via_hosts_from_status` 仍在用它们。
3. 删除全部构造点里的该字段（生产 `STATE` `mod.rs:212-317`、`server.rs:537`、`a3_state` `:2331-2340`、`server.rs:1153/1538/1642/1854/2145/6339/10560/10607/10684`、`inject/queue.rs:1094-1130`）。
4. 删除只为它存在的测试：`tunnel_hosts_fail_closed_when_running_without_url`（`mod.rs:2031`）、`tunnel_hosts_remembered_addr_rescues_missing_named_url`（`:2071`）、`tunnel_hosts_normal_state_lists_running_channel_domain`（`:2100`）。
   **保留** `via_hosts_from_snapshot_aggregates_both_channels_and_fails_closed_on_error`（`:1960`）——它测的是 `via`，仍然有效。

- [ ] **Step 6: 以编译告警为准清理剩余引用**

```bash
cd src-tauri && cargo clippy --all-targets -- -D warnings 2>&1 | tail -30
```

Expected: 逐条修掉报出的未用符号（**不要预判**哪些还有消费者——以编译器为准）。若报出 `owned_channel_pids` 之外的残留消费者，一并删除其调用。

- [ ] **Step 7: 精确 grep 自证：全套标识符应零命中**

```bash
grep -rn "本机免密\|is_local_access\|conn_owner\|tunnel_channel_for_conn\|owned_channel_pids\|tunnel_hosts_source\|tunnel_hosts_from" \
     --include=*.rs --include=*.ts --include=*.tsx src-tauri/src src/
```

Expected: **零命中**（这是 §G2 的实施完成自证判据）

- [ ] **Step 8: 跑 Rust 全量测试**

```bash
cd src-tauri && cargo test 2>&1 | tail -20
```

Expected: 全绿（`conn_owner.rs` 的内嵌单测随文件消失，用例数会减少——这是预期的）

- [ ] **Step 9: 提交**

```bash
git add -A src-tauri/src/remote
git commit -m "refactor(remote): 连接归属判据整块退役——删 conn_owner（约 300 行）与隧道 PID 账本

零豁免生效后该判据失去唯一安全用途；Linux 上每次回环请求的全量连接表扫描
开销一并消失。macOS 无归属实锤的历史落差随之失去意义。"
```

---

### Task 3: 配对限速的来源分桶修正（§G5）

**Files:**

- Modify: `src-tauri/src/remote/pin.rs`（阈值参数化 + `ip` 参数更名 `key` + 新增全局桶常量）
- Modify: `src-tauri/src/remote/gate.rs`（新增纯函数 `rate_bucket_key`）
- Modify: `src-tauri/src/remote/api.rs`（`/pair/pin` 改用分桶键 + 全局桶双查双记）
- Modify: `src-tauri/src/remote/server.rs`（`RemoteState` 加 `global_pin_limiter` 字段与装配、测试夹具补字段）

**Interfaces:**

- Consumes: Task 1 的 `normalize_host`
- Produces:
  - `gate::rate_bucket_key(source: std::net::IpAddr, host: &str, headers: &axum::http::HeaderMap, tunnel_hosts: Option<&[String]>) -> String`
  - `pin::PinRateLimiter::with_limits(max_failures: u32, lock_ms: i64) -> PinRateLimiter`
  - `pin::GLOBAL_MAX_FAILURES: u32`（= 50）
  - `pin::GLOBAL_BUCKET: &str`（= `"\u{0}global"`，与任何真实来源键不可能冲突）
  - `RemoteState.global_pin_limiter: std::sync::Mutex<pin::PinRateLimiter>`

- [ ] **Step 1: 写 `rate_bucket_key` 的失败测试（红）**

在 `src-tauri/src/remote/gate.rs` 的 `mod tests` 内追加：

```rust
    /// 分桶键真值表（安全关键，逐档锁定）。实测依据（2026-10-06）：
    /// 伪造 `CF-Connecting-IP` → Cloudflare 边缘 403；`X-Forwarded-For` 真实值被追加在后；
    /// `True-Client-IP` 原样透传（可伪造）；`X-Real-IP` 被边缘丢弃。
    #[test]
    fn rate_bucket_key_truth_table() {
        use axum::http::{HeaderMap, HeaderValue};
        let tunnels = vec!["mam-test.trycloudflare.com".to_string()];
        let lb: IpAddr = "127.0.0.1".parse().unwrap();
        let lan: IpAddr = "192.168.1.9".parse().unwrap();

        let mk = |pairs: &[(&str, &str)]| {
            let mut h = HeaderMap::new();
            for (k, v) in pairs {
                h.insert(
                    axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                    HeaderValue::from_str(v).unwrap(),
                );
            }
            h
        };

        // 注意：签名收 `Option<&[String]>`——**必须**用 `as_slice()`，
        // 写 `Some(&tunnels)` 是 `Option<&Vec<String>>`，不会自动 deref，编译不过。
        let ts = Some(tunnels.as_slice());

        // ① 非回环 → 一律用来源地址（真实不可伪造），与头无关
        let h = mk(&[("cf-connecting-ip", "1.2.3.4")]);
        assert_eq!(rate_bucket_key(lan, "mam-test.trycloudflare.com", &h, ts), "192.168.1.9");

        // ② 回环 + 隧道域名 + 权威头存在 → 用权威头
        assert_eq!(rate_bucket_key(lb, "mam-test.trycloudflare.com", &h, ts), "1.2.3.4");
        assert_eq!(rate_bucket_key(lb, "mam-test.trycloudflare.com:443", &h, ts), "1.2.3.4", "带端口归一");

        // ③ 回环但 Host 不在名单 → 回落全局桶（fail-closed）
        assert_eq!(rate_bucket_key(lb, "evil.example.com", &h, ts), "\u{0}global");
        // ④ 回环 + 隧道域名但权威头缺失 → 回落全局桶
        assert_eq!(rate_bucket_key(lb, "mam-test.trycloudflare.com", &mk(&[]), ts), "\u{0}global");
        // ⑤ 隧道名单不可得（快照错误态）→ 回落全局桶
        assert_eq!(rate_bucket_key(lb, "mam-test.trycloudflare.com", &h, None), "\u{0}global");
        // ⑥ **绝不**采信可伪造的 True-Client-IP；**绝不**裸信 X-Forwarded-For
        let bad = mk(&[("true-client-ip", "9.9.9.9"), ("x-forwarded-for", "5.6.7.8")]);
        assert_eq!(rate_bucket_key(lb, "mam-test.trycloudflare.com", &bad, ts), "\u{0}global");
        // ⑦ 空 Host → 回落全局桶
        assert_eq!(rate_bucket_key(lb, "", &h, ts), "\u{0}global");
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd src-tauri && cargo test -- rate_bucket_key_truth_table 2>&1 | tail -15
```

Expected: FAIL with "cannot find function `rate_bucket_key`"

- [ ] **Step 3: 参数化 `PinRateLimiter` 的阈值 + 新增全局桶常量**

在 `src-tauri/src/remote/pin.rs` 中：

1. 在 `MAX_FAILURES` / `LOCK_MS` 之后追加：

```rust
/// 全局桶的键（固定值，含 NUL 前缀——与任何真实来源键不可能冲突）
pub const GLOBAL_BUCKET: &str = "\u{0}global";

/// 全局上限（分布式爆破只能靠它挡：单个来源的限速挡不住多来源齐爆）。
/// **必须宽到单个来源填不满**——这样捣乱者只会填满自己的桶，伤不到别人。
/// 取值 50：单来源阈值 5 的 10 倍，正常使用绝无可能触及。
pub const GLOBAL_MAX_FAILURES: u32 = 50;
```

2. 把 `PinRateLimiter` 改为持有自己的阈值，并新增 `with_limits`：

```rust
pub struct PinRateLimiter {
    entries: HashMap<String, Entry>,
    max_failures: u32,
    lock_ms: i64,
}

impl PinRateLimiter {
    /// 分来源桶：沿用既有语义（5 次 / 10 分钟）
    pub fn new() -> Self {
        Self::with_limits(MAX_FAILURES, LOCK_MS)
    }

    /// 全局桶：更宽的阈值（见 `GLOBAL_MAX_FAILURES`）
    pub fn global() -> Self {
        Self::with_limits(GLOBAL_MAX_FAILURES, LOCK_MS)
    }

    pub fn with_limits(max_failures: u32, lock_ms: i64) -> Self {
        Self {
            entries: HashMap::new(),
            max_failures,
            lock_ms,
        }
    }
```

3. 把 `check` / `record_failure` / `remaining_attempts` 三个方法里**所有** `ip: &str` 参数名改为 `key: &str`，并把函数体里硬编码的 `MAX_FAILURES` / `LOCK_MS` 改为 `self.max_failures` / `self.lock_ms`（**注意 `record_failure` 里 `e.failures >= MAX_FAILURES` 与 `e.locked_until = Some(now + LOCK_MS)` 两处**）。

> **`record_success` 的语义边界**：成功**只清分来源桶**，**不清全局桶**——全局桶只按时间衰减（`check` 的到期分支）。攻击者不可能产生成功，故这条只影响正常用户的计数残留，而残留会在锁定期满后自然清零。

- [ ] **Step 4: 实现 `rate_bucket_key`（绿）**

在 `src-tauri/src/remote/gate.rs` 中追加（放在 `classify_via` 之后）：

```rust
/// 配对限速的**分桶键**推导（纯函数，安全关键）：按来源「可证性」分档，
/// **不可证即 fail-closed**（设计说明书 §G5）。
///
/// 分档规则：
/// 1. 来源**非回环** → 直接用来源地址。真实且不可伪造，覆盖局域网与一切直连来源；
/// 2. 来源**回环**（流量经本机连接器进来）→ 只有**同时**满足
///    「`Host` 命中该通道已登记域名」**且**「权威来源头存在」时，才用该头；
///    两个条件缺一即**回落全局桶**；
/// 3. 其余一切情况 → **回落全局桶**。宁可更严，**绝不**回落成"每请求一个新桶"
///    （那等于取消限速）。
///
/// 权威来源头**按通道静态声明**：Cloudflare 系（临时隧道 / 命名隧道）= `CF-Connecting-IP`。
/// **不得在运行时按请求内容推断"该信哪个头"**。
///
/// **实测依据（2026-10-06，经 quick tunnel 回显全部请求头）**：
/// - 伪造 `CF-Connecting-IP` → **Cloudflare 直接 403**，整个请求被拒（比"覆盖"更强）；
/// - `X-Forwarded-For` → 真实值被**追加在后**（`伪造值,真实值`），前缀可注入；
/// - `True-Client-IP` → **原样透传**，可伪造，**绝不可采信**；
/// - `X-Real-IP` → 被边缘丢弃。
/// 另：Host 那一条为何够用——**Cloudflare 边缘会拒绝伪造的 Host**（同日实测两次均 403），
/// 故"Host 命中隧道域名"等价于"确实经它进来"。
///
/// **不得用于放行判定**——它只进限速与审计（不变量 §G1 推论③的同源纪律）。
pub fn rate_bucket_key(
    source: std::net::IpAddr,
    host: &str,
    headers: &axum::http::HeaderMap,
    tunnel_hosts: Option<&[String]>,
) -> String {
    // 档 1：非回环来源，真实不可伪造
    if !source.is_loopback() {
        return source.to_string();
    }
    // 档 2：回环来源需两个条件同时成立
    let normalized = normalize_host(host);
    let host_is_tunnel = !normalized.is_empty()
        && tunnel_hosts
            .map(|ts| ts.iter().any(|t| normalize_host(t) == normalized))
            .unwrap_or(false);
    if host_is_tunnel {
        if let Some(v) = headers
            .get("cf-connecting-ip")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return v.to_string();
        }
    }
    // 档 3：fail-closed 回落全局桶
    crate::remote::pin::GLOBAL_BUCKET.to_string()
}
```

- [ ] **Step 5: 跑测试确认通过**

```bash
cd src-tauri && cargo test -- rate_bucket_key_truth_table 2>&1 | tail -10
```

Expected: PASS

- [ ] **Step 6: 给 `RemoteState` 加全局桶并装配**

在 `src-tauri/src/remote/server.rs` 中：

1. `RemoteState` 里 `pin_limiter` 之后追加：

```rust
    /// 全局限速桶（§G5）：只拦"多来源一起爆破"，阈值远宽于分来源桶——
    /// 单个捣乱者填不满它，因此伤不到别人。
    pub global_pin_limiter: std::sync::Mutex<crate::remote::pin::PinRateLimiter>,
```

2. 找到构造 `RemoteState` 的生产处（`serve` 或 `router` 附近的装配代码）与测试夹具 `a3_state`，**两处都补**：

```rust
                global_pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::global()),
```

- [ ] **Step 7: 改 `/pair/pin` 用分桶键 + 双桶查询记账**

`src-tauri/src/remote/api.rs::pair_pin` 的**真实起点**（已核对，勿凭记忆改）：

```rust
    use crate::remote::pin::RateDecision;
    let ip = addr.ip().to_string();
    // 限速时钟走注入缝（测试可推进）；设备时间戳走真实时钟（gate 滑动 TTL 域，见 persist）
    let now = (st.now_source)();
```

**改动 1 —— 用分桶键取代 `ip`**（替换上面那两行）：

```rust
    use crate::remote::pin::RateDecision;
    // 分桶键：按来源可证性分档（§G5）。**不是**鉴权判据——只进限速与审计。
    // 隧道并集名单从 via 源取（None 哨兵 = 快照错误态 → 回落全局桶，fail-closed）。
    let tunnel_hosts: Option<Vec<String>> = (st.via_hosts_source)().map(|(q, n)| {
        let mut v = q;
        v.extend(n);
        v
    });
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let key = super::gate::rate_bucket_key(addr.ip(), host, &headers, tunnel_hosts.as_deref());
    // 限速时钟走注入缝（测试可推进）；设备时间戳走真实时钟（gate 滑动 TTL 域，见 persist）
    let now = (st.now_source)();
```

**改动 2 —— ① 过闸段：分来源桶 + 全局桶双查**（替换既有 ① 整段；保留「decision 先绑定出锁」与「audit 不持锁」两条既有约定）：

```rust
    // ① 分来源桶过闸：锁定期内即使 PIN 正确也拒（429），不泄露任何 PIN 有效性信息。
    let decision = st.pin_limiter.lock().unwrap().check(&key, now);
    if let RateDecision::Locked { retry_after_secs } = decision {
        super::events::audit(
            "pair_pin_locked",
            &format!("bucket={key} retry_after={retry_after_secs}s"),
        );
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({ "retryAfter": retry_after_secs })),
        )
            .into_response();
    }
    // ①b 全局桶过闸：只拦"多来源一起爆破"（分布式爆破单个来源限速挡不住）。
    // 阈值远宽于分来源桶——单个捣乱者填不满它，因此伤不到别人。
    let g = st
        .global_pin_limiter
        .lock()
        .unwrap()
        .check(crate::remote::pin::GLOBAL_BUCKET, now);
    if let RateDecision::Locked { retry_after_secs } = g {
        super::events::audit(
            "pair_pin_locked_global",
            &format!("bucket={key} retry_after={retry_after_secs}s"),
        );
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({ "retryAfter": retry_after_secs })),
        )
            .into_response();
    }
```

**改动 3 —— ③ 失败记账段：双记**（替换既有 `let remaining = { ... };` 与其后的 audit 行）：

```rust
        // 记失败与读剩余次数在同一锁临界区（限速器锁内查改，契约如此）
        let remaining = {
            let mut limiter = st.pin_limiter.lock().unwrap();
            limiter.record_failure(&key, now);
            limiter.remaining_attempts(&key)
        };
        // 全局桶同记一笔（各自阈值独立；全局桶只按时间衰减）
        st.global_pin_limiter
            .lock()
            .unwrap()
            .record_failure(crate::remote::pin::GLOBAL_BUCKET, now);
        super::events::audit("pair_pin_wrong", &format!("bucket={key} remaining={remaining}"));
```

**改动 4 —— ④ 成功清零：只清分来源桶**（替换既有 `record_success(&ip)` 那一行；**不清全局桶**——它只按时间衰减，避免"一次成功抹掉攻击者的全局账"）：

```rust
    // ④ PIN 正确：清零该分来源桶 → 设备上限门（沿用既有直通上限语义：403 + cap_full）
    st.pin_limiter.lock().unwrap().record_success(&key);
```

**改动 5 —— 全文 `&ip` / `ip={ip}` 收尾**：本函数内不再有任何 `ip` 绑定，`grep -n "\bip\b" api.rs` 在本函数体内应零命中（`addr` 仍被 `rate_bucket_key` 消费，保留）。若 `device_fingerprint` 之类还接收来源地址，传 `key` 并在注释写明它是**尽力而为的装饰值、可被伪造、不得用于安全判定**。

- [ ] **Step 8: 补两条端点级测试**

在 `src-tauri/src/remote/server.rs` 的测试模块追加：

```rust
    /// §G5 端点级验收 1：回环 + 隧道域名 + 权威头存在 → 按权威头分桶，
    /// 不同 CF-Connecting-IP 互不影响（捣乱者只填自己的桶）。
    #[tokio::test]
    async fn pair_pin_buckets_by_cf_connecting_ip_when_tunneled() {
        let (state, _t) = a3_state(Some("1234"), 3, &["mam-test.trycloudflare.com"], &[], false);
        let app = router(state);

        // 捣乱者（1.2.3.4）连错 5 次
        for _ in 0..5 {
            let r = app
                .clone()
                .oneshot(pin_post_with(
                    "127.0.0.1:40200",
                    Some("mam-test.trycloudflare.com"),
                    Some(("cf-connecting-ip", "1.2.3.4")),
                    r#"{"pin":"0000"}"#,
                ))
                .await
                .unwrap();
            assert_ne!(r.status(), 200, "错误 PIN 不得成功");
        }
        // 捣乱者被锁
        let r = app
            .clone()
            .oneshot(pin_post_with(
                "127.0.0.1:40201",
                Some("mam-test.trycloudflare.com"),
                Some(("cf-connecting-ip", "1.2.3.4")),
                r#"{"pin":"1234"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 429, "捣乱者自己的桶应已锁");
        // **本人不受影响**：换一个来源（5.6.7.8）用正确 PIN 仍可配对
        let r = app
            .oneshot(pin_post_with(
                "127.0.0.1:40202",
                Some("mam-test.trycloudflare.com"),
                Some(("cf-connecting-ip", "5.6.7.8")),
                r#"{"pin":"1234"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "他人的桶不得影响本人（§G5 核心断言）");
    }

    /// §G5 端点级验收 2：回环 + **非隧道 Host**（直连反代形态）→ 回落全局桶，
    /// 即不同 CF-Connecting-IP 也共用一个桶（fail-closed 的有意选择）。
    #[tokio::test]
    async fn pair_pin_falls_back_to_global_bucket_for_non_tunnel_host() {
        let (state, _t) = a3_state(Some("1234"), 3, &["mam-test.trycloudflare.com"], &[], false);
        let app = router(state);

        for _ in 0..5 {
            let _ = app
                .clone()
                .oneshot(pin_post_with(
                    "127.0.0.1:40300",
                    Some("mam.example.com"),
                    Some(("cf-connecting-ip", "1.2.3.4")),
                    r#"{"pin":"0000"}"#,
                ))
                .await
                .unwrap();
        }
        // 换个来源、正确 PIN——仍被锁（因为落的是同一个全局桶）
        let r = app
            .oneshot(pin_post_with(
                "127.0.0.1:40301",
                Some("mam.example.com"),
                Some(("cf-connecting-ip", "5.6.7.8")),
                r#"{"pin":"1234"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 429, "非隧道 Host 回落全局桶（fail-closed）");
    }
```

同时在测试夹具区**新增** `pin_post_with` 助手（在既有 `pin_post` 旁边）：

```rust
    /// 带自定义来源头的 POST /pair/pin（§G5 分桶测试用）
    fn pin_post_with(
        addr: &str,
        host: Option<&str>,
        extra: Option<(&str, &str)>,
        body: &str,
    ) -> axum::http::Request<Body> {
        let mut b = http_req("POST", "/m/api/v1/pair/pin", addr, host, None, None, Some(body));
        if let Some((k, v)) = extra {
            b.headers_mut().insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(v).unwrap(),
            );
        }
        b
    }
```

- [ ] **Step 9: 跑端点测试**

```bash
cd src-tauri && cargo test -- pair_pin 2>&1 | tail -25
```

Expected: 全绿（含既有 `pair_pin` 系列——若既有断言依赖「同一来源共桶」，按新分档口径改掉并注明原因）

- [ ] **Step 10: 托盘与「复制远程地址」纳入 tailscale 地址（M10，计划漏项）**

设计说明书 §C1 的输入输出表要求「设置页常驻固定地址 + 二维码；**托盘可复制**」，但托盘当前只走 `first_available_tunnel_url`（`mod.rs:1431-1446`），**不消费 tailscale 地址** → 该要求在本批与后续 Task 都**无归属**。

1. 扩展托盘取值：tailscale 地址 `Verified` 时优先（它是**永久**地址，比临时隧道的漂移地址更该上托盘），否则回落既有优先序。
2. 同步 `tray_display_from` / `tray_display` 的既有测试（`mod.rs:2163` 的 `tray_display_prefers_tunnel_url` 需扩一档 tailscale）。
3. 补一条"未验证的 tailscale 地址**不得**上托盘"的断言（与 §C3「未验证不得自称可用」同纪律）。

- [ ] **Step 10: 门禁全绿并提交**

```bash
cd src-tauri && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test 2>&1 | tail -5
git add src-tauri/src/remote/pin.rs src-tauri/src/remote/gate.rs src-tauri/src/remote/api.rs src-tauri/src/remote/server.rs
git commit -m "fix(remote): 配对限速按来源可证性分桶 + 全局桶兜底（§G5）

分档：非回环用来源地址；回环仅当 Host 命中隧道域名且 CF-Connecting-IP 存在时
用该头；其余 fail-closed 回落全局桶。不采信 True-Client-IP、不裸信 XFF。
全局桶阈值 50（分来源桶 5），宽到单个来源填不满，捣乱者只伤自己。"
```

---

### Task 4: 前端文案口径（本机也需访问密码）

**Files:**

- Modify: `src/i18n/locales/zh.json`
- Modify: `src/i18n/locales/en.json`
- Modify: `src/components/settings/RemoteSection.tsx`
- Test: `tests/settings/remoteSection.test.tsx`
- Modify: `docs/product/manual/zh/*.md`、`docs/product/manual/en/*.md`（远程接入相关章节）

**Interfaces:**

- Consumes: 无（纯文案）
- Produces: 无

> **背景**：零豁免后本机浏览器也需输入一次访问密码（设计说明书 §G1 / §C6）。用户裁决：**不论本机还是远端都应输入密码**，故这不是体验落差，而是口径要对齐。

- [ ] **Step 1: 保留历史 `local` 徽标可读（§G3 硬要求）**

`src/components/settings/RemoteSection.tsx:101` 的 `ViaBadge` 已能渲染 `local`（映射到 `settings.remote.chanLocal` = 「本机」）。服务端不再产生该值，但**历史数据里已存的 `local` 标注必须保留可读**（§G3，不做数据迁移）——因此：

1. **不要删除** `VIA_LABEL_KEY` 里的 `local` 条目。
2. **替换**第 101 行的注释为：

```tsx
// 设备行 via 徽标（装饰字段，不变量 G1 推论③：只展示，永不进安全判定）。
// quick/named/lan 由服务端按域名推断；**local 是历史值**——2026-10-06 零豁免后
// 服务端不再产生（本机不是一条通道），但历史行保留可读，故映射保留。
// 未知/缺失值不渲染——前端不猜。
```

3. 在 `tests/settings/remoteSection.test.tsx` 追加：

```tsx
it("历史 local 标注仍可读（零豁免后服务端不再产生，但历史行不得变成空白）", () => {
  renderSectionWithDevices([{ id: "d1", name: "旧手机", via: "local", lastSeen: Date.now() }]);
  expect(screen.getByText("本机")).toBeInTheDocument();
});
```

（若该测试文件尚无 `renderSectionWithDevices` 之类助手，沿用文件内既有的渲染方式，勿新造一套。）

- [ ] **Step 2: 找出现有文案键**

```bash
python3 -c "
import json
d=json.load(open('src/i18n/locales/zh.json'))['settings']['remote']
for k in ['chanLocalDesc','localDetailHint','localAddr']:
    print(k,'=',d.get(k))
"
grep -n "localDetailHint\|chanLocalDesc" src/components/settings/RemoteSection.tsx | head
```

- [ ] **Step 3: 改中文文案**

把 `settings.remote.chanLocalDesc` 从「随远程接入常驻，无需密码」改为：

```
随远程接入常驻；**本机浏览器访问同样需要访问密码**
```

把 `settings.remote.localDetailHint` 改为：

```
本机地址仅供在**这台电脑的浏览器**里访问；首次访问需输入访问密码（此后 180 天免密码）
```

（若 `chanLocalDesc` 的现值与上引不同，以实际值为准改"无需密码"这一处语义即可——**要点是不得再出现"无需密码"的说法**。）

- [ ] **Step 4: 同步英文文案**

`en.json` 里对应的两个键改为：

```
chanLocalDesc: "Always on with remote access; the browser on this computer also needs the access password"
localDetailHint: "Reachable only from this computer's browser; enter the access password on first visit (then remembered for 180 days)"
```

- [ ] **Step 5: 跑 i18n 键数门禁与前端测试**

```bash
pnpm test 2>&1 | tail -15
```

Expected: 全绿（若 `tests/settings/remoteSection.test.tsx` 断言了旧文案，按新文案改掉）

- [ ] **Step 6: 更新用户手册**

在两份语言的手册里，远程接入章节补一条：**本机浏览器访问也需要访问密码**（首次一次，此后 180 天）。

```bash
grep -rn "访问密码\|无需密码" docs/product/manual/zh docs/product/manual/en | head
```

- [ ] **Step 7: 前端门禁全绿**

```bash
pnpm format:check && pnpm lint && pnpm build 2>&1 | tail -10
```

Expected: 全绿（**必须显式跑 `pnpm build`**——tsc 门在其中）

- [ ] **Step 8: 提交**

```bash
git add src/i18n/locales/zh.json src/i18n/locales/en.json src/components/settings/RemoteSection.tsx tests/settings/remoteSection.test.tsx docs/product/manual
git commit -m "docs(remote): 文案口径对齐零豁免——本机浏览器访问同样需要访问密码"
```

---

### 第一部分验收清单（§G1–§G5 出口）

| #   | 场景                                                                                  | 预期                                                                                                                       |
| --- | ------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------- |
| 1   | 回环 + 任意 Host（`localhost` / `127.0.0.1:9420` / 隧道域名 / 自有域名 / 空）+ 无凭据 | **一律 403**（自动化：`gate_never_exempts_loopback_regardless_of_host`）                                                   |
| 2   | 同机反代免密洞（三种自有域名形态）                                                    | **403**（自动化：工作区那条红测试**由红转绿**）                                                                            |
| 3   | `/pair/pin` 无凭据可达                                                                | 非 403（自动化：`gate_still_allows_pair_pin_only`）                                                                        |
| 4   | 隧道域名 + 权威头 → 按来源分桶                                                        | 捣乱者自己的桶锁，**他人不受影响**（自动化：`pair_pin_buckets_by_cf_connecting_ip_when_tunneled`）                         |
| 5   | 非隧道 Host → 回落全局桶                                                              | 不同来源共桶、fail-closed（自动化：`pair_pin_falls_back_to_global_bucket_for_non_tunnel_host`）                            |
| 6   | 标识符零命中自证                                                                      | `grep "本机免密\|is_local_access\|conn_owner\|tunnel_channel_for_conn\|owned_channel_pids"` → **零命中**                   |
| 7   | 本机浏览器访问看板                                                                    | 出现访问密码表单；输入桌面显示的密码后进入（**人工**）                                                                     |
| 8   | 已配对设备经隧道访问                                                                  | 正常（凭据未受影响）；花名册 `via` 标注显示 quick/named/lan（**人工**）                                                    |
| 9   | 既有三通道零回归                                                                      | 局域网 / 临时隧道 / 命名隧道行为与交付前一致（**人工**）                                                                   |
| 10  | 门禁全绿                                                                              | `cargo fmt --check` / `clippy -D warnings` / `cargo test` / `pnpm test` / `pnpm build` / `pnpm format:check` / `pnpm lint` |

### 第一部分已知边界（本计划内不解决）

1. **第二部分（固定网址通道 §C1–§C6）不在本计划范围**：另行出计划；其中 §C2 的引导步骤表需先补 **Windows 实机实测**（macOS 九步 / Windows 八步，Windows 侧零实机验证）。
2. **「直连域名 + 同机反代」的配对限速落在全局桶**：这是 §G5 fail-closed 的**有意选择**（该接线下的限速与今天等价）。精细化需让用户声明"我的反代会设某个来源头"，列为后续可选。
3. **Tailscale 通道的权威来源头未定**：本计划只覆盖 Cloudflare 系（临时 / 命名隧道）。新通道落地时**必须先用同样的方法实测其入口行为**（伪造头是否被覆盖/拒绝、是否透传），再登记权威头；未验证前一律回落全局桶。
4. **设备指纹里的来源地址是装饰值**：可被伪造，**不得**用于任何安全判定；相关注释须写明。

---

# 第二部分 · 固定网址通道（§C1–§C6）

> **前置**：第一部分的 Task 1–4 **全部通过**（§C6 要求新增通道在门禁侧**零接线**——若 Task 5 需要改 `gate.rs` 的鉴权分支，说明第一部分没做干净）。
> **执行顺序**：Task 5 → 6 → 7 → **8（线稿，先于 9）** → 9 → 10 → 11；**Task 12 可延后**，不阻塞任何任务。
> **本部分的共同约束（在第一部分 Global Constraints 之上）**：
>
> - **不打包、不再分发 Tailscale 安装包**（其 macOS/Windows 图形安装包许可为「不可转让」且禁止复制再分发）——只从官方源按需下载并校验。
> - **不接触任何用户凭据**：登录一律由用户在浏览器完成，MAM 只把授权链接做成按钮。
> - **不静默提权**：安装触发的系统授权框是预期行为，不得绕过。
> - **不覆盖用户已存在的 Tailscale serve/Funnel 配置**（见 Task 5 Step 6 的守卫）。
> - **引导必须能自检每一步**：卡住时要能说出卡在哪一步。

### Task 5: Tailscale 通道本体（§C1）

**Files:**

- Create: `src-tauri/src/remote/tailscale.rs`
- Modify: `src-tauri/src/remote/mod.rs`
- Modify: `src-tauri/src/remote/gate.rs`
- Modify: `src-tauri/src/remote/server.rs`
- Test: `src-tauri/src/remote/tailscale.rs`（内嵌 `mod tests`）、`src-tauri/src/remote/mod.rs`（内嵌 tests）、`src-tauri/src/remote/server.rs`（内嵌 tests）

**Interfaces:**

- Consumes: Task 1 的 `gate::normalize_host`；Task 2 保留的 `via_hosts_from_status` / `channel_hosts` / `host_of_board_url`；`tunnel::ChannelStatus`
- Produces:
  - `mod::ChannelKind::Tailscale`；`mod::ChannelFlags.tailscale: bool`；`mod::KEY_CHAN_TAILSCALE = "remote.chan_tailscale"`
  - `tailscale::find_cli() -> Option<std::path::PathBuf>`（**探测**，与 `tunnel::cloudflared_path()` 语义不同——后者是"应该放哪"的**位置**，不判存在）
  - `tailscale::run_cli(args: &[&str]) -> Result<String, String>`
  - `tailscale::parse_status(json: &str) -> Result<TsStatus, String>`，`TsStatus { backend_state: String, auth_url: String, dns_name: String, cert_domains: Vec<String> }`
  - `tailscale::funnel_active(json: &str) -> bool`（`{}` → false）
  - `tailscale::board_url_from_dns_name(dns: &str) -> String`（**去结尾点** + 补 `/m`）
  - `tailscale::ts_snapshot() -> tunnel::ChannelStatus`；`pub(crate) tailscale::set_ts_snapshot(f: impl FnOnce(&mut tunnel::ChannelStatus))`（测试缝；`pub(crate)` 与 `tunnel::set_snapshot`（`tunnel.rs:306`）同级）
  - **通道生命周期（三个公开入口，与 `tunnel::{start_channel,stop_channel,stop_all}` 对称）**：
    `pub fn start_channel(port: u16) -> Result<(), String>`、`pub fn stop_channel()`、`pub fn stop_all()`
  - **向导单入口**：`pub fn run_step(step: &str, port: u16) -> Result<serde_json::Value, String>`
    （向导的每一步都走这一个口，**不逐个暴露底层原语**——底层 `enable_funnel` / `disable_funnel` / `disable_shields_up` 均为模块私有）
  - 资产表（**命名对齐 `tunnel.rs` 既有约定** `asset_name()` / `expected_sha256(asset)` / `download_url_for()`）：
    `tailscale::ts_asset_name(p: Platform) -> &'static str`、
    `tailscale::ts_download_url(p: Platform) -> String`、
    `tailscale::ts_expected_sha256(asset: &str) -> Result<&'static str, String>`
  - `gate::classify_via` 新增 `"tailscale"` 分支
  - **`mod::channels_payload` 签名变化**（Task 7 会再加一个参数，两处都要同步改调用与测试）：
    `channels_payload(listener_alive: bool, flags: ChannelFlags, port: u16, lan_addresses: Vec<String>, tun: tunnel::TunnelStatus, ts: tunnel::ChannelStatus) -> serde_json::Value`
    （Task 7 追加第 7 个参数 `reach: tailscale::Reachability`）

> **架构差异（务必先读，别照抄 cloudflared 的守护模型）**：`cloudflared` 是**前台子进程**，故 `tunnel.rs` 用 `Command::spawn` + `kill_on_drop` + `child.wait()` 守护。**Tailscale Funnel 不是**——`tailscale funnel --bg` **立即返回**，配置由常驻的 `tailscaled` 服务持有，**没有子进程可守**。所以本通道的守护模型是**周期轮询**：起停都发 CLI 命令，运行态由轮询 `status --json` / `funnel status --json` 现算写进快照。**不要**为此通道引入 `child_pid`（Task 2 已把它删掉）。
>
> **已实测的字段契约（2026-10-06，macOS 1.102.4 只读命令）**：`BackendState == "Running"` = 已登录在线；`AuthURL` 登录后为空串、**待登录时带授权链接**；**`Self.DNSName` 带结尾点**（`xxx.ts.net.`），`CertDomains[0]` 同值**不带点**；`funnel status --json` 未开通时是 **`{}`**；`get --json` 里该偏好项键名精确为 **`shields-up`**。

- [ ] **Step 1: 写纯函数测试（红）**

在新建的 `src-tauri/src/remote/tailscale.rs` 末尾写入测试模块（**先只有测试与函数签名桩**）：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// 实测样本（2026-10-06，macOS 1.102.4 真机 `status --json` 截取关键字段）。
    /// 注意 `Self.DNSName` **带结尾点**——这是解析必须处理的真实形态。
    const STATUS_RUNNING: &str = r#"{
        "BackendState": "Running",
        "AuthURL": "",
        "Self": { "DNSName": "jarvismac-mini.example-tailnet.ts.net." },
        "CertDomains": ["jarvismac-mini.example-tailnet.ts.net"]
    }"#;

    /// 待登录形态：AuthURL 带链接（MAM 要把它做成按钮）
    const STATUS_NEEDS_LOGIN: &str = r#"{
        "BackendState": "NeedsLogin",
        "AuthURL": "https://login.tailscale.com/a/abc123",
        "Self": { "DNSName": "" },
        "CertDomains": []
    }"#;

    #[test]
    fn parse_status_reads_backend_state_and_auth_url() {
        let s = parse_status(STATUS_RUNNING).expect("运行态应可解析");
        assert_eq!(s.backend_state, "Running");
        assert!(s.auth_url.is_empty(), "已登录时 AuthURL 应为空串");

        let s = parse_status(STATUS_NEEDS_LOGIN).expect("待登录态应可解析");
        assert_eq!(s.backend_state, "NeedsLogin");
        assert_eq!(s.auth_url, "https://login.tailscale.com/a/abc123", "授权链接必须能取到（做成按钮用）");
    }

    #[test]
    fn parse_status_is_total_on_garbage() {
        // 不 panic、不猜：坏 JSON / 缺字段一律 Err（引导要如实报"读不到状态"）
        assert!(parse_status("not json").is_err());
        assert!(parse_status("{}").is_err(), "缺 BackendState 应 Err 而非默认 Running");
    }

    #[test]
    fn funnel_active_only_when_config_non_empty() {
        assert!(!funnel_active("{}"), "空对象 = 未伺服（实测形态）");
        assert!(!funnel_active(""));
        assert!(!funnel_active("not json"), "解析不了按未激活处理（fail-closed）");
        assert!(funnel_active(r#"{"TCP":{"443":{"HTTPS":true}},"Web":{"x.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:9420"}}}}}"#));
    }

    #[test]
    fn board_url_from_dns_name_strips_trailing_dot_and_appends_m() {
        // 契约（与既有通道一致）：快照 url 恒为含 /m 的看板完整地址
        assert_eq!(
            board_url_from_dns_name("jarvismac-mini.example-tailnet.ts.net."),
            "https://jarvismac-mini.example-tailnet.ts.net/m"
        );
        assert_eq!(
            board_url_from_dns_name("jarvismac-mini.example-tailnet.ts.net"),
            "https://jarvismac-mini.example-tailnet.ts.net/m",
            "不带点的形态也要能处理（CertDomains 就不带点）"
        );
    }

    #[test]
    fn board_url_from_dns_name_is_empty_safe() {
        assert_eq!(board_url_from_dns_name(""), "", "无名字时给空串，不要造出 https:///m");
        assert_eq!(board_url_from_dns_name("."), "");
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd src-tauri && cargo test tailscale 2>&1 | tail -20
```

Expected: FAIL（`cannot find function parse_status` / `funnel_active` / `board_url_from_dns_name`）

- [ ] **Step 3: 实现纯函数（绿）**

在 `tailscale.rs` 顶部实现（**不碰网络、不碰进程，纯函数**——符合项目「零网络/零 `~/.mam`」测试红线）：

```rust
//! Tailscale 通道（§C1「固定网址 · 免域名」）：驱动**用户本机已安装的** Tailscale CLI
//! 开通 Funnel，把 `https://<机器名>.<尾网名>.ts.net/m` 作为永久看板地址。
//!
//! **与 tunnel.rs 的架构差异（勿照抄）**：cloudflared 是前台子进程（spawn + kill_on_drop +
//! wait 守护）；`tailscale funnel --bg` **立即返回**，配置由常驻 tailscaled 服务持有，
//! **没有子进程可守**。故本模块的守护模型是**周期轮询**，运行态由 CLI 状态现算。
//!
//! **不持有任何用户凭据**：登录由用户在浏览器完成，本模块只把 `AuthURL` 递出去。

use std::path::PathBuf;

/// `status --json` 的关键字段（只取需要的，不做全量建模）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TsStatus {
    /// 实测取值：`Running`（已登录在线）/ `NeedsLogin` / `Stopped` 等
    pub backend_state: String,
    /// 待登录时的授权链接；已登录为空串。**MAM 把它做成按钮，不自己登录**
    pub auth_url: String,
    /// **带结尾点**（实测 `xxx.ts.net.`）——使用前必须归一
    pub dns_name: String,
    /// 不带结尾点的同值域名（实测 `CertDomains[0]`）
    pub cert_domains: Vec<String>,
}

/// 解析 `tailscale status --json`。**坏 JSON / 缺关键字段一律 Err**——
/// 引导要能如实报「读不到状态」，不得默认成 Running（那会把失败伪装成成功）。
pub fn parse_status(json: &str) -> Result<TsStatus, String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| format!("状态非 JSON: {e}"))?;
    let backend_state = v
        .get("BackendState")
        .and_then(|x| x.as_str())
        .ok_or_else(|| "状态缺少 BackendState".to_string())?
        .to_string();
    Ok(TsStatus {
        backend_state,
        auth_url: v.get("AuthURL").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        dns_name: v
            .get("Self")
            .and_then(|s| s.get("DNSName"))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        cert_domains: v
            .get("CertDomains")
            .and_then(|x| x.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default(),
    })
}

/// Funnel 是否已伺服。实测：未开通时 `funnel status --json` 是 **`{}`**。
/// 解析不了按「未激活」处理（fail-closed——宁可说没开，也不谎称已开）。
pub fn funnel_active(json: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|v| v.as_object().map(|o| !o.is_empty()))
        .unwrap_or(false)
}

/// 由域名生成看板地址（**契约：恒为含 `/m` 的完整地址**）。
/// **复用 `tunnel::board_url`（`tunnel.rs:357`）做归一，不要重造**——它已实现
/// "去尾斜杠 + 补 `/m`" 且是幂等的，并有既有测试覆盖。
/// 本函数只多做一件事：**去掉实测存在的结尾点**（`Self.DNSName` 是 `xxx.ts.net.`）。
pub fn board_url_from_dns_name(dns: &str) -> String {
    let host = dns.trim().trim_end_matches('.');
    if host.is_empty() {
        return String::new();
    }
    crate::remote::tunnel::board_url(&format!("https://{host}"))
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd src-tauri && cargo test tailscale 2>&1 | tail -12
```

Expected: PASS（4 条）

- [ ] **Step 5: 扩展通道枚举与 KV（三处 + 一处，缺一不可）**

地图已确认新通道**必须同时扩这四处**，否则编译或解析会缺角：

1. `mod.rs:69-73` `ChannelFlags` 加 `pub tailscale: bool`。
2. `mod.rs:77-81` `ChannelKind` 加 `Tailscale` 变体；`parse`（`:85-92`）认字面量 `"tailscale"`；`as_str`（`:94-100`）返回 `"tailscale"`；`flag`（`:102-108`）返回 `flags.tailscale`。
3. `mod.rs:35-37` 旁加 `pub const KEY_CHAN_TAILSCALE: &str = "remote.chan_tailscale";`（值口径沿用 `"1"`/`"0"`）。
4. `read_channels`（`mod.rs:136-160`）读新键；`write_chan_flag`（`:163-170`）写新键。

补测试（`mod.rs` tests 内，与既有 `channel_kind_parse_rejects_unknown_values` `:2603` 同格式）：

```rust
    #[test]
    fn channel_kind_parses_tailscale() {
        assert_eq!(ChannelKind::parse("tailscale").unwrap(), ChannelKind::Tailscale);
        assert_eq!(ChannelKind::Tailscale.as_str(), "tailscale");
        assert!(ChannelKind::Tailscale.flag(ChannelFlags { tailscale: true, ..Default::default() }));
        assert!(!ChannelKind::Tailscale.flag(ChannelFlags::default()));
    }
```

- [ ] **Step 6: 实现快照与起停（含「不覆盖用户既有配置」守卫）**

在 `tailscale.rs` 追加（**进程与网络的唯一入口**，全部经 `run_cli` 便于测试替换）：

```rust
/// CLI 路径探测：装了才返回 Some。**只探测标准安装位置，不做全盘搜索。**
pub fn find_cli() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    const CANDIDATES: &[&str] = &["/Applications/Tailscale.app/Contents/MacOS/Tailscale"];
    #[cfg(target_os = "windows")]
    const CANDIDATES: &[&str] = &[r"C:\Program Files\Tailscale\tailscale.exe"];
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    const CANDIDATES: &[&str] = &["/usr/bin/tailscale", "/usr/local/bin/tailscale"];
    CANDIDATES.iter().map(PathBuf::from).find(|p| p.exists())
}

/// 统一的 CLI 调用口。macOS 的 GUI 版兼作 CLI，**必须置 `TAILSCALE_BE_CLI=1`**
/// 才会走命令分发（实测）；Windows 是独立 CLI，无此需求。
/// **绝不 sudo**：本模块用到的命令都不需要管理员权限。
pub fn run_cli(args: &[&str]) -> Result<String, String> {
    let bin = find_cli().ok_or_else(|| "未检测到 Tailscale（尚未安装）".to_string())?;
    let mut cmd = std::process::Command::new(bin);
    cmd.args(args);
    #[cfg(target_os = "macos")]
    cmd.env("TAILSCALE_BE_CLI", "1");
    let out = cmd.output().map_err(|e| format!("调用 Tailscale 失败: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// 快照（**复用 `tunnel::ChannelStatus` 这个通用形状** {running,url,error}）。
///
/// **为什么不把 tailscale 塞进 `tunnel::TunnelStatus`**（测绘报告曾建议扩那个结构）：
/// `TunnelStatus` 装的是**隧道**（有前台子进程、有 stderr 地址解析、有退避重启），
/// 而本通道**没有子进程**、状态来自 CLI 查询——塞进去会让一个语义明确的类型变杂物箱，
/// 并让 tailscale 的运行态写入必须绕经 `tunnel.rs`。这里**复用的是类型与测试缝形态**，
/// 不是那个容器。`channels_payload` 因此多收一个参数（已在其签名里写明）。
static TS_SNAPSHOT: once_cell::sync::Lazy<std::sync::Mutex<crate::remote::tunnel::ChannelStatus>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(crate::remote::tunnel::ChannelStatus::default()));

pub fn ts_snapshot() -> crate::remote::tunnel::ChannelStatus {
    TS_SNAPSHOT.lock().unwrap().clone()
}

/// 测试注入缝（与 `tunnel::set_channel_snapshot` 同形）
pub fn set_ts_snapshot(f: impl FnOnce(&mut crate::remote::tunnel::ChannelStatus)) {
    f(&mut TS_SNAPSHOT.lock().unwrap());
}
```

**「不覆盖用户既有 serve 配置」守卫**（写进 `start_channel` 的第一步，并单独测）：

```rust
/// 开通前的守卫：若**当前已有 serve/Funnel 配置**且**不是 MAM 自己那份**，
/// 拒绝开通并如实报错——`funnel reset` 会清掉整份 serve 配置，静默清掉用户的
/// 自建配置是不可接受的副作用。
/// 判据：配置里是否有一个 handler 指向 `http://127.0.0.1:<本机 MAM 端口>`。
pub fn foreign_serve_config(funnel_json: &str, mam_port: u16) -> bool {
    if !funnel_active(funnel_json) {
        return false; // 本来就没有配置，不算外来
    }
    let ours = format!("http://127.0.0.1:{mam_port}");
    let Ok(v) = serde_json::from_str::<serde_json::Value>(funnel_json) else {
        return true; // 解析不了又非空 → 保守视为外来（fail-closed，不覆盖）
    };
    !v.to_string().contains(&ours)
}
```

- [ ] **Step 7: 接线（开关 / 恢复 / 载荷 / via）**

地图给出的接线点，**逐个接、一个都不能漏**：

1. `toggle_channel_core`（`mod.rs:1212-1258`）：`(k, o)` 的兜底分支当前一律 `tunnel(k,o)`（`:1253-1256`）。改为 Tailscale 走**新闭包**：给 `ChannelToggleDeps`（`mod.rs:1192-1202`）加第五个参数 `ts: impl FnMut(ChannelKind, bool)`，match 里 `(ChannelKind::Tailscale, on) => ts(kind, on)`。**测试要同步补 spy**：断言「切 Tailscale 不得触碰 `tunnel` 闭包」，与既有 `toggle_channel_core_tunnel_lifecycle_gated_on_master`（`:2771`）同格式。
2. 生产接线（`mod.rs:1278-1290`）：`ts` 闭包 = `on ? tailscale::start_channel(port) : tailscale::stop_channel()`（端口取 `bind_and_port()` 的 port，与 tunnel 同源）。
3. `restore_tunnels_core`（`mod.rs:702-709`）：现只 ensure quick/named，**加上 tailscale**（否则开机不恢复）。同步改其测试 `restore_tunnels_core_only_ensures_enabled_channels`（`:2837`）。
4. `channels_payload`（`mod.rs:861-890`）：新增 `tailscale` 段，形状与既有通道一致（`{enabled,running,address,error}`），地址同样**双门**：`url.filter(|_| error.is_none() && running)`（照 `:872-873`）。其形状契约测试 `channels_payload_shape_contract`（`:2893`）**必须同步扩**。
5. `remote_status`（`mod.rs:793-832`）：把 ts 快照喂给 `channels_payload`。
6. `gate::classify_via`（`gate.rs:51-73`）：加 `"tailscale"` 分支（否则**新通道连接会被设备花名册误标成「局域网」**——地图明确指出的坑）。`via_hosts_from_status`（`mod.rs:418-424`）相应扩为三元组或加一路；`classify_via` 的返回值域随 `§C4` 变为**四值**：`quick` / `named` / `tailscale` / `lan`。
7. 前端类型 `src/lib/api/remote.ts:24-36` `RemoteChannels` 加 `tailscale` 字段（**本 Task 只加类型，卡片在 Task 9**）。

- [ ] **Step 8: 补一条「生产接线探针」**

地图指出的纪律：`mod.rs:1478-1496` 有一条 `production_state_home_source_is_wired_to_real_home`，专防「端点测试自己注入真值、掩盖生产漏接」。本 Task 若给 `RemoteState` 加了任何生产装配字段（例如把 ts 快照源做成注入缝），**必须补同类探针**：

```rust
    /// 契约守卫：生产装配的 tailscale 快照源必须真的可调用（不是占位闭包）。
    /// 防「端点测试自注入真值 → 掩盖生产漏接」这一类缺陷。
    #[test]
    fn production_tailscale_source_is_wired() {
        // 只断言可调用且不 panic；不绑定真机、不触网络
        let s = crate::remote::tailscale::ts_snapshot();
        let _ = s.running; // 形状可用即可
    }
```

- [ ] **Step 9: 跑全部 Rust 测试**

```bash
cd src-tauri && cargo test 2>&1 | tail -20
```

Expected: 全绿（含 Task 2 删除后减少的用例数）

- [ ] **Step 11: 门禁全绿并提交**

```bash
cd src-tauri && cargo fmt --check && cargo clippy --all-targets -- -D warnings
git add src-tauri/src/remote/tailscale.rs src-tauri/src/remote/mod.rs src-tauri/src/remote/gate.rs src-tauri/src/remote/server.rs
git commit -m "feat(remote): Tailscale 通道本体——CLI 探测/状态解析/Funnel 起停与轮询守护（§C1）

与 cloudflared 的架构差异：Funnel 由 tailscaled 持有、无子进程，守护模型为周期轮询。
含「不覆盖用户既有 serve 配置」守卫。via 新增 tailscale 分支，避免误标为局域网。"
```

> **本 Task 不做**（留给后续）：安装/下载/引导（Task 6）、可达性校验（Task 7）、前端卡片（Task 9）。做完本 Task 时，`remote_status` 会多出一个 `tailscale` 段而前端尚未消费——这是预期的中间态。

---

### Task 6: 首次配置引导（一条龙）（§C2）

**Files:**

- Modify: `src-tauri/src/remote/tailscale.rs`（步骤表 / 资产表 / 供给动作）
- Modify: `src-tauri/src/remote/tunnel.rs`（把 `verify_sha256` 改 `pub(crate)`，抽出可传 URL 的下载器）
- Modify: `src-tauri/src/remote/mod.rs`（三条 IPC 命令）
- Modify: `src-tauri/src/lib.rs`（命令注册表 `:274-286`）
- Create: `src/components/settings/TailscaleWizard.tsx`
- Modify: `src/components/settings/RemoteSection.tsx`、`src/lib/api/remote.ts`、`src/i18n/locales/{zh,en}.json`
- Test: `tailscale.rs` 内嵌 tests、`tests/settings/tailscaleWizard.test.tsx`

**Interfaces:**

- Consumes: Task 5 的 `find_cli` / `run_cli` / `parse_status` / `funnel_active` / `foreign_serve_config`；`tunnel::verify_sha256`
- Produces:
  - `tailscale::Platform`（`Mac` / `Windows` / `Other`）、`tailscale::current_platform() -> Platform`
  - `tailscale::WizardStep { id: &'static str, needs_human: bool, human_action_key: &'static str }`
  - `tailscale::wizard_steps(Platform) -> Vec<WizardStep>`（**macOS 9 步 / Windows 8 步**）
  - `tailscale::StepState { id: &'static str, done: bool, blocked_reason: Option<String> }`
  - `tailscale::probe_steps(port: u16) -> Vec<StepState>`
  - `tailscale::ts_asset_name(p) -> &'static str`、`tailscale::ts_download_url(p) -> String`、`tailscale::ts_expected_sha256(asset) -> Result<&'static str, String>`
  - `mod::remote_ts_probe() -> Result<serde_json::Value, String>`；`mod::remote_ts_run_step(step: String) -> Result<serde_json::Value, String>`
  - 前端 `<TailscaleWizard />`

> **两条必须遵守的既有范式（照抄，别自创）**：
>
> 1. **固定版本 + 硬编码 sha256 表**——`tunnel.rs:42-47` 已确立此范式（"固定版本是 sha256 校验与镜像回退不引入…改本常量 + 逐项更新 `expected_sha256` 表，登记于设计文档 §12.5"）。**不要**去抓官方索引页取"最新版"——那是 TOFU（信任首次抓取），**比固定版本弱**。sha256 在实施时用 `curl <url>.sha256` 取一次、粘进表里。
> 2. **复用 `tunnel::verify_sha256`**（`tunnel.rs:103`）——它已存在且有测试（`:986-996`）。**不要新写一套校验**。下载同理：复用/抽出 `tunnel.rs` 的 reqwest 流式下载（`:234 download_to`），抽成接受 URL 的通用版本，**不要重复实现**。

- [ ] **Step 1: 写步骤表与资产表的测试（红）**

```rust
    /// 平台差异**只有一个**：macOS 多一步「批准系统扩展」。这条锁死「Windows = macOS - 1 步」。
    #[test]
    fn windows_has_exactly_one_fewer_step_than_macos() {
        let mac = wizard_steps(Platform::Mac);
        let win = wizard_steps(Platform::Windows);
        assert_eq!(mac.len(), 9, "macOS 九步（已实测）");
        assert_eq!(win.len(), 8, "Windows 八步（预填，待实测校验）");
        assert!(mac.iter().any(|s| s.id == "sys_ext"), "macOS 必须有系统扩展步");
        assert!(!win.iter().any(|s| s.id == "sys_ext"), "Windows 不得有系统扩展步");
        // 其余八步 id 序列必须完全一致（防止两平台各自漂移）
        let strip = |v: &[WizardStep]| v.iter().filter(|s| s.id != "sys_ext").map(|s| s.id).collect::<Vec<_>>();
        assert_eq!(strip(&mac), strip(&win));
    }

    /// 每个需要人的步骤都必须带一个可展示的动作文案键——否则引导会变成"卡住但不说为什么"。
    #[test]
    fn human_steps_all_carry_an_action_key() {
        for p in [Platform::Mac, Platform::Windows] {
            for s in wizard_steps(p) {
                if s.needs_human {
                    assert!(!s.human_action_key.is_empty(), "{} 的需要人步骤缺文案键", s.id);
                }
            }
        }
    }

    /// 人工确认数：macOS ≤4 / Windows ≤3（设计说明书 §C2 的出口标准）。
    #[test]
    fn human_step_counts_match_spec() {
        assert_eq!(wizard_steps(Platform::Mac).iter().filter(|s| s.needs_human).count(), 4);
        assert_eq!(wizard_steps(Platform::Windows).iter().filter(|s| s.needs_human).count(), 3);
    }

    /// 安装包资产表：平台齐备且 sha256 是 64 位十六进制（防手滑粘错）。
    #[test]
    fn installer_asset_table_is_complete_and_well_formed() {
        for p in [Platform::Mac, Platform::Windows] {
            let asset = ts_asset_name(p);
            let url = ts_download_url(p);
            assert!(url.starts_with("https://pkgs.tailscale.com/stable/"), "只允许官方源");
            assert!(url.ends_with(asset), "URL 末段必须等于资产名（与 tunnel::download_url_for 同约定）");
            assert!(url.ends_with(".pkg") || url.ends_with(".msi"), "macOS=.pkg / Windows=.msi");
            // 实测锚点：照索引页顶部"最新版"(1.102.5) 拼 URL 会 404——那是静态 Linux 包的版本
            assert!(!url.contains("1.102.5"), "Windows 1.102.5 实测 404");
            let h = ts_expected_sha256(asset).expect("必须有固定校验值");
            assert_eq!(h.len(), 64, "sha256 必须 64 位");
            assert!(h.chars().all(|c| c.is_ascii_hexdigit()), "sha256 必须是十六进制");
        }
        // 未知资产必须 Err（不返回空串——那是 fail-open）
        assert!(ts_expected_sha256("nope.msi").is_err());
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd src-tauri && cargo test tailscale 2>&1 | tail -20
```

Expected: FAIL（`cannot find function wizard_steps` 等）

- [ ] **Step 3: 实现步骤表与资产表（绿）**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform { Mac, Windows, Other }

pub fn current_platform() -> Platform {
    if cfg!(target_os = "macos") { Platform::Mac }
    else if cfg!(target_os = "windows") { Platform::Windows }
    else { Platform::Other }
}

/// 引导步骤（**平台数据，不是写死的流程**）。`id` 是稳定的机器标识，UI 文案走 i18n。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WizardStep {
    pub id: &'static str,
    pub needs_human: bool,
    /// 需要人时展示的动作文案键（i18n），例如 "settings.remote.tsWizard.actAdminPwd"
    pub human_action_key: &'static str,
}

/// macOS 九步（**已实测**）/ Windows 八步（**预填，待实测校验**）。
/// Windows 行的依据 = 官方文档 + MSI 行为调研，**零实机验证**；
/// 探测提示词见 `docs/engineering/plans/2026-10-06-windows-tailscale-probe-prompt.md`，
/// 回填后把 `WINDOWS_VERIFIED` 置 true 并在设计文档 §C2 追记。
pub fn wizard_steps(p: Platform) -> Vec<WizardStep> {
    let mut v = vec![
        WizardStep { id: "detect",     needs_human: false, human_action_key: "" },
        WizardStep { id: "download",   needs_human: false, human_action_key: "" },
        WizardStep { id: "install",    needs_human: true,  human_action_key: "settings.remote.tsWizard.actAdmin" },
    ];
    if p == Platform::Mac {
        // macOS 独有：系统扩展需去「系统设置」点一次允许（Apple 规定只有 MDM 能预批准，而 MDM 描述文件又禁止用户自行安装）
        v.push(WizardStep { id: "sys_ext", needs_human: true, human_action_key: "settings.remote.tsWizard.actSysExt" });
    }
    v.extend([
        WizardStep { id: "login",      needs_human: true,  human_action_key: "settings.remote.tsWizard.actLogin" },
        WizardStep { id: "shields_up", needs_human: false, human_action_key: "" },
        WizardStep { id: "funnel",     needs_human: true,  human_action_key: "settings.remote.tsWizard.actFunnel" },
        WizardStep { id: "verify",     needs_human: false, human_action_key: "" },
        WizardStep { id: "autostart",  needs_human: false, human_action_key: "" },
    ]);
    v
}

/// **固定版本**（照 `tunnel.rs:42-47` 范式；升级 = 改常量 + 重取哈希 + 设计文档追记）。
/// ⚠️ **实测陷阱（2026-10-06）**：索引页**顶部**那串「latest 1.102.5 1.102.4 …」
/// **指的是静态 Linux 包，不是 macOS/Windows 包**——照它去拼 Windows MSI 会 **404**。
/// **必须读页面对应 OS 段落里的文件名**。（当时 macOS 与 Windows 恰好都是 1.102.4。）
const TS_VER_MAC: &str = "1.102.4";
const TS_VER_WIN: &str = "1.102.4";

/// MSI **分架构**——选错架构会装不上
fn win_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") { "arm64" }
    else if cfg!(target_arch = "x86") { "x86" }
    else { "amd64" }
}

/// 资产名（**与 `tunnel::asset_name()` 同形**：一个平台的安装包文件名）
pub fn ts_asset_name(p: Platform) -> &'static str {
    match p {
        Platform::Mac => "Tailscale-1.102.4-macos.pkg",
        Platform::Windows => match win_arch() {
            "arm64" => "tailscale-setup-1.102.4-arm64.msi",
            "x86" => "tailscale-setup-1.102.4-x86.msi",
            _ => "tailscale-setup-1.102.4-amd64.msi",
        },
        Platform::Other => "",
    }
}

/// 下载地址（**与 `tunnel::download_url_for()` 同形**）
pub fn ts_download_url(p: Platform) -> String {
    format!("https://pkgs.tailscale.com/stable/{}", ts_asset_name(p))
}

/// 固定校验值。**全部于 2026-10-06 实测取自官方 `<文件URL>.sha256`**（官方明示 URL 后
/// 追加 `.sha256` 即得校验值；实测对 `.pkg` 与 `.msi` **均有效**；四个文件的 HTTP
/// 可达性也已实测——macOS .pkg 与三个 MSI 全部 200）。
/// 与 `tunnel.rs` 的 `expected_sha256` 表同一纪律：**改版本必须同步改这里**。
const SHA_MAC: &str = "b40b733af76233fd1e4af7acaeb325268e55e6818c15c6e9aa9e78f427245c5b";
const SHA_WIN_AMD64: &str = "80eb007e39dfebe17299fa1a09c79a8e1d934f76e0246c0817ebe3af675b7ef6";
const SHA_WIN_ARM64: &str = "b7dd1c03bf2e2c430f1fffc4e47ef92829c86d5190febbd9c025dcada5f410b6";
const SHA_WIN_X86: &str = "a8bda9fb254374bb13d46ebf02b6ffba4ed009a739580be511aa7afa8dddd42d";

/// 按资产名查固定校验值（**与 `tunnel::expected_sha256(asset)`（`tunnel.rs:69`）同形**：
/// 未知资产返回 Err，不返回空串——那是 fail-open）
pub fn ts_expected_sha256(asset: &str) -> Result<&'static str, String> {
    match asset {
        "Tailscale-1.102.4-macos.pkg" => Ok(SHA_MAC),
        "tailscale-setup-1.102.4-amd64.msi" => Ok(SHA_WIN_AMD64),
        "tailscale-setup-1.102.4-arm64.msi" => Ok(SHA_WIN_ARM64),
        "tailscale-setup-1.102.4-x86.msi" => Ok(SHA_WIN_X86),
        other => Err(format!("未知安装包资产: {other}")),
    }
}
```

> **上面的版本与四个哈希都是 2026-10-06 实取的真值**（不是占位符）。升级版本时按注释里的 `<文件URL>.sha256` 重取即可；**取不到就停下报告**，不要填假值让校验形同虚设。

- [ ] **Step 4: 复用下载与校验（不新写）**

1. `tunnel.rs:103` 的 `fn verify_sha256` 改为 `pub(crate) fn verify_sha256`（其测试 `:986-996` 不动）。
2. 从 `tunnel.rs` 抽出接受 URL 的下载器（现 `download_to` `:234` 的 URL 是 cloudflared 硬编码）：新增 `pub(crate) fn download_url_to(url: &str, dest: &Path) -> Result<(), String>`，沿用既有 reqwest 流式写盘与错误口径，`download_to` 改为调用它。
3. 补一条纯函数测试，确认「错误的哈希被拒」（沿用 `tunnel.rs:986` 的 HELLO_SHA 手法）：

```rust
    #[test]
    fn installer_hash_rejects_wrong_bytes() {
        assert!(crate::remote::tunnel::verify_sha256(b"hello", "00".repeat(32).as_str()).is_err());
    }
```

- [ ] **Step 5: 安装触发（平台分支，**不静默提权**）**

```rust
/// 触发安装。**必须让系统弹授权框**——要自动化就得让 MAM 常驻管理员，
/// 那正是本项目安全上要避免的（设计说明书 §C2「安全红线」）。
pub fn run_installer(pkg: &std::path::Path) -> Result<(), String> {
    match current_platform() {
        // macOS：installer 自身会弹「输入管理员密码」
        Platform::Mac => {
            let out = std::process::Command::new("installer")
                .arg("-pkg").arg(pkg).arg("-target").arg("/")
                .output().map_err(|e| format!("调用 installer 失败: {e}"))?;
            if !out.status.success() {
                return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
            }
            Ok(())
        }
        // Windows：msiexec 弹 UAC（per-machine 安装必然提权）。**不加 /qn**——
        // 静默参数是否可用仍在探测中（见探测提示词第 3 步），未实测前一律走交互式。
        Platform::Windows => {
            let out = std::process::Command::new("msiexec")
                .arg("/i").arg(pkg)
                .output().map_err(|e| format!("调用 msiexec 失败: {e}"))?;
            if !out.status.success() {
                return Err(format!("msiexec 退出码 {:?}", out.status.code()));
            }
            Ok(())
        }
        Platform::Other => Err("当前平台暂不支持一键配置".into()),
    }
}
```

- [ ] **Step 6: 关闭 shields-up + 开通 Funnel（含守卫与批准链接捕获）**

```rust
/// 关闭「阻止传入连接」。偏好项键名 **已实测为 `shields-up`**（`get --json`）；
/// **写入侧尚未实测**（探测提示词第 6 步），故写完必须回读确认，读不回就如实报错。
pub fn disable_shields_up() -> Result<(), String> {
    run_cli(&["set", "--shields-up=false"])?;
    let after = run_cli(&["get", "--json"])?;
    let v: serde_json::Value = serde_json::from_str(&after).map_err(|e| format!("回读偏好失败: {e}"))?;
    match v.get("shields-up").and_then(|x| x.as_bool()) {
        Some(false) => Ok(()),
        Some(true) => Err("「阻止传入连接」仍为开启——写入未生效".into()),
        None => Err("回读不到 shields-up 字段，无法确认已关闭".into()),
    }
}

/// 开通 Funnel（端口，不是文件伺服——macOS GUI 版支持端口 Funnel）。
/// 首次开通时上游会要求一次浏览器批准；**把链接递给用户**，MAM 不代点。
pub fn enable_funnel(port: u16) -> Result<Option<String>, String> {
    // 守卫：不覆盖用户既有的 serve/Funnel 配置
    let before = run_cli(&["funnel", "status", "--json"]).unwrap_or_default();
    if foreign_serve_config(&before, port) {
        return Err("检测到已有的 Tailscale serve/Funnel 配置，且不是本应用创建的。\
                    为避免清掉你的自建配置，已停止开通——请先手动确认。".into());
    }
    run_cli(&["funnel", "--bg", &port.to_string()])?;
    // 若未立即生效，通常是等待首次批准；把批准链接（若有）交回前端
    Ok(extract_approval_url(&run_cli(&["funnel", "status"]).unwrap_or_default()))
}

/// 从 CLI 输出里捞批准链接（纯函数，可测）。
pub fn extract_approval_url(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|w| w.starts_with("https://") && w.contains("tailscale.com"))
        .map(|w| w.trim_end_matches(['.', ',', ')']).to_string())
}
```

- [ ] **Step 7: 三条 IPC 命令 + 前端类型**

`mod.rs` 新增并注册到 `lib.rs:274-286`：

```rust
/// 引导状态探测：返回当前平台、步骤表、每步完成情况与**卡在哪一步**
#[tauri::command]
pub fn remote_ts_probe() -> Result<serde_json::Value, String> { /* 组装 wizard_steps + probe_steps */ }

/// 执行一步（幂等；需要人的步骤只做"触发+回报"，不代点）
#[tauri::command]
pub fn remote_ts_run_step(step: String) -> Result<serde_json::Value, String> { /* match step id */ }
```

`probe_steps` 的判据（**每步都要能说出"怎么算完成"**）：

| 步骤                  | 判据                                                                                                                                                      |
| --------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `detect` / `install`  | `find_cli().is_some()`                                                                                                                                    |
| `sys_ext`（仅 macOS） | **CLI 能连上本机后端**（`status --json` 可解析且 `BackendState != "Stopped"`）——⚠️ 未批准系统扩展时的确切形态**待实测校验**，实施时以实机为准并在注释登记 |
| `login`               | `BackendState == "Running"`                                                                                                                               |
| `shields_up`          | `get --json` 的 `shields-up == false`                                                                                                                     |
| `funnel`              | `funnel_active(funnel status --json)` 为真                                                                                                                |
| `verify`              | Task 7 的校验结果                                                                                                                                         |
| `autostart`           | MAM 自身行为（启动时自动恢复），恒 done=true（由 Task 5 的 `restore_tunnels_core` 保证）                                                                  |

- [ ] **Step 8: 前端向导组件（含 Windows 行标注）**

新建 `src/components/settings/TailscaleWizard.tsx`：按 `wizard_steps` 渲染纵向步骤列表，每步三态（**待做 / 进行中 / 已完成**），需要人的步骤**突出显示用户要做什么**，并提供按钮（`remote_ts_run_step`）。**关键要求**：卡住时明确显示卡在哪一步 + 该步的 `blocked_reason`，不得只转圈。

**Windows 行必须在 UI 上如实标注**（`Platform::Windows` 时）：在向导顶部显示一行弱提示「Windows 步骤尚未实机校验」——**不许把未验证的流程伪装成已验证**。

- [ ] **Step 9: i18n 两语言同步 + 门禁全绿并提交**

`zh.json` / `en.json` 的 `settings.remote` 下新增 `tsWizard.*` 键组（**两 locale 键集必须完全相等**——有测试 `tests/settings/remoteSection.test.tsx:659,669` 把关）。

```bash
pnpm test && pnpm build && pnpm format:check && pnpm lint
cd src-tauri && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
git add -A
git commit -m "feat(remote): 首次配置引导一条龙——平台步骤表 + 安装/登录/开通自动化（§C2）

固定版本 + 硬编码 sha256（照 tunnel.rs 既有范式，不用索引页抓取以免 TOFU）。
不静默提权、不代持凭据、不覆盖用户既有 serve 配置。Windows 行标注待实测校验。"
```

---

### Task 7: 强制可达性校验 + 自愈（§C3）

**Files:**

- Modify: `src-tauri/src/remote/tailscale.rs`
- Modify: `src-tauri/src/remote/mod.rs`（载荷里带上校验态）
- Modify: `src/components/settings/TailscaleWizard.tsx`、`src/lib/api/remote.ts`、`src/i18n/locales/{zh,en}.json`
- Test: `tailscale.rs` 内嵌 tests、`tests/settings/tailscaleWizard.test.tsx`

**Interfaces:**

- Consumes: Task 5 的 `board_url_from_dns_name` / `funnel_active`；Task 6 的 `probe_steps`
- Produces:
  - `tailscale::Reachability`（`Unverified` / `Verifying` / `Verified` / `Failed { reason: String }`）
  - `tailscale::parse_doh_answer(json: &str) -> Vec<String>`（公网 A/AAAA 记录）
  - `tailscale::doh_url(name: &str) -> String`
  - `tailscale::reachability() -> Reachability`；`tailscale::set_reachability(r)`（测试缝）
  - `tailscale::verify_reachability(host: &str, probe: &ProbeFn) -> Reachability`
  - `tailscale::heal_and_reverify(port: u16) -> Reachability`

> **机制主次（设计说明书 §C3 要求 5，2026-10-06 修正）**：**以外部解析 / 探测为主**。
> 曾经的写法是「首选手机端首次配对时回传」——**它恰好测不到它要测的故障**：手机若解析不出来，就根本打不开页面、也就无从回传。手机回传**只能确认成功**，不能报告失败。
> **本机制只覆盖「解析记录尚未发布」这一类失败**（用户 2026-10-06 确认该情形：新增通道时解析需要一段时间才生效），**不覆盖**证书错误、上游限流、本机进程崩溃。
> **外部服务不可用时不得据此判定地址可用**——宁可报「尚未验证」，也不谎报「已生效」。
>
> **本机为什么不能自测（两个原因各自都足以致命）**：① 本机是尾网成员，该名字被 MagicDNS 接管，**压根不走公网路径**；② 本机可能装有网络加速/代理工具（实测机器上就有——它用自签 CA 做本地 MITM）。**实测中我们正是被这点骗过**：本地六项信号全绿（连 Let's Encrypt 证书都真签下来了）而公网根本打不开。

- [ ] **Step 1: 写 DoH 解析的纯函数测试（红）**

```rust
    /// 外部解析：只认 A / AAAA 记录（CNAME 不算"已发布"）。
    /// 样本形态取自阿里 DNS 的 JSON 接口（实测可用：`https://223.5.5.5/resolve?name=X&type=A`）。
    #[test]
    fn parse_doh_answer_extracts_addresses_only() {
        let ok = r#"{"Status":0,"Answer":[{"name":"a.ts.net.","type":1,"data":"203.0.113.10"},{"name":"a.ts.net.","type":5,"data":"b.ts.net."}]}"#;
        assert_eq!(parse_doh_answer(ok), vec!["203.0.113.10"], "CNAME(type 5) 不得当作已发布");
        let none = r#"{"Status":0}"#;
        assert!(parse_doh_answer(none).is_empty(), "无 Answer = 记录尚未发布");
        assert!(parse_doh_answer("not json").is_empty(), "解析不了按未发布处理");
    }

    #[test]
    fn doh_url_targets_a_public_resolver() {
        let u = doh_url("jarvismac-mini.example-tailnet.ts.net");
        assert!(u.starts_with("https://"), "必须是 https");
        assert!(u.contains("223.5.5.5") || u.contains("1.1.1.1"), "必须是公共解析器，不能用系统 DNS");
        assert!(u.contains("jarvismac-mini.example-tailnet.ts.net"));
    }

    /// 判定门：**没有公网记录就绝不能是 Verified**（这是 §C3 的核心断言）。
    #[test]
    fn empty_answer_never_verifies() {
        let probe = |_host: &str| vec![]; // 假探针：解析不到
        assert!(matches!(
            verify_reachability("a.ts.net", &probe),
            Reachability::Failed { .. }
        ));
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd src-tauri && cargo test tailscale 2>&1 | tail -20
```

Expected: FAIL

- [ ] **Step 3: 实现解析与校验（绿）**

```rust
/// 可达性四态。**地址只有在 `Verified` 时才算可用**（§C3 要求 2：
/// 校验不通过就不许把地址当可用呈现）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Reachability {
    Unverified,
    Verifying,
    Verified,
    Failed { reason: String },
}

/// 公共解析器的 DoH JSON 接口。**首选阿里 DNS（223.5.5.5）——本机实测可用**；
/// Cloudflare（1.1.1.1）作为备选。**绝不使用系统解析器**（本机解析会走 MagicDNS，
/// 正是要绕开的那条路）。
pub fn doh_url(name: &str) -> String {
    format!("https://223.5.5.5/resolve?name={name}&type=A")
}

/// 从 DoH JSON 里取 A/AAAA 记录（type 1 / 28）。**CNAME(5) 不算已发布**——
/// 记录没发布时接口返回空 Answer（实测形态）。
pub fn parse_doh_answer(json: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else { return Vec::new() };
    v.get("Answer")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|r| matches!(r.get("type").and_then(|t| t.as_u64()), Some(1) | Some(28)))
                .filter_map(|r| r.get("data").and_then(|d| d.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// 探针缝（测试注入假实现，零网络）：给定主机名返回其公网地址。
pub type ProbeFn = dyn Fn(&str) -> Vec<String> + Send + Sync;

/// 尾网之外的可达性校验：**先看公网有没有记录，再试着接通**。
/// 两者缺一即失败——只有记录但连不通，说明上游还没ready。
pub fn verify_reachability(host: &str, probe: &ProbeFn) -> Reachability {
    let addrs = probe(host);
    if addrs.is_empty() {
        return Reachability::Failed {
            reason: "公网解析不到该地址——解析记录可能尚未发布".into(),
        };
    }
    // 用解析到的地址**钉住 IP** 去请求（绕过本机 DNS），能拿到任何 HTTP 响应即算通
    match probe_http(host, &addrs[0]) {
        Ok(_) => Reachability::Verified,
        Err(e) => Reachability::Failed { reason: format!("地址已解析但连不通: {e}") },
    }
}
```

> 实现提示：`probe_http` 用既有 `reqwest`（`Cargo.toml:84`）加 `.resolve(host, ip)` 钉住地址，等价于实测中用过的 `curl --resolve` 手法——**这是唯一能绕开本机 DNS 与本地代理的做法**。定义如下（模块私有）：

```rust
/// 用解析到的公网地址**钉住 IP** 去请求（绕过本机 DNS 与本地代理）。
/// 拿到任意 HTTP 响应即算"接通"——**不校验状态码**（403/404 也说明链路通了，
/// 我们要验的是"地址可达"，不是"内容正确"）。
fn probe_http(host: &str, ip: &str) -> Result<(), String> {
    let addr: std::net::IpAddr = ip.parse().map_err(|_| format!("地址非法: {ip}"))?;
    let client = reqwest::blocking::Client::builder()
        .resolve(host, std::net::SocketAddr::new(addr, 443))
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;
    client
        .get(format!("https://{host}/"))
        .send()
        .map(|_| ())
        .map_err(|e| e.to_string())
}
```

> **`real_probe()` 是生产探针**（`ProbeFn` 的真实现）：`doh_url(host)` → reqwest 取 JSON → `parse_doh_answer`。测试一律注入假探针（**零网络红线**），与 `tunnel.rs:936`「fetch 闭包全为内存假实现」同纪律。

- [ ] **Step 4: 接入快照：地址双门 + 校验门**

在 `channels_payload` 的 tailscale 段，`address` 的给出门从既有双门（`running && error.is_none()`）**再加一道**：`reachability == Verified`。补形状契约测试：

```rust
    /// §C3 核心断言（载荷级）：**未验证通过时，载荷里不得出现可用地址**。
    #[test]
    fn channels_payload_hides_address_until_reachability_verified() {
        // 快照 running + url 就绪，但可达性仍是 Unverified → address 必须为 None
        tailscale::set_ts_snapshot(|c| { c.running = true; c.url = Some("https://a.ts.net/m".into()); });
        tailscale::set_reachability(Reachability::Unverified);
        let ts = tailscale::ts_snapshot(); // 上面刚注入 running=true + url 就绪
        let p = channels_payload(
            true,
            ChannelFlags { tailscale: true, ..Default::default() },
            9420,
            vec![],
            tunnel::TunnelStatus::default(),
            ts,
            Reachability::Unverified,
        );
        assert!(p["tailscale"]["address"].is_null(), "未验证不得报可用地址");
        assert_eq!(p["tailscale"]["error"], "尚未生效");
    }
```

- [ ] **Step 5: 自愈（实测有效的那一招）**

```rust
/// 失败时自动尝试自愈：**重置 Funnel 后重新开启**——2026-10-06 实测这一招能
/// 触发解析记录重新发布。仍失败才报错（§C3 要求 3）。
pub fn heal_and_reverify(port: u16) -> Reachability {
    let _ = run_cli(&["funnel", "reset"]);
    if let Err(e) = enable_funnel(port) {
        return Reachability::Failed { reason: format!("自愈失败: {e}") };
    }
    // 记录发布有延迟，给一个短退避再验（不要立刻断言失败）
    std::thread::sleep(std::time::Duration::from_secs(5));
    let host = /* 由 status 快照取 DNS 名 */;
    verify_reachability(&host, &real_probe())
}
```

- [ ] **Step 6: 长期化（上游是 beta，这是一等要求而非一次性）**

在 Task 5 的轮询守护里加一路：**已 Verified 的通道每 60 秒重验一次**；一旦转为 Failed，立刻把地址从载荷撤下并 `emit_ui("remote-tunnel-error", ...)`（复用既有事件名与形状），同时**保持其他通道可用**（宪法原则 2「通道无关」）。

补测试：用注入的"先通后不通"假探针断言**状态可回退**（`Verified → Failed`），且地址随之撤下。

- [ ] **Step 7: 前端呈现（"尚未生效" + 重试）**

`TailscaleWizard.tsx` / 卡片详情区：`Reachability::Failed` 时显示「**固定地址尚未生效**」+ `reason` + **重试按钮**（调 `remote_ts_run_step("verify")`），**不得显示成可用地址让用户干等**；`Verified` 才显示地址与二维码。

- [ ] **Step 8: i18n 两语言 + 门禁全绿并提交**

```bash
pnpm test && pnpm build && pnpm format:check && pnpm lint
cd src-tauri && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
git add -A
git commit -m "feat(remote): 配置完成后的强制可达性校验 + 自愈（§C3）

以外部解析为主（本机自检不可信：MagicDNS 接管 + 本地代理 MITM，实测被其骗过）。
未验证不得报可用地址；失败自动 reset+重开自愈；已生效地址每 60s 重验，可回退。"
```

---

### Task 8: UI 线稿更新（§C4 前置，**必须先于 Task 9**）

**Files:**

- Modify: `docs/engineering/architecture/wireframes/2026-09-17-remote-settings-redesign.html`（381 行，自包含单文件）

**Interfaces:**

- Consumes: 设计说明书 §C4 的卡片表（§C4 要求「先改线稿、再改实现」）
- Produces: 更新后的线稿（Task 9 照它实现）

> **预览方式更正（重要）**：项目里「**localhost 8940 可预览**」的说法**没有脚本支撑**——全库只有 M5 计划 `:4` 一处孤证，`scripts/` 与 `package.json` 里没有任何 HTML 预览命令（`preview` 是 `vite preview`，只服务 `dist/`），实测该端口 `curl` 连接被拒（exit 7）。
> **可行方式 = 浏览器直接打开该文件**（自包含、零外部依赖、`file://` 即可）。本次顺手把这个说法在设计文档里改掉。

> **行号级机械清单**（测绘结论，逐条照做；**行号以改动前为准，改完会漂移——请从后往前改**）：
>
> | #   | 位置                         | 动作                                                                                                                                                       |
> | --- | ---------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- |
> | 1   | `351`                        | `pick()` 的硬编码 id 数组 `["p-local","p-lan","p-quick","p-named"]` **去掉 `p-local`**（漏改会留死引用）                                                   |
> | 2   | `206-209`                    | 删展开块 `#p-local`（**本机地址整行 = 207**）                                                                                                              |
> | 3   | `183-187`                    | 删「本机」卡片；并**另定默认选中**（原默认是它，见 183 的 `sel`）                                                                                          |
> | 4   | `188-192`                    | 「局域网」→「**局域网连接**」                                                                                                                              |
> | 5   | `198-202`                    | 「命名隧道」→「**自有域名**」（**改名，不是新增**）                                                                                                        |
> | 6   | 之后                         | **新增第 4 张卡**「**外部域名（免域名）**」——`.modes` 仍是 `repeat(4,1fr)`（`63`），**卡数不变**                                                           |
> | 7   | 新增                         | 新卡的**三态** DOM/CSS：未安装（一句价值主张 + 「一键配置」按钮）/ 配置中（向导进度）/ 已配置（地址 + 二维码 + 复制 + 运行态）——线稿现无对应 class，需新增 |
> | 8   | `253`、`296`                 | 连带文案：「仅对 局域网 / 临时隧道 / 命名隧道 生效（本机无需）」与「三通道二维码」**必须随本机不再是通道而改写**（测绘指出：这两处最容易漏）               |
> | 9   | `6`、`152`                   | `<title>` 写 **v3** 而 h1 写 **v5**，自相矛盾——统一为 **v6**                                                                                               |
> | 10  | `305-344`（§四文件预览增强） | **新增带宽提示 + 进度条**画块（线稿现**完全没有**这两样，属全新内容）                                                                                      |
>
> ⚠️ **不要误删 `176-179`「本机名称」**——那是主机名设置行，与「本机通道地址」无关。

- [ ] **Step 1: 从后往前改（避免行号漂移）**

按上表 **#10 → #1** 的顺序改（先改文件末尾、再改前面）。

- [ ] **Step 2: 可执行的验收断言（线稿也能自动化自检）**

```bash
W=docs/engineering/architecture/wireframes/2026-09-17-remote-settings-redesign.html
echo -n "卡片数（期望 4）: "; grep -c 'data-p=' "$W"   # ⚠️ 勿用 'class="mode'——它会同时命中 .modes 容器，实测得 5
echo -n "残留 p-local（期望 0）: "; grep -c 'p-local' "$W"
echo -n "新卡名在位（期望各 1）: "; grep -c '外部域名（免域名）' "$W"
echo -n "旧卡名残留（期望 0）: "; grep -cE '命名隧道' "$W"
echo -n "版本号统一 v6（期望 ≥2）: "; grep -c 'v6' "$W"
echo -n "带宽提示/进度条画块（期望 ≥2）: "; grep -cE '带宽|进度' "$W"
```

Expected: `4 / 0 / 1 / 0 / ≥2 / ≥2`

- [ ] **Step 3: 本地打开目视复核**

```bash
open docs/engineering/architecture/wireframes/2026-09-17-remote-settings-redesign.html
```

逐项确认：4 张卡、无「本机」、点每张卡都能正常切换展开区（`pick()` 没坏）、新卡三态可见、文件区有带宽提示与进度条。

- [ ] **Step 4: 提交**

```bash
git add docs/engineering/architecture/wireframes/2026-09-17-remote-settings-redesign.html
git commit -m "docs(wireframe): 通道区线稿更新为 4 卡（删本机/改名/新卡三态）+ 文件区进度与带宽位

线稿是 UI 唯一契约（实现与线稿不一致即缺陷），故先改线稿再改实现（§C4）。
顺带更正『localhost 8940 可预览』这一无脚本支撑的过时说法。"
```

---

### Task 9: 通道区实现重排 + 卡片三态（§C4）

**Files:**

- Modify: `src/components/settings/RemoteSection.tsx`
- Modify: `src/lib/api/remote.ts`（`RemoteChannels` 加 tailscale）
- Modify: `src/i18n/locales/{zh,en}.json`
- Test: `tests/settings/remoteSection.test.tsx`、`tests/remote/api.test.ts`

**Interfaces:**

- Consumes: Task 5 的 `channels_payload.tailscale` 段；Task 6/7 的 `remote_ts_probe` / `remote_ts_run_step`；Task 8 的线稿（**实现必须与线稿一致**）
- Produces: 4 张对外卡片的实现；`VIA_LABEL_KEY` 新增 `tailscale`

> **实现与线稿不一致即缺陷**（项目契约）。线稿里**卡面只有**「名称 + 状态点 + 开关 + 一行 desc」；**地址与二维码只在展开区**；地址行统一 `badge + code.url + 复制按钮`；**唯一展开区**靠 `selected` state 控制。

- [ ] **Step 1: 改卡片数据源（这是重排的核心）**

`RemoteSection.tsx:421-439` 的 `cardDefs` 改为 4 张（**`local` 不再是一张卡**）：

```tsx
// 通道卡定义（4 张对外通道；「本机」不再是通道——它是访问方式，见 §G1/§C4）
// 顺序与线稿一致：局域网连接 / 临时隧道 / 自有域名 / 外部域名（免域名）
const cardDefs = [
  { key: "lan", nameKey: "settings.remote.chanLan", descKey: "settings.remote.chanLanDesc" },
  { key: "quick", nameKey: "settings.remote.chanQuick", descKey: "settings.remote.chanQuickDesc" },
  { key: "named", nameKey: "settings.remote.chanNamed", descKey: "settings.remote.chanNamedDesc" },
  {
    key: "tailscale",
    nameKey: "settings.remote.chanTailscale",
    descKey: "settings.remote.chanTailscaleDesc",
  },
] as const;
```

- [ ] **Step 2: 改默认选中 + 删本机展开块**

1. `RemoteSection.tsx:161` 的 `selected` 默认值由 `"local"` 改为 `"lan"`（本机卡已删，留着默认会指向不存在的卡）。
2. **删除** `:557-576` 的 `{selected === "local" && …}` 展开块（本机地址整行不再显示）。
3. `:410` 的 `localAddr` 若仅被该块消费则一并删除；若别处仍在用（如 Task 2 之前的口径），保留但**不得在卡片区显示**。
4. **本机访问怎么办**（用户裁决）：用「局域网连接」卡显示的地址——**前提是该开关已打开**（服务只在回环监听时局域网地址不可达）。故 `lan` 卡在关闭状态下必须显式呈现"未开启"（用户要知道为什么本机也进不去）。

- [ ] **Step 3: via 徽标新增 tailscale 分支**

`RemoteSection.tsx:102-107` 的 `VIA_LABEL_KEY` 加一行：

```tsx
  tailscale: "settings.remote.chanTailscale",
```

> 不加这一行，**Tailscale 通道的设备在花名册上不会显示任何来源徽标**（未知 via 一律不渲染——前端不猜）。配套：Task 5 已让 `classify_via` 返回 `"tailscale"`，两边必须同时到位。

- [ ] **Step 4: 新卡的三态（未安装 / 配置中 / 已配置）**

`cardDefs` 渲染分支里，`key === "tailscale"` 时按 `remote_ts_probe()` 的步骤状态渲染三态：

| 态         | 触发条件                                  | 卡面/展开区                                                             |
| ---------- | ----------------------------------------- | ----------------------------------------------------------------------- |
| **未安装** | `detect` 步未完成                         | 一句价值主张 + 「**一键配置**」按钮 → 进入 Task 6 的向导                |
| **配置中** | 有步骤未完成且已开始                      | **向导进度**（每步可见，**卡在哪一步要说出来**——设计说明书 §C2 原则 3） |
| **已配置** | 全部步骤完成且 `reachability == Verified` | 固定地址 + 二维码 + 复制 + 运行态                                       |

> `reachability !== Verified` 时**不得显示成可用地址**（Task 7 Step 4/7 的载荷与 UI 双重门）。

- [ ] **Step 5: i18n 两语言（键集必须完全相等）**

`zh.json` / `en.json` 的 `settings.remote` 下：

- 改 `chanLan` → 「局域网连接」；`chanLanDesc` → 「同一 WiFi / 网线接入；**本机访问也走这张**」
- 改 `chanNamed` → 「自有域名」；`chanNamedDesc` → 「永久地址，需你自己的域名」
- **新增** `chanTailscale` → 「外部域名（免域名）」；`chanTailscaleDesc` → 「永久地址，不需要域名」
- **新增** `chanLocalDesc` 改为「随远程接入常驻；**本机浏览器访问同样需要访问密码**」（Task 4 已改，此处复核一致）
- 新增 `tsWizard.*` 键组（Task 6）

```bash
pnpm test 2>&1 | tail -20   # 有 i18n 无缺键/两 locale 相等的测试把关
```

- [ ] **Step 6: 改前端测试（新增通道必须同步这几处）**

测绘点名的必改项：

1. `tests/settings/remoteSection.test.tsx:45-70` 的载荷工厂 `channelsOf` —— 加 `tailscale` 段。
2. `:114-144` 的四卡断言 —— 改为新四卡（局域网连接 / 临时隧道 / 自有域名 / 外部域名（免域名）），并**断言「本机」不在卡片区**。
3. `:530` 的 via 徽标测试 —— 覆盖 `tailscale` 值。
4. `:659-680` 的 i18n 无缺键测试 —— 自动覆盖新键。

- [ ] **Step 7: 门禁全绿并提交**

```bash
pnpm test && pnpm build && pnpm format:check && pnpm lint
git add -A
git commit -m "feat(remote-ui): 通道区重排为 4 张对外卡片 + 新卡三态（§C4）

局域网连接 / 临时隧道 / 自有域名 / 外部域名（免域名）；本机地址整行不再显示，
本机访问改走局域网卡地址（需该开关已打开）。via 徽标新增 tailscale 分支。"
```

---

### Task 10: 文件区带宽提示 + 进度条 + 满速升级指引（§C5）

**Files:**

- Modify: `src/mobile/api.ts`（下行流式读 + 上行换 XHR）
- Modify: `src/mobile/FilePreview.tsx`（下行进度落点 `:350`）
- Modify: `src/mobile/MessageComposer.tsx`（上行进度落点 `:983-987`；`PendingAttachment` `:86-94`）
- Modify: `src/mobile/FilePanel.tsx`（带宽提示落点 `:349-357`）
- Modify: `src-tauri/src/remote/server.rs` + `api.rs`（新增 `/channel` 装饰端点）
- Test: `tests/mobile/*`（若无，新建 `tests/mobile/transfer.test.ts`）

**Interfaces:**

- Consumes: Task 5 的 `gate::classify_via`（**只用于装饰**）
- Produces:
  - `api.ts` 的 `fetchFile(sessionId, filePath, onProgress?)` —— **第 3 参是新增的**（真实签名 `api.ts:207` 只有两个参数，加第三个不冲突）
  - `api.ts` 的 `uploadAttachment(sessionId, file, signal?, onProgress?)` —— ⚠️ **`onProgress` 必须是第 4 个参数**：真实签名 `api.ts:426-430` 的第 3 参**已经是 `signal?: AbortSignal`**，把回调插在第 3 位会**被当成 signal 传进去**（编译期不一定报错，运行时静默失效）
  - `MessageComposer` 的 `PendingAttachment` 由 `status: "uploading"|"ready"|"failed"` 扩为带进度：`{ status, loaded?: number, total?: number }`
  - 新端点 `GET /m/api/v1/channel` → `{ via: string, limited: boolean, est_mbps_down: number, est_mbps_up: number }`

> **机制事实（测绘结论，决定怎么做）**：
>
> - **下行能拿进度**：现在走同源 `fetch` + `r.blob()`（`api.ts:211/228`），**不是** `<img src>` 直连（`<img>` 只消费 blob object URL，`FilePreview.tsx:358`）→ 改成 `r.body.getReader()` 累计字节即可。
> - **上行现在零原生进度**：**全仓零 `XMLHttpRequest`**，`uploadAttachment` 用 `fetch` + `await file.arrayBuffer()`（`api.ts:434-439`）——**fetch 规范不暴露上传进度事件，必须换 XHR**。
> - **`Content-Length` 是 axum/hyper 隐式写的**（Rust 侧无显式设置；图片分支返回 `Vec<u8>` 带精确 size hint）→ **唯一外部变量是 cloudflared 是否透传**，Step 7 真机确认。
> - **带宽提示是装饰**（不变量 §G1 推论③）：它读的是 Host，可被伪造——**允许不准确，永不得进安全判定**。

- [ ] **Step 1: 下行进度——写测试（红）**

```ts
it("下行按累计字节回调进度（分块流）", async () => {
  const chunks = [new Uint8Array(100), new Uint8Array(150)];
  const fake = {
    ok: true,
    headers: new Headers({ "content-type": "image/png", "content-length": "250" }),
    body: new ReadableStream({
      start(c) {
        chunks.forEach((x) => c.enqueue(x));
        c.close();
      },
    }),
  };
  const seen: Array<[number, number | null]> = [];
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(fake));
  await fetchFile("s1", "/tmp/a.png", (loaded, total) => seen.push([loaded, total]));
  expect(seen).toEqual([
    [100, 250],
    [250, 250],
  ]);
});
```

- [ ] **Step 2: 跑测试确认失败**

```bash
pnpm test tests/mobile/transfer.test.ts 2>&1 | tail -15
```

Expected: FAIL（`onProgress` 参数不存在 / 未按块回调）

- [ ] **Step 3: 实现下行进度（绿）**

把 `api.ts:227-230` 的 `await r.blob()` 改为流式读取：

```ts
// 下行进度：分块读 + 累计字节。total 取 content-length（**axum/hyper 隐式写入**，
// 但隧道是否透传需真机确认——拿不到就传 null，UI 退化为只显示已传字节，不显示假百分比）。
const total = Number(r.headers.get("content-length") ?? "") || null;
let loaded = 0;
const chunks: Uint8Array[] = [];
if (r.body && onProgress) {
  const reader = r.body.getReader();
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    chunks.push(value);
    loaded += value.byteLength;
    onProgress(loaded, total);
  }
}
const blob = chunks.length
  ? new Blob(chunks as BlobPart[], { type: r.headers.get("content-type") ?? "" })
  : await r.blob();
```

进度 UI 插 `FilePreview.tsx:350` 的 loading 处（现在只有「加载中…」文字）：**已传字节 + 百分比（有 total 时）+ 预估剩余**。

- [ ] **Step 4: 上行进度——先证明 fetch 不行（红），再换 XHR（绿）**

```ts
it("上传进度回调必须在 send() 之前注册（否则收不到事件）", async () => {
  const order: string[] = [];
  class FakeXHR {
    upload = { onprogress: null as null | ((e: ProgressEvent) => void) };
    open() {
      order.push("open");
    }
    send() {
      order.push("send");
      this.upload.onprogress?.(new ProgressEvent("progress", { loaded: 5, total: 10 }));
    }
    setRequestHeader() {}
    get status() {
      return 200;
    }
    get responseText() {
      return "{}";
    }
  }
  vi.stubGlobal("XMLHttpRequest", FakeXHR as unknown as typeof XMLHttpRequest);
  const seen: number[] = [];
  // ⚠️ onProgress 是**第 4 参**——第 3 参是既有签名里的 signal（传 undefined 占位）。
  // 真实签名：uploadAttachment(sessionId, file, signal?, onProgress?)（api.ts:426-430）
  await uploadAttachment("s1", new File([new Uint8Array(10)], "a.bin"), undefined, (loaded) => {
    order.push("progress");
    seen.push(loaded);
  });
  expect(seen).toEqual([5]);
  expect(order.indexOf("progress")).toBeLessThan(order.indexOf("send") + 1); // 注册早于 send
});
```

实现：把 `api.ts:426-439` 的 `fetch` 换成 `XMLHttpRequest` **封装成 Promise**，`xhr.upload.onprogress` **必须在 `xhr.send()` 之前**注册（顺序错会收不到事件——本次会话中已踩过一次）。**保留既有第 3 参 `signal` 的位置与语义**（改绑 `xhr.abort()`；`signal.aborted` 时立即中止），`onProgress` 追加为第 4 参。

`PendingAttachment`（`MessageComposer.tsx:86-94`）扩为带 `loaded/total`；进度 UI 插在 `:983-987` 的「上传中：${a.name}」处，且**必须与线稿一致地做成进度条**（不是只加一行文字）：chip 内或 chip 下方补一条 **`.pbar` 等价进度条**，类串沿用 §C5 下行已用的写法（`h-1.5 rounded-full bg-[var(--cb)]`）；**`total == null` 时不渲染进度条**（只显示已传字节），并可附一句口径说明「速率未知时只显示已传字节，不显示假百分比」。

> **口径来源（2026-10-07 用户裁决）**：计划原稿自相矛盾——Task 8 的线稿画了上传进度条，而本 Task 只要求"扩 chip 文字"。用户裁定**以线稿为准：补进度条**。

- [ ] **Step 5: 受限通道判定端点（装饰）**

`server.rs` 注册 `GET /channel`（在 `/m/api/v1` nest 内，**受门禁保护**），`api.rs` 实现：

```rust
/// 通道能力装饰端点：告诉移动端「你当前走的是哪条通道、带宽是否受限」。
/// **纯装饰**（不变量 §G1 推论③）：读 Host 推断，**可被伪造**，允许不准，
/// **永不得**用于任何安全判定。
pub async fn channel_info(...) -> Response {
    let via = /* 与 /pair/pin 同款：via_hosts_source + classify_via；None → "lan" */;
    let limited = via != "lan";   // 局域网不受限；三条隧道走外部中继
    Json(json!({ "via": via, "limited": limited, "est_mbps_down": 1.8, "est_mbps_up": 0.8 }))
}
```

> 数值 `1.8 / 0.8` 是**实测值**（2026-10-06，手机蜂窝 → Funnel → 本机；下行用客户端实收、上行用服务端实收）。**不要**用"服务端发出量÷耗时"的口径——那会高估约 66%（实测教训，见设计说明书 §C5）。

- [ ] **Step 6: 带宽提示 + 满速升级指引（用户点名要求）**

落点 `FilePanel.tsx:349-357`（与既有「可预览上限」提示同层），类串照抄 `MessageComposer.tsx:1078-1086` 的说明型横幅。

> **复用排查结论（已核）**：移动端的 `pushBanner`（`Board.tsx:169`）**不可复用**——它是该组件内的 `useCallback`（**未导出**），且入参是 `TransitionEvent`（跃迁事件）而非任意文案。移动端也**没有**现成的进度条组件（只有若干 `animate-pulse` 圆点）。故本 Task 的横幅与进度条**都是新增**，照抄类串即可，别去改 `Board.tsx`。

```tsx
{
  /* ⚠️ 移动端整体无 i18n（src/mobile/ 既有做法，见 PairPage.tsx:19）——直接硬编码中文。
     数值必须过 Number.isFinite && > 0 守卫：est 为 0 / NaN / Infinity 时会算出"0.0 分钟"
     这种看似合理的假耗时。 */
  channel?.limited && estOk && (
    <div className="mt-1 rounded-lg bg-[var(--cbg)] px-2 py-1.5 text-[11px] leading-4 text-[var(--mut)]">
      当前通道带宽受限：{sizeMb} MB 附件约需 {downMin} 分钟，手机上传约 {upMin} 分钟。
      这是通道限制，不是故障。想要满速可让手机也装上 Tailscale 走尾网直连。
    </div>
  );
}
```

文案方向（**硬编码中文，不走 i18n**——`src/mobile/` 整体无 i18n，`PairPage.tsx:19` 已注明该 surface 的口径）：**「当前通道带宽受限：20 MB 附件约需 1.5 分钟，手机上传约 3.3 分钟」+「这是通道限制，不是故障」**；并给**满速升级指引**（手机装上 Tailscale 后走尾网直连，带宽高出 1–2 个数量级）。

> 升级指引**不占通道卡片位**（仍维持 §C4 的 4 张卡），只在用户真遇到大文件时就地出现。

> **落点有两个，缺一不可**（设计说明书 §C5 原文是「在**文件面板与附件区**显示提示」）：① 文件面板底部 `FilePanel.tsx:349-357`（浏览文件的人看得到）；② **附件区**——`MessageComposer.tsx:970-1000` 的 chips 容器附近（**只上传、不浏览文件的用户**也要看得到"这是通道限制，不是故障"）。2026-10-07 评审指出原计划只给了①。

- [ ] **Step 7: 真机确认 `Content-Length` 是否透传（唯一外部变量）**

```bash
# 起一个临时隧道指向本地服务，再从外部请求一个图片端点，看响应头有没有 content-length
curl -sI --resolve <隧道域名>:443:<解析IP> "https://<隧道域名>/m/api/v1/file?session_id=<真实会话>&path=<真实图片>" | grep -i content-length
```

Expected: **有 `content-length`**。若无 → 退化为「只显示已传字节、不显示百分比」（`onProgress` 的 `total` 传 `null`），**不要**在 UI 上编一个百分比。**把实测结论写进代码注释。**

- [ ] **Step 8: 门禁全绿并提交**

```bash
pnpm test && pnpm build && pnpm format:check && pnpm lint
cd src-tauri && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
git add -A
git commit -m "feat(mobile): 文件传输进度可见 + 带宽如实告知与满速升级指引（§C5）

下行改流式读拿进度；上行由 fetch 换 XHR（fetch 无上传进度事件，全仓原零 XHR）。
受限通道下按实测速率预估耗时并说明是通道限制；提供尾网直连升级指引。"
```

---

### Task 11: 文档收口

**Files:**

- Modify: `docs/engineering/specs/2026-09-12-remote-access-level1-design.md`（一期设计说明书 §P6 / §P7 追记）
- Modify: `docs/engineering/specs/2026-10-06-remote-fixed-address-and-gate-design.md`（两处更正 + 变更记录）
- Modify: `docs/product/manual/zh/*.md`、`docs/product/manual/en/*.md`（远程接入章节）

**Interfaces:**

- Consumes: 全部前序 Task 的实测结论
- Produces: 与实现一致的文档

- [ ] **Step 1: 一期设计说明书 §P6 / §P7 追记两处裁决**

`docs/engineering/specs/2026-09-12-remote-access-level1-design.md` 的 §P6（配对）与 §P7（固定地址与通道）各加一条**追记**（不改原文，只追记——这是项目惯例）：

1. **§P6**：本机免密豁免已撤销（2026-10-06，设计说明书 §G1）。原文若有「本机免密」表述，追记说明其已被零豁免取代，理由 = 任何中继都把公网流量从回环转进来，「从回环来」不再等于「人在本机」。
2. **§P7**：Tailscale 模式由「**仅文档指引**」升为「**内置通道**」（2026-10-06）。追记须写明：宪法 §0.1 / F1.2 早已把 Tailscale 列为四模式之一，故**本次无需修宪**；同时记录 2026-09-17「删除 Tailscale 提示行」的裁决被本次取代（当时删的是一行被动文案，本次升为一等内置通道）。

> **不要修改 `docs/MASTER-PLAN.md`**（宪法）——需用户在场明确同意才可动。本次两处都**不需要**动宪法（§P7 的升级是在宪法既有条款内实现）。

- [ ] **Step 2: 更正设计说明书里那条无据的预览说法**

`docs/engineering/specs/2026-10-06-remote-fixed-address-and-gate-design.md` 的 §C4 里写了线稿「**可在 localhost 预览**」——**该说法无脚本支撑**（全库仅 M5 计划一处孤证，`scripts/` 与 `package.json` 无 HTML 预览命令，实测端口连接被拒）。改为：

```
先在线稿上落地并**在浏览器直接打开该文件预览**（自包含单文件，`file://` 即可）。
```

- [ ] **Step 3: 设计说明书 §G3 的 via 值域由三值更正为四值**

Task 5 给 `classify_via` 加了 `tailscale` 分支，故 §G3 的「取值为「临时隧道 / 自有域名 / 局域网」三值」应更正为**四值**（临时隧道 / 自有域名 / 外部域名（免域名）/ 局域网）。同处 `§C4` 的卡片表已含第 4 张，二者必须一致。

- [ ] **Step 4: 用户手册补三条**

在远程接入相关章节（**先 grep 定位**：`grep -rn "访问密码\|远程接入" docs/product/manual/zh docs/product/manual/en | head`）：

1. **新增通道**：如何开通「外部域名（免域名）」——说明要装 Tailscale、需要一次浏览器登录、地址永久不变。
2. **本机也需密码**：本机浏览器访问同样需要访问密码（首次一次，此后 180 天）。
3. **带宽限制**：该通道带宽受限（实测下行 ≈1.8 / 上行 ≈0.8 Mbps）；大文件会慢（20 MB 附件约 1.5 分钟、手机上传约 3.3 分钟），**这是通道限制不是故障**；想要满速可让手机也装上 Tailscale 走尾网直连。

中英双语同步。

- [ ] **Step 5: 设计说明书补变更记录并提交**

```bash
npx prettier --write docs/engineering/specs/2026-10-06-remote-fixed-address-and-gate-design.md docs/engineering/specs/2026-09-12-remote-access-level1-design.md
npx prettier --check docs/engineering/specs/2026-10-06-remote-fixed-address-and-gate-design.md
git add docs/
git commit -m "docs: 收口——一期设计说明书 §P6/P7 追记两处裁决、更正线稿预览说法、via 四值、用户手册三条"
```

---

### Task 12: Windows 步骤表实测校验（**可延后，不阻塞任何任务**）

**Files:**

- Modify: `src-tauri/src/remote/tailscale.rs`（回填 + `WINDOWS_VERIFIED`）
- Modify: `docs/engineering/specs/2026-10-06-remote-fixed-address-and-gate-design.md`（§C2 追记）

**Interfaces:**

- Consumes: `docs/engineering/plans/2026-10-06-windows-tailscale-probe-prompt.md`（交付件，已在库）
- Produces: 由「预填」转为「实测」的 Windows 步骤表

> **为什么要独立成 Task**：Windows 行的依据只有**官方文档 + MSI 行为调研，零实机验证**；而引导要能**自检每一步**，检测点必须实测。**但它是数据回填，不动架构**，故不阻塞任何编码。

- [ ] **Step 1: 在 Windows 机器上执行探测提示词**

把 `docs/engineering/plans/2026-10-06-windows-tailscale-probe-prompt.md` **整份**交给 Windows 端的 Agent（或人）执行，收回填好的表。

- [ ] **Step 2: 核对三个必答问题**

1. **Windows 到底几步需要人？**（预期 3 步；若实测不是，改 `wizard_steps` 的 `needs_human` 并同步 `human_step_counts_match_spec` 的期望值）
2. **哪一步最容易卡住用户？**（用于打磨向导文案）
3. **有没有官方文档没写的额外步骤？**（防火墙 / 杀软 / 需重启——**若有，`wizard_steps` 必须加步，不能漏**）

- [ ] **Step 3: 回填与校验点修正**

按回填结果改 `tailscale.rs`：

1. `ts_asset_name` / `ts_download_url` / `ts_expected_sha256` 的 Windows 行按实测版本与哈希校正（**哈希必须真的取自 `.sha256`**）；改版本时三处必须同步（`ts_asset_name` 的字符串就是 `ts_expected_sha256` 的查表键）。
2. `run_installer` 的 Windows 分支：若实测确认 `msiexec /qn` **可用且确实不需要再点确认**，可改用静默参数；**若不确定，保持交互式**（宁可多点一次，也不要静默提权）。
3. **步骤检测点**按实测替换（原 `probe_steps` 已在 B4 删除，活路径内核为 `probe_steps_from`）（尤其 `sys_ext` 那条 macOS 的判据在 Windows 上不存在，删掉即可——步骤表已按平台分叉）。
4. **（2026-10-07 已实施，命名已变更）** Windows 验证位**按覆盖面拆细**，不再是一个整行布尔：
   - `WINDOWS_VERIFIED_FROM: &str = "shields_up"` —— 实测覆盖的**起点步骤**
   - `windows_unverified_steps(p) -> Vec<&'static str>` —— 按步骤表顺序**派生**的未验步骤清单（当前 = `detect`/`download`/`install`/`login`）
   - `windows_verification_for(p)` —— 载荷块 `{windowsVerified, windowsVerifiedFrom, windowsUnverifiedSteps}`

   > **为什么拆**：本次探测**从第 6 步开始**（安装与登录此前已完成），故 1–5 步的**流程**未被端到端跑过。
   > 一个整行布尔置 true 会让 UI 把「Windows 步骤尚未实机校验」整行撤下 ⇒ **过度声明**，与项目红线
   > 「不得把未验证的伪装成已验证」同类。拆细后提示收窄为「只实测了后半段：X/Y/Z 尚未端到端实机跑过」。

5. `TailscaleWizard.tsx` 里那条「Windows 步骤尚未实机校验」的提示，改为**读这个常量**决定是否显示（而不是按平台硬编码）。

- [ ] **Step 4: 设计说明书 §C2 追记 + 提交**

```bash
cd src-tauri && cargo test && cargo fmt --check && cargo clippy --all-targets -- -D warnings
pnpm test && pnpm build
git add -A
git commit -m "docs+feat(remote): Windows 步骤表按实机探测回填，WINDOWS_VERIFIED 置真（§C2）"
```

---

## 第二部分验收清单（§C1–§C6 出口）

| #   | 场景                           | 预期                                                                                                                                        |
| --- | ------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------- |
| 1   | 全新机器从"没装"到"拿到地址"   | 人工确认 **macOS ≤4 次 / Windows ≤3 次**，全程无命令行（**人工**）                                                                          |
| 2   | 固定地址重启后                 | **地址完全不变**；已配对设备免重配（**人工**）                                                                                              |
| 3   | 首次开通后公网尚未发布解析记录 | 界面报「**固定地址尚未生效**」+ 原因 + 重试；**绝不显示成可用地址**（自动化：`channels_payload_hides_address_until_reachability_verified`） |
| 4   | 自愈路径                       | `funnel reset` + 重开能恢复（2026-10-06 实测有效）（**人工**）                                                                              |
| 5   | 已生效地址失效                 | 60 秒内重验转 Failed 并撤下地址（自动化：假探针「先通后不通」）                                                                             |
| 6   | 通道区                         | **恰好 4 张对外卡片**；「本机」不在卡片区；**实现与线稿一致**（自动化：`remoteSection.test.tsx` 四卡断言 + 线稿 `grep` 断言）               |
| 7   | 本机访问                       | 用局域网卡地址可达（**前提：该开关已打开**）；首次需输入访问密码（**人工**）                                                                |
| 8   | 设备花名册 via 徽标            | Tailscale 连接显示「外部域名（免域名）」，**不得显示成「局域网」**（自动化：via 徽标测试）                                                  |
| 9   | 下行进度                       | 图片预览显示已传字节 + 百分比 + 预估剩余（自动化：分块流测试）                                                                              |
| 10  | 上行进度                       | 上传显示进度（**XHR 且 `onprogress` 在 `send()` 前注册**）（自动化：注册顺序测试）                                                          |
| 11  | 受限通道提示                   | 文件区显示耗时预估 + 「这是通道限制不是故障」+ 满速升级指引（**人工**）                                                                     |
| 12  | 不覆盖用户既有配置             | 已有非 MAM 的 serve/Funnel 配置时**拒绝开通**并如实报错（自动化：`foreign_serve_config` 测试）                                              |
| 13  | 门禁零接线自证                 | Task 5–10 **未修改 `gate.rs` 的鉴权分支**（只加了 via 装饰分支）（**人工复核 git diff**）                                                   |
| 14  | 既有三通道零回归               | 局域网 / 临时隧道 / 命名隧道行为与交付前一致（**人工**）                                                                                    |
| 15  | 门禁全绿                       | `cargo fmt --check` / `clippy -D warnings` / `cargo test` / `pnpm test` / `pnpm build` / `pnpm format:check` / `pnpm lint`                  |

## 第二部分已知边界（如实申报）

1. **Windows 步骤表在 Task 12 之前是"预填、待实测校验"**——UI 会**如实显示**这一状态，不得伪装成已验证。
2. **macOS「系统扩展是否已批准」的判据待实测**：现用「CLI 能连上本机后端」作代理判据，**未批准时的确切形态没有实测过**；实施时以实机为准并在代码注释登记。
3. **「阻止传入连接」的写入侧尚未实测**（读取侧已实测 = `shields-up`）：故 `disable_shields_up` **写完必须回读确认**，读不回就报错，不假定成功。
4. **可达性校验只覆盖"解析记录尚未发布"这一类失败**（用户 2026-10-06 确认该情形），**不覆盖**证书错误、上游限流、本机进程崩溃。
5. **`Content-Length` 是否经 cloudflared 透传需真机确认**（Step 7）；拿不到就退化为"只显示已传字节"，**不编造百分比**。
6. **上传侧没有类型白名单**（测绘发现：`<input type="file">` 无 `accept`，服务端也不校验）——**本计划不修**（不在 §C1–§C6 范围），如需处置请另开批次。
7. **Funnel 仍为 beta**：上游可能变更；卡片文案不得承诺 SLA。
8. **一次性登录无法省略**（物理约束）：任何免域名方案都要求用户在某个服务商处有一次身份，只能压到一次，不能消除。

---

## 实施期偏差登记（2026-10-07，评审 B 段实测确认）

下列偏差均**正当**（实现比计划更合理或等价），但计划原文与实现不符，**回写以免下一位执行者照错**：

| #   | 计划原文                                          | 实现实际                                                                                                         | 判定                              |
| --- | ------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- | --------------------------------- |
| 1   | Task 7 Files 未列 `Cargo.toml`                    | 加了 reqwest `blocking` 特性（可达性探测需同步客户端，`tailscale.rs` 在其专职 std::thread 里跑、不在异步上下文） | 正当，补记                        |
| 2   | Task 6 写「三条 IPC 命令」                        | 实际**两条**（第三条由 `run_step("status")` 覆盖）                                                               | 正当，改口径                      |
| 3   | Task 6 Step 4 要求 `download_url_to` **流式写盘** | 实现用 `bytes()` 全量入内存（与既有 `tunnel.rs` 下载同形）                                                       | 等价，无功能影响                  |
| 4   | Task 5/7 只提了 `set_ts_snapshot` 一个测试缝      | 实际新增 `RUN_CLI_OVERRIDE` / `HTTP_PROBE_OVERRIDE` 两个缝（**好事**：CLI 与网络都可注入，符合项目零网络红线）   | 改进，补记                        |
| 5   | Task 5 未提**托盘**                               | 托盘仍只消费 `first_available_tunnel_url`，**不消费 tailscale 地址** → §C1 输入输出表的「托盘可复制」**无归属**  | **计划漏项**，见 Task 5 新增 Step |
| 6   | Global Constraints 写「门禁全绿才提交」           | 实为「**相对基线不新增失败**」——macOS 上 clippy 2 条、`cargo test` 数条为**分支前既存基线红**                    | 口径已在本节澄清，见下            |

**门禁口径（已澄清）**：本计划的「门禁全绿」= **fmt 全绿 + 前端全绿 + Rust 失败集相对基线不新增**。已知基线红：clippy 2 条（`commands/session.rs:100` 未用 import、`remote/server.rs` 的 `inject_state_with_probe` 在 macOS 上 dead-code）；`cargo test` 的 `session_mode_switch_*` 等数条（另需 `pnpm build:mobile` 产物与合适 `$HOME` 才不误报）。**新批次不得新增失败，也不必修基线红**（基线红另开批次，需用户裁决）。

## 任务 ↔ 里程碑对照

| 设计说明书里程碑                      | 本计划 Task                              | 状态 |
| ------------------------------------- | ---------------------------------------- | ---- |
| 1 零豁免 + 判据退役 + 展示解耦        | Task 1 + Task 2                          | ⬜   |
| 2 同机反代免密洞锚点由红转绿          | Task 1（Step 1/4，工作区已有那条红测试） | ⬜   |
| 3 文案口径：本机也需访问密码          | Task 4                                   | ⬜   |
| 4 限速分桶修正 + 全局上限             | Task 3                                   | ⬜   |
| 5 通道本体                            | Task 5                                   | ⬜   |
| 6 首次配置引导                        | Task 6                                   | ⬜   |
| 7 强制可达性校验 + 自愈               | Task 7                                   | ⬜   |
| 8 UI 线稿更新                         | Task 8                                   | ⬜   |
| 9 通道区重排 + 卡片三态               | Task 9                                   | ⬜   |
| 10 文件区带宽提示 + 进度条 + 满速指引 | Task 10                                  | ⬜   |
| 11 文档收口                           | Task 11                                  | ⬜   |
| 12 Windows 步骤表实测校验             | Task 12（可延后）                        | ⬜   |
