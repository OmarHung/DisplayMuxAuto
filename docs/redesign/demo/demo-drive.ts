/**
 * Development only: puts the demo into a state named by the URL, so a
 * screenshot can be taken without clicking. Runs after the app has rendered.
 *
 *   page  = settings        open the settings page
 *   tab   = displays | hosts | startup | appearance | help | reset
 *   look  = <route id>      open that host's icon and colour picker
 *   scrollTo = <selector>   scroll that element into view
 */
const params = new URLSearchParams(location.search);

function whenReady(run: () => void): void {
  const started = Date.now();
  const tick = () => {
    if (document.querySelector("#switch-panel")?.children.length || Date.now() - started > 3000) run();
    else setTimeout(tick, 50);
  };
  tick();
}

whenReady(() => {
  if (params.get("page") === "settings") document.querySelector<HTMLElement>('[data-page="settings"]')?.click();
  const tab = params.get("tab");
  if (tab) document.querySelector<HTMLElement>(`[data-settings-tab="${CSS.escape(tab)}"]`)?.click();
  const look = params.get("look");
  if (look) document.querySelector<HTMLElement>(`[data-edit-look="${CSS.escape(look)}"]`)?.click();
  const target = params.get("scrollTo");
  if (target) {
    setTimeout(() => {
      const element = document.querySelector<HTMLElement>(target);
      // Leave room for the floating top bar above the section title.
      element?.style.setProperty("scroll-margin-top", "96px");
      element?.scrollIntoView({ block: "start" });
    }, 50);
  }
  (document.activeElement as HTMLElement | null)?.blur();
  document.documentElement.dataset.demoReady = "true";
});
