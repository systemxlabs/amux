/**
 * GUI 本地配置：机器注册表 / 按钮 / 通知偏好。
 *
 * 存储：Tauri 环境下经 Rust command 写入 `~/.amux/gui/config.json`
 * （docs/DESIGN.md §5 统一数据目录；原子写、Unix 0600，含机器 token）；
 * 纯浏览器开发环境（无 Tauri）回退 localStorage 单 key。
 */

import { DEFAULT_BUTTONS } from "./buttons.js";

// ---- 配置形状 ----

export interface MachineConfig {
  id: string;
  name: string;
  /** ws://host:port */
  url: string;
  token: string;
  defaultModel?: string;
}

export interface NotifyPrefs {
  workEnded: boolean;
  onError: boolean;
  longIdleSeconds: number;
}

/** 按钮持久化形状（lib/buttons.ts ActionButton 的存储子集；kind/promptTemplate 兼容未来自定义） */
export interface StoredButton {
  id: string;
  label: string;
  kind?: string;
  promptTemplate?: string;
  enabled: boolean;
}

export interface GuiConfig {
  /** 结构版本，供未来迁移 */
  version: 1;
  machines: MachineConfig[];
  buttons: StoredButton[];
  notify: NotifyPrefs;
}

export const DEFAULT_NOTIFY: NotifyPrefs = { workEnded: true, onError: true, longIdleSeconds: 300 };

function defaultConfig(): GuiConfig {
  return { version: 1, machines: [], buttons: [], notify: { ...DEFAULT_NOTIFY } };
}

// ---- 存储后端 ----

interface ConfigBackend {
  load(): Promise<string | null>;
  save(json: string): Promise<void>;
}

const LS_CONFIG_KEY = "amux.config.v1";

function isTauri(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

/** Tauri：Rust command 读写 ~/.amux/gui/config.json */
const fileBackend: ConfigBackend = {
  async load() {
    try {
      return await tauriInvoke<string | null>("load_gui_config");
    } catch {
      return null;
    }
  },
  async save(json) {
    await tauriInvoke("save_gui_config", { json });
  },
};

/** 浏览器开发环境回退：localStorage 单 key */
const lsBackend: ConfigBackend = {
  async load() {
    if (typeof localStorage === "undefined") return null;
    return localStorage.getItem(LS_CONFIG_KEY);
  },
  async save(json) {
    if (typeof localStorage === "undefined") return;
    localStorage.setItem(LS_CONFIG_KEY, json);
  },
};

/** 惰性加载 @tauri-apps/api/core：仅 Tauri 环境真正执行，测试/浏览器不触碰 */
async function tauriInvoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<T>(cmd, args);
}

// 后端可注入（测试用）
let injectedBackend: ConfigBackend | null = null;

export function setConfigBackendForTests(b: ConfigBackend | null): void {
  injectedBackend = b;
}

function activeBackend(): ConfigBackend {
  if (injectedBackend) return injectedBackend;
  return isTauri() ? fileBackend : lsBackend;
}

// ---- 校验与归一化 ----

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

function isStoredButton(v: unknown): v is StoredButton {
  const b = v as StoredButton;
  return typeof b === "object" && b !== null && typeof b.id === "string" && typeof b.label === "string";
}

function normalizeNotify(v: unknown): NotifyPrefs {
  const n = (v ?? {}) as Record<string, unknown>;
  return {
    workEnded: typeof n.workEnded === "boolean" ? n.workEnded : DEFAULT_NOTIFY.workEnded,
    onError: typeof n.onError === "boolean" ? n.onError : DEFAULT_NOTIFY.onError,
    longIdleSeconds: typeof n.longIdleSeconds === "number" ? n.longIdleSeconds : DEFAULT_NOTIFY.longIdleSeconds,
  };
}

/** 归一化任意输入为合法配置（坏字段回退默认，不抛错）。 */
export function normalizeConfig(raw: unknown): GuiConfig {
  const cfg = defaultConfig();
  const o = (raw ?? {}) as Record<string, unknown>;
  if (Array.isArray(o.machines)) cfg.machines = o.machines.filter(isMachineConfig);
  if (Array.isArray(o.buttons)) cfg.buttons = o.buttons.filter(isStoredButton);
  if (o.notify) cfg.notify = normalizeNotify(o.notify);
  return cfg;
}

// ---- 对外 API（全部异步） ----

export async function loadConfig(): Promise<GuiConfig> {
  const raw = await activeBackend().load();
  if (!raw) return defaultConfig();
  try {
    return normalizeConfig(JSON.parse(raw) as unknown);
  } catch {
    return defaultConfig();
  }
}

export async function saveConfig(cfg: GuiConfig): Promise<void> {
  await activeBackend().save(JSON.stringify(normalizeConfig(cfg), null, 2));
}

export async function loadMachines(): Promise<MachineConfig[]> {
  return (await loadConfig()).machines;
}

export async function addMachine(m: Omit<MachineConfig, "id">): Promise<MachineConfig[]> {
  const cfg = await loadConfig();
  cfg.machines = [...cfg.machines, { id: crypto.randomUUID(), ...m }];
  await saveConfig(cfg);
  return cfg.machines;
}

export async function updateMachine(id: string, patch: Partial<Omit<MachineConfig, "id">>): Promise<MachineConfig[]> {
  const cfg = await loadConfig();
  cfg.machines = cfg.machines.map((m) => (m.id === id ? { ...m, ...patch } : m));
  await saveConfig(cfg);
  return cfg.machines;
}

export async function removeMachine(id: string): Promise<MachineConfig[]> {
  const cfg = await loadConfig();
  cfg.machines = cfg.machines.filter((m) => m.id !== id);
  await saveConfig(cfg);
  return cfg.machines;
}

/** 按钮注册表：默认按钮 + 本地增删改（id 去重，保留默认顺序）。 */
export async function loadButtons(): Promise<Array<{ id: string; label: string; enabled: boolean }>> {
  const cfg = await loadConfig();
  const merged = new Map<string, StoredButton>();
  for (const d of DEFAULT_BUTTONS) {
    merged.set(d.id, { id: d.id, label: d.label, kind: d.kind, promptTemplate: d.promptTemplate, enabled: d.enabled });
  }
  for (const o of cfg.buttons) if (isStoredButton(o)) merged.set(o.id, o);
  return [...merged.values()];
}

export async function saveButtons(list: Array<{ id: string; label: string; enabled: boolean }>): Promise<void> {
  const cfg = await loadConfig();
  cfg.buttons = list.filter(isStoredButton);
  await saveConfig(cfg);
}

export async function loadNotifyPrefs(): Promise<NotifyPrefs> {
  return (await loadConfig()).notify;
}

export async function saveNotifyPrefs(p: NotifyPrefs): Promise<void> {
  const cfg = await loadConfig();
  cfg.notify = normalizeNotify(p);
  await saveConfig(cfg);
}
