// The desktop app on Linux, driven (step 14.7, D14.7-2): Tauri's WebDriver
// (`tauri-driver`, at 127.0.0.1:4444, over WebKitGTK's WebKitWebDriver)
// opens the real app, and this checks what a rootless user sees: the API's
// service started by the app itself (nothing served it before), the map
// with the user's containers, the rail and status bar saying rootless and
// that root's aren't shown, each section with its own, and the design's
// styles applied under the release CSP. A screenshot of each view lands in
// OUT. Plain W3C WebDriver over `fetch`: no client library.
//
// usage: node drive.mjs APP OUT
import { writeFileSync } from "node:fs";

const [app, out] = process.argv.slice(2);
const DRIVER = "http://127.0.0.1:4444";

async function call(method, path, body) {
  // Each call bounded: a driver that hangs fails the test, never holds it.
  const response = await fetch(`${DRIVER}${path}`, {
    signal: AbortSignal.timeout(60_000),
    method,
    headers: { "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const answer = await response.json();
  if (!response.ok) throw new Error(`${method} ${path}: ${JSON.stringify(answer.value)}`);
  return answer.value;
}

const session = await call("POST", "/session", {
  capabilities: { alwaysMatch: { browserName: "wry", "tauri:options": { application: app } } },
});
const id = session.sessionId;
const at = (path) => `/session/${id}${path}`;
const run = (script, ...args) => call("POST", at("/execute/sync"), { script, args });

/** `selector`'s elements' texts. */
const texts = (selector) => run("return [...document.querySelectorAll(arguments[0])].map((e) => e.textContent.trim());", selector);

/** Waits until `check` (on `selector`'s texts) holds, up to `seconds`. */
async function until(what, selector, check, seconds = 60) {
  const end = Date.now() + seconds * 1000;
  let seen = [];
  while (Date.now() < end) {
    seen = await texts(selector);
    if (check(seen)) return seen;
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  const body = await run("return document.body.innerText.slice(0, 2000);");
  throw new Error(`${what}: never (saw ${JSON.stringify(seen)}; the window said: ${body})`);
}

async function shot(name) {
  writeFileSync(`${out}/${name}.png`, Buffer.from(await call("GET", at("/screenshot")), "base64"));
}

/** Clicks the first `selector` whose text includes `text`: found and
 * clicked in the page, by its `textContent` (WebKitWebDriver's own text of
 * an element differs from it, and the checks above read `textContent`). */
async function click(selector, text) {
  const clicked = await run(
    "const found = [...document.querySelectorAll(arguments[0])].find((e) => e.textContent.includes(arguments[1])); if (found) found.click(); return Boolean(found);",
    selector,
    text,
  );
  if (!clicked) throw new Error(`no ${selector} saying ${text}`);
}

/** Fails if the window has heard an error, a rejection or a CSP refusal. */
async function noFaults(where) {
  const faults = await run("return window.shipyardFaults ?? ['the page never loaded its fault log'];");
  // A failed read of the API is said in the window, never a fault of its own;
  // anything else is.
  if (faults.length) throw new Error(`${where}: the window says ${JSON.stringify(faults)}`);
}

let failed = null;
try {
  // The map (the app opens on it): the service started, the containers in.
  await until("the map's boxes", ".mnode .box .nm", (names) => names.includes("web") && names.includes("db"), 90);
  await shot("map");
  await noFaults("the map");
  const status = (await texts(".status"))[0] ?? "";
  if (!status.includes("rootless")) throw new Error(`the status bar: ${status}`);
  const rail = (await texts(".rail .host"))[0] ?? "";
  if (!rail.includes("your own")) throw new Error(`the rail: ${rail}`);
  // The design's stylesheet applied (the release CSP: `style-src 'self'`).
  const background = await run("return getComputedStyle(document.querySelector('.rail')).backgroundColor;");
  if (background === "rgba(0, 0, 0, 0)") throw new Error("the rail has no background: the stylesheet wasn't applied");

  await click(".nav button", "Images");
  await until("the image", ".irow .ref", (refs) => refs.some((ref) => ref.startsWith("busy:1")));
  await shot("images");
  await click(".nav button", "Volumes");
  await until("the volume", ".vrow .ref", (refs) => refs.some((ref) => ref.startsWith("kept")));
  await shot("volumes");
  await click(".nav button", "Networks");
  await until("rootless's networks", ".secsum", (said) => said.some((line) => line.includes("No bridge networks here")));
  await shot("networks");
  await noFaults("the sections");

  await click(".nav button", "Yard");
  await click(".mnode .box", "web");
  await until("web's inspector", ".insp h3", (heads) => heads.some((head) => head.includes("web")));
  await shot("inspector");
  // The console's shell: xterm draws in the real window (its styles kept to
  // the CSP by sheets.ts), and the shell answers.
  await click(".dock .termtabs button", "Shell");
  await until("the shell's terminal", ".dock .xterm-rows", (rows) => rows.some((row) => row.includes("#") || row.includes("$")), 30);
  await shot("shell");
  await noFaults("the console");
  console.log("the app on Linux: ok (screenshots in target/desktop-linux/)");
} catch (error) {
  failed = error;
  await shot("failed").catch(() => {});
} finally {
  await call("DELETE", at("")).catch(() => {});
}
if (failed) {
  console.error(`the app on Linux: ${failed.message}`);
  process.exit(1);
}
