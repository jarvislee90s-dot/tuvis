import { describe, expect, it, vi } from "vitest";
import { createSignedBuildPlan, runSignedBuild } from "../../scripts/zcode-signed-build.mjs";

const ROOT = "/tmp/tuvis-synthetic";
const IDENTITY = 'Developer ID Application: Synthetic "Quoted"';

describe("zcode signed debug build", () => {
  it("identity_is_a_single_config_argument", () => {
    const plan = createSignedBuildPlan({
      platform: "darwin",
      identity: IDENTITY,
      projectRoot: ROOT,
    });
    expect(plan.cwd).toBe(ROOT);
    expect(plan.command).toBe("pnpm");
    expect(plan.args.slice(0, 7)).toEqual([
      "exec",
      "tauri",
      "build",
      "--debug",
      "--bundles",
      "app",
      "--config",
    ]);
    expect(plan.args).toHaveLength(8);
    expect(JSON.parse(plan.args[7]).bundle.macOS.signingIdentity).toBe(IDENTITY);
  });

  it("missing_identity_never_runs_build", async () => {
    const runner = vi.fn();
    expect(() =>
      createSignedBuildPlan({ platform: "darwin", identity: "", projectRoot: ROOT })
    ).toThrow();
    expect(runner).not.toHaveBeenCalled();
  });

  it("non_macos_has_no_side_effect", () => {
    expect(() =>
      createSignedBuildPlan({ platform: "linux", identity: IDENTITY, projectRoot: ROOT })
    ).toThrow();
  });

  it("signed_build_does_not_launch_app_or_touch_acl", async () => {
    const plan = createSignedBuildPlan({
      platform: "darwin",
      identity: IDENTITY,
      projectRoot: ROOT,
    });
    const runner = vi.fn(async () => ({ code: 0 }));
    await runSignedBuild(plan, runner);
    expect(runner).toHaveBeenCalledTimes(1);
    const [command, args, options] = runner.mock.calls[0];
    expect(command).toBe("pnpm");
    expect(args).toEqual(plan.args);
    expect(options).toMatchObject({ cwd: ROOT, shell: false });
    expect(args.join(" ")).not.toMatch(/\bsecurity\b|open\s+-a|Keychain|ACL/);
  });
});
