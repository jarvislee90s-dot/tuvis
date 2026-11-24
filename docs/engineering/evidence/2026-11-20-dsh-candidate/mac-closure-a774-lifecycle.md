# `a774f99b` Mac 隔离开关与历史复验

实机副本二进制 SHA-256 `976b1f622d4f70d10eb29351bc25b1f210564d5fa128eb1c172d847ecead0b87`。从候选源码链接临时验证器调用生产 `dsh_switch_disable`、`dsh_switch_enable` 与开关视图。HTTP 请求使用冻结应用，始终监听回环 19436。使用同一临时设备配对和同一个隔离 Desktop 测试会话，核对浏览器所发 M1–M3。

| 阶段 | 读取与开关观察 | 写入口 |
| --- | --- | --- |
| `Ready` | 正常隔离配对 200。看板含目标和实例标识，正文返回 200，三条标记在场。 | `send-info` 返回 200 且 `injectable: true`。 |
| `WaitingUninstall` | 调用关闭后等待宿主退出。目标仍在看板，正文返回 200。 | 发送返回账本 `failed`，探针正文在宿主转写中出现 0 次。上传返回 403 `DSH_SWITCH_OFF`，控制台入口返回 503 `DSH_SWITCH_OFF`。此时 `send-info` 仍显示 `injectable: true`。 |
| 完全 `Off` | 仅退出隔离宿主，卸载作业自动完成，实例插件目录清空。目标从看板消失，按实例读正文仍为 200，转写保持完整。 | `send-info` 404，控制台入口 404。新发送返回原 ID 的 `unknown_delivery`，原因 `dsh_admission_unconfirmed`，该接口未取得投递证据。 |
| 重启 Tuvis 后 | 无需重新配对。同一设备正文返回 200，七条消息与 Ready 期逐字节一致。候选重启后确认数据库文件仍在隔离根。 | — |

完全 `Off` 时 `/sessions-archived` 返回 200，原会话条目有 `instanceId`。以该键和 `history=1` 读取返回 200，同一 v4 转写中的标记仍在。完全 Off 的上传响应为 403 `DSH_SESSION_OWNERSHIP_UNPROVEN`，探针未落盘。

再开启停在 `Authorizing`，代次 10，开启意图仍在。上游的安装与加载检查已前进，但隔离配置中的插件身份目录未落盘，授权握手未收敛。应用和验证器并发驱动曾收到 `DSH_IPC_STATE`。改为单驱动并重试仍未回到 `Ready`，本轮停止继续启停。

浏览器和命令探针两台临时设备均撤销并清除，设备表为 0；浏览器回到配对页。临时配对材料已清理，隔离候选和隔离 Desktop 保持运行。