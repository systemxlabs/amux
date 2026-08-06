import { useEffect, useState } from "react";
import type { SessionMeta } from "shared";
import type { MachineStore } from "../lib/machineStore.js";

export interface DiffPanelProps {
  store: MachineStore;
  meta: SessionMeta;
}

interface Change {
  path: string;
  status: string;
  staged: boolean;
  additions: number;
  deletions: number;
}

interface StatusResult {
  branch: string;
  changes: Change[];
}

export function DiffPanel(props: DiffPanelProps) {
  const [tab, setTab] = useState<"diff" | "detail">("diff");
  const [status, setStatus] = useState<StatusResult | null>(null);
  const [diff, setDiff] = useState<string>("");
  const [diffPath, setDiffPath] = useState<string | null>(null);
  const [statusError, setStatusError] = useState<string | null>(null);
  const [diffError, setDiffError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);

  const cwd = props.meta.cwd;

  const loadStatus = async () => {
    setStatusError(null);
    try {
      const res = (await props.store.gitStatus(cwd)) as StatusResult;
      setStatus(res);
    } catch (e) {
      setStatusError((e as Error).message);
    }
  };

  const loadDiff = async (path: string | null) => {
    setDiffError(null);
    try {
      setDiff(await props.store.gitDiff(cwd, path ?? undefined));
    } catch (e) {
      setDiffError((e as Error).message);
    }
  };

  useEffect(() => {
    setDiff("");
    setDiffPath(null);
    void loadStatus();
    void loadDiff(null);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [cwd]);

  const run = async (fn: () => Promise<{ ok: boolean; message?: string }>) => {
    setBusy(true);
    setMessage(null);
    try {
      const res = await fn();
      setMessage(res.ok ? "完成" : `失败：${res.message ?? ""}`);
      void loadStatus();
      void loadDiff(diffPath);
    } catch (e) {
      setMessage(`失败：${(e as Error).message}`);
    } finally {
      setBusy(false);
    }
  };

  return (
    <aside className="diff-panel">
      <div className="panel-tabs">
        <button className={`tab ${tab === "diff" ? "active" : ""}`} onClick={() => setTab("diff")}>
          Diff
        </button>
        <button className={`tab ${tab === "detail" ? "active" : ""}`} onClick={() => setTab("detail")}>
          详情
        </button>
      </div>
      {tab === "detail" ? (
        <div className="session-detail">
          <dl>
            <dt>ID</dt>
            <dd className="mono">{props.meta.id}</dd>
            <dt>Harness</dt>
            <dd>{props.meta.harness}</dd>
            <dt>工作目录</dt>
            <dd className="mono">{props.meta.cwd}</dd>
            <dt>模型</dt>
            <dd>{props.meta.model ?? "默认"}</dd>
            <dt>状态</dt>
            <dd>
              {props.meta.interrupted ? "已中断" : props.meta.closed ? "已关闭" : props.meta.state}
            </dd>
            <dt>创建时间</dt>
            <dd>{new Date(props.meta.createdAt).toLocaleString()}</dd>
            <dt>最近事件</dt>
            <dd>{new Date(props.meta.lastEventAt).toLocaleString()}</dd>
          </dl>
        </div>
      ) : (
        <div className="diff-view">
          <div className="diff-head">
            <span className="mono">{status?.branch ?? "…"}</span>
            <span className="dim">{status?.changes.length ?? 0} 个变更</span>
            <button className="btn subtle small" onClick={() => void loadStatus()} disabled={busy}>
              刷新
            </button>
          </div>
          {statusError && <div className="error-line">{statusError}</div>}
          <div className="file-list">
            {(status?.changes ?? []).map((c) => (
              <div key={c.path} className={`file-row ${diffPath === c.path ? "selected" : ""}`} onClick={() => { setDiffPath(c.path); void loadDiff(c.path); }}>
                <span className="file-status">{c.status === "untracked" ? "?" : c.status.slice(0, 1).toUpperCase()}</span>
                <span className="file-path mono">{c.path}</span>
                <span className="file-nums">
                  <span className="add">+{c.additions}</span>
                  <span className="del">-{c.deletions}</span>
                </span>
              </div>
            ))}
            {status && status.changes.length === 0 && <div className="empty-hint">工作区干净</div>}
          </div>
          <div className="diff-actions">
            <button className="btn" onClick={() => run(() => props.store.gitRevert(cwd, diffPath ? { path: diffPath } : {}))} disabled={busy || (status?.changes.length ?? 0) === 0}>
              撤销{diffPath ? "文件" : "全部"}
            </button>
            <button className="btn" onClick={() => run(() => props.store.gitPush(cwd))} disabled={busy}>
              Push
            </button>
          </div>
          {message && <div className="flash">{message}</div>}
          {diffError && <div className="error-line">{diffError}</div>}
          {diff === "" && diffPath && status?.changes.find((c) => c.path === diffPath)?.status === "untracked" && (
            <div className="empty-hint">未跟踪文件（git diff 不含未跟踪内容；提交后可见对比）</div>
          )}
          <pre className="diff-text">{diff}</pre>
        </div>
      )}
    </aside>
  );
}
