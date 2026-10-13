// 开机自动启动（2026-10-11）：官方 tauri-plugin-autostart 插件的命令封装。
//
// **真值源 = OS 侧插件状态**（Windows 写 HKCU\Run 注册表项 / macOS 写
// LaunchAgent），设置页与托盘各自在展示时实时读 `is_enabled`，**不落 settings
// KV 镜像**——用户在系统侧手动删除/改动自启项也能被如实读出，无双源漂移。
// capabilities 零改动：全部走 Rust 侧调用（仅 JS 直调插件 API 才需要权限声明）。
use tauri::{AppHandle, Emitter, Runtime};
use tauri_plugin_autostart::ManagerExt;

/// 注册自启项时随命令行写入的识别参数：插件把该参数追加进平台自启项
///（注册表 Run 键的命令行 / launchd plist 的 ProgramArguments）。应用启动期
/// 据此判定「本次是开机自启」→ 主窗口静默（用户裁决：仅主窗口静默，桌宠按
/// 自身开关照常出现；手动启动无此参数、永远正常显窗）
pub const AUTOSTART_ARG: &str = "--tuvis-autostart";

/// 启动参数判定（纯函数）：参数表**含有**自启标记 → true（不需要是唯一参数）。
/// `lib.rs` setup 早期调用——窗口以 `visible: true` 静态创建，但 setup 先于
/// 事件循环首轮绘制，此处 hide 不闪窗
pub fn is_autostart_launch(args: &[String]) -> bool {
    args.iter().any(|a| a == AUTOSTART_ARG)
}

/// 读当前自启开关（OS 侧实时真值；读取失败如实回传，不给假默认）
#[tauri::command]
pub fn get_autostart_enabled(app: AppHandle) -> Result<bool, String> {
    app.autolaunch().is_enabled().map_err(|e| e.to_string())
}

/// 翻转自启动（设置页命令与托盘菜单臂的**共用单点**）：debug 拒写门 →
/// enable/disable → Windows 引号修复（评审 I1）→ 广播 `autostart-changed`
///（设置页监听同步）。托盘菜单重建由调用方各自执行（泛型 R 的托盘臂走统一
/// 重建路径，命令侧走兜底重建），本函数不做。
/// 失败如实回传（注册表/LaunchAgent 写入可能被系统策略拦截，如杀软/MDM），
/// 调用方按失败回滚乐观更新；失败时**不发事件**（状态未变，无假信号）。
/// 泛型 R：托盘回调持有 `&AppHandle<R>`（Wry 实型），命令与托盘共用本函数
/// 就不必经全局句柄绕行
pub fn flip_and_sync<R: Runtime>(app: &AppHandle<R>, enabled: bool) -> Result<(), String> {
    ensure_writable_runtime()?;
    let manager = app.autolaunch();
    if enabled {
        manager.enable().map_err(|e| e.to_string())?;
        // 引号修复失败 = 条目已写入但注定解析错位 → 撤掉坏条目再回传失败，
        // 杜绝「显示已开启却登录永远起不来」的假真值
        #[cfg(target_os = "windows")]
        match std::env::current_exe() {
            Ok(exe) => match exe.to_str() {
                Some(exe_path) => {
                    if let Err(e) = fix_run_value_quoting(exe_path) {
                        if let Err(d) = manager.disable() {
                            log::warn!("回滚失败的自启条目亦未成功（需人工清理注册表 Run）: {d}");
                        }
                        return Err(e);
                    }
                }
                // 评审 Nit-1：路径非 UTF-8 时修复静默缺席 = 假真值缝——如实告警
                None => log::warn!("当前 exe 路径非 UTF-8 表示，引号修复跳过"),
            },
            Err(e) => log::warn!("取当前 exe 失败，引号修复跳过: {e}"),
        }
    } else {
        manager.disable().map_err(|e| e.to_string())?;
        // 写后验证（用户裁决 2026-10-11 修 ③）：插件 disable 对 HKLM 提权腿的
        // 失败会静默容忍，残留条目把 is_enabled 钉在 true（开关表现为关不掉）——
        // 回读真值，撤不干净就如实报错，不让「关闭」假成功
        match manager.is_enabled() {
            Ok(true) => {
                return Err(
                    "撤销未完成：存在需要管理员权限的遗留自启项（HKLM）。请以管理员身份运行兔维斯再关一次，或在任务管理器的启动应用里清理".to_string(),
                );
            }
            Ok(false) => {}
            Err(e) => {
                log::warn!("disable 成功但回读失败（状态不确定）: {e}");
            }
        }
    }
    let _ = app.emit(
        "autostart-changed",
        serde_json::json!({ "enabled": enabled }),
    );
    Ok(())
}

/// 写注册表/LaunchAgent 仅 release 构建（评审 N3）：dev/debug 注册会把
/// `target/debug` 的开发二进制写进登录项，每次开机登录都被拉起——如实报错拒写
pub fn ensure_writable_runtime() -> Result<(), String> {
    if cfg!(debug_assertions) {
        return Err("Debug 构建不注册开机自启，请用正式安装版验证该开关".into());
    }
    Ok(())
}

/// 「Run 值命令行是否需要改写成带引号形态」（评审 I1 实修，纯函数测试面）：
/// auto-launch 0.6.0 的 Windows `enable` 写 `format!("{exe} {args}")` **不加
/// 引号**——exe 路径含空格（用户名带空格 / 自选安装目录）时，登录启动按
/// CreateProcess 语义解析错位（执行 `C:\Program Files\tuvis.exe` 会被读成
/// `C:\Program`），自启静默失败而 `is_enabled` 只查值名存在照样报真。
/// 返回修正后的带引号命令行；无需修正 → None：
/// * 已带引号（`"` 开头）——别家或旧版写下的正确形态，不碰；
/// * exe 路径无空格——CreateProcess 无歧义，无需引号；
/// * 值不以本应用 exe 路径开头（或 `exePath` 后紧跟的不是空格/终点，如
///   `tuvis.exe2 --x` 的形似值）——不是本应用写过/写法，不接管。
#[cfg(target_os = "windows")]
pub fn quoted_run_command(exe_path: &str, value_data: &str) -> Option<String> {
    if value_data.starts_with('"') || !exe_path.contains(' ') {
        return None;
    }
    let after = value_data.strip_prefix(exe_path)?;
    if !(after.is_empty() || after.starts_with(' ')) {
        return None;
    }
    let args = after.strip_prefix(' ').unwrap_or("");
    // 无参数时剪掉收尾空格
    if args.is_empty() {
        Some(format!("\"{exe_path}\""))
    } else {
        Some(format!("\"{exe_path}\" {args}"))
    }
}

/// enable 成功后把 HKCU\Run 里的本应用条目**原位**改写为带引号形态（I1 收口）。
/// 只扫 HKCU——插件 enable 在非管理员会话下的落点（Dynamic 模式 HKLM 失败即
/// 回落 HKCU）；HKLM = 提权安装的罕路且无提权写不了，如实申报不修。
/// 找到即按值名回写（REG_SZ，与插件 `set_string` 同类型；值名沿用插件写的那个，
/// 后续 disable/is_enabled 按名操作不受影响）；无待修条目 → 静默通过。
#[cfg(target_os = "windows")]
fn fix_run_value_quoting(exe_path: &str) -> Result<(), String> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE};
    use winreg::types::FromRegValue;
    use winreg::RegKey;

    // 评审 Min-1：无需修复（exe 路径无空格）早退——绝大多数默认安装在此
    // 零注册表副作用，不让 open/enum 的瞬态拦截（EDR/AV 等）凭空变成
    // 「开不了自启」的失败面
    if !exe_path.contains(' ') {
        return Ok(());
    }
    const RUN_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Run";
    // 双 hive 扫描（用户裁决 2026-10-11 修 ③）：插件 enable 是 Dynamic 模式
    //（HKLM 失败回落 HKCU）——提权会话下条目会落 HKLM，此处可写则一并修，
    // 打不开（无提权）静默跳过；随后 disable 的写后验证会把 HKLM 残留如实暴露
    for (hive, label) in [(HKEY_CURRENT_USER, "HKCU"), (HKEY_LOCAL_MACHINE, "HKLM")] {
        let Ok(key) =
            RegKey::predef(hive).open_subkey_with_flags(RUN_KEY, KEY_READ | KEY_SET_VALUE)
        else {
            log::info!("打开 {label} Run 键失败（无权限等），跳过该 hive 的引号修复");
            continue;
        };
        // 先收集再回写（终审 Min-1）：Win32 枚举契约对「枚举中途改写键」未定义——
        // 只改 data 不动名/数量在实践安全，但按契约把枚举物化成表后再写，不赌实现
        let entries: Vec<(String, _)> = key
            .enum_values()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("枚举 {label} Run 值失败: {e}"))?;
        for (name, value) in entries {
            // 字符串三型（REG_SZ / REG_EXPAND_SZ / REG_MULTI_SZ，winreg 按
            // from_utf16_lossy 取文本）若字面命中本应用路径即接管；DWORD 等非
            // 字符串型取不出文本直接跳过。回写 set_value 统一为 REG_SZ——若接管了
            // EXPAND/MULTI 型值类型会被归一，评审 Nit-2 如实申报（语义近无害）。
            let Ok(data) = String::from_reg_value(&value) else {
                continue;
            };
            let Some(fixed) = quoted_run_command(exe_path, &data) else {
                continue;
            };
            key.set_value(&name, &fixed)
                .map_err(|e| format!("改写自启条目带引号形态失败（{label}\\{name}）: {e}"))?;
            log::info!("自启条目已原位改写为带引号形态: {label}\\{name}");
        }
    }
    Ok(())
}

#[tauri::command]
pub fn set_autostart_enabled(app: AppHandle, enabled: bool) -> Result<(), String> {
    flip_and_sync(&app, enabled)?;
    // 托盘勾选态回环（对齐托盘预设翻转自刷新模式；评审 M2：失败告警不静默）
    if let Err(e) = crate::plugins::system_tray::update_tray_with_presets(&app) {
        log::warn!("托盘菜单重建失败: {e}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ensure_writable_runtime, is_autostart_launch, AUTOSTART_ARG};

    /// 恰含自启参数 → 判定为自启启动
    #[test]
    fn detects_marker_arg() {
        assert!(is_autostart_launch(&[AUTOSTART_ARG.to_string()]));
        assert!(is_autostart_launch(&[
            "tuvis.exe".to_string(),
            "--tuvis-autostart".to_string()
        ]));
    }

    /// 手动启动（无参 / 其它参数 / 仅前缀形似）不误判
    #[test]
    fn manual_launch_is_not_autostart() {
        assert!(!is_autostart_launch(&[]));
        assert!(!is_autostart_launch(&["tuvis.exe".to_string()]));
        assert!(!is_autostart_launch(&[
            "random-flag".to_string(),
            "--v".to_string()
        ]));
        assert!(!is_autostart_launch(&["--tuvis-autostar".to_string()]));
    }

    /// 评审 N3：debug 构建拒写（cargo test 恒 debug，Err 属预期）
    #[test]
    fn debug_builds_reject_write() {
        assert!(ensure_writable_runtime().is_err());
    }
}

#[cfg(all(test, target_os = "windows"))]
mod quoted_run_command_tests {
    use super::quoted_run_command;

    /// I1 主案：空格路径 + 未加引号的插件原文 → 改写为带引号命令行
    #[test]
    fn fixes_unquoted_space_path() {
        let exe = r"C:\Users\John Doe\AppData\Local\Tuvis\tuvis.exe";
        let data = format!("{exe} --tuvis-autostart");
        assert_eq!(
            quoted_run_command(exe, &data).as_deref(),
            Some(r#""C:\Users\John Doe\AppData\Local\Tuvis\tuvis.exe" --tuvis-autostart"#)
        );
    }

    /// 已带引号（正确形态）不重复改写
    #[test]
    fn leaves_quoted_value_alone() {
        let exe = r"C:\Program Files\Tuvis\tuvis.exe";
        let quoted = r#""C:\Program Files\Tuvis\tuvis.exe" --tuvis-autostart"#;
        assert_eq!(quoted_run_command(exe, quoted), None);
    }

    /// 无空格路径：CreateProcess 无歧义，不需要引号
    #[test]
    fn leaves_space_free_path_alone() {
        assert_eq!(
            quoted_run_command(
                r"C:\Tools\tuvis.exe",
                r"C:\Tools\tuvis.exe --tuvis-autostart"
            ),
            None
        );
    }

    /// 他家条目（不以本 exe 开头 / 形似后缀）不接管
    #[test]
    fn does_not_touch_foreign_values() {
        let exe = r"C:\Tools\tuvis.exe";
        assert_eq!(quoted_run_command(exe, r"C:\Other\tool.exe --flag"), None);
        assert_eq!(quoted_run_command(exe, r"C:\Tools\tuvis.exe2 --flag"), None);
    }

    /// 无参数形态（裸 exe 路径值）：引号包全路径，不加尾巴空格
    #[test]
    fn quotes_path_without_args() {
        let exe = r"C:\Users\John Doe\Tuvis\tuvis.exe";
        assert_eq!(
            quoted_run_command(exe, exe).as_deref(),
            Some(r#""C:\Users\John Doe\Tuvis\tuvis.exe""#)
        );
    }
}
