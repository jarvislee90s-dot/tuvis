//! 跨切面两轮增量验收（Task 24 前置门禁）。
//!
//! 断言三件事：
//! ① 首轮**每个源**都真的扫到了东西（`new_records > 0`）——否则"第二轮为 0"是空跑；
//! ② 第二轮**每个源**的 `new_records == 0`（同一份数据，只把 now 往后推 60s）；
//! ③ `usage_cursor` 与 `usage_detail` 的行数**不增长**，四桶与请求数之和**不变**
//!    （后者能抓住"往已存在的行里重复累加"这种行数不变但账目翻倍的情形）。
//!
//! 为什么这三条能抓住整族缺陷（**本任务的变异实测结论**，逐条见 task-23B-report.md）：
//! * **B1**（循环内 `return`）→ **原型红在 ①'**（`parsed_files >= 1` 前提断言：那个 `return`
//!   连 `b.parsed_files += 1` 一起跳过 → `文件型源 codex 首轮必须报出真的读了文件（实际 0）`，
//!   实测 M1）；**「游标丢失」形态红在 ②**（实测 M1b：`第二轮源 codex 仍报 6 条新增`）。
//!   两者合起来才是「B1 整族」的锁——**单看 ② 会误以为 B1 原型也红在 ②**；
//! * **B4**（累计型源不做增量）→ **②** 失败（实测 M2：zcode 忽略游标 → 第二轮报 1 条新增）；
//!   更隐蔽的一种（实测 M3：opencode 不推进会话级累计快照）**②③ 的行数断言全绿**，
//!   只有 ③ 的 `bucket_sum` 抓住（30014 → 33789）——这正是第 ③ 条存在的理由；
//! * **B5**（游标键退化成文件名 stem）→ kimi 两个同名 `wire.jsonl` 互相覆盖 → **②** 失败
//!   （实测 M4：`第二轮源 kimi 仍报 1 条新增`），**并且 I-3 的「两把键必须都在」绝对锁**
//!   （fix round 1 补）无论快速路怎么判都会红。
//!   **注意**：② 的可检测性依赖「两个同名文件字节数不同」——同毫秒 + 同大小时，
//!   `file_unchanged` 的 `(mtime, size)` **已知盲区**会让撞车的游标照样命中、变异存活
//!   （实测 M4 第一版全绿）；详见 `fixture_home` 里 kimi 段落与 I-3 段落。
//!   ③ 的「游标行数不增长」是**跨轮**比较，键撞车时两轮都是 1 行、并不会红
//!   （任务书原文这句注释已被本节修正）。
mod support;

use multi_agents_manager_lib::database;
use multi_agents_manager_lib::database::dao::usage as dao;
use multi_agents_manager_lib::services::usage::collect;
use multi_agents_manager_lib::services::usage::ledger;
use multi_agents_manager_lib::services::usage::model::{
    UsageCollectResult, UsageSettings, UsageSourceId,
};
use multi_agents_manager_lib::services::usage::settings;
use rusqlite::Connection;

const T0: i64 = 1_700_000_000_000; // 2023-11-14T22:13:20Z：本文件唯一的"现在"（run_collection 的 now_ms）

/// fixture 里"数据发生时刻"的**统一锚点**（`T0` 同一天、早 1 小时）。
///
/// **为什么必须统一（独立验证复核 §3.2.2 次要 9）**：`run_collection` 每轮末尾都调
/// `purge_expired(settings.detail_retention_days, now_ms)`，cutoff = **T0 的前一天**。
/// 旧 fixture 里三套时间戳并存（opencode / zcode = 1970 的 1/10/100/1000、kimi = 2026-06、
/// claude / codex / workbuddy = 2026-10）：1970 那批明细**每一轮都被静默删掉**，
/// 于是"第二轮行数不增长"验的其实是"被删光了"，而不是"增量生效"——断言偶然成立。
/// 统一到 T0 同日后，七个源的明细都真的留在库里，本用例才在验增量。
/// （dsh 投影缓存 / 原始日志侧的 ts 取 `ctx.now_ms`（= T0），本就不受 fixture 时间戳影响。）
const T_FIXTURE: i64 = T0 - 3_600_000; // 2023-11-14T21:13:20Z

/// 七个源的**最小真实形态** fixture（与各采集器任务里的单测 fixture 同构，这里是跨源版本）。
/// 只求"每个源都能扫到至少一条记录"，不求覆盖全部字段组合。
///
/// **时间戳纪律**：本函数里出现的每一个时间戳都在 `T_FIXTURE` 同代（见上面的说明），
/// 新增 fixture 时照此办理——不要写 1970、也不要写"未来"（T0 之后）。
///
/// **本函数同时做环境重定向**：`KIMI_CODE_HOME` / `DSH_HOME` 指向本 fixture 根——
/// 这两个采集器的数据根优先读 env（DSH 宿主里 `DSH_HOME` 通常已存在），不重定向就会
/// ① 扫不到 fixture（首轮 `new_records == 0`，用例失败）② 去读开发机的**真实** dsh/kimi 数据。
fn fixture_home(home: &std::path::Path) {
    std::env::set_var("KIMI_CODE_HOME", home.join(".kimi-code"));
    std::env::set_var("DSH_HOME", home.join(".dsh"));

    // ---- claude：一个 assistant 记录（带 usage）----
    let claude = home.join(".claude/projects/-p-A");
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::write(
        claude.join("sess-1.jsonl"),
        format!(
            "{}\n",
            serde_json::json!({"type":"assistant","sessionId":"s1","cwd":"/p/A","uuid":"u1",
                "message":{"id":"m1","model":"claude-sonnet-4",
                    "usage":{"input_tokens":100,"cache_read_input_tokens":800,
                             "cache_creation_input_tokens":50,"output_tokens":20},
                    "content":[]},
                "timestamp":"2023-11-14T09:00:00Z"})
        ),
    )
    .unwrap();

    // ---- codex：rollout（含一条 stub + 一条相邻重放，顺带覆盖 B1 的分支）----
    let codex = home.join(".codex/sessions/2023/11/14");
    std::fs::create_dir_all(&codex).unwrap();
    let codex_lines = [
        serde_json::json!({"timestamp":"2023-11-14T09:00:00Z","type":"session_meta",
            "payload":{"id":"cx1","cwd":"/p/A","originator":"codex-tui","source":{"cli":{"origin":"user"}}}}),
        serde_json::json!({"timestamp":"2023-11-14T09:00:01Z","type":"turn_context",
            "payload":{"model":"gpt-5-codex","timezone":"Asia/Shanghai"}}),
        serde_json::json!({"timestamp":"2023-11-14T09:00:02Z","type":"event_msg",
            "payload":{"type":"task_started","turn_id":"t1"}}),
        serde_json::json!({"timestamp":"2023-11-14T09:00:10Z","type":"event_msg","payload":{"type":"token_count",
            "info":{"total_token_usage":{"total_tokens":999},
                    "last_token_usage":{"input_tokens":0,"cached_input_tokens":0,
                        "cache_write_input_tokens":0,"output_tokens":0,"total_tokens":999}}}}),
        serde_json::json!({"timestamp":"2023-11-14T09:00:11Z","type":"event_msg","payload":{"type":"token_count",
            "info":{"total_token_usage":{"total_tokens":1200},
                    "last_token_usage":{"input_tokens":1000,"cached_input_tokens":800,
                        "cache_write_input_tokens":0,"output_tokens":200,"total_tokens":1200}}}}),
        serde_json::json!({"timestamp":"2023-11-14T09:00:20Z","type":"event_msg",
            "payload":{"type":"task_complete","turn_id":"t1","duration_ms":18000}}),
    ];
    std::fs::write(
        codex.join("rollout-2023-11-14-cx1.jsonl"),
        codex_lines
            .iter()
            .map(|l| format!("{}\n", serde_json::to_string(l).unwrap()))
            .collect::<String>(),
    )
    .unwrap();

    // ---- kimi：**两个**同名 wire.jsonl（顺带覆盖 B5）----
    // **两个文件的字节数必须不同**（本任务变异实测的硬要求）：`file_unchanged` 只比
    // `(mtime_ms, file_size)`，而这两个文件是在**同一毫秒**内顺序写出、旧 fixture 的
    // `output: 10 / 20` 又**恰好同宽** → 键撞车后（B5 变异）那个「张冠李戴」的游标
    // 照样命中快速路、第二轮零读取 → **变异存活、B5 门禁失效**（实测：M4 全绿）。
    // 让两者不同宽（10 vs 20000）后，撞车的游标必然 `file_size` 不符 → 重扫 → 真红。
    for (n, out) in [("session_a", 10i64), ("session_b", 20_000)] {
        let d = home.join(format!(".kimi-code/sessions/wd_p/{n}/agents/main"));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("wire.jsonl"),
            format!(
                "{}\n",
                serde_json::json!({"type":"usage.record","model":"ollama-cloud/glm-5.3-flash",
                    "usageScope":"turn",
                    "usage":{"inputOther":1,"output":out,"inputCacheRead":0,"inputCacheCreation":0},
                    "time":T_FIXTURE})
            ),
        )
        .unwrap();
        std::fs::write(
            home.join(format!(".kimi-code/sessions/wd_p/{n}/state.json")),
            serde_json::json!({"title":"t","agents":{"main":{"type":"main"}}}).to_string(),
        )
        .unwrap();
    }
    std::fs::write(
        home.join(".kimi-code/session_index.jsonl"),
        format!(
            "{}\n{}\n",
            serde_json::json!({"sessionId":"session_a","sessionDir":"sessions/wd_p/session_a","workDir":"/p/A"}),
            serde_json::json!({"sessionId":"session_b","sessionDir":"sessions/wd_p/session_b","workDir":"/p/B"})
        ),
    )
    .unwrap();

    // ---- opencode：V1 + V2 各一条会话 ----
    let odb = home.join(".local/share/opencode/opencode.db");
    std::fs::create_dir_all(odb.parent().unwrap()).unwrap();
    let conn = Connection::open(&odb).unwrap();
    conn.execute_batch(
        "CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, title TEXT, time_updated INTEGER,
             model TEXT, tokens_input INTEGER, tokens_output INTEGER, tokens_reasoning INTEGER,
             tokens_cache_read INTEGER, tokens_cache_write INTEGER);
         CREATE TABLE session_v2 (id TEXT PRIMARY KEY, directory TEXT, title TEXT, time_updated INTEGER,
             model TEXT, tokens_input INTEGER, tokens_output INTEGER, tokens_reasoning INTEGER,
             tokens_cache_read INTEGER, tokens_cache_write INTEGER);
         CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
         CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER, data TEXT);
         CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, data TEXT);",
    )
    .unwrap();
    let model =
        serde_json::json!({"id":"DeepSeek-V4-Flash","providerID":"volcengine-plan"}).to_string();
    // 时间戳统一锚在 T_FIXTURE（旧版是 100/101/1/10 这种 1970 毫秒值 → 明细每轮被 purge 静默删掉）
    conn.execute(
        "INSERT INTO session (id, directory, title, time_updated, model, tokens_input, tokens_output,
             tokens_reasoning, tokens_cache_read, tokens_cache_write)
         VALUES ('ses_v1','/p/A','t',?2,?1,10,5,0,0,0)",
        rusqlite::params![&model, T_FIXTURE],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session_v2 (id, directory, title, time_updated, model, tokens_input, tokens_output,
             tokens_reasoning, tokens_cache_read, tokens_cache_write)
         VALUES ('ses_v2','/p/B','t',?2,?1,1000,100,50,800,0)",
        rusqlite::params![&model, T_FIXTURE + 1_000],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO message (id, session_id, time_created, data) VALUES ('m1','ses_v1',?2,?1)",
        rusqlite::params![
            serde_json::json!({"role":"user","time":{"created":T_FIXTURE}}).to_string(),
            T_FIXTURE
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session_message (id, session_id, type, seq, data) VALUES ('v1','ses_v2','user',1,?1)",
        [serde_json::json!({"time":{"created":T_FIXTURE + 1_000},"text":"hi"}).to_string()],
    )
    .unwrap();
    drop(conn);

    // ---- workbuddy：DB（会话 + cwd）+ 一份 jsonl ----
    let wdb = home.join(".workbuddy/workbuddy.db");
    std::fs::create_dir_all(wdb.parent().unwrap()).unwrap();
    let conn = Connection::open(&wdb).unwrap();
    conn.execute_batch(
        "CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT, title TEXT, custom_title TEXT,
             model TEXT, deleted_at TEXT);",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sessions (id, cwd, title, model) VALUES ('wb1','/p/A','标题','hy3')",
        [],
    )
    .unwrap();
    drop(conn);
    let wproj = home.join(".workbuddy/projects/p-A");
    std::fs::create_dir_all(&wproj).unwrap();
    // **用量必须在 `message.usage`**（fix round 1 的 I-1 顺带抓出的**第二处夹具级假绿**）：
    // 任务书把 `usage` 放在行**顶层**，而 `workbuddy.rs:50` 的 `usage_of` 只认
    // `/message/usage`（真机 61 行全在那里；顶层那份**一列都读不到** → workbuddy
    // **一个明细行都不产**，`bucket_sum` 从来没覆盖过它的四桶，而 `new_records` 仍报 1）。
    // 这正是 D-29 同族的「夹具绕开真机形态 = 假绿」——本任务的 I-1 逐源存活断言把它抓了出来。
    std::fs::write(
        wproj.join("wb1.jsonl"),
        format!(
            "{}\n",
            serde_json::json!({"type":"message","role":"assistant","timestamp":T_FIXTURE,
                "content":[{"type":"output_text","text":"ok"}],
                "providerData":{"model":"deepseek-v4.1-flash","requestModelId":"deepseek-v4.1-flash"},
                "message":{"usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120,
                          "cache_read_input_tokens":80}}})
        ),
    )
    .unwrap();

    // ---- zcode：session + model_usage + turn_usage + tool_usage ----
    // **列名逐字与 `load_*` 的 SQL 对齐**（C2：`tool_usage` 必须有 `started_at`，
    // 否则 `prepare()` 直接失败 → 整个 zcode 源 `Err`、首轮 `new_records` 恒 0）。
    // 时间戳统一锚在 T_FIXTURE（旧版 1000 是 1970 毫秒值 → 明细每轮被 purge 静默删掉）。
    let zdb = home.join(".zcode/cli/db/db.sqlite");
    std::fs::create_dir_all(zdb.parent().unwrap()).unwrap();
    let conn = Connection::open(&zdb).unwrap();
    conn.execute_batch(
        "CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, task_type TEXT, parent_id TEXT,
             time_updated INTEGER);
         CREATE TABLE model_usage (session_id TEXT, turn_id TEXT, trace_id TEXT, query_source TEXT,
             status TEXT, provider_id TEXT, model_id TEXT, variant TEXT,
             input_tokens INTEGER, output_tokens INTEGER, reasoning_tokens INTEGER,
             cache_creation_input_tokens INTEGER, cache_read_input_tokens INTEGER,
             computed_total_tokens INTEGER, provider_total_tokens INTEGER,
             started_at INTEGER, completed_at INTEGER, tool_call_count INTEGER);
         CREATE TABLE turn_usage (session_id TEXT, turn_id TEXT, status TEXT, started_at INTEGER,
             duration_ms INTEGER, input_tokens INTEGER, output_tokens INTEGER, reasoning_tokens INTEGER,
             cache_creation_input_tokens INTEGER, cache_read_input_tokens INTEGER,
             computed_total_tokens INTEGER, tool_call_count INTEGER, cancelled_by_user INTEGER,
             error_type TEXT, PRIMARY KEY (session_id, turn_id));
         CREATE TABLE tool_usage (id TEXT PRIMARY KEY, session_id TEXT, turn_id TEXT, tool_name TEXT,
             status TEXT, duration_ms INTEGER, exit_code INTEGER, cancelled_by_user INTEGER,
             started_at INTEGER NOT NULL);",
    )
    .unwrap();
    // **列名漂移（D-39 之后）**：`load_sessions` 的 SQL 逐字读 `id, directory, time_updated,
    // task_type, parent_id` 五列（`parent_id` 是「本会话是否子代理会话」的唯一判据）——
    // 任务书旧 fixture 的 4 列表格会让 `prepare()` 直接失败 → 整源 `Err`（首轮实测复现）。
    conn.execute(
        "INSERT INTO session (id, directory, task_type, parent_id, time_updated)
         VALUES ('sess_z','/p/A','interactive',NULL,?1)",
        [T_FIXTURE],
    )
    .unwrap();
    // **W-50 的承重前提**：zcode 也必须有**第二个会话**，否则「`parsed_files` = 1 个库文件」
    // 与「逐会话累加」两种口径都得出 1，断言不可伪证。子会话（`parent_id` 非空、无任何
    // turn/tool 行）不影响四桶与 `new_records`——它只让「会话数」这一个数字变成 2。
    conn.execute(
        "INSERT INTO session (id, directory, task_type, parent_id, time_updated)
         VALUES ('sess_z_sub','/p/B','subagent_child','sess_z',?1)",
        [T_FIXTURE],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO model_usage (session_id, turn_id, query_source, status, provider_id, model_id,
             input_tokens, output_tokens, reasoning_tokens, cache_creation_input_tokens,
             cache_read_input_tokens, computed_total_tokens, started_at, completed_at, tool_call_count)
         VALUES ('sess_z','turn_1','main','completed','bigmodel','GLM-5.3',1000,100,0,0,800,1100,?1,?2,0)",
        rusqlite::params![T_FIXTURE, T_FIXTURE + 1_000],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO turn_usage (session_id, turn_id, status, started_at, duration_ms,
             input_tokens, output_tokens, reasoning_tokens, cache_creation_input_tokens,
             cache_read_input_tokens, computed_total_tokens, tool_call_count, cancelled_by_user, error_type)
         VALUES ('sess_z','turn_1','completed',?1,310607,1000,100,0,0,800,1100,1,0,NULL)",
        [T_FIXTURE],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO tool_usage VALUES ('t1','sess_z','turn_1','Bash','completed',293,0,0,?1)",
        [T_FIXTURE],
    )
    .unwrap();
    drop(conn);

    // ---- dsh：投影缓存一份 + **原始日志一份**（原始日志侧走 B4 的事实水位，必须一起验）----
    // 路径 = `<DSH_HOME>/…`（上面已把 DSH_HOME 指到 `home/.dsh`）
    let dsh = home.join(".dsh/storages/session_projcache/sessions");
    std::fs::create_dir_all(&dsh).unwrap();
    std::fs::write(
        dsh.join("dsh1.json"),
        serde_json::json!({"version":7,"record":{"identity":{"formatVersion":3,"createdAt":T_FIXTURE,"cwd":"/p/A"},
            "rows":{"tokenUsage":{"ver":1,"seq":1,"val":{"totals":{"uncachedInputTokens":100,
                        "outputTokens":10,"cacheReadTokens":50,"cacheWriteTokens":0},"last":null}},
                    "sessionStats":{"ver":1,"seq":1,"val":{"turns":3,"steps":9,"toolMs":1000}},
                    "modelSelection":{"ver":1,"seq":1,"val":{"lastUsed":{"provider":"ollama-pro","model":"deepseek-v4.1-flash"}}}}}})
        .to_string(),
    )
    .unwrap();
    let raw_dir = home.join(".dsh/sessions/proj-a/session-raw-1");
    std::fs::create_dir_all(&raw_dir).unwrap();
    // 原始日志的 `time` 只用于 turn 时长差值（`turn/end - turn/start`）与工具间隔，
    // 明细的小时桶取 `ctx.now_ms`（= T0）；这里同样锚到 T_FIXTURE 同代（相对间隔保持不变）。
    let raw_body = [
        r#"{"type":"session","version":3,"id":"raw-1","cwd":"/p/A"}"#.to_string(),
        format!(r#"{{"type":"turn/start","time":{}}}"#, T_FIXTURE + 1_000),
        // **工具对必须带真机形态的配对键**（fix round 1 的 M-1）：`tool/call.data.callId`
        // ↔ `tool/result.data.message.source.callId`（`dsh.rs:781/832`；顶层扁平形态
        // `toolCallId` 真机 0 条、是 D-29/D-36 点名的任务书假绿形态）。
        // 少了它 → 只记次数、**耗时恒 0** → 「只重复累加耗时/工具统计」的形态从 ②③ 同时漏
        // （`counters_sum` 那条断言就不可伪证了）。
        format!(
            r#"{{"type":"tool/call","time":{},"data":{{"name":"Bash","callId":"c1"}}}}"#,
            T_FIXTURE + 1_100
        ),
        format!(
            r#"{{"type":"tool/result","time":{},"data":{{"message":{{"source":{{"callId":"c1"}}}},"error":{{"code":"E1"}}}}}}"#,
            T_FIXTURE + 1_200
        ),
        format!(
            r#"{{"type":"turn/end","time":{},"data":{{"reason":{{"kind":"error","error":{{"code":"X"}}}}}}}}"#,
            T_FIXTURE + 2_000
        ),
    ]
    .join("\n")
        + "\n";
    std::fs::write(
        raw_dir.join("session.v3.jsonl.zstd"),
        zstd::stream::encode_all(raw_body.as_bytes(), 3).unwrap(),
    )
    .unwrap();
}

/// 明细里四桶 + 请求数的总和（行数不变但重复累加时它会变——比行数断言更强）
fn bucket_sum(conn: &Connection) -> i64 {
    dao::query_detail_conn(conn, "0000-00-00T00", "9999-99-99T99")
        .iter()
        .map(|r| {
            r.buckets.input_fresh
                + r.buckets.cache_read
                + r.buckets.cache_write
                + r.buckets.output
                + r.request_total
                + r.requests
        })
        .sum()
}

/// 计数器侧的总和（**M-1，fix round 1**）：`bucket_sum` 不含 `tool_ms` / `tool_stats` / `turns` /
/// 三层报错 / `interrupted` / `turn_ms`，而 dsh 的「迟到回执」轮明确允许 `new_records == 0`
/// （`dsh.rs:595-600`）⇒「**只**重复累加耗时/工具统计」的形态此前从 ②③ **同时漏**。
/// 这里按 `dao::query_counters_conn` 把计数器与逐工具统计（次数 + 耗时）全部并入哨兵。
fn counters_sum(conn: &Connection) -> i64 {
    dao::query_counters_conn(conn, "0000-00-00T00", "9999-99-99T99")
        .iter()
        .map(|r| {
            r.turns
                + r.error_model
                + r.error_turn
                + r.error_tool
                + r.interrupted
                + r.tool_calls
                + r.tool_ms
                + r.turn_ms.iter().sum::<i64>()
                + r.tool_stats
                    .values()
                    .map(|(calls, ms)| calls + ms)
                    .sum::<i64>()
        })
        .sum()
}

/// 七源的预期 id 集合（I-2，fix round 1）：`len() == 7` 挡不住「重复一个源 / 换掉一个源」。
const SOURCE_IDS: [&str; 7] = [
    "claude",
    "codex",
    "kimi",
    "opencode",
    "workbuddy",
    "zcode",
    "dsh",
];

/// 一轮结果的源 id 集合（I-2 用；与 `SOURCE_IDS` 的集合直接比较）。
fn source_id_set(
    sources: &[multi_agents_manager_lib::services::usage::model::UsageSourceStatus],
) -> std::collections::BTreeSet<String> {
    sources
        .iter()
        .map(|s| s.source_id.db_id().to_string())
        .collect()
}

/// **逐源数字留档**（门禁命令带 `--nocapture`）：两轮逐源 `new_records` 本身是本门禁的
/// 一等证据（控制窗口要按它记账）；断言失败时它也是第一定位信息（哪个源在膨胀）。
fn dump_round(tag: &str, r: &UsageCollectResult) {
    println!(
        "[{tag}] collected_at={} duration_ms={} total_new_records={} sources={}",
        r.collected_at,
        r.duration_ms,
        r.total_new_records,
        r.sources.len()
    );
    for s in &r.sources {
        println!(
            "[{tag}]   {:<10} ok={:<5} parsed_files={:<2} new_records={:<4} error={:?}",
            s.source_id.db_id(),
            s.ok,
            s.parsed_files,
            s.new_records,
            s.error_code
        );
    }
}

// ---- 失败时的定位表（任务书 Step 3，逐字留档，便于后来者直接照做）----
//
// | 症状（断言消息） | 最可能的原因 | 先去哪里看 |
// |---|---|---|
// | 某个**文件型源**（claude/codex/kimi/workbuddy）第二轮报新增 | 行循环里用了 `return`
//   而不是 `continue` → 游标没落库 → 整文件重读 | 该采集器的行循环（B1 原型：codex 三处）；
//   跑该源的**单测**看 `delta.cursors.len()` |
// | 某个**累计型源**（opencode / zcode / dsh）第二轮报新增 | 游标的"上次快照/rowid 水位"
//   没读或没写 | 看该源游标的 `state_json` 是否为空、`ordinal` 是否推进（B4） |
// | 源第二轮为 0，但**游标行数增长了** | 游标键每轮都在变（不稳定键，如含绝对路径/时间戳）
//   | `file_key_of` 的入参 root 是否稳定；kimi 是否用了 sessions 根（B5） |
// | `kimi 的游标键缺 wd_p:session_…:wire.jsonl`（I-3） | 游标键退化成文件名 stem / root 传错
//   | `file_key_of(&root.sessions, &wire)` 的入参（B5 的**绝对**锁，不看快速路判定） |
// | `源 X 首轮之后在 usage_detail 里一行都没有`（I-1） | 该源 fixture 的时间戳早于保留期边界
//   → 明细被 `purge_expired` 删光，本用例退化成「验删光」 | 该源 fixture 的时间戳（必须与
//   `T_FIXTURE` 同代）；`cutoff` 值在 panic 消息里 |
// | 明细行数不变但 `bucket_sum` / `counters_sum` 变了 | 往已存在的键里重复累加（"行数不变"
//   骗过了行数断言） | 该源是否漏了增量门（B4）；这是本任务第 ③ 条断言存在的理由 |
// | 某个源首轮 `new_records == 0` | fixture 没被扫到（**不是**被测逻辑的问题）
//   | 该源的路径发现（`KIMI_CODE_HOME`、`~/.zcode` 根、`~/.workbuddy` 编码目录） |
// | 某个**文件型源**首轮 `parsed_files == 0` | 扫描被提前 `return` 截断（B1 的另一种露出）
//   | 该源的 `parsed_files += 1` 与 `push_cursor` 是否都在行循环**之后** |
// | 第二轮 `源 X ok=false` | 该源在第二轮才真正失败（**不是**增量生效；W-23 配对断言的用武之地）
//   | `errorCode` 是哪一码 → 该源的读取/落库路径 |
//
// （实测校正：键撞车（B5）**不会**让"游标行数不增长"变红——那是跨轮比较，两轮都是 1 行；
//   抓它的是第二轮 `new_records == 0` **与** I-3 的绝对键锁。见模块头的变异实测结论。）
#[test]
fn second_round_reports_zero_new_records_for_every_source() {
    // **范式样板（§3.2.2 阻塞 2 指定）**：本用例断言的全是**相对量**——逐源 `new_records`
    // 第二轮是否为 0、两轮之间 `usage_cursor` / `usage_detail` 行数与四桶之和是否**变化**，
    // 而不是"某张表应该有 N 行"。所以它可以跑在 `run_collection` 的**全局库**上
    // （`support::setup()` 的临时库；本文件只有这一条用例，不存在同文件互相污染）。
    // 这与 Task 4/11/13 里那几条**需要绝对行数**的用例（必须走
    // `support::open_ledger_db(tag)` 私有库 + `apply_delta_conn`，Global Constraints 19）形成对照。
    // **后续新增断言时保持本口径**：不要在这里写 `count_rows_conn(...) == N`。
    support::setup();
    // fixture 根 = 采集读源根（collect.rs 的 `TUVIS_USAGE_FIXTURE_ROOT` 缝）同款 temp
    // 目录。**不要**从 HOME 环境变量取：Windows 下 `run_collection` 的扫描根走
    // `dirs::home_dir()`（不认 HOME），两边不同根时 fixture 落 A 处、采集扫 B 处
    // （实测在 Windows 扫进真实 profile：zcode 71681 行、workbuddy 12 文件）
    let home = support::usage_fixture_root().to_path_buf();
    std::env::set_var("TUVIS_USAGE_FIXTURE_ROOT", &home);
    fixture_home(&home);
    // **安全前提（Task 20 同款纪律）**：绝不在真实家目录里跑采集——`support::setup()` 若未生效
    // （或 HOME 被外部覆盖），本断言**当场红**，而不是去扫开发机的真实 dsh/kimi 数据
    // （R-25 补充：Task 20 首跑踩过一次，11.9 s + 脏数据）。
    assert!(
        home.starts_with(std::env::temp_dir()),
        "安全前提：fixture 根必须在本进程 temp 目录下（support::usage_fixture_root()），实际 {}",
        home.display()
    );
    // 两个 env 重定向必须真的指向 fixture 根：删掉 `fixture_home` 里的 `set_var` 就会去读
    // 开发机真实的 `~/.dsh` / `~/.kimi-code`（DSH 宿主里 `DSH_HOME` 常态存在）。
    for (k, sub) in [("KIMI_CODE_HOME", ".kimi-code"), ("DSH_HOME", ".dsh")] {
        let want = home.join(sub);
        assert_eq!(
            std::path::PathBuf::from(std::env::var(k).expect("fixture_home 必须设置该变量")),
            want,
            "{k} 必须指向 fixture 根（否则本用例会去读开发机的真实数据）"
        );
    }

    // ---- 首轮 ----
    let first = collect::run_collection(T0);
    assert_eq!(
        first.sources.len(),
        7,
        "七个源都必须跑（缺源 = 采集器没登记）"
    );
    // **I-2（fix round 1）**：`len() == 7` 挡不住「同一源出现两次 / 换掉一个源仍凑满 7」
    // —— 断言 id **集合**等于契约 §2 的七源（第二轮同款断言见下）。
    assert_eq!(
        source_id_set(&first.sources),
        SOURCE_IDS.iter().map(|s| s.to_string()).collect(),
        "首轮七源 id 必须**恰好**是契约 §2 的这 7 个（重复/替换也算红）"
    );
    for s in &first.sources {
        assert!(
            s.ok,
            "源 {} 首轮失败：{:?}",
            s.source_id.db_id(),
            s.error_code
        );
        assert!(
            s.new_records > 0,
            "源 {} 首轮 new_records = 0 —— fixture 没被扫到，本用例会退化成空跑",
            s.source_id.db_id()
        );
    }
    // **逐源数字留档**（门禁命令带 `--nocapture`）：两轮数字本身是本门禁的一等证据，
    // 失败时也能一眼看出是哪个源在膨胀。
    dump_round("首轮", &first);
    // **W-50（控制窗口裁定）**：`parsed_files` 是**用户可见数字**，跨源口径必须统一为
    // 「本轮读了几个**文件**」——SQLite 型源读的是一个库文件（不是会话行数、不是表行数）。
    // 本 fixture 里 opencode 有 2 个会话、zcode 有 2 个会话（1 父 + 1 子），而两者都必须报
    // **1**：「逐会话 `parsed_files += 1`」的实现会报 2 → 本断言承重。
    for s in &first.sources {
        let sqlite_source = matches!(
            s.source_id,
            UsageSourceId::OpenCode | UsageSourceId::WorkBuddy | UsageSourceId::ZCode
        );
        if sqlite_source {
            assert_eq!(
                s.parsed_files,
                1,
                "源 {} 的 parsed_files 必须是「1 个库文件」（W-50：逐会话累加会让同一个 UI \
                 字段出现两种语义），实际 {}",
                s.source_id.db_id(),
                s.parsed_files
            );
        } else {
            assert!(
                s.parsed_files >= 1,
                "文件型源 {} 首轮必须报出真的读了文件（实际 {}）",
                s.source_id.db_id(),
                s.parsed_files
            );
        }
    }
    // `database::open()` 这个名字在实现里不存在（任务书笔误，D-25 同族）：全局库入口是
    // `database::connection::open()`（`database/mod.rs` 只重导出 DAO,不重导出它）。
    let conn = database::connection::open().unwrap();
    let cursors_1 = dao::count_rows_conn(&conn, "usage_cursor");
    let details_1 = dao::count_rows_conn(&conn, "usage_detail");
    let sum_1 = bucket_sum(&conn);
    let csum_1 = counters_sum(&conn);
    assert!(cursors_1 > 0 && details_1 > 0, "首轮必须真的落了账与游标");
    // 哨兵自身的**非空前提**（M-1 的承重性）：`counters_sum` 若恒为 0，那条「计数器不变」
    // 断言就是重言式——真机形态的工具对（dsh 的 `callId` 配对）+ zcode 的 tool_usage
    // 必须让它非 0。数字一并留档（`--nocapture`）。
    println!(
        "[账本] cursors={cursors_1} details={details_1} bucket_sum={sum_1} counters_sum={csum_1}"
    );
    assert!(
        csum_1 > 0,
        "前提：计数器哨兵必须非空（否则「计数器不得变化」是重言式；先检查 dsh 工具对的 callId 形态）"
    );

    // ---- I-3（fix round 1）：**B5 的绝对锁**（不依赖 `(mtime,size)` 快速路的可检测性）----
    // kimi 的两个会话文件**同名**（`wire.jsonl`）但必须各自成键；游标键 = 相对
    // `~/.kimi-code/sessions` 的路径（`file_key_of(root.sessions, wire)`，B5）。
    // **为什么不能只靠第二轮 `new_records == 0`**：那份可检测性来自「两个同名文件字节数不同」
    // （撞车的游标 `file_size` 不符才会重扫）——同毫秒 + 同大小时会命中 `file_unchanged`
    // 的 `(mtime,size)` 已知盲区、**变异存活**（本任务 M4 第一版实测全绿）。这条断言是**绝对**的：
    // 键退化成 stem（或 root 传错）时两把键立刻缺一 → 无论快速路怎么判都红。
    let kimi_keys: std::collections::BTreeSet<String> =
        dao::load_cursors_conn(&conn, "kimi").into_keys().collect();
    for want in [
        "wd_p:session_a:agents:main:wire.jsonl",
        "wd_p:session_b:agents:main:wire.jsonl",
    ] {
        assert!(
            kimi_keys.contains(want),
            "kimi 的游标键缺 `{want}`（B5：键必须是**相对 sessions 根的路径**，\
             退化成文件名 stem 时两个同名 wire.jsonl 会撞成一把键）；实际键集：{kimi_keys:?}"
        );
    }

    // ---- I-1（fix round 1）：**逐源明细存活**（真能抓「被 purge 静默删光」的断言）----
    // 旧版那条 `day_key >= cutoff` 是**结构上打不响**的：`purge_expired` 在 `run_collection`
    // **末尾**跑、**晚于**明细写入 ⇒ 早于边界的行在测试查询前就已被删除，永远不会出现在
    // `detail_days` 里 ⇒ 「把 fixture 时间戳改回 1970」这种退化**照样全绿**（评审推演已证）。
    // 正确的锁是这条：七源的明细必须**都还活着**（任一源被删光 = fixture 时间戳跨代 = 本用例
    // 退化成「验删光」而不是「验增量」）。变异 M9（claude 的 fixture 时间戳改回 1970）真红。
    let detail = dao::query_detail_conn(&conn, "0000-00-00T00", "9999-99-99T99");
    let survived: std::collections::BTreeSet<String> =
        detail.iter().map(|r| r.source_id.clone()).collect();
    for want in SOURCE_IDS {
        assert!(
            survived.contains(want),
            "源 {want} 首轮之后在 usage_detail 里**一行都没有** —— 它的 fixture 时间戳早于\
             保留期边界，明细被 `run_collection` 末尾的 `purge_expired` 静默删光，\
             本用例就退化成「验删光」而不是「验增量」；存活源：{survived:?}"
        );
    }

    // ---- **fixture 新鲜度哨兵**（不是清理边界本身，I-1 的措辞纠正）----
    // 用**真实保留期口径**（`settings.detail_retention_days`，默认 90 —— `model.rs:324`）算边界，
    // 而**不是**旧版的字面量 1：字面量既与生产不符（假红：将来加一行「10 天前但在 90 天保留期内」
    // 的合法 fixture 会误报），又给人「这条在守清理边界」的错觉。
    // **它的定位**：哨兵只在「purge 没跑/被关掉、而上古行仍留在库里」时才会响（变异 M12 实测）；
    // 真正的承重断言是上面的**逐源存活**。
    let retention = settings::load().detail_retention_days;
    let cutoff = ledger::cutoff_day(retention, T0);
    // 任务书原文是 `dao\n.query_detail_conn(…)`（换行断了 `::`）→ E0423 编译不过，
    // 这里只补回路径分隔符，语义一字未改。
    let detail_days: Vec<String> = detail.iter().map(|r| r.day_key.clone()).collect();
    assert!(
        detail_days.iter().all(|d| d.as_str() >= cutoff.as_str()),
        "存在早于保留期边界（{cutoff}）的明细行 → fixture 时间戳跨时代、且 purge 未生效\
         （正常情况这类行早该被删掉、根本查不到），本用例退化成假绿：{detail_days:?}"
    );

    // ---- 第二轮：同一份数据，只把 now 往后推 60 秒 ----
    let second = collect::run_collection(T0 + 60_000);
    dump_round("次轮", &second);
    // **I-2（fix round 1）**：第二轮**必须**再断一次「七源齐」。旧版只在首轮断 `len == 7`，
    // 而第二轮只有一个 `for` 循环——`second.sources` 为空时循环体**一次都不执行**：
    // ② 全体空转、③ 因无写入必然「不增长」⇒ **整道门绿**（结构性空洞）。
    // 同款 id 集合断言一并加：`len == 7` 挡不住「重复/替换一个源」。
    assert_eq!(
        second.sources.len(),
        7,
        "第二轮也必须七源齐（空/少源时下面的循环会空转，整道门退化成假绿）"
    );
    assert_eq!(
        source_id_set(&second.sources),
        SOURCE_IDS.iter().map(|s| s.to_string()).collect(),
        "第二轮七源 id 必须**恰好**是契约 §2 的这 7 个"
    );
    for s in &second.sources {
        // **W-23 配对断言（本任务硬要求）**：`new_records == 0` 必须与 `ok == true` 配对——
        // D-3 让**落库失败**的源也报 `new_records = 0`（批级提交，前几批可能已落盘），
        // 只看 `new_records` 会把「源失败」读成「增量正确」，那是最危险的一种假绿。
        // **真红证据（M8，fix round 1）**：让 codex 在第二轮返回 `Err` → 本断言先于
        // `new_records` 断言响（那时它 `new_records` 恰为 0，只有配对断言抓得住）。
        assert!(
            s.ok,
            "源 {} 第二轮 ok=false（errorCode={:?}）→ 它的 new_records==0 是「采集/落库失败」\
             而不是「增量生效」，本断言拒绝把它当成绿灯（W-23）",
            s.source_id.db_id(),
            s.error_code
        );
        assert_eq!(
            s.new_records,
            0,
            "第二轮源 {} 仍报 {} 条新增 —— 游标没生效（B1 循环内 return / B4 累计型源无增量 / \
             B5 游标键撞车，见本任务 Step 3 的定位表）",
            s.source_id.db_id(),
            s.new_records
        );
        // **M-3（fix round 1）**：`parsed_files` 的「本轮真读到的**文件**数」这半的可伪证面
        // **只在第二轮**——首轮「有 jsonl 的会话数」与「读到的文件数」恒等（本 fixture 里两者
        // 同为 1；且 W-50 的 `parsed_files += 1` 位于「无 jsonl 会话 continue」**之后**，
        // 再加会话也分不开）。第二轮未变更文件不该被计入 → 文件型源必须是 0；
        // 「逐会话 / 逐候选文件累加」的实现会给 1..N（M13 实测真红——**workbuddy 必须算在
        // 文件型这一档**：它虽然同时有 DB，但该字段的口径同样是「本轮真读到的文件数」）。
        match s.source_id {
            // SQLite 型源每轮都读那**一个**库文件 → 两轮都恒 1
            UsageSourceId::OpenCode | UsageSourceId::ZCode => assert_eq!(
                s.parsed_files,
                1,
                "SQLite 型源 {} 每轮都读同一个库文件 → parsed_files 恒 1（W-50 口径）",
                s.source_id.db_id()
            ),
            // 文件型源（claude / codex / kimi / workbuddy / dsh）：未变更文件不计入 → 必须 0
            _ => assert_eq!(
                s.parsed_files,
                0,
                "文件型源 {} 第二轮不该报 parsed_files（未变更文件不计入「本轮真读到的文件数」，\
                 W-50 口径；报 1..N 说明它改成了逐会话/逐候选累加）",
                s.source_id.db_id()
            ),
        }
    }
    assert_eq!(
        dao::count_rows_conn(&conn, "usage_cursor"),
        cursors_1,
        "游标行数不得增长"
    );
    assert_eq!(
        dao::count_rows_conn(&conn, "usage_detail"),
        details_1,
        "明细行数不得增长"
    );
    assert_eq!(
        bucket_sum(&conn),
        sum_1,
        "四桶与请求数之和不得变化（防「行数不变、值被重复累加」）"
    );
    // **M-1（fix round 1）**：计数器侧（tool_ms / tool_stats / turns / 三层报错 / interrupted /
    // turn_ms）也必须并入哨兵——dsh 的「迟到回执」轮**有意**允许 `new_records == 0`
    // （`dsh.rs:595-600`），于是「只重复累加耗时/工具统计」的形态此前从 ②③ 同时漏。
    assert_eq!(
        counters_sum(&conn),
        csum_1,
        "计数器与工具统计之和不得变化（防「new_records 为 0 但耗时/工具统计被重复累加」）"
    );

    // ---- W-23 第 1 条（登记要求）：**总开关在生产入口侧的接线** ----
    // 为什么必须由本任务的集成构建来锁：Task 10 的登记表是**空表**，`enabled` 改写成恒 true
    // 在 lib 单测里**全绿**（空表结构性假绿）；只有真 `run_collection` + 真采集器登记表才能验。
    // **诚实标注不可断言的那半**：`collect_sources` 的「在读游标**之前**返回」本身没有计数器，
    // 外部观察不到「零游标读」——本段只锁它的**可见后果**（零源、零扫描时长、账本零变化）。
    let off = UsageSettings {
        enabled: false,
        ..UsageSettings::default()
    };
    settings::save(&off).expect("写入关闭态设置必须成功（写后回读一致）");
    let disabled = collect::run_collection(T0 + 120_000);
    assert!(
        disabled.sources.is_empty(),
        "总开关关闭时一个源都不许跑（实际跑了 {} 个）——关闭态必须短路在采集循环之前",
        disabled.sources.len()
    );
    assert_eq!(disabled.total_new_records, 0, "关闭态零新增");
    // **M-2（fix round 1）**：`duration_ms == 0` 只是**辅助信号**（`run_collection` 在关闭态
    // 把它置 0），真正的判据是上面的 `sources.is_empty()` —— 别把这条读成「扫描时长的主锁」。
    assert_eq!(
        disabled.duration_ms, 0,
        "关闭态不做任何扫描（durationMs = 0 哨兵，辅助信号）"
    );
    assert_eq!(
        dao::count_rows_conn(&conn, "usage_cursor"),
        cursors_1,
        "关闭态不得写游标"
    );
    assert_eq!(
        dao::count_rows_conn(&conn, "usage_detail"),
        details_1,
        "关闭态不得写明细"
    );
    // 恢复默认（本文件只有这一条用例，但仍把库状态还原，便于后续在别处复用同一临时库）
    settings::save(&UsageSettings::default()).expect("恢复默认设置必须成功");
    assert!(
        settings::load().enabled,
        "前提：总开关确已恢复（否则后续断言全体假绿）"
    );

    // ---- W-21（登记要求）：集成构建的 `default_rules_override()` 必须是 `None` ----
    // `cfg(not(test))` 的那条分支在 **lib 单测里永远走不到**（`cfg(test)` 已把它编译掉）。
    // `default_rules_override` 本身是私有函数、集成测试拿不到，故这里用**行为代偿**：
    // 往设置里写一条供应商规则 → 新建 `CollectContext` → `provider_rules()` 必须**读到它**。
    // lib 单测构建下该方法返回注入的**空表**，本断言必红（这正是「生产路径已接上」的判据）。
    let mut with_rules = UsageSettings {
        provider_map_rules: r#"{"rules":[{"prefix":"zzz-w21","provider":"p-w21"}]}"#.to_string(),
        ..UsageSettings::default()
    };
    settings::save(&with_rules).expect("写入供应商规则必须成功");
    let no_cursors = std::collections::HashMap::new();
    let ctx = collect::CollectContext::new(&home, T0, &no_cursors);
    let rules = ctx.provider_rules();
    assert!(
        rules.iter().any(|r| r.provider == "p-w21"),
        "集成（生产）构建下 `CollectContext` 的默认供应商规则必须来自 settings::load()；\
         读到 {} 条规则 = `default_rules_override()` 仍返回注入空表（W-21）",
        rules.len()
    );
    with_rules.provider_map_rules.clear();
    settings::save(&with_rules).expect("清空供应商规则必须成功");
}
