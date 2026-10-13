pub mod connection;
pub mod dao;
pub mod migration;
pub mod schema;

// 重新导出 DAO 模块，保持 crate::database::xxx 引用兼容
pub use dao::agent_tool;
pub use dao::extension;
pub use dao::preset;
pub use dao::session;
pub use dao::settings;

// 重新导出公共类型（保持 crate::database::Type -> crate::database::Type 兼容）
pub use dao::agent_tool::SubAgentRecord;
pub use dao::base_snapshot::BaseSnapshotItemRecord;
pub use dao::extension::{AssignmentRecord, ExtensionRecord};
pub use dao::preset::{PresetItemRecord, PresetRecord};
pub use dao::stash::StashEntryRecord;

// 重新导出公共函数
pub use dao::agent_tool::{
    enabled_tool_ids, ensure_tool_rows, get_tool_enabled, list_sub_agents, set_tool_enabled,
};
pub use dao::archive::{
    agent_type_from_tool_id, delete_archive, query_archive_all, register_sessions,
    SessionArchiveRow,
};
pub use dao::base_snapshot::{
    destroy_base_snapshot, get_base_snapshot, save_base_snapshot, set_active_preset,
};
pub use dao::board_hidden::{board_hidden_hide, board_hidden_ids, board_hidden_unhide};
pub use dao::extension::{
    delete_assignments_for, delete_extension, disable_subagent_assignment, ensure_extension,
    insert_extension, list_all_assignments, list_assignments, list_extensions,
    list_registered_native_names, migrate_legacy_extension_id, upsert_assignment,
    upsert_assignment_with_subagent,
};
pub use dao::preset::{
    create_preset, create_preset_with_meta, delete_preset, get_preset, get_preset_items,
    list_presets, record_preset_application, record_preset_application_subagent, update_preset,
};
pub use dao::resource_binding::{
    delete_resource_binding, get_resource_binding, list_resource_bindings, tool_allowed,
    upsert_resource_binding, ResourceBindingRecord,
};
pub use dao::session::{cleanup_stale_sessions, find_status, update_session_status};
pub use dao::settings::{get_setting, set_setting};
pub use dao::stash::{mark_stash_restored, record_stash, unrestored_stash};
pub use dao::tool_resident::{
    delete_tool_residents_for, is_tool_resident, list_tool_residents, set_tool_resident,
};
pub use dao::unread::UnreadSessionRecord;
pub use dao::unread::{
    clear_tool as clear_unread_tool, delete as delete_unread, list as list_unread_sessions,
};
pub use dao::usage::{
    count_rows_conn, load_cursors_conn, load_sessions_conn, purge_detail_before_conn,
    query_counters_conn, query_daily_conn, query_detail_conn, save_cursors_conn, upsert_daily_conn,
    upsert_detail_conn, upsert_session_conn, CounterAggRow, DailyAggRow, DetailAggRow,
    SessionDimRow,
};

/// 初始化数据库（兼容旧 store::init() 调用）
pub fn init() {
    connection::init();
    if let Ok(conn) = connection::open() {
        let _ = migration::migrate(&conn);
    }
    // 种子工具启用行（全部 enabled=1，幂等；缺行视为启用）
    dao::agent_tool::ensure_tool_rows();
}
