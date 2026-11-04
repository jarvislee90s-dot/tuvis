> **2026-10-28 追加**：定点复审的 8 项 P2 已全部修复并通过独立复审，最终代码 HEAD `9cc40249`；详见 [2026-10-28-zcode-p2-fixes-report.md](2026-10-28-zcode-p2-fixes-report.md)。本文件其余 C1–H3 结论保留。

# ZCode 凭据复用与自动恢复连接 · 最终执行报告

日期：2026-10-27～28。分支：`codex/zcode-unified-remote`。最终代码 HEAD：`b589711d`。
范围：C1–C4 可体验核心 + H1–H3 加固。结论：**代码可合并、实机待验**；三平台真实保险箱 / 真实 OAuth / 真实进程重启 / 真实 401 **未执行**，未标 PASS。

## 一、任务与提交

| 任务 | 内容 | 提交 |
| --- | --- | --- |
| 文档 | PRD / 设计 / 执行计划 | `5b5e4358` |
| C1 | 系统凭据存储适配口、记录编码、settings checked read、2,560 上限 | `6525311f` → fix `163161f9` |
| C2 | AuthService 保存/恢复/忘记、AuthStatus、store_lock+epoch、401/过期清理 | `219ff533` → fix `82fe9fb1` |
| C3 | Hub 用户意图、单 worker 自动连接、换链接保留账号、KICKED 暂停 | `5c093ee8` → fix `c544d696` |
| C4 | 启动/远程开关/设置卡/IPC、已记住/未记住/失效状态、忘记授权 | `5935e0d8` → fix `99767d22` |
| H1 | 1s/3s/10s、最多 3 次额外重试；真实掉线新轮、显式连接重开轮、取消 | `989ef61a` → fix `32872949` |
| H2 | 5 个受控交错 + load→forget、存储闸门/并发探针、失败不误删 | `36d3230e` → fix `997c515b` |
| H3 | 4 个真实组件假链 e2e、验收记录、spec/合同/手册/CHANGELOG、全量门禁 | `cb130edd` → fix `328c9395` |
| 终审修整 | restore `restoring` 早退复位、手机 AuthStatus 文档纠正、KICKED 写失败告警 | `b589711d` |

说明：H3 过程中出现过一次 `1b773b86`（同内容提交，未进入最终历史）；最终历史为 `cb130edd` + `328c9395`。

## 二、最终门禁（HEAD `b589711d`）

| 门禁 | 结果 |
| --- | --- |
| `cargo test --lib --features marker-helper` | **3399 passed / 0 failed / 38 ignored** |
| `cargo test --lib --features marker-helper,zcode-bridge-evidence` | **3418 passed / 0 failed / 38 ignored** |
| 全量普通（H3 收口，含集成/示例） | **3912 passed / 0 failed / 46 ignored** |
| 全量证据（H3 收口） | **3931 passed / 0 failed / 46 ignored** |
| `wire_e2e` | **13/13** |
| 集成 4 套件 | `zcode_control_remote` 25、`zcode_control_e2e` 8、`zcode_config_remote` 3、`zcode_read_remote` 5，全 0 failed |
| clippy 普通/证据 `--all-targets ... -D warnings` | 两构建 **0 warning** |
| `cargo fmt --check` | 0 |
| `pnpm test` | **194 files / 2362 passed / 5 skipped** |
| `pnpm check` | 0（format/lint/i18n/error-codes/tsc+build） |
| `pnpm build:mobile` | 0 |

H3 首次 `pnpm test` 与 cargo 全量编译并行时出现 6 个 UI/用量页超时；3 个失败文件空载复跑 133/133，全量空载 2362/5/0；验收文档保留首轮失败，不隐瞒。

## 三、需求落实

- **授权一次、重启复用**：OAuth JWT 写入系统凭据存储；启动只读恢复，不重新 OAuth；JWT 仅系统存储 + 受控 Rust 内存，不进 settings/SQLite/文件/日志/错误/IPC/手机。
- **自动恢复连接**：授权成功、启动、换链接、重新开启远程均唤醒同一单 worker；真实 `paired→offline` 才开新轮；显式连接可重置退避；1s/3s/10s，最多 3 次额外；自身失败/needs_auth/needs_config/conflict/unknown 不重试。
- **用户意图**：主动断开持久 `auto=false` 并暂停；KICKED 持久暂停且重启不互抢；普通退出/停服不写暂停；总开关关闭不连接，重开按意图恢复。
- **换/删链接**：保留账号 JWT，只断旧 relay/作废旧投影与动作；删除链接保留授权，显示需要配置。
- **失效/忘记**：exp/401 只清对应代并删持久记录；offline/5xx/403 不误删；迟到旧 401 不删新记录；忘记清内存+删除记录、保留链接；删除失败如实报。
- **状态展示**：AuthStatus 仅桌面 IPC；手机仍只读既有 ConnectionView，无 AuthStatus 接口；设置卡显示已记住/已授权未连接/本次可用但未记住/需要重新授权/存储不可访问/记录无效。

## 四、计划偏离与已知边界

1. **C3/H1/H3 的 RED 是“突变/破坏桩”而非严格先测后码**：C3 临时禁用 worker、H1 临时回退预算/掉线接线、H3 临时在 restore 顶部加 return；均记录日志并还原。H2 为严格先加闸/测试后 GREEN。验收文档如实标注，不冒充 TDD 首次 RED。
2. **`restore` 采用 async**（brief 裁决）覆盖设计文档的同步写法；C3 为 `BoxFuture` Send 将锁内决策与锁外删除分离，C2 行为测试保持通过。
3. **H3 首轮 pnpm flake** 已保留并空载复跑；非测试弱化。
4. **平台未验**：macOS Keychain 保存/读取/删除/锁定、Windows Credential Manager（2,560 边界/当前用户/删除映射）、Linux Secret Service（DBus 有无/锁定）、真实 OAuth/relay、真实进程重启、真实 401/撤销、双手机并发——全部 `awaiting_platform_validation` / `not_run`，未标 PASS。
5. **残余 Minor（不阻塞）**：`Ok(Offline)` 显式连接路径未单独覆盖；`start_auto_connect` 非 Tokio 上下文会 panic；Windows 读访问拒绝映射为 Unavailable；401 后若删除失败且 authorized=false，UI 无“忘记授权”重试入口；验收报告全量数字仍以 328c9395 为口径（本报告已补 HEAD lib 数字）；CHANGELOG 首行 BOM 变更；`restoring` 第二条早退的并发直接回归缺失（修复已由代码与第一条测试覆盖，5 处 epoch 递增均清 restoring）。
6. **未改** `docs/MASTER-PLAN.md`；未 push/merge/tag/发版；无真实 JWT/链接/正文入库。

## 五、终审

独立终审（34307def..328c9395 + 终审修整 b589711d）：

- Critical：无。
- 原 Important 3 项（restore `restoring` 卡死、手机 AuthStatus 假声明、KICKED 暂停写失败静默）均已修并 scoped 确认。
- Verdict：**代码可合并、实机待验**。

最终代码 HEAD `b589711d`；工作树在提交本报告前干净。真实平台验收仍待用户/专用环境执行。
