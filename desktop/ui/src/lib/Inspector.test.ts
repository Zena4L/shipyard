import { cleanup, fireEvent, render } from "@testing-library/svelte";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Container, LinkEdge, Links } from "./backend";
import { mapModel } from "./graph";
import Inspector from "./Inspector.svelte";
import { row } from "./model";
import type { Focus } from "./store";

afterEach(cleanup);

const HOSTILE = '<img src=x onerror="window.pwned=1">';

/** worker connects to db, undeclared; worker's argument and db's service
 * name are hostile. */
function props() {
  const node = (id: string, service: string) => ({
    id,
    name: id,
    serial: "SHPU 000001 0",
    memory_max: 256 * 1024 * 1024,
    started_at: null,
    project: "shop",
    service,
    state: "running",
    exit_code: null,
  });
  const edge: LinkEdge = {
    from: "worker",
    to: "db",
    to_missing: false,
    kind: "observed",
    port: 5432,
    protocol: "tcp",
    condition: null,
    connections: 1,
    via_host_port: false,
    refused: false,
  };
  const links: Links = {
    version: 1,
    containers: [node("worker", "worker"), node("db", HOSTILE)],
    links: [edge],
    observed: { available: true, cause: null, reason: null },
  };
  const containers = new Map<string, Container>([
    [
      "worker",
      {
        Id: "worker",
        Image: "w:1",
        Labels: { "com.docker.compose.project.config_files": "/srv/shop/compose.yaml" },
        NetworkSettings: { Networks: { shop_back: { IPAddress: "10.2.0.3" } } },
        Mounts: [{ Type: "volume", Name: "shop_data", Destination: "/data", RW: true }],
      },
    ],
    ["db", { Id: "db", Image: "d:1" }],
  ]);
  const model = mapModel(links, containers, []);
  const focus: Focus = {
    id: "worker",
    isolation: {
      version: 1,
      name: "worker",
      namespaces: [],
      cgroup: "/c",
      init_pid: 10,
      command: { pid: 11, args: ["run", HOSTILE], truncated: false },
      user: null,
    },
    inspected: null,
    samples: [],
    ended: null,
  };
  const worker = model.containers[0]!;
  return {
    node: worker,
    row: row(links.containers[0] as never, containers.get("worker"), undefined, 0),
    model,
    summary: containers.get("worker"),
    focus,
    rootless: false,
    onact: vi.fn(),
    onshell: vi.fn(),
    onselect: () => {},
    onsection: vi.fn(),
  };
}

describe("the inspector", () => {
  it("says what a container says as text, never as markup, and offers the lines to copy", async () => {
    const { container, getByRole } = render(Inspector, { props: props() });
    expect(container.querySelector("img")).toBeNull();
    expect(container.textContent).toContain(`run ${HOSTILE}`);
    await fireEvent.click(getByRole("button", { name: "Add db to depends_on" }));
    const yaml = container.querySelector("pre.files")?.textContent ?? "";
    expect(yaml).toContain("  worker:\n    depends_on:\n");
    expect(yaml).toContain(JSON.stringify(HOSTILE));
    expect(container.textContent).toContain("Merge into /srv/shop/compose.yaml");
    expect((window as unknown as { pwned?: number }).pwned).toBeUndefined();
  });

  it("says whether Copy copied, and how to copy where it can't", async () => {
    const clipboard = Object.getOwnPropertyDescriptor(navigator, "clipboard");
    const copied: string[] = [];
    try {
      Object.defineProperty(navigator, "clipboard", {
        configurable: true,
        value: { writeText: vi.fn(async (text: string) => void copied.push(text)) },
      });
      const { container, getByRole, findByText } = render(Inspector, { props: props() });
      await fireEvent.click(getByRole("button", { name: "Add db to depends_on" }));
      await fireEvent.click(getByRole("button", { name: "Copy" }));
      await findByText("Copied");
      expect(copied).toEqual([container.querySelector("pre.files")?.textContent]);
      cleanup();
      // A web view that refuses it.
      Object.defineProperty(navigator, "clipboard", {
        configurable: true,
        value: { writeText: vi.fn(async () => Promise.reject(new Error("not allowed"))) },
      });
      const again = render(Inspector, { props: props() });
      await fireEvent.click(again.getByRole("button", { name: "Add db to depends_on" }));
      await fireEvent.click(again.getByRole("button", { name: "Copy" }));
      await again.findByText("Select the lines to copy them");
    } finally {
      if (clipboard) Object.defineProperty(navigator, "clipboard", clipboard);
      else delete (navigator as { clipboard?: unknown }).clipboard;
    }
  });

  it("closes a hint's lines for another container", async () => {
    const first = props();
    const { container, getByRole, rerender } = render(Inspector, { props: first });
    await fireEvent.click(getByRole("button", { name: "Add db to depends_on" }));
    expect(container.querySelector("pre.files")).not.toBeNull();
    // Another container with the same hint: closed until asked.
    const other = { ...first.node, id: "worker-2", name: "worker-2" };
    const model = { ...first.model, edges: first.model.edges.map((edge) => ({ ...edge, from: "worker-2" })) };
    await rerender({ ...first, node: other, model });
    expect(getByRole("button", { name: "Add db to depends_on" })).not.toBeNull();
    expect(container.querySelector("pre.files")).toBeNull();
  });

  it("says each action's command, and confirms a remove inline, stopping a running one", async () => {
    const given = props();
    const { container, getByRole, queryByRole } = render(Inspector, { props: given });
    expect(getByRole("button", { name: "Stop" }).getAttribute("title")).toBe("shipyard stop worker");
    expect(getByRole("button", { name: "Pause" }).getAttribute("title")).toBe("shipyard pause worker");
    await fireEvent.click(getByRole("button", { name: "Stop" }));
    expect(given.onact).toHaveBeenLastCalledWith("stop", false);
    // Remove asks first, saying what it does and its command.
    await fireEvent.click(getByRole("button", { name: "Remove" }));
    expect(given.onact).toHaveBeenCalledTimes(1);
    const confirm = container.querySelector(".confirm")?.textContent ?? "";
    expect(confirm).toContain("It's stopped, and its writable layer deleted.");
    expect(confirm).toContain("shipyard rm -f worker");
    await fireEvent.click(getByRole("button", { name: "Keep" }));
    expect(container.querySelector(".confirm")).toBeNull();
    expect(given.onact).toHaveBeenCalledTimes(1);
    await fireEvent.click(getByRole("button", { name: "Remove" }));
    await fireEvent.click(getByRole("button", { name: "Remove" }));
    expect(given.onact).toHaveBeenLastCalledWith("remove", true);
    expect(queryByRole("button", { name: "Keep" })).toBeNull();
  });

  it("closes a remove's confirm for another container, and waits while one is under way", async () => {
    const first = props();
    const { container, getByRole, rerender } = render(Inspector, { props: first });
    await fireEvent.click(getByRole("button", { name: "Remove" }));
    expect(container.querySelector(".confirm")).not.toBeNull();
    const other = { ...first.node, id: "worker-2", name: "worker-2" };
    await rerender({ ...first, node: other, busy: true });
    expect(container.querySelector(".confirm")).toBeNull();
    expect((getByRole("button", { name: "Stop" }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("removes a stopped one without forcing it, its name as text", async () => {
    const given = props();
    const stopped = { ...given.node, name: HOSTILE, state: "exited" as const };
    const { container, getByRole } = render(Inspector, { props: { ...given, node: stopped } });
    await fireEvent.click(getByRole("button", { name: "Remove" }));
    const confirm = container.querySelector(".confirm")!;
    expect(confirm.textContent).toContain(`Remove ${HOSTILE}? Its writable layer is deleted.`);
    expect(confirm.textContent).toContain(`shipyard rm ${HOSTILE}`);
    expect(container.querySelector("img")).toBeNull();
    await fireEvent.click(getByRole("button", { name: "Remove" }));
    expect(given.onact).toHaveBeenLastCalledWith("remove", false);
  });

  it("closes a remove's confirm once a running container comes to need it", async () => {
    const given = props();
    // db: nothing needs it while worker is down.
    const db = given.model.all.find((n) => n.id === "db")!;
    const quiet = {
      ...given.model,
      all: given.model.all.map((n) => (n.id === "worker" ? { ...n, state: "exited" as const } : n)),
    };
    const { container, getByRole, rerender } = render(Inspector, { props: { ...given, node: db, model: quiet } });
    expect((getByRole("button", { name: "Remove" }) as HTMLButtonElement).disabled).toBe(false);
    await fireEvent.click(getByRole("button", { name: "Remove" }));
    expect(container.querySelector(".confirm")).not.toBeNull();
    // worker starts, and needs it.
    await rerender({ ...given, node: db, model: given.model });
    expect(container.querySelector(".confirm")).toBeNull();
    expect((getByRole("button", { name: "Remove" }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("keeps what's open across a new read of the same container", async () => {
    const given = props();
    const { container, getByRole, rerender } = render(Inspector, { props: given });
    await fireEvent.click(getByRole("button", { name: "Add db to depends_on" }));
    await fireEvent.click(getByRole("button", { name: "Remove" }));
    // The links read again: the same container, as a new node.
    await rerender({ ...given, node: { ...given.node }, model: { ...given.model } });
    expect(container.querySelector(".confirm")).not.toBeNull();
    expect(container.querySelector("pre.files")).not.toBeNull();
  });

  it("opens the console's shell from its Shell button, its command its tooltip", async () => {
    const given = props();
    const { getByRole } = render(Inspector, { props: given });
    const shell = getByRole("button", { name: "Shell" });
    expect(shell.getAttribute("title")).toBe("shipyard exec -it worker sh");
    await fireEvent.click(shell);
    expect(given.onshell).toHaveBeenCalled();
    expect(given.onact).not.toHaveBeenCalled();
  });

  it("opens its network and volume in their sections, by name", async () => {
    const given = props();
    const { container } = render(Inspector, { props: given });
    const attached = [...container.querySelectorAll(".att .attv")] as HTMLElement[];
    await fireEvent.click(attached.find((el) => el.textContent?.includes("10.2.0.3"))!);
    expect(given.onsection).toHaveBeenCalledWith("networks", "shop_back");
    await fireEvent.click(attached.find((el) => el.textContent?.includes("/data"))!);
    expect(given.onsection).toHaveBeenCalledWith("volumes", "shop_data");
  });
});
