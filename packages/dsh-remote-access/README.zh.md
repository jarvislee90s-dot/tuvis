# @tuvis/dsh-remote-access

兔维斯（Tuvis）接入 DeepSeek Harness（DSH）Desktop 远控的外部插件 bundle。
单个包、两张面孔：

- **宿主半区**（`lib/index.js`，`exports["."]`）：装进 DSH `desktop` profile 的
  Cordis 插件。注入宿主服务 `connection` 与 `webServer`，在
  `/tuvis-remote/health` 挂一个健康路由；`apply` 返回 disposer（Cordis effect），
  卸载时注册面清零，不留悬挂路由。
- **客户端半区**（`lib/client.js`，`exports["./client"]`）：本机授权卡。以
  **经典脚本**下发给浏览器，并自带官方信封自注册：
  `window.__ModuleLoader__.load({ id: "@tuvis/dsh-remote-access", factory })`。
  产物**没有**顶层 `import`/`export`（宿主用 `<script>` 注入，ESM 顶层在那里
  就是 SyntaxError），React 经 loader 的 `require("react")` 取宿主种子模块
  （不打包自带的副本），挂载面只有 slot 注册表 `settings.section`——先用
  `slots.spec` 探座位（设置页没打开时座位可能不存在）。卡片只显示绑定态与
  两端核对码，绝不显示任何凭据材料。

不做 `/remote` 聊天路由，不带完整 WebUI。

## 安装形态

发行产物是**预构建的 tgz**（仓库内 `resources/dsh/remote-access.tgz` +
`resources/dsh/manifest.json`），由兔维斯桌面端通过
`dsh plugin --profile desktop add <tgz>` 安装。**用户机器不需要 Node、
不需要网络、不需要源码构建**：

- tgz 内没有任何 `node_modules`（运行时零依赖；Cordis / DSH / React 全部
  由宿主以 peer 身份提供）；
- 打包清单剥掉 `scripts`，绝无 install 钩子；
- `manifest.json = { pluginVersion, sha256, bindingProtocol: 1 }`，
  安装方先流式校验 tgz 的 SHA-256 再落盘。

## 包契约（`dsh` 字段）

```jsonc
{
  "dsh": {
    "engines": { "dsh": ">=0.2.1-rc.3" }, // 只声明下界，不做精确版本豁免
    "bundle": { "patch": "./cordis.patch.yml" },
    "client": { "inject": ["@deepseek-ai/dsh-client-connection"], "platform": "web" },
  },
}
```

## 运行时行为（宿主半区）

- **访问边界**：自注册路由**不继承**官方 `/api` 认证与 Host/Origin 围栏
  （宿主实测事实），因此每条路由都自带门禁：Host 主机名须为回环字面量
  （端口不参与判定）、Origin 存在时须与 Host 逐字同期、`sec-fetch-site:
  cross-site` 拒绝；拒绝为 403 + 固定短原因。路由只有 `/tuvis-remote/health`
  与 `/tuvis-remote/binding-state` 两条**只读**路由——插件侧不存在任何
  approve/grant/revoke 写入口，批准只在兔维斯设置卡（Tauri IPC）。
- **绑定会话**：整个生命周期**最多一条** IPC 连接。待批期间连接保留、核对码
  不随重连翻动（Rust 按客户端公钥幂等是后盾）；批准后在同一条连接上兑换并
  交付，随后跑读循环消费 `granted`/`revoked`。撤权是**永久**语义，由推送的
  `revoked` 帧、或下一次重连握手被 `DSH_PEER_REVOKED` 拒绝来表达（撤过的键
  永久拒绝）→ 进撤权态、停止交付、**不再重连**（重新绑定须显式换新钥）。
  `DSH_AUTH_NOT_APPROVED` 是**可恢复**语义（从未批准 / 待批记录过期 / 并发
  替换）：退回待批 + 退避重连，**不写**撤权墓碑。
- **数据根**：`TUVIS_DSH_STATE_DIR` **就是**状态目录（与 Rust 同义），未设则
  `~/.tuvis/dsh`。插件**不读** `TUVIS_HOME`。
- **Cookie 兑换**：`connection.authenticatedUrl(origin)` → `GET
  /connect?token=…&interactions=answer`（`redirect: "manual"`，只在 302 上取
  `set-cookie` 里的 `dsh-auth-*` 完整 `name=value`）。任何偏离抛带**阶段名**的
  类型化错误；token 与 cookie 绝不进错误消息或日志。

## 开发

```bash
pnpm --filter @tuvis/dsh-remote-access build   # tsc：src/ → lib/ 且 src/client/（经典脚本）→ lib/client.js
pnpm --filter @tuvis/dsh-remote-access test    # 先 build 再 vitest（入口测试跑真实产物文件）
node scripts/build-dsh-plugin.mjs              # 确定性打包 → resources/dsh/
node scripts/build-dsh-plugin.mjs --check      # 重建到临时目录并与已提交产物逐字节比对
```

客户端半区由 `tsconfig.client.json` 以 `module: "preserve"` 编译，输出保持
纯经典脚本：无模块包装、无 `export`、`require` 是运行期由宿主 loader 解析的
自由标识符。`test/client-entry.test.ts` 在 `node:vm` 沙箱里执行**构建产物**
`lib/client.js`（沙箱的 `window.__ModuleLoader__` 是录制型假件），以此验证信封
与 slot 挂载——**不**用测试工厂代替入口验收（审查对 R03 的硬要求）。

### 确定性打包

同输入必出同字节（见 `scripts/build-dsh-plugin.mjs` 头注释）：tar 条目按路径
排序、全部 mtime=epoch0、uid/gid=0；gzip 不写时间戳；包内 package.json 按
固定键序白名单重建（剥 `scripts`/`devDependencies`）；归档内只有相对路径。

### 测试基线

基线 DSH 为 `0.2.1-rc.3`：devDependencies 精确锁定 `0.2.1-rc.3`，
peerDependencies 只声明 `>=0.2.1-rc.3` 下界，配合宿主动态契约准入，
不使用精确版本豁免（`allow-version`）。

## License

MIT（见 `LICENSE`；`NOTICE` 说明第三方材料情况）。
