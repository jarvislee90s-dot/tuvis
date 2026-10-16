// dsh 跳转（设计 P4）：无 per-session URL（M0 F9 穷举确证）——只到应用级：
// 聚焦已打开的 dsh web 浏览器标签（URL 前缀匹配），没有则 open 新页。
// AppleScript 已在 M0 证据包 focus-tab.applescript 只读验证（Chrome/Safari，不抢焦点）。
// 每个 tell 块前有 System Events exists 守卫（同 window/iterm.rs 先例）：
// 浏览器未运行时跳过探测——向未运行应用发 Apple event 会将其冷启动（review Important）

/// 找 dsh web 监听端口：探测监听中的 node 308x 端口，失败回落默认 3080
/// （lsof 只读查询，不触碰 ~/.dsh、不请求 dsh HTTP API）
fn dsh_port() -> u16 {
    if let Ok(out) = std::process::Command::new("sh")
        .arg("-c")
        .arg("lsof -nP -iTCP -sTCP:LISTEN | grep -i node | grep -E '308[0-9]' | head -1 | awk '{print $9}' | awk -F: '{print $NF}'")
        .output()
    {
        if let Ok(s) = String::from_utf8(out.stdout) {
            if let Ok(p) = s.trim().parse() {
                return p;
            }
        }
    }
    3080
}

/// 探测/聚焦标签（probe=true 只定位不抢焦点，供诊断）
fn applescript_focus(port: u16, activate: bool) -> Result<String, String> {
    let url_prefix = format!("http://127.0.0.1:{port}");
    let script = format!(
        r#"
on run
  set hits to ""
  tell application "System Events" to set chromeRunning to exists (processes where name is "Google Chrome")
  if chromeRunning then
    tell application "Google Chrome"
      repeat with w in windows
        repeat with t in tabs of w
          if URL of t starts with "{url_prefix}" or URL of t starts with "http://localhost:{port}" then
            set hits to "found:Chrome"
            if {activate_flag} then
              set active tab index of w to index of t
              set index of w to 1
              activate
            end if
            return hits
          end if
        end repeat
      end repeat
    end tell
  end if
  tell application "System Events" to set safariRunning to exists (processes where name is "Safari")
  if safariRunning then
    tell application "Safari"
      repeat with w in windows
        repeat with t in tabs of w
          if URL of t starts with "{url_prefix}" or URL of t starts with "http://localhost:{port}" then
            set hits to "found:Safari"
            if {activate_flag} then
              set current tab of w to t
              set index of w to 1
              activate
            end if
            return hits
          end if
        end repeat
      end repeat
    end tell
  end if
  return hits
end run
"#,
        url_prefix = url_prefix,
        activate_flag = if activate { "true" } else { "false" },
    );
    let out = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .map_err(|e| format!("FOCUS_OSASCRIPT_FAIL: osascript 执行失败: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if stdout.starts_with("found:") {
        Ok(stdout)
    } else {
        Err("FOCUS_DSH_TAB_MISSING: 未找到 dsh 标签页".into())
    }
}

/// 卡片点击入口：聚焦标签 → 没有 open（cookie 持久 30 天免重登；绝不带 ?token=）
pub fn focus_dsh_tab() -> Result<serde_json::Value, String> {
    let port = dsh_port();
    match applescript_focus(port, true) {
        Ok(_) => Ok(serde_json::json!({ "type": "focused", "via": "dsh-tab" })),
        Err(_) => {
            let url = format!("http://127.0.0.1:{port}/");
            std::process::Command::new("open")
                .arg(&url)
                .spawn()
                .map_err(|e| format!("FOCUS_DSH_OPEN_FAIL: 打开 dsh 失败: {e}"))?;
            Ok(serde_json::json!({ "type": "focused", "via": "dsh-open" }))
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn port_falls_back_to_default() {
        // 不可依赖真机端口；仅验证默认值路径不 panic
        assert!(super::dsh_port() > 0);
    }
}
