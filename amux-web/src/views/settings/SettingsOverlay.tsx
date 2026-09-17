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
  { tab: "orchestrator", label: "内置智能体" },
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
      className="fixed inset-0 z-40 flex items-center justify-center bg-black/25 lg:p-6"
      onClick={() => closeSettings(core)}
    >
      {/* 浮窗尺寸对齐桌面应用（55rem × 38.75rem，其 rem 基准 14px → 770 × 540）；
          窗口更小时按 p-6 内边距收缩。窄视口占满整屏，分类导航从侧边栏改为顶部横向标签。
          点击面板需阻止冒泡，否则会误触发遮罩的关闭 */}
      <div
        data-slot="settings-panel"
        className="flex h-full w-full flex-col overflow-hidden bg-card lg:max-h-[540px] lg:max-w-[770px] lg:flex-row lg:rounded-lg lg:border lg:border-border lg:shadow-lg"
        onClick={(event) => event.stopPropagation()}
      >
        <div
          data-slot="settings-nav"
          className="flex shrink-0 flex-col border-b border-border p-2 lg:w-44 lg:border-r lg:border-b-0"
        >
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
          <div className="flex flex-row gap-0.5 overflow-x-auto lg:flex-col">
            {TABS.map((item) => (
              <Button
                key={item.tab}
                data-slot="settings-nav-item"
                data-tab={item.tab}
                data-active={state.settings.tab === item.tab ? "true" : "false"}
                variant="ghost"
                className={cn(
                  "w-auto shrink-0 justify-start lg:w-full",
                  state.settings.tab === item.tab && "bg-accent text-accent-foreground",
                )}
                onClick={() => selectSettingsTab(core, item.tab)}
              >
                {item.label}
              </Button>
            ))}
          </div>
        </div>
        <div data-slot="settings-content" className="min-h-0 flex-1 overflow-auto p-4">
          {section(state.settings.tab)}
        </div>
      </div>
    </div>
  );
}
