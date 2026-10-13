// 自动化设置：决策模型供应商连接（docs/PRD.md「自动化设置」）。

import { useEffect, useState } from "react";

import { Button } from "../../components/ui/button";
import { Input } from "../../components/ui/input";
import { Label } from "../../components/ui/label";
import { saveAutomationConfig } from "../../core/actions";
import { useCore, useCoreState } from "../../core/store";
import type { AutomationConfig } from "../../lib/types";

function defaultConfig(): AutomationConfig {
  return {
    apiFormat: "system_one",
    endpoint: "",
    apiKey: "",
    model: "jev-latest",
  };
}

function configKey(config: AutomationConfig | null): string {
  if (config === null) return "";
  return [config.apiFormat, config.endpoint, config.apiKey, config.model].join("\u0000");
}

function sameConfig(left: AutomationConfig, right: AutomationConfig): boolean {
  return configKey(left) === configKey(right);
}

export function AutomationSection() {
  const core = useCore();
  const state = useCoreState();
  const loaded = state.settings.automation;
  const key = configKey(loaded);
  const [form, setForm] = useState<AutomationConfig>(() => loaded ?? defaultConfig());

  useEffect(() => {
    setForm(loaded ?? defaultConfig());
  }, [key]);

  const changed = !sameConfig(form, loaded ?? defaultConfig());
  const canSave =
    form.endpoint.trim() !== "" && form.apiKey.trim() !== "" && form.model.trim() !== "";

  return (
    <section className="flex flex-col gap-4">
      <h2 className="text-sm font-medium">自动化</h2>
      <div className="flex flex-col gap-1.5">
        <Label>API 格式</Label>
        <div className="text-sm text-muted-foreground">system_one</div>
      </div>
      <div className="flex flex-col gap-1.5">
        <Label htmlFor="automation-endpoint">Endpoint</Label>
        <Input
          id="automation-endpoint"
          value={form.endpoint}
          onChange={(event) => setForm({ ...form, endpoint: event.target.value })}
          placeholder="https://api.typesafe.ai/v1/systemone"
        />
      </div>
      <div className="flex flex-col gap-1.5">
        <Label htmlFor="automation-api-key">API Key</Label>
        <Input
          id="automation-api-key"
          type="password"
          value={form.apiKey}
          onChange={(event) => setForm({ ...form, apiKey: event.target.value })}
        />
      </div>
      <div className="flex flex-col gap-1.5">
        <Label htmlFor="automation-model">模型名称</Label>
        <Input
          id="automation-model"
          value={form.model}
          onChange={(event) => setForm({ ...form, model: event.target.value })}
        />
      </div>
      <Button
        className="self-start"
        disabled={!changed || !canSave}
        onClick={() => void saveAutomationConfig(core, form)}
      >
        保存
      </Button>
    </section>
  );
}
