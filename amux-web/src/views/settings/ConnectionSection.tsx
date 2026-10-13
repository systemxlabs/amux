// 连接设置（docs/PRD.md「连接设置」）。
//
// Web 应用由 Server 托管并与 Server 同源，因此没有 Server 地址输入框，
// 用户只能配置认证 token。

import { useState } from "react";

import { Button } from "../../components/ui/button";
import { Input } from "../../components/ui/input";
import { Label } from "../../components/ui/label";
import { saveConnection } from "../../core/actions";
import { useCore } from "../../core/store";

export function ConnectionSection() {
  const core = useCore();
  const saved = core.client?.authToken ?? "";
  const [token, setToken] = useState(saved);

  return (
    <section className="flex flex-col gap-4">
      <h2 className="text-sm font-medium">连接</h2>
      <div className="flex flex-col gap-1.5">
        <Label htmlFor="connection-token">认证 token</Label>
        <Input
          id="connection-token"
          data-slot="connection-token"
          value={token}
          onChange={(event) => setToken(event.target.value)}
        />
      </div>
      <div className="text-xs text-muted-foreground">Server 地址：同源（当前站点地址）</div>
      <div>
        <Button
          data-slot="connection-save"
          disabled={token === saved}
          onClick={() => void saveConnection(core, token)}
        >
          保存
        </Button>
      </div>
    </section>
  );
}
