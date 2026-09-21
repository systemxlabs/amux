// 右侧面板入口：按 state.sidePanel 渲染唯一面板（docs/PRD.md「主页面」）。
//
// sidePanel 只在适用时才被设置（openEntry 与悬浮按钮都按 panelAvailable 过滤），
// 因此这里直接渲染对应面板。

import { useCoreState } from "../../core/store";
import { ActivitiesPanel } from "./ActivitiesPanel";
import { DetailsPanel } from "./DetailsPanel";
import { DiffPanel } from "./DiffPanel";
import { PlanPanel } from "./PlanPanel";
import { TerminalPanel } from "./TerminalPanel";
import { WorkspacePanel } from "./WorkspacePanel";

export function SidePanelView() {
  const state = useCoreState();
  const target = state.open;
  const panel = state.sidePanel;
  if (panel === null || target === null) return null;

  const body = () => {
    switch (panel) {
      case "workspace":
        return <WorkspacePanel />;
      case "diff":
        return <DiffPanel />;
      case "details":
        return <DetailsPanel />;
      case "activities":
        return <ActivitiesPanel />;
      case "plan":
        return <PlanPanel />;
      case "terminal":
        return <TerminalPanel />;
    }
  };

  return (
    <div data-slot="side-panel" data-panel={panel} className="h-full min-h-0">
      {body()}
    </div>
  );
}
