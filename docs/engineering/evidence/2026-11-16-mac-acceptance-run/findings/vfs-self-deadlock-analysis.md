# 真机发现：应用内并发文件遍历内核互锁（2026-11-16 02:00-03:10）

## 现象（跨 4 个应用实例、两种启动方式复现）
- 应用进程的 readdir/getdirentries 间歇性内核级阻塞数分钟：scan_bus 看板扫描（generation_logs）、
  只读消息走（resolve_readonly_root_with）、wakeup 快照三条线轮流楔死；`sample` 显示同一时刻
  多线程分别挂在 readdir / open（SQLite journal）/ getattrlist（canonicalize）。
- 同机单线程探针（ls -f、git、python readdir）对同一目录全部瞬时通过——**仅应用进程楔死**。
- 杀掉断链目录里的楔死进程（worktree3 的 Tuvis Candidate）曾解除一轮；但新实例数分钟内复发，
  与启动方式（open / shell 直启）无关；直启实例 2 分钟后同样复发。

## 判读
最贴合全部证据的解释：**应用内并发 FS 遍历在 macOS 内核层互锁**——scan_bus 扫描、只读读、
wakeup/动作路径快照三条并发遍历线共享目录树（~/.dsh/sessions 等），readdir 持目录 vnode 锁、
其他线程的 open/getattrlist 需同树节点锁，交错形成自锁；锁随应用进程存在，重启短暂清除后
同一并发形态再次触发。跨进程证据（楔死 Tuvis Candidate 钉住断链目录）是早期叠加因素。

## 产品加固项（登记，待修复轮）
1. 重型文件遍历串行化：scan_bus 扫描 / 只读消息走 / wakeup·动作路径快照共享一把进程内 FS 互斥
   （或每源预算+让位），消除内核层并发互锁面。
2. 请求级死线：只读消息走与动作路径补扫必须有预算（对照 git 2s 死线同型），超时降级
   （消息面显示「读取超时，点重试」；动作面 degraded 继续）——现状是无预算即无限期挂。
3. 环境卫生：断链克隆（tuvis-worktree3）内 6 个 `caffeinate -ims` 残留进程建议清理；
   楔死期间任何触及该树的 VFS 操作都会放大互锁。

## 对本轮手测的影响
- 手机面配对（A01/A09）✓、看板渲染（间歇）✓、历史可读（新鲜窗口内）✓ 已取证。
- 消息发送面受归属门 `DSH_SESSION_OWNERSHIP_UNPROVEN`（动作路径快照超时→归属无法证明）
  阻塞；M1-M3、附件、控制台新页待该加固轮后继续。
- 开关生命周期（桌面 GUI）仍待解锁屏幕。
