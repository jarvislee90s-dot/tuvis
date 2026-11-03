# Codex 附件字段核验记录

> 日期：2026-10-20。
> 用途：为正文产物关联实施计划提供字段依据，区分静态代码核验与真实 IPC 样本、产品验收。
> 关联规格：`../../specs/2026-10-20-codex-message-artifacts-design.md`。

## 一、本轮已核验的来源

- 本机安装包显示版本：macOS `26.1013.40934`；`CFBundleVersion` 为 `20770`。
- 对象：安装包 `Contents/Resources/app.asar`，仅静态读取，未修改安装包，未连接 IPC 或触发真实会话动作。
- 主要依据：归档内 `webview/assets/execution-11111e21e1a0.js` 的 item／用户输入映射函数。
- 该资源长度：`122436` 字节。
- 该资源 SHA-256：`fc3412c176d0985eb5805e23fcb31de9960e47dedf558cce0f9cb82b53cfaaf4`。
- 辅助依据：`.vite/build/src-DsLVGakC.js` 的图片显示转换与输入构造，以及 `webview/assets/workspace-11e385dee7b5.js` 的图片来源收集。

以下位置为主要资源解码后字符串中的零起始字符偏移，用于复查定位，不作为跨版本稳定行号。记录只归纳字段，不复制整份应用代码或用户数据。

## 二、字段依据与边界

| 对象              | 静态代码实际读取的字段                                                  | 定位依据                                                              | 本期处理依据                                                                                    |
| ----------------- | ----------------------------------------------------------------------- | --------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| `userMessage`     | `id`、`content[]`，逐块分派 `type`                                      | 偏移约 `79923`                                                        | 文本和附件来自同一用户消息；不能仅因文本为空丢弃消息                                            |
| `imageGeneration` | `id`、`status`、`revisedPrompt`、`savedPath`；后者允许空值              | 偏移约 `81560`；辅助转换优先使用字符串 `savedPath`，否则处理 `result` | 使用本地路径与摘要，路径空时占位；不输出 `result`／base64                                       |
| `localImage`      | `path`                                                                  | 偏移 `82183`                                                          | 输入本地图片；不把它当作模型生成图                                                              |
| `mention`         | `name`、`path`                                                          | 偏移 `82323`，与 `skill` 共用字段映射                                 | 仅收 `mention`，按本期允许的文件类型展示，不能因此扩大到 `skill`                                |
| `fileChange`      | `id`、`status`、`changes[]`，每项包含 `path`、`kind`，输出另可含 `diff` | `userMessage` 分支后相邻的 item 映射                                  | 使用变更路径，不将 diff 塞入附件 DTO；失败／进行中状态不能伪装成已完成的文件修改                |
| `imageView`       | `id`、`path`                                                            | 偏移 `81428`                                                          | 标记查看来源，路径仍通过既有文件预览安全读取                                                    |
| 用户内容 `image`  | `url` 或 `fileId`；映射会区别 data URL 与普通 URL                       | 偏移 `82064`                                                          | 仅 HTTP(S) URL 外链可用；纯 fileId、data URL 或其他 scheme 明确降级，不推测本地路径、不代理下载 |

补充结论：

- `imageGeneration` 的路径字段及摘要字段在本机代码中有直接读取依据，不能再把外部方案中的字段当成毫无来源的猜测。
- URL 与 fileId 是两种实际形态；缺少本地路径的 fileId 不能凭文件名反查或变成服务器代理请求。
- `fileChange` 有状态及变更类型，提取器必须保留相应语义，不能把任意出现的 change 都标成已经成功写盘。
- 这些字段读取证据不能证明所有运行时空值、状态枚举、相对路径基准或 Windows build 的兼容性。

## 三、项目自有附件契约（任务 1）

下表是兔维斯的输出契约，**不是 Desktop 原生 schema**。输入位置据本机
`26.1013.40934` 静态映射确定；`observed/` 现有真实 canonical 快照只证明
`turn.items[]`、`userMessage.content[]`、`turn.params.input[]` 的外壳，未出现本节附件。
`artifacts/` 的分支输入均为 SYNTHETIC，不能提升为运行时字段或枚举的实测证据。

| 来源／输入位置 | 读取字段；空值处理 | 状态、变更类型；证据等级 |
| --- | --- | --- |
| `imageGeneration` item | `id`、`savedPath`、`revisedPrompt`、`status`；无字符串路径时 `path=null`、`fileKey=null`，保留不可用引用；摘要只取有界 `revisedPrompt`，不读取 `result` | `sourceStatus` 只保留可确认的字符串标量；不把缺失、失败或进行中视为可预览。字段名为 macOS 静态；运行时取值未实测 |
| 用户 `localImage` 内容块（正式 `content[]` 或开场 `params.input[]`） | `path`；空值或非字符串不可预览；相对路径仅交给共享文件身份／预览规则，不以消息文本或文件名猜 cwd | 无独立状态，`sourceStatus=null`；字段名为 macOS 静态，开场外壳为既有 Windows 脱敏样本，附件组合为合成 |
| 用户 `mention` 内容块 | `name`、`path`；仅 `mention`，排除 `skill`；路径缺失不可预览；`name` 可作有界摘要而非路径 | 无独立状态，`sourceStatus=null`；macOS 静态，附件组合为合成 |
| `fileChange` item | `id`、`status`、`changes[]`，每项 `path`、`kind`；每个有路径的 change 形成文件引用；空路径保留不可用引用；不将 `diff` 当摘要或附件 | `sourceStatus` 与 `changeKind` 分别保留原始字符串标量，不猜归一枚举；只有状态有已完成证据且路径可读时才可表述为已写盘。字段名为 macOS 静态，具体枚举值／失败、进行中运行态未实测 |
| `imageView` item | `id`、`path`；缺路径为不可用引用 | 无独立状态，`sourceStatus=null`；macOS 静态，附件组合为合成 |
| 用户 `image` 内容块 | `url` 为 HTTP(S) 时输出外链；只有 `fileId`、data URL 或其他 scheme 时 `path=null`、`url=null`、`fileKey=null`，仍保留不可用引用 | 无独立状态；`url`／`fileId` 分支为 macOS 静态，运行时样本未实测。不从 fileId、data URL 解码、下载或反查本地路径 |

附件对象固定字段：`artifactId`、`kind`、`source`、`path`、`url`、`fileKey`、
`ordinal`、`imageIndexInTurn`、`summary`、`sourceStatus`、`changeKind`、
`unavailableReason`。`kind` 只取 `image`／`document`／`external_image`；
`source` 只取 `image_generation`／`local_image`／`mention`／`file_change`／
`image_view`／`image_url`。这六个 source 对应五类结构化引用，其中
`image_url` 是已有范围内用户 `image.url` 的独立分支。`artifactId` 由稳定消息身份
与块／change 位置组成，不使用展示路径作唯一身份。可缺省的 `path`、`url`、
`fileKey`、`summary`、`sourceStatus`、`changeKind`、`unavailableReason` 用 `null`；
不可预览时 `unavailableReason` 给有界代码，不塞原始 URL、fileId 或路径。
非字符串状态／类型、未知枚举和值缺失都不转成“成功”；未知原值不得被猜测为
`add`／`modify`／`delete`。来源序号 `ordinal` 独立按来源中的位置计算，
`imageIndexInTurn` 仅给图片卡按 canonical 回合次序编号，两者不能混用。
`image_generation`、`local_image`、`image_view` 的图片引用为 `kind=image`；
HTTP(S) 用户图片 URL 为 `kind=external_image`，不触发本地下载；`mention`
按允许的文件类型选择 `image` 或 `document`；`file_change` 无论扩展名都作为
`document` 文件 chip。`unavailableReason` 只承载机器可判定的短码，例如
`missing_path`、`unresolved_file_id`、`data_url`、`unsupported_url_scheme`、
`source_not_completed`、`unreadable_path`，具体展示文案由 UI 决定；短码不
包含原始内容。图片编号只用于实际可作为图片卡显示的本地图片，不把
`file_change` chip 或外链纳入 12 图限额。

消息兼容扩展为 `turnId`、`artifacts`、`turnImageCount`、`omittedArtifactCount`：
旧载荷的 `artifacts` 缺省视为空数组，计数缺省为 0；`turnId` 缺省为空值。
只有附件而文字为空的正式用户消息及开场后备仍需保留。正式项与开场后备沿用
`clientId`／`clientUserMessageId` 的身份对位，后备仅来自已验明身份的 Turn；
正式项抵达后替换后备，不叠成两条。`turnImageCount` 按完整 canonical 回合统计，
`omittedArtifactCount` 只数服务端预算省略，现由消息级双语提示展示（`mobile.artifacts.serverOmitted`，
只说数量、与 UI 限额分开；附件全部被省略时仍显示）；UI 的每回合 12 图／每消息 6 个
`fileChange` chip 限额另行显示。`fileKey` 由任务 2 的共享函数计算，路径不可用
时为 `null`；附件读取仍受现有文件预览安全规则约束。

## 四、取证阻断与覆盖清单

2026-10-21 本机只读复核：安装包仍为 macOS `26.1013.40934`；已有
`observed/canonical-before-load.sanitized.json` 与 Windows `4894.0` 脱敏快照
没有五类附件项。可调用的真实 canonical IPC 只覆盖现有用户会话；在不读取
私人对话正文、不触发真实会话动作的约束下，无法取得含附件的真实样本。
因此 **本轮没有新增 OBSERVED 附件文件**，也没有用 JSONL／其他存储格式冒充
canonical IPC。此处是证据缺口，不是删减首期范围的依据。

| 分支 | 可追溯输入 | 证据结论 |
| --- | --- | --- |
| 纯图片、纯文档、空文本开场 | `artifacts/cases.synthetic.json`；开场数组外壳另参照既有 `observed/windows-4894-start-turn.sanitized.json` | 组合 SYNTHETIC；附件实测待取 |
| 路径缺失、相对路径、HTTP(S) URL、纯 fileId、data URL | `artifacts/cases.synthetic.json` | 分支 SYNTHETIC；字段读取仅 macOS 静态 |
| `fileChange` 失败／进行中及增删改 | `artifacts/cases.synthetic.json` | 示例状态／类型为 SYNTHETIC；真实枚举及 Windows 目标 build 未核验 |
| Windows `26.1013.4894.0` 附件形态与移动端闭环 | 无附件目标样本；原 Windows 样本仅证明普通 canonical 外壳 | **未核验**；任务 3 相关分支编码前需核对，任务 8 产品验收必须实测 |

若后续目标 build 与上述静态映射不同，先记录差异，再处理受影响分支；涉及
已批准产品行为变化时交用户裁决。

## 五、证据状态与后续验证

| 验证事项                                          | 当前状态 | 负责阶段                      |
| ------------------------------------------------- | -------- | ----------------------------- |
| 本机上述字段的静态读取核验                        | 已完成   | 计划编写前调查                |
| 本机真实 IPC 附件样本及纯附件开场形态             | 未取得：私人会话读取／真实动作约束阻断 | 实施任务 3 前核对 |
| Windows `26.1013.4894.0` 字段、状态和路径形态对照 | 尚未核验：本机无 Windows 设备 | 实施任务 3 前核对；任务 8 做产品验收 |
| 三图、一文档、一上传图片的移动端闭环              | 尚未执行 | 实施任务 8                    |

缺失证据保持未完成；任务 3 不可将合成状态值当作目标 build 的实测枚举。

### 5.1 任务 8 结论（2026-10-21）

任务 8 完成自动化门禁与交付记录（命令与计数见
`2026-10-20-codex-message-artifacts-acceptance.md` §四 表 A／表 B），但**没有取得任何新的运行时字段证据**：

- **仍然缺失：真实 IPC 附件样本。** 本轮延续“不读取私人会话正文、不触发真实会话动作”的
  约束，因此没有 live IPC 的 `imageGeneration`／`localImage`／`mention`／`fileChange`／
  `imageView`／`image.url` 样本；纯附件开场消息的真实形态同样未观察到。
- **仍然缺失：Windows 目标 build `26.1013.4894.0` 的字段与产品验收。** 本机为 macOS，
  无法启动该 build；三张生成图、一个上传文档、一张上传图片、五页边界定位、同路径覆盖后
  再打开及失败占位均未执行（NOT EXECUTED）。
- **仍然缺失：macOS 实机产品验收与移动端真机闭环。** 自动化只在 jsdom 与 Rust 集成层；
  macOS 安装包 `26.1013.40934` 只有静态字段读取，不构成运行证明。
- **证据升级记录：0 项。** 本轮没有把任何 SYNTHETIC 输入升级为 OBSERVED，没有新增
  `observed/` 文件，也没有修改既有 observed 样本；`artifacts/cases.synthetic.json` 及其
  派生状态、任务 3 测试内联的合成状态值仍然只是回归输入，不是目标 build 的实测枚举。
- 自动化通过（`pnpm test`、`pnpm check`、`cargo fmt/clippy/test` 等）只证明实现与合成契约
  自洽，**不能**替代上述运行时证据；本记录的分级不因自动化通过而上调。

2026-10-21 合并前修复轮（R1–R5）只改实现、UI 呈现与测试，未新增或升级任何运行时附件证据，
上述结论不变。文件池合成行的 `modified:false` 与 24 字摘要属实现口径，不构成 `fileChange`
完成状态枚举或运行时附件字段的证据。
