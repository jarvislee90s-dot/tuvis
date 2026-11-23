//! 一次性安装 OpenCode TUI 只读路由观察插件。
//!
//! 安装记录保存已写入文件的摘要，避免覆盖或删除后来被用户修改的文件。
//!
//! 事务语义（2026-11-12 裁定）：
//! - **跨进程串行**：安装/卸载/收尾在 `opencode/.tuvis-route-observer.lock` 的
//!   `flock` 排他锁内执行；锁文件**不随卸载删除**（删了会出现两组互不约束的锁）。
//!   协作锁只约束兔维斯自身操作，锁外变化靠锁内重查的实际结果校验。
//! - **锁内重查**：进入锁后重读插件与 marker 的实际内容再决策；重查**可见**的
//!   外部替换被拒绝（内容不同 → Conflict，绝不覆盖/删除）。已知边界：重查与最终
//!   rename/remove 之间的窗口在非协作同 UID 写入者模型下没有 POSIX 条件替换原语
//!   可消除；本实现保证「要么写入自有完整内容、要么失败保留现场，绝无部分写」，
//!   不承诺该窗口内外来文件绝不变。
//! - **可恢复提交**：plugin 字节恰等于本轮待写内容而 marker 缺失/失配 = 本次未完成的
//!   自有提交，续写 marker 完成安装（内容逐字节相等 = 即使外部写入也内容等价）。
//! - **无 marker 的卸载拒绝**：归属不可证时保留文件，明确冲突。
//! - **卸载裁决在锁内**：幂等 Absent / 残留收尾 / 归属匹配删除 / 冲突保留，全部按
//!   锁内实际状态裁定；锁前只做路径安全校验。
//! - `status_at` 严格只读：不建锁文件、不补 marker、不清理任何东西。
//! - 其余边界（如实声明）：目录句柄未逐级锚定，检查后的父目录替换、marker 原地
//!   覆盖窗口、进程中途断电持久性不在本实现保证范围内。
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[allow(dead_code)] // settings 命令消费；#[path] 引入的测试编译单元内未见使用
pub const PLUGIN: &[u8] = include_bytes!("opencode_tui_route.mjs");

/// 本实现专属的暂存文件名（前缀归属兔维斯；锁内可清理）。
const TEMP_NAME: &str = ".tuvis-tui.mjs.tmp";

#[cfg(test)]
thread_local! {
    #[allow(clippy::type_complexity)] // 测试停点回调签名；仅测试编译单元使用
    pub static TEST_HOOK: std::cell::RefCell<Option<Box<dyn FnMut(&str) -> Result<(), String>>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
fn test_hook(point: &str) -> Result<(), String> {
    TEST_HOOK.with(|hook| match hook.borrow_mut().as_mut() {
        Some(hook) => hook(point),
        None => Ok(()),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum InstallStatus {
    Absent,
    Current,
    Outdated,
    Conflict,
}

fn paths(root: &Path) -> (PathBuf, PathBuf) {
    let dir = root.join("opencode/plugins/tuvis-route-observer");
    (dir.join("tui.mjs"), dir.join(".tuvis-observer-sha256"))
}

/// 检查受控根下每一级；真实文件操作仍需目录句柄以防检查后的替换。
fn reject_linked_parents(root: &Path) -> Result<(), String> {
    let mut path = root.to_path_buf();
    for part in ["opencode", "plugins", "tuvis-route-observer"] {
        path.push(part);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => {
                return Err(format!(
                    "observer path is not a directory: {}",
                    path.display()
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(format!("stat observer directory: {error}")),
        }
    }
    Ok(())
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn read_regular(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => std::fs::read(path)
            .map(Some)
            .map_err(|e| format!("read {}: {e}", path.display())),
        Ok(_) => Err(format!("not a regular file: {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("stat {}: {e}", path.display())),
    }
}

/// 跨进程排他锁：flock 在独立打开的锁文件上；文件不随卸载删除。
/// 释放随 fd 关闭自动完成（Drop → File 关闭 → flock 解除）。
struct ObserverLock(#[allow(dead_code)] std::fs::File);

pub fn status_at(root: &Path, expected: &[u8]) -> Result<InstallStatus, String> {
    reject_linked_parents(root)?;
    let (plugin, marker) = paths(root);
    if let Some(dir) = plugin.parent() {
        match std::fs::symlink_metadata(dir) {
            Ok(metadata) if !metadata.file_type().is_dir() => {
                return Err(format!(
                    "observer path is not a directory: {}",
                    dir.display()
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("stat observer directory: {e}")),
        }
    }
    let file = read_regular(&plugin)?;
    let owner = read_regular(&marker)?;
    Ok(match (file, owner) {
        (None, None) => InstallStatus::Absent,
        (Some(file), Some(owner)) if owner == digest(&file).as_bytes() => {
            if file == expected {
                InstallStatus::Current
            } else {
                InstallStatus::Outdated
            }
        }
        _ => InstallStatus::Conflict,
    })
}

pub fn install_at(root: &Path, bytes: &[u8]) -> Result<InstallStatus, String> {
    // 只取路径校验（父级符号链接等）；Conflict 的裁决交给锁内重查——
    // 「plugin 内容恰为本轮待写内容」的未完成提交在这里必须允许续写。
    status_at(root, bytes)?;
    #[cfg(test)]
    test_hook("install_checked")?;
    let _lock = acquire_observer_lock(root)?;
    let (plugin, marker) = paths(root);
    let dir = plugin.parent().ok_or("observer directory unavailable")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("create observer directory: {e}"))?;
    let new_digest = digest(bytes);
    let current = read_regular(&plugin)?;
    let owner = read_regular(&marker)?;
    match (&current, &owner) {
        // 已有自有完整安装（marker 摘要匹配现文件）：幂等确认或受控升级。
        (Some(file), Some(owner_digest)) if owner_digest.as_slice() == digest(file).as_bytes() => {
            if file.as_slice() == bytes {
                return status_at(root, bytes);
            }
            publish_plugin_replace(dir, &plugin, bytes)?;
            #[cfg(test)]
            test_hook("install_plugin_written")?;
            publish_marker(dir, &marker, &new_digest)?;
        }
        // 未完成的自有提交：内容恰为本轮待写字节（首次安装中断 / 升级中断），
        // 续写 marker 完成提交。同内容外来文件与中断无法区分，按自有处理（裁定）。
        (Some(file), _) if file.as_slice() == bytes => {
            // 中断续写：检查时 marker 在（失配旧摘要）→ 替换；缺席 → create-only。
            publish_marker_with_mode(dir, &marker, &new_digest, owner.is_some())?;
        }
        // 未认领的外来内容：绝不覆盖，明确冲突。
        (Some(_), _) => return Err("observer plugin conflicts with an existing file".into()),
        // 插件缺席：create-only 发布（目标已在即失败），再补 marker。
        (None, _) => {
            publish_plugin_no_replace(dir, &plugin, bytes)?;
            #[cfg(test)]
            test_hook("install_plugin_written")?;
            // 首次安装：检查时 marker 缺席，create-only 发布；竞争创建报冲突。
            publish_marker_with_mode(dir, &marker, &new_digest, false)?;
        }
    }
    status_at(root, bytes)
}

pub fn uninstall_at(root: &Path) -> Result<InstallStatus, String> {
    // 锁前只做路径安全校验；状态裁决全部在锁内按实际内容进行（并发重复卸载幂等）。
    reject_linked_parents(root)?;
    let (plugin, marker) = paths(root);
    if let Some(dir) = plugin.parent() {
        match std::fs::symlink_metadata(dir) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => {
                return Err(format!(
                    "observer path is not a directory: {}",
                    dir.display()
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("stat observer directory: {e}")),
        }
    }
    #[cfg(test)]
    test_hook("uninstall_checked")?;
    let _lock = acquire_observer_lock(root)?;
    let current = read_regular(&plugin)?;
    let owner = read_regular(&marker)?;
    match (current, owner) {
        // 已缺席：幂等成功（含上一个卸载者已完成全部清理）。
        (None, None) => Ok(InstallStatus::Absent),
        // 插件已删、marker 残留：上一次卸载中断，收尾。
        (None, Some(_)) => {
            std::fs::remove_file(&marker).map_err(|e| format!("remove observer marker: {e}"))?;
            remove_observer_dir(&plugin);
            Ok(InstallStatus::Absent)
        }
        // 归属匹配：删除插件 → marker。
        (Some(current_bytes), Some(owner))
            if owner.as_slice() == digest(&current_bytes).as_bytes() =>
        {
            std::fs::remove_file(&plugin).map_err(|e| format!("remove observer: {e}"))?;
            #[cfg(test)]
            test_hook("uninstall_plugin_removed")?;
            std::fs::remove_file(&marker).map_err(|e| format!("remove observer marker: {e}"))?;
            remove_observer_dir(&plugin);
            Ok(InstallStatus::Absent)
        }
        // 内容不匹配（外部替换）或归属记录缺失：保留现场，明确冲突。
        (Some(_), Some(_)) => Err("observer plugin was modified externally".into()),
        (Some(_), None) => Err("observer ownership marker is missing".into()),
    }
}

fn remove_observer_dir(plugin: &Path) {
    if let Some(dir) = plugin.parent() {
        let _ = std::fs::remove_dir(dir);
    }
}

/// 跨进程 flock（阻塞获取）。锁文件位于 `opencode/` 下、卸载不删——
/// 删除再创建会出现两组互不约束的锁。
fn acquire_observer_lock(root: &Path) -> Result<ObserverLock, String> {
    let lock_path = root.join("opencode/.tuvis-route-observer.lock");
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create observer lock directory: {e}"))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .open(&lock_path)
        .map_err(|e| format!("open observer lock: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(format!(
                "lock observer: errno {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    #[cfg(not(unix))]
    {
        // 非 Unix 平台暂无跨进程锁实现：显式拒绝，不静默退化为无锁写。
        return Err("observer install lock is unsupported on this platform".into());
    }
    Ok(ObserverLock(file))
}

/// 首次发布：create-only 硬链接（目标已在即失败，绝不覆盖外来文件）。
fn publish_plugin_no_replace(dir: &Path, plugin: &Path, bytes: &[u8]) -> Result<(), String> {
    let temp = write_plugin_temp(dir, bytes)?;
    if let Err(error) = std::fs::hard_link(&temp, plugin) {
        let _ = std::fs::remove_file(&temp);
        return Err(format!("publish observer plugin: {error}"));
    }
    let _ = std::fs::remove_file(&temp);
    Ok(())
}

/// 升级发布：目标已核实属于我们（marker 摘要匹配），同目录完整文件原子替换，
/// 读方只看到完整旧版或完整新版，绝无截断中间态。
fn publish_plugin_replace(dir: &Path, plugin: &Path, bytes: &[u8]) -> Result<(), String> {
    let temp = write_plugin_temp(dir, bytes)?;
    if let Err(error) = std::fs::rename(&temp, plugin) {
        let _ = std::fs::remove_file(&temp);
        return Err(format!("replace observer plugin: {error}"));
    }
    Ok(())
}

/// 标记独立发布（**绝不原地写共享 inode**）：完整临时文件后发布。
/// - `replace_existing=true`（自有标记在检查时已在）：原子 rename 替换——既有
///   硬链接伙伴保持旧内容不受影响（rename 只换本目录项）；
/// - `replace_existing=false`（首次安装，检查时缺席）：create-only 硬链接发布；
///   EEXIST = 观察到竞争创建，报冲突，绝不替换外来文件。
fn publish_marker(dir: &Path, marker: &Path, marker_digest: &str) -> Result<(), String> {
    publish_marker_with_mode(dir, marker, marker_digest, true)
}

fn publish_marker_with_mode(
    dir: &Path,
    marker: &Path,
    marker_digest: &str,
    replace_existing: bool,
) -> Result<(), String> {
    let temp = dir.join(".tuvis-observer-sha256.tmp");
    let _ = std::fs::remove_file(&temp);
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|e| format!("create observer marker temp: {e}"))?;
        file.write_all(marker_digest.as_bytes())
            .map_err(|e| format!("write observer marker temp: {e}"))?;
        file.sync_all()
            .map_err(|e| format!("sync observer marker temp: {e}"))?;
    }
    let exists = std::fs::symlink_metadata(marker)
        .map(|meta| meta.is_file())
        .unwrap_or(false);
    if exists {
        if !replace_existing {
            let _ = std::fs::remove_file(&temp);
            return Err("observer marker appeared during install (competing creation)".into());
        }
        // 替换：目标此刻必须是普通文件（symlink 等异常在读侧已拒，这里兜底）。
        let meta = std::fs::symlink_metadata(marker).map_err(|error| {
            let _ = std::fs::remove_file(&temp);
            format!("stat observer marker: {error}")
        })?;
        if !meta.is_file() {
            let _ = std::fs::remove_file(&temp);
            return Err("observer marker is not a regular file".into());
        }
        std::fs::rename(&temp, marker).map_err(|error| {
            let _ = std::fs::remove_file(&temp);
            format!("replace observer marker: {error}")
        })
    } else {
        match std::fs::hard_link(&temp, marker) {
            Ok(()) => {
                let _ = std::fs::remove_file(&temp);
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = std::fs::remove_file(&temp);
                Err("observer marker appeared during install (competing creation)".into())
            }
            Err(error) => {
                let _ = std::fs::remove_file(&temp);
                Err(format!("publish observer marker: {error}"))
            }
        }
    }
}

/// 写完整暂存文件（create_new 独占；同名暂存是本实现遗留，锁内先清）。
fn write_plugin_temp(dir: &Path, bytes: &[u8]) -> Result<PathBuf, String> {
    use std::io::Write;
    let temp = dir.join(TEMP_NAME);
    let _ = std::fs::remove_file(&temp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|e| format!("create observer temp: {e}"))?;
    file.write_all(bytes)
        .map_err(|e| format!("write observer temp: {e}"))?;
    file.sync_all()
        .map_err(|e| format!("sync observer temp: {e}"))?;
    Ok(temp)
}
