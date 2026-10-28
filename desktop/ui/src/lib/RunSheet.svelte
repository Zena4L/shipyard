<script lang="ts">
  // The Run image sheet (step 14.4b): the mock's form (docs/design/shipyard-ui.html,
  // `#runsheet`), element for element, and its CLI twin as you type
  // (`shipyard run -d …`: the sheet runs it detached, D14.4b-5). What it sends
  // is the spec alone; the core builds the container (D14.4b-7). A pull's
  // progress, or the run's failure, is said under it.
  import { onMount } from "svelte";
  import { MEMORY_CHOICES, type RunSpec } from "./backend";
  import { runTwin } from "./actions";
  import type { Run } from "./store";

  let {
    image: given = null,
    run,
    taken,
    onrun,
    onedit,
  }: {
    /** The image to fill it with (an image's Run), else its own. */
    image?: string | null;
    /** The run under way, or its failure. */
    run: Run | null;
    /** Whether a container has this name already. */
    taken: (name: string) => boolean;
    onrun: (spec: RunSpec) => void;
    /** The form changed: a failure said is forgotten. */
    onedit: () => void;
  } = $props();

  // svelte-ignore state_referenced_locally
  let image = $state(given ?? "alpine:3.20");
  let name = $state("");
  let memory = $state(256);
  let port = $state("");
  let imageField: HTMLInputElement | undefined = $state();
  let nameField: HTMLInputElement | undefined = $state();

  const echo = $derived(runTwin({ image, name, memory, port }));

  onMount(() => {
    imageField?.focus();
    imageField?.select();
  });

  function submit(event: SubmitEvent) {
    event.preventDefault();
    if (!image.trim() || run?.going) return;
    const wanted = name.trim();
    if (wanted && taken(wanted)) {
      nameField?.setCustomValidity(`"${wanted}" is already in the yard`);
      nameField?.reportValidity();
      return;
    }
    onrun({ image: image.trim(), name: wanted || null, memoryMib: memory, port: port.trim() || null });
  }
  function edited() {
    nameField?.setCustomValidity("");
    onedit();
  }
</script>

<div class="runsheet" id="runsheet">
  <form autocomplete="off" onsubmit={submit} oninput={edited}>
    <div class="field">
      <label for="rImage">Image</label><input id="rImage" bind:value={image} bind:this={imageField} required />
    </div>
    <div class="field"><label for="rName">Name</label><input id="rName" bind:value={name} bind:this={nameField} placeholder="auto" /></div>
    <div class="field">
      <label for="rMem">Memory limit</label>
      <select id="rMem" bind:value={memory}
        >{#each MEMORY_CHOICES as choice (choice)}<option value={choice}>{choice >= 1024 ? `${choice / 1024} GiB` : `${choice} MiB`}</option
          >{/each}</select
      >
    </div>
    <div class="field"><label for="rPort">Publish port</label><input id="rPort" bind:value={port} placeholder="8081:80" /></div>
    <button class="btn primary" type="submit" disabled={run?.going}>Run</button>
  </form>
  {#if run?.going}
    <p class="hint">{run.line ? `Pulling ${run.image}: ${run.line}` : "Starting…"}</p>
  {:else}
    <p class="hint">Same as <code id="cliEcho">{echo}</code></p>
  {/if}
  {#if run?.error}<div class="alert">{run.error.message}</div>{/if}
</div>
