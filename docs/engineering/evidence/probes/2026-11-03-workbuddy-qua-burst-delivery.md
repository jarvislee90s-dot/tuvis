# WorkBuddy qua 路线「整包突发交付」取证（2026-11-03）

## 结论速览

- 第三方中转路线（WorkBuddy 自定义供应商，模型 id 形如 `custom-local:glm-5.3-flash`）存在
  **非流式交付**：整段补全在末尾 1~2 个 chunk 一次性送达。载荷自证：`total_chunks` = 1~2、
  `ttft_ms ≈ total_cost_ms`（sse_connect 等的就是那一个包）、`interval_*` 八个分桶几乎全空
  （仅 `interval_0_50_count` 偶有 1）。
- 该形态下生成窗（`total_cost_ms − ttft_ms`，即首 chunk→末 chunk 跨度）退化到 3~6ms，
  被既有的「流式输出速度 = completion_tokens ÷ 生成窗」算式放大成 **10 万+ TPS**
  （单条最高 219666.7：659 token ÷ 3ms）。
- 对照组：官方流式路线（minimax-m3.1-flash）同一字段健康——`total_chunks` 58、区间散布
  在 0-50ms（21）/100-500ms（33）/500-1000ms（3）多个桶、生成窗 3761~10041ms。
- 分布（tps_observation 实测，2026-11-02~11-03）：custom-local 670 条中生成窗 <50ms 的
  **349 条**、50~1000ms 的 80 条（其中 chunks≤2 的 34 条）、>1000ms 的 241 条（平均可分
  间隔 290，真流式）；minimax 59 条生成窗全部 >1000ms、0 条 <50ms。

## 现象

PR #195 分支构建（v3 游标恢复把 24h 内 qua 历史首次补进速度账本后）速度页出现异常值：
「当前模式速度 6452 TPS」、时间桶单点 ~32000 TPS；而组合表 WorkBuddy·custom-local:glm-5.3-flash
聚合 68.9 TPS（被健康记录稀释）。速度页此前从未展示过 WorkBuddy（见
2026-11-02-workbuddy-qua-zhcn-prefix.md），故该数据形态是首次可见，非回归引入。

## 取证样本（qua.end 原始载荷，字段摘选）

突发（custom-local，tps_observation 离群 top1）：

```json
{"model":"custom-local:glm-5.3-flash","finish_reason":"tool_calls","completion_tokens":"659",
 "total_cost_ms":"87962","ttft_ms":"87959","total_chunks":"1","sse_connect_cost_ms":"87959",
 "text_first_cost_ms":"0","text_total_cost_ms":"0",
 "interval_0_50_count":"0",...,"interval_gt_60000_count":"0"}
```

流式（minimax 对照）：

```json
{"model":"minimax-m3.1-flash","completion_tokens":"1188","total_cost_ms":"17541","ttft_ms":"7500",
 "total_chunks":"58","interval_0_50_count":"21","interval_100_500_count":"33","interval_500_1000_count":"3",...}
```

## 判定

- **不是解析缺陷**：逐闸核对（身份/finish/数值归一/时刻前缀）全部正确，算式按 Spec §4
  如实执行；是「无流式过程的交付」缺一道语义闸门——突发交付没有可测的流式速率。
- 修复语义：突发交付的观察 `generation_ms = None`（同 Codex「无生成窗」缺口语义），
  仅保留响应口径（如 659 token ÷ 87.962s ≈ 7.5 TPS，含等待）与首 Token 等待（87.96s，
  真实）。
- 判据（全部来自载荷自身遥测，不引入模型速率先验；依据为经验分布而非机制断言——
  分桶记的是 chunk 间隔时长，与窗口总长无关）：
  1. 生成窗 < 50ms——该量级窗实测仅出现于整包突发（custom-local 349/670 条；官方流式
     路线 0/59、最短 3761ms），窗内至多 1~2 个 chunk、速率分母失真（覆盖 349 条）；
  2. interval 字段在场且 8 桶计数之和 < 3——少于 3 个可分间隔无法构成速率估计，覆盖
     「窗口稍大但交付仍是整包突发」（50~1000ms 桶 34 条 chunks≤2）；字段整体缺席的旧
     载荷只按窗口规则兜底。
- 8 个分桶字段名（`interval_0_50_count` / `interval_50_100_count` / `interval_100_500_count` /
  `interval_500_1000_count` / `interval_1000_3000_count` / `interval_3000_10000_count` /
  `interval_10000_60000_count` / `interval_gt_60000_count`）已由完整原始载荷的 51 键字段
  清单逐一核实，非按边界连续性推断。
- 残余声明：轻微流式（3~20 个可分间隔的短窗交付，如 600 token/0.5s）不会被本闸覆盖，
  其速率按遥测如实入账；若后续实机仍见异常，凭新证据再扩判据。

## 修复接线

读取器加突发闸门 + 游标 v4（再次 24h 有界恢复，UPSERT 改写已入库的退化观察）；
同分支 `fix/workbuddy-qua-locale-timestamp` 增量提交。

## 复现命令

```bash
# 离群 top（tps_observation，只读）
py -3 -c "import sqlite3;con=sqlite3.connect(r'file:C:/Users/bunny/.tuvis/tuvis.db?mode=ro',uri=True);print(list(con.execute(\"select stable_call_key,generation_ms,output_tokens,round(1000.0*output_tokens/generation_ms) from tps_observation where source='workbuddy' and generation_ms>0 order by 4 desc limit 5\")))"
# 按模型的生成窗分布
# （见取证过程：custom-local lt50=349 / 50_1000=80 / gt1000=241；minimax lt50=0）
```
