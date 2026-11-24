//! The reader's view of an HTML message: its sanitised segments as styled text and images,
//! and the privacy controls around the images.
//!
//! Remote images are off by default. A message's images load only when the user presses
//! "Load images" for that message, or when its sender is on the allow list *and* the message
//! passed DMARC (a forged `From` cannot ride on someone's allow-list entry). Until then every
//! image is a placeholder and nothing is requested. `http` images never load.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    AnyElement, ClickEvent, Context, ElementId, ImageSource, ObjectFit, SharedString, Window, div,
    img, px, relative, rgb,
};

use super::reader::ReaderView;
use super::theme::Palette;
use super::widgets::{caption, ghost_button};
use crate::images;
use crate::mail::rich::{ImageKind, RichDoc, RichImage, Segment};
use crate::model::{AuthResult, Message};

/// An image's load state.
pub enum ImageState {
    Loading,
    Ready(Arc<gpui::Image>),
    Failed(String),
}

/// What the reader remembers about remote images; dropped when the open thread changes.
#[derive(Default)]
pub struct RichState {
    pub images: HashMap<String, ImageState>,
    /// Message ids whose images the user chose to load.
    pub loaded: HashSet<String>,
}

/// Images shown wider than this are scaled down to it.
const MAX_IMAGE_W: f32 = 640.0;
/// Width of an image with no declared size.
const DEFAULT_IMAGE_W: f32 = 480.0;

/// Whether an allow-list entry applies to `m`: the sender is listed and DMARC passed.
pub fn sender_trusted(allowed: &BTreeSet<String>, m: &Message) -> bool {
    trusted(allowed, &m.from.email, m.signals.auth.dmarc)
}

fn trusted(allowed: &BTreeSet<String>, email: &str, dmarc: AuthResult) -> bool {
    dmarc == AuthResult::Pass && allowed.contains(&email.to_ascii_lowercase())
}

/// The lowercase address the allow list keys on.
fn sender_key(m: &Message) -> String {
    m.from.email.to_ascii_lowercase()
}

/// How many images in `doc` could load if allowed.
fn loadable(doc: &RichDoc) -> usize {
    doc.images().filter(|i| i.kind == ImageKind::Remote).count()
}

impl ReaderView {
    /// Whether `m`'s remote images may load now.
    pub(super) fn images_allowed(&self, m: &Message, cx: &gpui::App) -> bool {
        self.rich.loaded.contains(&m.id)
            || sender_trusted(&self.store.read(cx).prefs.image_senders, m)
    }

    /// Drops the text bodies of `id`'s segments (and its quote's).
    pub(super) fn drop_rich_bodies(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let main = format!("{id}~");
        let quote = format!("{id}#quote~");
        let keys: Vec<String> = self
            .bodies
            .keys()
            .filter(|k| k.starts_with(&main) || k.starts_with(&quote))
            .cloned()
            .collect();
        for key in keys {
            self.drop_body(&key, window, cx);
        }
    }

    /// Drops only the quote's segment bodies (the quote was folded again).
    pub(super) fn drop_rich_quote(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let quote = format!("{id}#quote~");
        let keys: Vec<String> = self
            .bodies
            .keys()
            .filter(|k| k.starts_with(&quote))
            .cloned()
            .collect();
        for key in keys {
            self.drop_body(&key, window, cx);
        }
    }

    /// Lays out `segments` as a column of selectable text bodies and images. `prefix` keys
    /// the text bodies (the message id, or its quote's key).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn rich_column(
        &mut self,
        m: &Message,
        segments: &[Segment],
        prefix: &str,
        secondary: bool,
        allowed: bool,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut column = div().w_full().flex().flex_col().gap(px(8.0));
        for (i, segment) in segments.iter().enumerate() {
            match segment {
                Segment::Text(t) => {
                    let body = self.rich_body(format!("{prefix}~{i}"), t, secondary, cx);
                    column = column.child(body);
                }
                Segment::Image(image) => {
                    column =
                        column.child(self.image_element(m, image, (prefix, i), allowed, p, cx));
                }
            }
        }
        column.into_any_element()
    }

    fn image_element(
        &mut self,
        _m: &Message,
        image: &RichImage,
        (prefix, i): (&str, usize),
        allowed: bool,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = if image.alt.is_empty() {
            "Image".to_string()
        } else {
            image.alt.clone()
        };
        let placeholder = |text: String| {
            div()
                .px(px(10.0))
                .py(px(6.0))
                .rounded(px(6.0))
                .border_1()
                .border_color(rgb(p.border))
                .bg(rgb(p.panel_raised))
                .text_size(px(12.0))
                .text_color(rgb(p.muted))
                .child(text)
                .into_any_element()
        };
        let shown = match (image.kind, allowed) {
            (ImageKind::Insecure, _) => placeholder(format!(
                "{label} · not loaded: the image link is not secure"
            )),
            (ImageKind::Remote, false) => placeholder(format!("{label} · remote image blocked")),
            (ImageKind::Remote, true) => match self.rich.images.get(&image.url) {
                Some(ImageState::Ready(data)) => {
                    let width = image
                        .width
                        .map_or(DEFAULT_IMAGE_W, |w| (w as f32).clamp(1.0, MAX_IMAGE_W));
                    img(ImageSource::Image(data.clone()))
                        .w(px(width))
                        .max_w(relative(1.0))
                        .object_fit(ObjectFit::Contain)
                        .into_any_element()
                }
                Some(ImageState::Failed(reason)) => {
                    placeholder(format!("{label} · not available ({reason})"))
                }
                Some(ImageState::Loading) | None => placeholder(format!("{label} · loading…")),
            },
        };
        let Some(link) = image.link.clone() else {
            return shown;
        };
        let message_id = prefix
            .split(['~', '#'])
            .next()
            .unwrap_or(prefix)
            .to_string();
        div()
            .id(ElementId::from((
                SharedString::from(format!("{prefix}-img")),
                i,
            )))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.open_link(&link, &message_id, cx);
            }))
            .child(shown)
            .into_any_element()
    }

    /// Starts loading every remote image of `doc` that is not loading or loaded yet.
    pub(super) fn load_images(&mut self, doc: &RichDoc, cx: &mut Context<Self>) {
        let urls: Vec<String> = doc
            .images()
            .filter(|i| i.kind == ImageKind::Remote)
            .map(|i| i.url.clone())
            .filter(|u| !self.rich.images.contains_key(u))
            .collect();
        for url in urls {
            self.rich.images.insert(url.clone(), ImageState::Loading);
            cx.spawn(async move |this, cx| {
                let fetch_url = url.clone();
                let result = cx
                    .background_spawn(async move { images::fetch(&fetch_url) })
                    .await;
                let state = match result {
                    Ok(f) => {
                        ImageState::Ready(Arc::new(gpui::Image::from_bytes(f.format, f.bytes)))
                    }
                    Err(reason) => ImageState::Failed(reason),
                };
                this.update(cx, |this, cx| {
                    this.rich.images.insert(url, state);
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
    }

    /// The strip above an HTML body that says whether remote images are blocked, with the
    /// actions to load them. `None` when the message has no remote images.
    pub(super) fn images_bar(
        &mut self,
        m: &Message,
        doc: &RichDoc,
        ix: usize,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let count = loadable(doc);
        if count == 0 {
            return None;
        }
        let id = m.id.clone();
        let sender = sender_key(m);
        let listed = self.store.read(cx).prefs.image_senders.contains(&sender);
        let trusted = sender_trusted(&self.store.read(cx).prefs.image_senders, m);
        let loaded = self.rich.loaded.contains(&id);
        let bar = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(4.0))
            .px(px(14.0))
            .pb(px(8.0));
        let plural = if count == 1 { "image" } else { "images" };
        if trusted {
            let stop_sender = sender.clone();
            return Some(
                bar.child(caption(
                    p,
                    format!("Showing remote images from {} ·", m.from.email),
                ))
                .child(
                    ghost_button(
                        p,
                        ("images-stop", ix),
                        "Stop",
                        Some(Box::new(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.set_image_sender(&stop_sender, false, cx);
                            this.rich.loaded.clear();
                            cx.notify();
                        }))),
                    )
                    .py(px(0.0))
                    .text_size(px(11.0)),
                )
                .into_any_element(),
            );
        }
        if loaded {
            return Some(
                bar.child(caption(p, format!("Showing {count} remote {plural}")))
                    .into_any_element(),
            );
        }
        let note = if listed {
            format!(
                "{count} remote {plural} blocked: {} is on your list, but this message's sender could not be verified.",
                m.from.email
            )
        } else {
            format!("{count} remote {plural} blocked. Loading them tells the sender you read this.")
        };
        let load_id = id.clone();
        let doc = Arc::new(doc.clone());
        let load_doc = doc.clone();
        let always_doc = doc;
        let always_sender = sender.clone();
        let always_id = id;
        Some(
            bar.child(caption(p, note))
                .child(
                    ghost_button(
                        p,
                        ("images-load", ix),
                        "Load images",
                        Some(Box::new(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.rich.loaded.insert(load_id.clone());
                            this.load_images(&load_doc, cx);
                            cx.notify();
                        }))),
                    )
                    .py(px(0.0))
                    .text_size(px(11.0)),
                )
                .child(
                    ghost_button(
                        p,
                        ("images-always", ix),
                        format!("Always load from {}", m.from.email),
                        Some(Box::new(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.set_image_sender(&always_sender, true, cx);
                            this.rich.loaded.insert(always_id.clone());
                            this.load_images(&always_doc, cx);
                            cx.notify();
                        }))),
                    )
                    .py(px(0.0))
                    .text_size(px(11.0)),
                )
                .into_any_element(),
        )
    }

    /// Adds or removes a sender on the allow list.
    fn set_image_sender(&mut self, sender: &str, allow: bool, cx: &mut Context<Self>) {
        let sender = sender.to_string();
        self.store.update(cx, |s, cx| {
            s.change_prefs(|p| {
                if allow {
                    p.image_senders.insert(sender);
                } else {
                    p.image_senders.remove(&sender);
                }
            });
            cx.notify();
        });
    }

    /// Whether to fetch now: the user (or the allow list) allowed it and something is missing.
    pub(super) fn fetch_if_allowed(&mut self, m: &Message, allowed: bool, cx: &mut Context<Self>) {
        if allowed && let Some(doc) = m.rich.clone() {
            self.load_images(&doc, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_allow_list_needs_a_passing_dmarc() {
        let allowed: BTreeSet<String> = ["news@example.com".to_string()].into_iter().collect();
        assert!(trusted(&allowed, "News@Example.com", AuthResult::Pass));
        // Listed, but forged or unchecked: not trusted.
        assert!(!trusted(&allowed, "news@example.com", AuthResult::Fail));
        assert!(!trusted(&allowed, "news@example.com", AuthResult::Unknown));
        assert!(!trusted(&allowed, "news@example.com", AuthResult::None));
        // Verified, but not listed.
        assert!(!trusted(&allowed, "other@example.com", AuthResult::Pass));
    }
}
