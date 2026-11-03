# Foxbell 桌宠移植设计（MAM 独立窗口版）

- 日期：2026-09-01
- 状态：待评审
- 来源：DeepSeek Harness 插件 `dsh-foxbell-pet` v1.3.0 完整移植
- 原始仓库：github.com/jarvislee90s-dot/dsh-foxbell-pet（本地 `/Users/jarvis/Documents/DeepSeek/DeepSeek-plugins/dsh-foxbell-pet`，与 origin/main 一致）

## 1. 背景与目标

把 DSH 网页内的 Foxbell 小狐狸桌宠（精灵动画 + 拖拽物理 + 多项目状态卡片 + 语音提醒）完整移植到 MultiAgents-Manager（Tauri 2 桌面应用），并升级为**独立悬浮窗口**形态。除"红灯报错/断联"（MAM 无数据源，见 §15）外，原版全部 48 条交互行为 1:1 或等价移植。

**成功标准**：逐条对照 §9 交互清单验收通过；宠物开启时完成提示音由宠物语音接管；置顶时通知浮窗抑制。

## 2. 决策记录（已与用户确认）

| # | 决策项 | 结论 |
|---|---|---|
| D1 | 形态 | 独立透明窗口，可悬浮在所有程序前 |
| D2 | 完成语义 | 严格按 MAM 颜色：变绿→done 组语音+绿卡；变红(waiting)→approval 组语音+红卡（10s 限频）。与看板及 PR #28 语义一致 |
| D3 | 声音接管 | 宠物开启即接管完成提示音；宠物声音开关关闭则静默，不回落 MAM 音效 |
| D4 | 浮窗策略 | 宠物置顶 → 通知浮窗不弹（头顶状态栏常显）；宠物非置顶 → 浮窗照弹。以置顶开关为判断条件（macOS 无法可靠检测真实遮挡） |
| D5 | 字幕开关 | 保留右键菜单；字幕与出声两条独立线（muted 只拦声音、talkative 只拦字幕；实现取独立线读法，与原版 muted 连带吞字幕略有差异，见 §17.3-2） |
| D6 | 自定义素材 | v1 内置打包，不做外部素材目录 |
| D7 | 重新打开入口 | 主窗口状态摘要栏 🦊 快捷按钮 + 托盘菜单"显示/隐藏桌宠" |
| D8 | 看板设置分区 | 开启/关闭开关 + 悬浮最前开关 |
| D9 | 红灯报错/断联 | 跳过；`errorAction` 配置项与 error 语音素材保留占位（无触发场景） |
| D10 | 窗口结构 | 方案 A：单窗口 + 动态尺寸（卡片为窗口内独立 DOM 层，按实际数量伸缩，窗口底部锚定） |
| D11 | 点击穿透 | macOS `setIgnoreCursorEvents(true, { forward: true })` + 悬停命中切换（§4.4） |
| D12 | 跳转歧义 | 多候选时在宠物旁弹出迷你候选浮层，点选聚焦 |
| D13 | 平台范围 | v1 仅 macOS 完整支持 |
| D14 | 动作场景重映射 | 原蓝灯(done)→🟢绿灯、原黄灯(approval)→🔴红灯(waiting)；error 子页从菜单移除（配置键保留占位）。菜单显示三个绑定场景：双击 / 红灯 / 绿灯；运行中（黄灯）沿用原版固定工作姿态，不可绑定 |
| D15 | 桌宠缩放 | 三档预设：小 0.75 / 中 1.0 / 大 1.25；精灵、字幕气泡、卡片等比例联动；右键菜单 + 看板设置两处入口。Codex pet 官方无缩放标准（仅固定图集契约 8×9、192×208/帧），其社区正请求 S/M/L 预设（openai/codex#21864），本设计与该提案对齐 |
| D16 | 卡片尺寸模型 | 多卡等宽；每行单行省略号截断，卡片高度不随文字长度变化、仅随行数（标题+最多 2 行）微变；窗口尺寸按内容实测（ResizeObserver），随卡片数 / 每卡行数 / 缩放档位统一伸缩 |

## 3. 总体架构

**前端驱动 + 复用现有数据管道**（对比过"Rust 后端状态机推送"方案，因状态差分逻辑需在 Rust 重写一遍而否决）：

```
宠物窗口 (#/pet 路由，独立 webview)
  ├─ useSessionsQuery（自己 3s 轮询，与主窗口互不依赖）
  ├─ petStatus 差分推导（纯函数）
  ├─ petVoices 语音播放 / petAnimations 动画引擎
  └─ usePetWindow 窗口控制（显隐/置顶/穿透/位置/尺寸/物理）

主窗口 (home + settings + useNotification)
  ├─ 🦊 快捷按钮 / 设置页"桌宠"分区
  └─ useNotification：声音接管 + 浮窗抑制判定

Rust 侧（仅窗口管理，无业务逻辑）
  ├─ commands/pet.rs：ensure_pet_window / set_pet_visible / set_pet_always_on_top
  └─ plugins/system_tray.rs：托盘菜单项
```

- 配置主存储为 localStorage（同 origin 跨窗口共享，storage 事件同步），键见 §10。
- 宠物窗口隐藏时 webview `visibilityState=hidden`，React Query 轮询自动暂停（`refetchIntervalInBackground: false`），不耗资源。

## 4. 宠物窗口设计

### 4.1 创建与生命周期

- Rust `setup` 阶段创建 `label: "pet"` 窗口，参数：`transparent: true, decorations: false, shadow: false, always_on_top: <配置>, skip_taskbar: true, resizable: false, visible: false, focus: false`。`tauri.conf.json` 已开 `macOSPrivateApi`。
- 宠物页面（`#/pet`）加载后读 localStorage 显隐配置，自行 `show()`；保证启动时窗口不闪现。
- "关闭"= 隐藏窗口（webview 保活，恢复即时）；应用退出随进程销毁。

### 4.2 动态尺寸（D10/D15/D16）

尺寸模型 = **统一缩放乘子 + 内容实测**：

**缩放**：配置 `scale` 三档（小 0.75 / 中 1.0 / 大 1.25）。所有视觉常量由单一乘子派生 `px(v) = v × scale`——精灵 192×208×s（`background-size` 与帧偏移同步 ×s）、字幕气泡字号 13×s 与内边距 ×s、卡片宽 320×s、卡片字号 12×s、卡片间隙 5×s、卡片与精灵间隙 10×s、底部气泡区 50×s。调整精灵大小时，气泡与卡片**等比例联动**，观感比例与原版 1.0 档完全一致。

**卡片尺寸规则**（沿用原版）：多卡**等宽**（列宽 = 320×s）；每卡 = 加粗标题行 + 最多 2 行摘要，**每行单行省略号截断**——卡片高度不随文字长度增长，仅随行数（1–3 行文本）微变。

**窗口尺寸 = 内容实测**：ResizeObserver 监听内容根元素实际渲染尺寸，高度 = 底部气泡区 + 精灵 + 间隙 + Σ 各卡实际高度，宽度 = max(精灵宽, 卡片宽) + 边距。卡片增减、每卡行数变化、缩放档位变化统一走同一条 resize 路径（防抖 50ms → `setSize` + 底部锚定 `setPosition`：`newY = oldY + oldH − newH`，精灵不动的锚点稳定）。

**上限保护**：窗口总高不超过 `workArea` 高度 − 余量；超出时卡片层内部裁剪（缩放档位越大可见张数越少），"+N 更多"徽章兜底，沿用原版最多显示 6 张规则。

**右键菜单**：打开时窗口临时扩展至菜单实际高度，关闭还原（窗口全透明，伸缩不可见）。

### 4.3 位置记忆

- localStorage 记窗口 x、y（全量，不限原版"只记 x"）。
- 恢复时夹紧到当前 `workArea` 范围内（显示器变更/分辨率变化的兜底）。

### 4.4 点击穿透（D11）

- 默认 `setIgnoreCursorEvents(true, { forward: true })`（macOS 专属：拦截放行的同时把 mousemove 转发给 webview）。
- 前端监听转发来的 mousemove，对**交互实体**做 `getBoundingClientRect` 命中测试（精灵、卡片、菜单；字幕气泡 pointer-events:none 不参与）：
  - 命中 → `setIgnoreCursorEvents(false)`（进入交互模式）
  - 离开全部实体 → 恢复 `(true, { forward: true })`
- 拖拽进行中保持交互模式，松手结算后恢复。
- 效果：整个屏幕只有宠物身体（及展开的卡片/菜单）是"实心"可交互区，其余穿透。
- **风险项**：forward 转发的 mousemove 可靠性需原型验证；不可行时降级为"精灵区常驻接收 + 其余区域穿透"的静态划分。

### 4.5 置顶

- `setAlwaysOnTop(bool)` 动态切换，看板设置与右键菜单两处入口读写同一配置（§10）。
- 切换即时生效，无需重建窗口。

## 5. 状态推导与灯色映射

数据源：宠物窗口自己的 `useSessionsQuery`（3s）。灯色**严格对齐 MAM 语义**（D2）：

| 宠物灯 | MAM 状态 | 卡片 | 动画 | 语音 |
|---|---|---|---|---|
| 🔴 红 | `waiting` | 红卡"等待操作" + 摘要 | waiting 姿态 | approval 组，**10s 限频**，仅差分触发（此前无 waiting → 有） |
| 🟡 黄 | `processing` / `thinking` / `compacting` | 黄卡运行摘要 | running 工作姿态（任一会话运行即触发，MAM 无"当前会话"概念） | 不播 |
| 🟢 绿（未读） | 差分：非绿 → `idle/finished` | 绿卡"已完成" | `doneAction`（默认跳跃） | done 组 |

规则（沿用原版差分语义）：

- **卡片是状态展示，语音/未读是事件差分**——两个概念分离。首帧加载：卡片按当前状态直接显示，但**不触发任何语音与未读标记**（比原版首轮更保守，避免启动噪音）。
- **绿卡未读即消**：点卡片跳转（`focus_session`）后卡片消失；该会话再次"非绿→绿"时重新亮起（差分代数天然支持）。
- 红卡不消（waiting 是持续状态，状态解除自然变绿/黄）。
- 会话从列表消失：卡片 60s 后清理（沿用原版 H4）；**不做断联红灯**（D9）。
- 卡片排序：waiting(0) > running(1) > done(2)，组内按标题 zh locale 排序。
- 卡片内容：`title ?? projectName`（加粗）+ 最新 2 行摘要（`lastMessage` 截断，等宽多行，样式沿用原版暖棕配色）。

## 6. 语音系统与通知联动

### 6.1 素材

- 一次性搬运：`public/pet/spritesheet.webp`（2.5MB，Codex V2 图集 8 列×11 行，每帧 192×208）、`public/pet/voice/{general,approval,done,error}/*.m4a`（31 条）、`public/pet/manifest.json`（构建期 `scripts/copy-pet-assets.mjs` 生成：`[{index, group, name, file}]`，浏览器无法列目录必须有清单）。
- 字幕 = 语音文件名（原版语义，中文台词原样）。

**语音组 ↔ 场景映射表**（文件夹即状态分组，触发语义与灯色对齐 D2/D14）：

| 素材文件夹 | 触发场景（MAM） | 灯色 | 动作配置键 |
|---|---|---|---|
| `general/`（11 条） | 双击形象闲聊 | — | `dblAction` |
| `approval/`（6 条） | waiting 差分出现（原"待批准"，撒娇催促，10s 限频） | 🔴 红 | `approvalAction` |
| `done/`（7 条） | 非绿→绿 差分（原"完成"，求夸/元气） | 🟢 绿 | `doneAction` |
| `error/`（7 条） | **占位保留，v1 无触发场景**（原"报错"，MAM 无数据源 D9） | — | `errorAction`（占位） |

运行中（黄灯）不播语音（原版绿灯同样不播）。

### 6.2 播放机制（1:1 复刻原版）

1. 首次数据同步后每条语音一个 `Audio(preload=auto)` 元素预加载，播放即时出声；
2. 组内随机、不连续重复同一条；空组静默跳过；general 空 → 回退全部语音池；
3. 字幕时长 = max(2500ms, 音频时长 + 250ms)，`loadedmetadata` 对齐，超时兜底 3.3s；
4. `muted` 只拦发声不拦动作（静音≠静止）；`talkative=false` 语音照播无字幕；
5. **WKWebView 自动播放解锁**：首次 pointerdown 在用户手势内 muted 试播解锁；被拦截则标记 blocked，下次点击重试。

### 6.3 完成提示音接管（用户核心需求）

主窗口 `useNotification` 变绿时：

```
宠物关闭        → playCompletionSound 照旧
宠物开启 + 出声  → 跳过 playCompletionSound，done 语音由宠物窗口自行差分播放
宠物开启 + 静音  → 完全静默（D3）
```

判定读 localStorage 实时值（跨窗口同 origin 共享，无竞态）。

### 6.4 通知浮窗联动（D4）

主窗口 `useNotification` 发浮窗前判定：

```
宠物开启 + 置顶   → 不调 show_notification_window（头顶状态栏常显）
宠物开启 + 非置顶 → 浮窗照弹（宠物可能被遮挡）
宠物关闭          → 浮窗照弹
```

通知历史（`addHistory`）与系统 toast 降级路径不受影响，照旧记录。

## 7. 动画引擎（1:1 复刻）

- `ANIM` 逐帧时长表：9 行动画（idle / run-right / run-left / waving / jumping / failed / waiting / running / review），各帧独立 ms 时长、末帧加长停顿（如 idle 280/110/110/140/140/320）；
- look 环顾：空闲 6s 后行 9→10 连续 16 帧顺时针扫视（250ms/帧），播完再等 6s，任何交互打断；
- JS setTimeout 链帧步进 + CSS `background-position` 驱动精灵图（`background-size: 1536px 2288px`）；
- 状态机优先级：**拖拽 > 瞬时事件 > 任务态 > look > idle**，切换动画重启帧步进；
- 代数计数器（transient / bubble / speech 三组 generation）防过期定时器覆盖新状态；
- 拖拽方向动画：上拖(dy<−8)→跳跃、左拖(dx<−6)→向左跑、右拖(dx>6)→向右跑。

## 8. 拖拽与物理（窗口级等价移植）

原版移动 DOM 元素 → MAM 版**移动窗口本身**，参数与手感 1:1：

- **拖拽**：pointermove 内逐帧 `setPosition`（不用系统 `startDragging`，保留方向动画与速度采样）；
- **松手物理**（gravity 开 + 移动过 + 150ms 采样窗内 ≥2 采样）：
  - 重力坠落 `GRAVITY=1400px/s²`、水平抛掷惯性（采样窗末速为初速）、空气阻尼 `DAMP=0.86^(dt·60)`；
  - 落地 = 窗口底缘到达 `workArea.bottom`（地面取工作区，避开 Dock/菜单栏）；
  - 落地压扁回弹：`scaleY(1→0.55, 60ms)` → 弹性回弹（`cubic-bezier(.34,1.56,.64,1)`, 240ms，窗口内 DOM transform）→ 补一段跳跃 1500ms；
  - `|vx|<24` 停止并记忆窗口位置；
- **非物理松手**（gravity 关或未移动）：停在原地 + 记忆位置；
- rAF 驱动 `setPosition`（LogicalPosition，Tauri 自动 DPI 换算）；若 60fps IPC 性能不足，预案降到 30fps（视觉差异微小）。

## 9. 交互逻辑完整清单（48 条，验收基准）

### A. 指针交互（6 条）

| # | 交互 | 移植方式 |
|---|---|---|
| A1 | 单击形象：只挥手 1700ms 不出声 | 1:1 |
| A2 | 双击形象：general 组随机语音 + `dblAction` + 字幕 | 1:1 |
| A3 | 拖拽：方向动画（上跳/左跑/右跑） | 1:1（窗口移动） |
| A4 | 松手物理：坠落/惯性/压扁回弹/补跳 | 等价（§8） |
| A5 | 非物理松手：即停 + 记忆 | 等价（§8） |
| A6 | 右键：弹菜单，定位夹紧视口内 | 1:1（菜单在宠物窗口内渲染，窗口临时扩展至菜单高度，菜单定位在窗口内夹紧） |

### B. 右键菜单（11 条）

| # | 交互 | 移植方式 |
|---|---|---|
| B1 | 🔊 出声开关（=自身设置 2a） | 1:1 |
| B2 | 💬 语音字幕开关 | 1:1（D5，独立于出声） |
| B3 | 🧲 物理坠落开关 | 1:1 |
| B4-B7 | 动作绑定子页：🖱️ 双击动作 / 🔴 红灯动作（等待操作）/ 🟢 绿灯动作（完成），6 动作可选（跳/挥手/委屈/等待/审查/工作），进入即循环预览、点选即时切换、返回停止 | 等价（D14 语义重映射：原蓝灯 done→绿灯、原黄灯 approval→红灯；error 场景无数据源，子页移除、配置键保留占位） |
| B8 | 🦊 隐藏桌宠 | 1:1（=看板开关关闭，同一配置） |
| B9 | ℹ️ 关于 | 1:1（显示 MAM 桌宠版本） |
| B10 | 菜单外点击 / Esc 关闭 | 1:1 |
| B11 | 菜单分组：开关区/动作绑定区/隐藏·关于区，分隔线隔开 | 1:1 |

### C. 项目卡片（4 条）

| # | 交互 | 移植方式 |
|---|---|---|
| C1 | 点卡片：跳转会话 + 标记已读，不发声 | 等价（`sessions.open`→`focus_session`） |
| C2 | 已读即消（原"当前会话自动 ack"） | 等价（改为点击即 ack） |
| C3 | 卡片布局：头顶最多 6 张 + "+N 更多"，等宽多行（加粗标题+状态灯+2 行摘要），状态排序 | 1:1（排序去 error 位：waiting>running>done） |
| C4 | 未读卡片消后，新事件重新亮起 | 1:1（差分代数） |

### D. 状态驱动（6 条）

| # | 触发 | 移植方式 |
|---|---|---|
| D1 | 完成：done 组语音 + `doneAction` + 字幕 | 等价（completions 队列→前端"非绿→绿"差分） |
| D2 | 新报错出现：error 组语音/动作 | **跳过**（D9，无数据源） |
| D3 | 待批准出现：approval 组语音，10s 限频 | 等价（waiting 差分触发） |
| D4 | 持续任务姿态：waiting > review > running 优先级 | 等价（"当前会话运行"→"任一会话运行"） |
| D5 | 断联红灯 | **跳过**（D9） |
| D6 | 运行中不发声 | 1:1 |

### E. 语音系统（8 条）

| # | 行为 | 移植方式 |
|---|---|---|
| E1 | 每条语音独立预载 Audio 元素 | 1:1 |
| E2 | 优先预载元素播放，暂停其它在播 | 1:1 |
| E3 | 组内随机不连续重复 | 1:1 |
| E4 | 字幕时长对齐音频 | 1:1 |
| E5 | 空组静默跳过；general 空回退全池 | 1:1 |
| E6 | 自动播放手势解锁 + blocked 重试 | 1:1 |
| E7 | 双击 general 空回退 | 1:1 |
| E8 | 字幕气泡样式（精灵下方居中，白底棕字圆角） | 1:1 |

### F. 动画引擎（4 条）

| # | 行为 | 移植方式 |
|---|---|---|
| F1 | 逐帧时长表 + 末帧停顿 | 1:1 |
| F2 | 空闲 6s 环顾一圈，可被打断 | 1:1 |
| F3 | 状态机优先级 | 1:1 |
| F4 | 三组代数计数器 | 1:1 |

### G. 显隐与持久化（4 条）

| # | 行为 | 移植方式 |
|---|---|---|
| G1 | 显隐开关持久化 | 1:1（入口：右键隐藏/看板设置/🦊按钮/托盘） |
| G2 | 位置记忆 | 等价（窗口 x,y 全量记忆，workArea 夹紧恢复） |
| G3 | 配置持久化 | 等价（localStorage 单后端，跨窗口 storage 事件同步） |
| G4 | 设置卡片与右键菜单同一份配置双向同步 | 等价（看板设置分区 + 右键菜单，同一 localStorage） |

### H. 数据底座（5 条）

| # | 行为 | 移植方式 |
|---|---|---|
| H1 | 轮询 | 等价（1.5s HTTP→3s `useSessionsQuery`，隐藏自动暂停） |
| H2 | 完成事件 seq 去重 | 等价（前端差分 + 代数计数） |
| H3 | 卡片摘要（标题+2 行） | 等价（`title??projectName` + `lastMessage` 截断） |
| H4 | 消失会话 60s 清理 | 1:1 |
| H5 | 状态排序 | 等价（§5） |

**统计：44 条 1:1 或等价移植 + 2 条跳过（D2/D5）+ error 占位（B6/D9）+ 2 条新增能力（置顶开关、通知联动）。**

## 10. 配置系统与入口

### 10.1 配置项（localStorage）

主键 `mam-pet-config`（JSON）：

| 键 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `alwaysOnTop` | bool | `true` | 悬浮最前（看板设置 1b 与右键菜单 2c 两处入口同一配置） |
| `muted` | bool | `false` | 出声（自身设置 2a） |
| `talkative` | bool | `true` | 语音字幕 |
| `gravity` | bool | `true` | 物理坠落（自身设置 2b） |
| `scale` | 0.75 / 1.0 / 1.25 | `1.0` | 桌宠大小三档（小/中/大），精灵/气泡/卡片等比例联动（D15） |
| `dblAction` | Action | `waving` | 双击动作 |
| `approvalAction` | Action | `waiting` | 红灯（waiting）动作 |
| `errorAction` | Action | `failed` | 占位保留（不在菜单显示，无触发场景，D14） |
| `doneAction` | Action | `jumping` | 绿灯（完成）动作 |

Action 枚举：`jumping` 跳一跳 / `waving` 挥挥手 / `failed` 委屈 / `waiting` 等待 / `review` 审查 / `running` 工作。

显隐为独立键 `mam-pet-visible`（bool，默认 `false`，不并入主配置键，避免双写打架）；窗口位置独立键 `mam-pet-position`（`{x,y}`）。sanitize 规则沿用原版（bool 强转、Action 白名单回落默认）。

### 10.2 入口与同步

| 入口 | 控制 | 实现 |
|---|---|---|
| 看板设置页"桌宠"分区 | visible + alwaysOnTop + scale（大小三档） | React 组件读写 localStorage + invoke `set_pet_visible` / `set_pet_always_on_top` |
| 宠物右键菜单 | muted / talkative / gravity / alwaysOnTop / 大小三档 / 三动作绑定（双击·红灯·绿灯）/ 隐藏 / 关于 | 宠物窗口内直接读写 + invoke |
| 主窗口 🦊 快捷按钮 | visible | 同设置页开关（图标灰化表示隐藏态，沿用原版） |
| 托盘菜单"显示/隐藏桌宠" | visible | Rust 侧直接控制窗口 + emit 事件同步前端 localStorage |

同步机制：所有入口写 localStorage 后广播 Tauri event `pet-config-changed`，各窗口监听刷新 UI；托盘由 Rust emit。窗口显隐/置顶的实际动作走 Rust command（窗口隐藏时前端事件仍可达，webview 保活）。

### 10.3 i18n

菜单与设置文案走 i18next（中英）；语音字幕为文件名（中文台词）原样显示；动作标签中英对照。

## 11. 跳转与歧义候选（D12）

点卡片 → `focus_session`（宠物窗口 invoke）：

- 返回唯一窗口 → 直接聚焦 + ack；
- 返回 `ambiguous` → 在宠物旁弹出迷你候选浮层（列窗口标题 + 进程名，样式类右键菜单），点选一条 → `focus_hwnd` 聚焦 + ack；Esc/点外关闭（不 ack，卡片保留）。

## 12. 文件结构

```
src/pages/pet.tsx                    — #/pet 宠物窗口路由页（main.tsx hash 分流）
src/components/pet/
  FoxbellPet.tsx                     — 本体（状态机/交互/卡片/气泡）
  PetMenu.tsx                        — 右键菜单 + 动作子页 + 候选浮层
  petAnimations.ts                   — ANIM 表 + LOOK_FRAMES + 帧步进
  petVoices.ts                       — manifest 加载/预载/播放/字幕
  petConfig.ts                       — 配置读写 + sanitize + 事件同步
  petStatus.ts                       — 六态→灯色/事件差分（纯函数，单测）
  usePetWindow.ts                    — 显隐/置顶/穿透/位置/尺寸/物理
src/hooks/useNotification.ts         — 声音接管 + 浮窗抑制判定（修改）
src/pages/home.tsx                   — 🦊 快捷按钮（修改）
src/pages/settings.tsx               — "桌宠"分区（修改）
src-tauri/src/commands/pet.rs        — 窗口管理 command
src-tauri/src/plugins/system_tray.rs — 托盘菜单项（修改）
src-tauri/src/lib.rs                 — command 注册（修改）
public/pet/                          — spritesheet.webp + voice/* + manifest.json
scripts/copy-pet-assets.mjs          — 一次性素材搬运 + manifest 生成
```

## 13. 错误处理

- manifest 拉取失败 → 语音能力静默降级（动画/卡片照常，空组跳过语义复刻）；
- `focus_session` 失败 → toast 提示（sonner），卡片保留；
- 穿透切换失败（API 异常）→ 保持当前模式并 console 记录，不崩溃；
- 位置恢复越界 → 夹紧 workArea；
- 配置解析失败 → 回落默认值（原版语义）。

## 14. 测试与验收

- **单元测试**（vitest，`petStatus`）：非绿→绿差分、waiting 差分 + 10s 限频、首帧不触发、未读 ack 后消卡再亮、消失 60s 清理、排序。
- **Rust 测试**：pet command 注册与窗口参数。
- **人工验收**：§9 清单逐条过；重点：穿透悬停切换、拖拽物理手感（坠落/抛掷/压扁）、双击说话字幕对齐、动作子页实时预览、声音接管三分支、浮窗抑制两分支、托盘/设置/🦊 三入口同步。
- **浏览器预览**：`#/pet` 在 tauri-mock 下可渲染（窗口 API mock），便于动画与卡片布局调试。

## 15. 范围外（v1 不做）

- 红灯报错/断联（MAM 无报错判定；断联不做特殊显示）；
- 自定义素材（换形象/换语音，预留 `~/.mam/pet/` 覆盖机制作后续增强）；
- Windows/Linux 穿透与置顶适配（forward 为 macOS 特性）；
- 设置项进 SQLite（localStorage 足够，跨窗口共享天然支持）。

## 16. 风险项与预案

| 风险 | 预案 |
|---|---|
| forward mousemove 转发不可靠 | **已命中并降级**：Tauri 2.11 无 forward 选项（JS/Rust 均核实），且原预案"静态划分"同样不可行——`setIgnoreCursorEvents` 是整窗开关，忽略态下 webview 收不到任何鼠标事件，无法实现按区域接收。实际降级：**整窗常驻交互、穿透禁用**（透明矩形遮挡下层点击）。恢复路径：`cursorPosition()` 模块级 API（@tauri-apps/api 2.11 已有）~30Hz 轮询 + hitTest 切换，或 Tauri 提供 forward 后 §4.4 原设计直接生效 |
| 60fps setPosition IPC 性能不足 | 降到 30fps（半帧 16ms→33ms，坠落视觉差异微小） |
| WKWebView 自动播放解锁失败 | 原版 blocked 标记 + 手势重试机制复刻；仍失败则首次双击无声、字幕照常 |
| 主窗口 + 宠物窗口双轮询开销 | 宠物隐藏自动暂停；实测超标则宠物窗口改 5s 轮询 |
| 两个 always-on-top 窗口（宠物 vs 通知浮窗）层叠 | 通知浮窗在宠物置顶时本就抑制（§6.4），冲突面极小；实测异常再调 z 序 |

## 17. 验收记录（2026-09-02，pet 分支实现收尾）

### 17.1 自动检查（全部真实运行）

| 检查 | 命令 | 结果 |
|---|---|---|
| 前端全量检查 | `pnpm check`（format:check + lint + check:i18n + build） | ✅ 全绿（i18n 309 键对齐；build 产出含 `pet-BJtgVVJ1.js` 21.11 kB chunk） |
| 前端测试 | `pnpm vitest run` | ✅ 16 文件 / 55 用例全部通过（含 tests/pet/ 39 例：assets 3、petConfig 5、petStatus 8、petAnimations 3、petVoices 3、usePetWindow 4、foxbell-render 2、foxbell-interactions 3、foxbell-cards 1、petMenu 4、foxbell-events 3、notificationTakeover 2、petSettings 3…以实际为准） |
| Rust 测试 | `cd src-tauri && cargo test` | ✅ 108 通过 / 0 失败（97 单元 + 4 集成 + 7 其它） |
| Rust lint | `cargo clippy --all-targets -- -D warnings` | ✅ 零告警 |
| TypeScript | `tsc --noEmit` | ✅ 零错误 |
| ESLint | `pnpm lint` | ✅ 0 error（2 条 warning 为 main 分支既有基线，与本移植无关） |

依赖约束核验：未新增任何 npm / crate 依赖（package.json、pnpm-lock.yaml、Cargo.toml/Cargo.lock 在 pet 分支无版本变更）。

### 17.2 tauri:dev 人工验收清单（spec §9/§14 逐条）

说明：实现者为无 GUI 交互能力的自动化流程，以下含"待人工复核"标注的项无法由机器完成；已由自动化测试覆盖语义的项给出证据。

| # | 清单项 | 状态 | 证据 / 备注 |
|---|---|---|---|
| 1 | 设置页开启桌宠 → 右下角出现，idle 动画逐帧步进；🦊 按钮与托盘同步状态 | 部分 → 待人工复核 | 出现位置/视觉待人工；逻辑已测：设置开关写 localStorage + invoke（petSettings 测试）、🦊/托盘/设置三入口经 subscribeConfig + storage 事件 + pet-visibility-changed 闭环（评审逐环核实） |
| 2 | 空闲 6s 环顾一圈，交互即断 | 逻辑已测，手感待人工复核 | look 16 帧×250ms、6s 调度、打断语义有单测与代码评审；实际观感待人工 |
| 3 | 拖动：左/右跑、上跳；松手坠落+压扁回弹+补跳；gravity 关则停驻 | 逻辑已测，手感待人工复核 | 方向阈值 -8/±6、GRAVITY=1400、DAMP=0.86、MIN_VX=24 均有单测；压扁回弹时序对照原版 1:1；拖拽手感/60fps IPC 流畅度待人工 |
| 4 | 穿透：透明区点击透到下层；精灵/卡片/菜单可交互 | **穿透禁用（降级）** | D11 forward 穿透不可实现：Tauri 2.11 无 `{forward:true}` 选项（JS/Rust API 均核实），忽略态一旦生效事件流即断、无法悬停恢复，§16"静态划分"预案也因整窗开关限制不可行。实际采用**整窗常驻交互、穿透禁用**：精灵/卡片/菜单常驻可交互，代价是透明矩形（两侧各 ~74px、底部 50px、顶部 10px @scale=1）遮挡下层应用点击。恢复路径见 §16 |
| 5 | 双击说话出字幕（时长对齐）；单击只挥手；静音开→动作有声音无 | 逻辑已测，声音待人工复核 | 单击挥手 -624px、双击字幕气泡、muted 只拦声音不拦字幕（D5 独立线语义）有单测；语音外放/时长对齐（真实 Audio 元数据）待人工 |
| 6 | 会话 waiting→红卡+approval 语音（10s 限频）；运行中黄卡不发声；完成→绿卡+done 语音 | 逻辑已测 | 差分触发、10s 限频（区分度经过证伪验证的测试）、运行中不发声均有单测（foxbell-events） |
| 7 | 完成时主窗口不播提示音（宠物开启）；宠物置顶时浮窗不弹、非置顶照弹 | 逻辑已测 | `green && !petSoundTakeover()` 门（useNotification.ts:168）、`!petSuppressPopup()` 包裹浮窗块、addHistory 无条件；判定函数有单测 |
| 8 | 点卡片跳转终端；多候选浮层可选；绿卡点后消失、再次完成重亮 | 逻辑已测，跳转实机待人工 | focus_session invoke 参数、ambiguous→focus_hwnd、ackDone 差分重亮均有单测/评审；终端聚焦实机效果待人工 |
| 9 | 右键菜单：四开关+大小三档+三动作子页实时预览+隐藏+关于 | 逻辑已测，交互待人工 | 菜单项/子页/预览循环/外点 Esc 关闭有单测；菜单几何经修复轮改为向上锚定+实测高度（评审核实窗口内完整可见）；实机点击体验待人工 |
| 10 | 大小切换与卡片增减时窗口底部锚定（精灵不跳动） | 逻辑已测，视觉待人工复核 | bottomAnchoredY 公式 + syncSize 防抖 50ms + 内容实测高度有单测；等比例联动 px(v)=v×scale 有渲染测试 |
| 11 | 位置重启记忆（含夹紧屏幕内） | 逻辑已测，重启待人工复核 | loadPosition/savePosition 取整回环 + clampToWorkArea 夹紧有单测；实际重启恢复待人工 |
| 12 | 托盘"显示/隐藏桌宠"切换生效且各入口状态同步 | 逻辑已测，托盘实机待人工 | Rust 以 is_visible() 实际可见性切换、双分支 emit；事件闭环逐环评审核实；托盘菜单实机显示待人工 |

### 17.3 已知偏差与遗留（不阻塞，均已评审记录）

1. **E6 blocked 重试未实现**：语音自动播放解锁仅做了"手势内 muted 试播 + manifest 就绪后补解锁"，spec §6.2 第 5 点的"被拦截则标记 blocked，下次点击重试"未完整复刻（计划蓝图即未包含）；影响面：定时器触发的完成/催批语音在 WKWebView 拦截时静默失败，字幕照常。
2. **muted 语义**：muted 开启时字幕照常显示（仅 talkative 拦截）——依据 spec D5"字幕与出声两条独立线"，与原插件 muted 吞字幕行为不同，已在实现注释与本记录中固化。
3. **shared 回退字幕固定 2.5s**：预载元素缺失的回退路径无法拿到音频元数据，字幕按最短 2.5s（该路径生产几乎不可达）。
4. **菜单 x 夹紧假设菜单宽 ≤180px**：0.75 缩放 + 长文案语言下菜单右缘可能溢出窗口 10-20px（纯视觉，评审 Minor）。
5. **D11 forward 缺口（已降级，用户已确认接受）**：Tauri 2.11 API 无 forward 选项，忽略态一旦生效事件流即断、无法悬停恢复（spec §4.4 风险命中），§16 原预案"静态划分"也因整窗开关限制不可行；实际降级为**整窗常驻交互、穿透禁用**（透明矩形遮挡下层点击，影响范围见 §17.2 第 4 项）。**2026-09-02 用户显式确认接受此降级**；恢复路径（cursorPosition() 轮询 + hitTest，API 已存在未实现）与"等 Tauri 提供 forward"均已入跟进账，需要时再启用。

**结论**：自动可验证项全绿；标注"待人工复核"的 5 类项（拖拽手感、穿透悬停、语音外放、窗口视觉、托盘实机）需在 tauri:dev 下按本清单人工过一遍后方可关闭本移植。
