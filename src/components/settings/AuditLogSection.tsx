// 设置页「注入审计」分区（M7 W5：移动端注入写审计的桌面只读查看入口）。
// 数据源 = inject_list_audit（最近 100 条，AuditRow serde camelCase；行内无 device_id，
// 设备标识不外泄）；动作词表 send|queue|flush|jump|retract|approve|reject|fail|key 由后端
// 约束，前端原样小写展示（不翻译不改写）。样式对齐 RemoteSection：分区标题 + 边框卡片。
import { SETTINGS_CARD_TITLE } from "@/components/settings/typography";
import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Button } from "@/components/ui/button";
import { useAppTranslation } from "@/hooks/use-app-translation";

// Rust AuditRow（database/dao/write_audit.rs，serde camelCase）前端同构
type AuditRow = {
  ts: number;
  deviceName: string;
  agentType: string;
  sessionId: string;
  channel: string;
  action: string;
  summary: string;
  result: string;
};

// 短时间格式：MM-dd HH:mm:ss（本地时区；账本 ts 为毫秒时间戳）。
// 不引第三方库——设置页既有组件均无日期依赖，两行 padStart 足够
function shortTime(ts: number): string {
  const d = new Date(ts);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

// 五列网格（时间/设备/会话/动作/摘要）：表头与行共用同一列宽，摘要列弹性占余
const GRID_COLS = "grid grid-cols-[92px_120px_minmax(0,1fr)_72px_minmax(0,1.4fr)] gap-2";

export function AuditLogSection() {
  const { t } = useAppTranslation();
  const [items, setItems] = useState<AuditRow[]>([]);
  // 加载失败可见（用户主动动作的失败不得伪装成「暂无记录」空态）：非空 = 展示错误行
  const [loadError, setLoadError] = useState(false);
  // 刷新在途标记：仅用于禁用刷新按钮（连点无新语义，读操作幂等）
  const [loading, setLoading] = useState(false);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      // 兜底空表：mock/旧后端异常载荷不致 items.map 崩溃（同 settings.tsx `?? []` 防御）
      const payload = await invoke<{ items: AuditRow[] }>("inject_list_audit");
      setItems(payload?.items ?? []);
      setLoadError(false);
    } catch (e) {
      // 只读展示：失败保留旧数据（有旧数据时仅刷新按钮语义受损，可重试），
      // 空数据时必须显式报错而非伪装空态
      console.error("inject_list_audit failed:", e);
      setLoadError(true);
    } finally {
      setLoading(false);
    }
  }, []);

  // 挂载即拉取（W5：进分区看最近写审计；无自动轮询——审计是事后查账不是盯屏）
  useEffect(() => {
    void refresh();
  }, [refresh]);

  return (
    <div className="overflow-hidden rounded-xl border">
      {/* 卡头条（2026-10-11 两级标题拉开）：h2 保留档位常量搬进条内（测试锁/语义不动） */}
      <div className="bg-muted/40 flex items-center justify-between gap-2 border-b px-4 py-2.5">
        <h2 className={SETTINGS_CARD_TITLE}>{t("settings.audit.title")}</h2>
        <Button variant="outline" size="sm" onClick={() => void refresh()} disabled={loading}>
          {t("settings.audit.refresh")}
        </Button>
      </div>

      <div className="p-3">
        {items.length === 0 ? (
          loadError ? (
            <p className="text-sm text-red-500" data-testid="audit-load-error">
              {t("settings.audit.loadError")}
            </p>
          ) : (
            <p className="text-muted-foreground text-sm">{t("settings.audit.empty")}</p>
          )
        ) : (
          <div className="text-[12.5px]">
            {/* 表头（与行共用五列网格） */}
            <div
              data-audit-header
              className={`text-muted-foreground bg-muted/40 px-3 py-1.5 text-[11px] font-semibold ${GRID_COLS}`}
            >
              <span>{t("settings.audit.time")}</span>
              <span>{t("settings.audit.device")}</span>
              <span>{t("settings.audit.session")}</span>
              <span>{t("settings.audit.action")}</span>
              <span>{t("settings.audit.summary")}</span>
            </div>
            {/* `max-h` + 内滚只加在**行列表**上，表头留在滚动区外 ⇒ 滚到底也看得见列名
                （2026-10-07 用户裁决：「注入审计的窗口有点过长了，限定一个高度，在里面做滚轮」）。
                `60vh` 的取法：设置窗口 880×640 ⇒ 约 384px 高，一屏能看到表头 + 6~7 行；
                再高就会把同块内的「远程服务 / 信号健康度」两张卡整个顶出视野。 */}
            <ul className="max-h-[60vh] overflow-y-auto">
              {items.map((r, i) => (
                <li
                  // AuditRow 无唯一 id（ts 理论可撞），拼 index 兜底键稳定
                  key={`${r.ts}-${r.sessionId}-${i}`}
                  data-audit-row
                  className={`${GRID_COLS} items-center px-3 py-2 [&:not(:last-child)]:border-b [&:not(:last-child)]:border-dashed`}
                >
                  <span className="text-muted-foreground text-xs">{shortTime(r.ts)}</span>
                  <span className="truncate" title={r.deviceName}>
                    {r.deviceName}
                  </span>
                  <span className="truncate font-mono text-xs" title={r.sessionId}>
                    {r.sessionId}
                  </span>
                  <span className="font-mono text-xs">{r.action}</span>
                  <span className="truncate" title={r.summary}>
                    {r.summary}
                  </span>
                </li>
              ))}
            </ul>
          </div>
        )}
      </div>
    </div>
  );
}
