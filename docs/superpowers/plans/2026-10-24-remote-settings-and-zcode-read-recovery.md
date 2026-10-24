# 远程设置重排与 ZCode 读取恢复执行计划（工作包 A）

> **执行 Agent：** 使用 superpowers:executing-plans 逐 task 执行；仅在用户选择子 agent 执行方式后使用 superpowers:subagent-driven-development。步骤用复选框跟踪。本文是执行计划，不代表本会话已开始改产品代码。

**目标：** 交付六段远程设置、默认隐藏的 4 位密码、默认开启的两个投递偏好、ZCode 折叠配置卡，以及新旧 ID 的桌面／手机读取恢复。

**架构：** RemoteSection 保留通用／四通道／访问安全，向下组合独立工具卡和无头配置组件；AuditLogSection 仍由 SettingsPage 最后装配。URL 设置走独立后端校验，读取沿现有 parser/content/files，不引入新 ZCode 写路由。

**技术栈：** 现有 Tauri 2、Rust/rusqlite/reqwest::Url、React 19/TypeScript、shadcn/ui、Tailwind v4、Vitest。

**设计：** [ZCode 统一远控设计 v3](../specs/2026-10-24-zcode-unified-remote-design.md)。同时阅读用户最新 AGENTS.md。

## 全局约束

- 用户已指定本轮目录 /Users/jarvis/Documents/tuvis-worktree2；按总计划从执行时最新origin/main新建codex/功能分支并带入文档提交，不切换主目录、不丢弃研究文档。
- MASTER-PLAN 不改；产品代码与真实用量账本不在本计划取证中混写。单测用 tempdir／内存 DB。
- 两键缺失=true、true=true、false=false、非法值=false；保留明确关闭，不做覆盖式迁移。
- 超时默认 600000ms、范围 1000–3600000；并发默认 2、范围 1–8，保持现有行为。
- 顺序：通用→四通道→访问与安全／设备列表→工具特殊配置→无头注入／兼容发送→注入审计。
- ZCode URL：本机一条、桌面编辑、手机只读；仅格式校验通过不算联机。官方凭据不在本工作包处理。
- 本包不实现发送／审批／问答／打断／模型控制，不移植 #117，不调用真实 hi。控制区域保持具体不可用原因，阅读可用。
- 不复制用户完整链接、真实 PIN、会话正文、账号或凭据进测试／日志／文档。
- 改 UI 文案同时改 zh/en；发版行为变化写升级须知，尚未发版不打 tag。

## 评审重点

1. 默认开不得覆盖用户 false，也不得绕过 Codex eligibility／设备／pending 等守卫（A1）。
2. 调整顺序不能丢设备列表、四通道向导或重复轮询；隐藏密码不能改 PIN／配对（A2、A3）。
3. URL 校验需防伪域名和存量坏值，但保留官方新增参数／版本化路径（A4）。
4. 链接错误／未联机不能清正文和文件；卡片状态不因点击外部入口变化（A5、A8）。
5. 新 ID 不是新数据库，不能重命名 ID 或放进子代理；附件 seq 需对齐实际显示投影（A6、A7）。

## 文件责任与接口

| 范围                     | 主要文件                                                                                                                                    | 所有者 |
| ------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------- | ------ |
| 默认值、已有能力门       | src-tauri/src/remote/mod.rs；src/lib/api/remote.ts                                                                                          | A1     |
| 六段装配、无头区子组件   | src/components/settings/RemoteSection.tsx；新 HeadlessSettingsSection.tsx；src/pages/Settings.tsx                                           | A2     |
| PIN 可见性               | RemoteSection.tsx；tests/settings/remoteSection.test.tsx                                                                                    | A3     |
| 链接后端／IPC／只读 API  | 新 services/zcode_control/{mod.rs,config.rs}、commands/zcode_remote.rs；commands/mod.rs、services/mod.rs、lib.rs、remote/{server.rs,api.rs} | A4     |
| 工具卡、教程、发送区状态 | 新 ToolRemoteSettingsSection.tsx、ZcodeRemoteSettingsCard.tsx；新 src/lib/api/zcodeRemote.ts；mobile/SessionDetail.tsx、MessageComposer.tsx | A5     |
| ID／读取                 | monitor/zcode_parser.rs；remote/content.rs、files.rs                                                                                        | A6、A7 |
| 回归／文档               | tests/settings、tests/mobile、src-tauri/tests；双语手册、changelog                                                                          | A8     |

不为本工作包重构全部大文件。所有 task 先提交可失败的行为测试，再最小实现；一个 task 通过并 review 后才进依赖 task。

## A0：核对基线与隔离

**文件：** 无产品编辑；记录到本任务执行日志。

**接口：** 消费设计与最新 main；产出本次工作目录、branch、base SHA、原有脏文件清单。

- [ ] 读取 AGENTS／MASTER-PLAN；git status、branch、fetch origin main，记录实际 base。不对不属于本任务的脏文件执行 reset／stash。
- [ ] 检查是否已有适合的 codex/ 分支；正常工作区从最新 main 建分支，已有隔离区不再嵌套创建 worktree。
- [ ] 核对本计划中的函数、文件和依赖仍存在；若 main 已改默认／设置结构，按差异调整计划并保留双方功能。
- [ ] 记录测试基线所需的已知失败；不运行真实模型／启动 Tuvis dev。此 task 不作无意义空提交。

**通过：** 后续所有提交有清楚基线，工作目录与用户其他工作分离。

## A1：H3 与兼容发送的新默认合同

**文件：** remote/mod.rs、src/lib/api/remote.ts、RemoteSection.tsx；tests/settings/remoteSection.test.tsx；src-tauri/tests/codex_ipc_remote.rs。

**接口：** 保留 headless_enabled_from(Option<&str>)->bool、compat_send_enabled_from(Option<&str>)->bool、remote_status 的现有字段；仅缺键默认语义变化。

- [ ] 红测 default_flags_preserve_user_choice：对两个函数各断言 None→true、true→true、false→false、空串/1/TRUE→false；在同一个内存库验证两个 key 不互相替代。
- [ ] 红测 default_compat_still_requires_evidence：开关默认 true 时，不具备既有资格的 Codex build 仍拒发；明确 false 仍拒发；已取证 build 不因偏好变化改路由。复用现有 fake hub/设备缝，不只测布尔函数。
- [ ] 修改两个解释函数及状态装配注释；前端在 status 未返回时 loading/disabled，返回旧载荷缺字段时按新默认显示，资格字段仍独立决定有效性。
- [ ] 修改旧测试中 None=false 的预期，保留非法值／显式 false 回归。启动不写 toggle 审计，不给缺键补一个伪用户操作。
- [ ] 运行：src-tauri 下 cargo test --lib remote::tests；cargo test --test codex_ipc_remote；根目录 pnpm exec vitest run tests/settings/remoteSection.test.tsx。非零用例、全部相关目标通过。
- [ ] 提交：feat(remote): enable headless and compatibility defaults while preserving choices。

**通过：** 用户不配置即可请求兼容路径，能力守卫仍有效，旧显式关闭保留。

## A2：六段顺序与无头区抽取

**文件：** RemoteSection.tsx；新 src/components/settings/HeadlessSettingsSection.tsx；Settings.tsx；tests/settings/remoteSection.test.tsx、settingsBlocks.test.ts。

**接口：** HeadlessSettingsSection 消费 RemoteStatus|null、busy:boolean，以及现有 toggle／limits 保存回调；不创建第二套设置状态和 3s 定时器。RemoteSection 继续拥有现有轮询。工具卡位置先作为有标题的空装配点，由 A5 填入；不交付空工具卡给产品。

- [ ] 红测 remote_sections_follow_user_order：render 后断言通用／通道／安全／设备／工具配置／无头／审计的可访问顺序；四通道、设备列表各一份。
- [ ] 抽取原无头与兼容发送 markup 为子组件；保留状态 owner、保存／错误回执、H4 区间和现有资格提示。
- [ ] RemoteSection 的前三段后渲染工具区与无头区，SettingsPage 的 AuditLogSection 保持其后；不新增一级侧栏或遗失原 sections 可达性锁。
- [ ] 验证切换通道、远程关闭期间设备列表、向导、PIN 草稿和同一轮询语义不变。
- [ ] 运行 pnpm exec vitest run tests/settings/remoteSection.test.tsx tests/settings/settingsBlocks.test.ts tests/settings/settingsTypography.test.ts。
- [ ] 提交：refactor(settings): order remote controls and extract headless section。

## A3：密码遮罩与眼睛

**文件：** RemoteSection.tsx；tests/settings/remoteSection.test.tsx；src/i18n/locales/{zh,en}.json。

**接口：** 新增页内 pinVisible:boolean，初值 false；setPin 及原输入值／随机／保存函数签名不变。

- [ ] 红测 pin_visibility_is_presentation_only：初始 type=password；点击显示→text、再点击→password；值始终为合成 1234，mock setPin／resetDevices 调用次数为 0。
- [ ] 红测 pin_hides_on_blur_or_remount：窗口失焦、离开设置块再返回均隐藏；轮询／随机／保存不自动显示；键盘可操作且 aria-label/aria-pressed 对应状态。
- [ ] 实现眼睛按钮（type=button），复用 lucide Eye/EyeOff 和现有 Input；事件清理完整，不用 CSS blur，也不把 PIN 复制到 title。
- [ ] 运行 pnpm exec vitest run tests/settings/remoteSection.test.tsx；pnpm check:i18n。
- [ ] 提交：feat(settings): mask remote PIN with visibility toggle。

## A4：ZCode 官方链接的配置合同

**文件：** 新 services/zcode_control/{mod.rs,config.rs}、commands/zcode_remote.rs、src/lib/api/zcodeRemote.ts；注册文件；remote/api.rs、server.rs；新 src-tauri/tests/zcode_config_remote.rs。

**接口（本 task 冻结给 A5／B）：**

- Rust ConfigState=Missing|Saved|Invalid；ZcodeRemoteConfig{config_state:ConfigState,url:Option<String>}，serde camelCase。
- validate_official_url(raw:&str)->Result<Option<String>,ConfigError>；空白为 None，其他按设计标准 URL 校验，错误不带原 URL。
- get_config(conn:&Connection)->ZcodeRemoteConfig；set_config(conn:&Connection,raw:&str)->Result<ZcodeRemoteConfig,ConfigError>；key=remote.zcode_official_url。读取也复验。
- Tauri zcode_remote_config_get()->Result<ZcodeRemoteConfig,String>、zcode_remote_config_set(raw_url:String)->Result<ZcodeRemoteConfig,String>，内部复用全局 DB／现有错误码规范。
- GET /m/api/v1/official-control?agent_type=zcode 返回该 DTO，现有配对门保护、Cache-Control:no-store，无远端抓取、无手机写接口。
- TypeScript getZcodeRemoteConfig():Promise<ZcodeRemoteConfig>、setZcodeRemoteConfig(rawUrl:string):Promise<ZcodeRemoteConfig>；mobile 读取同 wire DTO。Saved 只表示格式已存，不能当 connected。

- [ ] 红测 official_url_roundtrip：空白删除；v4/v5、有中文／编码名称、新增 query 原样语义保留；更新／删除后 get 一致。
- [ ] 红测 official_url_rejects_unsafe_or_ambiguous：伪域名、userinfo、http／javascript、非 remote 路径、非默认端口、超4096、缺失／重复 sid、重复已知身份字段拒绝；sid 最多128 UTF-8字节；未知 query 不擅自丢。
- [ ] 红测 stored_invalid_never_leaks：直接用通用 settings 注入坏值后 get=Invalid/url=null；日志／错误无完整 URL；失败保存保留旧合法值。
- [ ] 红测 remote_config_requires_pairing：未配对／吊销拒绝；已配对读 Saved；其他 agent_type 不支持；不读取官方账号、不发外部 HTTP。
- [ ] 用 reqwest::Url 及现有 DAO 实现核心、注册 IPC／GET，夹具仅用合成 sid。
- [ ] 运行 cargo test --lib zcode_control::config；cargo test --test zcode_config_remote；cargo check；pnpm exec vitest run tests/remote/api.test.ts。
- [ ] 提交：feat(zcode): persist and validate official remote configuration。

## A5：折叠工具卡、教程与手机局部状态

**文件：** 新 ToolRemoteSettingsSection.tsx、ZcodeRemoteSettingsCard.tsx；RemoteSection.tsx；mobile/SessionDetail.tsx、MessageComposer.tsx；新 tests/settings/zcodeRemoteSettings.test.tsx；tests/mobile 下新增 zcodeControlAvailability.test.tsx；zh/en。

**接口：** ToolRemoteSettingsSection 只渲染 ZCode；ZcodeRemoteSettingsCard 使用 A4 API；手机 configState 与未来 B 的 connectionState 分开，现阶段没有适配器就为 needs_connection，不伪造可写按钮。

- [ ] 红测 zcode_card_starts_collapsed：只有 ZCode 头部；点击／键盘展开；草稿与已存值分开；折叠不删配置。
- [ ] 红测 config_error_is_inline_and_non_destructive：坏 URL 显示字段旁错误；保存失败保留草稿与旧值；删除及空白保存正确；复制使用已保存值。
- [ ] 红测 read_view_survives_control_unavailable：Missing/Invalid/needs_connection/网络失败下，正文、文件仍在；发送区给配置／连接提示，失败不被当删除。无适配器时保留草稿、不调用发送／审批 API。
- [ ] 实现应用内获取链接教程：ZCode APP 开移动远控→开启→复制／扫码获取→粘贴。教程不承诺自动登录、网页引导或固定有效月数。
- [ ] 手机仅读取配置，显式“打开官方页面”辅助动作保留原页，不触发卡片点击／标已读。统一原生控制在 B 后续交付，不能以外跳冒充完成。
- [ ] 运行 pnpm exec vitest run tests/settings/zcodeRemoteSettings.test.tsx tests/mobile/zcodeControlAvailability.test.tsx；pnpm check:i18n；pnpm build:mobile。
- [ ] 提交：feat(settings): add collapsed ZCode remote card and local control feedback。

## A6：双形态 ID 与读取恢复

**文件：** monitor/zcode_parser.rs（保留 is_valid_session_id(&str)->bool）；既有 parser tests；新 src-tauri/tests/zcode_read_remote.rs（按现有 RemoteState 假源／tempdir 风格）。

**接口：** Session／SessionsResponse wire 不变；不重命名／归一化真实 ID；仍用同一 adapter 聚合。

- [ ] 红测 accepts_both_zcode_main_id_shapes：旧 UUID、新32hex及大小写接受；缺前缀、短长串、非ASCII／非hex／路径、子代理前缀拒绝。
- [ ] 红测 mixed_main_sessions_keep_distinct_ids：tempdir 两库 fixture 中新旧主会话共存，parent非空／subagent／归档／deleted／窗口外及无宿主逐项断言，空串parent沿用旧口径。
- [ ] 红测 new_id_reaches_remote_reading：合成新ID通过快照→session-messages／files，正文synthetic过滤和状态输入不变，隐藏／未读仍按各自门。
- [ ] 最小扩充合法域，不改状态核、数据根或 budget；不接 #117 写侧。
- [ ] 运行 cargo test --lib monitor::zcode_parser；cargo test --test zcode_read_remote；相关既有附件 fixture 测试。
- [ ] 提交：fix(zcode): support both observed main session ID formats。

## A7：正文与文件引用的同源锚点

**文件：** remote/content.rs、remote/files.rs；既有 zcode 读取 tests。

**接口：** read_zcode_messages_with／read_zcode_attachment_refs_with 的消费者 wire 保持兼容。新增私有 ZcodeProjection{messages:MessagesPage,source_anchors:HashMap<String,i64>}，以数据库 message ID 建显示序号映射；该 ID 不新增到公开 wire。没有显示条目的附件保留文件池，lastSeq 使用项目既有中性0口径，仅作排序占位，不伪造正文或负数哨兵。

- [ ] 红测 zcode_attachment_anchor_matches_projected_message：fixture依次为合成消息、user文本、assistant含thinking/tool/text、下一user文本+file；附件必须锚到该最后user最终显示seq，不能使用原DB ordinal。改变limit后仍对齐。
- [ ] 红测 synthetic_or_unrendered_has_no_fake_anchor：被过滤条目不指向其他消息；若file-only没有可显示条目，文件保留、lastSeq=0作为中性排序占位，不新增正文消息或负数哨兵；验证预览仍可用。
- [ ] 将正文投影与附件锚点推导共享同一个有界窗口／finalize 结果；消息多parts展开和tail裁剪后再定最终seq，避免两个地方各算一份偏移。
- [ ] 保留 artifact URL／source.path、缺实体与历史清理的旧行为，不新增全库扫描／附件来源。
- [ ] 运行 cargo test --lib zcode；cargo test --test zcode_read_remote；相关 files／content 单测。
- [ ] 提交：fix(zcode): align attachment anchors with rendered message order。

## A8：集成、双语与交付审阅

**文件：** 受影响测试；docs/user-manual/{zh,en}/{04-dashboard,08-mobile-remote,12-troubleshooting}.md；CHANGELOG.md、CHANGELOG.en.md；下一版本发布时 release notes／capabilities.json。

**接口：** 输出本包提交SHA、测试、截图、已知限制及与 B 的接口登记，不宣称统一控制已完成。

- [ ] 执行 pnpm check、pnpm test、pnpm build:mobile；src-tauri 下 cargo fmt --check、cargo clippy --all-targets --features marker-helper -- -D warnings、cargo test --features marker-helper；再跑A新增集成目标与Codex既有IPC／控制回归。基线失败有证据单列，不删测试。
- [ ] Mac／Windows 分别检查六段、眼睛遮罩／失焦、开关默认及显式false；手机核新旧ID正文／文件和局部控制提示，不发送生产消息。
- [ ] 文档同步默认值与升级须知、配置教程和能力边界；README只放概览链接，用户手册放操作细节，不重复版本账本。
- [ ] 用用户原报告的脱敏结构作样本设计，不提交真实DB／URL／PIN；逐项核新增中文对应英文，不只跑键集门禁。
- [ ] 提交并交付 review，停止。B门未完成时功能文案只能说“读取／配置可用，统一控制待接入”，不显示虚假联机。
