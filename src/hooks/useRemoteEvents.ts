// 桌面全局远程事件（M5 A6 更新）：/pair/pin 认证制落地后审批/配对请求事件已下线
//（remote-pair-request 监听与配套 i18n 文案随 A6 删除）；保留隧道地址与守护失败
// 通知 / 托盘触发的地址复制与开关失败提示。main.tsx AppWrapper 挂载一次。
import { useEffect } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  sendNotification,
  isPermissionGranted,
  requestPermission,
} from "@tauri-apps/plugin-notification";
import { toast } from "sonner";
import { useAppTranslation } from "@/hooks/use-app-translation";
import { formatInvokeError } from "@/lib/invokeError";

export function useRemoteEvents() {
  const { t } = useAppTranslation();
  useEffect(() => {
    // 通知浮窗/宠物窗口不订阅：各自 WebView 重复弹系统通知与 toast，主窗口是唯一出口
    if (window.location.hash === "#/notification" || window.location.hash === "#/pet") return;
    const unlisten: Array<() => void> = [];
    void (async () => {
      // 系统通知统一出口：权限未授先请求；动作类型默认 open-pairing（点击直达设置页，
      // 分发在 useNotification.ts 的全局 onAction 单点）
      const notify = async (title: string, body: string, actionTypeId = "open-pairing") => {
        let ok = await isPermissionGranted();
        if (!ok) ok = (await requestPermission()) === "granted";
        if (ok) sendNotification({ title, body, actionTypeId });
      };
      unlisten.push(
        await listen<{ url: string }>("remote-tunnel-address", (e) => {
          toast.success(t("settings.remote.tunnelReadyToast", { url: e.payload.url }));
        })
      );
      unlisten.push(
        await listen<{ error: string }>("remote-tunnel-error", async (e) => {
          // spec T1b：守护放弃时桌面通知报错（toast 在窗口隐藏时不可见——双通道）
          await notify(t("settings.remote.tunnelErrorTitle"), e.payload.error);
          toast.error(
            t("settings.remote.tunnelErrorToast", { error: formatInvokeError(e.payload.error, t) })
          );
        })
      );
      unlisten.push(
        await listen<{ url: string }>("remote-copy-addr", async (e) => {
          // 托盘菜单「复制远程地址」：剪贴板写入在本窗口完成（托盘进程无剪贴板上下文）
          await navigator.clipboard.writeText(e.payload.url);
          toast.success(t("settings.remote.addrCopiedToast"));
        })
      );
      unlisten.push(
        await listen<{ error: string }>("remote-toggle-failed", async (e) => {
          // spec T4：托盘开关失败「桌面通知报错」——toast + 系统通知双通道
          await notify(t("settings.remote.toggleFailedTitle"), e.payload.error);
          toast.error(t("settings.remote.toggleFailedToast", { error: e.payload.error }));
        })
      );
    })();
    return () => unlisten.forEach((f) => f());
  }, [t]);
}
