<script lang="ts">
  // A volume's inspector (step 14.6b): the mock's `volumeInsp`
  // (docs/design/shipyard-ui.html), element for element. Its name, whether
  // it's mounted, size and age; Delete (disabled while a container names
  // it, confirmed inline with its command); who mounts it and who can
  // write; where its data is; its top level. Every string may come from a
  // container or the volume's own files: text.
  import { listOf } from "./actions";
  import Drum from "./Drum.svelte";
  import { agoIso } from "./layers";
  import { fmtSize } from "./model";
  import { fillOf, type VolumeView } from "./storage";

  let {
    volume,
    largest,
    now,
    mac,
    busy = false,
    ondelete,
    ongoto,
  }: {
    volume: VolumeView;
    /** The largest volume's MB (the drum's fill). */
    largest: number;
    now: number;
    /** On a Mac: its data is in the VM. */
    mac: boolean;
    /** A deletion under way: its button waits. */
    busy?: boolean;
    /** Delete confirmed. */
    ondelete: (name: string) => void;
    ongoto: (id: string) => void;
  } = $props();

  // The confirm, open (closed for another volume).
  let confirming = $state(false);
  const name = $derived(volume.name);
  $effect(() => {
    void name;
    confirming = false;
  });

  const twin = $derived(`shipyard volume rm ${volume.name}`);
  const writers = $derived(volume.users.filter((user) => user.mode === "rw").length);
  const size = $derived(`${volume.complete ? "" : "at least "}${fmtSize(volume.size)}`);
  const created = $derived(agoIso(volume.created, now));
  // Its top level as a tree; a directory's name ends in `/`.
  const tree = $derived(
    [
      `${volume.name}/`,
      ...volume.files.map((file, i) => {
        const last = i === volume.files.length - 1 && !volume.more;
        return `${last ? "└──" : "├──"} ${file.name}${file.kind === "directory" ? "/" : ""}`;
      }),
      ...(volume.more ? [`└── … and ${volume.more} more`] : []),
    ].join("\n"),
  );
</script>

<div class="insp-top">
  <div class="bigcode" style:font-size="30px">
    <Drum fill={fillOf(volume.size, largest)} size={26} dashed={!volume.users.length} />
    {volume.name.toUpperCase()}
  </div>
  <div class="ident">
    <h3 style:font-size="16px">
      {volume.name} <span class="pill" class:running={volume.users.length > 0}>{volume.users.length ? "Mounted" : "Not mounted"}</span>
    </h3>
    <p>local volume · {size} · created {created}</p>
  </div>
  {#if confirming}
    <div class="confirm">
      Delete <b>{volume.name}</b> and its {size} of data? <code>{twin}</code> <span class="spacer"></span><button
        class="btn danger"
        type="button"
        onclick={() => {
          confirming = false;
          ondelete(volume.name);
        }}>Delete</button
      ><button class="btn" type="button" onclick={() => (confirming = false)}>Keep</button>
    </div>
  {:else}
    <div class="actions">
      <span class="spacer"></span><button
        class="btn danger"
        type="button"
        disabled={volume.users.length > 0 || busy}
        title={volume.users.length ? `Mounted by ${listOf(volume.users.map((user) => user.node.name))}` : twin}
        onclick={() => (confirming = true)}>Delete</button
      >
    </div>
  {/if}
</div>
<div class="insp-scroll">
  <div class="sect">
    <h4>Mounted by <span>{volume.users.length}</span></h4>
    {#if volume.users.length}<div class="attlist">
        {#each volume.users as user (user.node.id + user.path)}<button class="attv" type="button" onclick={() => ongoto(user.node.id)}
            ><i class="dot {user.node.state}"></i><b>{user.node.name}</b><em>{user.path} · {user.mode === "ro" ? "read-only" : "read-write"}</em></button
          >{/each}
      </div>{:else}<p class="tnote">No container mounts this volume. Its data is kept until you delete it.</p>{/if}
    {#if volume.users.length > 1}<p class="tnote">
        Shared by {volume.users.length} containers; {writers === 0 ? "none can write" : writers === 1 ? "only one can write" : `${writers} can write`}.
      </p>{/if}
  </div>
  <div class="sect">
    <h4>Location</h4>
    <dl class="kv"><dt>{mac ? "Path in VM" : "Path"}</dt><dd>{volume.mountpoint}</dd><dt>Driver</dt><dd>local</dd></dl>
  </div>
  <div class="sect"><h4>Files</h4><pre class="files">{tree}</pre></div>
</div>
