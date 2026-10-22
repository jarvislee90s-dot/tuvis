# Codex 附件合成输入

`cases.synthetic.json` 是 SYNTHETIC 回归输入集合，**不是实机 IPC 样本**，也不是
`observed/` 的脱敏派生副本。字段名来自本机 Desktop `26.1013.40934` 安装包的
静态映射，Turn／item 外壳参考同级 `observed/canonical-before-load.sanitized.json`
和 `observed/windows-4894-start-turn.sanitized.json`。本文件从零构造了所有
ID、文本、路径、URL、状态与变更类型，没有复制任何用户对话或真实文件路径。

- `user-only-image`、`user-only-document`：纯附件正式用户项。
- `opening-empty-text-with-attachments`：`params.input[]` 只有空文字及附件，
  用来暴露既有只靠正文判定开场消息的盲点；这种附件组合尚未实测。
- `missing-path-and-image-references`：空路径、纯 fileId、data URL、HTTP(S)
  URL 及生成／查看图片缺路径。
- `generated-and-viewed-image`：生成图与查看图路径，含相对路径。
- `file-changes-with-unverified-status-values`：进行中／失败、增删改和空路径。
  所有 `synthetic-*` 状态与类型值只是哨兵，不代表 Desktop 的真实枚举。

这里只提供输入分支。状态枚举、相对路径基准、目标 Windows build 的附件
形态及可预览性仍待真实 canonical 样本核验；实现不得按本集合推断成功状态。
