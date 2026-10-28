import { cleanup, fireEvent, render } from "@testing-library/svelte";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Container, LinkEdge, Links, Network } from "./backend";
import { imagesModel } from "./layers";
import { mapModel } from "./graph";
import Map from "./Map.svelte";

// jsdom measures nothing: the map's width is watched with a stub.
globalThis.ResizeObserver ??= class {
  observe() {}
  unobserve() {}
  disconnect() {}
} as unknown as typeof ResizeObserver;

afterEach(cleanup);

const HOSTILE = '<img src=x onerror="window.pwned=1">';

const link = (from: string, to: string, extra: Partial<LinkEdge> = {}): LinkEdge => ({
  from,
  to,
  to_missing: false,
  kind: "declared",
  port: null,
  protocol: null,
  condition: null,
  connections: 0,
  via_host_port: false,
  refused: false,
  ...extra,
});

const node = (id: string, name: string, state = "running") => ({
  id,
  name,
  serial: "SHPU 000001 0",
  memory_max: 256 * 1024 * 1024,
  started_at: null,
  project: "p",
  state,
  exit_code: null,
});

/** web → api → db, db `dbState` and refused; solo on its own, publishing
 * 53 for TCP and UDP; a hostile name on api, its network and its volume. */
function model(dbState = "exited") {
  const entry = (protocol: string) => ({
    container: "solo",
    host_ip: "0.0.0.0",
    host_port: 53,
    container_port: 53,
    protocol,
    connections: 1,
  });
  const links: Links = {
    version: 1,
    containers: [node("web", "web"), node("api", HOSTILE), node("db", "db", dbState), node("solo", "solo")],
    links: [link("web", "api"), link("api", "db", { refused: true })],
    entries: [entry("tcp"), entry("udp")],
    observed: { available: true, cause: null, reason: null },
  };
  const containers = new globalThis.Map<string, Container>([
    ["web", { Id: "web", Image: "web:1" }],
    [
      "api",
      {
        Id: "api",
        Image: "<b>x</b>",
        NetworkSettings: { Networks: { [HOSTILE]: { IPAddress: "10.0.0.2" } } },
        Mounts: [{ Type: "volume", Name: HOSTILE, Destination: "/data", RW: true }],
      },
    ],
    ["db", { Id: "db", Image: "db:1" }],
    ["solo", { Id: "solo", Image: "solo:1" }],
  ]);
  const networks: Network[] = [{ name: HOSTILE, short: HOSTILE, subnet: "10.0.0.0/24", internal: false, gateway: null, driver: "bridge" }];
  return mapModel(links, containers, networks);
}

const props = (dbState = "exited") => ({
  model: model(dbState),
  selected: null,
  onselect: vi.fn(),
  onsection: vi.fn(),
  onstart: vi.fn(),
});

describe("the map", () => {
  it("shows what a container names as text, never as markup", () => {
    const { container } = render(Map, { props: props() });
    expect(container.querySelector("img")).toBeNull();
    expect(container.querySelector(".mnode .box b")).toBeNull();
    expect(container.textContent).toContain(HOSTILE);
    expect(container.querySelector(".netkey")?.textContent).toContain(HOSTILE);
    expect(container.querySelector(".vchip")?.textContent).toContain(HOSTILE);
    expect((window as unknown as { pwned?: number }).pwned).toBeUndefined();
  });

  it("says what's refused and offers its start, by the container's ID", async () => {
    const given = props();
    const { container, getByRole } = render(Map, { props: given });
    expect(container.querySelector(".mapstatus.warn")?.textContent).toContain("db is stopped");
    await fireEvent.click(getByRole("button", { name: "Start db" }));
    expect(given.onstart).toHaveBeenCalledWith("db");
  });

  it("offers no Start for a target that runs but isn't ready", () => {
    const { container, queryByRole } = render(Map, { props: props("running") });
    expect(container.querySelector(".mapstatus.warn")?.textContent).toContain("db isn't ready");
    expect(queryByRole("button", { name: "Start db" })).toBeNull();
  });

  it("says when there are no links at all, or none active, instead of counting none healthy", () => {
    const quiet = (links: LinkEdge[], webState: string) =>
      mapModel(
        {
          version: 1,
          containers: [node("web", "web", webState), node("api", "api")],
          links,
          entries: [],
          observed: { available: true, cause: null, reason: null },
        },
        new globalThis.Map<string, Container>([
          ["web", { Id: "web", Image: "web:1" }],
          ["api", { Id: "api", Image: "api:1" }],
        ]),
        [],
      );
    const said = (model: ReturnType<typeof quiet>) => {
      const { container } = render(Map, { props: { ...props(), model } });
      const text = container.querySelector(".mapstatus")?.textContent?.replace(/\s+/g, " ").trim();
      cleanup();
      return text;
    };
    expect(said(quiet([], "running"))).toBe(
      "No links between containers yet. A container that publishes a port, declares depends_on or connects to another is drawn here.",
    );
    // web, stopped, declares api: a link, none of it active.
    expect(said(quiet([link("web", "api")], "exited"))).toBe("No active links. Every connection is declared.");
    expect(said(quiet([link("web", "api")], "running"))).toBe("The one active link is healthy. Every connection is declared.");
  });

  it("draws a port published for TCP and for UDP as two chips", () => {
    const { container } = render(Map, { props: props() });
    expect([...container.querySelectorAll(".hostchip")].map((chip) => chip.textContent)).toEqual([":53", ":53/udp"]);
  });

  it("traces a hovered container's chain, dims the rest, and selects on a click", async () => {
    const given = props();
    const { container } = render(Map, { props: given });
    const box = (id: string) => container.querySelector(`.box[data-name="${id}"]`) as HTMLElement;
    const dimmed = (id: string) => box(id).closest(".mnode")?.classList.contains("dimmed");
    await fireEvent.mouseOver(box("db"));
    // db's chain: web and api; solo, outside it, dims.
    expect(["web", "api", "db", "solo"].map(dimmed)).toEqual([false, false, false, true]);
    await fireEvent.mouseOver(container.querySelector(".netkey") as HTMLElement);
    expect(["web", "api", "db", "solo"].map(dimmed)).toEqual([true, false, true, true]);
    await fireEvent.click(box("web"));
    expect(given.onselect).toHaveBeenCalledWith("web");
    // A volume's chip and a network's key: that one, in its section.
    await fireEvent.click(container.querySelector(".vchip") as HTMLElement);
    expect(given.onsection).toHaveBeenCalledWith("volumes", HOSTILE);
    await fireEvent.click(container.querySelector(".netkey") as HTMLElement);
    expect(given.onsection).toHaveBeenCalledWith("networks", HOSTILE);
  });

  it("draws each container's image above it, its shared layer lit in every strip", async () => {
    const stored = [
      { id: "sha256:web", tags: ["web:1"], created: 0, layers: [{ diff: "sha256:base", size: 1e6, created_by: "base" }, { diff: "sha256:w", size: 1e6, created_by: "w" }] },
      { id: "sha256:db", tags: ["db:1"], created: 0, layers: [{ diff: "sha256:base", size: 1e6, created_by: "base" }] },
    ];
    const given = props();
    const containers = new globalThis.Map<string, Container>([
      ["web", { Id: "web", Image: "web:1" }],
      ["db", { Id: "db", Image: "db:1" }],
    ]);
    const images = imagesModel(stored, containers);
    const { container } = render(Map, { props: { ...given, images, lit: "sha256:base", onimage: vi.fn() } });
    const rows = [...container.querySelectorAll(".strata-row")];
    expect(rows.map((row) => row.getAttribute("title")).filter(Boolean)).toEqual(["web:1 · 2 layers", "db:1 · 1 layers"]);
    // The base lit in both strips, the rest dimmed.
    expect([...container.querySelectorAll('[data-layer="sha256:base"]')].every((layer) => layer.classList.contains("lit"))).toBe(true);
    expect(container.querySelectorAll('[data-layer="sha256:base"]')).toHaveLength(2);
    expect(container.querySelector('[data-layer="sha256:w"]')?.classList.contains("dim")).toBe(true);
  });
});
