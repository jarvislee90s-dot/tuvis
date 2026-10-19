use rusqlite::Connection;

/// 初始化数据库 schema（所有 CREATE TABLE 语句）
pub fn init(conn: &Connection) {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS session_status_cache (
            session_id    TEXT PRIMARY KEY,
            agent_type    TEXT NOT NULL,
            status        TEXT NOT NULL,
            last_seen     TEXT NOT NULL,
            previous_status TEXT
        );
        CREATE TABLE IF NOT EXISTS settings (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS extensions (
            id          TEXT PRIMARY KEY,
            kind        TEXT NOT NULL,
            name        TEXT NOT NULL,
            description TEXT,
            source_path TEXT NOT NULL,
            source_url  TEXT,
            version     TEXT,
            tags        TEXT,
            suite       TEXT,
            source_tool TEXT,
            is_native   INTEGER NOT NULL DEFAULT 0,
            installed_at TEXT NOT NULL,
            updated_at  TEXT NOT NULL,
            manifest_path TEXT,
            permissions TEXT,
            min_runtime TEXT
        );
        CREATE TABLE IF NOT EXISTS extension_assignments (
            id            TEXT PRIMARY KEY,
            extension_id  TEXT NOT NULL,
            agent_tool_id TEXT NOT NULL,
            sub_agent_id  TEXT,
            enabled       INTEGER NOT NULL DEFAULT 1,
            link_status   TEXT NOT NULL DEFAULT 'missing',
            assigned_at   TEXT NOT NULL,
            UNIQUE(extension_id, agent_tool_id, sub_agent_id)
        );
        CREATE TABLE IF NOT EXISTS agent_tools (
            id                TEXT PRIMARY KEY,
            name              TEXT NOT NULL,
            process_name      TEXT NOT NULL,
            base_dir          TEXT NOT NULL,
            hook_supported    INTEGER NOT NULL DEFAULT 0,
            hook_event_case   TEXT NOT NULL DEFAULT 'none',
            mcp_format        TEXT NOT NULL DEFAULT 'json',
            detected          INTEGER NOT NULL DEFAULT 0,
            enabled           INTEGER NOT NULL DEFAULT 1
        );
        CREATE TABLE IF NOT EXISTS sub_agents (
            id            TEXT PRIMARY KEY,
            name          TEXT NOT NULL,
            agent_tool_id TEXT NOT NULL,
            config_path   TEXT NOT NULL,
            format        TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS presets (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            description TEXT NOT NULL DEFAULT '',
            scope       TEXT NOT NULL DEFAULT 'universal',
            bound_tool  TEXT,
            created_at  TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS preset_items (
            id           TEXT PRIMARY KEY,
            preset_id    TEXT NOT NULL,
            extension_id TEXT NOT NULL,
            kind         TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS preset_applications (
            id            TEXT PRIMARY KEY,
            preset_id     TEXT NOT NULL,
            agent_tool_id TEXT NOT NULL,
            sub_agent_id  TEXT,
            applied_at    TEXT NOT NULL,
            active        INTEGER NOT NULL DEFAULT 1
        );
        CREATE TABLE IF NOT EXISTS resource_bindings (
            extension_id    TEXT PRIMARY KEY,
            exclusive_tools TEXT NOT NULL,
            reason          TEXT,
            updated_at      TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS tool_residents (
            tool_id       TEXT NOT NULL,
            extension_id  TEXT NOT NULL,
            PRIMARY KEY (tool_id, extension_id)
        );
        CREATE TABLE IF NOT EXISTS tool_base_snapshots (
            tool_id          TEXT PRIMARY KEY,
            active_preset_id TEXT,
            created_at       TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS tool_base_snapshot_items (
            tool_id       TEXT NOT NULL,
            extension_id  TEXT NOT NULL,
            kind          TEXT NOT NULL,
            origin        TEXT NOT NULL,
            PRIMARY KEY (tool_id, extension_id)
        );
        CREATE TABLE IF NOT EXISTS stash_journal (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            tool_id       TEXT NOT NULL,
            skill_name    TEXT NOT NULL,
            stashed_path  TEXT NOT NULL,
            original_path TEXT NOT NULL,
            created_at    TEXT NOT NULL,
            restored_at   TEXT
        );
        CREATE TABLE IF NOT EXISTS unread_sessions (
            tool_id          TEXT NOT NULL,
            session_id       TEXT NOT NULL,
            project_name     TEXT NOT NULL DEFAULT '',
            title            TEXT,
            last_message     TEXT,
            turned_green_at  INTEGER NOT NULL,
            expires_at       INTEGER NOT NULL,
            PRIMARY KEY (tool_id, session_id)
        );
        CREATE TABLE IF NOT EXISTS unread_read_tombstones (
            tool_id     TEXT NOT NULL,
            session_id  TEXT NOT NULL,
            read_at     INTEGER NOT NULL,
            PRIMARY KEY (tool_id, session_id)
        );
        CREATE TABLE IF NOT EXISTS heartbeat_observations (
            pid           INTEGER PRIMARY KEY,
            tool_id       TEXT NOT NULL,
            session_id    TEXT NOT NULL,
            last_seen_at  INTEGER NOT NULL
        );
        -- T3（手工验收修复批）：审批等待持久标记——hook 审批进入事件写入、清除事件/会话
        -- 消失删除；状态链接入（adapter/mod.rs）据此强制 Waiting。不用 30s TTL 事件文件
        -- 承载等待态（事件文件只是触发器，等待是一等持久信号，issue #74 根因①）
        CREATE TABLE IF NOT EXISTS approval_wait_marks (
            tool_id       TEXT NOT NULL,
            session_id    TEXT NOT NULL,
            ts            INTEGER NOT NULL,
            summary       TEXT,
            PRIMARY KEY (tool_id, session_id)
        );
        -- T8（手工验收修复批·批次乙）：问题等待持久标记——AskUserQuestion 的 PreToolUse
        -- hook 事件写入、清除事件/会话消失删除；叠加层同样强制 Waiting。**与
        -- approval_wait_marks 严格分表**（硬约束：问题标记不得触发审批红卡、审批标记
        -- 不得触发问答卡——分表使两类标记在 DAO 层天然隔离，隔离用例见 server.rs /
        -- adapter/mod.rs tests）。payload = 事件携带的 tool_input 原文 JSON（questions
        -- 载荷随标记落库，问答端点据此出卡；helper 问答通道 64KB 上限同源约束）
        CREATE TABLE IF NOT EXISTS question_wait_marks (
            tool_id       TEXT NOT NULL,
            session_id    TEXT NOT NULL,
            ts            INTEGER NOT NULL,
            summary       TEXT,
            payload       TEXT,
            PRIMARY KEY (tool_id, session_id)
        );
        CREATE TABLE IF NOT EXISTS inject_queue (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id    TEXT NOT NULL,
            agent_type    TEXT NOT NULL,
            device_id     TEXT NOT NULL,
            device_name   TEXT NOT NULL,
            content       TEXT NOT NULL,
            enqueued_at   INTEGER NOT NULL,
            sent_at       INTEGER,
            failed_reason TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_inject_queue_session ON inject_queue(session_id, id);
        -- ===== Codex Desktop IPC Task 7：持久动作账本 =====
        -- 每个远程控制动作（普通消息/两类审批/阻塞问答）一行，FIFO 按 seq（AUTOINCREMENT，
        -- 不按毫秒时间戳/随机 UUID）；state ∈ queued/reserved/accepted/rejected/unknown/cancelled。
        -- queued_text 仅普通待发消息保存**原文**（派发后清空；来源标记在派发时恰好组装一次）；
        -- 审批答案只存 payload_hash，不存答案原文。receipt_json 是 ActionReceipt 的 camelCase
        -- JSON。compat_* 三列是 PR #117 兼容兜底的持久 fence（compat_outstanding=1 期间该
        -- Thread 队列不接力）；compat_rollout_path/compat_rollout_baseline/compat_wire_hash 是
        -- 只读核对（reconcile）的证据字段，随 fence 持久保存、跨热重启与应用重启有效。
        -- barrier_force_released_at 是人工解除屏障的时间戳（2026-10-19 无出口屏障修复，
        -- 可空）：非 NULL = 屏障已被人工解除，全部置屏障写点带同名单调性守卫不回置。
        -- 撤回 = CAS queued→cancelled，**保留行作墓碑**（已撤 actionId 重放据此拒绝，不重发）。
        CREATE TABLE IF NOT EXISTS codex_actions (
            seq                     INTEGER PRIMARY KEY AUTOINCREMENT,
            action_id               TEXT UNIQUE NOT NULL,
            device_id               TEXT NOT NULL,
            device_name             TEXT NOT NULL,
            thread_id               TEXT NOT NULL,
            kind                    TEXT NOT NULL,
            request_key             TEXT,
            context_token           TEXT,
            payload_hash            TEXT NOT NULL,
            client_message_id       TEXT NOT NULL,
            state                   TEXT NOT NULL,
            route                   TEXT NOT NULL DEFAULT 'none',
            queued_text             TEXT,
            created_at              INTEGER NOT NULL,
            updated_at              INTEGER NOT NULL,
            receipt_json            TEXT,
            compat_outstanding      INTEGER NOT NULL DEFAULT 0,
            compat_message_id       TEXT,
            compat_turn_id          TEXT,
            compat_rollout_path     TEXT,
            compat_rollout_baseline INTEGER,
            compat_wire_hash        TEXT,
            barrier_force_released_at INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_codex_actions_thread_state_seq
            ON codex_actions(thread_id, state, seq);
        CREATE TABLE IF NOT EXISTS write_audit (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            ts          INTEGER NOT NULL,
            device_id   TEXT NOT NULL,
            device_name TEXT NOT NULL,
            agent_type  TEXT NOT NULL,
            session_id  TEXT NOT NULL,
            channel     TEXT NOT NULL,
            action      TEXT NOT NULL,
            summary     TEXT NOT NULL,
            result      TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS session_archive (
            session_id   TEXT PRIMARY KEY,
            agent_type   TEXT NOT NULL,
            project_path TEXT NOT NULL,
            project_name TEXT NOT NULL,
            title        TEXT,
            last_status  TEXT NOT NULL,
            first_seen   TEXT NOT NULL,
            last_seen    TEXT NOT NULL,
            updated_at   TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS session_board_hidden (
            session_id   TEXT PRIMARY KEY,
            hidden_at    TEXT NOT NULL
        );
        -- ===== 用量账本（计划①，契约 §4）=====
        -- 明细：per-(源, 会话, **小时桶**, **记录级项目键**, 模型, 供应商) 的增量账；保留期可配（默认 90 天）。
        -- hour_key = "YYYY-MM-DDTHH"（本地时区，支持 5h 档小时趋势）；day_key 为派生冗余列
        -- （substr(hour_key,1,10)），只服务日聚合折叠与保留期按天裁剪。
        -- project_key **进主键**（D21 规则④「记录级归属」，与契约 §4 的同步句逐字同义）：一个会话文件
        -- 可跨多个项目，同一会话同一小时里的两个 cwd 必须是两行——若只当普通列，UPSERT 会把两条 cwd 的
        -- 量折进一行、归属丢失，且与日聚合（主键含 project_key）口径分叉。
        -- **会话维度表的项目列是会话级近似（首个带 cwd 的记录），只作会话列表展示，不得作为聚合依据**。
        -- user_est **可空**（不是 NOT NULL DEFAULT 0，契约 §4 明文）：claude/codex/kimi 之外拿不到用户
        -- 文本的源写 NULL，小时档与日档都按「有值才累加、全 NULL 才是 NULL」处理，绝不显示成 0（D15/§8.2/§9.5）。
        -- 四桶**一律是归一化后的规范桶**（语义差异已在归一化层消化）；request_total 是
        -- **落库派生列**（逐条按该条语义算出后累加），因此同一条明细行内部语义混存也正确；
        -- cache_semantics 记录该行主导语义（诊断用，不参与聚合）。
        -- 计数类（turns/error_*/interrupted/tool_calls/tool_ms）与时长样本（turn_ms 换行分隔、
        -- tool_stats JSON）用于「工作小结区」随区间取数（明细保留期内可得，超出即空态）。
        CREATE TABLE IF NOT EXISTS usage_detail (
            source_id       TEXT NOT NULL,
            session_id      TEXT NOT NULL,
            project_key     TEXT NOT NULL DEFAULT '',
            hour_key        TEXT NOT NULL,
            day_key         TEXT NOT NULL,
            model           TEXT NOT NULL DEFAULT '',
            provider        TEXT NOT NULL DEFAULT '',
            provider_kind   TEXT NOT NULL DEFAULT 'unknown',
            input_fresh     INTEGER NOT NULL DEFAULT 0,
            cache_read      INTEGER NOT NULL DEFAULT 0,
            cache_write     INTEGER NOT NULL DEFAULT 0,
            output          INTEGER NOT NULL DEFAULT 0,
            request_total   INTEGER NOT NULL DEFAULT 0,
            requests        INTEGER NOT NULL DEFAULT 0,
            user_est        INTEGER,
            cache_semantics TEXT NOT NULL DEFAULT 'exclusive',
            turns           INTEGER NOT NULL DEFAULT 0,
            error_model     INTEGER NOT NULL DEFAULT 0,
            error_turn      INTEGER NOT NULL DEFAULT 0,
            error_tool      INTEGER NOT NULL DEFAULT 0,
            interrupted     INTEGER NOT NULL DEFAULT 0,
            tool_calls      INTEGER NOT NULL DEFAULT 0,
            tool_ms         INTEGER NOT NULL DEFAULT 0,
            turn_ms         TEXT NOT NULL DEFAULT '',
            tool_stats      TEXT NOT NULL DEFAULT '',
            first_seen_at   INTEGER NOT NULL,
            updated_at      INTEGER NOT NULL,
            -- 主键列序与契约 §4 逐字一致：projectKey 在 sessionId 之后、hourKey 之前
            PRIMARY KEY (source_id, session_id, project_key, hour_key, model, provider)
        );
        CREATE INDEX IF NOT EXISTS idx_usage_detail_day ON usage_detail(day_key);
        CREATE INDEX IF NOT EXISTS idx_usage_detail_session ON usage_detail(source_id, session_id);
        CREATE INDEX IF NOT EXISTS idx_usage_detail_project ON usage_detail(project_key, day_key);
        -- 日聚合：永久保留（长期趋势靠它）；写入时与明细同事务维护，清理只删明细不重算。
        -- 源能力相关列的取舍（逐条写明，避免「日档缺列」再次发生）：
        --   * provider_kind —— **保留**（三态标记必须随日聚合可见，D8）；
        --   * user_est —— **本列必须有且可空**：日档（7d/30d/自定义）是主视图，缺列会让
        --     claude/codex/kimi 在日档恒显示「—」（R1 缺口）；
        --   * 计数类（turns/error_*/interrupted/tool_*/turn_ms/tool_stats）与 cache_semantics
        --     —— **刻意不进日聚合**：前者的口径是「只在明细保留期内可得」（超出即空态，见遗留点 10），
        --     后者是逐行诊断值、按天折叠无意义（契约 §4 的日聚合列清单也只列四桶与 requests）。
        CREATE TABLE IF NOT EXISTS usage_daily (
            day_key       TEXT NOT NULL,
            source_id     TEXT NOT NULL,
            project_key   TEXT NOT NULL DEFAULT '',
            provider      TEXT NOT NULL DEFAULT '',
            provider_kind TEXT NOT NULL DEFAULT 'unknown',
            model         TEXT NOT NULL DEFAULT '',
            input_fresh   INTEGER NOT NULL DEFAULT 0,
            cache_read    INTEGER NOT NULL DEFAULT 0,
            cache_write   INTEGER NOT NULL DEFAULT 0,
            output        INTEGER NOT NULL DEFAULT 0,
            request_total INTEGER NOT NULL DEFAULT 0,
            requests      INTEGER NOT NULL DEFAULT 0,
            user_est      INTEGER,
            updated_at    INTEGER NOT NULL,
            PRIMARY KEY (day_key, source_id, project_key, provider, model)
        );
        CREATE INDEX IF NOT EXISTS idx_usage_daily_day ON usage_daily(day_key);
        -- 采集游标：per-(源, 会话) 水位。byte_offset+ordinal = 字节双水位（借鉴 Codex APP
        -- thread_history_projection_state 的 (next_rollout_byte_offset, next_rollout_ordinal)）；
        -- fingerprint = 已消费前缀末尾 ≤64 字节的 sha256（截断/重写检测）；last_cumulative =
        -- 累计型源（opencode 会话级列 / dsh 投影缓存四桶）的上次累计值，取差值算增量；
        -- mtime_ms/file_size = 文件代际，(mtime,size) 未变即整文件跳过（不打开不读取）。
        -- state_json = 采集器自己的**续读状态**（模型继承 / 未配对工具调用 / 相邻去重上一条
        -- 等，serde JSON 小对象）：增量读从文件中段开始，没有它就会丢模型归属与工具配对
        -- （等价于 Codex APP 的 thread_history_projection_state 思路）。
        CREATE TABLE IF NOT EXISTS usage_cursor (
            source_id       TEXT NOT NULL,
            session_id      TEXT NOT NULL,
            fingerprint     TEXT NOT NULL DEFAULT '',
            byte_offset     INTEGER NOT NULL DEFAULT 0,
            ordinal         INTEGER NOT NULL DEFAULT 0,
            last_cumulative INTEGER,
            mtime_ms        INTEGER NOT NULL DEFAULT 0,
            file_size       INTEGER NOT NULL DEFAULT 0,
            state_json      TEXT NOT NULL DEFAULT '',
            updated_at      INTEGER NOT NULL,
            PRIMARY KEY (source_id, session_id)
        );
        -- 会话维度：per-(源, 会话) 的**会话级**项目近似/标题/活跃区间/子代理血缘（D17 分层依据）。
        -- ⚠️ 本表的项目列**不是**归属来源（R2）：归属按记录级键落在 usage_detail.project_key 与
        -- usage_daily.project_key 上（一个会话可跨多个项目）；本表只存该会话**首个带 cwd 的记录**
        -- 的项目（会话级近似，确定性规则），供会话维度留档与血缘展示，**不进任何分组口径**。
        -- 因此小时档/日档都不存在「只能拿会话级」的档位 → availability 无需新增项目条目
        -- （UsageAvailability.metric 是契约冻结的 8 值枚举）；若将来真出现会话级降级档位，
        -- 必须在此登记 availability 条目后才可上线。
        -- D21 口径：project_key = **projectName 原文的小写**（分组键）、project_label =
        -- projectName 原文（显示名，来自既有 project_name_from_path，basename）；project_path_raw
        -- = 记录内 cwd 原文（各源形态不同，仅留档）；project_realpath = realpath 规范化路径
        -- （**不作 UI 用途**，只为日后修归类时无需重扫）。
        -- originator：Codex 的 session_meta.originator 原文（CLI/APP 拆分的唯一依据，§5.2）；
        -- 其余源留 NULL。
        CREATE TABLE IF NOT EXISTS usage_session (
            source_id         TEXT NOT NULL,
            session_id        TEXT NOT NULL,
            project_key       TEXT NOT NULL DEFAULT '',
            project_label     TEXT NOT NULL DEFAULT '',
            project_path_raw  TEXT NOT NULL DEFAULT '',
            project_realpath  TEXT NOT NULL DEFAULT '',
            title             TEXT,
            is_subagent       INTEGER NOT NULL DEFAULT 0,
            parent_session_id TEXT,
            originator        TEXT,
            first_seen_at     INTEGER NOT NULL,
            last_seen_at      INTEGER NOT NULL,
            PRIMARY KEY (source_id, session_id)
        );
        "#,
    )
    .expect("Failed to initialize database schema");
}
