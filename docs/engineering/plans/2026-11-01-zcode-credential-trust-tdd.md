# ZCode 凭据复用与稳定应用身份 TDD 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: 使用 superpowers:subagent-driven-development 或 superpowers:executing-plans。K1 是新增代码 TDD；K2 是既有能力回归与交付，不伪造历史 RED。

**Goal:** 提供使用既有签名身份构建可验收 App 的入口，保留已实现的 JWT 复用，明确常规重启与开发重编译的提示边界。

**Architecture:** 不改方案 A 的原生 Keychain/Windows/Linux 存储；通过 Tauri 支持的签名配置产出稳定身份 App，避免在 Rust 编译前给旧二进制签名的假修复。真实钥匙串验收单列。

**Tech Stack:** Node ESM、现有 Tauri CLI、Vitest；零新依赖。

**Spec:** [设计 §5](../specs/2026-11-01-zcode-local-queue-and-controls-design.md)；[总索引](./2026-11-01-zcode-remote-control-tdd.md) 的 R01/R02。

## Global Constraints

- debug service 保持 `com.jarvis.tuvis.dev.zcode`，release 保持 `com.jarvis.tuvis.zcode`；不迁移/清除条目。
- 不创建/安装证书、不读取私钥、不改 Keychain ACL、不明文保存 JWT、不运行真实 App。测试只检查命令计划/假 runner/假保险箱。
- 本机已检查的 Tauri CLI 支持 `build --debug --bundles app --config <JSON>`，配置 schema 有 `bundle.macOS.signingIdentity`；实施时核对 lockfile 版本，不能想当然把 beforeDevCommand 当“编译后签名”。
- 没有既有签名材料时明确缺外部条件；不能把脚本单测绿称为“用户重启不再弹窗”。不阻塞队列/模型/模式代码。

## Review Focus

- 身份字符串含空格/引号：K1 参数数组传入，禁止 shell 拼接。
- 未配置/非 macOS：K1 失败前零构建、零安全命令。
- 签名步骤在 Rust 构建之前执行：K1 使用 Tauri bundle 配置，拒绝伪热重载修复。
- 凭据恢复失败被当成 OAuth 失效：K2 假体回归仍正确区分。
- 签名同构建重启与代码重编译：K2 单独记录，不能互相替代。

### K1：稳定身份 App 构建入口

**Files:** Create `scripts/zcode-signed-build.mjs`、`tests/scripts/zcodeSignedBuild.test.ts`；Modify `package.json`。

**Interfaces:** 导出 `createSignedBuildPlan({platform,identity,projectRoot}):{command:string,args:string[],cwd:string}`、async `runSignedBuild(plan,runner):Promise<void>`。identity 输入为 `TUVIS_APPLE_SIGN_IDENTITY` 的已存在证书名称；缺失即失败。不列举/创建证书，不接收证书私钥。

产物命令固定为参数数组：`pnpm exec tauri build --debug --bundles app --config <JSON>`，JSON 仅追加 `bundle.macOS.signingIdentity=identity`，保留现有 bundleId/版本/保险箱命名空间。新 package script 为 `build:macos:signed-dev`，普通 tauri:dev 不伪称已因此稳定身份。

- [ ] **RED：**写 `identity_is_a_single_config_argument`、`missing_identity_never_runs_build`、`non_macos_has_no_side_effect`、`signed_build_does_not_launch_app_or_touch_acl`。断言 runner 只调用一条 Tauri build、cwd准确、JSON identity原样等于输入；恶意引号不产生额外命令；参数不含 security/证书导入/启动App/更改用户目录。

```ts
expect(JSON.parse(plan.args.at(-1)!).bundle.macOS.signingIdentity).toBe(identity);
expect(plan.args.slice(0, 7)).toEqual([
  "exec",
  "tauri",
  "build",
  "--debug",
  "--bundles",
  "app",
  "--config",
]);
expect(runner).not.toHaveBeenCalled(); // 缺 identity 或非 macOS 的分支
```

- [ ] **验证 RED：**`pnpm exec vitest run tests/scripts/zcodeSignedBuild.test.ts --pool=threads`，新导出未实现/参数断言失败。
- [ ] **最小实现：**使用 `spawn` 的参数数组和 `shell:false`；模块 import 不运行构建。入口显式运行时才构建，失败退出非零，不回退 adhoc。新增 script 不进入现有 dev-preflight，不自动替换 /Applications 中 App，不修改 release workflow 的证书 Secrets。
- [ ] **GREEN：**所有假 runner 测试通过；解析本地 CLI config.schema 确认字段存在，命令 help确认参数合法。仅产构建入口，不用真实证书签名取证。
- [ ] **Review/提交：**`feat(dev): add explicit signed macos debug app build`。

### K2：原生恢复回归、说明与平台验收表

**Files:** 既有 `src-tauri/src/services/zcode_control/{auth,credential_store,auto_connect}.rs` 与 `src-tauri/src/remote/mod.rs` 测试；Modify 中英 `docs/user-manual/{zh,en}/08-mobile-remote.md`、执行报告。除回归暴露缺陷外，不改已稳定的授权生命周期。

- [ ] **先验证既有 GREEN：**`cargo test --locked --lib services::zcode_control::auth --quiet`、`cargo test --locked --lib services::zcode_control::credential_store --quiet`、`cargo test --locked --lib services::zcode_control::auto_connect --quiet`（src-tauri 下）。复核 startup_wiring 相关假体：一次启动恢复、不重复冷读，worker用内存；status轮询零 OS读取，401/网络失败/忘记失败区别保留。
- [ ] **若确有新缺陷才进入 RED→修复→GREEN：**先以计数假体复现重复 load，不能因系统弹窗就重写 AuthService；保持 epoch/store_lock/purge_pending 与启动单次标记。没有缺陷就记录回归，不造“新增TDD修复”。
- [ ] **文档交付：**解释系统“允许”与“始终允许”；稳定签名入口与普通开发热重载的差别；同一构建/签名升级/重新编译/身份改变四类结果分开。说明 JWT持久化不是永久免系统确认。
- [ ] **真实验收表保持 NOT RUN：**专用条目首次授权后同构建完全退出/冷启动3次；同签名重新构建/升级；开发普通热重载；拒绝/钥匙串锁定；Windows/Linux原生库。只有真实执行才填写提示次数与 PASS；无证书记外部条件待就绪，不伪造。
- [ ] **Review/提交：**`docs(zcode): document credential trust boundaries and acceptance`；若有实际代码修复另提交并附新 RED。R01 可代码回归闭合；R02 需真实同身份验收后才可功能闭合。
