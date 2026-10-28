// Styles a script makes, kept to the app's CSP (step 14.5). xterm.js writes
// its theme, sizes and scrollbar as `<style>` elements, which the release
// CSP (`style-src 'self'`) refuses, even empty ones as they're inserted (the
// probe: WebKit reports each). Here a `<style>` that script creates is an
// `<x-style>` instead, whose text is a constructed stylesheet (CSSOM, which
// the CSP doesn't govern), adopted while it's in the document. Markup
// parsed from text still can't style anything: only `<style>` elements the
// app's own scripts make (and the CSP allows its own alone) become sheets.
// Their text is set by `textContent` alone (as xterm does); a sheet comes
// after the document's own in the cascade, wherever its element sits. The
// patch is the window's for its life, once a terminal has opened.

/** A `<style>` made by script: its text a constructed sheet, adopted while
 * it's in the document. */
class SheetElement extends HTMLElement {
  readonly sheet = new CSSStyleSheet();
  css = "";

  connectedCallback() {
    document.adoptedStyleSheets = [...document.adoptedStyleSheets, this.sheet];
  }

  disconnectedCallback() {
    document.adoptedStyleSheets = document.adoptedStyleSheets.filter((sheet) => sheet !== this.sheet);
  }
}

// Its text is its sheet's (DOM's `textContent` is a property of `Node`'s,
// so it's redefined here rather than overridden in the class).
Object.defineProperty(SheetElement.prototype, "textContent", {
  configurable: true,
  get(this: SheetElement) {
    return this.css;
  },
  set(this: SheetElement, css: string | null) {
    this.css = css ?? "";
    this.sheet.replaceSync(this.css);
  },
});

/** The tag a script's `<style>` becomes. */
export const SHEET_TAG = "x-style";

/** Makes every `<style>` a script creates a {@link SheetElement} (once). */
export function keepStylesToTheCsp() {
  if (customElements.get(SHEET_TAG)) return;
  customElements.define(SHEET_TAG, SheetElement);
  const create = Document.prototype.createElement;
  Document.prototype.createElement = function (this: Document, name: string, options?: ElementCreationOptions) {
    return create.call(this, name.toLowerCase() === "style" ? SHEET_TAG : name, options);
  } as typeof Document.prototype.createElement;
}
