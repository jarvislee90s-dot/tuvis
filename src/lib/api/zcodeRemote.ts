import { invoke } from "@tauri-apps/api/core";
import type { ConnectionView } from "@/lib/api/zcodeControl";

// A4：ZCode 官方远控链接配置（桌面端 IPC 封装）。
// wire DTO 与 Rust `services::zcode_control::ZcodeRemoteConfig`（serde camelCase）
// 逐字段一致；命令定义见 src-tauri/src/commands/zcode_remote.rs——命令名/参数形态
// 是隐式契约，由 tests/remote/api.test.ts 锁形。
//
// 语义红线（设计 §4.3/§5.2）：`saved` 只表示**格式已校验并落库**，绝不表示已连接
// 官方服务或已取得官方身份——消费方不得据此渲染「已联机」；`invalid` 表示历史值
// 复验不通过，`url` 恒为 null（坏值不下发）。
//
// C4 起新增桌面授权状态 DTO 与「忘记授权」入口（凭据复用设计 §4.1/§6）：
// - `AuthStatus` 是唯一授权 DTO（camelCase；`AuthProblem` 为 snake_case），
//   **不含 JWT**；`auth_status` 只读内存快照，不读保险箱、不触网；
// - 忘记授权返回 `ConnectionView`（链接保留，重新授权后再连接）。

export type ZcodeConfigState = "missing" | "saved" | "invalid";

export type ZcodeRemoteConfig = {
  configState: ZcodeConfigState;
  url: string | null;
};

/** 与 Rust `AuthProblem`（serde snake_case）逐字对应；词表外值不猜。 */
export const ZCODE_AUTH_PROBLEMS = [
  "expired",
  "revoked",
  "locked_or_denied",
  "unavailable",
  "storage_failed",
  "delete_failed",
  "invalid_record",
  "too_large",
] as const;

export type ZcodeAuthProblem = (typeof ZCODE_AUTH_PROBLEMS)[number];

const ZCODE_AUTH_PROBLEM_SET: ReadonlySet<string> = new Set(ZCODE_AUTH_PROBLEMS);

/** 词表守卫：词表外的 problem 一律不认（调用方按读取失败处理，不猜原因）。 */
export function isZcodeAuthProblem(value: unknown): value is ZcodeAuthProblem {
  return typeof value === "string" && ZCODE_AUTH_PROBLEM_SET.has(value);
}

/** 与 Rust `AuthStatus`（serde camelCase）逐字段一致；**绝不含 JWT**。 */
export type ZcodeAuthStatus = {
  /** 当前进程内存里是否有账号 JWT。 */
  authorized: boolean;
  /** 内存授权是否已成功写入系统存储（写失败不假报）。 */
  remembered: boolean;
  /** 是否正在从系统存储恢复。 */
  restoring: boolean;
  /** 当前凭据所属 OAuth 通道。 */
  provider: "bigmodel" | "zai" | null;
  /** 最近一次失效/存储问题的可见原因（封闭枚举，不含原生错误）。 */
  problem: ZcodeAuthProblem | null;
};

/** 读取本机 ZCode 官方远控链接配置（服务端读取即复验；只读 settings，不触网）。 */
export async function getZcodeRemoteConfig(): Promise<ZcodeRemoteConfig> {
  return await invoke<ZcodeRemoteConfig>("zcode_remote_config_get");
}

/**
 * 保存 ZCode 官方远控链接（空白 = 删除，与删除同源）。校验单点在服务端：
 * 失败 reject 带结构化码前缀（`ZCODE_URL_*`，见 src-tauri/src/errcode.rs 约定）
 * 且不携带原链接；失败保存不影响已存合法值。成功后返回服务端规范化 DTO。
 */
export async function setZcodeRemoteConfig(rawUrl: string): Promise<ZcodeRemoteConfig> {
  return await invoke<ZcodeRemoteConfig>("zcode_remote_config_set", { rawUrl });
}

/**
 * 只读授权状态（只读内存快照；不读保险箱、不触网）。读取失败/形态不符由调用方按
 * 「未知」显示，绝不猜成已授权或需要授权。
 */
export async function getZcodeAuthStatus(): Promise<ZcodeAuthStatus> {
  return await invoke<ZcodeAuthStatus>("zcode_control_auth_status");
}

/**
 * 忘记授权：清进程内存并删除系统存储里的账号记录，**保留设备链接**。
 * 删除失败 reject 带 `ZCODE_CONTROL:*` 码前缀；成功后返回当前连接视图
 * （链接仍在，重新授权后可再连接）。
 */
export async function forgetZcodeAuthorization(): Promise<ConnectionView> {
  return await invoke<ConnectionView>("zcode_control_forget_authorization");
}
