import { test, expect, devices, type Page } from "@playwright/test";

// Regression for the soft-keyboard bug where the cockpit composer was
// rendered behind the keyboard instead of riding flush above it.
//
// Repro: focus the composer on iOS — the OS slides up the keyboard,
// visualViewport.height shrinks (sometimes innerHeight too), but the App
// root stays pinned to the latched stableViewportHeight so a flex-bottom
// composer ends up at the App-root-bottom, which is now below the
// keyboard top. iMessage / WhatsApp / ChatGPT all anchor the composer to
// the visual viewport bottom; we now do the same by padding the cockpit
// pane's bottom by `keyboardOffset` (gap between App root content-box
// bottom and visualViewport.bottom).
//
// The assertion: composer.getBoundingClientRect().bottom === vv.height +
// vv.offsetTop, within 1px. Anything else means the composer is behind
// the keyboard (positive delta) or hovering above it (negative delta).

test.use({ ...devices["iPhone 13"] });

const SESSION_ID = "cockpit-kb-test";

/** Mock just enough of the REST surface for the cockpit route to render
 *  the CockpitView + Composer. The cockpit WebSocket is allowed to fail;
 *  the StartupErrorBanner appears alongside the composer, which is the
 *  surface we want to measure. The composer's layout is independent of
 *  the WS message stream so this is sufficient for a layout regression. */
async function mockCockpitSession(page: Page) {
  await page.route("**/api/login/status", (r) =>
    r.fulfill({ json: { required: false, authenticated: true } }),
  );
  const session = {
    id: SESSION_ID,
    title: "cockpit-kb-test",
    project_path: "/tmp/cockpit-kb-test",
    group_path: "/tmp",
    tool: "claude",
    status: "Idle",
    yolo_mode: false,
    created_at: new Date().toISOString(),
    last_accessed_at: null,
    idle_entered_at: null,
    last_error: null,
    branch: null,
    main_repo_path: null,
    is_sandboxed: false,
    has_terminal: false,
    profile: "default",
    workspace_repos: [],
    cockpit_mode: true,
    cockpit_worker_state: "resuming",
  };
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() === "POST") return r.fulfill({ status: 400 });
    return r.fulfill({ json: [session] });
  });
  await page.route("**/api/sessions/cockpit-kb-test", (r) =>
    r.fulfill({ json: session }),
  );
  await page.route("**/api/sessions/*/ensure", (r) =>
    r.fulfill({ json: { ok: true } }),
  );
  await page.route("**/api/sessions/*/diff/files", (r) =>
    r.fulfill({ json: { files: [], per_repo_bases: [], warning: null } }),
  );
  await page.route("**/api/sessions/*/files", (r) =>
    r.fulfill({ json: { files: [] } }),
  );
  for (const path of [
    "settings",
    "themes",
    "agents",
    "profiles",
    "groups",
    "devices",
    "docker/status",
    "about",
  ]) {
    await page.route(`**/api/${path}`, (r) =>
      r.fulfill({ json: path === "docker/status" ? {} : [] }),
    );
  }
}

/** Simulate iOS-Safari-mode soft keyboard (innerHeight unchanged, vv
 *  shrinks) or iOS-PWA-mode (both shrink). Override the descriptors and
 *  fire visualViewport's resize so useMobileKeyboard re-measures. */
async function simulateKeyboard(
  page: Page,
  opts: { px: number; pwa?: boolean },
) {
  await page.evaluate(
    ({ px, pwa }) => {
      const vv = window.visualViewport!;
      const fullH = window.innerHeight;
      const newVvH = fullH - px;
      Object.defineProperty(vv, "height", {
        get: () => newVvH,
        configurable: true,
      });
      Object.defineProperty(vv, "offsetTop", {
        get: () => 0,
        configurable: true,
      });
      if (pwa) {
        Object.defineProperty(window, "innerHeight", {
          get: () => newVvH,
          configurable: true,
        });
      }
      vv.dispatchEvent(new Event("resize"));
    },
    { px: opts.px, pwa: opts.pwa ?? false },
  );
  // Two RAFs to let the rAF-driven re-measure loop in useMobileKeyboard
  // catch the resize and React flush the keyboardOffset state.
  await page.waitForTimeout(200);
}

async function readGeometry(page: Page) {
  return page.evaluate(() => {
    const ta = document.querySelector<HTMLTextAreaElement>(
      'textarea[name="input"]',
    );
    const composer = ta?.closest<HTMLElement>(".border-t.border-surface-800");
    const cockpit = ta?.closest<HTMLElement>(
      ".flex.h-full.flex-col.bg-surface-900",
    );
    const vv = window.visualViewport!;
    const composerRect = composer?.getBoundingClientRect();
    return {
      innerHeight: window.innerHeight,
      vvHeight: vv.height,
      vvOffsetTop: vv.offsetTop,
      composerBottom: composerRect?.bottom ?? null,
      composerTop: composerRect?.top ?? null,
      cockpitPaddingBottom: cockpit?.style?.paddingBottom ?? "",
      composerClasses: composer?.className ?? "",
    };
  });
}

async function gotoCockpit(page: Page) {
  await mockCockpitSession(page);
  await page.goto(`/session/${SESSION_ID}`);
  // Composer textarea is what we measure; it always renders for a
  // cockpit session regardless of the worker state. Other (hidden)
  // textareas live in the page for IME/proxy purposes; target the
  // composer specifically via its `name="input"` attribute.
  await page
    .locator('textarea[name="input"]')
    .waitFor({ state: "visible", timeout: 5_000 });
}

test.describe("Cockpit composer soft-keyboard layout", () => {
  test("keyboard closed: composer sits at the App root content-box bottom", async ({
    page,
  }) => {
    await gotoCockpit(page);
    const g = await readGeometry(page);
    // No keyboard, no offset padding, composer at the bottom of the
    // visual viewport (== innerHeight for the closed case).
    expect(g.cockpitPaddingBottom).toBe("");
    expect(g.composerClasses).toContain("pb-3");
    expect(g.composerBottom).not.toBeNull();
    // Allow up to 1px slop for sub-pixel rounding.
    expect(Math.abs(g.composerBottom! - g.vvHeight)).toBeLessThanOrEqual(1);
  });

  test("Safari mode (innerHeight unchanged): composer rides flush above the keyboard", async ({
    page,
  }) => {
    await gotoCockpit(page);
    await simulateKeyboard(page, { px: 300, pwa: false });
    const g = await readGeometry(page);
    // CockpitView added paddingBottom == keyboardOffset.
    expect(parseInt(g.cockpitPaddingBottom)).toBeGreaterThan(100);
    // Composer dropped its own pb-3 padding.
    expect(g.composerClasses).toContain("pb-0");
    // The contract: composer.bottom === vv.height + vv.offsetTop (±1px).
    const target = g.vvHeight + g.vvOffsetTop;
    expect(Math.abs(g.composerBottom! - target)).toBeLessThanOrEqual(1);
  });

  test("PWA mode (innerHeight shrinks with vv): composer still rides flush above the keyboard", async ({
    page,
  }) => {
    await gotoCockpit(page);
    await simulateKeyboard(page, { px: 300, pwa: true });
    const g = await readGeometry(page);
    expect(parseInt(g.cockpitPaddingBottom)).toBeGreaterThan(100);
    expect(g.composerClasses).toContain("pb-0");
    const target = g.vvHeight + g.vvOffsetTop;
    expect(Math.abs(g.composerBottom! - target)).toBeLessThanOrEqual(1);
  });

  test("keyboard close restores the baseline layout (no leftover padding)", async ({
    page,
  }) => {
    await gotoCockpit(page);
    await simulateKeyboard(page, { px: 300, pwa: false });
    const open = await readGeometry(page);
    expect(parseInt(open.cockpitPaddingBottom)).toBeGreaterThan(100);

    // Restore vv.height to the natural getter.
    await page.evaluate(() => {
      const vv = window.visualViewport!;
      const proto = Object.getPrototypeOf(vv);
      const orig = Object.getOwnPropertyDescriptor(proto, "height");
      if (orig) Object.defineProperty(vv, "height", orig);
      vv.dispatchEvent(new Event("resize"));
    });
    await page.waitForTimeout(200);

    const closed = await readGeometry(page);
    expect(closed.cockpitPaddingBottom).toBe("");
    expect(closed.composerClasses).toContain("pb-3");
    expect(Math.abs(closed.composerBottom! - closed.vvHeight)).toBeLessThanOrEqual(1);
  });
});
