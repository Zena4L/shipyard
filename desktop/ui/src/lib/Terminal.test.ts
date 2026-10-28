import { cleanup, render } from "@testing-library/svelte";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

// jsdom lays nothing out: xterm itself stands in, recording what it's told.
const written: string[] = [];
let typed: (data: string) => void = () => {};
const terminals: { options: unknown; cols: number; rows: number }[] = [];
vi.mock("@xterm/xterm", () => ({
  Terminal: class {
    cols = 80;
    rows = 24;
    options: unknown;
    constructor(options: unknown) {
      this.options = options;
      terminals.push(this);
    }
    loadAddon() {}
    open() {}
    focus() {}
    dispose() {}
    write(data: string | Uint8Array, done?: () => void) {
      written.push(typeof data === "string" ? data : new TextDecoder().decode(data));
      done?.();
    }
    onData(handler: (data: string) => void) {
      typed = handler;
      return { dispose() {} };
    }
    onResize() {
      return { dispose() {} };
    }
  },
}));
vi.mock("@xterm/addon-fit", () => ({ FitAddon: class { fit() {} } }));
vi.mock("./sheets", () => ({ keepStylesToTheCsp: () => {} }));

const { default: Terminal } = await import("./Terminal.svelte");

// What WebKit has from Safari 16.4 (jsdom hasn't): constructed stylesheets.
function withSheets(on: boolean) {
  if (on) {
    Object.defineProperty(Document.prototype, "adoptedStyleSheets", { configurable: true, value: [], writable: true });
    CSSStyleSheet.prototype.replaceSync ??= () => {};
  } else {
    delete (Document.prototype as { adoptedStyleSheets?: unknown }).adoptedStyleSheets;
  }
}
beforeEach(() => withSheets(true));
afterEach(() => {
  cleanup();
  written.length = 0;
});

/** A core whose shells say `/ # `; `output(session)` writes more, `end` ends it. */
function backend(opts: { endAtOnce?: boolean } = {}) {
  let sessions = 0;
  const outputs = new Map<number, (bytes: Uint8Array) => void>();
  const calls = {
    shellOpen: vi.fn(async (_id: string, _cols: number, _rows: number, onoutput: (bytes: Uint8Array) => void) => {
      const number = ++sessions;
      outputs.set(number, onoutput);
      onoutput(new TextEncoder().encode("/ # "));
      // An `sh` that exits at once: its end before its number is back.
      if (opts.endAtOnce) onoutput(new Uint8Array());
      return number;
    }),
    shellInput: vi.fn(async () => {}),
    shellResize: vi.fn(async () => {}),
    shellAck: vi.fn(async () => {}),
    shellClose: vi.fn(async () => {}),
  };
  return {
    calls,
    output: (session: number, bytes: Uint8Array) => outputs.get(session)?.(bytes),
    end: (session: number) => outputs.get(session)?.(new Uint8Array()),
  };
}

describe("the terminal", () => {
  it("opens a shell in the container, types into it in order, and closes it when it goes", async () => {
    const { calls } = backend();
    let release = () => {};
    calls.shellInput.mockImplementationOnce(() => new Promise<void>((resolve) => (release = resolve)));
    const { unmount } = render(Terminal, { props: { id: "api", backend: calls } });
    await vi.waitFor(() => expect(calls.shellOpen).toHaveBeenCalledWith("api", 80, 24, expect.any(Function)));
    await vi.waitFor(() => expect(written.join("")).toContain("/ # "));
    typed("l");
    typed("s");
    await vi.waitFor(() => expect(calls.shellInput).toHaveBeenCalledTimes(1));
    // The second waits for the first.
    expect(calls.shellInput).toHaveBeenLastCalledWith(1, "l");
    release();
    await vi.waitFor(() => expect(calls.shellInput).toHaveBeenLastCalledWith(1, "s"));
    unmount();
    expect(calls.shellClose).toHaveBeenCalledWith(1);
  });

  it("says what it wrote, so the core sends more", async () => {
    const { calls, output } = backend();
    render(Terminal, { props: { id: "api", backend: calls } });
    await vi.waitFor(() => expect(calls.shellOpen).toHaveBeenCalled());
    await vi.waitFor(() => expect(written.join("")).toContain("/ # "));
    output(1, new Uint8Array(64 * 1024));
    await vi.waitFor(() => expect(calls.shellAck).toHaveBeenCalledWith(1, 64 * 1024 + 4));
  });

  it("says its end, after its output, and opens another on Enter", async () => {
    const { calls, end } = backend();
    render(Terminal, { props: { id: "api", backend: calls } });
    await vi.waitFor(() => expect(calls.shellOpen).toHaveBeenCalledTimes(1));
    await vi.waitFor(() => expect(written.join("")).toContain("/ # "));
    end(1);
    expect(written.join("")).toContain("[the shell ended: Enter for another]");
    typed("x");
    expect(calls.shellInput).not.toHaveBeenCalled();
    typed("\r");
    await vi.waitFor(() => expect(calls.shellOpen).toHaveBeenCalledTimes(2));
  });

  it("says an end that came before its number did", async () => {
    const { calls } = backend({ endAtOnce: true });
    render(Terminal, { props: { id: "api", backend: calls } });
    await vi.waitFor(() => expect(written.join("")).toContain("[the shell ended: Enter for another]"));
    typed("\r");
    await vi.waitFor(() => expect(calls.shellOpen).toHaveBeenCalledTimes(2));
  });

  it("sends a resize made while it opened", async () => {
    const { calls } = backend();
    let finish = (_n: number) => {};
    calls.shellOpen.mockImplementationOnce(() => new Promise<number>((resolve) => (finish = resolve)));
    render(Terminal, { props: { id: "api", backend: calls } });
    await vi.waitFor(() => expect(calls.shellOpen).toHaveBeenCalled());
    const term = terminals.at(-1)!;
    term.cols = 100;
    finish(1);
    await vi.waitFor(() => expect(calls.shellResize).toHaveBeenCalledWith(1, 100, 24));
  });

  it("says a refusal as text, and needs constructed stylesheets", async () => {
    const { calls } = backend();
    calls.shellOpen.mockRejectedValueOnce({ kind: "api", message: "not running\u001b]0;x\u0007" });
    render(Terminal, { props: { id: "api", backend: calls } });
    await vi.waitFor(() => expect(written.join("")).toContain("not running]0;x"));
    expect(written.join("")).not.toContain("\u001b");
    cleanup();
    withSheets(false);
    const { container } = render(Terminal, { props: { id: "api", backend: calls } });
    expect(container.textContent).toContain("The shell needs macOS 13.3 or later");
    expect(calls.shellOpen).toHaveBeenCalledTimes(1);
  });

  it("opens no link a container prints", async () => {
    const { calls } = backend();
    render(Terminal, { props: { id: "api", backend: calls } });
    await vi.waitFor(() => expect(calls.shellOpen).toHaveBeenCalled());
    const opened = vi.spyOn(window, "open").mockReturnValue(null);
    const options = terminals.at(-1)?.options as { linkHandler: { activate: (event: MouseEvent, uri: string) => void } };
    options.linkHandler.activate(new MouseEvent("click"), "https://example.test/");
    expect(opened).not.toHaveBeenCalled();
    opened.mockRestore();
  });
});
