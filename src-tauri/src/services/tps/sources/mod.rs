// TPS 原生只读来源（2026-10-24 计划 Task 3；Spec §4 首版来源表 + §6 ReadBatch 契约）。
//
// ## 读取契约（Spec §6「来源读取统一返回」）
//
// 每来源一个纯读取函数：
//
// ```text
// read_incremental(home: &Path, cursor: &SourceCursor, now_ms: i64) -> Result<ReadBatch, UsageError>
// ```
//
// * **入参 cursor 只读**：本层绝不落库、不推进任何已提交水位——next_cursor /
//   source_state 随 ReadBatch 返回，由 Task 7 的采样编排经 `store::commit_batch`
//   在同事务里提交（本层**不得**调用 store，也不得写任何 Tuvis 账本）。
// * **sourceState 始终返回**（Spec §6）：无数据、缺计时也照返；只有「整源读不了」
//   才走 `Err`（错误通道），此时调用方保持原水位不动（Spec §6「读取失败不改变水位」）。
// * **字段缺失返回诊断（diagnostics），不报 0**（Spec §3「失败缺计时为缺失，不填 0」）：
//   缺失以 `Option::None` + 诊断串表达，绝不伪造 0 值或 0 时刻。
// * **只读新增**：文件只解码自上次水位之后的追加内容，SQL 只取 rowid 水位之后的
//   尾部（≤200 行/次）；禁止为新图每轮全量解压历史或全表 COUNT（Spec §2/§6）。
// * **复用既有读取纪律**：与 `services/usage/collectors/{dsh,zcode}.rs` 同根定位、
//   同一套只读打开（`monitor::sqlite::open_usage_db`，ro 优先、绝不 checkpoint、
//   尊重 WAL）与「在飞行行不跨水位 / 僵尸安全阀」口径。
//
// ## 来源与工具 id
//
// `source` 字段与 usage 域同一套小写工具 id 字符串（`UsageSourceId::db_id()`）：
// dsh.rs = `"dsh"`、zcode.rs = `"zcode"`、kimi.rs = `"kimi"`、workbuddy.rs = `"workbuddy"`、
// codex.rs = `"codex"`（Task 4）、claude.rs = `"claude"`（Task 5）、opencode.rs =
// `"opencode"`（Task 6，读自家观察插件落盘的事实 JSONL）。
//
// **opencode.rs 的形态备注**：它读的是**自家插件**（resources/tps/opencode/，经
// install.rs 装进宿主 plugins 目录）写的 `<home>/.tuvis/tps/opencode/facts.jsonl`
// （append-only JSONL），不是宿主自己的数据——宿主（opencode 2.0.22）没有可只读
// 消费的本地计时面，服务端观察插件是 Spec §4 该行的正路。计时方法固定
// StreamObserved；适配验收状态以 opencode::ADAPTER_STATUS 为准
// （sourceState.detail 前缀 `status=` 机器可读）。
//
// **claude.rs 是本目录的形态例外**：它不是 `read_incremental(home, …)` 的文件/SQL
// 读取器，而是本机 `/v1/traces` 接收器的 **OTLP JSON 纯解码器**（`decode_trace(body)`；
// socket/端口/凭证守卫在 `super::install`，接线归 Task 7）——遥测从本机回环进来，
// 没有可增量扫描的数据根，L1–L3 扫描预算不适用（解码单批 ≤1MiB 有独立上限，
// 见 install::MAX_BODY_BYTES）。其余读取器纪律（纯函数、诊断不报 0、稳定键幂等）
// 对它同样生效。
//
// ## 根解析与测试隔离
//
// 两个读取器都以**入参 home 直接拼接**工具数据根（`home/.dsh`、`home/.zcode`），
// 刻意**不认** `DSH_HOME` / `ZCODE_HOME` 环境覆盖（usage 采集器认，见
// `monitor::dsh::dsh_home_with`）：TPS 读取必须是入参的纯函数，测试传 tempdir 即
// 天然隔离，绝不触真机目录。调用方（Task 7 编排）若需影子目录，直接传影子 home。
//
// ## 宿主实例
//
// `host_instance` = 工具数据根的稳定短摘要（sha2 前 12 hex，`host_instance_tag`）：
// 同机多实例（不同 home/影子根）各成一段事实，主键 source+host+call_key 不互撞。

pub mod claude;
pub mod codex;
pub mod dsh;
pub mod kimi;
pub mod opencode;
pub mod workbuddy;
pub mod zcode;

use sha2::{Digest, Sha256};

/// 工具数据根 → 稳定宿主实例标签（sha2 前 12 hex；跨重启逐字稳定，不泄漏完整路径）
pub(crate) fn host_instance_tag(root: &std::path::Path) -> String {
    let digest = Sha256::digest(root.to_string_lossy().as_bytes());
    let head: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
    head
}

/// 单批增量尾读上限（独立复核 R3）：一轮最多从一个文件读入这么多新增字节；超出部分
/// 下轮续读（游标只推进到已消费边界）。8 MiB 远大于任何合法单行/单帧，又把前台
/// 5 秒节奏下每轮的 IO 与内存上界钉死在与**历史长度无关**的常量上。
pub(crate) const TAIL_READ_MAX_BYTES: usize = 8 * 1024 * 1024;

/// 增量尾读：从 `offset` 起 seek 读取至多 `max_bytes`，返回 (读到的字节, 是否已到文件尾)。
///
/// 这是文件类来源（kimi / workbuddy / dsh / opencode）唯一的读文件入口：读取量 =
/// min(新增量, 上限)，与已消费前缀大小无关——1 GB 已消费日志追加一行时只读那一行
///（独立复核 R3：此前 `std::fs::read` 整文件读入再切片，前台每 5 秒重读 1 GB）。
/// 读取途中文件收缩（截断/轮转）按短读处理，由调用方的「size < offset → 归零重读」
/// 守卫在下一轮纠正。
pub(crate) fn read_tail(
    path: &std::path::Path,
    offset: u64,
    max_bytes: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    if offset >= len {
        return Ok((Vec::new(), true));
    }
    f.seek(SeekFrom::Start(offset))?;
    let want = usize::try_from(len - offset)
        .unwrap_or(usize::MAX)
        .min(max_bytes);
    let mut buf = vec![0u8; want];
    let mut filled = 0usize;
    while filled < want {
        let n = f.read(&mut buf[filled..])?;
        if n == 0 {
            break; // 读取途中文件收缩：短读，不假装读满
        }
        filled += n;
    }
    buf.truncate(filled);
    let eof = offset.saturating_add(filled as u64) >= len;
    Ok((buf, eof))
}

#[cfg(test)]
mod read_tail_tests {
    use super::*;

    /// R3（独立复核 P1）：增量尾读的读取量只与新增量有关，与已消费历史长度无关。
    #[test]
    fn read_tail_reads_only_bytes_after_offset_regardless_of_history_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.log");
        // 6 MiB 已消费前缀 + 一行小追加
        let prefix: Vec<u8> = vec![b'x'; 6 * 1024 * 1024];
        std::fs::write(&path, &prefix).unwrap();
        let appended = b"{\"type\":\"new\"}\n";
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(appended).unwrap();
        }
        let (bytes, eof) = read_tail(&path, prefix.len() as u64, TAIL_READ_MAX_BYTES).unwrap();
        assert_eq!(bytes.len(), appended.len(), "只读到追加的字节，不是整文件");
        assert_eq!(&bytes[..], &appended[..]);
        assert!(eof);
    }

    /// 尾部超过单批上限：只读上限字节并报告未到 EOF（未消费部分下轮续读）
    #[test]
    fn read_tail_honours_byte_cap_and_reports_more_pending() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("burst.log");
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let (bytes, eof) = read_tail(&path, 10, 1_000).unwrap();
        assert_eq!(bytes.len(), 1_000);
        assert_eq!(&bytes[..], &data[10..1_010]);
        assert!(!eof, "上限截断时必须报告仍有未读字节");
        // offset 到达或越过文件尾：空读 + eof
        let (tail, eof2) = read_tail(&path, data.len() as u64, 1_000).unwrap();
        assert!(tail.is_empty());
        assert!(eof2);
        let (tail3, eof3) = read_tail(&path, data.len() as u64 + 5, 1_000).unwrap();
        assert!(tail3.is_empty());
        assert!(eof3);
    }
}
