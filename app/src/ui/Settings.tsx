/**
 * 设置页面（docs/PRD.md §3.4）：全局配置集中于此，经左侧面板底部的设置入口打开。
 * 当前包含机器管理（接入 / 移除 / 连接配置 / 默认模型）；主界面只负责浏览机器与会话。
 */

import { useState } from "react";
import { addMachine, removeMachine, updateMachine, type MachineConfig } from "../lib/configStore.js";
import type { MachineStore } from "../lib/machineStore.js";

export interface SettingsDialogProps {
  machines: MachineConfig[];
  stores: Map<string, MachineStore>;
  onClose: () => void;
  /** 增删改后回传最新机器列表（App 同步主界面） */
  onMachinesChange: (list: MachineConfig[]) => void;
}

const STATUS_LABEL: Record<string, string> = {
  connected: "在线",
  connecting: "连接中",
  disconnected: "离线",
  "auth-error": "认证失败",
};

/** 机器表单草稿（新增或编辑共用）。 */
interface MachineDraft {
  name: string;
  url: string;
  token: string;
  defaultModel: string;
}

const EMPTY: MachineDraft = { name: "", url: "", token: "", defaultModel: "" };

export function SettingsDialog(props: SettingsDialogProps) {
  /** null=列表；"new"=新增表单；machineId=编辑该机器 */
  const [editing, setEditing] = useState<null | "new" | string>(null);
  const [draft, setDraft] = useState<MachineDraft>(EMPTY);
  const [busy, setBusy] = useState(false);

  const startAdd = () => {
    setDraft(EMPTY);
    setEditing("new");
  };

  const startEdit = (m: MachineConfig) => {
    setDraft({ name: m.name, url: m.url, token: m.token, defaultModel: m.defaultModel ?? "" });
    setEditing(m.id);
  };

  const save = async () => {
    if (draft.name.trim() === "" || !/^ws:\/\//.test(draft.url.trim()) || draft.token.trim() === "") return;
    setBusy(true);
    try {
      const body = {
        name: draft.name.trim(),
        url: draft.url.trim(),
        token: draft.token.trim(),
        ...(draft.defaultModel.trim() ? { defaultModel: draft.defaultModel.trim() } : {}),
      };
      const list = editing === "new" ? await addMachine(body) : await updateMachine(editing as string, body);
      props.onMachinesChange(list);
      setEditing(null);
    } catch (e) {
      window.alert(`保存失败：${(e as Error).message}`);
    } finally {
      setBusy(false);
    }
  };

  const remove = async (id: string, name: string) => {
    if (!window.confirm(`移除机器「${name}」？连接配置将删除，已存在的会话不受影响。`)) return;
    setBusy(true);
    try {
      const list = await removeMachine(id);
      props.onMachinesChange(list);
      if (editing === id) setEditing(null);
    } catch (e) {
      window.alert(`移除失败：${(e as Error).message}`);
    } finally {
      setBusy(false);
    }
  };

  const valid = draft.name.trim() !== "" && /^ws:\/\//.test(draft.url.trim()) && draft.token.trim() !== "";

  return (
    <div className="dialog-backdrop" onClick={props.onClose}>
      <div className="dialog settings-dialog" onClick={(e) => e.stopPropagation()}>
        <h3>设置</h3>
        <h4 className="settings-section-title">机器管理</h4>
        {props.machines.length === 0 && !editing && <div className="empty-hint">还没有机器，点击「添加机器」接入</div>}
        {props.machines.map((m) => {
          const status = props.stores.get(m.id)?.state.status ?? "disconnected";
          return (
            <div key={m.id} className="settings-machine">
              <span className={`dot dot-${status}`} title={status} />
              <span className="settings-machine-name">{m.name}</span>
              <span className="dim settings-machine-status">{STATUS_LABEL[status] ?? status}</span>
              <span className="dim mono settings-machine-url">{m.url}</span>
              {m.defaultModel && <span className="dim">模型 {m.defaultModel}</span>}
              <span className="settings-machine-actions">
                <button className="btn subtle small" onClick={() => startEdit(m)} disabled={busy}>
                  编辑
                </button>
                <button className="btn subtle small danger" onClick={() => void remove(m.id, m.name)} disabled={busy}>
                  移除
                </button>
              </span>
            </div>
          );
        })}
        {editing ? (
          <div className="settings-form">
            <label>
              名称
              <input value={draft.name} onChange={(e) => setDraft({ ...draft, name: e.target.value })} placeholder="本机 / 远程服务器" autoFocus />
            </label>
            <label>
              WebSocket 地址
              <input value={draft.url} onChange={(e) => setDraft({ ...draft, url: e.target.value })} placeholder="ws://host:34567" />
            </label>
            <label>
              Token（server 启动时经 --token / AMUX_TOKEN 指定，不落盘）
              <input value={draft.token} onChange={(e) => setDraft({ ...draft, token: e.target.value })} placeholder="粘贴 token" />
            </label>
            <label>
              默认模型（可选）
              <input value={draft.defaultModel} onChange={(e) => setDraft({ ...draft, defaultModel: e.target.value })} placeholder="如 gpt-5-codex" />
            </label>
            <div className="dialog-actions">
              <button className="btn" onClick={() => setEditing(null)} disabled={busy}>
                取消
              </button>
              <button className="btn primary" disabled={!valid || busy} onClick={() => void save()}>
                保存
              </button>
            </div>
          </div>
        ) : (
          <button className="btn" onClick={startAdd} disabled={busy}>
            添加机器
          </button>
        )}
        <div className="settings-note dim">机器接入与管理在设置页进行；主界面左侧面板只浏览机器与会话。</div>
        <div className="dialog-actions">
          <button className="btn" onClick={props.onClose}>
            关闭
          </button>
        </div>
      </div>
    </div>
  );
}
