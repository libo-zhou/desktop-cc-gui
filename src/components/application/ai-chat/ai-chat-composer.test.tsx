import { useState } from "react";
import { ConversationFooter } from "@/features/chat/components/ConversationFooter";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { HashRouter } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import "@/lib/i18n";
import { Composer } from "./ai-chat-composer";
import { extractText, getCaretOffset } from "./file-tags";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT =
  true;

const PREFILL = "/ccgui-plugin-creator ";

/**
 * 草稿恢复（外部 value 变化）会重建 editable 的 DOM；重建时浏览器手里的插入
 * 点一并消失，随后 focus()（creator flow 的 focusComposerWhenVisible）把光标
 * 放回内容开头。这里钉住修复后的落点：文本末尾，用户可以直接接着敲需求；
 * 文本本身按原文渲染（斜杠指令不再有特殊展示逻辑）。
 */
describe("Composer 草稿恢复", () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => {
      root.unmount();
    });
    container.remove();
  });

  /** 用受控 value 渲染（等价于 store 的 draft），返回 editable。 */
  async function render(value: string): Promise<HTMLElement> {
    await act(async () => {
      root.render(
        <HashRouter>
          <Composer value={value} onValueChange={() => {}} />
        </HashRouter>,
      );
    });
    const el = container.querySelector<HTMLElement>(".composer-editable");
    expect(el).not.toBeNull();
    return el!;
  }

  it("挂载即带草稿时光标落在文本末尾", async () => {
    const el = await render(PREFILL);
    expect(extractText(el)).toBe(PREFILL);
    expect(getCaretOffset(el)).toBe(PREFILL.length);
  });

  it("挂载后才收到草稿（插件中心「创建插件」）同样落在末尾", async () => {
    await render("");
    const el = await render(PREFILL);
    expect(extractText(el)).toBe(PREFILL);
    expect(getCaretOffset(el)).toBe(PREFILL.length);
  });

  it("回车发送消息", async () => {
    let submitted = "";
    await act(async () => {
      root.render(
        <HashRouter>
          <Composer
            value={PREFILL}
            onValueChange={() => {}}
            onSubmit={(val) => {
              submitted = val;
            }}
            sendShortcut="enter"
          />
        </HashRouter>,
      );
    });
    const el = container.querySelector<HTMLElement>(".composer-editable")!;
    await act(async () => {
      el.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }));
    });
    expect(submitted).toBe(PREFILL);
  });
  it("即使用受控 disabled=true 挂载，只要输入框有内容回车也能发送", async () => {
    let submitted = "";
    await act(async () => {
      root.render(
        <HashRouter>
          <Composer
            value=""
            disabled={true}
            onValueChange={() => {}}
            onSubmit={(val) => {
              submitted = val;
            }}
            sendShortcut="enter"
          />
        </HashRouter>,
      );
    });
    const el = container.querySelector<HTMLElement>(".composer-editable")!;
    el.innerHTML = "即时打字";
    await act(async () => {
      el.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }));
    });
    expect(submitted).toBe("即时打字");
  });

  it("输入法 composition 异常残留状态在收到非组合回车时自愈并发送", async () => {
    let submitted = "";
    await act(async () => {
      root.render(
        <HashRouter>
          <Composer
            value=""
            onValueChange={() => {}}
            onSubmit={(val) => {
              submitted = val;
            }}
            sendShortcut="enter"
          />
        </HashRouter>,
      );
    });
    const el = container.querySelector<HTMLElement>(".composer-editable")!;
    // Simulate compositionstart without compositionend
    await act(async () => {
      el.dispatchEvent(new CompositionEvent("compositionstart", { bubbles: true }));
    });
    el.innerHTML = "中文输入完成";
    // Press Enter with native isComposing=false
    await act(async () => {
      el.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }));
    });
    expect(submitted).toBe("中文输入完成");
  });

  it("Shift+Enter 不发送消息", async () => {
    let submitted = "";
    await act(async () => {
      root.render(
        <HashRouter>
          <Composer
            value="多行文本"
            onValueChange={() => {}}
            onSubmit={(val) => {
              submitted = val;
            }}
            sendShortcut="enter"
          />
        </HashRouter>,
      );
    });
    const el = container.querySelector<HTMLElement>(".composer-editable")!;
    await act(async () => {
      el.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", shiftKey: true, bubbles: true, cancelable: true }));
    });
    expect(submitted).toBe("");
  });

  it("空内容回车不发送消息", async () => {
    let submitted = "";
    await act(async () => {
      root.render(
        <HashRouter>
          <Composer
            value=""
            disabled={true}
            onValueChange={() => {}}
            onSubmit={(val) => {
              submitted = val;
            }}
            sendShortcut="enter"
          />
        </HashRouter>,
      );
    });
    const el = container.querySelector<HTMLElement>(".composer-editable")!;
    el.innerHTML = "   ";
    await act(async () => {
      el.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }));
    });
    expect(submitted).toBe("");
  });

  it("ConversationFooter 中回车发送消息", async () => {
    let submitted = "";
    function Harness() {
      const [draft, setDraft] = useState("");
      return (
        <ConversationFooter
          active={{ engine: "claude", sessionId: "s-1", workspacePath: "/test-ws" }}
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
          onSubmit={(val) => {
            submitted = val;
          }}
          sendShortcut="enter"
          onStop={() => {}}
          streaming={false}
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
          branchRepoName={undefined}
          onBranchSelect={() => {}}
          startNewChat={() => {}}
        />
      );
    }

    await act(async () => {
      root.render(
        <HashRouter>
          <Harness />
        </HashRouter>,
      );
    });

    const el = container.querySelector<HTMLElement>(".composer-editable")!;
    expect(el).not.toBeNull();

    // Simulate typing
    el.innerHTML = "测试消息";
    await act(async () => {
      el.dispatchEvent(new Event("input", { bubbles: true }));
    });

    // Simulate pressing Enter
    await act(async () => {
      el.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }));
    });

    expect(submitted).toBe("测试消息");
  });
});
