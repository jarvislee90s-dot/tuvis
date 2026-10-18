//! 移动端子 agent 运行检测（spec：docs/superpowers/specs/2026-10-08-mobile-subagent-chips-design.md §5.1）
//!
//! 模块职责：四工具（claude/opencode/kimi/codex）「该会话下有哪些运行中子 agent」
//! 的判据与装配。判据/解析/累计全部纯函数化（注入字节流/路径，测试零真实磁盘）。
//!
//! 预算纪律：opencode 查询即最新（SQLite 豁免）；claude/kimi/codex 走**会话级
//! offset 增量缓存**——**为何不用 SessionFileScan 摘要缓存**：摘要缓存对「持续增长
//! 的转写累计 token」意味着每轮全文件重解析；offset 增量是 O(delta)。缓存物全是
//! 文件内容纯函数，无时间叠加；运行时长服务端下发 spawnTs、客户端自走。
//! 防后人「归一」：不要把本模块的缓存改回 (mtime,size) 全量摘要形态。

pub(crate) mod claude;
pub(crate) mod codex;
pub(crate) mod kimi;
pub(crate) mod opencode;

use serde::Serialize;
use std::path::Path;

/// 四桶 token 载荷（端点 JSON camelCase；各工具映射时已把 reasoning 类桶排除在四桶外）
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    pub input: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
    pub output: u64,
}

impl TokenUsage {
    /// 四桶合计（chip 显示口径；饱和加法防溢出。生产端点下发明细四桶、
    /// 合计由前端求和——本方法主要供测试数字锁使用，随载荷类型一并 pub）
    pub fn total(&self) -> u64 {
        self.input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_creation)
            .saturating_add(self.output)
    }
}

/// 子 agent 运行状态（观察台 §二.3 清单卡绿/灰点）：running=活跃（时长/token 走字）、
/// idle=不活跃（已完成/已停止——数据冻结原位）
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SubagentStatus {
    Running,
    Idle,
}

/// 端点载荷单条（spec §5.2 + 观察台 §二）。spawnTs=None：首条时间戳尚未落盘
/// （spawn 竞态，下轮自愈，前端不显示时长只显 token）。
/// 2026-10-09 T2：**全量名单**（含终态）——运行过滤移交前端 chip 层；
/// endTs = 终态锚（ISO）：运行中恒 None；不活跃 = 最后已知活动/完成时刻
/// （claude=子转写末行 ts / opencode=time_updated / kimi=wire mtime /
/// codex=task_complete 行 ts）——前端冻结时长锚：elapsed = (endTs ?? now) − spawnTs。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentView {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub spawn_ts: Option<String>,
    pub tokens: TokenUsage,
    pub status: SubagentStatus,
    pub end_ts: Option<String>,
}

/// 活跃窗口（opencode §4.2 / kimi §4.3 共用取值：详情轮询 10s 的 9 倍冗余，
/// 容忍子 agent 长静默工具调用；常量可调）
pub(crate) const ACTIVE_SECS: u64 = 90;

/// 活跃判定（纯函数，now 注入便于测试边界）：严格小于 ACTIVE_SECS 才活跃
pub(crate) fn active_within(
    last_activity: std::time::SystemTime,
    now: std::time::SystemTime,
) -> bool {
    now.duration_since(last_activity)
        .map(|d| d.as_secs() < ACTIVE_SECS)
        .unwrap_or(false)
}

/// 毫秒 → ISO8601 字符串（spawnTs 下发口径；溢出/负值 → None）。
/// **待收口申报（评审 P2-3）**：content.rs 与 kimi_parser.rs 各有一份私有 ms_to_iso，
/// 本模块是第三份——跨文件提取公用 helper 属独立重构批次，不在本计划扩面；
/// 三份实现语义一致（from_timestamp_millis + to_rfc3339），先注明不静默新造。
pub(crate) fn ms_to_iso(ms: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(ms).map(|t| t.to_rfc3339())
}

/// JSON 时间值（int 毫秒 / ISO 字符串双形态）→ ISO 字符串（kimi created_at 用）。
/// 与 kimi_parser::updated_at_to_string 同构（那处不做毫秒→ISO 归一）——同属
/// P2-3 待收口申报：C1 取证 created_at 形态后随 kimi 判据收严一并评估合并。
pub(crate) fn json_time_to_iso(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Number(n) => n.as_i64().and_then(ms_to_iso),
        serde_json::Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

// ============================================================
// 回报提醒打标（#151 全工具化）：时间关联判据共享核
// ============================================================

/// 回报关联系数窗：与 ACTIVE_SECS=90 同值、独立常量——两语义可分别调参
/// （活跃窗是「还在跑」分诊；回报窗是「子代理完成/最后活动落在父末条多久内」）。
pub(crate) const SUBAGENT_REPORT_WINDOW_MS: i64 = 90_000;
/// 时钟/秒级取整容差（锚可略晚于父末条落盘）
pub(crate) const SUBAGENT_REPORT_SKEW_MS: i64 = 2_000;

/// 锚 ≤ last+skew ∧ last−anchor ≤ window（回报发生在父末条之前不久）。
/// 比 last_msg 而不比「当前时刻」：digest-now 随轮询推进会打标翻转（flappy），
/// last_msg 锚使打标是会话内容的稳定函数（与 L2「纯内容产物」纪律同构）。
pub(crate) fn report_anchor_near_last(anchor_ms: i64, last_msg_ms: i64) -> bool {
    anchor_ms <= last_msg_ms.saturating_add(SUBAGENT_REPORT_SKEW_MS)
        && last_msg_ms.saturating_sub(anchor_ms) <= SUBAGENT_REPORT_WINDOW_MS
}

/// rfc3339 → 毫秒（ms_to_iso 的逆）；解析失败 None（该锚放弃，保守不打标）
pub(crate) fn iso_to_ms(ts: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|d| d.timestamp_millis())
}

// ============================================================
// 增量读：offset 只跨过完整行；尾部半行留待下轮；stat 门控；护栏作废重建
// ============================================================

/// 增量读状态（进会话级缓存，随缓存条目存活；Clone 供缓存条目回取后的可变化拷贝）
#[derive(Default, Clone)]
pub(crate) struct IncrState {
    /// 已消费字节数（不含尾部半行——半行下次从 offset 重读再拼）
    offset: u64,
    /// 上次读到的文件大小（stat 门控：不变则不开文件；亦即下轮续读点——
    /// 已缓冲半行不重复读）
    last_size: u64,
    /// 尾部半行缓冲（UTF-8 安全：按字节切、按行转 String）
    partial: Vec<u8>,
}

impl IncrState {
    /// 重建后对齐增量游标（claude 倒序扫描用，评审 P1-1）：offset = size − partial.len()、
    /// last_size = size、partial = 文件尾未终结半行——T1 核心不变式不被破坏
    /// （partial 恒等于「文件尾尚未换行收尾的字节」，其事件留给增量路径补齐后消费）。
    pub(crate) fn resume_at(&mut self, size: u64, partial: Vec<u8>) {
        // 饱和：扫描与对齐之间文件被并发截断（partial.len() > size）时不下溢 panic
        self.offset = size.saturating_sub(partial.len() as u64);
        self.last_size = size;
        self.partial = partial;
    }
}

pub(crate) enum IncrRead {
    /// stat 门控命中：文件未增长，未打开
    Unchanged,
    /// 新完整行（文件序）。reset=true：size<offset 护栏触发，已清零状态并从 0
    /// 重读全文件——调用方必须先清空自己的累计器再消费 lines
    Lines { lines: Vec<String>, reset: bool },
}

/// 增量读（spec §5.1「offset 增量 + 半行缓冲 + stat 门控」骨架，三工具复用）。
/// 文件不存在/不可读 → Lines{lines:[], reset:false}（与既有解析器防御语义一致，
/// 状态保留——下次文件出现按旧 offset 续读）。
pub(crate) fn read_increment(path: &Path, st: &mut IncrState) -> IncrRead {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(meta) = std::fs::metadata(path) else {
        return IncrRead::Lines {
            lines: Vec::new(),
            reset: false,
        };
    };
    let size = meta.len();
    if size == st.last_size {
        return IncrRead::Unchanged;
    }
    let mut reset = false;
    if size < st.last_size {
        // 护栏：文件被截断/重写（换代会话重写同名文件）——状态作废，从 0 重读
        st.offset = 0;
        st.last_size = 0;
        st.partial.clear();
        reset = true;
    }
    let Ok(mut f) = std::fs::File::open(path) else {
        return IncrRead::Lines {
            lines: Vec::new(),
            reset: false,
        };
    };
    // 续读点 = last_size：partial 里已缓冲的半行字节不再从磁盘重读（否则与
    // 下方 extend 拼接重复）；offset 只作「已完整消费字节」护栏基准
    if st.last_size > 0 && f.seek(SeekFrom::Start(st.last_size)).is_err() {
        return IncrRead::Lines {
            lines: Vec::new(),
            reset: false,
        };
    }
    let mut chunk = vec![0u8; (size - st.last_size) as usize];
    if f.read_exact(&mut chunk).is_err() {
        return IncrRead::Lines {
            lines: Vec::new(),
            reset: false,
        };
    }
    // 拼上半行缓冲，按字节切 '\n'；末段（其后无换行）→ 新 partial
    let mut buf = std::mem::take(&mut st.partial);
    buf.extend_from_slice(&chunk);
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (i, &b) in buf.iter().enumerate() {
        if b == b'\n' {
            lines.push(String::from_utf8_lossy(&buf[start..i]).into_owned());
            start = i + 1;
        }
    }
    st.partial = buf[start..].to_vec();
    // 核心不变式：partial 永远是「文件尾部尚未换行收尾的字节」，故
    // offset = size - partial.len()（字节精确；from_utf8_lossy 不参与计数）
    st.offset = size - st.partial.len() as u64;
    st.last_size = size;
    IncrRead::Lines { lines, reset }
}

// ============================================================
// 会话级缓存 registry（spec §5.1：进程内全局表；仅 claude / kimi / codex 使用，
// opencode 查询即最新无缓存）。评审 P2-1：定稿实现，不再是「只有测试契约」。
// ============================================================

/// 缓存条目盒（Arc<dyn Any>——存取形态照抄 session_scan.rs 的 FileParseCache）
pub(crate) type CacheBox = std::sync::Arc<dyn std::any::Any + Send + Sync>;

/// 条目硬上限（照抄 session_scan.rs:75 EVICT_HARD_CAP 模式：超限整段清空——
/// 正常活跃会话远达不到；病态增殖下宁可重建也不吃内存）
const SUBAGENT_CACHE_CAP: usize = 1024;

fn registry(
) -> &'static std::sync::Mutex<std::collections::HashMap<(&'static str, String), CacheBox>> {
    static REGISTRY: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<(&'static str, String), CacheBox>>,
    > = std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 读改写一条缓存条目：`f` 收到当前条目（None = 缓存缺失/被清/键类型不符），
/// 返回 (新条目, 结果)。downcast 失败按 None 处理——与 FileParseCache
/// 「类型不匹配按未命中覆盖」同语义（同键换类型只发生在实现演化期，防御即可）。
pub(crate) fn update_entry<R>(
    tool: &'static str,
    session_id: &str,
    f: impl FnOnce(Option<CacheBox>) -> (CacheBox, R),
) -> R {
    let mut map = registry().lock().unwrap_or_else(|e| e.into_inner());
    update_entry_locked(&mut map, tool, session_id, f)
}

/// 锁内核心（终审修复：CAP 检查/读改写语义的唯一定义点——update_entry 只做取锁
/// 转调，测试持锁直调本核即守住真函数，不再在测试里手工复刻锁/CAP/读改写语义）。
/// 调用方必须已持 registry 锁且锁内不得再调 update_entry（Mutex 不可重入）。
fn update_entry_locked<R>(
    map: &mut std::collections::HashMap<(&'static str, String), CacheBox>,
    tool: &'static str,
    session_id: &str,
    f: impl FnOnce(Option<CacheBox>) -> (CacheBox, R),
) -> R {
    if map.len() >= SUBAGENT_CACHE_CAP {
        map.clear(); // EVICT_HARD_CAP 同语义：整段清空
    }
    let cur = map.remove(&(tool, session_id.to_string()));
    let (entry, out) = f(cur);
    map.insert((tool, session_id.to_string()), entry);
    out
}

/// 端点输出序（spec §5.2）：spawnTs 升序，None 排尾（无时间不可比），稳定排序
pub(crate) fn sort_views(v: &mut [SubagentView]) {
    v.sort_by(|a, b| match (&a.spawn_ts, &b.spawn_ts) {
        (Some(x), Some(y)) => x.cmp(y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
}

/// 测试隔离：清空全部条目（兔维斯 重启语义的测试等价物；仅测试构建）
#[cfg(test)]
pub(crate) fn reset_cache_for_tests() {
    registry().lock().unwrap_or_else(|e| e.into_inner()).clear();
    codex::reset_walk_cache_for_tests();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// 增量读：追加完整行 → 返回；追加半行 → 不返回，下轮补齐后返回
    #[test]
    fn read_increment_handles_partial_line() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("wire.jsonl");
        std::fs::write(&p, "{\"a\":1}\n").unwrap();
        let mut st = IncrState::default();
        let r = read_increment(&p, &mut st);
        assert!(matches!(r, IncrRead::Lines { ref lines, reset: false } if lines.len() == 1));
        // 追加半行：不产出
        std::fs::write(&p, "{\"a\":1}\n{\"b\":").unwrap();
        let r = read_increment(&p, &mut st);
        assert!(
            matches!(r, IncrRead::Lines { ref lines, reset: false } if lines.is_empty()),
            "半行不得提前产出"
        );
        // 补齐换行：产出一行
        std::fs::write(&p, "{\"a\":1}\n{\"b\":2}\n").unwrap();
        let r = read_increment(&p, &mut st);
        assert!(
            matches!(r, IncrRead::Lines { ref lines, reset: false } if *lines == ["{\"b\":2}"])
        );
    }

    /// stat 门控（spec §7「读计数断言」的单元层落点）：文件未变 → Unchanged（未开文件）
    #[test]
    fn read_increment_stat_gate() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("w.jsonl");
        std::fs::write(&p, "l1\n").unwrap();
        let mut st = IncrState::default();
        let _ = read_increment(&p, &mut st);
        assert!(
            matches!(read_increment(&p, &mut st), IncrRead::Unchanged),
            "size 未变 → Unchanged（T3/T5/T6 的功能层等价断言：两次 collect 结果一致）"
        );
    }

    /// 护栏（spec §5.1「size < offset 作废重建」）：截断/重写 → reset=true 且全量重读
    #[test]
    fn read_increment_truncation_resets() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("w.jsonl");
        std::fs::write(&p, "aaaa\nbbbb\ncccc\n").unwrap();
        let mut st = IncrState::default();
        let _ = read_increment(&p, &mut st);
        // 重写为更短内容（size < offset）
        std::fs::write(&p, "x\ny\n").unwrap();
        let r = read_increment(&p, &mut st);
        assert!(
            matches!(r, IncrRead::Lines { reset: true, ref lines } if lines.len() == 2),
            "护栏触发：状态清零后从 0 重读，调用方据此清空累计器"
        );
    }

    /// 会话级缓存：读改写往返 + 硬上限整段清空（EVICT_HARD_CAP 模式）。
    /// 并行竞态修复（T2 批次实测抓获）：registry 是进程级单例，其余 collect 测试的
    /// reset_cache_for_tests() 若插在本用例两次写条目之间，会击穿「二次进入拿到上次
    /// 条目」断言（约 1/8 复现率）——故整个用例**持 registry 锁**串行执行。
    /// 终审修复：锁内直调**真核 update_entry_locked**（update_entry 要重入同一把锁，
    /// 会死锁）——CAP 检查/读改写语义不再在测试里手工复刻，真函数恢复直接守护；
    /// 断言语义与 v1 原版一致（闭包内断言 + 返回值管道）。
    #[test]
    fn cache_update_and_hard_cap() {
        reset_cache_for_tests();
        let mut map = registry().lock().unwrap_or_else(|e| e.into_inner());
        // 首次进入：闭包收到的条目缺失（读改写管道：返回值透传）
        let v = update_entry_locked(&mut map, "t-cap", "s1", |cur| {
            assert!(cur.is_none());
            (Arc::new(7u32) as CacheBox, 1u8)
        });
        assert_eq!(v, 1);
        // 二次进入：拿到上次条目（读改写往返）
        let v = update_entry_locked(&mut map, "t-cap", "s1", |cur| {
            let old = cur.and_then(|b| b.downcast::<u32>().ok()).map(|a| *a);
            (Arc::new(old.unwrap_or(0) + 1) as CacheBox, old)
        });
        assert_eq!(v, Some(7), "二次进入拿到上次条目");
        // 硬上限：len >= CAP → 先整段清空再插入（0..=CAP 共 CAP+1 次触发清空）
        for i in 0..=SUBAGENT_CACHE_CAP {
            update_entry_locked(&mut map, "t-cap", &format!("fill-{i}"), |_| {
                (Arc::new(0u32) as CacheBox, ())
            });
        }
        // 超限后整段清空 → s1 条目不复存在（闭包视角的「缺失」断言）
        let gone = update_entry_locked(&mut map, "t-cap", "s1", |cur| {
            (Arc::new(0u32) as CacheBox, cur.is_none())
        });
        assert!(gone, "条目超上限 → 整段清空（宁可重建也不吃内存）");
        drop(map); // 先放锁再清场（reset 重入同一把锁会死锁）
        reset_cache_for_tests();
    }

    /// 活跃窗口边界（spec §4.2/§4.3 共用取值）：恰好 90s 视为已过期
    #[test]
    fn active_within_boundary() {
        let now = std::time::SystemTime::now();
        assert!(active_within(now - std::time::Duration::from_secs(89), now));
        assert!(
            !active_within(now - std::time::Duration::from_secs(ACTIVE_SECS), now),
            "距 now 恰好 ACTIVE_SECS → 已过期（严格小于才活跃）"
        );
    }

    /// review 修复：并发截断微竞态（partial.len() > size）下 resume_at 不下溢
    /// panic——饱和到 0（调用方 next 轮由 last_size 门控/护栏接管）
    #[test]
    fn resume_at_saturates_on_concurrent_truncation() {
        let mut st = IncrState::default();
        st.resume_at(5, vec![0u8; 10]); // 修复前：5 - 10 → debug 构建下溢 panic
        assert_eq!(st.offset, 0);
        assert_eq!(st.last_size, 5);
    }

    #[test]
    fn token_usage_total_saturating() {
        let t = TokenUsage {
            input: u64::MAX,
            cache_read: 1,
            cache_creation: 0,
            output: 0,
        };
        assert_eq!(t.total(), u64::MAX, "四桶合计饱和加法，不 panic");
    }

    /// SubagentStatus 序列化为小写单词（端点载荷契约，T3 契约矩阵的单元层锁）
    #[test]
    fn subagent_status_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&SubagentStatus::Running).unwrap(),
            "\"running\""
        );
        assert_eq!(
            serde_json::to_string(&SubagentStatus::Idle).unwrap(),
            "\"idle\""
        );
    }

    // ==== #151：回报打标时间关联判据（边界表）====

    /// report_anchor_near_last 边界：锚 ≤ last+skew ∧ last−anchor ≤ window
    #[test]
    fn report_anchor_near_last_boundaries() {
        let last = 1_000_000_000_000i64;
        // 窗内：回报距父末条 89_999 → 命中
        assert!(report_anchor_near_last(last - 89_999, last), "窗内 89_999 命中");
        // 窗外：90_001 → 不命中（90_000 恰在窗沿也命中——≤ window）
        assert!(!report_anchor_near_last(last - 90_001, last), "窗外 90_001 拒");
        assert!(report_anchor_near_last(last - 90_000, last), "窗沿 90_000 命中（≤）");
        // 锚晚于 last：容差内（≤ skew）命中、超容差拒
        assert!(report_anchor_near_last(last + 2_000, last), "锚=last+skew 命中（≤）");
        assert!(!report_anchor_near_last(last + 2_001, last), "锚>last+skew 拒");
        assert!(!report_anchor_near_last(last + 100_000, last), "锚远晚于 last 拒");
        // 锚==last → 命中
        assert!(report_anchor_near_last(last, last), "锚==last 命中");
        // last 早于锚的极远端（i64 域无溢出 panic；saturating 语义下仍拒）
        assert!(
            !report_anchor_near_last(i64::MAX, i64::MIN),
            "极值不 panic，远超容差拒（saturating_sub 饱和到 i64::MAX > window）"
        );
        // 加法支路极值（评审 N-1）：last 接近 i64::MAX 时 +skew 曾可溢出——
        // saturating_add 后远晚于锚，仍正确拒且不 panic
        assert!(
            !report_anchor_near_last(i64::MIN, i64::MAX),
            "last=i64::MAX 不因 +skew 溢出（saturating_add），锚远早于 last 拒"
        );
    }

    /// iso_to_ms：合法 rfc3339 → 毫秒；非法形态 → None（保守不打标）
    #[test]
    fn iso_to_ms_valid_and_invalid() {
        assert_eq!(iso_to_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(iso_to_ms("1969-12-31T23:59:59Z"), Some(-1_000), "纪元前为负毫秒");
        assert_eq!(iso_to_ms("2026-10-18T00:00:00Z"), Some(1_792_281_600_000));
        // ms_to_iso 的逆（round-trip，顺带锁时区偏移形态）
        let ms = 1_760_000_000_123i64;
        assert_eq!(ms_to_iso(ms).as_deref().and_then(iso_to_ms), Some(ms));
        assert_eq!(iso_to_ms("not a timestamp"), None);
        assert_eq!(iso_to_ms(""), None);
        assert_eq!(iso_to_ms("2026-10-18"), None, "缺时间部分的日期不算 rfc3339");
    }
}
