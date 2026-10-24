#!/usr/bin/env node
// B3 交互探针薄包装：位置参数 <approve|reject|answer|interrupt> → --interaction=<mode>，
// 其余 flags 原样透传给 session-probe（同一连接/映射/证据机制，零重复逻辑）。
// 用法：node scripts/zcode-bridge-probe/interaction-probe.ts approve \
//         --i-know-this-connects --interaction-authorized [--provider=bigmodel] [--open]
// 缺 --interaction-authorized → 映射后只打印 WOULD-interact，绝不发送。

import { main } from "./session-probe.ts";

const KNOWN_MODES = new Set(["approve", "reject", "answer", "interrupt"]);

const argv = [...process.argv.slice(2)];
const head = argv[0];
if (head !== undefined && !head.startsWith("-")) {
  if (!KNOWN_MODES.has(head)) {
    console.error(
      `interaction-probe: 未知交互模式「${head}」（只接受 approve | reject | answer | interrupt）`
    );
    process.exit(2);
  }
  argv.shift();
  argv.unshift(`--interaction=${head}`);
}

void main(argv);
