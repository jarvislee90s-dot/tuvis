# 隔离 Mac Desktop 授权与 M1–M3

冻结包 `8cde171d`。系统启动官方 Desktop 时为本轮进程设置独立 `HOME`、`CFFIXED_USER_HOME`、`DSH_HOME`。冻结源码的生产发现只匹配到一个属于测试 profile 的宿主，`kind=Desktop`、`verified=true`。原有宿主进程保留。

使用链接到冻结源码的本机验证器调用生产实例选择与开关命令。开关经过 `Checking` → `Installing`（等待测试宿主退出、安装、验证）→ `AwaitingReopen` → `VerifyingPlugin` → `Authorizing` → `Ready`，绑定账本已创建。重开测试宿主时同时传入同一个短 `TUVIS_DSH_STATE_DIR`，使已装插件能发现授权服务。官方 `session/create` 在独立临时项目创建测试会话。验证器首次创建时工作目录落在工作树，未向该会话发送消息，已移走验证器临时标识并清理空项目目录。计入验收的会话在独立临时项目和独立宿主转写根中重新创建。

## 本轮证据

| 证据 | 实测 |
| --- | --- |
| 入口和宿主受理 | 从冻结包的 `/m/api/v1/session-send` 发送三条带唯一标记的消息，分别使用独立动作 ID。M1、M2、M3 均返回 HTTP 200、账本 `accepted`。首次诊断请求字段大小写错误返回 400，未进入投递。 |
| 浏览器实际加载 | 本机浏览器完成测试配对，真实 v4 DSH 卡片标题显示 M1、摘要显示 M3。进入该卡片详情后，浏览器按序显示 M1、M2、M3 三条正文。发送由冻结包的移动 API 执行。 |
| 宿主转写 | 在隔离根内按官方返回的会话 ID 找到唯一 `session.v4.jsonl.zstd`，通过 `zstd -dc` 解压，仅统计 `user/message` 事件。M1 序号 8、M2 序号 23、M3 序号 31，各恰好一次。`agent/inbox/spliced` 和标题派生事件也含相同文字，均未计作重复用户消息。 |
| 宿主回合 | `turn/end` 序号为 18、26、34，三个回合均返回 `MISSING_CREDENTIAL`。隔离宿主没有配置可用的模型凭据，附件实读未验。 |
| 控制台 | 同一测试设备的 `dsh-console-entry`、`dsh-console-redeem`、控制台入口 HTML 请求分别返回 200。浏览器中的打开动作未观察到独立标签页，原页草稿仍在。新页渲染、资源和 WS、业务与返回阅读位置未验。 |
| 清理 | 清除独立账户的两条测试配对后记录为 0。同一测试 cookie 查询由 200 变为 403。仅停止了本轮冻结应用及测试宿主进程。 |

## 同钥重启追加核验

隔离宿主重启后，实际宿主 PID 与原授权账本中记录的 PID 不同。启动冻结包 `8cde171d` 后，原会话 `session-send-info` 返回 200 且 `injectable: true`。绑定账本仍为一条记录，绑定 ID 与公钥均与重启对照一致。未发起新模型回合。清理本轮测试配对后，同一 cookie 返回 403。隔离 Desktop 保留窗口供模型配置。
