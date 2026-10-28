import { cleanup, fireEvent, render } from "@testing-library/svelte";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Container, Links, Network, NetworkView, VolumesAnswer } from "./backend";
import { mapModel } from "./graph";
import NetworkInspector from "./NetworkInspector.svelte";
import Networks from "./Networks.svelte";
import { fillOf, networksModel, volumesModel } from "./storage";
import VolumeInspector from "./VolumeInspector.svelte";
import Volumes from "./Volumes.svelte";

afterEach(cleanup);

const HOSTILE = '<img src=x onerror="window.pwned=1">';
const NOW = Date.parse("2026-10-28T12:00:00Z");

const node = (id: string, state: string) => ({
  id,
  name: id,
  serial: "SHPU 000001 0",
  memory_max: null,
  started_at: null,
  project: "p",
  state,
  exit_code: null,
});

/** web (running, on front and back, publishing 8080, mounting `data`
 * read-only) and db (stopped, on back, mounting `data` read-write and
 * `HOSTILE`); `spare` mounted by none. */
const links: Links = {
  version: 1,
  containers: [node("web", "running"), node("db", "exited")],
  links: [],
  entries: [
    {
      container: "web",
      host_ip: "0.0.0.0",
      host_port: 8080,
      container_port: 80,
      protocol: "tcp",
      connections: 0,
    },
  ],
  observed: { available: true, cause: null, reason: null },
};
const containers = new Map<string, Container>([
  [
    "web",
    {
      Id: "web",
      Image: "web:1",
      NetworkSettings: {
        Networks: {
          p_front: { IPAddress: "10.1.0.2" },
          p_back: { IPAddress: "10.2.0.2" },
        },
      },
      Mounts: [
        {
          Type: "volume",
          Name: "data",
          Destination: "/srv",
          RW: false,
        },
      ],
    },
  ],
  [
    "db",
    {
      Id: "db",
      Image: "db:1",
      NetworkSettings: {
        Networks: { p_back: { IPAddress: "10.2.0.3" } },
      },
      Mounts: [
        {
          Type: "volume",
          Name: "data",
          Destination: "/var/lib/db",
          RW: true,
        },
        { Type: "volume", Name: HOSTILE, Destination: "/x", RW: true },
      ],
    },
  ],
]);
const networks: Network[] = [
  {
    name: "p_front",
    short: "front",
    subnet: "10.1.0.0/24",
    gateway: "10.1.0.1",
    internal: false,
    driver: "bridge",
  },
  {
    name: "p_back",
    short: "back",
    subnet: "10.2.0.0/24",
    gateway: "10.2.0.1",
    internal: true,
    driver: "bridge",
  },
  {
    name: "host",
    short: "host",
    subnet: null,
    gateway: null,
    internal: false,
    driver: "host",
  },
];
const map = mapModel(links, containers, networks);
const answer: VolumesAnswer = {
  version: 1,
  volumes: [
    {
      name: "data",
      created: new Date(NOW - 3 * 86_400_000).toISOString(),
      size: 400e6,
      complete: true,
      files: [
        { name: "db", kind: "directory", size: 4096 },
        { name: HOSTILE, kind: "file", size: 9 },
      ],
      more: 2,
      mountpoint: "/v/data/_data",
    },
    {
      name: HOSTILE,
      created: "",
      size: 1e6,
      complete: false,
      files: [],
      more: 0,
      mountpoint: "/v/x/_data",
    },
    {
      name: "spare",
      created: "",
      size: 100e6,
      complete: true,
      files: [],
      more: 0,
      mountpoint: "/v/spare/_data",
    },
  ],
  warnings: [],
};
const view = (name: string, veths: Record<string, string>): NetworkView => ({
  name,
  bridge: `sy-${name}`,
  gateway: "10.9.9.1",
  internal: false,
  dns_running: true,
  endpoints: Object.entries(veths).map(([id, veth]) => ({ container: id, address: `10.9.9.${id.length}`, host_veth: veth })),
});

describe("the volumes' model", () => {
  it("counts every container that names a volume as its user, running or not, and what no one mounts", () => {
    const model = volumesModel(answer, map.containers);
    const data = model.volumes[0]!;
    expect(data.users.map((user) => [user.node.id, user.path, user.mode])).toEqual([
      ["web", "/srv", "ro"],
      ["db", "/var/lib/db", "rw"],
    ]);
    // db is stopped, and still names HOSTILE: it isn't unused.
    expect(model.unused.map((volume) => volume.name)).toEqual(["spare"]);
    expect([model.total, model.unusedSize, model.largest]).toEqual([501, 100, 400]);
    expect([fillOf(100, 400), fillOf(800, 400), fillOf(1, 0)]).toEqual([0.25, 1, 0]);
  });
});

describe("the networks' model", () => {
  it("draws the bridges alone, each container's tap with its veth, the other network it's on and its host port", () => {
    const model = networksModel(networks, map, new Map([["p_back", view("back", { db: "shy1234" })]]));
    expect(model.bands.map((band) => band.full)).toEqual(["p_front", "p_back"]);
    // web on two, db on one: three endpoints.
    expect(model.endpoints).toBe(3);
    const [front, back] = model.bands;
    // No view: the list's gateway, no bridge; a view: its own.
    expect([front!.gateway, front!.bridge, front!.dns]).toEqual(["10.1.0.1", null, null]);
    expect([back!.gateway, back!.bridge, back!.dns]).toEqual(["10.9.9.1", "sy-back", "running"]);
    const tap = (band: typeof front, id: string) => band!.taps.find((t) => t.node.id === id)!;
    // The live lease's address first (the list's can lag a start); the list's without a view.
    expect([tap(back, "db").address, tap(back, "db").veth, tap(back, "db").also]).toEqual(["10.9.9.2", "shy1234", null]);
    expect([tap(back, "web").address, tap(front, "web").address]).toEqual(["10.2.0.2", "10.1.0.2"]);
    expect(tap(back, "web").also?.full).toBe("p_front");
    expect(tap(front, "web").hostPort).toBe(8080);
    expect(back!.bridging.map((b) => [b.node.id, b.others])).toEqual([["web", ["front"]]]);
  });

  it("names no stopped container a path between networks, and says the default network has no DNS", () => {
    const stopped = mapModel({ ...links, containers: [node("web", "exited"), node("db", "exited")] }, containers, networks);
    expect(networksModel(networks, stopped, new Map()).bands[1]!.bridging).toEqual([]);
    const plain: Network = { name: "bridge", short: "bridge", subnet: "10.88.0.0/16", gateway: "10.88.0.1", internal: false, driver: "bridge" };
    const model = networksModel([plain], map, new Map([["bridge", { ...view("bridge", {}), dns_running: false }]]));
    expect(model.bands[0]!.dns).toBe("none");
    const own = networksModel(networks, map, new Map([["p_front", { ...view("front", {}), dns_running: false }]]));
    expect(own.bands[0]!.dns).toBe("stopped");
  });
});

describe("the Volumes section", () => {
  it("lists every volume as text, and deletes the unused ones it listed after its confirm", async () => {
    const onremoveall = vi.fn(async () => true);
    const model = volumesModel(answer, map.containers);
    const { container, getByRole } = render(Volumes, {
      props: {
        model,
        selected: "data",
        now: NOW,
        onselect: vi.fn(),
        ongoto: vi.fn(),
        onremoveall,
      },
    });
    expect(container.querySelector("img")).toBeNull();
    expect(container.textContent).toContain(HOSTILE);
    expect(container.querySelector(".secsum")?.textContent).toContain("501 MB in 3 volumes. 100 MB isn't mounted by any container.");
    // Not walked whole: at least.
    expect(container.querySelectorAll(".vrow .num")[1]?.textContent).toBe("≥ 1.0 MB");
    await fireEvent.click(getByRole("button", { name: "Delete 1 unused" }));
    const yes = getByRole("button", { name: "Delete" });
    expect(yes.getAttribute("title")).toBe("shipyard volume rm spare");
    await fireEvent.click(yes);
    expect(onremoveall).toHaveBeenCalledWith(["spare"]);
  });

  it("deletes only what its confirm listed as it opened, says at least for a volume not walked whole, and waits while a deletion runs", async () => {
    const onremoveall = vi.fn(async () => false);
    const props = { model: volumesModel(answer, map.containers), selected: null, now: NOW, onselect: vi.fn(), ongoto: vi.fn(), onremoveall };
    const { container, getByRole, rerender } = render(Volumes, { props });
    await fireEvent.click(getByRole("button", { name: "Delete 1 unused" }));
    // A volume made meanwhile, not walked yet (compose's, before its container).
    const made = { name: "made", created: "", size: 0, complete: false, files: [], more: 0, mountpoint: "/v/made/_data" };
    await rerender({ ...props, model: volumesModel({ ...answer, volumes: [...answer.volumes, made] }, map.containers) });
    expect(container.querySelector(".confirming")?.textContent).toContain("Delete spare and all 100 MB of data in it?");
    await fireEvent.click(getByRole("button", { name: "Delete" }));
    expect(onremoveall).toHaveBeenCalledWith(["spare"]);
    // Refused: still open. Cancelled, the line says the new one, at least.
    await fireEvent.click(getByRole("button", { name: "Cancel" }));
    expect(container.querySelector(".secsum")?.textContent).toContain("at least 100 MB isn't mounted by any container.");
    await rerender({ ...props, model: volumesModel({ ...answer, volumes: [...answer.volumes, made] }, map.containers), busy: true });
    expect((getByRole("button", { name: "Delete 2 unused" }) as HTMLButtonElement).disabled).toBe(true);
  });
});

describe("a volume's inspector", () => {
  const inspector = (index: number) => {
    const model = volumesModel(answer, map.containers);
    const props = {
      volume: model.volumes[index]!,
      largest: model.largest,
      now: NOW,
      mac: true,
      ondelete: vi.fn(),
      ongoto: vi.fn(),
    };
    return { ...render(VolumeInspector, { props }), props };
  };

  it("keeps Delete while a container names it, says who can write, and its files as text", () => {
    const { container, getByRole } = inspector(0);
    const remove = getByRole("button", {
      name: "Delete",
    }) as HTMLButtonElement;
    expect(remove.disabled).toBe(true);
    expect(remove.title).toBe("Mounted by web and db");
    expect(container.textContent).toContain("Shared by 2 containers; only one can write.");
    expect(container.querySelector(".files")?.textContent).toBe(`data/\n├── db/\n├── ${HOSTILE}\n└── … and 2 more`);
    expect(container.querySelector("img")).toBeNull();
    expect(container.querySelector(".kv")?.textContent).toContain("Path in VM/v/data/_data");
  });

  it("deletes an unused one after its confirm, by name", async () => {
    const { container, getByRole, props } = inspector(2);
    const remove = getByRole("button", { name: "Delete" });
    expect(remove.getAttribute("title")).toBe("shipyard volume rm spare");
    await fireEvent.click(remove);
    expect(container.querySelector(".confirm")?.textContent).toContain("Delete spare and its 100 MB of data?");
    await fireEvent.click(getByRole("button", { name: "Delete" }));
    expect(props.ondelete).toHaveBeenCalledWith("spare");
  });
});

describe("the Networks section", () => {
  it("draws a band a bridge, its way out by whether it's internal, and selects one", async () => {
    const model = networksModel(networks, map, new Map());
    const onselect = vi.fn();
    const { container } = render(Networks, {
      props: { model, selected: null, onselect, ongoto: vi.fn() },
    });
    const bands = [...container.querySelectorAll(".netband")];
    expect(bands.map((band) => band.querySelector(".egress")?.textContent)).toEqual(["NAT → internet", "⊘ no route out"]);
    // web's tag: front's band says its host port, back's the other network.
    // web is on both: each band's tag names the other.
    expect(bands.map((band) => band.querySelector(".tag")?.textContent)).toEqual(["also back", "also front"]);
    await fireEvent.click(bands[1]!.querySelector(".nethead")!);
    expect(onselect).toHaveBeenCalledWith("p_back");
  });

  it("tags a container on one network with its host port, only where there's a way out", () => {
    const alone = new Map(containers);
    alone.set("web", { ...containers.get("web")!, NetworkSettings: { Networks: { p_front: { IPAddress: "10.1.0.2" } } } });
    const tags = (front: Network) => {
      const model = networksModel([front], mapModel(links, alone, [front]), new Map());
      const { container } = render(Networks, { props: { model, selected: null, onselect: vi.fn(), ongoto: vi.fn() } });
      const tag = container.querySelector(".tag")?.textContent;
      cleanup();
      return tag;
    };
    expect(tags(networks[0]!)).toBe("host :8080");
    expect(tags({ ...networks[0]!, internal: true })).toBe("");
  });

  it("says rootless has no bridges", () => {
    const model = networksModel([networks[2]!], map, new Map());
    const { container } = render(Networks, {
      props: {
        model,
        selected: null,
        onselect: vi.fn(),
        ongoto: vi.fn(),
      },
    });
    expect(container.querySelector(".secsum")?.textContent).toContain("No bridge networks here");
  });
});

describe("a network's inspector", () => {
  it("names each endpoint's veth and the container that joins it to another", () => {
    const model = networksModel(networks, map, new Map([["p_back", view("back", { db: "shy1234", web: "shy5678" })]]));
    const { container } = render(NetworkInspector, {
      props: { band: model.bands[1]!, ongoto: vi.fn() },
    });
    expect([...container.querySelectorAll(".attv em")].map((em) => em.textContent)).toEqual(["10.9.9.3 · shy5678", "10.9.9.2 · shy1234"]);
    expect(container.textContent).toContain("web is also on front, so it's the only path between back and front.");
    expect(container.textContent).toContain("none (no masquerade rule)");
  });
});
