# Codex APP 正文与文件产物关联实施计划

> **执行状态：** 八项任务已完成；后续修复轮已收口 T1/T2。当前按最新 `main` 基线整理提交与文档，实机验收仍按 R6 保持未完成。

**目标：** 为 Codex APP 接入结构化附件卡片和现有正文文件链接，并支持从文件预览在最近五页消息内定位最后一次引用。

**架构：** 后端复用 canonical 历史与现有文件身份规则，输出有界附件投影和同代只读分页。前端分别维护实时尾窗、有限历史页、附件展示与文件定位，复用既有文件读取及预览布局。

**技术栈：** Tauri 2、Rust、React 19、TypeScript、Tailwind CSS v4、Vitest、Testing Library。

**规格依据：** `docs/engineering/specs/2026-10-20-codex-message-artifacts-design.md`（用户已批准）。

**字段依据：** `docs/engineering/evidence/codex-desktop-ipc/2026-10-20-codex-artifact-field-evidence.md`。已核验 macOS 安装包字段读取；Windows 样本与产品实测未执行，不能记作通过。

**基线与交付：** 初始实现基于 `origin/main@0ad8d763`；PR 前同步至 `origin/main@fd75142fd6fdc07d674cb70eb01c3f8e5f75f068`。提交按后端、移动端、文档三个功能区块整理；推送、开 PR 与 CI 通过后合并由用户明确授权。

## 全局约束

- `docs/MASTER-PLAN.md` 为最高依据；不修改该文件及既有 IPC 冻结 spec。
- 首期仅 Codex APP；不改其他工具链路，不重做生成图扫描入池，不改真实用量账本。
- 文件身份和正文识别复用既有规则；不新增按名称猜路径、猜版本或工具输出正则抽路径。
- 所有文件链接打开当前文件；不存档历史文件，也不以缩略图缓存充当文件预览。
- 五页包含当前尾窗；最多四次更早读取，每次最多 50 条。到头或五页未命中即结束，不扩大搜索。
- 图片卡每回合最多 12 张；`fileChange` 每消息最多 6 个 chip；同消息与同回合的编号／数量不能混淆。
- 单线程镜像 16 MiB、全局镜像 64 MiB；消息尾窗最多 500 条／1 MiB，附件计入预算；保留最新消息的既有保底语义。
- 文件预览沿用敏感目录黑名单及大小限制：图片 5 MB、其他文件 500 KB；不新增项目目录边界。
- HTTP(S) 图片只显示外链；fileId、base64 和其他 scheme 不代理下载、不猜路径。
- 新增字段兼容旧载荷；新文案中英同步，错误码棘轮不提高；附件内容及路径不新增日志／审计输出。
- Windows 验收目标为 Desktop `26.1013.4894.0`；换 build 时记录版本与差异；没有实机证据不宣称产品验收通过。

## 复核重点

1. 同一路径内容被覆盖、预览还开着时再次点击，必须重新读取当前文件，不能命中旧图片缓存（任务 6、8）。
2. 文件仅在普通正文链接中出现、没有结构化附件时，也能在限定范围内反向定位（任务 7）。
3. 已缓存第六页或更多旧消息时，当前定位仍只搜索五页，且第五页末条能命中（任务 5、7）。
4. 查找过程中到达 SSE 新帧、历史改写或切换会话，不能拼接旧代历史、跳错消息或留下过期高亮（任务 4、5、7）。
5. 纯附件开场消息、超大附件列表、缺路径和非 HTTP 图片引用，不得整条消失、突破预算或被误报为可预览（任务 1、3、6）。

## 任务总览与依赖

| 任务 | 实现的功能点                   | 主要交付物                                        | 依赖    |
| ---- | ------------------------------ | ------------------------------------------------- | ------- |
| 1    | 核验输入形态并确定附件契约     | 字段／状态矩阵、来源清楚的夹具、项目自有 DTO 契约 | 无      |
| 2    | 让文件池与附件使用同一文件身份 | 共享身份函数、文件池 `fileKey`、旧载荷兼容        | 1       |
| 3    | 将五类结构化引用投影成附件     | 附件／回合元数据、纯附件消息、预算与截断          | 1、2    |
| 4    | 按同一历史代际读取更早消息     | 只读分页端点、游标与失效语义                      | 3       |
| 5    | 保留实时尾窗与有限历史页       | 前端 DTO／API、历史合并与生命周期                 | 4       |
| 6    | 点击正文或附件打开当前文件     | IPC 链接接线、卡片、图片懒加载、预览刷新          | 3、5    |
| 7    | 从文件池／预览定位最近正文引用 | 来源合并、文件列表排序、五页搜索与高亮            | 2、5、6 |
| 8    | 验证完整功能并形成交付记录     | 自动化门禁、跨端实测记录、后续事项草稿            | 1—7     |

每个任务结束形成一个可独立评审的本地提交。任务 1 是证据前置，不能通过勾选它来掩盖尚未取得的目标平台样本；依赖相关字段的实现先核对证据，再编码。

## 任务 1：输入形态取证与附件契约

**功能点：** 确定支持的结构化引用、状态与缺省行为，给后续任务统一输入和输出口径。

**涉及文件：** 更新上述字段核验记录；在 `src-tauri/tests/fixtures/codex-desktop-ipc/observed/` 添加真实脱敏样本及 README 来源说明；合成回归数据放在独立 `artifacts/` 子目录，不能标为 observed。

**关键方法与契约：**

- 对照静态映射与真实 canonical state，核验用户 `content[]`、开场 `params.input`、各 item 路径和状态；不借其他会话存储形态冒充 IPC 样本。
- 项目自有附件字段固定为 `artifactId`、`kind`、`source`、`path`、`url`、`fileKey`、`ordinal`、`imageIndexInTurn`、`summary`、`sourceStatus`、`changeKind`、`unavailableReason`。可缺省信息使用空值，不从文本猜补；状态和变更类型只保留有依据的标量信息。
- `kind` 为 `image/document/external_image`；`source` 为 `image_generation/local_image/mention/file_change/image_view/image_url`。最后一项用于已有范围内的用户图片 URL。
- 消息增加 `turnId`、`artifacts`、`turnImageCount`、`omittedArtifactCount`；`ordinal` 表示来源自己的序号，`imageIndexInTurn` 服务每回合图片卡限额，两者分别计算。
- fileId／data URL 无可读本地路径时显示不可用引用；`fileChange` 的状态、增删改类型按真实证据表达，不将失败或未完成项算成成功写盘。

**验收条件：**

- 五类来源和 URL／fileId 分支均有字段、空值、状态及证据等级记录；记录明确区分 macOS 静态核验与 Windows 样本。
- 纯图片、纯文档、空文本开场、路径空缺、相对路径、失败／进行中变更均有可追溯输入。
- 没有原始 prompt、用户私有路径、base64 或凭据入库；衍生夹具注明来源与改动。
- 未取得的证据保持未完成；不据此自动删减首期范围。目标形态与方案不同，先记录差异再处理相关分支。

**执行步骤：**

- [ ] 完成并复查字段矩阵，补充真实脱敏样本与独立合成输入；确认后续读取字段都有依据。
- [ ] 将本节项目自有契约补入核验记录，明确状态映射及缺省行为；业务代码保持未修改。
- [ ] 本地提交证据与契约记录；缺少目标证据时明确标记受影响任务，不能假勾完成。

## 任务 2：共享文件身份与文件池身份键

**功能点：** 同一文件的既有路径写法能关联一次，不同文件不误合并，前端无需重写宿主路径算法。

**涉及文件：** 新建 `src-tauri/src/file_identity.rs`；修改 `src-tauri/src/lib.rs`、`src-tauri/src/remote/files.rs`、`src-tauri/src/remote/api.rs`、`src/mobile/api.ts`；更新受 `FileEntry` 新字段影响的构造及现有测试。

**关键方法：** 将现有 `normalize_key`／`artifact_dedup_key` 纯函数提取到共享模块，提供 `file_identity_key(path, cwd, windows) -> String`。文件池输出兼容的可选 `fileKey`，附件使用同一函数；不改变展示路径与既有合并顺序。

**验收条件：**

- 现有大小写、斜杠、相对／绝对路径、`.`、保留 `..` 和 cwd 缺失规则全部保持；不存在的文件仍可生成身份键。
- 不同目录同名文件及不同文件名不被合并；不引入 basename 别名猜测。
- `/session-files` 的 `fileKey` 依据该会话实际预览 cwd 计算，不能用另一套 cwd。
- 原有文件来源、修改标志、排序及全盘黑名单读取行为不回退；旧 JSON 没有 `fileKey` 仍能读取和预览。

**执行步骤：**

- [ ] 扩充身份与序列化回归：同文件等键、不同文件异键、文件不存在不触盘、旧载荷可读。
- [ ] 先运行现有身份测试，随后提取纯函数并给 HTTP 文件条目附键；保持原测试结果不变。
- [ ] 运行 `cargo test --lib file_identity`、`cargo test --lib remote::files::tests` 和 `pnpm test -- tests/mobile/api.test.ts`，确认共享函数与既有 API 回归通过。
- [ ] 确认测试通过后本地提交本任务。

## 任务 3：结构化附件、纯附件消息与有界投影

**功能点：** 五类引用在投影中拥有稳定附件与回合身份，附件独占消息不丢失，新增内容不会突破预算。

**涉及文件：** 新建 `src-tauri/src/services/codex_desktop_ipc/artifacts.rs`；修改同目录 `mod.rs`、`types.rs`、`mirror.rs`；更新 `src/mobile/codexIpcApi.ts` 及相关 DTO fixture；测试位于 `src-tauri/tests/codex_desktop_ipc.rs` 和提取模块内。

**关键方法：** `project_item_artifacts` 按任务 1 证据抽取；`project_messages` 统一处理正式用户项与开场后备。先按 canonical 回合顺序统计编号，再裁尾窗，避免编号随翻页变化。文件路径通过任务 2 生成身份，不新增文件扫描。

**验收条件：**

- 各来源产出任务 1 定义的附件字段，未知 item 仍保留现有 unsupported 信息；空路径显示原因。
- 纯附件用户消息可见，正式项到达后替换后备项，不产生两条用户消息或跳变的来源身份。
- 回合内图片卡编号及总数稳定；生成图自己的序号与全回合图片卡索引互不混用。fileChange 图片仍是文件 chip，按 6 chip 限额，外链 chip 不触发图片下载。
- `artifacts` 缺省为空；无 `turnId` 等字段的旧载荷不崩溃。原有文本、pending、发送及审批投影不受影响。
- 消息窗口按实际序列化体积计算，最多 500 条／1 MiB；最新一条保底保留，裁剪有明确标记。
- 附件摘要最多 1024 个 Unicode 字符，路径及文件身份键分别最多 16 KiB，每消息最多 128 个附件；字段或数量超限有显式原因／省略计数，不从巨大 base64 生成 DTO。
- 服务端预算省略与 UI 的 12 图／6 chip 展示省略分别表达；未传到文件池的被裁剪数据不伪称“都能去文件池找到”。

**执行步骤：**

- [ ] 先加抽取、纯附件、后备替换、跨尾窗编号、空路径及超大元数据用例，运行并确认新增断言在实现前失败。
- [ ] 实现提取和回合统计，接入共享身份与完整消息预算；不改生成图入池实现。
- [ ] 运行 `cargo test --lib services::codex_desktop_ipc`、`cargo test --test codex_desktop_ipc`；新增断言及既有回归全通过。
- [ ] 本地提交本任务，记录新 DTO 与省略字段的兼容性。

## 任务 4：同代只读消息分页

**功能点：** 在镜像已取得的 canonical 历史中读取更早消息，并准确表达可继续、到头及不可用状态。

**涉及文件：** 修改 `codex_desktop_ipc/types.rs`、`mirror.rs`、`adapter.rs`、`remote/codex_ipc.rs`、`remote/server.rs`；测试位于 `src-tauri/tests/codex_desktop_ipc.rs`、`src-tauri/tests/codex_ipc_remote.rs`。

**关键接口：** `DesktopIpcAdapter::messages_page` 复用投影器；新增 `GET /m/api/v1/codex/thread/messages`，参数为 `session_id`、`before_id`、`history_token`、`limit`。limit 缺省为 50，允许 1—50，其余返回 400；每页最多 1 MiB，不能再次经过仅保留当前尾窗的裁剪器。

返回契约为 `messages/historyToken/revision/nextBefore/status/reasonCode`。`status` 为 `ready/end/loading/unavailable/cursor_invalid`；非正常状态不能当作完整空页。当前投影同时输出兼容的 `historyToken`。

**验收条件：**

- 分页不含游标自身，顺序与当前投影一致；连续翻页不重复、不跳过；空历史及真正到头返回 `end`。
- before_id 不存在、代际不一致或历史不完整均有独立状态；镜像预算拒绝不能伪装成无更多。
- token 隔离进程／owner 换代、镜像重建及旧历史改写；普通尾部追加和活动消息流式补全不使历史页无故失效。已分页区域被修改、删除或重排后不能继续使用旧页。
- 复用鉴权、session → thread 解析与只读 hydrate；无 token 拒绝，错误会话不能读取其他线程；不触发发送、审批或其他写动作。
- 序列化与历史取得在有界范围内完成，不持互斥锁跨 await，不为分页重新扫描 rollout 或磁盘历史目录。

**执行步骤：**

- [ ] 增加页边界、越界 limit、超大单条、缺参、鉴权、部分历史、代际切换和历史改写用例；确认缺接口／旧行为下失败。
- [ ] 实现游标、历史 token 和状态输出，复用已有 canonical 读取与 hydrate，保持新增能力只读。
- [ ] 运行 `cargo test --test codex_desktop_ipc`、`cargo test --test codex_ipc_remote`、`cargo test --test codex_control_remote`；均通过后本地提交。

## 任务 5：实时投影与有限历史页合并

**功能点：** 移动端能消费新 DTO 和分页，同时保留实时更新、正确清理失效历史并防止跨会话串台。

**涉及文件：** 修改 `src/mobile/codexIpcApi.ts`、`src/mobile/useCodexThread.ts`；新建 `src/mobile/codex-history.ts` 纯合并函数；测试使用 `tests/mobile/codexIpcApi.test.tsx`、`tests/mobile/useCodexThread.test.tsx` 及新建 `tests/mobile/codex-history.test.ts`。

**关键接口：** `fetchCodexMessagesPage` 接收任务 4 参数和 AbortSignal；hook 保留 `state.messages` 为当前尾窗，额外提供 `loadedMessages`、`historyPages` 与 `loadEarlier`。`mergeCodexHistory` 按 token、消息身份和 canonical 顺序合并，当前投影覆盖重叠区。

**验收条件：**

- 首帧、SSE 更新、GET 轮询和手动刷新使用同一 DTO 归一化；旧载荷缺少新数组或 token 时安全降级。
- SSE 新帧不直接冲掉有效更早页；消息滚出尾窗时不产生缓存接缝缺口或重复。
- 每次定位固定当前尾窗与游标链；历史缓存最多四页、每页最多 50 条、合计最多 4 MiB，超过时丢弃更早缓存。
- 缓存中出现更远消息不代表本次可越过五页；token 失效、线程重建、禁用订阅和切换会话清理旧页、在途请求及定位状态。
- 旧响应、旧 token 或已卸载组件的数据不得覆盖当前会话；现有 SSE 三次失败转 3 秒轮询行为保持。

**执行步骤：**

- [ ] 先加旧 DTO、尾窗移动、重叠覆盖、五页缓存界限、历史改写和迟到响应测试，确认旧实现不满足新增行为。
- [ ] 实现 API、纯合并函数与 hook 扩展；以保留当前尾窗的方式向后兼容现有详情页消费方。
- [ ] 运行 `pnpm test -- tests/mobile/codexIpcApi.test.tsx tests/mobile/useCodexThread.test.tsx tests/mobile/codex-history.test.ts`，确认订阅、轮询与清理的既有断言也通过；本地提交。

## 任务 6：正文链接、附件卡片和当前文件预览

**功能点：** IPC 正文及附件可点击进入现有预览；图片按需显示，重复打开同一路径能取得当前内容。

**涉及文件：** 新建 `src/mobile/ProjectedArtifacts.tsx`、`src/mobile/useArtifactImages.ts`；修改 `src/mobile/SessionDetail.tsx`、`src/mobile/message-render.tsx`、`src/mobile/FilePreview.tsx`、`src/mobile/api.ts`、`src/i18n/locales/zh.json`、`src/i18n/locales/en.json`；新建 `tests/mobile/ProjectedArtifacts.test.tsx`、`tests/mobile/useArtifactImages.test.tsx`、`tests/mobile/SessionDetailArtifacts.test.tsx`，扩充 `tests/mobile/FilePreview.test.tsx`。

**关键方法：** IPC 文本接入既有 `renderMarkdown/renderLinkifiedText`；附件点击复用 `openFile`。`fetchFile` 增加兼容的可选 AbortSignal；图片 hook 用 IntersectionObserver 和最多两个请求调度。预览使用显式打开次数作为刷新依据，布局切换不算重新打开。

**验收条件：**

- 现有能识别的路径在 IPC 正文也可点击，不新增 basename 推断；结构化附件显示上传／生成／修改／查看标签及正确文件名。
- 图片单列大卡，文档 chip，fileChange 至多 6 个 chip，整卡／整 chip 可点；每回合最多 12 张图片卡，汇总只出现一次，超出部分不下载。
- 来源摘要只取同回合非空用户消息，再退到附件摘要／来源标签；文件池摘要最多 24 字，不能取前一回合冒充本轮。
- 图片只在进入视口时加载，最多两个在途；离开视口无消费者、换会话、卸载及迟到结果均释放资源。仅同会话可见消费者之间共享请求，最后一个离开即释放，不保留无消费者的原图缓存。
- 每次显式打开预览均重新读取；同路径内容已改变、预览仍开着时再次点击，也显示新内容。宽窄屏、返回列表及布局切换行为沿用现状。
- 外链仅 HTTP(S)，使用既有安全外链方式；缺路径、fileId、拒绝、文件清理、超限及图片解码失败均有可理解占位，不触发代理或 base64 传输。
- 文档 chip 复用既有预览能力；现有预览不支持的类型如实降级，不新增 Office／PDF 等专用渲染器。

**执行步骤：**

- [ ] 编写组件和资源生命周期测试：12／13 张、6／7 个 chip、点击事件、无视口不请求、并发上限、取消和 URL 释放、同路径第二次读取；先确认旧行为失败。
- [ ] 实现卡片与图片 hook，给 IPC 正文接线，扩展读取取消及显式预览刷新；同步中英文文案和错误原因。
- [ ] 运行 `pnpm test -- tests/mobile/ProjectedArtifacts.test.tsx tests/mobile/useArtifactImages.test.tsx tests/mobile/SessionDetailArtifacts.test.tsx tests/mobile/FilePreview.test.tsx`；通过后本地提交。

## 任务 7：文件池来源与五页反向定位

**功能点：** 文件池和预览均能返回限定范围内最后一次正文引用，展示来源且保持文件身份与读写属性正确。

**涉及文件：** 新建 `src/mobile/artifact-links.ts`、`src/mobile/useArtifactJump.ts`；修改 `src/mobile/SessionDetail.tsx`、`src/mobile/FilePanel.tsx`、`src/mobile/FilePreview.tsx`、`src/mobile/message-render.tsx`、`src/i18n/locales/zh.json`、`src/i18n/locales/en.json`；新建 `tests/mobile/artifact-links.test.ts`、`tests/mobile/useArtifactJump.test.tsx`，扩充任务 6 的详情页测试及 `tests/mobile/FilePanel.test.tsx`。

**关键方法：** `buildArtifactFileView` 按 fileKey 合并来源、条目与最近引用顺序；`findLastFileReference` 对单页逆序匹配结构化附件和现有可识别正文链接；`useArtifactJump` 编排当前页加最多四页更早读取。文本路径通过已知条目映射到 fileKey，不从正文抽新文件或推测别名。

**验收条件：**

- 同文件只显示一次；已有来源优先级及“写过不撤销已修改”保持。只有证据证明修改已完成才提升写来源和 `modified`；失败／进行中的修改引用保留状态标签，不能撤销已有写标志，也不能凭来源名称虚构成功写盘。
- 按 spec 排序：本次范围内有引用证据的条目按最近引用倒序，其余既有条目保持后端相对顺序。外链不进入本地文件池。
- 来源按钮与文件名预览两个热区互不触发；预览中的定位按钮使用同一查找流程。未知来源也能发起查找，不预造 messageId。
- 当前页命中零分页请求；第二至第五页按顺序查找；第五页末条能命中；总共五页未命中只发四次更早请求并结束。
- 第六页即使已缓存也不搜索；普通正文链接无 artifact 时能命中；同文件多次引用取最近消息，同消息多处引用取最后一个对应节点。
- 未命中不提示继续加载；加载失败／历史不完整／游标失效单独提示并停止，不伪装正常未命中。
- 窄屏先显示消息区再滚动，命中节点及消息短暂高亮两秒；结束、换会话及新动作清理计时器。失败仍可返回原文件预览。

**执行步骤：**

- [ ] 写纯关联与搜索状态机测试，重点覆盖仅正文链接、第六页缓存、第五页末条、反向最近引用和历史失效；确认实现前失败。
- [ ] 实现关联纯函数及定位 hook，再接文件池／预览按钮、消息引用节点和高亮；不在 SessionDetail 内再造路径算法。
- [ ] 运行 `pnpm test -- tests/mobile/artifact-links.test.ts tests/mobile/useArtifactJump.test.tsx tests/mobile/SessionDetailArtifacts.test.tsx tests/mobile/FilePanel.test.tsx`，验证点击热区、排序与窄屏交互；通过后本地提交。

## 任务 8：完整验收、平台证据与交付记录

**功能点：** 证明上述行为协作成立，明确自动化结果和真实设备的完成边界。

**涉及文件：** 新建 `docs/engineering/acceptance/2026-10-20-codex-message-artifacts-acceptance.md`；更新字段证据记录；在验收记录附跨工具统一 artifacts 的本地 issue 草稿，不自动对外创建。

**验收条件：**

- 前端测试、格式、lint、类型／构建、i18n 键集及新增英文值核对、错误码门禁和移动端构建通过。
- Rust 格式、clippy、lib 及三组 IPC／控制集成测试通过；复用现有能力门，不翻转未实测能力。
- Windows 目标 build 完成三张生成图、一个上传文档、一张上传图片、五页边界定位、同路径覆盖后再打开及失败占位验证。
- macOS 与移动端分别记录设备／版本、自动化或实机、结论；Mac 静态字段证据不当成 Windows 运行证明。
- 没有设备或证据时将对应项目保留未完成；自动化交付和产品验收分别报告，不把未执行写成通过。
- 验收记录包含任务到 spec 的覆盖映射、真实命令结果、未执行项、已知限制及本地分支／提交；不含用户私有数据。

**执行步骤：**

- [ ] 运行 `pnpm test`、`pnpm check`、`pnpm build:mobile`；新增英文文案逐项核对。
- [ ] 在 `src-tauri` 运行 `cargo fmt --all -- --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test --lib`、`cargo test --test codex_desktop_ipc`、`cargo test --test codex_ipc_remote`、`cargo test --test codex_control_remote`；Windows 构建并发保持 `CARGO_BUILD_JOBS=2`。
- [ ] 完成并记录上述跨端实测；失败归属到对应功能任务修复，只重跑受影响检查及必要回归。
- [ ] 自审验收记录和后续事项草稿，确认每项有证据后本地提交；向用户报告已完成与未完成的边界。

## 计划自审与执行入口

本计划按功能验收拆分，不把测试集中拖到最后；每个业务任务先补有意义的失败断言，再实现、回归、本地提交。取证任务和最终验收任务以其证据／检查结果为完成标准。

覆盖关系：spec §一／二由任务 2、3、6、7落实；§三由任务 1—5 落实；§四由任务 4—7 落实；§五由任务 2、4、6、8 落实；§六由各任务验收与任务 8 汇总。五项复核重点均已分配到对应任务。

接口、文件名和限制只在所属任务定义，后续任务复用。实施时发现真实结构差异，先更新证据记录及受影响计划；涉及已批准产品行为则交用户裁决，不边实现边扩大范围。

用户评审本计划后选择本会话顺序执行或子代理分工；未选择前不启动实现。推荐本会话顺序执行：这些任务共享 DTO、文件身份和历史状态，按顺序推进更容易保持契约一致。
