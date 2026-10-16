# High Performance Mode

DeckCraft's editor has an optional high performance mode for presentations whose pages contain
large pictures, many effects, or a large number of slides.

The mode is available from **View > High Performance Mode** and from the status bar. It is a
persisted UI preference, so it remains enabled after restarting the desktop application.

When enabled, the editor:

- renders only the selected page's canvas texture and releases the previous page when navigation
  moves to a different page;
- limits the canvas texture to an adaptive screen-sized budget (about 1.8 million pixels and a
  2048-pixel maximum side), so zooming in does not allocate a texture for the entire logical zoom;
- renders the active page on a native worker thread and repaints when it is ready, keeping pointer
  and page navigation responsive while the page is being prepared;
- uses a compressed proxy canvas while an object is dragged, keeping the moving page visible while
  the next proxy frame is prepared, then queues the normal adaptive render after release;
- keeps a small, low-resolution thumbnail cache instead of retaining hundreds of page textures.

The document model, saved presentation, export output, and slide-show rendering are unchanged.
The mode is an editor responsiveness trade-off: a very high zoom can look softer until the mode is
turned off. Web builds keep the setting but use the existing synchronous renderer because native
worker threads are unavailable there.
