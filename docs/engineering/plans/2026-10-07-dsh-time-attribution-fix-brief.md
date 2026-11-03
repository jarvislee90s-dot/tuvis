# 施工提示词 · 修 dsh 采集器的「时间归属」

> 用法：把本文件整份交给执行 Agent。**开工前必须读完**，尤其是「三、口径决定」与「五、硬约束」——
> 这两节里的每一条都是**用户已拍板**或**本仓既有纪律**，不是建议。

---

## 一、任务一句话

把 `dsh` 来源的 token **按「事件发生时间」入账**，而不是按「采集时刻」入账；
日志已被清理、只能从累计差额补录的部分，**只进全天账、不进任何短窗口**。

仓库：`/Users/jarvis/Documents/MultiAgents-Manager`（Tauri 2 + Rust + React 19；分支从 `main` 开新分支）。

---

## 二、根因（务必先理解，否则一定改错方向）

### 2.1 现状：两条取数路径**都没有"逐事件时间"**

| 路径 | 手里有什么 | 时间从哪来 |
|---|---|---|
| **主路径**：投影缓存 `~/.dsh/storages/session_projcache/sessions/*.json` | 会话的**累计账**（`uncachedInputTokens` / `cacheReadTokens` / `cacheWriteTokens` / `outputTokens` …）。**只有"到现在一共用了多少"，没有"什么时候用的"** | 只能借用**采集时刻** `ctx.now_ms` ✗ |
| **兜底路径**：原始日志 `~/.dsh/sessions/<proj>/<session>/session.v*.jsonl.zstd` | **逐条事件、每条自带 `time` 与 `usage`** ✓ | 但解析只保留了 `facts.last_event_ms`（文件里**最后一条**事件的时间）⇒ 整个文件的增量被**压成一个时间点** ✗ |

**证据（自行核对）**：
- `src-tauri/src/services/usage/collectors/dsh.rs` 的 `scan_raw_logs` 内：`facts.last_event_ms.unwrap_or(ctx.now_ms)`
- `src-tauri/src/services/usage/collect.rs:31` 起：所有 `hour_key` / `day_key` 一律走 `hour_key_of_host`（宿主本地）

⇒ **根因不是算错，而是"信息在源头被丢掉"**：增量算出来后，唯一可用的时间就是"现在"。

### 2.2 三种必然出错的场景

1. **首次见到某会话（bootstrap）**：`usage_cursor` 里没有它 ⇒ 上次累计按 0 ⇒ **本轮增量 = 它的一生** ✗✗
2. **应用停摆**（睡眠 / 退出 / 关着几小时）：期间的一切在下次采集时**一次性**变成"当前小时"的增量 ✗
3. **常规量化误差**：10 分钟采集间隔内的 token 全落到那一个整点小时（影响小，但存在）

### 2.3 实测活标本（可直接复现）

会话 `session-f63fb929-67dd-4ceb-8e31-a82276f9185b`（project `mam-worktree2`）：
- 真实事件跨 **10-06 18:00 → 10-07 17:00（14 个小时）**：451 条 / cache_read **133,746,944**
- MAM 库 `usage_detail` 把它写成 **`2026-10-07T12` 一行**：435 条 / cache_read **127,809,786**

### 2.4 后果：**短窗口偏、方向不定；总量不偏**

- 总量**是对的** ✓：增量相减保证每个 token 只入账一次 ⇒ 实测 7 天 `dsh` 库值 `4,496,088,132` vs
  事件流口径 `4,509,654,230`，**仅差 −0.3%**
- 短窗口**是错的** ✗ 且**方向不定**：12 点虚高 **5.8×**（148.8M vs 25.7M），16 点反而**偏小**（67.5M vs 76.8M）

⇒ 准确表述是**「时间平移」**，不是「膨胀」。

---

## 三、口径决定（**用户已拍板，照做，不要另立**）

1. **主路径改为逐事件**：每条 `assistant/message` 的 `usage` 四桶，按**该事件自己的 `time`**
   落到 `hour_key_of_host(event.time)`。
2. **兜底（日志已被清理的部分）→ 只进全天账**：
   - 只累加进 `day_key` 维度的账（并且**不写任何 `hour_key` 明细**）⇒ **不进任何短窗口** ✓
   - 打 **`backfill` 标记**（可用 `cache_semantics` 或新增列；**新增列必须走 migration 并申报接口变更**），
     让界面/导出能区分「实测」与「补录」。
3. **互斥对账（防双计，硬要求）**：
   ```
   同一轮：D = 投影缓存累计值 − usage_cursor.last_cumulative
           E = 本轮①已入账的事件四桶和
           D − E ≤ 0  ⇒ 什么都不记（日志已覆盖，绝不能重复入账）
           D − E > 0  ⇒ 只作补录，进 ② 的全天账
   ```
4. **历史数据不回填**：只从改动生效之后开始准确；在 PR 正文里写明这一条。
5. **总量护栏**：改完 7 天 `dsh` 总量必须仍在 `4,496,088,132` 的 **±0.5%** 内 ✓（这是"没把账改坏"的判据）。

---

## 四、实施要点（文件级）

| 面 | 要做什么 |
|---|---|
| `collectors/dsh.rs` | ① 新增**逐事件**解析：解多帧 zstd 后逐行认 `assistant/message` 且带 `usage` 的行，按 `event.time` 聚合；② `usage_cursor.byte_offset` **按字节**从上次位置继续（**不要按时间戳推进**——时间戳会重复/回退，按字节才不会重复入账）；③ `last_event_ms` 的单点归属退场；④ 接 `D − E` 对账与 `backfill` |
| `collect.rs` | 增量/落库路径跟着改；**保持** `hour_key_of_host` / `day_key_of_host` 的唯一性 |
| `usage_cursor` | **表结构不用改**（已有 `byte_offset` / `ordinal` / `last_cumulative` / `mtime_ms` / `file_size` / `state_json`） |
| `usage_daily` | 跟随同一套归属重算，**不得**出现日档与小时档分叉 |
| **自省锁** | `collectors/dsh.rs` 生产半边的判据面尺寸会变 ⇒ `src-tauri/src/services/usage/collect.rs` 里
`scan_face_size_is_pinned` 的钉子 `(6917, 149472)` 必须同步为实测新值，并追加**第 14 轮同步**说明（写清归属：哪几处新增/删减）|
| 文档 | 契约 `docs/engineering/plans/2026-10-03-usage-interface-contract.md` 若涉及落库口径/新增列 ⇒ 加**日期化变更申报**；需求说明书与验收清单同步描述 |
| 性能 | 解码量从"今天动过的会话文件"开始（实测今天 18 个文件、秒级）；**全量首扫会慢** ⇒ 允许两段式（先速报、后台补齐纠正），但**不得**让界面等解码 |

---

## 五、硬约束（本仓纪律，违反即打回）

1. **门禁全过，且一律看 `echo $?`（不看被 grep 过的摘要）**：
   - `pnpm lint` / `pnpm format:check` / `pnpm check` / `pnpm test`
   - `cd src-tauri && cargo fmt --check`
   - `cd src-tauri && cargo clippy --all-targets --features marker-helper -- -D warnings`
     （**必须带 `--all-targets`**，否则 lint 不到 `#[cfg(test)]` 里的 doc 注释；
     本机已知会有 1 条 `remote/server.rs::inject_state_with_probe` 死代码假阳性——它只在 `#[cfg(windows)]` 测试里被用，
     这条不算你的问题）
   - `cd src-tauri && cargo test --features marker-helper`（用 `MAM_HOME=$(mktemp -d)` 或
     `scripts/check-real-db-untouched.sh "<cmd>"` 做真机库隔离；**四张用量表必须逐字不变**）
2. **不得修改 `docs/MASTER-PLAN.md`**（项目宪法，需用户在场）。
3. **不得** `git add -A` / `git add .` / `git add -a`；**路径级 add**。
4. **`hour_key_of_host` / `day_key_of_host` 必须继续是唯一入口**：自省锁禁止出现带参键函数
   （`hour_key_of(` / `day_key_of(` 且其后不是 `_host`）。
5. **文档锁**：改 `docs/engineering/specs/2026-10-03-pet-token-usage-dashboard-design.md` 或
   `docs/engineering/acceptance/history/usage-dashboard-acceptance.md` 时，**不得触碰含 `19/99` 的行**，且
   `19/99` 的出现次数必须保持 **spec 3 / ACCEPT 2**
   （由 `tests/pet/usage-partial-coverage-lock.test.ts` 互锁）。
6. **不得**改三条读查询的 `refetchInterval: false` 与前端 `POLL_INTERVAL`（有静态锁）。
7. **不得**新增 IPC 命令、不得用 `let _ =` 吞错、不得在导出层取 DB 锁后再调采集（ABBA）。
8. 新增/修改测试文件放在 `tests/pet/` 与 `src-tauri/src/**` 的既有位置，风格与既有用例一致（中文注释写"为什么"）。

---

## 六、必交的测试（缺一打回）

1. **逐事件归属**：造一条**跨小时**的事件流夹具 ⇒ 断言每个 token 落在**它自己的小时**（这条在当前代码上必红）。
2. **不双计**：`D − E ≤ 0` 分支 ⇒ 断言不产生任何新账。
3. **补录只进全天账**：断言 `backfill` 部分**不写任何 `hour_key` 明细**、只影响 `day_key` 维度。
4. **bootstrap**：首次见到的会话（`last_cumulative` 为空）⇒ 其历史**不得**落进当前小时。
5. **总量护栏**：7 天 `dsh` 总量仍在基线 ±0.5% 内。
6. 受影响的既有用例（自省锁、契约对账、双端 mock 同源）同步更新。

---

## 七、验收基准（reviewer 会拿这些对）

| 项 | 期望 |
|---|---|
| 7 天 `dsh` 总量 | ≈ `4,496,088,132`（±0.5%） |
| 近 5 小时（10-07 12–16 时） | 从当前 ~`300,383,682` 变为 ≈ **`172,987,508`**（事件流口径） |
| 归属正确性 | 随机抽 3 个会话，逐小时值与事件流**逐字一致**（13 点两侧现已逐字一致，可作为对照） |
| 来源可辨 | 每条账能区分「实测」/「补录」 |
| 门禁 | 上述命令全部 exit 0；真机四表逐字不变 |

### 复现命令（reviewer 会用）

```bash
# 事件流口径（逐事件、按事件时间）—— 任取窗口
zstd -dc ~/.dsh/sessions/<proj>/<session>/session.v4.jsonl.zstd \
 | python3 -c "
import sys,json
from collections import defaultdict
from datetime import datetime
h=defaultdict(lambda:[0,0,0,0])
for line in sys.stdin.buffer:
    if b'assistant/message' not in line: continue
    try: e=json.loads(line)
    except: continue
    if e.get('type')!='assistant/message': continue
    u=(e.get('data') or {}).get('usage') or {}
    t=e.get('time')
    if not t: continue
    k=datetime.fromtimestamp(t/1000).strftime('%m-%d %H')
    b=h[k]; b[0]+=1; b[1]+=int(u.get('inputTokens') or 0); b[2]+=int(u.get('cacheReadTokens') or 0); b[3]+=int(u.get('outputTokens') or 0)
for k in sorted(h):
    b=h[k]; print(k, '事件', b[0], 'fresh', b[1], 'cache_read', b[2], 'output', b[3])
"
```

```bash
# MAM 侧逐小时（当前实现）
sqlite3 -header -column ~/.mam/mam.db "SELECT hour_key, SUM(input_fresh) fresh, SUM(cache_read) cread,
 SUM(output) out, SUM(requests) req FROM usage_detail WHERE source_id='dsh' AND day_key='2026-10-07'
 GROUP BY hour_key ORDER BY hour_key;"
```

**事件语义（已实测确认，不要重新发明）**：
`usage.inputTokens` 是**未缓存的新增**，与 `cacheReadTokens` **互斥**，且
`inputTokens + outputTokens + cacheReadTokens == totalTokens` 恒成立（例：`2882+396+15708 = 18986`）。
⇒ 四桶映射为 `input_fresh=inputTokens` / `cache_read=cacheReadTokens` / `output=outputTokens` / `cache_write=0`（dsh 不报）。

---

## 八、交付物与回报格式

- 按**功能块**分组的 commit（不要零散；每个 commit 自带一次门禁通过），路径级 `git add`。
- 回报必须含：
  1. 改了哪些文件、每处**为什么**（对齐本文件的哪一条口径）
  2. 门禁命令 + **退出码**（不是摘要）
  3. 验收基准表里的三个数字（7 天总量 / 近 5 小时 / 抽样会话逐小时对照）
  4. 你**没做**或**做了取舍**的地方（如实列，不许含糊）
