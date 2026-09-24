// Repro fixture: the REAL ConversationFooter (same composer the session
// page uses) with an active session seeded into the real chat store, plus
// seeded agent/prompt catalogs. Type `#` / `!` in the field to exercise the
// pickers exactly as the app does. Probe: after typing, document body text
// must contain 我的智能体 / 新建智能体 (agent menu) or 新建提示词.
import { useState } from "react";
import { createRoot } from "react-dom/client";
import { HashRouter } from "react-router-dom";
import "../../src/index.css";
import "../../src/lib/i18n";
import { ConversationFooter } from "../../src/features/chat/components/ConversationFooter";
import { useChatStore } from "../../src/features/chat/store";
import { useAgentStore } from "../../src/features/agents/agent-store";
import { usePromptStore } from "../../src/features/prompts/prompt-store";

const ROOT = "/fixture-ws";

useAgentStore.setState({
  agents: [
    { id: "a1", name: "代码审查员", prompt: "你是严格的代码审查员…", icon: "🔍" },
  ],
  builtInAgents: [],
  builtInDivisions: [],
  loaded: true,
  refresh: async () => {},
});
usePromptStore.setState({
  byRoot: {
    [ROOT]: {
      entries: [
        { name: "review", path: "/fixture-ws/.ccgui/prompts/review.md", description: "逐行审查", content: "请审查…", scope: "workspace" },
      ],
      status: "ready",
      fetchedAt: Date.now(),
    },
  },
  ensure: () => {},
  refresh: async () => {},
});

const ACTIVE = { engine: "claude", sessionId: "s-1", workspacePath: ROOT };

function Fixture() {
  const [draft, setDraft] = useState("");
  const [sentMessages, setSentMessages] = useState<string[]>([]);
  return (
    <div className="flex min-h-dvh flex-col justify-between bg-background-primary-default p-4">
      <div className="mx-auto flex w-full max-w-3xl flex-col gap-2 pt-6">
        <h2 className="text-body-bold text-text-primary">回车发送本地验证界面（无需编译桌面端）</h2>
        <p className="text-body-small text-text-secondary">
          在下方输入框中输入文本（支持中文拼音/英文），按 <strong>Enter</strong> 发送，<strong>Shift+Enter</strong> 换行：
        </p>
        <div className="flex flex-col gap-2 pt-2">
          {sentMessages.length === 0 ? (
            <div className="text-body-small text-text-tertiary">（暂无发送记录，请在下方输入并按回车测试）</div>
          ) : (
            sentMessages.map((msg, idx) => (
              <div
                key={idx}
                className="rounded-xl border border-separator-border bg-background-secondary-default p-3 text-body-medium text-text-primary shadow-xs"
              >
                <span className="mr-2 font-mono text-xs text-text-tertiary">#{idx + 1}</span>
                {msg}
              </div>
            ))
          )}
        </div>
      </div>
      <ConversationFooter
        active={ACTIVE}
        workspaces={[]}
        queue={[]}
        onRemoveQueued={() => {}}
        onSendQueuedNow={() => {}}
        imageError={null}
        branchError={null}
        onDismissImageError={() => {}}
        onDismissBranchError={() => {}}
        images={[]}
        previews={{}}
        onRemoveImage={() => {}}
        draft={draft}
        onDraftChange={setDraft}
        onSubmit={(text) => {
          setSentMessages((prev) => [...prev, text]);
          setDraft("");
        }}
        noEnabledEngines={false}
        composerInputRef={{ current: null }}
        addMenu={null}
        cliMenu={null}
        permissionMenu={null}
        supportsImages={false}
        onPasteImages={() => {}}
        sessionUsage={null}
        contextMax={0}
        branch={undefined}
        branches={undefined}
        onBranchSelect={() => {}}
        startNewChat={() => {}}
      />
    </div>
  );
}

createRoot(document.getElementById("fixture")!).render(
  <HashRouter>
    <Fixture />
  </HashRouter>,
);
