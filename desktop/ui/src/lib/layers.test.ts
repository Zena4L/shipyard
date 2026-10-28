import { describe, expect, it } from "vitest";
import type { Container, StoredImage } from "./backend";
import { FAMILY_COLORS, agoSeconds, imagesModel, refOf } from "./layers";

const MB = 1_000_000;
const image = (tag: string | null, layers: [string, number][], id = `sha256:${tag ?? "x"}`): StoredImage => ({
  id,
  tags: tag ? [tag] : [],
  created: 0,
  layers: layers.map(([diff, mb]) => ({ diff, size: mb * MB, created_by: `make ${diff}` })),
});

/** alpine (one layer), nginx and redis on it (redis and postgres share an
 * unrelated top layer), postgres on debian, tool alone. */
const STORE = [
  image("nginx:1", [["alpine", 8], ["nginx", 30]]),
  image("postgres:16", [["debian", 90], ["pg", 300], ["entry", 1]]),
  image("redis:7", [["alpine", 8], ["redis", 20], ["entry", 1]]),
  image("python:3", [["debian", 90], ["py", 40]]),
  image("alpine:3.20", [["alpine", 8]]),
  image(null, [["tool", 5]], "sha256:0123456789abcdef0123"),
];

describe("the images' model", () => {
  it("makes families by bottom layer, coloured in the order they first appear, named by their stored base", () => {
    const model = imagesModel(STORE, new Map());
    expect(model.families).toEqual([
      { color: FAMILY_COLORS[0], name: "alpine:3.20 base", base: 1 },
      { color: FAMILY_COLORS[1], name: "shared base, 1 layer", base: 1 },
    ]);
    const nginx = model.images[0]!;
    expect(nginx.layers.map((l) => l.family)).toEqual([0, null]);
    // A top layer two images of other families share isn't a base: faint.
    const redis = model.images[2]!;
    expect(redis.layers.map((l) => l.family)).toEqual([0, null, null]);
    // An image of no family: every layer its own.
    expect(model.images[5]!.layers.map((l) => l.family)).toEqual([null]);
  });

  it("counts each layer once on disk, an image's own its unshared layers, and who uses it", () => {
    const containers = new Map<string, Container>([
      ["c1", { Id: "c1", Image: "nginx:1", ImageID: "sha256:nginx:1" }],
      ["c2", { Id: "c2", Image: "redis:7" }],
    ]);
    const model = imagesModel(STORE, containers);
    expect(model.naive).toBe(8 + 30 + 90 + 300 + 1 + 8 + 20 + 1 + 90 + 40 + 8 + 5);
    expect(model.unique).toBe(8 + 30 + 90 + 300 + 1 + 20 + 40 + 5);
    const [nginx, postgres, redis] = model.images;
    expect(nginx!.own).toBe(30);
    // `entry` is shared with redis: not postgres's own.
    expect(postgres!.own).toBe(300);
    expect(model.sharers.get("entry")?.map((i) => i.ref)).toEqual(["postgres:16", "redis:7"]);
    expect(nginx!.users).toEqual(["c1"]);
    expect(redis!.users).toEqual(["c2"]);
    expect(model.images[3]!.users).toEqual([]);
    expect(model.byContainer.get("c2")).toBe(redis);
    // Pruning frees what no used image holds, each layer once: postgres's
    // and python's `debian` (shared by the two unused), never `alpine`
    // (nginx holds it) nor `entry` (redis does).
    expect(model.unused.map((i) => i.ref)).toEqual(["postgres:16", "python:3", "alpine:3.20", "0123456789ab"]);
    expect(model.unusedFrees).toBe(90 + 300 + 40 + 5);
  });

  it("counts a container as its image's by ID, by tag only when its record has none", () => {
    // `app:1` moved to a new image since the container was made.
    const moved = [image("app:1", [["new", 1]], "sha256:new"), image(null, [["old", 1]], "sha256:old")];
    const containers = new Map<string, Container>([
      ["runs-old", { Id: "runs-old", Image: "app:1", ImageID: "sha256:old" }],
      ["no-id", { Id: "no-id", Image: "app:1" }],
    ]);
    const model = imagesModel(moved, containers);
    expect(model.images[0]!.users).toEqual(["no-id"]);
    expect(model.images[1]!.users).toEqual(["runs-old"]);
    expect(model.unused).toEqual([]);
  });

  it("names an untagged image by its short ID, and says an age as the mock does", () => {
    expect(refOf(STORE[5]!)).toBe("0123456789ab");
    const now = Date.parse("2026-10-28T12:00:00Z");
    const at = (seconds: number) => agoSeconds(now / 1000 - seconds, now);
    // The client's 0: not known.
    expect(agoSeconds(0, now)).toBe("a while ago");
    expect([at(10), at(300), at(7200), at(2 * 86_400), at(21 * 86_400)]).toEqual([
      "just now",
      "5 minutes ago",
      "2 hours ago",
      "2 days ago",
      "3 weeks ago",
    ]);
  });
});
