# Codex 异步答题契约 Windows 布局兼容 · 实施与验收记录

- 日期：2026-11-04
- 关联 issue/PR：本批 PR（`fix/codex-async-contract-windows`）
- 前置文档：
  - `docs/engineering/acceptance/2026-10-28-v0.6.3-Codex异步答题识别修复验收.md`
  - `docs/engineering/evidence/codex-desktop-ipc/2026-10-28-codex-app-26.1023.51421-async-envelope-reprobe.md`
  - `docs/engineering/specs/2026-10-26-Codex-F01-F08阶段需求与设计.md`

## 1. 问题现象

用户通过 Tuvis 移动端（远程连接）查看 Codex 桌面会话时，异步答题卡显示：

> 「当前 Desktop 版本未匹配到异步答题契约，暂不能从手机提交；答案已保留，可到桌面端作答或查看适配详情」

即 `reason_code = operation_contract_unmatched`，卡片无法提交，且**桌面端作答后卡片不会消失**。

## 2. 根因（真机取证）

### 2.1 不是规则失配，是探测根本没跑到

`capability.rs` 里同一个 `CONTRACT_UNMATCHED` 文案有**两个来源分支**：

```rust
} else if context.contract_error.is_some() {
    codes::OPERATION_CONTRACT_UNMATCHED      // ← 实际走这条
} else if evidence == EvidenceSource::None {
    if context.contract_identified { OPERATION_CONTRACT_UNMATCHED }
    else { OPERATION_NO_EVIDENCE }
}
```

真机日志与账本证据：

```
mam.log: codex-ipc contract: key=windows|26.1029.7803.0|0.166.0-alpha.6|1
         识别失败（contract_package_unavailable）
tuvis.db: codex_ipc_contract_evidence 表 0 行   ← 该机器从未成功识别过任何 build
```

**断点**：`desktop_contract.rs` 的 `cli_path_in_bundle` 硬编码旧路径
`app\resources\codex-cli\bin\codex.exe`；而 Codex ≥ 26.1027 已把随包 CLI 提到
`app\resources\codex.exe`。

实测（`C:\Program Files\WindowsApps\OpenAI.Codex_26.1029.7803.0_x64__2p2nqsd0c76g0`）：

| 路径 | 状态 |
|---|---|
| `app\resources\codex-cli\` | **不存在** |
| `app\resources\codex.exe` | 37 MB ✅ |
| `app\resources\codex` | 319 MB（单文件版） |

### 2.2 版本号双体系（易误导）

| 显示位置 | 值 |
|---|---|
| 界面「版本」 | `26.1029.61003`（来自 `app\resources\owl-app.ini` 的 `AppVersion=`） |
| **MSIX `AppxManifest.xml` Identity** | **`26.1029.7803.0`** ← `profile_for()` 走这个 |

排查时必须以 MSIX 版本为准。

### 2.3 规则层还有两处词法钉死（修好路径后才会暴露）

用 Node 只读解析 `app.asar`（720 MB），**先用仓库 20261028 夹具做阳性对照**确认解析器可信，再在真机包上比对：

| 条件 | v2 要求 | 真机 26.1029.7803.0 实际 |
|---|---|---|
| 信封解包切片 | 字面 `slice(34,-35)` | `slice(Sc.length,-Sd.length)`（标记绑定推导） |
| 题目标识合成 | `function F(P1,P2){return JSON.stringify(...)}` + 调用点 | 内联进 map 箭头 `(e.questions??[]).map((t,r)=>JSON.stringify([...,e.id,r]))` |

**能力本身完整**：信封、守卫、schema、合成、carrier 全链在场，与 26.1023 语义等价。
排除「OWL 定制版砍掉功能」与「压缩器改名」两种假设。

## 3. 修复内容

### 3.1 CLI 路径双布局

`desktop_contract.rs::cli_path_in_bundle` 改为新布局优先、旧布局兜底：

```rust
let resources = bundle.join("app").join("resources");
let modern = resources.join("codex.exe");                                  // ≥26.1027
let legacy = resources.join("codex-cli").join("bin").join("codex.exe");    // ≤26.1016
```

保持「只 `is_file()` 判定、不执行、不读内容」的既有契约。

### 3.2 契约规则 v3

- **切片**：接受数字字面量 **或** 从已核验的标记绑定取 `.length`
  （后者天然免疫标记长度变化）。
- **合成**：接受命名函数+调用点 **或** 内联进 `.map` 箭头；
  内联形态**复用同一套参数关系判据**（第三元素 = map index 形参、
  第二元素 = 同 map 源条目的 `.id`、map 源 = `(<条目>.questions??[])`）。
- `requires` 里的 `slice(34,-35)` 字面门槛改为 `.slice(`（形态由 structural 规则核验）。
- `CONTRACT_RULE_VERSION` **2 → 3**：v2 证据行对该 build 落账为「不匹配」，必须作废重识别。

### 3.3 移动端手动收纳

组件 `AsyncQuestionGroupCard` 早有 `onArchive` 按钮（「在 Tuvis 中收纳本组」），
但 `SessionDetail` **从未传该 prop**。本批接上：

- 收纳写 `sessionStorage` 标记 → `inbox` 过滤隐藏该组，**不向桌面发送任何动作**。
- 已收纳组单独回传，dock 顶部提供「恢复显示」入口。
- 收纳键含 `sourceTurnId/sourceItemId`：桌面若有新回合，题目换代即生成新 key，
  卡自然重现——不会被一次收纳永久吞掉。

## 4. 测试

### 自动化

| 用例 | 断言 |
|---|---|
| `cli_path_accepts_both_windows_layouts` | 新布局命中 / 旧布局命中 / 两者皆无 → None |
| `async_contract_accepts_derived_slice_lengths` | `slice(Sc.length,-Sd.length)` 形态匹配成功 |
| `async_contract_accepts_inline_question_id_synthesis` | 内联形态匹配成功 |
| `async_contract_rejects_inline_with_index_derived_item` | **阴性锁**：内联但第二元素取 index → 拒绝 |
| `contract_rule_version_bumped_for_structural_async_rule` | 版本 = 3 |

`cargo test --lib codex_desktop_ipc`：**298 passed, 0 failed**。

## 5. 真机验收（待执行）

前置：重启 Tuvis（使新规则生效，并让契约证据按 v3 重探）。

### 5.1 契约识别恢复

| 项 | 期望 |
|---|---|
| `mam.log` 中 `codex-ipc contract` 行 | 不再出现 `contract_package_unavailable` |
| `~/.tuvis/tuvis.db` 的 `codex_ipc_contract_evidence` | 出现 `windows\|26.1029.7803.0\|0.166.0-alpha.6\|1` 行 |
| 移动端卡片 | 不再显示「未匹配契约」，按钮可提交 |

### 5.2 「正式提交 → Desktop 原卡撤下」闭环

> 该路径在 2026-10-28 验收中标注为**未执行（待验）**，本批必须补做。

在**隔离会话**中验证四件事同时成立：

1. 手机提交回执 `Accepted`
2. Desktop 原问答卡撤下
3. 会话里出现 `<send_user_message_question_reply>` 信封正文
4. 模型按答案继续

任一项不成立记失败。

### 5.3 收纳与恢复

| 步骤 | 期望 |
|---|---|
| 点「在 Tuvis 中收纳本组」 | 卡片立即隐藏，dock 顶部出现「已收纳 1 组」 |
| 点「恢复显示」 | 卡片重新出现，草稿仍在 |
| 桌面该会话产生新回合 | 旧收纳不影响新题（新 key） |

## 6. 已知限制

- **桌面已答 → 移动端自动消失仍未根治**：Tuvis 判断「已答」的唯一手段是
  「userMessage 文本信封 + questionItemId 逐字符配对」（`async_question.rs:424-452`），
  而 Codex 的 canonical state **没有** answered/resolved 字段。若镜像漏掉作答事件，
  投影永远 pending。本批只提供**手动收纳**作为出口；服务端主动对账（有活卡 + 转 idle
  时触发有界重取）为后续 follow-up。
- **compat 档不因此解锁写入**：`profile_for` 白名单未新增该 build，
  写能力仍走契约证据门控（`auto_adapt_enabled && contract`）。
  若要 shipped 档位，需补完整只读探测取证后单独登记。
- **macOS 未验证**：本批形态变化取证仅限 Windows 26.1029.7803.0；
  macOS 包若出现同类内联/推导形态，v3 规则同样适用（形态判据平台无关），
  但未实测。
