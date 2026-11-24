# DSH 插件开发与一键管理规范核对

核对日期：2026-10-29。用于执行计划，不是新运行实测。

## 官方与社区参考

- [官方贡献说明](https://github.com/deepseek-ai/deepseek-harness/blob/master/CONTRIBUTING.md)：生态插件可独立开发，公开仓库可加 `dsh-plugin` topic；不是要求往官方 monorepo 提 PR。
- [官方包开发指南](https://github.com/deepseek-ai/deepseek-harness/blob/master/docs/cookbook/adding-a-package.md)：插件采用 name/inject/apply/Config、ESM 导出和声明文件；Cordis peer 与开发依赖一致；需要客户端部分时声明 client 导出。其 private、与根版本相同等为官方内部 workspace 门禁，不机械施加到兔维斯外部 bundle。
- [官方 Plugin Manager](https://github.com/deepseek-ai/deepseek-harness/blob/master/packages/boot/plugin-manager/README.md)：通过管理器安装/移除/启停，保存选择与实际运行是不同状态；无 HMR 时需重启；保留原生锁和版本兼容检查，避免手改用户文件。
- [官方 CLI](https://github.com/deepseek-ai/deepseek-harness/blob/master/apps/cli/README.md)：desktop profile 归 Electron 所有；普通 npm CLI 不能管理该 profile。
- [官方 Desktop 的 bundled command runtime](https://github.com/deepseek-ai/deepseek-harness/blob/master/apps/desktop/README.md#bundled-command-runtime)：先启动一次初始化、完全退出，再以该 Desktop 自带 CLI 执行 `plugin --profile desktop add/list/remove`；重开应用加载变更。
- 本机社区 `dsh-remote-web-ui@0.4.11` 的 package.json、cordis.patch.yml、inner-auth.ts 和 effect/route 注册是可参考的外部 bundle 形态；不把它的转发、全权配对模型或浏览器补丁当兔维斯必须照抄的要求。

以上链接为核对日 master，可能变化；实施 T01 必须保存源修订、关键摘录及本机 0.2.1-rc.3 包的对应摘要，分清规范新变化与实际安装包契约。

## 本计划据此采用

独立外部包 `@tuvis/dsh-remote-access`（计划内本地包名，不代表 npm 命名空间已申请/包已发布）；预构建 bundle 随兔维斯安装版嵌入。首版不用网络拉 latest，不带 install/postinstall 脚本，不携带模型/API 密钥或多余隧道依赖。

首次安装不依赖尚未获得的 owner 凭据：走正确 Desktop 安装自带的 CLI，遵守完全退出、初始化和 profile 写锁。后续已授权的在线管理只有在实装契约明确支持时才可优化。卸载不得把其他配置/插件回灌为旧文件快照，敏感授权状态按专门撤权策略处理。

现有仓库 `services/plugin/mod.rs` 为通用资源插件管理，不具备 DSH profile 包管理语义，不直接复用为此功能；借鉴 `services/tps/install.rs` 的资源嵌入、预览指纹、自有项所有权与用户改动保护，但不改采集域。

## 隔离平台补探测留存

- **插件管理锁。** 官方 `0.2.1-rc.3` 的 Desktop 启动恢复锁为 `<profile>/lock`，插件 CLI 与应用内包管理共用 `<profile>/package.json.lock`。Mac 和 Windows 的影子 profile 实测都可接管死 PID 的写锁，活 PID 则等待；Windows 的 CLI 在启动恢复锁被持有时仍可运行。应用继续核验实际宿主退出。`--profile` 使用 `DSH_HOME/profiles` 下的名称，Desktop profile 由自带 CLI 管理。
- **macOS UDS。** 独立子进程探针中 Tokio `peer_cred()` 返回的 PID 与子进程一致，监听路径长度上限为 103 字节。旧探针中只设置关闭标志的读仍挂起，对整 socket 调用 shutdown 可解除读并让对端收到 EOF。裸结构体探针读出的异常 UID 未采信。
- **Windows 管道。** 隔离 Node 客户端连通，内核识别的 PID 和进程启动时间与独立观测一致。客户端关闭后读返回 EOF，`IsConnected` 仍可为真。PowerShell 5.1 的空闲 `ReadAsync` 未随取消令牌完成，释放服务端管道后返回 0。远程客户端与允许 Everyone 的对照都超时，远程 DACL 的实际拒绝仍需受控双机验证。
- **官方 Cookie。** 旧版官方隔离探测确认 API Peer 与浏览器会话为不同凭据，随宿主换代失效。正式分支的安装、撤权及四渠道结果见[现行验收矩阵](../../acceptance/2026-11-06-dsh-hybrid-h14-matrix.md)。
