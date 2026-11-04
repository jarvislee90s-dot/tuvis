#!/usr/bin/env node
// 用已存在的 Apple 代码签名身份构建 debug .app：只生成 Tauri bundle 配置，
// 不安装/列举证书、不读私钥、不修改 Keychain ACL、不启动 App。
import { spawn } from "node:child_process";
import { pathToFileURL } from "node:url";

export function createSignedBuildPlan({
  platform = process.platform,
  identity = process.env.TUVIS_APPLE_SIGN_IDENTITY,
  projectRoot = process.cwd(),
} = {}) {
  if (platform !== "darwin") {
    throw new Error("signed macOS build is only available on darwin");
  }
  if (typeof identity !== "string" || identity.trim() === "") {
    throw new Error("TUVIS_APPLE_SIGN_IDENTITY is required");
  }
  const config = JSON.stringify({ bundle: { macOS: { signingIdentity: identity } } });
  return {
    command: "pnpm",
    args: ["exec", "tauri", "build", "--debug", "--bundles", "app", "--config", config],
    cwd: projectRoot,
  };
}

function defaultRunner(command, args, options) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, options);
    child.on("error", reject);
    child.on("exit", (code) => resolve({ code: code ?? 1 }));
  });
}

export async function runSignedBuild(plan, runner = defaultRunner) {
  const result = await runner(plan.command, plan.args, {
    cwd: plan.cwd,
    shell: false,
    stdio: "inherit",
  });
  if (result && typeof result.code === "number" && result.code !== 0) {
    throw new Error(`tauri build failed with exit code ${result.code}`);
  }
  return result;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    await runSignedBuild(createSignedBuildPlan());
  } catch (error) {
    console.error(String(error instanceof Error ? error.message : error));
    process.exitCode = 1;
  }
}
