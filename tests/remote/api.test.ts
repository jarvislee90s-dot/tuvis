// tests/remote/api.test.ts — M5 A6：远程命令封装的锁形测试（重写后口径）。
// 命令名与参数形态是前端 ↔ Rust（src-tauri/src/remote/mod.rs #[tauri::command]）的
// 隐式契约：字符串漂移后 invoke 只会静默 resolve undefined，不报错——必须在此锁定。
// M5 A3 起 /pair/pin 认证制下线审批/直通命令：remote_issue_token / remote_pending_requests /
// remote_approve_request / remote_set_channel 的封装已随 A6 删除，此处不再出现。
// mock 模式仿 tests/session/dismissCard.test.tsx（vi.hoisted + vi.mock，自带一份，
// 不依赖 setup.ts 全局 tauriInvokeMock 的 default 分支）。
import { beforeEach, describe, expect, it, vi } from "vitest";

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));

vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));

import {
  remoteConfirmPublic,
  remoteDevices,
  remoteRevokeAllDevices,
  remoteRevokeDevice,
  remoteStatus,
  remoteToggle,
  remoteTsProbe,
  remoteTsRunStep,
  remoteNamedProbe,
  remoteNamedSetAddress,
  remoteNamedVerify,
  remoteTsRevealDownloads,
  renameDevice,
  resetDevices,
  setPin,
  toggleChannel,
} from "@/lib/api/remote";
import {
  forgetZcodeAuthorization,
  getZcodeAuthStatus,
  getZcodeRemoteConfig,
  setZcodeRemoteConfig,
} from "@/lib/api/zcodeRemote";

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockResolvedValue(undefined);
});

describe("remote api wrappers", () => {
  it("Windows 重定向下载目录仅定位父目录，保留含空格路径", async () => {
    await remoteTsRevealDownloads("D:\\My Downloads\\tailscale-setup-1.102.4-amd64.msi");
    expect(invokeMock).toHaveBeenCalledWith("reveal_dir", { path: "D:\\My Downloads" });
  });

  it("Windows 下载目录重定向到盘符根时保留根分隔符", async () => {
    await remoteTsRevealDownloads("D:\\tailscale.msi");
    expect(invokeMock).toHaveBeenCalledWith("reveal_dir", { path: "D:\\" });
  });
  it("remoteStatus 无参调用 remote_status，透传返回值", async () => {
    const status = {
      enabled: true,
      maxDevices: 10,
      channels: {
        local: { running: true, address: "http://127.0.0.1:9420/m" },
        lan: { enabled: true, running: true, addresses: ["http://192.168.1.5:9420/m"] },
        quick: { enabled: false, running: false, address: null, error: null },
        named: {
          enabled: false,
          running: false,
          connected: false,
          address: null,
          error: null,
          configKey: "named-config",
          reach: { state: "unverified" },
          waiting: null,
        },
        // §C1/§C3（Task 5/7）：tailscale 段随四通道载荷透出，reach 随段
        tailscale: {
          enabled: false,
          running: false,
          address: null,
          error: null,
          reach: { state: "unverified" },
          configKey: "tailscale-config",
          waiting: null,
        },
      },
      pin: "4827",
    };
    invokeMock.mockResolvedValue(status);
    expect(await remoteStatus()).toEqual(status);
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith("remote_status");
  });

  it("remoteToggle 以 { enabled } 形态传参（Rust 端 enabled: bool）", async () => {
    await remoteToggle(true);
    expect(invokeMock).toHaveBeenCalledWith("remote_toggle", { enabled: true });
    await remoteToggle(false);
    expect(invokeMock).toHaveBeenCalledWith("remote_toggle", { enabled: false });
    expect(invokeMock).toHaveBeenCalledTimes(2);
  });

  it("toggleChannel 以 (channel, on) 字面形态传参（remote_toggle_channel(channel, on)）", async () => {
    await toggleChannel("lan", true);
    expect(invokeMock).toHaveBeenCalledWith("remote_toggle_channel", { channel: "lan", on: true });
    await toggleChannel("quick", false);
    expect(invokeMock).toHaveBeenCalledWith("remote_toggle_channel", {
      channel: "quick",
      on: false,
    });
    await toggleChannel("named", true);
    expect(invokeMock).toHaveBeenCalledWith("remote_toggle_channel", {
      channel: "named",
      on: true,
    });
    // §C1：tailscale 是第四条可开关通道（Funnel 起停，与隧道互斥触达由后端保证）
    await toggleChannel("tailscale", true);
    expect(invokeMock).toHaveBeenCalledWith("remote_toggle_channel", {
      channel: "tailscale",
      on: true,
    });
  });

  it("setPin 以 { pin } 形态传参（remote_set_pin(pin: String)）", async () => {
    await setPin("4827");
    expect(invokeMock).toHaveBeenCalledWith("remote_set_pin", { pin: "4827" });
  });

  it("resetDevices 无参调用 remote_reset_devices（吊销全部，不改 PIN）", async () => {
    await resetDevices();
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith("remote_reset_devices");
  });

  it("renameDevice 以 { id, name } 形态传参（remote_rename_device(id, name)）", async () => {
    await renameDevice("d1", "我的手机");
    expect(invokeMock).toHaveBeenCalledWith("remote_rename_device", { id: "d1", name: "我的手机" });
  });

  it("remoteConfirmPublic 无参调用 remote_confirm_public", async () => {
    await remoteConfirmPublic();
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith("remote_confirm_public");
  });

  it("设备命令三封装：花名册 / 单吊销 / 全吊销（命令去留由 A8 裁决，契约先锁定）", async () => {
    invokeMock.mockResolvedValueOnce([{ id: "d1", name: "phone", online: true }]);
    expect(await remoteDevices()).toEqual([{ id: "d1", name: "phone", online: true }]);
    expect(invokeMock).toHaveBeenCalledWith("remote_devices");
    await remoteRevokeDevice("d1");
    expect(invokeMock).toHaveBeenCalledWith("remote_revoke_device", { id: "d1" });
    await remoteRevokeAllDevices();
    expect(invokeMock).toHaveBeenCalledWith("remote_revoke_all_devices");
  });

  it("向导双命令锁形（§C2/§C3）：remote_ts_probe 无参；remote_ts_run_step 以 { step } 传参", async () => {
    await remoteTsProbe();
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith("remote_ts_probe");
    const stepResult = { ok: true, approvalUrl: "https://login.tailscale.com/a/xyz" };
    invokeMock.mockResolvedValueOnce(stepResult);
    expect(await remoteTsRunStep("funnel")).toEqual(stepResult);
    expect(invokeMock).toHaveBeenCalledWith("remote_ts_run_step", { step: "funnel" });
  });

  it("named 向导 API 只通过固定 Tauri 命令取状态、保存地址与只读验证", async () => {
    const probe = {
      configKey: "cfg-1",
      hasToken: true,
      enabled: true,
      running: true,
      connected: true,
      originUrl: "http://127.0.0.1:9420",
      address: "https://remote.example.com/m",
      lastAddress: null,
      reach: { state: "unverified" },
      waiting: null,
    };
    invokeMock.mockResolvedValueOnce(probe);
    expect(await remoteNamedProbe()).toEqual(probe);
    expect(invokeMock).toHaveBeenLastCalledWith("remote_named_probe");

    invokeMock.mockResolvedValueOnce(probe);
    expect(await remoteNamedSetAddress("https://remote.example.com/m")).toEqual(probe);
    expect(invokeMock).toHaveBeenLastCalledWith("remote_named_set_address", {
      address: "https://remote.example.com/m",
    });

    invokeMock.mockResolvedValueOnce(probe);
    expect(await remoteNamedVerify()).toEqual(probe);
    expect(invokeMock).toHaveBeenLastCalledWith("remote_named_verify");
  });
});

// A4：ZCode 官方远控链接配置的桌面 IPC 封装锁形。命令名/参数形态与 Rust 端
// commands::zcode_remote（zcode_remote_config_get() / zcode_remote_config_set(raw_url)）
// 是隐式契约；wire DTO 与 Rust ZcodeRemoteConfig（serde camelCase）逐字段一致。
// 夹具只用合成 sid，不含任何真实用户链接。
describe("zcode remote config api wrappers", () => {
  it("getZcodeRemoteConfig 无参调用 zcode_remote_config_get，透传 Saved wire DTO", async () => {
    const saved = {
      configState: "saved",
      url: "https://zcode.z.ai/remote/v4?sid=synthetic-sid-a4-ts-0001",
    };
    invokeMock.mockResolvedValueOnce(saved);
    expect(await getZcodeRemoteConfig()).toEqual(saved);
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith("zcode_remote_config_get");
  });

  it("setZcodeRemoteConfig 以 { rawUrl } 形态传参（Rust raw_url: String）；空白=删除同源回 missing/null", async () => {
    const saved = {
      configState: "saved",
      url: "https://zcode.z.ai/remote/v4?sid=synthetic-sid-a4-ts-0002",
    };
    invokeMock.mockResolvedValueOnce(saved);
    expect(await setZcodeRemoteConfig(saved.url)).toEqual(saved);
    expect(invokeMock).toHaveBeenLastCalledWith("zcode_remote_config_set", { rawUrl: saved.url });

    invokeMock.mockResolvedValueOnce({ configState: "missing", url: null });
    expect(await setZcodeRemoteConfig("   ")).toEqual({ configState: "missing", url: null });
    expect(invokeMock).toHaveBeenLastCalledWith("zcode_remote_config_set", { rawUrl: "   " });
  });

  it("保存失败透传后端错误码串（ZCODE_URL_* 前缀、不含原链接）", async () => {
    invokeMock.mockRejectedValueOnce("ZCODE_URL_HOST: 仅支持 zcode.z.ai 官方主机");
    await expect(
      setZcodeRemoteConfig("https://evil.example/remote/v4?sid=synthetic-sid-a4-ts-0003")
    ).rejects.toBe("ZCODE_URL_HOST: 仅支持 zcode.z.ai 官方主机");
  });
});

// C4：记住授权相关的桌面 IPC 封装锁形。命令名/参数形态与 Rust
// commands::zcode_remote（zcode_control_auth_status() /
// zcode_control_forget_authorization()）是隐式契约；AuthStatus 为 camelCase，
// 词表外的 problem 由调用方按读取失败处理。夹具全合成，不含真实 JWT/链接。
describe("zcode auth api wrappers (C4)", () => {
  it("getZcodeAuthStatus 无参调用 zcode_control_auth_status，透传 AuthStatus（camelCase）", async () => {
    const status = {
      authorized: true,
      remembered: true,
      restoring: false,
      provider: "bigmodel",
      problem: null,
    };
    invokeMock.mockResolvedValueOnce(status);
    expect(await getZcodeAuthStatus()).toEqual(status);
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith("zcode_control_auth_status");
    expect(Object.keys(status)).toEqual([
      "authorized",
      "remembered",
      "restoring",
      "provider",
      "problem",
    ]);
  });

  it("forgetZcodeAuthorization 无参调用 zcode_control_forget_authorization，返回 ConnectionView", async () => {
    const view = {
      state: "needs_auth",
      generation: 9,
      reasonCode: null,
      deviceLabel: null,
    };
    invokeMock.mockResolvedValueOnce(view);
    expect(await forgetZcodeAuthorization()).toEqual(view);
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith("zcode_control_forget_authorization");
  });

  it("forget 失败透传后端码前缀（ZCODE_CONTROL:*），前端不伪造成功", async () => {
    invokeMock.mockRejectedValueOnce("ZCODE_CONTROL:storage_failed");
    await expect(forgetZcodeAuthorization()).rejects.toBe("ZCODE_CONTROL:storage_failed");
  });
});
