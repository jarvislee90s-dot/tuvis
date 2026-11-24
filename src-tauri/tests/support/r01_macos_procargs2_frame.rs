//! KERN_PROCARGS2 合成输入；只编码帧，不解码、不计算生产期望值。
//! 固定公开 XNU xnu-12377.1.9 / f6217f891ac0bb64f3d375211650a4c1ff8ca1ea 的 64 位布局：
//! kern_exec.c exec_save_path / exec_extract_strings，kern_sysctl.c sysctl_procargsx。
//! argv_start = 4 + round_up(16 + path_len_with_nul, 8) - 16；不允许调用方任选 padding。
//! 空 argv[0] 与其余空参数都必须计入 argc，不能用扫描连续 NUL 推导 argv 起点。
//! 本夹具在最后一条 env 的 C 字符串终止符处结束，供纯解析边界测试使用；
//! 不模拟平台相关 Apple 字符串区，不声称是本机 Darwin 27 的活体完整 dump。

pub struct Frame {
    pub bytes: Vec<u8>,
    pub argv_start: usize,
    pub argv_end: usize,
}

impl Frame {
    pub fn new(executable: &[u8], argv: &[&[u8]], env: &[&[u8]]) -> Self {
        assert!(!argv.is_empty());
        assert!(!executable.contains(&0));
        let argc = i32::try_from(argv.len()).unwrap();
        let mut bytes = argc.to_ne_bytes().to_vec();
        bytes.extend_from_slice(executable);
        bytes.push(0);
        // 原始字符串区含 16 字节 executable_path= 前缀；sysctl 去掉前缀再前置 argc。
        let path_with_nul = executable.len() + 1;
        let argv_start = 4 + (16 + path_with_nul).next_multiple_of(8) - 16;
        bytes.resize(argv_start, 0);
        for argument in argv {
            assert!(!argument.contains(&0));
            bytes.extend_from_slice(argument);
            bytes.push(0);
        }
        let argv_end = bytes.len();
        for record in env {
            assert!(!record.contains(&0));
            bytes.extend_from_slice(record);
            bytes.push(0);
        }
        Self {
            bytes,
            argv_start,
            argv_end,
        }
    }

    pub fn set_argc(&mut self, argc: i32) {
        self.bytes[..4].copy_from_slice(&argc.to_ne_bytes());
    }

    /// bytes_written 是 sysctl 成功回写的长度；buffer.len() 是传给内核的原始容量。
    /// 尾部故意不用零填充，防止解析器借用未写入区中的 NUL 补全截断帧。
    pub fn spare_buffer(&self) -> Vec<u8> {
        let mut buffer = self.bytes.clone();
        buffer.resize(buffer.len() + 64, 0xa5);
        buffer
    }
}
