//! On-demand link checks. At most one worker reads/hashes/validates a picture at a time.

use std::sync::mpsc;

use deckcraft_engine::cmd::media::{PreparedPicture, prepare_picture_refresh};
use deckcraft_model::{MediaId, ShapeId, ShapeKind};

use crate::SlideApp;

type SelectionKey = (u64, ShapeId, MediaId, String);

struct Job {
    uid: u64,
    rx: mpsc::Receiver<Result<Option<PreparedPicture>, String>>,
}

#[derive(Default)]
pub(crate) struct LinkChecks {
    selected: Option<SelectionKey>,
    pending: bool,
    job: Option<Job>,
}

impl SlideApp {
    /// Re-clicking an already selected image must check again as well.
    pub(crate) fn request_picture_check(&mut self) {
        self.link_checks.pending = true;
    }

    pub(crate) fn poll_picture_checks(&mut self, ctx: &egui::Context) {
        let selected = self.session.active().and_then(|st| {
            let [shape] = st.selection.shapes.as_slice() else { return None };
            let ShapeKind::Picture { fill } = &st.shape(*shape)?.kind else { return None };
            let item = st.doc.media(fill.media)?;
            Some((st.uid, *shape, fill.media, item.link.clone()?))
        });
        if self.link_checks.selected != selected {
            self.link_checks.selected = selected;
            self.link_checks.pending = self.link_checks.selected.is_some();
        }
        // Do not edit documents while dragging, typing, showing slides, or handling a modal.
        if self.show.is_some()
            || self.dialog.is_some()
            || self.backstage.is_some()
            || self.session.tool.dragging()
            || ctx.input(|i| i.pointer.any_down())
            || self.session.active().is_some_and(|d| d.selection.text.is_some())
        {
            return;
        }
        if let Some(job) = &self.link_checks.job {
            let uid = job.uid;
            match job.rx.try_recv() {
                Ok(result) => {
                    self.link_checks.job = None;
                    if self.session.active().is_some_and(|d| d.uid == uid) {
                        match result {
                            Ok(Some(update)) => match self.session.apply_picture_refresh(update) {
                                Ok(v) if v.get("refreshed").and_then(serde_json::Value::as_bool) == Some(true) => self.set_status("外部图片已更新"),
                                Ok(_) => {}
                                Err(e) => self.set_status(e.to_string()),
                            },
                            Ok(None) => {}
                            Err(e) => self.set_status(format!("未更新外部图片，保留当前预览：{e}")),
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(100));
                    return;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.link_checks.job = None;
                    self.set_status("外部图片检查中断，保留当前预览");
                }
            }
        }
        if !std::mem::take(&mut self.link_checks.pending) {
            return;
        }
        let Some((uid, _, media, _)) = self.link_checks.selected.clone() else { return };
        let Some(item) = self.session.active().and_then(|d| d.doc.media(media)).cloned() else { return };
        let (tx, rx) = mpsc::channel();
        let repaint = ctx.clone();
        match std::thread::Builder::new().name("deckcraft-picture-check".into()).spawn(move || {
            let result = prepare_picture_refresh(uid, &item).map_err(|e| e.to_string());
            let _ = tx.send(result);
            repaint.request_repaint();
        }) {
            Ok(_) => {
                self.link_checks.job = Some(Job { uid, rx });
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
            Err(e) => self.set_status(format!("无法检查外部图片：{e}")),
        }
    }
}
