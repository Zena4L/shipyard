// Takes the design's stylesheet from the mock (docs/design/shipyard-ui.html)
// into src/lib/design.css, whole: the app is the mock, pixel for pixel.
import { readFileSync, writeFileSync } from "node:fs";

const mock = readFileSync(new URL("../../docs/design/shipyard-ui.html", import.meta.url), "utf8");
const start = mock.indexOf("<style>\n  :root {");
const end = mock.indexOf("</style>", start);
if (start < 0 || end < 0) throw new Error("the mock's stylesheet isn't where it was");
const header = `/* The design's stylesheet, taken whole from docs/design/shipyard-ui.html
   (step 14.2: the app is the mock, pixel for pixel; the visual test compares
   them). Never edited here: change the mock, then run
   \`npm run design-css\` to take it again. The app's own differences (the
   window fills the app's window) are in app.css, after it. */
`;
writeFileSync(new URL("./src/lib/design.css", import.meta.url), header + mock.slice(start + 7, end).trim() + "\n");
