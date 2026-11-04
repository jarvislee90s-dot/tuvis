# ZCode 供应商 → 模型二级选择设计

日期：2026-10-31  
状态：**NO-GO（等待真实协议与授权证据；本文件不构成实施授权）**  
范围：可选 O1；独立于本轮队列可靠性主线。  
依据：`docs/superpowers/plans/2026-10-31-zcode-next-round-engineering-plan-v2.md` §7、`docs/superpowers/reviews/2026-10-31-zcode-next-round-review-and-change-proposals.md` §5、`docs/superpowers/reviews/2026-10-31-zcode-queue-debug-evidence.md`、`docs/superpowers/specs/2026-10-24-zcode-unified-remote-design.md`、`docs/superpowers/specs/2026-10-28-zcode-capability-parity-spec.md`。

## 1. 结论

当前手机端只允许切换**本会话当前供应商**下的模型与思考档位。代码刻意不把其他供应商的目录传到手机，也不接受手机指定一个不同供应商。这是服务端安全边界，不是前端少画了一个下拉框。

因此当前版本维持现有单供应商选择，不显示跨供应商入口。只有官方当前 build 的目录、切换命令、授权边界、CAS 及会话上下文影响都通过下文证据门后，才进入正式实现设计与施工计划。

## 2. 目标与非目标

### 目标

- 若官方确实允许当前会话切换到另一个**已授权**供应商，在手机端以「供应商 → 模型」两级选择表达该能力。
- 选择供应商、模型和模型专属思考档位后，只派发一次完整的选择动作；不提交 provider-only 中间状态。
- 目录投影、动作校验、幂等、确认和缓存都以服务端新鲜官方视图为准，保持会话、代际和 revision 边界。
- 若证据不成立，维持现有单供应商行为且不显示看似可用的跨供应商控件。

### 非目标

- 不新增 ZCode 账号、OAuth 流程或账号管理能力。
- 不把 `model-selection.getView()` 返回某个 provider 直接解释为“当前用户有权切换到该 provider”。
- 不接受手机传入任意 provider 字符串，不转发客户端自报目录，不通过放宽白名单绕过服务端检查。
- 不在一次选择里分开发出供应商切换、模型切换、档位切换多个写命令。
- 不改变官方页面或桌面端既有选择行为，不把 provider 切换做成自动恢复原模型的事务。
- 不阻塞队列操作、发送 ACK、视图 revision 等本轮必做修复。

## 3. 当前代码与根因

### 3.1 现有数据通路

- `src-tauri/src/services/zcode_control/selection.rs::parse_selection_view` 解析官方 `providers[]`，但只选取 `config.provider` 对应的目录；其他供应商目录不会进入手机投影。
- `SelectionView` 目前只有一个 `provider`、当前 `model`、`thought` 和该 provider 的 `models`。
- `build_selection_command(view, model, thought)` 从可信视图取 provider，校验模型及其专属 `thought_levels`，命令以 `baseRevision` 做 CAS；前端不能指定目标 provider。
- `ControlAction::Selection` 当前只有 `model` 和 `thought`，幂等动作参数及确认投影都按当前供应商的选择语义设计。
- `src/mobile/ZcodeControlPanel.tsx` 从 `session.selection.models` 渲染单级模型选择；模型变更后沿现有 `Selection` 通道提交，档位仍根据目标模型目录验证。

### 3.2 已知根因与证据等级

上游静态协议资料记载，官方 `model-selection.getView()` 的目录形状含 `providers[].providerId / models[]`。但“目录里出现多个 provider”不能证明它们都属于当前用户可切换范围，也不能证明 `switchModelConfig` 接受跨 provider 目标。

当前实现过滤非当前 provider，是防止其他账号目录泄漏和误切换的显式服务端限制。当前材料没有真实 ZCode build 上的跨供应商切换抓包，也没有多账号隔离、授权提示、上下文保留和 revision CAS 的实机验证；以上均为 **NOT RUN（需实机）**。在这些问题闭合前，NO-GO 是必要的安全结论。

## 4. 方案与取舍

| 方案                          | 做法                                                                       | 优点                                             | 代价／风险                                                                | 当前判断                           |
| ----------------------------- | -------------------------------------------------------------------------- | ------------------------------------------------ | ------------------------------------------------------------------------- | ---------------------------------- |
| A. 保持当前单供应商目录       | 保持 `SelectionView` 与现有选择器不变；只显示当前 provider 的模型          | 维持已验证的服务端边界；不泄露其他 provider 目录 | 无跨供应商快捷操作                                                        | **当前采用**                       |
| B. 仅把当前供应商模型分组显示 | 前端对现有 `models` 做视觉分组                                             | 不改服务端协议                                   | 不提供真实跨供应商能力，容易让用户误解；目录若只有当前 provider，收益有限 | 不建议作为 O1 交付                 |
| C. 基于授权目录做两级选择     | 先实机证明可切换与授权边界，再由后端投影受限 provider 目录并校验目标三元组 | 与用户期望一致，服务端保留授权、隔离和 CAS 控制  | 需要扩 DTO、动作指纹、命令构造、确认、缓存失效与窄屏交互                  | **证据通过后再审阅，不在当前实现** |

本文件记录的是条件化方案，不宣称用户已批准 C 进入实现。当前可执行选择仍为 A，直到证据门通过并另行确认施工范围。

## 5. 证据门：实现前必须完成

只在受控测试会话与用户已授权账号中操作。证据仅记录字段形状、命令类别、状态迁移、revision、冻结原因码和是否出现授权提示；不得记录 JWT、Cookie、凭据、完整会话正文或未授权账号数据。

1. **目录可见性**：在官方当前 build 读取 `model-selection.getView()`，确认 `providers[]` 的实际字段与稳定性；验证它是当前已授权账号的可切换 provider 清单，而非营销目录、缓存列表或其他账号残留。
2. **跨供应商命令**：在同一真实会话中从当前 provider 切换至另一已授权 provider，记录官方实际 `switchModelConfig` 信封字段、`baseRevision` 语义、ACK 及后续状态变化。先确认切换不要求 Tuvis 当前没有的独立授权步骤。
3. **上下文与会话影响**：确认切换后原会话、工作区、上下文、活动回合及工具调用是否保留、重建或被打断；记录模型和思考档位是否同时改变。
4. **CAS 与并发**：用过期 `baseRevision` 做一次受控拒绝验证；在官方端并发改变档位或 provider 后，确认 Tuvis 命令不会覆盖更新状态，且旧快照不能把手机选择器回退。
5. **授权隔离**：对未登录或用户无权限的 provider 验证目录是否隐藏、命令是否明确拒绝；测试期间使用第二账号时不得把其目录并入当前手机投影。
6. **切回与失败语义**：实测切回当前 provider、目标模型没有思考档位、官方拒绝、网络中断、ACK 未知、会话换代等情况。未知结果不得自动再发或静默恢复旧值。

任一项无法验证、响应语义不明确、官方目录不能证明属于已授权账户，或切换会破坏当前会话而产品后果未裁决，保持 NO-GO；不以静态包内代码、mock、截图或单纯 HTTP 200 替代。

## 6. 证据通过后的条件化接口设计

以下接口只是证据门通过后的候选，尚非已批准 DTO 或可直接实施的 patch。最终形状须由真实官方帧与现有 crate 接口重新核定。

### 6.1 服务端投影

候选方向：保留当前值，并只暴露后端依据官方状态和授权证据确认可切换的目录。

```rust
pub struct ProviderChoice {
    pub provider_id: String,
    pub models: Vec<ModelChoice>,
}

pub struct SelectionView {
    pub provider: String,
    pub model: String,
    pub thought: Option<String>,
    pub models: Vec<ModelChoice>, // 可作为当前 provider 的兼容投影
    pub providers: Vec<ProviderChoice>, // 仅在证据确认可切换时填充
}
```

候选结构仅表达方向：

- `providers` 必须由后端从新鲜、可信的官方状态解析；不接受前端传回目录作为权限凭证。
- 每个 provider 的模型和思考档位分开验证，不得跨供应商或跨模型合并档位。
- 对目标 provider 目录的缺字段、越界数量、重复 ID、畸形档位或授权状态未知一律 fail closed；必要时退回当前 provider 视图。
- 若官方目录无法区分“已授权可切换”和“曾配置/可展示”，不得公开它。

### 6.2 动作和执行

候选动作增加明确目标 provider：

```ts
type SelectionAction = {
  kind: "selection";
  sessionId: string;
  generation: number;
  provider: string;
  model: string;
  thought: string;
};
```

服务端收到动作后必须对**新鲜视图**复验目标 provider ∈ 当前允许切换目录、目标 model 属于该 provider、thought 属于该 model，并用同一新鲜视图的 `revision` 构造 `switchModelConfig` 与 `baseRevision`。客户端不能把自报 `baseRevision` 当授权或 CAS 依据。

目标 provider 必须进入 `action_params` 指纹；同一 `actionId` 若 provider、model 或 thought 任一不同，视为 payload 冲突。provider 变化后立即淘汰旧 provider 的 selection 目录缓存，不让上一供应商的模型列表继续显示。

成功回执不能仅以 RPC 返回或 ACK 为准。只有新鲜官方视图的 revision 前进，并且 provider/model/thought 与目标三元组精确一致时，才可显示切换完成。超时、拒绝、视图未跟上或三元组不匹配时保留最后可信状态，显示未知/失败，不自动补发、回滚或假装切换完成。

### 6.3 手机交互

证据门通过后，供应商选择只改变**本地待选状态**，此时不向官方发写命令。用户选定目标供应商后，模型列表严格来自该供应商目录；选择模型后，思考档位仅来自该模型的 `thoughtLevels`。用户确认最终目标后一次发送完整三元组，避免 provider-only 中间命令造成会话处于半切换状态。

若当前选中的思考档位不属于目标模型，显示该模型声明的合法档位；只有官方语义与产品裁决允许默认首档时，才可在待选状态中预选它。不得先发模型变更、再隐式发一次档位修正。

布局沿用现有「打开官方页面 + 模型 + 思考档位」紧凑工具栏：第一级供应商与第二级模型使用紧凑控件或可访问的选择面板，当前 provider/model 始终可辨认；长名称保留完整可访问名称。窄屏换行不能遮挡任何选择或官方页面入口。若 provider 目录不可用或证据门关闭，只显示当前实现，不渲染假灰色菜单。

## 7. 幂等、并发与安全不变量

- 唯一目标身份为会话 + generation + 新鲜官方 selection revision；不按前端数组序号或旧目录定位。
- 一个完整选择只有一个 `Selection` 动作和一个 actionId；provider/model/thought 都进入指纹。
- `baseRevision` 失配时拒绝；刷新目录后需要用户重新确认，不自动重试旧目标。
- 会话切换、generation 变化、掉线、目录撤回或 provider 授权变化时，清除或冻结待选状态；不得用旧 provider 视图完成新会话选择。
- provider 列表不是账号发现 API；只暴露官方证明当前用户有权切换的必要最小集合。
- 不在日志、SQLite 动作账本或前端调试数据中写凭据、Cookie、账号令牌或官方完整原始目录。
- 未取得明确证据时，既不开放新的 provider，也不借用其他 provider 的模型/思考档位。

## 8. 验收与自动化覆盖（仅证据通过后纳入施工单）

### 自动化测试

- 解析多 provider 响应，只投影已授权且已验证可切换的 provider；未授权、未知、畸形或缺字段目标不出现在投影中。
- provider A 模型与档位只能配成 A；provider B 的目录和档位不得串入 A。
- 旧协议/无跨 provider 证据响应保持当前单 provider 回退，不改变现有同供应商选择。
- `SelectionAction` 中目标 provider 纳入序列化、动作指纹和审计摘要（审计只记类别/必要字段，不记原始目录或凭据）。
- provider/model/thought 任一不匹配、revision 过期、generation 变化均零帧拒绝。
- 精确目标快照确认成功；ACK 先于快照、旧 revision、同 revision、超时和目标不匹配都不虚报完成、不自动重发。
- 前端 provider 变更本身零 POST；最终确认恰一 POST；CAS 失配刷新目录并要求重新选择；发送中用户切会话不把结果应用到新会话。
- 320px、360px、390px 与放大字号下布局和读屏文案可用，状态未知时不显示百分比/已切换结论。

### 实机验收

所有第 5 节证据门逐条通过后，再按实际 build、账号类型、权限组合和会话状态重跑跨 provider 成功/拒绝/并发案例。记录官方命令、ACK 摘要、revision 前后值和 UI 状态；敏感值与正文不入档。自动化假宿主测试只证明 Tuvis 内部合同，不替代本节实机验收。

## 9. 停止门与未决问题

出现以下任一情况即维持 NO-GO：

1. 当前 build 没有跨 provider 写命令，或命令是否需要额外官方授权无法确认。
2. `getView` 的 providers 不能区分当前账号已授权目录，或包含其他账号/历史配置。
3. `baseRevision` 的 CAS 作用范围、切换确认方式或并发安全不明确。
4. provider 切换会新建/中断会话、丢失上下文或改变当前回合，但用户后果尚未裁决。
5. 官方响应不支持精确确认目标三元组，或错误 ACK 会造成未知写入结果。
6. 真实协议结果与本设计候选结构不一致；应先更新设计与施工单，不得盲贴接口骨架。

**当前未决项**：官方当前 build 的可切换 provider 集合；跨 provider 的真实命令载荷与授权流程；切换对当前会话和执行中 turn 的影响；CAS 与并发修改原因码；思考档位是否可在 provider 切换时原样保留。

在这些问题由实机证据回答并经过产品裁决前，本文件仅供后续探测与设计评审，不得据此打开生产能力或宣称 O1 已实现。
