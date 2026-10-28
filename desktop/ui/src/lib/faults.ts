// What went wrong in the page, kept from before any other module loads
// (step 14.7): imported first by main.ts, so a library that throws as it's
// evaluated (xterm.js under a frozen prototype left the window blank, 14.5)
// or a style the CSP refuses is counted. The Linux UI test reads it from the
// real window (`window.shipyardFaults`); nothing in the app does.
const faults: string[] = [];
(window as unknown as { shipyardFaults: string[] }).shipyardFaults = faults;
window.addEventListener("error", (event) => faults.push(`error: ${event.message}`));
window.addEventListener("unhandledrejection", (event) => faults.push(`rejection: ${String(event.reason)}`));
document.addEventListener("securitypolicyviolation", (event) => faults.push(`csp: ${event.effectiveDirective} ${event.blockedURI}`));
