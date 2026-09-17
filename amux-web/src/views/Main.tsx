// 主页面（docs/PRD.md「主页面」）：左中右三块面板，面板宽度均可拖拽。
//
// 左侧为会话列表与左下角设置入口；中间为新建会话视图或会话交互视图，右侧面板默认折叠，
// 由中间面板右上方的悬浮按钮展开。

import { useCallback, useRef, useState } from "react";
import {
  Activity as ActivityIcon,
  FileDiff,
  FolderTree,
  Info,
  ListTodo,
  Settings as SettingsIcon,
  SquareTerminal,
} from "lucide-react";

import { openSettings, toggleSidePanel } from "../core/actions";
import { panelAvailable } from "../core/core";
import { useCore, useCoreState } from "../core/store";
import type { SidePanel } from "../core/core";
import { cn } from "../lib/utils";
import { InteractionView } from "./InteractionView";
import { NewSessionView } from "./NewSessionView";
import { SessionListPanel } from "./SessionListPanel";
import { SidePanelView } from "./panels";
import { SettingsOverlay } from "./settings/SettingsOverlay";

/** 面板宽度拖拽边界：拖拽只影响自身宽度，中间面板自适应剩余空间。 */
const LEFT_MIN = 180;
const LEFT_MAX = 520;
const RIGHT_MIN = 280;
const RIGHT_MAX = 900;

export function Main() {
  const core = useCore();
  const state = useCoreState();
  const [leftWidth, setLeftWidth] = useState(260);
  const [rightWidth, setRightWidth] = useState(420);
  const dragging = useRef<"left" | "right" | null>(null);

  const onMouseDown = useCallback((which: "left" | "right") => {
    dragging.current = which;
    const onMove = (event: MouseEvent) => {
      if (dragging.current === "left") {
        setLeftWidth(clamp(event.clientX, LEFT_MIN, LEFT_MAX));
      } else if (dragging.current === "right") {
        setRightWidth(clamp(window.innerWidth - event.clientX, RIGHT_MIN, RIGHT_MAX));
      }
    };
    const onUp = () => {
      dragging.current = null;
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
    };
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
  }, []);

  const panels: { panel: SidePanel; label: string; icon: typeof Info }[] = [
    { panel: "workspace", label: "工作目录", icon: FolderTree },
    { panel: "diff", label: "改动审查", icon: FileDiff },
    { panel: "details", label: "会话详情", icon: Info },
    { panel: "activities", label: "会话活动", icon: ActivityIcon },
    { panel: "plan", label: "会话计划", icon: ListTodo },
    { panel: "terminal", label: "终端", icon: SquareTerminal },
  ];

  return (
    <div data-slot="main-layout" className="flex h-full w-full overflow-hidden">
      <aside
        data-slot="left-panel"
        className="flex min-h-0 shrink-0 flex-col border-r border-border bg-card"
        style={{ width: leftWidth }}
      >
        <SessionListPanel />
        <button
          data-slot="settings-entry"
          type="button"
          className="flex items-center gap-2 border-t border-border px-3 py-2 text-left hover:bg-accent"
          onClick={() => openSettings(core)}
        >
          <SettingsIcon className="size-4" />
          设置
        </button>
      </aside>

      <div
        data-slot="panel-divider-left"
        className="w-1 shrink-0 cursor-col-resize bg-border/40 hover:bg-primary/60"
        onMouseDown={() => onMouseDown("left")}
      />

      <main
        data-slot="middle-panel"
        className="relative flex min-h-0 min-w-0 flex-1 flex-col"
      >
        {state.middle === "interaction" ? <InteractionView /> : <NewSessionView />}

        {state.middle === "interaction" && state.open ? (
          <div
            data-slot="floating-buttons"
            className="absolute top-2 right-2 z-10 flex flex-col gap-1"
          >
            {panels
              .filter((entry) => state.open !== null && panelAvailable(entry.panel, state.open))
              .map((entry) => (
                <button
                  key={entry.panel}
                  type="button"
                  data-slot="floating-button"
                  data-panel={entry.panel}
                  title={entry.label}
                  aria-label={entry.label}
                  className={cn(
                    "rounded-md border border-border bg-popover p-1.5 hover:bg-accent",
                    state.sidePanel === entry.panel && "bg-accent",
                  )}
                  onClick={() => toggleSidePanel(core, entry.panel)}
                >
                  <entry.icon className="size-4" />
                </button>
              ))}
          </div>
        ) : null}
      </main>

      {state.sidePanel ? (
        <>
          <div
            data-slot="panel-divider-right"
            className="w-1 shrink-0 cursor-col-resize bg-border/40 hover:bg-primary/60"
            onMouseDown={() => onMouseDown("right")}
          />
          <aside
            data-slot="right-panel"
            className="min-h-0 shrink-0 border-l border-border bg-card"
            style={{ width: rightWidth }}
          >
            <SidePanelView />
          </aside>
        </>
      ) : null}

      <SettingsOverlay />
    </div>
  );
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(Math.max(value, min), max);
}
