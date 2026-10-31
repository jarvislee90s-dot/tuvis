use rusqlite::Connection;

fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    conn.prepare(&format!("SELECT {} FROM {} LIMIT 0", column, table))
        .is_ok()
}

/// 表是否存在（老库形状探测用；避免对不存在的表做 ALTER）。
fn table_exists(conn: &Connection, table: &str) -> bool {
    conn.prepare(&format!("SELECT 1 FROM {} LIMIT 0", table))
        .is_ok()
}

/// 列是否 NOT NULL（老库形状探测用）：表或列不存在 → None
fn column_notnull(conn: &Connection, table: &str, column: &str) -> Option<bool> {
    conn.prepare(&format!("PRAGMA table_info({})", table))
        .ok()?
        .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(3)?)))
        .ok()?
        .filter_map(Result::ok)
        .find(|(name, _)| name == column)
        .map(|(_, notnull)| notnull != 0)
}

pub fn migrate(conn: &Connection) -> Result<(), String> {
    // ---- 用量账本（计划①）①：预览版明细表**形状不可就地迁移** ----
    // 主键要加 project_key（记录级归属，GC 18-R2）、user_est 要从 NOT NULL DEFAULT 0 改成可空（R1）
    // ——SQLite 改不了主键、也去不掉 NOT NULL。处理：整表重建（明细本就是「源文件还在、可重采」的
    // 增量账，且账本从未随版本发布过），四张表数据一并清空——**否则**「明细被删但游标已推进」会留下
    // 永久缺口、日聚合还会被重采的同一批记录二次累加。
    //
    // ⚠️ 本段**必须**在下面 `schema::init` **之前**执行：schema.rs 会在 usage_detail 上建
    // `idx_usage_detail_project(project_key, day_key)`，而预览版明细表没有 project_key 列，
    // 那时建索引会以 `no such column: project_key` 让 `schema::init`（内部 `.expect`）直接 panic。
    // （任务书 Step 4 把本段放在 migrate 末尾 → 永远不可达，实测见报告「偏差申报」。）
    let preview_detail = column_exists(conn, "usage_detail", "hour_key")
        && (!column_exists(conn, "usage_detail", "project_key")
            || column_notnull(conn, "usage_detail", "user_est") == Some(true));
    if preview_detail {
        // DROP 而非 DELETE 其余三表：预览版四表由同一条 DDL 批量建立，形状同样可能不合规
        // （缺列同样会让 init 建索引/后续读列失败）；DROP 后由紧随的 init 按新 DDL 重建，
        // 语义与「四表清空」完全一致（数据一律不保留）
        conn.execute_batch(
            "DROP TABLE IF EXISTS usage_detail;
             DROP TABLE IF EXISTS usage_daily;
             DROP TABLE IF EXISTS usage_cursor;
             DROP TABLE IF EXISTS usage_session;",
        )
        .map_err(|e| format!("MIGRATE_PREVIEW_LEDGER_FAIL: 重建预览版用量账本失败: {}", e))?;
        log::warn!("usage: 检测到预览版账本形状，已重建（四表清空，下次采集重算）");
    }

    let claims_exists = conn
        .prepare("SELECT 1 FROM codex_interaction_claims LIMIT 0")
        .is_ok();
    // B05：旧库的 action_id 唯一索引会阻止同组多题共享一个动作，必须撤掉。
    if claims_exists {
        conn.execute(
            "DROP INDEX IF EXISTS idx_codex_interaction_claims_action",
            [],
        )
        .map_err(|e| format!("MIGRATE_CLAIM_GROUP_FAIL: 迁移同组占用索引失败: {e}"))?;
    }
    if claims_exists && !column_exists(conn, "codex_interaction_claims", "claim_owner") {
        conn.execute(
            "ALTER TABLE codex_interaction_claims ADD COLUMN claim_owner TEXT NOT NULL DEFAULT ''",
            [],
        )
        .map_err(|e| format!("MIGRATE_CLAIM_OWNER_TOKEN_FAIL: 迁移占用调用令牌失败: {e}"))?;
    }
    if claims_exists && !column_exists(conn, "codex_interaction_claims", "claimed_owner") {
        conn.execute(
            "ALTER TABLE codex_interaction_claims ADD COLUMN claimed_owner TEXT NOT NULL DEFAULT ''",
            [],
        )
        .map_err(|e| format!("MIGRATE_CLAIM_OWNER_FAIL: 迁移同题占用世系列失败: {e}"))?;
    }
    if claims_exists && !column_exists(conn, "codex_interaction_claims", "claimed_epoch") {
        conn.execute(
            "ALTER TABLE codex_interaction_claims ADD COLUMN claimed_epoch INTEGER NOT NULL DEFAULT 0",
            [],
        )
        .map_err(|e| format!("MIGRATE_CLAIM_EPOCH_FAIL: 迁移同题占用代际列失败: {e}"))?;
    }

    // H01：旧成功表缺包内容身份（无 package_sha256）→ 重建（成功证据是派生数据，
    // 旧行本就无归因，不能猜归属）。重建后由紧随的 init 按新 DDL 建表。
    if !column_exists(conn, "codex_operation_success", "package_sha256") {
        conn.execute_batch(
            "DROP TABLE IF EXISTS codex_operation_success;
             CREATE TABLE codex_operation_success (
                 probe_key       TEXT NOT NULL,
                 package_sha256  TEXT NOT NULL,
                 operation       TEXT NOT NULL,
                 rule_version    INTEGER NOT NULL,
                 evidence_at     INTEGER NOT NULL,
                 PRIMARY KEY (probe_key, package_sha256, operation, rule_version)
             );",
        )
        .map_err(|e| format!("MIGRATE_OPERATION_SUCCESS_REBUILD_FAIL: 重建成功证据表失败: {e}"))?;
    }

    // 先幂等重放 schema.rs 全量 DDL（全部 IF NOT EXISTS）：老库/裸连接补建新表，
    // 已存在的表保持原样（缺列由下方 ALTER 迁移补齐），与生产启动顺序（schema::init → migrate）一致。
    // 上面 DROP 掉的预览版账本四表，由这次 init 按新 DDL 重建为空表。
    crate::database::schema::init(conn);

    // tps_point.cadence_ms（独立复核 R11）：老库的点表无此列 → 补加可空列（旧行 NULL = 无节奏信息，
    // 前端不据此断线）；schema::init 的 CREATE IF NOT EXISTS 对已存在表不生效，须 ALTER 补齐
    if !column_exists(conn, "tps_point", "cadence_ms") {
        conn.execute("ALTER TABLE tps_point ADD COLUMN cadence_ms INTEGER", [])
            .map_err(|e| format!("MIGRATE_FAIL: 迁移失败: {}", e))?;
    }

    // tps_observation.response_elapsed_ms（V2 Task 1）：老库的事实表无此列 → 补加可空列
    // （旧行 NULL = 旧记录无显式响应窗，读取层走旧流式有效性的兼容路径，不回填不猜测）；
    // 同上须 ALTER 幂等补齐。**只动 TPS 域三表，绝不触碰旧 usage_* 账本与游标**
    if !column_exists(conn, "tps_observation", "response_elapsed_ms") {
        conn.execute(
            "ALTER TABLE tps_observation ADD COLUMN response_elapsed_ms INTEGER",
            [],
        )
        .map_err(|e| format!("MIGRATE_FAIL: 迁移失败: {}", e))?;
    }

    // 检查 extension 表是否有 manifest_path 列
    if !column_exists(conn, "extensions", "manifest_path") {
        conn.execute("ALTER TABLE extensions ADD COLUMN manifest_path TEXT", [])
            .map_err(|e| format!("MIGRATE_FAIL: 迁移失败: {}", e))?;
    }
    if !column_exists(conn, "extensions", "permissions") {
        conn.execute("ALTER TABLE extensions ADD COLUMN permissions TEXT", [])
            .map_err(|e| format!("MIGRATE_FAIL: 迁移失败: {}", e))?;
    }
    if !column_exists(conn, "extensions", "min_runtime") {
        conn.execute("ALTER TABLE extensions ADD COLUMN min_runtime TEXT", [])
            .map_err(|e| format!("MIGRATE_FAIL: 迁移失败: {}", e))?;
    }

    // 旧版 extensions 表没有 suite/source_tool/is_native，会导致资源列表和补链读不到历史数据
    if !column_exists(conn, "extensions", "suite") {
        conn.execute("ALTER TABLE extensions ADD COLUMN suite TEXT", [])
            .map_err(|e| format!("MIGRATE_EXT_FIELDS_FAIL: 迁移扩展资源字段失败: {}", e))?;
    }
    if !column_exists(conn, "extensions", "source_tool") {
        conn.execute("ALTER TABLE extensions ADD COLUMN source_tool TEXT", [])
            .map_err(|e| format!("MIGRATE_EXT_FIELDS_FAIL: 迁移扩展资源字段失败: {}", e))?;
    }
    if !column_exists(conn, "extensions", "is_native") {
        conn.execute(
            "ALTER TABLE extensions ADD COLUMN is_native INTEGER NOT NULL DEFAULT 0",
            [],
        )
        .map_err(|e| format!("MIGRATE_EXT_FIELDS_FAIL: 迁移扩展资源字段失败: {}", e))?;
    }

    // #172：Codex Desktop IPC 能力账本补 fingerprint 列（官方 schema 探针的内容指纹，
    // 只对 capability='schema' 的行有意义）。新库由 schema.rs 的建表 DDL 直接带上；
    // 老库（表已存在、缺列）在此幂等补列——**不加会读列失败**（no such column）。
    if !column_exists(conn, "codex_ipc_probe", "fingerprint") {
        conn.execute(
            "ALTER TABLE codex_ipc_probe ADD COLUMN fingerprint TEXT",
            [],
        )
        .map_err(|e| format!("MIGRATE_FAIL: 迁移失败: {}", e))?;
    }
    // S1（2026-10-23 设计 §3.2）：成功证据与最近失败观测分离——补两列成功证据列。
    // 幂等可重入；**不改任何旧行的既有列值**（「老数据」由读侧 has_evidence 按
    // state∈probed|verified 兼容识别，写侧失败观测落账时按旧值提升，见 DAO）。
    // 终审 R3：codex_queue_pause 补归属 token 列（旧行 NULL = 无归属，仅 resume
    // /显式清除可删；新写入恒带 token——撤回只清本次拥有的行）。
    if !column_exists(conn, "codex_queue_pause", "pause_token") {
        conn.execute(
            "ALTER TABLE codex_queue_pause ADD COLUMN pause_token TEXT",
            [],
        )
        .map_err(|e| {
            format!(
                "MIGRATE_QUEUE_PAUSE_TOKEN_FAIL: 老库补列 codex_queue_pause.pause_token 失败: {e}"
            )
        })?;
    }
    // P3（2026-10-30）：停止类暂停绑定其等待的回合（打断/取消端点的 expectedTurnId
    // 安全阀）——settle 路径按该 Turn 的终态证据自动清除停止类暂停行；旧行 NULL =
    // 未绑定，保守保留（仅显式 resume 可清）。幂等可重入；必须在下方 R-T2 整表
    // 重建之前执行（重建的 SELECT 携带本列）。
    if !column_exists(conn, "codex_queue_pause", "pause_turn_id") {
        conn.execute(
            "ALTER TABLE codex_queue_pause ADD COLUMN pause_turn_id TEXT",
            [],
        )
        .map_err(|e| {
            format!(
                "MIGRATE_QUEUE_PAUSE_TURN_FAIL: 老库补列 codex_queue_pause.pause_turn_id 失败: {e}"
            )
        })?;
    }
    // 合并发送：批次成员需冻结各自的签名状态（回队时按原样恢复正文，不能二次
    // 签名/换正文）。schema 建表已含该列，但**早期建出的库只有 3 列**——补列幂等，
    // 老行按 0（未签名）回填，命中时与旧行为一致。
    if table_exists(conn, "codex_send_batch_members")
        && !column_exists(conn, "codex_send_batch_members", "signature_applied")
    {
        conn.execute(
            "ALTER TABLE codex_send_batch_members ADD COLUMN signature_applied INTEGER NOT NULL DEFAULT 0",
            [],
        )
        .map_err(|e| {
            format!(
                "MIGRATE_BATCH_SIGNATURE_FAIL: 老库补列 codex_send_batch_members.signature_applied 失败: {e}"
            )
        })?;
    }
    // R05：优先处理「停止尝试未知结果」幂等归属列（旧行 NULL = 未做过停止尝试；
    // 非 NULL = 同 actionId 重放先读本记录，不得再次发停止帧）。与动作行同生命周期，
    // 重启不丢。
    if !column_exists(conn, "codex_actions", "stop_attempt_reason") {
        conn.execute(
            "ALTER TABLE codex_actions ADD COLUMN stop_attempt_reason TEXT",
            [],
        )
        .map_err(|e| {
            format!(
                "MIGRATE_CODEX_STOP_ATTEMPT_FAIL: 老库补列 codex_actions.stop_attempt_reason 失败: {e}"
            )
        })?;
    }
    // C02（R05 残留收口）：停止尝试的持久占有状态列——占有在首次可能写出 interrupt
    // 之前 CAS 落库（'claimed'），覆盖「writer 已写出帧后 handler 被取消/服务退出」
    // 的事后记录盲窗；首写前明确零投递才释放（'released'，允许显式新尝试）。
    // 老库行恒 NULL：读侧兼容识别——NULL + stop_attempt_reason 非 NULL = C02 前已
    // 记录的未知结果（重放同样不再发停止帧），NULL + NULL = 未占有（行为不变）。
    if !column_exists(conn, "codex_actions", "stop_attempt_state") {
        conn.execute(
            "ALTER TABLE codex_actions ADD COLUMN stop_attempt_state TEXT",
            [],
        )
        .map_err(|e| {
            format!(
                "MIGRATE_CODEX_STOP_ATTEMPT_STATE_FAIL: 老库补列 codex_actions.stop_attempt_state 失败: {e}"
            )
        })?;
    }
    // R06：越队授权补尝试代际列（旧行 = attempt 1，行为不变）。优先处理路径在
    // 「上次明确零投递回队、动作仍 queued」时把已消费授权重铸为下一 attempt
    //（下一次用户授权独立可 CAS）；老库行恒可被重铸，语义与旧行为兼容。
    if !column_exists(conn, "codex_bypass_grant", "attempt") {
        conn.execute(
            "ALTER TABLE codex_bypass_grant ADD COLUMN attempt INTEGER NOT NULL DEFAULT 1",
            [],
        )
        .map_err(|e| {
            format!(
                "MIGRATE_CODEX_BYPASS_ATTEMPT_FAIL: 老库补列 codex_bypass_grant.attempt 失败: {e}"
            )
        })?;
    }
    // R17：契约证据缓存必须能核对**当前包内容**（同 build/CLI 但包被替换时，旧结论
    // 不得安装）。新增两列记录取证时的包文件大小与 mtime；老行 NULL = 未记录 →
    // 读侧按"未能核对"处理（重新识别，不猜）。
    for (column, ddl) in [
        (
            "package_size",
            "ALTER TABLE codex_ipc_contract_evidence ADD COLUMN package_size INTEGER",
        ),
        (
            "package_mtime_ms",
            "ALTER TABLE codex_ipc_contract_evidence ADD COLUMN package_mtime_ms INTEGER",
        ),
    ] {
        if !column_exists(conn, "codex_ipc_contract_evidence", column) {
            conn.execute(ddl, []).map_err(|e| {
                format!("MIGRATE_CONTRACT_EVIDENCE_PKG_FAIL: 老库补契约证据包指纹列失败: {e}")
            })?;
        }
    }
    if !column_exists(conn, "codex_ipc_probe", "evidence_at") {
        conn.execute(
            "ALTER TABLE codex_ipc_probe ADD COLUMN evidence_at INTEGER",
            [],
        )
        .map_err(|e| format!("MIGRATE_FAIL: 迁移失败: {}", e))?;
    }
    if !column_exists(conn, "codex_ipc_probe", "evidence_stage") {
        conn.execute(
            "ALTER TABLE codex_ipc_probe ADD COLUMN evidence_stage TEXT",
            [],
        )
        .map_err(|e| format!("MIGRATE_FAIL: 迁移失败: {}", e))?;
    }
    // 复审 R-T1：`codex_write_revoke` 升级为**两态行**（revoked_at 可空 + 新增
    // claimed_at 列）——旧表 `revoked_at NOT NULL` 且无 claimed_at，SQLite 无法
    // ALTER 列约束，只能整表重建。旧行全部是撤销行（revoked_at 原样保留，语义
    // 不变）；重建幂等（新形状已有 claimed_at → 跳过）。无该表（全新库）时由
    // schema::init 直接建新形状，本块跳过。
    if !column_exists(conn, "codex_write_revoke", "claimed_at")
        && conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='codex_write_revoke'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0
    {
        conn.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE codex_write_revoke_migrated (
                 action_id  TEXT PRIMARY KEY,
                 revoked_at INTEGER,
                 reason     TEXT,
                 claimed_at INTEGER
             );
             INSERT INTO codex_write_revoke_migrated (action_id, revoked_at, reason)
                 SELECT action_id, revoked_at, reason FROM codex_write_revoke;
             DROP TABLE codex_write_revoke;
             ALTER TABLE codex_write_revoke_migrated RENAME TO codex_write_revoke;
             COMMIT;",
        )
        .map_err(|e| {
            format!("MIGRATE_WRITE_REVOKE_TWO_STATE_FAIL: codex_write_revoke 两态行重建失败: {e}")
        })?;
    }
    // 复审 R-T2：`codex_queue_pause` 升级为**按 token 多行共存**（复合主键
    // thread_id+pause_token）——旧表 `thread_id` 单列主键只容一行，第二个打断的
    // 前置保护落不进（ON CONFLICT DO NOTHING），首个打断失败撤回后后受理打断
    // 的暂停保护随之丢失（R-T2 反向定序）。SQLite 无法改主键，整表重建；旧行
    // 原样保留（NULL token 行与 token 行共存，NULL 在复合主键中互异）。幂等：
    // 新形状（pause_token 在主键中）跳过；全新库由 schema::init 直接建新形状。
    // 顺序：必须在上方 pause_token 补列之后（最老库先补列再重建）。
    {
        let composite_pk = conn
            .prepare("PRAGMA table_info(codex_queue_pause)")
            .ok()
            .map(|mut stmt| {
                stmt.query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(5)?)))
                    .ok()
                    .map(|rows| {
                        rows.filter_map(Result::ok)
                            .any(|(name, pk)| name == "pause_token" && pk > 0)
                    })
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        let table_present = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='codex_queue_pause'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if table_present && !composite_pk {
            conn.execute_batch(
                "BEGIN IMMEDIATE;
                 CREATE TABLE codex_queue_pause_migrated (
                     thread_id    TEXT NOT NULL,
                     paused_at    INTEGER NOT NULL,
                     pause_reason TEXT NOT NULL,
                     pause_token  TEXT,
                     pause_turn_id TEXT,
                     PRIMARY KEY (thread_id, pause_token)
                 );
                 INSERT INTO codex_queue_pause_migrated (thread_id, paused_at, pause_reason, pause_token, pause_turn_id)
                     SELECT thread_id, paused_at, pause_reason, pause_token, pause_turn_id FROM codex_queue_pause;
                 DROP TABLE codex_queue_pause;
                 ALTER TABLE codex_queue_pause_migrated RENAME TO codex_queue_pause;
                 COMMIT;",
            )
            .map_err(|e| {
                format!("MIGRATE_QUEUE_PAUSE_MULTI_TOKEN_FAIL: codex_queue_pause 多行共存重建失败: {e}")
            })?;
        }
    }
    // 终审 F1 + Gate C 裁决（方向 b）+ 复审 R1（一次性化）：清除 bug 时代的假
    // 成功证据——**只跑一次**，持久完成标记与清理**同一事务原子执行**。
    // 背景：S1 的 INSERT 臂曾无条件把 now/stage 写进证据列——首次 failed/inconclusive
    // 的行带上「从未成功」的证据（复用门与 DTO 据此误报已取证、跳过重试）。bug 写出
    // 的假证据经 ON CONFLICT 的 COALESCE 保留臂**跨重试/跨重启累积**（attempts 推到
    // ≥2），行级与真「成功后失败」不可判别——裁决：清除全部 failed|inconclusive 行
    // 的证据列（表未发版，代价 = 少数真「成功后失败」行重探一次，可接受）。
    // R1（P1）：无版本的 UPDATE 会在**每次启动**重复执行——历史修复完成后，新产生
    // 的真「成功→暂时失败」证据也会被清，直接违反成功跨重启保留。修法：专用完成
    // 标记表（`codex_ipc_probe_fake_evidence_purge_done`）为持久凭据——标记表缺席时
    // 进入本块：清理 UPDATE、建标记表、落标记行在**同一事务**内提交（原子；无待清
    // 行也落标记）；标记在位则永久跳过（后续新增的真证据绝不复发被清）。
    // 不触其他表。
    {
        let already_done = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' \
                 AND name='codex_ipc_probe_fake_evidence_purge_done'",
                [],
                |_| Ok(()),
            )
            .is_ok();
        if !already_done {
            let tx = conn
                .unchecked_transaction()
                .map_err(|e| format!("MIGRATE_FAIL: 清理事务开启失败: {}", e))?;
            let cleaned = tx
                .execute(
                    "UPDATE codex_ipc_probe
                         SET evidence_at = NULL, evidence_stage = NULL
                     WHERE state IN ('failed', 'inconclusive')
                       AND evidence_at IS NOT NULL",
                    [],
                )
                .map_err(|e| format!("MIGRATE_FAIL: 清除假证据失败: {}", e))?;
            tx.execute(
                "CREATE TABLE codex_ipc_probe_fake_evidence_purge_done (
                     done_at INTEGER NOT NULL
                 )",
                [],
            )
            .map_err(|e| format!("MIGRATE_FAIL: 建清理标记表失败: {}", e))?;
            tx.execute(
                "INSERT INTO codex_ipc_probe_fake_evidence_purge_done (done_at) \
                 VALUES (strftime('%s','now') * 1000)",
                [],
            )
            .map_err(|e| format!("MIGRATE_FAIL: 落清理标记失败: {}", e))?;
            tx.commit()
                .map_err(|e| format!("MIGRATE_FAIL: 清理事务提交失败: {}", e))?;
            if cleaned > 0 {
                log::warn!(
                    "codex_ipc: 已一次性清除 {cleaned} 行 bug 时代的假成功证据（表未发版；\
                     含真「成功后失败」行的代价重探；完成标记已落，此后不再执行）"
                );
            }
        }
    }

    // 预设 v2（spec §4）：presets 补描述/类型/绑定工具三列（新表由 schema.rs IF NOT EXISTS 覆盖）
    for (col, ddl) in [
        (
            "description",
            "ALTER TABLE presets ADD COLUMN description TEXT NOT NULL DEFAULT ''",
        ),
        (
            "scope",
            "ALTER TABLE presets ADD COLUMN scope TEXT NOT NULL DEFAULT 'universal'",
        ),
        (
            "bound_tool",
            "ALTER TABLE presets ADD COLUMN bound_tool TEXT",
        ),
    ] {
        if !column_exists(conn, "presets", col) {
            conn.execute(ddl, []).map_err(|e| {
                format!("MIGRATE_PRESET_COL_FAIL: 迁移 presets.{} 失败: {}", col, e)
            })?;
        }
    }

    // 根据旧表记录回填来源工具
    conn.execute_batch(
        "UPDATE extensions SET source_tool = 'claude'
            WHERE kind = 'skill' AND source_tool IS NULL
              AND (source_path LIKE '%/.claude/skills/%' OR tags = 'claude');
         UPDATE extensions SET source_tool = 'codex'
            WHERE kind = 'skill' AND source_tool IS NULL
              AND (source_path LIKE '%/.codex/skills/%'
                   OR source_path LIKE '%/.agents/skills/%'
                   OR tags = 'codex');
         UPDATE extensions SET source_tool = 'opencode'
            WHERE kind = 'skill' AND source_tool IS NULL
              AND (source_path LIKE '%/.config/opencode/skills/%' OR tags = 'opencode');
         UPDATE extensions SET source_tool = 'openclaw'
            WHERE kind = 'skill' AND source_tool IS NULL
              AND (source_path LIKE '%/.openclaw/skills/%' OR tags = 'openclaw');",
    )
    .map_err(|e| format!("MIGRATE_SOURCE_TOOL_FAIL: 回填来源工具失败: {}", e))?;

    // 015：native_extensions 表从未被业务写入，移除（历史库中 DROP）
    conn.execute_batch("DROP TABLE IF EXISTS native_extensions;")
        .map_err(|e| {
            format!(
                "MIGRATE_DROP_NATIVE_EXT_FAIL: 移除 native_extensions 失败: {}",
                e
            )
        })?;

    // M2（远程接入）：已配对设备表（cookie deviceId 跨重启持久）
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS remote_devices (
             id TEXT PRIMARY KEY,
             name TEXT NOT NULL DEFAULT '',
             ua TEXT NOT NULL DEFAULT '',
             origin_ip TEXT NOT NULL DEFAULT '',
             fingerprint TEXT NOT NULL DEFAULT '',
             via TEXT NOT NULL DEFAULT '',
             first_paired_at INTEGER NOT NULL DEFAULT 0,
             last_seen_at INTEGER NOT NULL DEFAULT 0,
             revoked INTEGER NOT NULL DEFAULT 0
         );",
    )
    .map_err(|e| format!("MIGRATE_REMOTE_DEVICES_FAIL: 建 remote_devices 失败: {}", e))?;

    // M5 A1：设备指纹（sha256(UA|"|"|origin_ip) hex 小写，同浏览器重绑 upsert 的去重键）
    // 与接入通道（ASCII 枚举 "local"|"lan"|"quick"|"named"）两列——
    // 存量库走 ALTER（新库已由上方建表语句直建，column_exists 短路）。
    // SQLite 加 NOT NULL 列必须带 DEFAULT。存量行取 DEFAULT 空串：指纹算法在 Rust 侧
    // 无法在 SQL 内回填，空串也永不与真实指纹撞键（只影响首次重连多一行，可接受）
    if !column_exists(conn, "remote_devices", "fingerprint") {
        conn.execute(
            "ALTER TABLE remote_devices ADD COLUMN fingerprint TEXT NOT NULL DEFAULT ''",
            [],
        )
        .map_err(|e| format!("MIGRATE_DEVICE_FINGERPRINT_FAIL: 迁移设备指纹列失败: {}", e))?;
    }
    if !column_exists(conn, "remote_devices", "via") {
        conn.execute(
            "ALTER TABLE remote_devices ADD COLUMN via TEXT NOT NULL DEFAULT ''",
            [],
        )
        .map_err(|e| format!("MIGRATE_DEVICE_VIA_FAIL: 迁移设备 via 列失败: {}", e))?;
    }

    // ---- 用量账本（计划①）②：其余列逐列补齐 ----
    // 新库已由 schema.rs 直建，column_exists 全部短路。**幂等**是硬要求（重复调用不得重复加列）。
    // 注：usage_detail 的 day_key 那条已被上面的重建分支覆盖（且它是索引引用列，缺失时 init 会先
    // panic → 实际不可达，仅作纵深防御保留）；tool_stats 不进任何索引，是本循环里唯一可达的
    // usage_detail 补列路径。
    for (table, col, ddl) in [
        (
            "usage_detail",
            "day_key",
            "ALTER TABLE usage_detail ADD COLUMN day_key TEXT NOT NULL DEFAULT ''",
        ),
        (
            "usage_detail",
            "tool_stats",
            "ALTER TABLE usage_detail ADD COLUMN tool_stats TEXT NOT NULL DEFAULT ''",
        ),
        (
            "usage_daily",
            "user_est",
            // 可空、无默认（R1）：老库补列后该列全为 NULL = 「不可得」，与语义一致
            "ALTER TABLE usage_daily ADD COLUMN user_est INTEGER",
        ),
        (
            "usage_cursor",
            "mtime_ms",
            "ALTER TABLE usage_cursor ADD COLUMN mtime_ms INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "usage_cursor",
            "file_size",
            "ALTER TABLE usage_cursor ADD COLUMN file_size INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "usage_cursor",
            "state_json",
            "ALTER TABLE usage_cursor ADD COLUMN state_json TEXT NOT NULL DEFAULT ''",
        ),
        (
            "usage_session",
            "originator",
            "ALTER TABLE usage_session ADD COLUMN originator TEXT",
        ),
        (
            "usage_session",
            "project_path_raw",
            "ALTER TABLE usage_session ADD COLUMN project_path_raw TEXT NOT NULL DEFAULT ''",
        ),
        (
            "usage_session",
            "project_realpath",
            "ALTER TABLE usage_session ADD COLUMN project_realpath TEXT NOT NULL DEFAULT ''",
        ),
    ] {
        if !column_exists(conn, table, col) {
            conn.execute(ddl, []).map_err(|e| {
                format!("MIGRATE_TABLE_COL_FAIL: 迁移 {}.{} 失败: {}", table, col, e)
            })?;
        }
    }
    // Codex 动作账本：人工解除屏障标记列——additive 幂等迁移；可空、无默认
    // （存量行全 NULL = 从未人工解除，无回填需求）。
    if !column_exists(conn, "codex_actions", "barrier_force_released_at") {
        conn.execute(
            "ALTER TABLE codex_actions ADD COLUMN barrier_force_released_at INTEGER",
            [],
        )
        .map_err(|e| {
            format!(
                "MIGRATE_CODEX_BARRIER_RELEASE_FAIL: 迁移 codex_actions.barrier_force_released_at 失败: {}",
                e
            )
        })?;
    }
    // 签名开关冻结值（2026-10-20 复审）：登记时固定该动作的签名形态，排队期间
    // 改设置/重启/同 actionId 重试都不改变既定正文。旧行补列后保持 NULL = 升级前
    // 语义（恒带签名），不误改已完成动作。
    if !column_exists(conn, "codex_actions", "effect_probe_key") {
        conn.execute_batch(
            "ALTER TABLE codex_actions ADD COLUMN effect_probe_key TEXT;
             ALTER TABLE codex_actions ADD COLUMN effect_package_sha256 TEXT;
             ALTER TABLE codex_actions ADD COLUMN effect_rule_version INTEGER;",
        )
        .map_err(|e| format!("MIGRATE_EFFECT_TARGET_FAIL: 迁移效果归属列失败: {e}"))?;
    }
    if !column_exists(conn, "codex_actions", "signature_applied") {
        conn.execute(
            "ALTER TABLE codex_actions ADD COLUMN signature_applied INTEGER",
            [],
        )
        .map_err(|e| {
            format!(
                "MIGRATE_SIGNATURE_APPLIED_FAIL: 迁移 codex_actions.signature_applied 失败: {}",
                e
            )
        })?;
    }

    // 老库 day_key 空串回填（派生列口径：strftime 不可用于 'YYYY-MM-DDTHH' 形态，用 substr）
    conn.execute(
        "UPDATE usage_detail SET day_key = substr(hour_key, 1, 10) WHERE day_key = ''",
        [],
    )
    .map_err(|e| {
        format!(
            "MIGRATE_USAGE_DAYKEY_FAIL: 回填 usage_detail.day_key 失败: {}",
            e
        )
    })?;

    // 手动覆盖价币种（2026-10-20）：存量行一律 USD（旧语义），新行由 UI 选择 USD/CNY。
    if !column_exists(conn, "usage_price_override", "currency") {
        conn.execute(
            "ALTER TABLE usage_price_override ADD COLUMN currency TEXT NOT NULL DEFAULT 'USD' CHECK(currency IN ('USD','CNY'))",
            [],
        )
        .map_err(|e| format!("MIGRATE_OVERRIDE_CURRENCY_FAIL: 迁移覆盖价币种失败: {}", e))?;
    }

    // 分时价格（2026-10-21）：存量规则一律固定价（time_enabled=0），分时列补空。
    for (column, ddl) in [
        (
            "time_enabled",
            "ALTER TABLE usage_price_override ADD COLUMN time_enabled INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "time_timezone",
            "ALTER TABLE usage_price_override ADD COLUMN time_timezone TEXT NOT NULL DEFAULT 'asiaShanghai'",
        ),
        (
            "time_windows",
            "ALTER TABLE usage_price_override ADD COLUMN time_windows TEXT",
        ),
        (
            "offpeak_input_scaled",
            "ALTER TABLE usage_price_override ADD COLUMN offpeak_input_scaled INTEGER",
        ),
        (
            "offpeak_cache_read_scaled",
            "ALTER TABLE usage_price_override ADD COLUMN offpeak_cache_read_scaled INTEGER",
        ),
        (
            "offpeak_cache_write_scaled",
            "ALTER TABLE usage_price_override ADD COLUMN offpeak_cache_write_scaled INTEGER",
        ),
        (
            "offpeak_output_scaled",
            "ALTER TABLE usage_price_override ADD COLUMN offpeak_output_scaled INTEGER",
        ),
    ] {
        if !column_exists(conn, "usage_price_override", column) {
            conn.execute(ddl, []).map_err(|e| {
                format!(
                    "MIGRATE_OVERRIDE_TIME_FAIL: 迁移 usage_price_override.{column} 失败: {}",
                    e
                )
            })?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_adds_skill_source_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE extensions (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                name TEXT NOT NULL,
                description TEXT,
                source_path TEXT NOT NULL,
                source_url TEXT,
                version TEXT,
                tags TEXT,
                installed_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );",
        )
        .unwrap();
        migrate(&conn).unwrap();
        assert!(conn
            .prepare("SELECT source_tool FROM extensions LIMIT 0")
            .is_ok());
        assert!(conn
            .prepare("SELECT is_native FROM extensions LIMIT 0")
            .is_ok());
    }

    #[test]
    fn migrate_backfills_source_tool_from_path() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE extensions (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                name TEXT NOT NULL,
                description TEXT,
                source_path TEXT NOT NULL,
                source_url TEXT,
                version TEXT,
                tags TEXT,
                installed_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            INSERT INTO extensions (id, kind, name, source_path, tags, installed_at, updated_at)
            VALUES ('skill-old', 'skill', 'old', '/Users/test/.agents/skills/old', 'codex', 'now', 'now');"
        ).unwrap();

        migrate(&conn).unwrap();

        let source_tool: Option<String> = conn
            .query_row(
                "SELECT source_tool FROM extensions WHERE id = 'skill-old'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(source_tool.as_deref(), Some("codex"));
    }

    /// M5 A1：旧结构 remote_devices（M4，无 fingerprint/via）经迁移后两列存在，
    /// 存量行取 DEFAULT 空串（无法回填指纹——算法在 Rust 侧，且空串永不与真实指纹撞键）
    #[test]
    fn migrate_adds_remote_device_fingerprint_and_via_columns() {
        let conn = Connection::open_in_memory().unwrap();
        // migrate 首段是 ALTER TABLE extensions——沿用本文件既有测试做法先建 extensions
        conn.execute_batch(
            "CREATE TABLE extensions (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                name TEXT NOT NULL,
                description TEXT,
                source_path TEXT NOT NULL,
                source_url TEXT,
                version TEXT,
                tags TEXT,
                installed_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );",
        )
        .unwrap();
        // 旧结构表：M4 原始形状（故意不含两新列），并预置一行存量数据
        conn.execute_batch(
            "CREATE TABLE remote_devices (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL DEFAULT '',
                ua TEXT NOT NULL DEFAULT '',
                origin_ip TEXT NOT NULL DEFAULT '',
                first_paired_at INTEGER NOT NULL DEFAULT 0,
                last_seen_at INTEGER NOT NULL DEFAULT 0,
                revoked INTEGER NOT NULL DEFAULT 0
            );
            INSERT INTO remote_devices (id, name) VALUES ('legacy', '旧设备');",
        )
        .unwrap();
        migrate(&conn).unwrap();
        assert!(conn
            .prepare("SELECT fingerprint FROM remote_devices LIMIT 0")
            .is_ok());
        assert!(conn
            .prepare("SELECT via FROM remote_devices LIMIT 0")
            .is_ok());
        // 幂等：迁移两遍不报错（column_exists 短路 ALTER）
        migrate(&conn).unwrap();
        // 存量行新列为 DEFAULT 空串
        let (fp, via): (String, String) = conn
            .query_row(
                "SELECT fingerprint, via FROM remote_devices WHERE id = 'legacy'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((fp.as_str(), via.as_str()), ("", ""));
    }

    /// M5 A1：全新库（schema::init → migrate）建出的 remote_devices 直含两列，
    /// 无需走 ALTER 路径
    #[test]
    fn fresh_remote_devices_table_includes_fingerprint_and_via() {
        let conn = Connection::open_in_memory().unwrap();
        // 真机调用序：schema::init 建全部表 → migration::migrate 增量迁移
        crate::database::schema::init(&conn);
        migrate(&conn).unwrap();
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(remote_devices)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            cols.contains(&"fingerprint".to_string()),
            "缺 fingerprint 列: {cols:?}"
        );
        assert!(cols.contains(&"via".to_string()), "缺 via 列: {cols:?}");
    }

    #[test]
    fn migrate_drops_native_extensions() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE extensions (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                name TEXT NOT NULL,
                description TEXT,
                source_path TEXT NOT NULL,
                source_url TEXT,
                version TEXT,
                tags TEXT,
                installed_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE native_extensions (id TEXT PRIMARY KEY);",
        )
        .unwrap();
        migrate(&conn).unwrap();
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='native_extensions'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!exists);
    }

    /// 预设 v2（spec §4）：老库迁移补 presets 3 列 + 5 张新表
    #[test]
    fn migrate_adds_preset_v2_columns_and_tables() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE extensions (
                id TEXT PRIMARY KEY, kind TEXT NOT NULL, name TEXT NOT NULL,
                description TEXT, source_path TEXT NOT NULL, source_url TEXT,
                version TEXT, tags TEXT, installed_at TEXT NOT NULL, updated_at TEXT NOT NULL
            );
            CREATE TABLE presets (
                id TEXT PRIMARY KEY, name TEXT NOT NULL, created_at TEXT NOT NULL
            );
            CREATE TABLE preset_items (
                id TEXT PRIMARY KEY, preset_id TEXT NOT NULL,
                extension_id TEXT NOT NULL, kind TEXT NOT NULL
            );",
        )
        .unwrap();

        migrate(&conn).unwrap();

        for col in ["description", "scope", "bound_tool"] {
            assert!(
                conn.prepare(&format!("SELECT {} FROM presets LIMIT 0", col))
                    .is_ok(),
                "presets 缺列 {}",
                col
            );
        }
        for table in [
            "resource_bindings",
            "tool_residents",
            "tool_base_snapshots",
            "tool_base_snapshot_items",
            "stash_journal",
        ] {
            let exists: bool = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='{}'",
                        table
                    ),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(exists, "缺表 {}", table);
        }
    }

    /// 列是否 NOT NULL（形状断言用）：表或列不存在 → panic（不静默当成 false）
    fn notnull(conn: &Connection, table: &str, col: &str) -> bool {
        conn.prepare(&format!("PRAGMA table_info({})", table))
            .unwrap()
            .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(3)?)))
            .unwrap()
            .filter_map(Result::ok)
            .find(|(name, _)| name == col)
            .map(|(_, nn)| nn != 0)
            .unwrap_or_else(|| panic!("{table} 缺列 {col}"))
    }

    /// 用量账本 4 张表：全新库（schema::init → migrate）建齐全部列，且 migrate 幂等
    #[test]
    fn usage_tables_created_with_expected_columns() {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        migrate(&conn).unwrap();
        migrate(&conn).unwrap(); // 幂等：跑两遍不报错

        let cols = |table: &str| -> Vec<String> {
            conn.prepare(&format!("PRAGMA table_info({})", table))
                .unwrap()
                .query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        for t in [
            "usage_detail",
            "usage_daily",
            "usage_cursor",
            "usage_session",
        ] {
            // 注意：任务书此处写的是 assert_eq!(…, true)；clippy 的 bool_assert_comparison 在
            // 本任务门禁 `--all-targets -- -D warnings` 下会直接报 error（实测 migration.rs:486），
            // 故等价改写为 assert!（期望值未变：仍要求该查询为 true）
            assert!(
                conn.query_row(
                    "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name=?1",
                    [t],
                    |r| r.get::<_, bool>(0)
                )
                .unwrap(),
                "缺表 {t}"
            );
        }
        // 明细表：**project_key 为第三列（记录级，进主键，D21 规则④）**、小时桶第四列，
        // 列序与主键列序同契约 §4；day_key 为派生冗余列（见计划 Global Constraints 15/18-R2）
        assert_eq!(
            cols("usage_detail"),
            vec![
                "source_id",
                "session_id",
                "project_key",
                "hour_key",
                "day_key",
                "model",
                "provider",
                "provider_kind",
                "input_fresh",
                "cache_read",
                "cache_write",
                "output",
                "request_total",
                "requests",
                "user_est",
                "cache_semantics",
                "turns",
                "error_model",
                "error_turn",
                "error_tool",
                "interrupted",
                "tool_calls",
                "tool_ms",
                "turn_ms",
                "tool_stats",
                "first_seen_at",
                "updated_at"
            ]
        );
        // R1：`user_est` 在**明细表与日聚合表**都必须可空（拿不到就是 NULL，绝不 NOT NULL DEFAULT 0）
        assert!(
            !notnull(&conn, "usage_detail", "user_est"),
            "R1：明细 user_est 必须可空"
        );
        // 日聚合表：永久保留，按（日 × 源 × 项目 × 供应商 × 模型）折叠
        assert!(cols("usage_daily").contains(&"project_key".to_string()));
        assert!(cols("usage_daily").contains(&"request_total".to_string()));
        assert!(
            cols("usage_daily").contains(&"provider_kind".to_string()),
            "三态标记随日聚合保留"
        );
        assert!(
            cols("usage_daily").contains(&"user_est".to_string()),
            "R1：日聚合必须有 user_est 列"
        );
        assert!(
            !notnull(&conn, "usage_daily", "user_est"),
            "R1：日聚合 user_est 必须可空"
        );
        // 日聚合**刻意不带**的源能力相关列（逐条说明，见 Step 3 的 DDL 注释）：
        // provider_kind 已在（三态）；计数类（turns/error_*/interrupted/tool_*/turn_ms）与
        // cache_semantics 不入日聚合——它们的口径是「只在明细保留期内可得」（遗留点 10 已登记取舍）
        for c in [
            "turns",
            "error_model",
            "tool_calls",
            "cache_semantics",
            "turn_ms",
        ] {
            assert!(
                !cols("usage_daily").contains(&c.to_string()),
                "usage_daily 不应有 {c}（口径见 DDL 注释）"
            );
        }
        // 游标表：字节双水位 + 尾指纹 + 文件代际（mtime/size，未变即跳过读取）+ 续读状态
        for c in [
            "fingerprint",
            "byte_offset",
            "ordinal",
            "last_cumulative",
            "mtime_ms",
            "file_size",
            "state_json",
        ] {
            assert!(
                cols("usage_cursor").contains(&c.to_string()),
                "usage_cursor 缺列 {c}"
            );
        }
        // 会话维度表：D17 分层所需字段 + D21 项目四列 + Codex originator（§5.2 CLI/APP 拆分留档）
        for c in [
            "project_key",
            "project_label",
            "project_path_raw",
            "project_realpath",
            "title",
            "is_subagent",
            "parent_session_id",
            "originator",
        ] {
            assert!(
                cols("usage_session").contains(&c.to_string()),
                "usage_session 缺列 {c}"
            );
        }
    }

    /// 明细表唯一键：同一 (源,会话,小时,**项目**,模型,供应商) 只允许一行；不同小时 / 不同项目各行独立
    #[test]
    fn usage_detail_unique_key_is_hourly() {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        let ins = |hour: &str, project: &str| {
            conn.execute(
                "INSERT INTO usage_detail
                 (source_id, session_id, hour_key, project_key, day_key, model, provider,
                  input_fresh, cache_read, cache_write, output, request_total, requests, user_est,
                  cache_semantics, first_seen_at, updated_at)
                 VALUES ('claude','s1',?1,?2,substr(?1,1,10),'m','p',1,0,0,1,2,1,NULL,
                         'exclusive',0,0)",
                rusqlite::params![hour, project],
            )
        };
        assert!(ins("2026-10-03T09", "a").is_ok());
        assert!(ins("2026-10-03T10", "a").is_ok(), "不同小时桶必须各行独立");
        assert!(
            ins("2026-10-03T09", "a").is_err(),
            "同一小时桶重复插入必须撞主键"
        );
        // R2：同一会话同一小时里的**两个 cwd** 必须是两行（否则记录级归属会被折叠掉）
        assert!(
            ins("2026-10-03T09", "b").is_ok(),
            "同一会话同一小时的两个项目必须各行独立"
        );
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage_detail", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 3);
        // R1：无值写入就是 NULL，不是 0
        let nulls: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage_detail WHERE user_est IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(nulls, 3, "拿不到 userEst 时必须落 NULL");
    }

    /// R1/R2 迁移：预览版明细表（无 project_key、user_est NOT NULL DEFAULT 0）→ **整表重建**成新形状，
    /// 且四张表数据一并清空（否则「明细被删但游标已推进」会留永久缺口、日聚合会被重采记录二次累加）
    #[test]
    fn migrate_rebuilds_preview_ledger_shape() {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        // 造一个预览版形状（旧 DDL：无 project_key、user_est NOT NULL DEFAULT 0），并塞入残留数据
        conn.execute_batch(
            "DROP TABLE usage_detail;
             CREATE TABLE usage_detail (
                 source_id TEXT NOT NULL, session_id TEXT NOT NULL, hour_key TEXT NOT NULL,
                 day_key TEXT NOT NULL, model TEXT NOT NULL DEFAULT '',
                 provider TEXT NOT NULL DEFAULT '', user_est INTEGER NOT NULL DEFAULT 0,
                 updated_at INTEGER NOT NULL,
                 PRIMARY KEY (source_id, session_id, hour_key, model, provider));
             INSERT INTO usage_detail VALUES
                 ('claude','s1','2026-10-03T09','2026-10-03','m','p',0,1);
             INSERT INTO usage_daily (day_key, source_id, project_key, provider, model, updated_at)
                 VALUES ('2026-10-03','claude','a','p','m',1);
             INSERT INTO usage_cursor (source_id, session_id, updated_at) VALUES ('claude','f1',1);
             INSERT INTO usage_session (source_id, session_id, first_seen_at, last_seen_at)
                 VALUES ('claude','s1',1,1);",
        )
        .unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap(); // 幂等：第二次不再重建（形状已合规）
                                 // R2：project_key 已进主键（同会话同小时两个项目可并存）
        conn.execute(
            "INSERT INTO usage_detail
             (source_id, session_id, hour_key, project_key, day_key, model, provider,
              first_seen_at, updated_at)
             VALUES ('claude','s1','2026-10-03T09','a','2026-10-03','m','p',0,0),
                    ('claude','s1','2026-10-03T09','b','2026-10-03','m','p',0,0)",
            [],
        )
        .unwrap();
        assert!(
            !notnull(&conn, "usage_detail", "user_est"),
            "R1：重建后 user_est 必须可空"
        );
        assert!(
            !notnull(&conn, "usage_daily", "user_est"),
            "R1：日聚合 user_est 必须可空"
        );
        // 四表清空（重建的语义完整性，不只清明细）
        for t in [
            "usage_detail",
            "usage_daily",
            "usage_cursor",
            "usage_session",
        ] {
            // 明细已被上面两条新插入占位 → 单独判
            if t == "usage_detail" {
                let n: i64 = conn
                    .query_row("SELECT COUNT(*) FROM usage_detail", [], |r| r.get(0))
                    .unwrap();
                assert_eq!(n, 2, "重建后只应有测试新插入的两行");
                continue;
            }
            let n: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {}", t), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0, "预览版残留必须清空: {t}");
        }
    }

    /// 老库补列路径（形状已是新主键、但缺若干列）：migrate 逐列补齐、**不丢已有数据**、重复调用幂等。
    /// 任务书的两条用例分别锁「全新库」与「预览版整表重建」；本用例补第三条路径——上面 9 条 ALTER
    /// 若不在此走到，语句写错也没有任何用例会发现（全新库全部短路、预览版整表 DROP 后由 init 重建）。
    /// 私有内存库，不碰 tests/support.rs 的共享库（GC 19）。
    #[test]
    fn migrate_adds_missing_ledger_columns_without_data_loss() {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        // 造「新形状但缺列」的老库：SQLite 3.35+ 支持 DROP COLUMN，但**被索引引用的列删不掉**
        // （day_key / project_key 都进了索引），故这里只删不进任何索引的列
        conn.execute_batch(
            "ALTER TABLE usage_detail DROP COLUMN tool_stats;
             ALTER TABLE usage_daily DROP COLUMN user_est;
             ALTER TABLE usage_cursor DROP COLUMN mtime_ms;
             ALTER TABLE usage_cursor DROP COLUMN file_size;
             ALTER TABLE usage_cursor DROP COLUMN state_json;
             ALTER TABLE usage_session DROP COLUMN originator;
             ALTER TABLE usage_session DROP COLUMN project_path_raw;
             ALTER TABLE usage_session DROP COLUMN project_realpath;
             INSERT INTO usage_detail
                 (source_id, session_id, project_key, hour_key, day_key, model, provider,
                  input_fresh, user_est, first_seen_at, updated_at)
                 VALUES ('claude','s1','a','2026-10-03T09','2026-10-03','m','p',7,42,0,0);
             INSERT INTO usage_cursor (source_id, session_id, updated_at) VALUES ('claude','f1',0);
             INSERT INTO usage_session (source_id, session_id, first_seen_at, last_seen_at)
                 VALUES ('claude','s1',0,0);",
        )
        .unwrap();
        // 前提断言（防止假绿）：这些列在迁移前**确实**不在，否则后面的「已补齐」断言什么都没锁住
        for (t, c) in [
            ("usage_detail", "tool_stats"),
            ("usage_daily", "user_est"),
            ("usage_cursor", "state_json"),
            ("usage_session", "originator"),
        ] {
            assert!(
                !column_exists(&conn, t, c),
                "前提不成立：{t}.{c} 本应已被删除"
            );
        }
        migrate(&conn).unwrap();
        // 幂等：再跑一遍（补过的列不再 ALTER——重复 ADD COLUMN 会报 duplicate column name）
        migrate(&conn).unwrap();
        assert!(
            notnull(&conn, "usage_detail", "tool_stats"),
            "补列后 tool_stats 应存在"
        );
        // R1：补列路径也必须让 user_est 可空（ADD COLUMN 时不给 DEFAULT，老行落 NULL = 不可得）
        assert!(
            !notnull(&conn, "usage_daily", "user_est"),
            "R1：补列后日聚合 user_est 必须仍可空"
        );
        // notnull() 在表/列缺失时 panic → 这几条同时锁「列已补齐」
        for (t, c) in [
            ("usage_cursor", "mtime_ms"),
            ("usage_cursor", "file_size"),
            ("usage_cursor", "state_json"),
            ("usage_session", "originator"),
            ("usage_session", "project_path_raw"),
            ("usage_session", "project_realpath"),
        ] {
            let _ = notnull(&conn, t, c);
        }
        // 与「预览版重建」路径的分野：补列路径**不得**清掉存量数据
        let (fresh, est): (i64, Option<i64>) = conn
            .query_row(
                "SELECT input_fresh, user_est FROM usage_detail
                 WHERE source_id='claude' AND session_id='s1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((fresh, est), (7, Some(42)), "补列不得丢存量明细行");
        let cursors: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage_cursor", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            cursors, 1,
            "补列不得清空游标（清了会重采，与重建路径的取舍不同）"
        );
    }

    /// Task 3/8 + V2 Task 2/3 + 2026-10-20 计划 Task 2：**当前形状**的旧账本（尚未有
    /// billing/pricing/provider-alias/price-override/model-config 七张新表）连续迁移两次，
    /// 旧四表逐行 / 桶 / 游标逐值不变，且新表被幂等补齐。
    ///
    /// 该形状不满足 `preview_detail` 判定（有 hour_key/project_key 且 user_est 可空），
    /// 因此**不得**触发预览版重建分支（那会清空四表）。
    #[test]
    fn migrate_twice_preserves_current_ledger_and_creates_v2_tables() {
        use rusqlite::types::ValueRef;

        fn render(value: ValueRef<'_>) -> String {
            match value {
                ValueRef::Null => "NULL".to_string(),
                ValueRef::Integer(v) => v.to_string(),
                ValueRef::Real(v) => format!("{v:?}"),
                ValueRef::Text(v) => String::from_utf8_lossy(v).into_owned(),
                ValueRef::Blob(v) => v.iter().map(|b| format!("{b:02x}")).collect(),
            }
        }

        fn snapshot(conn: &Connection, sql: &str) -> Vec<Vec<String>> {
            let mut stmt = conn.prepare(sql).unwrap();
            stmt.query_map([], |row| {
                let mut values = Vec::new();
                for index in 0..row.as_ref().column_count() {
                    values.push(render(row.get_ref(index)?));
                }
                Ok(values)
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
        }

        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        // 模拟“当前形状的旧库”：用量四表已是新形状，但 billing/pricing/alias/override/
        // model-config 七张新表尚未出现。
        conn.execute_batch(
            "DROP TABLE usage_billing_state;
             DROP TABLE usage_billing_revision;
             DROP TABLE usage_pricing_state;
             DROP TABLE usage_pricing_snapshot;
             DROP TABLE usage_provider_alias;
             DROP TABLE usage_price_override;
             DROP TABLE usage_model_config;",
        )
        .unwrap();
        conn.execute_batch(
            "INSERT INTO usage_detail
                 (source_id, session_id, project_key, hour_key, day_key, model, provider,
                  provider_kind, input_fresh, cache_read, cache_write, output,
                  request_total, requests, user_est, first_seen_at, updated_at)
                 VALUES ('codex','s1','proj','2026-10-17T10','2026-10-17','gpt-5','openai',
                         'measured',10,2,3,4,12,1,5,100,200);
             INSERT INTO usage_daily
                 (day_key, source_id, project_key, provider, provider_kind, model,
                  input_fresh, cache_read, cache_write, output, request_total, requests,
                  user_est, updated_at)
                 VALUES ('2026-10-17','codex','proj','openai','measured','gpt-5',
                         10,2,3,4,12,1,5,200);
             INSERT INTO usage_cursor
                 (source_id, session_id, fingerprint, byte_offset, ordinal, last_cumulative,
                  mtime_ms, file_size, state_json, updated_at)
                 VALUES ('codex','s1','fp',42,3,99,123,456,'{\"k\":1}',789);
             INSERT INTO usage_session
                 (source_id, session_id, project_key, project_label, project_path_raw,
                  project_realpath, title, is_subagent, parent_session_id, first_seen_at,
                  last_seen_at)
                 VALUES ('codex','s1','proj','Proj','/tmp/p','/tmp/p','T',0,NULL,100,200);",
        )
        .unwrap();

        let detail_sql = "SELECT * FROM usage_detail
             ORDER BY source_id, session_id, project_key, hour_key, model, provider";
        let daily_sql = "SELECT * FROM usage_daily
             ORDER BY day_key, source_id, project_key, provider, model";
        let cursor_sql = "SELECT * FROM usage_cursor ORDER BY source_id, session_id";
        let session_sql = "SELECT * FROM usage_session ORDER BY source_id, session_id";

        let before = [
            snapshot(&conn, detail_sql),
            snapshot(&conn, daily_sql),
            snapshot(&conn, cursor_sql),
            snapshot(&conn, session_sql),
        ];
        assert!(
            before.iter().all(|table| !table.is_empty()),
            "前提：四张旧表都必须有数据"
        );

        migrate(&conn).unwrap();
        migrate(&conn).unwrap();

        let after = [
            snapshot(&conn, detail_sql),
            snapshot(&conn, daily_sql),
            snapshot(&conn, cursor_sql),
            snapshot(&conn, session_sql),
        ];
        assert_eq!(before, after, "连续迁移两次不得改动旧账本任何行或游标");

        for table in [
            "usage_billing_state",
            "usage_billing_revision",
            "usage_pricing_state",
            "usage_pricing_snapshot",
            "usage_provider_alias",
            "usage_price_override",
            "usage_model_config",
        ] {
            assert!(
                conn.prepare(&format!("SELECT * FROM {table} LIMIT 0"))
                    .is_ok(),
                "迁移后必须存在新表 {table}"
            );
        }
    }

    /// TPS 三表（2026-10-24 计划 Task 2）：老库形状（无三表）经 migrate 幂等补齐；
    /// 重复迁移不报错、形状逐列与 spec §6 对齐（主键 tick_id+series_key / 三元组事实主键 /
    /// window_ms 钉 120000）。新库由 schema.rs 直建，migrate 重放 init 全部短路。
    #[test]
    fn migrate_creates_tps_tables_idempotently() {
        let conn = Connection::open_in_memory().unwrap();
        // 老库形状：先建 extensions（migrate 前段 ALTER 依赖），不建 TPS 三表
        conn.execute_batch(
            "CREATE TABLE extensions (
                id TEXT PRIMARY KEY, kind TEXT NOT NULL, name TEXT NOT NULL,
                description TEXT, source_path TEXT NOT NULL, source_url TEXT,
                version TEXT, tags TEXT, installed_at TEXT NOT NULL, updated_at TEXT NOT NULL
            );",
        )
        .unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap(); // 幂等：跑两遍不报错（IF NOT EXISTS 全部短路）
        let cols = |table: &str| -> Vec<String> {
            conn.prepare(&format!("PRAGMA table_info({})", table))
                .unwrap()
                .query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        // 事实表：主键三元组逐字在场（spec §6：source+host_instance+stable_call_key）；
        // V2 Task 1 补列 response_elapsed_ms（原先漏登记的本完备性清单，Task 6 补上）
        let obs_cols = cols("tps_observation");
        for c in [
            "source",
            "host_instance",
            "stable_call_key",
            "started_at_ms",
            "first_token_at_ms",
            "ended_at_ms",
            "native_duration_ms",
            "generation_ms",
            "response_elapsed_ms",
            "output_tokens",
            "output_scope",
            "tool_ms",
            "model",
            "channel",
            "session_key",
            "turn_key",
            "purpose",
            "outcome",
            "method",
            "failure_reason",
        ] {
            assert!(
                obs_cols.contains(&c.to_string()),
                "tps_observation 缺列 {c}"
            );
        }
        // 点表：spec §6 点列清单（n/sum_output/sum_generation_ms/ttft_sum/count/
        // latest_ended_at/quality/sampled_keys）+ window_ms 逐字钉 120000
        let point_cols = cols("tps_point");
        for c in [
            "tick_id",
            "series_key",
            "sampled_at",
            "window_ms",
            "run_id",
            "trigger_kind",
            "status",
            "quality",
            "n",
            "sum_output_tokens",
            "sum_generation_ms",
            "ttft_sum_ms",
            "ttft_count",
            "latest_ended_at",
            "sampled_keys",
            "cadence_ms",
        ] {
            assert!(point_cols.contains(&c.to_string()), "tps_point 缺列 {c}");
        }
        // 状态表：key+JSON
        assert_eq!(
            cols("tps_state"),
            vec!["key", "value_json", "updated_at"],
            "tps_state 列序 = key+JSON"
        );
        // V2 Task 6：dashboard 复合索引由 migrate 重放 init 幂等补齐（老库也拿到）
        let idx: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' \
                 AND name='idx_tps_observation_series_ended'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(idx, 1, "migrate 必须带出 idx_tps_observation_series_ended");
    }

    /// Codex 动作账本：旧结构（**两列都不存在**）经迁移补列（barrier + signature）——
    /// `barrier_force_released_at` 与 `signature_applied` 都可空、存量行保留 NULL；
    /// 幂等两遍不报错。
    #[test]
    fn migrate_adds_codex_barrier_and_signature_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE codex_actions (
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                action_id TEXT UNIQUE NOT NULL,
                device_id TEXT NOT NULL,
                device_name TEXT NOT NULL,
                thread_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                request_key TEXT,
                context_token TEXT,
                payload_hash TEXT NOT NULL,
                client_message_id TEXT NOT NULL,
                state TEXT NOT NULL,
                route TEXT NOT NULL DEFAULT 'none',
                queued_text TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                receipt_json TEXT,
                compat_outstanding INTEGER NOT NULL DEFAULT 0,
                compat_message_id TEXT,
                compat_turn_id TEXT,
                compat_rollout_path TEXT,
                compat_rollout_baseline INTEGER,
                compat_wire_hash TEXT
            );
            INSERT INTO codex_actions (action_id, device_id, device_name, thread_id, kind,
                 payload_hash, client_message_id, state, created_at, updated_at, compat_outstanding)
             VALUES ('legacy-a', 'd', 'iPhone', 't', 'send', 'h', 'cm', 'unknown', 1, 1, 1);",
        )
        .unwrap();
        migrate(&conn).unwrap();
        let (flag, ts, signed): (i64, Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT compat_outstanding, barrier_force_released_at, signature_applied \
                 FROM codex_actions WHERE action_id = 'legacy-a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (flag, ts, signed),
            (1, None, None),
            "存量行保留，两列均为 NULL（旧行签名语义 = 升级前恒签名）"
        );
        // 幂等：迁移两遍不报错
        migrate(&conn).unwrap();
    }

    /// 独立复核 R11：老库 tps_point 无 cadence_ms 列 → migrate 以 ALTER 补加可空列（旧行 NULL），
    /// 补后可按新列写读；重放幂等
    #[test]
    fn migrate_adds_cadence_ms_to_existing_tps_point() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE extensions (
                id TEXT PRIMARY KEY, kind TEXT NOT NULL, name TEXT NOT NULL,
                description TEXT, source_path TEXT NOT NULL, source_url TEXT,
                version TEXT, tags TEXT, installed_at TEXT NOT NULL, updated_at TEXT NOT NULL
            );
            -- 修复前形状的点表（无 cadence_ms）
            CREATE TABLE tps_point (
                tick_id TEXT NOT NULL, series_key TEXT NOT NULL, sampled_at INTEGER NOT NULL,
                window_ms INTEGER NOT NULL CHECK(window_ms = 120000), run_id TEXT NOT NULL,
                trigger_kind TEXT NOT NULL, status TEXT NOT NULL CHECK(status IN ('value','gap')),
                method TEXT NOT NULL, output_scope TEXT NOT NULL, quality TEXT NOT NULL,
                n INTEGER NOT NULL DEFAULT 0, sum_output_tokens INTEGER NOT NULL DEFAULT 0,
                sum_generation_ms INTEGER NOT NULL DEFAULT 0, ttft_sum_ms INTEGER,
                ttft_count INTEGER NOT NULL DEFAULT 0, model_ms INTEGER, tool_ms INTEGER,
                avg_first_token_ms INTEGER, tps REAL, candidate_count INTEGER NOT NULL DEFAULT 0,
                timed_model_count INTEGER NOT NULL DEFAULT 0, timed_tool_count INTEGER NOT NULL DEFAULT 0,
                failed_count INTEGER NOT NULL DEFAULT 0, cancelled_count INTEGER NOT NULL DEFAULT 0,
                missing_timing_count INTEGER NOT NULL DEFAULT 0, latest_ended_at INTEGER,
                sampled_keys TEXT NOT NULL DEFAULT '[]',
                PRIMARY KEY (tick_id, series_key)
            );
            INSERT INTO tps_point (tick_id, series_key, sampled_at, window_ms, run_id, trigger_kind,
                status, method, output_scope, quality)
            VALUES ('t1', 's1', 1, 120000, 'run', 'scheduled', 'gap', 'client-native', 'unknown', 'normal');",
        )
        .unwrap();
        let cols = |t: &str| -> Vec<String> {
            let mut st = conn.prepare(&format!("PRAGMA table_info({t})")).unwrap();
            st.query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert!(!cols("tps_point").contains(&"cadence_ms".to_string()));
        migrate(&conn).unwrap();
        assert!(
            cols("tps_point").contains(&"cadence_ms".to_string()),
            "ALTER 补列"
        );
        // 旧行 NULL；新行可写读
        let old: Option<i64> = conn
            .query_row(
                "SELECT cadence_ms FROM tps_point WHERE tick_id='t1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old, None);
        conn.execute(
            "UPDATE tps_point SET cadence_ms = 60000 WHERE tick_id='t1'",
            [],
        )
        .unwrap();
        let now: Option<i64> = conn
            .query_row(
                "SELECT cadence_ms FROM tps_point WHERE tick_id='t1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(now, Some(60000));
        migrate(&conn).unwrap(); // 幂等
    }

    /// S1：`codex_ipc_probe` 老库（无证据列）迁移补 `evidence_at` / `evidence_stage`
    /// 两列 + 建 `codex_ipc_schema_evidence` 决策词表。**不改旧行**：存量行两列为
    /// NULL（旧成功由读侧 has_evidence 按旧值兼容识别，见 DAO 老数据回归）；
    /// 幂等两遍不报错。
    #[test]
    fn migrate_codex_ipc_probe_adds_evidence_columns_keeps_legacy_rows() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE codex_ipc_probe (
                probe_key        TEXT NOT NULL,
                capability       TEXT NOT NULL,
                state            TEXT NOT NULL,
                probe_stage      TEXT,
                error_code       TEXT,
                attempts         INTEGER NOT NULL DEFAULT 1,
                first_seen_at    INTEGER NOT NULL,
                last_seen_at     INTEGER NOT NULL,
                next_retry_at    INTEGER,
                last_duration_ms INTEGER,
                PRIMARY KEY (probe_key, capability)
            );
            INSERT INTO codex_ipc_probe
                (probe_key, capability, state, probe_stage, attempts, first_seen_at, last_seen_at)
             VALUES ('windows|old|-|1', 'state', 'probed', 'P3', 2, 500, 900);",
        )
        .unwrap();
        migrate(&conn).unwrap();
        // 两列就位 + 决策词表就位
        assert!(conn
            .prepare("SELECT evidence_at, evidence_stage FROM codex_ipc_probe LIMIT 0")
            .is_ok());
        assert!(conn
            .prepare("SELECT probe_key FROM codex_ipc_schema_evidence LIMIT 0")
            .is_ok());
        // 旧行原样保留（不删不改），证据列取 NULL 缺省
        let (state, evidence_at): (String, Option<i64>) = conn
            .query_row(
                "SELECT state, evidence_at FROM codex_ipc_probe WHERE probe_key = 'windows|old|-|1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((state.as_str(), evidence_at), ("probed", None));
        // 幂等：迁移两遍不报错
        migrate(&conn).unwrap();
    }

    /// 终审 F1 + Gate C 裁决（方向 b）：bug 时代的假证据（含**重试累积形态**）被
    /// 迁移清除；真成功证据保留；**「成功后失败」行按裁决一并清据、承担一次性
    /// 重探代价**（表未发版；写路径的保留语义由 ON CONFLICT 臂继续保证）。
    /// 形态覆盖：①首次失败假证据（attempts=1）；②假证据经退避重试累积到
    /// attempts=2（COALESCE 保留臂——初版 attempts=1 判据漏掉的形态）；③真首次
    /// 成功（保留）；④真成功后失败（清据 + 注明代价）。幂等两遍。
    #[test]
    fn migrate_codex_ipc_probe_clears_fake_evidence_keeps_real() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE codex_ipc_probe (
                probe_key        TEXT NOT NULL,
                capability       TEXT NOT NULL,
                state            TEXT NOT NULL,
                probe_stage      TEXT,
                error_code       TEXT,
                attempts         INTEGER NOT NULL DEFAULT 1,
                first_seen_at    INTEGER NOT NULL,
                last_seen_at     INTEGER NOT NULL,
                next_retry_at    INTEGER,
                last_duration_ms INTEGER,
                fingerprint      TEXT,
                evidence_at      INTEGER,
                evidence_stage   TEXT,
                PRIMARY KEY (probe_key, capability)
            );
            -- 假证据①：首次 inconclusive（bug INSERT 臂写的 now=1000）
            INSERT INTO codex_ipc_probe
                (probe_key, capability, state, probe_stage, error_code, attempts,
                 first_seen_at, last_seen_at, evidence_at, evidence_stage)
             VALUES ('windows|fake|-|1', 'state', 'inconclusive', 'P1', 'timeout', 1,
                     1000, 1000, 1000, 'P1');
            -- 假证据②：首次 failed
            INSERT INTO codex_ipc_probe
                (probe_key, capability, state, probe_stage, error_code, attempts,
                 first_seen_at, last_seen_at, evidence_at, evidence_stage)
             VALUES ('windows|fake|-|1', 'send', 'failed', NULL, 'method_not_found', 1,
                     2000, 2000, 2000, NULL);
            -- 真证据①：首次成功（attempts=1 ∧ probed）
            INSERT INTO codex_ipc_probe
                (probe_key, capability, state, probe_stage, attempts,
                 first_seen_at, last_seen_at, evidence_at, evidence_stage)
             VALUES ('windows|real|-|1', 'state', 'probed', 'P3', 1,
                     3000, 3000, 3000, 'P3');
            -- 假证据③（Gate C 补充形态）：假证据经退避重试累积（attempts=2）——
            -- ON CONFLICT 的 COALESCE 保留臂把首写假证据带到 attempts≥2，
            -- 行级与真「成功后失败」不可判别（evidence_at 都=首写时间）
            INSERT INTO codex_ipc_probe
                (probe_key, capability, state, probe_stage, error_code, attempts,
                 first_seen_at, last_seen_at, evidence_at, evidence_stage)
             VALUES ('windows|fake|-|2', 'state', 'inconclusive', 'P1', 'timeout', 2,
                     1500, 6000, 1500, 'P1');
            -- 真证据②：成功后失败（attempts=2）——**Gate C 裁决：一并清据**，
            -- 承担一次性重探代价（表未发版；新数据的保留语义由写路径继续保证）
            INSERT INTO codex_ipc_probe
                (probe_key, capability, state, probe_stage, error_code, attempts,
                 first_seen_at, last_seen_at, evidence_at, evidence_stage)
             VALUES ('windows|real|-|1', 'send', 'failed', 'P1', 'cli_missing', 2,
                     4000, 5000, 4000, 'P1');",
        )
        .unwrap();
        migrate(&conn).unwrap();
        // 假证据被清（含 Gate C 补充的重试累积形态）——读侧 has_evidence 随之
        // 回落 false → 重探不再被短路
        for (key, cap) in [
            ("windows|fake|-|1", "state"),
            ("windows|fake|-|1", "send"),
            ("windows|fake|-|2", "state"),
        ] {
            let (evidence_at, evidence_stage): (Option<i64>, Option<String>) = conn
                .query_row(
                    "SELECT evidence_at, evidence_stage FROM codex_ipc_probe \
                     WHERE probe_key = ?1 AND capability = ?2",
                    [key, cap],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(
                (evidence_at, evidence_stage),
                (None, None),
                "假证据（首写 + 重试累积形态）必须被清除：{key}/{cap}"
            );
        }
        // 真首次成功保留
        let real_state: Option<i64> = conn
            .query_row(
                "SELECT evidence_at FROM codex_ipc_probe \
                 WHERE probe_key = 'windows|real|-|1' AND capability = 'state'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(real_state, Some(3000), "首次成功的真证据保留");
        // Gate C 裁决：「成功后失败」行在修复后证据为空（一次性代价——下次触发
        // 重探一次；写路径的保留语义由 ON CONFLICT 臂继续保证，新数据不受影响）
        let real_send: Option<i64> = conn
            .query_row(
                "SELECT evidence_at FROM codex_ipc_probe \
                 WHERE probe_key = 'windows|real|-|1' AND capability = 'send'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            real_send, None,
            "成功后失败行按裁决清据（一次性重探代价，表未发版）"
        );
        // 幂等：迁移两遍不再清任何行（真成功证据不被二遍误清）
        migrate(&conn).unwrap();
        let real_state2: Option<i64> = conn
            .query_row(
                "SELECT evidence_at FROM codex_ipc_probe \
                 WHERE probe_key = 'windows|real|-|1' AND capability = 'state'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(real_state2, Some(3000), "幂等：真成功证据不被二遍迁移误清");
        // 完成标记已落（一次性清理的持久凭据；首跑无待清行也要落——见下方空表用例）
        assert!(
            conn.query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' \
                 AND name='codex_ipc_probe_fake_evidence_purge_done'",
                [],
                |_| Ok(()),
            )
            .is_ok(),
            "一次性清理必须留持久完成标记"
        );
    }

    /// 复审 R1（P1）：一次性清理必须**真的只跑一次**——标记后新增的真「成功→
    /// 失败」证据不得被后续迁移清除。序列 = 复审原文：migrate（清一次）→ 写真
    /// 成功证据 → 记失败（证据保留）→ 再 migrate → **断言证据保留**（红 = 被清）。
    #[test]
    fn fake_evidence_purge_is_one_shot_new_evidence_survives_later_migrations() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE codex_ipc_probe (
                probe_key        TEXT NOT NULL,
                capability       TEXT NOT NULL,
                state            TEXT NOT NULL,
                probe_stage      TEXT,
                error_code       TEXT,
                attempts         INTEGER NOT NULL DEFAULT 1,
                first_seen_at    INTEGER NOT NULL,
                last_seen_at     INTEGER NOT NULL,
                next_retry_at    INTEGER,
                last_duration_ms INTEGER,
                fingerprint      TEXT,
                evidence_at      INTEGER,
                evidence_stage   TEXT,
                PRIMARY KEY (probe_key, capability)
            );",
        )
        .unwrap();
        // 第一次 migrate：无待清行也必须落完成标记
        migrate(&conn).unwrap();
        assert!(
            conn.query_row(
                "SELECT 1 FROM codex_ipc_probe_fake_evidence_purge_done",
                [],
                |_| Ok(()),
            )
            .is_ok(),
            "无待清行也要记录完成"
        );
        // 修复后写路径：真成功证据 → 随后失败（证据由 ON CONFLICT 保留）
        crate::database::dao::codex_ipc_probe::upsert_ok_conn(
            &conn,
            "windows|fresh|-|1",
            "state",
            Some("P3"),
            None,
            Some(100),
            1_000,
        )
        .unwrap();
        crate::database::dao::codex_ipc_probe::upsert_fail_conn(
            &conn,
            "windows|fresh|-|1",
            "state",
            Some("P1"),
            "cli_missing",
            None,
            Some(80),
            2_000,
        )
        .unwrap();
        let before: (Option<i64>, String, i64) = conn
            .query_row(
                "SELECT evidence_at, state, attempts FROM codex_ipc_probe \
                 WHERE probe_key = 'windows|fresh|-|1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            before,
            (Some(1_000), "failed".into(), 2),
            "前置：成功后失败保留证据（既有契约）"
        );
        // 再 migrate：**不得**清除标记之后新增的真证据（红 = 被清成 NULL）
        migrate(&conn).unwrap();
        let after: (Option<i64>, Option<String>) = conn
            .query_row(
                "SELECT evidence_at, evidence_stage FROM codex_ipc_probe \
                 WHERE probe_key = 'windows|fresh|-|1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            after,
            (Some(1_000), Some("P3".into())),
            "一次性清理完成后，新增真「成功→失败」证据必须跨迁移保留（R1 红 = 被清）"
        );
    }
}

#[cfg(test)]
mod batch_migration_tests {
    use super::*;

    /// 2026-10-31 实机：早期建出的 `codex_send_batch_members` 只有 3 列，
    /// 而查询按 `member_id, wire_text, signature_applied` 取值 →
    /// `no such column: signature_applied`，合并发送的回队路径直接失败。
    /// 迁移必须把这一列补上（幂等、可重入、老行有确定默认值）。
    #[test]
    fn migrate_adds_batch_signature_column_to_legacy_table() {
        let conn = Connection::open_in_memory().unwrap();
        // 老库形状：只有 3 列
        conn.execute_batch(
            "CREATE TABLE codex_send_batch_members (
                member_id TEXT PRIMARY KEY,
                leader_id TEXT NOT NULL,
                wire_text TEXT NOT NULL
            );
            INSERT INTO codex_send_batch_members (member_id, leader_id, wire_text)
            VALUES ('m1', 'l1', '旧批次正文');",
        )
        .unwrap();
        migrate(&conn).unwrap();
        assert!(
            conn.prepare("SELECT signature_applied FROM codex_send_batch_members LIMIT 0")
                .is_ok(),
            "迁移后必须有 signature_applied 列"
        );
        // 老行有确定默认值（未签名 = 0，与旧的行为一致）
        let v: i64 = conn
            .query_row(
                "SELECT signature_applied FROM codex_send_batch_members WHERE member_id='m1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, 0, "老行回填 0（未签名）");
        // 幂等：再跑一遍不报错
        migrate(&conn).unwrap();
    }
}
