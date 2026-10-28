# codex 私有 Skill 目录化 + `.agents` 共享目录只读化 — 设计文档

- 日期：2026-09-09
- 状态：待评审
- 关联：ZCode 接入 spec（2026-09-08）中「跨工具共享目录 `~/.agents/skills` 与每工具独立激活模型冲突，不采用」的同源决策，本次补齐 codex 侧

## 1. 背景与问题

`.agents/skills` 是 Agent Skills 开放标准的**跨工具共享**用户级目录：codex 与 zcode 都读取它（claude 不读）。而 MAM 的 codex 激活目标当前指向该共享目录（`skill_dir_for_tool("codex")` → `~/.agents/skills`，`src-tauri/src/adapter/mod.rs:762`），导致：

1. **精度丢失**：对 codex「启用技能」= 在 `.agents/skills` 建链 → zcode（及任何遵循该标准的未适配工具）同时看到该技能；「仅禁用 codex」做不到。
2. **共享目录被污染**：`.agents/skills` 混入 MAM 管理的链接（本机实测约 40 个，target 指向 `~/.mam/active/codex/*`），与用户手装的真实技能目录（brainstorming、context7、executing-plans、finishing-a-development-branch、frontend-design）混杂。

两种用户情景对 `.agents` 的处置要求相反：

| 情景 | 特征 | 期望 |
|------|------|------|
| A：所有在用工具均被 MAM 覆盖 | `.agents` 无外部消费者 | MAM 链接迁走，共享目录回归纯净 |
| B：存在 MAM 未适配、依赖 `.agents` 的工具 | `.agents` 是外部工具的技能来源 | MAM 不得轻举妄动清理，迁移需脱钩而非删除 |

## 2. 事实基座（实机 + 官方文档，2026-09-09 取证）

| # | 事实 | 证据 |
|---|------|------|
| F1 | codex 读 `~/.agents/skills` | [官方文档](https://developers.openai.com/codex/skills)：用户级 = `$HOME/.agents/skills`（开放标准路径） |
| F2 | **codex 也读 `~/.codex/skills`（私有路径）** | 本机 codex 0.149.1 全局状态留有 `[$ask-matt](C:\Users\bunny\.codex\skills\ask-matt\SKILL.md)` 的真实调用记录；官方捆绑系统技能落在 `~/.codex/skills/.system`（skill-creator / skill-installer / imagegen / openai-docs / plugin-creator / review-agent，9 月 8 日随版本更新生成）；[社区文档](https://blog.fsck.com/2025/12/19/codex-skills/)确认用户技能可与 `.system` 并列放置、[GitHub Discussion #9682](https://github.com/openai/codex/discussions/9682) 确认符号链接受支持 |
| F3 | zcode 双读：`~/.zcode/skills`（优先）+ `~/.agents/skills`，同名 first-wins | ZCode 官方配置指南（本地插件缓存 `zcode-guide/zcode-configuration-guide`） |
| F4 | MAM 后端对 codex skill 目录的硬编码仅一处 | `skill_dir_for_tool`（adapter/mod.rs）；启用写入（services/skill、services/preset）、生效检测（commands/resource）、导入扫描（services/resource `auto_import_extensions`）均派生自该注册表或 `skill_dirs()[0]`；linker 零硬编码 |
| F5 | 启用状态真源 = DB assignments + Layer 2（`~/.mam/active/codex/<name>` → `~/.mam/skills/<name>`），工具侧目录只是投影 | 磁盘实查 + linker 三层模型 |
| F6 | codex adapter 的 `skill_dirs()` fallback 为 `base_dir().join("skills")`，`base_dir()` = `~/.codex` | adapter/codex.rs:27,62 |

**残留风险**：`.codex/skills` 中现有技能均为真目录，「符号链接在该目录被 codex 跟随」目前只有 F2 的社区文档背书 + codex 已在 `.agents/skills` 跟随 MAM 链接的旁证（同一套发现逻辑），需实机验证（§8 V1）。

## 3. 方案选型

- **方案 A（选定）**：codex → `~/.codex/skills`；`.agents` 降级为只读通用导入源；启动时对 MAM 遗留链接弹一次性「迁移 / 保留为共享」对话框。
- 方案 B（否决，留作后续立项）：把 `.agents` 注册为第八个可启用的伪工具目标。tool 注册表贯穿 monitor/MCP/预设/徽标，伪工具需处处特判；当前无实锤的未适配工具需求，YAGNI。
- 方案 C（否决）：目录不动，用 codex `[[skills.config]] enabled=false` / zcode config.json 禁用覆盖做精准化。破坏「链接存在 = 已启用」单一真源，需逆向维护两种配置格式，复杂度最高。

## 4. 设计

### 4.1 变更一：目录注册表单点切换

`skill_dir_for_tool("codex")`：`~/.agents/skills` → **`~/.codex/skills`**。

- 同步更新：该行注释（codex 双路径，MAM 选私有路径，与 zcode 决策对齐）、既有测试 `codex_skill_dir_uses_real_cli_directory` 的断言。
- 派生影响（F4，均无需逐处修改）：
  - **写入路径**（启用/预设/补链）→ 只写 `~/.codex/skills`；
  - **导入扫描** → 扫 `.codex/skills`，**排除 `.system` 子目录**（官方捆绑技能不入 SSOT；实测其结构为两层深度不会被 `*/SKILL.md` 单层扫描匹配，加测试锁死）；
  - **`detect_source_tool`** → `.agents/skills` 来源变为 None = 跳过补链（恰好是新语义：共享目录发现的技能「入库不归属」）；`.codex/skills` 来源 → codex 正常补链。

### 4.2 变更二：`.agents` 保留为通用导入源（只读）

`auto_import_extensions` 的扫描源在七工具 primary dir 之外**显式追加 `~/.agents/skills`**，来源标签 `agents-shared`：

- 手装技能继续自动入库（统一资源仓库定位不变、重新扫描按钮语义不变），但入库后不补链任何工具（`detect_source_tool` → None）。
- 实现检查点：前端对 `ImportStats.source_counts` 的来源徽标渲染需兼容非 tool id 标签（若按 tool id 映射徽标需兜底）。
- **MAM 从此永不写 `~/.agents/skills`**（含删除——唯一例外是 §4.3 迁移对话框对「MAM 自建链接」的处置：搬迁或改指目标，且仅限识别谓词命中的链接）。

### 4.3 变更三：遗留链接识别 + 一次性迁移对话框

**识别谓词**（机器判定）：`~/.agents/skills/*` 中符号链接/交接点，且 target 规范化后位于 `~/.mam/active/codex/` 之下。手装真目录、外部无关链接、断链均不匹配。

**触发**：启动后资源服务初始化完成时异步检测；发现 ≥1 遗留链接 → 前端弹对话框（中英词条，列技能名清单）。检测为后台任务，不阻塞主界面。

**动作一「迁移到 .codex/skills」**（情景 A）：

- 逐个在 `~/.codex/skills/<name>` 重建链接（target 仍指 Layer 2 原路径），删除 `.agents` 侧旧链；
- **DB assignments 与 Layer 2 完全不动**（F5），启停状态无损；
- 冲突处理：`~/.codex/skills/<name>` 已存在（真目录或链接）→ 跳过该条并报告，`.agents` 侧旧链保留，由用户自行处置（本机已知个案：frontend-design 手装真目录同名）。

**动作二「保留为共享」**（情景 B）：

- 逐个把 `.agents` 侧链接 target 从 Layer 2 改指 Layer 1（`~/.mam/skills/<name>`），实现为「删旧链 + 建新链」；
- 从此**脱钩 codex 启停**：codex 侧启停只动 `~/.codex/skills`，未适配工具经 `.agents` 照常可见；
- 防御：Layer 1 目标不存在（损坏态）→ 跳过并报告。

**自熄灭（无需持久化开关）**：迁移后谓词无匹配；保留后 target 前缀为 `~/.mam/skills/`，不匹配 `active/codex/` 谓词——两种结局对话框都不再触发。

### 4.4 情景 B 加强（低成本增强）：SSOT 删除保护

删除 `~/.mam/skills/<name>` 前检查 `~/.agents/skills/<name>` 是否为指向它的直链；有则确认提示「该技能仍被共享目录引用，删除后依赖它的未适配工具将失效」。

### 4.5 过渡期显示一致性

生效检测（commands/resource `scan_skills`）的 `enabled_tools` 以 DB assignments 为主、原生目录检测为补充。MAM 自建链接均有 DB 记录，故切换后**面板显示不失真**；仅「DB 无记录但目录有实体」的手动场景从「算 codex 生效」变为「不算」——这正是精度提升后的正确新语义，不做兼容特判。

## 5. 数据流（迁移后稳态）

```
Layer 1 (SSOT):   ~/.mam/skills/<name>
                      ↑ 链接
Layer 2 (激活):   ~/.mam/active/codex/<name>          ← DB assignments 驱动，迁移不触碰
                      ↑ 链接
codex 私有目录:    ~/.codex/skills/<name>              ← MAM 唯一写入的 codex 侧投影

外部共享目录:      ~/.agents/skills/                   ← MAM 只读（导入源 agents-shared）
  ├─ <手装真目录>                                    ← codex/zcode 直接读取，与 MAM 无关
  └─ <Layer 1 直链>（仅情景 B「保留」产生）           ← 脱钩 codex 启停，服务未适配工具
```

## 6. 错误处理

| 场景 | 行为 |
|------|------|
| `.agents/skills` 不存在 / 不可读 | 检测静默跳过（无遗留 → 无对话框） |
| 迁移中单个链接重建失败 | 该条跳过并计入报告，其余继续；不回滚已成功项 |
| `.codex/skills` 创建失败（权限等） | 同上，报告错误详情 |
| 保留动作 Layer 1 缺失 | 跳过并报告 |
| 对话框被关闭（未选择） | 不做任何变更；下次启动谓词仍命中，再次提示 |

## 7. 测试计划（全部 tempdir fixture，零真实家目录访问）

**Rust 单测**：

1. 注册表：`skill_dir_for_tool("codex")` → `.codex/skills`（改既有断言）；
2. 溯源：`.agents/skills/...` → None；`.codex/skills/...` → codex；
3. 识别谓词四态：遗留 MAM 链接 ✓ / 手装真目录 ✗ / 外部无关链接（target 不在 `~/.mam`）✗ / 断链 ✗；
4. 迁移动作全链路：tempdir 构造 Layer1 + Layer2 + `.agents` 链接 → 执行（迁移函数以注入路径为边界、纯文件系统操作、不触碰 DB）→ 断言 `.codex/skills/<name>` 链接指向 Layer 2、`.agents` 旧链消失；
5. 保留动作：断言 target 改指 Layer 1、`.agents` 侧链接仍存在；
6. 同名冲突：`.codex/skills/<name>` 预置真目录 → 跳过且报告、旧链保留；
7. 导入：`.codex/skills/.system/<skill>/SKILL.md` 不被导入；`.agents/skills/<手装>` 以 `agents-shared` 来源入库且零补链；
8. SSOT 删除保护：`.agents` 存在 Layer 1 直链时删除返回需确认提示。

**前端测试**：迁移对话框渲染（清单、双按钮文案、关闭不动作）、i18n 中英词条存在性。

**回归承诺**：既有六工具 + zcode 行为零变化（改动仅 codex 注册表行、导入源追加、新增独立迁移模块）；既有测试仅两处断言随设计更新（§7.1、§7.2），其余不动。

## 8. 实机验证清单（发布前执行，照 Z4.1 方法论）

- **V1（唯一残留风险）**：`~/.codex/skills/` 放测试技能**符号链接** → codex 会话 `/` 菜单识别；
- V2：迁移后 codex 会话中技能仍可调用；zcode `/` 菜单**不再**出现 codex 专属技能（如 lark-base）；
- V3：`.agents` 手装技能（brainstorming 等）在 codex / zcode 照常可见；
- V4（情景 B 模拟）：选「保留为共享」后，在 MAM 禁用该技能（codex 侧），zcode 侧仍可见（经 `.agents`）。

## 9. 非目标（明确不做）

- 不把 `.agents` 注册为 UI 可启用的伪工具（方案 B，留待真实需求立项）；
- 不改 zcode / claude 等其余工具的目标目录；
- 不做 codex `[[skills.config]]` / zcode 禁用覆盖等配置面精准化（方案 C）；
- 不处理 codex 的 REPO 级 `.agents/skills`（项目内目录，与 MAM 用户级管理正交）。

## 10. 文档同步

- README / README.en：导入源说明（`.agents` 标注为通用共享来源）、codex skill 目录描述；
- IMPLEMENTATION_NOTES：新增条目记录 F1-F6 证据与本决策；
- CHANGELOG：合并到下一版本条目。
