import { invoke } from "@tauri-apps/api/core";

// A4：ZCode 官方远控链接配置（桌面端 IPC 封装）。
// wire DTO 与 Rust `services::zcode_control::ZcodeRemoteConfig`（serde camelCase）
// 逐字段一致；命令定义见 src-tauri/src/commands/zcode_remote.rs——命令名/参数形态
// 是隐式契约，由 tests/remote/api.test.ts 锁形。
//
// 语义红线（设计 §4.3/§5.2）：`saved` 只表示**格式已校验并落库**，绝不表示已连接
// 官方服务或已取得官方身份——消费方不得据此渲染「已联机」；`invalid` 表示历史值
// 复验不通过，`url` 恒为 null（坏值不下发）。

export type ZcodeConfigState = "missing" | "saved" | "invalid";

export type ZcodeRemoteConfig = {
  configState: ZcodeConfigState;
  url: string | null;
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
