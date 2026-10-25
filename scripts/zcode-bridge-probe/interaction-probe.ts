#!/usr/bin/env node
// B3 交互探针薄包装：位置参数 <approve|reject|answer|interrupt> → --interaction=<mode>，
// 其余 flags 原样透传给 session-probe（同一连接/映射/证据机制，零重复逻辑）。
// 用法：node scripts/zcode-bridge-probe/interaction-probe.ts <approve|reject|answer|interrupt|select> \
//         --i-know-this-connects --interaction-authorized [--provider=bigmodel] [--open]
// 缺 --interaction-authorized → 映射后只打印 WOULD-interact，绝不发送。

import { main } from "./session-probe.ts";

const KNOWN_MODES = new Set(["approve", "reject", "answer", "interrupt", "select"]);

const argv = [...process.argv.slice(2)];
const head = argv[0];
// I7：必须显式给出位置参数模式——缺省/只给 flags 时绝不静默退化成 B2 发送探针。
if (head === undefined || head.startsWith("-")) {
  console.error(
    "interaction-probe: 用法：interaction-probe.ts <approve|reject|answer|interrupt|select> [flags]\n" +
      "  （位置参数模式必填；交互探针不会退化为 B2 发送探针）"
  );
  process.exit(2);
}
if (!KNOWN_MODES.has(head)) {
  console.error(
    "interaction-probe: 未知交互模式（只接受 approve | reject | answer | interrupt | select）"
  );
  process.exit(2);
}
argv.shift();
argv.unshift(`--interaction=${head}`);

void main(argv);
