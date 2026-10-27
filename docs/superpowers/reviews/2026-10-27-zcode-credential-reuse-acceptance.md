# ZCode 凭据复用验收报告（H3）

- 分支：`codex/zcode-unified-remote`
- 验收代码基线：`328c9395`（H3 收口；终审小修随后）
- 验收平台：macOS 27.0.1 (26A434) arm64（开发机）
- 工具链：rustc 1.97.1 / cargo 1.97.1 / Node v26.8.1 / pnpm 11.23.0
- 范围：C1–C4 凭据持久化与自动恢复、H1/H2 生命周期加固之后的**真实组件假链**验收、自动化门禁与平台边界申报
- 纪律：不改 `docs/MASTER-PLAN.md`；不访问真实系统保险箱、真实官方账号、真实 relay、真实进程重启；无 JWT / 真实链接 / 正文入库；未 push / merge / tag / 发版

## 一、总判定

| 维度 | 判定 | 依据 |
| --- | --- | --- |
| `code_complete`（本批 H3 范围） | **是** | 4 个真实组件假链 e2e 全部 GREEN；集成 4 套件、全量普通/证据测试、clippy、fmt、pnpm 门禁见 §三 |
| `platform_validated` | **否** | 真实系统保险箱、真实官方 OAuth、真实进程重启在 macOS / Windows / Linux 均**未执行**；见 §四，全部 `awaiting_platform_validation` / `not_run` |
| `real_official_verified` | **否** | 本批 e2e 使用回环假官方（HTTP/WS）与 fake 持久存储；未连接真实官方端点、未使用真实账号 |
| `spec_complete` | **本批范围是；全产品否** | 凭据复用合同的自动证据闭合；但平台实机与真实官方链路未跑，不能把「自动门禁绿」写成「平台验收通过」 |

**一句话结论：代码与自动门禁完成，平台与真实官方验收未完成。**

## 二、验收对象与运行形态

| 项 | 值 |
| --- | --- |
| 被测生产组件 | `AuthService`（真实 OAuth 交换、保存/恢复/失效删除）、`PartsBackend::production_test`（真实 RelayClient / BridgeManager / CommandSender / ZcodeHub / 事件泵） |
| 假替身边界 | 回环假官方 HTTP（OAuth token、terminal token）与回环假 WS（`auth_init/auth_ack`、bridge RPC 应答）；只有端点与官方服务被替换 |
| 持久存储 | `MemoryCredentialStore`（`#[cfg(test)]` 记录型 fake），**不是**真实 Keychain / Credential Manager / Secret Service |
| 进程重启模拟 | 断开并丢弃旧 `AuthService`/`Hub`，在**同一个** fake 持久存储上重新构造两者；不调用 `mark_authorized_for_test` |
| 合成数据 | 合成 JWT `header.payload.sig`（仅远期 exp）、合成 terminal token、合成 SID / workspace / 会话 ID；零真实账号数据 |
| 测试构建 | `cargo test` debug test profile；证据构建追加 `--features marker-helper,zcode-bridge-evidence` |

## 三、自动化验收（逐行）

### 3.1 H3 新增 e2e（真实组件假链）

命令基线：`cd src-tauri && cargo test --lib services::zcode_control::wire_e2e`（GREEN 运行退出码 `0`，13 passed / 0 failed）

| ID | 平台 / 构建 | 用例 | 预期 | 实际 | 命令 | 退出码 | 是否原生 | 是否真实官方 | 判定 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| H3-E2E-01 | macOS 27.0.1 arm64 / debug test | `whole_new_process_state_reuses_vault_without_oauth` | 第一次 OAuth + 自动连接后丢弃旧 Auth/Hub；新实例 restore → 第二次 terminal token / WS → paired；OAuth 仍 1 | PASS：OAuth=1、terminal token=2、WS accepted=2、vault save=1 / load=1、新实例 paired | `cargo test --lib services::zcode_control::wire_e2e` | `0` | 否（fake 持久存储 + 回环假官方） | 否 | PASS（自动化） |
| H3-E2E-02 | 同上 | `default_closed_capabilities_remain_closed_after_restore` | 恢复后八项能力仍默认关闭；send 409 且零业务帧 | PASS：八项 `enabled=false` + `needs_evidence`；POST send 409、零命令帧、零新增上游请求 | 同上 | `0` | 否 | 否 | PASS（自动化） |
| H3-E2E-03 | 同上 | `paused_or_forgotten_restart_has_zero_upstream` | 断开（auto=false）/ 忘记后新实例零 WS、零 terminal token | PASS：两分支 restart 后 WS 增量 0、terminal token 增量 0；忘记后记录已删 | 同上 | `0` | 否 | 否 | PASS（自动化） |
| H3-E2E-04 | 同上 | `auto_connection_only_never_dispatches_messages` | 自动连接只配对，零业务命令帧 / 零账本 / 零审计 | PASS：配对后 150ms 观察窗内 `sendConversationCommandV4`=0、账本=0、审计=0 | 同上 | `0` | 否 | 否 | PASS（自动化） |

RED 证据：测试写完后，临时在 `AuthService::restore` 顶部插入 `return self.status();` 破坏桩（运行后已还原，`auth.rs` 无 diff）。

| 命令 | 预期 | 实际 | 退出码 |
| --- | --- | --- | --- |
| `cargo test --lib services::zcode_control::wire_e2e`（破坏桩） | 依赖恢复的用例失败 | `10 passed; 3 failed`：`whole_new_process…`（启动恢复必须取回授权）、`default_closed…`（重启后应恢复授权）、`paused_or_forgotten…`（重启恢复仍应看到已保存授权）失败；`auto_connection_only…` 不依赖恢复故通过 | `101` |
| 同上（还原后） | 全绿 | `13 passed; 0 failed` | `0` |

日志：`/tmp/h3-red.log`、`/tmp/h3-green-wire.log`。

### 3.2 集成套件与全量测试

| 命令 | 结果 | 退出码 |
| --- | --- | --- |
| `cargo test --test zcode_control_remote --test zcode_control_e2e --test zcode_config_remote --test zcode_read_remote` | 25 + 8 + 3 + 5 passed；0 failed | `0` |
| `cargo test --features marker-helper` | 3912 passed / 0 failed / 46 ignored（36 个测试目标） | `0` |
| `cargo test --features marker-helper,zcode-bridge-evidence` | 3931 passed / 0 failed / 46 ignored（36 个测试目标） | `0` |
| `cargo clippy --all-targets --features marker-helper -- -D warnings` | Finished，0 warnings | `0` |
| `cargo clippy --all-targets --features marker-helper,zcode-bridge-evidence -- -D warnings` | Finished，0 warnings | `0` |
| `cargo fmt --check` | 重新运行通过（`/tmp/h3-fmt-final.log`，`FMT_EXIT:0`） | `0` |
| `pnpm test`（首轮，与 cargo 全量证据编译并行） | **6 failed / 2356 passed / 5 skipped**；失败集中在 `tests/mobile/SessionDetail.test.tsx`（超时 15s、滚动断言 `expected +0 to be 5000`、缺 `bookmark-jump-miss`）与 `tests/usage/v2-{acceptance,identity-ui}.test.tsx`（5s 超时） | `1` |
| 上述 3 个失败文件空载复跑 | 3 files / 133 passed / 0 failed | `0` |
| `pnpm test`（空载全量复跑） | **2362 passed / 5 skipped（194 文件）/ 0 failed** | `0` |
| `pnpm check` | prettier + eslint + i18n + error-codes + tsc/build 全通过 | `0` |
| `pnpm build:mobile` | built | `0` |

**稳定性说明（如实申报）**：`pnpm test` 首轮与 cargo 全量证据构建并行执行，6 个失败全部是移动端/用量页的超时或滚动定位断言，不涉及 ZCode 设置卡与本次改动；同一批 3 个文件与全量套件在空载下复跑均 0 failed。报告把首轮失败原样保留，不把复跑通过写成「从未失败」。

日志：`/tmp/h3-green-integration.log`、`/tmp/h3-full-normal.log`、`/tmp/h3-full-evidence.log`、`/tmp/h3-clippy-normal.log`、`/tmp/h3-clippy-evidence.log`、`/tmp/h3-fmt-final.log`（`cargo fmt --check` 重新运行，`FMT_EXIT:0`）、`/tmp/h3-pnpm-test.log`、`/tmp/h3-pnpm-flake-retry.log`、`/tmp/h3-pnpm-test-rerun.log`、`/tmp/h3-pnpm-check.log`、`/tmp/h3-pnpm-mobile.log`。

## 四、平台与真实官方验收（未执行，禁止标 PASS）

### 4.1 平台矩阵

| 平台 | 构建 / 环境 | 真实保险箱读写 | 真实 OAuth | 真实进程重启后自动恢复 | 判定 |
| --- | --- | --- | --- | --- | --- |
| macOS 27.0.1 arm64 | debug 测试构建（开发机） | 未执行（只用 `MemoryCredentialStore`） | 未执行（回环假官方） | 未执行（同进程内重建 Auth/Hub） | `awaiting_platform_validation` |
| Windows | 未构建、未运行 | 未执行 | 未执行 | 未执行 | `not_run` |
| Linux | 未构建、未运行 | 未执行 | 未执行 | 未执行 | `not_run` |

### 4.2 真实边界逐项

| 项 | 预期（若执行） | 实际 | 命令 | 退出码 | 是否原生 | 是否真实官方 | 判定 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| macOS Keychain 保存 / 读取 / 删除 | 授权后单条目写入；重启可读；忘记后删除；锁定/拒绝如实报错 | 未执行 | 未执行（无脚本） | `—` | 是（原生保险箱） | 是 | `not_run` |
| macOS 真实 OAuth + 真实 relay | 用户本人授权 → terminal token → paired | 未执行 | 未执行（需用户授权） | `—` | 是 | 是 | `not_run` |
| macOS 真实进程重启 | 完全退出 App 后重启 → 自动恢复 → 自动连接一次 | 未执行 | 未执行（需重新打包/实机） | `—` | 是 | 是 | `awaiting_platform_validation` |
| Windows Credential Manager（2,560 字节边界、当前用户、删除映射） | 保存/读取/删除与错误映射符合 C1；重启恢复 | 未构建（无 Windows 主机） | 未执行 | `—` | 是 | 是 | `not_run` |
| Linux Secret Service（DBus 可用 / 服务缺失 / 锁定） | vendored 构建成功；保存/读取/删除与错误映射符合 C1 | 未构建 | 未执行 | `—` | 是 | 是 | `not_run` |
| 真实链接失效 / 官方 401 / 撤销 | 删除记录并回「需要重新授权」；网络错误不删除 | 未执行 | 未执行 | `—` | 是 | 是 | `not_run` |
| 两台已配对手机同时读取授权状态 | 手机只读既有 `ConnectionView`（无 AuthStatus 接口），无 JWT / terminal token | 未执行 | 未执行 | `—` | 是 | 是 | `not_run` |

## 五、偏差与说明

1. **`FakeOfficialHttp` 增加计数**：为断言「OAuth 仍一次 / terminal token 每次连接一次」，假官方端点新增 `oauth_exchanges`、`terminal_token_requests` 两个原子计数；只在测试 fixture 内，不触生产代码、不改变响应。
2. **共享 fake 存储而非真实保险箱**：按 brief 用 `MemoryCredentialStore` 共享同一持久记录，重新构造 AuthService/Hub 模拟整进程；这证明**生产恢复编排**，不证明平台保险箱本身。
3. **能力断言改为业务请求白名单**：`default_closed_capabilities_remain_closed_after_restore` 只比较 bind / 订阅 / hello / initialize / 命令五类**业务请求**；`rpc-frame-ack` 是生产对假体 RPC 帧的传输层入站回执，不属于业务请求、也不由 send 触发，因此不放入白名单。断言直接锁定要断言的项（新业务请求为零），不再对整份日志做排除式比较。命令帧计数与 409 断言不受影响。
4. **RED 为破坏桩证据**：目标行为在 C1–H2 已实现，H3 无新增生产实现；因此 RED 采用临时恢复桩证明新用例可证伪（与 H2 同法），不是「先写实现再写测试」的首次运行记录。
5. **未改前端设置卡测试**：C4 已覆盖授权状态 / 忘记按钮 / 零 connect 等交互；H3 未发现需要新增的前端契约，`tests/settings/zcodeRemoteSettings.test.tsx` 无 diff。
6. **未跑项目**：真实 Keychain、真实 OAuth、真实重启、Windows/Linux 构建与运行、真实官方链路、两台手机并发均为未跑；不以 fake 结果替代。

### 5.1 复审修整（2026-10-28）

1. **fmt 证据更正**：不再引用格式化前失败的 `/tmp/h3-fmt.log`；重新运行 `cargo fmt --check`，`/tmp/h3-fmt-final.log` 记录 `FMT_EXIT:0`。
2. **能力断言更正**：删除对 `rpc-frame-ack` 的整日志排除式过滤，改为**业务请求白名单**（bind / 订阅 / hello / initialize / 命令）；并更正旧解释——`rpc-frame-ack` 是生产对假体 RPC 帧的传输层入站回执，不是「假体应答晚一拍」。
3. **英文手指标签逐字对齐 UI**：`Credential storage unavailable` / `Credential storage failed` / `Stored credential is invalid or too large`。
4. **未改动项**：暂停 / 忘记 e2e 的 100ms 观察窗保留——它用于证明 gate 失败后不会在观察期内发起连接；`auto_attempt_count` 的同步等待已避免固定毫秒级轮询，该观察窗不参与正确性判定。
5. **修整后复跑**：`cargo test --lib services::zcode_control::wire_e2e` 13/13 PASS（EXIT 0）；`cargo fmt --check` EXIT 0；普通 / 证据 clippy 均 0 warning（EXIT 0）。

## 六、交付边界

- 本报告与统一 spec §10、合同 §8.12、`contract.json` `auth.credentialPersistence`、中英手册 08、CHANGELOG 同批提交。
- 普通构建仍不开放任何 ZCode 控制能力；恢复授权不放宽能力闸（H3-E2E-02 已证）。
- 平台验收完成前，发布说明只能写「自动恢复已实现、首次升级需授权一次」，不得写「全平台重启恢复已验证」。
