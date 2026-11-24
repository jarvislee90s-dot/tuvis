//! DSH 子任务只读发现的 Windows 实现。
//!
//! 与 Unix 版**结构同构**（同预算、同三道发布前校验、同拒绝重解析点），
//! 但**不是行为等价**：每层打开仍按路径（Win32 无 `openat` 等价物），
//! 且本实现**没有任何运行证据**。差别主要在「怎么安全地打开」：
//!
//! * Unix：逐层 `openat(O_NOFOLLOW)` + `fstatat`/`fstat` 前后快照；
//! * Windows：逐层 `CreateFileW`，目录带 `FILE_FLAG_BACKUP_SEMANTICS`，
//!   每一层都带 `FILE_FLAG_OPEN_REPARSE_POINT` **拒绝重解析点**（符号链接 /
//!   目录联接 / 挂载点），因此链接不会被跟随。
//!
//! 打开后的一切判断都基于**句柄**，不是路径：`GetFileInformationByHandle`
//! 给出卷序列号 + 文件索引（等价 dev+ino）、大小与时间戳，用于
//! 「打开前后是同一实体」的核验。正文读取用 `ReadFile` 按预算截断。
//!
//! 覆盖不足（任一会话身份未知/不可读/超过补读上限）一律返回「无行」，
//! 与 Unix 版同一条纪律：**不把「读不到」当成「没有子任务」**。

use std::collections::{HashMap, HashSet};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetFileInformationByHandle, ReadFile, BY_HANDLE_FILE_INFORMATION,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ,
    FILE_ID_BOTH_DIR_INFO, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

use crate::monitor::dsh::log;
use crate::monitor::subagents::{ms_to_iso, sort_views, SubagentStatus, SubagentView, TokenUsage};

const HEADER_BUDGET: u64 = 64 * 1024;
/// 缓存上限（与 Unix 版同量级；按实体键去重，避免冷文件被反复重读）。
const CACHE_LIMIT: usize = 4096;
/// 单次调用允许补读的冷文件上限（与 Unix 版同一数值与理由）。
const COLD_FILL_LIMIT: usize = 512;
/// 回合证据读取的正文预算（解压后字节；与 Unix 版同一数值与理由：真实
/// Desktop home 最大 6.5MB，超出如实降级 Unknown）。
const TURN_EVIDENCE_BUDGET: u64 = 16 * 1024 * 1024;

/// 子任务回合证据（与 Unix 版同一枚举与取证依据：435/435 真实子代理日志
/// 携带 turn/end 终态帧 → 闭回合可证终态；开回合无存活证明 → 不发明 running）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TurnEvidence {
    /// 最近回合已闭合（终态）：end_time_ms = turn/end 事件时间；事件缺时间
    /// → 回落日志 mtime（最后已知写入）。
    Closed { end_time_ms: Option<i64> },
    /// 回合打开：无写入者存活证明 → Unknown（登记裁决维持）。
    Open,
    /// 无回合事件，或证据不可得（读取失败/超预算）。
    NoEvidence,
}

/// 回合证据缓存：键 = 日志绝对路径宽字符，值 = (实体快照, 证据)；None =
/// 稳定失败（超预算/不可读），同一实体不重复读取（与 header 缓存同一纪律；
/// 证据缓存的是「这份内容里的回合事实」——纯内容产物，跨轮询安全）。
type EvidenceCache = std::sync::Mutex<HashMap<Vec<u16>, (FileKey, Option<TurnEvidence>)>>;

fn evidence_cache() -> &'static EvidenceCache {
    static CACHE: std::sync::LazyLock<EvidenceCache> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));
    &CACHE
}

/// FILETIME（100ns ticks since 1601）→ epoch 毫秒（endTs 的 mtime 回落锚）。
fn filetime_to_ms(ticks: u64) -> Option<i64> {
    let ms = (ticks / 10_000) as i128 - 11_644_473_600_000i128;
    i64::try_from(ms).ok()
}

/// 从已核验实体读取目标行全文（预算内）→ 推导回合证据（与 Unix 版同一推导）。
/// 返回 None = 本轮失效（实体身份漂移——调用方按 Unknown 处理，不进缓存）。
fn turn_evidence(sessions: &Path, row: &Row) -> Option<TurnEvidence> {
    let path = sessions
        .join(&row.project)
        .join(&row.session)
        .join(&row.log);
    let wide_path = wide(&path)?;
    if let Ok(cache) = evidence_cache().lock() {
        if let Some((cached_key, evidence)) = cache.get(&wide_path) {
            if *cached_key == row.log_key {
                return Some(evidence.unwrap_or(TurnEvidence::NoEvidence));
            }
        }
    }
    // 会话目录必须仍是核验时的同一实体（与 read_verified_content 同门）。
    let session_dir = sessions.join(&row.project).join(&row.session);
    let session_file = open_no_follow(&session_dir, true)?;
    if key_of(&session_file) != Some(row.session_key) {
        return None;
    }
    let file = open_no_follow(&path, false)?;
    if key_of(&file) != Some(row.log_key) {
        return None;
    }
    let compressed = row
        .log
        .to_str()
        .map(|name| name.ends_with(".zstd"))
        .unwrap_or(false);
    let read_result = read_log_text(&file, row.log_key, compressed, TURN_EVIDENCE_BUDGET);
    let Some((text, mtime_ms)) = read_result else {
        // 稳定失败：缓存避免每轮重读 16MB（与 Unix 同一理由）。
        if let Ok(mut cache) = evidence_cache().lock() {
            cache.insert(wide_path, (row.log_key, None));
        }
        return Some(TurnEvidence::NoEvidence);
    };
    let events = log::parse_events(&text);
    let facts = crate::monitor::dsh::status::scan_facts(&events);
    let evidence = if facts.has_open_turn() {
        TurnEvidence::Open
    } else {
        match facts.last_end {
            Some((end_seq, _kind)) => {
                let end_time_ms = events
                    .iter()
                    .rev()
                    .find(|e| e.kind == "turn/end" && e.seq == Some(end_seq))
                    .and_then(|e| e.time)
                    .or(mtime_ms);
                TurnEvidence::Closed { end_time_ms }
            }
            None => TurnEvidence::NoEvidence,
        }
    };
    if let Ok(mut cache) = evidence_cache().lock() {
        if cache.len() >= CACHE_LIMIT {
            // 证据重算只是一次有界正文读：满即淘汰任意一半（与 Unix 同策略）。
            let victims: Vec<Vec<u16>> = cache.keys().take(cache.len() / 2 + 1).cloned().collect();
            for victim in victims {
                cache.remove(&victim);
            }
        }
        cache.insert(wide_path, (row.log_key, Some(evidence)));
    }
    Some(evidence)
}

/// 从已打开且核验身份的日志句柄读取全文（预算内），返回 (正文, mtime 毫秒)。
/// 与 `read_verified_content` 的读取尾段同款纪律：读取前后实体快照一致、
/// 压缩输入超预算直接拒绝（防「截断恰好落在完整帧边界」被当成 EOF）。
fn read_log_text(
    file: &OwnedFile,
    expected: FileKey,
    compressed: bool,
    budget: u64,
) -> Option<(String, Option<i64>)> {
    let mtime_ms = filetime_to_ms(expected.last_write);
    if compressed && expected.len > budget {
        return None;
    }
    let mut raw = vec![0u8; budget.saturating_add(1) as usize];
    let mut read: u32 = 0;
    unsafe { ReadFile(file.0, Some(&mut raw), Some(&mut read), None) }.ok()?;
    raw.truncate(read as usize);
    if raw.len() as u64 > budget {
        return None;
    }
    if key_of(file) != Some(expected) {
        return None;
    }
    let text = if compressed {
        let mut decoder = zstd::stream::read::Decoder::new(std::io::Cursor::new(raw)).ok()?;
        decoder.window_log_max(22).ok()?;
        let mut out = Vec::new();
        use std::io::Read as _;
        std::io::BufReader::new(decoder.take(budget + 1))
            .read_to_end(&mut out)
            .ok()?;
        if out.len() as u64 > budget {
            return None;
        }
        String::from_utf8_lossy(&out).to_string()
    } else {
        String::from_utf8_lossy(&raw).to_string()
    };
    Some((text, mtime_ms))
}

/// 实体快照：卷序列号 + 文件索引（等价 dev+ino）+ 大小 + 最后写时间 +
/// **change time**（等价 Unix 的 ctime）。
///
/// 为什么必须有 change time：只有 mtime 时，「同 inode、同长度、把 mtime 改回
/// 原值」的原地改写会命中旧缓存并发布过期的父子关系。Unix 侧为此专门把 ctime
/// 纳入 `FileKey`，Windows 用 `FILE_BASIC_INFO.ChangeTime` 承担同一职责。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct FileKey {
    volume: u32,
    index_high: u32,
    index_low: u32,
    len: u64,
    last_write: u64,
    change_time: i64,
    attributes: u32,
}

/// `FILE_ATTRIBUTE_REPARSE_POINT`：符号链接 / 目录联接 / 挂载点都带此位。
const ATTR_REPARSE_POINT: u32 = 0x400;
const ATTR_DIRECTORY: u32 = 0x10;

struct OwnedFile(HANDLE);

impl Drop for OwnedFile {
    fn drop(&mut self) {
        if self.0 != INVALID_HANDLE_VALUE {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

///
/// 结果缓存：键 = 绝对路径，值 = 实体快照 + 结果（`None` 表示**稳定失败**）。
/// 稳定失败也缓存，否则一个坏 header 会在每轮扫描被重读一次——冷历史放宽后
/// 这会变成可观测的成本，且违背「同一实体不重复付代价」的既有纪律。
/// 成功值 = (header, descriptor chip 名)——与 Unix 版同一提取语义（共享
/// `descriptor_label_from_line` 纯函数）。
type HeaderRead = (log::DshHeader, Option<String>);
type HeaderCache = std::sync::Mutex<HashMap<Vec<u16>, (FileKey, Option<HeaderRead>)>>;

fn header_cache() -> &'static HeaderCache {
    static CACHE: std::sync::LazyLock<HeaderCache> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));
    &CACHE
}

/// 字节视图：把「缓冲区 + 长度」与「按字节取地址」绑在一起。
///
/// 存在目的：让「偏移是字节量」不可能被误用。早期版本直接对 `Vec<u64>` 做
/// `ptr.add(offset)`，而 `offset` 是字节量 → 实际前进 `offset*8` 字节
/// （文件名全错、越界读）。恒等函数挡不住这种回归，因此把取址收进类型：
/// 唯一的偏移入口都以字节为单位，元素级指针在类型外不可达。
///
/// 本结构本身不依赖 Windows 类型，但**整个文件只在 `cfg(windows)` 下编译**，
/// 所以本文件内的任何测试在 macOS/Linux 上都不会运行。可跨平台执行的等价
/// 不变量断言放在 `dsh.rs` 的 `windows_source_contract_tests`——注意那个替身与
/// 这里**不同源**：改动本文件的 `read_at` 不会让那条用例变红。
struct ByteView {
    base: *const u8,
    len: usize,
}

impl ByteView {
    fn new(buffer: &[u64]) -> Self {
        Self {
            base: buffer.as_ptr() as *const u8,
            len: std::mem::size_of_val(buffer),
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    /// 在 `offset`（**字节**）处按 `T` 读取一个值（按值复制）。
    ///
    /// 按值返回而非引用：缓冲区在下一轮迭代会被 `fill(0)` 改写，引用会与
    /// 后续写入重叠（借用检查因持裸指针而不报）。按值把读取限制在本函数内。
    ///
    /// # Safety
    /// `offset + size_of::<T>()` 必须 ≤ `self.len()`。
    unsafe fn read_at<T: Copy>(&self, offset: usize) -> Option<T> {
        if offset.checked_add(std::mem::size_of::<T>())? > self.len {
            return None;
        }
        Some(unsafe { std::ptr::read_unaligned(self.base.add(offset) as *const T) })
    }

    /// 取 `offset`（**字节**）处的 `len` 字节名字指针。
    ///
    /// # Safety
    /// 调用方保证 `offset + len ≤ self.len()`（本函数会检查）且对 `u16` 对齐。
    unsafe fn name_at(&self, offset: usize, len: usize) -> Option<*const u16> {
        if offset.checked_add(len)? > self.len {
            return None;
        }
        Some(unsafe { self.base.add(offset) as *const u16 })
    }

    /// 记录级读取：偏移由调用方给出（字节），大小由结构体决定。
    unsafe fn record_at(&self, offset: usize) -> Option<FILE_ID_BOTH_DIR_INFO> {
        unsafe { self.read_at::<FILE_ID_BOTH_DIR_INFO>(offset) }
    }
}

/// 该路径的缓存是否命中且实体未变（不读正文）。
fn header_cache_hit(path: &Path, expected: FileKey) -> bool {
    let Some(key) = wide(path) else {
        return false;
    };
    header_cache()
        .lock()
        .map(|cache| {
            cache
                .get(&key)
                .is_some_and(|(cached_key, _)| *cached_key == expected)
        })
        .unwrap_or(false)
}

fn wide(path: &Path) -> Option<Vec<u16>> {
    let mut units: Vec<u16> = path.as_os_str().encode_wide().collect();
    if units.contains(&0) {
        return None;
    }
    units.push(0);
    Some(units)
}

fn key_of(file: &OwnedFile) -> Option<FileKey> {
    use windows::Win32::Storage::FileSystem::{
        FileBasicInfo, GetFileInformationByHandleEx, FILE_BASIC_INFO,
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(file.0, &mut info) }.ok()?;
    // ChangeTime 只能从 FILE_BASIC_INFO 取（BY_HANDLE_FILE_INFORMATION 没有）。
    let mut basic = FILE_BASIC_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            file.0,
            FileBasicInfo,
            (&mut basic as *mut FILE_BASIC_INFO).cast(),
            std::mem::size_of::<FILE_BASIC_INFO>() as u32,
        )
    }
    .ok()?;
    Some(FileKey {
        volume: info.dwVolumeSerialNumber,
        index_high: info.nFileIndexHigh,
        index_low: info.nFileIndexLow,
        len: ((info.nFileSizeHigh as u64) << 32) | info.nFileSizeLow as u64,
        last_write: ((info.ftLastWriteTime.dwHighDateTime as u64) << 32)
            | info.ftLastWriteTime.dwLowDateTime as u64,
        change_time: basic.ChangeTime,
        attributes: basic.FileAttributes,
    })
}

/// 打开一个路径（不跟随重解析点）。
///
/// 目录必须带 `BACKUP_SEMANTICS`；两者都带 `OPEN_REPARSE_POINT`，因此
/// 「目录联接指向别处」不会被当成那个别处。
fn open_no_follow(path: &Path, directory: bool) -> Option<OwnedFile> {
    let path = wide(path)?;
    let mut flags = FILE_FLAG_OPEN_REPARSE_POINT;
    if directory {
        flags |= FILE_FLAG_BACKUP_SEMANTICS;
    }
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            FILE_GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            flags,
            None,
        )
    }
    .ok()?;
    let file = OwnedFile(handle);
    // **关键一步**：`OPEN_REPARSE_POINT` 只保证「打开的是重解析点本身」，
    // 并不会替你拒绝它。必须读属性确认它不是链接/联接/挂载点，否则
    // 后续任何按路径的访问都会跟随到别处（这就等于没有隔离）。
    let key = key_of(&file)?;
    if key.attributes & ATTR_REPARSE_POINT != 0 {
        return None;
    }
    if directory && key.attributes & ATTR_DIRECTORY == 0 {
        return None;
    }
    if !directory && key.attributes & ATTR_DIRECTORY != 0 {
        return None;
    }
    Some(file)
}

/// 逐层打开目录链：任一层不是目录或无法打开即失败。
///
/// 只在**已授权根**之上逐层推进；`..`/前缀等非规范分量一律拒绝。
fn open_dir_chain(path: &Path) -> Option<OwnedFile> {
    if !path.is_absolute() {
        return None;
    }
    let mut current = std::path::PathBuf::new();
    let mut file = None;
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => {
                current = std::path::PathBuf::from(prefix.as_os_str());
                continue;
            }
            Component::RootDir => {
                current.push("\\");
                file = open_no_follow(Path::new(&current), true);
                file.as_ref()?;
                continue;
            }
            Component::Normal(name) => {
                current.push(name);
            }
            _ => return None,
        }
        if current.as_os_str().is_empty() {
            continue;
        }
        file = open_no_follow(&current, true);
        file.as_ref()?;
    }
    file
}

/// 目录项枚举：按名称读取（Windows 没有 `dirfd` 相对枚举的稳定公开 API，
/// 因此每一步都重新以 `OPEN_REPARSE_POINT` 打开，打开后立刻用句柄核验身份）。
// 编译期锁定结构布局：`FILE_ID_BOTH_DIR_INFO` 的 `FileName` 偏移必须与
// windows crate 实际声明一致。这条断言让「crate 升级导致布局变化」在编译期
// 暴露，而不是在运行时读出乱码文件名（手算 76 曾被误用，正确值是 104）。
#[cfg(windows)]
const _: () = {
    use windows::Win32::Storage::FileSystem::FILE_ID_BOTH_DIR_INFO;
    assert!(
        std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName) == 104,
        "FILE_ID_BOTH_DIR_INFO.FileName offset changed; re-check the enumeration code"
    );
};

/// 从**已打开的目录句柄**枚举子项。
///
/// 为什么不用 `std::fs::read_dir(path)`：按路径枚举会重新解析整条路径，中间目录
/// 在此期间被换成目录联接时就会跟随到别处——即使句柄本身校验过也无济于事。
/// 句柄枚举把「看到哪些子项」绑定到同一个已核验实体上（Unix 侧 `fdopendir(dup)`
/// 的等价物）。
///
/// 同时在此**逐项检查重解析点**：链接/联接条目直接跳过，不进入候选。
fn entries(dir: &OwnedFile) -> Option<Vec<(std::ffi::OsString, bool)>> {
    use windows::core::HRESULT;
    use windows::Win32::Foundation::ERROR_NO_MORE_FILES;
    use windows::Win32::Storage::FileSystem::{
        FileIdBothDirectoryInfo, GetFileInformationByHandleEx,
    };

    let mut out = Vec::new();
    // 缓冲区用 `u64` 底层分配：`FILE_ID_BOTH_DIR_INFO` 含 i64，`align_of` 为 8，
    // 而 `Vec<u8>` 只保证 1 字节对齐——把它 cast 成 `&FILE_ID_BOTH_DIR_INFO`
    // 在 Rust 语义下是 UB（硬件通常容忍，miri 会报）。
    let mut buffer = vec![0u64; 64 * 1024 / 8];
    let buffer_bytes = buffer.len() * std::mem::size_of::<u64>();
    loop {
        // **每轮清零**：API 封装不透出实际写入字节数，若某次返回成功但未写入
        // 任何记录，缓冲区里会残留上一批的内容，被下面的「空批」判据误判为
        // 「有记录」→ 重复追加上一批。清零后该判据才是可信的。
        buffer.fill(0);
        let result = unsafe {
            GetFileInformationByHandleEx(
                dir.0,
                FileIdBothDirectoryInfo,
                buffer.as_mut_ptr().cast(),
                buffer_bytes as u32,
            )
        };
        match result {
            Ok(()) => {}
            // `Error::code()` 返回的是 **HRESULT**（Win32 18 → 0x80070012），
            // 不是裸错误码；直接拿 `.0 as u32` 比 18 永不相等（曾导致恒空列表）。
            Err(error) if error.code() == HRESULT::from_win32(ERROR_NO_MORE_FILES.0) => break,
            Err(_) => return None,
        }
        // 第二重终止判据：**本批是否真的产出了记录**。
        // 只依赖「结束时返回 ERROR_NO_MORE_FILES」是脆弱的假设（不同 Windows
        // 版本/文件系统对「枚举完毕」的返回形态有差异）。若本次成功返回但首条
        // 的 NextEntryOffset 与 FileNameLength 都为 0，说明没有记录可用 → 结束。
        // 这样两种语义下都不会无限循环，也不会把「空批」当成错误。
        let view = ByteView::new(&buffer);
        let first = unsafe { view.record_at(0) }?;
        if first.NextEntryOffset == 0 && first.FileNameLength == 0 {
            break;
        }
        // 所有偏移都是**字节**量：一律经 `ByteView`，内部只在 `*const u8` 上
        // 做 `add`（`Vec<u64>` 的元素级 `add` 会按 8 字节缩放，曾导致乱码+越界）。
        let mut offset = 0usize;
        loop {
            let info = unsafe { view.record_at(offset) }?;
            let name_bytes = info.FileNameLength as usize;
            // 名字在结构尾部、长度可变：偏移必须由编译器给（`offset_of!`），
            // 手算容易漏掉 `FileId: i64` 这一层对齐（正确值是 104，不是 76）。
            const NAME_OFFSET: usize = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
            if !name_bytes.is_multiple_of(2) {
                return None;
            }
            let name_ptr = unsafe { view.name_at(offset.checked_add(NAME_OFFSET)?, name_bytes) }?;
            let units = unsafe { std::slice::from_raw_parts(name_ptr, name_bytes / 2) };
            let name = std::ffi::OsString::from_wide(units);
            let is_link = info.FileAttributes & ATTR_REPARSE_POINT != 0;
            let is_dir = info.FileAttributes & ATTR_DIRECTORY != 0;
            if !is_link && name != std::ffi::OsStr::new(".") && name != std::ffi::OsStr::new("..") {
                out.push((name, is_dir));
            }
            let step = info.NextEntryOffset as usize;
            if step == 0 {
                break;
            }
            // 系统返回的链必须单调前进且不越界；异常值一律拒绝（不猜、不越读）。
            if step < std::mem::size_of::<FILE_ID_BOTH_DIR_INFO>() || offset + step >= view.len() {
                return None;
            }
            offset += step;
        }
        // 单次调用已消费完本批；继续下一批。
    }
    out.sort_by(|left, right| left.0.cmp(&right.0));
    Some(out)
}

/// FILETIME（100ns since 1601-01-01）→ 是否在 24h 新鲜窗口内。
///
/// 两边同为 UTC 计数，差值与时区无关；Unix epoch 与 Windows epoch 相差
/// 11644473600 秒。
/// 未来时间戳（时钟回拨 / 被篡改的 mtime）一律视为**不新鲜**，与 Unix 的
/// `modified.elapsed()` 返回 Err → `false` 完全一致。这里不用
/// `saturating_sub` 把未来时间当成「刚刚发生」——那是放宽，会让冷补读额度
/// 永远不被触发。
fn within_fresh_window(last_write: u64) -> bool {
    const TICKS_PER_SEC: u64 = 10_000_000;
    const FRESH_SECONDS: u64 = 24 * 60 * 60;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let now_ticks = (now.as_secs() + 11_644_473_600)
        .saturating_mul(TICKS_PER_SEC)
        .saturating_add(now.subsec_nanos() as u64 / 100);
    // 记录时间晚于现在 → 未来时间戳 → 不新鲜（与 Unix 同判）。
    if last_write > now_ticks {
        return false;
    }
    (now_ticks - last_write) / TICKS_PER_SEC <= FRESH_SECONDS
}

fn generation_of(name: &std::ffi::OsStr) -> Option<i64> {
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

fn newest_log(dir: &OwnedFile) -> Result<Option<std::ffi::OsString>, ()> {
    let mut logs: Vec<(i64, std::ffi::OsString)> = entries(dir)
        .ok_or(())?
        .into_iter()
        // 日志必须是**文件**：目录名恰好叫 session.vN.jsonl 不算候选。
        .filter(|(_, is_dir)| !is_dir)
        .filter_map(|(name, _)| generation_of(&name).map(|version| (version, name)))
        .collect();
    logs.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    Ok(logs.pop().map(|(_, name)| name))
}

/// 有界读取首行：`ReadFile` 最多取 `HEADER_BUDGET + 1` 字节。
///
/// 压缩日志（`.zstd`）在 Windows 侧同样受输入预算约束：解码器只被喂
/// `HEADER_BUDGET + 1` 字节，且限制窗口上限。
fn read_header(path: &Path, compressed: bool) -> Option<(log::DshHeader, Option<String>, FileKey)> {
    let wide_path = wide(path)?;
    let file = open_no_follow(path, false)?;
    let before = key_of(&file)?;
    // 命中缓存且实体未变 → 直接返回（成功或稳定失败），不重读正文。
    if let Ok(cache) = header_cache().lock() {
        if let Some((key, outcome)) = cache.get(&wide_path) {
            if *key == before {
                return outcome
                    .clone()
                    .map(|(header, label)| (header, label, before));
            }
        }
    }
    let mut raw = vec![0u8; (HEADER_BUDGET + 1) as usize];
    let mut read: u32 = 0;
    unsafe { ReadFile(file.0, Some(&mut raw), Some(&mut read), None) }.ok()?;
    raw.truncate(read as usize);
    let after = key_of(&file)?;
    if before != after {
        return None;
    }
    // 解析成「结果」，成功与稳定失败都写缓存（失败不写会让坏 header 每轮重读）。
    let parsed = parse_header_and_label(raw, compressed);
    if let Ok(mut cache) = header_cache().lock() {
        if cache.len() >= CACHE_LIMIT {
            // 与 Unix 同策略：**不整表清空**（清空会让已解析结果全丢，冷候选
            // 超过上限时每轮重读一批、永不收敛）。优先淘汰稳定失败（可重算），
            // 其次才丢弃任意一半，保证已成功的 header 尽量留下。
            let negative: Vec<Vec<u16>> = cache
                .iter()
                .filter(|(_, (_, outcome))| outcome.is_none())
                .map(|(path, _)| path.clone())
                .take(cache.len() / 2 + 1)
                .collect();
            if negative.is_empty() {
                let victims: Vec<Vec<u16>> =
                    cache.keys().take(cache.len() / 2 + 1).cloned().collect();
                for path in victims {
                    cache.remove(&path);
                }
            } else {
                for path in negative {
                    cache.remove(&path);
                }
            }
        }
        cache.insert(wide_path, (after, parsed.clone()));
    }
    parsed.map(|(header, label)| (header, label, after))
}

/// 从已读取的字节解析首行 + 次行 descriptor chip 名（压缩输入另行解压，同样
/// 受输出预算约束；次行必须以换行收尾——半行按「未写完」不猜，与 Unix 同判据）。
fn parse_header_and_label(raw: Vec<u8>, compressed: bool) -> Option<HeaderRead> {
    let (line, label_line) = if compressed {
        let mut decoder = zstd::stream::read::Decoder::new(std::io::Cursor::new(raw)).ok()?;
        decoder.window_log_max(22).ok()?;
        use std::io::Read as _;
        let mut reader = std::io::BufReader::new(decoder.take(HEADER_BUDGET + 1));
        let mut buf = Vec::new();
        reader.read_until_newline(&mut buf)?;
        // 次行：同一 BufReader 续读，缺失/半行 → None（实体增长后缓存失效自愈）。
        let mut second = Vec::new();
        let label_line = reader
            .read_until_newline(&mut second)
            .and_then(|_bytes_read| String::from_utf8(second).ok());
        (buf, label_line)
    } else {
        let end = raw.iter().position(|byte| *byte == b'\n')? + 1;
        let label_line = raw[end..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|offset| String::from_utf8(raw[end..end + offset + 1].to_vec()).ok())
            .unwrap_or(None);
        let mut truncated = raw;
        truncated.truncate(end);
        (truncated, label_line)
    };
    // 首行必须**以换行结尾**：无换行意味着「写者还没写完这一行」，此时解析
    // 成功的 JSON 只是巧合（截断点恰好落在合法 JSON 之后）。Unix 侧对此一律
    // 拒绝，Windows 必须同判据——补一个换行再解析是放宽，方向错。
    if line.is_empty() || line.len() as u64 > HEADER_BUDGET || !line.ends_with(b"\n") {
        return None;
    }
    let header: log::DshHeader = serde_json::from_slice(&line).ok()?;
    let label = label_line.and_then(|line| super::descriptor_label_from_line(&line));
    Some((header, label))
}

/// 供 `read_header` 用的小型读取适配（避免引入额外 trait 依赖）。
trait ReadUntilNewline {
    fn read_until_newline(&mut self, out: &mut Vec<u8>) -> Option<()>;
}

impl<R: std::io::Read> ReadUntilNewline for std::io::BufReader<R> {
    fn read_until_newline(&mut self, out: &mut Vec<u8>) -> Option<()> {
        use std::io::BufRead as _;
        self.read_until(b'\n', out).ok()?;
        Some(())
    }
}

struct Row {
    project: std::ffi::OsString,
    session: std::ffi::OsString,
    log: std::ffi::OsString,
    session_key: FileKey,
    log_key: FileKey,
    header: log::DshHeader,
    /// descriptor chip 名（None = 日志无 descriptor 行，name 回落 header.id）
    label: Option<String>,
}

/// 严格扫描：任何候选身份未知即整轮放弃（与 Unix 版同一纪律）。
fn scan(sessions: &Path) -> Option<Vec<Row>> {
    let mut rows = Vec::new();
    let mut seen = HashSet::new();
    let mut cold_fills = 0usize;
    // 根句柄：枚举与后续每层都从**已核验的句柄**出发，不靠路径重新解析。
    let sessions_file = open_no_follow(sessions, true)?;
    let _ = key_of(&sessions_file)?;
    for (project, project_is_dir) in entries(&sessions_file)? {
        // 非目录不是候选（与 Unix 版一致：符号链接已由枚举层拒绝，
        // 这里再按属性排除普通文件，避免一个杂文件把整轮拖成「覆盖不足」）。
        if !project_is_dir {
            continue;
        }
        let project_dir = sessions.join(&project);
        let project_file = open_no_follow(&project_dir, true)?;
        let _ = key_of(&project_file)?;
        for (session, session_is_dir) in entries(&project_file)? {
            if !session_is_dir {
                continue;
            }
            let session_dir = project_dir.join(&session);
            let session_file = open_no_follow(&session_dir, true)?;
            let session_key = key_of(&session_file)?;
            if !seen.insert((
                session_key.volume,
                session_key.index_high,
                session_key.index_low,
            )) {
                continue;
            }
            let Some(log_name) = newest_log(&session_file).ok()? else {
                continue;
            };
            let log_path = session_dir.join(&log_name);
            let compressed = log_name
                .to_str()
                .map(|name| name.ends_with(".zstd"))
                .unwrap_or(false);
            // 冷历史判定必须在**打开/解码之前**：先 stat 再决定读不读。
            let pre = stat_no_follow(&log_path)?;
            // **先查缓存再计额度**（与 Unix 同一顺序）：已缓存的冷文件不再付代价，
            // 否则「>512 个冷会话但都已缓存」每轮都会被判覆盖不足 —— 那等于
            // 每轮白报一次错误，且与「同一实体不重复付代价」的纪律冲突。
            let cached = header_cache_hit(&log_path, pre);
            if !within_fresh_window(pre.last_write) && !cached {
                if cold_fills >= COLD_FILL_LIMIT {
                    return None;
                }
                cold_fills += 1;
            }
            let (header, label, log_key) = read_header(&log_path, compressed)?;
            rows.push(Row {
                project: project.clone(),
                session,
                log: log_name,
                session_key,
                log_key,
                header,
                label,
            });
        }
    }
    Some(rows)
}

/// 只取实体快照（不读正文），用于「先 stat 后决定是否读」的预过滤。
fn stat_no_follow(path: &Path) -> Option<FileKey> {
    let file = open_no_follow(path, false)?;
    key_of(&file)
}

/// 只读列表（与 Unix 版 `collect` 同一语义与过滤规则）。
pub fn collect(home: &Path, parent_id: &str) -> Vec<SubagentView> {
    let Some(rows) = verified_rows(home) else {
        // 与 Unix 版同一纪律：覆盖不足 ≠ 没有子任务。端点契约仍是空列表，
        // 但必须留下可诊断痕迹，否则用户只看到 chip 凭空消失。
        ::log::warn!(
            "dsh subagents (windows): coverage incomplete under an authorized home; \
             returning empty list (not equal to 'no subagents')"
        );
        return Vec::new();
    };
    project_views(None, &rows, parent_id, false)
}

/// 列表 + 回合状态证据（2026-11-18 销项批，与 Unix 版同一分工与预算纪律）：
/// `collect` 保持 header-only 发现（status 恒 Unknown）；本函数额外对每个
/// 存活直属子任务读一次有界全文（[`TURN_EVIDENCE_BUDGET`]，按实体身份缓存）
/// 推导回合终态。消费方 = 远程 chip 列表。
pub fn collect_with_status(home: &Path, parent_id: &str) -> Vec<SubagentView> {
    let sessions = home.join("sessions");
    let Some(rows) = verified_rows(home) else {
        ::log::warn!(
            "dsh subagents (windows): coverage incomplete under an authorized home; \
             returning empty list (not equal to 'no subagents')"
        );
        return Vec::new();
    };
    project_views(Some(&sessions), &rows, parent_id, true)
}

/// 行过滤（唯一身份 + 直属父子 + 祖先链闭合）→ 视图投影（与 Unix 版同一结构）。
/// `evidence=true` 时按已核验实体推导回合证据；`false` 状态恒 Unknown。
fn project_views(
    sessions: Option<&Path>,
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
        // 完整祖先链：fork（无父引用）与孤儿不挂到本父下。
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
        // 状态证据（2026-11-18 销项批，与 Unix 版同一推导与取证依据）：闭回合
        // = 可证终态 → Idle + endTs；开回合 → 保持 Unknown（不发明 running）。
        let (status, end_ts) = match (evidence, sessions) {
            (true, Some(sessions)) => match turn_evidence(sessions, row) {
                Some(TurnEvidence::Closed { end_time_ms }) => {
                    (SubagentStatus::Idle, end_time_ms.and_then(ms_to_iso))
                }
                _ => (SubagentStatus::Unknown, None),
            },
            _ => (SubagentStatus::Unknown, None),
        };
        result.push(SubagentView {
            id: row.header.id.clone(),
            // 与 Unix 版同一语义：descriptor.label 优先，无 descriptor 行回落 id。
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

/// 打开已授权数据根下的 `sessions` 目录，并完成一次覆盖完整的严格扫描。
fn verified_rows(home: &Path) -> Option<Vec<Row>> {
    if !home.is_absolute() {
        return None;
    }
    // 逐层打开整条 home 链：任一层是重解析点或不是目录即拒绝。
    // 这一步不能用「只打开末级」代替——中间的目录联接会把读取引到别处。
    let home_file = open_dir_chain(home)?;
    let _ = key_of(&home_file)?;
    let sessions = home.join("sessions");
    let sessions_file = open_no_follow(&sessions, true)?;
    // 只用于证明 sessions 根可打开且可核验；后续各步各自复核实体。
    let _ = key_of(&sessions_file)?;
    let mut rows = scan(&sessions)?;
    // 发布前三道校验（与 Unix 同一纪律，缺一即视为覆盖不足）：
    //   ① refresh —— 内容版本变了的行重读 header，关系投影不一致即放弃；
    //   ② confirm —— 逐行重开，确认实体仍是核验时的那个；
    //   ③ membership —— 整树成员集合与已发布行逐项相等（含实体身份），
    //      扫描期间新增的候选与同名换实体都必须在这里被拦住。
    if !refresh(&sessions, &mut rows)
        || !confirm(&sessions, &rows)
        || !membership_confirmed(&sessions, &rows)
    {
        return None;
    }
    Some(rows)
}

/// ① 内容版本变化时重读首行；关系投影不变则刷新快照，否则整轮放弃。
fn refresh(sessions: &Path, rows: &mut [Row]) -> bool {
    for row in rows.iter_mut() {
        let path = sessions
            .join(&row.project)
            .join(&row.session)
            .join(&row.log);
        let Some(pre) = stat_no_follow(&path) else {
            return false;
        };
        if pre == row.log_key {
            continue;
        }
        let compressed = row
            .log
            .to_str()
            .map(|name| name.ends_with(".zstd"))
            .unwrap_or(false);
        let Some((header, label, stable)) = read_header(&path, compressed) else {
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
    }
    true
}

/// ② 发布前再确认每条参与唯一性/父链判定的记录仍是最新代际且未被替换。
fn confirm(sessions: &Path, rows: &[Row]) -> bool {
    for row in rows {
        let session_dir = sessions.join(&row.project).join(&row.session);
        let Some(session_file) = open_no_follow(&session_dir, true) else {
            return false;
        };
        if key_of(&session_file) != Some(row.session_key) {
            return false;
        }
        let Some(log_name) = newest_log(&session_file).ok().flatten() else {
            return false;
        };
        if log_name != row.log {
            return false;
        }
        let Some(log_key) = stat_no_follow(&session_dir.join(&row.log)) else {
            return false;
        };
        if log_key != row.log_key {
            return false;
        }
    }
    true
}

/// 末验用的成员快照：名称 + 实体身份。
struct Membership {
    project: std::ffi::OsString,
    session: std::ffi::OsString,
    log: std::ffi::OsString,
    session_key: FileKey,
    log_key: FileKey,
}

/// 只枚举、不读正文的成员快照。
fn membership(sessions: &Path) -> Option<Vec<Membership>> {
    let sessions_file = open_no_follow(sessions, true)?;
    let mut members = Vec::new();
    let mut seen = HashSet::new();
    for (project, project_is_dir) in entries(&sessions_file)? {
        if !project_is_dir {
            continue;
        }
        let project_dir = sessions.join(&project);
        let project_file = open_no_follow(&project_dir, true)?;
        for (session, session_is_dir) in entries(&project_file)? {
            if !session_is_dir {
                continue;
            }
            let session_dir = project_dir.join(&session);
            let session_file = open_no_follow(&session_dir, true)?;
            let session_key = key_of(&session_file)?;
            if !seen.insert((
                session_key.volume,
                session_key.index_high,
                session_key.index_low,
            )) {
                continue;
            }
            let Some(log_name) = newest_log(&session_file).ok()? else {
                continue;
            };
            let log_key = stat_no_follow(&session_dir.join(&log_name))?;
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

/// ③ 目录成员集合必须与本轮扫描所见完全一致（名称 + 实体身份）。
fn membership_confirmed(sessions: &Path, rows: &[Row]) -> bool {
    let Some(members) = membership(sessions) else {
        return false;
    };
    if members.len() != rows.len() {
        return false;
    }
    let published: HashMap<
        (&std::ffi::OsStr, &std::ffi::OsStr, &std::ffi::OsStr),
        (&FileKey, &FileKey),
    > = rows
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

/// 详情：同一轮完整核验后，只从**已核验实体**读取目标正文。
pub fn read_verified_content(
    home: &Path,
    parent_id: &str,
    subagent_id: &str,
    budget: u64,
) -> Result<(String, log::DshHeader), &'static str> {
    let rows = verified_rows(home)
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
    let session_dir = home
        .join("sessions")
        .join(&target.project)
        .join(&target.session);
    // 会话目录必须仍是核验时的同一实体：同路径换目录（rename 换入）会让下面的
    // 相对路径指向另一个会话，即使日志文件名相同。
    let session_file =
        open_no_follow(&session_dir, true).ok_or("DSH_SUBAGENT_CHANGED: 会话目录已变化")?;
    let reopened_session =
        key_of(&session_file).ok_or("DSH_SUBAGENT_CHANGED: 会话目录元数据不可读")?;
    if reopened_session != target.session_key {
        return Err("DSH_SUBAGENT_CHANGED: 会话目录实体已变化");
    }
    let path = session_dir.join(&target.log);
    let file = open_no_follow(&path, false).ok_or("DSH_SUBAGENT_CHANGED: 目标实体已变化")?;
    let reopened = key_of(&file).ok_or("DSH_SUBAGENT_CHANGED: 目标元数据不可读")?;
    if reopened != target.log_key {
        return Err("DSH_SUBAGENT_CHANGED: 目标内容版本已变化");
    }
    let compressed = target
        .log
        .to_str()
        .map(|name| name.ends_with(".zstd"))
        .unwrap_or(false);
    let mut raw = vec![0u8; budget.saturating_add(1) as usize];
    let mut read: u32 = 0;
    unsafe { ReadFile(file.0, Some(&mut raw), Some(&mut read), None) }
        .map_err(|_| "DSH_SUBAGENT_UNREADABLE: 目标正文不可读")?;
    raw.truncate(read as usize);
    if raw.len() as u64 > budget {
        return Err("DSH_SUBAGENT_UNREADABLE: 目标正文超出预算");
    }
    if key_of(&file) != Some(target.log_key) {
        return Err("DSH_SUBAGENT_CHANGED: 目标读取期间被修改");
    }
    // 压缩输入预算：文件本身超出预算时直接拒绝。否则 `take` 的截断可能恰好
    // 落在完整帧边界，解码器会把它当成正常 EOF——于是「超预算」被静默读成
    // 「正文就到这里」。Unix 侧有这道闸，Windows 必须等价。
    if compressed && target.log_key.len > budget {
        return Err("DSH_SUBAGENT_UNREADABLE: 压缩输入超出预算");
    }
    let text = if compressed {
        // 与 Unix 等价：限制解压窗口上限，避免异常帧申请大窗口造成内存放大。
        let mut decoder = zstd::stream::read::Decoder::new(std::io::Cursor::new(raw))
            .map_err(|_| "DSH_SUBAGENT_UNREADABLE: 压缩帧无法解码")?;
        decoder
            .window_log_max(22)
            .map_err(|_| "DSH_SUBAGENT_UNREADABLE: 压缩窗口超出上限")?;
        let mut out = Vec::new();
        use std::io::Read as _;
        std::io::BufReader::new(decoder.take(budget + 1))
            .read_to_end(&mut out)
            .map_err(|_| "DSH_SUBAGENT_UNREADABLE: 压缩正文不可解")?;
        // 输出预算：`take` 会把超长输出截断成 budget+1，若不检查就会把**被截断
        // 的正文当成功返回**。超预算必须如实拒绝。
        if out.len() as u64 > budget {
            return Err("DSH_SUBAGENT_UNREADABLE: 解压后正文超出预算");
        }
        String::from_utf8_lossy(&out).to_string()
    } else {
        String::from_utf8_lossy(&raw).to_string()
    };
    Ok((text, target.header.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FILETIME 换算必须与「24h 窗口」语义一致。
    ///
    /// **注意**：整个模块只在 `cfg(windows)` 下编译，因此在 macOS/Linux 主机上
    /// 这组测试**不会运行**（`cargo test --lib` 里 filtered out）。它们只有在
    /// Windows 上编译测试目标时才会生效——那正是本批没有的环境。
    /// 保留它们是为了：① 在 Windows 机器上能立刻跑；② 在代码里留下可执行的
    /// 规格说明。**不得**把它们的「存在」当成已有运行证据。
    fn filetime_for_seconds_ago(seconds: u64) -> u64 {
        const TICKS_PER_SEC: u64 = 10_000_000;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let now_ticks = (now.as_secs() + 11_644_473_600)
            .saturating_mul(TICKS_PER_SEC)
            .saturating_add(now.subsec_nanos() as u64 / 100);
        now_ticks.saturating_sub(seconds.saturating_mul(TICKS_PER_SEC))
    }

    #[test]
    fn fresh_window_boundary_is_24_hours() {
        assert!(within_fresh_window(filetime_for_seconds_ago(0)));
        assert!(within_fresh_window(filetime_for_seconds_ago(3600)));
        // 刚刚在窗口内（留足余量避免边界抖动）。
        assert!(within_fresh_window(filetime_for_seconds_ago(23 * 3600)));
        // 明确超窗。
        assert!(!within_fresh_window(filetime_for_seconds_ago(25 * 3600)));
        assert!(!within_fresh_window(filetime_for_seconds_ago(48 * 3600)));
    }

    /// 未来时间戳必须与 Unix 同判为**不新鲜**（不是「当成刚发生」）。
    #[test]
    fn future_timestamp_is_not_fresh() {
        const TICKS_PER_SEC: u64 = 10_000_000;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        // 比「现在」晚 1 小时的 mtime 必须判**不新鲜**：Unix 侧
        // `modified.elapsed()` 对未来返回 Err → `unwrap_or(false)`，
        // Windows 必须同判（否则时钟回拨/被篡改的 mtime 会被当成刚发生，
        // 冷补读额度永远不触发）。
        let future = (now.as_secs() + 11_644_473_600 + 3600).saturating_mul(TICKS_PER_SEC);
        assert!(
            !within_fresh_window(future),
            "未来时间戳必须判为不新鲜（与 Unix 的 elapsed()→Err→false 一致）"
        );
    }

    /// **核心回归断言**：在足够大的缓冲区上取**非零** offset，断言返回的指针
    /// 与基址的实际字节差 == offset。
    ///
    /// 为什么必须这样写：小缓冲区 + offset=0/62/63 的断言对「元素级缩放」
    /// 完全不敏感（缩放后仍被同一界检查挡住，四条断言照样通过）。只有
    /// 「非零 offset + 指针差值」能把「按 8 字节缩放」暴露出来。
    #[test]
    fn record_and_name_offsets_are_real_byte_distances() {
        // 4KB，足够容纳多条记录。
        let mut buffer = vec![0u64; 4096 / 8];
        let view = ByteView::new(&buffer);
        assert_eq!(view.len(), 4096);

        // 在每个 offset 的**记录起始处**写入一个可辨识的 FileAttributes 值，
        // 然后读回：只有偏移真的按字节计算，才会读到该位置写入的值。
        // 元素级缩放会读到别处（全零），断言立即失败。
        for (index, offset) in [128usize, 1024, 2048].into_iter().enumerate() {
            let marker = 0xDEAD_0000u32 + index as u32;
            let base = buffer.as_mut_ptr() as *mut u8;
            let attrs_off = offset + std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileAttributes);
            unsafe { (base.add(attrs_off) as *mut u32).write_unaligned(marker) };
            let record = unsafe { view.record_at(offset) }.expect("应可读");
            assert_eq!(
                record.FileAttributes, marker,
                "offset {offset} 读到的 FileAttributes={:#x}，期望 {marker:#x}                 （元素级缩放会读到 {} 字节处）",
                record.FileAttributes,
                offset * std::mem::size_of::<u64>()
            );
        }

        // 名字指针同理：在 名字偏移 处写入哨兵，读回的指针必须指向它。
        const NAME_OFFSET: usize = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
        let base = buffer.as_mut_ptr() as *mut u8;
        for offset in [0usize, 128] {
            let sentinel_off = offset + NAME_OFFSET;
            unsafe { (base.add(sentinel_off) as *mut u16).write_unaligned(0xABCD) };
            let name_ptr = unsafe { view.name_at(sentinel_off, 2) }.expect("名字区间应可读");
            let actual = (name_ptr as usize) - (view.base as usize);
            assert_eq!(
                actual, sentinel_off,
                "名字指针必须是字节偏移 {sentinel_off}，实际 {actual}"
            );
            assert_eq!(unsafe { std::ptr::read_unaligned(name_ptr) }, 0xABCD);
        }
    }

    /// 越界一律返回 None，且溢出不得 panic。
    #[test]
    fn byte_view_refuses_out_of_range_without_panicking() {
        let buffer = vec![0u64; 8]; // 64 字节
        let view = ByteView::new(&buffer);
        assert_eq!(view.len(), 64);
        // 一条记录就要 112 字节，64 字节缓冲区放不下任何一条。
        assert!(unsafe { view.record_at(0) }.is_none());
        assert!(unsafe { view.name_at(63, 2) }.is_none());
        assert!(unsafe { view.name_at(62, 2) }.is_some());
        // 整数溢出必须被 checked_add 拦住，不能 panic。
        assert!(unsafe { view.name_at(usize::MAX, 2) }.is_none());
        assert!(unsafe { view.record_at(usize::MAX) }.is_none());
    }

    /// 名字偏移是 104 **字节**——若被当成 u64 元素数会变成 832。
    #[test]
    fn name_offset_is_a_byte_count() {
        const NAME_OFFSET: usize = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
        assert_eq!(NAME_OFFSET, 104);
        assert_ne!(NAME_OFFSET, 104 * std::mem::size_of::<u64>());
    }

    /// 代际名解析与 Unix 版必须同一规则（否则同一日志在两平台会被认成不同代）。
    #[test]
    fn generation_names_match_the_unix_rules() {
        use std::ffi::OsStr;
        assert_eq!(generation_of(OsStr::new("session.jsonl")), Some(0));
        assert_eq!(generation_of(OsStr::new("session.jsonl.zstd")), Some(0));
        assert_eq!(generation_of(OsStr::new("session.v4.jsonl")), Some(4));
        assert_eq!(
            generation_of(OsStr::new("session.v12.jsonl.zstd")),
            Some(12)
        );
        // 非日志名一律不是候选。
        assert_eq!(generation_of(OsStr::new("notes.txt")), None);
        assert_eq!(generation_of(OsStr::new("session.v.jsonl")), None);
        assert_eq!(generation_of(OsStr::new("session.vX.jsonl")), None);
    }
}
