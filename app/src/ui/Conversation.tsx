import { useEffect, useMemo, useRef, useState } from "react";
import type { ContentBlock, Event, Input } from "ahal";
import type { SessionMeta } from "shared";
import { contentToText, eventsToView } from "../lib/viewModel.js";

export interface ConversationProps {
  meta: SessionMeta;
  events: readonly { seq: number; event: Event; timestamp: number }[];
  /** 事件流版本号（SessionFeed.revision）：events 原地变更，用它触发视图重算 */
  revision: number;
  onPrompt: (input: Input) => void;
  onCancel: () => void;
}

const STATE_LABEL: Record<string, string> = {
  idle: "空闲",
  thinking: "思考中",
  responding: "回复中",
  acting: "执行中",
};

function readFileAsText(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const r = new FileReader();
    r.onload = () => resolve(String(r.result));
    r.onerror = () => reject(r.error);
    r.readAsText(file);
  });
}

function readFileAsDataUrl(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const r = new FileReader();
    r.onload = () => resolve(String(r.result));
    r.onerror = () => reject(r.error);
    r.readAsDataURL(file);
  });
}

export function Conversation(props: ConversationProps) {
  const view = useMemo(() => eventsToView(props.events), [props.events, props.revision]);
  const scrollRef = useRef<HTMLDivElement>(null);
  const [pinnedToBottom, setPinnedToBottom] = useState(true);
  const [text, setText] = useState("");
  const [refs, setRefs] = useState<ContentBlock[]>([]);
  const [refPath, setRefPath] = useState("");
  const [dragOver, setDragOver] = useState(false);

  useEffect(() => {
    if (pinnedToBottom && scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
    }
  }, [props.events, pinnedToBottom]);

  const onScroll = () => {
    const el = scrollRef.current;
    if (!el) return;
    const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
    setPinnedToBottom(atBottom);
  };

  const send = () => {
    const input: Input = [];
    if (text.trim()) input.push({ type: "text", text });
    input.push(...refs);
    if (input.length === 0) return;
    props.onPrompt(input);
    setText("");
    setRefs([]);
  };

  const addRef = () => {
    const p = refPath.trim();
    if (!p) return;
    const name = p.split("/").pop() ?? p;
    setRefs((r) => [...r, { type: "resource_link", uri: p.startsWith("/") ? `file://${p}` : p, name }]);
    setRefPath("");
  };

  const onDrop = async (e: React.DragEvent) => {
    e.preventDefault();
    setDragOver(false);
    const files = Array.from(e.dataTransfer.files);
    for (const f of files) {
      if (f.type.startsWith("image/")) {
        const dataUrl = await readFileAsDataUrl(f);
        setRefs((r) => [...r, { type: "resource", mimeType: f.type, blob: dataUrl }]);
      } else {
        const t = await readFileAsText(f);
        setRefs((r) => [...r, { type: "resource", mimeType: f.type || "text/plain", text: t }]);
      }
    }
  };

  const onPaste = async (e: React.ClipboardEvent) => {
    for (const item of Array.from(e.clipboardData.items)) {
      if (item.type.startsWith("image/")) {
        e.preventDefault();
        const f = item.getAsFile();
        if (!f) continue;
        const dataUrl = await readFileAsDataUrl(f);
        setRefs((r) => [...r, { type: "resource", mimeType: f.type, blob: dataUrl }]);
      }
    }
  };

  return (
    <section className="conversation">
      <div className="conversation-head">
        <span className="session-state badge">{STATE_LABEL[view.state] ?? view.state}</span>
        <span className="mono">{props.meta.harness}</span>
        <span className="mono dim">{props.meta.cwd}</span>
        {view.usage && (
          <span className="mono dim usage">
            {view.usage.context.toLocaleString()} / {view.usage.contextWindow.toLocaleString()} tokens
          </span>
        )}
      </div>
      <div
        className={`conversation-scroll ${dragOver ? "drag-over" : ""}`}
        ref={scrollRef}
        onScroll={onScroll}
        onDragOver={(e) => {
          e.preventDefault();
          setDragOver(true);
        }}
        onDragLeave={() => setDragOver(false)}
        onDrop={(e) => void onDrop(e)}
      >
        {view.bubbles.length === 0 && view.tools.length === 0 && <div className="empty-hint">尚无内容 · 可拖拽文件/粘贴图片作为上下文</div>}
        {view.bubbles.map((b) => (
          <div key={b.key} className={`bubble ${b.kind === "thought" ? "thought" : "message"}`}>
            <div className="bubble-label">{b.kind === "thought" ? "思考" : "Agent"}</div>
            <pre className="bubble-content">{contentToText(b.content)}</pre>
          </div>
        ))}
        {view.tools.map((t) => (
          <div key={t.key} className={`tool-call status-${t.status}`}>
            <div className="tool-head">
              <span className="tool-name">{t.name ?? "工具"}</span>
              {t.title && <span className="dim">{t.title}</span>}
              <span className="tool-status">{t.status}</span>
            </div>
            {t.content.length > 0 && <pre className="tool-content">{contentToText(t.content)}</pre>}
          </div>
        ))}
        {view.errors.map((e, i) => (
          <div key={i} className="error-line">
            错误：{e}
          </div>
        ))}
        {!pinnedToBottom && (
          <button className="btn subtle jump-bottom" onClick={() => setPinnedToBottom(true)}>
            ⬇ 跳回底部
          </button>
        )}
      </div>
      {refs.length > 0 && (
        <div className="refs">
          {refs.map((r, i) => (
            <span key={i} className="ref-chip" title={r.type === "resource_link" ? r.uri : undefined}>
              {r.type === "text" ? "文本" : r.type === "resource_link" ? `@${r.name}` : `[${r.mimeType}]`}
              <button className="ref-remove" onClick={() => setRefs((list) => list.filter((_, j) => j !== i))}>
                ✕
              </button>
            </span>
          ))}
        </div>
      )}
      <div className="conversation-input">
        <textarea
          value={text}
          onChange={(e) => setText(e.target.value)}
          onPaste={(e) => void onPaste(e)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
              e.preventDefault();
              send();
            }
          }}
          placeholder="输入消息（Ctrl/Cmd+Enter 发送；忙时发送即 steer）；拖拽文件、粘贴图片、@ 引用路径"
          rows={2}
        />
        <div className="ref-input">
          <input
            value={refPath}
            onChange={(e) => setRefPath(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                addRef();
              }
            }}
            placeholder="@ 引用文件或目录路径"
          />
          <button className="btn subtle small" onClick={addRef} disabled={!refPath.trim()}>
            引用
          </button>
        </div>
        <div className="input-actions">
          <span className="dim">{view.state === "idle" ? "空闲：发送将启动新工作" : "忙：发送将作为 steer 注入"}</span>
          <button className="btn primary" onClick={send} disabled={!text.trim() && refs.length === 0}>
            发送
          </button>
          <button className="btn" onClick={props.onCancel} disabled={view.state === "idle"}>
            取消
          </button>
        </div>
      </div>
    </section>
  );
}
