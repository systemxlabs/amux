// 设置浮窗：半透明遮罩 + 分类导航侧边栏 + 右侧设置内容（docs/PRD.md「设置页面」）。

import { X } from "lucide-react";

import { Button } from "../../components/ui/button";
import { closeSettings, selectSettingsTab } from "../../core/actions";
import type { SettingsTab } from "../../core/core";
import { useCore, useCoreState } from "../../core/store";
import { cn } from "../../lib/utils";
import { ConnectionSection } from "./ConnectionSection";
import { MachinesSection } from "./MachinesSection";
import { OrchestratorSection } from "./OrchestratorSection";
import { QuickCommandsSection } from "./QuickCommandsSection";
import { SkillsSection } from "./SkillsSection";
import { WorkflowPlansSection } from "./WorkflowPlansSection";

/** 分类导航（数组顺序即展示顺序）。 */
const TABS: { tab: SettingsTab; label: string }[] = [
  { tab: "connection", label: "连接" },
  { tab: "machines", label: "机器管理" },
  { tab: "orchestrator", label: "编排智能体" },
  { tab: "quickCommands", label: "快捷指令" },
  { tab: "skills", label: "技能管理" },
  { tab: "plans", label: "工作流计划" },
];

function section(tab: SettingsTab) {
  switch (tab) {
    case "connection":
      return <ConnectionSection />;
    case "machines":
      return <MachinesSection />;
    case "orchestrator":
      return <OrchestratorSection />;
    case "quickCommands":
      return <QuickCommandsSection />;
    case "skills":
      return <SkillsSection />;
    case "plans":
      return <WorkflowPlansSection />;
  }
}

export function SettingsOverlay() {
  const core = useCore();
  const state = useCoreState();
  if (!state.settings.open) return null;

  return (
    <div
      data-slot="settings-overlay"
      className="fixed inset-0 z-40 bg-black/50"
      onClick={() => closeSettings(core)}
    >
      {/* 面板嵌在遮罩内，点击面板需阻止冒泡，否则会误触发遮罩的关闭 */}
      <div
        data-slot="settings-panel"
        className="fixed inset-6 z-50 flex overflow-hidden rounded-lg border border-border bg-card"
        onClick={(event) => event.stopPropagation()}
      >
        <div data-slot="settings-nav" className="flex w-44 shrink-0 flex-col border-r border-border p-2">
          <div className="mb-2 flex items-center justify-between px-2">
            <span className="text-xs font-medium text-muted-foreground">设置</span>
            <Button
              data-slot="settings-close"
              variant="ghost"
              size="icon"
              onClick={() => closeSettings(core)}
            >
              <X />
              <span className="sr-only">关闭设置</span>
            </Button>
          </div>
          <div className="flex flex-col gap-0.5">
            {TABS.map((item) => (
              <Button
                key={item.tab}
                data-slot="settings-nav-item"
                data-tab={item.tab}
                data-active={state.settings.tab === item.tab ? "true" : "false"}
                variant="ghost"
                className={cn(
                  "w-full justify-start",
                  state.settings.tab === item.tab && "bg-accent text-accent-foreground",
                )}
                onClick={() => selectSettingsTab(core, item.tab)}
              >
                {item.label}
              </Button>
            ))}
          </div>
        </div>
        <div data-slot="settings-content" className="flex-1 overflow-auto p-4">
          {section(state.settings.tab)}
        </div>
      </div>
    </div>
  );
}
