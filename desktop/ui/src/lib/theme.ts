// Light or dark, the title bar's toggle (as the design mock's): the
// system's until the user chooses, then the choice for good, kept in this
// window's storage (a view's preference, not Shipyard's: lost with it, the
// system's again). The stylesheets read `data-theme` on the root both ways; what
// draws with colours read from them (the shell's terminal) hears the change.

/** A theme the user chose. */
export type Theme = "light" | "dark";

/** Where the choice is kept. */
export const THEME_KEY = "shipyard-theme";

/** The event a change sends on `window`. */
const CHANGED = "shipyard-theme";

/** The theme kept, if any (storage may refuse: none then). */
export function keptTheme(): Theme | null {
  try {
    const kept = localStorage.getItem(THEME_KEY);
    return kept === "light" || kept === "dark" ? kept : null;
  } catch {
    return null;
  }
}

/** The theme shown now: the one chosen, else the system's. */
export function currentTheme(): Theme {
  const chosen = document.documentElement.dataset.theme;
  if (chosen === "light" || chosen === "dark") return chosen;
  return typeof matchMedia === "function" && matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

/** The kept theme put on the page, before it first draws. */
export function applyKeptTheme() {
  const kept = keptTheme();
  if (kept) document.documentElement.dataset.theme = kept;
}

/** The other theme shown, and kept. */
export function toggleTheme(): Theme {
  const next: Theme = currentTheme() === "dark" ? "light" : "dark";
  document.documentElement.dataset.theme = next;
  try {
    localStorage.setItem(THEME_KEY, next);
  } catch {
    // Kept for this window only.
  }
  window.dispatchEvent(new Event(CHANGED));
  return next;
}

/** `heard` called when the theme may have changed: the toggle, or the
 * system's appearance (which shows only while nothing is chosen; a hearer
 * reads the colours again either way); a function that stops it. */
export function onThemeChange(heard: () => void): () => void {
  const system = typeof matchMedia === "function" ? matchMedia("(prefers-color-scheme: dark)") : null;
  window.addEventListener(CHANGED, heard);
  system?.addEventListener("change", heard);
  return () => {
    window.removeEventListener(CHANGED, heard);
    system?.removeEventListener("change", heard);
  };
}
