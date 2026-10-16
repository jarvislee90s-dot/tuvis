// dsh 会话日志解码：多 frame 拼接的 zstd 容器（每个追加批次独立一帧，M0 F1）
// torn 尾帧（EOF 打断）是设计内常态 —— 尾帧解码失败则前缀容错（评审升级 #1）

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

pub struct DecodedFrames {
    pub text: String,
    /// 被容错丢弃的帧数（诊断计数：正常 ≤1，只应是尾帧）
    pub torn_frames: usize,
}

fn frame_offsets(bytes: &[u8]) -> Vec<usize> {
    let mut offsets = Vec::new();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if bytes[i..i + 4] == ZSTD_MAGIC {
            offsets.push(i);
            i += 4;
        } else {
            i += 1;
        }
    }
    offsets
}

pub fn decode_zstd_frames(bytes: &[u8]) -> Result<DecodedFrames, String> {
    let offsets = frame_offsets(bytes);
    if offsets.is_empty() {
        return Err("DSH_LOG_MAGIC_MISSING: dsh log: 未找到 zstd frame 魔数".into());
    }
    let mut text = String::new();
    let mut torn = 0usize;
    for (idx, &start) in offsets.iter().enumerate() {
        let end = offsets.get(idx + 1).copied().unwrap_or(bytes.len());
        match zstd::stream::decode_all(&bytes[start..end]) {
            Ok(part) => text.push_str(&String::from_utf8_lossy(&part)),
            Err(e) => {
                // 监控只读场景：任何解码失败的帧按丢失处理（尾帧 torn 常态；
                // 中间帧失败≈损坏，跳过并 warn——单帧丢失只影响增量精度，不阻塞出卡）
                torn += 1;
                log::warn!("dsh log: 第 {} 帧解码失败已跳过: {}", idx, e);
            }
        }
    }
    Ok(DecodedFrames {
        text,
        torn_frames: torn,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用 zstd crate 造多帧拼接文件（每批一帧，模拟 dsh 追加写入）
    fn make_frames(batches: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for b in batches {
            out.extend_from_slice(&zstd::stream::encode_all(b.as_bytes(), 3).unwrap());
        }
        out
    }

    #[test]
    fn decodes_all_frames() {
        let bytes = make_frames(&["line1\nline2\n", "line3\n", "line4\n"]);
        let out = decode_zstd_frames(&bytes).unwrap();
        assert_eq!(out.text, "line1\nline2\nline3\nline4\n");
        assert_eq!(out.torn_frames, 0);
    }

    #[test]
    fn tolerates_torn_tail_frame() {
        // M0 评审升级 #1：zstd 后端不写终帧，运行中会话尾帧 EOF 打断是常态
        let mut bytes = make_frames(&["line1\n", "line2\nline3\n"]);
        bytes.truncate(bytes.len() - 5); // 截断尾帧
        let out = decode_zstd_frames(&bytes).unwrap();
        assert_eq!(out.text, "line1\n");
        assert_eq!(out.torn_frames, 1);
    }

    #[test]
    fn rejects_no_magic() {
        assert!(decode_zstd_frames(b"not zstd at all").is_err());
    }

    #[test]
    fn decode_all_on_torn_data_documented_behavior() {
        // 开工实证（备忘 §A3）：记录 decode_all 整文件解压在 torn 数据上的行为
        let mut bytes = make_frames(&["a\n", "b\n"]);
        bytes.truncate(bytes.len() - 5);
        let whole = zstd::stream::decode_all(&bytes[..]);
        // 无论报错还是部分成功，我们的逐帧解码都必须给出确定结果：
        let ours = decode_zstd_frames(&bytes).unwrap();
        assert_eq!(ours.text, "a\n");
        let _ = whole; // 行为记录：当前实现返回 Err（观察于本测试运行时）
    }
}
