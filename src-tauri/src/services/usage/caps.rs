//! 逐源指标可得性静态表（说明书 §9.5 落码）。
//! **不可得一律 false**：查询层据此产出 `availability`（available=false + reason），
//! 采集器据此跳过整类计算（省 IO），二者共用同一张表，杜绝两边漂移。
use super::model::UsageSourceId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceCaps {
    pub turn: bool,
    pub error_model: bool,
    pub error_turn: bool,
    pub error_tool: bool,
    pub interrupted: bool,
    pub longest_turn: bool,
    pub tool_calls: bool,
    pub user_est: bool,
}

/// §9.5 表 + §6 报错表的落码。逐项依据见注释（评审时可直接对照矩阵）。
pub fn caps_of(source: UsageSourceId) -> SourceCaps {
    match source {
        // claude：turn 需归一化（「已发起回合」）；**无回合级失败字段**（A 列只有 API 层 1 条 +
        // 工具层 29 条）；工具耗时要配对（toolUseResult 上无耗时字段）；用户输入可读
        UsageSourceId::Claude => SourceCaps {
            turn: true,
            error_model: true,
            error_turn: false,
            error_tool: true,
            interrupted: true,
            longest_turn: true,
            tool_calls: true,
            user_est: true,
        },
        // codex：五面报错全可得；时长走 task_complete.duration_ms（按 turn_id 去重）
        UsageSourceId::Codex => SourceCaps {
            turn: true,
            error_model: true,
            error_turn: true,
            error_tool: true,
            interrupted: true,
            longest_turn: true,
            tool_calls: true,
            user_est: true,
        },
        // kimi：turn 靠 append_loop_event[].event.turnId 去重；最长 turn 原生仅 11 条
        // （覆盖率 5%）须按 append_loop_event.time 推导；**矩阵 §6 未给出 kimi 的用户打断判据 → 空态**
        UsageSourceId::Kimi => SourceCaps {
            turn: true,
            error_model: true,
            error_turn: true,
            error_tool: true,
            interrupted: false,
            longest_turn: true,
            tool_calls: true,
            user_est: true,
        },
        // opencode：**工具级不可得**（part.state.status 全 completed）；用户输入不可读
        UsageSourceId::OpenCode => SourceCaps {
            turn: true,
            error_model: true,
            error_turn: true,
            error_tool: false,
            interrupted: true,
            longest_turn: true,
            tool_calls: true,
            user_est: false,
        },
        // workbuddy：**无回合概念**、**无 duration 字段**、供应商不可得；错误弱可得（无分类）。
        // 子代理（5.9.3 起 `subagents/*.jsonl`）自 2026-10-30 起独立入账为 `is_subagent=1` 的会话，
        // 但**不代表口径变化**：token 仍计入总量、会话数/turn 数由查询层 D17 分层排除，
        // 本表各字段的可得性对父/子代理完全一致（子代理文件同形），故不新增 caps 维度。
        UsageSourceId::WorkBuddy => SourceCaps {
            turn: false,
            error_model: false,
            error_turn: false,
            error_tool: true,
            interrupted: true,
            longest_turn: false,
            tool_calls: true,
            user_est: false,
        },
        // zcode：唯一「A、B 均可得且最完整」的源（原生 tool_usage.duration_ms 非空率 100%）
        UsageSourceId::ZCode => SourceCaps {
            turn: true,
            error_model: true,
            error_turn: true,
            error_tool: true,
            interrupted: true,
            longest_turn: true,
            tool_calls: true,
            user_est: false,
        },
        // dsh：报错与最长 turn **必须读原始 .zstd 会话日志**（投影缓存拿不到），
        // 覆盖率受限于「原始日志并非每个会话都还在」——available=true 但 reason 说明覆盖范围。
        // ⚠️ **不写死比例**（2026-10-06 起）：覆盖率随机器与清理策略变化，旧注释里的「19/99」是
        // spec 探测期的矩阵口径，**已作废**；2026-10-06 实测账本口径已接近全量。
        // ⚠️ 这一档**不在界面表达**（用户 2026-10-06 裁决）：可得性模型是**布尔两态**
        // （`available` + `perSource`），没有「部分」这一档，界面只按「有值 / `EM_DASH`」呈现
        // ——「能显示哪些就显示哪些，不能显示的就用横杠表示没有这个数据」。
        UsageSourceId::Dsh => SourceCaps {
            turn: true,
            error_model: true,
            error_turn: true,
            error_tool: true,
            interrupted: true,
            longest_turn: true,
            tool_calls: true,
            user_est: false,
        },
    }
}

/// 逐源 turn 口径说明（D16 要求「各源口径须逐源写明」，查询层填进 availability.reason 供 tooltip）
pub fn turn_semantics(source: UsageSourceId) -> &'static str {
    match source {
        UsageSourceId::Claude => "claude: 已发起回合（user ∧ toolUseResult==null ∧ isMeta!=true）",
        UsageSourceId::Codex => "codex: event_msg.task_started 计数（turn_context 不可用）",
        UsageSourceId::Kimi => {
            "kimi: context.append_loop_event[].event.turnId 去重（含全部 agents/*/）"
        }
        UsageSourceId::OpenCode => "opencode: 数 user 行（assistant 是 step，数它会虚高 71%~81%）",
        UsageSourceId::WorkBuddy => "workbuddy: 无回合概念（不可得）",
        UsageSourceId::ZCode => "zcode: turn_usage 行数（118/665 会话无 turn 行，左连补 0）",
        UsageSourceId::Dsh => "dsh: sessionStats.val.turns（与原始 turn/start 精确相等）",
    }
}
// ==== usage 自省锁：以下为排除区（测试代码），勿删勿复制 ====
// ==== usage 自省锁：排除区结束，勿删勿复制 ====
