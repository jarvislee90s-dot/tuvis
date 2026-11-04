# Claude Stop 通知落盘位置取证（2026-11-04）

## 背景

`docs/engineering/specs/2026-11-04-session-age-anchor-discipline-design.md` §9.2 曾断言
「claude 的 Pending 数据是现成的」——依据是 `subagents/claude.rs` 已有生产在用的三态机
（`is_running = registered ∧ last ∈ {None, Spawn, Resume}`），其 Stop 信号由
`classify_parent_line` → `message_text` → `task_id_in` 从父转写中提取。

该实现落盘于 2026-10-08（`2026-10-08-mobile-subagent-chips-design.md`）。本次实施 P3 时
经 code review 指出、并由本取证证实：**现行 Claude Code 版本的 `<task-id>` 落盘位置已经变了**。

## 取证方法与结果

扫描本机全部 `~/.claude/projects/**/*.jsonl`，统计含 `<task-id>` 或 `<task-notification>`
的行，按「条目 type + 承载字段路径」归类：

| 条目 type | 承载 `<task-id>` 的字段 | 条数 |
|---|---|---|
| `queue-operation` | `.content` | **42** |
| `attachment` | `.attachment.prompt` | **20** |
| `attachment` | （字符串在 `rendered` 等渲染字段，非结构化内容） | 63 |
| `assistant` | （仅出现在消息文本提及） | 2 |
| `user` | `.message.content` | **2** |
| `api-request-shape` | （无关） | 2 |
| `queue-operation` | （无关） | 1 |

**合计 134 行；其中 `message.content` 仅命中 2 条（约 1.5%）。**

### 真实样本（脱敏）

`queue-operation` 形态：

```json
{
  "type": "queue-operation",
  "operation": "enqueue",
  "timestamp": "2026-10-01T16:52:25.022Z",
  "sessionId": "ae92d97a-...",
  "content": "<task-notification>\n<task-id>b6d2kuyxu</task-id>\n<tool-use-id>call_809b...</tool-use-id>\n<output-file>C:\Users\..."
}
```

`attachment` 形态：

```json
{
  "type": "attachment",
  "attachment": {
    "type": "queued_command",
    "prompt": "<task-notification>\n<task-id>b6d2kuyxu</task-id>\n...",
    "source_uuid": "...",
    "commandMode": "...",
    "timestamp": "..."
  },
  "sessionId": "ae92d97a-...",
  ...
}
```

## 影响

`classify_parent_line`（`src-tauri/src/monitor/subagents/claude.rs`）只读
`v["message"]["content"]`：

- 对 chip 展示：影响小——灰点状态不准，可接受（v1 已申报）；
- 对**会话级状态判定**：这是 P3 要接入的路径，影响严重——Stop 通知读不到 ⇒ 子代理
  永远处于 `Running` ⇒ `has_unsettled` 恒真 ⇒ **主会话被拦绿最长 24 小时**
  （只有僵尸保护窗能兜底）。

## 结论

在 `message_text` 补齐 `queue-operation.content` / `attachment.prompt` 等载体、
并用真实脱敏夹具验证之前，**不得把 claude 子代理状态机接为会话级判定门槛**。

P3 已从 2026-11-04 批次整块撤回（revert `5661f348` + `47d0e17c`），
后续见 issue #200。

## 复现方式

```bash
python - <<'PY'
import json, glob, os, collections
files = glob.glob(os.path.expanduser(r'~/.claude/projects/**/*.jsonl'), recursive=True)
c = collections.Counter()
for p in files:
    for line in open(p, encoding='utf-8', errors='replace'):
        if '<task-id>' not in line: continue
        try: v = json.loads(line)
        except: continue
        fields = []
        def walk(o, path=''):
            if isinstance(o, dict):
                for k, val in o.items(): walk(val, f"{path}.{k}" if path else k)
            elif isinstance(o, list):
                for i, val in enumerate(o): walk(val, f"{path}[{i}]")
            elif isinstance(o, str) and '<task-id>' in o: fields.append(path)
        walk(v)
        c[(v.get('type'), tuple(sorted({f.split('[')[0] for f in fields})))] += 1
for k, n in c.most_common(): print(n, k)
PY
```
