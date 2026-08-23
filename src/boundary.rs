// SPDX-License-Identifier: Apache-2.0
//! The boundary of the row region, classified into edges and corners.
//!
//! Endcap placement is driven entirely by this classification: which master goes where is decided
//! by whether a corner is an *outer* bottom-left or an *inner* one, and whether an edge faces
//! top, bottom, left or right. The upstream goldens carry it in the instance names —
//! `PHY_CORNER_ROW_0_OuterBottomLeft_0`, `PHY_EDGE_ROW_0_Bottom_10` — which makes them an
//! unusually direct test of whether our classification agrees with theirs.
//!
//! Rules **T7** (the region) and **T8** (edges and corners).

pub use vyges_loom::poly90::{Point, Poly90Set, Polygon90, Rect};

/// Which way an edge of the region faces, seen from the material.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeType {
    Left,
    Top,
    Right,
    Bottom,
}

/// A corner of the region. **Outer** corners are convex (the material turns around them);
/// **inner** corners are concave (the material wraps into them) — the corners of a macro's
/// cut-out, for instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CornerType {
    OuterBottomLeft,
    OuterTopLeft,
    OuterTopRight,
    OuterBottomRight,
    InnerBottomLeft,
    InnerTopLeft,
    InnerTopRight,
    InnerBottomRight,
}

/// One classified boundary segment, from `p0` to `p1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edge {
    pub kind: EdgeType,
    pub p0: Point,
    pub p1: Point,
}

impl Edge {
    pub fn is_horizontal(&self) -> bool {
        self.p0.y == self.p1.y
    }
    /// The x-range this edge spans, low first. Meaningless for a vertical edge.
    pub fn x_span(&self) -> (i32, i32) {
        (self.p0.x.min(self.p1.x), self.p0.x.max(self.p1.x))
    }
    /// The y-range this edge spans, low first. Meaningless for a horizontal edge.
    pub fn y_span(&self) -> (i32, i32) {
        (self.p0.y.min(self.p1.y), self.p0.y.max(self.p1.y))
    }
}

/// One classified corner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Corner {
    pub kind: CornerType,
    pub p: Point,
}

/// **T7** — the region the rows actually cover, inside the core.
///
/// Upstream computes this as `mask = core − rows; region = core − mask`, which is how Boost's
/// polygon set is easiest to drive. It is just the intersection, and `loom::poly90` has a test
/// pinning the two together.
///
/// `rows` must already exclude PAD-class rows: which rows count is a question about the
/// technology, and this module never touches a database.
pub fn row_region(core: Rect, rows: &[Rect]) -> Poly90Set {
    Poly90Set::from_rects(&[core]).intersection(&Poly90Set::from_rects(rows))
}

/// The direction of travel along an outline, as a unit step.
fn delta(from: Point, to: Point) -> (i32, i32) {
    ((to.x - from.x).signum(), (to.y - from.y).signum())
}

/// **T8** — classify every segment of one outline.
///
/// # Winding
///
/// The classification reads the turn made *into* each segment, so it depends on which way the
/// outline is wound. `loom::poly90` winds outer boundaries counter-clockwise and holes
/// **clockwise**, and this rule is correct for both as written.
///
/// That is worth stating because upstream looks different: Boost hands it holes wound the *same*
/// way as outers, so it classifies them as if they were outers and then flips every label
/// (`Bottom`↔`Top`, `Left`↔`Right`). Our holes arrive already reversed, so the flip is already
/// baked in and applying it again would undo it. [`the_hole_flip_is_already_baked_into_the_winding`]
/// checks the two agree.
pub fn classify_edges(outline: &[Point]) -> Vec<Edge> {
    let n = outline.len();
    if n < 4 {
        return Vec::new();
    }
    (0..n)
        .map(|i| {
            let p0 = outline[i];
            let p1 = outline[(i + 1) % n];
            let (in_dx, in_dy) = delta(outline[(i + n - 1) % n], p0);
            let (out_dx, out_dy) = delta(p0, p1);
            let kind = match (in_dx, in_dy, out_dx, out_dy) {
                (_, iy, ox, _) if iy < 0 && ox > 0 => EdgeType::Bottom,
                (ix, _, _, oy) if ix > 0 && oy > 0 => EdgeType::Right,
                (_, iy, ox, _) if iy > 0 && ox < 0 => EdgeType::Top,
                (ix, _, _, oy) if ix < 0 && oy < 0 => EdgeType::Left,
                (ix, _, _, oy) if ix < 0 && oy > 0 => EdgeType::Right,
                (_, iy, ox, _) if iy < 0 && ox < 0 => EdgeType::Top,
                (ix, _, _, oy) if ix > 0 && oy < 0 => EdgeType::Left,
                _ => EdgeType::Bottom,
            };
            Edge { kind, p0, p1 }
        })
        .collect()
}

/// **T8** — classify every vertex of one outline. Same winding contract as [`classify_edges`].
pub fn classify_corners(outline: &[Point]) -> Vec<Corner> {
    let n = outline.len();
    if n < 4 {
        return Vec::new();
    }
    (0..n)
        .map(|i| {
            let p = outline[i];
            let (in_dx, in_dy) = delta(outline[(i + n - 1) % n], p);
            let (out_dx, out_dy) = delta(p, outline[(i + 1) % n]);
            let kind = match (in_dx, in_dy, out_dx, out_dy) {
                (_, iy, ox, _) if iy < 0 && ox > 0 => CornerType::OuterBottomLeft,
                (ix, _, _, oy) if ix > 0 && oy > 0 => CornerType::OuterBottomRight,
                (_, iy, ox, _) if iy > 0 && ox < 0 => CornerType::OuterTopRight,
                (ix, _, _, oy) if ix < 0 && oy < 0 => CornerType::OuterTopLeft,
                (ix, _, _, oy) if ix < 0 && oy > 0 => CornerType::InnerBottomLeft,
                (_, iy, ox, _) if iy < 0 && ox < 0 => CornerType::InnerBottomRight,
                (ix, _, _, oy) if ix > 0 && oy < 0 => CornerType::InnerTopRight,
                _ => CornerType::InnerTopLeft,
            };
            Corner { kind, p }
        })
        .collect()
}

/// **T16** — put a hole's boundary back into the order upstream walks it.
///
/// 🔑 **Classification and traversal need OPPOSITE windings, and only classification gets one for
/// free.** `loom::poly90` hands holes wound clockwise, which is what [`classify_edges`] wants —
/// see its Winding note. Upstream receives them counter-clockwise, so it *walks* a hole the other
/// way round from us, and the walk order is not cosmetic: where two cells contend for one
/// position, whoever is placed first keeps it and the loser is skipped outright.
///
/// So reverse the sequence and each segment's endpoints, and carry every KIND through unchanged —
/// a segment's classification is a property of the boundary, not of the direction walked round it.
///
/// Measured on `abutting_macros_step_no_corners` at pin `945a9f4`: with the hole walked our way,
/// the `Right` edge at `(11020, 140000)` reached row `ROW_49_1` before the `Top` edge did and took
/// the position; upstream walks `Top` first and logs *"Skipping ROW_49_1 due to placed cells in
/// Right"*. Two cells, two rows, one reversed walk.
fn reversed_ring(edges: Vec<Edge>, corners: Vec<Corner>) -> (Vec<Edge>, Vec<Corner>) {
    let edges = edges
        .into_iter()
        .rev()
        .map(|e| Edge {
            kind: e.kind,
            p0: e.p1,
            p1: e.p0,
        })
        .collect();
    // ⚠️ A corner belongs to the vertex an edge STARTS at, so plain `.rev()` would slide the
    // corner sequence one place against the reversed edge sequence. Reversing puts the ring's
    // last vertex first; rotating by one puts vertex 0 back at the front, which is where the
    // reversed walk begins.
    let mut corners: Vec<Corner> = corners.into_iter().rev().collect();
    if !corners.is_empty() {
        corners.rotate_right(1);
    }
    (edges, corners)
}

/// Both boundary answers for a region, kept per polygon so a caller can tell which hole belongs
/// to which piece — endcap placement needs that structure, not a flat list.
pub fn classify(region: &Poly90Set) -> Vec<ClassifiedPolygon> {
    region
        .polygons()
        .into_iter()
        .map(|Polygon90 { outer, holes }| ClassifiedPolygon {
            outer_edges: classify_edges(&outer),
            outer_corners: classify_corners(&outer),
            holes: holes
                .into_iter()
                .map(|h| reversed_ring(classify_edges(&h), classify_corners(&h)))
                .collect(),
        })
        .collect()
}

/// One connected piece of the region, with its boundary classified.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassifiedPolygon {
    pub outer_edges: Vec<Edge>,
    pub outer_corners: Vec<Corner>,
    /// `(edges, corners)` per hole.
    pub holes: Vec<(Vec<Edge>, Vec<Corner>)>,
}

impl ClassifiedPolygon {
    /// Every edge, outer and holes.
    pub fn edges(&self) -> impl Iterator<Item = &Edge> {
        self.outer_edges
            .iter()
            .chain(self.holes.iter().flat_map(|(e, _)| e.iter()))
    }
    /// Every corner, outer and holes.
    pub fn corners(&self) -> impl Iterator<Item = &Corner> {
        self.outer_corners
            .iter()
            .chain(self.holes.iter().flat_map(|(_, c)| c.iter()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(x: i32, y: i32) -> Point {
        Point::new(x, y)
    }

    /// A counter-clockwise square, the way `poly90` emits an outer boundary.
    fn ccw_square() -> Vec<Point> {
        vec![pt(0, 0), pt(10, 0), pt(10, 10), pt(0, 10)]
    }

    #[test]
    fn the_four_sides_of_a_square_are_named_for_the_way_they_face() {
        let e = classify_edges(&ccw_square());
        assert_eq!(e.len(), 4);
        assert_eq!(e[0].kind, EdgeType::Bottom, "y=0, material above");
        assert_eq!(e[1].kind, EdgeType::Right, "x=10, material to the left");
        assert_eq!(e[2].kind, EdgeType::Top, "y=10, material below");
        assert_eq!(e[3].kind, EdgeType::Left, "x=0, material to the right");
    }

    #[test]
    fn the_four_corners_of_a_square_are_outer_corners() {
        let c = classify_corners(&ccw_square());
        assert_eq!(c.len(), 4);
        assert_eq!(
            c[0],
            Corner {
                kind: CornerType::OuterBottomLeft,
                p: pt(0, 0)
            }
        );
        assert_eq!(
            c[1],
            Corner {
                kind: CornerType::OuterBottomRight,
                p: pt(10, 0)
            }
        );
        assert_eq!(
            c[2],
            Corner {
                kind: CornerType::OuterTopRight,
                p: pt(10, 10)
            }
        );
        assert_eq!(
            c[3],
            Corner {
                kind: CornerType::OuterTopLeft,
                p: pt(0, 10)
            }
        );
        assert!(
            c.iter().all(|c| !matches!(
                c.kind,
                CornerType::InnerBottomLeft
                    | CornerType::InnerTopLeft
                    | CornerType::InnerTopRight
                    | CornerType::InnerBottomRight
            )),
            "a convex shape has no concave corners"
        );
    }

    #[test]
    fn an_l_shape_has_one_inner_corner_among_its_outer_ones() {
        // The notch's corner is where the material turns *into* itself.
        let region = Poly90Set::from_rects(&[Rect::new(0, 0, 10, 4), Rect::new(0, 0, 4, 10)]);
        let polys = region.polygons();
        let c = classify_corners(&polys[0].outer);
        assert_eq!(c.len(), 6);
        let inner: Vec<_> = c
            .iter()
            .filter(|c| {
                matches!(
                    c.kind,
                    CornerType::InnerBottomLeft
                        | CornerType::InnerTopLeft
                        | CornerType::InnerTopRight
                        | CornerType::InnerBottomRight
                )
            })
            .collect();
        assert_eq!(inner.len(), 1, "an L has exactly one concave corner");
        assert_eq!(inner[0].p, pt(4, 4), "and it is where the two arms meet");
        // Named by analogy with the convex case: OuterBottomLeft has material up-and-right of
        // the point, InnerBottomLeft has VOID up-and-right. Here the missing quadrant is exactly
        // x>4, y>4, so this is the inner bottom-left — the same local shape as the lower-left
        // corner of a hole.
        assert_eq!(inner[0].kind, CornerType::InnerBottomLeft);
    }

    #[test]
    fn a_hole_is_handed_back_walked_the_way_upstream_walks_it() {
        // 🔑 The upstream rule, from `Tapcell::getBoundaryAreas`: a hole reaches it from
        // `polygon_90_set_data::get_polygons`, which winds it the SAME way as an outer boundary.
        // Measured across the suite at pin `945a9f4`, every ring the reference walks — 21 outer
        // and 4 hole rings — is counter-clockwise. `loom::poly90` winds holes clockwise, so the
        // sequence has to be turned round to place cells in the reference's order.
        //
        // ⚠️ Order is not cosmetic. `placeEndcapEdge` skips a cell whose row span is already
        // taken, so whichever of two contending cells is walked to first keeps the position.
        let hole_cw = vec![pt(10, 10), pt(10, 20), pt(20, 20), pt(20, 10)];
        let (edges, corners) = reversed_ring(classify_edges(&hole_cw), classify_corners(&hole_cw));

        let walked: Vec<Point> = edges.iter().map(|e| e.p0).collect();
        assert_eq!(
            walked,
            vec![pt(10, 10), pt(20, 10), pt(20, 20), pt(10, 20)],
            "counter-clockwise, and still starting at the ring's first vertex"
        );
        for e in &edges {
            assert!(
                classify_edges(&hole_cw)
                    .iter()
                    .any(|o| o.p0 == e.p1 && o.p1 == e.p0 && o.kind == e.kind),
                "reversing a walk must not reclassify {:?} -- the flip is in the winding, and \
                 applying it a second time would undo it",
                e.p0
            );
        }
        // A corner belongs to the vertex its edge starts at; the two sequences have to stay in
        // step, or a contested corner is resolved against the wrong neighbour.
        let corner_pts: Vec<Point> = corners.iter().map(|c| c.p).collect();
        assert_eq!(corner_pts, walked, "corners walk in step with the edges");
    }

    #[test]
    fn the_hole_flip_is_already_baked_into_the_winding() {
        // Upstream receives holes wound the SAME way as outers, classifies them as if they were
        // outers, then flips every label. poly90 hands us holes already reversed. This checks the
        // two routes land on the same answer, because applying the flip again would undo it.
        let hole_cw = vec![pt(10, 10), pt(10, 20), pt(20, 20), pt(20, 10)];
        let hole_ccw = vec![pt(10, 10), pt(20, 10), pt(20, 20), pt(10, 20)];

        let flip_edge = |k: EdgeType| match k {
            EdgeType::Bottom => EdgeType::Top,
            EdgeType::Top => EdgeType::Bottom,
            EdgeType::Left => EdgeType::Right,
            EdgeType::Right => EdgeType::Left,
        };
        let flip_corner = |k: CornerType| match k {
            CornerType::OuterBottomLeft => CornerType::InnerBottomLeft,
            CornerType::OuterBottomRight => CornerType::InnerBottomRight,
            CornerType::OuterTopLeft => CornerType::InnerTopLeft,
            CornerType::OuterTopRight => CornerType::InnerTopRight,
            CornerType::InnerBottomLeft => CornerType::OuterBottomLeft,
            CornerType::InnerBottomRight => CornerType::OuterBottomRight,
            CornerType::InnerTopLeft => CornerType::OuterTopLeft,
            CornerType::InnerTopRight => CornerType::OuterTopRight,
        };

        // Compare by point, since the two windings visit the corners in different orders.
        for c_cw in classify_corners(&hole_cw) {
            let upstream = classify_corners(&hole_ccw)
                .into_iter()
                .find(|c| c.p == c_cw.p)
                .map(|c| flip_corner(c.kind))
                .expect("same corner points");
            assert_eq!(c_cw.kind, upstream, "corner {:?} disagrees", c_cw.p);
        }
        for e_cw in classify_edges(&hole_cw) {
            let upstream = classify_edges(&hole_ccw)
                .into_iter()
                .find(|e| e.p0 == e_cw.p1 && e.p1 == e_cw.p0)
                .map(|e| flip_edge(e.kind))
                .expect("the same segment, traversed the other way");
            assert_eq!(e_cw.kind, upstream, "edge {:?} disagrees", e_cw.p0);
        }
    }

    #[test]
    fn a_macro_cut_out_of_the_core_gives_four_inner_corners() {
        // The case endcap placement exists for: every corner of the hole is concave, because the
        // material wraps around the macro.
        let region = row_region(Rect::new(0, 0, 100, 100), &[Rect::new(0, 0, 100, 100)])
            .difference(&Poly90Set::from_rects(&[Rect::new(40, 40, 60, 60)]));
        let classified = classify(&region);
        assert_eq!(classified.len(), 1);
        assert_eq!(classified[0].holes.len(), 1);

        let (edges, corners) = &classified[0].holes[0];
        assert_eq!(corners.len(), 4);
        assert!(
            corners.iter().all(|c| matches!(
                c.kind,
                CornerType::InnerBottomLeft
                    | CornerType::InnerTopLeft
                    | CornerType::InnerTopRight
                    | CornerType::InnerBottomRight
            )),
            "every corner of a hole is concave: {corners:?}"
        );
        // The hole's own bottom edge (y=40) faces DOWN from the material's point of view — the
        // material there is below it, so it is that material's Top.
        let bottom = edges
            .iter()
            .find(|e| e.is_horizontal() && e.p0.y == 40)
            .expect("the hole has a segment at y=40");
        assert_eq!(bottom.kind, EdgeType::Top);
        let top = edges
            .iter()
            .find(|e| e.is_horizontal() && e.p0.y == 60)
            .expect("the hole has a segment at y=60");
        assert_eq!(top.kind, EdgeType::Bottom);
    }

    #[test]
    fn the_row_region_is_the_rows_clipped_to_the_core() {
        // T7. Rows may overhang the core; what is outside is not part of the region.
        let core = Rect::new(0, 0, 100, 100);
        let rows = [Rect::new(-20, 0, 120, 10), Rect::new(-20, 10, 120, 20)];
        let region = row_region(core, &rows);
        assert_eq!(
            region.rects(),
            vec![Rect::new(0, 0, 100, 20)],
            "clipped, and merged"
        );
        assert!(!region.contains(-5, 5));
        assert!(region.contains(0, 0));
    }

    #[test]
    fn every_edge_and_corner_is_accounted_for() {
        // A rectilinear outline has as many edges as corners, and each edge is axis-aligned.
        let region =
            Poly90Set::from_rects(&[Rect::new(0, 0, 100, 100), Rect::new(100, 20, 140, 60)])
                .difference(&Poly90Set::from_rects(&[Rect::new(30, 30, 50, 50)]));
        for p in classify(&region) {
            assert_eq!(p.outer_edges.len(), p.outer_corners.len());
            for e in p.edges() {
                assert!(
                    (e.p0.x == e.p1.x) ^ (e.p0.y == e.p1.y),
                    "a rectilinear edge moves in exactly one axis: {e:?}"
                );
            }
            for (he, hc) in &p.holes {
                assert_eq!(he.len(), hc.len());
            }
        }
    }

    #[test]
    fn degenerate_outlines_are_ignored_rather_than_panicking() {
        assert!(classify_edges(&[]).is_empty());
        assert!(
            classify_corners(&[pt(0, 0), pt(1, 0), pt(1, 1)]).is_empty(),
            "not a rectilinear loop"
        );
        assert!(classify(&Poly90Set::new()).is_empty());
    }
}
