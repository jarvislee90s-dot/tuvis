// GitHub URL 查询 — 进程内缓存，避免批量解析会话时风暴式 spawn git
// 各工具解析器共用（claude/codex/kimi），与具体会话协议无关

use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::process::Command;
use std::sync::Mutex;

static GIT_URL_CACHE: Lazy<Mutex<HashMap<String, Option<String>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// git 子进程死线：GitHub URL 是卡片装饰，不是事实来源——超时即放弃并降级 None。
/// 真机实证（2026-11-16 验收运行）：任一项目目录文件系统查询挂死（陈旧挂载/断链
/// 克隆）会让 `git` 卡在 `getcwd` 内核调用永不退出，裸 `Command::output` 永等把
/// 整个扫描总线楔死、看板全瘫（docs/engineering/evidence/2026-11-16-mac-acceptance-run）。
const GIT_DEADLINE_MS: u64 = 2_000;

/// 有界执行 git：死线内未退出即 kill 整组并返回 None（stdin 已断、管道随 kill 关闭，
/// 不会留存持管道的孤儿把 `output` 式读尾拖住）。调用方负责注入 CREATE_NO_WINDOW
///（Windows）与 cwd；stdout/stderr 由本函数接管。
fn bounded_git_output(mut cmd: Command) -> Option<std::process::Output> {
    use std::io::Read;
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().ok()?;
    let deadline = Instant::now() + Duration::from_millis(GIT_DEADLINE_MS);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                // kill 后必须 wait 收尸，防 zombie；管道随即闭合。
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => return None,
        }
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_end(&mut stdout);
    }
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_end(&mut stderr);
    }
    Some(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// 查询项目路径的 GitHub 仓库 URL（仅 github 远端；结果按路径缓存）
pub(crate) fn get_github_url(project_path: &str) -> Option<String> {
    {
        let cache = GIT_URL_CACHE.lock().unwrap();
        if let Some(cached) = cache.get(project_path) {
            return cached.clone();
        }
    }
    let result = (|| {
        let mut cmd = Command::new("git");
        cmd.args(["remote", "get-url", "origin"])
            .current_dir(project_path);
        // Windows 下 GUI 进程 spawn 控制台程序会闪黑窗，必须加 CREATE_NO_WINDOW
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let output = bounded_git_output(cmd)?;
        if !output.status.success() {
            return None;
        }
        let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if let Some(p) = url.strip_prefix("git@github.com:") {
            let p = p.strip_suffix(".git").unwrap_or(p);
            Some(format!("https://github.com/{}", p))
        } else if url.starts_with("https://github.com/") {
            Some(url.strip_suffix(".git").unwrap_or(&url).to_string())
        } else {
            None
        }
    })();
    GIT_URL_CACHE
        .lock()
        .unwrap()
        .insert(project_path.to_string(), result.clone());
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 在临时目录构造一个带 origin remote 的 git 仓库，验证 get_github_url 的完整调用链
    #[test]
    fn test_get_github_url_reads_origin() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_str().unwrap().to_string();
        let mut init = std::process::Command::new("git");
        init.args(["init"]).current_dir(&dir);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            init.creation_flags(CREATE_NO_WINDOW);
        }
        init.output()
            .expect("git init 失败（CI/开发机均应安装 git）");
        let mut remote = std::process::Command::new("git");
        remote
            .args([
                "remote",
                "add",
                "origin",
                "git@github.com:some-org/some-repo.git",
            ])
            .current_dir(&dir);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            remote.creation_flags(CREATE_NO_WINDOW);
        }
        remote.output().expect("git remote add 失败");

        assert_eq!(
            get_github_url(&dir).as_deref(),
            Some("https://github.com/some-org/some-repo")
        );
    }

    #[test]
    fn test_get_github_url_none_for_plain_dir() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(get_github_url(temp.path().to_str().unwrap()), None);
    }

    /// 真机验收回归（2026-11-16）：git 子进程卡死（陈旧挂载目录的 getcwd 挂起）
    /// 必须在死线内被 kill 并降级 None，而不是裸 `output()` 永等把扫描总线楔死。
    #[cfg(unix)]
    #[test]
    fn bounded_git_output_kills_command_past_deadline() {
        use std::time::{Duration, Instant};
        let started = Instant::now();
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 30"]);
        let result = bounded_git_output(cmd);
        let elapsed = started.elapsed();
        assert!(result.is_none(), "超时命令必须降级 None: {result:?}");
        assert!(
            elapsed < Duration::from_secs(5),
            "死线应是 2s 量级（实测 {elapsed:?}），超过 5s 说明 kill 未生效"
        );
    }

    #[cfg(unix)]
    #[test]
    fn bounded_git_output_captures_fast_command_stdout() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "printf hello"]);
        let output = bounded_git_output(cmd).expect("快命令必须成功返回");
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "hello");
    }
}
