# ZCode Keychain／“安全 Cookie”重复确认：来源定位与收敛设计

日期：2026-10-31  
适用分支：`codex/zcode-unified-remote`  
设计状态：**调查设计，NO-GO（弹窗来源尚未实机定位）**  
关联任务：ZCode 手机远控下一轮 O3  
依据：`docs/superpowers/reviews/2026-10-31-zcode-next-round-review-and-change-proposals.md` §7、`docs/superpowers/plans/2026-10-31-zcode-next-round-engineering-plan-v2.md` §9、`docs/superpowers/specs/2026-10-27-zcode-credential-reuse-design.md`

> 本文规定怎样确认弹窗由谁触发，以及在确认后可考虑的安全收敛方向。现在没有真实 Mac 上的弹窗截图、请求方进程、钥匙串条目名或系统日志；所有根因均为待验证假设。**取证之前不得修改 Keychain ACL、迁移凭据、降低系统确认级别或承诺“以后绝不再提示”。**

## 1. 目标与非目标

### 目标

- 确认用户口中的“安全 Cookie”提示究竟是 Keychain 访问、Gatekeeper、浏览器存储、其他进程的凭据访问，还是别的系统对话框。
- 如果证实来自 Tuvis 对现有 ZCode Keychain 条目的访问，找出触发的是 `load`、`save` 还是 `delete`，并记录对应的应用身份、版本、构建方式和安装路径。
- 在保持 macOS 用户授权边界的前提下，评估正式版应用身份稳定化与减少重复存储读取能否降低正常重启和常规升级时的重复提示。
- 将可消除的重复提示与系统安全策略合法要求的再次确认分开验收。

### 非目标

- 不根据“安全 Cookie”这个口语称呼推断弹窗必然来自 ZCode JWT 或 Keychain。
- 不读取浏览器 Cookie、不把 JWT/凭据值写入日志、SQLite、普通文件或 IPC。
- 不设置“允许所有应用访问”、不批量修改 ACL、不关闭 macOS 授权提示、不自动替用户点击允许。
- 不承诺应用重装、签名身份改变、系统更新或管理策略变化后永远不再出现确认。
- 本轮不变更 `credential_store.rs`、Keychain access group、签名流水线或用户设备上的证书/钥匙串状态。

## 2. 名词与数据边界

| 名称                | 当前含义                                                                                 | 是否等于用户所说的“安全 Cookie”                                                       |
| ------------------- | ---------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------- |
| ZCode JWT           | ZCode OAuth 授权后由 Rust `AuthService` 管理的账号凭据；可选持久化在系统凭据库           | **未知**。只有弹窗条目/请求方确认后才能关联                                           |
| `mam_device`        | Tuvis 手机远程配对时由 `/m` 服务端设置的 HTTP Cookie；配对设备身份与有效期由远程服务管理 | **不是** ZCode JWT/Keychain 凭据，参见 `src-tauri/src/remote/gate.rs` 与 `pairing.rs` |
| Gatekeeper 提示     | macOS 对未识别或未签名应用的启动安全提示                                                 | 不是 Keychain 读取授权对话框；必须按系统文案和触发时机分开记录                        |
| 浏览器/其他程序凭据 | 浏览器密码、Cookie 或其他进程自己的 Keychain 条目                                        | 目前没有证据证明与 Tuvis 的 ZCode 凭据存储有关                                        |

## 3. 当前仓库可核实的事实

以下是仓库代码/文档事实，不证明真实机器上出现的弹窗由这些路径触发。

1. **ZCode 凭据使用原生存储**：`src-tauri/src/services/zcode_control/credential_store.rs` 在 macOS/Linux 使用 `keyring`；正式构建的 service 为 `com.jarvis.tuvis.zcode`，debug 构建为 `com.jarvis.tuvis.dev.zcode`，两者使用固定账户 `primary`。存储值是包含 provider 与 JWT 的单条凭据记录；`StoredCredential` 不实现 `Debug`，错误映射为封闭枚举，不保留原生错误文本或秘密值。
2. **授权恢复不是状态轮询**：`AuthService::restore` 在当前代内存 JWT 有效时直接返回；`auth_status` 只读内存。启动接线 `remote::start_zcode_auto_connect_after_restore` 先执行一次授权恢复，再启动同一 Hub 的自动连接 worker；worker复用恢复后的内存状态，避免再读一次原生存储。原生读写通过 `store_lock` 串行化，并在 `spawn_blocking` 中执行。显式连接/重试仍可能主动触发恢复。需要实测具体请求次数，不能从代码推断用户每次启动一定读了一次或弹了一次。
3. **发布身份存在重要嫌疑点**：`src-tauri/tauri.conf.json` 的 bundle identifier 为 `com.jarvis.tuvis`。仓库现有 `docs/RELEASE.md` 写明 macOS DMG 当前未完整 Apple 签名；`.github/workflows/release.yml` 设置的是 Tauri updater 签名，当前文件未配置 Apple Developer ID 证书注入和公证步骤。这提示未签名/不同构建身份值得调查，但**不能单独证明它就是当前 Keychain 提示根因**。
4. **源文件路径勘误**：上游提案引用 `docs/operations/RELEASE.md`；当前仓库实际存在的是 `docs/RELEASE.md`，并不存在前者。后续引用均以实际文件为准。
5. **当前还没有实机归因证据**：未记录真实弹窗的完整文案、请求方 app/process、访问条目名、触发操作、macOS 版本、Tuvis `.app` 路径或签名状态。O3 仍是 `NOT RUN（需实机）`。

## 4. 根因假设与区分方法

| 假设                                           | 可能触发点                                                                      | 能确认/排除它的证据                                                                                                                                                                               |
| ---------------------------------------------- | ------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| H1：Tuvis 读取已保存的 ZCode Keychain 条目     | `AuthService::restore` → `CredentialStore::load` → `keyring::Entry::get_secret` | 系统对话框中的请求方为 Tuvis 进程，条目名对应 `com.jarvis.tuvis(.dev).zcode / primary`；仅在冷恢复阶段出现                                                                                        |
| H2：授权保存或失效清理访问 Keychain            | OAuth 成功后的 `CredentialStore::save`，或过期/401/忘记路径中的 `delete`        | 以授权、失效或忘记操作为触发点；脱敏事件记录显示 `save/delete`；对应条目和请求方能在系统 UI 中确认                                                                                                |
| H3：发布 app 身份/签名变化导致系统重新信任确认 | 未签名、adhoc、签名团队变化、路径/构建渠道变化可能改变系统识别的代码身份        | 比对触发前后的 `codesign` 身份、TeamIdentifier、Designated Requirement、bundle ID 与分发路径；在同一测试账户分别观察同签名升级与身份变化                                                          |
| H4：这其实是 Gatekeeper 提示                   | 首次从 DMG 启动未签名 app                                                       | 文案发生在启动/打开 app 时，显示开发者或下载来源，不对应 Keychain 条目；`spctl` 与签名检查可辅助确认                                                                                              |
| H5：弹窗来自浏览器或另一子进程                 | 浏览器安全存储、官方客户端/其他本地程序访问自己的凭据                           | 系统对话框请求方或进程不是 Tuvis；提示项也与 ZCode service 不符。此时不得修改 Tuvis `credential_store.rs`                                                                                         |
| H6：同一启动阶段存在重复冷恢复                 | 多个调用点并发执行 `restore`，重复触发 `get_secret`                             | 按 `operation + request correlation id + epoch` 记录脱敏事件；若一次启动出现多次 `load`，再核实启动、自动连接与显式重试的调用链。现有代码已有单飞存储锁和启动恢复标记，不能先假定重复读取就是根因 |

## 5. 推荐方案与取舍

### 方案 A：先归因，保持当前存储策略（推荐，当前唯一允许的阶段）

只读采集提示来源、条目名、操作时点、构建身份和脱敏存储事件。发现原因后再决定是否需要改代码、签名发布流程或浏览器侧说明。

- 优点：不会误改凭据访问边界；能区分系统提示、签名身份问题与别的进程。
- 代价：需要用户提供真实 Mac 测试窗口；在取证前不承诺减少提示。

### 方案 B：若证实是正式版身份不稳定，再稳定分发身份

评估为正式 macOS 产物使用一致的 Developer ID Team、bundle identifier、代码签名要求和固定发布路径，并补齐 CI 签名/公证/验证流程。现有 debug/release Keychain service 隔离继续保留。不要改 service/账户名迁移旧条目，除非另有兼容迁移设计和回滚验证。

- 优点：可能让系统在常规升级后继续识别同一可信代码身份。
- 代价：需要 Apple Developer Program、证书/Secrets 管理、公证和真实发布验收；签名本身不保证在每种系统状态下都不再询问。

### 方案 C：仅在证据证明冷启动有重复读取时，合并并发恢复

当前 `restore` 在已有有效内存 JWT 时不读 OS；存储锁会串行化原生调用，启动接线还避免启动恢复与 auto-connect worker 重复恢复。只有脱敏调用计数证明同一次冷启动仍有多个 `load` 时，才为相同 epoch 的 cold restore 增加“共享同一个在途恢复结果”的单飞层。

- 优点：不改变 Keychain ACL，只减少确实重复的 API 调用。
- 代价：需要设计 epoch/取消/新授权覆盖的并发语义；它不能修复代码身份变更导致的授权提示，也不能将系统提示隐藏掉。

### 明确排除的方案

通过打开所有应用访问、批量修改 ACL、私自替换/清理 Keychain 条目、把 JWT 落 SQLite/文件、抓取浏览器 Cookie 或自动接受弹窗来消除提示。这些方案扩宽权限或削弱秘密保护，与项目安全边界冲突。

## 6. 第一阶段：只读实机诊断

使用专用测试 Mac 用户或可销毁的测试环境，先记录、不改 ACL、不删除真实条目。截图需避开秘密值、账号邮箱和其他钥匙串内容，仅保留系统对话框来源、条目名称和按钮选择。

### 每次提示记录字段

- macOS 版本、Tuvis 版本、debug/release、下载来源、`.app` 完整路径、bundle ID。
- 提示出现时间及前一动作：冷启动、auto-connect、显式连接、OAuth 保存、401/过期、忘记授权、更新/重装、打开浏览器等。
- 对话框标题/正文、请求方应用名或进程、Keychain item 名称（只记名称，不打开/导出秘密值）、用户选项与是否允许。
- 同一窗口内脱敏的存储操作事件：`load/save/delete` 类别、单调 request id、凭据 epoch、时间、结果枚举（如 `locked_or_denied`/`unavailable`），不写 JWT、bytes、原生错误全文。
- 签名/身份摘要：TeamIdentifier、BundleIdentifier、CodeDirectory/Designated Requirement、签名有效性和产物 SHA-256。不要打印证书私钥、Secret 内容或 keychain dump。

### 建议只读检查

在用户明确提供的测试 `.app` 路径上运行并保存脱敏输出：

```bash
codesign -dv --verbose=4 "/path/to/Tuvis.app" 2>&1
codesign -dr - "/path/to/Tuvis.app" 2>&1
codesign --verify --deep --strict --verbose=2 "/path/to/Tuvis.app"
spctl -a -t exec -vv "/path/to/Tuvis.app"
```

这些检查用于区分是否签名、签名身份与系统执行评估；它们不应被表述为“证明 Keychain ACL 一定怎样处理”。禁止把 `security` 命令用于批量更改 ACL，也不要读取钥匙串条目的秘密值。

### 最小场景

1. 同一测试账号首次授权，分别记 OAuth 保存和下次冷启动恢复是否出现提示。
2. 退出后同一版本重启三次，观察“首次允许”后是否还重复提示，并确认每次实际 `load` 次数。
3. 在同一测试 Keychain 条目保留的前提下安装同签名版本升级，再做一次重启。
4. 安装位置变化、未签名开发构建与已签名正式构建分开记录；不得拿 debug 的结果替代 release。
5. 拒绝 Keychain 访问、锁定登录钥匙串、换 macOS 登录用户、更新系统等情形分别验证系统错误和 Tuvis 状态提示。
6. 单独观察浏览器配对 `mam_device` Cookie；不要读其值或把它与 ZCode JWT 的 item 混为一谈。

## 7. 条件化实施设计

只有 §6 归因完成且用户确认进入实施阶段后，按触发类型选择最小变更：

| 证据结论                                           | 允许的下一步                                                                             | 不允许的推断                                         |
| -------------------------------------------------- | ---------------------------------------------------------------------------------------- | ---------------------------------------------------- |
| 弹窗不是 Tuvis/Keychain 的访问                     | 追踪对应进程或浏览器的凭据流程；更新用户说明                                             | 修改 ZCode `CredentialStore` 或 Keychain ACL         |
| Tuvis `get_secret` 触发，且确由未签名/变更身份导致 | 另行计划正式版稳定签名身份、原子更新和公证；先对旧条目/迁移行为做测试                    | 仅换 bundle ID/service name 就声称问题已解决         |
| `save_secret` 或 `delete_credential` 触发          | 检查触发动作与错误处理，保留失败状态；在专用测试 Keychain 验证                           | 为了“少弹窗”跳过失败处理或偷偷清理条目               |
| 同一次冷启动重复 `load` 被计数证实                 | 设计同 epoch 并发 restore 合并测试，避免重复访问；授权失效/忘记/新授权都必须让旧请求失效 | 将有效 JWT 永久缓存到磁盘或跨登录用户共享            |
| Gatekeeper 提示来自未签名 DMG                      | 单独处理发布签名/公证和安装说明                                                          | 把 Gatekeeper 与 Keychain 授权弹窗混称为 Cookie 问题 |

任何实现需继续满足：

- `AuthStatus` 不含秘密；Keychain 错误只给封闭类别，不把原生错误文本或 JWT 进入日志。
- Keychain 被拒绝/锁定时不自动打开浏览器、不假称 JWT 失效；保存失败时明确显示“本次可用但未记住”。
- 忘记授权的删除失败不得清除恢复禁用标记或报告成功，避免旧记录重启复活。
- 只在同一 macOS 用户、同一可信身份与系统策略下追求常规重启/升级少提示；应用身份变化、系统更新或管理员策略变化可要求重新确认。

## 8. 验收矩阵与停止门

| 场景                                  | 预期观察                                                      | 证据等级                     |
| ------------------------------------- | ------------------------------------------------------------- | ---------------------------- |
| 首次安装、首次 OAuth 保存             | 记录具体请求方/条目/操作，允许后授权状态正确                  | 实机必测                     |
| 同签名同路径冷启动 ×3                 | 每次 load 次数可解释；系统提示次数记录，不预设为 0            | 实机必测                     |
| 同 Team/DR 的常规版本升级             | 对比身份和提示；若提示，保留系统原因/文案                     | 实机必测                     |
| unsigned/debug 与 signed/release 切换 | 区分构建命名空间和 Gatekeeper/Keychain 提示                   | 实机必测                     |
| Keychain 拒绝/锁定                    | 显示可恢复的拒绝/不可用状态；不自动 OAuth、不泄密             | fake 测试 + 实机确认系统提示 |
| 忘记授权，删除成功/失败               | 成功后不恢复旧凭据；失败如实报错且旧记录不自动复活            | fake 测试 + 实机失败场景     |
| Safari/浏览器/其他进程弹窗            | 归因到确切 requester 和条目；Tuvis 服务不误判 JWT 已授权/失效 | 实机必测                     |

### 停止门

- 没有弹窗截图/请求方/条目名/触发动作：维持 **NO-GO**，只保留诊断清单，不改产品代码和 ACL。
- 确认来自其他进程或浏览器：转其对应的授权流程，不改 ZCode JWT 存储。
- 证实为 Keychain 但签名身份/条目迁移尚不清：先做专门实现计划和测试，不直接更新发布流水线。
- 正式版签名需要新增证书/Secrets/Developer Program 预算或影响既有用户凭据：先交付具体方案与成本，等用户裁决后再实现。
- 验收结果必须分别写“常规同身份升级”“重装”“debug/release 切换”“系统策略变化”；不把一个通过场景推广为“一台电脑永不提示”。

## 9. 尚待取得的信息

1. 弹窗原始文案、显示时间和用户实际按过的按钮是什么？
2. 系统提示的请求方进程名与条目名是否对应 `com.jarvis.tuvis(.dev).zcode / primary`？
3. 发生在 `pnpm tauri dev`、DMG 首次打开、首次授权保存、应用重启还是某个浏览器动作？Tuvis 版本、`.app` 路径和 macOS 版本分别是什么？
4. 是每次启动都出现、每次更新出现，还是只发生过一次？用户是否点过“始终允许”？
5. 是否允许使用专用测试 macOS 用户和测试 ZCode 账号执行上述非破坏性诊断？

## 10. 参考来源

- 仓库凭据实现：`src-tauri/src/services/zcode_control/credential_store.rs`、`auth.rs`、`auto_connect.rs`。
- 仓库发布证据：`docs/RELEASE.md`（当前 DMG 未完整签名）；`.github/workflows/release.yml`（updater minisign 流程）；`src-tauri/tauri.conf.json`（bundle identifier）。
- 仓库手机配对 Cookie：`src-tauri/src/remote/gate.rs`、`pairing.rs`、`server.rs`。
- Apple 关于 Keychain ACL 如何根据授权 app 检查访问并在不可信时提示：[Access Control Lists](https://developer.apple.com/documentation/security/access-control-lists)。
- Apple 对“已信任 app 更新/更改后可能需再次授权”的说明：[If a Mac app you’ve already trusted asks for keychain access](https://support.apple.com/guide/keychain-access/if-a-trusted-app-asks-for-keychain-access-kyca1331/mac)。
- Apple 对 Keychain 访问提示选项的说明：[Allow apps to access your keychain](https://support.apple.com/en-gb/guide/mac-help/kychn002/26/mac/26)。
- Apple 关于 macOS Designated Requirement 与代码身份的说明：[TN3127: Inside Code Signing: Requirements](https://developer.apple.com/documentation/technotes/tn3127-inside-code-signing-requirements)；历史技术说明：[TN2206: macOS Code Signing In Depth](https://developer.apple.com/library/archive/technotes/tn2206/)。
