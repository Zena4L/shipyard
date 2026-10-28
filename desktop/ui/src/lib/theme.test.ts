import { afterEach, describe, expect, it, vi } from "vitest";
import { THEME_KEY, applyKeptTheme, currentTheme, keptTheme, onThemeChange, toggleTheme } from "./theme";

afterEach(() => {
  delete document.documentElement.dataset.theme;
  localStorage.clear();
});

describe("the theme", () => {
  it("is the system's until chosen, then flips both ways and is kept", () => {
    // jsdom's system is light (no matchMedia match).
    expect(currentTheme()).toBe("light");
    expect(keptTheme()).toBeNull();
    const heard = vi.fn();
    const stop = onThemeChange(heard);
    expect(toggleTheme()).toBe("dark");
    expect(document.documentElement.dataset.theme).toBe("dark");
    expect(localStorage.getItem(THEME_KEY)).toBe("dark");
    expect(toggleTheme()).toBe("light");
    expect(keptTheme()).toBe("light");
    expect(heard).toHaveBeenCalledTimes(2);
    stop();
    toggleTheme();
    expect(heard).toHaveBeenCalledTimes(2);
  });

  it("puts the kept choice on the page, and ignores anything else kept", () => {
    localStorage.setItem(THEME_KEY, "dark");
    applyKeptTheme();
    expect(document.documentElement.dataset.theme).toBe("dark");
    delete document.documentElement.dataset.theme;
    localStorage.setItem(THEME_KEY, "<b>sepia</b>");
    applyKeptTheme();
    expect(document.documentElement.dataset.theme).toBeUndefined();
  });
});
