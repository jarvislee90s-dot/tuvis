/**
 * T03 公共常量与产物清单形状。
 *
 * bindingProtocol 是本插件与兔维斯 Rust 端（T04/T06）之间本机授权握手
 * 的协议代际号：任何握手字段变更都必须递增，旧代际消息一律拒绝。
 */

/** 本机授权握手协议代际（计划 §全局约束：签名消息固定字段数组含 bindingProtocol）。 */
export const BINDING_PROTOCOL = 2 as const;

/** 发版产物 manifest（resources/dsh/manifest.json）的固定形状。 */
export interface ReleaseManifest {
  /** 插件包版本（与 package.json version 一致）。 */
  pluginVersion: string;
  /** remote-access.tgz 的 SHA-256（hex，小写）。 */
  sha256: string;
  /** 本机授权握手协议代际。 */
  bindingProtocol: typeof BINDING_PROTOCOL;
}
