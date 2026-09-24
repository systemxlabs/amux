// 主页面（docs/PRD.md「主页面」）：左中右三块面板，面板宽度均可拖拽。
//
// 左侧为会话列表与左下角设置入口；中间为新建会话视图或会话交互视图，右侧面板默认折叠，
// 由中间面板右上方的悬浮按钮展开。
//
// 窄视口（手机浏览器）下三栏放不下：左栏收成贴在左侧的抽屉浮层（由中间面板左上方的悬浮按钮打开，
// 选中会话后自动收起），右侧面板改为整屏浮层（自带关闭按钮，因为此时悬浮按钮被浮层盖住），
// 面板分隔条与拖拽宽度只在宽视口下有效。

import { useCallback, useEffect, useRef, useState } from "react";
import {
  Activity as ActivityIcon,
  FileDiff,
  FolderTree,
  Info,
  ListTodo,
  Menu,
  Paperclip,
  Settings as SettingsIcon,
  SquareTerminal,
  X,
} from "lucide-react";

import { openSettings, toggleSidePanel } from "../core/actions";
import { panelAvailable } from "../core/core";
import { useCore, useCoreState } from "../core/store";
import type { SidePanel } from "../core/core";
import { cn } from "../lib/utils";
import { useIsMobile } from "../lib/viewport";
import { InteractionView } from "./InteractionView";
import { NewSessionView } from "./NewSessionView";
import { SessionListPanel } from "./SessionListPanel";
import { SidePanelView } from "./panels";
import { SettingsOverlay } from "./settings/SettingsOverlay";

/**
 * 面板宽度拖拽边界（docs/PRD.md「主页面」：不限制最大宽度，最小宽度以保证可拖拽回为准）。
 *
 * 下限取 0：分隔条是面板之外的独立元素，面板宽度归零后分隔条仍留在原位，可以再拖回来。
 * 上限取「视口宽度 − 分隔条宽度」：面板再宽也只会把分隔条挤出屏幕、反而拖不回来，
 * 因此上限刚好留出分隔条的位置（分隔条在左栏之后，会被溢出到视口外的是它自己）。
 */
const PANEL_MIN_WIDTH = 0;
/** 分隔条宽度，与分隔条的 `w-1` 保持一致。 */
const PANEL_DIVIDER_WIDTH = 4;
const panelMaxWidth = (): number => window.innerWidth - PANEL_DIVIDER_WIDTH;

export function Main() {
  const core = useCore();
  const state = useCoreState();
  const isMobile = useIsMobile();
  const [leftWidth, setLeftWidth] = useState(260);
  const [rightWidth, setRightWidth] = useState(420);
  const [drawerOpen, setDrawerOpen] = useState(false);
  const dragging = useRef<"left" | "right" | null>(null);

  const onMouseDown = useCallback((which: "left" | "right") => {
    dragging.current = which;
    const onMove = (event: MouseEvent) => {
      if (dragging.current === "left") {
        setLeftWidth(clamp(event.clientX, PANEL_MIN_WIDTH, panelMaxWidth()));
      } else if (dragging.current === "right") {
        setRightWidth(clamp(window.innerWidth - event.clientX, PANEL_MIN_WIDTH, panelMaxWidth()));
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

  // 回到宽视口后抽屉状态不再有意义：留着会让下次进窄视口时左栏直接是展开的
  useEffect(() => {
    if (!isMobile) setDrawerOpen(false);
  }, [isMobile]);

  const closeDrawer = useCallback(() => setDrawerOpen(false), []);

  const panels: { panel: SidePanel; label: string; icon: typeof Info }[] = [
    { panel: "exec", label: "执行目录", icon: FolderTree },
    { panel: "diff", label: "改动审查", icon: FileDiff },
    { panel: "details", label: "会话详情", icon: Info },
    { panel: "activities", label: "会话活动", icon: ActivityIcon },
    { panel: "attachments", label: "附件", icon: Paperclip },
    { panel: "plan", label: "会话计划", icon: ListTodo },
    { panel: "terminal", label: "终端", icon: SquareTerminal },
  ];

  const openPanel = state.sidePanel;
  const openPanelLabel = panels.find((entry) => entry.panel === openPanel)?.label ?? "";

  return (
    <div data-slot="main-layout" className="flex h-full w-full overflow-hidden">
      {drawerOpen ? (
        <div
          data-slot="drawer-backdrop"
          className="fixed inset-0 z-20 bg-black/25 lg:hidden"
          onClick={closeDrawer}
        />
      ) : null}

      <aside
        data-slot="left-panel"
        data-open={drawerOpen ? "true" : "false"}
        className={cn(
          // overflow-hidden：面板可被拖到接近 0 宽，内容溢出会盖住分隔条（见 middle-panel）
          "min-h-0 flex-col overflow-hidden border-r border-border bg-card",
          // 窄视口：固定定位的抽屉，收起时不渲染（不占布局空间；也不给内部 fixed 的
          // 会话右键菜单引入偏移的包含块）。抽屉是浮层、没有分隔条可以拖回来，
          // 因此给一个最小宽度，避免宽视口下把左栏拖到 0 之后进手机浏览器看不到会话列表
          "fixed inset-y-0 left-0 z-30 min-w-[200px] max-w-[85vw] shadow-lg",
          drawerOpen ? "flex" : "hidden",
          // 宽视口：回到三栏布局中的普通一栏
          "lg:static lg:z-auto lg:flex lg:min-w-0 lg:max-w-none lg:shrink-0 lg:shadow-none",
        )}
        style={{ width: leftWidth }}
      >
        <SessionListPanel onNavigate={closeDrawer} />
        <button
          data-slot="settings-entry"
          type="button"
          className="flex items-center gap-2 border-t border-border px-3 py-3 text-left hover:bg-accent lg:py-2"
          onClick={() => {
            closeDrawer();
            openSettings(core);
          }}
        >
          <SettingsIcon className="size-4" />
          设置
        </button>
      </aside>

      <div
        data-slot="panel-divider-left"
        className="hidden w-1 shrink-0 cursor-col-resize bg-border/40 hover:bg-primary/60 lg:block"
        onMouseDown={(event) => {
          // 阻止默认行为：否则拖拽会顺带在面板里刷出一片文字选区
          event.preventDefault();
          onMouseDown("left");
        }}
      />

      <main
        data-slot="middle-panel"
        // overflow-hidden：面板可以被拖到接近 0 宽，内容溢出会盖住相邻的分隔条，分隔条就点不到了
        className="relative flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden"
      >
        {state.middle === "interaction" ? <InteractionView /> : <NewSessionView />}

        {/* 窄视口下左栏收在抽屉里，需要一个入口把它拉出来；宽视口下左栏常驻，不渲染 */}
        <button
          data-slot="drawer-toggle"
          type="button"
          aria-label="打开会话列表"
          className="absolute top-2 left-2 z-10 rounded-md border border-border bg-popover p-2.5 hover:bg-accent lg:hidden"
          onClick={() => setDrawerOpen(true)}
        >
          <Menu className="size-4" />
        </button>

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
                    "rounded-md border border-border bg-popover p-2.5 hover:bg-accent lg:p-1.5",
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

      {openPanel !== null ? (
        <>
          <div
            data-slot="panel-divider-right"
            className="hidden w-1 shrink-0 cursor-col-resize bg-border/40 hover:bg-primary/60 lg:block"
            onMouseDown={(event) => {
              event.preventDefault();
              onMouseDown("right");
            }}
          />
          <aside
            data-slot="right-panel"
            className="fixed inset-0 z-30 flex min-h-0 flex-col overflow-hidden bg-card lg:static lg:z-auto lg:shrink-0 lg:border-l lg:border-border"
            style={isMobile ? undefined : { width: rightWidth }}
          >
            <div className="flex shrink-0 items-center justify-between border-b border-border px-3 py-1 lg:hidden">
              <span className="text-sm font-medium">{openPanelLabel}</span>
              <button
                data-slot="side-panel-close"
                type="button"
                aria-label="关闭面板"
                className="rounded-md p-2.5 text-muted-foreground hover:bg-accent hover:text-foreground"
                onClick={() => toggleSidePanel(core, openPanel)}
              >
                <X className="size-4" />
              </button>
            </div>
            <div className="min-h-0 flex-1">
              <SidePanelView />
            </div>
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
