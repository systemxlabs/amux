/**
 * 端到端冒烟脚本（独立消费脚本，非测试内部）：
 * 对指定 runtime 的 ahal driver 跑 create → prompt → 事件 → idle → cancel → resume，
 * 整轮执行两次并比较结果一致。
 *
 * 用法：pnpm exec tsx scripts/smoke.ts <codex|claude|kimi>
 */
import { createCodexDriver } from "../ahal-codex/src/index.js";
import { createClaudeDriver } from "../ahal-claude/src/index.js";
import { createKimiDriver } from "../ahal-kimi/src/index.js";
import type { Driver, Session, SessionEvent } from "../ahal/src/index.js";

const runtime = process.argv[2] ?? "codex";
const factories: Record<string, () => Driver> = {
  codex: createCodexDriver,
  claude: createClaudeDriver,
  kimi: createKimiDriver,
};
const factory = factories[runtime];
if (!factory) {
  console.error(`未知 runtime: ${runtime}（可选 codex | claude | kimi）`);
  process.exit(1);
}

class EventCollector {
  events: SessionEvent[] = [];
  private waiters = new Set<() => void>();
  start(session: Session): void {
    void (async () => {
      for await (const se of session.events) {
        this.events.push(se);
        for (const w of this.waiters) w();
        this.waiters.clear();
      }
    })();
  }
  async waitForIdle(fromIndex: number, timeoutMs: number): Promise<number> {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      for (let i = fromIndex; i < this.events.length; i++) {
        const e = this.events[i].event;
        if (e.kind === "state_changed" && e.state === "idle") return i;
      }
      await new Promise<void>((resolve) => {
        this.waiters.add(resolve);
        setTimeout(resolve, 200);
      });
    }
    const tail = this.events.slice(-5).map((e) => JSON.stringify(e.event).slice(0, 120));
    throw new Error("waitForIdle 超时（事件尾: " + tail.join(" | ") + "）");
  }
}

function messageText(events: SessionEvent[], until: number): string {
  let text = "";
  for (let i = 0; i < until; i++) {
    const e = events[i].event;
    if ((e.kind === "agent_message" || e.kind === "agent_message_chunk") && "content" in e) {
      const c = (e as { content?: { type?: string; text?: string } | { type?: string; text?: string }[] }).content;
      if (Array.isArray(c)) {
        for (const b of c) if (b.type === "text" && b.text) text += b.text;
      } else if (c && c.type === "text" && c.text) {
        text += c.text;
      }
    }
  }
  return text;
}

function assert(cond: boolean, msg: string): void {
  if (!cond) throw new Error(`断言失败: ${msg}`);
}

async function runOnce(runNo: number): Promise<Record<string, unknown>> {
  const driver = factory();
  const summary: Record<string, unknown> = { run: runNo, turns: 0, cancelled: false, resumed: false };

  // 1. createSession → prompt → idle（turn 1）
  const session = await driver.createSession({ cwd: process.cwd() });
  const col = new EventCollector();
  col.start(session);

  await session.prompt([{ type: "text", text: "Reply with exactly: hello" }]);
  const idle1 = await col.waitForIdle(0, 180000);
  const txt1 = messageText(col.events, idle1).toLowerCase();
  assert(txt1.includes("hello"), `turn1 回复应含 hello，实际: "${txt1}"`);
  const last1 = col.events[idle1].event;
  assert(last1.kind === "state_changed" && last1.state === "idle", "turn1 末事件应为 idle");
  assert(idle1 === col.events.length - 1, "idle 应为区间最后一条事件");
  summary["turns"] = (summary["turns"] as number) + 1;

  // 2. 同会话续跑（turn 2）
  const start2 = col.events.length;
  await session.prompt([{ type: "text", text: "Reply with exactly: world" }]);
  const idle2 = await col.waitForIdle(start2, 180000);
  const txt2 = messageText(col.events.slice(start2, idle2), idle2 - start2).toLowerCase();
  assert(txt2.includes("world"), `turn2 回复应含 world，实际: "${txt2}"`);
  summary["turns"] = (summary["turns"] as number) + 1;

  // 3. cancel：长任务，接受后立即取消
  const start3 = col.events.length;
  await session.prompt([
    { type: "text", text: "Write a detailed 1500-word essay about the history of computing, one paragraph at a time" },
  ]);
  setTimeout(() => void session.cancel().catch(() => {}), 800);
  const idle3 = await col.waitForIdle(start3, 90000);
  const last3 = col.events[idle3].event;
  assert(last3.kind === "state_changed" && last3.state === "idle", "cancel 后应回到 idle");
  if (last3.kind === "state_changed" && last3.reason === "cancelled") {
    summary["cancelled"] = true;
  } else {
    console.warn(`  [warn] cancel 未产生 cancelled 收尾（reason=${(last3 as { reason?: string }).reason}），任务可能已完成`);
  }
  await session.close();

  // 4. resume：独立的干净会话 create → close → resume（验证持久化）
  const sessionA = await driver.createSession({ cwd: process.cwd() });
  const colA = new EventCollector();
  colA.start(sessionA);
  await sessionA.prompt([{ type: "text", text: "Reply with exactly: first" }]);
  await colA.waitForIdle(0, 180000);
  const sessionAId = sessionA.id;
  await sessionA.close();

  const session2 = await driver.resumeSession(sessionAId as string);
  const col2 = new EventCollector();
  col2.start(session2);
  // resume 偶发受 SDK/模型状态影响，允许一次重试
  try {
    await session2.prompt([{ type: "text", text: "Reply with exactly: resumed" }]);
    await col2.waitForIdle(0, 180000);
  } catch {
    await session2.close();
    const session2b = await driver.resumeSession(sessionAId as string);
    const col2b = new EventCollector();
    col2b.start(session2b);
    await session2b.prompt([{ type: "text", text: "Reply with exactly: resumed" }]);
    await col2b.waitForIdle(0, 180000);
    Object.assign(col2, col2b);
  }
  const idleR = await col2.waitForIdle(0, 5000);
  const txtR = messageText(col2.events, idleR).toLowerCase();
  if (!txtR.includes("resumed")) {
    const tail = col2.events.slice(-8).map((e) => JSON.stringify(e.event).slice(0, 100));
    throw new Error(`resume 回复应含 resumed，实际: "${txtR.slice(0, 200)}"（事件尾: ${tail.join(" | ")}）`);
  }
  summary["resumed"] = true;
  await session2.close();

  // 5. resume 不存在的会话 → SessionNotFoundError
  let notFound = false;
  try {
    await driver.resumeSession("does-not-exist-0000-0000-0000-000000000000");
  } catch (e) {
    notFound = (e as Error).name === "SessionNotFoundError";
  }
  assert(notFound, "resume 不存在的会话应抛 SessionNotFoundError");
  summary["resumeNotFoundChecked"] = true;

  return summary;
}

async function main(): Promise<void> {
  console.log(`=== smoke ${runtime} 开始 ===`);
  const r1 = await runOnce(1);
  const r2 = await runOnce(2);
  const { run: _r1, ...cmp1 } = r1;
  const { run: _r2, ...cmp2 } = r2;
  assert(
    JSON.stringify(cmp1) === JSON.stringify(cmp2),
    `两轮结果应一致\n${JSON.stringify(r1)}\n${JSON.stringify(r2)}`,
  );
  console.log("=== 结果 ===");
  console.log(JSON.stringify({ runtime, runs: [r1, r2] }, null, 2));
  console.log(`=== smoke ${runtime} 通过 ===`);
}

main().catch((e) => {
  console.error("SMOKE FAILED:", e);
  process.exit(1);
});
