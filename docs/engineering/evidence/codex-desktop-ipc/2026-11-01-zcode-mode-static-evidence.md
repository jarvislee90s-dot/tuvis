# ZCode R10 模式切换静态证据与实施判定

日期：2026-11-01。类别：PACKAGE + 本仓代码核对。结论：**GO_STATIC，可进入本批编码；不是 LIVE_PASS。**

## 1. 来源

本机 `/Applications/ZCode.app` 的 `CFBundleVersion=3.16.2.8769`。只读解析 `Contents/Resources/app.asar` 和 `Contents/Resources/glm/zcode.cjs`；未运行提取代码、连接账号、修改模式或读取凭据。以下为解码后字符串索引，不混称原始字节偏移。执行前复核 build/文件 SHA-256，新包不得套旧索引。

## 2. 证据链

| 环节         | 已确认事实                                                                        | 来源                                                                                   |
| ------------ | --------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------- |
| 输入 schema  | switchCollaborationMode payload 为 build/edit/plan/yolo/guarded 枚举              | CLI 1231827；固定公开源码 pinned-src-3c5b2276/command.ts:261 同形                      |
| CAS          | 命令位于要求 baseRevision 的集合                                                  | CLI 1232757；固定源码 command.ts:327                                                   |
| 实际 handler | xfc 读取 payload.mode；未变化且无 planEnabled 时 noop，否则调用 app.setMode(mode) | CLI 14773332；注册到 Mpi.switchCollaborationMode，14775179                             |
| 配置读取     | 会话 config 含 mode，默认 build，及可选 planEnabled                               | asar out/main/chunk-XYX2CJ3H.js，637258 附近                                           |
| UI 映射      | build=变更前确认；guarded=自主模式；plan=计划模式；yolo=完全访问；edit=自动编辑   | asar out/renderer/assets/IntlProvider-C2IbGUkl.js，312873–313200 附近 mode.label.glm.* |

命令复用现有 zcode-agent / sendConversationCommandV4 信封，不另造 terminal/stdin 通道。身份、目标、revision、运行态和 pending 守卫继续执行。

## 3. 可做的准确边界

1. schema、handler、读取字段和标签映射齐备，足以实施 Mode DTO、纯构造、受控派发和配置确认。
2. 首版写选项为 plan/build/guarded/yolo；edit/未来值只读兼容。
3. planEnabled 是独立配置因素。G0 须核对 setMode 的配置规范化与序列化，不能只用原始 mode 字符串相等判断所有模式生效。
4. 已知 build + 形状齐备按设计进入用户主动动作候选；缺证据则仅关闭 Mode，不复制宽泛 true。
5. 模式配置生效不证明 terminal-origin sendText 会触发审批；旧合同 guarded 负结果保留，其中文简称不用于新标签。

## 4. 尚未实跑

四项真实切换、原生界面同步、planEnabled 规范化、运行中拒绝条件、完全访问确认效果、手机发送的实际审批策略均未实跑。必须分开记录自动化与实机，不能以本文件声称已提供相应权限保护。
