//! Prepared slide/animation frames. CPU work runs before playback on a worker.
//! Frames share identical pixel tiles; only the displayed frame occupies GPU memory.
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, atomic::{AtomicBool, AtomicUsize, Ordering}, mpsc};
use deckcraft_anim::ShowState;
use deckcraft_model::Presentation;
use deckcraft_render::{Image, RenderOpts};
use egui::{Color32, ColorImage, TextureHandle};

const FPS: f64 = 30.0;
const TILE: usize = 64;
/// Maximum decoded playback cache size. This is a process-local RAM cache;
/// it does not change the document or any on-disk settings.
pub const MEMORY_LIMIT_MIB: usize = 2048;
const BUDGET: usize = MEMORY_LIMIT_MIB * 1024 * 1024;
const MAX_FRAMES: usize = 18_000;

struct Frame {
    size: [usize; 2],
    tiles: Vec<Arc<Vec<Color32>>>,
}
impl Frame {
    fn image(&self) -> ColorImage {
        let [w, h] = self.size;
        let mut image = ColorImage::filled(self.size, Color32::TRANSPARENT);
        for (n, tile) in self.tiles.iter().enumerate() {
            let x = (n % w.div_ceil(TILE)) * TILE;
            let y = (n / w.div_ceil(TILE)) * TILE;
            let tw = TILE.min(w - x);
            for row in 0..TILE.min(h - y) {
                image.pixels[(y + row) * w + x..(y + row) * w + x + tw]
                    .copy_from_slice(&tile[row * tw..(row + 1) * tw]);
            }
        }
        image
    }
}
#[derive(Default)]
struct Tiles {
    map: HashMap<u64, Vec<Arc<Vec<Color32>>>>,
    bytes: usize,
}
impl Tiles {
    fn frame(&mut self, img: Image, budget: usize) -> Result<Arc<Frame>, String> {
        let image = crate::textures::to_color_image(&img, false);
        let [w, h] = image.size;
        let mut tiles = Vec::new();
        for y in (0..h).step_by(TILE) {
            for x in (0..w).step_by(TILE) {
                let mut pixels = Vec::new();
                for row in y..(y + TILE).min(h) {
                    pixels.extend_from_slice(&image.pixels[row * w + x..row * w + (x + TILE).min(w)]);
                }
                let mut hash = std::collections::hash_map::DefaultHasher::new();
                pixels.hash(&mut hash);
                let bucket = self.map.entry(hash.finish()).or_default();
                let tile = if let Some(tile) = bucket.iter().find(|t| t.as_ref() == &pixels) {
                    tile.clone()
                } else {
                    self.bytes += pixels.len() * 4;
                    if self.bytes > budget { return Err("Playback memory limit reached".into()); }
                    let tile = Arc::new(pixels);
                    bucket.push(tile.clone());
                    tile
                };
                tiles.push(tile);
            }
        }
        self.bytes += tiles.len() * std::mem::size_of::<Arc<Vec<Color32>>>();
        if self.bytes > budget { return Err("Playback memory limit reached".into()); }
        Ok(Arc::new(Frame { size: [w, h], tiles }))
    }
}

struct Page {
    states: Vec<Arc<Frame>>,
    steps: Vec<Vec<Arc<Frame>>>,
    parts: [Arc<Frame>; 2],
    bytes: usize,
    live_media: bool,
}
impl Page {
    fn frame(&self, step: usize, t: f64, playing: bool) -> Option<Arc<Frame>> {
        if playing && let Some(frames) = self.steps.get(step) {
            return frames.get(((t.max(0.0) * FPS).floor() as usize).min(frames.len().saturating_sub(1))).cloned();
        }
        self.states.get(step.min(self.states.len().saturating_sub(1))).cloned()
    }
}
struct Entry {
    key: u64,
    _source: Arc<Presentation>,
    page: Arc<Page>,
}
struct Job {
    index: usize,
    key: u64,
    source: Arc<Presentation>,
    rx: mpsc::Receiver<Result<Page, String>>,
    cancel: Arc<AtomicBool>,
    done: Arc<AtomicUsize>,
    total: Arc<AtomicUsize>,
}
impl Drop for Job {
    fn drop(&mut self) { self.cancel.store(true, Ordering::Relaxed); }
}

#[derive(Default)]
pub struct PlaybackCache {
    source: Option<Arc<Presentation>>,
    revision: u64,
    size: (u32, u32),
    keys: Vec<u64>,
    entries: HashMap<usize, Entry>,
    stale: std::collections::HashSet<usize>,
    errors: HashMap<usize, String>,
    queue: VecDeque<usize>,
    job: Option<Job>,
    auto_slide: Option<usize>,
    auto_last_activity: f64,
    auto_bulk_started: bool,
    /// A small LRU: current frames, transition parts and presenter previews only.
    textures: VecDeque<(Arc<Frame>, TextureHandle)>,
}
impl PlaybackCache {
    pub fn sync(&mut self, doc: Arc<Presentation>, revision: u64, size: (u32, u32)) {
        if self.size == size && self.revision == revision && self.source.as_ref().is_some_and(|p| Arc::ptr_eq(p, &doc)) { return; }
        self.cancel();
        let mut global = std::collections::hash_map::DefaultHasher::new();
        size.hash(&mut global);
        doc.slide_size.width.to_bits().hash(&mut global);
        doc.slide_size.height.to_bits().hash(&mut global);
        serde_json::to_vec(&(&doc.header_footer, &doc.default_text_style, &doc.table_styles, &doc.show, doc.first_slide_number)).unwrap_or_default().hash(&mut global);
        for media in &doc.media { media.id.hash(&mut global); Arc::as_ptr(&media.data).hash(&mut global); }
        self.keys = doc.slides.iter().enumerate().map(|(i, s)| {
            let mut hash = global.clone();
            i.hash(&mut hash);
            Arc::as_ptr(s).hash(&mut hash);
            // Relevant inherited styles/layouts, including the fallback master.
            if let Some((master, _)) = doc.master_for(s) { std::ptr::from_ref(master).hash(&mut hash); }
            hash.finish()
        }).collect();
        self.entries.retain(|i, e| {
            let valid = self.keys.get(*i) == Some(&e.key);
            if !valid { self.stale.insert(*i); }
            valid
        });
        self.errors.clear();
        self.textures.clear();
        self.size = size;
        self.revision = revision;
        self.source = Some(doc);
        self.auto_slide = None;
        self.auto_last_activity = 0.0;
        self.auto_bulk_started = false;
    }
    pub fn start(&mut self, only: Option<usize>) {
        if let Some(i) = only {
            if !self.entries.contains_key(&i) && !self.queue.contains(&i) && self.job.as_ref().is_none_or(|j| j.index != i) { self.queue.push_back(i); }
        } else {
            for i in 0..self.keys.len() { self.start(Some(i)); }
        }
        self.errors.clear();
    }
    /// Put one slide at the front of the worker queue when navigation changes the user's focus.
    pub fn prioritize(&mut self, index: usize) {
        if self.entries.contains_key(&index) || self.job.as_ref().is_some_and(|j| j.index == index) {
            return;
        }
        self.queue.retain(|i| *i != index);
        self.errors.remove(&index);
        self.queue.push_front(index);
    }
    /// Prepare the selected slide immediately, then queue all remaining slides after a short
    /// period without user input. The timer resets on navigation or editing input.
    pub fn auto_compile(&mut self, enabled: bool, slide: usize, now: f64, user_active: bool) {
        if !enabled {
            self.auto_slide = None;
            self.auto_bulk_started = false;
            return;
        }
        if self.auto_slide != Some(slide) {
            let previous = self.auto_slide;
            self.auto_slide = Some(slide);
            self.auto_last_activity = now;
            self.auto_bulk_started = false;
            if let Some(previous) = previous {
                // The page just left is normally the page the user finished editing.
                self.prioritize(previous);
                self.start(Some(slide));
            } else {
                self.prioritize(slide);
            }
        } else if user_active {
            self.auto_last_activity = now;
            self.auto_bulk_started = false;
        }
        if !self.auto_bulk_started && now - self.auto_last_activity >= 1.5 {
            self.start(None);
            self.auto_bulk_started = true;
        }
    }
    pub fn cancel(&mut self) { self.queue.clear(); if let Some(j) = &self.job { j.cancel.store(true, Ordering::Relaxed); } }
    pub fn clear(&mut self) { self.cancel(); self.entries.clear(); self.textures.clear(); self.errors.clear(); }
    pub fn busy(&self) -> bool { self.job.is_some() || !self.queue.is_empty() }
    pub fn ready(&self) -> bool { !self.keys.is_empty() && self.entries.len() == self.keys.len() }
    pub fn bytes(&self) -> usize { self.entries.values().map(|e| e.page.bytes).sum() }
    pub fn count(&self) -> usize { self.entries.len() }
    pub fn status(&self, i: usize) -> &'static str {
        if self.job.as_ref().is_some_and(|j| j.index == i) { "Preparing" }
        else if self.queue.contains(&i) { "Queued" }
        else if self.errors.contains_key(&i) { "Precompile failed" }
        else if let Some(e) = self.entries.get(&i) { if e.page.live_media { "Cached; media plays live" } else { "Cached" } }
        else if self.stale.contains(&i) { "Modified — rebuild required" }
        else { "Not cached" }
    }
    pub fn error(&self, i: usize) -> Option<&str> { self.errors.get(&i).map(String::as_str) }
    pub fn page_info(&self, i: usize) -> Option<(usize, usize)> {
        self.entries.get(&i).map(|e| (e.page.states.len() + e.page.steps.iter().map(Vec::len).sum::<usize>(), e.page.bytes))
    }
    pub fn progress(&self) -> Option<(usize, usize, usize)> {
        self.job.as_ref().map(|j| (j.index, j.done.load(Ordering::Relaxed), j.total.load(Ordering::Relaxed)))
    }
    pub fn tick(&mut self, ctx: &egui::Context) {
        if let Some(job) = self.job.take() {
            match job.rx.try_recv() {
                Ok(result) if !job.cancel.load(Ordering::Relaxed) && self.keys.get(job.index) == Some(&job.key) => match result {
                    Ok(page) => {
                        self.stale.remove(&job.index);
                        self.entries.insert(job.index, Entry { key: job.key, _source: job.source.clone(), page: Arc::new(page) });
                    }
                    Err(e) => { self.errors.insert(job.index, e); }
                },
                Err(mpsc::TryRecvError::Empty) => self.job = Some(job),
                Err(mpsc::TryRecvError::Disconnected) => { self.errors.insert(job.index, "Playback worker stopped".into()); }
                _ => {}
            }
        }
        if self.job.is_none() && let Some(index) = self.queue.pop_front()
            && let Some(key) = self.keys.get(index).copied() && let Some(source) = self.source.clone() {
            let (tx, rx) = mpsc::sync_channel(1);
            let cancel = Arc::new(AtomicBool::new(false));
            let done = Arc::new(AtomicUsize::new(0));
            let total = Arc::new(AtomicUsize::new(1));
            let job = Job { index, key, source: source.clone(), rx, cancel: cancel.clone(), done: done.clone(), total: total.clone() };
            let size = self.size;
            let budget = BUDGET.saturating_sub(self.bytes());
            let wake = ctx.clone();
            #[cfg(not(target_arch = "wasm32"))]
            {
                match std::thread::Builder::new().name("playback-prepare".into()).spawn(move || {
                    let result = prepare(&source, index, size, budget, &cancel, &done, &total);
                    let _ = tx.send(result);
                    wake.request_repaint();
                }) {
                    Ok(_) => self.job = Some(job),
                    Err(e) => { self.errors.insert(index, e.to_string()); }
                }
            }
            #[cfg(target_arch = "wasm32")]
            {
                let _ = (tx, job, size, budget, wake, cancel, done, total);
                self.errors.insert(index, "Background precompile requires the desktop app".into());
            }
        }
        if self.busy() { ctx.request_repaint_after(std::time::Duration::from_millis(50)); }
    }
    fn upload(&mut self, ctx: &egui::Context, frame: Arc<Frame>) -> TextureHandle {
        if let Some(i) = self.textures.iter().position(|(f, _)| Arc::ptr_eq(f, &frame))
            && let Some(pair) = self.textures.remove(i) {
            let tex = pair.1.clone(); self.textures.push_back(pair); return tex;
        }
        let tex = ctx.load_texture("prepared-playback", frame.image(), egui::TextureOptions::LINEAR);
        self.textures.push_back((frame, tex.clone()));
        while self.textures.len() > 8 { self.textures.pop_front(); }
        tex
    }
    pub fn texture(&mut self, ctx: &egui::Context, index: usize, step: usize, t: f64, playing: bool) -> Option<TextureHandle> {
        let frame = self.entries.get(&index)?.page.frame(step, t, playing)?;
        Some(self.upload(ctx, frame))
    }
    pub fn has_live_media(&self, index: usize) -> bool { self.entries.get(&index).is_some_and(|e| e.page.live_media) }
    pub fn parts(&mut self, ctx: &egui::Context, index: usize) -> Option<[TextureHandle; 2]> {
        let frames = self.entries.get(&index)?.page.parts.clone();
        Some([self.upload(ctx, frames[0].clone()), self.upload(ctx, frames[1].clone())])
    }
}

fn prepare(doc: &Presentation, index: usize, size: (u32, u32), budget: usize, cancel: &AtomicBool, done: &AtomicUsize, total: &AtomicUsize) -> Result<Page, String> {
    let tl = ShowState::timeline(doc, index);
    let steps = if doc.show.without_animation { 0 } else { tl.steps() };
    let counts: Vec<usize> = (0..steps).map(|s| (tl.step_duration(s).max(0.0) * FPS).ceil().min(MAX_FRAMES as f64) as usize + 1).collect();
    let count = counts.iter().try_fold(steps + 3, |n, c| n.checked_add(*c)).ok_or("Animation is too long to cache")?;
    if count > MAX_FRAMES { return Err("Animation is too long to cache".into()); }
    total.store(count, Ordering::Relaxed);
    let mut tiles = Tiles::default();
    let mut store = |image: Image| -> Result<Arc<Frame>, String> {
        if cancel.load(Ordering::Relaxed) { return Err("Cancelled".into()); }
        let frame = tiles.frame(image, budget)?;
        done.fetch_add(1, Ordering::Relaxed);
        Ok(frame)
    };
    let opts = RenderOpts { scale: size.0 as f64 / doc.slide_size.width.max(1.0), size: Some(size), threads: 2, ..Default::default() };
    let mut states = Vec::new();
    let mut animations = Vec::new();
    for step in 0..=steps {
        if cancel.load(Ordering::Relaxed) { return Err("Cancelled".into()); }
        let f = |id| crate::show::state_for(&tl, id, if doc.show.without_animation { tl.steps() } else { step }, 0.0);
        states.push(store(deckcraft_render::render_slide(doc, index, &RenderOpts { scale: opts.scale, size: opts.size, state: Some(&f), threads: opts.threads, ..Default::default() }))?);
        if let Some(count) = counts.get(step) {
            let mut frames = Vec::new();
            for frame in 0..*count {
                if cancel.load(Ordering::Relaxed) { return Err("Cancelled".into()); }
                let f = |id| crate::show::state_for(&tl, id, step, frame as f64 / FPS);
                frames.push(store(deckcraft_render::render_slide(doc, index, &RenderOpts { scale: opts.scale, size: opts.size, state: Some(&f), threads: opts.threads, ..Default::default() }))?);
            }
            animations.push(frames);
        }
    }
    let back = store(deckcraft_render::render_slide_part(doc, index, deckcraft_render::SlidePart::Backdrop, &opts))?;
    let content = store(deckcraft_render::render_slide_part(doc, index, deckcraft_render::SlidePart::Content, &opts))?;
    let mut live_media = false;
    if let Some(slide) = doc.slides.get(index) {
        deckcraft_model::walk(&slide.shapes, &mut |s, _| {
            live_media |= matches!(s.kind, deckcraft_model::ShapeKind::Media(_));
            if let deckcraft_model::ShapeKind::Picture { fill } = &s.kind {
                live_media |= doc.media(fill.media).is_some_and(|m| m.data.starts_with(b"GIF8"));
            }
        });
    }
    Ok(Page { states, steps: animations, parts: [back, content], bytes: tiles.bytes, live_media })
}

pub fn menu(app: &mut crate::SlideApp, ui: &mut egui::Ui) -> bool {
    use crate::i18n::tr;
    let Some(doc) = app.session.active().map(|d| d.doc.clone()) else { return true };
    ui.set_min_width(620.0);
    ui.checkbox(&mut app.ui.playback_auto_compile, tr(ui.ctx(), "Automatically precompile slides"));
    ui.label(tr(ui.ctx(), "Precompile the selected slide after navigation, then continue while idle."));
    let size = crate::show::playback_size_for_context(ui.ctx(), &doc);
    ui.label(crate::i18n::format_tr!(ui.ctx(), "Cache size: {} × {} px", "缓存尺寸：{} × {} 像素", size.0, size.1));
    ui.label(crate::i18n::format_tr!(ui.ctx(), "Cached {} of {} slides", "已缓存 {} / {} 张幻灯片", app.playback_cache.count(), doc.slides.len()));
    ui.label(format!(
        "{}: {:.1} / {} MiB",
        tr(ui.ctx(), "Memory"),
        app.playback_cache.bytes() as f64 / 1048576.0,
        MEMORY_LIMIT_MIB,
    ));
    ui.label(tr(ui.ctx(), "Animation steps are prepared at 30 fps. Video and GIF playback remains live."));
    if let Some((i, done, total)) = app.playback_cache.progress() {
        ui.label(format!("{} {}: {} / {}", tr(ui.ctx(), "Preparing slide"), i + 1, done, total));
        ui.add(egui::ProgressBar::new(done as f32 / total.max(1) as f32).show_percentage());
    }
    let mut rebuild = None;
    egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
        for (i, slide) in doc.slides.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(format!("{}. {}", i + 1, slide.title()));
                ui.label(tr(ui.ctx(), app.playback_cache.status(i)));
                if let Some((frames, bytes)) = app.playback_cache.page_info(i) {
                    ui.label(format!("{} {} · {:.1} MiB", frames, tr(ui.ctx(), "frames"), bytes as f64 / 1048576.0));
                } else if ui.small_button(tr(ui.ctx(), "Cache this slide")).clicked() { rebuild = Some(i); }
            });
            if let Some(error) = app.playback_cache.error(i) { ui.label(tr(ui.ctx(), error)); }
        }
    });
    if let Some(i) = rebuild { app.playback_cache.start(Some(i)); }
    let mut close = false;
    ui.horizontal(|ui| {
        if ui.add_enabled(!app.playback_cache.busy(), egui::Button::new(tr(ui.ctx(), "Cache pending slides"))).clicked() { app.playback_cache.start(None); }
        if ui.add_enabled(app.playback_cache.busy(), egui::Button::new(tr(ui.ctx(), "Stop preparation"))).clicked() { app.playback_cache.cancel(); }
        if ui.button(tr(ui.ctx(), "Clear Cache")).clicked() { app.playback_cache.clear(); }
        if ui.button(tr(ui.ctx(), "Close")).clicked() { close = true; }
    });
    close
}
