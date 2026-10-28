<script lang="ts">
  // The app's window: the mock's (docs/design/shipyard-ui.html), element for
  // element (step 14.2: the visual test compares them, pixel for pixel).
  // What exists so far: the rail, the main head, the map (14.3), the Yard
  // and the Manifest, the inspector and its actions (14.4), the Run sheet
  // (⌘K), the status bar; the console (14.5); the Images, Volumes and
  // Networks sections (14.6). Settings (⌘,) open in the inspector's place.
  import { onDestroy, onMount } from "svelte";
  import type { Act } from "./lib/actions";
  import type { Backend, RunSpec } from "./lib/backend";
  import { FAILURE_HEAD, createStore } from "./lib/store";
  import { monitorsMemory } from "./lib/usage";
  import { STATE_LABEL, fmtSize, listed, row, type BoxState, type LinkNode } from "./lib/model";
  import Manifest from "./lib/Manifest.svelte";
  import Map from "./lib/Map.svelte";
  import Inspector from "./lib/Inspector.svelte";
  import RunSheet from "./lib/RunSheet.svelte";
  import Console from "./lib/Console.svelte";
  import Images from "./lib/Images.svelte";
  import ImageInspector from "./lib/ImageInspector.svelte";
  import { imagesModel } from "./lib/layers";
  import Volumes from "./lib/Volumes.svelte";
  import VolumeInspector from "./lib/VolumeInspector.svelte";
  import Networks from "./lib/Networks.svelte";
  import NetworkInspector from "./lib/NetworkInspector.svelte";
  import { networksModel, volumesModel } from "./lib/storage";
  import type { Section } from "./lib/graph";
  import Yard from "./lib/Yard.svelte";
  import { mapModel } from "./lib/graph";

  let { backend, now = () => Date.now() }: { backend: Backend; now?: () => number } = $props();

  // The backend is the app's for its life: taken once.
  // svelte-ignore state_referenced_locally
  const store = createStore(backend);
  const sections = [
    ["yard", "Yard"],
    ["images", "Images"],
    ["volumes", "Volumes"],
    ["networks", "Networks"],
  ] as const;
  let section: (typeof sections)[number][0] = $state("yard");
  let view: "yard" | "map" | "list" = $state("map");
  let selected: string | null = $state(null);
  let settings = $state(false);

  const nodes = $derived(listed(($store.links?.containers ?? []) as LinkNode[], $store.containers));
  const rows = $derived(
    nodes.map((node) => {
      const summary = $store.containers.get(node.id);
      return row(
        { ...node, state: summary?.State ?? node.state },
        summary,
        $store.live?.[node.id],
        now(),
      );
    }),
  );
  const count = (state: BoxState) => rows.filter((r) => r.state === state).length;

  // The images (14.6a): the section, its inspector, the map's overlay and
  // the container inspector's image all draw this one model.
  const imagesView = $derived($store.images ? imagesModel($store.images, $store.containers) : null);
  const who = $derived(new globalThis.Map(rows.map((r) => [r.id, { name: r.name, state: r.state }])));
  const imagesUnused = $derived(imagesView?.unused.length ?? 0);
  let selImage: string | null = $state(null);
  const chosenImageView = $derived(imagesView?.images.find((image) => image.id === selImage) ?? imagesView?.images[0] ?? null);
  // The layer under the pointer, wherever it's drawn (the mock's `litLayer`).
  let lit: string | null = $state(null);
  const lightLayer = (event: Event) => {
    const layer = event.target instanceof Element ? event.target.closest("[data-layer]") : null;
    lit = layer?.getAttribute("data-layer") ?? null;
  };
  // The pointer out of the window: nothing lit (no `mouseover` says so).
  const leaveWindow = (event: MouseEvent) => {
    if (event.relatedTarget === null) lit = null;
  };
  const showImage = (id: string) => {
    selImage = id;
    section = "images";
  };
  const showContainer = (id: string) => {
    selected = id;
    section = "yard";
  };
  /** The key to select once `key` is gone from `keys`: the one after it
   * (the mock's rule), else the one before. */
  function nextAfter(keys: string[], key: string): string | null {
    const at = keys.indexOf(key);
    const rest = keys.filter((_, i) => i !== at);
    return rest[Math.min(at, rest.length - 1)] ?? null;
  }
  // One removal at a time, the inspector's or the section's.
  let removingImage = $state(false);
  async function removeImages(images: { id: string; tags: string[] }[]): Promise<boolean> {
    const keys = (imagesView?.images ?? []).map((one) => one.id);
    removingImage = true;
    const done = await store.removeImages(images);
    removingImage = false;
    if (done && selImage !== null && images.some((image) => image.id === selImage)) selImage = nextAfter(keys, selImage);
    return done;
  }
  async function removeChosenImage(image: { id: string; tags: string[] }) {
    selImage = image.id;
    await removeImages([image]);
  }
  const tally = $derived(
    (
      [
        ["running", "running"],
        ["paused", "paused"],
        ["exited", "stopped"],
        ["failed", "failed"],
      ] as const
    ).filter(([state]) => count(state) > 0),
  );
  const model = $derived($store.links ? mapModel($store.links, $store.containers, $store.networks) : null);
  const degradedCount = $derived(model ? [...model.health.values()].filter((h) => h === "degraded").length : 0);

  // The volumes and networks (14.6b): their sections and inspectors; the
  // map's chips and keys and the inspector's rows select one there. Each
  // read again as its section is shown.
  const volumesView = $derived($store.volumes && model ? volumesModel($store.volumes, model.containers) : null);
  const networksView = $derived(model ? networksModel($store.networks, model, $store.networkViews) : null);
  let selVolume: string | null = $state(null);
  let selNetwork: string | null = $state(null);
  const chosenVolume = $derived(volumesView?.volumes.find((volume) => volume.name === selVolume) ?? volumesView?.volumes[0] ?? null);
  const chosenBand = $derived(networksView?.bands.find((band) => band.full === selNetwork) ?? networksView?.bands[0] ?? null);
  $effect(() => {
    if (section === "volumes") void store.readVolumes();
  });
  $effect(() => store.watchNetworks(section === "networks"));
  /** Another section shown, the volume or network `name` selected in it. */
  function goto(to: Section, name?: string) {
    if (to === "volumes" && name) selVolume = name;
    if (to === "networks" && name) selNetwork = name;
    section = to;
  }
  // One deletion at a time, the inspector's or the section's.
  let deletingVolume = $state(false);
  async function deleteVolumes(names: string[]): Promise<boolean> {
    const keys = (volumesView?.volumes ?? []).map((one) => one.name);
    deletingVolume = true;
    const done = await store.removeVolumes(names);
    deletingVolume = false;
    if (done && selVolume !== null && names.includes(selVolume)) selVolume = nextAfter(keys, selVolume);
    return done;
  }
  $effect(() => store.watchLinks(section === "yard" && view === "map"));
  const upRows = $derived(rows.filter((r) => r.state === "running" || r.state === "paused"));
  const monitors = $derived(upRows.length);
  // The shown containers' monitors' memory (each measured by its own ID).
  const monitorsMiB = $derived(monitorsMemory($store.live, upRows.map((r) => r.id)));
  const chosen = $derived(rows.find((r) => r.id === selected) ?? null);
  const chosenNode = $derived(model?.containers.find((node) => node.id === selected) ?? null);
  // The inspector's data follows the selection, and its state and image (a
  // start, a stop or a re-create read again). Plain values: the effect runs
  // again when one changes, never because another update rebuilt the list.
  const chosenState = $derived(selected ? ($store.containers.get(selected)?.State ?? null) : null);
  const chosenImage = $derived(selected ? ($store.containers.get(selected)?.ImageID ?? null) : null);
  $effect(() => {
    void chosenState;
    void chosenImage;
    void store.focusOn(selected);
  });
  const focus = store.focus;
  const logs = store.logs;

  // The console (14.5), under the Yard section's views while a container
  // is selected: its tab and whether it's folded, the window's alone. A
  // selection that doesn't run (another chosen, or this one stopped) shows
  // its logs; its Shell tab, asked for, says why there's none.
  let consoleTab: "logs" | "shell" = $state("logs");
  let consoleCollapsed = $state(false);
  const docked = $derived($store.phase === "live" && section === "yard" && chosen !== null);
  $effect(() => {
    void selected;
    if (chosenState !== "running") consoleTab = "logs";
  });
  const host = $derived($store.host);

  const visibility = () => store.setVisible(document.visibilityState === "visible");
  const keys = (event: KeyboardEvent) => {
    if (event.metaKey && event.key === ",") {
      event.preventDefault();
      settings = !settings;
    }
    // ⌘K (Ctrl+K off the Mac, outside a text field: there it deletes to
    // the line's end).
    const typing = event.target instanceof HTMLInputElement || event.target instanceof HTMLTextAreaElement;
    if ((event.metaKey || (event.ctrlKey && !typing)) && event.key.toLowerCase() === "k") {
      event.preventDefault();
      runOpen = true;
    }
    if (event.key === "Escape" && runOpen) closeRun();
  };

  // The Run sheet (14.4b): opened by the title bar's button, Run image or
  // ⌘K; a run that worked shows its container, selected, in the Yard.
  let runOpen = $state(false);
  // The image Run fills it with (an image inspector's Run), else its own.
  let runPrefill: string | null = $state(null);
  function closeRun() {
    runOpen = false;
    runPrefill = null;
    store.clearRun();
  }
  async function runImage(spec: RunSpec) {
    const id = await store.run(spec);
    // Closed meanwhile: whatever was opened since stays shown.
    if (!id || !runOpen) return;
    runOpen = false;
    section = "yard";
    view = "yard";
    selected = id;
  }

  // An inspector's action on the selection; removed, the one after it in
  // the list is selected (the mock's rule), else the one before.
  async function act(action: Act, force: boolean) {
    const id = selected;
    if (!id) return;
    const keys = rows.map((r) => r.id);
    const done = await store.act(id, action, force);
    if (done && action === "remove" && selected === id) selected = nextAfter(keys, id);
  }
  onMount(() => {
    document.addEventListener("visibilitychange", visibility);
    document.addEventListener("keydown", keys);
    document.addEventListener("mouseover", lightLayer);
    document.addEventListener("focusin", lightLayer);
    document.addEventListener("mouseout", leaveWindow);
    void store.open();
  });
  onDestroy(() => {
    document.removeEventListener("visibilitychange", visibility);
    document.removeEventListener("keydown", keys);
    document.removeEventListener("mouseover", lightLayer);
    document.removeEventListener("focusin", lightLayer);
    document.removeEventListener("mouseout", leaveWindow);
    store.close();
  });

  function toggleStart(event: Event) {
    void store.setStartVmOnOpen((event.currentTarget as HTMLInputElement).checked);
  }
</script>

<div class="win" role="application" aria-label="Shipyard">
  <div class="titlebar" data-tauri-drag-region>
    <div class="lights" aria-hidden="true"><i></i><i></i><i></i></div>
    <div class="wintitle" data-tauri-drag-region>Shipyard</div>
    <button class="cmd" type="button" onclick={() => (runOpen = true)}><span class="long">Run an image…</span><kbd>⌘K</kbd></button>
  </div>

  <div class="body">
    <aside class="rail">
      <div class="brand">
        <svg width="24" height="24" viewBox="0 0 24 24" aria-hidden="true">
          <rect x="1" y="3" width="22" height="3" fill="var(--accent)" />
          <rect x="15" y="6" width="2" height="6" fill="var(--accent)" />
          <rect x="3" y="13" width="18" height="9" rx="1" fill="var(--steel)" />
          <path d="M7 14.5v6M10 14.5v6M13 14.5v6M16 14.5v6" stroke="var(--steel-ink)" stroke-opacity=".35" stroke-width="1" />
        </svg>
        <span>Shipyard</span>
      </div>
      <div>
        <div class="railhead">Local</div>
        <nav class="nav" aria-label="Sections">
          {#each sections as [key, label] (key)}
            <button type="button" aria-current={section === key ? "page" : undefined} onclick={() => (section = key)}
              >{label} <small>{key === "yard" ? rows.length : ($store.counts?.[key] ?? "")}</small></button
            >
          {/each}
        </nav>
      </div>
      <div class="host">
        <div class="row"><span>{host?.label ?? "Linux"}</span><b>{host?.kind ?? "—"}</b></div>
        <div class="row"><span>Memory</span><b>{host?.memory ?? "—"}</b></div>
        <div class="bar"><i style:width="{host?.memoryShare ?? 0}%"></i></div>
        <div class="row"><span>Kernel</span><b>{host?.kernel ?? "—"}</b></div>
        {#if host?.ownOnly}<div class="row" title={host.ownOnly}><span>Shows</span><b>your own</b></div>{/if}
      </div>
    </aside>

    <section class="main" class:docked>
      <div class="mainhead">
        <h1>{sections.find(([key]) => key === section)?.[1]}</h1>
        <div class="tally">
          {#if section === "images" && imagesView}<span>{imagesView.images.length} images</span><span>{fmtSize(imagesView.unique)} on disk</span
            >{#if imagesUnused}<span><i class="dot"></i>{imagesUnused} unused</span>{/if}{:else if section === "volumes" && volumesView}<span
              >{volumesView.volumes.length} volumes</span
            >{#if volumesView.unused.length}<span><i class="dot"></i>{volumesView.unused.length} not mounted</span>{/if}{:else if section === "networks" && networksView}<span
              >{networksView.bands.length} networks</span
            ><span>{networksView.bands.filter((band) => band.internal).length} internal</span>{:else}{#each tally as [state, word], i (state)}{#if i === 1 && degradedCount}<span
                  ><i class="dot degraded"></i>{degradedCount} degraded</span
                >{/if}<span><i class="dot {state}"></i>{count(state)} {word}</span>{/each}{/if}
        </div>
        <div class="spacer"></div>
        <div class="seg" role="group" aria-label="View" hidden={section !== "yard"}>
          <button type="button" aria-pressed={view === "yard"} onclick={() => (view = "yard")}>Yard</button>
          <button type="button" aria-pressed={view === "map"} onclick={() => (view = "map")}>Map</button>
          <button type="button" aria-pressed={view === "list"} onclick={() => (view = "list")}>Manifest</button>
        </div>
        <button class="btn primary" type="button" onclick={() => (runOpen ? closeRun() : (runOpen = true))}>Run image</button>
      </div>

      {#if runOpen}
        <RunSheet
          image={runPrefill}
          run={$store.run}
          taken={(name) => rows.some((r) => r.name === name)}
          onrun={runImage}
          onedit={() => store.clearRun()}
        />
      {/if}

      <div class="viewport">
        {#if $store.actionError && $store.phase === "live" && section !== "networks"}<div class="alert">{$store.actionError.message}</div>{/if}
        {#if $store.phase === "live" && section === "yard" && view === "yard" && model}
          <Yard {model} {selected} onselect={(id) => (selected = id)} onsection={goto} />
        {:else if $store.phase === "live" && section === "yard" && view === "map" && model}
          <Map
            {model}
            images={imagesView}
            {lit}
            onimage={showImage}
            {selected}
            onselect={(id) => (selected = id)}
            onsection={goto}
            onstart={(id) => store.startContainer(id)}
          />
        {:else if $store.phase === "live" && section === "yard" && view === "list"}
          <Manifest {rows} {selected} onselect={(id) => (selected = id)} />
        {:else if $store.phase === "live" && section === "images" && imagesView}
          <div class="secwrap">
            <Images
              model={imagesView}
              {who}
              selected={chosenImageView?.id ?? null}
              {lit}
              now={now()}
              onselect={(id) => (selImage = id)}
              ongoto={showContainer}
              busy={removingImage}
              onremoveall={removeImages}
            />
          </div>
        {:else if $store.phase === "live" && section === "volumes" && volumesView}
          <div class="secwrap">
            <Volumes
              model={volumesView}
              selected={chosenVolume?.name ?? null}
              now={now()}
              onselect={(name) => (selVolume = name)}
              ongoto={showContainer}
              busy={deletingVolume}
              onremoveall={deleteVolumes}
            />
          </div>
        {:else if $store.phase === "live" && section === "networks" && networksView}
          <div class="secwrap">
            <Networks model={networksView} selected={chosenBand?.full ?? null} onselect={(full) => (selNetwork = full)} ongoto={showContainer} />
          </div>
        {:else}
          <div class="secwrap">
            {#if $store.phase === "checking"}
              <div class="secsum">Looking for Shipyard…</div>
            {:else if $store.phase === "stopped"}
              <div class="secsum"><b>Shipyard's VM is stopped.</b><button class="btn primary" type="button" onclick={() => store.startVm()}>Start</button></div>
            {:else if $store.phase === "connecting"}
              <div class="secsum">
                <b>Connecting to Shipyard…</b>{#if $store.status && $store.status.platform !== "macos"}
                  Its service is started if it isn't running: this can take a few seconds.{/if}
              </div>
            {:else if $store.phase === "starting"}
              <div class="secsum"><b>Starting Shipyard's VM…</b></div>
              {#if $store.lines.length}<pre class="files">{$store.lines.join("\n")}</pre>{/if}
            {:else if $store.phase === "failed"}
              <div class="alert"><b>{FAILURE_HEAD[$store.error?.kind ?? "other"] ?? FAILURE_HEAD.other}</b>{$store.error?.message ?? ""}</div>
              <div><button class="btn" type="button" onclick={() => store.open()}>Try again</button></div>
            {:else}
              <!-- A section whose first read hasn't landed, or failed (the
                   status bar says why: Not current). -->
              <div class="secsum">{$store.stale ? `Can't read it: ${$store.stale.message}` : "Loading…"}</div>
            {/if}
          </div>
        {/if}
      </div>
      {#if docked && chosen && chosenNode}
        <div class="dock" class:collapsed={consoleCollapsed}>
          {#key chosenNode.id}<Console
            node={chosenNode}
            hostPid={$focus?.id === chosenNode.id ? ($focus.isolation?.command?.pid ?? null) : null}
            logs={$logs}
            tab={consoleTab}
            collapsed={consoleCollapsed}
            {backend}
            ontab={(tab) => {
              consoleTab = tab;
              consoleCollapsed = false;
            }}
            ontoggle={() => (consoleCollapsed = !consoleCollapsed)}
          />{/key}
        </div>
      {/if}
    </section>

    <aside class="insp" aria-live="polite">
      {#if settings && $store.status}
        <div class="sect">
          <h4>Settings</h4>
          {#if $store.status.vm}
            <label><input type="checkbox" checked={$store.status.startVmOnOpen} onchange={toggleStart} /> Start Shipyard's VM when the app opens</label>
          {/if}
          {#if host?.ownOnly}<p class="tnote">{host.ownOnly}</p>{/if}
          <dl class="kv"><dt>shipyard</dt><dd>{"path" in $store.status.shipyard ? $store.status.shipyard.path : $store.status.shipyard.missing}</dd></dl>
          <div><button class="btn" type="button" onclick={() => store.chooseShipyard()}>Choose shipyard…</button></div>
          {#if $store.settingsError}<div class="warnbox">{$store.settingsError.message}</div>{/if}
          {#if $store.status.settingsSaid}<div class="warnbox">{$store.status.settingsSaid}</div>{/if}
        </div>
      {:else if section === "images" && chosenImageView && imagesView}
        <ImageInspector
          image={chosenImageView}
          model={imagesView}
          {who}
          {lit}
          now={now()}
          onrun={(reference) => {
            runPrefill = reference;
            runOpen = true;
          }}
          busy={removingImage}
          onremove={removeChosenImage}
          ongoto={showContainer}
        />
      {:else if section === "volumes" && chosenVolume && volumesView}
        <VolumeInspector
          volume={chosenVolume}
          largest={volumesView.largest}
          now={now()}
          mac={$store.status?.platform === "macos"}
          busy={deletingVolume}
          ondelete={(name) => {
            selVolume = name;
            void deleteVolumes([name]);
          }}
          ongoto={showContainer}
        />
      {:else if section === "networks" && chosenBand}
        <NetworkInspector band={chosenBand} ongoto={showContainer} />
      {:else if chosen && chosenNode && model}
        <Inspector
          node={chosenNode}
          row={chosen}
          {model}
          summary={$store.containers.get(chosenNode.id)}
          focus={$focus?.id === chosenNode.id ? $focus : null}
          rootless={host?.rootless ?? false}
          images={imagesView}
          {lit}
          onimage={showImage}
          busy={$store.busy.has(chosenNode.id)}
          onact={act}
          onshell={() => {
            consoleTab = "shell";
            consoleCollapsed = false;
          }}
          onselect={(id) => (selected = id)}
          onsection={goto}
        />
      {:else}
        <div class="sect"><p class="nothing">Select a container in the yard.</p></div>
      {/if}
    </aside>
  </div>

  <div class="status">
    <span><span class="dot running"></span>&nbsp; <b>Daemonless</b> — nothing runs when no containers do</span>
    <span class="mono">{monitors} monitor{monitors === 1 ? "" : "s"}{monitorsMiB !== null ? ` · ${monitorsMiB.toFixed(1)} MiB` : ""}</span>
    <span class="spacer"></span>
    {#if $store.stale}<span title={$store.stale.message}>Not current: {FAILURE_HEAD[$store.stale.kind] ?? FAILURE_HEAD.other}</span>{/if}
    <span class="mono">{host?.backend ?? ""}</span>
    <span class="mono">{host?.version ?? ""}</span>
  </div>
</div>
