# PikScreen landing page

Date: 2026-07-29

## What it is for

One page that makes a visitor understand what PikScreen is within a few
seconds, shows it working, and sends them to the repository. There is no
packaged download yet, so asking for one would waste the visit; the repository
is the honest destination.

## Constraints

- Early alpha. The page says so, and says what does not work yet. A page that
  oversells a tool the visitor then cannot run costs more than it gains.
- Linux and Wayland only.
- AGPL-3.0, forked from Recordly. The credit is on the page, not only in the
  licence file.
- No screenshots exist in the repository yet. The page is built around real
  media, with the demo captured using PikScreen itself, and reads properly
  until that media lands.

## Structure

1. **Hero** — wordmark, the positioning line, one sentence on what makes it
   different: zoom moves are placed while recording and refined afterwards
   without touching the take. Primary action to the repository, secondary to
   the walkthrough below. An alpha badge sits with it rather than hidden.
2. **Demo** — the largest block on the page: a recording being directed, then
   refined in the editor. This is the whole argument; everything else supports
   it.
3. **What is different** — three short blocks: directing during the take, a
   non-destructive editor, real window capture on Wayland.
4. **How it works** — record, refine, export.
5. **Where it stands** — what works, what does not yet. Same tone as the
   README.
6. **Footer** — licence, the fork's origin, repository.

## Look

The page borrows the application's own surfaces rather than a generic template:
the same near-black backgrounds, the same blue accent, the same rounded panels
and hairline borders. Someone who has seen the editor should recognise the page
as the same thing. The filled button sits one step deeper than the interface
accent so white label text clears WCAG AA against it.

The corner brackets from the wordmark frame every piece of media, which is also
the shape of the capture border the recorder draws.

Dark only, as the application is.

## Build

A single static page: `docs/index.html` with `docs/landing.css`, no build step
and no external requests, so GitHub Pages can serve it straight from `docs/` on
the default branch. Type comes from the system stack. `docs/media/` holds the
wordmark and a square mark cropped from it, since Pages publishes only the
`docs/` tree and cannot reach `src/assets`. A `.nojekyll` file keeps Pages from
running the files through Jekyll.

Icons are real Phosphor Regular glyphs, the set the application already uses,
inlined as SVG or as data URI masks rather than loaded from a CDN.

Serving from `docs/` also publishes the specs beside it. They are already public
in the repository, so nothing new is exposed.

## Media slots

- Hero demo: one looping capture, muted, no controls, with a still frame as its
  poster so the block is never empty.
- Two supporting stills: the editor with a zoom selected, and the recording HUD.

Until they exist the slots hold a labelled placeholder that states what belongs
there, so an unfinished page still reads as deliberate.

## Done when

The page renders correctly from a file and over Pages, at desktop and phone
widths, makes no external requests, and states the project's real status.
