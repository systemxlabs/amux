// 应用入口：连接状态决定登录页面或主页面（PRD「登录页面」「主页面」）。

import { useEffect } from "react";

import { Notice } from "./components/Notice";
import { start } from "./core/actions";
import { useCore, useCoreState, usePolling } from "./core/store";
import { Login } from "./views/Login";
import { Main } from "./views/Main";

export function App() {
  const core = useCore();
  const state = useCoreState();
  usePolling(core);

  useEffect(() => {
    void start(core);
  }, [core]);

  return (
    <>
      {state.status === "online" ? <Main /> : <Login />}
      <Notice />
    </>
  );
}
