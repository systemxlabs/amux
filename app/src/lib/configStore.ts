/**
 * GUI 本地配置存储：机器注册表、按钮配置、通知配置。
 * 存于 webview 本地存储（localStorage）。注：DESIGN 约定配置在 ~/.amux/gui，
 * 当前以 localStorage 落地（webview 会话级持久化），迁移到文件存储见偏差记录。
 */

import { DEFAULT_BUTTONS } from "./buttons.js";

export interface MachineConfig {
  id: string;
  name: string;
  /** ws://host:port */
  url: string;
  token: string;
  defaultModel?: string;
}

const MACHINES_KEY = "amux.machines.v1";
const BUTTONS_KEY = "amux.buttons.v1";
const NOTIFY_KEY = "amux.notify.v1";

export function loadMachines(): MachineConfig[] {
  try {
    const raw = localStorage.getItem(MACHINES_KEY);
    if (!raw) return [];
    const list = JSON.parse(raw) as unknown;
    if (!Array.isArray(list)) return [];
    return list.filter(isMachineConfig);
  } catch {
    return [];
  }
}

export function saveMachines(list: MachineConfig[]): void {
  localStorage.setItem(MACHINES_KEY, JSON.stringify(list));
}

export function addMachine(m: Omit<MachineConfig, "id">): MachineConfig {
  const cfg: MachineConfig = { id: crypto.randomUUID(), ...m };
  saveMachines([...loadMachines(), cfg]);
  return cfg;
}

export function updateMachine(id: string, patch: Partial<Omit<MachineConfig, "id">>): MachineConfig[] {
  const list = loadMachines().map((m) => (m.id === id ? { ...m, ...patch } : m));
  saveMachines(list);
  return list;
}

export function removeMachine(id: string): MachineConfig[] {
  const list = loadMachines().filter((m) => m.id !== id);
  saveMachines(list);
  return list;
}

function isMachineConfig(v: unknown): v is MachineConfig {
  const m = v as MachineConfig;
  return (
    typeof m === "object" &&
    m !== null &&
    typeof m.id === "string" &&
    typeof m.name === "string" &&
    typeof m.url === "string" &&
    typeof m.token === "string"
  );
}

export interface NotifyPrefs {
  workEnded: boolean;
  onError: boolean;
  longIdleSeconds: number;
}

export function loadNotifyPrefs(): NotifyPrefs {
  try {
    const raw = localStorage.getItem(NOTIFY_KEY);
    if (!raw) return { workEnded: true, onError: true, longIdleSeconds: 300 };
    const p = JSON.parse(raw) as Partial<NotifyPrefs>;
    return {
      workEnded: typeof p.workEnded === "boolean" ? p.workEnded : true,
      onError: typeof p.onError === "boolean" ? p.onError : true,
      longIdleSeconds: typeof p.longIdleSeconds === "number" ? p.longIdleSeconds : 300,
    };
  } catch {
    return { workEnded: true, onError: true, longIdleSeconds: 300 };
  }
}

export function saveNotifyPrefs(p: NotifyPrefs): void {
  localStorage.setItem(NOTIFY_KEY, JSON.stringify(p));
}

/** 按钮注册表：默认按钮 + 本地增删改（id 去重，保留默认顺序）。 */
export function loadButtons(): Array<{ id: string; label: string; enabled: boolean }> {
  try {
    const raw = localStorage.getItem(BUTTONS_KEY);
    const list = raw ? (JSON.parse(raw) as Array<{ id: string; label: string; enabled: boolean }>) : [];
    const merged = new Map<string, { id: string; label: string; enabled: boolean }>();
    for (const d of DEFAULT_APP_BUTTONS) merged.set(d.id, d);
    for (const o of list) if (o && typeof o.id === "string") merged.set(o.id, o);
    return [...merged.values()];
  } catch {
    return DEFAULT_APP_BUTTONS;
  }
}

export function saveButtons(list: Array<{ id: string; label: string; enabled: boolean }>): void {
  localStorage.setItem(BUTTONS_KEY, JSON.stringify(list));
}

// 与 lib/buttons.ts 的 DEFAULT_BUTTONS 保持一致（此处为持久化形状）
const DEFAULT_APP_BUTTONS: Array<{ id: string; label: string; enabled: boolean }> = DEFAULT_BUTTONS.map((b) => ({
  id: b.id,
  label: b.label,
  enabled: b.enabled,
}));
