/**
 * Login launch for CodeBuddy: the CLI owns the interactive OAuth/login flow,
 * so the app drives its built-in terminal: open the dock on a fresh tab,
 * wait for the PTY, then run codebuddy and trigger /login.
 */
import { ipc } from "@/lib/ipc";
import { useChatStore } from "@/features/chat/store";
import { ensureTerminalOutputListener } from "@/features/terminal/sessions";
import { useTerminalStore } from "@/features/terminal/store";
import {
  ENTER,
  settleTerminalBacklog,
  shellQuote,
  waitForCliReady,
  waitForSession,
} from "./terminalLogin";

export type CodeBuddyLoginResult =
  | { ok: true }
  /** No chat tab and no known workspace to host the terminal. */
  | { ok: false; reason: "no-workspace" }
  /** The terminal tab opened but its PTY never became usable. */
  | { ok: false; reason: "terminal-unavailable" };

/**
 * Open the terminal dock on a fresh tab and start CodeBuddy login flow.
 * Falls back to the first known workspace when no chat tab is active (fresh
 * app launch). A failed spawn closes the tab it created so no orphan is left.
 */
export async function launchCodeBuddyLogin(): Promise<CodeBuddyLoginResult> {
  const state = useChatStore.getState();
  const workspacePath = state.active?.workspacePath ?? state.workspaces[0]?.path;
  if (!workspacePath) return { ok: false, reason: "no-workspace" };

  const term = useTerminalStore.getState();
  if (!term.open) term.toggle(workspacePath);
  useTerminalStore.getState().newTab(workspacePath);
  const id = useTerminalStore.getState().activeByWorkspace[workspacePath];
  if (!id) return { ok: false, reason: "terminal-unavailable" };

  const settings = await ipc.getAppSettings().catch(() => null);
  const customBin = settings?.codebuddyBin?.trim();
  const bin = shellQuote(customBin || "codebuddy");

  if (!(await waitForSession(id))) {
    // The dock is open on a tab whose PTY never came up; close it instead of
    // leaving an orphan behind a "no workspace" message.
    useTerminalStore.getState().closeTab(workspacePath, id);
    return { ok: false, reason: "terminal-unavailable" };
  }

  ensureTerminalOutputListener(); // idempotent; terminalBacklog depends on it
  // Baseline after the prompt settles: the CLI's TUI must produce output past
  // this point, otherwise /login lands while the shell still owns the line.
  const baseline = await settleTerminalBacklog(id);
  void ipc.terminalWrite(id, `${bin}${ENTER}`).catch(() => {});
  // Fire-and-forget: resolve /login delivery in the background so the caller
  // can navigate away immediately.
  void (async () => {
    await waitForCliReady(id, baseline, bin.length + ENTER.length);
    void ipc.terminalWrite(id, `/login${ENTER}`).catch(() => {});
  })();
  return { ok: true };
}
