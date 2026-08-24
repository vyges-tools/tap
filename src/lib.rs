// SPDX-License-Identifier: Apache-2.0
//! Tapcell and endcap insertion — the decisions, separated from the database.
//!
//! Same split as `vyges-ifp`: this module is pure arithmetic and policy over values, and the
//! binary reads the database, asks this module what to do, and applies it.
//!
//! # What is delegated, and why
//!
//! Row cutting is **not** implemented here. `cutRows` lives in `odb/util.h` — it is odb's own
//! algorithm operating on odb's own rows, and OpenDB is our substrate, not a reference to
//! reimplement. What belongs to the engine is the *policy*: which instances count as blockages,
//! what the halo and minimum row width are, and what to say about a macro it had to skip. Those
//! are here; the cutting itself goes straight to odb.
//!
//! # Provenance
//!
//! Rules are numbered **T1**… and cited on each
//! function), extracted by reading OpenROAD's `tapcell.cpp` — the reference implementation for
//! this behaviour. Nothing is copied from it.

/// This crate's version, as Cargo knows it — the single number the whole suite is released on.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The copyright line `--version` prints.
pub const COPYRIGHT: &str = "© 2026 Vyges. All Rights Reserved.  https://vyges.com";

pub mod boundary;
pub mod endcaps;
pub mod tapcells;

/// One instance, reduced to what row cutting cares about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    pub name: String,
    /// A macro — `dbInst::isBlock()`. Only macros block rows.
    pub is_block: bool,
    pub is_placed: bool,
}

/// What [`select_blockages`] decided.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Blockages {
    /// Instances to cut rows around, in the order given.
    pub cut_around: Vec<String>,
    /// Macros skipped because they are not placed — reported, never silently dropped
    /// (upstream TAP-32).
    pub unplaced: Vec<String>,
}

/// **T1** — which instances block rows.
///
/// A macro that has not been placed has no meaningful position to cut around, so it is skipped —
/// but it is *named*, because silently ignoring it would leave rows crossing wherever that macro
/// eventually lands, and nothing downstream would say why.
pub fn select_blockages(instances: &[Instance]) -> Blockages {
    let mut out = Blockages::default();
    for i in instances {
        if !i.is_block {
            continue;
        }
        if i.is_placed {
            out.cut_around.push(i.name.clone());
        } else {
            out.unplaced.push(i.name.clone());
        }
    }
    out
}

/// **T2** — the default halo and distance: 2 µm, expressed in DBU.
///
/// Upstream's `defaultDistance()`. It is a length, so it has to be converted through the
/// database's own scale rather than assumed.
pub fn default_distance(dbu_per_micron: i32) -> i32 {
    2 * dbu_per_micron
}

/// **T3** — the minimum width a row may be left with.
///
/// Two sources, and the *larger* wins: a row must be wide enough to hold two endcaps (one at each
/// end) when an endcap master is known, and it must respect any explicit floor the caller gave.
/// Taking the larger is what keeps a caller's `-row_min_width` from quietly producing rows too
/// narrow to cap.
pub fn min_row_width(endcap_width: Option<i32>, row_min_width: Option<i32>) -> i32 {
    let from_endcap = endcap_width.map(|w| 2 * w).unwrap_or(0);
    from_endcap.max(row_min_width.unwrap_or(0))
}

/// **T10** — the shortest row worth keeping, which is what odb cuts narrow regions against.
///
/// 🔑 **A region between two vertically stacked blockages has to fit an endcap ABOVE, an endcap
/// BELOW, and a cell between them** — so the floor is `2 * endcap height + the tallest core cell`,
/// and `2 * the tallest core cell` where no endcap master was given.
///
/// ⚠️ **Zero is not the neutral value here, it is "do not cut narrow regions at all".** odb gates
/// the whole check on `min_row_height > 0`, so passing 0 silently keeps slivers a tapcell run can
/// never fill. `ifp` passes 0 deliberately — it is not placing endcaps — and `tap` must not.
///
/// ⚠️ **The tallest CORE cell, not the tallest master.** Blocks, pads, covers, endcaps and fillers
/// are excluded, and so is anything the library marks as not auto-placeable: a macro's height would
/// put the floor far above any real row.
pub fn min_row_height(
    endcap_height: Option<i32>,
    max_core_cell_height: i32,
    row_min_height: Option<i32>,
) -> i32 {
    let from_endcap = match endcap_height {
        Some(h) => 2 * h + max_core_cell_height,
        None => 2 * max_core_cell_height,
    };
    from_endcap.max(row_min_height.unwrap_or(0))
}

/// A value the caller gave, or the default, resolved once so the applier never re-derives it.
///
/// Upstream encodes "not given" as a negative number; this keeps the distinction in the type so
/// a caller cannot pass -1 through by accident and get a silent default.
pub fn or_default(given: Option<i32>, default: i32) -> i32 {
    match given {
        Some(v) if v >= 0 => v,
        _ => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_minimum_row_height_stacks_two_endcaps_and_a_cell() {
        // Upstream rule: 2 * endcap height + the tallest core cell — a narrow region must fit an
        // endcap above, one below, and a placeable cell between them.
        assert_eq!(min_row_height(Some(1400), 2800, None), 5600);
    }

    #[test]
    fn with_no_endcap_master_it_is_two_core_cells() {
        assert_eq!(min_row_height(None, 2800, None), 5600);
    }

    #[test]
    fn a_callers_floor_wins_when_it_is_larger() {
        assert_eq!(min_row_height(Some(1400), 2800, Some(9000)), 9000);
        assert_eq!(min_row_height(Some(1400), 2800, Some(1000)), 5600, "and loses when smaller");
    }

    #[test]
    fn ZERO_would_disable_the_check_entirely_so_it_is_never_the_answer_here() {
        // ⚠️ odb gates narrow-region cutting on `min_row_height > 0`. `ifp` passes 0 because it
        // places no endcaps; a tapcell run passing 0 keeps slivers it can never fill.
        assert!(min_row_height(None, 2800, None) > 0);
        assert!(min_row_height(None, 0, Some(0)) == 0, "only a library with no core cells at all");
    }

    fn inst(name: &str, is_block: bool, is_placed: bool) -> Instance {
        Instance {
            name: name.into(),
            is_block,
            is_placed,
        }
    }

    #[test]
    fn only_placed_macros_are_cut_around_and_the_unplaced_ones_are_named() {
        let insts = vec![
            inst("std_cell", false, true),
            inst("ram0", true, true),
            inst("ram1", true, false),
            inst("unplaced_std_cell", false, false),
        ];
        let b = select_blockages(&insts);
        assert_eq!(b.cut_around, vec!["ram0"], "only placed macros block rows");
        assert_eq!(
            b.unplaced,
            vec!["ram1"],
            "an unplaced macro is reported, not silently dropped"
        );
    }

    #[test]
    fn a_design_with_no_macros_asks_for_no_cutting() {
        let b = select_blockages(&[inst("a", false, true), inst("b", false, true)]);
        assert_eq!(b, Blockages::default());
    }

    #[test]
    fn the_minimum_row_width_takes_the_larger_of_its_two_sources() {
        // Two endcaps' worth, or the caller's floor, whichever is bigger.
        assert_eq!(min_row_width(Some(100), None), 200, "room for two endcaps");
        assert_eq!(min_row_width(None, Some(500)), 500, "the caller's floor");
        assert_eq!(min_row_width(Some(100), Some(500)), 500);
        assert_eq!(
            min_row_width(Some(400), Some(500)),
            800,
            "endcaps win when wider"
        );
        assert_eq!(
            min_row_width(None, None),
            0,
            "no constraint stated is no constraint"
        );
    }

    #[test]
    fn the_default_distance_is_two_microns_in_database_units() {
        assert_eq!(default_distance(1000), 2000);
        assert_eq!(default_distance(2000), 4000);
    }

    #[test]
    fn a_negative_value_means_not_given_and_falls_back() {
        // Upstream signals "unset" with -1; zero is a real value and must survive.
        assert_eq!(or_default(Some(-1), 2000), 2000);
        assert_eq!(or_default(None, 2000), 2000);
        assert_eq!(
            or_default(Some(0), 2000),
            0,
            "an explicit zero is not a missing value"
        );
        assert_eq!(or_default(Some(37), 2000), 37);
    }
}

/// **T11** — the cells a rip-up removes: those whose name starts with `prefix`.
///
/// 🔑 **An empty prefix removes NOTHING.** Every name starts with the empty string, so the
/// obvious reading would delete every instance in the design — the netlist included. Upstream
/// guards this explicitly and so does this; it is the difference between undoing a tap step and
/// destroying the design.
///
/// Matching is by name because that is the only mark these cells carry: they are physical-only
/// instances with no net, and nothing else distinguishes a tap from any other filler.
/// The verdict word for a run that changed `did` things.
///
/// 🔑 **A pass word asserts that work was DONE; if none was, do not say it.** `applied` is what
/// the descriptor's assertion reads (`status == "applied"`), so a run that inserted, cut or
/// removed nothing would otherwise pass a gate having changed the design in no way at all. That
/// is not a hypothetical failure mode here: an option that does not arrive does not error, it
/// silently does nothing — `-halo_width_x` hid four cases that way, and `ppl` lost a day to
/// `set_slots_per_section` doing the same.
///
/// ⚠️ **`vacuous` is not an error.** Zero may be the right answer — a design with no macros cuts
/// no rows. The caller reads the count and decides; what it must not do is read a no-op as a
/// verified transformation. A dry run keeps `planned`, which never claimed to have applied
/// anything and already fails the assertion.
pub fn settle_status(dry_run: bool, did: usize) -> &'static str {
    if dry_run {
        return "planned";
    }
    if did == 0 {
        return "vacuous";
    }
    "applied"
}

#[cfg(test)]
mod settle_status_tests {
    use super::settle_status;

    #[test]
    fn a_run_that_changed_nothing_is_not_reported_as_applied() {
        assert_eq!(settle_status(false, 0), "vacuous");
        assert_eq!(settle_status(false, 1), "applied");
    }

    #[test]
    fn a_dry_run_never_claims_to_have_applied_anything() {
        // `planned` already fails the declared assertion, so it needs no vacuous variant of its
        // own — and giving it one would churn every dry-run consumer for no safety gain.
        assert_eq!(settle_status(true, 0), "planned");
        assert_eq!(settle_status(true, 9), "planned");
    }
}

pub fn cells_with_prefix(names: &[String], prefix: &str) -> Vec<String> {
    if prefix.is_empty() {
        return Vec::new();
    }
    names
        .iter()
        .filter(|n| n.starts_with(prefix))
        .cloned()
        .collect()
}

#[cfg(test)]
mod ripup_tests {
    use super::*;

    fn names() -> Vec<String> {
        [
            "TAP_TAPCELL_ROW_0_1",
            "PHY_CORNER_ROW_0_OuterBottomLeft_0",
            "PHY_EDGE_ROW_1_Top_2",
            "u_alu/adder_3",
            "TAPPED_BUT_NOT_OURS",
        ]
        .into_iter()
        .map(String::from)
        .collect()
    }

    #[test]
    fn a_prefix_selects_only_its_own_cells() {
        assert_eq!(
            cells_with_prefix(&names(), "TAP_"),
            vec!["TAP_TAPCELL_ROW_0_1"]
        );
        assert_eq!(
            cells_with_prefix(&names(), "PHY_"),
            vec!["PHY_CORNER_ROW_0_OuterBottomLeft_0", "PHY_EDGE_ROW_1_Top_2"]
        );
        // Design instances are never touched.
        assert!(!cells_with_prefix(&names(), "TAP_")
            .iter()
            .any(|n| n.contains('/')));
    }

    #[test]
    fn an_empty_prefix_removes_nothing_rather_than_everything() {
        // Every name starts with "", so the naive reading deletes the whole design. This is the
        // one case where doing nothing is obviously right and doing the literal thing is fatal.
        assert!(cells_with_prefix(&names(), "").is_empty());
    }

    #[test]
    fn a_prefix_that_matches_nothing_is_not_an_error() {
        assert!(cells_with_prefix(&names(), "NOSUCH_").is_empty());
        assert!(cells_with_prefix(&[], "TAP_").is_empty());
    }

    #[test]
    fn matching_is_a_prefix_not_a_substring() {
        // "TAPPED_BUT_NOT_OURS" contains "TAP" but does not start with "TAP_".
        let picked = cells_with_prefix(&names(), "TAP_");
        assert!(!picked.iter().any(|n| n == "TAPPED_BUT_NOT_OURS"));
    }
}
