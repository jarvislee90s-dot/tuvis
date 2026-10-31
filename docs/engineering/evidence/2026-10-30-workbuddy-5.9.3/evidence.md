# 证据附录

> 调查日期：2026-10-30；基线：Tuvis 0.6.3（main `5a53b9a7`）+ WorkBuddy 5.9.3（macOS arm64）。
> 所有读取均为只读；账本用 `sqlite3 -readonly "file:...?immutable=1"` 打开。调查期间未改任何代码/数据。
> 数值会随 WorkBuddy 会话继续运行而增长；下文的「快照时刻」必须连同数字一起引用。

## 1. 用量真值核算

### 1.1 按天（快照 2026-10-30 09:40 左右）

| 日期 | 主会话 | 子代理 | 合计 | 账本 | 覆盖率 |
| --- | ---: | ---: | ---: | ---: | ---: |
| 10/24 | 132,479 | 0 | 132,479 | 132,479 | 100% |
| 10/26 | 802,036 | 0 | 802,036 | 802,036 | 100% |
| 10/27 | 11,200,574 | 8,967,697 | 20,168,271 | 11,200,574 | 55.5% |
| 10/28 | 17,454,823 | 3,214,727 | 20,669,550 | 17,454,823 | 84.4% |
| 10/29 | 13,071,339 | 0 | 13,071,339 | 13,071,339 | 100% |
| 10/30 | 9,530,672 | 106,526,790 | 116,057,462 | 8,297,202（09:09 快照） | 7.2% |

结论：账本逐日**精确等于主会话**（10/27、10/28、10/29 完全相等），差额 100% 来自子代理。

### 1.2 10/30 明细（快照 09:40）

| 维度 | 主会话 | 子代理 | 合计 |
| --- | ---: | ---: | ---: |
| 请求数 | 92 | 847 | 939 |
| input_tokens | 9,153,544 | 102,268,648 | 111,422,192 |
| cache_read_input_tokens | 8,939,392 | 100,779,264 | 109,718,656 |
| output_tokens | 61,574 | 654,942 | 716,516 |

账本当天（09:09 快照）：requests=84，input_fresh=206,880，cache_read=8,034,944，output=55,378，合计 **8,297,202**。
> 注：§1.2 表的 input/output 列取自不同时刻（主 input/cache 09:36、主 output/req 09:38、子 input 约 09:33、子 output/req 09:39），所以主 input+output ≠ 主合计。单点 09:39 是 116,057,462；09:40 是 116,398,149。实现验收请用单点重算，不要混用本表的两列。
`829 万` 的构成：803 万 cache_read + 21 万 fresh input + 5.5 万 output——主要是上下文重复读，不是生成 token。

### 1.3 全历史（快照 10:55 左右，会话仍在运行）

| 类别 | 文件数 | 字节 | requests | input | output | 合计（input+output） |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 主会话 | 18 | 8.3 MB | 516 | 62,232,076 | 370,567 | 62,602,643 |
| 子代理 | 32 | 25.9 MB | 1,612 | 193,010,610 | 1,327,774 | 194,338,384 |
| 合计 | 50 | 34.2 MB | 2,128 | 255,242,686 | 1,698,341 | **256,941,027** |

账本全历史 WorkBuddy（截至 10/30 09:09 的 `usage_daily` 求和）实际为 **54,248,840**（5,425 万）；若排除 2026-09-03 的 2,343,620 则为 51,905,220（5,190 万，此前写作 5,190 万时未注明排除该项）。它只覆盖主会话的一部分（更早日期账本晚于文件产生，另有时区/首采差异）。

### 1.4 真值核算脚本（实现方直接复用）

```python
import json, os, glob, datetime
root = os.path.expanduser('~/.workbuddy/projects')

def scan(path):
    tin = tout = tcr = n = 0
    for ln in open(path, encoding='utf-8', errors='replace'):
        try:
            v = json.loads(ln)
        except Exception:
            continue
        u = (v.get('message') or {}).get('usage')
        if not isinstance(u, dict):
            continue
        n += 1
        tin += u.get('input_tokens') or 0
        tout += u.get('output_tokens') or 0
        tcr += u.get('cache_read_input_tokens') or 0
    return tin, tout, tcr, n

main = [0, 0, 0, 0]
sub = [0, 0, 0, 0]
for proj in os.listdir(root):
    pdir = os.path.join(root, proj)
    if not os.path.isdir(pdir):
        continue
    for sid in os.listdir(pdir):
        sp = os.path.join(pdir, sid)
        if os.path.isdir(sp):
            for f in glob.glob(os.path.join(sp, 'subagents', '*.jsonl')):
                r = scan(f)
                sub = [a + b for a, b in zip(sub, r)]
        elif sid.endswith('.jsonl'):
            r = scan(sp)
            main = [a + b for a, b in zip(main, r)]
print('main input/output/cache/req =', main, 'total =', main[0] + main[1])
print('sub  input/output/cache/req =', sub, 'total =', sub[0] + sub[1])
print('all  total =', main[0] + main[1] + sub[0] + sub[1])
```

按日汇总时把 `v['timestamp']`（epoch ms）转本地日期再累加即可（与账本 `day_key` 同时区）。

## 2. 子代理文件形态

路径：

```
~/.workbuddy/projects/<mangled-cwd>/<parentSessionId>/subagents/agent-<agentId>.jsonl
```

实测样本（`d0de8a03-.../subagents/agent-9bf48a4f-...jsonl`）：

```json
{"type":"message","role":"user","sessionId":"01a18a39-174c-7065-9d83-be2a4e76a0a0",
 "cwd":"/Users/jarvis/Documents/tuvis-worktree3","timestamp":1793320359773, ...}
{"type":"message","role":"assistant","providerData":{"isSubAgent":true,
 "model":"deepseek-v4.1-flash","messageId":"01a18a391771...","agent":"general-purpose"},
 "sessionId":"01a18a39-174c-7065-9d83-be2a4e76a0a0", ...}
{"type":"function_call","name":"Bash","sessionId":"01a18a39-174c-7065-9d83-be2a4e76a0a0",
 "message":{"usage":{"input_tokens":22125,"output_tokens":162,"total_tokens":22287,
                     "cache_read_input_tokens":20096}},
 "providerData":{"model":"deepseek-v4.1-flash","isSubAgent":true}, ...}
```

要点：

- 文件名 `agent-9bf48a4f-...` ≠ 行内 `sessionId=01a18a39-...`（两者都是 UUID）
- 首条 user 行可能没有 `isSubAgent`，assistant/function_call 行有 `isSubAgent: true`
- 用量在 `message.usage`（与主会话一致），可挂在 `function_call` 行
- 父目录名 = 父会话 id（如 `d0de8a03-21a4-4782-a24f-f923a3dcaff5`）
- 全量扫描：父 jsonl 中 `isSubAgent=true` 的行数 = **0**（无双计）
- 子代理文件 32 个 / 25.9 MB / 1,612 次 usage；会话目录首次出现于 2026-10-27 23:02（5.9.3 于 10/27 17:25 安装）
- **主会话没有实质漏采**：131/176 条 `function_call` 带 usage；45 条不带 usage 的与同 stepSeq 的另一条共享 `messageId`（同一请求的并行工具调用）。全 18 个主文件 2,298 个 messageId 中仅 1 个（`37d6668a` step4，status=incomplete）完全无 usage。
- **父 descriptor 不能入账**：父 jsonl 的 `providerData.toolResult.subAgent.descriptor.usage` 30/34 存在，29 与子文件逐桶精确相等、1 个滞后、4 个缺失；只能做交叉核对，叠加会双计。

## 3. 心跳证据

### 3.1 文件与字段

`~/.workbuddy/sessions/35950.json`：

```json
{"pid":35950,"lastHeartbeat":1793317102142,
 "sessionId":"5d229cd1-c3ff-46a6-873d-4da4c4f07f8b",
 "cwd":"/Users/jarvis/Documents/tuvis-worktree2",
 "startedAt":1793317102138,"kind":"interactive","updatedAt":1793317102724}
```

`~/.workbuddy/sessions/36335.json`：`lastHeartbeat=1793317165621`，`startedAt=1793317165603`，cwd=`tuvis-worktree3`。

- 两个 `lastHeartbeat` 分别停在 07:38:22 / 07:39:25；与 `startedAt` 只差 4ms / 18ms
- 对应 jsonl 一直写到 09:34（会话仍在运行）
- `kind=interactive`、sessionId 严格 UUID、pid 与文件名一致、pid 存活——除「新鲜度」外全部满足 Tuvis 的过滤条件

### 3.2 打包代码证据

文件：`/Applications/WorkBuddy.app/Contents/Resources/app.asar.unpacked/node_modules/@genie/agent-harness/cli/dist-server/shared.cjs`

- `register()`：写入一次 `{pid, lastHeartbeat: Date.now(), ...}`，`registered=true`
- `update({lastHeartbeat: Date.now()})`：仅被 `startHeartbeat()` 的 30s interval 调用
- `startHeartbeat()`：**有调用点**——`WorkerRegistryLifecycle.waitForEndpointAndUpdate()` 在 `endpointProvider.get()` 于 30s 内就绪后执行 `update(...)` + `startHeartbeat()`；`.catch(()=>{})` 静默吞掉超时/失败。本机 `lastHeartbeat` 冻结说明该分支未走到（endpoint 未就绪），不是“没有调用点”。
- `unregister()`：**有调用点**——`WorkerRegistryLifecycle.onStop()` 执行 `stopHeartbeat()` + `unregister()`，会 `unlink` 心跳文件；已完成会话的心跳文件消失与此一致。因此“文件存在 + pid 存活”可以作为“会话运行时在场”的候选信号，但本设计仍以 DB/jsonl 活性做最终判活。

### 3.3 WorkBuddy DB 活性

`~/.workbuddy/workbuddy.db`（本机 17 行 sessions）：

| session | status | last_activity_at | updated_at |
| --- | --- | --- | --- |
| d0de8a03-... | working | 10:52:36 | 10:55:19 |
| 5d229cd1-... | working | 10:40:23 | 10:54:57 |
| 0247e27d-... | completed | 07:33:17 | 07:33:24 |

- `status` 实测值只有 `working` / `completed`
- **`last_activity_at` 不是活信号**：12:00 实测 `5d229cd1 last_activity=10:40:23`、`d0de8a03=10:52:36`（与心跳文件 `updatedAt` 同刻），而 `updated_at=11:57:26/11:57:45`；`COALESCE(last_activity_at, updated_at) >= now-300s` 返回 **0 行**，`MAX(...)` 返回 **2 行**。活性判据必须用 `MAX(last_activity_at, updated_at)`。
- **9 分钟采样（13:13:57–13:22:58，54 样本）**：81/133/143/92/51s 的父 jsonl 空档里 `updated_at` 均冻结（143s 空档：13:15:29 → 父 13:17:44 写入后才到 13:17:48），`last_activity_at` 全程冻结。**`updated_at` 不是独立心跳，只随父会话写入刷新；纯 300s 规则会在子代理运行期掉卡。**
- **历史 10 分钟空档**：5d229cd1 父 jsonl 08:05:49–08:15:53 只有 `credit-usage` 行，同期子代理 `agent-6e1ce7ab` 写入 95 行；按 300s 规则会在 08:10:49 掉卡。故看板主判据必须是「心跳文件存在 + pid 存活」，300s 只用于 completed / DB-only 候选。
- `MAX(last_activity_at, updated_at)` 查询 20 次合计 0.03ms（17 行）
- `deleted_at = -1`：打包 `app.asar` 字符串检索命中 15 处 `deleted_at = -1`，确认 cloud 形态以 **-1 表示未删**；B2 的 SQL 用 `deleted_at IS NULL OR deleted_at <= 0`。
- `read_dir` 对“文件冒充目录”在 macOS 返回 `ErrorKind::NotADirectory`（`raw_os_error=20`，rustc 实测）；A5 的目录错误分支可据此断言“跳过而非整源失败”。
- `deleted_at IS NULL` 过滤有效；`sessions` 表无本地活跃时间索引，但行数极小

## 4. TPS 交叉验证

- 来源：`~/.workbuddy/logs/2026-10-30/*.log` 的 `qua.end` 行；含 `request_type: subagent:general-purpose`
- `tps_observation` 当天（至 09:29）：`workbuddy` 651 条，output 558,131，平均 **196.0 tok/s**（生成窗合计 2,848 秒）
- 与用户的「200 tok/s」吻合；与 jsonl output 716,516（09:40）同量级
- **覆盖不是 100%**：09:29 时 648/939 请求（69%），11:31 时 828/1,691（49%）。原因：① 24h 保留清理（10/29 的 42 条 `qua.end` 已超 24h 被删，不是解析 bug）；② 日志轮转/采集滞后（当前 `.log` 尾部仍有 180–390KB 未读，秒级会补）；③ 轮转覆盖 `.old.log` 时只看 path+size，若新文件更大可能跳过开头。单条数值与 jsonl 精确一致（387 个共同 messageId 中 382 个完全相等，5 个 `qua=0` vs jsonl>0）。
- 结论修正：**TPS 是“单条准、覆盖不完整”**；不能当 token 真值，也不作为本轮两个核心修复的依赖。
- 结论：TPS 链路本来就把子代理请求算进去了；Token 账本没有

## 5. 国际版（P1，本轮不做）

| 应用 | 版本 | product.json endpoint | 数据目录 |
| --- | --- | --- | --- |
| `/Applications/WorkBuddy.app`（中国版） | 5.9.3 | `https://www.workbuddy.cn` | `~/.workbuddy` |
| `/Applications/WorkBuddy AI.app`（国际版） | 5.9.3 | `https://www.workbuddy.ai` | `~/.workbuddy-ai` |

国际版实测（调查时）：

- `~/.workbuddy-ai/workbuddy.db`：2 个会话（`96d3fc7f`、`01c6c63b`，最后活动 10/30 00:28）
- projects：主会话 203 次请求 / input 35,598,288 / output 101,586；子代理 602 次请求 / input 64,775,752 / output 331,035；合计约 **1.008 亿 token**
- logs：170 条 `qua.end`
- 以上数据在 Tuvis 的用量、TPS、看板中**全部不存在**：代码里所有路径硬编码 `.workbuddy`，无 `.workbuddy-ai` 引用
- 未来若做：适配器/TPS/monitor 支持多数据根（实例化配置），并复用本轮的子代理与心跳兜底设计；本轮明确排除

## 6. 代码锚点

| 位置 | 内容 |
| --- | --- |
| `src-tauri/src/services/usage/collectors/workbuddy.rs:36` | `collect` 入口 |
| `src-tauri/src/services/usage/collectors/workbuddy.rs:283` | `collect_with_home` |
| `src-tauri/src/services/usage/collectors/workbuddy.rs:287` | `~/.workbuddy/workbuddy.db` |
| `src-tauri/src/services/usage/collectors/workbuddy.rs:309-311` | 只枚举 DB 会话 + 主 jsonl |
| `src-tauri/src/services/usage/collectors/workbuddy.rs:430` | `b.session(...)` 父会话登记 |
| `src-tauri/src/monitor/workbuddy_parser.rs:41` | `HEARTBEAT_FRESH_MS=90_000` |
| `src-tauri/src/monitor/workbuddy_parser.rs:91` | `is_strict_uuid_form`（可复用） |
| `src-tauri/src/monitor/workbuddy_parser.rs:170` | `find_session_jsonl`（只找主 jsonl） |
| `src-tauri/src/monitor/workbuddy_parser.rs:314` | `discover_workbuddy_processes_with` |
| `src-tauri/src/monitor/workbuddy_parser.rs:382` | `get_workbuddy_sessions` |
| `src-tauri/src/monitor/workbuddy_parser.rs:402` | 心跳/UUID/kind/pid 过滤 |
| `src-tauri/src/monitor/workbuddy_parser.rs:513` | `compensate_vanished_heartbeats_in` |
| `src-tauri/src/services/usage/query.rs:1006` / `:1037` | D17：turn/会话数排除子代理，token 仍计 |
| `src-tauri/src/database/dao/usage.rs:413` / `:432` | `usage_session` upsert / `MAX(is_subagent)` |
| `src-tauri/src/adapter/mod.rs:628` / `:2179` | L1 零进程门禁 / 契约测试 |
| `src-tauri/src/monitor/host.rs:8` / `:47` | 宿主 exe 判定 / `tool_host_alive_in` |
| `src-tauri/src/monitor/app_status.rs:37` | `APP_STATUS_STALE_MS=300_000` |
| `src-tauri/src/services/tps/sources/workbuddy.rs:162` / `:468` | TPS 日志根 / `qua.end` 解析 |
| `src-tauri/src/commands/session.rs:71` | `focus_session`（App 跳转链路） |
| `src/lib/api/usage.ts:27` | `usageCollect` 唯一扫描触发 |
| `/Applications/WorkBuddy.app/.../@genie/agent-harness/cli/dist-server/shared.cjs` | `register()` / `startHeartbeat()` / `unregister()` 心跳实现（外部包，非仓库文件） |

## 7. 调查命令备忘

```bash
# 账本（只读）
sqlite3 -readonly "file:$HOME/.tuvis/tuvis.db?immutable=1" \
  "SELECT day_key, source_id, SUM(input_fresh+cache_read+cache_write+output) \
   FROM usage_daily GROUP BY day_key, source_id ORDER BY day_key DESC;"

# WorkBuddy 会话/状态
python3 - <<'PY'
import sqlite3, os
db=os.path.expanduser('~/.workbuddy/workbuddy.db')
con=sqlite3.connect(f'file:{db}?immutable=1', uri=True)
for r in con.execute("SELECT id,status,last_activity_at,updated_at FROM sessions ORDER BY updated_at DESC LIMIT 10"):
    print(r)
PY

# 心跳
cat ~/.workbuddy/sessions/*.json

# 子代理文件清单
find ~/.workbuddy/projects -path '*/subagents/*.jsonl' -exec ls -la {} \;

# TPS 当天
sqlite3 -readonly "file:$HOME/.tuvis/tuvis.db?immutable=1" \
  "SELECT COUNT(*), SUM(output_tokens) FROM tps_observation WHERE source='workbuddy' \
   AND ended_at_ms >= strftime('%s','2026-10-30 00:00:00','localtime')*1000;"
```

## 8. 调查边界与未验证项

- 未验证 WorkBuddy 5.8.x 是否刷新心跳（磁盘上只有 5.9.3）；5.9.3 的「写一次」由文件时间戳与打包代码双重证据支持
- 未验证会话关闭时 `unregister()` 是否被调用（打包代码中未找到会话登记器的调用点）；因此活性模型不依赖文件存在性
- 未观察国际版升级/运行对 Tuvis 的影响；本轮只做数据目录盘点
- 调查期间 WorkBuddy 会话持续运行，数字会增长；实现方验证时请在**同一时刻**重新核算真值与账本
- 所有账本读取均为 `immutable=1`；未执行任何写操作
