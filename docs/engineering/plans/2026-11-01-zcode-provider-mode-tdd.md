# ZCode 供应商与 R10 模式切换 TDD 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: 使用 superpowers:subagent-driven-development 或 superpowers:executing-plans。C1/C2、M1/M2 分别闭环，R10 不作为可选项跳过。

**Goal:** 手机按供应商/模型/思考强度一次切换，并实现当前 build 的计划、变更前确认、自主和完全访问模式。

**Architecture:** 扩展既有目录、Selection 命令及其确认；Mode 独立动作/能力，沿现有官方信封和同一会话写锁。候选只由当前 build 的静态链路与运行形状建立，用户真实动作后有界确认，不后台试写。

**Tech Stack:** 现有 Rust/React/TypeScript/Vitest；不新增依赖。

**Spec:** [设计 §4、§6](../specs/2026-11-01-zcode-local-queue-and-controls-design.md)、[R10 证据](../reviews/2026-11-01-zcode-mode-static-evidence.md)；继承 [总索引](./2026-11-01-zcode-remote-control-tdd.md)。

## Global Constraints

- build `3.16.2.8769`、协议 3；不升级、不真实写入、不改 MASTER-PLAN/AGENTS；先完成 G0。C1 可在 Q 主线后台完成后开始，M1 使用 Q3 的新鲜状态及共享会话锁。
- R10 四标签固定：plan=计划模式、build=变更前确认、guarded=自主模式、yolo=完全访问；edit 只读兼容。不照抄旧文档 guarded 中文简称。
- 目录只来自本机官方当前配置；不读取/下发 API Key。模型档位属于具体 provider+model，不取并集。
- ACK 不等于生效；未知不重发、不恢复旧值；不修改原有审批卡 fullAccess/常驻选项硬闸。
- 代码/自动验证全部纳入本批，真实切换和实际审批效果单列 NOT RUN；不能把“UI 能选”写成“权限保护已生效”。

## Review Focus

- 同名模型分属不同 provider：C1 指纹和确认精确区分。
- 供应商切换到模型无合法档位：C1/C2 不造默认 high/max。
- config.mode 与 planEnabled 组合：M1 规范化后确认，不只比原字符串。
- 桌面同步改变配置/任务起跑：C1/M1 用同一会话锁和写前现读，CAS 失败不盲写。
- 手机选择 yolo 后取消/网络未知/重启：M2 零隐式扩权、零自动重发。

### C1：可信分组目录与跨 provider 命令闭环

**Files:** Modify `src-tauri/src/services/zcode_control/{types,selection,bridge,command,action,hub}.rs`、`src-tauri/src/services/zcode_control/capability/{interface,state,resolver}.rs`、`src-tauri/src/remote/zcode.rs`、`src/lib/api/zcodeControl.ts`；Test selection.rs、command.rs、wire_e2e.rs 及 HTTP e2e。

结构新增字段同步 projection/wire_e2e/auto_connect 和 tests/common/zcode.rs、zcode_control_e2e/remote 中的 SelectionView/SessionView 构造；只补兼容默认，不改变旧场景预期。

**Interfaces:**

- `ProviderChoice {provider_id:String,label:String,models:Vec<ModelChoice>,available:bool,reason_code:Option<String>}`；`SelectionView.providers:Vec<ProviderChoice>`（serde default），原 provider/model/thought/models 保留兼容，models 是当前 provider 的派生视图。
- `ControlAction::Selection.provider:Option<String>`（serde default）；HTTP 仅 selection 允许此字段，不允许在顶层或别的动作任意自报 provider。
- 新 `build_selection_command_for_provider(view:&SessionView,provider:&str,model:&str,thought:&str)->ZResult<Value>`；旧 `build_selection_command(view,model,thought)` 作为当前 provider 包装，防止旧调用方静默改语义。
- 复用 `SelectionPort::load(target)->BoxFuture<ZResult<(SessionView,SelectionView)>>`、`wait_selection(target,provider,model,thought,baseline_revision,timeout)->BoxFuture<bool>`；含义从当前 provider 目录扩为经 G0 核实的可用分组。目标 provider 必须传给 wait 与 Hub 确认账本。

- [ ] **RED：**用 G0 合成目录写 `two_providers_with_same_model_are_distinct_targets`、`cross_provider_switch_requires_its_own_levels`、`provider_enters_payload_fingerprint`、`old_selection_without_provider_keeps_current_scope`。精确断言：

```rust
assert_eq!(cmd["payload"]["provider"], "provider_b");
assert_ne!(hash_for_provider_a, hash_for_provider_b);
assert!(foreign_level_command.is_err());
assert_eq!(legacy_cmd["payload"]["provider"], current_provider);
```

- [ ] **验证 RED：**src-tauri 下 `cargo test --locked --lib services::zcode_control::selection --quiet`、`cargo test --locked --lib services::zcode_control::command --quiet`。不能把 fake 的“所有模型均支持所有档位”当目录。
- [ ] **最小实现：**按 G0 找到的官方 configured registry/可用性字段分组，不借营销目录；标签缺失可安全显示 providerId。过滤不可用 provider 的写资格；解析返回不含凭据。Some(provider) 加入指纹；None 保留旧指纹形状。写前取得 Q3 的同会话锁并现读目录/ready/idle/no pending，发带 baseRevision 的一帧；不先 provider-only 再 model 写两次。
- [ ] **GREEN：**加 `selection_confirms_target_provider_not_original`、`catalog_is_invalidated_after_provider_change`、`catalog_read_precedes_write_gate`、`live_delta_preserves_all_provider_catalogs`；保留现有 GET 惰性目录读取，不把取目录挪回写能力闸之后形成死门。旧 GET/SSE 不回退配置、目标换代/拒绝/超时不写 confirmed、其他 provider 坏项不拖垮合法目录、缺 provider 兼容。HTTP 正例须穿过 ACTION_KEYS，否则纯构造绿不能证明能切。
- [ ] **Review/提交：**`feat(zcode): switch models across configured providers`。报告目录资格的确切源字段、production load/wait 接线、仍待真实切换验证项。

### C2：两个联动选择器与一次提交

**Files:** Modify `src/mobile/ZcodeControlPanel.tsx`、`src/lib/api/zcodeControl.ts`、`src/i18n/locales/{zh,en}.json`；Create `tests/mobile/ZcodeProviderSelection.test.tsx`；Modify `tests/mobile/{ZcodeControlPanel,ZcodeControlFlow}.test.tsx`。

**Interfaces:** 本地 `PendingModelChoice {provider:string,model:string,thought:string}`；纯函数 `resolveModelChoice(view:SelectionView,provider:string,model?:string):PendingModelChoice|null`（可位于新 `src/mobile/zcode-selection-state.ts`）。只选目录明确支持的组合，null 表示无可写组合。

- [ ] **RED：**`provider_change_alone_emits_zero_post`、`choosing_model_sends_one_full_triple`、`model_without_thought_levels_is_not_faked`、`rejected_switch_keeps_host_selection`；UI 选 provider B 零 POST，再选 B/model2 恰一 selection，带 provider/model/合法 thought；不能继承 A 的 max。

```tsx
expect(posts).toHaveLength(0); // 只选 provider_b 之后
expect(posts[0].action).toMatchObject({
  kind: "selection",
  provider: "provider_b",
  model: "model2",
  thought: "low",
}); // 再选该模型，fixture 中唯一合法档位为 low
expect(posts).toHaveLength(1);
```

- [ ] **验证 RED：**`pnpm exec vitest run tests/mobile/ZcodeProviderSelection.test.tsx tests/mobile/ZcodeControlPanel.test.tsx --pool=threads`。
- [ ] **最小实现：**供应商/模型两个单选下拉框，保留思考档位与官方页面。变更组合仅在空闲且无 pending 时可写，用户选择模型后一次完整提交；在途显示应用中，confirmed/新鲜官方配置才成为实际选择。运行中可准备本地选择，不自动停止回合。缺目录/拒绝可见原因，不返回旧 provider 的目录冒充新目录。
- [ ] **GREEN：**同帧 model/permission/queue 写槽互斥；配置 SSE/GET 收敛、切会话清待选、长名称/中英/320/360/390、100/125% 不卡正文不藏按钮。原 R08 同供应商切模型/档位完整回归。
- [ ] **Review/提交：**`feat(mobile): select zcode provider and model together`。

### M1：R10 Mode 投影、能力、命令与确认

**Files:** Create `src-tauri/src/services/zcode_control/mode.rs`；Modify `src-tauri/src/services/zcode_control/{types,pending,projection,bridge,command,action,hub,mod}.rs`、`src-tauri/src/services/zcode_control/capability/{state,key,interface,resolver,confirm}.rs`、`src-tauri/src/database/dao/zcode_capability.rs`、`src-tauri/src/remote/zcode.rs`、`src/lib/api/zcodeControl.ts`；Test mode.rs/command.rs、DAO 测试、HTTP e2e。

**Interfaces:**

- `CollaborationMode::{Build,Edit,Plan,Yolo,Guarded}`（wire lowercase）。首版目标允许 Plan/Build/Guarded/Yolo，Edit 只读。
- `ModeView {raw_mode:Option<CollaborationMode>,plan_enabled:Option<bool>,effective_mode:Option<CollaborationMode>}`；`SessionView.mode:Option<ModeView>`（serde default）；`normalize_mode(raw:Option<&str>,plan_enabled:Option<bool>)->Option<CollaborationMode>`：true 优先为 Plan；其余按 G0 证明的缺省/规范化语义，证据未知不能把 None 直接当 false。
- `ControlAction::Mode {session_id:String,generation:u64,mode:CollaborationMode,confirm_full_access:bool}`（确认字段 default=false）；`build_mode_command(view:&SessionView,mode:CollaborationMode,confirm_full_access:bool)->ZResult<Value>`，payload **仅** `{mode}`，带 baseRevision。
- `ModePort:Send+Sync`：`load<'a>(&'a self,target:&'a BoundTarget)->BoxFuture<'a,ZResult<SessionView>>`；`wait_mode<'a>(&'a self,target:&'a BoundTarget,mode:CollaborationMode,baseline_revision:u64,timeout:Duration)->BoxFuture<'a,bool>`；BridgeManager 实现，CommandSender 加 `with_mode_port(Arc<dyn ModePort>)->Self` 和 execute_mode 分支。
- 新 `Capability::Mode`、`CapabilityView.mode:CapabilityFlag`、`ZcodeInterfaceEvidence.method_mode_switch/has_mode_config:bool`，前后端能力清单由 11 扩为 12。已知 build/协议 3 + G0 handler/schema 证据 + 新鲜可规范化模式形状为候选；未知 build/缺形状关闭。Mode 不借 model/approvalReply/官方 queue 的判断。

`method_mode_switch` 必须由“既有 sendConversationCommandV4 方法在场 + 已知 build 的 Mode handler/schema 证据”派生；switchCollaborationMode 是信封 type，不能等一个不存在的同名独立 RPC，否则会重新形成永久关闭的能力死门。

- [ ] **RED：**`mode_mapping_matches_installed_labels`、`plan_enabled_prevents_false_build_confirmation`、`full_access_requires_explicit_confirmation`、`mode_candidate_needs_known_build_and_shape`、`mode_fingerprint_includes_target_and_confirmation`。

```rust
assert_eq!(ui_label(CollaborationMode::Build), "变更前确认");
assert_eq!(ui_label(CollaborationMode::Guarded), "自主模式");
assert_eq!(normalize_mode(Some("build"), Some(true)), Some(CollaborationMode::Plan));
assert!(build_mode_command(&view, CollaborationMode::Yolo, false).is_err());
assert_eq!(wire["payload"], serde_json::json!({"mode":"guarded"}));
```

测试标签可在 fixture 映射表断言，不把中文写进 Rust 生产逻辑。规范化的更多组合按 G0 结果写入 fixture，而不是修改 fake 迎合实现。

- [ ] **验证 RED：**`cargo test --locked --lib services::zcode_control::mode --quiet`；`cargo test --locked --lib services::zcode_control::capability --quiet`，预期新模式链未存在/判据失败。
- [ ] **最小实现：**补 mode 读取、命令类型/指纹/严格 HTTP 白名单、required_capabilities、Hub check_policy、生产端口装配与同会话写锁。只在 fresh ready/idle/no pending 允许写；noop 不冒称新成功。ACK 受理后等 revision 前进与规范化目标相符，有界轮询沿既有 50ms/5s 选择确认规则；超时 Accepted/Unknown 分明，不能伪造 confirmed 或自动恢复旧模式。
- [ ] **GREEN：**`mode_accepted_without_config_change_is_not_confirmed`、`mode_capability_recomputes_on_config_sse`、旧 revision/错会话/换代不确认、未知mode字段安全关闭、yolo未确认零帧零账本、后台 restore/resume零模式写、模式变更与 Queue FIFO 互斥；真实 HTTP `mode_action_reaches_one_official_frame` 过 ACTION_KEYS→Hub→CommandSender。原 fullAccess 审批卡拒绝回归仍绿；新增 mode capability 同步更新手机 degradedCapabilities/CAPABILITY_KEYS 和未知 payload 校验。
- [ ] **账本 GREEN：**`all_capability_words_roundtrip`、`mode_ledger_survives_reopen`；DAO 的 capability_from_str 需与 12 项 as_str 互逆，不能只更新枚举而在读库时丢掉 Mode。补齐逆映射不会取消官方队列 W0 闸；普通构建即使旧 queue confirmed 也仍关闭。
- [ ] **结构兼容核对：**按编译器与 `rg 'CapabilityView \{|SessionView \{'` 定位本域及 tests/common/zcode.rs、zcode_control_e2e/remote、auto_connect、wire_e2e 中字面量；mode 缺省必须关闭/未知，不能为修测试顺手全部设 true。
- [ ] **Review/提交：**`feat(zcode): add guarded collaboration mode switching`。回报 R10 支持范围与静态/假链/实机三个证据等级，不能把本任务改回可选。

### M2：R10 手机选择、扩权确认与可读状态

**Files:** Modify `src/mobile/ZcodeControlPanel.tsx`、`src/lib/api/zcodeControl.ts`、`src/i18n/locales/{zh,en}.json`；Create `tests/mobile/ZcodeModeControl.test.tsx`；Modify `tests/mobile/{ZcodeControlFlow,ZcodeControlPanel}.test.tsx`、`docs/user-manual/{zh,en}/08-mobile-remote.md`。

**Interfaces:** `postZcodeAction` 现有联合类型增加 mode；Mode 控件只从 SessionView.mode 与 capabilities.mode 读取，写入沿 Q6 的 runWrite/runAction。UI 默认四选项依次计划/变更前确认/自主/完全访问，不写隐含默认模式。

- [ ] **RED：**`full_access_cancel_sends_nothing`、`full_access_confirm_sends_one_mode_action`、`mode_ack_does_not_optimistically_change_confirmed_label`、`unknown_mode_is_readonly_not_default_build`；yolo对话框默认取消，确定后仅一帧 mode，payload确认标记不丢。

```tsx
expect(posts).toHaveLength(0); // 选 yolo 后取消
expect(posts[0].action).toMatchObject({ kind: "mode", mode: "yolo", confirmFullAccess: true });
expect(posts).toHaveLength(1); // 重新选择并确认后
expect(currentModeLabel.textContent).toBe("变更前确认"); // ACK 到但配置仍 build
```

- [ ] **验证 RED：**`pnpm exec vitest run tests/mobile/ZcodeModeControl.test.tsx tests/mobile/ZcodeControlFlow.test.tsx --pool=threads`。
- [ ] **最小实现：**窄屏按需弹出选择，保持正文优先；选定模式后显示目标和“应用/取消”明确确认，完全访问额外说明会减少确认，默认聚焦取消；只选择列表项不立即写入。权限变更影响范围按 G0 实际 handler 说明，若更新工作区偏好也明示；不新增常驻大卡。Mode 未证原因就地显示，后台不默认写更宽权限。模式配置效果不宣传为每条手机消息必经审批。
- [ ] **GREEN：**四标签与 source fixture 完全一致、Edit/未来值只读、取消/双击/同帧其它写动作、拒绝/未知/会话切换、planEnabled 真实形态映射；既有问答/审批/附件/主按钮回归无损。
- [ ] **Review/提交：**`feat(mobile): control zcode modes with explicit elevation confirmation`；手册写支持模式与实机待验，不把全部枚举都宣称完成。
