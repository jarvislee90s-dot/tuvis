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
        -- ===== Codex Desktop IPC：持久动作账本 =====
        -- 每个远程控制动作（普通消息/两类审批/阻塞问答）一行，FIFO 按 seq（AUTOINCREMENT，
        -- 不按毫秒时间戳/随机 UUID）；state ∈ queued/reserved/accepted/rejected/unknown/cancelled。
        -- queued_text 仅普通待发消息保存**原文**（派发后清空；来源标记在派发时恰好组装一次）；
        -- 审批答案只存 payload_hash，不存答案原文。receipt_json 是 ActionReceipt 的 camelCase
        -- JSON。compat_* 三列是 PR #117 兼容兜底的持久 fence（compat_outstanding=1 期间该
        -- Thread 队列不接力）；compat_rollout_path/compat_rollout_baseline/compat_wire_hash 是
        -- 只读核对（reconcile）的证据字段，随 fence 持久保存、跨热重启与应用重启有效。
        -- barrier_force_released_at 是人工解除屏障的时间戳（可空）：非 NULL = 已被人工解除，
        -- 全部置屏障写点带同名单调性守卫不回置。
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
            barrier_force_released_at INTEGER,
            -- 签名开关的登记时冻结值（1 = 正文已带 `[mobile 花名]` 签名）。
            -- NULL = 本列上线前登记的旧行：按升级前语义（恒带签名）派发。
            signature_applied       INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_codex_actions_thread_state_seq
            ON codex_actions(thread_id, state, seq);
        -- partial index 只收「调度/对账关心的行」，随 migrate 幂等重放补建到老库。
        -- ① 兼容屏障/Turn 绑定在册行（周期对账枚举 + thread 内屏障/fence 查询共用）
        CREATE INDEX IF NOT EXISTS idx_codex_actions_compat_outstanding
            ON codex_actions(thread_id, seq) WHERE compat_outstanding = 1;
        -- ② queued 普通消息（对账枚举的第二分支；与 thread_state_seq 索引互补）
        CREATE INDEX IF NOT EXISTS idx_codex_actions_queued_send
            ON codex_actions(thread_id, seq) WHERE state = 'queued' AND kind = 'send';
        -- ③ Turn 顺序绑定行（路由器重启重建输入）
        CREATE INDEX IF NOT EXISTS idx_codex_actions_turn_binding
            ON codex_actions(route, state, seq) WHERE compat_outstanding = 1;
        -- ④ 终态行墓碑化候选（保留清理的批量分批选择：updated_at 范围 SEEK + LIMIT 早停）
        CREATE INDEX IF NOT EXISTS idx_codex_actions_terminal_retention
            ON codex_actions(updated_at)
            WHERE state IN ('accepted', 'rejected', 'cancelled') AND compat_outstanding = 0;
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
        -- 三终端语言自适应（2026-10-22）：远程端配对设备语言手动记账（C-5）。
        -- 键 = remote_devices.id（配对身份同源，换网络/换 IP 不丢；重新配对新身份天然无记录）；
        -- 行存在即「该设备设置过手动语言」（manual 恒 true，无独立布尔列），
        -- language 只认 "zh"/"en"（值集校验在 Rust 侧 remote::language::UiLanguage::parse）。
        -- 只由移动端手动切换端点写入，自动跟随结果绝不落账；读侧无行 = 未手动选择。
        CREATE TABLE IF NOT EXISTS remote_device_language (
            device_id   TEXT PRIMARY KEY,
            language    TEXT NOT NULL,
            updated_at  INTEGER NOT NULL DEFAULT 0
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
        -- 计费配置 v2（用量看板 V2 Task 3）：当前有效配置 + 每次提交的历史快照。
        -- 撤销不删除历史，而是以新 revision 再追加一条快照；state 只保留 singleton=1 的当前版本。
        CREATE TABLE IF NOT EXISTS usage_billing_state (
            singleton   INTEGER PRIMARY KEY CHECK(singleton=1),
            revision    INTEGER NOT NULL,
            config_json TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS usage_billing_revision (
            revision     INTEGER PRIMARY KEY,
            committed_at INTEGER NOT NULL,
            config_json  TEXT NOT NULL
        );
        -- 公开价格快照（Task 8）：active 指针 + 内容/获取时间；关闭仍保留最后有效快照。
        -- 价格只落在 tuvis.db，不创建临时价格文件。
        CREATE TABLE IF NOT EXISTS usage_pricing_state (
            singleton          INTEGER PRIMARY KEY CHECK(singleton=1),
            enabled            INTEGER NOT NULL DEFAULT 0,
            active_snapshot_id TEXT,
            last_error         TEXT
        );
        CREATE TABLE IF NOT EXISTS usage_pricing_snapshot (
            id           TEXT PRIMARY KEY,
            source_url   TEXT NOT NULL,
            fetched_at   INTEGER NOT NULL,
            payload_json TEXT NOT NULL
        );
        -- 原生人民币价格（2026-10-20 用户裁决）：刷新时从 models.dev 源 TOML 的
        -- `# List price: CNY …` / `# CNY per 1M tokens …` / `# input ¥…` 注释解析。
        -- 一条 = 某快照下某目录条目的人民币四价；`*_scaled` 单位 10^-9 CNY / 百万 Token。
        -- 只存**明确写出的**人民币价；没有的人民币价由费用查询按当日汇率换算并标 converted。
        CREATE TABLE IF NOT EXISTS usage_pricing_native (
            snapshot_id         TEXT NOT NULL,
            provider_id         TEXT NOT NULL,
            model_id            TEXT NOT NULL,
            normalized_model_id TEXT NOT NULL,
            currency            TEXT NOT NULL CHECK(currency = 'CNY'),
            input_scaled        INTEGER,
            cache_read_scaled   INTEGER,
            cache_write_scaled  INTEGER,
            output_scaled       INTEGER,
            source_url          TEXT NOT NULL,
            fetched_at          INTEGER NOT NULL,
            PRIMARY KEY (snapshot_id, provider_id, model_id)
        );
        CREATE INDEX IF NOT EXISTS idx_usage_pricing_native_normalized
            ON usage_pricing_native(snapshot_id, normalized_model_id);
        -- 当日汇率（2026-10-20 用户裁决）：与价格快照同生命周期保存 USD→CNY；
        -- `rate_scaled` 单位 10^-9 CNY / USD。缺失时费用层回退 cached/default 并标注。
        CREATE TABLE IF NOT EXISTS usage_pricing_fx (
            snapshot_id TEXT PRIMARY KEY,
            rate_scaled INTEGER NOT NULL,
            fetched_at  INTEGER NOT NULL,
            source_url  TEXT NOT NULL
        );
        -- Provider 别名（用量看板 V2 Task 2）：非空 provider 按原始串精确映射；
        -- 空 provider 按「工具 + 原始模型」逐条指派（model_id 可为空串 = 该工具的未知模型）。
        -- 别名只影响展示/分组，绝不回写账本 observedProvider，也不参与 billing revision。
        CREATE TABLE IF NOT EXISTS usage_provider_alias (
            id           TEXT PRIMARY KEY,
            kind         TEXT NOT NULL CHECK(kind IN ('provider','toolModel')),
            provider_raw TEXT,
            source_id    TEXT,
            model_id     TEXT,
            display_name TEXT NOT NULL,
            created_at   INTEGER NOT NULL,
            updated_at   INTEGER NOT NULL,
            CHECK (
                (kind = 'provider' AND provider_raw IS NOT NULL AND provider_raw <> ''
                 AND source_id IS NULL AND model_id IS NULL)
                OR
                (kind = 'toolModel' AND provider_raw IS NULL
                 AND source_id IS NOT NULL AND model_id IS NOT NULL)
            )
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_usage_provider_alias_provider
            ON usage_provider_alias(provider_raw) WHERE provider_raw IS NOT NULL;
        CREATE UNIQUE INDEX IF NOT EXISTS idx_usage_provider_alias_tool_model
            ON usage_provider_alias(source_id, model_id) WHERE provider_raw IS NULL;
        -- 手动覆盖价（V2 Task 3 / 2026-10-20 增加币种）：按归一化模型 ID 全局生效，
        -- 四价逐项可空（至少一项）。`*_scaled` 单位 = 10^-9 `currency` / 百万 Token；
        -- 刷新流程只写快照/状态表，绝不触碰本表。
        CREATE TABLE IF NOT EXISTS usage_price_override (
            normalized_model_id TEXT PRIMARY KEY,
            currency            TEXT NOT NULL DEFAULT 'USD' CHECK(currency IN ('USD','CNY')),
            input_scaled        INTEGER,
            cache_read_scaled   INTEGER,
            cache_write_scaled  INTEGER,
            output_scaled       INTEGER,
            -- 分时价格（2026-10-21）：time_enabled=1 时上面四价 = 高峰价，
            -- offpeak_* = 低谷价；time_windows 为 JSON 数组（start/end HH:00）。
            time_enabled        INTEGER NOT NULL DEFAULT 0 CHECK(time_enabled IN (0,1)),
            time_timezone       TEXT NOT NULL DEFAULT 'asiaShanghai',
            time_windows        TEXT,
            offpeak_input_scaled        INTEGER,
            offpeak_cache_read_scaled   INTEGER,
            offpeak_cache_write_scaled  INTEGER,
            offpeak_output_scaled       INTEGER,
            updated_at          INTEGER NOT NULL,
            CHECK (
                input_scaled IS NOT NULL OR cache_read_scaled IS NOT NULL
                OR cache_write_scaled IS NOT NULL OR output_scaled IS NOT NULL
            ),
            CHECK (
                (input_scaled IS NULL OR input_scaled >= 0)
                AND (cache_read_scaled IS NULL OR cache_read_scaled >= 0)
                AND (cache_write_scaled IS NULL OR cache_write_scaled >= 0)
                AND (output_scaled IS NULL OR output_scaled >= 0)
            ),
            CHECK (
                (offpeak_input_scaled IS NULL OR offpeak_input_scaled >= 0)
                AND (offpeak_cache_read_scaled IS NULL OR offpeak_cache_read_scaled >= 0)
                AND (offpeak_cache_write_scaled IS NULL OR offpeak_cache_write_scaled >= 0)
                AND (offpeak_output_scaled IS NULL OR offpeak_output_scaled >= 0)
            )
        );
        -- 模型条目配置（2026-10-20 计划 §2.1）：自定义模型显示名 + 计价身份覆盖。
        -- 按**观测身份**存储：非空 provider 用 (provider_raw, model_raw)（source_id 为 NULL，跨工具生效）；
        -- 空 provider 条目用 (source_id, model_raw)（provider_raw 为空串）。展示键（显示供应商, 原始模型）
        -- 在读取时经 usage_provider_alias 解析派生——别名改名/删除不需要迁移本表。
        -- display_name 为 NULL = 未自定义（显示原始模型名）；pricing_* 成对为 NULL = 未覆盖
        -- （走高级规则/自动匹配）。id 由服务层按身份确定性派生（主键冲突 = 身份冲突）。
        CREATE TABLE IF NOT EXISTS usage_model_config (
            id               TEXT PRIMARY KEY,
            provider_raw     TEXT NOT NULL,
            source_id        TEXT,
            model_raw        TEXT NOT NULL,
            display_name     TEXT,
            pricing_provider TEXT,
            pricing_model    TEXT,
            updated_at       INTEGER NOT NULL,
            CHECK (
                (provider_raw <> '' AND source_id IS NULL)
                OR (provider_raw = '' AND source_id IS NOT NULL)
            ),
            CHECK ((pricing_provider IS NULL) = (pricing_model IS NULL)),
            CHECK (pricing_provider IS NULL OR (pricing_provider <> '' AND pricing_model <> ''))
        );
        -- SQLite 表级 PRIMARY KEY 不折叠 NULL：必须用表达式唯一索引锁身份键。
        CREATE UNIQUE INDEX IF NOT EXISTS idx_usage_model_config_identity
            ON usage_model_config(provider_raw, COALESCE(source_id, ''), model_raw);
        -- #172：Codex Desktop IPC 能力探测账本（程序自用的证据缓存，不随会话增长）。
        --   probe_key = platform|build|cli_id|probe_epoch（probe/fingerprint.rs）；
        --   一行 = 一个 (版本指纹, 能力)，upsert 覆盖（ON CONFLICT），**不是 append**；
        --   state ∈ probed|failed|inconclusive（`verified` 只来自 shipped 表，账本永不写）；
        --   不存正文 / threadId / 原始帧（只存 stage / 错误码 / 耗时，对齐审计隐私纪律）。
        --   时间列用 INTEGER 毫秒（与 heartbeat_observations 等既有表同惯例；设计稿写
        --   TEXT 是示意，语义只要求单调可比较）。
        --   fingerprint：官方 schema 探针的**内容指纹**（文件名+大小的 sha256）——只对
        --   capability='schema' 的行有意义；让「build × 能力」数据集能追到具体
        --   schema 版本（老库由 migration.rs 补列）。
        CREATE TABLE IF NOT EXISTS codex_ipc_probe (
            probe_key        TEXT NOT NULL,
            capability       TEXT NOT NULL,
            state            TEXT NOT NULL,
            probe_stage      TEXT,
            error_code       TEXT,
            fingerprint      TEXT,
            attempts         INTEGER NOT NULL DEFAULT 1,
            first_seen_at    INTEGER NOT NULL,
            last_seen_at     INTEGER NOT NULL,
            next_retry_at    INTEGER,
            last_duration_ms INTEGER,
            PRIMARY KEY (probe_key, capability)
        );
        -- ===== TPS 短时抽样三表（2026-10-24 计划 Task 2 / spec §6 固定工程契约）=====
        -- 与旧 usage_* 账本**完全解耦**：不共用表、不共用游标（usage_cursor 是旧用量域的，
        -- 本域游标只落 tps_state）、保留期独立 24 小时（本域 purge 只删这三张表，绝不动
        -- usage_* 的保留设置）。
        -- ① 事实表：主键 = source+host_instance+stable_call_key（spec §6）；UPSERT =
        --    后写整行覆盖（同一调用重复读取取更新鲜的值，spec §7「同调用重复读取不翻倍」
        --    的存储半边；点入样仍由 policy 层按稳定键去重）。失败/取消也落表（spec §3：
        --    不入速度但有可靠起止仍进耗时详情），可空列 = 「不可得」，读回 None 不填 0。
        --    started/ended 的 epoch 非负 CHECK 既是数据哨兵，也是「批内中途失败」原子性
        --    验收的注入缝（tests/tps_store_test.rs 用 ended_at_ms=-1 的行让事务在批中段失败）。
        CREATE TABLE IF NOT EXISTS tps_observation (
            source             TEXT NOT NULL,
            host_instance      TEXT NOT NULL,
            stable_call_key    TEXT NOT NULL,
            started_at_ms      INTEGER NOT NULL CHECK(started_at_ms >= 0),
            first_token_at_ms  INTEGER,
            ended_at_ms        INTEGER NOT NULL CHECK(ended_at_ms >= 0),
            native_duration_ms INTEGER,
            generation_ms      INTEGER,
            -- V2（桌面四页导航与输出速度 V2 Task 1）：含等待的响应时长 ms（开始→终态）。
            -- 可空无默认：旧行 NULL = 旧记录无显式响应窗（读取层走旧流式有效性的兼容路径）；
            -- 老库由 migration.rs ALTER 幂等补列
            response_elapsed_ms INTEGER,
            output_tokens      INTEGER,
            output_scope       TEXT NOT NULL,
            tool_ms            INTEGER,
            model              TEXT NOT NULL,
            channel            TEXT,
            session_key        TEXT,
            turn_key           TEXT,
            purpose            TEXT NOT NULL,
            outcome            TEXT NOT NULL,
            method             TEXT NOT NULL,
            failure_reason     TEXT,
            PRIMARY KEY (source, host_instance, stable_call_key)
        );
        -- spec「三表与 source/ended_at、sampled_at/series 索引」：保留清理与按 series 取数都走这两列
        CREATE INDEX IF NOT EXISTS idx_tps_observation_ended
            ON tps_observation(ended_at_ms);
        CREATE INDEX IF NOT EXISTS idx_tps_observation_series
            ON tps_observation(source, model, method);
        -- V2 dashboard（2026-10-26 计划 Task 6）：series 维过滤 + 完成窗范围扫描的复合索引
        -- （source, model, channel, method, ended_at_ms）——桶查询/工具筛选都以此命中
        -- （ channel 可空：SQLite 索引含 NULL，`channel IS ?` 仍可走该索引）
        CREATE INDEX IF NOT EXISTS idx_tps_observation_series_ended
            ON tps_observation(source, model, channel, method, ended_at_ms);
        -- ② 采样点表：主键 = tick_id+series_key（spec §6）。tick_id 由 store 层取
        --    TpsPoint.point_id（policy 层已按 {run_id}:{sampled_at}:{series_id} 确定性生成，
        --    见 services/tps/store.rs），同点重存 = UPSERT 不重复。
        --    列名与 spec §6 点列清单对齐：n = TpsPoint.sample_count（TPS 有效 n，≤20）；
        --    ttft_count = ttft_sample_count（首点可对齐的独立分母）；ttft_sum_ms = ttft_sum_ms；
        --    trigger_kind = trigger（避开 SQL 关键字）。其余列为 TpsMetrics 平铺镜像
        --    （TpsPoint 承载全部指标面，读侧要还原整点）；age_ms 是「当时钟−结束」派生值，
        --    **不落库**，读侧按 sampled_at 重算。status 钉 spec 逐字二值；window_ms 钉
        --    spec 逐字 120000（与 policy::TPS_WINDOW_MS 同源常量）。
        CREATE TABLE IF NOT EXISTS tps_point (
            tick_id              TEXT NOT NULL,
            series_key           TEXT NOT NULL,
            sampled_at           INTEGER NOT NULL,
            window_ms            INTEGER NOT NULL CHECK(window_ms = 120000),
            run_id               TEXT NOT NULL,
            trigger_kind         TEXT NOT NULL,
            status               TEXT NOT NULL CHECK(status IN ('value','gap')),
            -- 发点时生效的计划节奏 ms（可见 60000 / 后台 600000；独立复核 R11）；旧行 NULL
            cadence_ms           INTEGER,
            method               TEXT NOT NULL,
            output_scope         TEXT NOT NULL,
            quality              TEXT NOT NULL,
            n                    INTEGER NOT NULL DEFAULT 0,
            sum_output_tokens    INTEGER NOT NULL DEFAULT 0,
            sum_generation_ms    INTEGER NOT NULL DEFAULT 0,
            ttft_sum_ms          INTEGER,
            ttft_count           INTEGER NOT NULL DEFAULT 0,
            model_ms             INTEGER,
            tool_ms              INTEGER,
            avg_first_token_ms   INTEGER,
            tps                  REAL,
            candidate_count      INTEGER NOT NULL DEFAULT 0,
            timed_model_count    INTEGER NOT NULL DEFAULT 0,
            timed_tool_count     INTEGER NOT NULL DEFAULT 0,
            failed_count         INTEGER NOT NULL DEFAULT 0,
            cancelled_count      INTEGER NOT NULL DEFAULT 0,
            missing_timing_count INTEGER NOT NULL DEFAULT 0,
            latest_ended_at      INTEGER,
            sampled_keys         TEXT NOT NULL DEFAULT '[]',
            PRIMARY KEY (tick_id, series_key)
        );
        CREATE INDEX IF NOT EXISTS idx_tps_point_sampled
            ON tps_point(sampled_at, series_key);
        -- ③ 状态表：key+JSON（spec §6：源游标/安装记录/调度元数据）。键约定由
        --    services/tps/store.rs 固定（cursor:<source> / source_state:<source>）；通用
        --    key/JSON 通道供安装记录与调度元数据复用。游标**只**落本表，不借旧 usage_cursor。
        CREATE TABLE IF NOT EXISTS tps_state (
            key        TEXT PRIMARY KEY,
            value_json TEXT NOT NULL,
            updated_at INTEGER NOT NULL
        );
        "#,
    )
    .expect("Failed to initialize database schema");
}
