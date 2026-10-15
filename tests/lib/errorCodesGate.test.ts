// @vitest-environment node
// 真实 CLI + 临时树：探针不得写进仓库源码，避免影响并行测试与开发监视器。
import { spawnSync } from "node:child_process";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { describe, expect, it } from "vitest";

const SCRIPT = path.resolve("scripts/check-error-codes.mjs");
function runGate(files: Record<string, string>, codes: string[] = []) {
  const root = mkdtempSync(path.join(os.tmpdir(), "tuvis-error-gate-"));
  const errors = Object.fromEntries(
    codes.map((code) => [
      code.toLowerCase().replace(/_([a-z])/g, (_, c: string) => c.toUpperCase()),
      "ok",
    ])
  );
  try {
    for (const [file, content] of Object.entries({
      "src-tauri/src/empty.rs": "",
      "scripts/error-code-baseline.json": JSON.stringify({ files: {} }),
      "src/i18n/locales/zh.json": JSON.stringify({ errors }),
      "src/i18n/locales/en.json": JSON.stringify({ errors }),
      ...files,
    })) {
      const target = path.join(root, file);
      mkdirSync(path.dirname(target), { recursive: true });
      writeFileSync(target, content);
    }
    const r = spawnSync(process.execPath, [SCRIPT], { cwd: root, encoding: "utf8" });
    return { status: r.status, output: r.stdout + r.stderr };
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}
const PROD = 'const KEEP: &str = "PROBE_KEEP: 生产码";';
const TEST = 'const HIDDEN: &str = "PROBE_HIDDEN: 测试码";';

describe("错误码门禁：生产/测试边界", () => {
  it.each([
    'E::Scripted { .. } => "PROBE_HIDDEN: 测试码",',
    'E::Scripted(_) => if true { "PROBE_HIDDEN: 测试码" } else { "PROBE_ELSE: 测试码" },',
  ])("match arm 的 pattern/else 不能结束测试区：%s", (arm) => {
    const r = runGate(
      {
        "src-tauri/src/probe.rs": `fn f() { match e {\n#[cfg(test)]\n${arm}\nE::Real => { ${PROD} } } }`,
      },
      ["PROBE_KEEP"]
    );
    expect(r.status, r.output).toBe(0);
  });
  it.each([
    'r##"{"a":"}"}"##',
    'r###"{"a":"}"}"###',
    'br##"{"a":"}"}"##',
    'cr##"{"a":"}"}"##',
    String.raw`"转义\" }"`,
    "'}'",
    "'\\''",
    "b'}'",
  ])("测试串 %s 不泄漏且生产码可见", (literal) => {
    const r = runGate(
      {
        "src-tauri/src/probe.rs": `#[cfg(test)]\nmod tests { const S: &str = ${literal}; ${TEST} }\n${PROD}`,
      },
      ["PROBE_KEEP"]
    );
    expect(r.output).not.toContain("PROBE_HIDDEN");
    expect(r.status, r.output).toBe(0);
  });
  it("嵌套块注释与生命周期不能提前结束测试模块", () => {
    const r = runGate(
      {
        "src-tauri/src/probe.rs": `#[cfg(test)]\nmod tests { /* a /* b } } */ c } */\nfn f<'a>(s: &'a str) {}\n${TEST}\n}\n${PROD}`,
      },
      ["PROBE_KEEP"]
    );
    expect(r.status, r.output).toBe(0);
  });
  it("enum 变体 / match arm 后的生产函数不能被删掉", () => {
    const r = runGate(
      {
        "src-tauri/src/probe.rs": `enum E {\n#[cfg(test)]\nScripted(String), Real }\nfn f() { match e {\n#[cfg(test)]\nE::Scripted(_) => (),\nE::Real => () } }\nfn wait_turn_stopped() { ${PROD} }`,
      },
      ["PROBE_KEEP"]
    );
    expect(r.status, r.output).toBe(0);
  });
  it.each([
    "test",
    "all(test, windows)",
    "all(windows, test)",
    'all(feature = "probe", test)',
    "any(test, all(test, unix))",
  ])("纯测试 cfg(%s) 必须剔除", (cfg) => {
    const r = runGate(
      {
        "src-tauri/src/probe.rs": `#[cfg(${cfg})]\n/* 注释含 { ; */\n#[allow(dead_code)]\nmod tests { ${TEST} }\n${PROD}`,
      },
      ["PROBE_KEEP"]
    );
    expect(r.status, r.output).toBe(0);
  });
  it.each(["not(test)", "any(windows, test)", "all(not(test), unix)"])(
    "生产可达 cfg(%s) 不能剔除",
    (cfg) => {
      const r = runGate({ "src-tauri/src/probe.rs": `#[cfg(${cfg})]\nmod live { ${PROD} }` });
      expect(r.status).toBe(1);
      expect(r.output).toContain("PROBE_KEEP");
    }
  );
  it("注释/原始串里的伪 cfg 不能吞生产码", () => {
    const r = runGate({
      "src-tauri/src/probe.rs": `/*\n#[cfg(test)]\n*/\nconst SAMPLE: &str = r###"\n#[cfg(test)]\nmod example {}\n"###;\n${PROD}`,
    });
    expect(r.status).toBe(1);
    expect(r.output).toContain("PROBE_KEEP");
  });
});

describe("错误码门禁：交集与诊断", () => {
  it("TS 正则内引号不能遮住后续码，模板表达式的字符串也参与扫描", () => {
    const r = runGate({ "src/probe.tsx": 'const re = /["\']/; const msg = "PROBE_KEEP: 生产码";' });
    expect(r.status).toBe(1);
    expect(r.output).toContain("PROBE_KEEP");
    const green = runGate(
      { "src/probe.tsx": 'const re = /["\']/; const msg = "PROBE_KEEP: 生产码";' },
      ["PROBE_KEEP"]
    );
    expect(green.status, green.output).toBe(0);
    const nested = runGate({
      "src/probe.tsx": 'const msg = `prefix ${flag ? "PROBE_KEEP: 生产码" : "fallback"}`;',
    });
    expect(nested.status).toBe(1);
    expect(nested.output).toContain("PROBE_KEEP");
  });
  it.each([
    'StageAbort::screen(\n"切题前读不到问答屏"\n)',
    'const MSG: &str = "切题前读不到问答屏";',
    'fn helper() -> &\'static str { "切题前读不到问答屏" }',
  ])("跨文件裸串不能漏收：%s", (raw) => {
    const r = runGate(
      {
        "src-tauri/src/coded.rs": 'const MSG: &str = "INJECT_SCREEN_ABORT: 切题前读不到问答屏";',
        "src-tauri/src/raw.rs": raw,
      },
      ["INJECT_SCREEN_ABORT"]
    );
    expect(r.status).toBe(1);
    expect(r.output).toContain("半迁移");
  });
  it("日志只豁免自身参数，不能遮住邻近错误", () => {
    const r = runGate(
      {
        "src-tauri/src/probe.rs":
          'const MSG: &str = "PROBE_KEEP: 错误正文";\nlog::warn!("诊断");\nreturn Err("错误正文");',
      },
      ["PROBE_KEEP"]
    );
    expect(r.status).toBe(1);
    expect(r.output).toContain("半迁移");
  });
  it("块注释不参与交集，路径通配符不得吞生产代码", () => {
    const r = runGate(
      {
        "src-tauri/src/probe.rs": `// monitor/*_parser.rs\n${PROD}\n// projects/*/\nconst URL: &str = "https://example.test/中文";\n/* error("生产码") /* error("生产码") */ */`,
      },
      ["PROBE_KEEP"]
    );
    expect(r.status, r.output).toBe(0);
  });
  it("FE↔FE 交集也必须可见（单引号与模板）", () => {
    const r = runGate(
      {
        "src/a.ts": "export const coded = 'PROBE_KEEP: 请求 ${endpoint} 失败';",
        "src/b.tsx": "const raw = `请求 ${name} 失败`;",
      },
      ["PROBE_KEEP"]
    );
    expect(r.status).toBe(1);
    expect(r.output).toContain("半迁移");
  });
  it("重复前缀不崩，缺键与半迁移诊断仍跑完", () => {
    const r = runGate({
      "src-tauri/src/probe.rs":
        'const MSG: &str = "PROBE_DUP: PROBE_DUP: 中文";\nconst CODED: &str = "PROBE_KEEP: 错误正文";\nreturn Err("错误正文");',
    });
    expect(r.status).toBe(1);
    expect(r.output).not.toContain("ReferenceError");
    expect(r.output).toContain("重复码前缀");
    expect(r.output).toContain("缺键");
    expect(r.output).toContain("半迁移");
  });
  it("用量 detail 中文必须被门禁拒绝", () => {
    const r = runGate({
      "src-tauri/src/services/usage/probe.rs":
        'fn f() { UsageError::new("usage-source-io", format!("读取 {} 失败", path.display())); }',
    });
    expect(r.status).toBe(1);
    expect(r.output).toContain("用量 detail");
  });

  it("用量 internal 包装同受 detail 门禁约束，测试夹具不受限制", () => {
    const r = runGate({
      "src-tauri/src/probe.rs":
        'fn f() { UsageError::internal(format!("查询任务异常: {e}")); }\n#[cfg(test)]\nmod tests { fn f() { UsageError::new("usage-source-io", "中文夹具"); } }',
    });
    expect(r.status).toBe(1);
    expect(r.output).toContain("用量 detail");
    expect(r.output).not.toContain("中文夹具");
    const green = runGate({
      "src-tauri/src/probe.rs":
        'fn f() { UsageError::new("usage-source-io", format!("read {} failed: {e}", path.display())); }',
    });
    expect(green.status, green.output).toBe(0);
  });

  it("按文件棘轮拒绝新增裸串，补码/减少裸串允许通过", () => {
    const files = {
      "scripts/error-code-baseline.json": JSON.stringify({
        files: { "src-tauri/src/probe.rs": 0 },
      }),
    };
    const red = runGate({ ...files, "src-tauri/src/probe.rs": 'return Err("新错误正文");' });
    expect(red.status).toBe(1);
    expect(red.output).toContain("棘轮上升");
    const green = runGate({ ...files, "src-tauri/src/probe.rs": PROD }, ["PROBE_KEEP"]);
    expect(green.status, green.output).toBe(0);
    const missing = runGate({ "scripts/error-code-baseline.json": "null" });
    expect(missing.status).toBe(1);
    expect(missing.output).toContain("迁移基线");
  });

  it("不同插值参数的不同码不误判同串多码，逐字同句必须拒绝", () => {
    const green = runGate(
      {
        "src-tauri/src/probe.rs":
          'const A: &str = "PROBE_ONE: {stage}；请核对（{reason}）"; const B: &str = "PROBE_TWO: {msg}；请核对（{hint}）";',
      },
      ["PROBE_ONE", "PROBE_TWO"]
    );
    expect(green.status, green.output).toBe(0);
    const red = runGate(
      {
        "src-tauri/src/probe.rs":
          'const A: &str = "PROBE_ONE: 正文"; const B: &str = "PROBE_TWO: 正文";',
      },
      ["PROBE_ONE", "PROBE_TWO"]
    );
    expect(red.status).toBe(1);
    expect(red.output).toContain("映射了多个码");
  });
});
