/**
 * Shared helpers for "run the CLI's interactive login in the built-in
 * terminal": open a dedicated tab, wait for the PTY, wait until the CLI has
 * taken over the screen, then type the login command. Engine-specific
 * launchers (pi/omp, codebuddy) supply the command sequence.
 */
import { hasTerminalSession, terminalBacklog } from "@/features/terminal/sessions";

/** PTY Enter is CR, not LF. */
export const ENTER = "\r";

/** Characters that need no shell quoting on either platform. */
const SAFE_UNQUOTED = /^[A-Za-z0-9_@%+=:,./-]+$/;

/**
 * Quote a user-supplied binary path for the terminal's shell. The backend only
 * validates "absolute path", so `$`, backticks, quotes and `;` reach here; the
 * old "wrap only when it contains a space" left them live. Plain command names
 * and safe paths stay unquoted so the typed line reads as before.
 */
export function shellQuote(value: string): string {
  if (value && SAFE_UNQUOTED.test(value)) return value;
  if (typeof navigator !== "undefined" && /Windows/i.test(navigator.userAgent)) {
    // PowerShell / cmd: double quotes; a literal quote is doubled.
    return `"${value.replace(/"/g, '""')}"`;
  }
  // POSIX sh: single quotes make everything literal except `'` itself.
  return `'${value.replace(/'/g, `'\\''`)}'`;
}

function delay(ms: number): Promise<void> {
  const { promise, resolve } = Promise.withResolvers<void>();
  setTimeout(resolve, ms);
  return promise;
}

/** Poll until TerminalView has spawned the backend PTY for `id`. */
export async function waitForSession(id: string, timeoutMs = 5000): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (hasTerminalSession(id)) return true;
    await delay(100);
  }
  return false;
}

/**
 * Wait until the terminal output has settled, then return the backlog length.
 *
 * Taking the baseline as soon as the PTY exists raced the shell prompt:
 * `terminal_open` resolves when the shell is spawned, so the baseline was
 * usually 0, and a starship/zsh prompt (100–300 bytes of escapes) already
 * exceeded the readiness threshold below — `/login` was typed before the CLI
 * had started. Waiting for a quiet period puts the prompt inside the baseline.
 */
export async function settleTerminalBacklog(id: string, timeoutMs = 2000): Promise<number> {
  const deadline = Date.now() + timeoutMs;
  let last = terminalBacklog(id).length;
  let stableSince = Date.now();
  while (Date.now() < deadline) {
    await delay(100);
    const next = terminalBacklog(id).length;
    if (next !== last) {
      last = next;
      stableSince = Date.now();
      continue;
    }
    if (Date.now() - stableSince >= 250) return last;
  }
  return terminalBacklog(id).length;
}

/**
 * Wait until the CLI has actually started before typing into it: output must
 * grow well past the echoed launch command (the CLI clears the screen and
 * draws its TUI). On timeout the caller still sends — no worse than a fixed
 * delay.
 */
export async function waitForCliReady(
  id: string,
  baseline: number,
  echoLength: number,
  timeoutMs = 15_000,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  // Echo of the launch line plus shell control bytes; real CLI output (TUI
  // redraw) is far larger than this slack.
  const threshold = baseline + echoLength + 80;
  while (Date.now() < deadline) {
    if (terminalBacklog(id).length > threshold) return;
    await delay(200);
  }
}
