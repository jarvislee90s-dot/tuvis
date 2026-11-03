# WorkBuddy Windows qua 路线 zh-CN 区域行前缀取证（2026-11-02）

## 结论速览

- Windows 桌面端 WorkBuddy 的 **qua 路线日志带完整数值**（`completion_tokens` / `total_cost_ms` / `ttft_ms`，
  整数字符串形态，v2 数字归一已兼容），`2026-10-24-model-speed-supplement-windows.md` §7.5
  「Windows 正常路线无数值 usage」的定论**对 sdk/conversations 路线仍然成立，但未覆盖 qua 路线**——
  qua 路线在 Windows 上同样产出且数值齐全，当时未被探测到。
- 真正的拦点：qua 日志行前缀时刻的**区域形态**。zh-CN 区域 Windows 写 `[YYYY/M/D H:MM:SS.mmm]`
  （如 `[2026/11/2 15:14:12.917]`），读取器 `parse_bracketed_local_prefix` 只认
  `[M/D/YYYY, h:mm:ss AM|PM.mmm]`（en-US，探针机器形态）与 RFC3339 → 全部 qua.end 在
  「行无可解析时刻前缀 → 跳过（不造时刻）」闸丢弃，`tps_observation` 中 workbuddy 0 条。
- 影响面仅 workbuddy 一源：其余 6 源（codex/zcode/kimi/dsh/opencode/claude）速度时间全部来自
  结构化数据（SQLite 整数列 / JSON 数值字段 / JSON 内 RFC3339 串），无区域设置暴露面。

## 环境

- Tuvis v0.6.4（tag 含 PR #189），Windows 10.0.26200 x64，系统区域 zh-CN
- WorkBuddy 日志根 `~/.workbuddy/logs/<日期>/`，2026-11-02 当日 qua.end 257 条

## 现象

`~/.tuvis/tuvis.db`（只读查询）：

```sql
select source, count(*) from tps_observation group by source;
-- codex 409 / opencode 181 / zcode 345 /（无 workbuddy 行）
```

速度页 WorkBuddy 无任何组合条目；「首Token平均等待」「工具累计用时」为 —（前者尚有
ZCode 原生首点可补、后者为全源 tool_ms 恒空的全局现状，均与本缺口正交）。

## 取证

### 1. qua.end 行形态（zh-CN 区域）

```
[2026/11/2 15:14:12.917] [Info] [pid=30436] [ModelProvider]  [SseMonitor#01a19b1b] qua.end: {"conversation_id":"c0bb5964-…","conversation_request_id":"01a19b1b800475aab1c452c34d30f7db","message_id":"01a19b1b88517505b26a0fde09620cad","model":"minimax-m3.1-flash","finish_flag":"done","finish_reason":"tool_calls","model_error":"","completion_tokens":"1188","total_cost_ms":"17541","ttft_ms":"7500","request_type":"chat",…}
```

- 当日 257/257 条 qua.end 前缀形态唯一：`[#/#/# #:#:#.#]`（YYYY/M/D、24 小时制、
  无 AM/PM、无逗号分隔、毫秒必有）。
- 逐闸核对（对照 `tps/sources/workbuddy.rs` 既有准入）：身份四元组 ✓、
  `finish_reason=tool_calls` 成功收尾 ✓、数值归一（整数字符串）✓——**唯一拦点是行前缀时刻**。
- 载荷含 51 个键（`sse_connect_cost_ms`/`text_first_cost_ms`/`interval_*` 分桶等），
  数值字段全部为整数字符串，与 Mac 5.8.3 形态一致（supplement-macos §9 归一已覆盖）。

### 2. 读取器已消费到 EOF（行被吃、闸上全跳）

`tps_state` 表 `cursor:workbuddy`：`format_version:2`，qua 日志文件
（如 `2026-10-30/DM_dataset__*.log`）`offset` 已推进到文件末尾（7,886,434 等）——
即读取器持续读到文件尾，所有 qua.end 在时刻闸被丢弃，游标照常越过。

### 3. 第三方 key 不影响

qua.end 载荷中 `model":"custom-local:glm-5.3-flash"`（第三方自定义供应商）与
`minimax-m3.1-flash` 数值字段同样齐全。「配第三方 key 拉不到速度」与渠道无关，
速度链路不区分渠道；用量侧该身份 provider 为空串是另一件事（渠道需手动指派）。

## 与既有证据的关系

- `2026-10-24-model-speed-supplement-macos.md` §4.4（Mac qua.end 字段清单）不变，本证据补其 Windows 侧对应物。
- `2026-10-24-model-speed-supplement-windows.md` §7.5 对 **sdk 路线**的结论
  （`usage_update` 只有 `hasUsage=true`）经本次复测仍然成立；其「Windows 整体无数值」的
  外推被本证据推翻：qua 路线 Windows 桌面端存在且数值齐全。
- 行前缀形态随环境（系统区域）变化：en-US 形态能被现行解析器处理，zh-CN 形态不能。
  后续遇到未证形态按「跳过不猜」纪律处理，凭新证据再扩。

## 复现命令

```bash
# qua.end 前缀形态统计
grep -h "qua.end" ~/.workbuddy/logs/2026-11-02/*.log | cut -c1-25 | sed 's/[0-9]/#/g' | sort | uniq -c

# 速度观察计数
py -3 -c "import sqlite3;con=sqlite3.connect(r'file:C:/Users/bunny/.tuvis/tuvis.db?mode=ro',uri=True);print(list(con.execute('select source,count(*) from tps_observation group by source')))"
```

## 后续

修复 = 读取器前缀解析兼容 `YYYY/M/D H:MM:SS.mmm` + 游标 v3 一次性有界恢复（24h 窗内
qua 日志退书签重读，稳定键 UPSERT 幂等）；同 PR 修正 `SOURCE_LIMITS` 过时声明。
见分支 `fix/workbuddy-qua-locale-timestamp`。
