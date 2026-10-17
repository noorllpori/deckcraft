# High Performance Mode

Updated: 2026-10-18. The maintained branch is `performance`; see the full
[capabilities and performance notes](performance-edition.md) for scope and limitations.

Use the **High Performance** toggle in the top ribbon. The preference persists across launches.
On desktop, the editor renders the current canvas on a worker and retains an available frame
while a replacement is prepared. The normal performance canvas has a nominal budget of
1,800,000 pixels / 2048 px longest side; the drag proxy uses 360,000 pixels / 960 px.
The complete proxy respects object stacking. Thumbnail retention is capped at 24 entries
instead of the ordinary mode's 600. These are cache/render policies, not measured speedups
or a cap on total process memory; the document, original media and undo history remain loaded.

Editor proxies can appear softer at high zoom and do not replace source image bytes. Native
workers are unavailable in the web path. The underlying slide renderer remains CPU based.

Slide-show precompilation is a separate feature: screen-sized static/animation frames are
prepared in RAM, with identical tiles shared. Current limits include a 30 fps animation sampling
rate, 2560 px longest output side, and a 2048 MiB playback-cache budget. Uncached pages and
live media have rendering fallbacks. Preparation costs time and memory; no constant playback
frame rate or performance percentage has been established by a reproducible benchmark.
