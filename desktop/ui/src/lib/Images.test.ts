import { cleanup, fireEvent, render } from "@testing-library/svelte";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Container, StoredImage } from "./backend";
import ImageInspector from "./ImageInspector.svelte";
import Images from "./Images.svelte";
import { imagesModel } from "./layers";

afterEach(cleanup);

const HOSTILE = '<img src=x onerror="window.pwned=1">';
const NOW = Date.parse("2026-10-28T12:00:00Z");
const stored: StoredImage[] = [
  { id: "sha256:aaaa", tags: ["app:1"], created: NOW / 1000 - 86_400, layers: [{ diff: "sha256:base", size: 8e6, created_by: HOSTILE }, { diff: "sha256:app", size: 3e7, created_by: "COPY app /" }] },
  { id: "sha256:bbbb", tags: ["base:1", "base:latest"], created: 0, layers: [{ diff: "sha256:base", size: 8e6, created_by: HOSTILE }] },
];
const containers = new Map<string, Container>([["c1", { Id: "c1", Image: "app:1" }]]);
const model = imagesModel(stored, containers);
const who = new Map([["c1", { name: "web", state: "running" as const }]]);

function inspector(index: number) {
  const props = { image: model.images[index]!, model, who, lit: null, now: NOW, onrun: vi.fn(), onremove: vi.fn(), ongoto: vi.fn() };
  return { ...render(ImageInspector, { props }), props };
}

describe("an image's inspector", () => {
  it("says its layers as text, and keeps Remove while a container uses it", () => {
    const { container, getByRole } = inspector(0);
    expect(container.querySelector("img")).toBeNull();
    expect(container.textContent).toContain(HOSTILE);
    const remove = getByRole("button", { name: "Remove" }) as HTMLButtonElement;
    expect(remove.disabled).toBe(true);
    expect(remove.title).toBe("In use by web");
    expect(container.textContent).toContain("shared with 1 other image");
  });

  it("runs it, and removes an unused one after its confirm, by its tags, never forced", async () => {
    const { getByRole, container, props } = inspector(1);
    await fireEvent.click(getByRole("button", { name: "Run" }));
    expect(props.onrun).toHaveBeenCalledWith("base:1");
    const remove = getByRole("button", { name: "Remove" });
    expect(remove.getAttribute("title")).toBe("shipyard image rm base:1 base:latest");
    await fireEvent.click(remove);
    expect(container.querySelector(".confirm")?.textContent).toContain("Remove base:1? Frees 0 B.");
    await fireEvent.click(getByRole("button", { name: "Remove" }));
    expect(props.onremove).toHaveBeenCalledWith(model.images[1]);
  });

  it("can't run an image with no name to run it by", () => {
    const untagged = imagesModel([{ id: "sha256:cccc", tags: [], created: 0, layers: [] }], new Map());
    const props = { image: untagged.images[0]!, model: untagged, who, lit: null, now: NOW, onrun: vi.fn(), onremove: vi.fn(), ongoto: vi.fn() };
    const { getByRole } = render(ImageInspector, { props });
    const run = getByRole("button", { name: "Run" }) as HTMLButtonElement;
    expect(run.disabled).toBe(true);
    expect(run.title).toBe("It has no name to run it by");
  });
});

describe("the Images section", () => {
  it("says what's on disk, prunes after its confirm, and captions a lit layer", async () => {
    const onremoveall = vi.fn(async () => true);
    const props = { model, who, selected: null, lit: null as string | null, now: NOW, onselect: vi.fn(), ongoto: vi.fn(), onremoveall };
    const { container, getByRole, rerender } = render(Images, { props });
    expect(container.querySelector(".secsum")?.textContent).toContain("38 MB on disk for 2 images. Shared layers save 8.0 MB.");
    await fireEvent.click(getByRole("button", { name: "Remove 1 unused" }));
    expect(container.querySelector(".secsum")?.textContent).toContain("Remove base:1? This frees 0 B. Shared layers stay.");
    expect(onremoveall).not.toHaveBeenCalled();
    await fireEvent.click(getByRole("button", { name: "Remove" }));
    expect(onremoveall).toHaveBeenCalled();
    await rerender({ ...props, lit: "sha256:base" });
    const caption = container.querySelector("#layercap")?.textContent ?? "";
    expect(caption).toContain(HOSTILE);
    expect(caption).toContain("shared by app:1 and base:1, stored once");
    expect(container.querySelector("img")).toBeNull();
  });

  it("removes only what its confirm listed as it opened", async () => {
    const onremoveall = vi.fn(async () => false);
    const props = { model, who, selected: null, lit: null as string | null, now: NOW, onselect: vi.fn(), ongoto: vi.fn(), onremoveall };
    const { getByRole, rerender } = render(Images, { props });
    await fireEvent.click(getByRole("button", { name: "Remove 1 unused" }));
    // An image pulled meanwhile, used by no container.
    const pulled = { id: "sha256:dddd", tags: ["pulled:1"], created: 0, layers: [] };
    await rerender({ ...props, model: imagesModel([...stored, pulled], containers) });
    await fireEvent.click(getByRole("button", { name: "Remove" }));
    expect(onremoveall).toHaveBeenCalledWith([model.images[1]]);
  });
});
