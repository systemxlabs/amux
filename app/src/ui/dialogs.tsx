import { useState } from "react";

export interface AddMachineDraft {
  name: string;
  url: string;
  token: string;
  defaultModel?: string;
}

export function AddMachineDialog(props: { onCancel: () => void; onSave: (draft: AddMachineDraft) => void }) {
  const [name, setName] = useState("");
  const [url, setUrl] = useState("");
  const [token, setToken] = useState("");
  const [model, setModel] = useState("");
  const valid = name.trim() !== "" && /^ws:\/\//.test(url.trim()) && token.trim() !== "";
  return (
    <div className="dialog-backdrop" onClick={props.onCancel}>
      <div className="dialog" onClick={(e) => e.stopPropagation()}>
        <h3>添加机器</h3>
        <label>
          名称
          <input value={name} onChange={(e) => setName(e.target.value)} placeholder="本机 / 远程服务器" autoFocus />
        </label>
        <label>
          WebSocket 地址
          <input value={url} onChange={(e) => setUrl(e.target.value)} placeholder="ws://host:34567" />
        </label>
        <label>
          Token（server 首次启动时展示一次）
          <input value={token} onChange={(e) => setToken(e.target.value)} placeholder="粘贴 token" />
        </label>
        <label>
          默认模型（可选）
          <input value={model} onChange={(e) => setModel(e.target.value)} placeholder="如 gpt-5-codex" />
        </label>
        <div className="dialog-actions">
          <button className="btn" onClick={props.onCancel}>
            取消
          </button>
          <button className="btn primary" disabled={!valid} onClick={() => props.onSave({ name: name.trim(), url: url.trim(), token: token.trim(), ...(model.trim() ? { defaultModel: model.trim() } : {}) })}>
            保存
          </button>
        </div>
      </div>
    </div>
  );
}

export interface NewSessionDraft {
  harness: string;
  cwd: string;
  model?: string;
}

export function NewSessionDialog(props: { machineName: string; harnesses: string[]; onCancel: () => void; onSave: (draft: NewSessionDraft) => void }) {
  const [harness, setHarness] = useState(props.harnesses[0] ?? "codex");
  const [cwd, setCwd] = useState("");
  const [model, setModel] = useState("");
  const valid = cwd.trim() !== "";
  return (
    <div className="dialog-backdrop" onClick={props.onCancel}>
      <div className="dialog" onClick={(e) => e.stopPropagation()}>
        <h3>新会话 · {props.machineName}</h3>
        <label>
          Harness
          <select value={harness} onChange={(e) => setHarness(e.target.value)}>
            {props.harnesses.map((h) => (
              <option key={h} value={h}>
                {h}
              </option>
            ))}
          </select>
        </label>
        <label>
          工作目录
          <input value={cwd} onChange={(e) => setCwd(e.target.value)} placeholder="/home/user/project" autoFocus />
        </label>
        <label>
          模型（可选）
          <input value={model} onChange={(e) => setModel(e.target.value)} placeholder="留空用默认" />
        </label>
        <div className="dialog-actions">
          <button className="btn" onClick={props.onCancel}>
            取消
          </button>
          <button className="btn primary" disabled={!valid} onClick={() => props.onSave({ harness, cwd: cwd.trim(), ...(model.trim() ? { model: model.trim() } : {}) })}>
            创建
          </button>
        </div>
      </div>
    </div>
  );
}
