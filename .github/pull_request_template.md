## 变更描述

## 变更类型
- [ ] 新功能（feat）
- [ ] Bug 修复（fix）
- [ ] 文档更新（docs）
- [ ] 代码重构（refactor）
- [ ] 测试（test）
- [ ] 工程化（chore）

## 测试
- [ ] 已跑 `bash scripts/ci-backend-local.sh`（等同 CI backend job：含 `--features marker-helper` 与 Windows 交叉覆盖——本机裸 `cargo test` 无 feature，全绿推不出 CI 会绿，见 #127/#128）
- [ ] `pnpm test` 通过
- [ ] `pnpm check` 通过
- [ ] 手动验证通过

## 检查清单
- [ ] 代码已自审
- [ ] 相关文档已更新
- [ ] CHANGELOG 已更新（如有用户可见变更）
- [ ] 无安全风险
