# F9 dsh 代际路径缓存与告警去重实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox syntax for tracking.

**Goal:** 减少 dsh 热扫的代际目录枚举，并让未知格式 WARN 只在内容变化时重报。

**Architecture:** `log.rs` 增加按会话目录索引的 2.5 秒 monotonic TTL 路径缓存；`mod.rs` 在现有 stat 预过滤和 `SessionFileScan` `(mtime,size)` 内容缓存之前取路径列表。未知代际告警按 session/文件代际/解析内容指纹记账。

**Tech Stack:** Rust、`Instant`、`Mutex<HashMap<...>>`、现有 SessionFileScan 与 dsh fixtures。

---

## 任务 1：写路径缓存失败测试

**文件：**`src-tauri/src/monitor/dsh/log.rs`。

- [ ] **Step 1：为 path index 写单调 TTL 用例**

建一个临时 session 目录和注入 `Instant` 的 `GenerationPathCache`：首轮只枚举一次；同一时刻/TTL 内第二轮命中；`t+2.499s` 仍命中；`t+2.5s` 重新枚举。测试使用缓存内 `enumeration_count`，不读用户目录。

- [ ] **Step 2：写新增、替换和删除用例**

TTL 内追加 `session.v4.jsonl`，列表暂时保持旧代际；到期刷新后发现 v4。随后删除未压缩 generation、写入同版本 `.jsonl.zstd`，到期后只返回新压缩路径；再删除压缩文件，到期后列表为空。

- [ ] **Step 3：运行确认失败**

运行 `cargo test monitor::dsh::log::tests::generation_path_cache --lib`。预期当前没有路径缓存类型。

- [ ] **Step 4：实现路径 TTL 缓存**

`GenerationPathCache` 缓存 `HashMap<PathBuf, { scanned_at, last_used, entries }>`；`generation_logs_at(dir, now)` 在 `elapsed < 2.5s` 时克隆缓存列表，否则调用现有 `generation_logs(dir)`，计数并替换缓存。禁止查询目录 mtime。老闲置缓存按 last_used 超时清理。

## 任务 2：接入 dsh 会话扫描并核对热扫成本

**文件：**`src-tauri/src/monitor/dsh/mod.rs`。

- [ ] **Step 1：用路径索引替代每轮 raw generation enumeration**

增加进程级 `DSH_GENERATION_PATHS`；生产 `scan_sessions` 改从 path index 取代际路径，再保留原有 `metadata`、24h 预过滤、`SessionFileScan::peek/parse` 与 `retain_existing` 顺序。测试可注入独立 path index。

- [ ] **Step 2：写新代际发现时限测试**

同一个临时 home 连续调用 `scan_sessions_with_caches`：Active 虚拟 2 秒一轮，新 generation 在旧列表过期后不迟于下一次 TTL 扫描入选；Idle 10 秒间隔自然超过 TTL，第一轮即重枚举。计数记录每轮 generation `read_dir` 与预过滤 stat。

- [ ] **Step 3：覆盖 append 与坏文件恢复**

复用 `digest_invalidates_when_log_appends`；另增加坏 zstd 文件首轮不出卡，原路径内容替换为有效且 `(mtime,size)` 改变后重解析并恢复出卡。压缩替换和删除由任务 1 的路径测试覆盖。

## 任务 3：未知格式告警去重

**文件：**`src-tauri/src/monitor/dsh/mod.rs`。

- [ ] **Step 1：写 warning-cache 纯测试**

`WarningKey(session_id, filename_generation)` 首次同 fingerprint 返回 log；相同 fingerprint 第二次不 log；内容 hash/version 改变后返回 log；新 session 或 generation 独立返回 log。

- [ ] **Step 2：在内容解析产物中保存 fingerprint**

`build_digest` 对已经读入的原始文件 bytes 计算稳定的 64-bit in-process hash，并放入 `DshSessionDigest`。未命中 `SessionFileScan` 内容缓存时才重新计算，热扫不重复读/哈希整个文件。

- [ ] **Step 3：接未知版本 WARN 去重器**

用 `Mutex<HashMap<WarningKey, (header_version, content_hash)>>` 限定 WARN 次数；未知版本分支先登记，只有 fingerprint 改变时调用 `log::warn!`。不改出卡、状态与版本门判据。

## 任务 4：验证

- [ ] **Step 1：运行定向测试**

运行 `cargo test monitor::dsh::log::tests --lib` 与 `cargo test monitor::dsh::integration_tests --lib`。

- [ ] **Step 2：运行格式与 lint**

运行 `cargo fmt -- --check`、`cargo clippy --lib -- -D warnings`、`git diff --check`。

- [ ] **Step 3：提交**

只暂存 `src-tauri/src/monitor/dsh/{log.rs,mod.rs}` 和本计划内测试，提交 `perf(dsh): cache generation paths and dedupe warnings`。

## 规格覆盖核对

- 新 generation 发现时间、压缩替换、删除：任务 1、2。
- 不使用目录 mtime、保持 L3 预过滤顺序：任务 1、2。
- 原地 append 与坏文件修复：任务 2。
- 同 session/generation/content 告警去重：任务 3。
- 枚举/stat 成本对照：任务 2。
