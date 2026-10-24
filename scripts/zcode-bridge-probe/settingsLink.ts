// scripts/zcode-bridge-probe/settingsLink.ts — 只读读取 Tuvis settings 里的官方远控链接。
//
// 归属：工作包 B 探针工具目录，不在产品运行链上。
// 读取口径：
//   - DB：`~/.tuvis/tuvis.db`，settings KV 表 `settings(key TEXT PRIMARY KEY,
//     value TEXT NOT NULL)` → src-tauri/src/database/schema.rs:14；
//     读取语句同 src-tauri/src/database/dao/settings.rs:20（SELECT value ... WHERE key = ?）；
//   - 键：`remote.zcode_official_url` → src-tauri/src/services/zcode_control/config.rs:17；
//   - **只读打开**（node:sqlite DatabaseSync readOnly）：探针绝不写产品账本；
//   - 链接值只在内存中交给 parseOfficialLink；任何错误/日志不携带链接原文。
//
// 语法纪律：可被 `node`（type-stripping）直接执行——仅可擦除 TS 语法。

import { DatabaseSync } from "node:sqlite";

/** settings KV 键（src-tauri/src/services/zcode_control/config.rs:17）。 */
export const ZCODE_OFFICIAL_URL_SETTINGS_KEY = "remote.zcode_official_url";

export type SettingsLinkResult =
  { kind: "ok"; link: string } | { kind: "missing" } | { kind: "unavailable"; reason: string };

/**
 * 只读打开 SQLite 并读取官方链接。任何打开/查询失败 → unavailable（reason 为静态
 * 标签 + 底层 message，可能含路径但不含凭据）；键不存在 → missing。
 */
export function readOfficialLinkFromSettings(
  dbPath: string,
  key: string = ZCODE_OFFICIAL_URL_SETTINGS_KEY
): SettingsLinkResult {
  let db: DatabaseSync;
  try {
    db = new DatabaseSync(dbPath, { readOnly: true });
  } catch (error) {
    return {
      kind: "unavailable",
      reason: `db-open-failed: ${error instanceof Error ? error.message : String(error)}`,
    };
  }
  try {
    const row = db.prepare("SELECT value FROM settings WHERE key = ?").get(key) as
      { value?: unknown } | undefined;
    if (!row || typeof row.value !== "string" || !row.value.trim()) {
      return { kind: "missing" };
    }
    return { kind: "ok", link: row.value };
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    if (/no such table/i.test(message)) {
      return { kind: "missing" }; // 旧库无 settings 表 = 未配置过
    }
    return { kind: "unavailable", reason: `db-query-failed: ${message}` };
  } finally {
    try {
      db.close();
    } catch {
      // 关闭失败不影响结果
    }
  }
}
