# PikScreen surface design

## Scope

Apply the approved visual system to the working PikScreen surfaces without changing the capture or export protocol.

## HUD

- A compact pure-black bottom-center controller.
- Ready state shows a source picker, audio controls, a left-side drag handle and a circular red record action.
- Recording state contracts to its timer, audio controls and a red stop action.
- The existing native window/input recovery remains authoritative; this work only changes markup styling and visual state.

## Settings

- Keep the dedicated normal window and its sections: Recording, Appearance, Audio, Capture and Camera.
- Use the PikScreen wordmark, neutral dark surfaces, narrow navigation and no blue-led visual system.
- Retain current settings IDs and behavior so automatic persistence, portal selection and media controls continue to work.

## Editor

- Keep the normal editor window.
- Make the left rail a narrow, attached, full-height sidebar from beneath the topbar to the timeline.
- The rail opens a small floating tool panel beside it; the panel does not reserve a third persistent column or shrink the preview.
- Scene controls background/frame choices; Cursor controls visibility, scale, smoothing and clicks; Zoom controls the selected marker; Audio controls recorded tracks. Camera remains available as an optional tool.
- Keep the preview and compact two-lane timeline as the primary workspace. Existing marker dragging, trimming, playback and export commands remain unchanged.

## Acceptance checks

- Existing element IDs and TypeScript event bindings stay intact.
- HUD remains native-clickable in Tauri after visual changes.
- Editor preview gets its width from the expanded two-column workspace.
- `npm run build` and `cargo check` pass before manual GUI verification.
