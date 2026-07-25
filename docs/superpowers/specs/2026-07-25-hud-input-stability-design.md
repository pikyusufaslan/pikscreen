# HUD input stability

## Problem

The HUD currently resets Wayland input by enabling click-through and immediately
disabling it again. Page-load and surface-change retries can overlap, leaving
the native surface with an empty input region even though the web UI is ready.

## Design

HUD input recovery is idempotent:

- keep the native window enabled;
- only request `ignore_cursor_events(false)`;
- write the measured HUD rectangle directly to GTK's native input region on
  Wayland instead of relying on Tao to clear a stale region;
- reapply that state after page load, surface changes, and HUD restoration;
- retry after mapping without ever entering click-through mode;
- log retry failures instead of silently discarding them.

The transparent visual regions remain transparent, but the HUD's native surface
always accepts pointer input. Capture exclusion remains controlled separately
through the compositor integration and is not coupled to click-through state.

## Verification

Run the Rust test suite and frontend build, restart the dev app, then activate
the Source control with a real pointer event. Repeat after opening Settings and
returning to the HUD to cover the surface transition that previously triggered
the race.
