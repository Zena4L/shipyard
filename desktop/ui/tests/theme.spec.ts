// The title bar's light/dark toggle (as the design mock's): a click shows the
// other theme over the system's, and a reload keeps it.
import { expect, test } from "@playwright/test";

test("the toggle flips the theme and keeps it", async ({ page }) => {
  await page.emulateMedia({ colorScheme: "light" });
  await page.goto("http://127.0.0.1:1420/preview.html");
  const surface = () => page.locator(".win").evaluate((element) => getComputedStyle(element).backgroundColor);
  const light = await surface();
  await page.getByRole("button", { name: "Dark mode" }).click();
  await expect.poll(surface).not.toBe(light);
  const dark = await surface();
  await page.reload();
  await expect.poll(surface).toBe(dark);
  await expect(page.getByRole("button", { name: "Dark mode" })).toHaveAttribute("aria-pressed", "true");
  // And back.
  await page.getByRole("button", { name: "Dark mode" }).click();
  await expect.poll(surface).toBe(light);
});

test("a light choice holds over a dark system, across a reload", async ({ page }) => {
  await page.emulateMedia({ colorScheme: "dark" });
  await page.goto("http://127.0.0.1:1420/preview.html");
  const surface = () => page.locator(".win").evaluate((element) => getComputedStyle(element).backgroundColor);
  const dark = await surface();
  await expect(page.getByRole("button", { name: "Dark mode" })).toHaveAttribute("aria-pressed", "true");
  await page.getByRole("button", { name: "Dark mode" }).click();
  await expect.poll(surface).not.toBe(dark);
  const light = await surface();
  await page.reload();
  await expect.poll(surface).toBe(light);
  await expect(page.getByRole("button", { name: "Dark mode" })).toHaveAttribute("aria-pressed", "false");
});
