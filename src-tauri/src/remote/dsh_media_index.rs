//! 媒体引用索引不承载任何设备或宿主授权，只缓存当前转写解析结果。

use super::{dsh_image, dsh_present_media, read_media_transcript, SessionMedia, MEDIA_LOG_INPUT};
use crate::monitor::dsh::log;
use serde_json::Value;
use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    time::SystemTime,
};

#[cfg(unix)]
use std::{
    collections::VecDeque,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    sync::{Arc, LazyLock, Mutex, OnceLock},
};

pub(super) struct Scope<'a> {
    pub(super) root: &'a Path,
    pub(super) instance_id: &'a str,
    pub(super) session_id: &'a str,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct Stamp {
    len: u64,
    modified: SystemTime,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
    #[cfg(windows)]
    identity: (u32, u32, u32, i64),
}

impl Stamp {
    fn read(file: &File) -> Option<Self> {
        let meta = file.metadata().ok()?;
        if !meta.is_file() || meta.len() > MEDIA_LOG_INPUT {
            return None;
        }
        Some(Self {
            len: meta.len(),
            modified: meta.modified().ok()?,
            #[cfg(unix)]
            identity: (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec()),
            #[cfg(windows)]
            identity: windows_file_identity(file)?,
        })
    }
}

#[cfg(windows)]
fn windows_file_identity(file: &File) -> Option<(u32, u32, u32, i64)> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{
            FileBasicInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, FILE_BASIC_INFO,
        },
    };
    let handle = HANDLE(file.as_raw_handle());
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    let mut basic = FILE_BASIC_INFO::default();
    // SAFETY: File owns the live handle; both APIs write only their stated output structures.
    unsafe {
        GetFileInformationByHandle(handle, &mut info).ok()?;
        GetFileInformationByHandleEx(
            handle,
            FileBasicInfo,
            (&mut basic as *mut FILE_BASIC_INFO).cast(),
            std::mem::size_of::<FILE_BASIC_INFO>() as u32,
        )
        .ok()?;
    }
    if basic.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        || (info.nFileIndexHigh == 0 && info.nFileIndexLow == 0)
        || basic.ChangeTime == 0
    {
        return None;
    }
    Some((
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
        basic.ChangeTime,
    ))
}

fn open_transcript(path: &Path) -> Option<File> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        opts.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0);
    }
    opts.open(path).ok()
}

#[cfg(unix)]
#[derive(Eq, PartialEq)]
struct Key {
    instance: String,
    session: String,
    path: PathBuf,
    generation: i64,
    stamp: Stamp,
}

#[cfg(unix)]
type Slot = Arc<OnceLock<Option<Index>>>;
#[cfg(unix)]
static CACHE: LazyLock<Mutex<VecDeque<(Key, Slot)>>> =
    LazyLock::new(|| Mutex::new(VecDeque::new()));

#[cfg(unix)]
fn find_slot(key: Key) -> Option<Slot> {
    const MAX_ENTRIES: usize = 8;
    let mut cache = CACHE.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(pos) = cache.iter().position(|(cached, _)| *cached == key) {
        let entry = cache.remove(pos)?;
        let slot = Arc::clone(&entry.1);
        cache.push_back(entry);
        return Some(slot);
    }
    if cache.len() == MAX_ENTRIES {
        // 不能驱逐正在构建的项，否则同转写并发请求会重新解析。
        let idle = cache
            .iter()
            .position(|(_, slot)| Arc::strong_count(slot) == 1)?;
        cache.remove(idle);
    }
    let slot = Slot::default();
    cache.push_back((key, Arc::clone(&slot)));
    Some(slot)
}

pub(in crate::remote) struct ResolvedMedia {
    pub(in crate::remote) media: SessionMedia,
    pub(in crate::remote) cwd: Option<String>,
    dir: PathBuf,
    generation: PathBuf,
    canonical: PathBuf,
    version: i64,
    stamp: Stamp,
}

impl ResolvedMedia {
    pub(in crate::remote) fn still_current(&self) -> bool {
        log::generation_logs(&self.dir)
            .pop()
            .is_some_and(|(version, path)| {
                version == self.version
                    && path == self.generation
                    && path.canonicalize().ok().as_deref() == Some(self.canonical.as_path())
                    && open_transcript(&self.canonical)
                        .as_ref()
                        .and_then(Stamp::read)
                        == Some(self.stamp)
            })
    }
}

pub(super) fn lookup(
    scope: &Scope<'_>,
    dir: &Path,
    version: i64,
    generation: &Path,
    ref_id: &str,
) -> Option<ResolvedMedia> {
    let canonical = generation.canonicalize().ok()?;
    if scope.instance_id.is_empty()
        || !canonical.starts_with(dir)
        || !dir.starts_with(scope.root.canonicalize().ok()?)
    {
        return None;
    }
    let file = open_transcript(&canonical)?;
    let stamp = Stamp::read(&file)?;
    let compressed = generation.extension().is_some_and(|ext| ext == "zstd");
    let build = || {
        #[cfg(feature = "dsh-test-probes")]
        super::media_parse_probe(scope.root, scope.session_id, true);
        let text = read_media_transcript(file, compressed)?;
        let header = log::parse_header(&text)?;
        if header.id != scope.session_id {
            return None;
        }
        Index::build(&text, header.cwd)
    };
    #[cfg(unix)]
    let slot = find_slot(Key {
        instance: scope.instance_id.to_owned(),
        session: scope.session_id.to_owned(),
        path: canonical.clone(),
        generation: version,
        stamp,
    })?;
    #[cfg(unix)]
    let index = slot.get_or_init(build).as_ref()?;
    // 没有可移植的打开文件身份时，不按时间戳猜测缓存是否仍有效。
    #[cfg(not(unix))]
    let index = build()?;
    let result = ResolvedMedia {
        media: index.items.get(ref_id)?.clone(),
        cwd: index.cwd.clone(),
        dir: dir.to_owned(),
        generation: generation.to_owned(),
        canonical,
        version,
        stamp,
    };
    result.still_current().then_some(result)
}

struct Index {
    cwd: Option<String>,
    items: HashMap<String, SessionMedia>,
    bytes: usize,
}

impl Index {
    fn insert(&mut self, item: SessionMedia) -> Option<()> {
        const MAX_REFERENCES: usize = 4096;
        const MAX_INDEX_BYTES: usize = 2 * 1024 * 1024;
        if self.items.contains_key(&item.ref_id) {
            return Some(());
        }
        self.bytes += item.ref_id.len() * 2
            + item.name.len()
            + item.present_path.as_ref().map_or(0, String::len);
        if self.items.len() == MAX_REFERENCES || self.bytes > MAX_INDEX_BYTES {
            return None;
        }
        self.items.insert(item.ref_id.clone(), item);
        Some(())
    }

    fn build(text: &str, cwd: Option<String>) -> Option<Self> {
        const MAX_CALLS: usize = 262_144;
        let mut index = Self {
            bytes: cwd.as_ref().map_or(0, String::len),
            cwd,
            items: HashMap::new(),
        };
        // 只构造一条事件的 JSON；不同时物化整个解压转写的所有 DTO。
        const MAX_EVENT_BYTES: usize = 2 * 1024 * 1024;
        const MAX_CALL_BYTES: usize = 4 * 1024 * 1024;
        let mut calls = HashMap::new();
        let mut call_bytes = 0;
        let mut register_call = |calls: &mut HashMap<String, bool>, id: &str, present| {
            if !calls.contains_key(id) {
                call_bytes += id.len();
                if call_bytes > MAX_CALL_BYTES || calls.len() == MAX_CALLS {
                    return None;
                }
                calls.insert(id.to_owned(), present);
            }
            Some(())
        };
        for line in text.lines() {
            if line.len() > MAX_EVENT_BYTES {
                return None;
            }
            let Ok(event) = serde_json::from_str::<log::DshEvent>(line) else {
                continue;
            };
            let data = &event.data;
            match event.kind.as_str() {
                "user/message" if string(data.pointer("/source/kind")) == Some("user") => {
                    for block in array(data.get("content")) {
                        if string(block.get("type")) == Some("image") {
                            if let Some(media) = dsh_image(block) {
                                index.insert(media)?;
                            }
                        }
                    }
                }
                "assistant/message" => {
                    for block in array(data.pointer("/message/content")) {
                        if string(block.get("type")) == Some("tool-call") {
                            if let Some(id) = string(block.get("id")) {
                                register_call(
                                    &mut calls,
                                    id,
                                    string(block.get("name")) == Some("present"),
                                )?;
                            }
                        }
                    }
                }
                "tool/call" => {
                    if let Some(id) = string(data.get("callId")) {
                        register_call(&mut calls, id, string(data.get("name")) == Some("present"))?;
                    }
                }
                "deliverables/presented" => {
                    let Some(id) = string(data.get("callId")) else {
                        continue;
                    };
                    if calls.get(id) == Some(&true) {
                        for file in array(data.get("files")) {
                            if let Some(media) = dsh_present_media(id, file) {
                                index.insert(media)?;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        Some(index)
    }
}

fn array(value: Option<&Value>) -> impl Iterator<Item = &Value> {
    value.and_then(Value::as_array).into_iter().flatten()
}

fn string(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str)
}
