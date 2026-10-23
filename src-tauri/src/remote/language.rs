// 三终端语言自适应（2026-10-22 需求书 §5）桌面侧基建，两件事：
//
// 1. **C-3 桌面当前语言内存源**：真值来自前端推送（启动 i18n 初始化后 + 每次切换，
//    与托盘重建同路触发）；Rust **只存不判**——未推送时保持「未知」态，绝不自行推断
//    （桌面语言判定是前端 i18n 的职责，两侧值集以 C-1 为准，二值 "zh" | "en"）。
//    消费方：GET /host 的 C-4 下发（api::host）。
//
// 2. **C-5 配对设备语言手动记账**：键 = `remote_devices.id`（**配对身份同源**——
//    复用 pairing 的设备 id，绝不另造第二套身份）；值 = 该设备是否设置过手动语言 +
//    所选语言。**行存在即手动标记**（manual 恒 true），自动跟随结果绝不写账；
//    记录只由移动端手动切换端点（POST /device-language）写入；**配对完成即清零**
//    （重新配对 = 回到自动跟随，L2 / §6.4-3——upsert 复用同 id 时旧记录不得穿越）；
//    换网络 / 换 IP 不影响（键不随 origin 变化）。

use std::sync::{Arc, Mutex};

/// 界面语言二值桶（C-1：与前端 `src/i18n/language-prefs.ts` 的 `UiLanguage` 同名同义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiLanguage {
    Zh,
    En,
}

impl UiLanguage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Zh => "zh",
            Self::En => "en",
        }
    }

    /// 解析（C-1 值集钉死）：只认 `"zh"` / `"en"`，其余一律拒——Rust 侧不判语言，
    /// 收到的值不在值集内就是非法输入，不得猜测归流。
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "zh" => Some(Self::Zh),
            "en" => Some(Self::En),
            _ => None,
        }
    }
}

/// 桌面当前语言内存源（C-3 单一内存源）：`None` = 前端尚未推送（未知态）。
///
/// Clone 共享同一份内部态：生产 = `RemoteState` 单例持有一份（`remote::STATE`），
/// tauri 推送命令与 `/host` 下发经同一实例读写；测试各自构造独立实例，
/// 避免并行用例互踩（capability_table 同款纪律）。
#[derive(Clone, Default)]
pub struct LanguageSource(Arc<Mutex<Option<UiLanguage>>>);

impl LanguageSource {
    /// 推送（覆盖式：最后一次推送即当前语言，与前端「以最后一次为准」同语义）
    pub fn set(&self, lang: UiLanguage) {
        *self.0.lock().unwrap() = Some(lang);
    }

    /// 读当前语言；`None` = 未知（前端尚未推送——启动即推，正常连接下不可达；
    /// 下发侧如实给 null，由移动端决策链兜底，绝不在此替前端猜一个值）
    pub fn get(&self) -> Option<UiLanguage> {
        *self.0.lock().unwrap()
    }
}

// ==== C-5 配对设备语言手动记账（SQL 层；连接由 DeviceStore.with 锁内提供）====

/// 记录设备的手动语言选择（upsert：再次手动切换覆盖前值——最后一次手动为准，U14）。
/// **唯一写入口**，只允许移动端手动切换端点调用；表行存在即「该设备设置过手动语言」。
pub fn set_manual_language(
    conn: &rusqlite::Connection,
    device_id: &str,
    lang: UiLanguage,
    now: i64,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO remote_device_language (device_id, language, updated_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(device_id) DO UPDATE SET language = ?2, updated_at = ?3",
        rusqlite::params![device_id, lang.as_str(), now],
    )
    .map(|_| ())
    // 错误串用英文且不带码前缀：错误码必须配 zh/en locale 双键（src/i18n/**
    // 不在本任务可改范围），而裸中文串会触发错误码迁移棘轮——内部失败串
    // （仅进日志与 500 error 体，无用户文案面）走英文原文
    .map_err(|e| format!("language ledger write failed: {e}"))
}

/// 读设备的手动语言记录：无记录（新设备身份 / 从未手动切换）→ `None`
/// （C-5 ①「manual 态为 false 或等价」的等价形态）。账本只经
/// [`set_manual_language`] 写入（值集受 parse 钉死），脏值防御性视同无记录。
pub fn manual_language(conn: &rusqlite::Connection, device_id: &str) -> Option<UiLanguage> {
    conn.query_row(
        "SELECT language FROM remote_device_language WHERE device_id = ?1",
        [device_id],
        |r| {
            let s: String = r.get(0)?;
            Ok(UiLanguage::parse(&s))
        },
    )
    .ok()
    .flatten()
}

/// 清除设备的手动语言记录（**配对完成即清零**——L2 / §6.4-3：重新配对 = 回到
/// 自动跟随）。只由配对完成路径（`api::persist_and_cookie`）调用。
///
/// 为什么必须在配对完成时清：fingerprint upsert（`persist_device_with_fingerprint`）
/// 命中旧行时**设备 id 不变**——同一浏览器不吊销直接重配会复用同一身份，若不清零，
/// 旧的手动语言记录会穿越重配幸存，§6.4-3「重新配对 → 记录清零」即失效。
/// 尽力而为语义（`touch_device` 同款）：清除失败不阻断配对——残留行只属于同一
/// 设备身份，该设备下次手动切换即覆盖，无安全面。
pub fn clear_manual_language(conn: &rusqlite::Connection, device_id: &str) {
    let _ = conn.execute(
        "DELETE FROM remote_device_language WHERE device_id = ?1",
        [device_id],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 内存库（真机调用序：schema::init → migration::migrate，见 DeviceStore::memory 注释）
    fn memory_conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        crate::database::migration::migrate(&conn).unwrap();
        conn
    }

    fn ledger_rows(conn: &rusqlite::Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM remote_device_language", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap()
    }

    // ==== UiLanguage / LanguageSource（C-3）====

    /// C-1 值集钉死：只认 "zh" / "en"；其余（含大小写变体 / 空串 / BCP-47 长标签）
    /// 一律拒——Rust 只存不判，非法值不得被猜成某个语言
    #[test]
    fn ui_language_parse_accepts_only_canonical_two_values() {
        assert_eq!(UiLanguage::parse("zh"), Some(UiLanguage::Zh));
        assert_eq!(UiLanguage::parse("en"), Some(UiLanguage::En));
        assert_eq!(
            UiLanguage::parse("ZH"),
            None,
            "大小写变体不收（值集二值钉死）"
        );
        assert_eq!(UiLanguage::parse("zh-CN"), None);
        assert_eq!(UiLanguage::parse(""), None);
        assert_eq!(UiLanguage::parse("fr"), None);
        assert_eq!(UiLanguage::Zh.as_str(), "zh");
        assert_eq!(UiLanguage::En.as_str(), "en");
    }

    /// C-3：未推送时内存源为「未知」态（None），且不自行判定；
    /// 两次推送（zh 后 en）→ 依次读到 zh、en（最后一次推送生效）
    #[test]
    fn language_source_starts_unknown_and_follows_pushes_in_order() {
        let src = LanguageSource::default();
        assert_eq!(
            src.get(),
            None,
            "未推送 = 未知态：Rust 不判语言，不得给默认值"
        );
        src.set(UiLanguage::Zh);
        assert_eq!(src.get(), Some(UiLanguage::Zh));
        src.set(UiLanguage::En);
        assert_eq!(src.get(), Some(UiLanguage::En), "第二次推送覆盖前值");
    }

    /// LanguageSource Clone 语义：克隆体共享同一份内部态（生产单例经 STATE 读写
    /// 与 /host 下发共用同一实例的结构前提）
    #[test]
    fn language_source_clone_shares_state() {
        let src = LanguageSource::default();
        let cloned = src.clone();
        cloned.set(UiLanguage::En);
        assert_eq!(src.get(), Some(UiLanguage::En));
    }

    // ==== 记账（C-5 ①–④）====

    /// C-5 ①：无记录设备读 → 无记录（manual 态 false 的等价形态 None）
    #[test]
    fn ledger_no_record_reads_as_none() {
        let conn = memory_conn();
        assert_eq!(manual_language(&conn, "no-such-device"), None);
    }

    /// C-5 ②：手动写 `{language:"zh"}` 后读 → 记录为 zh（manual = true 即行存在）
    #[test]
    fn ledger_manual_write_then_read_back() {
        let conn = memory_conn();
        set_manual_language(&conn, "dev-1", UiLanguage::Zh, 1000).unwrap();
        assert_eq!(manual_language(&conn, "dev-1"), Some(UiLanguage::Zh));
        assert_eq!(ledger_rows(&conn), 1, "同一设备只占一行（upsert）");
    }

    /// C-5 ③：再写 `{language:"en"}` → 覆盖为 en（最后一次手动为准，U14），不新增行
    #[test]
    fn ledger_second_manual_write_overwrites() {
        let conn = memory_conn();
        set_manual_language(&conn, "dev-1", UiLanguage::Zh, 1000).unwrap();
        set_manual_language(&conn, "dev-1", UiLanguage::En, 2000).unwrap();
        assert_eq!(
            manual_language(&conn, "dev-1"),
            Some(UiLanguage::En),
            "最后一次手动切换为准"
        );
        assert_eq!(ledger_rows(&conn), 1, "覆盖而非新增");
    }

    /// C-5 ④：新设备身份读 → 无记录（重新配对 = 新身份 = 回到自动跟随，L2；
    /// 且既有设备的记录互不可见——键级隔离）
    #[test]
    fn ledger_new_device_identity_has_no_record() {
        let conn = memory_conn();
        set_manual_language(&conn, "dev-old", UiLanguage::Zh, 1000).unwrap();
        assert_eq!(manual_language(&conn, "dev-new"), None, "新身份天然无记录");
        assert_eq!(
            manual_language(&conn, "dev-old"),
            Some(UiLanguage::Zh),
            "旧设备记录不受新设备影响（按设备逐一记账）"
        );
    }

    /// 账本值集防御：只经 set_manual_language 写入的表，脏值（外部手改）读作无记录，
    /// 绝不 panic（诚实口径：读不出合法语言 = 视同未手动选择）
    #[test]
    fn ledger_dirty_value_reads_as_no_record_not_panic() {
        let conn = memory_conn();
        conn.execute(
            "INSERT INTO remote_device_language (device_id, language, updated_at)
             VALUES ('dev-x', 'fr', 1)",
            [],
        )
        .unwrap();
        assert_eq!(manual_language(&conn, "dev-x"), None);
    }

    /// 配对完成清零（L2 / §6.4-3）：clear 后记录消失（回到自动跟随）；
    /// 未知设备 clear 是无记录 no-op 不报错；按键级隔离，他设备记录不受影响
    #[test]
    fn ledger_clear_removes_record_and_is_noop_for_unknown_device() {
        let conn = memory_conn();
        set_manual_language(&conn, "dev-1", UiLanguage::En, 1000).unwrap();
        set_manual_language(&conn, "dev-2", UiLanguage::Zh, 1000).unwrap();
        clear_manual_language(&conn, "dev-1");
        assert_eq!(
            manual_language(&conn, "dev-1"),
            None,
            "配对完成清零：该设备回到自动跟随"
        );
        assert_eq!(
            manual_language(&conn, "dev-2"),
            Some(UiLanguage::Zh),
            "按键级隔离：他设备记录不受影响"
        );
        // 未知设备（本就无记录）clear = no-op，不报错
        clear_manual_language(&conn, "no-such-device");
        assert_eq!(manual_language(&conn, "no-such-device"), None);
    }
}
