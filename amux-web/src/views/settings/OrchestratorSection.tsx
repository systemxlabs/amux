// 内置智能体设置（docs/PRD.md「内置智能体设置」）。

import { useEffect, useState } from "react";

import { Button } from "../../components/ui/button";
import { Input } from "../../components/ui/input";
import { Label } from "../../components/ui/label";
import { RadioGroup, RadioGroupItem } from "../../components/ui/radio-group";
import { saveOrchestrator } from "../../core/actions";
import { useCore, useCoreState } from "../../core/store";
import type { ApiFormat, OrchestratorConfig } from "../../lib/types";

const FORMATS: ApiFormat[] = ["chat_completions", "responses", "messages"];

/** 未配置时的初始表单值。 */
function defaultConfig(): OrchestratorConfig {
  return { apiFormat: "responses", baseUrl: "", apiKey: "", model: "", effort: "" };
}

/** 已加载配置的稳定渲染（对象的属性顺序不影响比较，字段值相同则视为未变化）。 */
function configKey(config: OrchestratorConfig | null): string {
  if (config === null) return "";
  return [config.apiFormat, config.baseUrl, config.apiKey, config.model, config.effort].join("\u0000");
}

function sameConfig(left: OrchestratorConfig, right: OrchestratorConfig): boolean {
  return configKey(left) === configKey(right);
}

export function OrchestratorSection() {
  const core = useCore();
  const state = useCoreState();
  const orchestrator = state.settings.orchestrator;
  // 未确认（读取中或失败）时无配置可编辑，退回空表单
  const loaded = orchestrator.status === "ready" ? orchestrator.config : null;
  const key = configKey(loaded);
  const [form, setForm] = useState<OrchestratorConfig>(() => loaded ?? defaultConfig());

  // 已加载配置变化（首次拉取完成、保存成功）时重置表单；key 是 loaded 的稳定渲染，故仅依赖 key
  useEffect(() => {
    setForm(loaded ?? defaultConfig());
  }, [key]);

  const changed = !sameConfig(form, loaded ?? defaultConfig());

  return (
    <section className="flex flex-col gap-4">
      <h2 className="text-sm font-medium">内置智能体</h2>
      {orchestrator.status === "failed" ? (
        <p data-slot="orchestrator-error" className="text-xs text-destructive">
          读取配置失败：{orchestrator.error}
        </p>
      ) : null}
      <div className="flex flex-col gap-1.5">
        <Label>API 格式</Label>
        <RadioGroup
          data-slot="orchestrator-api-format"
          className="flex flex-wrap items-center gap-4"
          value={form.apiFormat}
          onValueChange={(value) => setForm({ ...form, apiFormat: value as ApiFormat })}
        >
          {FORMATS.map((format) => (
            <div key={format} className="flex items-center gap-2">
              <RadioGroupItem id={`orchestrator-format-${format}`} value={format} />
              <Label htmlFor={`orchestrator-format-${format}`}>{format}</Label>
            </div>
          ))}
        </RadioGroup>
      </div>
      <div className="flex flex-col gap-1.5">
        <Label htmlFor="orchestrator-base-url">Base URL</Label>
        <Input
          id="orchestrator-base-url"
          data-slot="orchestrator-base-url"
          value={form.baseUrl}
          onChange={(event) => setForm({ ...form, baseUrl: event.target.value })}
        />
      </div>
      <div className="flex flex-col gap-1.5">
        <Label htmlFor="orchestrator-api-key">API Key</Label>
        <Input
          id="orchestrator-api-key"
          data-slot="orchestrator-api-key"
          value={form.apiKey}
          onChange={(event) => setForm({ ...form, apiKey: event.target.value })}
        />
      </div>
      <div className="flex flex-col gap-1.5">
        <Label htmlFor="orchestrator-model">模型名称</Label>
        <Input
          id="orchestrator-model"
          data-slot="orchestrator-model"
          value={form.model}
          onChange={(event) => setForm({ ...form, model: event.target.value })}
        />
      </div>
      <div className="flex flex-col gap-1.5">
        <Label htmlFor="orchestrator-effort">推理级别</Label>
        <Input
          id="orchestrator-effort"
          data-slot="orchestrator-effort"
          value={form.effort}
          onChange={(event) => setForm({ ...form, effort: event.target.value })}
        />
      </div>
      <div>
        <Button
          data-slot="orchestrator-save"
          disabled={!changed}
          onClick={() => void saveOrchestrator(core, form)}
        >
          保存
        </Button>
      </div>
    </section>
  );
}
