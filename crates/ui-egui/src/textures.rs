//! Rendered slides as GPU textures: the canvas slide and thumbnails, cached by the slide's and its
//! master's `Arc` identity (an unchanged slide keeps its `Arc`, so it never re-renders).
//!
//! The canvas texture covers the slide plus whatever of the slide's drawing reaches off it and is
//! in view (PowerPoint's Normal view shows off-slide objects; thumbnails clip to the slide).

use std::collections::HashMap;
use std::sync::{
    Arc,
    mpsc::{self, Receiver, TryRecvError},
};

use deckcraft_geom::Rect;
use deckcraft_model::{Master, Presentation, ShapeId, Slide};
use deckcraft_render::{Image, Region, RenderOpts};
use egui::{ColorImage, TextureHandle, TextureOptions};

#[derive(Clone, PartialEq)]
struct Key {
    slide: usize,
    master: usize,
    size: (u32, u32),
    edit: bool,
    language: crate::i18n::Language,
    extra: u64,
    /// Dragging uses a small proxy canvas so the moving page never goes blank.
    proxy: bool,
    skip: Vec<ShapeId>,
    /// The pasteboard region the slide's drawing calls for (canvas only).
    want: Region,
}

struct Entry {
    key: Key,
    tex: TextureHandle,
    /// The region the texture covers (at least `key.want`).
    region: Region,
    // Keep the Arcs alive so their addresses can't be reused while cached.
    _slide: Arc<Slide>,
    _master: Option<Arc<Master>>,
    stamp: u64,
}

struct RenderedCanvas {
    image: Image,
    drawn: Rect,
    elapsed_ms: f64,
}

struct PendingCanvas {
    key: Key,
    receiver: Receiver<RenderedCanvas>,
}

#[derive(Clone, PartialEq)]
struct DragShapeKey {
    slide: usize,
    id: ShapeId,
    size: (u64, u64),
    scale: u64,
}

struct DragShapeEntry {
    key: DragShapeKey,
    rect: Rect,
    tex: TextureHandle,
}

#[derive(Default)]
pub struct Textures {
    canvas: Option<Entry>,
    /// How far the canvas slide's shapes reach (from their boxes), for the slide and master `Arc`s
    /// held (kept alive so their addresses can't be reused).
    estimate: Option<(Arc<Slide>, Option<Arc<Master>>, Rect)>,
    /// How far the last canvas render of a slide (by index) actually drew: text spilling out of
    /// its box shows there.
    drawn: Option<(usize, Rect)>,
    /// Canvas renders so far (tests, performance checks).
    pub canvas_renders: u64,
    thumbs: HashMap<usize, Entry>,
    clock: u64,
    pub last_render_ms: f64,
    performance_mode: bool,
    pending: Option<PendingCanvas>,
    proxy_texture_id: u64,
    drag_snapshot: bool,
    drag_shape: Option<DragShapeEntry>,
}

const HIGH_PERFORMANCE_MAX_PIXELS: u64 = 1_800_000;
const HIGH_PERFORMANCE_MAX_SIDE: u32 = 2_048;
const DRAG_PROXY_MAX_PIXELS: u64 = 360_000;
const DRAG_PROXY_MAX_SIDE: u32 = 960;

pub fn to_color_image(img: &Image, grayscale: bool) -> ColorImage {
    let w = img.width as usize;
    let h = img.height as usize;
    let mut px = Vec::with_capacity(w * h);
    for c in img.pixels.as_chunks::<4>().0 {
        let (r, g, b, a) = (c[0], c[1], c[2], c[3]);
        if grayscale {
            let y = ((r as u32 * 77 + g as u32 * 150 + b as u32 * 29) >> 8) as u8;
            px.push(egui::Color32::from_rgba_premultiplied(y, y, y, a));
        } else {
            px.push(egui::Color32::from_rgba_premultiplied(r, g, b, a));
        }
    }
    px.resize(w * h, egui::Color32::TRANSPARENT);
    ColorImage::new([w, h], px)
}

fn master_of(p: &Presentation, slide: &Slide) -> Option<Arc<Master>> {
    p.masters.iter().find(|m| m.layout(slide.layout).is_some()).cloned()
}

fn same(a: &Option<Arc<Master>>, b: &Option<Arc<Master>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

fn threads() -> u16 {
    if cfg!(target_arch = "wasm32") { 0 } else { std::thread::available_parallelism().map(|n| n.get().min(8) as u16).unwrap_or(0) }
}

impl Textures {
    pub fn is_performance_mode(&self) -> bool {
        self.performance_mode
    }

    /// Enable the responsive editor path. Changing the mode drops GPU textures and any in-flight
    /// canvas work so the next frame starts from one active slide.
    pub fn set_performance_mode(&mut self, enabled: bool) {
        if self.performance_mode == enabled {
            return;
        }
        self.performance_mode = enabled;
        self.canvas = None;
        self.pending = None;
        self.drag_snapshot = false;
        self.drag_shape = None;
        self.estimate = None;
        self.drawn = None;
        if enabled {
            self.thumbs.clear();
        }
    }

    /// The high-performance canvas is sized to the visible monitor budget rather than the full
    /// logical zoom. This keeps a 400% zoom from allocating a huge texture while still adapting
    /// to ordinary and high-DPI windows.
    pub fn canvas_size(&self, size: (u32, u32)) -> (u32, u32) {
        if !self.performance_mode {
            return size;
        }
        let (w, h) = size;
        let area = u64::from(w).saturating_mul(u64::from(h));
        let side = w.max(h);
        if area <= HIGH_PERFORMANCE_MAX_PIXELS && side <= HIGH_PERFORMANCE_MAX_SIDE {
            return size;
        }
        let area_scale = (HIGH_PERFORMANCE_MAX_PIXELS as f64 / area.max(1) as f64).sqrt();
        let side_scale = HIGH_PERFORMANCE_MAX_SIDE as f64 / side.max(1) as f64;
        let factor = area_scale.min(side_scale).min(1.0);
        (((w as f64 * factor).round() as u32).max(1), ((h as f64 * factor).round() as u32).max(1))
    }

    /// A deliberately small canvas used while an object is being dragged. It is a visual proxy,
    /// not the final render; once the pointer is released, the normal adaptive canvas is queued.
    pub fn proxy_canvas_size(&self, size: (u32, u32)) -> (u32, u32) {
        if !self.performance_mode {
            return size;
        }
        let (w, h) = size;
        let area = u64::from(w).saturating_mul(u64::from(h));
        let side = w.max(h);
        if area <= DRAG_PROXY_MAX_PIXELS && side <= DRAG_PROXY_MAX_SIDE {
            return size;
        }
        let area_scale = (DRAG_PROXY_MAX_PIXELS as f64 / area.max(1) as f64).sqrt();
        let side_scale = DRAG_PROXY_MAX_SIDE as f64 / side.max(1) as f64;
        let factor = area_scale.min(side_scale).min(1.0);
        (((w as f64 * factor).round() as u32).max(1), ((h as f64 * factor).round() as u32).max(1))
    }

    /// The canvas texture for slide `index`, the slide being `size` pixels, and the region it
    /// covers: the slide, grown over the pasteboard to show what reaches off the slide within
    /// `visible` (the canvas, in pixels from the slide's top-left corner). `extra` changes with the
    /// GIF frames.
    #[allow(clippy::too_many_arguments)]
    pub fn slide(
        &mut self,
        ctx: &egui::Context,
        p: &Presentation,
        index: usize,
        size: (u32, u32),
        visible: Region,
        edit: bool,
        extra: u64,
        grayscale: bool,
        gif_times: &[(ShapeId, f64)],
        proxy: bool,
        skip: &[ShapeId],
    ) -> Option<(TextureHandle, Region)> {
        let slide = p.slides.get(index)?.clone();
        let master = master_of(p, &slide);
        let scale = size.0 as f64 / p.slide_size.width.max(1.0);
        let estimate = match &self.estimate {
            Some((s, m, r)) if Arc::ptr_eq(s, &slide) && same(m, &master) => *r,
            _ => {
                let r = deckcraft_render::slide_extent(p, index);
                self.estimate = Some((slide.clone(), master.clone(), r));
                r
            }
        };
        let reach = match self.drawn {
            Some((i, d)) if i == index => estimate.union(d),
            _ => estimate,
        };
        let want = deckcraft_render::pasteboard_region(size, scale, reach, visible);
        if !proxy {
            self.drag_snapshot = false;
            self.drag_shape = None;
        }
        let key = Key {
            slide: Arc::as_ptr(&slide) as usize,
            master: master.as_ref().map(|m| Arc::as_ptr(m) as usize).unwrap_or(0),
            size,
            edit,
            language: if edit { crate::i18n::language(ctx) } else { crate::i18n::Language::English },
            extra: extra ^ grayscale as u64,
            proxy,
            skip: skip.to_vec(),
            want,
        };
        if proxy && !skip.is_empty() && self.drag_snapshot {
            return self.canvas.as_ref().map(|e| (e.tex.clone(), e.region));
        }
        if proxy && !skip.is_empty() {
            let t0 = crate::now_ms();
            let opts = RenderOpts {
                scale,
                edit,
                gif_times,
                threads: threads(),
                offset: want.offset(),
                size: Some(want.size()),
                skip,
                placeholder_prompt: crate::i18n::placeholder_prompt(ctx),
                ..Default::default()
            };
            let (image, drawn) = deckcraft_render::render_slide_drawn(p, index, &opts);
            self.commit_canvas(ctx, key, slide, master, want, RenderedCanvas { image, drawn, elapsed_ms: crate::now_ms() - t0 }, grayscale, index);
            self.drag_snapshot = true;
            return self.canvas.as_ref().map(|e| (e.tex.clone(), e.region));
        }
        if self.performance_mode {
            if let Some(pending) = self.pending.take() {
                match pending.receiver.try_recv() {
                    Ok(rendered) if pending.key == key => {
                        self.commit_canvas(ctx, key.clone(), slide.clone(), master.clone(), want, rendered, grayscale, index);
                    }
                    Ok(rendered) if proxy && pending.key.proxy => {
                        // A drag changes the slide Arc on every pointer event. Keep the most
                        // recent completed proxy visible while the next compressed frame catches
                        // up instead of exposing an empty canvas.
                        let pending_key = pending.key.clone();
                        let pending_region = pending_key.want;
                        self.commit_canvas(ctx, pending_key, slide.clone(), master.clone(), pending_region, rendered, grayscale, index);
                    }
                    Ok(_) => {}
                    Err(TryRecvError::Empty) => self.pending = Some(pending),
                    Err(TryRecvError::Disconnected) => {}
                }
            }
        }
        if let Some(e) = &self.canvas
            && e.key == key
        {
            return Some((e.tex.clone(), e.region));
        }
        if self.performance_mode {
            if self.pending.is_none() {
                // Never show a different page while the newly selected page is rendering. The
                // old page's texture is released here, keeping the active-page memory bound.
                if !proxy && self.canvas.as_ref().is_some_and(|e| e.key.slide != key.slide) {
                    self.canvas = None;
                }
                self.start_async_canvas(p, index, key.clone(), scale, edit, gif_times, crate::i18n::placeholder_prompt(ctx));
            }
            ctx.request_repaint();
            return self.canvas.as_ref().map(|e| (e.tex.clone(), e.region));
        }
        let t0 = crate::now_ms();
        let render = |region: Region| {
            let opts = RenderOpts {
                scale,
                edit,
                gif_times,
                threads: threads(),
                offset: region.offset(),
                size: Some(region.size()),
                placeholder_prompt: crate::i18n::placeholder_prompt(ctx),
                ..Default::default()
            };
            deckcraft_render::render_slide_drawn(p, index, &opts)
        };
        let (mut img, drawn) = render(want);
        self.canvas_renders += 1;
        let mut region = want;
        // Text spilling out of its box can reach further than the boxes said. Once drawn, the
        // region it calls for is known (and is next frame's key): render again only if it shows
        // more than this render covered.
        let key = Key { want: deckcraft_render::pasteboard_region(size, scale, estimate.union(drawn), visible), ..key };
        if !region.contains(&key.want) {
            region = key.want;
            img = render(region).0;
            self.canvas_renders += 1;
        }
        self.drawn = Some((index, drawn));
        self.last_render_ms = crate::now_ms() - t0;
        let ci = to_color_image(&img, grayscale);
        let tex = match self.canvas.take() {
            Some(mut e) => {
                e.tex.set(ci, TextureOptions::LINEAR);
                e.tex
            }
            None => ctx.load_texture("slide-canvas", ci, TextureOptions::LINEAR),
        };
        self.canvas = Some(Entry { key, tex: tex.clone(), region, _slide: slide, _master: master, stamp: 0 });
        Some((tex, region))
    }

    fn commit_canvas(
        &mut self,
        ctx: &egui::Context,
        key: Key,
        slide: Arc<Slide>,
        master: Option<Arc<Master>>,
        region: Region,
        rendered: RenderedCanvas,
        grayscale: bool,
        index: usize,
    ) {
        self.canvas_renders += 1;
        self.drawn = Some((index, rendered.drawn));
        self.last_render_ms = rendered.elapsed_ms;
        let ci = to_color_image(&rendered.image, grayscale);
        if key.proxy {
            // Do not mutate the texture currently being painted. Upload a complete proxy to a
            // new GPU texture first, then replace the entry in one step; this removes the visible
            // blank frame caused by TextureHandle::set during rapid pointer drags.
            self.proxy_texture_id = self.proxy_texture_id.wrapping_add(1);
            let tex = ctx.load_texture(format!("slide-canvas-proxy-{}", self.proxy_texture_id), ci, TextureOptions::LINEAR);
            self.canvas = Some(Entry { key, tex, region, _slide: slide, _master: master, stamp: 0 });
            return;
        }
        let tex = match self.canvas.take() {
            Some(mut e) => {
                e.tex.set(ci, TextureOptions::LINEAR);
                e.tex
            }
            None => ctx.load_texture("slide-canvas", ci, TextureOptions::LINEAR),
        };
        self.canvas = Some(Entry { key, tex, region, _slide: slide, _master: master, stamp: 0 });
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn start_async_canvas(
        &mut self,
        p: &Presentation,
        index: usize,
        key: Key,
        scale: f64,
        edit: bool,
        gif_times: &[(ShapeId, f64)],
        placeholder_prompt: Option<fn(deckcraft_model::PhType) -> &'static str>,
    ) {
        let (sender, receiver) = mpsc::channel();
        let presentation = p.clone();
        let times = gif_times.to_vec();
        let region = key.want;
        std::thread::spawn(move || {
            let t0 = crate::now_ms();
            let opts = RenderOpts {
                scale,
                edit,
                gif_times: &times,
                threads: threads(),
                offset: region.offset(),
                size: Some(region.size()),
                placeholder_prompt,
                ..Default::default()
            };
            let (image, drawn) = deckcraft_render::render_slide_drawn(&presentation, index, &opts);
            let _ = sender.send(RenderedCanvas { image, drawn, elapsed_ms: crate::now_ms() - t0 });
        });
        self.pending = Some(PendingCanvas { key, receiver });
    }

    #[cfg(target_arch = "wasm32")]
    fn start_async_canvas(
        &mut self,
        _p: &Presentation,
        _index: usize,
        _key: Key,
        _scale: f64,
        _edit: bool,
        _gif_times: &[(ShapeId, f64)],
        _placeholder_prompt: Option<fn(deckcraft_model::PhType) -> &'static str>,
    ) {
        // Web builds have no native worker pool; the synchronous path remains available there.
        self.performance_mode = false;
    }

    /// Force the canvas to re-render next time (fonts loaded, theme changed in place…).
    pub fn invalidate(&mut self) {
        self.canvas = None;
        self.estimate = None;
        self.drawn = None;
        self.thumbs.clear();
        self.pending = None;
        self.drag_snapshot = false;
        self.drag_shape = None;
    }

    /// Render a selected shape once at proxy resolution. Moving it only updates the destination
    /// rectangle, so pointer motion never uploads a new full-size image.
    pub fn drag_shape(
        &mut self,
        ctx: &egui::Context,
        p: &Presentation,
        slide: &Slide,
        id: ShapeId,
        bounds: Rect,
        scale: f64,
    ) -> Option<(TextureHandle, Rect)> {
        let key =
            DragShapeKey { slide: slide.id.0 as usize, id, size: (bounds.width().to_bits(), bounds.height().to_bits()), scale: scale.to_bits() };
        if let Some(entry) = &mut self.drag_shape
            && entry.key == key
        {
            entry.rect = bounds;
            return Some((entry.tex.clone(), entry.rect));
        }
        let image = deckcraft_render::render_shape(p, slide, id, scale)?;
        self.proxy_texture_id = self.proxy_texture_id.wrapping_add(1);
        let tex = ctx.load_texture(format!("drag-shape-{}", self.proxy_texture_id), to_color_image(&image, false), TextureOptions::LINEAR);
        let rect = bounds.inflate(8.0, 8.0);
        self.drag_shape = Some(DragShapeEntry { key, rect, tex: tex.clone() });
        Some((tex, rect))
    }

    /// A thumbnail texture; renders at most `budget` new thumbnails per frame (returns None when
    /// it is over budget, so the caller draws a placeholder and asks for another frame).
    pub fn thumb(&mut self, ctx: &egui::Context, p: &Presentation, index: usize, width_px: u32, budget: &mut u32) -> Option<TextureHandle> {
        let slide = p.slides.get(index)?.clone();
        let master = master_of(p, &slide);
        let width_px = if self.performance_mode { width_px.min(180) } else { width_px };
        let h = (width_px as f64 * p.slide_size.height / p.slide_size.width.max(1.0)).round().max(1.0) as u32;
        let key = Key {
            slide: Arc::as_ptr(&slide) as usize,
            master: master.as_ref().map(|m| Arc::as_ptr(m) as usize).unwrap_or(0),
            size: (width_px, h),
            edit: false,
            language: crate::i18n::Language::English,
            extra: 0,
            proxy: false,
            skip: vec![],
            want: Region::slide(width_px, h),
        };
        self.clock += 1;
        let clock = self.clock;
        if let Some(e) = self.thumbs.get_mut(&key.slide)
            && e.key == key
        {
            e.stamp = clock;
            return Some(e.tex.clone());
        }
        if *budget == 0 {
            // Keep showing the stale thumbnail while the new one waits.
            return self.thumbs.get(&key.slide).map(|e| e.tex.clone());
        }
        *budget -= 1;
        let scale = width_px as f64 / p.slide_size.width.max(1.0);
        let img = deckcraft_render::render_slide(p, index, &RenderOpts { scale, size: Some((width_px, h)), threads: 0, ..Default::default() });
        let tex = ctx.load_texture(format!("thumb-{index}"), to_color_image(&img, false), TextureOptions::LINEAR);
        // Drop the old entry for this slide address; entries for slides that no longer exist age out.
        let region = key.want;
        self.thumbs.insert(key.slide, Entry { key, tex: tex.clone(), region, _slide: slide, _master: master, stamp: clock });
        let cache_limit = if self.performance_mode { 24 } else { 600 };
        if self.thumbs.len() > cache_limit {
            let mut v: Vec<(usize, u64)> = self.thumbs.iter().map(|(k, e)| (*k, e.stamp)).collect();
            v.sort_by_key(|x| x.1);
            let remove = if self.performance_mode { 8 } else { 200 };
            for (k, _) in v.into_iter().take(remove) {
                self.thumbs.remove(&k);
            }
        }
        Some(tex)
    }
}
