import { describe, expect, it } from "vitest";
import { shellQuote } from "./terminalLogin";

describe("shellQuote", () => {
  it("leaves plain command names and safe paths unquoted", () => {
    expect(shellQuote("pi")).toBe("pi");
    expect(shellQuote("/usr/local/bin/codebuddy")).toBe("/usr/local/bin/codebuddy");
  });

  it("quotes paths with spaces for the POSIX shell", () => {
    expect(shellQuote("/opt/my cli")).toBe("'/opt/my cli'");
  });

  it("keeps shell metacharacters literal", () => {
    for (const value of ["/opt/$HOME;rm -rf /", "/opt/`whoami`", "/opt/a|b&c"]) {
      expect(shellQuote(value)).toBe(`'${value}'`);
    }
  });

  it("escapes embedded single quotes instead of terminating the string", () => {
    expect(shellQuote("/opt/it's here")).toBe("'/opt/it'\\''s here'");
  });
});
