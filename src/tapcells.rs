// SPDX-License-Identifier: Apache-2.0
//! Where the well taps go — rules **T5** and **T6**.
//!
//! Taps are placed along each row at a fixed pitch, skipping past anything already sitting there.
//! Two details carry most of the behaviour and neither is obvious:
//!
//! - **rows on the top or bottom boundary get twice the density.** The pitch multiplier is 1 on an
//!   edge row and 2 elsewhere, so a boundary row gets a tap every `dist` rather than every
//!   `2 × dist`. Well ties matter most where the well ends.
//! - **a blocked candidate slides, unless it is swallowed.** A tap that pokes into a fixed
//!   instance from one side retreats to that side rather than being abandoned, and the scan
//!   resumes from where it actually landed. But a candidate the obstacle *contains* has no side
//!   to slide along, and is simply skipped — the scan carries on, so one macro never silences a
//!   whole row.
//!
//! Nothing here touches a database: rows, masters and obstacles arrive as values.

use vyges_loom::poly90::Rect;

use crate::boundary::{Edge, EdgeType};

/// A row, reduced to what tap placement needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    pub site: String,
    /// An odb orientation spelling (`R0`, `MX`, …).
    pub orient: String,
    pub bbox: Rect,
    pub site_width: i32,
}

/// The tapcell master's relevant properties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Master {
    pub name: String,
    pub site: String,
    pub width: i32,
    /// Needed by corner placement, which pulls a cell down as well as sideways.
    pub height: i32,
    pub symmetry_x: bool,
    pub symmetry_y: bool,
}

/// One tap to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub name: String,
    pub master: String,
    pub x: i32,
    pub y: i32,
    pub orient: String,
}

/// Snap a coordinate to the site grid — odb's `makeSiteLoc`.
///
/// Implemented rather than called across the bridge because it is three lines of arithmetic over
/// values, not an operation on odb data, and it runs once per candidate position in an inner loop.
pub fn make_site_loc(x: i32, site_width: i32, at_left: bool, offset: i32) -> i32 {
    if site_width <= 0 {
        return x;
    }
    let site_x = (x - offset) as f64 / site_width as f64;
    let n = if at_left {
        site_x.floor()
    } else {
        site_x.ceil()
    };
    (n * site_width as f64) as i32 + offset
}

/// **T5** — may this master be placed in a row of this orientation?
///
/// A cell is only legal upside-down if the library says it is symmetric that way. `R0` always
/// works; anything other than the four axis orientations is refused rather than assumed.
pub fn check_symmetry(symmetry_x: bool, symmetry_y: bool, orient: &str) -> bool {
    match orient {
        "R0" => true,
        "MX" => symmetry_x,
        "MY" => symmetry_y,
        "R180" => symmetry_x && symmetry_y,
        _ => false,
    }
}

/// The x-range a cell of `width` occupies when placed at `x` in this orientation.
///
/// Mirrored orientations grow to the *left* of the placement point, which is why an overlap test
/// that assumes `[x, x+width]` silently misplaces every cell in a mirrored row.
fn start_end(x: i32, width: i32, orient: &str) -> (i32, i32) {
    match orient {
        "MY" | "R180" => (x - width, x),
        _ => (x, x + width),
    }
}

fn overlaps(x: i32, width: i32, orient: &str, occupied: &[Rect]) -> bool {
    let (s, e) = start_end(x, width, orient);
    occupied.iter().any(|r| e > r.x0 && s < r.x1)
}

/// **T6** — the nearest legal x at or after `x`, or `None` if the cell cannot go here at all.
///
/// When the candidate is clear it stays put. When it collides, it slides to whichever side of the
/// obstacle it was already overhanging — right if it stuck out to the right, left if to the left —
/// and that new position is re-checked before being accepted.
///
/// `disallow_one_site_gaps` widens the cell by a site on each side, so a position leaving exactly
/// one site free next to an obstacle counts as a collision. A one-site gap is unfillable by any
/// legalizer, so leaving one is worse than moving the tap.
#[allow(clippy::too_many_arguments)]
pub fn find_valid_location(
    x: i32,
    orient: &str,
    occupied: &[Rect],
    site_width: i32,
    tap_width: i32,
    row_x_max: i32,
    disallow_one_site_gaps: bool,
) -> Option<i32> {
    let (mut s, mut e) = start_end(x, tap_width, orient);
    if disallow_one_site_gaps {
        // +1 turns the strict `>` in the overlap test into `>=`.
        s -= site_width + 1;
        e += site_width + 1;
    }

    let mut hit: Option<&Rect> = None;
    for r in occupied {
        if e > r.x0 && s < r.x1 {
            hit = Some(r);
            break;
        }
    }

    let x_loc = match hit {
        None => Some(x),
        // Overhanging to the right of the obstacle: pull back so the cell ends where it begins.
        Some(r) if s < r.x0 => Some(r.x0 - tap_width),
        // Overhanging to the left: push past its right edge.
        Some(r) if e > r.x1 => Some(r.x1),
        Some(_) => None,
    }?;

    let fits = x_loc + tap_width <= row_x_max;
    let clear = hit.is_none() || !overlaps(x_loc, tap_width, orient, occupied);
    (fits && clear).then_some(x_loc)
}

/// **T5** — which rows sit on the top or bottom boundary of the region.
///
/// Returns one flag per row, **positionally** — not a set of names. ⚠️ Row names are not unique:
/// `cut_rows` generates names that collide with another row family's, and a name-keyed set makes
/// one boundary row mark every other row sharing its name. Those rows are then tapped at twice
/// the density they should be.
///
/// Only horizontal boundaries count: a row's *ends* touching the left or right edge says nothing
/// about the well above or below it. A row that merely touches an edge at a single point is not
/// on it — the overlap has to have length.
pub fn edge_rows(edges: &[Edge], rows: &[Row], site: &str) -> Vec<bool> {
    rows.iter()
        .map(|row| {
            row.site == site
                && edges
                    .iter()
                    .filter(|e| matches!(e.kind, EdgeType::Top | EdgeType::Bottom))
                    .any(|e| {
                        let (ex0, ex1) = e.x_span();
                        let y = e.p0.y;
                        // The contact has to have length along the edge...
                        let ox = row.bbox.x1.min(ex1) - row.bbox.x0.max(ex0);
                        // ...and the row has to be on the material side of it. A Bottom edge has
                        // material ABOVE, so it belongs to the row starting there; a Top edge has
                        // material BELOW, so it belongs to the row ending there. Accepting either
                        // side makes every row next to a hole look like a boundary row, and boundary
                        // rows are tapped twice as densely.
                        let on_material_side = match e.kind {
                            EdgeType::Bottom => row.bbox.y0 == y,
                            EdgeType::Top => row.bbox.y1 == y,
                            _ => false,
                        };
                        ox > 0 && on_material_side
                    })
        })
        .collect()
}

/// **T5** — the taps for one row.
///
/// `phy_index` is the running count of physical instances created so far; upstream numbers them
/// globally, so the name of a tap depends on how many were made before it, in any row.
#[allow(clippy::too_many_arguments)]
pub fn place_in_row(
    row: &Row,
    master: &Master,
    dist: i32,
    is_edge: bool,
    disallow_one_site_gaps: bool,
    fixed_in_row: &[Rect],
    tap_prefix: &str,
    phy_index: &mut usize,
) -> Vec<Placement> {
    if row.site != master.site
        || !check_symmetry(master.symmetry_x, master.symmetry_y, &row.orient)
        || master.width <= 0
    {
        return Vec::new();
    }

    // An edge row is tapped twice as densely: multiplier 1 rather than 2.
    let pitch_mult = if is_edge { 1 } else { 2 };
    let pitch = master.width * ((pitch_mult * dist) as f64 / master.width as f64).floor() as i32;
    if pitch <= 0 {
        return Vec::new();
    }
    // Rows drawn R0, and every edge row, start half a pitch in; mirrored rows start a full pitch
    // in, which is what staggers taps between neighbouring rows.
    let offset = if row.orient == "R0" || is_edge {
        pitch / pitch_mult
    } else {
        pitch
    };

    let mut occupied: Vec<Rect> = fixed_in_row.to_vec();
    let mut out = Vec::new();
    let mut x = row.bbox.x0 + offset;
    while x < row.bbox.x1 {
        let snapped = make_site_loc(x, row.site_width, true, row.bbox.x0);
        match find_valid_location(
            snapped,
            &row.orient,
            &occupied,
            row.site_width,
            master.width,
            row.bbox.x1,
            disallow_one_site_gaps,
        ) {
            Some(x_loc) => {
                let (s, e) = start_end(x_loc, master.width, &row.orient);
                occupied.push(Rect::new(s, row.bbox.y0, e, row.bbox.y1));
                out.push(Placement {
                    name: format!("{tap_prefix}TAPCELL_{}_{}", row.name, phy_index),
                    master: master.name.clone(),
                    x: x_loc,
                    y: row.bbox.y0,
                    orient: row.orient.clone(),
                });
                *phy_index += 1;
                // Resume from where the tap actually landed, not from the grid position it was
                // nudged off — otherwise a nudged tap can be re-proposed at the same place.
                x = x_loc;
            }
            None => {
                // Nothing fits here; the scan still advances so a single obstruction does not
                // stop the row.
            }
        }
        x += pitch;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, orient: &str, x0: i32, x1: i32) -> Row {
        Row {
            name: name.into(),
            site: "core".into(),
            orient: orient.into(),
            bbox: Rect::new(x0, 0, x1, 10),
            site_width: 10,
        }
    }

    fn master() -> Master {
        Master {
            name: "TAP".into(),
            site: "core".into(),
            width: 20,
            height: 10,
            symmetry_x: true,
            symmetry_y: true,
        }
    }

    #[test]
    fn the_site_grid_snap_rounds_down_from_the_row_origin() {
        // Snapping is relative to the row's own start, not to absolute zero: a row that does not
        // begin on a multiple of the site width still gets taps on ITS grid.
        assert_eq!(make_site_loc(105, 10, true, 0), 100);
        assert_eq!(make_site_loc(105, 10, false, 0), 110, "ceil when asked");
        assert_eq!(
            make_site_loc(107, 10, true, 5),
            105,
            "offset row: 5, 15, 25 …"
        );
        assert_eq!(
            make_site_loc(100, 10, true, 0),
            100,
            "already on grid, unmoved"
        );
        assert_eq!(
            make_site_loc(42, 0, true, 0),
            42,
            "a zero site width cannot snap"
        );
    }

    #[test]
    fn a_cell_may_only_be_mirrored_where_the_library_says_so() {
        assert!(check_symmetry(false, false, "R0"), "upright always works");
        assert!(check_symmetry(true, false, "MX"));
        assert!(!check_symmetry(false, false, "MX"));
        assert!(check_symmetry(false, true, "MY"));
        assert!(!check_symmetry(true, false, "MY"));
        assert!(check_symmetry(true, true, "R180"));
        assert!(!check_symmetry(true, false, "R180"), "R180 needs both");
        assert!(
            !check_symmetry(true, true, "R90"),
            "a rotation is not a mirror"
        );
        assert!(!check_symmetry(true, true, "nonsense"));
    }

    #[test]
    fn taps_land_at_the_pitch_and_an_edge_row_gets_twice_as_many() {
        let m = master();
        let mut idx = 0;
        let plain = place_in_row(
            &row("R1", "R0", 0, 1000),
            &m,
            100,
            false,
            false,
            &[],
            "PHY_",
            &mut idx,
        );
        let mut idx2 = 0;
        let edge = place_in_row(
            &row("R1", "R0", 0, 1000),
            &m,
            100,
            true,
            false,
            &[],
            "PHY_",
            &mut idx2,
        );

        // pitch = 20 * floor(2*100/20) = 200, offset = 100.
        assert_eq!(
            plain.iter().map(|p| p.x).collect::<Vec<_>>(),
            vec![100, 300, 500, 700, 900]
        );
        // pitch = 20 * floor(1*100/20) = 100, offset = 100.
        assert_eq!(edge.len(), 9, "an edge row is tapped twice as densely");
        assert_eq!(edge[0].x, 100);
        assert_eq!(edge[1].x, 200);
    }

    #[test]
    fn a_mirrored_row_starts_a_full_pitch_in_so_taps_stagger() {
        let m = master();
        let (mut a, mut b) = (0, 0);
        let r0 = place_in_row(
            &row("R1", "R0", 0, 1000),
            &m,
            100,
            false,
            false,
            &[],
            "PHY_",
            &mut a,
        );
        let mx = place_in_row(
            &row("R2", "MX", 0, 1000),
            &m,
            100,
            false,
            false,
            &[],
            "PHY_",
            &mut b,
        );
        assert_eq!(r0[0].x, 100, "upright rows start half a pitch in");
        assert_eq!(mx[0].x, 200, "mirrored rows start a full pitch in");
        assert_ne!(
            r0[0].x, mx[0].x,
            "which is the point: neighbouring rows stagger"
        );
    }

    #[test]
    fn a_candidate_swallowed_by_an_instance_is_abandoned_not_moved() {
        // The obstacle CONTAINS the candidate on both sides, so there is no overhang to slide
        // along. Upstream sets neither direction flag and places nothing here — the scan simply
        // carries on, which is why one macro does not silence a row.
        let m = master();
        let blocked = [Rect::new(90, 0, 150, 10)];
        let mut idx = 0;
        let out = place_in_row(
            &row("R1", "R0", 0, 1000),
            &m,
            100,
            false,
            false,
            &blocked,
            "PHY_",
            &mut idx,
        );
        assert!(
            !out.is_empty(),
            "an obstruction must not silence the whole row"
        );
        assert_eq!(
            out[0].x, 300,
            "the blocked candidate at 100 is skipped, the next one lands"
        );
        for p in &out {
            assert!(
                p.x + m.width <= 90 || p.x >= 150,
                "tap at {} overlaps the macro",
                p.x
            );
        }
    }

    #[test]
    fn a_partly_overlapping_candidate_slides_to_the_side_it_overhangs() {
        // The two correction branches, which are easy to get backwards: a tap poking into an
        // obstacle from the left retreats to end at the obstacle's start; one poking in from the
        // right advances to begin at the obstacle's end.
        let from_left = find_valid_location(
            100,
            "R0",
            &[Rect::new(110, 0, 200, 10)],
            10,
            20,
            1000,
            false,
        );
        assert_eq!(
            from_left,
            Some(90),
            "ends exactly where the obstacle begins"
        );

        let from_right =
            find_valid_location(100, "R0", &[Rect::new(50, 0, 110, 10)], 10, 20, 1000, false);
        assert_eq!(
            from_right,
            Some(110),
            "begins exactly where the obstacle ends"
        );

        // ...and a correction that would hang off the row end is refused rather than clamped.
        assert_eq!(
            find_valid_location(100, "R0", &[Rect::new(50, 0, 110, 10)], 10, 20, 125, false),
            None,
            "sliding to 110 would need up to 130, past the row end at 125"
        );
    }

    #[test]
    fn a_tap_is_never_placed_past_the_end_of_its_row() {
        let m = master();
        let mut idx = 0;
        // Row ends at 210; the candidate at x=200 would need up to 220.
        let out = place_in_row(
            &row("R1", "R0", 0, 210),
            &m,
            100,
            true,
            false,
            &[],
            "PHY_",
            &mut idx,
        );
        assert!(
            out.iter().all(|p| p.x + m.width <= 210),
            "a tap hanging off the row end is not a tap: {out:?}"
        );
    }

    #[test]
    fn a_sub_site_gap_is_closed_up_when_the_technology_cannot_fill_it() {
        let obstacle = [Rect::new(200, 0, 400, 10)];
        // A tap ending at 195 leaves a 5-unit gap before the obstacle. Left alone that gap can
        // never be filled, so with the rule on, the tap is pulled flush against the obstacle.
        assert_eq!(
            find_valid_location(175, "R0", &obstacle, 10, 20, 1000, false),
            Some(175),
            "without the rule the gap is nobody's problem"
        );
        assert_eq!(
            find_valid_location(175, "R0", &obstacle, 10, 20, 1000, true),
            Some(180),
            "with it, the tap moves up to end flush at 200"
        );
        // A position already flush is left alone: widening detects a collision, but the
        // correction uses the cell's REAL width and lands back where it started.
        assert_eq!(
            find_valid_location(180, "R0", &obstacle, 10, 20, 1000, true),
            Some(180)
        );
    }

    #[test]
    fn a_row_of_the_wrong_site_or_illegal_orientation_gets_nothing() {
        let mut m = master();
        let mut idx = 0;
        let mut r = row("R1", "R0", 0, 1000);
        r.site = "other".into();
        assert!(place_in_row(&r, &m, 100, false, false, &[], "PHY_", &mut idx).is_empty());

        m.symmetry_x = false;
        let mirrored = row("R1", "MX", 0, 1000);
        assert!(
            place_in_row(&mirrored, &m, 100, false, false, &[], "PHY_", &mut idx).is_empty(),
            "an asymmetric master cannot be flipped into a mirrored row"
        );
        assert_eq!(idx, 0, "and nothing was numbered");
    }

    #[test]
    fn instances_are_numbered_globally_and_named_for_their_row() {
        let m = master();
        let mut idx = 7; // as if seven physical cells already existed
        let a = place_in_row(
            &row("ROW_3", "R0", 0, 500),
            &m,
            100,
            false,
            false,
            &[],
            "PHY_",
            &mut idx,
        );
        let b = place_in_row(
            &row("ROW_4", "R0", 0, 500),
            &m,
            100,
            false,
            false,
            &[],
            "PHY_",
            &mut idx,
        );
        assert_eq!(a[0].name, "PHY_TAPCELL_ROW_3_7");
        assert_eq!(a[1].name, "PHY_TAPCELL_ROW_3_8");
        assert_eq!(b[0].name, format!("PHY_TAPCELL_ROW_4_{}", 7 + a.len()));
        assert_eq!(
            idx,
            7 + a.len() + b.len(),
            "the counter is global, not per row"
        );
    }

    #[test]
    fn edge_rows_are_the_ones_touching_a_horizontal_boundary() {
        use crate::boundary::{self, Rect as BRect};
        let core = BRect::new(0, 0, 100, 40);
        let rows_r: Vec<Row> = (0..4)
            .map(|i| Row {
                name: format!("R{i}"),
                site: "core".into(),
                orient: "R0".into(),
                bbox: Rect::new(0, i * 10, 100, i * 10 + 10),
                site_width: 10,
            })
            .collect();
        let region = boundary::row_region(core, &rows_r.iter().map(|r| r.bbox).collect::<Vec<_>>());
        let edges: Vec<Edge> = boundary::classify(&region)
            .into_iter()
            .flat_map(|p| p.outer_edges)
            .collect();

        let on_edge = edge_rows(&edges, &rows_r, "core");
        let names: Vec<&str> = rows_r
            .iter()
            .zip(on_edge.iter())
            .filter(|(_, flag)| **flag)
            .map(|(r, _)| r.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["R0", "R3"],
            "only the bottom and top rows are on a boundary"
        );

        // Positional, not name-keyed: two rows sharing a name are judged separately.
        let mut dup = rows_r.clone();
        dup[1].name = "R0".into();
        let flags = edge_rows(&edges, &dup, "core");
        assert!(
            flags[0] && !flags[1],
            "an interior row is not a boundary row just by sharing a name"
        );
    }
}
