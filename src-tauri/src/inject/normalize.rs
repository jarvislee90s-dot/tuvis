//! 注入消息归一（裁决 6 定死）：手机多行输入 → 字面 `\n` 拼接单行——终端侧永远是
//! 「打字 + 一次回车」，避开 TUI 多次提交与 paste-buffer 粘贴确认两个版本敏感雷区；
//! 桌面不换行展示为已登记的已知限制（二期 spec 附录 C #6）。
//! F7⑩ 追加第二段归一：换行归一之后滤除无文本语义的控制字符（收尾批 P1 定稿为
//! C0 + DEL + C1 三段，依据见 normalize_newlines 注意二）。

/// 换行符（\n 与 \r\n）→ 字面 `\n` 两字符，再滤除 C0 控制字符，其余原样
///
/// 注意一：首步必须先按 `"\r\n"` 两字符序列整体归一（clippy collapsible_str_replace
/// 建议的单趟 `replace(['\n', '\r'], ..)` 会把 CRLF 拆成两个 `\n`，违背裁决 6 的
/// 「CRLF 整体归一为一个 `\n`」语义，不可采纳）。
///
/// 注意二（F7⑩ C0 滤除；收尾批 P1 扩 DEL 与 C1）：换行归一后正文中已无裸 \n/\r
/// （均成字面 `\n` 两字符），此时滤除控制字符即「只删控制字符、不伤换行语义」。依据：
/// - C0（U+0000–U+001F，\t 0x09 一并）：B 族把裸 ESC 当按键吃（M6R F3）；正文
///   控制字符无 TUI 语义，滤除=最安全归一；
/// - DEL（U+007F）：crossterm（B 族 codex）把 0x7F 当退格键——消息含 DEL 直发
///   终端会删除已打入的内容（比忽略更糟），与滤 C0 理由同源；
/// - C1（U+0080–U+009F）：同为无文本语义的不可见控制字符；0x9B/0x9D 在 8-bit
///   模式是 CSI/OSC 引入符，TUI 解析面与 C0 同源；真实用户文本几乎不会有意含
///   C1，粘贴脏字符滤除最安全（NBSP 是 U+00A0 不在 C1 区，不受影响）。
pub fn normalize_newlines(text: &str) -> String {
    let no_newlines = text.replace("\r\n", "\\n").replace(['\n', '\r'], "\\n");
    no_newlines
        .chars()
        .filter(|&c| !is_strippable_control(c))
        .collect()
}

/// 无文本语义控制字符判定（F7⑩ C0；收尾批 P1 扩 DEL 与 C1，依据见
/// [`normalize_newlines`] 注意二）。逐字符（Unicode 标量）判定——配合 `chars()`
/// 过滤天然多字节安全：emoji/中文按整标量整体处理，不存在按字节滤除的撕裂风险
fn is_strippable_control(c: char) -> bool {
    let cp = c as u32;
    cp <= 0x1F || cp == 0x7F || (0x80..=0x9F).contains(&cp)
}

/// 组装最终注入文本（**注入通道唯一出口**）——丁T3 裁2 起两种形态：
///
/// - **普通消息**：`{归一正文} [mobile {设备花名}]`——**签名后置**（原名在**最前**，
///   用户动机：正文在前一眼可读、溯源信息不变，见批次丁计划 §2.5 裁2）；
/// - **斜杠命令**（[`is_slash_message`]，正文以 `/` 开头）：**裸注入**——无签名。
///   斜杠命令的前后缀都会破坏命令解析（问题 8 实锤：手打 `/permissons` 被前缀
///   毁掉），故裸注入；其溯源走 兔维斯 审计页（`action=slash` + 设备名，裁2 明确
///   「终端不留痕是可接受的，审计页必须留」）。
///
/// **为什么在**本函数判斜杠（而不是入队层 / flush 层）：本函数是注入文本的唯一组装
/// 出口，队列存的就是它的产物（`content = 入队时 compose 完毕`，见 `inject::queue`
/// 的文档），故在这里分流 = 队列/投递/审计三处天然一致，flush 层无需再判一次
/// （在 flush 层判就得从 composed 文本反推原始正文，那正是双重判定漂移的来源）。
///
/// 设备花名同样归一：昵称来自手机端用户输入，可能含换行，不得破坏单行不变量。
///
/// # `Result` 的原因（裁决 24b：花名的 cmd 安全白名单，就在本单点判）
///
/// 签名 `[mobile <花名>]` 会随载荷进**无头 CLI 的命令行**（`codex queue --message` /
/// `kimi --prompt` / `opencode run` / `zcode.cjs --prompt`），Windows 上这些 CLI 常是 npm
/// 垫片（`.cmd` ⇒ `cmd /c` 重解析整条命令行）⇒ 花名与用户正文**同一条重解析面**。R3 当时
/// 只判了正文（[`crate::inject::headless::turn::cmd_shim_body_refusal`]），签名面**登记为敞口**；
/// 本函数在**拼接之前**过白名单判据
/// （[`crate::inject::headless::turn::device_name_refusal`]），不安全即 `Err(reason)`。
///
/// **判据落在这一层（共享组装单点），不在各通道的 argv 构造器**：本函数是注入文本的
/// **唯一组装出口**（见上），把门设在这里，后来新增的通道**没有绕开的路径**（若设在
/// 各通道 argv 构造器，新通道忘接一次就是敞口）。调用方收到 `Err` 一律 fail closed
/// （回执档 `refused`、零字节投递，原因点名字符），**不静默改写花名**。
///
/// **不按通道分岔**（与正文判据的形态条件不同）：花名是**注册期的值**、用户改一次名即可，
/// 而「正文走 stdin / `.exe` 直装」这类例外对花名并不成立（同一个花名进任意 argv 通道都
/// 有风险）——故这里无条件判：终端注入路径同样拒（终端打字没有 shell 重解析面，但按通道
/// 分岔会给未来的通道留缺口，不值得）。存量已登记的危险花名由本判据在**投递时**兜住
/// （**不做 migration**：注册点只挡新的，旧的靠这里，见 `device_name_refusal` 的调用点节）。
///
/// **有意从严（裁决 25，2026-10-05 用户裁决——勿改为按威胁面放宽）**：本单点放行的是
/// **字符类白名单**（Unicode 字母数字 + 空格 `-` `_` `.` `·`）——自定义花名用常规字符即可，
/// **emoji / 未列举符号一律拒**；**即便**某符号未必真能构成 `cmd` 语义，也**不按威胁面放宽**
/// （用户理由：余量留在威胁面之上，不做「该字符看起来无害」的减法论证）。判据本体与理由
/// 全文见 [`crate::inject::headless::turn::device_name_refusal`] 的「有意从严」节。
///
/// **斜杠消息不判花名**：`/` 开头走**裸注入**（无签名，见下），花名根本不进载荷——
/// 判它就成了「与风险无关的拒绝」（同 claude 的 stdin 例外面）。审计侧的设备名另走
/// `endpoint_audit`，与本判据无关。
pub fn compose_injection(device_name: &str, text: &str) -> Result<String, String> {
    compose_injection_flagged(device_name, text, true)
}

/// 带**签名开关**的组装变体（2026-10-05 用户裁决）：`signature=false`（设置
/// 「远程消息带设备签名」关闭）→ 普通消息也**裸注入**——签名是纯溯源便利，
/// 每条都吃 token；溯源真源在注入审计页（设备名逐条在账），终端不留痕可接受
/// （与斜杠命令的裸注入同一裁决口径）。队列存的是 compose 产物 → 开关在入队
/// 时刻生效（已入队消息维持入队时形态，语义自洽）。**花名白名单与签名开关
/// 正交**（裁决 24b：花名是注册期的值，不按载荷形态分岔——`signature=false`
/// 时花名虽不进载荷，仍过门；斜杠消息例外见上）。`Result` 语义同上。
pub fn compose_injection_flagged(
    device_name: &str,
    text: &str,
    signature: bool,
) -> Result<String, String> {
    let body = normalize_newlines(text);
    if is_slash_message(text) {
        // 裸注入：不加签名（斜杠命令的任何附加文本都会使其失效）——花名不进载荷，故不判花名
        return Ok(body);
    }
    // 花名白名单（裁决 24b）：拼接之前判（安全理由见上）；与签名开关正交，
    // 不按「signature=false ⇒ 花名不进载荷」分岔——按通道/形态分岔会给新通道留缺口
    if let Some(reason) = crate::inject::headless::turn::device_name_refusal(device_name) {
        return Err(reason);
    }
    if !signature {
        // 签名关：裸正文（花名门已在上方无条件通过——纵深防御，拒绝面保持一致）
        return Ok(body);
    }
    Ok(format!("{} [mobile {}]", body, normalize_newlines(device_name)))
}
/// 「远程消息带设备签名」设置的**读取单点**（api.rs 两处 compose 调用共用）：
/// settings KV `remote_message_signature`，缺省 **off**（2026-10-05 用户裁决——
/// 默认省 token，想要溯源签名的用户在设置里打开）。**连接注入式**（调用方经
/// `RemoteState.store.with` 传入——测试内存库零接触真实 `~/.tuvis`，生产=全局库）。
pub(crate) fn message_signature_enabled_conn(conn: &rusqlite::Connection) -> bool {
    crate::database::dao::settings::get_setting_conn(conn, "remote_message_signature")
        .map(|v| v == "on")
        .unwrap_or(false)
}

/// 是否「斜杠命令消息」（裁2 的裸注入判据，**单点**——compose 分流与审计 action 取用
/// 同一份判据，两处不可能漂移）。
///
/// 判据落在**归一后**的正文上（不是原始串）：终端最终看到的是归一产物，`\x1b/permissions`
/// 这类含控制字符的输入归一后才是 `/permissions`，以原始串判会把它当普通消息并追加签名
/// ——那正是问题 8 的形态。前导空白**不**跳过（TUI 的斜杠命令要求行首即 `/`，带前导
/// 空格的输入本就无法触发命令，按普通消息处理让签名语义保持可预期）。
pub fn is_slash_message(text: &str) -> bool {
    normalize_newlines(text).starts_with('/')
}

/// 剥去**尾部** `[mobile 设备名]` 签名，返回注入正文（丁T3 裁2 的连带改造）。
///
/// # 为什么必须存在（一个真 bug 的根线）
///
/// 签名后置后，**同一设备的所有消息共享同一个尾部**（` [mobile iPhone]`）。任何
/// 「取尾 N 字符」的判据（确认戳 [`super::confirm::stamp_of`]、屏读探针）都会因此
/// 退化成「设备级」判据：第二条消息的尾部与第一条相同 → 第一条的戳在第二条上假命中
/// （裁2 点名要求适配的正是这条）。故尾部类判据一律先经本函数取正文，再截尾。
///
/// 容错口径（宽进严出，只剥**形态完整**的尾部签名）：
/// - 先 `trim_end`（终端输入行尾部空白不可见，F8 既有口径）；
/// - 必须**以 `]` 结尾**且能 `rfind("[mobile ")` 到签名起点——两者缺一即原样返回
///   （正文里偶然出现 `[mobile` 字样不误剥）；
/// - 签名前的分隔空白随签名一并剥掉（compose 产出形态为 `{正文} [mobile X]`）；
/// - 设备名可含空格与多字节（compose 侧的归一产物），按 `rfind` + 尾 `]` 整段切。
///
/// **不做**的事：不识别旧版**前置**形态（`[mobile X] {正文}`）。理由：本函数服务于
/// 「取正文尾部」这一判据，而旧形态的正文尾部本来就是真正的正文（前缀不在尾部），
/// 取尾结论天然正确——升级窗口内既存队列项因此零特判即可工作。
///
/// # 已知边界：旧前缀形态 + 正文尾方括号 → 整段剥掉（F4-5 登记，不在生产路径上）
///
/// 判据是「以 `]` 结尾 + 能 `rfind("[mobile ")`」，故 `[mobile iPhone] 正文 [1]`
/// 这种**同时**含旧前缀与正文尾方括号的串会被整段剥成 `""`（`rfind` 取的是第一个
/// `[mobile ` 的位置）。与前端正则（`/\s*\[mobile[^\]]*\]$/`，要求签名紧贴尾部）
/// 口径不同——前端会保留该串原样。
///
/// **为什么无害**（生产不可达 + 后果方向保守）：
/// - 生产路径喂进来的只有 `compose_injection` 的两种产物：`{正文} [mobile X]`
///   （尾部签名，正例）或斜杠命令裸注入（无签名，恒等变换）。旧前缀形态只存在于
///   **升级窗口内既存队列项**（T3 之前入队的行），而那种行的正文尾部通常不是方括号
///   ——此时串**不以 `]` 结尾**，本函数**原样返回**，取尾得到的正是真实正文尾，
///   判据仍然成立；只有「旧前缀 + 正文尾恰好是 `[1]` 这类方括号」的交集才会多剥，
///   实机未见；
/// - 即便发生，后果是**戳变短/变空**：空戳在 [`super::confirm::stamp_in_messages`]
///   里恒不中（`!stamp.is_empty()` 守卫）→ 确认层降级为 `Submitted`（中性「已投递
///   未确认」）而非谎报送达——方向保守。
///
/// **实测对照**（由 `strips_known_boundary_old_prefix_plus_trailing_bracket` 钉住）：
/// | 输入 | 产物 |
/// |---|---|
/// | `正文 [1] [mobile iPhone]`（生产形态） | `正文 [1]` |
/// | `[mobile iPhone] 正文 [1]`（边界） | `""` |
/// | `[mobile iPhone] 正文`（无尾方括号） | 原样返回（取尾即真实正文尾） |
/// | `[mobile iPhone]`（纯签名） | `""` |
///
/// **收口点**：若未来需要在升级窗口内精确区分，判据改为「签名必须**紧贴尾部**
/// （`]` 前无其他内容）且其前是空白或行首」即可与前端同口径；当前不做（为一个
/// 不可达组合增加判据，会让主路径的容错面变窄）。
pub fn strip_mobile_signature(content: &str) -> &str {
    let trimmed = content.trim_end();
    if !trimmed.ends_with(']') {
        return trimmed;
    }
    let Some(idx) = trimmed.rfind("[mobile ") else {
        return trimmed;
    };
    trimmed[..idx].trim_end()
}

/// **截断核（私有单点）**：超限 → 前 `max_chars` 个字符；未超限 → `None`。
/// [`summarize`] 与 [`truncate_with_size_marker`] 都只经此切一刀——**不各写一份
/// `chars().take`**（两份写法改一处漏一处不会编译报错）。
fn cut_over_limit(text: &str, max_chars: usize) -> Option<String> {
    (text.chars().count() > max_chars).then(|| text.chars().take(max_chars).collect())
}

/// 审计摘要（W5：只存摘要不入全文，防审计库膨胀）——超限时以 `…` 收尾
/// （尾巴即「还有内容」的约定，**不带长度**：审计面不需要，见 [`truncate_with_size_marker`]）。
pub fn summarize(text: &str, max_chars: usize) -> String {
    match cut_over_limit(text, max_chars) {
        Some(cut) => format!("{cut}…"),
        None => text.to_string(),
    }
}

/// **展示用**截断（P0 安全：超限必须**显式**报出真实总长，不得静默砍）。
///
/// 用于**面向用户**的展示面（审批卡的「将要执行的命令」）：用户要据此点「批准」，
/// 静默截断 = 让人批准一条自己读不全的命令。故超限时产物 =
/// 前 `max_chars` 个字符 + 「…已截断，共 N 字符」（N = 原文**真实**字符数）。
/// 未超限时**逐字原文**（没有隐藏任何内容，就无需声明长度）。
///
/// **与 [`summarize`] 的区别（勿合并）**：`summarize` 是审计摘要（只存摘要、防库膨胀），
/// 本函数是用户展示（保真 + 自报长度）；两者语义不同、阈值也各自由调用方给常数
/// （如 `cli_three::APPROVAL_INPUT_DISPLAY_CHARS`）。
pub fn truncate_with_size_marker(text: &str, max_chars: usize) -> String {
    match cut_over_limit(text, max_chars) {
        Some(cut) => format!("{cut}…已截断，共 {} 字符", text.chars().count()),
        None => text.to_string(),
    }
}

/// 审计摘要截断长度（W5 只存摘要）
pub const AUDIT_SUMMARY_CHARS: usize = 80;

#[cfg(test)]
mod tests {
    use super::*;

    /// 裁决 6：换行符 → 字面 \n 两字符；其余原样（含已有的字面 \n 不动）
    #[test]
    fn newlines_become_literal_backslash_n() {
        assert_eq!(normalize_newlines("第一行\n第二行"), "第一行\\n第二行");
        assert_eq!(normalize_newlines("已是字面\\n"), "已是字面\\n");
        assert_eq!(normalize_newlines("无换行"), "无换行");
        assert_eq!(normalize_newlines("a\r\nb"), "a\\nb"); // \r\n 整体归一，杜绝裸 \r
    }

    /// W1 回归（丁T3 裁2 改写：前缀 → **后缀**）[mobile 设备名] + 归一正文
    #[test]
    fn compose_suffixes_and_normalizes() {
        assert_eq!(
            compose_injection("iPhone", "改一下\n继续")
                .expect("测试花名在册（白名单内，见 turn::device_name_refusal）"),
            "改一下\\n继续 [mobile iPhone]"
        );
    }

    /// 丁T3 裁2：`/` 开头消息**裸注入**（无前缀无签名）——问题 8 实锤（手打
    /// `/permissons` 被前缀毁掉）。判据落在**归一后**正文（控制字符先滤再判）
    #[test]
    fn slash_messages_are_bare() {
        assert_eq!(
            compose_injection("iPhone", "/permissions")
                .expect("测试花名在册（白名单内，见 turn::device_name_refusal）"),
            "/permissions"
        );
        assert_eq!(
            compose_injection("iPhone", "/plan")
                .expect("测试花名在册（白名单内，见 turn::device_name_refusal）"),
            "/plan"
        );
        assert_eq!(
            compose_injection("iPhone", "/permissions  申请写入")
                .expect("测试花名在册（白名单内，见 turn::device_name_refusal）"),
            "/permissions  申请写入",
            "斜杠命令后的参数原样保留（签名会破坏参数解析）"
        );
        // 归一先行：裸 ESC 前缀的「斜杠命令」归一后才是 `/permissions`，必须以归一
        // 产物判（否则控制字符脏输入会被当普通消息并追加签名=问题 8 形态复发）
        assert_eq!(
            compose_injection("iPhone", "\x1b/permissions")
                .expect("测试花名在册（白名单内，见 turn::device_name_refusal）"),
            "/permissions"
        );
        // 多行归一同样生效
        assert_eq!(
            compose_injection("iPhone", "/plan\n额外")
                .expect("测试花名在册（白名单内，见 turn::device_name_refusal）"),
            "/plan\\n额外"
        );
        // 非行首斜杠不是命令（TUI 的斜杠命令要求行首即 `/`）→ 走普通消息带签名
        assert_eq!(
            compose_injection("iPhone", "价格 /permissions 是多少")
                .expect("测试花名在册（白名单内，见 turn::device_name_refusal）"),
            "价格 /permissions 是多少 [mobile iPhone]"
        );
        assert_eq!(
            compose_injection("iPhone", " /permissions")
                .expect("测试花名在册（白名单内，见 turn::device_name_refusal）"),
            " /permissions [mobile iPhone]",
            "前导空白不跳过（带空格的输入本就无法触发命令）"
        );
    }

    /// 签名开关（2026-10-05 用户裁决）：`signature=false` → 普通消息也裸注入——
    /// 每条消息的 `[mobile X]` 尾签是纯溯源便利，默认关以省 token；溯源真源在
    /// 注入审计页（设备名逐条在账）。斜杠命令在开关开/关两态下都必须裸注入。
    #[test]
    fn compose_flagged_signature_off_is_bare() {
        use super::compose_injection_flagged as compose;
        assert_eq!(
            compose("iPhone", "帮我看看这个文件", false),
            "帮我看看这个文件"
        );
        assert_eq!(
            compose("iPhone", "帮我看看这个文件", true),
            "帮我看看这个文件 [mobile iPhone]",
            "开关开 = 既有签名形态（compose_injection 兼容壳同款）"
        );
        // 斜杠命令两态都裸（签名开关不得给命令追加任何文本）
        assert_eq!(compose("iPhone", "/plan", false), "/plan");
        assert_eq!(compose("iPhone", "/plan", true), "/plan");
    }

    /// `is_slash_message` 与 compose 的裸注入判据同源（单点判据的自锁）：
    /// 审计 action=slash 的判定与实际注入形态必须逐一对应，不得有一处漂移
    #[test]
    fn slash_predicate_matches_compose_bare_form() {
        for (text, is_slash) in [
            ("/permissions", true),
            ("/plan on", true),
            ("\x1b/permissions", true),
            ("/", true),
            ("普通消息", false),
            (" /permissions", false),
            ("", false),
        ] {
            assert_eq!(is_slash_message(text), is_slash, "判据格：{text:?}");
            let composed = compose_injection("iPhone", text)
                .expect("测试花名在册（白名单内，见 turn::device_name_refusal）");
            assert_eq!(
                composed.contains("[mobile iPhone]"),
                !is_slash,
                "判据与注入形态必须一致（{text:?} → {composed:?}）"
            );
        }
    }

    /// 丁T3：尾部签名剥离（stamp/屏读探针取「正文尾部」的第一步）。
    /// 只剥**形态完整**的尾部签名；正文里偶然出现 `[mobile` 字样不误剥
    #[test]
    fn strips_trailing_signature_only_when_well_formed() {
        assert_eq!(strip_mobile_signature("正文 [mobile iPhone]"), "正文");
        assert_eq!(
            strip_mobile_signature("多行\\n正文 [mobile iPhone\\n15]"),
            "多行\\n正文"
        );
        // 尾空白先 trim（终端输入行尾部不可见空白，F8 同口径）
        assert_eq!(strip_mobile_signature("正文 [mobile iPhone]   "), "正文");
        // 无签名 → 原样
        assert_eq!(strip_mobile_signature("正文"), "正文");
        // 未闭合的 `[mobile` 不剥（避免吃掉正文）
        assert_eq!(
            strip_mobile_signature("正文 [mobile iPhone"),
            "正文 [mobile iPhone"
        );
        // 中段出现（旧版前置形态 / 正文引用）→ 尾部无 `]` 签名时不剥
        assert_eq!(
            strip_mobile_signature("[mobile iPhone] 正文"),
            "[mobile iPhone] 正文"
        );
        // 正文里引用签名样式但不以 `]` 结尾 → 不剥
        assert_eq!(
            strip_mobile_signature("看看这个 [mobile X] 标签"),
            "看看这个 [mobile X] 标签"
        );
        // 空串 / 纯签名
        assert_eq!(strip_mobile_signature(""), "");
        assert_eq!(strip_mobile_signature("[mobile iPhone]"), "");
    }

    /// F4-5 **已知边界的回归锁**（把文档里的宣称变成可执行断言，防未来被「顺手修好」
    /// 却没人知道口径变了）：**旧前缀形态 + 正文尾方括号**会被整段剥掉（取第一个
    /// `[mobile ` 的位置），与前端正则（要求签名紧贴尾部）口径不同。
    ///
    /// 本断言**刻意钉住现状**而非期望值——生产路径喂不进这个组合（见
    /// [`strip_mobile_signature`] 的边界小节），而后果方向保守（空戳恒不中 →
    /// 确认层降级 Submitted，非谎报）。若将来收口（判据改为「签名紧贴尾部」），
    /// 本用例必须显式改写并同步上游文档。
    #[test]
    fn strips_known_boundary_old_prefix_plus_trailing_bracket() {
        // 现状（钉住）：整段剥成空
        assert_eq!(
            strip_mobile_signature("[mobile iPhone] 正文 [1]"),
            "",
            "F4-5 边界现状：旧前缀 + 尾方括号 → 整段剥（见函数文档「已知边界」）"
        );
        // 后果保守性（同一条注释的宣称也要可执行）：空产物作为戳恒不中
        assert!(
            !super::super::confirm::stamp_in_messages(
                &["任意正文"],
                super::super::confirm::stamp_of("")
            ),
            "空戳恒不中 ⇒ 该边界最坏只降级为 Submitted（非谎报送达）"
        );
        // 生产形态对照格：尾部签名（新口径）→ 正确剥出正文
        assert_eq!(
            strip_mobile_signature("正文 [1] [mobile iPhone]"),
            "正文 [1]"
        );
        // 旧前缀形态但正文尾**无**方括号（不以 `]` 结尾）→ 原样返回；其取尾结果
        // 仍是真实正文尾（判据成立——见函数文档的实测对照表）
        assert_eq!(
            strip_mobile_signature("[mobile iPhone] 正文"),
            "[mobile iPhone] 正文",
            "不以 `]` 结尾 ⇒ 原样返回（早期形态的取尾仍正确）"
        );
    }

    /// 纯核：截尾 24 字符时签名不参与——`strip_mobile_signature` 的产物作为
    /// `stamp_of` 输入的等价性（两函数组合的端到端锁，防未来只改一侧）
    #[test]
    fn strip_then_stamp_matches_stamp_of_directly() {
        let composed = compose_injection("iPhone", "请帮我检查一下这个文件")
            .expect("测试花名在册（白名单内，见 turn::device_name_refusal）");
        let manual = strip_mobile_signature(&composed);
        assert_eq!(
            super::super::confirm::stamp_of(&composed),
            super::super::confirm::stamp_of(manual),
            "stamp_of(composed) 必须等于 stamp_of(剥签名后的正文)"
        );
    }

    /// 审计摘要：超长截断加省略号
    #[test]
    fn summarize_truncates() {
        assert_eq!(summarize("abcdef", 4), "abcd…");
        assert_eq!(summarize("abc", 4), "abc");
        assert_eq!(summarize("一二三四五", 4), "一二三四…"); // 多字节按 chars 计
    }

    /// **R2 展示截断**：超限必须报**真实总长**（不是保留长度）；未超限逐字原文。
    /// 与 [`summarize`] 的差别正在这里——那个只有一个 `…`（审计面不需要长度）。
    #[test]
    fn truncate_with_size_marker_names_the_true_total() {
        assert_eq!(
            truncate_with_size_marker("abcdef", 4),
            "abcd…已截断，共 6 字符",
            "标记里的数字必须是**原文**长度（6），不是保留长度（4）"
        );
        assert_eq!(truncate_with_size_marker("abc", 4), "abc", "恰在上限：逐字");
        assert_eq!(
            truncate_with_size_marker("abcd", 4),
            "abcd",
            "等于上限：逐字"
        );
        assert_eq!(
            truncate_with_size_marker("一二三四五", 4),
            "一二三四…已截断，共 5 字符",
            "多字节按 chars 计总长（5），不按字节"
        );
        // 两条口径**不可互换**：审计摘要不会出现长度文案
        assert!(!summarize("abcdef", 4).contains("字符"));
    }

    /// 裸 CR（奇异客户端）也归一
    #[test]
    fn bare_cr_normalizes() {
        assert_eq!(normalize_newlines("a\rb"), "a\\nb");
    }

    /// 设备花名同样归一（用户可设昵称，堵单行不变量缺口）——丁T3 起签名在**尾部**。
    ///
    /// **R12-S2 改判（裁决 24b）**：本用例原断言「含换行的花名归一成字面 `\n` 后照常进签名」
    /// （`"hi [mobile iPhone\n15]"`）。白名单落地后，**含换行的花名在拼接前就被拒**——
    /// 换行本身不是白名单字符（`\` 也不是），归一只发生在**已放行**的花名上。故本用例改钉
    /// 「归一仍是花名的必经步，但危险字符根本到不了归一」：把原断言移到
    /// [`compose_refuses_cmd_unsafe_device_name`]，此处只留**放行花名**的归一行为。
    #[test]
    fn compose_normalizes_device_name_too() {
        // 放行花名里的多字节/空格原样进签名（归一不吞正常字符）
        assert_eq!(
            compose_injection("小明的 iPhone", "hi").unwrap(),
            "hi [mobile 小明的 iPhone]"
        );
        // 危险花名（换行 + 反斜杠形态）在拼接前被拒——不是归一后照进
        assert!(compose_injection("iPhone\n15", "hi").is_err());
    }

    /// F7⑩：C0 控制字符滤除——B 族把裸 ESC 当按键吃（M6R F3 实证），正文控制字符
    /// 无 TUI 语义，滤除=最安全归一。字面 `\n` 归一产物与普通中文/ASCII 原样。
    #[test]
    fn c0_controls_are_stripped() {
        // 裸 ESC+CSI、BEL、\t 全部滤除（\t 属 C0）；可见字符原样
        let out = normalize_newlines("a\x1b[31m红\x07色\t缩进");
        assert_eq!(out, "a[31m红色缩进");
        // 零残留断言：产物不含任何 0x00–0x1F 区间字符
        assert!(out.chars().all(|c| (c as u32) > 0x1F));
        // 字面 `\n` 两字符（换行归一产物）保留，同样零 C0
        let with_nl = normalize_newlines("行一\n行二\x1b");
        assert_eq!(with_nl, "行一\\n行二");
        assert!(with_nl.chars().all(|c| (c as u32) > 0x1F));
        // 普通中文/ASCII 原样
        assert_eq!(normalize_newlines("普通中文 abc 123"), "普通中文 abc 123");
        // DEL (0x7F) 不属 C0 区——其滤除依据（B 族退格）与 C0 不同源，单独断言见
        // del_is_stripped（收尾批 P1：便于未来对 DEL/C1 单独回退）
        // 端到端锁：compose_injection 输出同样零 C0（注入通道唯一出口）
        let composed = compose_injection("iPhone", "文本\x1b[0m\x07收尾")
            .expect("测试花名在册（白名单内，见 turn::device_name_refusal）");
        assert!(composed.chars().all(|c| (c as u32) > 0x1F));
    }

    /// 收尾批 P1：DEL(0x7F) 纳入滤除——crossterm（B 族 codex）把 0x7F 当退格键，
    /// 消息含 DEL 直发终端会删除已打入的内容（比忽略更糟），与滤 C0 理由同源。
    /// 与 C1 分开断言，便于未来单独回退
    #[test]
    fn del_is_stripped() {
        let out = normalize_newlines("del\x7fx");
        assert_eq!(out, "delx", "DEL 必须滤除（B 族退格误删已打内容）");
        assert!(
            !out.chars().any(|c| (c as u32) == 0x7F),
            "零残留：产物不含 DEL"
        );
        // 端到端锁：compose_injection 同样零 DEL（注入通道唯一出口）
        let composed = compose_injection("iPhone", "a\u{7f}b")
            .expect("测试花名在册（白名单内，见 turn::device_name_refusal）");
        assert!(!composed.chars().any(|c| (c as u32) == 0x7F));
    }

    /// 收尾批 P1 C1 连带评估（默认裁决：一并滤除）：C1（U+0080–U+009F）同为无文本
    /// 语义的不可见控制字符；0x9B/0x9D 在 8-bit 模式是 CSI/OSC 引入符，TUI 解析面
    /// 与 C0 同源；真实用户文本几乎不会有意含 C1，粘贴脏字符滤除最安全。与 DEL
    /// 分开断言，便于未来单独回退
    #[test]
    fn c1_controls_are_stripped() {
        // 采样：0x9B（8-bit CSI 引入符）、0x85（NEL）、0x90（DCS）、0x9D（OSC 引入符）
        let out = normalize_newlines("a\u{9b}31m\u{85}b\u{90}c\u{9d}d");
        assert_eq!(out, "a31mbcd", "C1 控制字符必须滤除");
        assert!(
            !out.chars().any(|c| (0x80..=0x9F).contains(&(c as u32))),
            "零残留：产物不含任何 C1 字符"
        );
        // NBSP（U+00A0）不在 C1 区——合法展示字符不受连坐，原样保留
        assert_eq!(normalize_newlines("a\u{a0}b"), "a\u{a0}b");
    }

    /// 收尾批 P2：多字节安全钉死——混入控制字符的正文经逐字符（Unicode 标量）
    /// 过滤，emoji（😀 U+1F600，4 字节）与中文必须完整保留、控制字符零残留；
    /// 防未来误改成按字节滤除（会把多字节标量撕成无效字节）
    #[test]
    fn emoji_multibyte_survives_filter() {
        let out = normalize_newlines("部署😀\u{7f}完成\u{1b}[31m中文\u{85}收尾");
        assert_eq!(out, "部署😀完成[31m中文收尾");
        // emoji 原样（整标量存活，未撕裂）
        assert!(out.contains('\u{1F600}'), "emoji 必须原样保留：{out}");
        assert!(
            out.contains("部署") && out.contains("中文"),
            "中文必须原样保留"
        );
        // 控制字符零残留（C0 + DEL + C1 全区间）
        assert!(
            out.chars().all(|c| {
                let cp = c as u32;
                cp > 0x1F && cp != 0x7F && !(0x80..=0x9F).contains(&cp)
            }),
            "零残留：产物不含 C0/DEL/C1 任何字符"
        );
    }

    /// **R12-S2 先红用例（裁决 24b：花名 cmd 安全白名单）**：花名含 `cmd` 元字符时
    /// **不得**进载荷——签名是**我们自己拼**的（`[mobile <花名>]`），而载荷在无头通道会进
    /// argv（Windows npm 垫片经 `cmd /c` 重解析命令行，同 [`super::headless::turn`] 的
    /// `CMD_SHIM_METACHARS` 依据）。R3 只判了**用户正文**，签名面当时**登记为敞口**
    /// （`inject/headless/codex.rs` 模块头「残留（如实登记）」）——本用例钉住收口。
    ///
    /// 先红证据（实现前跑）：`compose_injection("a&b", "hi")` 当时返回
    /// `"hi [mobile a&b]"`（断言 `!composed.contains("a&b")` 失败）。
    ///
    /// 期望形态：**拼接之前** fail closed（`compose_injection` 回 `Err(原因)`，调用方
    /// 按 `refused` 档零字节投递），**不是**静默改写花名（改写了用户就不知道实际发的是什么）。
    #[test]
    fn compose_refuses_cmd_unsafe_device_name() {
        for name in [
            "a&b",
            "手机%1",
            "say\"hi",
            "小明(工作)",
            "小明的手机📱",
            "iPhone\n15",
            "a|b",
            "x^y",
        ] {
            let err = compose_injection(name, "hi")
                .expect_err("危险花名必须在拼接前被拒（不得产出载荷）");
            assert!(
                err.contains("cmd") && err.contains("未投递"),
                "拒绝文案必须说清 cmd 重解析面 + 零字节投递: {err}"
            );
        }
        // 点名具体字符（用户能据此改名）
        let e = compose_injection("a&b", "hi").unwrap_err();
        assert!(e.contains('&'), "必须点名是哪个字符: {e}");
        // 正常中文/英文花名必须放行（白名单不得误伤中文——票面点名的回归面）
        assert_eq!(
            compose_injection("小明的手机", "hi").unwrap(),
            "hi [mobile 小明的手机]"
        );
        assert_eq!(
            compose_injection("iPad Pro", "hi").unwrap(),
            "hi [mobile iPad Pro]"
        );
        // 斜杠消息 = 裸注入（无签名）：花名不进载荷 ⇒ 不判花名（不是「与风险无关的拒绝」）
        assert_eq!(compose_injection("a&b", "/plan").unwrap(), "/plan");
    }
}
