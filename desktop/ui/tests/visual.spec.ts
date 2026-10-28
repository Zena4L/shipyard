// The app is the design's mock, pixel for pixel (step 14.2): the mock
// (docs/design/shipyard-ui.html) and the app fed the mock's own data
// (preview.html), rendered by the same engine at the same size, compared
// pixel by pixel, light and dark. Both are given the same fonts (the
// mock's Google Fonts are served from the app's bundled copies) and the
// mock's clock and dice are fixed (no ticks, Math.random at 0.5), so a
// difference is the app's. Masked, each where it's compared: what the app
// draws otherwise on purpose (api's Remove, disabled while web needs it;
// the out-of-memory alert's sentence, and worker's log, the mock's own
// words and markup; the Images rows' ages, no one rule in the mock, and
// its legend's names, D14.6-3); the mock's painted traffic lights (the
// app's are the window's own) and its window's rounded corners (the
// system's).
import { expect, test, type Page } from "@playwright/test";
import { existsSync, readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { fileURLToPath } from "node:url";
import pixelmatch from "pixelmatch";
import { PNG } from "pngjs";

const UI = fileURLToPath(new URL("..", import.meta.url));
// In standards mode, as its published artifact renders it (the file has no
// doctype, and quirks mode resets a table's line height).
const MOCK = `<!doctype html>\n${readFileSync(`${UI}/../../docs/design/shipyard-ui.html`, "utf8")}`;

/** The bundled fonts' stylesheets for a Google Fonts request (`url`): each
 * family it names, at the weights it asks for alone, as Google serves them
 * (a weight not asked for is the browser's to make up, in the mock as in
 * the app). Their files are served by the dev server, by absolute URL (the
 * sheet is served as Google's, so a relative URL would be too). */
function fonts(url: string): string {
  const asked = new URL(url).searchParams.getAll("family");
  return asked
    .flatMap((spec) => {
      const [name = "", axes = ""] = spec.split(":");
      const family = name.toLowerCase().replaceAll("+", "-").replaceAll(" ", "-");
      const weights = axes
        .replace(/^wght@/, "")
        .split(";")
        .filter(Boolean);
      return weights
        .filter((weight) => existsSync(`${UI}/node_modules/@fontsource/${family}/${weight}.css`))
        .map((weight) =>
          readFileSync(`${UI}/node_modules/@fontsource/${family}/${weight}.css`, "utf8").replaceAll(
            "./files/",
            `http://127.0.0.1:1420/node_modules/@fontsource/${family}/files/`,
          ),
        );
    })
    .join("\n");
}

type Box = { x: number; y: number; width: number; height: number };

/** The views compared: the mock's Manifest, its Map (its default, its
 * Images overlay on), its Yard, the Run sheet open over the map, and its
 * Images, Volumes (`uploads` selected) and Networks (`backend`) sections. */
type View = "manifest" | "map" | "yard" | "run" | "images" | "volumes" | "networks";

/** The mock, `view` shown, `chosen` selected (by its image), at its
 * window's place: that box. */
async function openMock(page: Page, view: View, chosen: string): Promise<Box> {
  await page.addInitScript(() => {
    Math.random = () => 0.5;
    window.setInterval = (() => 0) as unknown as typeof window.setInterval;
  });
  await page.route(/fonts\.googleapis\.com/, (route) =>
    route.fulfill({
      contentType: "text/css",
      body: fonts(route.request().url()),
    }),
  );
  await page.route("**/__mock__.html", (route) => route.fulfill({ contentType: "text/html", body: MOCK }));
  await page.goto("http://127.0.0.1:1420/__mock__.html");
  await page.evaluate(() => document.fonts.ready);
  if (view === "manifest") await page.click("#vList");
  else if (view === "yard") await page.click("#vYard");
  else if (view === "images" || view === "volumes" || view === "networks") await page.click(`.nav button[data-section="${view}"]`);
  if (view === "run") {
    // Opened, its image field focused and selected: the app's is too, and
    // both are blurred (a focus ring and a selection are the browser's).
    await page.click("#runBtn");
    await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
  }
  // Clicked without a hover: the mock measures its crane in the click, and
  // a hovered box is lifted 2 px (the hook would end short of it).
  if (chosen !== API) await page.locator(".yard .box", { hasText: chosen }).evaluate((box) => (box as HTMLElement).click());
  await page.mouse.move(0, 0);
  await settled(page);
  // The caption above the window is a fraction of a pixel tall: the page
  // moved down to the next whole pixel, so the window is drawn on the same
  // pixel grid as the app's.
  await page.evaluate(() => {
    const y = document.querySelector(".win")!.getBoundingClientRect().y;
    const body = document.body;
    body.style.paddingTop = `${parseFloat(getComputedStyle(body).paddingTop) + Math.ceil(y) - y}px`;
  });
  const box = await page.locator(".win").boundingBox();
  if (!box) throw new Error("the mock has no window");
  return box;
}

/** Once every transition under way has ended (the crane moving to a
 * selection: the mock's own run whatever motion is asked for). */
async function settled(page: Page) {
  await page.evaluate(() => Promise.all(document.getAnimations().map((animation) => animation.finished)));
}

/** The mock's selection: `api`, by its image. */
const API = "ghcr.io/acme/api";

/** The app with the mock's data, `view` shown, `chosen` selected. */
async function openApp(page: Page, size: { width: number; height: number }, view: View, chosen: string) {
  await page.setViewportSize(size);
  await page.goto("http://127.0.0.1:1420/preview.html");
  await page.evaluate(() => document.fonts.ready);
  if (view === "map" || view === "run") {
    await page.locator(".mnode .box", { hasText: chosen }).click();
    if (view === "run") await page.getByRole("button", { name: "Run image" }).click();
  } else if (view === "yard") {
    await page.getByRole("button", { name: "Yard", exact: true }).click();
    await page.locator(".yard .box", { hasText: chosen }).click();
  } else if (view === "images") {
    await page.locator(".nav button", { hasText: "Images" }).click();
    await page.locator(".irow").first().waitFor();
  } else if (view === "volumes") {
    // The mock's selection: `uploads`.
    await page.locator(".nav button", { hasText: "Volumes" }).click();
    await page.locator(".vrow", { hasText: "uploads" }).click();
  } else if (view === "networks") {
    // The mock's selection: `backend`.
    await page.locator(".nav button", { hasText: "Networks" }).click();
    await page.locator(".nethead", { hasText: "10.89.2.0/24" }).click();
  } else {
    await page.getByRole("button", { name: "Manifest" }).click();
    await page.locator("tbody tr", { hasText: chosen }).click();
    // The live numbers: a CPU share needs a second read of the usage.
    await page.locator("tbody tr", { hasText: chosen }).filter({ hasText: "%" }).waitFor();
  }
  // The rail's memory in use and the monitors' MiB: the first read.
  await page.locator(".status", { hasText: "MiB" }).waitFor();
  // The click's focus scrolled the row into view; the mock starts with `api`
  // selected and its table unscrolled.
  await page.evaluate(() => {
    (document.activeElement as HTMLElement | null)?.blur();
    document.querySelector(".manifest-wrap")?.scrollTo(0, 0);
  });
  await page.mouse.move(0, 0);
  await settled(page);
}

/** `image` with each of `boxes` (CSS pixels, at `scale`) painted black. */
function mask(image: PNG, boxes: Box[], scale: number) {
  for (const box of boxes) {
    const x0 = Math.max(0, Math.floor(box.x * scale));
    const y0 = Math.max(0, Math.floor(box.y * scale));
    const x1 = Math.min(image.width, Math.ceil((box.x + box.width) * scale));
    const y1 = Math.min(image.height, Math.ceil((box.y + box.height) * scale));
    for (let y = y0; y < y1; y++)
      for (let x = x0; x < x1; x++) {
        const at = (y * image.width + x) * 4;
        image.data[at] = image.data[at + 1] = image.data[at + 2] = 0;
        image.data[at + 3] = 255;
      }
  }
}

/** Every `selector`'s box, relative to `origin`. */
async function all(page: Page, selector: string, origin: { x: number; y: number }): Promise<Box[]> {
  const boxes = await page.locator(selector).evaluateAll((elements) =>
    elements.map((element) => {
      const r = element.getBoundingClientRect();
      return { x: r.x, y: r.y, width: r.width, height: r.height };
    }),
  );
  return boxes.map((box) => ({
    ...box,
    x: box.x - origin.x,
    y: box.y - origin.y,
  }));
}

/** `locator`'s box, relative to `origin`. */
async function within(page: Page, selector: string, origin: { x: number; y: number }): Promise<Box[]> {
  const box = await page.locator(selector).first().boundingBox();
  return box
    ? [
        {
          x: box.x - origin.x,
          y: box.y - origin.y,
          width: box.width,
          height: box.height,
        },
      ]
    : [];
}

// `worker`, stopped by an out-of-memory kill: the inspector's alert, its
// degraded dependents, its hint and its isolation while stopped.
for (const [view, scheme, chosen] of [
  ["manifest", "light", API],
  ["manifest", "dark", API],
  ["map", "light", API],
  ["map", "dark", API],
  ["yard", "light", API],
  ["yard", "dark", API],
  ["run", "light", API],
  ["run", "dark", API],
  ["images", "light", API],
  ["images", "dark", API],
  ["volumes", "light", API],
  ["volumes", "dark", API],
  ["networks", "light", API],
  ["networks", "dark", API],
  ["yard", "light", "ghcr.io/acme/worker"],
  ["yard", "dark", "ghcr.io/acme/worker"],
] as const) {
  const name = chosen === API ? view : `${view}-worker`;
  test(`the ${name} is the mock's, pixel for pixel (${scheme})`, async ({ browser }) => {
    // Less motion asked: no dots on the map's edges, in either page.
    const context = await browser.newContext({
      viewport: { width: 1400, height: 1000 },
      deviceScaleFactor: 2,
      colorScheme: scheme,
      reducedMotion: "reduce",
    });
    const mockPage = await context.newPage();
    const win = await openMock(mockPage, view, chosen);
    // The mock's window, inside its 1 px border: what the app's fills.
    const inner = {
      x: win.x + 1,
      y: win.y + 1,
      width: win.width - 2,
      height: win.height - 2,
    };
    const mock = PNG.sync.read(await mockPage.screenshot({ clip: inner }));
    const masks = [
      ...(await within(mockPage, ".lights", inner)),
      // api's Remove: web needs it, so the app disables it (D14.4b-2), the
      // mock never does. Compared where it's enabled (worker).
      ...(chosen === API && view !== "images" && view !== "volumes" && view !== "networks"
        ? await within(mockPage, '.insp .actions [data-act="remove"]', inner)
        : []),
      // The Images section's ages (the mock's "5 weeks" and "1 month" are
      // no one rule) and its legend's names (the app names a family by its
      // stored base, D14.6-3; the mock by words of its own).
      ...(view === "images" ? await all(mockPage, ".irow .ref small", inner) : []),
      ...(view === "images" ? await within(mockPage, ".legendrow", inner) : []),
      // The Volumes section's ages (the mock's "21 days ago" against the
      // app's "3 weeks ago", by the images' one rule), in its rows and its
      // inspector's line.
      ...(view === "volumes" ? await all(mockPage, ".vrow .ref small", inner) : []),
      ...(view === "volumes" ? await within(mockPage, ".insp .ident p", inner) : []),
      // A network's DNS: the mock's words are its own ("names resolve per
      // stack"); the app's say whether Shipyard's server runs.
      ...(view === "networks" ? await all(mockPage, ".insp .kv dd", inner).then((boxes) => boxes.slice(-1)) : []),
      // worker's log: the mock writes its own `[shipyard]` lines and marks
      // a WARN and a kill; a real log has none of that markup.
      ...(chosen === API ? [] : await within(mockPage, ".dock .termbody", inner)),
      // The out-of-memory alert's sentence: the mock's is its worker's own
      // ("reduce the batch size"), the app's says it of any container. Its
      // head, its box and what's below it are compared.
      ...(chosen === API ? [] : await within(mockPage, ".insp .alert", inner)).map((box) => ({
        ...box,
        y: box.y + 30,
        height: box.height - 30,
      })),
      // The rounded corners (12 px).
      { x: 0, y: 0, width: 12, height: 12 },
      { x: inner.width - 12, y: 0, width: 12, height: 12 },
      { x: 0, y: inner.height - 12, width: 12, height: 12 },
      {
        x: inner.width - 12,
        y: inner.height - 12,
        width: 12,
        height: 12,
      },
    ];
    const appPage = await context.newPage();
    await openApp(appPage, { width: inner.width, height: inner.height }, view, chosen);
    const app = PNG.sync.read(await appPage.screenshot());
    expect([app.width, app.height]).toEqual([mock.width, mock.height]);
    mask(mock, masks, 2);
    mask(app, masks, 2);
    const diff = new PNG({ width: mock.width, height: mock.height });
    const differing = pixelmatch(mock.data, app.data, diff.data, mock.width, mock.height, { threshold: 0.05 });
    mkdirSync(`${UI}/test-results`, { recursive: true });
    for (const [kind, image] of [
      ["mock", mock],
      ["app", app],
      ["diff", diff],
    ] as const)
      writeFileSync(`${UI}/test-results/${name}-${scheme}-${kind}.png`, PNG.sync.write(image));
    expect(differing, `pixels that differ (test-results/${name}-${scheme}-diff.png)`).toBe(0);
    await context.close();
  });
}
