// DSH 本机管理 IPC 唯一入口（T05）——§2.4 九命令包装（逐字，无多余参数）。
// 管理命令**不接受包名/命令/任意安装路径**（目标由后端 identity 单源解析）；
// 错误形态 = 既有「CODE: detail」字符串（展示经 formatInvokeError → errors.<码> 文案）。
import { invoke } from "@tauri-apps/api/core";
import type {
  DshBindingStartResult,
  DshInstanceCandidate,
  DshInstanceRequest,
  DshManagementJob,
  DshManagementStatus,
  DshSwitchRequest,
  DshSwitchView,
} from "@/types/dsh";

/** 本机候选实例（只报事实：不自动登记、不自动选择）。 */
export async function dshInstanceList(): Promise<DshInstanceCandidate[]> {
  return await invoke<DshInstanceCandidate[]>("dsh_instance_list");
}

/** 明确选择实例（稳定 instanceId；命令层拒绝路径/命令等额外字段）。 */
export async function dshInstanceSelect(instanceId: string): Promise<void> {
  return await invoke<void>("dsh_instance_select", {
    request: { instanceId } satisfies DshInstanceRequest,
  });
}

/** 受控重开：用户明确点击后，只启动所选实例对应的 Desktop 应用。 */
export async function dshRestartHost(instanceId: string): Promise<void> {
  return await invoke<void>("dsh_restart_host", {
    request: { instanceId } satisfies DshInstanceRequest,
  });
}

/** 按持久选择（或显式同一实例）查询管理状态；不含 autoAdapt（设置项另取）。 */
export async function dshInstanceStatus(instanceId?: string): Promise<DshManagementStatus> {
  return await invoke<DshManagementStatus>("dsh_instance_status", {
    request: instanceId === undefined ? null : ({ instanceId } satisfies DshInstanceRequest),
  });
}

/** 本机管理状态（ManagementStatus + 命令层叠加的 autoAdaptEnabled，缺省开）。 */
export async function dshRemoteStatus(): Promise<DshManagementStatus> {
  return await invoke<DshManagementStatus>("dsh_remote_status");
}

/** 启动固定包安装作业，返回 job_id。 */
export async function dshPluginInstall(): Promise<string> {
  return await invoke<string>("dsh_plugin_install");
}

/** 启动固定包修复作业，返回 job_id。 */
export async function dshPluginRepair(): Promise<string> {
  return await invoke<string>("dsh_plugin_repair");
}

/** 启动固定包卸载作业（先撤绑定再动包），返回 job_id。 */
export async function dshPluginUninstall(): Promise<string> {
  return await invoke<string>("dsh_plugin_uninstall");
}

/** 按 job_id 查询管理作业。 */
export async function dshPluginJob(jobId: string): Promise<DshManagementJob> {
  return await invoke<DshManagementJob>("dsh_plugin_job", { jobId });
}

/** 取消管理作业（阶段检查点生效）。 */
export async function dshPluginCancel(jobId: string): Promise<DshManagementJob> {
  return await invoke<DshManagementJob>("dsh_plugin_cancel", { jobId });
}

/** 发起本机绑定（幂等启动本机 IPC；返回挑战 + 双端身份核对面）。
 *  无待批连接时显式返回 DSH_AUTH_NO_PENDING（前端如实透出不隐藏）。 */
export async function dshBindingStart(): Promise<DshBindingStartResult> {
  return await invoke<DshBindingStartResult>("dsh_binding_start");
}

/** 一键启用（H10）：布防一次性授权意图——下一个通过全部核验的新钥握手被自动绑定。
 *
 *  语义：意图 60 秒有效且只消费一次；**不是**免核验（OS 对端身份 + 固定域 +
 *  签名挑战仍然全部要过），只是省掉最后一步人工点击。过期后回到人工批准。 */
export async function dshBindingArmIntent(instanceId: string): Promise<void> {
  return await invoke<void>("dsh_binding_arm_intent", { request: { instanceId } });
}

/** 用户批准绑定（T06 文档化扩展命令；批准后钉对端公钥并放行）。 */
export async function dshBindingApprove(bindingId: string): Promise<void> {
  return await invoke<void>("dsh_binding_approve", { bindingId });
}

/** 明确确认用待批准的新身份替换已撤销的旧身份。 */
export async function dshBindingRebind(
  instanceId: string,
  bindingId: string,
  expectedOldFingerprint: string
): Promise<void> {
  return await invoke<void>("dsh_binding_rebind", {
    instanceId,
    bindingId,
    expectedOldFingerprint,
  });
}

/** 撤销绑定（关受信 Peer、清凭据、禁自动重连）。 */
export async function dshBindingRevoke(bindingId: string): Promise<void> {
  return await invoke<void>("dsh_binding_revoke", { bindingId });
}

/** 写 DSH 自动适配开关（显式两值；返回生效值，缺省开）。 */
export async function dshSetAutoAdapt(enabled: boolean): Promise<boolean> {
  return await invoke<boolean>("dsh_set_auto_adapt", { enabled });
}

// ============================================================
// 「远程控制 DSH」单开关（Task 4；状态机在后端按选定实例持久化，
// 前端只发起意图与消费实际状态——不落 localStorage、不自行编排）
// ============================================================

/** 当前选定实例的开关状态外表（含发送/控制台各自就绪结果与活跃作业）。 */
export async function dshSwitchGet(instanceId?: string): Promise<DshSwitchView> {
  return await invoke<DshSwitchView>("dsh_switch_get", {
    request: instanceId === undefined ? null : ({ instanceId } satisfies DshSwitchRequest),
  });
}

/** 开启：本次安装 + 实例级授权意图；线性推进（含等待退出/重开/授权）由后端驱动。 */
export async function dshSwitchEnable(instanceId: string): Promise<DshSwitchView> {
  return await invoke<DshSwitchView>("dsh_switch_enable", {
    request: { instanceId } satisfies DshSwitchRequest,
  });
}

/** 关闭：同步失效（撤权/断控制台/清意图）+ 自动卸载（等待宿主正常退出）。 */
export async function dshSwitchDisable(instanceId: string): Promise<DshSwitchView> {
  return await invoke<DshSwitchView>("dsh_switch_disable", {
    request: { instanceId } satisfies DshSwitchRequest,
  });
}

/** 修复插件（只作用于当前实例；修复完成后由后端自动接续原流程）。 */
export async function dshSwitchRepair(instanceId: string): Promise<DshSwitchView> {
  return await invoke<DshSwitchView>("dsh_switch_repair", {
    request: { instanceId } satisfies DshSwitchRequest,
  });
}
