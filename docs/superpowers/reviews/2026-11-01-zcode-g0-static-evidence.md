# G0 静态取证（2026-11-01）

范围：只读安装包与代码；未读取真实凭据、连接服务或执行真实操作。R10 为 GO_STATIC，非 LIVE_PASS；供应商可用性和派发关联为 PARTIAL，不能据此猜字段或 RPC。

## 可复核来源

- `/Applications/ZCode.app/Contents/Info.plist` CFBundleVersion `3.16.2.8769`。
- `/Applications/ZCode.app/Contents/Resources/app.asar` SHA256 `506a2d032707a584229c8e2f02f12c202ff3c18fd138773dd0cfe00d469f2964`。
- `/Applications/ZCode.app/Contents/Resources/glm/zcode.cjs` SHA256 `a055234a6ca3da0172616d3987cf3c617b6a26deb1332d69035300c2a0ae0de1`。
- 下述数字是解码字符串的文件内位置，不是原始 asar 字节偏移；静态证据不等于宿主实测。

## 模式链路

- CLI 1231827：`switchCollaborationMode` payload 的 mode 枚举 build/edit/plan/yolo/guarded；1232757 在需 baseRevision 的命令集合中。14773487 的 handler 读取 mode；当前 mode 相等且 planEnabled 为 false 则拒绝 noop，否则 app.setMode；14775179 注册 handler。
- CLI 14190551：app.setMode 调用 runtime.setExecutionState({mode})；1056657 的 resolveExecutionState：已有合法非 plan mode 按值，plan 映射默认 mode 且 planEnabled=true；合法非 plan 且未显式传 planEnabled 时为 false。显式 planEnabled 优先；不应把 plan 原样相等作为唯一确认条件。
- CLI 14509546 的输入状态组装以 app.getMode 和 runtime.getPlanEnabled 经同一规范化后输出 mode 与 planEnabled，auto 输出 build。asar `out/main/chunk-XYX2CJ3H.js` 637258：config schema 的 mode 默认 build、planEnabled 可选。该输入组装不能单独证明所有订阅帧在所有路径均一定携带 planEnabled；需看实际新鲜 config。
- asar `out/renderer/assets/IntlProvider-C2IbGUkl.js` 312850：build=变更前确认、edit=自动编辑、plan=计划模式、guarded=自主模式、yolo=完全访问。真实切换和权限效果均未验。

## 关联与目录

- CLI 791196 的 activeWork schema 含可选 foregroundExecutionId、sourceCommandId；792680 control.activeWorks 容纳其数组；14609080 运行事件生成 `state.updated` control patch，按源事件存在与否携带以上两项。源字段可选；不能假设任何 accepted inputId 都能在后续 activeWorks 与 sourceCommandId 对位。仓库 `projection.rs` 与 `pending.rs` 也只应依据实际投影与守卫作判断。
- CLI 783666 turnHeader 的 sourceCommandId 可选；784956 用户消息可选 sourceCommandId/rootSourceCommandId；这些是 schema 证据，不等于某次输入到执行必然能关联。
- `model-selection.getView` 现有调用见 `src-tauri/src/services/zcode_control/bridge.rs` 的 MODEL_SELECTION_CHANNEL 与 SelectionPort；解析见 `selection.rs`。本次未证实其跨 provider 目录中表明配置/登录/可用性的确切字段及语义，也未证实每个 provider 的模型档位完整性；禁止由营销目录、字段名或合成例子推断可用。
- 追加静态核对：同一 CLI 解码位置 599443 的 `projectModelSelectionProviderView` 输出 `providerId/providerName/templateId/config/models`；600000 附近 `getView` 过滤 `config.visibility == hidden`。589688 的 provider config 序列化含 access 与 API 配置，可能含敏感字段，Tuvis 不得原样下发或记录；590389 的 model config 序列化含 `enabled` 与 `optionSpecs.reasoningLevel`。596667 的 `isSelectable` 进一步校验 provider 可见、模型存在及选择合法；598089 对账号类 provider 还检查 account state。**可见或模型 enabled 单独都不足以证明账号可用**，仍为 PARTIAL；未读任何实际配置或账号数据。
- `tests/fixtures/zcode-official-bridge/control-extensions.json` 全部 ID、数值和目录占位均合成，不代表真实会话。未放入未证的 provider 可用标志。

## 尚未实测

四模式真实切换、原生 UI 同步、订阅 config 实际序列化、前台执行关联完整性、跨两个已配置供应商切换、移动发送审批效果均 NOT RUN。G0 其他基线冻结、定向测试与账本由主任务负责。
