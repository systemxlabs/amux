// 登录页面（docs/PRD.md「登录页面」）。
//
// 建立连接过程中整页转圈；连接失败时内容居中展示错误提示、认证 token 输入框与进入按钮。
// Web 应用固定同源访问 Server，因此没有桌面应用的「Server 地址输入框」。

import { useState } from "react";

import { Button } from "../components/ui/button";
import { Input } from "../components/ui/input";
import { login } from "../core/actions";
import { useCore, useCoreState } from "../core/store";

export function Login() {
  const core = useCore();
  const state = useCoreState();
  const [token, setToken] = useState("");

  if (state.status === "connecting") {
    return (
      <div
        data-slot="login-page"
        className="flex h-full items-center justify-center"
        role="status"
        aria-label="正在建立连接"
      >
        <div
          data-slot="login-spinner"
          className="size-8 animate-spin rounded-full border-2 border-border border-t-primary"
        />
      </div>
    );
  }

  const submit = () => {
    if (token.trim() === "") return;
    void login(core, token.trim());
  };

  return (
    <div data-slot="login-page" className="flex h-full items-center justify-center">
      <form
        data-slot="login-form"
        className="flex w-80 flex-col gap-3"
        onSubmit={(event) => {
          event.preventDefault();
          submit();
        }}
      >
        <h1 className="text-center text-lg font-medium">amux</h1>
        {/* 连接失败才展示错误；本地无连接信息（无 token）时不展示 */}
        {state.error ? (
          <p
            data-slot="login-error"
            className="rounded-md border border-destructive/50 bg-card p-2 text-destructive"
          >
            {state.error}
          </p>
        ) : null}
        <label className="flex flex-col gap-1">
          <span className="text-muted-foreground">认证 token</span>
          <Input
            data-slot="login-token"
            aria-label="认证 token"
            autoFocus
            value={token}
            onChange={(event) => setToken(event.target.value)}
          />
        </label>
        <Button data-slot="login-submit" type="submit" disabled={token.trim() === ""}>
          进入
        </Button>
      </form>
    </div>
  );
}
