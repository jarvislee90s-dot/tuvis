//! 环境可见性回归的独立合成帧；不解析环境，不执行真实 syscall，不判断生产完整性。
//! 固定公开 XNU xnu-12377.1.9 / f6217f891ac0bb64f3d375211650a4c1ff8ca1ea 的 64 位布局。
//! 证据：kern_sysctl.c#L1528-L1553 的 omit_env_vars 循环把 path 后全部 NUL 跳过，
//! 包括空 argv[0]；argc=1 时下一轮把第一条环境当 argv 计入 copyout 后停止。
//! https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/kern/kern_sysctl.c#L1528
//! L=15，W=8，argv_start=20，path 后 padding=1；argv[0] 自身的终止 NUL 位于偏移 20。
//! 不声称本机 Darwin 27 的受限权限/omit 分支已实证。

pub const HOME: &str = "/synthetic/visible-user";
pub const DSH_HOME: &str = "/synthetic/hidden-profile";

fn empty_argv_zero_prefix() -> Vec<u8> {
    let mut bytes = 1i32.to_ne_bytes().to_vec();
    // path 的终止 NUL + 1 个对齐 NUL + 空 argv[0] 的终止 NUL。
    bytes.extend_from_slice(b"/synthetic/dsh\0\0\0");
    bytes
}

/// 目标真实环境为 HOME、DSH_HOME 两条；内核 omit 分支只返回第一条 HOME。
/// 模拟源不携带未返回的 DSH_HOME 字节，避免被生产代码越界借用。
pub fn omitted_after_first_home() -> Vec<u8> {
    let mut bytes = empty_argv_zero_prefix();
    bytes.extend_from_slice(b"HOME=/synthetic/visible-user\0");
    bytes
}

/// 第一条环境可以伪装成任意 Apple 字段或“完整”标记；后续 HOME/DSH_HOME 全部被省略。
/// 这些内容本身不能证明 omit_env_vars=false。
pub fn omitted_after_first_record(record: &[u8]) -> Vec<u8> {
    assert!(!record.contains(&0));
    let mut bytes = empty_argv_zero_prefix();
    bytes.extend_from_slice(record);
    bytes.push(0);
    bytes
}

/// 完整可见正控的原始字符串区；visibility 由独立 source 证据提供，绝不从此内容推导。
/// env 尾部与合成 Apple 字符串尾部按 8 字节对齐，不添加环境指针数组或伪造长度字段。
pub fn fully_visible(home_only: bool) -> Vec<u8> {
    let mut bytes = empty_argv_zero_prefix();
    bytes.extend_from_slice(b"HOME=/synthetic/visible-user\0");
    if !home_only {
        bytes.extend_from_slice(b"DSH_HOME=/synthetic/hidden-profile\0");
    }
    while !(bytes.len() - 4).is_multiple_of(8) {
        bytes.push(0);
    }
    bytes.extend_from_slice(b"pfz=0x1000\0");
    while !(bytes.len() - 4).is_multiple_of(8) {
        bytes.push(0);
    }
    bytes
}
