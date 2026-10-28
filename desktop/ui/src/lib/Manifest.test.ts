import { render } from "@testing-library/svelte";
import { describe, expect, it } from "vitest";
import Manifest from "./Manifest.svelte";
import { fmtSize, type Row } from "./model";

it("shows what a container named as text, never as markup", () => {
  const hostile = '<img src=x onerror="window.pwned=1">';
  const row: Row = {
    id: "a".repeat(64),
    owner: "SHPU",
    serial: "000001",
    check: "0",
    name: hostile,
    stack: "standalone",
    image: "<b>x</b>",
    state: "running",
    exit: null,
    cpu: null,
    mem: null,
    memMax: null,
    ports: "<i>80</i>",
    up: "1m",
  };
  const { container } = render(Manifest, { props: { rows: [row], selected: null, onselect: () => {} } });
  expect(container.querySelector("img")).toBeNull();
  expect(container.querySelector("tbody b")?.textContent).toBe(hostile);
  expect(container.querySelector("td.mono b")).toBeNull();
  expect(container.textContent).toContain("<i>80</i>");
  expect((window as unknown as { pwned?: number }).pwned).toBeUndefined();
});

describe("fmtSize", () => {
  it("says a size as the mock does, at each of its steps", () => {
    expect([2048, 41.61, 4.16, 0.5, 0.0001, 0].map(fmtSize)).toEqual([
      "2.00 GB",
      "42 MB",
      "4.2 MB",
      "512 KB",
      "1 KB",
      "0 B",
    ]);
  });
});
