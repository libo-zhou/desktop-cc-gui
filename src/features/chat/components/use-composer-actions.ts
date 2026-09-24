import { useCallback, useEffect } from "react";
import { useTranslation } from "react-i18next";
import { useShallow } from "zustand/react/shallow";
import type { ComposerInputHandle } from "@/components/application/ai-chat/ai-chat-composer";
import { mentionToken } from "@/components/application/ai-chat/file-tags";
import { pickFiles } from "@/lib/platform";
import { useChatStore, sessionKey as computeSessionKey, type ActiveSession } from "../store";
import { matchAppCommand } from "@/components/application/ai-chat/app-commands";
import { recordPrompt } from "../prompt-history";
import { IMAGE_EXTENSIONS } from "./use-composer-images";

/** Composer submit/draft/attach/stop handlers plus the pending-@mention
 * bridge, so ChatConversation stays a composition layer. */
export function useComposerActions({
  active,
  sessionKey,
  streaming,
  images,
  clearImages,
  importImageFiles,
  supportsImages,
  composerInputRef,
}: {
  active: ActiveSession | null;
  sessionKey: string;
  streaming: boolean;
  images: string[];
  clearImages: () => void;
  importImageFiles: (paths: string[], supported: boolean) => void;
  supportsImages: boolean;
  composerInputRef: React.RefObject<ComposerInputHandle | null>;
}) {
  const { t } = useTranslation();
  const pendingMention = useChatStore((s) => s.pendingMention);
  const {
    setDraft,
    clearPendingMention,
    send,
    queueMessage,
    interrupt,
    startNewChat,
    compactContext,
  } = useChatStore(
    useShallow((s) => ({
      setDraft: s.setDraft,
      clearPendingMention: s.clearPendingMention,
      send: s.send,
      queueMessage: s.queueMessage,
      interrupt: s.interrupt,
      startNewChat: s.startNewChat,
      compactContext: s.compactContext,
    })),
  );

  const submit = useCallback(
    (value: string) => {
      let targetActive = active;
      let targetKey = sessionKey;
      if (!targetActive) {
        const s = useChatStore.getState();
        const ws =
          s.workspaces.find((w) => !s.archivedWorkspaces?.includes(w.id)) ??
          s.workspaces[0];
        if (ws) {
          startNewChat(ws.path);
          targetActive = useChatStore.getState().active;
          if (targetActive) {
            targetKey = computeSessionKey(
              targetActive.engine,
              targetActive.sessionId,
              targetActive.workspacePath,
            );
          }
        }
      }
      if (!targetActive || (!value.trim() && images.length === 0)) return;
      recordPrompt(value);
      setDraft(targetKey, "");
      clearImages();
      // App-level commands ("/new", "/compact") never reach the engine —
      // headless/protocol launches can't interpret them. A user-defined
      // catalog command of the same name takes precedence (matchAppCommand).
      if (images.length === 0) {
        const command = matchAppCommand(value, targetActive.workspacePath);
        if (command === "new") {
          startNewChat(targetActive.workspacePath);
          return;
        }
        if (command === "compact" && targetActive.sessionId && !streaming) {
          void compactContext();
          return;
        }
      }
      // A turn is in flight: park the message in the session's queue; the
      // store drains it FIFO when the turn ends.
      if (streaming) {
        queueMessage(value, images);
        return;
      }
      void send(value, images);
    },
    [active, images, streaming, sessionKey, setDraft, clearImages, send, queueMessage, startNewChat, compactContext],
  );

  // File-tree "+" asks the composer to insert an @path mention at the caret.
  useEffect(() => {
    if (!pendingMention) return;
    clearPendingMention();
    const input = composerInputRef.current;
    if (!input) return;
    input.focus();
    input.insertText(`${mentionToken(pendingMention.path)} `);
  }, [pendingMention, clearPendingMention, composerInputRef]);

  const handleDraftChange = useCallback(
    (v: string) => setDraft(sessionKey, v),
    [sessionKey, setDraft],
  );

  // Shared partition for the file picker and OS drops: images flow through
  // the sandboxed image pipeline (chips); every other file becomes an
  // @mention at the caret — same as the file tree's "+" — so its content
  // stays live instead of a frozen sandbox copy.
  const routeIncomingPaths = useCallback(
    (paths: string[]) => {
      if (paths.length === 0) return;
      const imagePaths: string[] = [];
      const mentionPaths: string[] = [];
      for (const path of paths) {
        const ext = path.split(".").pop()?.toLowerCase() ?? "";
        (IMAGE_EXTENSIONS.includes(ext) ? imagePaths : mentionPaths).push(path);
      }
      if (mentionPaths.length > 0) {
        const input = composerInputRef.current;
        if (input) {
          input.focus();
          input.insertText(`${mentionPaths.map(mentionToken).join(" ")} `);
        }
      }
      if (imagePaths.length > 0) importImageFiles(imagePaths, supportsImages);
    },
    [composerInputRef, importImageFiles, supportsImages],
  );

  // "Add → Files and folders": native multi-picker.
  const handleAddAttachments = useCallback(() => {
    void (async () => {
      const picked = await pickFiles(t("chat.addFilesFolders"));
      routeIncomingPaths(picked);
    })();
  }, [t, routeIncomingPaths]);

  const handleStop = useCallback(() => void interrupt(), [interrupt]);
  const handlePickSkills = useCallback(
    () => composerInputRef.current?.openSlashPicker(),
    [composerInputRef],
  );

  return {
    submit,
    handleDraftChange,
    handleAddAttachments,
    /** OS file drop onto the composer (absolute native paths). */
    handleDroppedPaths: routeIncomingPaths,
    handleStop,
    handlePickSkills,
  };
}
