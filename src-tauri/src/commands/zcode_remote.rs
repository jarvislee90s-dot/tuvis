// ZCode 官方远控链接配置（A4，设计 §4.3/§5.2）：桌面端读写命令。
//
// 校验单点在服务端（services::zcode_control），本模块只做全局 DB 接线与
// Err(String) 形态适配：错误串 = 结构化码前缀（ZCODE_URL_*，errcode.rs 约定），
// 绝不携带原链接（完整链接不写日志/错误/夹具）。保存失败不落库，旧合法值不变；
// 空白保存与删除同源。手机端只经 GET /m/api/v1/official-control 读取，无写接口
// （首版桌面编辑，设计 §4.2）。

use crate::services::zcode_control::{self, ZcodeRemoteConfig};

/// 读取本机 ZCode 官方远控链接配置（读取即复验：历史坏值 → Invalid/url null）。
/// 只读 settings KV，不读取官方账号、不发任何外部 HTTP。
#[tauri::command]
pub fn zcode_remote_config_get() -> Result<ZcodeRemoteConfig, String> {
    let conn = crate::database::connection::DB.lock().unwrap();
    Ok(zcode_control::get_config(&conn))
}

/// 保存 ZCode 官方远控链接（空白 = 删除）。校验通过才写入；
/// 失败回传码前缀错误（不含原 URL），已存合法值保持不变。
#[tauri::command]
pub fn zcode_remote_config_set(raw_url: String) -> Result<ZcodeRemoteConfig, String> {
    let conn = crate::database::connection::DB.lock().unwrap();
    zcode_control::set_config(&conn, &raw_url).map_err(|e| e.to_string())
}
