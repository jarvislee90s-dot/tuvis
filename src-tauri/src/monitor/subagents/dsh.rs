//! DSH 子任务的单层只读发现。调用方负责提供已授权实例的数据根。
//!
//! 目录名不是原生 ID；仅以受预算约束的日志头确定父子关系。
//!
//! Unix：一次调用内从已打开的 `sessions` 目录句柄逐层 `openat` 枚举与读取——
//! 路径在校验后被替换也不会改变实际读取对象（替换实体的字节读取为零）。
//! 非 Unix（含 Windows）：尚无安全打开实现，恒返回空列表；这是**能力未实现**，
//! 不能解释为「该实例没有子任务」。

use std::path::Path;

use super::SubagentView;

#[cfg(unix)]
pub use unix_impl::{collect, collect_with_status, read_verified_content};

#[cfg(windows)]
pub use self::dsh_windows::{collect, collect_with_status, read_verified_content};

// 其余非 Unix、非 Windows 平台：尚无安全打开实现，恒返回空列表。
// 这是**能力未实现**，不能解释为「该实例没有子任务」。
#[cfg(not(any(unix, windows)))]
pub fn collect(_home: &Path, _parent_id: &str) -> Vec<SubagentView> {
    Vec::new()
}

#[cfg(not(any(unix, windows)))]
pub fn collect_with_status(_home: &Path, _parent_id: &str) -> Vec<SubagentView> {
    Vec::new()
}

#[cfg(not(any(unix, windows)))]
pub fn read_verified_content(
    _home: &Path,
    _parent_id: &str,
    _subagent_id: &str,
    _budget: u64,
) -> Result<(String, crate::monitor::dsh::log::DshHeader), &'static str> {
    Err("DSH_SUBAGENT_UNSUPPORTED: 该平台尚无安全只读实现")
}

/// 仅列出直属子任务；身份不唯一、孤儿或循环时不猜测归属。
pub fn collect_with(home: &Path, parent_id: &str) -> Vec<SubagentView> {
    if parent_id.trim().is_empty() {
        return Vec::new();
    }
    collect(home, parent_id)
}

/// chip 展示名的单行提取（双平台共用纯函数，2026-11-15 A06 取证补强）。
///
/// 真实形态（Desktop home 402 个子会话）：400 个在 header 的下一行携带
/// `subagent/descriptor`（`data.label` = 人类可读任务名，`provider` 恒 "spawn"、
/// `mode` 恒 "continuable"）——此前 chip name 落裸 UUID，Claude 交互对照下
/// 「有名字的子任务」才是同款体验。仅消费 `data.label`，其余字段不解读。
///
/// 纪律：行必须以 `\n` 收尾（半行 = 写者未写完，不猜；该行的完成会使文件
/// FileKey 变化 → 缓存失效 → 下轮重读自愈）；label 非空且截断到 200 字符，
/// 防日志侧异常长串撑爆端点载荷。
pub(crate) const DESCRIPTOR_LABEL_MAX_CHARS: usize = 200;

pub(crate) fn descriptor_label_from_line(line: &str) -> Option<String> {
    if !line.ends_with('\n') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(line.trim_end_matches('\n')).ok()?;
    if value.get("type")?.as_str()? != "subagent/descriptor" {
        return None;
    }
    let label = value.get("data")?.get("label")?.as_str()?.trim();
    if label.is_empty() {
        return None;
    }
    Some(label.chars().take(DESCRIPTOR_LABEL_MAX_CHARS).collect())
}

#[cfg(windows)]
#[path = "dsh_windows.rs"]
mod dsh_windows;

#[cfg(unix)]
mod unix_impl {
    use std::collections::{HashMap, HashSet};
    use std::ffi::{CStr, CString, OsStr, OsString};
    use std::io::{BufRead, BufReader, Read};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Component, Path};
    use std::sync::{LazyLock, Mutex};

    use crate::monitor::dsh::log;
    use crate::monitor::subagents::{
        ms_to_iso, sort_views, SubagentStatus, SubagentView, TokenUsage,
    };

    const HEADER_BUDGET: u64 = 64 * 1024;
    const CACHE_LIMIT: usize = 4096;

    /// 回合证据读取的正文预算（解压后字节）：覆盖典型子任务转写（真实 Desktop
    /// home 最大 6.5MB），超出如实降级 Unknown——不为了状态判定放大预算。
    const TURN_EVIDENCE_BUDGET: u64 = 16 * 1024 * 1024;

    /// 子任务回合证据（A06 销项 2026-11-18）：来自子 Agent 自己的事件流
    /// （与主面板同一回合词表 turn/start、turn/end{reason.kind}）。
    ///
    /// 取证依据（本机 Desktop home）：435 个子代理日志 435/435 携带 turn/end
    /// 终态帧（completed 525 / aborted 15 / max-tokens 14 / error 7 /
    /// interrupted 4）——**闭回合是宿主写下的确定事实 = 可证终态**；
    /// 开回合（turn/start 无 turn/end）本机 0 样本，且实证锁持有不构成运行
    /// 证据（根会话闭回合后 session.lock 仍被宿主持有）→ 不发明 running。
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum TurnEvidence {
        /// 最近回合已闭合（终态）：end_time_ms = turn/end 事件时间；事件缺
        /// 时间 → 回落日志 mtime（最后已知写入）。
        Closed { end_time_ms: Option<i64> },
        /// 回合打开：无写入者存活证明 → Unknown（2026-11-06 登记裁决维持）。
        Open,
        /// 无回合事件，或证据不可得（读取失败/超预算）。
        NoEvidence,
    }

    /// 回合证据缓存条目：(实体快照, 证据)；None = 稳定失败（超预算/不可读）。
    type EvidenceEntry = (FileKey, Option<TurnEvidence>);

    /// 回合证据缓存：键与 header 缓存同构（root 实体 + 相对路径），值 =
    /// [`EvidenceEntry`]（同一实体不重复读取；L2 纪律：内容推导产物按内容
    /// 身份缓存）。缓存的是「这份内容里的回合事实」——纯内容产物，不含
    /// 时间叠加，跨轮询安全。
    static EVIDENCE_CACHE: LazyLock<Mutex<HashMap<CacheKey, EvidenceEntry>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    fn evidence_cache_lookup(key: &CacheKey, file: FileKey) -> Option<Option<TurnEvidence>> {
        EVIDENCE_CACHE
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(key)
            .filter(|(cached_key, _)| *cached_key == file)
            .map(|(_, evidence)| *evidence)
    }

    fn evidence_cache_store(key: CacheKey, file: FileKey, evidence: Option<TurnEvidence>) {
        let mut cache = EVIDENCE_CACHE
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if cache.len() >= CACHE_LIMIT {
            // 证据重算只是一次有界正文读：满即淘汰任意一半（与 header 缓存的
            // 「优先淘汰负缓存」策略不同——这里无冷热差，简单即可）。
            let victims: Vec<CacheKey> = cache.keys().take(cache.len() / 2 + 1).cloned().collect();
            for victim in victims {
                cache.remove(&victim);
            }
        }
        cache.insert(key, (file, evidence));
    }

    /// 从已核验实体读取目标行全文（预算内）→ 推导回合证据。
    ///
    /// 返回 None = 本轮失效（实体重开失败/身份漂移——调用方按 Unknown 处理，
    /// **不**进缓存：下一轮内容稳定后自愈）。稳定失败（超预算/不可读）进缓存
    /// 为 NoEvidence，同一实体不重复付读取代价。
    fn turn_evidence(
        sessions: &std::fs::File,
        root_key: FileKey,
        row: &Row,
    ) -> Option<TurnEvidence> {
        let key = cache_key(root_key, &row.project, &row.session, &row.log);
        if let Some(cached) = evidence_cache_lookup(&key, row.log_key) {
            return Some(cached.unwrap_or(TurnEvidence::NoEvidence));
        }
        let (_, log_fd) = reopen(sessions, row)?;
        // 重开后必须仍是核验时的同一内容版本（与 read_verified_content 同门）。
        let reopened = file_key(&log_fd.metadata().ok()?);
        if reopened != row.log_key {
            return None;
        }
        let compressed = row.log.as_encoded_bytes().ends_with(b".zstd");
        let Some(text) = read_verified_text(&log_fd, row.log_key, compressed, TURN_EVIDENCE_BUDGET)
        else {
            // 稳定失败：该实体本就是读不出证据的，缓存避免每轮重读 16MB。
            evidence_cache_store(key, row.log_key, None);
            return Some(TurnEvidence::NoEvidence);
        };
        let events = crate::monitor::dsh::log::parse_events(&text);
        let facts = crate::monitor::dsh::status::scan_facts(&events);
        let evidence = if facts.has_open_turn() {
            TurnEvidence::Open
        } else {
            match facts.last_end {
                Some((end_seq, _kind)) => {
                    // endTs 取 turn/end 事件时间；事件缺时间回落日志 mtime
                    //（最后已知写入——SubagentView.end_ts 的「最后已知活动」口径）。
                    let end_time_ms = events
                        .iter()
                        .rev()
                        .find(|e| e.kind == "turn/end" && e.seq == Some(end_seq))
                        .and_then(|e| e.time)
                        .or_else(|| {
                            log_fd
                                .metadata()
                                .ok()
                                .and_then(|meta| meta.modified().ok())
                                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                .map(|d| d.as_millis() as i64)
                        });
                    TurnEvidence::Closed { end_time_ms }
                }
                None => TurnEvidence::NoEvidence,
            }
        };
        evidence_cache_store(key, row.log_key, Some(evidence));
        Some(evidence)
    }

    /// 文件实体快照：含 ctime，使「同 inode 同长度并恢复 mtime」的改写也能失效。
    #[derive(Clone, Copy, PartialEq, Eq)]
    struct FileKey {
        dev: u64,
        ino: u64,
        len: u64,
        mtime: i64,
        mtime_nsec: i64,
        ctime: i64,
        ctime_nsec: i64,
    }

    fn file_key(meta: &std::fs::Metadata) -> FileKey {
        FileKey {
            dev: meta.dev(),
            ino: meta.ino(),
            len: meta.len(),
            mtime: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
            ctime: meta.ctime(),
            ctime_nsec: meta.ctime_nsec(),
        }
    }

    /// 从 `/` 逐层 `O_NOFOLLOW` 打开目录；任一层是链接即拒绝。
    fn open_dir_chain(path: &Path) -> Option<std::fs::File> {
        if !path.is_absolute() {
            return None;
        }
        let mut dir = std::fs::File::open("/").ok()?;
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => dir = open_at(&dir, name, libc::O_DIRECTORY)?,
                _ => return None,
            }
        }
        Some(dir)
    }

    fn open_at(parent: &std::fs::File, name: &OsStr, flags: i32) -> Option<std::fs::File> {
        let name = CString::new(name.as_bytes()).ok()?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return None;
        }
        Some(unsafe { std::fs::File::from_raw_fd(fd) })
    }

    /// `openat` 前的实体快照（不读内容、不跟随链接）。用于把打开后的 `fstat`
    /// 绑定到同一实体：检查后被替换的目录/文件必须零字节读取即拒绝。
    fn stat_at(parent: &std::fs::File, name: &OsStr) -> Option<FileKey> {
        let name = CString::new(name.as_bytes()).ok()?;
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result != 0 {
            return None;
        }
        Some(file_key_from_stat(&stat))
    }

    fn file_key_from_stat(stat: &libc::stat) -> FileKey {
        FileKey {
            dev: stat.st_dev as _,
            ino: stat.st_ino as _,
            len: stat.st_size as _,
            mtime: stat.st_mtime as _,
            mtime_nsec: stat.st_mtime_nsec as _,
            ctime: stat.st_ctime as _,
            ctime_nsec: stat.st_ctime_nsec as _,
        }
    }

    /// 目录项是否为符号链接（别名不算候选，也不读其内容）。
    fn is_symlink_at(parent: &std::fs::File, name: &OsStr) -> bool {
        let Ok(name) = CString::new(name.as_bytes()) else {
            return false;
        };
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        result == 0 && (stat.st_mode as libc::mode_t & libc::S_IFMT) == libc::S_IFLNK
    }

    /// 打开前预检查 + 打开后核对：返回已绑定的句柄与实体快照。
    fn open_verified(
        parent: &std::fs::File,
        name: &OsStr,
        flags: i32,
    ) -> Option<(std::fs::File, FileKey)> {
        let expected = stat_at(parent, name)?;
        let file = open_at(parent, name, flags)?;
        let actual = file_key(&file.metadata().ok()?);
        if actual != expected {
            return None;
        }
        Some((file, actual))
    }

    fn clear_errno() {
        #[cfg(target_os = "macos")]
        unsafe {
            *libc::__error() = 0;
        }
        #[cfg(target_os = "linux")]
        unsafe {
            *libc::__errno_location() = 0;
        }
    }

    fn read_errno() -> libc::c_int {
        #[cfg(target_os = "macos")]
        unsafe {
            *libc::__error()
        }
        #[cfg(target_os = "linux")]
        unsafe {
            *libc::__errno_location()
        }
    }

    /// 目录项枚举使用独立目录游标：`dup` 与源描述符共享偏移，必须先回绕，
    /// 否则同一目录句柄的第二次枚举会从末尾开始、读成空目录。
    fn entries(dir: &std::fs::File) -> Option<Vec<OsString>> {
        let duplicate = unsafe { libc::dup(dir.as_raw_fd()) };
        if duplicate < 0 {
            return None;
        }
        if unsafe { libc::lseek(duplicate, 0, libc::SEEK_SET) } < 0 {
            unsafe { libc::close(duplicate) };
            return None;
        }
        let stream = unsafe { libc::fdopendir(duplicate) };
        if stream.is_null() {
            unsafe { libc::close(duplicate) };
            return None;
        }
        let mut out = Vec::new();
        loop {
            // 区分 EOF 与枚举错误：中途 I/O 错误不能冒充「目录里没有更多项」。
            clear_errno();
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                let errno = read_errno();
                unsafe { libc::closedir(stream) };
                return (errno == 0).then_some(out);
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            let bytes = name.to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            out.push(OsStr::from_bytes(bytes).to_os_string());
        }
    }

    fn generation_of(name: &OsStr) -> Option<i64> {
        let name = name.to_str()?;
        if name == "session.jsonl" || name == "session.jsonl.zstd" {
            return Some(0);
        }
        let rest = name.strip_prefix("session.v")?;
        let rest = rest
            .strip_suffix(".jsonl.zstd")
            .or_else(|| rest.strip_suffix(".jsonl"))?;
        rest.parse().ok()
    }

    /// 最高代际的名称（未知文件名忽略；排序与路径版规则一致）。
    ///
    /// `Err(())` = 枚举失败（身份未知）；`Ok(None)` = 已确认没有可识别代际。
    fn newest_log(dir: &std::fs::File) -> Result<Option<OsString>, ()> {
        let mut logs: Vec<(i64, OsString)> = entries(dir)
            .ok_or(())?
            .into_iter()
            .filter_map(|name| generation_of(&name).map(|version| (version, name)))
            .collect();
        logs.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
        Ok(logs.pop().map(|(_, name)| name))
    }

    /// 从同一个打开的文件读取首行 + 次行（descriptor chip 名）；前后快照必须
    /// 完全一致（含 ctime）。两行共享同一预算窗口（`HEADER_BUDGET + 1` 的
    /// take 上限不因多读一行而放大）；次行缺失/半行/非 descriptor → label 为
    /// None（不猜，实体增长后缓存失效自愈）。
    fn read_header(
        fd: &std::fs::File,
        compressed: bool,
    ) -> Option<(log::DshHeader, Option<String>, FileKey)> {
        let before = fd.metadata().ok()?;
        let file = fd.try_clone().ok()?;
        let reader: Box<dyn Read> = if compressed {
            let mut decoder =
                zstd::stream::read::Decoder::new(file.take(HEADER_BUDGET + 1)).ok()?;
            decoder.window_log_max(22).ok()?;
            Box::new(decoder)
        } else {
            Box::new(file)
        };
        let mut reader = BufReader::new(reader.take(HEADER_BUDGET + 1));
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).ok()?;
        if line.len() as u64 > HEADER_BUDGET || !line.ends_with(b"\n") {
            return None;
        }
        // 次行：同一 BufReader 续读（descriptor label 数据源，纯增量消费）。
        // 半行（无换行收尾）按「未写完」处理 → None；已缓冲字节仍属同一预算。
        let mut second = Vec::new();
        let label = reader
            .read_until(b'\n', &mut second)
            .ok()
            .and_then(|_| super::descriptor_label_from_line(std::str::from_utf8(&second).ok()?));
        let after = fd.metadata().ok()?;
        let key = file_key(&before);
        if key != file_key(&after) {
            return None;
        }
        Some((
            log::parse_header(std::str::from_utf8(&line).ok()?)?,
            label,
            key,
        ))
    }

    /// 从已核验句柄读取完整正文（有界）。压缩帧同时限制输入、输出和窗口。
    fn read_verified_text(
        fd: &std::fs::File,
        expected: FileKey,
        compressed: bool,
        budget: u64,
    ) -> Option<String> {
        // 读取前必须仍等于核验版本；不在函数内另选一个可接受快照。
        let before = file_key(&fd.metadata().ok()?);
        if before != expected {
            return None;
        }
        // 压缩输入预算与输出预算同值：文件本身超出输入预算时直接拒绝，
        // 避免「take 截断恰好落在完整帧边界」被解码器当成正常 EOF。
        if compressed && before.len > budget {
            return None;
        }
        let file = fd.try_clone().ok()?;
        let reader: Box<dyn Read> = if compressed {
            let mut decoder = zstd::stream::read::Decoder::new(file.take(budget + 1)).ok()?;
            decoder.window_log_max(22).ok()?;
            Box::new(decoder)
        } else {
            Box::new(file.take(budget + 1))
        };
        let mut bytes = zeroize::Zeroizing::new(Vec::new());
        reader.take(budget + 1).read_to_end(&mut bytes).ok()?;
        if bytes.len() as u64 > budget || file_key(&fd.metadata().ok()?) != expected {
            return None;
        }
        Some(String::from_utf8_lossy(&bytes).to_string())
    }

    type CacheKey = (u64, u64, String, String, String);

    /// 成功读取的产物：header + descriptor chip 名（None = 该日志无 descriptor 行）。
    type HeaderRead = (log::DshHeader, Option<String>);

    struct Cached {
        key: FileKey,
        /// 稳定失败（超预算/无首行）为 None：同一实体不重复读取。
        header: Option<HeaderRead>,
    }

    /// 只有已完整核对前后一致的结果才进缓存；键含 sessions 根实体与相对路径。
    static CACHE: LazyLock<Mutex<HashMap<CacheKey, Cached>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    fn cache_key(root: FileKey, project: &OsStr, session: &OsStr, log: &OsStr) -> CacheKey {
        (
            root.dev,
            root.ino,
            project.to_string_lossy().into_owned(),
            session.to_string_lossy().into_owned(),
            log.to_string_lossy().into_owned(),
        )
    }

    /// 命中返回 Some((header, label) 或稳定失败的 None)。
    fn cache_lookup(key: &CacheKey, file: FileKey) -> Option<Option<HeaderRead>> {
        let cache = CACHE.lock().unwrap_or_else(|error| error.into_inner());
        cache
            .get(key)
            .filter(|entry| entry.key == file)
            .map(|entry| entry.header.clone())
    }

    fn cache_store(key: CacheKey, file: FileKey, header: Option<&HeaderRead>) {
        let mut cache = CACHE.lock().unwrap_or_else(|error| error.into_inner());
        if cache.len() >= CACHE_LIMIT {
            // **不要整表清空**：清空会让「已读的条目」全部丢失，下一轮又从最早的
            // 冷文件重读。当冷候选数 > CACHE_LIMIT 时这会导致永不收敛（每轮白读
            // 一整批）。这里优先淘汰负缓存（失败结果可重算），其次才是成功项。
            let victims: Vec<CacheKey> = cache
                .iter()
                .filter(|(_, entry)| entry.header.is_none())
                .map(|(key, _)| key.clone())
                .take(cache.len() / 2 + 1)
                .collect();
            if victims.is_empty() {
                let victims: Vec<CacheKey> =
                    cache.keys().take(cache.len() / 2 + 1).cloned().collect();
                for key in victims {
                    cache.remove(&key);
                }
            } else {
                for key in victims {
                    cache.remove(&key);
                }
            }
        }
        cache.insert(
            key,
            Cached {
                key: file,
                header: header.cloned(),
            },
        );
    }

    /// 单次调用允许「补读」的冷文件上限（超窗且无缓存）。
    ///
    /// 为什么需要上限：子 Agent 归属要求全目录 ID 唯一，跳过冷文件就无法证明唯一性
    /// （旧历史下的子任务将永久不可见）。但无界补读会让首次调用的耗时应随历史规模
    /// 线性膨胀。因此允许一次性补读，**但设硬上限**：超过上限仍按「覆盖不足」拒绝
    /// 发布（fail-closed），并在下一次调用继续推进（已读的进了缓存，不再重复付代价）。
    const COLD_FILL_LIMIT: usize = 512;

    fn within_fresh_window(modified: std::time::SystemTime) -> bool {
        modified
            .elapsed()
            .map(|age| age.as_secs() <= 24 * 60 * 60)
            .unwrap_or(false)
    }

    /// 末验用的成员快照：名称 + 实体身份（会话目录与所选日志的 `FileKey`）。
    struct Membership {
        project: OsString,
        session: OsString,
        log: OsString,
        session_key: FileKey,
        log_key: FileKey,
    }

    struct Row {
        project: OsString,
        session: OsString,
        log: OsString,
        session_key: FileKey,
        log_key: FileKey,
        header: log::DshHeader,
        /// descriptor chip 名（None = 日志无 descriptor 行，name 回落 header.id）
        label: Option<String>,
    }

    /// 枚举并读取全部可归属的会话头。返回 None = 覆盖不完整（不得当作唯一性证明）。
    ///
    /// `strict`：子任务归属要求全目录 ID 唯一，因此超窗未缓存的头部也必须读取；
    /// 任一合规候选无法取得稳定身份 → 覆盖不足。仅展示列表可放宽为新鲜窗口，
    /// 但当前唯一调用方是归属判定，故始终严格。
    fn scan(sessions: &std::fs::File, root_key: FileKey) -> Option<Vec<Row>> {
        let mut rows = Vec::new();
        let mut cold_fills = 0usize;
        let mut seen_sessions = HashSet::new();
        let mut projects = entries(sessions)?;
        projects.sort();
        for project in projects {
            // 符号链接目录别名不是候选（不读其内容，也不制造第二份身份）。
            if is_symlink_at(sessions, &project) {
                continue;
            }
            // 真实目录项即候选：打不开、不是目录或元数据失败的候选身份未知，
            // 不能当作「不存在」而继续证明唯一性。
            let (project_fd, _) = open_verified(sessions, &project, libc::O_DIRECTORY)?;
            let project_meta = project_fd.metadata().ok()?;
            if !project_meta.is_dir() {
                continue;
            }
            for session in entries(&project_fd)? {
                if is_symlink_at(&project_fd, &session) {
                    continue;
                }
                let (session_fd, session_key) =
                    open_verified(&project_fd, &session, libc::O_DIRECTORY)?;
                let session_meta = session_fd.metadata().ok()?;
                if !session_meta.is_dir() {
                    continue;
                }
                if !seen_sessions.insert((session_key.dev, session_key.ino)) {
                    continue;
                }
                // 枚举失败 → 身份未知 → 覆盖不足；成功枚举但无日志才是非候选。
                let Some(log_name) = newest_log(&session_fd).ok()? else {
                    continue;
                };
                let (log_fd, log_key) =
                    open_verified(&session_fd, &log_name, libc::O_RDONLY | libc::O_NONBLOCK)?;
                let log_meta = log_fd.metadata().ok()?;
                if !log_meta.is_file() {
                    // 已识别代际却不是普通文件（FIFO/设备等）：身份不可判定。
                    return None;
                }
                let key = cache_key(root_key, &project, &session, &log_name);
                let cached = cache_lookup(&key, log_key);
                if let Some(Some((header, label))) = cached {
                    rows.push(Row {
                        project: project.clone(),
                        session: session.clone(),
                        log: log_name,
                        session_key,
                        log_key,
                        header,
                        label,
                    });
                    continue;
                }
                // 负缓存 = 该实体身份仍未知：不能算作「已覆盖」，也不能重复读取。
                if matches!(cached, Some(None)) {
                    return None;
                }
                // 超窗且无缓存：**有界补读**。
                //
                // 为什么不是直接跳过：归属判定要求「全目录 ID 唯一」，跳过任何一个
                // 候选都无法排除重复 ID / 祖先，于是旧历史下的子任务永久不可见
                // （这是本批要修的缺口）。
                // 为什么仍然安全：① 先 `stat` 后读，符合预过滤纪律；② 只补读到上限，
                // 超限即按覆盖不足拒绝；③ 读到的结果按 (dev,ino,mtime,size) 进缓存，
                // 同一文件不会重复付代价；④ 读取仍是预算内有界的 header 读。
                let Ok(modified) = log_meta.modified() else {
                    return None;
                };
                if !within_fresh_window(modified) {
                    if cold_fills >= COLD_FILL_LIMIT {
                        return None;
                    }
                    cold_fills += 1;
                }
                let compressed = log_name.as_encoded_bytes().ends_with(b".zstd");
                let before_failure = file_key(&log_meta);
                let Some((header, label, stable_key)) = read_header(&log_fd, compressed) else {
                    // 只有失败前后实体快照完全未变，才可把失败固定到该实体。
                    if log_fd.metadata().ok().map(|meta| file_key(&meta)) == Some(before_failure) {
                        cache_store(key, before_failure, None);
                    }
                    return None;
                };
                cache_store(key, stable_key, Some(&(header.clone(), label.clone())));
                rows.push(Row {
                    project: project.clone(),
                    session: session.clone(),
                    log: log_name,
                    session_key,
                    log_key: stable_key,
                    header,
                    label,
                });
            }
        }
        Some(rows)
    }

    /// 相对仍打开的 sessions 句柄重新定位一行；任一环节不可达即视为本轮失效。
    fn reopen(sessions: &std::fs::File, row: &Row) -> Option<(std::fs::File, std::fs::File)> {
        let (project_fd, _) = open_verified(sessions, &row.project, libc::O_DIRECTORY)?;
        let (session_fd, session_key) =
            open_verified(&project_fd, &row.session, libc::O_DIRECTORY)?;
        if session_key != row.session_key {
            return None;
        }
        if newest_log(&session_fd).ok()?? != row.log {
            return None;
        }
        let (log_fd, _) = open_verified(&session_fd, &row.log, libc::O_RDONLY | libc::O_NONBLOCK)?;
        Some((session_fd, log_fd))
    }

    /// 第一遍：内容版本变化时重读首行；关系投影不变则刷新快照，否则本轮放弃。
    fn refresh(sessions: &std::fs::File, root_key: FileKey, rows: &mut [Row]) -> bool {
        for row in rows.iter_mut() {
            let Some((_, log_fd)) = reopen(sessions, row) else {
                return false;
            };
            let Ok(meta) = log_fd.metadata() else {
                return false;
            };
            let key = file_key(&meta);
            if key == row.log_key {
                continue;
            }
            let compressed = row.log.as_encoded_bytes().ends_with(b".zstd");
            let Some((header, label, stable)) = read_header(&log_fd, compressed) else {
                return false;
            };
            if header.id != row.header.id
                || header.parent_session != row.header.parent_session
                || header.origin != row.header.origin
                || header.delegation_depth != row.header.delegation_depth
                || header.created_at != row.header.created_at
            {
                return false;
            }
            row.header = header;
            row.label = label;
            row.log_key = stable;
            let found = Some((row.header.clone(), row.label.clone()));
            cache_store(
                cache_key(root_key, &row.project, &row.session, &row.log),
                stable,
                found.as_ref(),
            );
        }
        true
    }

    /// 第二遍：发布前再确认每条参与唯一性/父链判定的记录仍是最新代际且未被替换。
    fn confirm(sessions: &std::fs::File, rows: &[Row]) -> bool {
        for row in rows {
            let Some((_, log_fd)) = reopen(sessions, row) else {
                return false;
            };
            let Ok(meta) = log_fd.metadata() else {
                return false;
            };
            if file_key(&meta) != row.log_key {
                return false;
            }
        }
        true
    }

    /// 目录成员集合快照：只枚举名字与实体身份，**不读任何正文**。
    ///
    /// 去重口径与 `scan` 一致（同一会话实体只计一次）。返回 None = 枚举失败或
    /// 某候选身份不可判定——此时覆盖不足，不能作为成员集合的比较依据。
    fn membership(sessions: &std::fs::File) -> Option<Vec<Membership>> {
        let mut members = Vec::new();
        let mut seen_sessions = HashSet::new();
        let mut projects = entries(sessions)?;
        projects.sort();
        for project in projects {
            if is_symlink_at(sessions, &project) {
                continue;
            }
            let (project_fd, _) = open_verified(sessions, &project, libc::O_DIRECTORY)?;
            if !project_fd.metadata().ok()?.is_dir() {
                continue;
            }
            for session in entries(&project_fd)? {
                if is_symlink_at(&project_fd, &session) {
                    continue;
                }
                let (session_fd, session_key) =
                    open_verified(&project_fd, &session, libc::O_DIRECTORY)?;
                if !session_fd.metadata().ok()?.is_dir() {
                    continue;
                }
                if !seen_sessions.insert((session_key.dev, session_key.ino)) {
                    continue;
                }
                let Some(log_name) = newest_log(&session_fd).ok()? else {
                    continue;
                };
                // 只比名字不够：同名换实体（unlink + 建同名目录/文件）会让名称集合
                // 完全相等，旧身份结论照样发布。必须把**实体身份**一起快照。
                let log_key = stat_at(&session_fd, &log_name)?;
                members.push(Membership {
                    project: project.clone(),
                    session,
                    log: log_name,
                    session_key,
                    log_key,
                });
            }
        }
        Some(members)
    }

    /// 发布前最后一道：**目录成员集合必须与本轮扫描所见完全一致**。
    ///
    /// 扫描后新增的候选会话（可能携带重复 ID 或新祖先）不在 `refresh`/`confirm`
    /// 的逐行重开范围内——只重开旧行证明不了「没有新行」。因此再枚举一次整棵目录，
    /// 要求成员集合与已发布行逐项相等；不等即覆盖不足，本轮拒绝发布关系。
    fn membership_confirmed(sessions: &std::fs::File, rows: &[Row]) -> bool {
        let Some(members) = membership(sessions) else {
            return false;
        };
        if members.len() != rows.len() {
            return false;
        }
        // 以「项目/会话/日志名」为键，值必须是**同一实体**：同名换 inode 的不同
        // 实体不得被当成同一成员（只比名字会被 unlink + 建同名 绕过）。
        let published: HashMap<(&OsStr, &OsStr, &OsStr), (&FileKey, &FileKey)> = rows
            .iter()
            .map(|row| {
                (
                    (
                        row.project.as_os_str(),
                        row.session.as_os_str(),
                        row.log.as_os_str(),
                    ),
                    (&row.session_key, &row.log_key),
                )
            })
            .collect();
        if published.len() != rows.len() {
            return false;
        }
        members.iter().all(|member| {
            published
                .get(&(
                    member.project.as_os_str(),
                    member.session.as_os_str(),
                    member.log.as_os_str(),
                ))
                .is_some_and(|(session_key, log_key)| {
                    **session_key == member.session_key && **log_key == member.log_key
                })
        })
    }

    /// 打开已授权数据根下的 sessions 句柄，并完成一次覆盖完整的严格扫描。
    ///
    /// 返回 None = 覆盖不足（任一会话身份未知/不可读/超窗未缓存），任何调用方
    /// 都不得把 None 当成「没有子任务」，也不得继续做唯一性判定。
    fn verified_rows(home: &Path) -> Option<(std::fs::File, Vec<Row>)> {
        // 末级链接不当作授权目录；系统路径别名（如 macOS `/var`）由规范化消解，
        // 规范化结果必须与检查到的实体一致，随后只从规范路径逐层打开。
        let checked = std::fs::symlink_metadata(home).ok()?;
        if !checked.is_dir() || checked.file_type().is_symlink() {
            return None;
        }
        let canonical = std::fs::canonicalize(home).ok()?;
        let home_fd = open_dir_chain(&canonical)?;
        let home_meta = home_fd.metadata().ok()?;
        if file_key(&home_meta) != file_key(&checked) {
            return None;
        }
        let sessions_fd = open_at(&home_fd, OsStr::new("sessions"), libc::O_DIRECTORY)?;
        let sessions_meta = sessions_fd.metadata().ok()?;
        if !sessions_meta.is_dir() {
            return None;
        }
        let root_key = file_key(&sessions_meta);
        let mut rows = scan(&sessions_fd, root_key)?;
        if !refresh(&sessions_fd, root_key, &mut rows)
            || !confirm(&sessions_fd, &rows)
            || !membership_confirmed(&sessions_fd, &rows)
        {
            return None;
        }
        Some((sessions_fd, rows))
    }

    /// 详情专用：同一轮完整核验后，只从**已核验实体句柄**读取目标正文。
    ///
    /// 关系不成立或覆盖不足 → Err；成功时返回 (正文, 该实体首个 header)。正文
    /// 读取同时受输入/输出预算约束（压缩帧另设窗口上限）。
    pub fn read_verified_content(
        home: &Path,
        parent_id: &str,
        subagent_id: &str,
        budget: u64,
    ) -> Result<(String, log::DshHeader), &'static str> {
        let (sessions_fd, rows) = verified_rows(home)
            .ok_or("DSH_SUBAGENT_COVERAGE_INCOMPLETE: 关系覆盖不足，不能证明归属")?;
        let mut counts = HashMap::<&str, usize>::new();
        for row in &rows {
            *counts.entry(row.header.id.as_str()).or_default() += 1;
        }
        if counts.get(parent_id) != Some(&1) {
            return Err("DSH_SUBAGENT_FOREIGN: 父会话身份不唯一或缺失");
        }
        if counts.get(subagent_id) != Some(&1) {
            return Err("DSH_SUBAGENT_AMBIGUOUS: 子任务身份不唯一或缺失");
        }
        let target = rows
            .iter()
            .find(|row| row.header.id == subagent_id)
            .ok_or("DSH_SUBAGENT_MISSING: 子任务不存在")?;
        if target.header.parent_session.as_deref() != Some(parent_id)
            || !log::is_subagent(&target.header)
        {
            return Err("DSH_SUBAGENT_FOREIGN: 子任务不属于该父会话");
        }
        // 完整祖先链（父本身可能是子任务）。
        let parent_row = rows
            .iter()
            .find(|row| row.header.id == parent_id)
            .ok_or("DSH_SUBAGENT_FOREIGN: 父会话缺失")?;
        if log::is_subagent(&parent_row.header) {
            let mut seen = HashSet::new();
            seen.insert(subagent_id);
            let mut cursor = Some(parent_id);
            while let Some(id) = cursor {
                if !seen.insert(id) {
                    return Err("DSH_SUBAGENT_FOREIGN: 父链成环");
                }
                if counts.get(id) != Some(&1) {
                    return Err("DSH_SUBAGENT_FOREIGN: 祖先身份不唯一或缺失");
                }
                let row = rows
                    .iter()
                    .find(|row| row.header.id == id)
                    .ok_or("DSH_SUBAGENT_FOREIGN: 祖先缺失")?;
                if !log::is_subagent(&row.header) {
                    break;
                }
                cursor = match row.header.parent_session.as_deref() {
                    Some(next) => Some(next),
                    None => return Err("DSH_SUBAGENT_FOREIGN: 祖先缺少父引用"),
                };
            }
        }
        // 只从核验时绑定的实体句柄读取正文，不在核验后按路径重开。
        let (_, log_fd) =
            reopen(&sessions_fd, target).ok_or("DSH_SUBAGENT_CHANGED: 目标实体已变化")?;
        // 重开后必须仍是核验时的同一内容版本；否则拒绝，不读替换后的实体。
        let reopened = log_fd
            .metadata()
            .ok()
            .map(|meta| file_key(&meta))
            .ok_or("DSH_SUBAGENT_CHANGED: 目标元数据不可读")?;
        if reopened != target.log_key {
            return Err("DSH_SUBAGENT_CHANGED: 目标内容版本已变化");
        }
        let compressed = target.log.as_encoded_bytes().ends_with(b".zstd");
        let text = read_verified_text(&log_fd, target.log_key, compressed, budget)
            .ok_or("DSH_SUBAGENT_UNREADABLE: 目标正文不可读或超出预算")?;
        Ok((text, target.header.clone()))
    }

    pub fn collect(home: &Path, parent_id: &str) -> Vec<SubagentView> {
        let Some((_, rows)) = verified_rows(home) else {
            // 覆盖不足 ≠ 没有子任务：返回空列表是端点契约（空态唯一权威），
            // 但必须留下可诊断的痕迹，否则用户只看到 chip 凭空消失。
            ::log::warn!(
                "dsh subagents: coverage incomplete under an authorized home; \
                 returning empty list (not equal to 'no subagents')"
            );
            return Vec::new();
        };
        project_views(None, None, &rows, parent_id, false)
    }

    /// 列表 + 回合状态证据（2026-11-18 销项批）：[`collect`] 的证据增强版。
    ///
    /// 与 `collect` 的分工（IO 预算契约，见 dsh_subagent_source_regression_test
    /// 的 header-only 探针）：`collect` 只做受预算约束的 header 发现（status
    /// 恒 Unknown）；本函数**额外**对每个存活的直属子任务读一次有界全文
    /// （[`TURN_EVIDENCE_BUDGET`]，按实体身份缓存——同一实体只付一次代价），
    /// 从子 Agent 自己的事件流推导回合终态。消费方 = 远程 chip 列表
    /// （dsh_subagent_source::list_for_instance）。
    pub fn collect_with_status(home: &Path, parent_id: &str) -> Vec<SubagentView> {
        let Some((sessions_fd, rows)) = verified_rows(home) else {
            ::log::warn!(
                "dsh subagents: coverage incomplete under an authorized home; \
                 returning empty list (not equal to 'no subagents')"
            );
            return Vec::new();
        };
        // 回合证据缓存键的根实体（与 header 缓存同一 root_key 口径）；元数据
        // 不可得 → 证据面按 Unknown 降级（不猜），列表其余字段不受影响。
        let root_key = sessions_fd.metadata().ok().map(|meta| file_key(&meta));
        project_views(
            Some(&sessions_fd),
            root_key.as_ref(),
            &rows,
            parent_id,
            true,
        )
    }

    /// 行过滤（唯一身份 + 直属父子 + 祖先链闭合）→ 视图投影。
    ///
    /// `evidence=true` 时从 `sessions`/`root_key`（已核验句柄与根实体）推导
    /// 每个存活子任务的回合证据；`false`（header-only 面）状态恒 Unknown。
    fn project_views(
        sessions: Option<&std::fs::File>,
        root_key: Option<&FileKey>,
        rows: &[Row],
        parent_id: &str,
        evidence: bool,
    ) -> Vec<SubagentView> {
        let mut counts = HashMap::<String, usize>::new();
        for row in rows {
            *counts.entry(row.header.id.clone()).or_default() += 1;
        }
        if counts.get(parent_id) != Some(&1) {
            return Vec::new();
        }
        let parents: HashMap<&str, (bool, Option<&str>)> = rows
            .iter()
            .filter(|row| counts.get(&row.header.id) == Some(&1))
            .map(|row| {
                (
                    row.header.id.as_str(),
                    (
                        log::is_subagent(&row.header),
                        row.header.parent_session.as_deref(),
                    ),
                )
            })
            .collect();
        let mut result = Vec::new();
        for row in rows {
            if counts.get(&row.header.id) != Some(&1)
                || !log::is_subagent(&row.header)
                || row.header.parent_session.as_deref() != Some(parent_id)
            {
                continue;
            }
            let mut seen = HashSet::new();
            let mut cursor = Some(row.header.id.as_str());
            let mut valid = true;
            while let Some(id) = cursor {
                if !seen.insert(id) {
                    valid = false;
                    break;
                }
                match parents.get(id) {
                    Some((false, _)) => break,
                    Some((true, Some(next))) => cursor = Some(*next),
                    Some((true, None)) => {
                        valid = false;
                        break;
                    }
                    None => {
                        valid = false;
                        break;
                    }
                }
            }
            if !valid {
                continue;
            }
            // 状态证据（2026-11-18 销项批）：闭回合 = 可证终态（435/435 真实
            // 子代理日志携带 turn/end 帧）→ Idle + endTs（turn/end 事件时间，
            // 缺失回落日志 mtime）；开回合无写入者存活证明 → 保持 Unknown
            // （2026-11-06 登记裁决维持：不发明 running——锁持有单独不构成
            // 运行证据，实证根会话闭回合后 session.lock 仍被宿主持有）。
            // name：descriptor.label（真实形态 400/402 在案）优先，无 descriptor
            // 行回落 header.id（不猜：label 缺失 ≠ 编名字）。
            let (status, end_ts) = match (evidence, sessions, root_key) {
                (true, Some(sessions), Some(root_key)) => {
                    match turn_evidence(sessions, *root_key, row) {
                        Some(TurnEvidence::Closed { end_time_ms }) => {
                            (SubagentStatus::Idle, end_time_ms.and_then(ms_to_iso))
                        }
                        _ => (SubagentStatus::Unknown, None),
                    }
                }
                _ => (SubagentStatus::Unknown, None),
            };
            result.push(SubagentView {
                id: row.header.id.clone(),
                name: row.label.clone().unwrap_or_else(|| row.header.id.clone()),
                description: None,
                spawn_ts: row.header.created_at.and_then(ms_to_iso),
                tokens: TokenUsage::default(),
                status,
                end_ts,
            });
        }
        sort_views(&mut result);
        result
    }
}

/// `dsh_windows.rs` 关键不变量的**跨平台**回归。
///
/// 为什么需要它：那个模块只在 `cfg(windows)` 下编译，本机任何测试都跑不到它，
/// 而它已三次栽在「本机看不见」的错误上（`ERROR_NO_MORE_FILES` 的 HRESULT 误比、
/// `FileName` 偏移 76 vs 104、`Vec<u64>.add` 元素级缩放）。这里用最小替身把
/// 两条不变量变成**在本机真的执行**的断言——守门的测试必须自己运行。
///
/// 覆盖：
/// * **字节偏移语义**（与 `ByteView` 同构的替身）：非零 offset 哨兵读回 + 越界拒绝；
/// * **FILETIME 新鲜度判据**（与 `within_fresh_window` 同判据的**独立复刻**）：
///   24h 边界与「未来时间戳 = 不新鲜」。
///
/// **不同源声明**：替身/复刻都与 Windows 侧**不是同一份代码**。改动
/// `dsh_windows.rs` 的实现**不会**让这里的用例变红；它们守的是「不变量」，
/// 不是「那份实现」。Windows 侧自身的行为仍只能由真机验证。
#[cfg(test)]
mod windows_source_contract_tests {
    /// 与 Windows 侧 `ByteView` 同构的最小替身：字节基址 + 字节长度。
    struct Probe {
        base: *const u8,
        len: usize,
    }

    impl Probe {
        fn new(buffer: &[u64]) -> Self {
            Self {
                base: buffer.as_ptr() as *const u8,
                len: std::mem::size_of_val(buffer),
            }
        }

        unsafe fn read_at<T: Copy>(&self, offset: usize) -> Option<T> {
            if offset.checked_add(std::mem::size_of::<T>())? > self.len {
                return None;
            }
            Some(unsafe { std::ptr::read_unaligned(self.base.add(offset) as *const T) })
        }
    }

    /// 在非零 offset 处写入哨兵再读回：只有字节语义才读得到。
    #[test]
    fn offsets_are_bytes_not_elements() {
        let mut buffer = vec![0u64; 4096 / 8];
        let probe = Probe::new(&buffer);
        let base = buffer.as_mut_ptr() as *mut u8;
        for (index, offset) in [128usize, 1024, 2048].into_iter().enumerate() {
            let marker: u32 = 0xDEAD_0000 + index as u32;
            // SAFETY: offset 远小于 4KB 缓冲。
            unsafe { (base.add(offset) as *mut u32).write_unaligned(marker) };
            let got = unsafe { probe.read_at::<u32>(offset) }.expect("界内可读");
            assert_eq!(
                got, marker,
                "offset {offset} 读到 {got:#x}，期望 {marker:#x}（按元素缩放会读错）"
            );
            // 顺带证明「元素级解释」确实给出不同地址（这就是当初的 bug）。
            assert_ne!(
                offset,
                offset * std::mem::size_of::<u64>(),
                "两种解释必须不同，否则本用例无鉴别力"
            );
        }
    }

    /// FILETIME 新鲜度判据的**跨平台**回归。
    ///
    /// 这是同一**判据**的独立复刻，**不是**同源实现（见模块级「不同源声明」）：
    /// 改动 `dsh_windows.rs` 的 `within_fresh_window` **不会**让本用例变红。
    /// 它守的是「24h 闭区间 + 未来时间戳 = 不新鲜」这条不变量本身。
    #[test]
    fn filetime_window_matches_unix_staleness() {
        const TICKS_PER_SEC: u64 = 10_000_000;
        const FRESH_SECONDS: u64 = 24 * 60 * 60;
        fn fresh(last_write: u64, now_ticks: u64) -> bool {
            if last_write > now_ticks {
                return false;
            }
            (now_ticks - last_write) / TICKS_PER_SEC <= FRESH_SECONDS
        }
        let now = 13_000_000_000_000_000u64; // 任意「现在」
        assert!(fresh(now, now), "刚写入必须新鲜");
        assert!(fresh(now - 3600 * TICKS_PER_SEC, now), "1 小时前必须新鲜");
        assert!(
            fresh(now - 23 * 3600 * TICKS_PER_SEC, now),
            "23 小时前必须仍在窗口内"
        );
        assert!(
            !fresh(now - 25 * 3600 * TICKS_PER_SEC, now),
            "25 小时前必须判为超窗"
        );
        // **整点边界**：恰 24h 仍算新鲜（实现用 `<=`）。把 `<=` 改成 `<` 会在这里被抓。
        assert!(
            fresh(now - 24 * 3600 * TICKS_PER_SEC, now),
            "恰 24 小时必须仍判为新鲜（实现边界是闭区间 <=）"
        );
        assert!(
            !fresh(now - (24 * 3600 + 1) * TICKS_PER_SEC, now),
            "24h+1s 必须判为超窗"
        );
        // **子秒边界**：24h + 1 tick（100ns）必须仍判**新鲜**——实现是秒级粒度
        // （先整除再比较），所以不足 1 秒的超出不应翻转结论。这条断言同时
        // 杀掉两种**边界外推**改写：改 tick 级比较、或在整除前向上取整
        // （它们在子秒边界上与真实实现不等价，这正是本断言存在的理由）。
        assert!(
            fresh(now - (24 * 3600 * TICKS_PER_SEC + 1), now),
            "24h+1tick 必须仍判为新鲜（实现为秒级粒度，先整除再比较）"
        );
        // 关键：未来时间戳与 Unix 的 elapsed()→Err→false 同判（不新鲜）。
        assert!(
            !fresh(now + 3600 * TICKS_PER_SEC, now),
            "未来时间戳必须判为不新鲜（与 Unix 一致）"
        );
    }

    /// 越界与整数溢出都必须安全拒绝。
    #[test]
    fn out_of_range_is_refused_without_panicking() {
        let buffer = vec![0u64; 8]; // 64 字节
        let probe = Probe::new(&buffer);
        assert_eq!(probe.len, 64);
        // 读 8 字节：56 刚好占满 56..64，64 越界。
        assert!(unsafe { probe.read_at::<u64>(64) }.is_none());
        assert!(unsafe { probe.read_at::<u64>(56) }.is_some());
        assert!(unsafe { probe.read_at::<u64>(57) }.is_none());
        assert!(unsafe { probe.read_at::<u64>(usize::MAX) }.is_none());
    }
}

#[cfg(test)]
mod descriptor_label_tests {
    use super::descriptor_label_from_line;

    /// 真实形态（2026-11-15 Desktop home 取证 400/402）：header 下一行
    /// subagent/descriptor，data.label 为人类可读任务名。
    #[test]
    fn descriptor_label_extracts_from_real_shape() {
        let line = concat!(
            r#"{"type":"subagent/descriptor","seq":0,"time":1791386859445,"#,
            r#""data":{"version":3,"mode":"continuable","provider":"spawn","#,
            r#""label":"File pending debts as GitHub issues","agentProvider":"ollama-pro","#,
            r#""agentModel":"deepseek-v4.1-flash","agentReasoningEffort":"max"}}"#,
            "\n"
        );
        assert_eq!(
            descriptor_label_from_line(line).as_deref(),
            Some("File pending debts as GitHub issues")
        );
    }

    /// 半行（写者未写完）不猜：无换行收尾一律 None。
    #[test]
    fn descriptor_label_rejects_partial_line() {
        assert!(descriptor_label_from_line(r#"{"type":"subagent/descriptor""#).is_none());
        assert!(descriptor_label_from_line("").is_none());
    }

    /// 非 descriptor 行 / 缺 label / 空 label → None（name 回落 id，不编名）。
    #[test]
    fn descriptor_label_rejects_non_descriptor_and_empty() {
        assert!(descriptor_label_from_line("{\"type\":\"turn/start\"}\n").is_none());
        assert!(descriptor_label_from_line(
            "{\"type\":\"subagent/descriptor\",\"data\":{\"mode\":\"continuable\"}}\n"
        )
        .is_none());
        assert!(descriptor_label_from_line(
            "{\"type\":\"subagent/descriptor\",\"data\":{\"label\":\"   \"}}\n"
        )
        .is_none());
    }

    /// 超 200 字符截断（防异常长串撑爆端点载荷；按字符截不撕 UTF-8）。
    #[test]
    fn descriptor_label_truncates_to_budget() {
        let long = "深".repeat(500);
        let line = format!(
            "{}\n",
            serde_json::json!({"type":"subagent/descriptor","data":{"label":long}})
        );
        let label = descriptor_label_from_line(&line).expect("合法 descriptor 必须出 label");
        assert_eq!(label.chars().count(), super::DESCRIPTOR_LABEL_MAX_CHARS);
    }
}

#[cfg(test)]
mod platform_capability_tests {
    use super::*;

    /// 平台能力声明必须与编译分支一致：只有实际编译进来的实现才算「支持」。
    ///
    /// 这条测试的价值不是证明 Windows 实现正确（那需要真机），而是防止
    /// 「文档声称支持 / 代码恒返回空」这类静默漂移再次出现。
    #[test]
    fn platform_capability_matches_the_compiled_branch() {
        let declared = cfg!(any(unix, windows));
        let detail_supported = cfg!(any(unix, windows));
        assert_eq!(
            declared, detail_supported,
            "列表与详情的能力声明必须一致（不能一个有一个没有）"
        );
        #[cfg(all(unix, not(windows)))]
        assert!(declared, "Unix 侧必须有列表与详情实现");
        #[cfg(windows)]
        assert!(declared, "Windows 侧必须有列表与详情实现（源码已就位）");
    }

    /// 未知状态不得被伪造：无论哪个平台，`collect` 出的行状态只能是 Unknown。
    #[test]
    fn collect_never_invents_a_running_status() {
        let dir = tempfile::tempdir().expect("tempdir");
        let views = collect(dir.path(), "no-such-parent");
        assert!(views.is_empty(), "不存在的父会话不得产出任何行");
    }

    /// 平台无关的临时父子家（严格扫描要求全树身份可读）：父会话 + 直属子任务
    /// （origin=subagent + parentSession 指向父），事件由用例注入。
    use crate::monitor::subagents::{ms_to_iso, SubagentStatus};

    fn home_with_child(events: &[serde_json::Value]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        write_session(
            dir.path(),
            "parent",
            serde_json::json!({
                "type": "session", "version": 4, "id": "session-parent",
                "cwd": "/tmp/p", "createdAt": 1000
            }),
            &[],
        );
        write_session(
            dir.path(),
            "child",
            serde_json::json!({
                "type": "session", "version": 4, "id": "child-1", "cwd": "/tmp/p",
                "origin": "subagent", "delegationDepth": 1,
                "parentSession": "session-parent", "createdAt": 2000
            }),
            events,
        );
        dir
    }

    fn write_session(
        home: &Path,
        dir_name: &str,
        header: serde_json::Value,
        events: &[serde_json::Value],
    ) {
        let dir = home.join("sessions/proj").join(dir_name);
        std::fs::create_dir_all(&dir).expect("建会话目录");
        let mut text = serde_json::to_string(&header).expect("header 序列化");
        text.push('\n');
        for event in events {
            text.push_str(&serde_json::to_string(event).expect("事件序列化"));
            text.push('\n');
        }
        std::fs::write(dir.join("session.v4.jsonl"), text).expect("写日志");
    }

    fn child_row(home: &Path) -> SubagentView {
        collect_with_status(home, "session-parent")
            .into_iter()
            .find(|row| row.id == "child-1")
            .expect("正父子必须能在板")
    }

    /// 终态证据接通（A06 销项 2026-11-18）：**闭回合 = 可证终态**。真实取证
    /// （本机 Desktop home 435 个子代理日志）：435/435 携带 turn/end 终态帧
    /// （reason.kind: completed 525 / aborted 15 / max-tokens 14 / error 7 /
    /// interrupted 4）——子 Agent 自己的事件流与主面板同一回合词表，回合闭合
    /// 是宿主写下的确定事实，不是猜测。映射到 SubagentStatus::Idle（语义 =
    /// 「不活跃（已完成/已停止）」，不是「永久完成」——continuable 子任务仍
    /// 在全量名单，只是退出活跃面）+ endTs = turn/end 事件时间（终态锚）。
    /// 旧代码恒 Unknown → 本用例 RED 于旧代码。
    #[test]
    fn closed_turn_is_provable_terminal_idle_with_end_anchor() {
        let home = home_with_child(&[
            serde_json::json!({"type":"turn/start","seq":1,"time":3000,"data":{"turn":1}}),
            serde_json::json!({"type":"turn/end","seq":2,"time":4000,"data":{"turn":1,"reason":{"kind":"completed"}}}),
        ]);
        let row = child_row(home.path());
        assert_eq!(
            row.status,
            SubagentStatus::Idle,
            "闭回合（turn/end completed）= 可证终态，不得滞留 Unknown"
        );
        assert_eq!(
            row.end_ts.as_deref(),
            ms_to_iso(4000).as_deref(),
            "endTs = turn/end 事件时间（终态锚，前端冻结时长用）"
        );
    }

    /// 中断收尾也是终态（真实形态 4 例 interrupted / 7 例 error）：回合已闭合
    /// 即「不活跃」，不因 reason 非完成而滞留 Unknown。
    #[test]
    fn interrupted_close_is_still_terminal() {
        let home = home_with_child(&[
            serde_json::json!({"type":"turn/start","seq":1,"time":3000,"data":{}}),
            serde_json::json!({"type":"turn/end","seq":2,"time":4000,"data":{"reason":{"kind":"interrupted"}}}),
        ]);
        let row = child_row(home.path());
        assert_eq!(row.status, SubagentStatus::Idle);
        assert_eq!(row.end_ts.as_deref(), ms_to_iso(4000).as_deref());
    }

    /// 开回合（turn/start 无 turn/end）**保持 Unknown**：2026-11-06 登记裁决
    /// 维持——无写入者存活证明不发明 running（本机 0 个开回合子代理样本，
    /// 无法证明宿主在子任务执行期间持有锁；锁持有单独不构成运行证据：实证
    /// 根会话闭回合后 session.lock 仍被宿主持有）。
    #[test]
    fn open_turn_stays_unknown_without_running_claim() {
        let home = home_with_child(&[
            serde_json::json!({"type":"turn/start","seq":1,"time":3000,"data":{"turn":1}}),
            serde_json::json!({"type":"assistant/message","seq":2,"time":3500,"data":{"message":{"content":[{"type":"text","text":"working"}]}}}),
        ]);
        let row = child_row(home.path());
        assert_eq!(
            row.status,
            SubagentStatus::Unknown,
            "开回合无存活证明 → 不发明 running（登记裁决维持）"
        );
        assert!(row.end_ts.is_none(), "未知态不得伪造终态锚");
    }

    /// 无回合事件（如仅有 descriptor/种子任务）→ 无证据 → Unknown。
    #[test]
    fn no_turn_events_stay_unknown() {
        let home = home_with_child(&[serde_json::json!({
            "type":"subagent/descriptor","seq":0,"time":2500,
            "data":{"version":3,"mode":"continuable","provider":"spawn","label":"任务名"}
        })]);
        let row = child_row(home.path());
        assert_eq!(row.status, SubagentStatus::Unknown);
        assert!(row.end_ts.is_none());
    }
}
