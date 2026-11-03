# Codex 兼容与能力裁决

## 2026-10-20 · Windows permissions 保持关闭

- 背景：Windows `26.1013.4894.0` 的权限读升权实际走 command approval，没有观察到独立的 `item/permissions/requestApproval` 证据。
- 决定：`permissions` 能力保持 `false`；只归档审批证据，不按“理论上支持”解锁。
- 后果：手机不展示权限审批入口；命令/文件审批仍按各自契约开放。
- 状态：active。

## 2026-10-22 · 未知 build 的自动适配缺省开启

- 背景：精确 build 白名单会让 Desktop 自动升级后整体不可用。
- 决定：自动适配设置缺省开启；未知 build 按目标/状态/协议守卫尝试发送；打断、审批等破坏性操作继续走证据门控。
- 后果：Desktop 会话永不自动转 headless；发送未送达即终态回执，不伪造成功。
- 状态：active。

## 2026-10-28 · Plan 审批保持不支持，远程不伪造退出

- 背景：`item/plan/requestImplementation` 未探测/未开放；planContent 不下发。
- 决定：Plan 审批投影为 `Unsupported`、不可操作；远程不提供“批准/退出计划模式”按钮，也不把普通正文当计划决策。
- 后果：用户需在 Desktop 处理；手机可刷新等待 pending 消失。无活动回合时打断也不能解决。
- 状态：active。

## 2026-10-28 · 原生自动成功归因暂不启用

- 背景：仅凭全局 revision 前进 + 全历史 `requestId+completed` 无法证明是本动作；requestId 复用时可能误认。
- 决定：关闭该自动 `ObservedSuccess` 提升；契约匹配与操作可用性保留。
- 后果：需要原 turn/item 与本动作答案强关联后再重新启用；在此之前不把原生完成记录当成功证据。
- 状态：active；强关联仍是后续能力债。
