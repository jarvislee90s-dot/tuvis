# 真机发现：scan_bus 永久挂起（2026-11-16 01:17-01:30）

## 现象
- 验收包（da4788a1，PID 14212/18139）在真实环境启动后，看板会话扫描（adapter::scan_bus）挂起且永不完成：
  `scan_bus: 扫描疑似 hang（已 122800ms 未完成）——不强杀在飞扫描、不清在飞位，需重启恢复`，重启后 2 分钟内复现。
- /m 看板 /m/api/v1/sessions 带合法设备 cookie 后请求挂起（无 cookie 时 403 正常 2.6s）。

## 根因链（sample 实锤）
scan_once_authorized_until → get_all_sessions_inner → ZCodeAdapter::find_sessions
→ zcode_parser::build_one_session → monitor::git::get_github_url → std::process::Command::output
→ poll（永等 stdout EOF）。

git 子进程（`git remote get-url origin`，cwd=/Users/jarvis/Documents/tuvis-worktree3）卡在
`init_git → strbuf_getcwd → __getcwd` 内核调用——该目录文件系统查询挂死（陈旧挂载/断链克隆）。
`Command::output` 无超时 ⇒ 任一项目目录文件系统不健康即永久楔死全量会话扫描（殃及所有工具的看板）。

## 影响
看板（手机面 + 桌面）会话列表永不渲染；扫描总线拒绝新扫描；远程看板不可用。

## 修复方向
`monitor/git.rs::get_github_url` 的 git 子进程必须带死线（超时杀进程、返回 None 降级——
GitHub URL 是卡片装饰，不是事实来源）；全 monitor 面排查同类裸 `Command::output`。
