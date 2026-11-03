# 第五轮验收后的收尾改进清单（round-5 follow-ups）

- 日期：2026-09-13
- 来源：issue #43 第五轮 Windows 实机验收（49aed8b 复验全过）评论的备注项
- 性质：小型收尾改动集（两项代码 + 一项可选前端 + 流程项），非新 spec；执行者照单实现即可
- 前置：分支 `feat/marker-and-issue-batch`，基于 49aed8b，先 `git pull`

---

## F1（P2，建议本轮做）：marker 防叠加 + 跳转聚焦后清除

**问题**（round-4 3d 实测 + round-5 备注 6）：codex 窗口标题静态不自愈，marker 一旦贴上持续残留；不同会话先后跳转同一窗口时 ` — MAM:xxx` 会**叠加**（实测出现过双 marker 标题）。claude 靠 TUI 重写自愈，codex 无自愈路径。

**方案**：注入侧防叠加（贴新前剥旧）+ 消费侧清痕（聚焦成功后把 marker 清掉，标题回到干净状态；下次跳转重新注入——按需注入本就是一次性模式，清痕不损失任何信息）。

**Files:**
- Modify: `src-tauri/src/bin/mam-marker.rs`
- Modify: `src-tauri/src/window/win32.rs`
- Modify: `src-tauri/src/commands/session.rs`

**实现要点：**

1. helper 纯函数改造（`mam-marker.rs`，替换 `append_marker_to_title`）：

```rust
/// 剥掉标题尾部全部 marker 残留（` — MAM:<hex>` 可叠加多层，循环剥净；
/// 只剥尾部、且仅剥 MAM: 前缀形态，用户标题正文不受影响）
fn strip_marker_suffix(title: &str) -> String {
    let mut t = title.to_string();
    while let Some(pos) = t.rfind(" — MAM:") {
        let tail = &t[pos + " — MAM:".len()..];
        // 仅当尾部是完整 marker 形态（MAM: + 1..16 个十六进制字符）才剥，防误伤
        if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_hexdigit()) {
            t.truncate(pos);
            t = t.trim_end().to_string();
        } else {
            break;
        }
    }
    t
}

/// 标题写入：Some(marker) = 剥旧贴新（防叠加）；None = 仅剥旧（--clear 模式）
fn title_with_marker(title: &str, marker: Option<&str>) -> String {
    let base = strip_marker_suffix(title);
    match marker {
        None => base,
        Some(m) if base.trim().is_empty() => m.to_string(),
        Some(m) => format!("{base} — {m}"),
    }
}
```

2. CLI 扩展：`mam-marker --pid <pid> <session_id>` 注入（现有）；`mam-marker --pid <pid> --clear` 清除（无 session_id）。`parse_marker_args` 返回 `Option<(u32, Option<String>)>`——`--clear` 出现时 session_id 位为 None。route B/A 内部的 `append_marker_to_title(title, marker)` 全部改为 `title_with_marker(&title, marker_opt)`（`run` 把 `Option<String>` 透传下去）。

3. `win32.rs`：把 `inject_marker_on_demand` 里的 spawn 段抽成私有 `spawn_marker_helper(args: &[&str]) -> bool`（helper 在场检查 + CREATE_NO_WINDOW + null stdio），原函数复用；新增：

```rust
/// 聚焦成功后清痕：对目标会话终端执行 --clear（剥掉本次注入的 marker）。
/// 全部失败静默（残留 marker 无功能影响，仅视觉）；仅在注入门控内调用
pub fn clear_marker_after_focus(pid: u32) {
    spawn_marker_helper(&["--pid", &pid.to_string(), "--clear"]);
}
```

4. `session.rs`：`FocusOutcome::Focused` 分支（:176 附近）在 `mark_read_on_jump` 前加：

```rust
                if on_demand_marker_applies(agent_type.as_deref(), form.as_deref()) {
                    crate::window::win32::clear_marker_after_focus(pid);
                }
```

（清除范围与注入门控一致；Ambiguous 分支不清——marker 还在选择器候选标题上，供用户辨认，关选择器后自然过期/下次注入时被剥。）

**测试**（mac 可跑）：`strip_marker_suffix`/`title_with_marker` 纯函数测试——单 marker 剥净、双 marker 叠加剥净、无 marker 原样、` — MAM:` 后非 hex 尾巴不剥、空标题+Some=marker 本体；`parse_marker_args` 补 `--clear` 形态两例。

**Commit**: `fix(jump): marker dedup on inject, clear after focus (#43 round-5)`

---

## F2（P3，可选）：选择器候选弱化非目标窗口

**问题**（round-5 备注 6）：歧义选择器候选含其他工具窗口（如 claude 卡的候选里混入无人认领的其他窗口），score=0 且 uiaPrefix=0 排后但仍同等视觉权重，人工挑时略费眼。

**方案**（纯前端、低风险）：`src/components/sessions/SessionCard.tsx:224` 选择器按钮，`score === 0 && uiaPrefix === 0` 的候选加视觉弱化（如 `opacity-60`），排序保持现状（后端已按分降序）。**不做后端过滤**——评分素材缺失时（无 lastMessage、标题无项目名）真目标可能恰好 0 分，过滤会误杀，弱化则无此风险。

**Commit**: `style(ui): de-emphasize zero-score window picker candidates (#43 round-5)`

---

## F3（流程项，需用户拍板，不由执行 agent 自行决定）

1. **PR #56 转正并合并**：第五轮报告结论「具备合并条件」。merge 后 `Closes #45 #47 #49 #50 #51 #52` 由 PR 描述自动关闭；**#43 待三张目视图补拍后再手动关**（1a TUI 无红框、1d 黄灯宽限、3c 点选后物理聚焦——行为证据已闭环，目视属最后确认）。
2. **新 issue：会话-进程配对正向证据**（round-4 3c 根因的长期解）：同 cwd 多进程时引入正向证据配对（候选：codex rollout 内的时间戳×进程启动时间、父链窗口标题快照对齐），配对可信后 codex 同项目双开可从「选择器」升级「自动锁对」。草稿要点：背景（phase1_match 枚举序×mtime 交叉）、round-4 3c 证据链、临时缓解（require_evidence 禁注入已上线）、长期方向与验收口径。建立后 **#48 一并关闭**（短期缓解已含、根治转移至新 issue）。
3. **OpenCode 工具开关目标状态确认**（round-5 备注 4）：验收中为出卡临时启用并保留，用户在 设置→工具管理 里确认是留是关。
4. 遗留测试产物清理（round-5 备注 5）：`~/mam-accept-*` 目录、各工具 sessions/DB 中的测试会话、codex 窗口标题历史 marker（重启终端即消失）。

## F4（记录，不改代码）

- kimi resume 会话跳转落选择器：素材不足（title=原始 sid、lastMessage 短）时「宁弹不锁错」是设计内行为，非回归（round-5 D 项确认）。
- 三张目视图待补拍：下次自然使用时顺手截图贴 #43 即可，不阻塞任何事项。
