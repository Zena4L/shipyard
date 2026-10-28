// The Images section under the pointer (14.7): hovering a layer must never
// move what's under the pointer. The caption above the rows says the layer's
// step, and a real image's step can be hundreds of characters: a caption
// that grew with it pushed the rows down, out from under the pointer, which
// unlit the layer, shrank the caption, and brought them back: a flicker the
// user saw. The preview's steps are short, so a long one is written in.
import { expect, test } from "@playwright/test";

test("a long step under the pointer moves nothing", async ({ page }) => {
  await page.setViewportSize({ width: 1400, height: 900 });
  await page.goto("http://127.0.0.1:1420/preview.html");
  await page.locator(".nav button", { hasText: "Images" }).click();
  const row = page.locator(".irow").first();
  await row.waitFor();
  const at = () => row.evaluate((element) => element.getBoundingClientRect().y);
  const caption = () => page.locator("#layercap").evaluate((element) => element.getBoundingClientRect().height);
  const before = [await at(), await caption()];
  await row.locator("[data-layer]").first().hover();
  await expect(page.locator("#layercap code")).toBeVisible();
  // Still lit: before the fix, even the preview's short steps made the
  // caption a few pixels taller than its 38, the rows moved, and the layer
  // was unlit here already.
  expect(await page.locator("#layercap").textContent()).toContain("·");
  await page.locator("#layercap code").evaluate((code) => {
    code.textContent = `RUN ${"apt-get install -y something-long ".repeat(18)}`;
  });
  expect([await at(), await caption()]).toEqual(before);
  // Both of its lines inside it, never cut off below (its text once took
  // the class `caption`, which the design's stylesheet also has: a 12 px
  // gap between them, the second line cut off).
  const inside = await page.locator("#layercap").evaluate((element) => element.scrollHeight <= element.clientHeight);
  expect(inside).toBe(true);
});
