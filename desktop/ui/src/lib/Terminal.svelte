<script lang="ts">
  // The console's Shell (step 14.5, D14.5-1): xterm.js on a shell in the
  // container (`shipyard exec -it NAME sh`), its bytes written as they come,
  // what's typed sent back, fitted to the dock and resized with it. What the
  // container prints may drive the terminal (that's a terminal's job) but
  // nothing past it: links open nothing, the window's title and the
  // clipboard aren't its. What it's written is acknowledged, so the core
  // never sends more than the page can draw. Its end said; Enter opens
  // another.
  import "@xterm/xterm/css/xterm.css";
  import { FitAddon } from "@xterm/addon-fit";
  import { Terminal } from "@xterm/xterm";
  import { onMount } from "svelte";
  import type { Backend } from "./backend";
  import { keepStylesToTheCsp } from "./sheets";
  import { failureOf } from "./store";

  let {
    id,
    backend,
  }: {
    id: string;
    backend: Pick<Backend, "shellOpen" | "shellInput" | "shellResize" | "shellAck" | "shellClose">;
  } = $props();

  let host: HTMLDivElement | undefined = $state();

  /** `text` without its control characters: said in the terminal as text. */
  const plain = (text: string) => text.replace(/[\u0000-\u001f\u007f-\u009f]/g, "");

  /** How much written output is acknowledged at once. */
  const ACK_EVERY = 64 * 1024;

  /** Whether this web view has constructed stylesheets (`sheets.ts` needs
   * them: Safari 16.4, macOS 13.3). */
  const sheetsWork = () => "adoptedStyleSheets" in Document.prototype && typeof CSSStyleSheet.prototype.replaceSync === "function";

  onMount(() => {
    if (!host) return;
    if (!sheetsWork()) {
      host.textContent = "The shell needs macOS 13.3 or later (its terminal's styles).";
      return;
    }
    keepStylesToTheCsp();
    const tokens = getComputedStyle(host);
    const term = new Terminal({
      fontFamily: '"IBM Plex Mono", ui-monospace, monospace',
      fontSize: 12,
      scrollback: 5000,
      cursorBlink: true,
      theme: {
        background: tokens.getPropertyValue("--term").trim(),
        foreground: tokens.getPropertyValue("--term-ink").trim(),
      },
      // An OSC 8 link the container prints opens nothing.
      linkHandler: { activate: () => {} },
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host);
    fit.fit();

    let session: number | null = null;
    let opening = false;
    let gone = false;
    // What's typed goes one after another: invokes run side by side.
    let typing: Promise<unknown> = Promise.resolve();
    const ended = () => {
      session = null;
      term.write("\r\n[the shell ended: Enter for another]\r\n");
    };
    async function open() {
      if (opening) return;
      opening = true;
      // Its end may come before its number does (an image whose `sh`
      // exits at once): kept, and said once the number is here.
      let endedEarly = false;
      let mine: number | null = null;
      let written = 0;
      const asked = { cols: term.cols, rows: term.rows };
      try {
        const opened = await backend.shellOpen(id, asked.cols, asked.rows, (bytes) => {
          if (gone) return;
          if (bytes.length === 0) {
            if (mine === null) endedEarly = true;
            else if (mine === session) ended();
            return;
          }
          term.write(bytes, () => {
            written += bytes.length;
            if (written >= ACK_EVERY && mine !== null) {
              void backend.shellAck(mine, written).catch(() => {});
              written = 0;
            }
          });
        });
        mine = opened;
        if (gone) {
          void backend.shellClose(opened).catch(() => {});
          return;
        }
        session = opened;
        if (endedEarly) ended();
        else if (term.cols !== asked.cols || term.rows !== asked.rows) {
          // Resized while it opened.
          void backend.shellResize(opened, term.cols, term.rows).catch(() => {});
        }
      } catch (error) {
        if (!gone) term.write(`\r\n${plain(failureOf(error).message)}\r\n`);
      } finally {
        opening = false;
      }
    }
    const typed = term.onData((data) => {
      const to = session;
      if (to !== null) typing = typing.then(() => backend.shellInput(to, data)).catch(() => {});
      else if (data === "\r") void open();
    });
    const resized = term.onResize(({ cols, rows }) => {
      if (session !== null) void backend.shellResize(session, cols, rows).catch(() => {});
    });
    const watch = typeof ResizeObserver === "function" ? new ResizeObserver(() => fit.fit()) : null;
    watch?.observe(host);
    void open();
    term.focus();
    return () => {
      gone = true;
      watch?.disconnect();
      typed.dispose();
      resized.dispose();
      if (session !== null) void backend.shellClose(session).catch(() => {});
      term.dispose();
    };
  });
</script>

<div class="termbody shell" bind:this={host}></div>
