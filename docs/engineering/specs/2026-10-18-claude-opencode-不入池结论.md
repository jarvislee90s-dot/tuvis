# claude / opencode 附件不入文件池结论（#69 降级落档）

> 日期：2026-10-18｜性质：降级结论文档（裁决来源：issue #69 本轮批次裁决）
> 结论一句话：两家无「带路径形态」的附件可提取，不入文件池；重开条件见 §三。

## 一、claude：base64 内嵌，无独立路径

claude 会话的图片附件以 **base64 内嵌于消息体**（transcript JSONL 的消息
content 块内），**没有独立文件路径**可提取——文件池按路径聚合（预览走磁盘
实体），无路径即无条目可建。解码落盘再入池会制造一份脱离工具管辖的副本
（与「文件池 = 会话引用的既有文件」语义冲突），否决。

## 二、opencode：part 无 file 类型

opencode 的消息部件（part 表）**没有 file 类型**——B1 调研（2026-09-17）
结论；登记面：`src-tauri/src/remote/attachment_fixtures.rs` 头注释
（kimi / zcode / claude / opencode 四家同处登记，claude 与 opencode 两行
为 2026-10-18 随本档补登——原文档初稿误引「:1-11 即含 opencode」，评审
I-2 抓获后补登使引用成立）。本档为该结论的文档面落档。

## 三、重开条件

任一工具未来引入**带路径的附件形态**（文件落盘 + 消息引用路径，如 codex 的
local_images 形态）时重评。重评入口：本档 + `attachment_fixtures.rs` 注释
双处登记面。

## 四、待办边界

- workbuddy / openclaw / dsh 的文件池调研**不在本批**（issue #69 内保持待办）。
- 本降级不改变两家既有文件池行为（tool 调用路径、正文内联标记仍照常提取）。
