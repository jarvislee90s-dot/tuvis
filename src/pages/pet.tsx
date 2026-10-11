// src/pages/pet.tsx — 宠物窗口路由页：应用显隐/置顶后渲染桌宠（spec §4.1/§4.5）
import { useEffect } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { Toaster } from "@/components/ui/sonner";
import { FoxbellPet } from "@/components/pet/FoxbellPet";
import { loadConfig, saveVisible, subscribeConfig } from "@/components/pet/petConfig";
import { invoke } from "@tauri-apps/api/core";

export default function PetPage() {
  useEffect(() => {
    // 诊断（决策三输入）：这一行回答「本次走的是哪条路」——`visible` 决定是否 show，
    // `alwaysOnTop` 决定置顶是否被设，`hash` 证明这个 WebView 确实被判成了宠物窗口
    // （`main.tsx` 按 `#/pet` 分流；分错了会静默回落首页 ⇒ 本 effect 压根不跑）。
    // 启动跟随（2026-10-11 用户裁决）：requestedVisible 改读 followStartup——
    // 开（默认）= 每次启动都 show + set_pet_visible(true)（上次隐身也跟出，「显隐」
    // 只管本会话中途，会被这一把重置为 true）；关 = 启动后先不出（requested=false，
    // 与「宠物被关掉」同一安全路径：可见态建窗 + 页面挂载后收回）。
    const cfg = loadConfig();
    const requestedVisible = cfg.followStartup;
    console.info("[pet] mount", {
      hash: window.location.hash,
      visible: requestedVisible,
      followStartup: cfg.followStartup,
      alwaysOnTop: cfg.alwaysOnTop,
    });
    const apply = async () => {
      try {
        await getCurrentWindow().setAlwaysOnTop(cfg.alwaysOnTop);
        if (requestedVisible) await getCurrentWindow().show();
      } catch (e) {
        // 诊断：原先这里是**空 catch**（「浏览器预览：忽略」）⇒ ACL 拒绝 / 窗口 API 失败
        // 全被吞掉。它不是致命路径（下面还有 `set_pet_visible` 兜底），但必须留痕。
        console.error("[pet] apply() 失败（setAlwaysOnTop / show）", e);
      }
    };
    apply();
    // 托盘/主窗口切换显隐后同步本地状态（spec §10.2）
    const un1 = listen<{ visible: boolean }>("pet-visibility-changed", (e) => {
      console.info("[pet] pet-visibility-changed →", e.payload.visible);
      saveVisible(e.payload.visible);
    }).catch(() => Promise.resolve(() => {}));
    const un2 = subscribeConfig(() => {
      const cfg2 = loadConfig();
      getCurrentWindow()
        .setAlwaysOnTop(cfg2.alwaysOnTop)
        .catch((e) => console.error("[pet] setAlwaysOnTop（配置变更）失败", e));
    });
    // 兜底：窗口存活但从未显式 show（如首次开启）。
    // 诊断（**最要紧的一行**）：这个调用是宠物窗口**唯一**的显示路径，而原先的
    // `.catch(() => {})` 把它的一切失败都吞掉了 ——「宠物不显示」因此**永远无迹可查**。
    // 失败必须留痕，且要带上「发出去的 visible 值」，否则分不清是「没发」还是「发了没生效」。
    // 启动决策落为权威值（评审 M1）：visible 键必须与窗口实况同步——跟随跟出把
    // 上次隐身覆盖为 true（storage 事件把标题栏/设置页开关同步成开，FoxbellPet
    // 的语音闸门随 loadVisible() 放行，跟出来的宠物不再哑）；跟随关写 false
    //（防隐形宠物的幽灵语音与「浮窗被 petSuppressPopup 抑制」的黑窗）
    saveVisible(requestedVisible);
    invoke("set_pet_visible", { visible: requestedVisible }).catch((e) => {
      console.error("[pet] set_pet_visible 失败", { requested: requestedVisible, error: e });
    });
    return () => {
      un1.then((f) => f());
      un2();
    };
  }, []);
  return (
    <>
      <FoxbellPet />
      {/* P2-7（issue #34）：宠物窗口此前未挂 Toaster，useSessionJump 的 M3 兜底
          提示与跳转失败提示在此窗口静默丢弃；顶部居中避免被桌宠本体遮挡 */}
      <Toaster position="top-center" />
    </>
  );
}
