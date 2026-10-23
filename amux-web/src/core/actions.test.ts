// 会话列表与对话的主动刷新（docs/DESIGN.md「会话列表刷新机制」）与打开会话时的面板收敛
// （docs/PRD.md「主页面」：执行目录、改动审查、计划、终端仅普通会话展示）。

import { describe, expect, it } from "vitest";

import type { ApiClient } from "../lib/api";
import type {
  ContentBlock,
  Group,
  GroupList,
  ListEntry,
  Session,
  SessionList,
  Workflow,
  WorkflowList,
} from "../lib/types";
import {
  addFiles,
  cancelOpen,
  createFromEntry,
  createWorkflow,
  deleteRecentWorkspace,
  deleteProject,
  openEntry,
  openTerminal,
  retryAttachment,
  sendPrompt,
  showNewSession,
  toggleEntryPinned,
  toggleSidePanel,
} from "./actions";
import { Core } from "./core";

function session(): Session {
  return {
    id: "s1",
    machine: "localpc",
    agent: "codex",
    title: "会话",
    state: "idle",
    pinned: false,
    unread: false,
    workspace: "/w",
    worktreeDir: "",
    createdAt: 1,
    updatedAt: 1,
  };
}

function workflow(): Workflow {
  return {
    id: "w1",
    title: "工作流",
    state: "idle",
    plan: "计划",
    pinned: false,
    unread: false,
    createdAt: 1,
    updatedAt: 1,
    linkedSessions: [],
  };
}

function group(): Group {
  return {
    id: "g1",
    title: "群聊",
    state: "idle",
    pinned: false,
    unread: false,
    createdAt: 1,
    updatedAt: 1,
    members: [],
    sessions: [],
  };
}

const sessionEntry: ListEntry = { kind: "session", session: session() };
const workflowEntry: ListEntry = { kind: "workflow", workflow: workflow() };

it("置顶普通会话和工作流都通过 configure 更新并刷新列表", async () => {
  const core = new Core();
  core.state.status = "online";
  const calls: unknown[][] = [];
  core.client = {
    configureSession: async (...args: unknown[]): Promise<void> => {
      calls.push(args);
    },
    configureWorkflow: async (...args: unknown[]): Promise<void> => {
      calls.push(args);
    },
    sessions: async (): Promise<SessionList> => ({ sessions: [], hasMore: false }),
    workflows: async (): Promise<WorkflowList> => ({ workflows: [], hasMore: false }),
  } as unknown as ApiClient;

  await toggleEntryPinned(core, sessionEntry);
  await toggleEntryPinned(core, workflowEntry);

  expect(calls).toEqual([
    ["s1", "会话", null, null, true],
    ["w1", "工作流", null, true],
  ]);
});

it("从普通会话新建时保留机器、agent、工作目录、项目和 worktree 设置", async () => {
  const core = new Core();
  core.state.status = "online";
  let request: unknown;
  core.client = {
    createSession: async (input: unknown) => {
      request = input;
      return { ...session(), id: "s2" };
    },
    listBranches: async () => ({
      branches: [
        {
          name: "main",
          fullName: "refs/heads/main",
          kind: "local",
          isWorktreeBase: true,
          isRemoteDefault: false,
        },
      ],
    }),
    sessions: async (): Promise<SessionList> => ({ sessions: [], hasMore: false }),
    workflows: async (): Promise<WorkflowList> => ({ workflows: [], hasMore: false }),
  } as unknown as ApiClient;

  const source = session();
  source.project = "project-a";
  source.worktreeDir = "/tmp/worktree-s1";

  await createFromEntry(core, { kind: "session", session: source });

  expect(request).toEqual({
    machine: "localpc",
    agent: "codex",
    workspace: "/w",
    useWorktree: true,
    worktreeBase: "refs/heads/main",
    project: "project-a",
  });
  expect(core.state.open).toEqual({ kind: "session", id: "s2" });
});

it("从工作流会话新建时保留计划与项目并切换", async () => {
  const core = new Core();
  core.state.status = "online";
  let request: unknown[] = [];
  core.client = {
    createWorkflow: async (...args: unknown[]) => {
      request = args;
      return { ...workflow(), id: "w2" };
    },
    sessions: async (): Promise<SessionList> => ({ sessions: [], hasMore: false }),
    workflows: async (): Promise<WorkflowList> => ({ workflows: [], hasMore: false }),
  } as unknown as ApiClient;

  const source = workflow();
  source.project = "project-a";

  await createFromEntry(core, { kind: "workflow", workflow: source });

  expect(request).toEqual(["计划", null, "project-a"]);
  expect(core.state.open).toEqual({ kind: "workflow", id: "w2" });
});

it("从群聊会话新建时保留成员、项目和 worktree 设置", async () => {
  const core = new Core();
  core.state.status = "online";
  let request: unknown[] = [];
  core.client = {
    createGroup: async (...args: unknown[]) => {
      request = args;
      return { ...group(), id: "g2" };
    },
    listBranches: async () => ({
      branches: [
        {
          name: "main",
          fullName: "refs/heads/main",
          kind: "local",
          isWorktreeBase: true,
          isRemoteDefault: false,
        },
      ],
    }),
    sessions: async (): Promise<SessionList> => ({ sessions: [], hasMore: false }),
    workflows: async (): Promise<WorkflowList> => ({ workflows: [], hasMore: false }),
    groups: async (): Promise<GroupList> => ({ groups: [], hasMore: false }),
  } as unknown as ApiClient;

  const ordinary = { ...session(), id: "s1", workspace: "/ordinary" };
  const worktree = {
    ...session(),
    id: "s2",
    workspace: "/repo",
    worktreeDir: "/worktrees/s2",
  };
  const source: Group = {
    ...group(),
    project: "project-a",
    members: [
      {
        id: "m1",
        name: "coder",
        description: "负责实现",
        machine: "localpc",
        agent: "codex",
        sessionId: "s1",
        pinned: false,
        createdAt: 1,
        updatedAt: 1,
      },
      {
        id: "m2",
        name: "reviewer",
        description: "负责审查",
        machine: "localpc",
        agent: "codex",
        sessionId: "s2",
        pinned: false,
        createdAt: 1,
        updatedAt: 1,
      },
    ],
    sessions: [ordinary, worktree],
  };

  await createFromEntry(core, { kind: "group", group: source });

  expect(request).toEqual([
    [
      {
        name: "coder",
        description: "负责实现",
        machine: "localpc",
        agent: "codex",
        workspace: "/ordinary",
        useWorktree: false,
        worktreeBase: undefined,
      },
      {
        name: "reviewer",
        description: "负责审查",
        machine: "localpc",
        agent: "codex",
        workspace: "/repo",
        useWorktree: true,
        worktreeBase: "refs/heads/main",
      },
    ],
    "project-a",
  ]);
  expect(core.state.open).toEqual({ kind: "group", id: "g2" });
});

it("附件上传成功后以待发送 ResourceLink 保存，不再内联文件内容", async () => {
  const core = new Core();
  core.state.open = { kind: "session", id: "s1" };
  core.client = {
    uploadSessionAttachment: async () => ({
      name: "uuid.txt",
      size: 5,
      createdAt: 1,
    }),
    sessionAttachmentUri: () =>
      "https://amux.example.com/sessions/s1/attachments/uuid.txt",
  } as unknown as ApiClient;

  await addFiles(core, [new File(["hello"], "note.txt", { type: "text/plain" })]);

  expect(core.state.attachments).toHaveLength(1);
  expect(core.state.attachments[0]).toMatchObject({
      id: expect.any(String),
      file: expect.any(File),
      status: "uploaded",
      block: {
        type: "resource_link",
        uri: "https://amux.example.com/sessions/s1/attachments/uuid.txt",
        name: "note.txt",
        mimeType: "text/plain",
      },
      label: "note.txt",
      remoteName: "uuid.txt",
  });
});

it("附件上传失败后保留文件并可重试", async () => {
  const core = new Core();
  core.state.open = { kind: "session", id: "s1" };
  let attempts = 0;
  core.client = {
    uploadSessionAttachment: async () => {
      attempts += 1;
      if (attempts === 1) throw new Error("network down");
      return { name: "uuid.txt", size: 5, createdAt: 1 };
    },
    sessionAttachmentUri: () =>
      "https://amux.example.com/sessions/s1/attachments/uuid.txt",
  } as unknown as ApiClient;

  await addFiles(core, [new File(["hello"], "note.txt", { type: "text/plain" })]);
  expect(core.state.attachments[0]?.status).toBe("failed");
  expect(core.state.attachments[0]?.error).toContain("network down");

  await retryAttachment(core, core.state.attachments[0]!.id);
  expect(core.state.attachments[0]).toMatchObject({
    status: "uploaded",
    remoteName: "uuid.txt",
  });
});

it("删除项目时同步移除其快捷指令", async () => {
  const core = new Core();
  core.state.settings.projects = [
    { name: "project-a", description: "" },
    { name: "project-b", description: "" },
  ];
  core.state.settings.quickCommands = [
    { project: "project-a", name: "项目指令", prompt: "a" },
    { project: "project-b", name: "项目指令", prompt: "b" },
    { name: "通用指令", prompt: "general" },
  ];
  core.client = {
    deleteProject: async (): Promise<void> => {},
    sessions: async (): Promise<SessionList> => ({ sessions: [], hasMore: false }),
    workflows: async (): Promise<WorkflowList> => ({ workflows: [], hasMore: false }),
  } as unknown as ApiClient;

  await deleteProject(core, "project-a");

  expect(core.state.settings.projects.map((project) => project.name)).toEqual(["project-b"]);
  expect(core.state.settings.quickCommands).toEqual([
    { project: "project-b", name: "项目指令", prompt: "b" },
    { name: "通用指令", prompt: "general" },
  ]);
});

it("删除最近工作目录：仅保留剩余项并全量保存", async () => {
  const core = new Core();
  core.state.recentWorkspaces = [
    { machine: "localpc", workspace: "/w1", lastUsed: 1 },
    { machine: "localpc", workspace: "/w2", lastUsed: 2 },
  ];
  const sent: unknown[] = [];
  core.client = {
    setRecentWorkspaces: async (list: unknown[]): Promise<void> => {
      sent.push(list);
    },
  } as unknown as ApiClient;

  await deleteRecentWorkspace(core, "localpc", "/w1");

  expect(sent).toEqual([
    [{ machine: "localpc", workspace: "/w2", lastUsed: 2 }],
  ]);
  expect(core.state.recentWorkspaces).toEqual([
    { machine: "localpc", workspace: "/w2", lastUsed: 2 },
  ]);
});

it("重新打开新建视图收起面板，创建失败保留模式和完整草稿", async () => {
  const core = new Core();
  core.state.middle = "interaction";
  core.state.sidePanel = "exec";
  core.state.newSession = {
    ...core.state.newSession,
    mode: "workflow",
    machine: "localpc",
    agent: "codex",
    workspace: "/draft",
    useWorktree: true,
    plan: "尚未创建的计划",
  };
  const draft = structuredClone(core.state.newSession);

  showNewSession(core);
  expect(core.state.middle).toBe("new");
  expect(core.state.sidePanel).toBeNull();
  expect(core.state.newSession).toEqual(draft);

  core.client = {
    createWorkflow: async () => { throw new Error("创建失败"); },
  } as unknown as ApiClient;
  await createWorkflow(core);
  expect(core.state.middle).toBe("new");
  expect(core.state.newSession).toEqual(draft);
});

describe("openEntry", () => {
  it("切到工作流会话时关闭仅普通会话有的面板", async () => {
    const core = new Core();
  core.state.sidePanel = "exec";

    await openEntry(core, workflowEntry);

    expect(core.state.sidePanel).toBeNull();
    expect(core.state.open).toEqual({ kind: "workflow", id: "w1" });
  });

  it("适用于工作流会话的面板在切换后保留", async () => {
    const core = new Core();
    core.state.sidePanel = "details";

    await openEntry(core, workflowEntry);

    expect(core.state.sidePanel).toBe("details");
  });

  it("切到普通会话时保留仅普通会话有的面板", async () => {
    const core = new Core();
    core.state.sidePanel = "plan";

    await openEntry(core, sessionEntry);

    expect(core.state.sidePanel).toBe("plan");
  });
});

describe("cancelOpen", () => {
  it("工作流会话取消以用户消息下发，并主动刷新对话与会话列表", async () => {
    const calls: string[] = [];
    const client = {
      promptWorkflow: async (): Promise<void> => {
        calls.push("promptWorkflow");
      },
      workflowHistory: async () => {
        calls.push("workflowHistory");
        return { items: [], hasMore: false };
      },
      sessions: async (): Promise<SessionList> => {
        calls.push("sessions");
        return { sessions: [], hasMore: false };
      },
      workflows: async (): Promise<WorkflowList> => {
        calls.push("workflows");
        return { workflows: [], hasMore: false };
      },
    } as unknown as ApiClient;
    const core = new Core();
    core.client = client;
    core.state.status = "online";
    core.state.open = { kind: "workflow", id: "w1" };

    await cancelOpen(core);

    expect(calls).toEqual(["promptWorkflow", "sessions", "workflows", "workflowHistory"]);
  });

  it("普通会话取消不发送用户消息，因此不刷新会话列表", async () => {
    const calls: string[] = [];
    const client = {
      cancelSession: async (): Promise<void> => {
        calls.push("cancelSession");
      },
      history: async () => {
        calls.push("history");
        return { items: [], hasMore: false };
      },
      sessions: async (): Promise<SessionList> => {
        calls.push("sessions");
        return { sessions: [], hasMore: false };
      },
    } as unknown as ApiClient;
    const core = new Core();
    core.client = client;
    core.state.status = "online";
    core.state.open = { kind: "session", id: "s1" };

    await cancelOpen(core);

    expect(calls).toEqual(["cancelSession", "history"]);
  });
});

describe("sendPrompt", () => {
  /** 记录 prompt 载荷并返回最小可用的假客户端。 */
  function promptingClient(): { client: ApiClient; prompts: ContentBlock[][] } {
    const prompts: ContentBlock[][] = [];
    const client = {
      promptSession: async (_id: string, input: ContentBlock[]) => {
        prompts.push(input);
      },
      history: async () => ({ items: [], hasMore: false }),
      sessions: async (): Promise<SessionList> => ({ sessions: [], hasMore: false }),
      workflows: async (): Promise<WorkflowList> => ({ workflows: [], hasMore: false }),
    } as unknown as ApiClient;
    return { client, prompts };
  }

  function withAttachments(core: Core): void {
    core.update((state) => {
      state.attachments = [
        {
          id: "pending-1",
          file: new File(["draft"], "draft.txt", { type: "text/plain" }),
          block: { type: "text", text: "draft.txt" },
          label: "draft.txt",
          status: "uploaded",
          remoteName: "remote.txt",
        },
      ];
    });
  }

  it("快捷指令只发送预设提示词，不携带也不清空待发送附件", async () => {
    const { client, prompts } = promptingClient();
    const core = new Core();
    core.client = client;
    core.state.open = { kind: "session", id: "s1" };
    withAttachments(core);

    await sendPrompt(core, "检查构建", false);

    expect(prompts).toEqual([[{ type: "text", text: "检查构建" }]]);
    // 未打算提交的附件保留在输入区，供后续手动发送。
    expect(core.state.attachments.map((attachment) => attachment.label)).toEqual(["draft.txt"]);
  });

  it("输入框发送仍一并携带附件并在成功后清空", async () => {
    const { client, prompts } = promptingClient();
    const core = new Core();
    core.client = client;
    core.state.open = { kind: "session", id: "s1" };
    withAttachments(core);

    const ok = await sendPrompt(core, "看一下这个文件");

    expect(ok).toBe(true);
    expect(prompts).toEqual([
      [
        { type: "text", text: "看一下这个文件" },
        { type: "text", text: "draft.txt" },
      ],
    ]);
    expect(core.state.attachments).toEqual([]);
  });
});

describe("终端视图", () => {
  it("打开终端面板与打开终端时追加 chunk 都更换数组引用", async () => {
    const core = new Core();
    const before = core.state.detail.terminalChunks;
    toggleSidePanel(core, "terminal");
    const after = core.state.detail.terminalChunks;
    expect(after).not.toBe(before);
    expect(after).toHaveLength(1);

    core.client = {
      openTerminal: async () => "t1",
      terminals: async () => [
        { id: "t1", cwd: "/w", cols: 80, rows: 24, state: "running" },
      ],
    } as unknown as ApiClient;
    core.state.open = { kind: "session", id: "s1" };
    core.state.detail.session = { ...session(), id: "s1" };
    const prior = core.state.detail.terminalChunks;
    await openTerminal(core, 80, 24);
    expect(core.state.detail.terminalChunks).not.toBe(prior);
    expect(core.state.detail.terminalChunks).toHaveLength(2);
    expect(core.state.detail.activeTerminal).toBe("t1");
  });
});
