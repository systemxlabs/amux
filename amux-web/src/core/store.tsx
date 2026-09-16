// React 绑定：单一 Core 实例 + 外部状态订阅 + 节拍驱动的轮询。

import { createContext, useContext, useEffect, useSyncExternalStore, type ReactNode } from "react";

import { Core, type CoreState } from "./core";
import { tick, TICK_INTERVAL } from "./poll";

const CoreContext = createContext<Core | null>(null);

export function CoreProvider({ core, children }: { core: Core; children: ReactNode }) {
  return <CoreContext.Provider value={core}>{children}</CoreContext.Provider>;
}

export function useCore(): Core {
  const core = useContext(CoreContext);
  if (!core) throw new Error("缺少 CoreProvider");
  return core;
}

/** 订阅状态；返回的状态对象每次变更后都是新的读取（组件按需读取字段）。 */
export function useCoreState(): CoreState {
  const core = useCore();
  useSyncExternalStore(core.subscribe, core.getVersion, core.getVersion);
  return core.state;
}

/** 各视图的定时刷新节拍（docs/DESIGN.md「应用」各节）。 */
export function usePolling(core: Core): void {
  useEffect(() => {
    let running = false;
    const timer = setInterval(() => {
      if (running) return;
      running = true;
      void tick(core).finally(() => {
        running = false;
      });
    }, TICK_INTERVAL);
    return () => clearInterval(timer);
  }, [core]);
}
