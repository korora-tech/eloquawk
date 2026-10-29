//! Wrapped, selectable text: the shared core under `BodyText` (and the compose `TextArea`
//! from M8). gpui at the pinned rev has no selectable static text, so this lays the text out
//! itself.
//!
//! [`SelectableText`] is an element that measures with `window.request_measured_layout`, whose
//! closure calls `shape_text(text, font_size, runs, Some(width), None)`, so the height follows
//! the width. The shaped lines live in a [`TextLayout`] the owning entity keeps, so its mouse
//! handlers can hit-test (position to byte offset) against the last paint. Painting draws the
//! selection rectangles under the glyphs and underlines link ranges in the link colour, with a
//! pointer cursor over them.
//!
//! The pure parts (wrap boundaries as byte offsets, selection normalisation and word
//! boundaries for double-click) are separate functions with tests.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    App, AvailableSpace, Bounds, CursorStyle, DispatchPhase, ElementId, GlobalElementId, Hitbox,
    HitboxBehavior, Hsla, IntoElement, LayoutId, MouseMoveEvent, Pixels, Point, SharedString, Size,
    Style, TextRun, UnderlineStyle, Window, WrapBoundary, WrappedLine, fill, point, px, size,
};
use unicode_segmentation::UnicodeSegmentation;

// ----- pure helpers -------------------------------------------------------------------------

/// Converts a shaped line's wrap boundaries to byte offsets within that line. `index_of(run,
/// glyph)` returns the byte index of a glyph (`unwrapped_layout.runs[r].glyphs[g].index`); a
/// boundary that points nowhere is skipped.
pub fn boundary_offsets(
    boundaries: &[WrapBoundary],
    index_of: impl Fn(usize, usize) -> Option<usize>,
) -> Vec<usize> {
    boundaries
        .iter()
        .filter_map(|b| index_of(b.run_ix, b.glyph_ix))
        .collect()
}

/// The visual lines of a logical line of `len` bytes wrapped at `offsets` (ascending), as
/// byte ranges within the line. Always at least one, possibly empty, range.
pub fn visual_lines(len: usize, offsets: &[usize]) -> Vec<Range<usize>> {
    let mut out = Vec::with_capacity(offsets.len() + 1);
    let mut start = 0;
    for &o in offsets {
        let o = o.clamp(start, len);
        out.push(start..o);
        start = o;
    }
    out.push(start..len);
    out
}

/// The selection between an anchor and the moving end, in ascending order.
pub fn normalise(anchor: usize, head: usize) -> Range<usize> {
    anchor.min(head)..anchor.max(head)
}

/// The word (or run of whitespace or punctuation) around byte `ix`, for double-click. At the
/// end of the text it is the last segment.
pub fn word_at(text: &str, ix: usize) -> Range<usize> {
    let ix = ix.min(text.len());
    let mut last = 0..0;
    for (start, seg) in text.split_word_bound_indices() {
        let range = start..start + seg.len();
        if range.contains(&ix) {
            return range;
        }
        last = range;
    }
    last
}

/// Splits `0..len` into plain and link segments (`true` for a link). `links` are byte ranges in
/// ascending order; overlapping or out-of-range ones are clipped.
pub fn segments(len: usize, links: &[Range<usize>]) -> Vec<(Range<usize>, bool)> {
    let mut out = Vec::new();
    let mut at = 0;
    for link in links {
        let start = link.start.clamp(at, len);
        let end = link.end.clamp(start, len);
        if start > at {
            out.push((at..start, false));
        }
        if end > start {
            out.push((start..end, true));
        }
        at = end;
    }
    if at < len || out.is_empty() {
        out.push((at..len, false));
    }
    out
}

// ----- layout state -------------------------------------------------------------------------

/// What a layout was shaped from; a change throws the cached shaping away.
#[derive(Clone, PartialEq)]
struct Signature {
    text: SharedString,
    links: Vec<Range<usize>>,
    color: Hsla,
    link: Hsla,
    font_size: Pixels,
    line_height: Pixels,
}

struct Shaped {
    lines: Vec<WrappedLine>,
    wrap_width: Option<Pixels>,
    size: Size<Pixels>,
}

#[derive(Default)]
struct State {
    signature: Option<Signature>,
    shaped: Option<Shaped>,
    line_height: Pixels,
    /// Where the text was last painted, in window coordinates.
    bounds: Option<Bounds<Pixels>>,
}

/// The shaped lines of one piece of text and where they were painted. Cheap to clone: the
/// owning entity keeps one and hands a clone to each [`SelectableText`] it renders.
#[derive(Clone, Default)]
pub struct TextLayout(Rc<RefCell<State>>);

/// A logical line (text between newlines) with its byte offset in the whole text and its top.
struct LineAt<'a> {
    line: &'a WrappedLine,
    start: usize,
    top: Pixels,
}

impl TextLayout {
    fn for_each_line<R>(&self, mut f: impl FnMut(LineAt<'_>, Pixels) -> Option<R>) -> Option<R> {
        let state = self.0.borrow();
        let shaped = state.shaped.as_ref()?;
        let bounds = state.bounds?;
        let lh = state.line_height;
        let (mut start, mut top) = (0, bounds.top());
        for line in &shaped.lines {
            if let Some(r) = f(LineAt { line, start, top }, lh) {
                return Some(r);
            }
            start += line.len() + 1;
            top += line.size(lh).height;
        }
        None
    }

    fn bounds(&self) -> Option<Bounds<Pixels>> {
        self.0.borrow().bounds
    }

    fn len(&self) -> usize {
        self.0
            .borrow()
            .signature
            .as_ref()
            .map_or(0, |s| s.text.len())
    }

    /// The byte offset nearest to a window position, clamped into the text: above it is 0 and
    /// below it is the end. `None` before the first paint.
    pub fn closest_index(&self, position: Point<Pixels>) -> Option<usize> {
        let bounds = self.bounds()?;
        if position.y < bounds.top() {
            return Some(0);
        }
        let hit = self.for_each_line(|at, lh| {
            let bottom = at.top + at.line.size(lh).height;
            if position.y >= bottom {
                return None;
            }
            let local = point(position.x - bounds.left(), position.y - at.top);
            let ix = match at.line.closest_index_for_position(local, lh) {
                Ok(i) | Err(i) => i,
            };
            Some(at.start + ix.min(at.line.len()))
        });
        Some(hit.unwrap_or_else(|| self.len()))
    }

    /// The byte offset of the glyph exactly under a window position, if any (for links).
    pub fn exact_index(&self, position: Point<Pixels>) -> Option<usize> {
        let bounds = self.bounds()?;
        if !bounds.contains(&position) {
            return None;
        }
        self.for_each_line(|at, lh| {
            let bottom = at.top + at.line.size(lh).height;
            if position.y >= bottom {
                return None;
            }
            let local = point(position.x - bounds.left(), position.y - at.top);
            // Inside this line's band: a miss here is a miss overall.
            Some(
                at.line
                    .index_for_position(local, lh)
                    .ok()
                    .map(|i| at.start + i),
            )
        })
        .flatten()
    }

    /// Window rectangles covering `range`, one per visual line it touches. With `newlines`, a
    /// range running past the end of a line also covers a sliver for the line break, so
    /// selected blank lines show.
    pub fn rects(&self, range: Range<usize>, newlines: bool) -> Vec<Bounds<Pixels>> {
        let mut out = Vec::new();
        if range.is_empty() {
            return out;
        }
        let Some(bounds) = self.bounds() else {
            return out;
        };
        self.for_each_line::<()>(|at, lh| {
            let len = at.line.len();
            let line_range = at.start..at.start + len;
            if range.start > line_range.end || range.end < line_range.start {
                return None;
            }
            let layout = &at.line.unwrapped_layout;
            let offsets = boundary_offsets(at.line.wrap_boundaries(), |r, g| {
                layout.runs.get(r)?.glyphs.get(g).map(|g| g.index)
            });
            let visual = visual_lines(len, &offsets);
            let last = visual.len() - 1;
            for (vi, v) in visual.iter().enumerate() {
                let (vs, ve) = (at.start + v.start, at.start + v.end);
                let s = range.start.max(vs);
                let e = range.end.min(ve);
                let past_end = vi == last && newlines && range.end > ve;
                if s > e || (s == e && !past_end) {
                    continue;
                }
                let x0 = layout.x_for_index(s - at.start) - layout.x_for_index(v.start);
                let mut x1 = layout.x_for_index(e - at.start) - layout.x_for_index(v.start);
                if past_end {
                    x1 += px(6.0);
                }
                let top = at.top + lh * vi as f32;
                out.push(Bounds::new(
                    point(bounds.left() + x0, top),
                    size((x1 - x0).max(px(0.0)), lh),
                ));
            }
            None
        });
        out
    }
}

// ----- element ------------------------------------------------------------------------------

/// A window-wide mouse move listener.
pub type MouseMoveListener = Box<dyn Fn(&MouseMoveEvent, &mut Window, &mut App)>;

/// Paints text from a [`TextLayout`] with a selection and underlined links. Font, size, line
/// height and colour come from the surrounding text style.
pub struct SelectableText {
    pub layout: TextLayout,
    pub text: SharedString,
    /// Link byte ranges, ascending and non-overlapping.
    pub links: Vec<Range<usize>>,
    pub selection: Range<usize>,
    pub selection_color: Hsla,
    pub link_color: Hsla,
    /// Called for every mouse move in the window, hovered or not, so a drag that leaves the
    /// text keeps extending the selection.
    pub on_mouse_move: Option<MouseMoveListener>,
}

pub struct Prepainted {
    text_hitbox: Hitbox,
    link_hitboxes: Vec<Hitbox>,
    selection: Vec<Bounds<Pixels>>,
}

impl IntoElement for SelectableText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl gpui::Element for SelectableText {
    type RequestLayoutState = ();
    type PrepaintState = Prepainted;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();
        let signature = Signature {
            text: self.text.clone(),
            links: self.links.clone(),
            color: style.color,
            link: self.link_color,
            font_size,
            line_height,
        };
        {
            let mut state = self.layout.0.borrow_mut();
            if state.signature.as_ref() != Some(&signature) {
                state.signature = Some(signature);
                state.shaped = None;
            }
            state.line_height = line_height;
        }
        let base = style.to_run(0);
        let link_color = self.link_color;
        let runs: Vec<TextRun> = segments(self.text.len(), &self.links)
            .into_iter()
            .map(|(r, link)| TextRun {
                len: r.len(),
                color: if link { link_color } else { base.color },
                underline: link.then_some(UnderlineStyle {
                    color: Some(link_color),
                    thickness: px(1.0),
                    wavy: false,
                }),
                ..base.clone()
            })
            .collect();
        let text = self.text.clone();
        let state = self.layout.0.clone();
        let layout_id =
            window.request_measured_layout(Style::default(), move |known, available, window, _| {
                let wrap_width = known.width.or(match available.width {
                    AvailableSpace::Definite(w) => Some(w),
                    _ => None,
                });
                if let Some(shaped) = state.borrow().shaped.as_ref()
                    && shaped.wrap_width == wrap_width
                {
                    return shaped.size;
                }
                let lines: Vec<WrappedLine> = match window.text_system().shape_text(
                    text.clone(),
                    font_size,
                    &runs,
                    wrap_width,
                    None,
                ) {
                    Ok(lines) => lines.into_iter().collect(),
                    Err(e) => {
                        eprintln!("eloquawk: could not shape message text: {e}");
                        Vec::new()
                    }
                };
                let mut measured: Size<Pixels> = Size::default();
                for line in &lines {
                    let s = line.size(line_height);
                    measured.height += s.height;
                    measured.width = measured.width.max(s.width).ceil();
                }
                state.borrow_mut().shaped = Some(Shaped {
                    lines,
                    wrap_width,
                    size: measured,
                });
                measured
            });
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        self.layout.0.borrow_mut().bounds = Some(bounds);
        let text_hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let mask = window.content_mask().bounds;
        let link_hitboxes = self
            .links
            .iter()
            .flat_map(|r| self.layout.rects(r.clone(), false))
            .filter(|r| mask.intersects(r))
            .map(|r| window.insert_hitbox(r, HitboxBehavior::Normal))
            .collect();
        Prepainted {
            text_hitbox,
            link_hitboxes,
            selection: self.layout.rects(self.selection.clone(), true),
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let mask = window.content_mask().bounds;
        for rect in &prepaint.selection {
            if mask.intersects(rect) {
                window.paint_quad(fill(*rect, self.selection_color));
            }
        }
        {
            let state = self.layout.0.borrow();
            if let Some(shaped) = state.shaped.as_ref() {
                let lh = state.line_height;
                let mut origin = bounds.origin;
                for line in &shaped.lines {
                    let height = line.size(lh).height;
                    // Only the lines inside the scroll viewport are painted; long bodies have
                    // thousands.
                    let band = Bounds::new(origin, size(bounds.size.width, height));
                    if mask.intersects(&band)
                        && let Err(e) =
                            line.paint(origin, lh, gpui::TextAlign::Left, Some(bounds), window, cx)
                    {
                        eprintln!("eloquawk: could not paint message text: {e}");
                    }
                    origin.y += height;
                }
            }
        }
        if let Some(listener) = self.on_mouse_move.take() {
            window.on_mouse_event(move |ev: &MouseMoveEvent, phase, window, cx| {
                if phase == DispatchPhase::Bubble {
                    listener(ev, window, cx);
                }
            });
        }
        window.set_cursor_style(CursorStyle::IBeam, &prepaint.text_hitbox);
        for hitbox in &prepaint.link_hitboxes {
            window.set_cursor_style(CursorStyle::PointingHand, hitbox);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_boundaries_become_byte_offsets() {
        // Two runs; glyph byte indices as a shaper would report them.
        let runs: Vec<Vec<usize>> = vec![vec![0, 1, 2, 3, 5], vec![7, 8, 11]];
        let index_of = |r: usize, g: usize| runs.get(r)?.get(g).copied();
        let boundaries = [
            WrapBoundary {
                run_ix: 0,
                glyph_ix: 3,
            },
            WrapBoundary {
                run_ix: 1,
                glyph_ix: 1,
            },
            // Points past the glyphs: skipped.
            WrapBoundary {
                run_ix: 4,
                glyph_ix: 0,
            },
        ];
        let offsets = boundary_offsets(&boundaries, index_of);
        assert_eq!(offsets, vec![3, 8]);
        assert_eq!(visual_lines(14, &offsets), vec![0..3, 3..8, 8..14]);
        assert_eq!(visual_lines(0, &[]), vec![0..0]);
        // Out-of-order or out-of-range offsets are clamped, never inverted.
        assert_eq!(visual_lines(5, &[4, 2, 9]), vec![0..4, 4..4, 4..5, 5..5]);
    }

    #[test]
    fn selection_is_normalised() {
        assert_eq!(normalise(3, 9), 3..9);
        assert_eq!(normalise(9, 3), 3..9);
        assert_eq!(normalise(4, 4), 4..4);
    }

    #[test]
    fn double_click_selects_a_word() {
        let text = "Your code is 482913, Zoë.";
        let at = |s: &str| text.find(s).unwrap_or(0);
        assert_eq!(&text[word_at(text, at("code") + 1)], "code");
        assert_eq!(&text[word_at(text, at("482913") + 3)], "482913");
        assert_eq!(&text[word_at(text, at("Zoë"))], "Zoë");
        // On the space between words: the space run.
        assert_eq!(&text[word_at(text, at(" is"))], " ");
        // At the very end: the last segment.
        assert_eq!(&text[word_at(text, text.len())], ".");
        assert_eq!(word_at("", 0), 0..0);
    }

    #[test]
    fn link_segments_cover_the_text() {
        assert_eq!(segments(0, &[]), vec![(0..0, false)]);
        assert_eq!(segments(10, &[]), vec![(0..10, false)]);
        assert_eq!(
            segments(20, &[2..5, 9..20]),
            vec![(0..2, false), (2..5, true), (5..9, false), (9..20, true)]
        );
        assert_eq!(segments(6, &[0..6, 6..6]), vec![(0..6, true)]);
        // Overlaps and overruns are clipped.
        assert_eq!(
            segments(8, &[1..4, 3..6, 7..12]),
            vec![
                (0..1, false),
                (1..4, true),
                (4..6, true),
                (6..7, false),
                (7..8, true)
            ]
        );
    }
}
