import { useState } from "react";

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
