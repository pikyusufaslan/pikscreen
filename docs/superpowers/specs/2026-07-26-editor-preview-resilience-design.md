# PikScreen Editor Preview Resilience

## Goal

Keep the editor preview responsive and frame-accurate after long idle periods while reducing the amount of empty space around the video.

This work changes only the non-destructive editor preview. It does not alter the preserved recording source or the final export pipeline.

## Current Problems

- The same preview state is updated by both `timeupdate` and a separate animation-frame loop during playback.
- The cursor, click effects, camera transform, DOM, audio, and webcam synchronization are all reconsidered on every animation frame.
- Click lookup scans the complete click sample list and recreates DOM nodes for every preview update.
- Sidecar audio and webcam elements are repeatedly seeked during normal playback.
- A media element left idle for a long time has no recovery path if WebKit suspends or loses its decoder state.
- The preview stage occupies all remaining editor height even when the actual video is much smaller, leaving a large empty region.
- Long-lived observers, animation callbacks, and Tauri listeners do not have a single teardown path.

## Chosen Approach

Retain native media elements and make the main screen video the authoritative playback clock.

### Frame Scheduling

- Prefer `HTMLVideoElement.requestVideoFrameCallback` when available.
- Fall back to one `requestAnimationFrame` loop only when video-frame callbacks are unavailable.
- Use `timeupdate` only as a low-frequency paused-state/timeline fallback, not as a second playback renderer.
- Guarantee that only one preview frame scheduler can be active.
- Stop scheduling when the video pauses, ends, the document becomes hidden, or the editor is torn down.

### Preview Work Per Frame

- Cache the zoom camera track and rebuild it only after zoom data changes.
- Maintain a moving index for cursor and click samples instead of filtering the full sample arrays every frame.
- Reuse click-pulse elements and update their state instead of replacing the click layer each frame.
- Separate static scene styling from frame-dependent styling. Background, webcam appearance, and cursor asset selection update only after their settings change.
- Avoid layout reads such as `clientHeight` during the playback loop. Cache preview geometry after resize.

### Audio and Webcam Synchronization

- The screen video remains the master clock.
- Synchronize sidecars on load, explicit seeking, resume, and when drift exceeds a conservative threshold.
- During ordinary playback, correct small drift through playback rate where supported instead of repeatedly assigning `currentTime`.
- Pause all sidecars when the master video pauses or the document becomes hidden.

### Idle Recovery

- Record the last time the editor preview was active.
- When Play is requested after a long idle period, inspect the media element state.
- If the element is stalled, errored, has no decoded data, or has been idle beyond the recovery threshold:
  1. preserve the current timeline position and mute state;
  2. stop all frame and sidecar activity;
  3. reload the same review URL;
  4. wait for metadata and a playable state with a bounded timeout;
  5. restore the timeline position and resume playback.
- Show a short `Refreshing preview…` status during recovery.
- If recovery fails, keep the source session intact and show an actionable inline preview error.

### Lifecycle Cleanup

- Own the resize observer, Tauri progress listener, media callbacks, and document/window listeners through one editor cleanup registry.
- Teardown cancels frame callbacks, disconnects observers, unregisters Tauri listeners, pauses media, and clears transient click nodes.
- Cleanup runs on page hide and before the editor window is destroyed.

## Compact Preview Layout

- The preview column no longer stretches the preview stage across all available height.
- The scene viewport is constrained to the recording aspect ratio and a maximum height of `min(54vh, 560px)`.
- The preview column sizes its stage to the scene viewport instead of displaying an empty full-height canvas.
- Playback controls sit directly below the scene.
- The timeline receives the reclaimed vertical space and remains usable at smaller window sizes.
- The source scene, background, camera inset, zoom behavior, and exported framing remain unchanged.

## Error Handling

- Media recovery uses a timeout so Play cannot remain permanently pending.
- A failed sidecar does not stop screen-video playback.
- A failed screen preview never deletes or rewrites the recording source.
- Repeated Play clicks while recovery is active share the same recovery operation.

## Verification

- Unit-test sample index lookup and idle-recovery decision logic as pure functions.
- Test that scheduler state permits only one active loop.
- Test lifecycle cleanup idempotence.
- Run all Rust tests and the frontend production build.
- Manually verify:
  - immediate playback;
  - pause/resume;
  - seeking and timeline dragging;
  - playback after at least the configured idle threshold;
  - background, zoom, cursor, click, audio, and webcam preview;
  - compact scene sizing at 1920×1080 and a smaller editor window.

## Non-Goals

- No proxy-video generation.
- No canvas/WebGL decoder rewrite.
- No export quality or codec changes.
- No destructive changes to source recordings.
