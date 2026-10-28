import { cleanup, fireEvent, render } from "@testing-library/svelte";
import { afterEach, describe, expect, it, vi } from "vitest";
import Console from "./Console.svelte";
import type { MapNode } from "./graph";

vi.mock("./Terminal.svelte", async () => ({ default: (await import("./TerminalStub.test.svelte")).default }));

afterEach(cleanup);

const HOSTILE = '<img src=x onerror="window.pwned=1">';

const node = (state: string): MapNode =>
  ({ id: "api", name: "api", image: "ghcr.io/acme/api:2.4.1", state }) as unknown as MapNode;

function props(state = "running", tab: "logs" | "shell" = "logs") {
  return {
    node: node(state),
    hostPid: 48240,
    logs: {
      id: "api",
      lines: ["09:01:14.208 INFO  listening", `2026-10-04 09:01:09 UTC LOG: ${HOSTILE}`, "no time here"].map((text, n) => ({ n, text })),
    },
    tab,
    collapsed: false,
    backend: {} as never,
    ontab: vi.fn(),
    ontoggle: vi.fn(),
  };
}

describe("the console", () => {
  it("says the container, its logs as text, a leading time dimmed", () => {
    const { container } = render(Console, { props: props() });
    expect(container.querySelector(".dockmeta")?.textContent).toBe("apighcr.io/acme/api:2.4.1host PID 48240");
    const body = container.querySelector(".termbody")!;
    expect(body.textContent).toBe(`09:01:14.208 INFO  listening\n2026-10-04 09:01:09 UTC LOG: ${HOSTILE}\nno time here`);
    expect([...body.querySelectorAll(".t")].map((t) => t.textContent)).toEqual(["09:01:14.208", "2026-10-04 09:01:09 UTC"]);
    expect(container.querySelector("img")).toBeNull();
  });

  it("switches tabs and folds, each by its own button, with its command as its tooltip", async () => {
    const given = props();
    const { getByRole } = render(Console, { props: given });
    expect(getByRole("tab", { name: "Logs" }).getAttribute("title")).toBe("shipyard logs -f api");
    expect(getByRole("tab", { name: "Shell" }).getAttribute("title")).toBe("shipyard exec -it api sh");
    await fireEvent.click(getByRole("tab", { name: "Shell" }));
    expect(given.ontab).toHaveBeenCalledWith("shell");
    await fireEvent.click(getByRole("button", { name: "Collapse console" }));
    expect(given.ontoggle).toHaveBeenCalled();
  });

  it("opens a shell on a running container only, and keeps it behind Logs", async () => {
    const stopped = render(Console, { props: props("exited", "shell") });
    expect(stopped.container.textContent).toContain("container is not running — start it to open a shell");
    expect(stopped.container.querySelector(".stub-terminal")).toBeNull();
    cleanup();
    const given = props("running", "shell");
    const { container, rerender } = render(Console, { props: given });
    expect(container.querySelector(".stub-terminal")).not.toBeNull();
    // Back to Logs: the shell kept, hidden, not closed.
    await rerender({ ...given, tab: "logs" });
    expect(container.querySelector(".stub-terminal")).not.toBeNull();
    expect(container.querySelector<HTMLElement>(".shellhost")?.hidden).toBe(true);
    // Stopped, it's gone; started again, it isn't opened until asked.
    await rerender({ ...given, tab: "logs", node: node("exited") });
    expect(container.querySelector(".stub-terminal")).toBeNull();
    await rerender({ ...given, tab: "logs", node: node("running") });
    expect(container.querySelector(".stub-terminal")).toBeNull();
    await rerender({ ...given, tab: "shell", node: node("running") });
    expect(container.querySelector(".stub-terminal")).not.toBeNull();
  });
});
