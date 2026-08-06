import { useEffect, useRef, useState } from "react";
import type { Input } from "ahal";
import { buttonDisabled, DEFAULT_BUTTONS } from "./lib/buttons.js";
import {
  addMachine,
  loadMachines,
  loadNotifyPrefs,
  removeMachine,
  type MachineConfig,
} from "./lib/configStore.js";
import { MachineStore } from "./lib/machineStore.js";
import { NotificationDetector } from "./lib/notify.js";
import { notifyError, notifyLongIdle, notifyWorkEnded, setNotificationOpenHandler } from "./lib/notifyBridge.js";
import { AddMachineDialog, NewSessionDialog } from "./ui/dialogs.js";
import { Conversation } from "./ui/Conversation.js";
import { DiffPanel } from "./ui/DiffPanel.js";
import { Sidebar } from "./ui/Sidebar.js";
import "./App.css";

function shortId(id: string): string {
  return id.length > 12 ? id.slice(0, 12) : id;
}

export default function App() {
  const [machines, setMachines] = useState<MachineConfig[]>([]);
  const [, setTick] = useState(0);
  const storesRef = useRef<Map<string, MachineStore>>(new Map());
  const [selected, setSelected] = useState<{ machineId: string; sessionId: string | null } | null>(null);
  const [dialog, setDialog] = useState<"add-machine" | "new-session" | null>(null);
  const [toast, setToast] = useState<string | null>(null);
  const toastTimer = useRef<number | null>(null);

  const showToast = (msg: string): void => {
    setToast(msg);
    if (toastTimer.current !== null) window.clearTimeout(toastTimer.current);
    toastTimer.current = window.setTimeout(() => setToast(null), 4000);
  };

  const ensureStore = (m: MachineConfig): MachineStore => {
    let s = storesRef.current.get(m.id);
    if (!s) {
      s = new MachineStore(m);
      s.subscribe(() => setTick((t) => t + 1));
      s.connect();
      storesRef.current.set(m.id, s);
    }
    return s;
  };

  useEffect(() => {
    setNotificationOpenHandler((machineId, sessionId) => setSelected({ machineId, sessionId }));
    void loadMachines()
      .then(setMachines)
      .catch(() => {});
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // 机器列表就绪 / 变更时确保每个机器有 store（含新增与移除后的增量）
  useEffect(() => {
    for (const m of machines) ensureStore(m);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [machines]);

  // 通知推导：周期扫描各会话事件流，经 NotificationDetector 触发桌面通知
  useEffect(() => {
    let stopped = false;
    let interval: number | null = null;
    void loadNotifyPrefs().then((prefs) => {
      if (stopped) return;
      const detectors = new Map<string, NotificationDetector>();
      const lastSeq = new Map<string, number>();
      interval = window.setInterval(() => {
        for (const [mid, store] of storesRef.current) {
          const machineName = store.state.config.name;
          for (const s of store.state.sessions) {
            const feed = store.state.feeds.get(s.id);
            if (!feed) continue;
            let d = detectors.get(s.id);
            if (!d) {
              d = new NotificationDetector({
                workEnded: prefs.workEnded,
                onError: prefs.onError,
                longIdleSeconds: prefs.longIdleSeconds,
              });
              detectors.set(s.id, d);
            }
            const label = `${s.harness} ${shortId(s.id)}`;
            const from = lastSeq.get(s.id) ?? -1;
            for (const ev of feed.events) {
              if (ev.seq > from) {
                const n = d.onEvent(ev.event, ev.timestamp);
                if (n) {
                  if (n.kind === "work-ended") notifyWorkEnded(mid, s.id, machineName, label, n.reason);
                  else if (n.kind === "error") notifyError(mid, s.id, machineName, label, n.message);
                }
              }
            }
            lastSeq.set(s.id, feed.lastSeq);
            const idle = d.tick();
            if (idle) notifyLongIdle(mid, s.id, machineName, label);
          }
        }
      }, 1000);
    });
    return () => {
      stopped = true;
      if (interval !== null) window.clearInterval(interval);
    };
  }, []);

  const selectedStore = selected ? storesRef.current.get(selected.machineId) : undefined;
  const selectedMeta = selectedStore?.state.sessions.find((s) => s.id === selected?.sessionId);
  const selectedFeed = selectedStore && selected?.sessionId ? selectedStore.state.feeds.get(selected.sessionId) : undefined;

  const promptSelected = (input: Input) => {
    const sel = selected;
    const store = sel ? storesRef.current.get(sel.machineId) : undefined;
    if (!store || !sel?.sessionId) return;
    void store.prompt(sel.sessionId, input).catch((e) => showToast(`prompt 失败：${(e as Error).message}`));
  };

  const cancelSelected = () => {
    const sel = selected;
    const store = sel ? storesRef.current.get(sel.machineId) : undefined;
    if (!store || !sel?.sessionId) return;
    void store.cancel(sel.sessionId).catch((e) => showToast(`cancel 失败：${(e as Error).message}`));
  };

  const resumeSelected = (machineId: string, sessionId: string) => {
    const store = storesRef.current.get(machineId);
    if (!store) return;
    void store
      .resumeSession(sessionId)
      .then(() => setSelected({ machineId, sessionId }))
      .catch((e) => showToast(`恢复失败：${(e as Error).message}`));
  };

  const onButton = (id: string) => {
    const sel = selected;
    const store = sel ? storesRef.current.get(sel.machineId) : undefined;
    const meta = store?.state.sessions.find((s) => s.id === sel?.sessionId);
    if (!store || !sel?.sessionId || !meta) return;
    const b = DEFAULT_BUTTONS.find((x) => x.id === id);
    if (!b) return;
    const cwd = meta.cwd;
    switch (b.kind) {
      case "prompt":
        void store.prompt(sel.sessionId, [{ type: "text", text: b.promptTemplate ?? b.label }]).catch((e) => showToast(`失败：${(e as Error).message}`));
        break;
      case "git-push":
        void store.gitPush(cwd).then((r) => showToast(r.ok ? "Push 完成" : `Push 失败：${r.message ?? ""}`));
        break;
      case "git-revert":
        void store.gitRevert(cwd).then((r) => showToast(r.ok ? "已撤销" : `撤销失败：${r.message ?? ""}`));
        break;
      case "kill-session":
        void store.closeSession(sel.sessionId).then(() => setSelected(null)).catch((e) => showToast(`失败：${(e as Error).message}`));
        break;
      case "new-session":
        setDialog("new-session");
        break;
    }
  };

  return (
    <div className="app">
      <Sidebar
        machines={machines}
        stores={storesRef.current}
        selectedMachineId={selected?.machineId ?? null}
        selectedSessionId={selected?.sessionId ?? null}
        onSelectSession={(machineId, sessionId) => setSelected({ machineId, sessionId })}
        onSelectMachine={(machineId) => setSelected({ machineId, sessionId: null })}
        onResumeSession={resumeSelected}
        onNewSession={(machineId) => {
          setSelected({ machineId, sessionId: null });
          setDialog("new-session");
        }}
        onAddMachine={() => setDialog("add-machine")}
        onRemoveMachine={(machineId) => {
          storesRef.current.get(machineId)?.disconnect();
          storesRef.current.delete(machineId);
          void removeMachine(machineId)
            .then(setMachines)
            .catch(() => {});
          if (selected?.machineId === machineId) setSelected(null);
        }}
      />
      <main className="main">
        {selectedStore && selectedMeta ? (
          <>
            <div className="button-bar">
              {DEFAULT_BUTTONS.map((b) => (
                <button
                  key={b.id}
                  className="btn"
                  disabled={buttonDisabled(b, selectedMeta.state, selectedMeta.closed, selectedMeta.interrupted)}
                  onClick={() => onButton(b.id)}
                >
                  {b.label}
                </button>
              ))}
            </div>
            <Conversation
              key={selectedMeta.id}
              meta={selectedMeta}
              events={selectedFeed?.events ?? []}
              revision={selectedFeed?.revision ?? 0}
              onPrompt={promptSelected}
              onCancel={cancelSelected}
            />
            <DiffPanel store={selectedStore} meta={selectedMeta} />
          </>
        ) : (
          <div className="empty-state">
            <h2>amux</h2>
            {selectedStore ? (
              <>
                <p>机器「{selectedStore.state.config.name}」在线，还没有选中会话。</p>
                <button className="btn primary" onClick={() => setDialog("new-session")}>
                  新建会话
                </button>
              </>
            ) : (
              <>
                <p>在左侧选择机器与会话，或添加机器开始使用。</p>
                {machines.length === 0 && (
                  <button className="btn primary" onClick={() => setDialog("add-machine")}>
                    添加第一台机器
                  </button>
                )}
              </>
            )}
          </div>
        )}
      </main>
      {dialog === "add-machine" && (
        <AddMachineDialog
          onCancel={() => setDialog(null)}
          onSave={(draft) => {
            void addMachine(draft)
              .then((list) => {
                setMachines(list);
                setDialog(null);
                // 新机器的 store 由 [machines] effect 懒创建
              })
              .catch((e) => showToast(`保存失败：${(e as Error).message}`));
          }}
        />
      )}
      {dialog === "new-session" && selectedStore && (
        <NewSessionDialog
          machineName={selectedStore.state.config.name}
          harnesses={(selectedStore.state.info?.harnesses ?? []).filter((h) => h.available).map((h) => h.name)}
          onCancel={() => setDialog(null)}
          onSave={async (draft) => {
            try {
              const meta = await selectedStore.createSession(draft.harness, draft.cwd, draft.model);
              setSelected({ machineId: selected!.machineId, sessionId: meta.id });
              setDialog(null);
            } catch (e) {
              showToast(`创建失败：${(e as Error).message}`);
            }
          }}
        />
      )}
      {toast && <div className="toast">{toast}</div>}
    </div>
  );
}
