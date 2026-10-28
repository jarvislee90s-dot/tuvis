# 2026-10-26 Mac 追加探测（F03 专项）· 脱敏样本

来源：`~/Documents/tuvis-verify-logs/codex-f03-mac-followup-20261026T203938+0800/`
（Desktop `26.1021.31345`、随包 CLI `0.164.0-alpha.11.1`，与主探测同一指纹）。
只复制了 `sanitized/` 下的 6 份；raw、回执与截图留在仓外，**不提交**。

| 文件                                | 用途                                                                            |
| ----------------------------------- | ------------------------------------------------------------------------------- |
| `h-freecompact.json`                | 空闲异步自由回答闭环；回执含超时形态（"超时可能已经落地"）                      |
| `f03-answer-route-static.json`      | 答题路由静态取证（信封合成/还原规则）                                           |
| `questionitemid-format-hazard.json` | `questionItemId` 必须紧凑（`separators=(',',':')`），带空格即失配               |
| `pairing-exact-vs-spaced.json`      | 精确配对 vs 带空格配对的对照                                                    |
| `j-decline.json`                    | 用户拒绝后的 `declined` 终态（不是"仍在运行"）                                  |
| `f04-decline-and-permissions.json`  | 未 offered 的 `decline` 被接受的单次样本；"请求权限"标题不等于 permissions 协议 |

样本中 ID 已替换为占位符、文本已替换；不得据此声称 Windows 已验（Windows 追加
运行缺脱敏归档，见同级 `../20261026-macos/README.md`）。
