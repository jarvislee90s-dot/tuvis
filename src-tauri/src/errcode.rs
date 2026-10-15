//! 用户可见错误的结构化码（#123/#150/#157 全量迁移，2026-10-14 定案）。
//!
//! # 问题
//! Rust 侧 `Err(String)` 的中文文案经 Tauri IPC 原样上屏——英文界面用户在任何
//! 出错路径都会看到中文。前端 [`crate`] 的既有先例 `W5_TOOL_DISABLED:<tool>`
//! （services/tool_settings.rs）证明了「消息前缀码 + 前端按码取 i18n 键」可行，
//! 但只有一个域在用。
//!
//! # 约定（唯一规则，前端 `src/lib/invokeError.ts` 与本模块互为镜像）
//! * **形态 A（语义替换，大多数）**：`"EXPORT_NAME_EMPTY: 导出文件名不得为空"`
//!   ——前缀码 + 冒号 + 空格 + 原中文句。前端命中已知码 → 整句换成 i18n 文案
//!   （en 为纯英文语义句；动态数值细节留在原串里供日志与未知码回退，en 展示
//!   不携带——如实申报的取舍）。
//! * **形态 B（机器参数）**：`"SESSION_PID_GONE:12345: 进程 12345 不存在"`
//!   ——码 + 冒号 + 单个机器值（id/pid/路径，**不含空格与冒号**）+ 冒号 + 原句。
//!   前端模板以 `{{param}}` 插值。
//! * **未知码 → 原样显示**（不静默吞）：迁移是逐点的，未迁移处走原文。
//! * **码字符集** `[A-Z][A-Z0-9_]{2,}`（≥3 字符，与门禁/前端解析器一致），命名 `域_原因`；locale 键 = `errors.` +
//!   camelCase(码)（如 `REMOTE_NO_TUNNEL_TOKEN` → `errors.remoteNoTunnelToken`），
//!   zh/en 双键齐备由 `scripts/check-error-codes.mjs`（挂 `pnpm check`）对账：
//!   Rust 使用的码必须有双键；locale 有而 Rust 无 → 孤儿键报错（防拼写漂移）。
//! * **协议结构体的 error 字符串字段**（如 ChannelStatus.error）同样走前缀码——
//!   #123 原案是给 ChannelStatus 加平行 `error_code` 字段，全量迁移定案后统一为
//!   前缀码（避免「539 处前缀 + 1 个结构体特例」两套规则）：前端展示该字段的
//!   位置过一遍 `formatInvokeError` 即可，原文保留在码后供日志与未知码降级。
//! * 既有三码 `W5_*` 与 `PRESET_ACTIVE:<id>` / `PRESET_NATIVE_IN_UNIVERSAL:<id>`
//!   在前端以 legacy 别名兼容（映射到既有 errors.* 键），不强制改写。
//! * **暂不打码的原文锚定族**（打码会打断消费方，登记于此防后人误补）：
//!   question.rs 的 codex 备注段族与「阶段机段名」文案——remote/api.rs
//!   `stage_from_abort` 按段名前缀（如「codex 备注:」）分类回执、mobile
//!   QuestionCard 按文案子串（「备注文本」等）分支；上码须与两消费方同批改。
//!
//! # 为什么码放前缀而不是单独协议字段
//! `Err(String)` 是 Tauri 命令的既有返回形态（539 处），逐处改返回类型是协议
//! 地震；前缀码对 Err(String) 零侵入，前端单点解析。log 与移动端原始展示不受
//! 影响（码前缀可读性损失可接受）。
//!
//! # 迁移实操口径
//! 字面量直写为主：`"CODE: 原句"` / `"CODE:param: 原句"` 直接进
//! `Err(format!(...))`——不设 helper（曾提供 coded()/coded_param()，零采用已删，
//! 避免后人误以为必经 helper）。门禁 `scripts/check-error-codes.mjs` 与本文件
//! 互为镜像，其 EXCLUDE_CODES（TUVIS marker 前缀）与 LEGACY 码表以脚本为单一出处。
//! * 部分码标注**仅日志可见**（如 Q_SUBMIT_MULTI_ONLY 的 BadParameter 文案在
//!   api.rs 映射层被换成 bad_index/tool_readonly 后只进 log::debug）——码仍登记，
//!   前端键备而不用。
//! * **审计行格式变更**（2026-10-15）：审计 `result` 字段自此带码前缀
//!   （`failed:RAPI_DELIVERY_BUSY: …`），外部按纯中文 grep 审计日志的脚本需知悉。
