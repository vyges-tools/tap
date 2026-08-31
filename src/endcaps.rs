// SPDX-License-Identifier: Apache-2.0
//! Boundary cells — rule **T9**.
//!
//! Every corner of the row region gets a corner cell, and every stretch of boundary between the
//! corners gets filled with edge cells. Which master goes where is decided by the corner or edge
//! *type* from [`crate::boundary`], and then by the **orientation of the row it lands in** — a
//! bottom-left corner in a flipped row wants the top-left master, because the row is upside down.
//!
//! Three things here are worth reading before changing anything:
//!
//! - **inner corners are filled with EDGE masters, not corner masters.** A concave corner is the
//!   inside of a notch; the cell that belongs there is the one that caps an edge.
//! - **corners are placed first, and the edges then shrink to avoid them.** A horizontal edge
//!   whose end is already covered by a corner cell starts (or stops) at that cell's far side.
//! - **the fill picks a master that divides the remaining span exactly** where one exists, so a
//!   boundary is covered without a ragged last cell.
//!
//! Nothing here touches a database.

use vyges_loom::poly90::Rect;

use crate::boundary::{Corner, CornerType, Edge, EdgeType};
use crate::tapcells::{check_symmetry, Master, Placement, Row};

/// The masters to use at each position on the boundary.
///
/// Every field is optional because a technology need not provide every kind, and a missing master
/// means "place nothing here" rather than an error — upstream fills the gaps from the LEF's own
/// master types before this point.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EndcapMasters {
    pub left_top_corner: Option<Master>,
    pub right_top_corner: Option<Master>,
    pub left_bottom_corner: Option<Master>,
    pub right_bottom_corner: Option<Master>,
    /// Used at **inner** (concave) corners.
    pub left_top_edge: Option<Master>,
    pub right_top_edge: Option<Master>,
    pub left_bottom_edge: Option<Master>,
    pub right_bottom_edge: Option<Master>,
    /// Horizontal fill, widest-first at use.
    pub top_edge: Vec<Master>,
    pub bottom_edge: Vec<Master>,
    pub left_edge: Option<Master>,
    pub right_edge: Option<Master>,
    pub prefix: String,
}

impl EndcapMasters {
    /// The extent of a placed master, by name — for recording what a row now holds.
    ///
    /// ⚠️ **Every field, not just the edges.** A row's occupancy has to cover whatever was put
    /// there, and the inner-corner masters are placed by the edge paths too.
    pub fn extent_of(&self, name: &str) -> Option<(i32, i32)> {
        let singles = [
            &self.left_top_corner,
            &self.right_top_corner,
            &self.left_bottom_corner,
            &self.right_bottom_corner,
            &self.left_top_edge,
            &self.right_top_edge,
            &self.left_bottom_edge,
            &self.right_bottom_edge,
            &self.left_edge,
            &self.right_edge,
        ];
        singles
            .into_iter()
            .flatten()
            .chain(self.top_edge.iter())
            .chain(self.bottom_edge.iter())
            .find(|m| m.name == name)
            .map(|m| (m.width, m.height))
    }

    /// The site corner placement works in, taken from the first master that states one.
    ///
    /// The order is upstream's and is arbitrary but fixed; it only matters when a technology
    /// mixes sites between masters, which would be a library bug rather than something to guess
    /// about.
    pub fn corner_site(&self) -> Option<&str> {
        [
            &self.left_bottom_corner,
            &self.right_bottom_corner,
            &self.left_top_corner,
            &self.right_top_corner,
            &self.left_bottom_edge,
            &self.right_bottom_edge,
            &self.left_top_edge,
            &self.right_top_edge,
        ]
        .into_iter()
        .flatten()
        .map(|m| m.site.as_str())
        .next()
    }

    /// The site the horizontal fill works in.
    pub fn horizontal_site(&self) -> Option<&str> {
        self.top_edge
            .last()
            .or_else(|| self.bottom_edge.last())
            .map(|m| m.site.as_str())
    }

    /// Which master caps this corner, given the orientation of the row it falls in.
    ///
    /// A flipped row turns the geometry upside down, so a *bottom* corner in an `MX` row is
    /// capped with the *top* master. Getting this backwards produces cells that look placed and
    /// are electrically wrong.
    pub fn for_corner(&self, kind: CornerType, row_upright: bool) -> Option<&Master> {
        use CornerType::*;
        let m = match (kind, row_upright) {
            (OuterBottomLeft, true) | (OuterTopLeft, false) => &self.left_bottom_corner,
            (OuterBottomRight, true) | (OuterTopRight, false) => &self.right_bottom_corner,
            (OuterTopLeft, true) | (OuterBottomLeft, false) => &self.left_top_corner,
            (OuterTopRight, true) | (OuterBottomRight, false) => &self.right_top_corner,
            // Inner corners take EDGE masters: a concave corner is the inside of a notch.
            (InnerBottomLeft, true) | (InnerTopLeft, false) => &self.left_bottom_edge,
            (InnerBottomRight, true) | (InnerTopRight, false) => &self.right_bottom_edge,
            (InnerTopLeft, true) | (InnerBottomLeft, false) => &self.left_top_edge,
            (InnerTopRight, true) | (InnerBottomRight, false) => &self.right_top_edge,
        };
        m.as_ref()
    }
}

/// Mirror an orientation across Y — odb's `dbOrientType::flipY`.
pub fn flip_y(orient: &str) -> &'static str {
    match orient {
        "R0" => "MY",
        "MY" => "R0",
        "MX" => "R180",
        "R180" => "MX",
        "R90" => "MXR90",
        "MXR90" => "R90",
        "R270" => "MYR90",
        "MYR90" => "R270",
        _ => "R0",
    }
}

/// Where a corner cell's lower-left goes, given the corner point and the cell's size.
///
/// The corner point is a *vertex of the region*, and the cell has to sit inside it — so which way
/// it is pulled back depends on which quadrant the material occupies.
fn corner_origin(kind: CornerType, p: (i32, i32), w: i32, h: i32) -> (i32, i32) {
    use CornerType::*;
    match kind {
        OuterBottomLeft | InnerTopRight => p,
        OuterBottomRight | InnerTopLeft => (p.0 - w, p.1),
        OuterTopRight | InnerBottomLeft => (p.0 - w, p.1 - h),
        OuterTopLeft | InnerBottomRight => (p.0, p.1 - h),
    }
}

/// Corner types whose cell is mirrored across Y when the master allows it — the right-hand and
/// inner-left families, which face the other way.
fn corner_wants_flip(kind: CornerType) -> bool {
    use CornerType::*;
    matches!(
        kind,
        OuterBottomRight | OuterTopRight | InnerBottomLeft | InnerTopLeft
    )
}

/// **T9** — the cell for one corner, or `None` when the technology has nothing to put there.
///
/// `row` is the row the corner falls in; upstream looks it up and places nothing when no row of
/// the right site reaches the corner, which is why a boundary can have more corners than cells.
pub fn place_corner(
    corner: &Corner,
    row: &Row,
    masters: &EndcapMasters,
    phy_index: &mut usize,
) -> Option<Placement> {
    let upright = row.orient == "R0";
    let master = masters.for_corner(corner.kind, upright)?;

    let (x, y) = corner_origin(
        corner.kind,
        (corner.p.x, corner.p.y),
        master.width,
        master.height,
    );

    let orient = if corner_wants_flip(corner.kind) && master.symmetry_y {
        flip_y(&row.orient).to_string()
    } else {
        row.orient.clone()
    };
    if !check_symmetry(master.symmetry_x, master.symmetry_y, &orient) {
        return None;
    }

    let name = format!(
        "{}CORNER_{}_{:?}_{}",
        masters.prefix, row.name, corner.kind, phy_index
    );
    *phy_index += 1;
    Some(Placement {
        name,
        master: master.name.clone(),
        x,
        y,
        orient,
    })
}

/// **T9** — fill a horizontal stretch of boundary with edge cells.
///
/// `corner_cells` are the corner cells already placed in this row: where one covers an end of the
/// edge, the fill starts (or stops) at its far side instead of overlapping it.
pub fn place_edge_horizontal(
    edge: &Edge,
    row: &Row,
    corner_cells: &[Rect],
    masters: &EndcapMasters,
    phy_index: &mut usize,
) -> Vec<Placement> {
    let upright = row.orient == "R0";
    let mut choices: Vec<&Master> = match (edge.kind, upright) {
        (EdgeType::Top, true) | (EdgeType::Bottom, false) => masters.top_edge.iter().collect(),
        (EdgeType::Bottom, true) | (EdgeType::Top, false) => masters.bottom_edge.iter().collect(),
        _ => return Vec::new(),
    };
    choices.retain(|m| m.width > 0);
    if choices.is_empty() {
        return Vec::new();
    }
    // Widest first: the fill prefers the largest cell that divides the remaining span.
    choices.sort_by(|a, b| b.width.cmp(&a.width));

    let (mut x0, mut x1) = edge.x_span();
    for c in corner_cells {
        if c.x0 == x0 {
            x0 = c.x1;
        }
        if c.x1 == x1 {
            x1 = c.x0;
        }
    }

    let mut out = Vec::new();
    let mut x = x0;
    while x < x1 {
        let remaining = x1 - x;
        // Upstream `fillEndcapEdge::pick_next_master`: walk the width-sorted list, SKIPPING any
        // master that cannot legally sit in this row's orientation, and take the first — widest —
        // whose width divides the REMAINING span exactly. If none does, take the last one still
        // standing, which is the narrowest that PASSED symmetry.
        //
        // ⛔ **The symmetry test belongs inside this walk.** Choosing first and testing afterwards
        // rejects a master upstream would simply have skipped over, and falls back to one upstream
        // would never have considered. `checkSymmetry` is false for any MX row whose master lacks
        // X symmetry, and rows alternate R0/MX, so this is ordinary rather than exotic.
        //
        // ⚠️ The comment here previously justified the old order by saying upstream `continue`s
        // "without advancing, which cannot terminate". It does not: that `continue` is inside the
        // master-selection loop and advances to the next master. The rationale was a misreading.
        let mut fallback: Option<&Master> = None;
        let mut chosen: Option<&Master> = None;
        for &m in choices.iter() {
            if !check_symmetry(m.symmetry_x, m.symmetry_y, &row.orient) {
                continue;
            }
            fallback = Some(m);
            if remaining % m.width == 0 {
                chosen = Some(m);
                break;
            }
        }
        let Some(master) = chosen.or(fallback) else {
            // No master in this list can legally sit in this row. Upstream reaches TAP-20 here.
            break;
        };
        if x + master.width > x1 {
            // Upstream raises TAP-20 and aborts the run. Here the caller decides what an
            // unfillable boundary means, so the fill stops and reports what it managed.
            break;
        }

        out.push(Placement {
            name: format!(
                "{}EDGE_{}_{:?}_{}",
                masters.prefix, row.name, edge.kind, phy_index
            ),
            master: master.name.clone(),
            x,
            y: row.bbox.y0,
            orient: row.orient.clone(),
        });
        *phy_index += 1;
        x += master.width;
    }
    out
}

/// **T9** — cap one end of a row with a vertical edge cell.
///
/// Returns `None` when a corner cell already covers that end of the row: a corner at the end of a
/// row *is* the row's cap, and placing another would overlap it.
pub fn place_edge_vertical(
    kind: EdgeType,
    row: &Row,
    corner_cells: &[Rect],
    masters: &EndcapMasters,
    phy_index: &mut usize,
) -> Option<Placement> {
    let master = match kind {
        EdgeType::Left => masters.left_edge.as_ref()?,
        EdgeType::Right => masters.right_edge.as_ref()?,
        _ => return None,
    };
    if master.site != row.site {
        return None;
    }

    let covered = corner_cells.iter().any(|c| match kind {
        EdgeType::Right => c.x1 == row.bbox.x1,
        EdgeType::Left => c.x0 == row.bbox.x0,
        _ => false,
    });
    if covered {
        return None;
    }

    let (x, orient) = match kind {
        EdgeType::Right => {
            // When the same master caps both ends it has to be mirrored on the right, or the two
            // ends would be identical cells facing the same way.
            let same_both_ends = masters.left_edge == masters.right_edge;
            let o = if same_both_ends && master.symmetry_y {
                flip_y(&row.orient).to_string()
            } else {
                row.orient.clone()
            };
            (row.bbox.x1 - master.width, o)
        }
        _ => (row.bbox.x0, row.orient.clone()),
    };

    if !check_symmetry(master.symmetry_x, master.symmetry_y, &orient) {
        return None;
    }

    let name = format!(
        "{}EDGE_{}_{:?}_{}",
        masters.prefix, row.name, kind, phy_index
    );
    *phy_index += 1;
    Some(Placement {
        name,
        master: master.name.clone(),
        x,
        y: row.bbox.y0,
        orient,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use vyges_loom::poly90::Point;

    fn m(name: &str, w: i32, h: i32) -> Master {
        Master {
            name: name.into(),
            site: "core".into(),
            width: w,
            height: h,
            symmetry_x: true,
            symmetry_y: true,
        }
    }

    fn masters() -> EndcapMasters {
        EndcapMasters {
            left_bottom_corner: Some(m("LBC", 10, 10)),
            right_bottom_corner: Some(m("RBC", 10, 10)),
            left_top_corner: Some(m("LTC", 10, 10)),
            right_top_corner: Some(m("RTC", 10, 10)),
            left_bottom_edge: Some(m("LBE", 10, 10)),
            right_bottom_edge: Some(m("RBE", 10, 10)),
            left_top_edge: Some(m("LTE", 10, 10)),
            right_top_edge: Some(m("RTE", 10, 10)),
            top_edge: vec![m("TE4", 40, 10), m("TE1", 10, 10)],
            bottom_edge: vec![m("BE4", 40, 10), m("BE1", 10, 10)],
            left_edge: Some(m("LE", 10, 10)),
            right_edge: Some(m("RE", 10, 10)),
            prefix: "PHY_".into(),
        }
    }

    fn row(name: &str, orient: &str, x0: i32, x1: i32, y0: i32) -> Row {
        Row {
            name: name.into(),
            site: "core".into(),
            orient: orient.into(),
            bbox: Rect::new(x0, y0, x1, y0 + 10),
            site_width: 10,
        }
    }

    fn corner(kind: CornerType, x: i32, y: i32) -> Corner {
        Corner {
            kind,
            p: Point::new(x, y),
        }
    }

    #[test]
    fn a_flipped_row_takes_the_master_from_the_other_side() {
        // The rule most likely to be got backwards: the row is upside down, so a bottom corner
        // wants the top master.
        let ms = masters();
        assert_eq!(
            ms.for_corner(CornerType::OuterBottomLeft, true)
                .unwrap()
                .name,
            "LBC"
        );
        assert_eq!(
            ms.for_corner(CornerType::OuterBottomLeft, false)
                .unwrap()
                .name,
            "LTC"
        );
        assert_eq!(
            ms.for_corner(CornerType::OuterTopRight, true).unwrap().name,
            "RTC"
        );
        assert_eq!(
            ms.for_corner(CornerType::OuterTopRight, false)
                .unwrap()
                .name,
            "RBC"
        );
    }

    #[test]
    fn inner_corners_are_filled_with_edge_masters_not_corner_masters() {
        // A concave corner is the inside of a notch; the cell that belongs there caps an edge.
        let ms = masters();
        for (kind, want) in [
            (CornerType::InnerBottomLeft, "LBE"),
            (CornerType::InnerBottomRight, "RBE"),
            (CornerType::InnerTopLeft, "LTE"),
            (CornerType::InnerTopRight, "RTE"),
        ] {
            assert_eq!(ms.for_corner(kind, true).unwrap().name, want, "{kind:?}");
        }
    }

    #[test]
    fn a_corner_cell_is_pulled_back_into_the_material() {
        // The corner POINT is a vertex of the region; the cell has to end up inside it.
        assert_eq!(
            corner_origin(CornerType::OuterBottomLeft, (100, 200), 10, 20),
            (100, 200)
        );
        assert_eq!(
            corner_origin(CornerType::OuterBottomRight, (100, 200), 10, 20),
            (90, 200)
        );
        assert_eq!(
            corner_origin(CornerType::OuterTopRight, (100, 200), 10, 20),
            (90, 180)
        );
        assert_eq!(
            corner_origin(CornerType::OuterTopLeft, (100, 200), 10, 20),
            (100, 180)
        );
        // Inner corners are pulled the opposite way, because the material is on the other side.
        assert_eq!(
            corner_origin(CornerType::InnerTopRight, (100, 200), 10, 20),
            (100, 200)
        );
        assert_eq!(
            corner_origin(CornerType::InnerBottomLeft, (100, 200), 10, 20),
            (90, 180)
        );
    }

    #[test]
    fn flipping_across_y_is_its_own_inverse() {
        for o in ["R0", "MY", "MX", "R180", "R90", "MXR90", "R270", "MYR90"] {
            assert_eq!(flip_y(flip_y(o)), o, "flipping {o} twice must return it");
            assert_ne!(flip_y(o), o);
        }
        assert_eq!(flip_y("R0"), "MY");
        assert_eq!(flip_y("MX"), "R180");
    }

    #[test]
    fn a_corner_is_placed_with_its_type_in_the_name() {
        let ms = masters();
        let mut idx = 3;
        let p = place_corner(
            &corner(CornerType::OuterBottomLeft, 0, 0),
            &row("ROW_0", "R0", 0, 100, 0),
            &ms,
            &mut idx,
        )
        .expect("placed");
        assert_eq!(
            p.name, "PHY_CORNER_ROW_0_OuterBottomLeft_3",
            "the golden's own format"
        );
        assert_eq!(p.master, "LBC");
        assert_eq!((p.x, p.y), (0, 0));
        assert_eq!(idx, 4, "the physical counter advanced");
    }

    #[test]
    fn right_hand_corners_are_mirrored_when_the_master_allows_it() {
        let ms = masters();
        let mut idx = 0;
        let right = place_corner(
            &corner(CornerType::OuterBottomRight, 100, 0),
            &row("ROW_0", "R0", 0, 100, 0),
            &ms,
            &mut idx,
        )
        .expect("placed");
        assert_eq!(right.orient, "MY", "a right corner faces the other way");

        let left = place_corner(
            &corner(CornerType::OuterBottomLeft, 0, 0),
            &row("ROW_0", "R0", 0, 100, 0),
            &ms,
            &mut idx,
        )
        .expect("placed");
        assert_eq!(
            left.orient, "R0",
            "a left corner keeps the row's orientation"
        );
    }

    #[test]
    fn an_asymmetric_master_is_not_forced_into_a_row_it_cannot_sit_in() {
        let mut ms = masters();
        // In an MX row a bottom-left corner takes the TOP-left master (the row is upside down),
        // so that is the one whose symmetry decides the outcome.
        ms.left_top_corner = Some(Master {
            symmetry_x: false,
            ..m("LTC", 10, 10)
        });
        let mut idx = 0;
        assert!(place_corner(
            &corner(CornerType::OuterBottomLeft, 0, 0),
            &row("ROW_0", "MX", 0, 100, 0),
            &ms,
            &mut idx
        )
        .is_none());
        assert_eq!(idx, 0, "and nothing was numbered");
    }

    #[test]
    fn a_horizontal_edge_is_filled_with_the_master_that_divides_it_exactly() {
        let ms = masters();
        let mut idx = 0;
        let e = Edge {
            kind: EdgeType::Bottom,
            p0: Point::new(0, 0),
            p1: Point::new(80, 0),
        };
        let out = place_edge_horizontal(&e, &row("ROW_0", "R0", 0, 80, 0), &[], &ms, &mut idx);
        assert_eq!(out.len(), 2, "80 divides by the 40-wide master");
        assert!(out.iter().all(|p| p.master == "BE4"));
        assert_eq!(out.iter().map(|p| p.x).collect::<Vec<_>>(), vec![0, 40]);
    }

    #[test]
    fn a_master_that_cannot_sit_in_the_row_is_skipped_over_not_selected_then_rejected() {
        // Upstream's pick_next_master skips a master failing checkSymmetry and keeps walking, so
        // the exact divisor may be a NARROWER master and the fallback is the narrowest that
        // PASSED. Selecting first and testing afterwards abandons the fill instead.
        //
        // MX row, so checkSymmetry is symmetry_x. The 40-wide master divides the 80 span exactly
        // but is not X-symmetric; the 20-wide one is. Upstream fills with four of the 20.
        // ⚠️ A Bottom edge in an MX row draws from top_edge, not bottom_edge — the same R0
        // crossing placeEndcapEdgeHorizontal applies. Setting the wrong list tests nothing.
        let mut ms = masters();
        ms.top_edge = vec![
            Master { symmetry_x: false, ..m("BE_WIDE", 40, 10) },
            Master { symmetry_x: true, ..m("BE_NARROW", 20, 10) },
        ];
        let e = Edge {
            kind: EdgeType::Bottom,
            p0: Point::new(0, 0),
            p1: Point::new(80, 0),
        };
        let mut idx = 0;
        let out = place_edge_horizontal(&e, &row("ROW_0", "MX", 0, 80, 0), &[], &ms, &mut idx);
        assert_eq!(out.len(), 4, "the span is filled, not abandoned: {out:?}");
        assert!(
            out.iter().all(|p| p.master == "BE_NARROW"),
            "the wide master cannot sit in an MX row and is skipped over: {out:?}"
        );
        assert_eq!(out.iter().map(|p| p.x).collect::<Vec<_>>(), vec![0, 20, 40, 60]);
    }

    #[test]
    fn a_span_no_master_divides_falls_back_to_the_narrowest() {
        let ms = masters();
        let mut idx = 0;
        // 50 is not a multiple of 40; the 10-wide master finishes it off.
        let e = Edge {
            kind: EdgeType::Bottom,
            p0: Point::new(0, 0),
            p1: Point::new(50, 0),
        };
        let out = place_edge_horizontal(&e, &row("ROW_0", "R0", 0, 50, 0), &[], &ms, &mut idx);
        // The choice is re-made at every step against what REMAINS, so the narrow cell goes
        // first (50 % 10 == 0) and the wide one finishes the job (40 % 40 == 0) -- not the other
        // way round, which is the intuitive but wrong reading.
        assert_eq!(out.iter().map(|p| p.x).collect::<Vec<_>>(), vec![0, 10]);
        assert_eq!(out[0].master, "BE1");
        assert_eq!(out[1].master, "BE4");
        assert_eq!(
            out.last().unwrap().x + 40,
            50,
            "the fill ends exactly at the boundary"
        );
    }

    #[test]
    fn a_corner_cell_shortens_the_edge_rather_than_being_overlapped() {
        let ms = masters();
        let mut idx = 0;
        let e = Edge {
            kind: EdgeType::Bottom,
            p0: Point::new(0, 0),
            p1: Point::new(100, 0),
        };
        // Corner cells occupy 0..10 and 90..100.
        let corners = [Rect::new(0, 0, 10, 10), Rect::new(90, 0, 100, 10)];
        let out =
            place_edge_horizontal(&e, &row("ROW_0", "R0", 0, 100, 0), &corners, &ms, &mut idx);
        assert!(!out.is_empty());
        assert_eq!(out[0].x, 10, "the fill starts after the corner cell");
        let last = out.last().unwrap();
        assert!(
            last.x + 10 <= 90,
            "and stops before the far corner cell: {last:?}"
        );
    }

    #[test]
    fn a_row_end_already_capped_by_a_corner_gets_no_vertical_cell() {
        let ms = masters();
        let mut idx = 0;
        let r = row("ROW_0", "R0", 0, 100, 0);
        // A corner cell already sits at the left end.
        let capped = [Rect::new(0, 0, 10, 10)];
        assert!(place_edge_vertical(EdgeType::Left, &r, &capped, &ms, &mut idx).is_none());
        // The right end is still open. Distinct left/right masters each face their own way, so
        // there is nothing to mirror.
        let right =
            place_edge_vertical(EdgeType::Right, &r, &capped, &ms, &mut idx).expect("placed");
        assert_eq!(right.x, 90, "flush against the row's right end");
        assert_eq!(right.orient, "R0", "distinct masters are not mirrored");
    }

    #[test]
    fn one_master_capping_both_ends_is_mirrored_on_the_right() {
        // When the SAME master caps both ends it has to be flipped on one of them, or the two
        // ends would be identical cells facing the same way.
        let mut ms = masters();
        ms.left_edge = Some(m("E", 10, 10));
        ms.right_edge = Some(m("E", 10, 10));
        let mut idx = 0;
        let r = row("ROW_0", "R0", 0, 100, 0);
        let left = place_edge_vertical(EdgeType::Left, &r, &[], &ms, &mut idx).expect("placed");
        let right = place_edge_vertical(EdgeType::Right, &r, &[], &ms, &mut idx).expect("placed");
        assert_eq!(left.orient, "R0");
        assert_eq!(
            right.orient, "MY",
            "mirrored, because it is the same cell as the left end"
        );
        assert_eq!((left.x, right.x), (0, 90));
    }

    #[test]
    fn a_missing_master_places_nothing_rather_than_failing() {
        let mut ms = masters();
        ms.left_edge = None;
        ms.left_bottom_corner = None;
        let mut idx = 0;
        let r = row("ROW_0", "R0", 0, 100, 0);
        assert!(place_edge_vertical(EdgeType::Left, &r, &[], &ms, &mut idx).is_none());
        assert!(place_corner(
            &corner(CornerType::OuterBottomLeft, 0, 0),
            &r,
            &ms,
            &mut idx
        )
        .is_none());
        // A technology that states no horizontal fill fills nothing.
        ms.top_edge.clear();
        ms.bottom_edge.clear();
        let e = Edge {
            kind: EdgeType::Bottom,
            p0: Point::new(0, 0),
            p1: Point::new(80, 0),
        };
        assert!(place_edge_horizontal(&e, &r, &[], &ms, &mut idx).is_empty());
        assert_eq!(idx, 0);
    }

    #[test]
    fn the_corner_site_comes_from_the_first_master_that_states_one() {
        let mut ms = masters();
        assert_eq!(ms.corner_site(), Some("core"));
        ms.left_bottom_corner = None;
        ms.right_bottom_corner = None;
        assert_eq!(ms.corner_site(), Some("core"), "falls through to the next");
        let empty = EndcapMasters::default();
        assert_eq!(empty.corner_site(), None);
        assert_eq!(empty.horizontal_site(), None);
    }
}

/// The row a corner falls in, or `None` when none reaches it.
///
/// This is what makes a boundary have more corners than cells: a corner the rows do not touch
/// gets nothing. Inner corners carry an extra condition — the row has to extend *past* the corner
/// in the direction the notch opens, or the cell would sit outside the material.
pub fn row_at_corner<'a>(corner: &Corner, rows: &'a [Row], site: &str) -> Option<&'a Row> {
    use CornerType::*;
    let (px, py) = (corner.p.x, corner.p.y);
    rows.iter().filter(|r| r.site == site).find(|r| {
        let b = &r.bbox;
        if !(b.x0 <= px && px <= b.x1 && b.y0 <= py && py <= b.y1) {
            return false;
        }
        match corner.kind {
            OuterBottomLeft | OuterBottomRight | OuterTopLeft | OuterTopRight => true,
            InnerBottomLeft | InnerBottomRight => b.y0 < py,
            InnerTopLeft | InnerTopRight => b.y1 > py,
        }
    })
}

/// The rows an edge runs along. A single-point touch does not count — the contact has to have
/// length, or a row merely clipping the end of an edge would be treated as lying on it.
pub fn rows_on_edge<'a>(edge: &Edge, rows: &'a [Row], site: &str) -> Vec<&'a Row> {
    let (ex0, ex1) = edge.x_span();
    let (ey0, ey1) = edge.y_span();
    rows.iter()
        .filter(|r| r.site == site)
        .filter(|r| {
            let dx = r.bbox.x1.min(ex1) - r.bbox.x0.max(ex0);
            let dy = r.bbox.y1.min(ey1) - r.bbox.y0.max(ey0);
            dx >= 0 && dy >= 0 && dx.max(dy) > 0
        })
        .collect()
}

/// **T9** — every boundary cell for one classified region.
///
/// Corners go first so the edge fills can shrink away from them, and each distinct edge is filled
/// once even when two polygons of the region share it.
/// **T15** — start a ring's edge list where upstream starts it.
///
/// 🔑 **The ORDER decides who wins a contested position.** Where a horizontal and a vertical edge
/// both reach a die corner, whichever is placed first claims it and occupancy blocks the other.
/// `abutting_macros_step_no_corners` says so in its own header: *"the horizontal edge fills and the
/// row-end endcaps must not be placed on top of each other"*.
///
/// ⚠️ **The cycle and the classifications already agree; only the starting vertex differs.**
/// Measured on that case, ours began at `(0, 282800)` and upstream at `(0, 0)` — the same four
/// edges rotated by one, so our `Left` claimed the origin where upstream's `Bottom` should.
///
/// ℹ️ Upstream's order is whatever Boost hands it, which is not a stated rule. **Leftmost, then
/// lowest** reproduces it for 45 of the 48 rings measured across the whole suite at pin `945a9f4`
/// — every outer boundary bar two, and every hole bar one.
///
/// ⛔ **MEASURED WRONG, and the reason recorded here until 2026-08-31 was FALSE.** It said all
/// three misses were *"polygons whose boundary touches itself"*. Re-measured against a freshly
/// regenerated oracle at pin `945a9f4`:
///
/// - `single_row_macros_offset`'s ring has **8 vertices, all 8 distinct** — it does not touch
///   itself anywhere. Ours starts at `(11400, 28000)`, which is what leftmost-then-lowest picks;
///   the reference starts at `(49400, 140000)`, which is not leftmost. **Why is still underived** —
///   but "self-touching" is not the reason, because it is not self-touching.
/// - `cut_rows_min_step_top_left` and `endcap_corners` are **not measured by anything**.
///   `tap_edge_order.py` runs only cases that call `tapcell`; those two call `cut_rows` and
///   `place_endcaps`. The "45 of 48 rings" this comment used to quote came from a broader
///   measurement nothing now reproduces — the instrument reports **30 of 32**, over 20 cases.
///
/// 🔑 **The one ring in the corpus that IS self-touching is `region1`'s, and the miss there is not
/// a starting vertex at all** — see [`crate::endcaps`] notes on that case. Upstream walks 10 edges
/// with `(166060, 149600)` visited twice; we walk the 4-edge bounding rectangle and miss the notch
/// entirely. A shape difference, not a rotation.
///
/// ⚠️ The predecessor rule (lowest-then-leftmost) was wrong on more rings than this one, so this is
/// strictly better rather than complete — **do not fit a further rule to the remainder.** Deriving
/// the true start means reproducing `polygon_90_set_data::get_polygons`, which has not been done.
fn rotate_to_lowest_left(edges: &[Edge]) -> Vec<Edge> {
    let Some(start) = (0..edges.len()).min_by_key(|&i| (edges[i].p0.x, edges[i].p0.y)) else {
        return edges.to_vec();
    };
    edges[start..]
        .iter()
        .chain(&edges[..start])
        .cloned()
        .collect()
}

/// **T14** — everything in a row that an EDGE has to avoid: the corners and the earlier edges.
///
/// 🔑 Upstream's `occupiedSpans` is exactly this union — `occupied_row_spans_` plus the bounding
/// boxes in `placed_corners_`. Consulting only the corners lets an edge land on an edge.
fn occupancy_of(
    corners: &std::collections::BTreeMap<String, Vec<PlacedCorner>>,
    edges: &std::collections::BTreeMap<String, Vec<Rect>>,
    row: &str,
) -> Vec<Rect> {
    let mut all: Vec<Rect> = corners
        .get(row)
        .map(|v| v.iter().map(|c| c.rect).collect())
        .unwrap_or_default();
    if let Some(e) = edges.get(row) {
        all.extend(e.iter().copied());
    }
    all
}

/// Note the span an edge cell now occupies, so nothing else is placed on top of it.
fn note_edges(
    edges: &mut std::collections::BTreeMap<String, Vec<Rect>>,
    row: &str,
    placed: &[Placement],
    masters: &EndcapMasters,
) {
    for p in placed {
        if let Some((w, h)) = masters.extent_of(&p.master) {
            edges
                .entry(row.to_string())
                .or_default()
                .push(Rect::new(p.x, p.y, p.x + w, p.y + h));
        }
    }
}

/// **T11** — a corner already placed in this row, and whether we may take it back.
#[derive(Debug, Clone)]
struct PlacedCorner {
    rect: Rect,
    /// Where it sits in the output, so displacement can remove it.
    out_idx: usize,
    /// Which ring placed it. ⚠️ **Only a corner from the SAME ring may be displaced** — upstream's
    /// `area_corners` is per area, and `placed_corners_` outlives it.
    ring: usize,
}

/// Do two rectangles share any area? Touching edges do not count.
fn rects_overlap(a: Rect, b: Rect) -> bool {
    a.x1 > b.x0 && b.x1 > a.x0 && a.y1 > b.y0 && b.y1 > a.y0
}

/// **T12** — is this cell flush with either end of its row?
///
/// 🔑 **This is the whole of the displacement priority.** A corner AT the row end has nowhere else
/// to go, so it wins; one in the middle can be given up. Upstream calls it `isAtRowEnd` and tests
/// only x, because a row is one cell tall.
fn is_at_row_end(cell: Rect, row: Rect) -> bool {
    cell.x0 == row.x0 || cell.x1 == row.x1
}

/// **T13** — resolve a proposed corner against what this row already holds.
///
/// Upstream's rule, verbatim in behaviour: *"a corner at the row end displaces a same-area corner
/// that is not at it; otherwise the new corner is skipped."*
///
/// ⚠️ **`else return` — not `else continue`.** One losing overlap abandons the corner outright;
/// it does not carry on hoping a later overlap is displaceable.
///
/// Returns the indices to displace, or `None` to skip this corner.
fn resolve_corner(
    cell: Rect,
    row_bbox: Rect,
    ring: usize,
    placed: &[PlacedCorner],
) -> Option<Vec<usize>> {
    let cell_at_end = is_at_row_end(cell, row_bbox);
    let mut displaced = Vec::new();
    for other in placed {
        if !rects_overlap(cell, other.rect) {
            continue;
        }
        let other_at_end = is_at_row_end(other.rect, row_bbox);
        if cell_at_end && !other_at_end && other.ring == ring {
            displaced.push(other.out_idx);
        } else {
            return None;
        }
    }
    Some(displaced)
}

pub fn place_all(
    classified: &[crate::boundary::ClassifiedPolygon],
    rows: &[Row],
    masters: &EndcapMasters,
    phy_index: &mut usize,
) -> Vec<Placement> {
    let mut out: Vec<Placement> = Vec::new();
    let mut filled: Vec<(EdgeType, i32, i32, i32, i32)> = Vec::new();
    // 🔑 **What each row already holds, PERSISTED ACROSS POLYGONS AND HOLES**, and split in two
    // because the halves have different powers. Upstream's `placed_corners_` is cleared once at
    // the end of the whole insertion — *"edges and corners of one macro's hole see the corners
    // already placed by an adjacent macro's hole in the same row"*.
    //
    // ⚠️ **Corners can be DISPLACED; edge spans cannot.** `occupied_row_spans_` blocks
    // unconditionally, `placed_corners_` only when the newcomer does not out-rank it.
    let mut placed_corners: std::collections::BTreeMap<String, Vec<PlacedCorner>> =
        Default::default();
    let mut edge_spans: std::collections::BTreeMap<String, Vec<Rect>> = Default::default();
    let mut dead: std::collections::HashSet<usize> = Default::default();
    let mut ring_id = 0usize;

    for poly in classified {
        // Upstream walks each RING in turn -- the outer boundary's corners then its edges, then
        // the same for every hole -- rather than all corners then all edges. Instance names carry
        // a running counter, so that interleaving is observable in the output.
        let rings: Vec<(&[Corner], &[Edge])> =
            std::iter::once((poly.outer_corners.as_slice(), poly.outer_edges.as_slice()))
                .chain(poly.holes.iter().map(|(e, c)| (c.as_slice(), e.as_slice())))
                .collect();

        for (corners, edges) in rings {
            ring_id += 1;
            // Mirrors upstream's `TAP Endcap 2` debug group, so the two edge lists can be diffed
            // directly rather than inferred from the cells they place.
            if std::env::var_os("TAP_EDGE_TRACE").is_some() {
                for e in &rotate_to_lowest_left(edges) {
                    eprintln!(
                        "[edge] ({}, {}) - ({}, {}) : {:?}",
                        e.p0.x, e.p0.y, e.p1.x, e.p1.y, e.kind
                    );
                }
            }
            if let Some(site) = masters.corner_site() {
                for c in corners {
                    let Some(row) = row_at_corner(c, rows, site) else {
                        continue;
                    };
                    if let Some(p) = place_corner(c, row, masters, phy_index) {
                        let Some(m) = masters.for_corner(c.kind, row.orient == "R0") else {
                            out.push(p);
                            continue;
                        };
                        let cell = Rect::new(p.x, p.y, p.x + m.width, p.y + m.height);
                        let existing = placed_corners.entry(row.name.clone()).or_default();
                        let Some(displaced) = resolve_corner(cell, row.bbox, ring_id, existing)
                        else {
                            continue; // an overlap this corner does not out-rank
                        };
                        // ⚠️ **An EDGE cell blocks outright** — it is not ours to take back.
                        if edge_spans
                            .get(&row.name)
                            .is_some_and(|v| v.iter().any(|r| rects_overlap(cell, *r)))
                        {
                            continue;
                        }
                        for idx in &displaced {
                            dead.insert(*idx);
                        }
                        let existing = placed_corners.entry(row.name.clone()).or_default();
                        existing.retain(|c| !displaced.contains(&c.out_idx));
                        existing.push(PlacedCorner {
                            rect: cell,
                            out_idx: out.len(),
                            ring: ring_id,
                        });
                        out.push(p);
                    }
                }
            }

            let ordered = rotate_to_lowest_left(edges);
            for e in &ordered {
                // 🔑 **The key is DIRECTION-INSENSITIVE, because upstream's `Edge::operator==` is:**
                // `type == other.type && ((pt0, pt1) == (o.pt0, o.pt1) || (pt0, pt1) == (o.pt1, o.pt0))`.
                // The same segment reached from two rings arrives reversed — an island's outer
                // boundary and the enclosing hole's inward excursion are the same metal — and the
                // hole flip is built so both give it the SAME type. Verified on `region1`: all three
                // shared segments classify Bottom/Right/Left from either side. An ordered key fills
                // them twice.
                let (a, b) = ((e.p0.x, e.p0.y), (e.p1.x, e.p1.y));
                let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                let key = (e.kind, lo.0, lo.1, hi.0, hi.1);
                if filled.contains(&key) {
                    continue;
                }
                filled.push(key);
                match e.kind {
                    EdgeType::Top | EdgeType::Bottom => {
                        let Some(site) = masters.horizontal_site() else {
                            continue;
                        };
                        // A horizontal segment lies along a single row by definition.
                        let Some(row) = rows_on_edge(e, rows, site).into_iter().next() else {
                            continue;
                        };
                        let cs = occupancy_of(&placed_corners, &edge_spans, &row.name);
                        let placed = place_edge_horizontal(e, row, &cs, masters, phy_index);
                        note_edges(&mut edge_spans, &row.name, &placed, masters);
                        out.extend(placed);
                    }
                    EdgeType::Left | EdgeType::Right => {
                        let master = match e.kind {
                            EdgeType::Left => masters.left_edge.as_ref(),
                            _ => masters.right_edge.as_ref(),
                        };
                        let Some(master) = master else {
                            continue;
                        };
                        for row in rows_on_edge(e, rows, &master.site) {
                            let cs = occupancy_of(&placed_corners, &edge_spans, &row.name);
                            if let Some(p) =
                                place_edge_vertical(e.kind, row, &cs, masters, phy_index)
                            {
                                note_edges(
                                    &mut edge_spans,
                                    &row.name,
                                    std::slice::from_ref(&p),
                                    masters,
                                );
                                out.push(p);
                            }
                        }
                    }
                }
            }
        }
    }
    // ⚠️ **Displaced corners are removed HERE, not in place** — removing mid-run would shift
    // every later index and silently displace the wrong cell.
    out.into_iter()
        .enumerate()
        .filter(|(i, _)| !dead.contains(i))
        .map(|(_, p)| p)
        .collect()
}
#[cfg(test)]
mod orchestration_tests {
    use super::*;
    use crate::boundary::{self, Rect as BRect};
    use vyges_loom::poly90::Point;

    fn m(name: &str, w: i32, h: i32) -> Master {
        Master {
            name: name.into(),
            site: "core".into(),
            width: w,
            height: h,
            symmetry_x: true,
            symmetry_y: true,
        }
    }

    fn rows(n: i32, x0: i32, x1: i32) -> Vec<Row> {
        (0..n)
            .map(|i| Row {
                name: format!("ROW_{i}"),
                site: "core".into(),
                orient: if i % 2 == 0 { "R0".into() } else { "MX".into() },
                bbox: Rect::new(x0, i * 10, x1, i * 10 + 10),
                site_width: 10,
            })
            .collect()
    }

    fn ms() -> EndcapMasters {
        EndcapMasters {
            left_bottom_corner: Some(m("LBC", 10, 10)),
            right_bottom_corner: Some(m("RBC", 10, 10)),
            left_top_corner: Some(m("LTC", 10, 10)),
            right_top_corner: Some(m("RTC", 10, 10)),
            left_bottom_edge: Some(m("LBE", 10, 10)),
            right_bottom_edge: Some(m("RBE", 10, 10)),
            left_top_edge: Some(m("LTE", 10, 10)),
            right_top_edge: Some(m("RTE", 10, 10)),
            top_edge: vec![m("TE", 10, 10)],
            bottom_edge: vec![m("BE", 10, 10)],
            left_edge: Some(m("LE", 10, 10)),
            right_edge: Some(m("RE", 10, 10)),
            prefix: "PHY_".into(),
        }
    }

    fn classify(rows: &[Row], core: BRect) -> Vec<boundary::ClassifiedPolygon> {
        let region = boundary::row_region(core, &rows.iter().map(|r| r.bbox).collect::<Vec<_>>());
        boundary::classify(&region)
    }

    #[test]
    fn a_plain_block_is_ringed_without_anything_overlapping() {
        let rs = rows(4, 0, 100);
        let c = classify(&rs, BRect::new(0, 0, 100, 40));
        let mut idx = 0;
        let out = place_all(&c, &rs, &ms(), &mut idx);

        assert!(!out.is_empty());
        assert_eq!(
            idx,
            out.len(),
            "every placement advanced the counter exactly once"
        );
        // Names are unique -- two cells sharing a name is a database error waiting to happen.
        let names: std::collections::BTreeSet<&str> = out.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names.len(), out.len(), "duplicate instance names");

        // Nothing lands outside the block.
        for p in &out {
            assert!(p.x >= 0 && p.x < 100, "{p:?} is outside the core");
            assert!(p.y >= 0 && p.y < 40, "{p:?} is outside the core");
        }
        // The four outer corners are all capped.
        let corners: Vec<&Placement> = out.iter().filter(|p| p.name.contains("CORNER")).collect();
        assert_eq!(corners.len(), 4, "one cell per outer corner: {corners:?}");
    }

    #[test]
    fn cells_on_one_row_do_not_overlap_each_other() {
        // The check that matters most: corners are placed first and the fills must avoid them.
        let rs = rows(4, 0, 100);
        let c = classify(&rs, BRect::new(0, 0, 100, 40));
        let mut idx = 0;
        let out = place_all(&c, &rs, &ms(), &mut idx);

        let mut by_y: std::collections::BTreeMap<i32, Vec<(i32, i32)>> = Default::default();
        for p in &out {
            by_y.entry(p.y).or_default().push((p.x, p.x + 10));
        }
        for (y, mut spans) in by_y {
            spans.sort();
            for w in spans.windows(2) {
                assert!(
                    w[0].1 <= w[1].0,
                    "cells overlap at y={y}: {:?} and {:?}",
                    w[0],
                    w[1]
                );
            }
        }
    }

    #[test]
    fn a_macro_hole_gets_its_own_boundary_cells() {
        // Rows cut around a macro leave a hole, and its inside needs capping too.
        let mut rs = Vec::new();
        for i in 0..10 {
            let (y0, y1) = (i * 10, i * 10 + 10);
            if (40..60).contains(&y0) {
                rs.push(Row {
                    name: format!("ROW_{i}a"),
                    site: "core".into(),
                    orient: "R0".into(),
                    bbox: Rect::new(0, y0, 40, y1),
                    site_width: 10,
                });
                rs.push(Row {
                    name: format!("ROW_{i}b"),
                    site: "core".into(),
                    orient: "R0".into(),
                    bbox: Rect::new(60, y0, 100, y1),
                    site_width: 10,
                });
            } else {
                rs.push(Row {
                    name: format!("ROW_{i}"),
                    site: "core".into(),
                    orient: "R0".into(),
                    bbox: Rect::new(0, y0, 100, y1),
                    site_width: 10,
                });
            }
        }
        let c = classify(&rs, BRect::new(0, 0, 100, 100));
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].holes.len(), 1, "the macro is a hole");

        let mut idx = 0;
        let out = place_all(&c, &rs, &ms(), &mut idx);
        // Inner corner cells use EDGE masters, so their presence proves the hole was walked.
        let inner: Vec<&Placement> = out
            .iter()
            .filter(|p| p.name.contains("CORNER") && p.name.contains("Inner"))
            .collect();
        assert!(
            !inner.is_empty(),
            "the hole's corners were never capped: {out:?}"
        );
    }

    #[test]
    fn a_segment_reached_from_two_rings_is_filled_once_either_way_round() {
        // Upstream's `Edge::operator==` matches the same segment traversed in EITHER direction,
        // provided the type agrees. Two rings can present one piece of metal reversed — an island's
        // outer boundary and the enclosing hole's inward excursion around it — and the hole flip is
        // built so both give it the same type. An ordered key fills such a segment twice.
        //
        // Measured on region1 at pin 945a9f4: the three segments the island and the hole share
        // classify Bottom, Right and Left from either side.
        let e = |kind, a: (i32, i32), b: (i32, i32)| Edge {
            kind,
            p0: Point { x: a.0, y: a.1 },
            p1: Point { x: b.0, y: b.1 },
        };
        let fwd = e(EdgeType::Bottom, (40020, 40800), (149960, 40800));
        let rev = e(EdgeType::Bottom, (149960, 40800), (40020, 40800));

        let key = |x: &Edge| {
            let (a, b) = ((x.p0.x, x.p0.y), (x.p1.x, x.p1.y));
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            (x.kind, lo.0, lo.1, hi.0, hi.1)
        };
        assert_eq!(key(&fwd), key(&rev), "the same metal, reached from two rings");

        // ...but a DIFFERENT type on the same segment is a different edge, as upstream requires.
        let other_type = e(EdgeType::Top, (149960, 40800), (40020, 40800));
        assert_ne!(key(&fwd), key(&other_type), "type is part of the identity");
    }

    #[test]
    fn a_shared_edge_is_filled_once() {
        // Two polygons of one region can present the same segment twice; filling it twice would
        // stack cells on top of each other.
        let rs = rows(2, 0, 100);
        let c = classify(&rs, BRect::new(0, 0, 100, 20));
        let mut idx = 0;
        let out = place_all(&c, &rs, &ms(), &mut idx);
        let mut seen: std::collections::BTreeSet<(i32, i32)> = Default::default();
        for p in &out {
            assert!(seen.insert((p.x, p.y)), "two cells at ({}, {})", p.x, p.y);
        }
    }

    #[test]
    fn a_corner_no_row_reaches_gets_nothing() {
        let rs = rows(2, 0, 100);
        // A corner far away from any row.
        let orphan = Corner {
            kind: CornerType::OuterBottomLeft,
            p: Point::new(5000, 5000),
        };
        assert!(row_at_corner(&orphan, &rs, "core").is_none());
        // ...and one that a row does reach is found.
        let real = Corner {
            kind: CornerType::OuterBottomLeft,
            p: Point::new(0, 0),
        };
        assert_eq!(
            row_at_corner(&real, &rs, "core").map(|r| r.name.as_str()),
            Some("ROW_0")
        );
        // A site no row uses finds nothing either.
        assert!(row_at_corner(&real, &rs, "other_site").is_none());
    }
}

/// The LEF58 master type that fills each unnamed position.
///
/// A library states what each cell is *for* — a LEF58 `CLASS ENDCAP LEFTBOTTOMCORNER` property,
/// which odb reports as the space-separated string `"ENDCAP LEFTBOTTOMCORNER"` (NOT the enum
/// spelling `ENDCAP_LEF58_…`, which is what it looks like in the source) — so a
/// caller who names nothing can still be served, provided the library is typed. This is the
/// mapping upstream's `correctEndcapOptions` uses.
/// Which command is auto-selecting — upstream carries this as `EndcapCellOptions::tapcell_cmd`.
///
/// 🔑 **It exists only to name the right option in the ambiguity error, and that is not cosmetic.**
/// `tapcell` and `place_endcaps` reach the same auto-selection through different flags, so the
/// message has to name the flag of the command the user actually ran. Upstream threads a bool
/// through the whole options struct for exactly this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    /// `place-endcaps`, whose options are the positions themselves.
    PlaceEndcaps,
    /// `tapcell`, whose options are the well-tie names.
    Tapcell,
}

/// The option a caller would use to disambiguate `position`.
///
/// Upstream's mapping, from the ternaries in `correctEndcapOptions`: under `tapcell` the two side
/// edges are `-endcap_master`, the corners are `-cnrcap_nw{out,in}_master` (out on top, in on
/// bottom) and the inner edges are `-incnrcap_nw{in,out}_master` (in on top, out on bottom) — the
/// same crossing `to_positions` applies. Under `place_endcaps` the position IS the option, and the
/// side edges additionally accept the general `--endcap`.
pub fn option_for(position: &str, caller: Caller) -> String {
    if caller == Caller::Tapcell {
        return match position {
            "left_edge" | "right_edge" => "--endcap-master",
            "right_top_corner" | "left_top_corner" => "--cnrcap-nwout-master",
            "right_bottom_corner" | "left_bottom_corner" => "--cnrcap-nwin-master",
            "right_top_edge" | "left_top_edge" => "--incnrcap-nwin-master",
            "right_bottom_edge" | "left_bottom_edge" => "--incnrcap-nwout-master",
            p => return format!("--{}", p.replace('_', "-")),
        }
        .to_string();
    }
    match position {
        "left_edge" => "--left-edge / --endcap".to_string(),
        "right_edge" => "--right-edge / --endcap".to_string(),
        p => format!("--{}", p.replace('_', "-")),
    }
}

pub const TYPE_FOR_POSITION: [(&str, &str); 12] = [
    ("left_edge", "ENDCAP LEFTEDGE"),
    ("right_edge", "ENDCAP RIGHTEDGE"),
    ("top_edge", "ENDCAP TOPEDGE"),
    ("bottom_edge", "ENDCAP BOTTOMEDGE"),
    ("right_top_corner", "ENDCAP RIGHTTOPCORNER"),
    ("left_top_corner", "ENDCAP LEFTTOPCORNER"),
    ("right_bottom_corner", "ENDCAP RIGHTBOTTOMCORNER"),
    ("left_bottom_corner", "ENDCAP LEFTBOTTOMCORNER"),
    ("right_top_edge", "ENDCAP RIGHTTOPEDGE"),
    ("left_top_edge", "ENDCAP LEFTTOPEDGE"),
    ("right_bottom_edge", "ENDCAP RIGHTBOTTOMEDGE"),
    ("left_bottom_edge", "ENDCAP LEFTBOTTOMEDGE"),
];

/// Why autoselection could not decide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoselectError {
    /// More than one master claims a position, so the choice is the caller's to make
    /// (upstream TAP-104).
    Ambiguous {
        position: String,
        /// The option THIS caller would use — see [`option_for`].
        option: String,
        ty: String,
        masters: Vec<String>,
    },
}

impl std::fmt::Display for AutoselectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AutoselectError::Ambiguous {
                option,
                ty,
                masters,
                ..
            } => write!(
                f,
                "found multiple masters of type {ty}; name one with {option} : {}",
                masters.join(" ")
            ),
        }
    }
}

/// Every master of one type, in library order.
fn of_type<'a>(library: &'a [(String, String)], ty: &str) -> Vec<&'a str> {
    library
        .iter()
        .filter(|(_, t)| t == ty)
        .map(|(n, _)| n.as_str())
        .collect()
}

/// **T9** — fill unnamed positions from the library's own master types.
///
/// Three passes, and the order matters:
///
/// 1. each empty position takes the single master of its type — **two candidates is an error**,
///    not a coin flip, because a wrong endcap is a well-tie fault nobody sees until silicon;
/// 2. corners still empty borrow from their own side (top borrows from bottom, and if a whole
///    side is missing, from the other side);
/// 3. left/right and top/bottom mirror each other if one is still empty.
///
/// A position with nothing to fill it stays empty, and placement then puts nothing there.
pub fn autoselect(
    masters: &mut EndcapMasters,
    library: &[(String, String)],
    caller: Caller,
    lookup: impl Fn(&str) -> Option<Master>,
) -> Result<(), AutoselectError> {
    // Pass 1: by type.
    for (position, ty) in TYPE_FOR_POSITION {
        let occupied = match position {
            "top_edge" => !masters.top_edge.is_empty(),
            "bottom_edge" => !masters.bottom_edge.is_empty(),
            p => slot(masters, p).is_some(),
        };
        if occupied {
            continue;
        }
        let found = of_type(library, ty);
        match position {
            // The horizontal fill takes ALL masters of its type -- it picks between them per
            // span, so more than one is a richer choice rather than an ambiguity.
            "top_edge" => masters.top_edge = found.iter().filter_map(|n| lookup(n)).collect(),
            "bottom_edge" => masters.bottom_edge = found.iter().filter_map(|n| lookup(n)).collect(),
            p => {
                if found.len() > 1 {
                    return Err(AutoselectError::Ambiguous {
                        position: p.to_string(),
                        option: option_for(p, caller),
                        ty: ty.to_string(),
                        masters: found.iter().map(|s| s.to_string()).collect(),
                    });
                }
                if let Some(n) = found.first() {
                    set_slot(masters, p, lookup(n));
                }
            }
        }
    }

    // Pass 2: corners borrow along their own side, then across.
    borrow_corners(
        masters,
        [
            "right_top_corner",
            "left_top_corner",
            "right_bottom_corner",
            "left_bottom_corner",
        ],
    );
    borrow_corners(
        masters,
        [
            "right_top_edge",
            "left_top_edge",
            "right_bottom_edge",
            "left_bottom_edge",
        ],
    );

    // Pass 3: the two sides, and the two horizontals, stand in for each other.
    if masters.right_edge.is_none() {
        masters.right_edge = masters.left_edge.clone();
    }
    if masters.left_edge.is_none() {
        masters.left_edge = masters.right_edge.clone();
    }
    if masters.top_edge.is_empty() {
        masters.top_edge = masters.bottom_edge.clone();
    }
    if masters.bottom_edge.is_empty() {
        masters.bottom_edge = masters.top_edge.clone();
    }
    Ok(())
}

/// `[upper_right, upper_left, lower_right, lower_left]` — each empty one takes the master from
/// its own side (upper borrows from lower), falling back to the other side.
fn borrow_corners(m: &mut EndcapMasters, quad: [&str; 4]) {
    let [ur, ul, lr, ll] = quad;
    let pref_l = slot(m, ul).or_else(|| slot(m, ll));
    let pref_r = slot(m, ur).or_else(|| slot(m, lr));
    let pref_r = pref_r.or_else(|| pref_l.clone());
    let pref_l = pref_l.or_else(|| pref_r.clone());

    for (name, pref) in [(ur, &pref_r), (ul, &pref_l), (lr, &pref_r), (ll, &pref_l)] {
        if slot(m, name).is_none() {
            set_slot(m, name, pref.clone());
        }
    }
}

fn slot(m: &EndcapMasters, name: &str) -> Option<Master> {
    match name {
        "left_top_corner" => m.left_top_corner.clone(),
        "right_top_corner" => m.right_top_corner.clone(),
        "left_bottom_corner" => m.left_bottom_corner.clone(),
        "right_bottom_corner" => m.right_bottom_corner.clone(),
        "left_top_edge" => m.left_top_edge.clone(),
        "right_top_edge" => m.right_top_edge.clone(),
        "left_bottom_edge" => m.left_bottom_edge.clone(),
        "right_bottom_edge" => m.right_bottom_edge.clone(),
        "left_edge" => m.left_edge.clone(),
        "right_edge" => m.right_edge.clone(),
        _ => None,
    }
}

fn set_slot(m: &mut EndcapMasters, name: &str, v: Option<Master>) {
    match name {
        "left_top_corner" => m.left_top_corner = v,
        "right_top_corner" => m.right_top_corner = v,
        "left_bottom_corner" => m.left_bottom_corner = v,
        "right_bottom_corner" => m.right_bottom_corner = v,
        "left_top_edge" => m.left_top_edge = v,
        "right_top_edge" => m.right_top_edge = v,
        "left_bottom_edge" => m.left_bottom_edge = v,
        "right_bottom_edge" => m.right_bottom_edge = v,
        "left_edge" => m.left_edge = v,
        "right_edge" => m.right_edge = v,
        _ => {}
    }
}

#[cfg(test)]
mod autoselect_tests {
    use super::*;

    fn lib() -> Vec<(String, String)> {
        [
            ("LE", "ENDCAP LEFTEDGE"),
            ("RE", "ENDCAP RIGHTEDGE"),
            ("TE", "ENDCAP TOPEDGE"),
            ("BE", "ENDCAP BOTTOMEDGE"),
            ("RTC", "ENDCAP RIGHTTOPCORNER"),
            ("LTC", "ENDCAP LEFTTOPCORNER"),
            ("RBC", "ENDCAP RIGHTBOTTOMCORNER"),
            ("LBC", "ENDCAP LEFTBOTTOMCORNER"),
            ("ordinary", "CORE"),
        ]
        .into_iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
    }

    fn look(name: &str) -> Option<Master> {
        Some(Master {
            name: name.to_string(),
            site: "core".into(),
            width: 10,
            height: 10,
            symmetry_x: true,
            symmetry_y: true,
        })
    }

    #[test]
    fn a_typed_library_fills_every_position_from_nothing() {
        // The case the whole function exists for: the caller names no masters at all.
        let mut m = EndcapMasters::default();
        autoselect(&mut m, &lib(), Caller::PlaceEndcaps, look).expect("unambiguous");
        assert_eq!(m.left_bottom_corner.map(|x| x.name).as_deref(), Some("LBC"));
        assert_eq!(m.right_top_corner.map(|x| x.name).as_deref(), Some("RTC"));
        assert_eq!(m.left_edge.map(|x| x.name).as_deref(), Some("LE"));
        assert_eq!(m.right_edge.map(|x| x.name).as_deref(), Some("RE"));
        assert_eq!(
            m.top_edge
                .iter()
                .map(|x| x.name.as_str())
                .collect::<Vec<_>>(),
            vec!["TE"]
        );
        assert_eq!(
            m.bottom_edge
                .iter()
                .map(|x| x.name.as_str())
                .collect::<Vec<_>>(),
            vec!["BE"]
        );
        // A CORE cell is not an endcap and must not be dragged in.
        assert!(![&m.left_top_corner, &m.right_bottom_corner]
            .iter()
            .any(|s| s.as_ref().is_some_and(|x| x.name == "ordinary")));
    }

    #[test]
    fn what_the_caller_named_is_never_overwritten() {
        let mut m = EndcapMasters {
            left_bottom_corner: look("MINE"),
            top_edge: vec![look("MINE_TOP").unwrap()],
            ..Default::default()
        };
        autoselect(&mut m, &lib(), Caller::PlaceEndcaps, look).expect("unambiguous");
        assert_eq!(
            m.left_bottom_corner.map(|x| x.name).as_deref(),
            Some("MINE")
        );
        assert_eq!(m.top_edge[0].name, "MINE_TOP");
        // ...while everything unnamed is still filled in.
        assert_eq!(m.right_top_corner.map(|x| x.name).as_deref(), Some("RTC"));
    }

    #[test]
    fn two_candidates_for_one_position_is_an_error_not_a_coin_flip() {
        // A wrong endcap is a well-tie fault nobody sees until silicon, so the caller decides.
        let mut library = lib();
        library.push(("LBC2".into(), "ENDCAP LEFTBOTTOMCORNER".into()));
        let mut m = EndcapMasters::default();
        let err = autoselect(&mut m, &library, Caller::PlaceEndcaps, look).expect_err("ambiguous");
        let AutoselectError::Ambiguous {
            position, masters, ..
        } = &err;
        assert_eq!(position, "left_bottom_corner");
        assert_eq!(masters, &vec!["LBC".to_string(), "LBC2".to_string()]);
        assert!(
            err.to_string().contains("--left-bottom-corner"),
            "names the option: {err}"
        );
    }

    #[test]
    fn the_ambiguity_names_the_option_of_the_command_that_was_run() {
        // Upstream rule: `correctEndcapOptions` picks the option name in TAP-104 from
        // `options.tapcell_cmd`, so the same ambiguity reads `-cnrcap_nwin_master` under `tapcell`
        // and `-left_bottom_corner` under `place_endcaps`. Naming the wrong one is not cosmetic:
        // `--left-bottom-corner` is not an option of `tapcell`, so the advice cannot be followed.
        let mut library = lib();
        library.push(("LBC2".into(), "ENDCAP LEFTBOTTOMCORNER".into()));

        let mut m = EndcapMasters::default();
        let e = autoselect(&mut m, &library, Caller::PlaceEndcaps, look).expect_err("ambiguous");
        assert!(e.to_string().contains("--left-bottom-corner"), "{e}");

        let mut m = EndcapMasters::default();
        let e = autoselect(&mut m, &library, Caller::Tapcell, look).expect_err("ambiguous");
        assert!(e.to_string().contains("--cnrcap-nwin-master"), "{e}");
        assert!(
            !e.to_string().contains("--left-bottom-corner"),
            "tapcell has no such option: {e}"
        );
    }

    #[test]
    fn the_tapcell_option_names_keep_the_corner_crossing() {
        // The same crossing `to_positions` applies: nwout on top, nwin on bottom for corners, and
        // the opposite for inner edges. Getting it backwards here sends the user to the flag that
        // fills the OTHER half of the design.
        use super::{option_for, Caller::Tapcell as T};
        assert_eq!(option_for("left_top_corner", T), "--cnrcap-nwout-master");
        assert_eq!(option_for("left_bottom_corner", T), "--cnrcap-nwin-master");
        assert_eq!(option_for("left_top_edge", T), "--incnrcap-nwin-master");
        assert_eq!(option_for("left_bottom_edge", T), "--incnrcap-nwout-master");
        assert_eq!(option_for("left_edge", T), "--endcap-master");
    }

    #[test]
    fn several_horizontal_masters_are_a_richer_choice_rather_than_an_ambiguity() {
        // The fill picks between widths per span, so more than one is useful, not confusing.
        let mut library = lib();
        library.push(("TE2".into(), "ENDCAP TOPEDGE".into()));
        let mut m = EndcapMasters::default();
        autoselect(&mut m, &library, Caller::PlaceEndcaps, look).expect("not ambiguous");
        assert_eq!(m.top_edge.len(), 2);
    }

    #[test]
    fn a_missing_corner_borrows_from_its_own_side_before_the_other() {
        // Only the bottom corners are typed: the top ones take them, rather than being left empty.
        let library: Vec<(String, String)> = [
            ("RBC", "ENDCAP RIGHTBOTTOMCORNER"),
            ("LBC", "ENDCAP LEFTBOTTOMCORNER"),
        ]
        .into_iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        let mut m = EndcapMasters::default();
        autoselect(&mut m, &library, Caller::PlaceEndcaps, look).expect("unambiguous");
        assert_eq!(
            m.left_top_corner.map(|x| x.name).as_deref(),
            Some("LBC"),
            "left borrows left"
        );
        assert_eq!(
            m.right_top_corner.map(|x| x.name).as_deref(),
            Some("RBC"),
            "right borrows right"
        );
    }

    #[test]
    fn one_side_only_is_mirrored_to_the_other() {
        let library: Vec<(String, String)> = [
            ("LBC", "ENDCAP LEFTBOTTOMCORNER"),
            ("LE", "ENDCAP LEFTEDGE"),
            ("BE", "ENDCAP BOTTOMEDGE"),
        ]
        .into_iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        let mut m = EndcapMasters::default();
        autoselect(&mut m, &library, Caller::PlaceEndcaps, look).expect("unambiguous");
        assert_eq!(
            m.right_bottom_corner.map(|x| x.name).as_deref(),
            Some("LBC"),
            "across sides"
        );
        assert_eq!(
            m.right_edge.map(|x| x.name).as_deref(),
            Some("LE"),
            "right takes left"
        );
        assert_eq!(
            m.top_edge
                .iter()
                .map(|x| x.name.as_str())
                .collect::<Vec<_>>(),
            vec!["BE"]
        );
    }

    #[test]
    fn an_untyped_library_fills_nothing_rather_than_guessing() {
        // LEF 5.7 libraries say only "ENDCAP", which does not say WHICH endcap.
        let library: Vec<(String, String)> = [("E1", "ENDCAP"), ("C1", "CORE")]
            .into_iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        let mut m = EndcapMasters::default();
        autoselect(&mut m, &library, Caller::PlaceEndcaps, look).expect("no ambiguity, just nothing to find");
        assert_eq!(
            m,
            EndcapMasters::default(),
            "nothing invented from an untyped library"
        );
    }
}

/// The `tapcell` command's flat options, which name cells by their electrical role rather than by
/// position on the boundary.
///
/// `nwin`/`nwout` are "inside the n-well" and "outside it"; `tie` cells make the well contact.
/// The combined command speaks this vocabulary and `place_endcaps` speaks positions, so something
/// has to translate — this does.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TapcellMasters {
    pub endcap_master: Option<Master>,
    pub tap_nwin2_master: Option<Master>,
    pub tap_nwin3_master: Option<Master>,
    pub tap_nwintie_master: Option<Master>,
    pub tap_nwout2_master: Option<Master>,
    pub tap_nwout3_master: Option<Master>,
    pub tap_nwouttie_master: Option<Master>,
    pub cnrcap_nwin_master: Option<Master>,
    pub cnrcap_nwout_master: Option<Master>,
    pub incnrcap_nwin_master: Option<Master>,
    pub incnrcap_nwout_master: Option<Master>,
    pub endcap_prefix: String,
}

impl TapcellMasters {
    /// **T10** — translate the role-based options into boundary positions.
    ///
    /// Three things worth noting, all upstream's:
    ///
    /// - the single `endcap_master` caps **both** the left and right edges;
    /// - the horizontal fill lists are ordered **3, 2, tie** — widest-first by convention, which
    ///   matters because the fill prefers the first master that divides a span exactly;
    /// - the well-inside cells go on the **bottom** and the well-outside cells on the **top**,
    ///   and the corner/inner-corner pairs swap that sense (`cnrcap_nwout` is the *top* corner,
    ///   `incnrcap_nwin` the *top* inner edge). Getting either backwards puts a well tie on the
    ///   wrong side of the well boundary.
    pub fn to_positions(&self) -> EndcapMasters {
        let list = |a: &Option<Master>, b: &Option<Master>, c: &Option<Master>| -> Vec<Master> {
            [a, b, c].into_iter().flatten().cloned().collect()
        };
        EndcapMasters {
            left_edge: self.endcap_master.clone(),
            right_edge: self.endcap_master.clone(),

            bottom_edge: list(
                &self.tap_nwin3_master,
                &self.tap_nwin2_master,
                &self.tap_nwintie_master,
            ),
            top_edge: list(
                &self.tap_nwout3_master,
                &self.tap_nwout2_master,
                &self.tap_nwouttie_master,
            ),

            left_top_corner: self.cnrcap_nwout_master.clone(),
            right_top_corner: self.cnrcap_nwout_master.clone(),
            left_bottom_corner: self.cnrcap_nwin_master.clone(),
            right_bottom_corner: self.cnrcap_nwin_master.clone(),

            left_top_edge: self.incnrcap_nwin_master.clone(),
            right_top_edge: self.incnrcap_nwin_master.clone(),
            left_bottom_edge: self.incnrcap_nwout_master.clone(),
            right_bottom_edge: self.incnrcap_nwout_master.clone(),

            prefix: self.endcap_prefix.clone(),
        }
    }
}

#[cfg(test)]
mod tapcell_option_tests {
    use super::*;

    fn m(n: &str) -> Option<Master> {
        Some(Master {
            name: n.into(),
            site: "core".into(),
            width: 10,
            height: 10,
            symmetry_x: true,
            symmetry_y: true,
        })
    }

    fn full() -> TapcellMasters {
        TapcellMasters {
            endcap_master: m("EC"),
            tap_nwin2_master: m("IN2"),
            tap_nwin3_master: m("IN3"),
            tap_nwintie_master: m("INTIE"),
            tap_nwout2_master: m("OUT2"),
            tap_nwout3_master: m("OUT3"),
            tap_nwouttie_master: m("OUTTIE"),
            cnrcap_nwin_master: m("CNRIN"),
            cnrcap_nwout_master: m("CNROUT"),
            incnrcap_nwin_master: m("INCNRIN"),
            incnrcap_nwout_master: m("INCNROUT"),
            endcap_prefix: "PHY_".into(),
        }
    }

    #[test]
    fn one_endcap_master_caps_both_sides() {
        let p = full().to_positions();
        assert_eq!(p.left_edge.map(|x| x.name).as_deref(), Some("EC"));
        assert_eq!(p.right_edge.map(|x| x.name).as_deref(), Some("EC"));
    }

    #[test]
    fn the_horizontal_lists_keep_their_three_two_tie_order() {
        // The fill takes the first master that divides the span exactly, so the order is not
        // cosmetic.
        let p = full().to_positions();
        assert_eq!(
            p.bottom_edge
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            vec!["IN3", "IN2", "INTIE"]
        );
        assert_eq!(
            p.top_edge
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            vec!["OUT3", "OUT2", "OUTTIE"]
        );
    }

    #[test]
    fn well_inside_goes_to_the_bottom_and_the_corner_pairs_swap_that_sense() {
        // The detail that puts a well tie on the wrong side of the well if inverted.
        let p = full().to_positions();
        assert_eq!(p.bottom_edge[0].name, "IN3", "nwin cells fill the BOTTOM");
        assert_eq!(p.top_edge[0].name, "OUT3", "nwout cells fill the TOP");
        assert_eq!(
            p.left_bottom_corner.map(|x| x.name).as_deref(),
            Some("CNRIN")
        );
        assert_eq!(p.left_top_corner.map(|x| x.name).as_deref(), Some("CNROUT"));
        // ...and the INNER corners invert it again.
        assert_eq!(p.left_top_edge.map(|x| x.name).as_deref(), Some("INCNRIN"));
        assert_eq!(
            p.left_bottom_edge.map(|x| x.name).as_deref(),
            Some("INCNROUT")
        );
    }

    #[test]
    fn masters_that_were_not_given_are_simply_absent() {
        let sparse = TapcellMasters {
            endcap_master: m("EC"),
            endcap_prefix: "PHY_".into(),
            ..Default::default()
        };
        let p = sparse.to_positions();
        assert!(p.bottom_edge.is_empty() && p.top_edge.is_empty());
        assert!(p.left_bottom_corner.is_none());
        assert_eq!(p.left_edge.map(|x| x.name).as_deref(), Some("EC"));
        // ...leaving autoselect to fill what the library can.
    }

    #[test]
    fn a_partial_horizontal_list_keeps_only_what_was_given() {
        let mut t = full();
        t.tap_nwin2_master = None;
        let p = t.to_positions();
        assert_eq!(
            p.bottom_edge
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            vec!["IN3", "INTIE"],
            "the gap closes up rather than leaving a hole in the list"
        );
    }
}
