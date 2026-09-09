import {
  normalizeOmpServiceTier,
  type OmpServiceTier,
} from "@/lib/omp-service-tier";
import { create } from "zustand";
import {
  ipc,
  type AppSettings,
  type Message,
  type SessionMeta,
  type Workspace,
  type WorkspaceGroup,
  type EngineInfo,
} from "@/lib/ipc";
import type { EffortLevel } from "@/components/application/ai-chat/cli-menu";
import type { ComposerPermission } from "@/components/application/ai-chat/permission-menu";
import { listenEngineEvents, listenSessionsChanged } from "@/lib/events";
import { errorText } from "@/lib/errors";
import { writeStored } from "@/lib/storage";
import { newId } from "@/lib/id";
import { subscribeTauriEvent } from "@/hooks/use-tauri-event";
import { CLI_CONFIG_CHANGED_EVENT } from "@/features/settings/providers";
import {
  ENGINE_PREF_KEY,
  PERMISSION_PREF_KEY,
  dedupeTabs,
  persistTabs,
  readPersistedActive,
  readPersistedTabs,
  sameTab,
  sessionKey,
  type ActiveSession,
} from "./store/persistence";
import {
  EMPTY_SESSION,
  applyStreamParts,
  drainPending,
  moveStreamingFlag,
  patchSession,
  runRouting,
  setStreamingFlag,
  settleLiveRows,
  type SessionState,
} from "./store/stream";
import {
  firstLineTitle,
  handleEngineEvents,
  optimisticMeta,
  upsertSessionMetaInto,
} from "./store/engine-events";

// Facade re-exports: callers keep importing everything from "../store".
export { sessionKey } from "./store/persistence";
export type { ActiveSession } from "./store/persistence";
export type { QueuedMessage, SessionState } from "./store/stream";

/** Sidebar workspace groups (工作区二级分类), ordered by sortOrder then name. */
export function sortedWorkspaceGroups(
  groups: WorkspaceGroup[],
): WorkspaceGroup[] {
  return groups.slice().sort((a, b) => {
    const diff =
      (a.sortOrder ?? Number.MAX_SAFE_INTEGER) -
      (b.sortOrder ?? Number.MAX_SAFE_INTEGER);
    return diff !== 0 ? diff : a.name.localeCompare(b.name);
  });
}

/** Unlisteners for the module-scope event subscriptions set up in init. */
const eventTeardowns: Array<() => void> = [];
/** History lists hide sessions of CLIs the user disabled in settings. An
 * empty engines list means listEngines failed — keep sessions rather than
 * blanking the sidebar. */
function visibleSessions(
  sessions: SessionMeta[],
  engines: EngineInfo[],
): SessionMeta[] {
  if (engines.length === 0) return sessions;
  const enabled = new Set<string>();
  for (const e of engines) {
    if (e.enabled) enabled.add(e.id);
  }
  return sessions.filter((s) => enabled.has(s.engine));
}
const PERMISSION_MODES: readonly ComposerPermission[] = [
  "auto",
  "manual",
  "plan",
  "bypass",
];

/** Persisted composer permission, validated against the known modes. */
function readPermissionPref(): ComposerPermission {
  const raw = localStorage.getItem(PERMISSION_PREF_KEY);
  return PERMISSION_MODES.includes(raw as ComposerPermission)
    ? (raw as ComposerPermission)
    : "auto";
}

/** The mode actually sent for an engine: the user's pick when the engine
 * honors it, else the engine's first supported mode (same fallback the
 * Rust side applies). */
export function effectivePermission(
  engines: EngineInfo[],
  engine: string,
  selected: ComposerPermission,
): ComposerPermission {
  const supported = engines.find((e) => e.id === engine)?.permissions;
  if (!supported || supported.length === 0) return selected;
  return supported.includes(selected)
    ? selected
    : (supported[0] as ComposerPermission);
}

export interface ChatStore {
  workspaces: Workspace[];
  sessions: SessionMeta[];
  engines: EngineInfo[];
  active: ActiveSession | null;
  /** Open conversation tabs, in display order. Persisted in localStorage. */
  openTabs: ActiveSession[];
  activeEngine: string;
  /** Composer permission mode ("auto" | "manual" | "plan" | "bypass"),
   * persisted in localStorage; engines resolve unsupported modes to their
   * first supported one at send time (and the picker greys them out). */
  permission: ComposerPermission;
  /** Per-engine reasoning effort ("low" | … | "max"), persisted in app settings. */
  efforts: Record<string, EffortLevel>;
  ompServiceTier: OmpServiceTier;
  /** Per-engine model override ("" = CLI/provider default), persisted in app settings. */
  models: Record<string, string>;
  /** Max sessions listed per workspace in the sidebar, persisted in app settings. */
  threadLimit: number;
  /** Sidebar workspace groups, persisted in app settings. The assignment
   *  lives on each workspace (`Workspace.groupId`), same as the legacy app. */
  workspaceGroups: WorkspaceGroup[];
  /** Workspace id -> sidebar display alias, persisted in app settings;
   *  workspaces missing here show their folder name. */
  workspaceAliases: Record<string, string>;
  /** Ids of workspaces hidden into the sidebar's collapsible 已归档 section,
   *  persisted in app settings; records and sessions stay intact. */
  archivedWorkspaces: string[];
  /** Composer send gesture ("enter" | "cmdEnter"), persisted in app settings. */
  sendShortcut: string;
  bySession: Record<string, SessionState>;
  /** Flat sessionKey -> streaming map, written only when a flag flips. The
   * tab strip and sidebar select this instead of scanning bySession on every
   * store write (streaming deltas would otherwise re-render them per frame). */
  streamingByKey: Record<string, true>;
  /** Sessions with activity the user has not opened yet (sidebar green dot),
   * keyed `${engine}/${sessionId}` like the sidebar thread id. In-memory only. */
  unseen: Record<string, boolean>;
  drafts: Record<string, string>;
  /** File-tree "+" click asking the active composer to insert an @path
   * mention; nonce re-fires for the same path. */
  pendingMention: { path: string; nonce: number } | null;
  /** Last failed store action (add/remove workspace, delete/pin/rename
   * session); surfaced as a dismissable banner in ChatPage. */
  actionError: string | null;
  initialized: boolean;

  init: () => Promise<void>;
  refreshSessions: () => Promise<void>;
  /** Re-read engines + sessions after CLI config changes (enable switch,
   * channel edits): refreshes the picker's enabled set and re-filters the
   * history list, migrating the engine pref off a disabled CLI. */
  refreshEngines: () => Promise<void>;
  refreshWorkspaces: () => Promise<void>;
  addWorkspace: (path: string) => Promise<void>;
  reorderWorkspaces: (ids: string[]) => Promise<void>;
  removeWorkspace: (id: string) => Promise<void>;
  selectSession: (
    engine: string,
    sessionId: string,
    workspacePath: string,
  ) => Promise<void>;
  closeTab: (
    engine: string,
    sessionId: string | null,
    workspacePath: string,
  ) => void;
  /** Activate an already-open tab without changing the tab list. */
  focusTab: (
    engine: string,
    sessionId: string | null,
    workspacePath: string,
  ) => void;
  /** Move an open tab to a new position (drag-reorder in the tab strip). */
  moveTab: (
    engine: string,
    sessionId: string | null,
    workspacePath: string,
    toIndex: number,
  ) => void;
  startNewChat: (workspacePath: string) => void;
  setActiveEngine: (engine: string) => void;
  setPermission: (permission: ComposerPermission) => void;
  setEffort: (engine: string, effort: EffortLevel) => Promise<void>;
  setOmpServiceTier: (tier: OmpServiceTier) => Promise<void>;
  setModel: (engine: string, model: string) => Promise<void>;
  /** Pin several engines' models at once (startup defaulting); one settings
   * write instead of one per engine. */
  pinModels: (updates: Record<string, string>) => Promise<void>;
  setThreadLimit: (limit: number) => void;
  /** Create a named sidebar group; throws on empty/duplicate names. */
  createWorkspaceGroup: (name: string) => Promise<WorkspaceGroup | null>;
  /** Rename a group; throws on empty/duplicate names. */
  renameWorkspaceGroup: (id: string, name: string) => Promise<boolean>;
  /** Persist a new sidebar group order (ids in display order). */
  reorderWorkspaceGroups: (orderedIds: string[]) => Promise<void>;
  /** Delete a group; its workspaces fall back to ungrouped. */
  deleteWorkspaceGroup: (id: string) => Promise<void>;
  /** Put a workspace into a group (null = ungrouped). */
  assignWorkspaceGroup: (
    workspaceId: string,
    groupId: string | null,
  ) => Promise<void>;
  /** Set (or clear, null/empty/name-equal) the sidebar alias of a workspace. */
  setWorkspaceAlias: (
    workspaceId: string,
    alias: string | null,
  ) => Promise<void>;
  /** Move a workspace into / out of the sidebar's archived section. */
  setWorkspaceArchived: (
    workspaceId: string,
    archived: boolean,
  ) => Promise<void>;
  setSendShortcut: (shortcut: string) => void;
  setDraft: (key: string, text: string) => void;
  /** Ask the active composer to insert an @path mention at the caret. */
  requestMention: (path: string) => void;
  clearPendingMention: () => void;
  dismissActionError: () => void;
  /** Clear a session's turn/load error banner. */
  dismissSessionError: (key: string) => void;
  loadEarlier: () => Promise<void>;
  send: (prompt: string, images: string[]) => Promise<void>;
  /** Enqueue a message on the active session while a turn streams. */
  queueMessage: (text: string, images: string[]) => void;
  /** Drop a queued message from the active session. */
  removeQueued: (id: string) => void;
  /** Drop every queued message from the active session. */
  clearQueue: () => void;
  interrupt: () => Promise<void>;
  deleteSession: (engine: string, sessionId: string) => Promise<void>;
  pinSession: (
    engine: string,
    sessionId: string,
    pinned: boolean,
  ) => Promise<void>;
  renameSession: (
    engine: string,
    sessionId: string,
    title: string,
  ) => Promise<void>;
}

/** Persist one app-settings patch; callers have already applied the in-memory
 * value, so a persist failure is non-fatal. */
async function persistSettings(
  patch: (settings: AppSettings) => Partial<AppSettings>,
) {
  try {
    const settings = await ipc.getAppSettings();
    await ipc.updateAppSettings({ ...settings, ...patch(settings) });
  } catch {
    // Persist failure is non-fatal: the in-memory value still applies.
  }
}

function omitKey(rec: Record<string, boolean>, key: string) {
  const next = { ...rec };
  delete next[key];
  return next;
}

/** Append committed timeline rows, assigning seq after the session's last
 * row; `patch` carries any extra per-site session changes. */
function appendCommittedRows(
  set: (fn: (s: ChatStore) => Partial<ChatStore>) => void,
  key: string,
  rows: Omit<Message, "seq">[],
  patch: Partial<SessionState> = {},
) {
  set((s) => {
    const prev = s.bySession[key] ?? EMPTY_SESSION;
    const lastSeq = prev.messages.length
      ? prev.messages[prev.messages.length - 1].seq
      : 0;
    return {
      bySession: {
        ...s.bySession,
        [key]: {
          ...prev,
          ...patch,
          messages: [
            ...prev.messages,
            ...rows.map((row, i) => ({ ...row, seq: lastSeq + 1 + i })),
          ],
        },
      },
    };
  });
}

export const useChatStore = create<ChatStore>((set, get) => {
  /** Activate a tab: existing sessions lazy-load via selectSession, pending chats just set. */
  function activateTab(tab: ActiveSession | null) {
    if (!tab) {
      set({ active: null });
      persistTabs(get().openTabs, null);
      return;
    }
    if (tab.sessionId)
      void get().selectSession(tab.engine, tab.sessionId, tab.workspacePath);
    else {
      // A pending tab sends with its own engine, so the picker must follow it
      // — otherwise the chip shows one CLI while sends go to another.
      const syncEngine = tab.engine !== get().activeEngine;
      if (syncEngine) writeStored(ENGINE_PREF_KEY, tab.engine);
      set(
        syncEngine
          ? { active: tab, activeEngine: tab.engine }
          : { active: tab },
      );
      persistTabs(get().openTabs, tab);
    }
  }

  /** Stamp a per-tab composer override (model/effort) onto the active tab, so
   * the picker follows each session across tab switches. Persists with the
   * tab list; no-op without an active tab. */
  function stampActiveTab(patch: Partial<ActiveSession>) {
    set((s) => {
      const current = s.active;
      if (!current) return {};
      const active: ActiveSession = { ...current, ...patch };
      const openTabs = s.openTabs.map((t) =>
        sameTab(t, current.engine, current.sessionId, current.workspacePath)
          ? active
          : t,
      );
      persistTabs(openTabs, active);
      return { openTabs, active };
    });
  }

  /** Remove a tab; when it was active, fall back to its nearest neighbor. */
  function removeTab(
    engine: string,
    sessionId: string | null,
    workspacePath: string,
  ) {
    const s = get();
    const idx = s.openTabs.findIndex((t) =>
      sameTab(t, engine, sessionId, workspacePath),
    );
    if (idx < 0) return;
    const openTabs = s.openTabs.filter((_, i) => i !== idx);
    set({ openTabs });
    if (s.active && sameTab(s.active, engine, sessionId, workspacePath)) {
      activateTab(openTabs[Math.min(idx, openTabs.length - 1)] ?? null);
    } else {
      persistTabs(openTabs, s.active);
    }
  }

  /**
   * Send a prompt to a specific tab. Unlike the public `send` action this is
   * not bound to the active session, so the queue can drain on a background
   * tab after its turn finishes there.
   */
  async function sendPrompt(
    tab: ActiveSession,
    prompt: string,
    images: string[],
  ) {
    if (!prompt.trim() && images.length === 0) return;
    const engine = tab.engine;
    const key = sessionKey(engine, tab.sessionId, tab.workspacePath);
    // Optimistic user message.
    set((s) => ({
      streamingByKey: setStreamingFlag(s.streamingByKey, key, true),
    }));
    appendCommittedRows(
      set,
      key,
      [
        {
          role: "user",
          text: prompt,
          ts: new Date().toISOString(),
          images: images.length ? [...images] : undefined,
        },
      ],
      {
        streaming: true,
        error: null,
        interrupted: false,
        turnStartedAt: Date.now(),
      },
    );
    try {
      const result = await ipc.sendMessage({
        engine,
        workspacePath: tab.workspacePath,
        sessionId: tab.sessionId,
        prompt,
        imagePaths: images.length ? images : null,
        model: (tab.model ?? get().models[engine]) || null,
        effort: tab.effort ?? get().efforts[engine] ?? null,
        permission: effectivePermission(
          get().engines,
          engine,
          get().permission,
        ),
      });
      if (result.sessionId && !tab.sessionId) {
        // Preassigned native id (grok): adopt immediately.
        set((s) => {
          const newKey = sessionKey(
            engine,
            result.sessionId,
            tab.workspacePath,
          );
          const bySession = { ...s.bySession };
          if (bySession[key]) {
            bySession[newKey] = bySession[key];
            if (newKey !== key) delete bySession[key];
          }
          runRouting.set(result.runId, newKey);
          // Stamp only the tab that owns this run; blanketing every pending
          // tab of this engine+workspace would create duplicate session tabs.
          let stamped = false;
          const openTabs = dedupeTabs(
            s.openTabs.map((t) => {
              if (
                stamped ||
                t.engine !== engine ||
                t.sessionId !== null ||
                t.workspacePath !== tab.workspacePath
              ) {
                return t;
              }
              stamped = true;
              return { ...t, sessionId: result.sessionId };
            }),
          );
          // Only the active tab adopts the native id on `active`; a
          // background drain leaves the user's current tab untouched.
          const active =
            s.active &&
            s.active.engine === engine &&
            s.active.sessionId === null &&
            s.active.workspacePath === tab.workspacePath
              ? { ...s.active, sessionId: result.sessionId }
              : s.active;
          return {
            bySession,
            openTabs,
            active,
            streamingByKey: moveStreamingFlag(s.streamingByKey, key, newKey),
          };
        });
        persistTabs(get().openTabs, get().active);
        // Sidebar row + tab title pick the new session up immediately
        // instead of waiting for the post-turn rescan.
        upsertSessionMetaInto(
          set,
          optimisticMeta(
            engine,
            result.sessionId,
            tab.workspacePath,
            firstLineTitle(prompt),
          ),
        );
      } else {
        runRouting.set(result.runId, key);
      }
    } catch (error) {
      set((s) => ({
        streamingByKey: setStreamingFlag(s.streamingByKey, key, false),
      }));
      patchSession(set, key, {
        error: String(error),
        streaming: false,
        turnStartedAt: null,
      });
    }
  }

  /** Flag a finished background session as unseen so the sidebar shows the
   * green dot until the user opens it; the watched active tab never flags. */
  function markUnseenIfBackground(key: string) {
    const active = get().active;
    const activeKey = active
      ? sessionKey(active.engine, active.sessionId, active.workspacePath)
      : null;
    if (key === activeKey) return;
    set((s) => (s.unseen[key] ? {} : { unseen: { ...s.unseen, [key]: true } }));
  }

  /** After a turn ends, send the oldest queued message for that session. */
  function drainQueue(key: string) {
    const s = get();
    const session = s.bySession[key];
    if (!session || session.streaming || session.queue.length === 0) return;
    const tab = s.openTabs.find(
      (t) => sessionKey(t.engine, t.sessionId, t.workspacePath) === key,
    );
    if (!tab) return;
    const [head, ...rest] = session.queue;
    set((prev) => ({
      bySession: {
        ...prev.bySession,
        [key]: { ...(prev.bySession[key] ?? EMPTY_SESSION), queue: rest },
      },
    }));
    void sendPrompt(tab, head.text, head.images);
  }

  /** Migrate the engine pref off a CLI that is gone or disabled in
   * settings. All CLIs disabled: leave the pref alone — the composer shows
   * the "no CLI enabled" placeholder instead of a misleading fallback. */
  function ensureUsableEngine(engines: EngineInfo[]) {
    const usable = engines.filter((e) => e.enabled);
    if (usable.length === 0 || usable.some((e) => e.id === get().activeEngine))
      return;
    get().setActiveEngine(usable.find((e) => e.available)?.id ?? usable[0].id);
  }

  return {
    workspaces: [],
    sessions: [],
    engines: [],
    active: null,
    openTabs: [],
    activeEngine: localStorage.getItem(ENGINE_PREF_KEY) ?? "claude",
    permission: readPermissionPref(),
    efforts: {},
    ompServiceTier: null,
    models: {},
    threadLimit: 10,
    workspaceGroups: [],
    workspaceAliases: {},
    archivedWorkspaces: [],
    sendShortcut: "enter",
    bySession: {},
    streamingByKey: {},
    unseen: {},
    drafts: {},
    pendingMention: null,
    actionError: null,
    initialized: false,

    init: async () => {
      if (get().initialized) return;
      set({ initialized: true });
      eventTeardowns.push(
        subscribeTauriEvent(() =>
          listenEngineEvents((events) =>
            handleEngineEvents(events, {
              set,
              get,
              drainQueue,
              markUnseenIfBackground,
              upsertSessionMeta: (meta) => upsertSessionMetaInto(set, meta),
            }),
          ),
        ),
        subscribeTauriEvent(() =>
          listenSessionsChanged(() => void get().refreshSessions()),
        ),
      );
      // Settings' CLI enable switch / channel edits: re-filter history and
      // picker options without a restart.
      const onCliConfigChanged = () => void get().refreshEngines();
      window.addEventListener(CLI_CONFIG_CHANGED_EVENT, onCliConfigChanged);
      eventTeardowns.push(() =>
        window.removeEventListener(
          CLI_CONFIG_CHANGED_EVENT,
          onCliConfigChanged,
        ),
      );
      const [workspaces, sessions, engines] = await Promise.all([
        ipc.listWorkspaces().catch(() => [] as Workspace[]),
        ipc.listSessions().catch(() => [] as SessionMeta[]),
        ipc.listEngines().catch(() => [] as EngineInfo[]),
      ]);
      set({
        workspaces,
        sessions: visibleSessions(sessions, engines),
        engines,
      });
      ensureUsableEngine(engines);
      // Restore persisted tabs; drop ones whose workspace/session is gone.
      const restoredTabs = readPersistedTabs().filter(
        (t) =>
          workspaces.some((w) => w.path === t.workspacePath) &&
          (t.sessionId === null ||
            sessions.some(
              (s) => s.engine === t.engine && s.sessionId === t.sessionId,
            )),
      );
      set({ openTabs: restoredTabs });
      const persistedActive = readPersistedActive();
      const activeTab =
        persistedActive &&
        restoredTabs.some((t) =>
          sameTab(
            t,
            persistedActive.engine,
            persistedActive.sessionId,
            persistedActive.workspacePath,
          ),
        )
          ? persistedActive
          : (restoredTabs[0] ?? null);
      if (activeTab) activateTab(activeTab);
      ipc
        .getAppSettings()
        .then((settings) =>
          set({
            efforts: (settings.defaultEfforts ?? {}) as Record<
              string,
              EffortLevel
            >,
            ompServiceTier: normalizeOmpServiceTier(
              settings.ompOpenaiServiceTier,
            ),
            models: settings.defaultModels ?? {},
            threadLimit: settings.sidebarThreadLimit ?? 5,
            workspaceGroups: settings.workspaceGroups ?? [],
            workspaceAliases: settings.workspaceAliases ?? {},
            archivedWorkspaces: settings.archivedWorkspaces ?? [],
            sendShortcut: settings.composerSendShortcut ?? "enter",
          }),
        )
        .catch(() => {});
    },

    refreshSessions: async () => {
      const sessions = await ipc.listSessions().catch(() => null);
      if (sessions) set({ sessions: visibleSessions(sessions, get().engines) });
    },

    refreshEngines: async () => {
      const [engines, sessions] = await Promise.all([
        ipc.listEngines().catch(() => null),
        ipc.listSessions().catch(() => null),
      ]);
      if (!engines) return;
      set({
        engines,
        ...(sessions ? { sessions: visibleSessions(sessions, engines) } : {}),
      });
      ensureUsableEngine(engines);
    },

    refreshWorkspaces: async () => {
      const workspaces = await ipc.listWorkspaces().catch(() => null);
      if (workspaces) set({ workspaces });
    },

    addWorkspace: async (path) => {
      try {
        await ipc.addWorkspace(path);
        await get().refreshWorkspaces();
        set({ actionError: null });
      } catch (error) {
        set({ actionError: errorText(error) });
      }
    },

    reorderWorkspaces: async (ids) => {
      // Optimistic: listed ids first in the given order, the rest keep their
      // relative order after them.
      const order = new Map(ids.map((id, index) => [id, index]));
      set((s) => ({
        workspaces: [...s.workspaces].sort(
          (a, b) =>
            (order.get(a.id) ?? Number.MAX_SAFE_INTEGER) -
            (order.get(b.id) ?? Number.MAX_SAFE_INTEGER),
        ),
      }));
      try {
        await ipc.reorderWorkspaces(ids);
      } catch {
        await get().refreshWorkspaces();
      }
    },

    removeWorkspace: async (id) => {
      try {
        const removedPath = get().workspaces.find((w) => w.id === id)?.path;
        await ipc.removeWorkspace(id);
        await get().refreshWorkspaces();
        set({ actionError: null });
        if (!removedPath) return;
        const openTabs = get().openTabs.filter(
          (t) => t.workspacePath !== removedPath,
        );
        set({ openTabs });
        const active = get().active;
        if (active && active.workspacePath === removedPath) {
          activateTab(openTabs[0] ?? null);
        } else {
          persistTabs(openTabs, active);
        }
      } catch (error) {
        set({ actionError: errorText(error) });
      }
    },

    selectSession: async (engine, sessionId, workspacePath) => {
      const key = sessionKey(engine, sessionId, workspacePath);
      const syncEngine = engine !== get().activeEngine;
      if (syncEngine) writeStored(ENGINE_PREF_KEY, engine);
      set((s) => {
        // Reuse the stored tab so its per-tab model/effort overrides survive;
        // a fresh object would drop them on every session switch.
        const stored = s.openTabs.find((t) =>
          sameTab(t, engine, sessionId, workspacePath),
        );
        const tab: ActiveSession = stored ?? {
          engine,
          sessionId,
          workspacePath,
        };
        const openTabs = stored ? s.openTabs : [...s.openTabs, tab];
        persistTabs(openTabs, tab);
        // Opening a session clears its unseen flag.
        const unseen = key in s.unseen ? omitKey(s.unseen, key) : s.unseen;
        // The picker must follow the session's engine — otherwise the chip
        // shows one CLI while sends go to another (same rule as pending tabs).
        return { openTabs, active: tab, unseen, activeEngine: engine };
      });
      const existing = get().bySession[key];
      if (existing && existing.messages.length > 0) return;
      patchSession(set, key, { loading: true });
      try {
        const page = await ipc.loadSessionPage(engine, sessionId, 100);
        patchSession(set, key, {
          messages: page.messages,
          nextBefore: page.nextBefore,
          loading: false,
          // Live "usage" events only cover fresh turns; a resumed session
          // adopts the newest usage snapshot carried by its history.
          usage:
            [...page.messages].reverse().find((m) => m.usage)?.usage ?? null,
        });
      } catch (error) {
        patchSession(set, key, { loading: false, error: String(error) });
      }
    },

    startNewChat: (workspacePath) => {
      // One pending chat per workspace+engine: re-focus it instead of
      // piling up empty "new" tabs.
      set((s) => {
        const existing = s.openTabs.find(
          (t) =>
            t.sessionId === null &&
            t.engine === s.activeEngine &&
            t.workspacePath === workspacePath,
        );
        const tab: ActiveSession = existing ?? {
          engine: s.activeEngine,
          sessionId: null,
          workspacePath,
        };
        const openTabs = existing ? s.openTabs : [...s.openTabs, tab];
        persistTabs(openTabs, tab);
        return { openTabs, active: tab };
      });
    },

    closeTab: (engine, sessionId, workspacePath) => {
      removeTab(engine, sessionId, workspacePath);
    },
    focusTab: (engine, sessionId, workspacePath) => {
      // Reuse the stored tab so its per-tab model/effort overrides apply.
      const stored = get().openTabs.find((t) =>
        sameTab(t, engine, sessionId, workspacePath),
      );
      activateTab(stored ?? { engine, sessionId, workspacePath });
    },
    moveTab: (engine, sessionId, workspacePath, toIndex) => {
      const s = get();
      const from = s.openTabs.findIndex((t) =>
        sameTab(t, engine, sessionId, workspacePath),
      );
      if (from < 0) return;
      const openTabs = [...s.openTabs];
      const [tab] = openTabs.splice(from, 1);
      openTabs.splice(Math.max(0, Math.min(toIndex, openTabs.length)), 0, tab);
      set({ openTabs });
      persistTabs(openTabs, s.active);
    },

    setActiveEngine: (engine) => {
      writeStored(ENGINE_PREF_KEY, engine);
      set((s) => {
        const active = s.active;
        // A pending (never-sent) tab has no backend session yet, so it
        // follows the picker: retarget it to the newly selected engine.
        // Otherwise the chip shows the new CLI while sends still go to the
        // engine the tab was created with.
        if (!active || active.sessionId !== null || active.engine === engine) {
          return { activeEngine: engine };
        }
        const oldKey = sessionKey(active.engine, null, active.workspacePath);
        // First turn still in flight (native id not yet assigned): event
        // routing is keyed to the old engine, so leave the tab untouched.
        if (s.bySession[oldKey]?.streaming) return { activeEngine: engine };
        const newKey = sessionKey(engine, null, active.workspacePath);
        const existing = s.openTabs.find(
          (t) =>
            !sameTab(
              t,
              active.engine,
              active.sessionId,
              active.workspacePath,
            ) &&
            t.sessionId === null &&
            t.engine === engine &&
            t.workspacePath === active.workspacePath,
        );
        // Carry unsent drafts / session state over to the new key.
        const bySession = { ...s.bySession };
        const drafts = { ...s.drafts };
        if (bySession[oldKey] && !bySession[newKey])
          bySession[newKey] = bySession[oldKey];
        delete bySession[oldKey];
        if (drafts[oldKey] !== undefined && drafts[newKey] === undefined) {
          drafts[newKey] = drafts[oldKey];
        }
        delete drafts[oldKey];
        let openTabs: ActiveSession[];
        let nextActive: ActiveSession;
        if (existing) {
          // A pending tab for this engine+workspace already exists: fold
          // into it instead of stacking a duplicate.
          openTabs = s.openTabs.filter(
            (t) =>
              !sameTab(
                t,
                active.engine,
                active.sessionId,
                active.workspacePath,
              ),
          );
          nextActive = existing;
        } else {
          // Engine retarget: the old engine's model/effort overrides don't
          // apply to the new one — drop them so defaults resolve fresh.
          nextActive = {
            ...active,
            engine,
            model: undefined,
            effort: undefined,
          };
          openTabs = s.openTabs.map((t) =>
            sameTab(t, active.engine, active.sessionId, active.workspacePath)
              ? nextActive
              : t,
          );
        }
        persistTabs(openTabs, nextActive);
        return {
          activeEngine: engine,
          openTabs,
          active: nextActive,
          bySession,
          drafts,
          streamingByKey: moveStreamingFlag(s.streamingByKey, oldKey, newKey),
        };
      });
    },

    setPermission: (permission) => {
      writeStored(PERMISSION_PREF_KEY, permission);
      set({ permission });
    },
    setOmpServiceTier: async (tier) => {
      const settings = await ipc.getAppSettings();
      await ipc.updateAppSettings({ ...settings, ompOpenaiServiceTier: tier });
      set({ ompServiceTier: tier });
    },
    setEffort: async (engine, effort) => {
      set({ efforts: { ...get().efforts, [engine]: effort } });
      if (get().active?.engine === engine) stampActiveTab({ effort });
      await persistSettings((settings) => ({
        defaultEfforts: { ...settings.defaultEfforts, [engine]: effort },
      }));
    },
    setModel: async (engine, model) => {
      const models = { ...get().models };
      if (model) models[engine] = model;
      else delete models[engine];
      set({ models });
      if (get().active?.engine === engine) {
        // Empty = "CLI default": clear the tab override too, so the tab
        // follows the (also cleared) global default again.
        stampActiveTab({ model: model || undefined });
      }
      await persistSettings((settings) => {
        const defaultModels = { ...settings.defaultModels };
        if (model) defaultModels[engine] = model;
        else delete defaultModels[engine];
        return { defaultModels };
      });
    },
    pinModels: async (updates) => {
      const entries = Object.entries(updates).filter(([, model]) =>
        model.trim(),
      );
      if (entries.length === 0) return;
      const models = { ...get().models };
      for (const [engine, model] of entries) models[engine] = model;
      set({ models });
      await persistSettings((settings) => ({
        defaultModels: {
          ...settings.defaultModels,
          ...Object.fromEntries(entries),
        },
      }));
    },

    setThreadLimit: (limit) => {
      set({ threadLimit: Math.max(1, Math.floor(limit)) });
    },
    createWorkspaceGroup: async (name) => {
      const trimmed = name.trim();
      if (!trimmed) throw new Error("Group name is required.");
      const current = get().workspaceGroups;
      if (current.some((g) => g.name === trimmed)) {
        throw new Error("Group name already exists.");
      }
      const group: WorkspaceGroup = {
        id: newId(),
        name: trimmed,
        sortOrder:
          current.reduce((max, g) => Math.max(max, g.sortOrder ?? -1), -1) + 1,
      };
      const workspaceGroups = [...current, group];
      set({ workspaceGroups });
      await persistSettings((settings) => ({
        workspaceGroups: [...(settings.workspaceGroups ?? []), group],
      }));
      return group;
    },
    renameWorkspaceGroup: async (id, name) => {
      const trimmed = name.trim();
      if (!trimmed) throw new Error("Group name is required.");
      const current = get().workspaceGroups;
      if (current.some((g) => g.id !== id && g.name === trimmed)) {
        throw new Error("Group name already exists.");
      }
      set({
        workspaceGroups: current.map((g) =>
          g.id === id ? { ...g, name: trimmed } : g,
        ),
      });
      await persistSettings((settings) => ({
        workspaceGroups: (settings.workspaceGroups ?? []).map((g) =>
          g.id === id ? { ...g, name: trimmed } : g,
        ),
      }));
      return true;
    },
    reorderWorkspaceGroups: async (orderedIds) => {
      const current = get().workspaceGroups;
      const byId = new Map(current.map((g) => [g.id, g]));
      const ordered = orderedIds
        .map((id) => byId.get(id))
        .filter((g): g is WorkspaceGroup => Boolean(g));
      // Groups missing from the submitted order keep trailing positions.
      const orderedIdSet = new Set(orderedIds);
      const rest = current.filter((g) => !orderedIdSet.has(g.id));
      const workspaceGroups = [...ordered, ...rest].map((g, i) => ({
        ...g,
        sortOrder: i,
      }));
      set({ workspaceGroups });
      await persistSettings(() => ({ workspaceGroups }));
    },
    deleteWorkspaceGroup: async (id) => {
      const affected = get().workspaces.filter((w) => w.groupId === id);
      const workspaceGroups = get().workspaceGroups.filter((g) => g.id !== id);
      set({
        workspaceGroups,
        workspaces: get().workspaces.map((w) =>
          w.groupId === id ? { ...w, groupId: null } : w,
        ),
      });
      await persistSettings((settings) => ({
        workspaceGroups: (settings.workspaceGroups ?? []).filter(
          (g) => g.id !== id,
        ),
      }));
      // Members of the deleted group fall back to ungrouped.
      await Promise.all(
        affected.map((w) => ipc.setWorkspaceGroup(w.id, null).catch(() => {})),
      );
    },
    assignWorkspaceGroup: async (workspaceId, groupId) => {
      const valid =
        groupId && get().workspaceGroups.some((g) => g.id === groupId);
      const resolved = valid ? groupId : null;
      set({
        workspaces: get().workspaces.map((w) =>
          w.id === workspaceId ? { ...w, groupId: resolved } : w,
        ),
      });
      try {
        await ipc.setWorkspaceGroup(workspaceId, resolved);
      } catch (error) {
        // Roll back to the persisted truth.
        await get().refreshWorkspaces();
        throw error;
      }
    },
    setWorkspaceAlias: async (workspaceId, alias) => {
      // An alias equal to the folder name is no alias at all — same rule the
      // sidebar display applies — so it clears the entry instead of storing.
      const name = get().workspaces.find((w) => w.id === workspaceId)?.name;
      const trimmed = alias?.trim() ?? "";
      const resolved = trimmed && trimmed !== name ? trimmed : null;
      const workspaceAliases = { ...get().workspaceAliases };
      if (resolved) workspaceAliases[workspaceId] = resolved;
      else delete workspaceAliases[workspaceId];
      set({ workspaceAliases });
      await persistSettings((settings) => {
        const next = { ...(settings.workspaceAliases ?? {}) };
        if (resolved) next[workspaceId] = resolved;
        else delete next[workspaceId];
        return { workspaceAliases: next };
      });
    },
    setWorkspaceArchived: async (workspaceId, archived) => {
      const current = get().archivedWorkspaces;
      const archivedWorkspaces = archived
        ? current.includes(workspaceId)
          ? current
          : [...current, workspaceId]
        : current.filter((id) => id !== workspaceId);
      if (archivedWorkspaces === current) return;
      set({ archivedWorkspaces });
      await persistSettings((settings) => {
        const next = (settings.archivedWorkspaces ?? []).filter(
          (id) => id !== workspaceId,
        );
        if (archived) next.push(workspaceId);
        return { archivedWorkspaces: next };
      });
    },
    setSendShortcut: (shortcut) => {
      set({ sendShortcut: shortcut });
    },

    setDraft: (key, text) => {
      set((s) => ({ drafts: { ...s.drafts, [key]: text } }));
    },
    requestMention: (path) => {
      set((s) => ({
        pendingMention: { path, nonce: (s.pendingMention?.nonce ?? 0) + 1 },
      }));
    },

    clearPendingMention: () => {
      set({ pendingMention: null });
    },

    dismissActionError: () => {
      set({ actionError: null });
    },
    dismissSessionError: (key) => {
      patchSession(set, key, { error: null });
    },

    loadEarlier: async () => {
      const { active, bySession } = get();
      if (!active?.sessionId) return;
      const key = sessionKey(
        active.engine,
        active.sessionId,
        active.workspacePath,
      );
      const state = bySession[key];
      if (!state?.nextBefore || state.loading) return;
      patchSession(set, key, { loading: true });
      try {
        const page = await ipc.loadSessionPage(
          active.engine,
          active.sessionId,
          100,
          state.nextBefore,
        );
        patchSession(set, key, {
          messages: [
            ...page.messages,
            ...(get().bySession[key] ?? EMPTY_SESSION).messages,
          ],
          nextBefore: page.nextBefore,
          loading: false,
        });
      } catch {
        patchSession(set, key, { loading: false });
      }
    },

    send: async (prompt, images) => {
      const { active } = get();
      if (active) await sendPrompt(active, prompt, images);
    },

    queueMessage: (text, images) => {
      const { active } = get();
      if (!active || (!text.trim() && images.length === 0)) return;
      const key = sessionKey(
        active.engine,
        active.sessionId,
        active.workspacePath,
      );
      set((s) => {
        const prev = s.bySession[key] ?? EMPTY_SESSION;
        return {
          bySession: {
            ...s.bySession,
            [key]: {
              ...prev,
              queue: [
                ...prev.queue,
                { id: newId(), text, images, queuedAt: Date.now() },
              ],
            },
          },
        };
      });
    },

    removeQueued: (id) => {
      const { active } = get();
      if (!active) return;
      const key = sessionKey(
        active.engine,
        active.sessionId,
        active.workspacePath,
      );
      set((s) => {
        const prev = s.bySession[key];
        if (!prev) return {};
        return {
          bySession: {
            ...s.bySession,
            [key]: {
              ...prev,
              queue: prev.queue.filter((item) => item.id !== id),
            },
          },
        };
      });
    },
    clearQueue: () => {
      const { active } = get();
      if (!active) return;
      const key = sessionKey(
        active.engine,
        active.sessionId,
        active.workspacePath,
      );
      set((s) => {
        const prev = s.bySession[key];
        if (!prev || prev.queue.length === 0) return {};
        return {
          bySession: {
            ...s.bySession,
            [key]: { ...prev, queue: [] },
          },
        };
      });
    },

    interrupt: async () => {
      const { active } = get();
      if (!active) return;
      const key = sessionKey(
        active.engine,
        active.sessionId,
        active.workspacePath,
      );
      // Settle locally FIRST: the killed run's done event can arrive while
      // the kill IPCs below are still in flight, and onDone drains the queue
      // whenever interrupted is still false — that would fire the next
      // queued message right after the user pressed stop. Keep `streaming`
      // true until the backend confirms termination so the composer cannot
      // start a competing turn in this short window.
      const pending = drainPending(key);
      set((s) => {
        const cur = s.bySession[key] ?? EMPTY_SESSION;
        const messages = settleLiveRows(
          pending
            ? applyStreamParts(cur.messages, pending.parts, pending.model)
            : cur.messages,
        );
        return {
          bySession: {
            ...s.bySession,
            [key]: {
              ...cur,
              messages,
              streaming: true,
              interrupted: true,
            },
          },
          streamingByKey: setStreamingFlag(s.streamingByKey, key, true),
        };
      });
      // Registry is keyed by native session id once known; before that the
      // run id routes. Try both, but only report Stop as completed after the
      // backend confirms at least one process-tree termination.
      const targets = new Set<string>();
      if (active.sessionId) targets.add(active.sessionId);
      const deadRunIds: string[] = [];
      for (const [runId, routed] of runRouting) {
        if (routed === key) {
          deadRunIds.push(runId);
          targets.add(runId);
        }
      }
      const results = await Promise.all(
        [...targets].map((target) =>
          ipc.interruptSession(target).catch(() => false),
        ),
      );
      if (!results.some(Boolean)) {
        // The child can finish naturally between the click and the IPC
        // handler. Its terminal event already settled the session, so do not
        // resurrect a running state merely because the registry is empty.
        if (!get().bySession[key]?.streaming) return;
        patchSession(set, key, {
          error: "Unable to confirm that the CLI process stopped. Please try Stop again.",
          interrupted: false,
          streaming: true,
        });
        return;
      }
      // The OS accepted termination: hide the running state and drop routes
      // immediately. A late done event is harmless, while content events are
      // fenced in engine-events.ts.
      patchSession(set, key, {
        interrupted: true,
        streaming: false,
        turnStartedAt: null,
      });
      set((s) => ({
        streamingByKey: setStreamingFlag(s.streamingByKey, key, false),
      }));
      for (const runId of deadRunIds) runRouting.delete(runId);
    },

    deleteSession: async (engine, sessionId) => {
      try {
        await ipc.deleteSession(engine, sessionId);
      } catch (error) {
        set({ actionError: errorText(error) });
        return;
      }
      set({ actionError: null });
      const tab = get().openTabs.find(
        (t) => t.engine === engine && t.sessionId === sessionId,
      );
      set((s) => ({
        sessions: s.sessions.filter(
          (x) => !(x.engine === engine && x.sessionId === sessionId),
        ),
      }));
      if (tab) {
        // removeTab activates the neighboring tab when the deleted one was active.
        removeTab(engine, sessionId, tab.workspacePath);
      } else {
        set((s) => ({
          active:
            s.active?.sessionId === sessionId && s.active.engine === engine
              ? null
              : s.active,
        }));
      }
    },

    pinSession: async (engine, sessionId, pinned) => {
      try {
        await ipc.pinSession(engine, sessionId, pinned);
        await get().refreshSessions();
        set({ actionError: null });
      } catch (error) {
        set({ actionError: errorText(error) });
      }
    },

    renameSession: async (engine, sessionId, title) => {
      try {
        await ipc.renameSession(engine, sessionId, title);
        await get().refreshSessions();
        set({ actionError: null });
      } catch (error) {
        set({ actionError: errorText(error) });
      }
    },
  };
});

// Dev-only handle for poking the store from the webview console; stripped
// from production builds by the env guard.
if (import.meta.env.DEV) {
  (window as unknown as { __chatStore: typeof useChatStore }).__chatStore =
    useChatStore;
}

// HMR swaps this module for a fresh store; without dispose the old module's
// engine/session listeners keep firing into the dead store (and init on the
// new store would double-subscribe).
if (import.meta.hot) {
  import.meta.hot.dispose(() => {
    for (const teardown of eventTeardowns.splice(0)) teardown();
  });
}
