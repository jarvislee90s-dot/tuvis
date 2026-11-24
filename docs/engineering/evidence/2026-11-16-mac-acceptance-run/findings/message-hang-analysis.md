# 真机发现：DSH 会话详情消息加载挂起（2026-11-16 01:41-02:10）

## 现象
- 会话详情页消息「加载中」永不完成；发送 M1 停在「投递中…」（宿主转写未追加，mtime 不变）。
- 新实例反复复现：每次重启后短暂正常（看板可渲染），数分钟内复发 scan_bus 挂起 + 动作路径快照降级。
- sample 实锤（acc-sample3/4）：session_messages 阻塞任务卡在 resolve_readonly_root_with → generation_logs → readdir
  （getdirentries 内核挂）；同进程 SQLite journal open、canonicalize getattrlist 多路磁盘系统调用同时挂；
  而 shell 对同目录 `ls -f` 全部瞬时通过（含楔死期间）。

## 根因判读（证据链）
1. **首次启动的新二进制 + 锁屏**：验收包（tuvis-build-r9 调试包）从未被授予 macOS TCC「文件与文件夹」/
   全盘访问；扫描与卡片预览要读 Documents 下的项目文件（会话 cwd 引用）。锁屏使 TCC 授权弹窗无法应答，
   syspolicyd（ps 呈 Us 卡住态）挂起请求 → 应用磁盘操作成批阻塞；shell（已授权）同操作瞬时通过。
2. **环境放大器**：断链克隆 tuvis-worktree3 内残留楔死的 Tuvis Candidate 进程（U 态，SIGKILL 后解除一轮）；
   6 个 `caffeinate -ims` 进程 cwd 钉在该目录；Spotlight mdbulkimport 反复导入。
3. 生产 0.6.6（/Applications）从无此问题 = 其二进制早已授权。

## 结论
- **非产品缺陷**：TCC 首次授权 + 锁屏是操作前置条件；解锁并应答/预授权后应解除。
- **产品加固项（登记，不阻塞本轮）**：远端只读/扫描路径对文件系统操作无请求级死线——宿主环境楔死时
  会话/消息端点无限期挂起。建议为 resolve_readonly_root 与扫描面补请求级预算（对照 git 2s 死线同型）。

## 解除路径
1. 解锁屏幕；若弹出「Tuvis Candidate 想访问…」对话框 → 允许；或系统设置→隐私与安全性→文件与文件夹/
   完全磁盘访问权限 给 tuvis（验收包二进制）授权一次。
2. 应用重启后重试（当前 M1 草稿保留在投递队列，投递应随之完成）。
