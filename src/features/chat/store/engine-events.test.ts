import { beforeEach, describe, expect, it } from "vitest";
import type { EngineEventPayload } from "@/lib/events";
import type { ChatStore } from "../store";
import { handleEngineEvents, type EngineEventDeps } from "./engine-events";
import { EMPTY_SESSION, runRouting } from "./stream";

const SESSION_KEY = "codex/session-1";
const RUN_ID = "run-1";

function createDeps(session = EMPTY_SESSION): {
  getState: () => ChatStore;
  deps: EngineEventDeps;
} {
  let state = {
    bySession: { [SESSION_KEY]: session },
    models: {},
    openTabs: [],
  } as unknown as ChatStore;
  const set: EngineEventDeps["set"] = (updater) => {
    state = { ...state, ...updater(state) };
  };
  return {
    getState: () => state,
    deps: {
      set,
      get: () => state,
      drainQueue: () => undefined,
      markUnseenIfBackground: () => undefined,
      upsertSessionMeta: () => undefined,
    },
  };
}

function assistantMessage(runId = RUN_ID): EngineEventPayload {
  return {
    runId,
    sessionId: "session-1",
    engine: "codex",
    seq: 1,
    kind: "message",
    data: { role: "assistant", text: "late output" },
  };
}

describe("engine event cancellation fence", () => {
  beforeEach(() => runRouting.clear());

  it("drops content that arrives after the user stops an active run", () => {
    const { getState, deps } = createDeps({
      ...EMPTY_SESSION,
      streaming: false,
      interrupted: true,
    });
    runRouting.set(RUN_ID, SESSION_KEY);

    handleEngineEvents([assistantMessage()], deps);

    expect(getState().bySession[SESSION_KEY]?.messages).toEqual([]);
  });

  it("does not route late events to a settled session by session id", () => {
    const { getState, deps } = createDeps({ ...EMPTY_SESSION, streaming: false });

    handleEngineEvents([assistantMessage("late-run")], deps);

    expect(getState().bySession[SESSION_KEY]?.messages).toEqual([]);
  });
});
