# ZCode 控制面板视觉证据（R6 · 最终 HEAD `3b7da564`）

> 生成日期：2026-10-31 ｜ 任务：R6 整批验收的视觉证据收口
> 渲染对象：**仓库真实组件** `src/mobile/ZcodeControlPanel.tsx`（浏览器夹具只替换网络层，
> 不替换组件、不替换样式）→ 截图里的排版 = 交付代码的排版。
> 夹具源码在 `.superpowers/sdd/2026-10-29-zcode-session-ui-repair/preview/`（`.gitignore` 内，
> 不入库、不进产物包）。

## 两批图，别混读

| 前缀 | 时期 / 提交 | 用途 |
| --- | --- | --- |
| `r4-*`（20 PNG + `r4-metrics.json`） | R4，`d0e8c894`（2026-10-30 14:51） | **历史证据，仅作对照**。那一版发送区还有**独立的「打断」行**；`741e6005`（R5）已删除该行，改为单一主按钮。R4 图里的发送区**不代表**最终 HEAD 界面。 |
| `r6-*`（34 PNG + `metrics.json`） | 最终 HEAD `3b7da564`（2026-10-31） | **现行证据**。单一主按钮的四种状态、官方队列展开/已消费、问答卡与审批卡均在最终代码上重出。 |

`r4-metrics.json` 是 R4 原始 `metrics.json` 的**原样改名**（数值未改，仅加前缀），避免两批量测互相覆盖；
其内部的 `file` 字段仍是 R4 时期的**旧文件名**（无 `r4-` 前缀），对应关系就是「加 `r4-` 前缀」，不改写历史记录。

## 怎么读这些截图

- **视口宽度**：文件名第 3 段（`360` / `390` / `320`）。用 Chrome CDP
  `Emulation.setDeviceMetricsOverride` 精确设定**布局视口**宽（`--window-size` 在本机
  Chrome headless 下有 500px 下限，不可用）。
- **主题**：第 4 段 `light` / `dark`（`documentElement.dark` 类，同 mobile.css 策略）。
- **语言**：第 5 段 `zh` / `en`。
- **字号**：第 6 段 `100` / `125`。125% 用**夹具页面 CSS** 实现（仓库自带的
  `data-font-scale="1.25"` 机制 → 覆写 `--text-*` 变量），**不用 DPR 冒充**
  （截图 `--force-device-scale-factor=1`，1 CSS px = 1 设备像素）。
- **图高**：按 `#root` 的**真实底边**裁切（`getBoundingClientRect().bottom + 16`），
  不留 1400px 的视口空白；只截视口，不用 `captureBeyondViewport` 撑宽。
- **`metrics.json`**：每个场景的真实浏览器布局量测（行数 / 横向溢出 / 主按钮态 /
  独立打断行残留 / 模型名文字宽 / 队列与待处理卡状态），是截图之外的**硬证据**；
  由 `shoot.mjs` 运行期生成，不手写、不硬编码。

### 量测字段（`metrics.json` 每项 `metrics` 内）

| 字段 | 口径 |
| --- | --- |
| `toolbarRows` | 工具栏子元素 `getBoundingClientRect()` 的**垂直区间不重叠分组数**（1 = 单行） |
| `doc/panel/toolbarHorizontalOverflow` | 三层各自的 `scrollWidth > clientWidth` |
| `officialVisible` / `modelVisible` / `thoughtVisible` | 三个控件的 `rect.right <= innerWidth + 1`（没有被右边界裁掉） |
| `primaryAction` / `primaryMode` / `primaryLabel` / `primaryDisabled` | 主按钮自身标的 `data-primary-action` / `data-primary-mode`、可见文案、`disabled` |
| `strayInterruptRows` | 主按钮之外、文案恰为「打断 / Interrupt」的 `button` 数量——R5 之后必须为 **0** |
| `modelTextWidth` vs `modelTextAvailable` | 选中模型名用 canvas 按**元素 computed font** 量的文字宽 vs. select 内框宽（原生 select 的 `scrollWidth` 不反映 option 文本，不能用它判截断） |
| `modelTitle` | 模型 select 的 `title`（长名被视觉裁剪时的可访问全名） |
| `attachmentStatuses` / `attachNotice` | 附件条目 `data-status` 与常驻提示原文 |
| `queueState` / `queueItemCount` / `queueText` | 官方队列三态根节点、展开后的条目数与可见文案 |
| `pendingKind` / `pendingBorderColor` / `pendingOptionLayout` / `pendingOptionCount` / `pendingDetailVisible` | 当前 `data-active="true"` 那一组卡片的种类、真实 computed 边框色（sky/rose）、选项纵向布局与 detail 展开 |

## 场景清单 A：16 张排版矩阵（360 / 390 × 浅深 × 中英 × 100% / 125%）

场景配置：`state=idle`（官方 `activeWorks` 为空）+ 一句草稿 → 主按钮为**发送**；
队列投影取 `unknown`（队列另有专项图），这样矩阵只压工具栏与发送区。

| 文件名 | 视口 / 主题 / 语言 / 字号 | 主按钮 | 模型 select 宽 / 名文字宽 | 结论 |
| --- | --- | --- | --- | --- |
| `r6-send-360-light-zh-100.png` | 360 / light / zh / 100 | 发送 | 144px / 468px | 工具栏**一行**；无横向溢出；长名在框内省略 |
| `r6-send-360-light-zh-125.png` | 360 / light / zh / 125 | 发送 | 113px / 585px | **一行**；无溢出 |
| `r6-send-360-light-en-100.png` | 360 / light / en / 100 | Send | 114px / 468px | **一行**（英文按钮更长） |
| `r6-send-360-light-en-125.png` | 360 / light / en / 125 | Send | 75px / 585px | **一行**（模型名只余 2–3 字符，箭头与 `title` 完整） |
| `r6-send-360-dark-zh-100.png` | 360 / dark / zh / 100 | 发送 | 144px / 468px | 深色 token 生效；**一行** |
| `r6-send-360-dark-zh-125.png` | 360 / dark / zh / 125 | 发送 | 113px / 585px | **一行**；无溢出 |
| `r6-send-360-dark-en-100.png` | 360 / dark / en / 100 | Send | 114px / 468px | **一行**；无溢出 |
| `r6-send-360-dark-en-125.png` | 360 / dark / en / 125 | Send | 75px / 585px | **一行**；无溢出 |
| `r6-send-390-light-zh-100.png` | 390 / light / zh / 100 | 发送 | 174px / 468px | **一行**；模型名露出更多 |
| `r6-send-390-light-zh-125.png` | 390 / light / zh / 125 | 发送 | 143px / 585px | **一行**；无溢出 |
| `r6-send-390-light-en-100.png` | 390 / light / en / 100 | Send | 144px / 468px | **一行**；无溢出 |
| `r6-send-390-light-en-125.png` | 390 / light / en / 125 | Send | 105px / 585px | **一行**；无溢出 |
| `r6-send-390-dark-zh-100.png` | 390 / dark / zh / 100 | 发送 | 174px / 468px | **一行**；无溢出 |
| `r6-send-390-dark-zh-125.png` | 390 / dark / zh / 125 | 发送 | 143px / 585px | **一行**；无溢出 |
| `r6-send-390-dark-en-100.png` | 390 / dark / en / 100 | Send | 144px / 468px | **一行**；无溢出 |
| `r6-send-390-dark-en-125.png` | 390 / dark / en / 125 | Send | 105px / 585px | **一行**；无溢出 |

## 场景清单 B：18 张状态与组件场景

| 文件名 | 场景 | 观察结论 |
| --- | --- | --- |
| `r6-stop-360-light-zh-100.png` | 运行中（唯一 `primaryTurn`）+ 空输入 | 主按钮＝**打断**；按钮下是「运行中：输入为空，此按钮将打断当前回合」；**没有任何独立打断行**（`strayInterruptRows=0`） |
| `r6-stop-360-dark-en-125.png` | 同上，深色 / 英文 / 125% | 主按钮＝**Interrupt**；单行工具栏；无独立打断行 |
| `r6-stop-390-light-en-100.png` | 同上，390 / 英文 | 主按钮＝**Interrupt**；无独立打断行 |
| `r6-stop-390-dark-zh-125.png` | 同上，390 / 深色 / 125% | 主按钮＝**打断**；无独立打断行 |
| `r6-send-attach-360-light-zh-100.png` | 空闲 + **就绪附件** + 空文字 | 主按钮＝**发送**（就绪附件算可发内容）；附件 chip 可见 |
| `r6-attach-uploading-360-light-zh-100.png` | 空闲 + 附件**上传中** | 主按钮**禁用**（灰）；常驻提示「附件还在上传，等上传完成再发送」 |
| `r6-attach-failed-360-light-zh-100.png` | 空闲 + 附件**上传失败** | 主按钮**禁用**；附件条目带失败原因；常驻提示「有附件上传失败，请先移除失败的附件再发送」 |
| `r6-attach-failed-390-dark-en-125.png` | 同上，390 / 深色 / 英文 / 125% | 主按钮＝Send 且**禁用**；英文常驻提示两行不溢出 |
| `r6-question-360-light-zh-100.png` | 三题问答卡（运行中） | 卡为**蓝色**（computed 边框 `oklab(0.685 -0.091 -0.142 / 0.6)` ≈ sky-500/60）；只显示「第 1/3 题」；选项**纵向**两项；「上一题／下一题／提交」 |
| `r6-question-390-dark-en-125.png` | 同上，390 / 深色 / 英文 / 125% | 蓝色卡；`Question 1 of 3`；纵向；无溢出 |
| `r6-approval-360-light-zh-100.png` | 审批卡（切到第 2 张，展开 detail） | 卡为**红色**（computed `oklab(0.645 0.236 0.070 / 0.6)` ≈ rose-500/60）；`Bash` + `npm run build --workspace @tuvis/mobile`；只有「允许一次／拒绝」纵向两项，**无完全访问类选项** |
| `r6-approval-390-dark-en-125.png` | 同上，390 / 深色 / 英文 / 125% | 红卡；`Hide command details`；命令正文折行不溢出 |
| `r6-queue-paused-expanded-360-light-zh-100.png` | 官方队列**暂停 + 展开** | 「排队 2 条 已暂停 已手动暂停」+ `#1/#2` 两条正文；**没有任何撤回／越队／恢复按钮**（只读展示） |
| `r6-queue-consumed-360-light-zh-100.png` | 官方队列**已消费**（暂停仍在、条目清空） | 「排队 0 条 已暂停 回合已停止」；暂停标志**不因清空而消失**；无空的列表块 |
| `r6-queue-active-expanded-390-dark-en-125.png` | 队列未暂停 + 展开（390 / 深色 / 英文 / 125%） | `2 queued` + `#1/#2` 两条正文；无暂停标志；无编辑动作 |
| `r6-longmodel-360-light-zh-100.png` | 长模型名专项 | `zai/glm-4.6-air-2026-10-29-reasoning-long-context-window-variant` 在框内被浏览器裁剪（文字宽 468px > 可用 124px），**下拉箭头完整**、`title` 保留全名 |
| `r6-narrow-320-light-zh-125.png` | 320px 极窄 + 125%（计划 §1 允许最窄档） | 工具栏仍**一行**、无横向溢出、三个控件全部可见；模型 select 收窄到 73px（文字宽 585px） |
| `r6-attachhelp-360-light-zh-100.png` | 附件「＋」旁问号已展开 | 展开的是**既有**附件存储说明（`.tuvis-attachments/<会话>/`、已在本地 git 排除）；按需展开，不弹文件选择器 |

## 汇总结论（全部 34 个场景的量测值）

- 工具栏行数：**34/34 = 1 行**（含 320px / 125% 极窄档）。
- 横向溢出：**34/34 无**（文档 / 面板 / 工具栏三层 `scrollWidth ≤ clientWidth`）；三个控件**全部** `right ≤ innerWidth`。
- 独立打断行残留：**34/34 = 0**（`741e6005` 删除后，HEAD 上只剩主按钮这一个写入口）。
- 主按钮态：**20 张 send / 11 张 stop / 3 张 disabled**——与各场景的输入、附件、官方 `activeWorks` 一一对应。
- 模型 select 全名可达：**34/34 `title` 非空**（长名 468–585px 文字宽 vs 55–154px 可用宽 → 视觉必被裁剪，全名仍可读）。
- 待处理卡：问答卡 computed 边框为 sky（蓝）、审批卡为 rose（红），选项容器均 `data-layout="vertical"`，任何时刻只有一题（`第 1/3 题`）。

## 已知观察（未在本轮代码范围内修改，如实登记）

- **队列暂停文案重复**：`pauseReason="manual"` 时同一行同时渲染「已暂停」与「已手动暂停」（`ZcodeQueuePanel.tsx:74-81`）。既有次要项 M-2，本轮不改 `src/`。
- **附件失败原因原样回显**：失败条目显示的是服务端冻结原因字面值（截图为 `too_large`），面板旁的常驻提示才是本地化文案。既有行为，未改。
- **125% + 英文下模型名只剩 2–3 字符**：这是「必要动作不被隐藏」的代价（官方页入口与档位不收缩），全名靠 `title` / option 原文取回；是否接受属产品口径，真机待验。
- 浏览器渲染 ≠ 真机：系统字号、动态字体、软键盘顶起后的可视区、真机触控命中区都仍需实机验收。

## 复现方式

```bash
cd .superpowers/sdd/2026-10-29-zcode-session-ui-repair/preview
bash shoot.sh    # 构建夹具 → 起静态服务器 → CDP 量测 + 批量截图 → 写 metrics.json
```

夹具只替换 `window.fetch` / `EventSource` / `XMLHttpRequest`（见 `preview/src/mocks.ts`），
组件、样式、判据均来自 `src/mobile/`；R6 新增参数：
`state=running|idle`、`queue=active|paused|consumed|unknown`、`upload=ok|hang|fail`、
`attach=0`、`draft=1`、`expand=1`、`tab=approval`、`help=1`。
