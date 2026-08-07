import { useEffect, useRef, useState } from "react";
import type { Input } from "ahal";
import { DEFAULT_BUTTONS } from "./lib/buttons.js";
import {
  loadMachines,
  loadNotifyPrefs,
  type MachineConfig,
} from "./lib/configStore.js";
import { MachineStore } from "./lib/machineStore.js";
import { NotificationDetector } from "./lib/notify.js";
import { notifyError, notifyLongIdle, notifyWorkEnded, setNotificationOpenHandler } from "./lib/notifyBridge.js";
import { NewSessionDialog } from "./ui/dialogs.js";
import { SettingsDialog } from "./ui/Settings.js";
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
  const [dialog, setDialog] = useState<"new-session" | "settings" | null>(null);
  /** 右侧面板展开状态（PRD §3.1：默认折叠，由悬浮按钮触发展开） */
  const [panel, setPanel] = useState<"diff" | "detail" | null>(null);
  /** 当前选中会话的 workspace 是否 git 仓库（非 git 仓库不提供 diff 按钮） */
  const [notRepo, setNotRepo] = useState(false);
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
            // 只推导实时/补齐到达的事件（历史不触发通知；server 按连接对齐保证无重复）
            for (const item of feed.drainNotify()) {
              if ("event" in item) {
                const n = d.onEvent(item.event, item.timestamp);
                if (n) {
                  if (n.kind === "work-ended") notifyWorkEnded(mid, s.id, machineName, label, n.reason);
                  else if (n.kind === "error") notifyError(mid, s.id, machineName, label, n.message);
                }
              }
            }
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

  // 选中会话变化：重置右侧面板（默认折叠）；检查 workspace 是否 git 仓库（非 git 仓库不提供 diff 按钮）
  useEffect(() => {
    setPanel(null);
    if (!selectedStore || !selectedMeta) {
      setNotRepo(false);
      return;
    }
    let stale = false;
    setNotRepo(false);
    void selectedStore
      .gitStatus(selectedMeta.cwd)
      .then((res) => {
        if (!stale) setNotRepo((res as { notRepo?: boolean }).notRepo === true);
      })
      .catch(() => {
        if (!stale) setNotRepo(false);
      });
    return () => {
      stale = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selectedStore, selectedMeta?.id]);

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

  /** 删除会话（永久，历史一并删除；不可恢复）。 */
  const deleteSelected = (machineId: string, sessionId: string): void => {
    const store = storesRef.current.get(machineId);
    if (!store) return;
    if (!window.confirm("删除会话将永久移除其历史记录，且不可恢复。确定删除？")) return;
    void store
      .deleteSession(sessionId)
      .then(() => {
        if (selected?.machineId === machineId && selected?.sessionId === sessionId) setSelected(null);
        showToast("会话已删除");
      })
      .catch((e) => showToast(`删除失败：${(e as Error).message}`));
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
    }
  };

  /** 关闭会话（Kill：结束但保留历史，可恢复）。侧边栏会话菜单入口。 */
  const closeSession = (machineId: string, sessionId: string): void => {
    const store = storesRef.current.get(machineId);
    if (!store) return;
    void store
      .closeSession(sessionId)
      .then(() => showToast("会话已关闭（可恢复）"))
      .catch((e) => showToast(`关闭失败：${(e as Error).message}`));
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
        onCloseSession={closeSession}
        onDeleteSession={(machineId, sessionId) => deleteSelected(machineId, sessionId)}
        onOpenSettings={() => setDialog("settings")}
      />
      <main className="main">
        {selectedStore && selectedMeta ? (
          <>
            <div className="main-body">
              <div className="conversation-wrap">
                <Conversation
                  key={selectedMeta.id}
                  meta={selectedMeta}
                  events={selectedFeed?.events ?? []}
                  revision={selectedFeed?.revision ?? 0}
                  buttons={DEFAULT_BUTTONS}
                  onButton={onButton}
                  onPrompt={promptSelected}
                  onCancel={cancelSelected}
                />
                {/* 悬浮按钮：对话流右侧竖排（PRD §3.1）；非 git 仓库无 diff 按钮 */}
                <div className="floating-buttons">
                  {!notRepo && (
                    <button
                      className={`btn subtle small ${panel === "diff" ? "active" : ""}`}
                      onClick={() => setPanel(panel === "diff" ? null : "diff")}
                      title="工作区 diff"
                    >
                      Diff
                    </button>
                  )}
                  <button
                    className={`btn subtle small ${panel === "detail" ? "active" : ""}`}
                    onClick={() => setPanel(panel === "detail" ? null : "detail")}
                    title="会话详情"
                  >
                    详情
                  </button>
                </div>
              </div>
              {panel && (
                <DiffPanel
                  store={selectedStore}
                  meta={selectedMeta}
                  view={panel}
                  onClose={() => setPanel(null)}
                  onDelete={() => deleteSelected(selected!.machineId, selectedMeta.id)}
                />
              )}
            </div>
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
                <p>在左侧选择机器与会话；机器接入在设置页完成。</p>
                {machines.length === 0 && (
                  <button className="btn primary" onClick={() => setDialog("settings")}>
                    添加机器
                  </button>
                )}
              </>
            )}
          </div>
        )}
      </main>
      {dialog === "settings" && (
        <SettingsDialog
          machines={machines}
          stores={storesRef.current}
          onClose={() => setDialog(null)}
          onMachinesChange={(list) => {
            setMachines(list);
            // 移除的机器断开连接并清理 store；新增机器由 [machines] effect 懒创建
            const alive = new Set(list.map((m) => m.id));
            for (const [id, store] of storesRef.current) {
              if (!alive.has(id)) {
                store.disconnect();
                storesRef.current.delete(id);
              }
            }
            if (selected && !alive.has(selected.machineId)) setSelected(null);
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
