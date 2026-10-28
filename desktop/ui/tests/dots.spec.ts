// The map's dots (step 14.3): they move along the edges with open
// connections, and a poll's new layout (every 2 s) never wipes the ones in
// flight. The fixture's edges carry about ten at a time once going: a count
// that falls to none means they were cleared.
import { expect, test } from "@playwright/test";

test("the dots flow on, poll after poll", async ({ page }) => {
  await page.setViewportSize({ width: 1318, height: 858 });
  await page.goto("http://127.0.0.1:1420/preview.html");
  await page.waitForSelector(".mnode .box");
  await expect.poll(() => page.locator("circle.packet").count(), { timeout: 5_000 }).toBeGreaterThan(3);
  // Six seconds, three polls: never none.
  const counts = await page.evaluate(async () => {
    const seen: number[] = [];
    for (let i = 0; i < 60; i++) {
      seen.push(document.querySelectorAll("circle.packet").length);
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    return seen;
  });
  expect(Math.min(...counts), `${counts}`).toBeGreaterThan(0);
});
