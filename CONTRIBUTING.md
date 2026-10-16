# 贡献指南

## 开发环境

### 前置要求
- Node.js ≥ 22.22.2、pnpm 11、Rust ≥ 1.89
  （下限不是拍脑袋写的：Node 由 `jsdom 30` 的 engines 决定、Rust 由代码实际用到的
  `File::unlock`(1.89) 决定，声明在 `package.json` 的 `engines` 与 `src-tauri/Cargo.toml`
  的 `rust-version`；来源与门禁说明见 `docs/DEPENDENCY-UPDATES.md` §6）

### 搭建步骤
```bash
git clone https://github.com/jarvislee90s-dot/tuvis.git
cd tuvis
pnpm install
pnpm tauri:dev
```

## 代码规范
- Rust: rustfmt + cargo clippy 无警告
- TypeScript: Prettier + ESLint
- 提交信息: Conventional Commits (feat/fix/docs/refactor/test/chore)

## PR 流程
> 处理依赖升级（dependabot）PR 前请先读 `docs/DEPENDENCY-UPDATES.md`：那里有月度批处理
> 流程、`Tauri version parity` 门禁的含义、以及各类红灯的处置表。

1. Fork 并创建分支: `git checkout -b feat/your-feature`
2. 确保测试通过: `bash scripts/ci-backend-local.sh && pnpm test`
3. 提交 PR，填写 PR 模板

## 本地测试
```bash
pnpm check          # format + lint + build
pnpm test           # vitest
bash scripts/ci-backend-local.sh   # 后端门禁：与 CI backend job 同口径（含 --features
                                   # marker-helper 与 Windows 交叉编译覆盖，见 #127/#128
                                   # ——本机 `cargo check && cargo test && cargo clippy`
                                   # 不带 feature，全绿推不出 CI 会绿）
```
