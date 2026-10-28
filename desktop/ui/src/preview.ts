// The app with the mock's own data (src/lib/fixture.ts), at the mock's
// time: what the visual test compares with the mock. The dev server's page
// alone: the app's build is index.html.
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
import { NOW, fixtureBackend } from "./lib/fixture";

const target = document.getElementById("app");
if (target) mount(App, { target, props: { backend: fixtureBackend, now: () => NOW } });
