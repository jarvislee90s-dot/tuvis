import { invoke } from "@tauri-apps/api/core";
export async function getSetting(key: string) {
  return await invoke<string | null>("get_setting", { key });
}
export async function setSetting(key: string, value: string) {
  return await invoke("set_setting", { key, value });
}
export async function detectTools() {
  return await invoke("detect_tools");
}
export interface SubagentDetection {
  supported: boolean;
  agents: string[];
}
export async function detectSubagents(toolId: string) {
  return await invoke<SubagentDetection>("detect_subagents", { toolId });
}
export async function listSubAgents(toolId: string) {
  return await invoke("list_sub_agents", { toolId });
}
