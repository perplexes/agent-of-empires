// Layout-decision tests for the cockpit composer's outer wrapper. The
// pure helper lets us check the className + inline style across
// keyboard-open / keyboard-closed without mounting the whole composer +
// assistant-ui runtime.

import { describe, expect, it } from "vitest";

import { composerWrapperLayout } from "./Composer";

describe("composerWrapperLayout", () => {
  it("uses pb-3 and no inline style when the soft keyboard is closed", () => {
    const layout = composerWrapperLayout({ keyboardOpen: false });
    expect(layout.className).toContain("pb-3");
    expect(layout.className).not.toContain("pb-0");
    expect(layout.style).toBeUndefined();
  });

  it("drops to pb-0 when the keyboard is open so the composer butts up against the keyboard top", () => {
    // CockpitView pads its root by keyboardOffset to lift the composer above
    // the keyboard; the composer's own pb-3 would re-introduce a visible
    // gap between the composer border and the keyboard top, so drop it.
    const layout = composerWrapperLayout({ keyboardOpen: true });
    expect(layout.className).toContain("pb-0");
    expect(layout.className).not.toContain("pb-3");
    expect(layout.style).toBeUndefined();
  });

  it("preserves shared base classes regardless of keyboard state", () => {
    for (const keyboardOpen of [true, false]) {
      const layout = composerWrapperLayout({ keyboardOpen });
      expect(layout.className).toContain("border-t");
      expect(layout.className).toContain("border-surface-800");
      expect(layout.className).toContain("bg-surface-900");
      expect(layout.className).toContain("px-4");
      expect(layout.className).toContain("pt-3");
    }
  });
});
