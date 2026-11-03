//! 会话扫描预算层（三层通用优化，2026-09-10 用户决策）
//!
//! 背景：前端每 3 秒轮询 `get_all_sessions`，解析器若每轮全量重扫历史会话文件，
//! CPU 会被持续占满（实测 `~/.codex/sessions` 2.0GB / 388 文件时主线程 100%）。
//! 成因：各工具会话归属信息的存放位置不同（目录名 / 索引文件 / DB 列 / 心跳 /
//! 配置均可解析前预过滤；codex 只在文件内容里，只能靠解析来发现），而七个
//! parser 独立演化、没有共享的解析预算契约——好与坏各自为政。
//!
//! 三层策略（对所有 AgentAdapter 通用，三道环强制执行）：
//! - **L1 零进程短路**：`adapter::scan_bus` 的每轮扫描编排统一执行——某工具进程为空
//!   时根本不调用其 `find_sessions`（契约见 trait 文档；有遍历全部注册 adapter 的
//!   防回归测试；各 parser 自带的空判早退保留为纵深防御）。
//! - **L2 内容摘要缓存**：`(mtime, size)` 不变 ⇒ 文件内容不变 ⇒ 解析产物直接复用。
//!   解析必须拆成「纯内容产物（进缓存）+ 时间叠加（每次现算）」——状态判定里的
//!   recently-modified / mtime 停更叠加依赖当前时间，不能进缓存。
//! - **L3 新鲜窗口**（仅无界历史扫描需要）：`SCAN_FRESH_SECS`（24h，用户拍板）
//!   窗口外文件不解析，仅缓存已有摘要时参与匹配；活动进程在窗口内找不到匹配
//!   才回退全量解析（保住"空闲超窗的活跃会话卡"，回退结果进缓存只付一次代价）。
//!   进程界定的有界扫描（claude 目录名匹配 / kimi 索引 / workbuddy 心跳直达）
//!   只需 L1+L2——窗口化反而会丢空闲超窗的活跃卡，缓存已使旧文件近似零成本。
//!
//! 各工具接入：
//! - codex：L1+L2+L3 全接（唯一无界历史扫描者，本次性能问题的主战场）
//! - claude / kimi / workbuddy：L1+L2（进程界定有界扫描）
//! - opencode / zcode：SQLite 单库查询即过滤，L2/L3 不适用，仅享 L1
//! - openclaw：配置 + workspace 匹配，IO 极小，仅享 L1
//!
//! 新工具接入模板（AGENTS.md「Agent Adapter 模式」同款要求）：
//! ```text
//! const SCAN: SessionFileScan = SessionFileScan::new("mytool");
//! let scanned = SCAN.collect(dir, accept);                  // 全量收集，mtime 倒序
//! let digests = scanned.map(|(f, m)|                        // L3 分流
//!     if fresh_within(m, now) { SCAN.parse(f, read_digest) }
//!     else { SCAN.peek(f).unwrap_or_else(|| Arc::new(None)) });
//! // 匹配失败 → SCAN.fill_uncached(files, digests, read_digest) 兜底
//! SCAN.retain_existing(&live_files);                        // 清孤儿缓存
//! ```

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

/// L3 新鲜窗口（用户拍板 24h；12h 亦可，收敛更狠但空闲会话卡更早消失）
pub(crate) const SCAN_FRESH_SECS: u64 = 24 * 60 * 60;

/// mtime 是否落在新鲜窗口内（纯函数，now 注入便于测试边界）
pub(crate) fn fresh_within(mtime: SystemTime, now: SystemTime) -> bool {
    now.duration_since(mtime)
        .map(|d| d.as_secs() <= SCAN_FRESH_SECS)
        .unwrap_or(false)
}

#[derive(Clone)]
struct CacheEntry {
    mtime: SystemTime,
    size: u64,
    value: Arc<dyn Any + Send + Sync>,
}

fn registry() -> &'static Mutex<HashMap<(&'static str, PathBuf), CacheEntry>> {
    static REGISTRY: OnceLock<Mutex<HashMap<(&'static str, PathBuf), CacheEntry>>> =
        OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 按 namespace 隔离的文件解析缓存（同一文件在不同工具/用途下的产物互不串扰）
pub(crate) struct FileParseCache {
    ns: &'static str,
}

/// 防御上限：条目数超限时整 namespace 清空（正常活跃会话远达不到；
/// 只在"会话文件疯狂增殖"的病态环境下触发，宁可重新解析也不吃内存）
const EVICT_HARD_CAP: usize = 8192;

impl FileParseCache {
    pub(crate) const fn new(ns: &'static str) -> Self {
        Self { ns }
    }

    /// 取解析产物：stat 失败（文件消失/不可读）→ 直接现算不缓存（保持与无缓存时
    /// 一致的语义）；`(mtime, size)` 命中 → 返回缓存克隆；未命中 → 现算并写入。
    pub(crate) fn get_or_parse<T, F>(&self, path: &Path, parse: F) -> Arc<T>
    where
        T: Send + Sync + 'static,
        F: FnOnce(&Path) -> T,
    {
        let Some((mtime, size)) = stat_key(path) else {
            return Arc::new(parse(path));
        };
        if let Some(hit) = self.locked().get(&(self.ns, path.to_path_buf())) {
            if hit.mtime == mtime && hit.size == size {
                // 存入时 Arc<T> 上转，胖指针指向的具体类型是 T——用 Arc::downcast 还原
                if let Ok(v) = hit.value.clone().downcast::<T>() {
                    return v;
                }
                // 类型不匹配 = namespace 被误用（同 ns 存了不同 T），按未命中覆盖
            }
        }
        let value = Arc::new(parse(path));
        let mut map = self.locked();
        if map.len() >= EVICT_HARD_CAP {
            map.retain(|k, _| k.0 != self.ns);
        }
        map.insert(
            (self.ns, path.to_path_buf()),
            CacheEntry {
                mtime,
                size,
                value: value.clone() as Arc<dyn Any + Send + Sync>,
            },
        );
        value
    }

    /// 只读缓存不解析：L3 窗口外的陈旧文件用它取"曾经解析过的摘要"参与匹配；
    /// 从未解析过 → None（调用方据此决定是否走全量回退）。
    pub(crate) fn peek<T>(&self, path: &Path) -> Option<Arc<T>>
    where
        T: Send + Sync + 'static,
    {
        let guard = self.locked();
        let hit = guard.get(&(self.ns, path.to_path_buf()))?.clone();
        hit.value.downcast::<T>().ok()
    }

    /// 收敛缓存：清掉已从磁盘消失的文件条目（每轮扫描后由持有全量文件列表的
    /// parser 调用，防孤儿条目常驻）
    pub(crate) fn retain_existing(&self, live: &HashSet<PathBuf>) {
        self.locked()
            .retain(|(ns, path), _| *ns != self.ns || live.contains(path));
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, HashMap<(&'static str, PathBuf), CacheEntry>> {
        registry()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// 会话文件扫描流水线：文件类解析器的统一入口（模块文档含接入模板）。
/// 以 namespace 隔离缓存；`collect` 全量收集，L3 窗口分流由调用方用
/// `fresh_within` 完成（新鲜 → `parse`，陈旧 → `peek`，匹配失败 → `fill_uncached`）。
pub(crate) struct SessionFileScan {
    cache: FileParseCache,
}

impl SessionFileScan {
    pub(crate) const fn new(ns: &'static str) -> Self {
        Self {
            cache: FileParseCache::new(ns),
        }
    }

    /// 递归收集目录下 `accept` 命中的会话文件（mtime 倒序）。
    /// 目录不存在 / 不可读 → 空（与各 parser 原有的防御语义一致）。
    /// 刻意不做窗口过滤：回退路径需要全量清单，L3 分流由调用方完成。
    pub(crate) fn collect(
        &self,
        dir: &Path,
        accept: impl Fn(&Path) -> bool,
    ) -> Vec<(PathBuf, SystemTime)> {
        let mut files: Vec<(PathBuf, SystemTime)> = Vec::new();
        Self::collect_inner(dir, &accept, &mut files);
        files.sort_by_key(|(_, m)| std::cmp::Reverse(*m));
        files
    }

    fn collect_inner(
        dir: &Path,
        accept: &impl Fn(&Path) -> bool,
        files: &mut Vec<(PathBuf, SystemTime)>,
    ) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                Self::collect_inner(&path, accept, files);
            } else if accept(&path) {
                if let Ok(modified) = entry.metadata().and_then(|m| m.modified()) {
                    files.push((path, modified));
                }
            }
        }
    }

    /// L2：解析产物走 (mtime,size) 缓存（`parse_fn` 必须是纯内容函数，时间叠加在外层现算）
    pub(crate) fn parse<T, F>(&self, file: &Path, parse_fn: F) -> Arc<T>
    where
        T: Send + Sync + 'static,
        F: FnOnce(&Path) -> T,
    {
        self.cache.get_or_parse(file, parse_fn)
    }

    /// L3：窗口外陈旧文件只读缓存不解析；从未解析过 → None
    pub(crate) fn peek<T>(&self, file: &Path) -> Option<Arc<T>>
    where
        T: Send + Sync + 'static,
    {
        self.cache.peek(file)
    }

    /// L3 回退兜底：把 `entries` 中仍为 None（陈旧且从未解析）的槽位全量解析一遍。
    /// 结果进缓存——全量代价整个应用生命周期只付一次。`entries` 与 `files` 按索引对齐。
    pub(crate) fn fill_uncached<T, F>(
        &self,
        files: &[PathBuf],
        entries: &mut [Arc<Option<T>>],
        parse_fn: F,
    ) where
        T: Send + Sync + 'static,
        F: Fn(&Path) -> Option<T> + Copy,
    {
        for (f, e) in files.iter().zip(entries.iter_mut()) {
            if e.is_none() {
                *e = self.cache.get_or_parse(f, parse_fn);
            }
        }
    }

    /// 收敛缓存：清掉已从磁盘消失的文件条目（每轮扫描后由持有全量清单的 parser 调用）
    pub(crate) fn retain_existing(&self, live: &HashSet<PathBuf>) {
        self.cache.retain_existing(live);
    }
}

fn stat_key(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpfile(tag: &str, content: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mam_scan_test_{}_{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("f.txt");
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn cache_hit_skips_reparse() {
        let p = tmpfile("hit", "v1");
        let cache = FileParseCache::new("test-hit");
        let mut calls = 0;
        let a = cache.get_or_parse(&p, |_: &Path| {
            calls += 1;
            "first".to_string()
        });
        let b = cache.get_or_parse(&p, |_: &Path| {
            calls += 1;
            "second".to_string()
        });
        assert_eq!(calls, 1, "mtime/size 未变，第二次应命中缓存");
        assert!(Arc::ptr_eq(&a, &b), "命中应返回同一份 Arc");
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn rewrite_invalidates() {
        let p = tmpfile("inv", "short");
        let cache = FileParseCache::new("test-inv");
        let a = cache.get_or_parse(&p, |_: &Path| "v1".to_string());
        std::fs::write(&p, "much longer content v2").unwrap();
        let b = cache.get_or_parse(&p, |_: &Path| "v2".to_string());
        assert_ne!(*a, *b, "size 变化应失效缓存");
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn missing_file_falls_back_to_direct_parse() {
        let ghost = std::env::temp_dir().join("mam_scan_test_ghost_missing.txt");
        let cache = FileParseCache::new("test-ghost");
        let v = cache.get_or_parse(&ghost, |p: &Path| p.to_string_lossy().into_owned());
        assert!(!v.is_empty());
        // stat 失败不写缓存：peek 不到
        assert!(cache.peek::<String>(&ghost).is_none());
    }

    #[test]
    fn peek_never_parses_and_respects_namespace() {
        let p = tmpfile("peek", "x");
        let ns1 = FileParseCache::new("test-peek-a");
        let ns2 = FileParseCache::new("test-peek-b");
        assert!(ns1.peek::<String>(&p).is_none(), "未解析过应返回 None");
        let _ = ns1.get_or_parse(&p, |_: &Path| 42u32);
        assert_eq!(*ns1.peek::<u32>(&p).unwrap(), 42);
        assert!(ns2.peek::<u32>(&p).is_none(), "namespace 隔离");
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn retain_existing_evicts_vanished() {
        let p1 = tmpfile("retain", "1");
        let p2 = p1.parent().unwrap().join("gone.txt");
        std::fs::write(&p2, "2").unwrap();
        let cache = FileParseCache::new("test-retain");
        let _ = cache.get_or_parse(&p1, |_: &Path| "a".to_string());
        let _ = cache.get_or_parse(&p2, |_: &Path| "b".to_string());
        std::fs::remove_file(&p2).unwrap();
        let live: HashSet<PathBuf> = [p1.clone()].into_iter().collect();
        cache.retain_existing(&live);
        assert!(cache.peek::<String>(&p1).is_some(), "存活文件保留");
        assert!(cache.peek::<String>(&p2).is_none(), "消失文件清除");
        std::fs::remove_dir_all(p1.parent().unwrap()).ok();
    }

    #[test]
    fn fresh_window_boundary() {
        let now = SystemTime::now();
        let in_window = now - std::time::Duration::from_secs(SCAN_FRESH_SECS);
        let out_window = now - std::time::Duration::from_secs(SCAN_FRESH_SECS + 1);
        assert!(fresh_within(in_window, now), "恰好 24h 算新鲜");
        assert!(!fresh_within(out_window, now), "超出 1 秒即过期");
    }

    #[test]
    fn collect_orders_desc_and_filters_by_accept() {
        let dir = std::env::temp_dir().join(format!("mam_scan_collect_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("2026/09")).unwrap();
        std::fs::write(dir.join("rollout-a.jsonl"), "a").unwrap();
        std::fs::write(dir.join("2026/09/rollout-b.jsonl"), "b").unwrap();
        std::fs::write(dir.join("2026/09/other.txt"), "x").unwrap();
        let scan = SessionFileScan::new("test-collect");
        let is_rollout = |p: &Path| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("rollout") && n.ends_with(".jsonl"))
                .unwrap_or(false)
        };
        let got = scan.collect(&dir, is_rollout);
        assert_eq!(got.len(), 2, "递归收集 + accept 过滤");
        assert!(got[0].1 >= got[1].1, "mtime 倒序");
        let names: Vec<String> = got
            .iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"rollout-a.jsonl".to_string()));
        assert!(names.contains(&"rollout-b.jsonl".to_string()));
        // 目录不存在 → 空
        assert!(scan.collect(&dir.join("nope"), is_rollout).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fill_uncached_only_fills_none_slots() {
        let dir = std::env::temp_dir().join(format!("mam_scan_fill_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f1 = dir.join("one.jsonl");
        let f2 = dir.join("two.jsonl");
        std::fs::write(&f1, "1").unwrap();
        std::fs::write(&f2, "2").unwrap();
        let scan = SessionFileScan::new("test-fill");
        let files = vec![f1.clone(), f2.clone()];
        let calls = std::sync::atomic::AtomicUsize::new(0);
        // Copy 约束下的计数闭包（捕获引用是 Copy 的 Fn）
        let parse = |_: &Path| {
            use std::sync::atomic::Ordering;
            calls.fetch_add(1, Ordering::SeqCst);
            Some("digest".to_string())
        };
        // 预置：f1 已有缓存摘要（Some），f2 为 None（陈旧未解析）
        let mut entries = vec![
            scan.parse(&f1, |_: &Path| Some("digest".to_string())),
            Arc::new(None),
        ];
        scan.fill_uncached(&files, &mut entries, parse);
        // f1 已有槽位 → 不重解析；f2 为 None → 恰好解析一次
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "仅 None 槽位被解析一次"
        );
        assert_eq!(
            entries[1].as_ref().as_ref().map(|s| s.as_str()),
            Some("digest")
        );
        // 再跑一遍：全部已填，零新增解析
        scan.fill_uncached(&files, &mut entries, parse);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
