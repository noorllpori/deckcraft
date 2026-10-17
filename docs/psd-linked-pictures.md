# PSD linked pictures — performance edition

Status: implemented, build verified; real-file and UI validation pending with the contributor.
Updated: 2026-10-18.

Desktop Insert Picture and drag/drop accept `.psd`. A local PSD defaults to an absolute
external path, with source bytes cached in memory for rendering. PNG/JPEG and other ordinary
image defaults remain embedded. Byte-only imports (including the web) are embedded because
there is no persistent local source path. `insert.picture` accepts `linked: false` to override
the PSD default, or `linked: true` with a `path` for an explicitly linked picture.

The supported PSD v1 merged image is RGB or grayscale, 8 or 16 bits/channel, using raw,
PackBits RLE, ZIP, or ZIP with prediction. Layers are not converted to slide objects.
Save a compatible merged image in the source editor. The decoder bounds both planar and
RGBA allocations to 256 MiB each and rejects unsupported or malformed input. CMYK, Lab,
indexed color, 32-bit, PSB, and ICC-managed color conversion are not implemented.

Multi-Function shows the source path. After saving the PSD in Photoshop, click the picture
or its selection-pane entry (including an already selected entry) to check for changes.
The desktop app reads and hashes the file in one background worker; only changed content
is decoded and applied. SHA-256 fingerprints are stored with linked media in the native
manifest, initialized on insertion/extraction/open, and updated on refresh. Byte-identical
files do not change the document, undo history, or render caches. The Refresh button uses
the same background check. Failed or in-progress file writes retain the current preview;
click again after the source has finished saving. Results are deferred during dragging and
discarded if their document, link, or cached bytes no longer match. There is no continuous
file watcher. Embed copies the original PSD into the
presentation and removes the link. Extract writes the original PSD bytes, not a PNG proxy.

Native `.deckcraft` saves links without embedding their bytes, and reloads the source on open.
Moving/deleting the source breaks the link; there is no portable fallback or relink UI yet.
PPTX retains the PSD link/original media for this fork. Other presentation applications may
not render PSD media; a PNG compatibility fallback is not implemented. For portable exchange,
use a PNG image instead. This is PSD picture support, not a PSD document/layer editor.

## Revision history

| Date | Change |
|---|---|
| 2026-10-17 | Added merged PSD decoding, local link defaults, and refresh cache invalidation. |
| 2026-10-18 | Added persisted SHA-256 and background refresh when selecting or re-clicking linked pictures. Build verification only; user validation pending. |
