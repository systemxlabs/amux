// 右侧面板入口：按 state.sidePanel 渲染唯一面板（docs/PRD.md「主页面」）。

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

  // 工作目录/改动审查/计划/终端仅普通会话有（docs/PRD.md「主页面」）
  const sessionOnly = target.kind === "session";
  const body = () => {
    switch (panel) {
      case "workspace":
        return sessionOnly ? <WorkspacePanel /> : null;
      case "diff":
        return sessionOnly ? <DiffPanel /> : null;
      case "details":
        return <DetailsPanel />;
      case "activities":
        return <ActivitiesPanel />;
      case "plan":
        return sessionOnly ? <PlanPanel /> : null;
      case "terminal":
        return sessionOnly ? <TerminalPanel /> : null;
    }
  };

  return (
    <div data-slot="side-panel" data-panel={panel} className="h-full min-h-0">
      {body()}
    </div>
  );
}
