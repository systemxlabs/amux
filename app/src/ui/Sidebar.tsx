import type { MachineConfig } from "../lib/configStore.js";
import type { MachineStore } from "../lib/machineStore.js";
import type { SessionMeta } from "shared";

const STATUS_LABEL: Record<string, string> = {
  idle: "空闲",
  thinking: "思考",
  responding: "回复",
  acting: "执行",
};

function statusBadge(s: SessionMeta): { text: string; cls: string } {
  if (s.closed) return { text: "已关闭", cls: "closed" };
  if (s.interrupted) return { text: "已中断", cls: "interrupted" };
  return { text: STATUS_LABEL[s.state] ?? s.state, cls: s.state === "idle" ? "idle" : "busy" };
}

function recentSummary(feed: { events: readonly { event: { kind: string }; timestamp: number }[] } | undefined, meta: SessionMeta): string {
  if (!feed || feed.events.length === 0) {
    return meta.cwd;
  }
  const last = feed.events[feed.events.length - 1];
  if (last.event.kind === "agent_message") {
    const msg = last.event as { content?: Array<{ type: string; text?: string }> };
    const texts = (msg.content ?? []).filter((c) => c.type === "text").map((c) => c.text ?? "");
    if (texts.length) return texts[0].slice(0, 60);
  }
  return meta.cwd;
}

export interface SidebarProps {
  machines: MachineConfig[];
  stores: Map<string, MachineStore>;
  selectedMachineId: string | null;
  selectedSessionId: string | null;
  onSelectSession: (machineId: string, sessionId: string) => void;
  onSelectMachine: (machineId: string) => void;
  onNewSession: (machineId: string) => void;
  onAddMachine: () => void;
  onRemoveMachine: (machineId: string) => void;
}

export function Sidebar(props: SidebarProps) {
  return (
    <aside className="sidebar">
      <div className="sidebar-header">
        <span className="app-title">amux</span>
        <button className="btn subtle" onClick={props.onAddMachine} title="添加机器">
          ＋ 机器
        </button>
      </div>
      <div className="machine-list">
        {props.machines.length === 0 && <div className="empty-hint">还没有机器，点击「＋ 机器」添加</div>}
        {props.machines.map((m) => {
          const store = props.stores.get(m.id);
          const status = store?.state.status ?? "disconnected";
          const sessions = store?.state.sessions ?? [];
          return (
            <div key={m.id} className={`machine ${props.selectedMachineId === m.id ? "selected" : ""}`}>
              <div
                className={`machine-head ${props.selectedMachineId === m.id ? "clickable selected" : "clickable"}`}
                onClick={() => props.onSelectMachine(m.id)}
                title="选中该机器"
              >
                <span className={`dot dot-${status}`} title={status} />
                <span className="machine-name">{m.name}</span>
                <span className="machine-status">{status === "connected" ? "在线" : status === "auth-error" ? "认证失败" : status === "connecting" ? "连接中" : "离线"}</span>
                <button
                  className="btn subtle small"
                  onClick={(e) => {
                    e.stopPropagation();
                    props.onNewSession(m.id);
                  }}
                  title="在该机器新建会话"
                >
                  ＋
                </button>
                <button className="btn subtle small" onClick={(e) => { e.stopPropagation(); props.onRemoveMachine(m.id); }} title="移除机器">
                  ✕
                </button>
              </div>
              <div className="session-list">
                {sessions.map((s) => {
                  const badge = statusBadge(s);
                  return (
                    <div
                      key={s.id}
                      className={`session ${props.selectedMachineId === m.id && props.selectedSessionId === s.id ? "selected" : ""}`}
                      onClick={() => props.onSelectSession(m.id, s.id)}
                    >
                      <div className="session-line">
                        <span className={`session-state ${badge.cls}`}>{badge.text}</span>
                        <span className="session-harness">{s.harness}</span>
                      </div>
                      <div className="session-summary">{recentSummary(store?.state.feeds.get(s.id), s)}</div>
                    </div>
                  );
                })}
                {sessions.length === 0 && <div className="empty-hint">无会话</div>}
              </div>
            </div>
          );
        })}
      </div>
    </aside>
  );
}
