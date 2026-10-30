# ZCode 控制面板视觉证据（R4 单行紧凑工具栏 + 附件问号说明）

> 生成日期：2026-10-30 ｜ 任务：R4「单行工具栏与附件说明」（计划 §1 布局裁决）
> 渲染对象：**仓库真实组件** `src/mobile/ZcodeControlPanel.tsx`（浏览器夹具只替换网络层，
> 不替换组件、不替换样式）→ 截图里的排版 = 交付代码的排版。
> 夹具源码在 `.superpowers/sdd/2026-10-29-zcode-session-ui-repair/preview/`（不入库、不进产物包）。

## 怎么读这些截图

- **视口宽度**：文件名第 3 段（`360` / `390` / `320`）。用 Chrome CDP
  `Emulation.setDeviceMetricsOverride` 精确设定**布局视口**宽（`--window-size` 在本机
  Chrome headless 下有 500px 下限，不可用）。
- **主题**：第 4 段 `light` / `dark`（`documentElement.dark` 类，同 mobile.css 策略）。
- **语言**：第 5 段 `zh` / `en`。
- **字号**：第 6 段 `100` / `125`。125% 用**夹具页面 CSS** 实现（仓库自带的
  `data-font-scale="1.25"` 机制 → 覆写 `--text-*` 变量），**不用 DPR 冒充**
  （截图 `--force-device-scale-factor=1`，1 CSS px = 1 设备像素）。
- **`metrics.json`**：每个场景的真实浏览器布局量测（行数 / 横向溢出 / 模型 select 宽度 /
  `title` 全名），是截图之外的**硬证据**。量测口径见文件内 `toolbarRows` 等字段。

## 场景清单

| 文件名 | 场景 | 观察结论 |
| --- | --- | --- |
| `toolbar-360-light-zh-100.png` | 360px / 浅色 / 中文 / 100% | 工具栏**一行**；「打开官方页面」+ 模型 + 思考强度同排；无横向溢出；模型名在框内省略、箭头完整 |
| `toolbar-360-light-zh-125.png` | 360px / 浅色 / 中文 / 125% | 仍**一行**（模型 select 收窄到 109px，未换行）；无溢出 |
| `toolbar-360-light-en-100.png` | 360px / 浅色 / 英文 / 100% | **一行**；英文「Open official page」较长，模型 select 收窄到 110px；无溢出 |
| `toolbar-360-light-en-125.png` | 360px / 浅色 / 英文 / 125% | **一行**（英文按钮最长 + 字号最大的组合；模型 select 收到 71px 仍不换行）；无溢出 |
| `toolbar-360-dark-zh-100.png` | 360px / 深色 / 中文 / 100% | 深色 token 生效；**一行**；无溢出 |
| `toolbar-360-dark-zh-125.png` | 360px / 深色 / 中文 / 125% | **一行**；无溢出 |
| `toolbar-360-dark-en-100.png` | 360px / 深色 / 英文 / 100% | **一行**；无溢出 |
| `toolbar-360-dark-en-125.png` | 360px / 深色 / 英文 / 125% | **一行**；无溢出 |
| `toolbar-390-light-zh-100.png` | 390px / 浅色 / 中文 / 100% | **一行**；模型 select 宽 170px；无溢出 |
| `toolbar-390-light-zh-125.png` | 390px / 浅色 / 中文 / 125% | **一行**；无溢出 |
| `toolbar-390-light-en-100.png` | 390px / 浅色 / 英文 / 100% | **一行**；无溢出 |
| `toolbar-390-light-en-125.png` | 390px / 浅色 / 英文 / 125% | **一行**；无溢出 |
| `toolbar-390-dark-zh-100.png` | 390px / 深色 / 中文 / 100% | **一行**；无溢出 |
| `toolbar-390-dark-zh-125.png` | 390px / 深色 / 中文 / 125% | **一行**；无溢出 |
| `toolbar-390-dark-en-100.png` | 390px / 深色 / 英文 / 100% | **一行**；无溢出 |
| `toolbar-390-dark-en-125.png` | 390px / 深色 / 英文 / 125% | **一行**；无溢出 |
| `longmodel-360-light-zh-100.png` | 360px / 浅色 / 中文 / 100%，**长模型名**（`zai/glm-4.6-air-2026-10-29-reasoning-long-context-window-variant`） | **一行**；长名在框内被浏览器裁剪（不撑破容器）；下拉箭头完整可见；全名仍可取（`title` + option 原文） |
| `narrow-320-light-zh-125.png` | **320px**（极窄）/ 浅色 / 中文 / 125%（计划 §1 允许的最窄档） | **一行**；模型 select 收到 69px（仍可见可用）；**无横向溢出**；无动作被隐藏 |
| `attachhelp-360-light-zh-100.png` | 360px / 浅色 / 中文 / 100%，**附件问号已展开** | 问号展开既有存储说明（复用 `mobile.composer.attachHint` 文案，含 `.tuvis-attachments/`）；说明按需展开、不平铺常驻 |
| `approval-360-light-zh-100.png` | 360px / 浅色 / 中文 / 100%，**切到审批卡并展开 detail** | 红色审批卡 + 命令正文（`npm run build --workspace @tuvis/mobile`）可见；工具栏仍**一行** |

## 汇总结论（全部 20 个场景的量测值）

- 工具栏行数：**全部场景 = 1 行**（含 320px/125% 极窄档）。
- 横向溢出：**全部场景 = 无**（文档 / 面板 / 工具栏三层 `scrollWidth ≤ clientWidth`）。
- 模型 select 全名可达：**全部场景 `title` 非空**（长名被视觉裁剪不影响可访问全名）。
- 必要动作（官方页入口 / 档位 select）均 `shrink-0`：不被裁掉、不被隐藏。

## 复现方式

```bash
cd .superpowers/sdd/2026-10-29-zcode-session-ui-repair/preview
bash shoot.sh    # 构建夹具 → 起静态服务器 → CDP 量测 + 批量截图 → 写 metrics.json
```
