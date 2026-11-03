# W5 视觉证据：ZCode 队列行（W4 之后）+ 待处理卡 F4/F5

- 日期：2026-10-31
- 仓库：`/Users/jarvis/Documents/tuvis-worktree2` · 分支 `codex/zcode-unified-remote`
- 施工单：`docs/superpowers/plans/2026-10-31-zcode-next-round-engineering-plan-v2.md` §6（F4/F5）、§10.3（视觉证据要求）
- 夹具来源：复用 R4/R6 的手机预览夹具
  （`.superpowers/sdd/2026-10-29-zcode-session-ui-repair/preview/`，gitignore 内），
  按 **W4 之后的队列行**改写为
  `.superpowers/sdd/2026-10-31-zcode-queue-round/preview/`（同样不入库）。
- 截图张数：**38 张**（24 队列行矩阵 + 4 修改确认弹层 + 4 恢复运行 + 3 F4 只读卡 + 3 F5 截断审批）
- 原始量测：`metrics.json`（同目录，CDP 从**真实浏览器布局**读回，非 jsdom）

## 0. 一句话结论

按 **W4 之后的队列行**重新出图，**38 张全部人工逐张看过**：非空白、每行三按钮都在、
预览都以 `…` 结尾、**整页零横向溢出**、「恢复运行」在 `paused=true` 时可见；F4 只读卡
**没有**选项/提交键/输入框，F5 截断审批**撤掉「允许一次」但保留「拒绝」与「打开官方页面」**。

## 1. 夹具口径（为什么这些截图能当证据）

- 夹具**直接 import 仓库里的 `src/mobile/ZcodeControlPanel.tsx`**，只把网络层换成合成响应
  → 截图里的排版 = 交付代码的排版，**不是**手写仿制品。
- 视口宽度用 CDP `Emulation.setDeviceMetricsOverride` 精确设为 360/320/390
  （本机 Chrome headless 的 `--window-size` 下限是 500px，用它截出来的根本不是 360px 版面）。
- 字号缩放走仓库自带的 `data-font-scale="1.25"`（CSS 变量覆写），**不用 DPR 冒充字号**；
  `deviceScaleFactor=1` → 1 CSS px = 1 设备像素。
- 队列项按 **W4 的真实协议形状**给出（`kind=sendText` / `dispatchState=queued` /
  `hasAttachments=false`）+ 队列 `availability.queueEditAllowed=true` →
  「修改 / 撤回」才是**真的可点**，而不是靠 mock 猜出来的灰按钮。
- 队列三写槽位按 Rust `capability/resolver.rs` 的**候选档**给出：
  `queueEdit`/`queueResume` = `enabled` + 候选原因码，`queueNow` 恒 `needs_evidence`
  （与「引导」恒禁用一致）。
- F4/F5 两个新字段（`answerable` / `detailTruncated`）**只由服务端投影**，夹具只负责给出
  与宿主同形的取值——前端**没有**第二套规则。

## 2. 场景与判据

| 场景族 | 张数 | 必查事实 |
| --- | --- | --- |
| `w5-queue-rows-*` | 24 | 三条排队 + 已暂停；每行**序号 → 预览 → 引导/修改/撤回**；预览以 `…` 结尾；「恢复运行」可见 |
| `w5-queue-edit-confirm-*` | 4 | 「输入框已有草稿」三选弹层（合并/替换/取消）真实弹出 |
| `w5-queue-resume-*` | 4 | 队列**已空但官方仍暂停**时「恢复运行」**仍可见** |
| `w5-pending-readonly-*` | 3 | F4：只读卡**无**选项 / **无**提交键 / **无**输入框；有原因句 + 「打开官方页面」 |
| `w5-pending-truncated-*` | 3 | F5：截断提示可见；「允许一次」**禁用**；「拒绝」**可用**；「打开官方页面」在场 |

## 3. 真实量测（`metrics.json` 摘要）

关键字段：`docScrollWidth` / `docClientWidth`（整页横向溢出）、`queueListScrollWidth` /
`queueListClientWidth`（队列容器**内部**滚动）、`queueRows[]`（逐行按钮可见性与省略）、
`resumeVisible`（恢复入口）、`readonlyHasOptions` / `readonlyHasSubmit` /
`readonlyHasTextbox`（F4 零可提交面）、`allowButtonDisabled` / `denyButtonDisabled`（F5）。

| 场景 | 视口 | 主题 | 语言 | 字号 | doc scroll/client | 横向溢出 | 队列容器 scroll/client | 队列内部横滚 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `w5-queue-rows-360-light-zh-100` | 360 | light | zh | 100 | 360/360 | 否 | 318/318 | 否 |
| `w5-queue-rows-360-light-zh-125` | 360 | light | zh | 125 | 360/360 | 否 | 318/318 | 否 |
| `w5-queue-rows-360-light-en-100` | 360 | light | en | 100 | 360/360 | 否 | 318/318 | 否 |
| `w5-queue-rows-360-light-en-125` | 360 | light | en | 125 | 360/360 | 否 | 318/318 | 否 |
| `w5-queue-rows-360-dark-zh-100` | 360 | dark | zh | 100 | 360/360 | 否 | 318/318 | 否 |
| `w5-queue-rows-360-dark-zh-125` | 360 | dark | zh | 125 | 360/360 | 否 | 318/318 | 否 |
| `w5-queue-rows-360-dark-en-100` | 360 | dark | en | 100 | 360/360 | 否 | 318/318 | 否 |
| `w5-queue-rows-360-dark-en-125` | 360 | dark | en | 125 | 360/360 | 否 | 318/318 | 否 |
| `w5-queue-rows-320-light-zh-100` | 320 | light | zh | 100 | 320/320 | 否 | 278/278 | 否 |
| `w5-queue-rows-320-light-zh-125` | 320 | light | zh | 125 | 320/320 | 否 | 278/278 | 否 |
| `w5-queue-rows-320-light-en-100` | 320 | light | en | 100 | 320/320 | 否 | 278/278 | 否 |
| `w5-queue-rows-320-light-en-125` | 320 | light | en | 125 | 320/320 | 否 | 278/278 | 否 |
| `w5-queue-rows-320-dark-zh-100` | 320 | dark | zh | 100 | 320/320 | 否 | 278/278 | 否 |
| `w5-queue-rows-320-dark-zh-125` | 320 | dark | zh | 125 | 320/320 | 否 | 278/278 | 否 |
| `w5-queue-rows-320-dark-en-100` | 320 | dark | en | 100 | 320/320 | 否 | 278/278 | 否 |
| `w5-queue-rows-320-dark-en-125` | 320 | dark | en | 125 | 320/320 | 否 | 278/278 | 否 |
| `w5-queue-rows-390-light-zh-100` | 390 | light | zh | 100 | 390/390 | 否 | 348/348 | 否 |
| `w5-queue-rows-390-light-zh-125` | 390 | light | zh | 125 | 390/390 | 否 | 348/348 | 否 |
| `w5-queue-rows-390-light-en-100` | 390 | light | en | 100 | 390/390 | 否 | 348/348 | 否 |
| `w5-queue-rows-390-light-en-125` | 390 | light | en | 125 | 390/390 | 否 | 348/348 | 否 |
| `w5-queue-rows-390-dark-zh-100` | 390 | dark | zh | 100 | 390/390 | 否 | 348/348 | 否 |
| `w5-queue-rows-390-dark-zh-125` | 390 | dark | zh | 125 | 390/390 | 否 | 348/348 | 否 |
| `w5-queue-rows-390-dark-en-100` | 390 | dark | en | 100 | 390/390 | 否 | 348/348 | 否 |
| `w5-queue-rows-390-dark-en-125` | 390 | dark | en | 125 | 390/390 | 否 | 348/348 | 否 |
| `w5-queue-edit-confirm-360-light-zh-100` | 360 | light | zh | 100 | 360/360 | 否 | 318/318 | 否 |
| `w5-queue-edit-confirm-360-dark-en-125` | 360 | dark | en | 125 | 360/360 | 否 | 318/318 | 否 |
| `w5-queue-edit-confirm-320-light-zh-125` | 320 | light | zh | 125 | 320/320 | 否 | 278/278 | 否 |
| `w5-queue-edit-confirm-390-dark-en-100` | 390 | dark | en | 100 | 390/390 | 否 | 348/348 | 否 |
| `w5-queue-resume-360-light-zh-100` | 360 | light | zh | 100 | 360/360 | 否 | —（队列已空） | — |
| `w5-queue-resume-360-dark-en-125` | 360 | dark | en | 125 | 360/360 | 否 | —（队列已空） | — |
| `w5-queue-resume-320-light-zh-125` | 320 | light | zh | 125 | 320/320 | 否 | —（队列已空） | — |
| `w5-queue-resume-390-dark-zh-100` | 390 | dark | zh | 100 | 390/390 | 否 | —（队列已空） | — |
| `w5-pending-readonly-360-light-zh-100` | 360 | light | zh | 100 | 360/360 | 否 | —（队列未知） | — |
| `w5-pending-readonly-360-dark-en-125` | 360 | dark | en | 125 | 360/360 | 否 | —（队列未知） | — |
| `w5-pending-readonly-320-light-zh-125` | 320 | light | zh | 125 | 320/320 | 否 | —（队列未知） | — |
| `w5-pending-truncated-360-light-zh-100` | 360 | light | zh | 100 | 360/360 | 否 | —（队列未知） | — |
| `w5-pending-truncated-360-dark-en-125` | 360 | dark | en | 125 | 360/360 | 否 | —（队列未知） | — |
| `w5-pending-truncated-320-light-zh-125` | 320 | light | zh | 125 | 320/320 | 否 | —（队列未知） | — |

### 3.1 逐行事实（24 张队列行矩阵，每张 3 行 = 72 行次，全部一致）

- `guideVisible=true` / `editVisible=true` / `deleteVisible=true`：**三按钮都在**（72/72 行次）。
- `guideDisabled=true`：「引导」**恒禁用**（与 W4 一致：无「已排队项转引导」的实机证据）。
- `editDisabled=false` / `deleteDisabled=false`：「修改 / 撤回」在可写资格下**真的可点**。
- `previewEllipsis=true`：预览**以 `…` 结尾**（72/72 行次），`previewLength=21`（20 字素 + 省略号）。
- `rowHorizontalOverflow=false`：**行容器自身零溢出**（72/72 行次）。
- `resumeVisible=true`（24/24 张）：`paused=true` 时「恢复运行」**可见**。
- `queueListHorizontalScroll=false`：320px/125% 下队列容器**也**不需要横滚（行内已收好）。

### 3.2 F4 / F5 专项事实

| 场景 | `readonlyReasonVisible` | `readonlyHasOptions` | `readonlyHasSubmit` | `readonlyHasTextbox` | `truncatedNoticeVisible` | `allowButtonDisabled` | `denyButtonDisabled` |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `w5-pending-readonly-*`（3 张） | **true** | **false** | **false** | **false** | — | — | — |
| `w5-pending-truncated-*`（3 张） | — | — | — | — | **true** | **true** | **false** |

## 4. 人工逐张核对（ViewImage，38/38）

| 判据 | 结果 |
| --- | --- |
| 非空白（都渲染出真实面板，不是空壳/裸 DOM） | **38/38 通过** |
| 三行队列条目都在，每行三按钮都在 | **24/24 队列行矩阵通过** |
| 预览以 `…` 结尾 | **24/24 通过**（72/72 行次） |
| 无横向溢出（内容没被裁掉） | **38/38 通过** |
| 恢复按钮在 `paused=true` 时可见 | **24/24 队列行 + 4/4 恢复场景通过** |
| F4 只读卡：无选项 / 无提交键 / 无输入框 | **3/3 通过**（截图上只有题干只读列表 + 原因句 + 「打开官方页面」） |
| F5 截断审批：「允许一次」灰化、「拒绝」可点 | **3/3 通过** |
| 修改确认三选弹层真实弹出 | **4/4 通过**（合并 / 替换 / 取消三键都在，默认焦点在「取消」） |

## 5. 已知边界（如实申报）

1. **这不是实机证据**：截图证明的是**交付代码在真实浏览器里的排版**，用的是**合成官方投影**。
   官方队列项的真实字段（W0 取证）与合成值是否逐字段同形，仍待实机复核。
2. **「引导」按钮恒灰**是 W4 的既定裁决（无实机证据），截图如实呈现灰态；**不**代表该能力可用。
3. **320px/125% 下队列容器未触发内部横滚**（本次 `278/278`）——结构上仍保留
   `overflow-x-auto` 兜底，但这一档没有实测到「必须横滚才能看到撤回」的极限情形。
4. **截断正文的真实长度**由服务端 `MAX_PENDING_DETAIL_CHARS`（2000 字符）决定；
   截图里用的是超限合成正文，展示的是**截断后的卡片形态**，不代表真实 Bash 正文长什么样。
5. 截图**不含**真机系统字号设置（iOS「更大字体」）、浏览器双指缩放与深色模式的系统级差异。
