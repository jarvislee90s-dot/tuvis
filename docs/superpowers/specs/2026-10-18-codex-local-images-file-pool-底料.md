# codex local_images 文件池抽取底料（#69）

> 日期：2026-10-18｜工具版本：codex（rollout JSONL 车道）｜宿主：macOS（实现侧）
> 来源：issue #69 用户实测数据引用｜性质：设计定案 + 实现锚点归档
> 结论一句话：codex 用户图片附件经 rollout `event_msg` 行的 `local_images` 字段
> 进文件池 **User 源**；限 rollout JSONL 车道，条数/字节窗与消息读取同刻度。

## 一、实测数据引用（引自 issue #69，不伪造原始底料）

- `event_msg` 含 local_images 相关行 **2290 行**；非空 184 行；绝对路径 210 个；
- 承载车道：**2290 行中 2289 行在 rollout JSONL**（`~/.codex/sessions/**/rollout-*.jsonl`
  的 `type=="event_msg"` 条目），1 行在 thread_history sqlite——APP 投影车道不接
  （量级 1/2290，投入产出否决；thread_items 表亦无 local_images 列）。
- 现有泛化管线触及不到：`map_codex_lines` 只处理 `response_item`，event_msg 明确
  跳过（有测试钉住），故需独立第二抽取源（照 zcode 先例形态）。

## 二、定案总表

| 维度 | 定案 | 理由 |
|---|---|---|
| 抽取源 | rollout JSONL `event_msg` 行 | 2289/2290 车道占比 |
| 判据层级 | `payload.local_images` 主判据 + 顶层 `local_images` 兜底双探 | 与 event_msg 既有消费先例（`subagents/codex.rs apply_rollout_line` 的 payload 层）同构主判据；顶层兜底为形态漂移容忍，命中任一即收 |
| 条目形态 | 字符串路径 或 对象 `path` 字段，其余跳过 | 双形态容忍 |
| 池 | `FileOrigin::User`（rank=1） | 语义合身（用户带入的图片）；零前端改动（新池需 files.rs/api.ts/FilePanel.tsx 四处同步，无用户可见收益）；artifact 兜底命中已存在条目时跳过不覆盖 |
| 窗口预算 | `read_recent_lines_with_budget(&f, line_budget(limit), byte_budget(limit))` | 与 `read_codex_messages_with` 同刻度（条数窗 / 512KB×⌈limit/200⌉ 封顶 4MB） |
| rollout 定位 | 提取共享 `locate_codex_rollout`（文件名快路径 + 首行 `payload.id` 校验兜底，未排序取首个含 id） | 与既有提取链同口径；同 id 多 rollout 的「提取侧首个 vs monitor 侧 mtime 最新」分叉为 `files.rs` ⚠ 既有登记问题，本源沿用提取侧口径、不扩面修 |
| seq 刻度 | 窗口内行序（单调计数） | codex event_msg 行不在 `pg.messages` 条目集内（被 map_codex_lines 过滤），取行序作单调代理；merger 只消费大小关系，排序语义一致 |
| 路径卫生（函数内前置） | URI 判别（冒号前缀 >1 字符跳过，Windows 盘符单字符不误伤）+ `path.exists()` 不存在跳过 | 与 files.rs:983-987 既有判别同款；不可预览条目无展示价值（照 zcode artifact 解析不到降级跳过先例） |
| 失败语义 | 任何失败（定位不到/读不了）→ 空 Vec | 附件是增强能力，不阻塞详情页（与整管线 `(Vec::new(), false)` 口径一致） |

## 三、证据状态申报（如实）

- 实测数据引自 issue #69 用户实测（本档不重录原始样本，不伪造原始底料）。
- `local_images` 字段层级（payload vs 顶层）以实现夹具双探 + 用户实机样本复核后
  回填本节定案（当前：payload 主 + 顶层兜底，两形态都收）。

## 四、实现锚点

- `src-tauri/src/remote/content.rs`：
  - `locate_codex_rollout(sessions_dir, session_id) -> Result<PathBuf, String>`
    （从 `read_codex_rollout_with` 提取，两 Err 语义原样保留：目录为空 / 未命中）；
  - `read_codex_local_images_refs_with(home, session_id, limit)
    -> Vec<(i64 窗口行序, String 绝对路径, Option<i64> ts 毫秒)>`
    （照 `read_zcode_attachment_refs_with` 先例署名与形态）。
- `src-tauri/src/remote/files.rs`：`extract_file_paths_with_env` 内 zcode 特例块
  之后、`merge_artifact_fallback` 之前插入 codex 臂（调用顺序契约不破）。
- 测试：content.rs 合成 rollout 夹具（payload/顶层双探、URI 跳过、不存在跳过、
  字节窗截断）；files.rs 端到端 User 池断言；`map_codex_lines` event_msg 跳过
  测试原样照跑（零改动回归锁）。

## 五、关联

- 上游裁决：issue #69（文件池附件来源逐工具）；批次 spec
  `docs/superpowers/specs/2026-10-18-tracking-batches-design.md` §3。
- 兄弟档：`2026-10-18-claude-opencode-不入池结论.md`（同批降级落档）。
