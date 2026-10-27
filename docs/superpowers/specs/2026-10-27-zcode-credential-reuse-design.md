# ZCode 记住授权与自动恢复连接设计

日期：2026-10-27；基线 `34307def`。需求依据：[PRD](2026-10-27-zcode-credential-reuse-prd.md) R1–R6／AC1–AC9。本文只定实现所需决策，不改官方协议或 MASTER-PLAN。产品代码尚未修改。

## 1. 需求核对与分阶段交付

需求方向一致：方案 A；用户授权一次后记住账号 JWT，授权成功／重启／重新开启远程时自动连接；主动断开与 KICKED 暂停恢复；换链接不清账号；到期／401再授权。

- **核心阶段 C1–C4**：系统存储→授权保存／启动恢复→复用 Hub.connect→设置卡真实显示，形成能运行的完整纵向功能。包括“不落明文、不假报保存、旧401不删新授权、单连接、主动暂停”这些必需底线。
- **加固阶段 H1–H3**：增加有界网络退避，补受控竞态／故障／完整假链测试和平台验收。它们不另造一套连接器；核心阶段结果不能被说成全部验收完成。
- 不重跑 ZCode 协议探测；本批真实保险箱与重启测试在实现之后进行。原有各控制能力的证据门保持原义。

## 2. 复用清单与改动边界

| 现有实现                                                                              | 本批处理                                                              |
| ------------------------------------------------------------------------------------- | --------------------------------------------------------------------- |
| `auth.rs` OAuth、回环callback、JWT exp及30秒窗口、epoch、迟到401保护                  | 原样复用；在唯一落JWT入口之后保存，在启动时恢复；扩展失效后的持久删除 |
| `hub.rs` connect_lock、现有connect、begin/finish_disconnect、关闭epoch、投影失效与SSE | 原样复用；区分用户暂停与运行时清理，新增恢复入口，不另建Hub           |
| `HubBackend`／`PartsBackend::assemble_parts` 测试端口与单实例装配                     | 增补默认方法，老fake无需整批重写；生产显式注入OS存储，测试仍注入fake  |
| `remote/mod.rs::remote_toggle/restore_on_launch`、`lib.rs:209–213` 启动顺序           | 在服务器恢复之后通知同一个Hub启动／唤醒自动连接；不改通道与配对规则   |
| 设置卡现有1.5秒status刷新、授权／连接／断开按钮                                       | 复用刷新和按钮布局，补非敏感授权状态与忘记授权                        |
| settings DAO `try_set_setting_conn`                                                   | 复用可失败写入；补同风格的可失败读取，不用吞错接口读取恢复禁用标记    |
| relay／bridge／command／移动API                                                       | 不改wire、控制动作或能力门；只复用connect与连接事件作为编排输入       |

仅新增 `credential_store.rs`、`auto_connect.rs` 两个生产模块。其他更改是现有组件增量与测试，不拆无关大文件、不建立通用账号框架。

## 3. 凭据存储

### 3.1 一个账号、一个条目

- 正式版service：`com.jarvis.tuvis.zcode`；debug：`com.jarvis.tuvis.dev.zcode`；条目user／target后缀固定 `primary`。条目不按sid/mid存储。
- secret为UTF-8 JSON bytes：`{schemaVersion:1,provider:"bigmodel"|"zai",jwt:"..."}`。仅在Rust内序列化／解析；含JWT的类型不实现Debug，不作为IPC DTO。
- `StoredCredential`只保存OAuth取得的ZCode账号JWT及必要provider，不保存terminal token、OAuth code/state或第三方access_token。
- 一条记录成功替换当前账号；无多账号列表、导入导出或按设备复制凭据。

### 3.2 原生实现与依赖决策

- macOS：`keyring = "=3.6.3"`，`default-features=false, features=["apple-native"]`；`Entry::new(service,"primary")`、`set_secret/get_secret/delete_credential`，使用用户Keychain。
- Linux：相同版本，仅 `sync-secret-service,crypto-rust,vendored`；不用keyutils或默认mock后端。vendored避免新增宿主DBus开发包要求，运行时仍须有Secret Service／会话DBus。
- Windows：复用已有 `windows = "0.62"`，补 `Win32_Security_Credentials` feature；使用 `CredWriteW/ReadW/DeleteW/Free`，`CRED_TYPE_GENERIC + CRED_PERSIST_LOCAL_MACHINE`，二进制blob且≤2,560 bytes。这里的持久范围仍为当前用户、同机；不使用DPAPI机器作用域。读取指针必须RAII释放。
- 选择该组合的原因：已核 `keyring 3.6.3` MSRV=1.75，低于项目1.89；其Windows实现硬编码 `CRED_PERSIST_ENTERPRISE`，故不使用该后端。没有改动该库或引入加密文件。
- keyring的NoEntry：load=None、delete成功；NoStorageAccess：locked_or_denied；PlatformFailure／未知：unavailable；TooLong：too_large；损坏／歧义记录：invalid_record。丢弃原始错误及BadEncoding携带的bytes，不打印。

### 3.3 最少的非敏感偏好

只使用已有settings表，不做schema迁移：

| 键                             | 值与语义                                                                                                                             |
| ------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------ |
| `remote.zcode_auto_connect`    | `true`／`false`；缺键=true，非法值=false。授权成功／显式连接设true；用户断开／KICKED／忘记授权设false；退出／总开关关闭不改它        |
| `remote.zcode_restore_allowed` | `true`／`false`；缺键=true，非法值=false。保存前置false，保险箱写成功后才true；忘记／失效删除前置false，防删除失败后重启又恢复旧条目 |

读取失败必须作为失败处理，不能当缺键。非敏感标记也失败时报告storage_failed，不宣称已完成记住／忘记。禁用恢复不等于已删除，删除失败仍需明确提示。

## 4. 授权生命周期与接口

### 4.1 公共类型（新增）

`CredentialStore: Send + Sync` 是唯一新增存储trait：`load()->Result<Option<StoredCredential>,CredentialError>`、`save(&StoredCredential)->Result<(),CredentialError>`、`delete()->Result<(),CredentialError>`。同步OS操作由AuthService在锁外的单飞blocking任务执行。

`AuthStatus` 为唯一新授权DTO：`{authorized:bool,remembered:bool,restoring:bool,provider:Option<OAuthProvider>,problem:Option<AuthProblem>}`，serde camelCase。`AuthProblem=expired|revoked|locked_or_denied|unavailable|storage_failed|delete_failed|invalid_record|too_large`；不含JWT／原生错误。现有 `ConnectionView` 和手机HTTP合同不变。

### 4.2 AuthService 增量

- 保留 `authorize/terminal_token/clear` 的现有对外签名及端点。
- 新增 builder `with_credential_store(self,Arc<dyn CredentialStore>)->Self`；旧构造与 `with_endpoints` 默认不接OS存储，避免旧测试／wire fake访问真实保险箱；生产工厂必须显式注入。
- 新增 `restore(&self)->AuthStatus`、`status(&self)->AuthStatus`、`forget(&self)->AuthStatus`。restore已有当前代有效内存JWT时直接返回，不因存储写失败又去覆盖本次有效身份。
- `authorize` 成功先沿用commit_jwt，然后保存；保存失败授权本次仍可用，status为authorized=true/remembered=false并附problem，不把存储失败冒充OAuth失败。
- `restore` 仅由启动或用户明确重试触发，不在1.5秒状态轮询里访问保险箱；恢复过期记录走失效清理。`status`只读内存快照。
- `clear`仅取消／清运行时内存；真正忘记与过期／401走持久删除入口。`terminal_token`的迟到401必须先通过已有credential epoch检查才清对应记录。
- 恢复禁用、记录丢失、过期或锁定均不自动打开浏览器。

### 4.3 核心并发底线

复用AuthState epoch；新增一条存储I/O串行通道（`Arc<tokio::sync::Mutex<()>>`的owned guard随blocking任务移动，直到实际OS调用完成）。原生调用不持AuthState或DB锁。操作开始与返回后复核epoch；旧删除不能越过新保存，忘记后的旧保存不能恢复授权。不能因调用方超时／取消就让仍执行中的原生写入脱离串行约束。

忘记先清内存／取消旧epoch并禁用恢复，再排队删除；失败如实回报。新授权成功保存后才重新允许恢复。以上属于核心正确性，不后移到“可选加固”。

## 5. Hub与自动连接

### 5.1 保留核心入口，增加用户意图入口

- 现有 `connect()` 保持连接内核；自动路径调用它，不修改用户意图。
- 新增 `connect_requested(&self)->ZResult<ConnectionView>`：保存auto=true；必要时重试restore，成功后调用原connect。
- 新增 `disconnect_requested(&self)->ZResult<ConnectionView>`：先保存auto=false，再调用原disconnect。原 `disconnect/begin/finish` 继续供退出／停服清理使用，不写用户偏好。
- 新增 `restore_authorization(&self)->AuthStatus`、`auth_status(&self)->AuthStatus`、`forget_authorization(&self)->ZResult<ConnectionView>`；HubBackend新增restore/status/forget默认实现，PartsBackend委托AuthService，不暴露JWT。
- `AuthStatus`以AuthService为权威；Hub的authorized是缓存，在恢复／授权／显式连接入口对齐，不能仅凭任意连接错误删除JWT。`config_changed`只断relay、作废投影／动作与推进目标代际，删除PartsBackend.reset里的auth.clear，按真实auth_status恢复authorized；忘记才走单独清凭据路径。保留保存失败不触碰旧连接的规则。
- `authorize`开始时先用既有运行时断开推进Hub目标代际，结束时复核代际，旧restore不得覆盖新授权；成功保存auto=true，唤醒自动连接，不再要求用户第二次点击；总开关关闭时只显示已授权未连接。

### 5.2 一个很小的自动连接任务

新增 `start_auto_connect(self:&Arc<Self>)->()`、`wake_auto_connect(&self)`。只启动一次任务；使用Weak Hub、既有连接事件和Notify，不另造连接状态机。手动／自动共用connect_lock和关闭epoch。

触发：启动恢复结束、授权成功、链接更新、远程总开关开启。前置条件每次现查：remote.enabled=true、auto=true、auth可用、链接已保存、当前不在closing/conflict、没有连接在途。缺少条件就停，不自动OAuth。

阶段C先实现一次恢复／一次连接。阶段H补网络恢复：首次立即尝试，最多3次额外重试，延迟1s／3s／10s；只对offline恢复。paired后重置故障预算，下一次真实掉线开始新一轮；自己产生的offline事件不得无限重置预算。预算耗尽显示离线，显式连接／重新开启远程等新用户触发才重开预算。needs_auth/needs_config/conflict/unknown不自动重试。

KICKED使持久auto=false并停止本轮；不因重启重新抢连接。手动断开、换链接、忘记、远程关闭取消旧轮次；旧定时器不能向新目标发起连接。自动编排只调用connect，不调用execute，不恢复写动作。

启动落点：`lib.rs`安装并manage同一个Hub、执行既有restore_on_launch之后，再非阻塞启动恢复任务；不能在Tauri主线程等待OS保险箱或浏览器。remote_toggle成功开启后只唤醒该任务；关闭沿用原运行时清理并唤醒取消等待。

## 6. UI与文档

设置卡复用原轮询／trailing合并：保留 `zcode_control_status`，新增只读IPC `zcode_control_auth_status()->AuthStatus`；并行读取后组合展示“正在恢复授权／已授权未连接／已记住／本次可用但未记住／需要重新授权／存储不可访问”。不扩充ConnectionState枚举。

沿用授权／连接／断开命令名，connect/disconnect内部改调用户意图入口；新增 `zcode_control_forget_authorization()->ConnectionView`。桌面API与AuthStatus TS类型放现有 `src/lib/api/zcodeRemote.ts`，不往手机纯fetch模块引入Tauri依赖。新增文案zh/en同步。

旧版仅内存授权没有可迁移持久记录，升级首次需授权一次，此后再复用；明确写入手册／变更说明。历史探测记录不重写，只在统一spec／合同追加本批持久化裁决并标注新设计生效范围。

## 7. 验收与回滚

核心以fake存储＋已有fake后端证明：授权一次→自动paired→销毁实例→恢复paired，OAuth次数不增加；换链接不增加OAuth；手动暂停／远程关闭／失效／忘记符合PRD。随后补完整loopback假链及双构建门禁。

平台验收使用专用测试service条目并显式清理，不在普通单测里调真实保险箱。macOS验签名升级／锁定，Windows验同机当前用户和2,560字节边界，Linux验服务缺失／锁定。平台未验不标spec_complete。

关闭auto偏好即可停止恢复尝试，不需要回滚relay协议；忘记授权删除本批条目。无自定义密钥、文件加密或新账号管理服务。

## 依据

- [keyring 3.6.3 API及feature](https://docs.rs/keyring/3.6.3/keyring/)
- [Entry二进制接口](https://docs.rs/keyring/3.6.3/keyring/struct.Entry.html)
- [keyring 3.6.3 Windows源码](https://github.com/hwchen/keyring-rs/blob/v3.6.3/src/windows.rs)
- 现有AuthService／Hub／remote装配，具体复用位置见配套执行计划。
