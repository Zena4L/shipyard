import { cleanup, fireEvent, render, waitFor } from "@testing-library/svelte";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App from "./App.svelte";
import { fixtureBackend, NOW } from "./lib/fixture";

// jsdom lays nothing out: the map's observer of its size has nothing to see.
beforeEach(() => {
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  );
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("the Run sheet in the window", () => {
  it("opens with ⌘K and the buttons, closes with Escape and a second click, and forgets a failure", async () => {
    const backend = {
      ...fixtureBackend,
      runImage: async () => Promise.reject({ kind: "api", message: "no such image" }),
    };
    const { container, getByRole, findByText } = render(App, { props: { backend, now: () => NOW } });
    await findByText("Daemonless");
    const sheet = () => container.querySelector("#runsheet");
    expect(sheet()).toBeNull();
    await fireEvent.keyDown(document, { key: "k", metaKey: true });
    await waitFor(() => expect(sheet()).not.toBeNull());
    expect(document.activeElement?.id).toBe("rImage");
    // A failed run is said, then forgotten when the sheet closes.
    await fireEvent.click(getByRole("button", { name: "Run" }));
    await findByText("no such image");
    await fireEvent.keyDown(document, { key: "Escape" });
    await waitFor(() => expect(sheet()).toBeNull());
    await fireEvent.click(getByRole("button", { name: "Run image" }));
    await waitFor(() => expect(sheet()).not.toBeNull());
    expect(container.textContent).not.toContain("no such image");
    await fireEvent.click(getByRole("button", { name: "Run image" }));
    await waitFor(() => expect(sheet()).toBeNull());
  });

  it("leaves the view alone for a run that ends after its sheet was closed", async () => {
    let finish = (_id: string) => {};
    const backend = { ...fixtureBackend, runImage: () => new Promise<string>((resolve) => (finish = resolve)) };
    const { container, getByRole, findByText } = render(App, { props: { backend, now: () => NOW } });
    await findByText("Daemonless");
    const mapButton = getByRole("button", { name: "Map", exact: true } as never);
    expect(mapButton.getAttribute("aria-pressed")).toBe("true");
    await fireEvent.keyDown(document, { key: "k", metaKey: true });
    await waitFor(() => expect(container.querySelector("#runsheet")).not.toBeNull());
    await fireEvent.click(getByRole("button", { name: "Run" }));
    await fireEvent.keyDown(document, { key: "Escape" });
    finish("new");
    await new Promise((resolve) => setTimeout(resolve));
    expect(mapButton.getAttribute("aria-pressed")).toBe("true");
    expect(container.querySelector("#runsheet")).toBeNull();
  });
});
