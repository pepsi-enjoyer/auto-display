use eframe::egui::{Pos2, Rect, Vec2, pos2, vec2};

use crate::display::{Area, Screen};

pub(super) fn offset_in_mm(offset: i32, moving_px: i32, neighbour_px: i32, moving_mm: f32, neighbour_mm: f32) -> f32 {
    let (pixels, mm) = if offset < 0 { (moving_px, moving_mm) } else { (neighbour_px, neighbour_mm) };
    offset as f32 * mm / pixels.max(1) as f32
}

pub(super) fn offset_in_pixels(offset: f32, moving_px: i32, neighbour_px: i32, moving_mm: f32, neighbour_mm: f32) -> i32 {
    let (pixels, mm) = if offset < 0.0 { (moving_px, moving_mm) } else { (neighbour_px, neighbour_mm) };
    (offset * pixels.max(1) as f32 / mm).round() as i32
}

#[derive(Clone, Copy, PartialEq)]
pub(super) struct SeamSpan {
    pixels: (i32, i32),
    mm: (f32, f32),
}

impl SeamSpan {
    pub(super) fn pixel_at(self, mm: f32) -> Option<i32> {
        if !(self.mm.0..=self.mm.1).contains(&mm) {
            return None;
        }
        let fraction = (mm - self.mm.0) / (self.mm.1 - self.mm.0);
        Some(self.pixels.0 + (fraction * (self.pixels.1 - self.pixels.0) as f32).round() as i32)
    }

    fn mm_at(self, pixel: i32) -> Option<f32> {
        if !(self.pixels.0..=self.pixels.1).contains(&pixel) {
            return None;
        }
        let fraction = (pixel - self.pixels.0) as f32 / (self.pixels.1 - self.pixels.0) as f32;
        Some(self.mm.0 + fraction * (self.mm.1 - self.mm.0))
    }
}

/// Map the open part of an edge between the desktop and the diagram. A third panel can cap
/// the slide at very different offsets because a high-DPI screen takes more room in pixels.
pub(super) fn constrained_seam(
    moving: (Area, Rect), anchor: (Area, Rect), panels: &[(Area, Rect)], along_x: bool, from_pixels: bool,
) -> Option<SeamSpan> {
    let pixel_axis = |a: Area| if along_x { (a.x, a.w, a.y, a.h) } else { (a.y, a.h, a.x, a.w) };
    let physical_axis = |r: Rect| {
        if along_x { (r.left(), r.width(), r.top(), r.height()) } else { (r.top(), r.height(), r.left(), r.width()) }
    };
    let (start_px, length_px, cross_px, width_px) = pixel_axis(moving.0);
    let (start_mm, length_mm, cross_mm, width_mm) = physical_axis(moving.1);
    let (anchor_px, anchor_length_px, _, _) = pixel_axis(anchor.0);
    let (anchor_mm, anchor_length_mm, _, _) = physical_axis(anchor.1);
    let full = SeamSpan {
        pixels: (anchor_px - length_px + 1, anchor_px + anchor_length_px - 1),
        mm: (
            anchor_mm - length_mm + length_mm / length_px as f32,
            anchor_mm + anchor_length_mm - anchor_length_mm / anchor_length_px as f32,
        ),
    };
    let mut span = full;
    for &(area, rect) in panels {
        let (px, len_px, other_px, other_width_px) = pixel_axis(area);
        let (mm, len_mm, other_mm, other_width_mm) = physical_axis(rect);
        let crosses_pixels = cross_px + width_px > other_px && other_px + other_width_px > cross_px;
        let crosses_mm = (cross_mm + width_mm).min(other_mm + other_width_mm) - cross_mm.max(other_mm) > 0.001;
        if !crosses_pixels || !crosses_mm {
            continue;
        }
        let (before, after) = if from_pixels {
            (start_px + length_px <= px, start_px >= px + len_px)
        } else {
            (start_mm + length_mm <= mm + 0.001, start_mm >= mm + len_mm - 0.001)
        };
        if before {
            span.pixels.1 = span.pixels.1.min(px - length_px);
            span.mm.1 = span.mm.1.min(mm - length_mm);
        } else if after {
            span.pixels.0 = span.pixels.0.max(px + len_px);
            span.mm.0 = span.mm.0.max(mm + len_mm);
        } else {
            return None;
        }
    }
    (span != full && span.pixels.0 < span.pixels.1 && span.mm.0 < span.mm.1).then_some(span)
}

fn size_mm(screen: &Screen) -> Vec2 {
    if let Some((w, h)) = screen.mm.filter(|&(w, h)| w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0) {
        // EDID describes the panel. The desktop dimensions include its current orientation.
        let portrait = if screen.width == screen.height { screen.rotation.swaps_axes() } else { screen.width < screen.height };
        return if portrait == (w < h) { vec2(w, h) } else { vec2(h, w) };
    }
    // Estimate a usable tile at 127 pixels per inch when the display supplies no panel size.
    vec2(screen.width.max(1) as f32, screen.height.max(1) as f32) * 0.2
}

#[derive(Clone, Copy, PartialEq)]
enum Seam {
    Horizontal,
    Vertical,
}

fn shared_seam(a: Area, b: Area) -> Option<Seam> {
    if (a.x + a.w == b.x || a.x == b.x + b.w) && a.y < b.y + b.h && b.y < a.y + a.h {
        Some(Seam::Vertical)
    } else if (a.y + a.h == b.y || a.y == b.y + b.h) && a.x < b.x + b.w && b.x < a.x + a.w {
        Some(Seam::Horizontal)
    } else {
        None
    }
}

fn has_corner(area: Area, neighbours: impl Iterator<Item = Area>) -> bool {
    let mut seams = neighbours.filter_map(|other| shared_seam(area, other));
    seams.next().is_some_and(|first| seams.any(|seam| seam != first))
}

// Map the desktop seam to a physical seam. A negative offset crosses the moving panel and a
// positive offset crosses the anchor panel, so an overlap in pixels stays an overlap on the card.
fn along_edge(a: Area, size: Vec2, b: Area, anchor: Rect) -> Option<(Rect, Seam, bool)> {
    let contains_center = |start: i32, length: i32, other_start: i32, other_length: i32| {
        let center = f64::from(start) + f64::from(length) * 0.5;
        center >= f64::from(other_start) && center < f64::from(other_start + other_length)
    };
    let seam = shared_seam(a, b)?;
    let (x, y, centered) = match seam {
        Seam::Vertical => (
            if a.x < b.x { anchor.left() - size.x } else { anchor.right() },
            anchor.top() + offset_in_mm(a.y - b.y, a.h, b.h, size.y, anchor.height()),
            contains_center(a.y, a.h, b.y, b.h),
        ),
        Seam::Horizontal => (
            anchor.left() + offset_in_mm(a.x - b.x, a.w, b.w, size.x, anchor.width()),
            if a.y < b.y { anchor.top() - size.y } else { anchor.bottom() },
            contains_center(a.x, a.w, b.x, b.w),
        ),
    };
    Some((Rect::from_min_size(pos2(x, y), size), seam, centered))
}

pub(super) fn is_clear(rect: Rect, placed: &[(Area, Rect)]) -> bool {
    placed.iter().all(|(_, other)| {
        let intersection = rect.intersect(*other);
        intersection.width() <= 0.001 || intersection.height() <= 0.001
    })
}

/// Two perpendicular desktop seams fix both coordinates in the sketch, even when pixel
/// density would project either seam's tangential offset somewhere else.
pub(super) fn corner_rect(area: Area, size: Vec2, placed: &[(Area, Rect)]) -> Option<Rect> {
    let mut best: Option<(Rect, f32)> = None;
    for &(other, side) in placed {
        let Some((vertical, Seam::Vertical, _)) = along_edge(area, size, other, side) else { continue };
        for &(other, above_or_below) in placed {
            let Some((horizontal, Seam::Horizontal, _)) = along_edge(area, size, other, above_or_below) else { continue };
            let rect = Rect::from_min_size(pos2(vertical.left(), horizontal.top()), size);
            if rect.intersect(side).height() <= 0.001 || rect.intersect(above_or_below).width() <= 0.001
                || !is_clear(rect, placed)
            {
                continue;
            }
            let shift = (rect.top() - vertical.top()).powi(2) + (rect.left() - horizontal.left()).powi(2);
            if best.is_none_or(|(_, previous)| shift < previous) {
                best = Some((rect, shift));
            }
        }
    }
    best.map(|(rect, _)| rect)
}

// Keep the edge contact, moving as little as possible to clear already placed panels. The closest
// feasible position is the ideal offset or one where an edge meets a blocking panel.
fn slide_along_edge(ideal: Rect, seam: Seam, anchor: Rect, placed: &[(Area, Rect)]) -> Option<Rect> {
    let horizontal = seam == Seam::Horizontal;
    let axes = |rect: Rect| if horizontal { (rect.left(), rect.right()) } else { (rect.top(), rect.bottom()) };
    let (start, end) = axes(ideal);
    let (anchor_start, anchor_end) = axes(anchor);
    let length = end - start;
    let contact = 0.001_f32.min(length * 0.5).min((anchor_end - anchor_start) * 0.5);
    let low = anchor_start - length + contact;
    let high = anchor_end - contact;
    let candidate = |offset: f32| {
        let shift = offset.clamp(low, high) - start;
        let delta = if horizontal { vec2(shift, 0.0) } else { vec2(0.0, shift) };
        (ideal.translate(delta), shift.abs())
    };
    let (closest, _) = candidate(start);
    if is_clear(closest, placed) {
        return Some(closest);
    }
    placed.iter().flat_map(|&(_, other)| {
        let (start, end) = axes(other);
        [start - length, end]
    }).map(candidate).filter(|(rect, _)| is_clear(*rect, placed))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(rect, _)| rect)
}

fn placement(screen: &Screen, anchors: &[(Area, Rect)], placed: &[(Area, Rect)]) -> Option<Rect> {
    let size = size_mm(screen);
    anchors.iter().filter_map(|&(area, anchor)| {
        let moving = screen.area();
        let (mut ideal, seam, centered) = along_edge(moving, size, area, anchor)?;
        let horizontal = seam == Seam::Horizontal;
        if let Some(span) = constrained_seam((moving, ideal), (area, anchor), placed, horizontal, true) {
            let position = if horizontal { moving.x } else { moving.y };
            if let Some(mm) = span.mm_at(position) {
                let delta = if horizontal { vec2(mm - ideal.left(), 0.0) } else { vec2(0.0, mm - ideal.top()) };
                ideal = ideal.translate(delta);
            }
        }
        let rect = slide_along_edge(ideal, seam, anchor, placed)?;
        Some((rect, (rect.min - ideal.min).length_sq(), centered))
    }).min_by(|a, b| a.1.total_cmp(&b.1).then_with(|| b.2.cmp(&a.2)))
        .map(|(rect, _, _)| rect)
}

pub(super) fn nearest_clear_rect(ideal: Rect, placed: &[(Area, Rect)]) -> Rect {
    if is_clear(ideal, placed) {
        return ideal;
    }
    let size = ideal.size();
    placed.iter().flat_map(|&(_, anchor)| {
        let options = [
            (pos2(anchor.left() - size.x, ideal.top()), Seam::Vertical),
            (pos2(anchor.right(), ideal.top()), Seam::Vertical),
            (pos2(ideal.left(), anchor.top() - size.y), Seam::Horizontal),
            (pos2(ideal.left(), anchor.bottom()), Seam::Horizontal),
        ];
        options.into_iter().filter_map(move |(pos, seam)| {
            slide_along_edge(Rect::from_min_size(pos, size), seam, anchor, placed)
        })
    }).min_by(|a, b| (a.min - ideal.min).length_sq().total_cmp(&(b.min - ideal.min).length_sq()))
        // The rightmost outer edge is always clear of the other panels.
        .unwrap_or_else(|| {
            let right = placed.iter().map(|(_, r)| r.right()).fold(f32::NEG_INFINITY, f32::max);
            ideal.translate(vec2(right - ideal.left(), 0.0))
        })
}

pub(super) fn physical_rects(screens: &[Screen]) -> Vec<Rect> {
    if screens.is_empty() {
        return Vec::new();
    }
    let first = screens.iter().position(|s| s.primary).unwrap_or(0);
    let anchor = &screens[first];
    let mut rects = vec![None; screens.len()];
    let main = Rect::from_min_size(Pos2::ZERO, size_mm(anchor));
    rects[first] = Some(main);
    let mut placed = vec![(anchor.area(), main)];
    let corners: Vec<_> = screens.iter().map(|s| has_corner(s.area(), screens.iter().map(Screen::area))).collect();
    loop {
        // Work outwards from the primary screen. Screens added in one pass become anchors for
        // the next pass, but already block overlap for their siblings in this pass.
        let anchors = placed.len();
        let mut waiting = None;
        for (index, screen) in screens.iter().enumerate() {
            if rects[index].is_some() {
                continue;
            }
            let area = screen.area();
            let corner = if corners[index] { corner_rect(area, size_mm(screen), &placed) } else { None };
            let Some(rect) = corner.or_else(|| placement(screen, &placed[..anchors], &placed)) else { continue };
            if corners[index] && !has_corner(area, placed.iter().map(|&(area, _)| area)) {
                waiting.get_or_insert((index, rect));
                continue;
            }
            rects[index] = Some(rect);
            placed.push((area, rect));
        }
        if placed.len() == anchors {
            // In a grid, two displays can each be waiting for the other's second seam.
            let Some((index, rect)) = waiting else { break };
            rects[index] = Some(rect);
            placed.push((screens[index].area(), rect));
        }
    }

    // If the physical sizes leave no room along the expected edge, use the nearest clear position.
    // Screens without a desktop contact start from their offset from the primary.
    let ratio = size_mm(anchor) / vec2(anchor.width.max(1) as f32, anchor.height.max(1) as f32);
    screens.iter().zip(rects).map(|(screen, rect)| {
        rect.unwrap_or_else(|| {
            let size = size_mm(screen);
            let rect = placement(screen, &placed, &placed).unwrap_or_else(|| {
                let ideal = placed.iter().filter_map(|&(area, rect)| along_edge(screen.area(), size, area, rect))
                    .max_by_key(|&(_, _, centered)| centered)
                    .map_or_else(|| {
                        let offset = vec2((screen.x - anchor.x) as f32, (screen.y - anchor.y) as f32);
                        Rect::from_min_size(Pos2::ZERO + offset * ratio, size)
                    }, |(rect, _, _)| rect);
                nearest_clear_rect(ideal, &placed)
            });
            placed.push((screen.area(), rect));
            rect
        })
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::{Rotation, valid_layout};

    #[test]
    fn physically_sized_neighbours_clear_the_same_corner() {
        let screen = |id: &str, (x, y, width, height), mm| Screen {
            id: id.into(),
            name: id.into(),
            internal: id == "laptop",
            primary: id == "main",
            x, y, width, height, mm: Some(mm),
            rotation: if width < height { Rotation::Portrait } else { Rotation::Landscape },
            scale: None,
        };
        let screens = [
            screen("portrait", (-1080, -1230, 1080, 1920), (530.0, 280.0)),
            screen("laptop", (-3840, 690, 3840, 2400), (344.0, 215.0)),
            screen("main", (0, 0, 2560, 1440), (597.0, 336.0)),
        ];
        assert!(valid_layout(&screens.iter().map(Screen::area).collect::<Vec<_>>()));
        let rects = physical_rects(&screens);

        assert_eq!(rects[0].size(), vec2(280.0, 530.0));
        assert_eq!(rects[1].size(), vec2(344.0, 215.0));
        assert_eq!(rects[0].right(), rects[2].left());
        assert_eq!(rects[1].right(), rects[2].left());
        assert!((rects[1].top() - rects[0].bottom()).abs() < 0.001);
    }

    #[test]
    fn a_fully_blocked_edge_uses_the_nearest_clear_attachment() {
        let anchor = Rect::from_min_size(Pos2::ZERO, vec2(100.0, 100.0));
        let blocker = Rect::from_min_size(pos2(-100.0, -100.0), vec2(100.0, 300.0));
        let placed = [(Area::default(), anchor), (Area::default(), blocker)];
        let ideal = Rect::from_min_size(pos2(-25.0, 30.0), vec2(25.0, 20.0));

        assert!(slide_along_edge(ideal, Seam::Vertical, anchor, &placed).is_none());
        let nearest = nearest_clear_rect(ideal, &placed);
        assert!(is_clear(nearest, &placed));
        assert_eq!(nearest.size(), ideal.size());
        assert_eq!(nearest.bottom(), anchor.top());
        assert_eq!(nearest.left(), anchor.left());
    }
}
