import { beforeEach, describe, expect, it, vi } from "vitest";
import { ipc } from "@/lib/ipc";
import { useChatStore } from "./store";
import { OPEN_TABS_KEY } from "./store/persistence";

vi.mock("@/lib/ipc", () => ({
  ipc: {
    sendMessage: vi.fn(async () => ({ runId: "run-1", sessionId: null })),
    interruptSession: vi.fn(async () => false),
    loadSessionPage: vi.fn(async () => ({ messages: [], nextBefore: null })),
    getAppSettings: vi.fn(async () => ({})),
    updateAppSettings: vi.fn(async () => {}),
  },
}));
vi.mock("@/lib/events", () => ({
  listenEngineEvents: vi.fn(async () => () => {}),
  listenSessionsChanged: vi.fn(async () => () => {}),
}));

const WS = "/tmp/ws";

function resetStore() {
  localStorage.clear();
  vi.mocked(ipc.sendMessage).mockClear();
  vi.mocked(ipc.interruptSession).mockReset();
  vi.mocked(ipc.interruptSession).mockResolvedValue(false);
  useChatStore.setState({
    openTabs: [],
    active: null,
    activeEngine: "claude",
    models: { omp: "kimi-k3" },
    efforts: {},
    bySession: {},
    streamingByKey: {},
    unseen: {},
    drafts: {},
  });
}

describe("per-session composer selection", () => {
  beforeEach(resetStore);

  it("switching to an existing session retargets the picker to its engine", async () => {
    useChatStore.setState({ activeEngine: "omp" });
    await useChatStore.getState().selectSession("claude", "s-1", WS);
    expect(useChatStore.getState().activeEngine).toBe("claude");
  });

  it("setModel/setEffort stamp the active tab and persist with it", async () => {
    useChatStore.setState({ activeEngine: "omp" });
    useChatStore.getState().startNewChat(WS); // pending omp tab
    await useChatStore.getState().setModel("omp", "fufei/kimi-k3");
    await useChatStore.getState().setEffort("omp", "max");

    const s = useChatStore.getState();
    expect(s.active?.model).toBe("fufei/kimi-k3");
    expect(s.active?.effort).toBe("max");
    // Overrides persist with the tab list (survive restart).
    const persisted = JSON.parse(localStorage.getItem(OPEN_TABS_KEY) ?? "[]");
    expect(persisted[0]).toMatchObject({
      model: "fufei/kimi-k3",
      effort: "max",
    });
  });

  it("re-selecting a session keeps its stamped overrides", async () => {
    useChatStore.setState({ activeEngine: "omp" });
    useChatStore.getState().startNewChat(WS);
    await useChatStore.getState().setModel("omp", "fufei/kimi-k3");
    // Simulate the tab gaining a native session id after the first turn.
    const stamped = { ...useChatStore.getState().active!, sessionId: "s-9" };
    useChatStore.setState({ openTabs: [stamped], active: null });

    await useChatStore.getState().selectSession("omp", "s-9", WS);
    expect(useChatStore.getState().active).toMatchObject({
      model: "fufei/kimi-k3",
    });
  });

  it("send uses the tab override over the engine default", async () => {
    useChatStore.setState({ activeEngine: "omp", models: { omp: "kimi-k3" } });
    useChatStore.getState().startNewChat(WS);
    await useChatStore.getState().setModel("omp", "fufei/kimi-k3");
    await useChatStore.getState().setEffort("omp", "low");

    await useChatStore.getState().send("hi", []);
    expect(vi.mocked(ipc.sendMessage)).toHaveBeenCalledWith(
      expect.objectContaining({
        engine: "omp",
        model: "fufei/kimi-k3",
        effort: "low",
      }),
    );
  });

  it("a tab without overrides sends with the engine default", async () => {
    useChatStore.setState({ activeEngine: "omp", models: { omp: "kimi-k3" } });
    useChatStore.getState().startNewChat(WS);

    await useChatStore.getState().send("hi", []);
    expect(vi.mocked(ipc.sendMessage)).toHaveBeenCalledWith(
      expect.objectContaining({ model: "kimi-k3", effort: null }),
    );
  });

  it("retargeting a pending tab to another engine drops the old overrides", async () => {
    useChatStore.setState({ activeEngine: "omp" });
    useChatStore.getState().startNewChat(WS);
    await useChatStore.getState().setModel("omp", "fufei/kimi-k3");
    await useChatStore.getState().setEffort("omp", "max");

    useChatStore.getState().setActiveEngine("claude");
    const s = useChatStore.getState();
    expect(s.active?.engine).toBe("claude");
    expect(s.active?.model).toBeUndefined();
    expect(s.active?.effort).toBeUndefined();
  });

  it("keeps the turn actionable when the backend cannot confirm Stop", async () => {
    const active = { engine: "codex", sessionId: "session-1", workspacePath: WS };
    const key = "codex/session-1";
    useChatStore.setState({
      active,
      openTabs: [active],
      bySession: {
        [key]: {
          messages: [], nextBefore: null, loading: false, streaming: true,
          turnStartedAt: Date.now(), usage: null, error: null, queue: [], interrupted: false,
        },
      } as never,
      streamingByKey: { [key]: true },
    });

    await useChatStore.getState().interrupt();

    expect(vi.mocked(ipc.interruptSession)).toHaveBeenCalledWith("session-1");
    expect(useChatStore.getState().bySession[key]).toMatchObject({
      streaming: true,
      interrupted: false,
      error: "Unable to confirm that the CLI process stopped. Please try Stop again.",
    });
  });
});
