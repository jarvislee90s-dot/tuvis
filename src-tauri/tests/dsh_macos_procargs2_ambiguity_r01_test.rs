//! R01 复审：必须按已核实的布局定位 argv[0]，不能把所有前导 NUL 当作路径填充。
//! 独立合成帧；不修改正在运行的目标，不读取任何进程环境。
//!
//! 证据固定为公开 XNU xnu-12377.1.9，提交 f6217f891ac0bb64f3d375211650a4c1ff8ca1ea：
//! https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/kern/kern_exec.c#L6051
//! https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/kern/kern_sysctl.c#L1436
//! exec_save_path 写入 16 字节 executable_path= 与含 NUL 的 path；exec_extract_strings
//! 按目标指针宽度 W 补齐后记录 ip_startargv；sysctl 去掉前缀并前置 4 字节 argc。
//! 因而 argv_start=4+round_up(16+L,W)-16，其中 L 包含 path 终止 NUL。
//! 本文件固定 W=8；L=15/16/17 对应 argv_start=20/20/28、padding=1/0/7。
//! 这些已核实布局中的空 argv[0] 可以恢复，不能把“跳过 NUL”造成的歧义当作 ABI 定论。
//! 不宣称该公开版本等于本机 Darwin 27/xnu-13432.1.9；后者的版本映射/活体帧未验证。
//! 不覆盖未知 Darwin 布局、32 位目标、Rosetta 或会修改初始字符串区的目标程序。
//! 本轮不运行 Cargo；生产回归尚未执行，不能声称行为 RED。
#![cfg(all(target_os = "macos", target_pointer_width = "64"))]

use multi_agents_manager_lib::services::dsh_control::process_environment::{
    parse_macos_procargs2, AllowlistedEnvironment, EnvironmentField, Procargs2Error,
};

/// argv_start 使用逐例手算的字面偏移，不复刻生产解析器的定位算法。
/// 只编码输入；保留 env 尾部/Apple 字符串区的 8 字节对齐，未写入区用 0xa5 污染。
fn decode_aligned(
    path: &[u8],
    argv_start: usize,
    argv: &[&[u8]],
) -> Result<AllowlistedEnvironment, Procargs2Error> {
    assert!(!path.contains(&0));
    let mut buffer = i32::try_from(argv.len()).unwrap().to_ne_bytes().to_vec();
    buffer.extend_from_slice(path);
    buffer.push(0);
    assert!(buffer.len() <= argv_start);
    buffer.resize(argv_start, 0);
    for argument in argv {
        assert!(!argument.contains(&0));
        buffer.extend_from_slice(argument);
        buffer.push(0);
    }
    buffer.extend_from_slice(b"DSH_HOME=/synthetic/present\0HOME=/synthetic/user\0");
    while (buffer.len() - 4) % 8 != 0 {
        buffer.push(0);
    }
    // 来自独立 Apple 字符串区的非白名单键，不是环境分界的替代判据。
    buffer.extend_from_slice(b"pfz=0x1000\0");
    while (buffer.len() - 4) % 8 != 0 {
        buffer.push(0);
    }
    let written = buffer.len();
    buffer.resize(written + 32, 0xa5);
    parse_macos_procargs2(&buffer, written)
}

fn expected() -> AllowlistedEnvironment {
    AllowlistedEnvironment {
        dsh_home: EnvironmentField::Value("/synthetic/present".to_owned()),
        home: EnvironmentField::Value("/synthetic/user".to_owned()),
    }
}

#[test]
fn empty_argv_zero_after_one_alignment_nul_preserves_present_dsh_home() {
    // L=15，路径后只允许跳过 1 个 padding NUL；下一个 NUL 必须计入 argc。
    assert_eq!(
        decode_aligned(b"/synthetic/dsh", 20, &[b""]),
        Ok(expected())
    );
}

#[test]
fn empty_argv_zero_without_path_padding_cannot_be_skipped_as_alignment() {
    // L=16，padding=0；路径后的第一个 NUL 已经是 argv[0]，不是可忽略填充。
    assert_eq!(
        decode_aligned(b"/synthetic/dshx", 20, &[b""]),
        Ok(expected())
    );
}

#[test]
fn empty_leading_arguments_after_seven_alignment_nuls_do_not_consume_environment() {
    // L=17，padding=7；两个空参数都必须消耗 argc 槽位。
    // HOME= 假值属于第三个参数，真实 HOME/DSH_HOME 都必须完整保留。
    assert_eq!(
        decode_aligned(
            b"/synthetic/dshxx",
            28,
            &[b"", b"", b"HOME=/synthetic/argument"]
        ),
        Ok(expected())
    );
}

#[test]
fn nonempty_argv_zero_remains_readable_at_each_verified_alignment_boundary() {
    // 正控：不能通过拒绝所有带路径 padding 的帧来满足空 argv[0] 回归。
    for (path, start) in [
        (b"/synthetic/dsh".as_slice(), 20),
        (b"/synthetic/dshx".as_slice(), 20),
        (b"/synthetic/dshxx".as_slice(), 28),
    ] {
        assert_eq!(decode_aligned(path, start, &[b"dsh"]), Ok(expected()));
    }
}

#[test]
fn empty_middle_and_final_arguments_remain_readable_after_known_path_padding() {
    // 正控：空 argv[0] 修复不应改成拒绝所有空参数。
    assert_eq!(
        decode_aligned(
            b"/synthetic/dsh",
            20,
            &[b"dsh", b"", b"HOME=/synthetic/argument", b""],
        ),
        Ok(expected())
    );
}
