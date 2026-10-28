// The Images section's model (step 14.6a): the mock's `imgSize`, `sharers`,
// `usersOfImage`, `diskTotals` and its layer families
// (docs/design/shipyard-ui.html), as plain functions over the API's images
// and containers. A layer is its digest: the same digest in two images is
// one layer, stored once. A family is the images that share their bottom
// layer (D14.6-3); the base they all share (their common bottom layers) is
// coloured as it, the rest are each image's own, even a layer another
// family's image happens to have too (the caption says who shares it).
import type { Container, StoredImage } from "./backend";
import { MB } from "./model";

/** A family's colours: the mock's three, then the app's own, in the order
 * families first appear. */
export const FAMILY_COLORS = ["var(--lay-alpine)", "var(--lay-distroless)", "var(--lay-bookworm)", "var(--lay-d)", "var(--lay-e)", "var(--lay-f)"];

/** A layer, as drawn. */
export interface LayerView {
  diff: string;
  /** MB. */
  size: number;
  createdBy: string;
  /** Its family's index, if it's in the base its family shares. */
  family: number | null;
  /** How it's drawn: its family's colour, else faint (an image's own). */
  color: string;
  faint: boolean;
}

/** What an image's own layer is drawn with. */
export const OWN_COLOR = "var(--faint)";

/** The step a layer's history doesn't say. */
export const NO_STEP = "(no step recorded)";

/** An image, as drawn. */
export interface ImageView {
  id: string;
  /** Its first tag, or its short ID. */
  ref: string;
  tags: string[];
  created: number;
  layers: LayerView[];
  /** MB, every layer. */
  size: number;
  /** MB, the layers no other image has: what removing it frees. */
  own: number;
  /** The containers that use it (running or not), by ID. */
  users: string[];
}

/** A family of images. */
export interface Family {
  color: string;
  /** "alpine:3.20 base", or "shared base, N layers". */
  name: string;
  /** How many bottom layers its members all share. */
  base: number;
}

/** The section's model. */
export interface ImagesModel {
  images: ImageView[];
  /** The images no container uses, and what removing them all frees (MB:
   * the layers no used image holds, each once). */
  unused: ImageView[];
  unusedFrees: number;
  /** Each container's image, by the container's ID. */
  byContainer: Map<string, ImageView>;
  families: Family[];
  /** The images that have each layer. */
  sharers: Map<string, ImageView[]>;
  /** MB on disk: each layer once. */
  unique: number;
  /** MB if no layer were shared. */
  naive: number;
}

/** An image's name: its first tag, else its short ID. */
export function refOf(image: Pick<StoredImage, "id" | "tags">): string {
  return image.tags[0] ?? image.id.replace(/^sha256:/, "").slice(0, 12);
}

/** The model of `stored` (the API's order), its users from `containers`. */
export function imagesModel(stored: StoredImage[], containers: Map<string, Container>): ImagesModel {
  const holders = new Map<string, number>();
  for (const image of stored) for (const diff of new Set(image.layers.map((l) => l.diff))) holders.set(diff, (holders.get(diff) ?? 0) + 1);
  // Families: by bottom layer, of two images or more, in order of first
  // appearance.
  const byBase = new Map<string, StoredImage[]>();
  for (const image of stored) {
    const bottom = image.layers[0]?.diff;
    if (bottom !== undefined) byBase.set(bottom, [...(byBase.get(bottom) ?? []), image]);
  }
  const families: Family[] = [];
  const familyOf = new Map<string, number>();
  for (const [bottom, members] of byBase) {
    if (members.length < 2) continue;
    familyOf.set(bottom, families.length);
    const base = sharedBase(members);
    families.push({
      color: FAMILY_COLORS[families.length % FAMILY_COLORS.length] ?? "var(--faint)",
      name: familyName(members, base),
      base,
    });
  }
  // The engine's own rule (its `remove_unused`): a container uses the image
  // its record names by ID; by its tag only when it names none (a record
  // older than the ID). A tag moved since names another image.
  const users = (image: StoredImage) =>
    [...containers.values()]
      .filter((c) => (c.ImageID ? c.ImageID === image.id : c.Image !== undefined && image.tags.includes(c.Image)))
      .map((c) => c.Id);
  const images = stored.map((image): ImageView => {
    const family = familyOf.get(image.layers[0]?.diff ?? "") ?? null;
    const shared = family === null ? 0 : (families[family]?.base ?? 0);
    const layers = image.layers.map((l, i) => {
      const of = i < shared ? family : null;
      const color = of === null ? undefined : families[of]?.color;
      return { diff: l.diff, size: l.size / MB, createdBy: l.created_by, family: of, color: color ?? OWN_COLOR, faint: color === undefined };
    });
    return {
      id: image.id,
      ref: refOf(image),
      tags: image.tags,
      created: image.created,
      layers,
      size: layers.reduce((sum, l) => sum + l.size, 0),
      own: layers.filter((l) => (holders.get(l.diff) ?? 0) <= 1).reduce((sum, l) => sum + l.size, 0),
      users: users(image),
    };
  });
  const sharers = new Map<string, ImageView[]>();
  const sizes = new Map<string, number>();
  let naive = 0;
  for (const image of images) {
    for (const l of image.layers) {
      naive += l.size;
      sizes.set(l.diff, l.size);
      const had = sharers.get(l.diff) ?? [];
      if (!had.includes(image)) sharers.set(l.diff, [...had, image]);
    }
  }
  const unique = [...sizes.values()].reduce((sum, size) => sum + size, 0);
  const unused = images.filter((image) => !image.users.length);
  const held = new Set(images.filter((image) => image.users.length).flatMap((image) => image.layers.map((l) => l.diff)));
  const freed = new Map(unused.flatMap((image) => image.layers.filter((l) => !held.has(l.diff)).map((l) => [l.diff, l.size] as const)));
  const unusedFrees = [...freed.values()].reduce((sum, size) => sum + size, 0);
  const byContainer = new Map(images.flatMap((image) => image.users.map((id) => [id, image] as const)));
  return { images, unused, unusedFrees, byContainer, families, sharers, unique, naive };
}

/** How many bottom layers `members` all share. */
function sharedBase(members: StoredImage[]): number {
  const first = members[0]?.layers ?? [];
  let common = first.length;
  for (const member of members) {
    let i = 0;
    while (i < common && member.layers[i]?.diff === first[i]?.diff) i++;
    common = i;
  }
  return common;
}

/** A family's name: the member whose layers are exactly the base they all
 * share (`alpine:3.20 base`), else the base's size in layers. */
function familyName(members: StoredImage[], common: number): string {
  const exact = members.find((m) => m.layers.length === common && m.tags.length);
  return exact ? `${refOf(exact)} base` : `shared base, ${common} layer${common === 1 ? "" : "s"}`;
}

/** An RFC 3339 time ago from `now` (ms); one that doesn't parse, "a while
 * ago". */
export function agoIso(time: string, now: number): string {
  return agoSeconds(Date.parse(time) / 1000 || 0, now);
}

/** `seconds` ago from `now` (ms), as the mock says it ("3 weeks ago"). */
export function agoSeconds(seconds: number, now: number): string {
  if (!seconds) return "a while ago";
  const s = Math.max(0, now / 1000 - seconds);
  const say = (n: number, unit: string) => `${n} ${unit}${n === 1 ? "" : "s"} ago`;
  if (s < 60) return "just now";
  if (s < 3600) return say(Math.floor(s / 60), "minute");
  if (s < 86_400) return say(Math.floor(s / 3600), "hour");
  if (s < 14 * 86_400) return say(Math.floor(s / 86_400), "day");
  if (s < 60 * 86_400) return say(Math.floor(s / (7 * 86_400)), "week");
  if (s < 365 * 86_400) return say(Math.floor(s / (30 * 86_400)), "month");
  return say(Math.floor(s / (365 * 86_400)), "year");
}
