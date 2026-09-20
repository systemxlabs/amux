// 会话活动视图：活动条目列表（docs/PRD.md「主页面」、docs/DESIGN.md「活动视图」）。

import { useEffect, useLayoutEffect, useRef, useState } from "react";

import { loadNewerActivities, loadOlderActivities } from "../../core/poll";
import { useCore, useCoreState } from "../../core/store";
import {
  activityDetail,
  activityKindLabel,
  activitySummary,
  formatTime,
} from "../../lib/format";
import { pageSizeForViewport } from "../../lib/paging";
import type { Activity } from "../../lib/types";

/** 距顶/底多少像素内视为「滚动到边界」（只有真正到达边界才预取下一页）。 */
const EDGE_THRESHOLD = 8;

type RowProps = { activity: Activity; expanded: boolean; onToggle: () => void };

function ActivityRow({ activity, expanded, onToggle }: RowProps) {
  return (
    <div
      data-slot="activity-item"
      role="button"
      tabIndex={0}
      className="cursor-pointer rounded-md bg-muted/40 p-2 hover:bg-muted"
      onClick={onToggle}
      onKeyDown={(event) => {
        if (event.key === "Enter" || event.key === " ") onToggle();
      }}
    >
      <div data-slot="activity-summary" className="flex items-start gap-2">
        <span className="shrink-0 text-xs text-muted-foreground">{activityKindLabel(activity)}</span>
        <span className="min-w-0 flex-1 truncate text-sm">
          {activitySummary(activity)}
        </span>
        <span className="shrink-0 text-xs text-muted-foreground">
          {formatTime(activity.timestamp)}
        </span>
      </div>
      {expanded && (
        <div
          data-slot="activity-detail"
          className="mt-1 whitespace-pre-wrap text-xs text-muted-foreground"
        >
          {activityDetail(activity)}
        </div>
      )}
    </div>
  );
}

export function ActivitiesPanel() {
  const core = useCore();
  const state = useCoreState();
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const previousHeight = useRef(0);
  const scrollOnEntry = useRef(true);
  const followingBottom = useRef(true);
  const [expanded, setExpanded] = useState<string[]>([]);

  const activities = state.detail.activities;
  const paging = state.detail.activitiesPaging;
  const shift = paging.shift;
  const target = state.open;

  /**
   * 内容不足一屏时主动补拉更早一页。
   *
   * 此时面板没有可滚动区域，滚轮与滚动条都不产生 scroll 事件，只在边界预取的 handleScroll
   * 永远不触发（大窗口下列表卡在第一页，表现为「滚轮完全没反应」）。补拉到填满可视区或没有
   * 更早的活动为止。
   */
  const fillViewport = (): void => {
    const element = scrollRef.current;
    if (element === null) return;
    const current = core.state.detail.activitiesPaging;
    if (!current.hasOlder || current.loadingOlder) return;
    if (element.scrollHeight > element.clientHeight) return;
    void loadOlderActivities(core);
  };

  // 每条进入时默认滚动到底部；切换会话（面板保持打开）时重新贴底
  useLayoutEffect(() => {
    scrollOnEntry.current = true;
    followingBottom.current = true;
  }, [core, target?.kind, target?.id]);

  useLayoutEffect(() => {
    const element = scrollRef.current;
    if (element === null || activities.length === 0) return;
    if (!scrollOnEntry.current && !followingBottom.current) return;
    element.scrollTop = element.scrollHeight;
    scrollOnEntry.current = false;
  }, [activities]);

  // 更早一页插入后内容整体下移：在下一帧之前按插入高度补偿滚动位置，
  // 让插入前可见的条目停在原处（上一帧已记录插入前的内容高度）
  useLayoutEffect(() => {
    const element = scrollRef.current;
    if (element === null) return;
    const height = element.scrollHeight;
    if (shift !== null) {
      if (previousHeight.current > 0) element.scrollTop += height - previousHeight.current;
      core.update((next) => {
        next.detail.activitiesPaging.shift = null;
      });
    }
    previousHeight.current = height;
  }, [core, activities, shift, expanded]);

  // 页大小随可视高度自适应：首次渲染与尺寸变化时也重新计算；顺带补拉填满可视区
  useEffect(() => {
    const element = scrollRef.current;
    if (element === null) return;
    const updatePageSize = () => {
      const size = pageSizeForViewport(
        element.clientHeight,
        element.scrollHeight,
        core.state.detail.activities.length,
      );
      core.update((next) => {
        next.detail.activitiesPaging.pageSize = size;
      });
      fillViewport();
    };
    updatePageSize();
    const observer = new ResizeObserver(updatePageSize);
    observer.observe(element);
    window.addEventListener("resize", updatePageSize);
    return () => {
      observer.disconnect();
      window.removeEventListener("resize", updatePageSize);
    };
  }, [core, core.state.detail.activities.length]);

  // 每次窗口内容变化后补拉一次：补拉结果会再次触发本副作用，直到填满可视区或没有更早的活动。
  // 失败时（内容不变）不会重试，避免请求风暴，留待下一次滚动或刷新节拍。
  useEffect(() => {
    fillViewport();
  }, [core, activities]);

  const handleScroll = () => {
    const element = scrollRef.current;
    if (element === null) return;
    followingBottom.current =
      element.scrollHeight - element.scrollTop - element.clientHeight <= 1;
    const paging = core.state.detail.activitiesPaging;
    const pageSize = pageSizeForViewport(
      element.clientHeight,
      element.scrollHeight,
      core.state.detail.activities.length,
    );
    if (pageSize !== paging.pageSize) {
      core.update((next) => {
        next.detail.activitiesPaging.pageSize = pageSize;
      });
    }
    const nearOlderEdge = element.scrollTop <= EDGE_THRESHOLD;
    const nearNewerEdge =
      element.scrollHeight - element.scrollTop - element.clientHeight <= EDGE_THRESHOLD;
    if (nearOlderEdge && paging.hasOlder && !paging.loadingOlder) {
      void loadOlderActivities(core);
    }
    if (nearNewerEdge && paging.hasNewer && !paging.loadingNewer) {
      void loadNewerActivities(core);
    }
  };

  const toggle = (id: string) => {
    setExpanded((prev) => (prev.includes(id) ? prev.filter((item) => item !== id) : [...prev, id]));
  };

  return (
    <div data-slot="activities-panel" className="flex h-full min-h-0 flex-col">
      <div
        ref={scrollRef}
        onScroll={handleScroll}
        className="flex min-h-0 flex-1 flex-col gap-2 overflow-y-auto p-3"
      >
        {activities.length === 0 ? (
          <div className="text-xs text-muted-foreground">暂无活动</div>
        ) : (
          activities.map((activity) => (
            <ActivityRow
              key={activity.id}
              activity={activity}
              expanded={expanded.includes(activity.id)}
              onToggle={() => toggle(activity.id)}
            />
          ))
        )}
      </div>
    </div>
  );
}
