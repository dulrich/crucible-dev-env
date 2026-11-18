use super::*;
use crate::palette::{BORDER_COLOR, MARK_COLOR};
use crucible_config::{Rgb, SurfaceStyle};

fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
    Rect::new(x, y, w, h)
}

// ---- stack_pips tests ----

#[test]
fn single_window_pane_produces_no_pips() {
    let pips = stack_pips(rect(0, 0, 800, 600), 0, 1, None, PIP_MARGIN);
    assert!(pips.is_empty());
}

#[test]
fn zero_window_pane_produces_no_pips() {
    let pips = stack_pips(rect(0, 0, 800, 600), 0, 0, None, PIP_MARGIN);
    assert!(pips.is_empty());
}

#[test]
fn pip_count_equals_len() {
    for len in 2..=6 {
        let pips = stack_pips(rect(0, 0, 800, 600), 0, len, None, PIP_MARGIN);
        assert_eq!(pips.len(), len, "expected {len} pips, got {}", pips.len());
    }
}

#[test]
fn exactly_one_focused_pip() {
    for len in 2..=5 {
        for focused in 0..len {
            let pips = stack_pips(rect(0, 0, 800, 600), focused, len, None, PIP_MARGIN);
            let active_count = pips.iter().filter(|(_, k)| *k == PipKind::Active).count();
            assert_eq!(
                active_count, 1,
                "len={len} focused={focused}: expected 1 active pip, got {active_count}"
            );
            // The focused pip must be at the `focused` index.
            assert!(
                pips[focused].1 == PipKind::Active,
                "pip at focused={focused} must be active"
            );
        }
    }
}

#[test]
fn all_pips_contained_within_pane_rect() {
    let pane = rect(100, 50, 400, 300);
    let pips = stack_pips(pane, 1, 3, None, PIP_MARGIN);
    for (r, _) in &pips {
        assert!(r.x >= pane.x, "pip left of pane: {r:?}");
        assert!(r.y >= pane.y, "pip above pane: {r:?}");
        assert!(
            r.x + r.width <= pane.x + pane.width,
            "pip right of pane: {r:?}"
        );
        assert!(
            r.y + r.height <= pane.y + pane.height,
            "pip below pane: {r:?}"
        );
    }
}

#[test]
fn focused_clamp_does_not_panic_on_out_of_bounds() {
    // focused >= len: should not panic, should clamp.
    let pips = stack_pips(rect(0, 0, 800, 600), 99, 3, None, PIP_MARGIN);
    assert_eq!(pips.len(), 3);
    let active_count = pips.iter().filter(|(_, k)| *k == PipKind::Active).count();
    assert_eq!(active_count, 1);
}

// ---- overlay chrome ----

/// The ring colour of an overlay of `style` with no accent, through the one resolver.
fn overlay_border_color(style: SurfaceStyle) -> [f32; 4] {
    ring_of_style(style).to_rgba_f32()
}

fn ring_of_style(style: SurfaceStyle) -> Rgb {
    crucible_config::surface_accent(
        crucible_config::Surface::OverlayRing,
        Some(style),
        crucible_config::SurfaceAccent::Style,
        None,
    )
    .unwrap()
}

fn overlay(border: bool, style: SurfaceStyle) -> Option<OverlayChrome> {
    Some(OverlayChrome {
        rect: rect(100, 100, 400, 300),
        border,
        ring: ring_of_style(style),
    })
}

/// A tile's focus-ring strips (empty unless they are a ring).
fn ring_of(p: &TilePlan) -> &[Rect] {
    if p.kind == StripKind::Ring {
        &p.strips
    } else {
        &[]
    }
}

/// A tile's frame strips (empty unless they are a frame).
fn frame_of(p: &TilePlan) -> &[Rect] {
    if p.kind == StripKind::Frame {
        &p.strips
    } else {
        &[]
    }
}

/// The resolved focus ring, fills, pips and overlay ring (the old `plan_chrome` slice).
fn chrome_plan(
    ring: &[Rect],
    fills: &[Rect],
    fill: [f32; 4],
    pips: &[(Rect, PipKind)],
    overlay: Option<OverlayChrome>,
    output_origin: (i32, i32),
) -> Vec<(Rect, [f32; 4])> {
    let parts = ChromeParts {
        ring: ring.to_vec(),
        fills: fills.to_vec(),
        pips: pips.to_vec(),
        ..ChromeParts::default()
    };
    resolve(
        &paint_list(&parts, fill, None, overlay),
        overlay,
        output_origin,
    )
}

/// The resolved mark ring and marked pip alone (no overlay ring is planned).
fn mark_plan(
    ring: &[Rect],
    pip: Option<Rect>,
    overlay: Option<OverlayChrome>,
    output_origin: (i32, i32),
) -> Vec<(Rect, [f32; 4])> {
    let parts = ChromeParts {
        mark_ring: ring.to_vec(),
        pips: pip.map(|p| (p, PipKind::Marked)).into_iter().collect(),
        ..ChromeParts::default()
    };
    resolve(
        &paint_list(&parts, [0.0; 4], None, None),
        overlay,
        output_origin,
    )
}

/// The resolved frames alone, in `color`.
fn frame_plan(
    frames: &[Rect],
    color: [f32; 4],
    overlay: Option<OverlayChrome>,
    output_origin: (i32, i32),
) -> Vec<(Rect, [f32; 4])> {
    let parts = ChromeParts {
        frames: frames.to_vec(),
        ..ChromeParts::default()
    };
    resolve(
        &paint_list(&parts, [0.0; 4], Some(color), None),
        overlay,
        output_origin,
    )
}

#[test]
fn paint_list_orders_every_part_front_to_back() {
    let parts = ChromeParts {
        mark_ring: vec![rect(0, 0, 1, 1)],
        pips: vec![(rect(1, 0, 1, 1), PipKind::Marked)],
        ring: vec![rect(2, 0, 1, 1)],
        fills: vec![rect(3, 0, 1, 1)],
        frames: vec![rect(4, 0, 1, 1)],
    };
    let frame_c = [0.5; 4];
    let out = resolve(
        &paint_list(&parts, APP_FILL, Some(frame_c), None),
        None,
        (0, 0),
    );
    assert_eq!(
        out,
        vec![
            (rect(0, 0, 1, 1), MARK_COLOR),
            (rect(1, 0, 1, 1), MARK_COLOR),
            (rect(2, 0, 1, 1), BORDER_COLOR),
            (rect(3, 0, 1, 1), APP_FILL),
            (rect(4, 0, 1, 1), frame_c),
        ]
    );
    // No frame colour configured: no frames.
    assert_eq!(
        resolve(&paint_list(&parts, APP_FILL, None, None), None, (0, 0)).len(),
        4
    );
}

#[test]
fn marked_pip_on_the_overlay_ring_sits_behind_the_ring() {
    // The overlay rect is (100, 100, 400, 300); its ring's top strip spans y 97..100. A marked
    // pip at y 92..100 stays clear of the overlay rect but overlaps that ring strip.
    let pip = rect(200, 92, 8, 8);
    let over = overlay(true, SurfaceStyle::N1Med);
    assert!(!pip.intersects(over.unwrap().rect));
    let parts = ChromeParts {
        pips: vec![(pip, PipKind::Marked)],
        ..ChromeParts::default()
    };
    let out = resolve(&paint_list(&parts, APP_FILL, None, over), over, (0, 0));
    assert_eq!(out.len(), 5);
    let ring_c = overlay_border_color(SurfaceStyle::N1Med);
    assert!(out[..4].iter().all(|(_, c)| *c == ring_c));
    assert!(out[..4].iter().any(|(s, _)| s.intersects(pip)));
    // The pip comes after (behind) the ring strip it overlaps.
    assert_eq!(out[4], (pip, MARK_COLOR));
}

#[test]
fn no_overlay_draws_pane_ring_and_all_pips() {
    let pips = [
        (rect(100, 100, 8, 8), PipKind::Active),
        (rect(900, 10, 8, 8), PipKind::Inactive),
    ];
    let ring = rect(0, 0, 1000, 800).border_strips(BORDER_THICKNESS);
    let out = chrome_plan(&ring, &[], APP_FILL, &pips, None, (0, 0));
    // 2 pips (in front), then 4 ring strips.
    assert_eq!(out.len(), 6);
    assert!(out[2..].iter().all(|(_, c)| *c == BORDER_COLOR));
}

// ---- mark indicator ----

#[test]
fn mark_ring_is_amber_and_at_least_mark_thickness_even_with_ring_zero() {
    let tile = rect(0, 0, 400, 300);
    let t = tiling(10, 0); // app gap 10, app ring 0 (the default)
    assert!(ring_of(&plan_tile(tile, WindowClass::App, &t, true)).is_empty());
    let strips = plan_mark_ring(tile, WindowClass::App, &t);
    assert_eq!(strips.len(), 4);
    let window = window_rect(tile, WindowClass::App, &t);
    for r in &strips {
        assert!(!r.intersects(window), "mark ring over content: {r:?}");
        assert!(inside(*r, tile));
    }
    let out = mark_plan(&strips, None, None, (0, 0));
    assert_eq!(out.len(), 4);
    assert!(out.iter().all(|(_, c)| *c == MARK_COLOR));
    assert_ne!(MARK_COLOR, BORDER_COLOR);
    assert_eq!(
        area_sum(&strips),
        i64::from((400 - 20 + 6) * (300 - 20 + 6) - (400 - 20) * (300 - 20))
    );
}

#[test]
fn mark_ring_falls_back_inside_the_tile_without_an_inset_and_for_terminals() {
    let tile = rect(0, 0, 400, 300);
    let flush = plan_mark_ring(tile, WindowClass::App, &tiling(0, 0));
    assert_eq!(area_sum(&flush), 400 * 300 - 394 * 294);
    let term = plan_mark_ring(tile, WindowClass::Terminal, &tiling(10, 0));
    assert_eq!(area_sum(&term), 400 * 300 - 394 * 294);
    assert!(term.iter().all(|r| inside(*r, tile)));
}

#[test]
fn mark_ring_clamps_to_a_thin_inset() {
    let tile = rect(0, 0, 400, 300);
    let strips = plan_mark_ring(tile, WindowClass::App, &tiling(1, 0));
    assert_eq!(strips[0].y, 0, "ring clamped to the 1px inset");
    assert!(strips.iter().all(|r| inside(*r, tile)));
}

#[test]
fn mark_pip_is_the_marked_stack_index() {
    let pane = rect(0, 0, 800, 600);
    let all = stack_pips(pane, 0, 3, None, PIP_MARGIN);
    for (i, (want, _)) in all.iter().enumerate() {
        let marked = stack_pips(pane, 0, 3, Some(i), PIP_MARGIN);
        let want_kind = if i == 0 {
            PipKind::Active
        } else {
            PipKind::Marked
        };
        assert_eq!(marked[i], (*want, want_kind), "index {i}");
        // Only the marked pip changes kind.
        for (j, p) in marked.iter().enumerate().filter(|&(j, _)| j != i) {
            assert_eq!(*p, all[j], "index {i}, pip {j}");
        }
    }
    assert!(
        stack_pips(pane, 0, 3, Some(3), PIP_MARGIN)
            .iter()
            .all(|(_, k)| *k != PipKind::Marked)
    );
    assert!(stack_pips(pane, 0, 1, Some(0), PIP_MARGIN).is_empty());
    let out = mark_plan(&[], Some(all[2].0), None, (0, 0));
    assert_eq!(out, vec![(all[2].0, MARK_COLOR)]);
}

#[test]
fn mark_chrome_respects_overlay_and_output_origin() {
    let ring = rect(100, 100, 50, 50).border_strips(3);
    let pip = rect(120, 105, 8, 8);
    let over = overlay(true, SurfaceStyle::Terminal).map(|mut o| {
        o.rect = rect(110, 100, 100, 100);
        o
    });
    // Overlay drops the ring and the intersecting pip.
    assert!(mark_plan(&ring, Some(pip), over, (0, 0)).is_empty());
    // Without an overlay everything is translated into the output frame.
    let out = mark_plan(&ring, Some(pip), None, (100, 100));
    assert_eq!(out.len(), 5);
    // The marked pip now sits with the pips, behind the mark ring (WP-R5 pinned order).
    assert_eq!(out[4].0, rect(20, 5, 8, 8));
}

const APP_FILL: [f32; 4] = [0.1, 0.2, 0.3, 1.0];

#[test]
fn overlay_suppresses_pane_ring() {
    let ring = rect(0, 0, 1000, 800).border_strips(BORDER_THICKNESS);
    let out = chrome_plan(
        &ring,
        &[],
        APP_FILL,
        &[],
        overlay(false, SurfaceStyle::N1Med),
        (0, 0),
    );
    assert!(out.is_empty(), "no ring and no overlay border: {out:?}");
}

#[test]
fn overlay_drops_intersecting_pips_only() {
    let pips = [
        (rect(120, 110, 8, 8), PipKind::Active),
        (rect(900, 10, 8, 8), PipKind::Inactive),
    ];
    let out = chrome_plan(
        &[],
        &[],
        APP_FILL,
        &pips,
        overlay(false, SurfaceStyle::N1Med),
        (0, 0),
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].0.x, 900);
}

#[test]
fn overlay_ring_sits_outside_the_rect() {
    let out = chrome_plan(
        &[],
        &[],
        APP_FILL,
        &[],
        overlay(true, SurfaceStyle::N1Med),
        (0, 0),
    );
    assert_eq!(out.len(), 4);
    let t = BORDER_THICKNESS;
    let inner = rect(100, 100, 400, 300);
    for (strip, color) in &out {
        assert_eq!(*color, overlay_border_color(SurfaceStyle::N1Med));
        assert!(!strip.intersects(inner), "strip inside: {strip:?}");
        assert!(strip.x >= 100 - t && strip.y >= 100 - t);
        assert!(strip.x + strip.width <= 500 + t);
        assert!(strip.y + strip.height <= 400 + t);
    }
    // Top strip spans the expanded width and sits directly above the rect.
    assert_eq!(out[0].0.y, 100 - t);
    assert_eq!(out[0].0.width, 400 + 2 * t);
}

#[test]
fn overlay_ring_is_translated_per_output() {
    let out = chrome_plan(
        &[],
        &[],
        APP_FILL,
        &[],
        overlay(true, SurfaceStyle::Terminal),
        (50, 20),
    );
    assert_eq!(out[0].0.x, 100 - BORDER_THICKNESS - 50);
    assert_eq!(out[0].0.y, 100 - BORDER_THICKNESS - 20);
    assert_eq!(out[0].1, BORDER_COLOR);
}

#[test]
fn overlay_border_colours_per_style() {
    assert_eq!(overlay_border_color(SurfaceStyle::Terminal), BORDER_COLOR);
    assert_eq!(
        overlay_border_color(SurfaceStyle::N1Dark),
        overlay_border_color(SurfaceStyle::N1Med)
    );
    assert_ne!(
        overlay_border_color(SurfaceStyle::N1Light),
        overlay_border_color(SurfaceStyle::N1Med)
    );
}

#[test]
fn overlay_ring_paints_the_resolved_colour() {
    // The resolved ring colour (any accent rule; the rules are tested in `crucible-config`)
    // paints the ring.
    let teal = Rgb(0x33, 0xcc, 0xcc);
    let o = OverlayChrome {
        ring: teal,
        ..overlay(true, SurfaceStyle::N1Dark).unwrap()
    };
    let parts = ChromeParts::default();
    let list = paint_list(&parts, [0.0; 4], None, Some(o));
    assert!(list.iter().any(|e| e.color == teal.to_rgba_f32()));
}

// ---- per-class tile planning ----

fn tiling(app_gap: u32, app_ring: u32) -> TilingConfig {
    let mut t = TilingConfig::default();
    t.app.gap = app_gap;
    t.app.ring = app_ring;
    t
}

fn area_sum(rects: &[Rect]) -> i64 {
    rects.iter().map(|r| r.area()).sum()
}

fn inside(inner: Rect, outer: Rect) -> bool {
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner.x + inner.width <= outer.x + outer.width
        && inner.y + inner.height <= outer.y + outer.height
}

#[test]
fn terminal_ring_is_inside_the_tile_and_has_no_fill() {
    let tile = rect(100, 50, 400, 300);
    let plan = plan_tile(tile, WindowClass::Terminal, &TilingConfig::default(), true);
    assert_eq!(ring_of(&plan).len(), 4);
    assert!(ring_of(&plan).iter().all(|r| inside(*r, tile)));
    // ring 3 around the tile edge: area = w*h - (w-6)*(h-6).
    assert_eq!(area_sum(ring_of(&plan)), 400 * 300 - 394 * 294);
    assert!(plan.fill.is_empty());
    // An unfocused terminal draws nothing.
    let idle = plan_tile(tile, WindowClass::Terminal, &TilingConfig::default(), false);
    assert_eq!(idle, TilePlan::default());
}

#[test]
fn terminal_ring_inset_and_unfocused_frame() {
    let tile = rect(100, 50, 400, 300);
    let mut t = TilingConfig::default();
    t.terminal.gap = 9;
    t.terminal.ring = 1;
    t.terminal.ring_inset = 4;
    // No frame colour: unfocused draws nothing, focused ring sits 4px inside the tile.
    assert_eq!(
        plan_tile(tile, WindowClass::Terminal, &t, false),
        TilePlan::default()
    );
    let focused = plan_tile(tile, WindowClass::Terminal, &t, true);
    assert!(frame_of(&focused).is_empty());
    assert_eq!(area_sum(ring_of(&focused)), 392 * 292 - 390 * 290);
    assert!(ring_of(&focused).iter().all(|r| inside(*r, tile.inset(4))));
    // With a frame colour the unfocused tile carries the same strips as a frame.
    t.terminal.frame = Some(Rgb(0x3a, 0x3a, 0x3a));
    let idle = plan_tile(tile, WindowClass::Terminal, &t, false);
    assert!(ring_of(&idle).is_empty());
    assert_eq!(frame_of(&idle), ring_of(&focused));
    assert!(frame_of(&plan_tile(tile, WindowClass::Terminal, &t, true)).is_empty());
    // The mark ring follows the same inset.
    let mark = plan_mark_ring(tile, WindowClass::Terminal, &t);
    assert!(mark.iter().all(|r| inside(*r, tile.inset(4))));
    // App tiles never carry a frame.
    assert!(frame_of(&plan_tile(tile, WindowClass::App, &t, false)).is_empty());
    // Frames clip around an overlay.
    let overlay = OverlayChrome {
        rect: rect(0, 0, 100000, 100000),
        border: false,
        ring: crucible_config::N1_MED.accent,
    };
    assert!(frame_plan(frame_of(&idle), [0.0; 4], Some(overlay), (0, 0)).is_empty());
    assert_eq!(
        frame_plan(frame_of(&idle), [0.0; 4], None, (100, 50)).len(),
        4
    );
}

#[test]
fn app_ring_is_outside_the_window_and_inside_the_tile() {
    let tile = rect(0, 0, 400, 300);
    let t = tiling(10, 4);
    let window = window_rect(tile, WindowClass::App, &t);
    assert_eq!(window, rect(10, 10, 380, 280));
    let plan = plan_tile(tile, WindowClass::App, &t, true);
    for r in ring_of(&plan) {
        assert!(inside(*r, tile), "ring leaves the tile: {r:?}");
        assert!(!r.intersects(window), "ring over content: {r:?}");
    }
    // The ring hugs the window: the top strip starts ring-thickness above it.
    assert_eq!(ring_of(&plan)[0].y, 10 - 4);
    assert_eq!(area_sum(ring_of(&plan)), 388 * 288 - 380 * 280);
}

#[test]
fn app_fill_and_ring_tile_the_inset_exactly() {
    let tile = rect(7, 9, 401, 299);
    for (gap, ring) in [(10, 4), (10, 10), (10, 0), (3, 3)] {
        let t = tiling(gap, ring);
        let window = window_rect(tile, WindowClass::App, &t);
        let inset_area = tile.area() - window.area();
        let focused = plan_tile(tile, WindowClass::App, &t, true);
        let all: Vec<Rect> = ring_of(&focused)
            .iter()
            .chain(&focused.fill)
            .copied()
            .collect();
        assert_eq!(area_sum(&all), inset_area, "gap={gap} ring={ring}");
        for (i, a) in all.iter().enumerate() {
            assert!(inside(*a, tile) && !a.intersects(window));
            for b in &all[i + 1..] {
                assert!(!a.intersects(*b), "overlap {a:?} {b:?}");
            }
        }
        // Unfocused: the fill alone covers the whole inset.
        let idle = plan_tile(tile, WindowClass::App, &t, false);
        assert!(ring_of(&idle).is_empty());
        assert_eq!(area_sum(&idle.fill), inset_area);
    }
}

#[test]
fn app_gap_zero_draws_nothing_and_ring_clamps_to_the_inset() {
    let tile = rect(0, 0, 100, 100);
    assert_eq!(
        plan_tile(tile, WindowClass::App, &tiling(0, 0), true),
        TilePlan::default()
    );
    // A ring wider than the (clamped) inset never leaves the tile.
    let plan = plan_tile(rect(0, 0, 10, 10), WindowClass::App, &tiling(50, 50), true);
    assert!(
        ring_of(&plan)
            .iter()
            .chain(&plan.fill)
            .all(|r| inside(*r, rect(0, 0, 10, 10)))
    );
}

#[test]
fn pips_centre_in_a_strip_that_fits_them() {
    assert_eq!(pip_margin(6), PIP_MARGIN);
    assert_eq!(pip_margin(8), 0);
    assert_eq!(pip_margin(12), 2);
    let pips = stack_pips(rect(0, 0, 400, 300), 0, 2, None, pip_margin(12));
    assert_eq!(pips[1].0.y, 2);
    assert_eq!(pips[1].0.x + PIP_SIZE, 400 - 2);
}
