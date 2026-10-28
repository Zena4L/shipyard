// What goes wrong is kept from the first line (the Linux UI test reads it).
import "./lib/faults";
// The mock's fonts at the mock's weights, bundled: nothing is fetched.
import "@fontsource/big-shoulders-stencil-display/600";
import "@fontsource/big-shoulders-stencil-display/800";
import "@fontsource/hanken-grotesk/400";
import "@fontsource/hanken-grotesk/500";
import "@fontsource/hanken-grotesk/600";
import "@fontsource/hanken-grotesk/700";
import "@fontsource/ibm-plex-mono/400";
import "@fontsource/ibm-plex-mono/500";
import "./lib/design.css";
import "./app.css";
import { mount } from "svelte";
import App from "./App.svelte";
import { tauriBackend } from "./lib/backend";

const target = document.getElementById("app");
if (target) mount(App, { target, props: { backend: tauriBackend } });
