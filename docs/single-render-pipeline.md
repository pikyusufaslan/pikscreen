# Single-render recording pipeline

## Goal

PikScreen must open the editor without first encoding a presentation-ready
video. The capture source and its sidecar data are the editable project. A
final MP4 is encoded only after the user chooses **Save as MP4**.

## Recording stop

Stopping a recording performs only the work required to make the capture safe
to edit:

1. Stop and finalize the video, audio, and optional webcam recorders.
2. Validate the source dimensions and required cursor samples.
3. Copy the source tracks and the zoom, cursor, click, and settings metadata
   into an editor session.
4. Open the editor.

This stage does not composite a background, animate zooms, draw a cursor, add
click effects, trim the timeline, or encode a final MP4.

## Editor

The editor reads the untouched source tracks and renders a non-destructive
preview in the webview. Timeline edits update only the in-memory editor model
until export. The manifest keeps the latest edit state so an export can be
reproduced from the source media.

## Export

**Save as MP4** first asks for a destination. Canceling the dialog performs no
render. After a destination is chosen, PikScreen validates and saves the
current edit state, renders one final MP4 from the preserved source tracks, and
copies that result to the chosen destination.

Render failures keep the editor session and source files intact and expose the
backend error to the editor. A later export can retry safely.

## Ownership and cleanup

The editor session owns its copied source media, sidecars, manifest, and
temporary export. Discarding the session removes all of them. Temporary capture
files outside the session may be deleted only after the session copy succeeds.
