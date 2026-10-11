// 桌宠窗口管理 — 建窗参数与显隐/置顶（spec §4.1/§4.5）
//
// Windows 实测问题（2026-09-02）：setup 期紧跟主窗口创建桌宠的透明 webview 时，
// WebView2 控制器初始化存在竞态，偶发 E_INVALIDARG 失败；且 tauri-runtime-wry 会
// 吞错——build() 仍返回 Ok，OS 层窗口不存在但注册表留下"幽灵"，此后 show/hide
// 全部对幽灵返回成功，桌宠永远出不来且无任何报错。本模块三层防御：
//   1. window_alive：OS 层校验（hwnd 失效即未落地），不信任 build() 的返回值
//   2. create_pet_window：幽灵销毁重建 + 重试（5 次 × 300ms）
//   3. set_pet_visible(true) 兜底：窗口缺失/幽灵时现场重建，保证开关必然拉出桌宠
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

const PET_W: f64 = 340.0;
const PET_H: f64 = 260.0;

/// 建窗/重建串行化：开关连点与启动延迟建窗并发时避免同标签竞争
static PET_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(windows)]
mod win {
    #[link(name = "user32")]
    extern "system" {
        pub fn IsWindow(hwnd: isize) -> i32;
    }
}

/// 窗口真实落地校验：hwnd 拿不到（注册表幽灵无句柄）或句柄已失效（IsWindow=0，
/// 窗口对象在但 OS 层从未建成）都判为未落地。非 Windows 无此竞态，恒真。
#[cfg(windows)]
fn window_alive(w: &tauri::WebviewWindow) -> bool {
    match w.hwnd() {
        Ok(h) => unsafe { win::IsWindow(h.0 as isize) != 0 },
        Err(_) => false,
    }
}
#[cfg(not(windows))]
fn window_alive(_w: &tauri::WebviewWindow) -> bool {
    true
}

/// 诊断取值：窗口自报的可见性文本（`is_visible` 失败时返回 err 文本）。
/// 纯观测——任何失败都不参与建窗/显隐结果，只为回答「窗口自己说它可见吗」
fn visible_text(w: &tauri::WebviewWindow) -> String {
    match w.is_visible() {
        Ok(v) => v.to_string(),
        Err(e) => format!("err: {}", e),
    }
}

/// 清除可能存在的幽灵/旧窗口（幂等）：destroy 后注册表条目经事件循环异步清除，
/// 轮询等待消失（最多 300ms），避免同标签重建冲突
fn clear_pet_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("pet") {
        let _ = w.destroy();
        for _ in 0..10 {
            if app.get_webview_window("pet").is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
    }
}

/// 页面显隐权威已行使标记（评审 B1，2026-10-11）：`set_pet_visible` 的调用来源
///（宠物页面 / 标题栏 / 设置页 / 启动守卫）都算「权威到过场」；注意**托盘臂
/// 不是**——它是窗口级直 show/hide、不经本命令，不在登记面（复评 I-1 如实
/// 修正：托盘在启动 ~5.8s 窗内的显隐不被兜底尊重，极窄时窗，留 fast-follow 收）。
/// 启动兜底触发前查它——兜底存在的意义是「页面死了救活」；页面活着且明确发声
///（含**把窗口藏起来**的那一次：跟随关的启动路径）就不许 5s 后翻脸把它弹回去。
/// 复位纪律：**只在新建成功**分支复位（新页面还没说过话）；复用已有窗口不复位
///（旧页面的隐藏决定继续权威）。
static PET_PAGE_SPOKE: AtomicBool = AtomicBool::new(false);

/// **启动兜底**（2026-10-07 用户裁决 E2）：宠物窗口是**隐藏态**建的，而把它显示出来的**唯一**
/// 路径是宠物页面自己的 JS（`pet.tsx` 那条 `invoke("set_pet_visible", …)`）。
///
/// 实机证据（本机日志，同一晚三次启动）：窗口建成功且 `is_visible=false`，此后到用户手动开之前
/// **一次 `set_pet_visible` 都没有**；而手动一开就 `false → true` 立刻成功。这指向一个死锁——
/// **隐藏窗口的 WebView 不执行页面 ⇒ 页面没机会显示自己**。
///
/// 兜底策略：建窗后等 `PET_SHOW_FALLBACK_MS`，若窗口**仍**隐藏，就由 Rust 侧直接 `show()`。
/// 窗口一旦可见，页面就会跑起来，随后它自己的 `set_pet_visible(loadVisible())` 会**权威纠正**
/// 最终状态（关掉宠物的用户会被立刻 `hide()` 回去）。
///
/// **为什么不干脆「建窗即可见」**：那会让**每个**关掉宠物的用户在每次启动时都看到一闪；
/// 本兜底只在「页面没干活」的故障路径上生效，正常路径**零行为变化**。
///
/// 这个 warn 日志本身就是判据：**看到它就证明页面没有自显示**（配合紧随其后的
/// `set_pet_visible` 行，就能确认「页面在窗口可见后才真正跑起来」）。
fn schedule_show_fallback(app: &AppHandle) {
    /// 给页面留的时间：足够它 mount + 发 IPC（实机在秒级内），又不至于让人觉得启动卡了
    const PET_SHOW_FALLBACK_MS: u64 = 5000;
    let handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(PET_SHOW_FALLBACK_MS));
        let Some(w) = handle.get_webview_window("pet") else {
            return;
        };
        // 正常路径：页面已经自己显示过了（那就什么都不做）。窗口没了也别碰。
        if !window_alive(&w) || w.is_visible().unwrap_or(false) {
            return;
        }
        // 评审 B1：页面行使过显隐权威（哪怕是把窗口**藏起来**的那次——启动跟随
        // 关掉的路径）＝页面活着且已有决定，兜底不得二次解释
        if PET_PAGE_SPOKE.load(Ordering::SeqCst) {
            log::info!("pet window 页面已行使显隐权威 → 启动兜底不插手");
            return;
        }
        log::warn!(
            "pet window 建窗 {}ms 后仍隐藏（页面未自显示）→ 启动兜底 show()",
            PET_SHOW_FALLBACK_MS
        );
        if let Err(e) = w.show() {
            log::error!("pet window 启动兜底 show() 失败：{}", e);
        }
    });
}

/// 创建桌宠窗口（**可见态**——2026-10-07 实证：隐藏态建出来的窗口页面不会执行，见 `build_pet_window`
/// 里 `.visible(true)` 的长注释；前端加载后仍按 localStorage 决定最终显隐，关掉宠物的人会被收回去）。
/// 带落地校验与重试：已存在且真实落地则直接复用；幽灵或创建失败则销毁重建
pub fn create_pet_window(app: &AppHandle) -> Result<(), String> {
    let _guard = PET_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = app.get_webview_window("pet") {
        if window_alive(&existing) {
            // 诊断（分支留痕）：复用分支此前完全静默——回答「这次建窗是复用已有窗口还是新建」
            // 与「复用那一刻窗口自报的可见性」（is_visible=false ⇒ 建窗成功但从未 show，与 H2 互通）
            log::info!(
                "pet window 复用已落地窗口（branch=reused existing, attempt=0, is_visible={}）",
                visible_text(&existing)
            );
            // 复用分支同样要挂兜底：窗口活着但隐藏 + 页面没跑 = 同一个死锁
            schedule_show_fallback(app);
            return Ok(());
        }
        log::warn!("pet window 未落地（启动竞态幽灵），销毁重建");
        clear_pet_window(app);
    }
    let mut last_err = String::new();
    for attempt in 1..=5u8 {
        match build_pet_window(app) {
            Ok(w) => {
                if window_alive(&w) {
                    // 诊断（分支留痕）：成功路径在 `attempt == 1` 时**完全静默** ⇒ 日志里
                    // 「建窗成功」与「压根没走到这一步」无法区分。本次事故正是这样：4 小时 15 分
                    // 的日志里宠物行数为 0，既不能排除建窗失败、也不能确认建窗成功。
                    // 记 `is_visible` 一并回答「建窗那一刻窗口自报可见吗」。
                    log::info!(
                        "pet window 新建成功（branch=built new, attempt={}, is_visible={}）",
                        attempt,
                        visible_text(&w)
                    );
                    if attempt > 1 {
                        log::info!("pet window 第 {} 次尝试创建成功", attempt);
                    }
                    // 评审 B1：新窗口＝新页面还没说过话，权威标记复位（复用分支不复位）
                    PET_PAGE_SPOKE.store(false, Ordering::SeqCst);
                    schedule_show_fallback(app);
                    return Ok(());
                }
                last_err = "窗口未落地（幽灵）".to_string();
            }
            Err(e) => last_err = e,
        }
        clear_pet_window(app);
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    Err(format!("桌宠窗口创建失败（已重试 5 次）: {}", last_err))
}

fn build_pet_window(app: &AppHandle) -> Result<tauri::WebviewWindow, String> {
    // 默认主显示器右下角；前端随后按记忆位置与实测尺寸重排
    let mut mx = 1440.0;
    let mut my = 900.0;
    if let Ok(Some(m)) = app.primary_monitor() {
        mx = m.size().width as f64 / m.scale_factor();
        my = m.size().height as f64 / m.scale_factor();
    }
    let x = mx - PET_W - 24.0;
    let y = my - PET_H - 76.0;
    let w = WebviewWindowBuilder::new(app, "pet", WebviewUrl::App("index.html#/pet".into()))
        .title("mam-pet")
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        // **可见态建窗**（2026-10-07 实证修复，原为 `.visible(false)`）：
        //
        // 原设计是「隐藏建窗 → 前端按 localStorage 决定显隐」，动机是「避免启动闪现」。
        // 但实测（本机日志，同一晚连续 4 次启动，每次都是同一形状）：隐藏态建出来的宠物窗口
        // **页面根本不会执行** —— 建窗成功、`is_visible=false`，此后**一次 `set_pet_visible`
        // 都没有**（那是页面唯一会发出的调用）；而 兔维斯 其余窗口都是**可见态**建窗，页面全都正常。
        // 更关键：**事后 `show()` 也救不回来** —— E2 的启动兜底 `show()` 确实执行了
        // （日志有「建窗 5000ms 后仍隐藏 → 启动兜底 show()」），页面依旧没跑。
        // ⇒ 页面必须在**建窗那一刻**就允许加载；隐藏建成之后没有第二次机会。
        // 旁证：URL 本身没问题（dev server 对 `/index.html`、`/index.html%23/pet`、`/usage`
        // 一律 200，SPA 回退），故不是路由或地址的问题。
        //
        // **为什么不再担心「启动闪现」**：宠物窗口是 `transparent(true)` 且这一刻**还没有任何
        // 内容**（React 未挂载）⇒ 一个空的全透明窗口**视觉上就是「什么都没有」**，用户看不到它。
        // 用户在设置里关掉了宠物时，页面挂载后第一件事就是 `set_pet_visible(false)` 把它收回去
        // （`pet.tsx` 的 effect），故关掉的用户也不会长期留一个空窗。
        .visible(true)
        .inner_size(PET_W, PET_H)
        .position(x, y)
        // **页面加载判据（2026-10-07）**：宠物窗口的问题一路查到「页面从不执行」，而「没加载」与
        // 「加载了但 JS 死了」在 Rust 侧长得一模一样（都只表现为「没有 `set_pet_visible`」）。
        // 这两步把它们分开：
        //   * 出现 `pet page load: …` ⇒ 文档**确实加载了** ⇒ 问题在页面 JS（看后面的错误探针）；
        //   * 从不出现 ⇒ 文档压根没加载 ⇒ 问题在窗口 / URL / WebView 层。
        // 顺带装个**错误探针**：把第一个未捕获错误写进 `document.title`，3 秒后由 Rust 读回来
        // —— 标题是 Rust 唯一能直接读到的页面状态，这样不必为探针新增 IPC 命令。
        .on_page_load(|w, payload| {
            log::info!(
                "pet page load: event={:?} url={}",
                payload.event(),
                payload.url()
            );
            if let Err(e) = w.eval(
                "window.addEventListener('error', function (e) { \
                   document.title = 'PETERR:' + (e.message || 'unknown'); \
                 });",
            ) {
                log::warn!("pet 错误探针注入失败：{}", e);
            }
            let w2 = w.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(3000));
                match w2.title() {
                    // 标题没被改写 ⇒ 页面没抛未捕获错误（`PETERR:` 前缀才是异常）
                    Ok(t) => log::info!("pet title after 3s = {t}"),
                    Err(e) => log::warn!("pet 读标题失败：{}", e),
                }
            });
        })
        .build()
        .map_err(|e| format!("创建桌宠窗口失败: {}", e))?;
    // dev 模式下 WebView2 可能给 webview 自动附加 DevTools 独立窗口，显式关闭
    #[cfg(debug_assertions)]
    w.close_devtools();
    Ok(w)
}

#[tauri::command]
pub async fn set_pet_visible(app: AppHandle, visible: bool) -> Result<(), String> {
    // 评审 B1：显隐权威行使登记——无论来源与方向，页面体系的显隐声音已经出现过，
    // 5s 启动兜底就此闭嘴（详见 PET_PAGE_SPOKE 注释）
    PET_PAGE_SPOKE.store(true, Ordering::SeqCst);
    if visible {
        // 开启兜底：窗口缺失或幽灵（启动竞态被吞错）→ 现场重建再显示，
        // 保证无论启动抽签结果如何，开关一定能把桌宠拉出来
        let alive = app
            .get_webview_window("pet")
            .map(|w| window_alive(&w))
            .unwrap_or(false);
        if !alive {
            create_pet_window(&app)?;
        }
    }
    if let Some(w) = app.get_webview_window("pet") {
        // 诊断（显隐前后各一行）：`is_visible` 的**前后对账**是区分本次事故三种假设的关键——
        // 若 after = true 而用户看不见 ⇒ 窗口在、页面没画出来（H1：渲染/量测自激）；若日志里
        // 压根没有这一对 ⇒ 前端那条**唯一**的显示路径没走到（H2：`pet.tsx` 的 `.catch` 吞了失败）。
        let before = visible_text(&w);
        let r = if visible { w.show() } else { w.hide() };
        if let Err(e) = r {
            log::warn!(
                "set_pet_visible({}) 失败：{}（is_visible before={}）",
                visible,
                e,
                before
            );
            return Err(e.to_string());
        }
        log::info!(
            "set_pet_visible({})：is_visible {} → {}",
            visible,
            before,
            visible_text(&w)
        );
    } else {
        // 窗口不在注册表里也要留痕：否则「调用发生了但没有窗口」与「调用压根没发生」无法区分
        log::warn!("set_pet_visible({}) 但 pet 窗口不在注册表里", visible);
    }
    Ok(())
}

#[tauri::command]
pub async fn set_pet_always_on_top(app: AppHandle, on_top: bool) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("pet") {
        w.set_always_on_top(on_top).map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ===== 外部宠物 IPC（spec 2026-09-03-external-pet-import §5.1）=====
// id 类参数一律先过 validate_pet_id：pet_dir 是裸 join，不设卡可经 ../ 或绝对路径
// 逃逸出仓库（如 pet_delete_pet("..") 会把 ~/.tuvis 整目录送回收站，P0-1）
use crate::services::pet::{self, error::PetRpcError, import, manifest, petdex, scan};

fn root() -> std::path::PathBuf {
    pet::pets_root()
}

#[tauri::command]
pub async fn pet_list_pets() -> Result<Vec<scan::PetSummary>, PetRpcError> {
    Ok(scan::list_pets_in(&root()))
}

#[tauri::command]
pub async fn pet_list_codex_pets() -> Result<Vec<scan::CodexPetInfo>, PetRpcError> {
    let codex = dirs::home_dir()
        .unwrap_or_default()
        .join(".codex")
        .join("pets");
    Ok(scan::list_codex_pets_in(&codex, &root()))
}

#[tauri::command]
pub async fn pet_scan(id: String) -> Result<scan::PetScan, PetRpcError> {
    pet::validate_pet_id(&id)?;
    scan::scan_pet_in(&root(), &id)
}

#[tauri::command]
pub async fn pet_read_manifest(id: String) -> Result<Option<manifest::PetManifest>, PetRpcError> {
    pet::validate_pet_id(&id)?;
    Ok(manifest::load(&pet::pet_dir(&root(), &id)))
}

#[tauri::command]
pub async fn pet_stage_from_folder(path: String) -> Result<import::StagedPet, PetRpcError> {
    import::stage_from_folder_in(&root(), std::path::Path::new(&path))
}

#[tauri::command]
pub async fn pet_stage_from_zip(path: String) -> Result<import::StagedPet, PetRpcError> {
    import::stage_from_zip_in(&root(), std::path::Path::new(&path))
}

#[tauri::command]
pub async fn pet_stage_from_codex(codex_id: String) -> Result<import::StagedPet, PetRpcError> {
    // N3：codex_id 同样是路径拼接参数（codex_root 裸 join），须与其他 id 命令同一门禁，
    // 否则 "../x" 可把 ~/.codex/pets 外任意目录的图集暂存进导入区（读侧逃逸）
    pet::validate_pet_id(&codex_id)?;
    let codex = dirs::home_dir()
        .unwrap_or_default()
        .join(".codex")
        .join("pets");
    import::stage_from_codex_in(&root(), &codex, &codex_id)
}

#[tauri::command]
pub async fn pet_stage_from_petdex(url: String) -> Result<import::StagedPet, PetRpcError> {
    petdex::stage_from_url(&root(), &url).await
}

#[tauri::command]
pub async fn pet_stage_audio(
    staging_id: String,
    src_paths: Vec<String>,
    group: String,
) -> Result<Vec<import::StagedVoiceFile>, PetRpcError> {
    import::stage_audio_in(&root(), &staging_id, &src_paths, &group)
}

#[tauri::command]
pub async fn pet_remove_staged_audio(staging_id: String, rel: String) -> Result<(), PetRpcError> {
    import::remove_audio_in(&root(), &staging_id, &rel, true)
}

#[tauri::command]
pub async fn pet_finalize_import(
    staging_id: String,
    name: String,
    manifest: manifest::PetManifest,
) -> Result<scan::PetSummary, PetRpcError> {
    import::finalize_in(&root(), &staging_id, &name, manifest)
}

#[tauri::command]
pub async fn pet_cancel_import(staging_id: String) -> Result<(), PetRpcError> {
    import::cancel_in(&root(), &staging_id)
}

#[tauri::command]
pub async fn pet_update_manifest(
    id: String,
    mut manifest: manifest::PetManifest,
    backup: bool,
) -> Result<(), PetRpcError> {
    pet::validate_pet_id(&id)?;
    manifest.id = id.clone();
    manifest::write_with_backup(&pet::pet_dir(&root(), &id), &manifest, backup)
}

#[tauri::command]
pub async fn pet_rename_pet(old_id: String, new_id: String) -> Result<(), PetRpcError> {
    pet::validate_pet_id(&old_id)?; // new_id 由 rename_pet_in 经 validate_pet_name 校验（含查重）
    pet::rename_pet_in(&root(), &old_id, &new_id)
}

#[tauri::command]
pub async fn pet_delete_pet(id: String) -> Result<(), PetRpcError> {
    pet::validate_pet_id(&id)?;
    pet::delete_pet_in(&root(), &id)
}

#[tauri::command]
pub async fn pet_add_voice_files(
    id: String,
    src_paths: Vec<String>,
    group: String,
) -> Result<Vec<import::StagedVoiceFile>, PetRpcError> {
    pet::validate_pet_id(&id)?;
    import::add_voice_files_in(&root(), &id, &src_paths, &group)
}

#[tauri::command]
pub async fn pet_remove_voice_file(id: String, rel: String) -> Result<(), PetRpcError> {
    pet::validate_pet_id(&id)?;
    import::remove_audio_in(&root(), &id, &rel, false)
}

#[tauri::command]
pub async fn pet_reveal_folder(id: String) -> Result<(), PetRpcError> {
    pet::validate_pet_id(&id)?;
    let dir = pet::pet_dir(&root(), &id);
    tauri_plugin_opener::open_path(dir.to_string_lossy().to_string(), None::<&str>).map_err(|e| {
        PetRpcError::new("reveal-failed", format!("打开文件夹失败: {}", e))
            .with("err", e.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P0-1 参数化恶意 id 样本：穿越（两种分隔符）、绝对路径（POSIX/Windows）、
    /// 空串、点、点前缀、保留名及其大小写变体
    const BAD_IDS: [&str; 9] = [
        "",
        ".",
        "..",
        "../skills",
        r"..\skills",
        "/etc/passwd",
        r"C:\Windows",
        ".hidden",
        "foxbell",
    ];

    fn dummy_manifest(id: &str) -> manifest::PetManifest {
        manifest::PetManifest {
            schema_version: 1,
            id: id.to_string(),
            display_name: "D".into(),
            description: String::new(),
            source: "folder".into(),
            sprite_version_number: 1,
            spritesheet_size_bytes: 1,
            has_voice: false,
            has_subtitle: false,
            voices: vec![],
        }
    }

    /// 9 个带 id 的 IPC 命令对恶意 id 全部拒绝（在触碰文件系统/opener 之前）
    #[tokio::test]
    async fn id_taking_commands_reject_unsafe_pet_ids() {
        for id in BAD_IDS {
            assert!(pet_scan(id.to_string()).await.is_err(), "pet_scan({id:?})");
            assert!(
                pet_read_manifest(id.to_string()).await.is_err(),
                "pet_read_manifest({id:?})"
            );
            assert!(
                pet_update_manifest(id.to_string(), dummy_manifest(id), true)
                    .await
                    .is_err(),
                "pet_update_manifest({id:?})"
            );
            assert!(
                pet_rename_pet(id.to_string(), "renamed-x".into())
                    .await
                    .is_err(),
                "pet_rename_pet({id:?})"
            );
            assert!(
                pet_delete_pet(id.to_string()).await.is_err(),
                "pet_delete_pet({id:?})"
            );
            assert!(
                pet_add_voice_files(id.to_string(), vec![], "general".into())
                    .await
                    .is_err(),
                "pet_add_voice_files({id:?})"
            );
            assert!(
                pet_remove_voice_file(id.to_string(), "voice/general/a.mp3".into())
                    .await
                    .is_err(),
                "pet_remove_voice_file({id:?})"
            );
            // N3：codex 来源的 id 同为路径拼接参数，纳入同一门禁
            assert!(
                pet_stage_from_codex(id.to_string()).await.is_err(),
                "pet_stage_from_codex({id:?})"
            );
            assert!(
                pet_reveal_folder(id.to_string()).await.is_err(),
                "pet_reveal_folder({id:?})"
            );
        }
    }

    /// 拒绝走结构化错误码（pet-name-* 白名单内，前端可翻译），而非裸字符串
    #[tokio::test]
    async fn rejection_is_structured_rpc_error() {
        let e = pet_delete_pet("/etc".into()).await.unwrap_err();
        assert_eq!(e.code, "pet-name-illegal");
        let e = pet_delete_pet("foxbell".into()).await.unwrap_err();
        assert_eq!(e.code, "pet-name-reserved");
    }
}
