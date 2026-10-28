// The built page breaks no rule of the release CSP (step 14.2): the page
// with the mock's data, built as the app's is, served with the CSP Tauri
// gives the app's window (tauri.conf.json's `csp`), every violation
// counted. A style written into markup is refused (the control: the
// listener hears a violation), and nothing of the page's own is. The
// window's `freezePrototype` is honoured too (the two settings read from
// tauri.conf.json, and a page error fails it): with it on,
// Tauri freezes `Object.prototype` before any of the page's scripts, and
// xterm.js's own namespace (`o.toString = …`) then throws as it loads, the
// window left blank (14.7: the app was so since 14.5, and no test said).
import { expect, test } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { extname, join, normalize } from "node:path";
import { fileURLToPath } from "node:url";

const UI = fileURLToPath(new URL("..", import.meta.url));
const BUILT = join(UI, "dist-fixture");
const SECURITY = JSON.parse(readFileSync(join(UI, "../src-tauri/tauri.conf.json"), "utf8")).app.security;
const CSP: string = SECURITY.csp;
const FROZEN: boolean = SECURITY.freezePrototype === true;
const TYPES: Record<string, string> = {
  ".html": "text/html",
  ".js": "text/javascript",
  ".css": "text/css",
  ".woff2": "font/woff2",
  ".woff": "font/woff",
  ".svg": "image/svg+xml",
  ".png": "image/png",
};

test.beforeAll(() => {
  execFileSync("npx", ["vite", "build", "--mode", "fixture", "--logLevel", "error"], { cwd: UI, stdio: "inherit" });
});

test("the built page keeps to the release CSP", async ({ page }) => {
  await page.route("http://app.test/**", (route) => {
    const path = normalize(new URL(route.request().url()).pathname);
    const file = join(BUILT, path);
    if (!file.startsWith(BUILT)) return route.fulfill({ status: 404 });
    try {
      route.fulfill({
        body: readFileSync(file),
        contentType: TYPES[extname(file)] ?? "application/octet-stream",
        headers: { "Content-Security-Policy": CSP },
      });
    } catch {
      route.fulfill({ status: 404 });
    }
  });
  // As Tauri does it, before the page's scripts.
  if (FROZEN) await page.addInitScript(() => Object.freeze(Object.prototype));
  const failures: string[] = [];
  page.on("pageerror", (error) => failures.push(String(error)));
  await page.addInitScript(() => {
    const seen: string[] = [];
    (window as unknown as { violations: string[] }).violations = seen;
    document.addEventListener("securitypolicyviolation", (event) =>
      seen.push(`${event.effectiveDirective} ${event.blockedURI}`),
    );
  });
  await page.goto("http://app.test/preview.html");
  await page.waitForSelector(".mnode .box", { timeout: 10_000 }).catch(() => {});
  expect(failures).toEqual([]);
  await page.waitForSelector(".mnode .box");
  await page.evaluate(() => document.fonts.ready);
  const violations = () => page.evaluate(() => [...(window as unknown as { violations: string[] }).violations]);
  expect(await violations()).toEqual([]);
  // Its fonts are its own, loaded; its memory bar's width set at run time
  // (once the live numbers' first read lands).
  expect(await page.evaluate(() => document.fonts.check('800 30px "Big Shoulders Stencil Display"'))).toBe(true);
  await expect
    .poll(() => page.locator(".rail .bar i").evaluate((bar) => bar.getBoundingClientRect().width))
    .toBeGreaterThan(0);
  // The console's shell (14.5): xterm draws under it too (its styles kept
  // to the CSP by `sheets.ts`), and what a container prints opens nothing
  // and names no window (no clipboard addon: OSC 52 is nothing to xterm).
  await page.locator(".mnode .box", { hasText: "ghcr.io/acme/api" }).click();
  await page.getByRole("tab", { name: "Shell" }).click();
  await page.locator(".xterm-rows").getByText("/ #").waitFor();
  const title = await page.title();
  await page.keyboard.type("link");
  await page.keyboard.press("Enter");
  await page.locator(".xterm-rows").getByText("open me").waitFor();
  const opened: string[] = [];
  page.on("popup", (popup) => opened.push(popup.url()));
  // xterm's own handler asks before it opens: a dialog would say it's on.
  const asked: string[] = [];
  page.on("dialog", (dialog) => {
    asked.push(dialog.message());
    void dialog.accept();
  });
  // The pointer on the link first (xterm marks its screen so): the click
  // below lands on a link, or this proves nothing.
  const link = (await page.locator(".xterm-rows").getByText("open me").boundingBox())!;
  await page.mouse.move(link.x + link.width / 2, link.y + link.height / 2);
  await expect(page.locator(".xterm .xterm-screen.xterm-cursor-pointer, .xterm.xterm-cursor-pointer").first()).toBeAttached();
  await page.mouse.click(link.x + link.width / 2, link.y + link.height / 2);
  expect(asked).toEqual([]);
  expect(opened).toEqual([]);
  expect(page.url()).toBe("http://app.test/preview.html");
  expect(await page.title()).toBe(title);
  expect(await violations()).toEqual([]);
  // The control: a style in markup is refused, and heard.
  await page.evaluate(() => {
    const p = document.createElement("div");
    p.innerHTML = '<p style="color: red">x</p>';
    document.body.append(p);
  });
  await expect.poll(violations).toEqual([expect.stringMatching(/^style-src/)]);
});
