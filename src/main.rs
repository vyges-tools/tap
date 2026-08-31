// SPDX-License-Identifier: Apache-2.0
//! `vyges-tap` CLI — row cutting and physical-cell insertion over a `.odb`.
//!
//! Exit status is the verdict: 0 applied, 1 the design cannot be processed as asked,
//! 2 usage/read/write error.

use std::process::ExitCode;
use vyges_opendb::Db;
use vyges_tap::boundary;
use vyges_tap::endcaps::{self, EndcapMasters, TapcellMasters};
use vyges_tap::tapcells::{self, Master, Placement};
use vyges_tap::{
    cells_with_prefix, default_distance, min_row_height, min_row_width, or_default,
    select_blockages, Instance,
};

// ⚠️ The prefix here is the CLI GROUP this engine belongs to, and vyges-cli's MODULES
// registry is what actually decides it (`group: "physical"`). It read `vyges loom tap`
// for a release after the construction engines were split out of the loom suite, because
// nothing ties this string to that registry -- `vyges loom tap` is now REFUSED by the CLI,
// so the help was telling users a command that no longer runs. If the group ever moves,
// this string moves with it. Running the binary directly as `vyges-tap` always works and
// is group-independent.
const USAGE: &str = "\
vyges physical tap — row cutting and physical-cell insertion

USAGE:
  vyges physical tap cut-rows <design.odb> [--halo-x UM] [--halo-y UM] [--row-min-width UM]
                                       [--endcap-master NAME]
  vyges physical tap place-tapcells <design.odb> --master NAME [--distance UM] [--tap-prefix P]
  vyges physical tap place-endcaps <design.odb> [--corner NAME] [--edge-corner NAME]
                               [--endcap-horizontal A,B] [--endcap-vertical NAME]
                               [--left-top-corner NAME] ... [--prefix P]
  vyges physical tap tapcell <design.odb> --tapcell-master NAME --endcap-master NAME
                          [--distance UM] [--halo-width-x UM] [--halo-width-y UM]
                          [--cnrcap-nwin-master NAME] [--tap-nwintie-master NAME] ...
  vyges physical tap ripup <design.odb> [--tap-prefix TAP_] [--endcap-prefix PHY_]
  vyges physical tap boundary <design.odb>
  vyges physical tap --describe
  vyges physical tap --help

OPTIONS:
  --halo-x UM            keep-out around a macro, horizontally, in MICRONS (default 2)
  --halo-y UM            keep-out around a macro, vertically, in MICRONS (default 2)
  --row-min-width UM     do not leave a row narrower than this, in MICRONS
  --row-min-height UM    do not leave a row region shorter than this, in MICRONS
  --endcap-master NAME   reserve room for one endcap at each end of every row
  --out-odb FILE         write the database here (default: IN PLACE, over the input)
  --out-def FILE         also write the result as DEF (for diffing against a golden)
  --dry-run              report what would be cut, write nothing
  -o FILE                write the report to FILE instead of stdout
  --json                 emit JSON (the default)
  --describe             print a machine-readable JSON description of the command

EXIT STATUS:
  0  applied     rows were cut and the database written
  0  vacuous     the run changed nothing -- NOT a transformation; read the count
  1  refused     the design cannot be processed as asked
  2  error       usage error, unreadable database, no DBU scale, or a failed write
";


/// The pin, inherited from the crate every engine already depends on.
const CRATE_PIN: &str = vyges_opendb::OPENROAD_PIN;

/// The pin this binary was built against, injected into the descriptor at print time.
///
/// 🔑 **One definition for the whole programme, inherited rather than typed.** The SHA lives in
/// `openroad-pin.yaml` in `vyges-opendb-lib` and reaches here through `vyges-opendb`, which this
/// engine already depends on. Before this, every engine spelled the pin out in its own
/// `--describe` prose, and four of them were still quoting the previous one a day after it moved.
///
/// ⚠️ **It reports what this BINARY was built against — not that the binary is current.** A stale
/// build reports its stale pin quite happily. That is the point: a harness compares this against
/// the oracle image it is about to launch and refuses on a mismatch, which is the check that was
/// missing when two engines ran a whole gate against the previous pin's oracle.
const PIN_TOKEN: &str = "@OPENROAD_PIN@";

fn describe() -> String {
    DESCRIBE.replace(PIN_TOKEN, CRATE_PIN)
}

const DESCRIBE: &str = r#"{
  "schema": "vyges-tool-descriptor/1.1",
  "openroad_pin": "@OPENROAD_PIN@",
  "name": "tap",
  "summary": "row cutting around macros, and physical-cell insertion (well taps, endcaps)",
  "maturity": "structured",
  "provenance_limitations": [
      "input_hash covers the argument vector, not the content of the .odb it names.",
      "The `boundary` verb is INSPECT-ONLY: it reports the row-region, edge and corner classification and writes nothing. Every other verb that places (`place-endcaps`, `place-tapcells`, `tapcell`) does mutate the design -- they create physical instances, orient, locate, lock and mark them -- as does `cut-rows`, and `ripup` removes them. This line previously read \"NOTHING IS PLACED from it yet, ONLY cut-rows mutates\", which was true of an early build and stayed here after placement shipped and was measured exact; it is corrected rather than deleted because a reader who saw the old text needs to know it moved, not wonder which of two claims to believe.",
      "Unnamed endcap positions are filled from the library's own LEF58 master types (odb reports these as space-separated strings like \"ENDCAP LEFTBOTTOMCORNER\", not the enum spelling). Two masters claiming one position is an ERROR naming both, not a coin flip: a wrong endcap is a well-tie fault nobody sees until silicon. A position nothing fills stays empty and places nothing.",
      "Taps and endcaps use DIFFERENT default name prefixes -- TAP_ and PHY_ -- because they are separate namespaces that can be ripped up independently.",
      "MEASURED 2026-08-23 against the upstream goldens at pin @OPENROAD_PIN@: cut_rows 10 of 10 comparable cases exact (DEF ROW diff), and endcap placement 9 of 9 exact -- every physical cell matching the golden in master, position and orientation.",
      "status is one of applied, planned, vacuous or error. VACUOUS IS NOT APPLIED: it means the run changed nothing -- no row cut, no cell inserted, none removed -- and the declared assertion passes only on applied, so a no-op fails it rather than reporting a transformation that did not happen. Zero may still be the right answer for the design; read the count and decide. A dry run reports planned, which never claimed to have applied anything.",
      "All five commands are implemented: cut-rows, place-endcaps, place-tapcells, the combined tapcell, and ripup. Rip-up matches by NAME PREFIX, which is the only mark these cells carry -- they are physical-only instances with no nets. An EMPTY prefix removes nothing rather than everything, which is the difference between undoing a tap step and destroying the design.",
      "COMBINED TAPCELL IS 20 OF 20 AT THIS PIN. It was 16 of 16 at the previous pin @OPENROAD_PIN@ and read 14 of 20 the moment the pin moved, with nothing in this engine changed: upstream had reworked endcap placement across sixteen commits and added four regression cases. A SCORE IS ONLY TRUE OF ONE COMMIT -- quote the pin beside it. The last case closed by walking a hole the way the reference walks it: the reference receives holes wound like outer boundaries and walks every ring counter-clockwise, while this engine winds holes clockwise, so the classification agreed and the placement ORDER did not -- and where two cells contend for one position, whichever is walked to first keeps it.",
      "Row cutting itself is odb's own cutRows from odb/util.h, not a reimplementation: it is odb's algorithm on odb's rows, and OpenDB is the substrate. What this engine decides is the policy around it -- which instances are blockages, the halo, and the minimum row width.",
      "Blockages are placed macros (dbInst::isBlock). A macro that is NOT placed is skipped and reported by name (upstream TAP-32), never silently ignored, because rows would otherwise be left crossing wherever it lands.",
      "The minimum row width is the LARGER of two endcap widths and any --row-min-width given, so a caller's floor cannot quietly produce rows too narrow to cap.",
      "Halos and widths are given in MICRONS and converted with the database's dbu_per_micron. A database with no DBU scale is an error rather than an assumed scale.",
      "Written against the upstream tap regression goldens at pin @OPENROAD_PIN@ (vyges-openroad 2026.08.0). Conformance for the implemented subset is measured by diffing the DEF ROWS section against each case's .defok: 10 of 10 comparable cases exact. ⚠️ A correlation result is a statement about ONE upstream commit -- this one was re-measured when the pin moved, and combined tapcell regressed from exact.",
      "The corner classification is no longer checked by a separate harness. That check compared this engine's corner CENSUS against the corner CELLS a golden holds and reported 4 exact and 6 unresolved; it was retired on 2026-08-23 because those 6 could never resolve -- upstream classifies every corner too and filters only at placement (getRow), so a corner no row reaches leaves no trace in any golden on either side. All 10 of its cases are compared cell by cell by the DEF gates instead, and the corner TYPE is part of the instance name those compare (PHY_CORNER_ROW_0_OuterBottomLeft_0), alongside master, position and orientation.",
      "The default output is IN PLACE, over the input database. Pass --out-odb to write elsewhere, or --dry-run to report without writing."
  ],
  "invocation": {
    "args_template": ["cut-rows", "{odb}"],
    "optional": [
      { "arg": "out", "flag": "-o" },
      { "arg": "out_odb", "flag": "--out-odb" }
    ],
    "emits_json": true
  },
  "inputs": {
    "type": "object",
    "required": ["odb"],
    "properties": {
      "odb": { "type": "string", "description": "path to the design database (.odb)" },
      "out_odb": { "type": "string", "description": "write the database here instead of in place" },
      "out": { "type": "string", "description": "write the report to FILE instead of stdout" }
    }
  },
  "consumes": ["odb"],
  "produces": ["odb"],
  "artifacts": [ { "role": "tap_report", "field": "report_path" } ],
  "assertion": {
    "id": "rows-cut",
    "field": "status",
    "pass_when": { "eq": "applied" }
  }
}
"#;

#[derive(Debug, Default)]
struct Cli {
    odb: String,
    halo_x: Option<f64>,
    halo_y: Option<f64>,
    row_min_width: Option<f64>,
    row_min_height: Option<f64>,
    endcap_master: Option<String>,
    out_odb: Option<String>,
    out_def: Option<String>,
    report: Option<String>,
    dry_run: bool,
}

/// Microns to DBU, rounded — the same conversion upstream's `micronsToDbu` makes.
fn to_dbu(microns: f64, dbu: f64) -> i32 {
    (microns * dbu).round() as i32
}

fn parse_args(args: &[String]) -> Result<Cli, String> {
    let mut cli = Cli::default();
    let mut odb: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let mut value = || -> Result<String, String> {
            i += 1;
            args.get(i)
                .cloned()
                .ok_or_else(|| format!("{a} needs a value"))
        };
        let number = |v: String| -> Result<f64, String> {
            v.parse::<f64>()
                .map_err(|_| format!("{a} wants a number, got `{v}`"))
        };
        match a {
            "--halo-x" => {
                let v = value()?;
                cli.halo_x = Some(number(v)?);
            }
            "--halo-y" => {
                let v = value()?;
                cli.halo_y = Some(number(v)?);
            }
            "--row-min-width" => {
                let v = value()?;
                cli.row_min_width = Some(number(v)?);
            }
            "--row-min-height" => {
                let v = value()?;
                cli.row_min_height = Some(number(v)?);
            }
            "--endcap-master" => cli.endcap_master = Some(value()?),
            "--out-odb" => cli.out_odb = Some(value()?),
            "--out-def" => cli.out_def = Some(value()?),
            "-o" => cli.report = Some(value()?),
            "--dry-run" => cli.dry_run = true,
            "--json" => {}
            a if a.starts_with('-') => return Err(format!("unknown option `{a}`")),
            a => odb = Some(a.to_string()),
        }
        i += 1;
    }
    cli.odb = odb.ok_or("`cut-rows` needs a path to a .odb")?;
    Ok(cli)
}

fn cut_rows(args: &[String]) -> ExitCode {
    let cli = match parse_args(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("vyges-tap: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    let mut db = match Db::open(&cli.odb) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("vyges-tap: cannot read {}: {e}", cli.odb);
            return ExitCode::from(2);
        }
    };
    let dbu = db.dbu_per_micron();
    if dbu <= 0 {
        eprintln!("vyges-tap: no DBU scale; cannot convert microns");
        return ExitCode::from(2);
    }
    let dbu_f = dbu as f64;

    let instances: Vec<Instance> = db
        .inst_names()
        .into_iter()
        .map(|name| Instance {
            is_block: db.inst_is_block(&name),
            is_placed: db.inst_is_placed(&name),
            name,
        })
        .collect();
    let blockages = select_blockages(&instances);

    let endcap_width = cli.endcap_master.as_deref().map(|m| {
        let w = db.master_get_width(m) as i32;
        if w <= 0 {
            eprintln!("vyges-tap: endcap master `{m}` is unknown or has no width");
        }
        w
    });
    if endcap_width == Some(0) {
        return ExitCode::from(2);
    }
    // 🔑 CALL ORDER: `Tapcell::cutRows` opens with `checkPlaceable(endcap_master, "-endcap_master")`
    // — before `findBlockages`, and before any row is touched. `cut_rows` only measures the endcap
    // to size the row floor, but it refuses a master it could not later place.
    if let Some(name) = cli.endcap_master.as_deref() {
        if let Ok(m) = read_master(&db, name) {
            if let Err(e) = check_placeable(&db, Some(&m), "-endcap_master") {
                eprintln!("vyges-tap: {e}");
                return ExitCode::from(2);
            }
        }
    }

    let default = default_distance(dbu);
    let halo_x = or_default(cli.halo_x.map(|v| to_dbu(v, dbu_f)), default);
    let halo_y = or_default(cli.halo_y.map(|v| to_dbu(v, dbu_f)), default);
    let min_width = min_row_width(endcap_width, cli.row_min_width.map(|v| to_dbu(v, dbu_f)));
    // 🔑 **Not zero.** odb gates narrow-region cutting on `min_row_height > 0`, and a tapcell run
    // that passes 0 keeps slivers it can never fill — see `min_row_height`.
    let endcap_height = cli
        .endcap_master
        .as_deref()
        .map(|m| db.master_get_height(m) as i32);
    let min_height = min_row_height(
        endcap_height,
        max_core_cell_height(&db),
        cli.row_min_height.map(|v| to_dbu(v, dbu_f)),
    );

    let rows_before = db.num_rows().unwrap_or(0);
    let mut written: Option<String> = None;
    if !cli.dry_run {
        if let Err(e) = db.cut_rows(min_width, min_height, &blockages.cut_around, halo_x, halo_y)
        {
            eprintln!("vyges-tap: cannot cut rows: {e}");
            return ExitCode::from(2);
        }
        let out = cli.out_odb.clone().unwrap_or_else(|| cli.odb.clone());
        if let Err(e) = db.write(&out) {
            eprintln!("vyges-tap: cannot write {out}: {e}");
            return ExitCode::from(2);
        }
        written = Some(out);
    }
    // DEF is how this engine's result is compared against upstream's golden, so it is written
    // even on a dry run: the point is to see what WOULD land without committing it.
    if let Some(def) = cli.out_def.as_deref() {
        if let Err(e) = db.write_def(def) {
            eprintln!("vyges-tap: cannot write {def}: {e}");
            return ExitCode::from(2);
        }
    }
    let rows_after = db.num_rows().unwrap_or(0);

    emit_events(&blockages, rows_before, rows_after, written.is_some());

    let json = format!(
        "{{
  \"tool\": \"vyges-tap\",
  \"status\": \"{status}\",
  \"dbu_per_micron\": {dbu},
  \"halo\": {{ \"x\": {halo_x}, \"y\": {halo_y} }},
  \"min_row_width\": {min_width},
  \"blockages\": {n_block},
  \"macros_unplaced\": {n_unplaced},
  \"rows_before\": {rows_before},
  \"rows_after\": {rows_after},
  \"odb_written\": {written}
}}",
        // A cut that changed no row count changed nothing — see `settle_status`.
        status = vyges_tap::settle_status(cli.dry_run, (rows_after != rows_before) as usize),
        n_block = blockages.cut_around.len(),
        n_unplaced = blockages.unplaced.len(),
        written = match written.as_deref() {
            Some(p) => format!("\"{p}\""),
            None => "null".to_string(),
        },
    );
    match cli.report.as_deref() {
        Some(f) => {
            if let Err(e) = std::fs::write(f, format!("{json}\n")) {
                eprintln!("vyges-tap: cannot write {f}: {e}");
                return ExitCode::from(2);
            }
        }
        None => println!("{json}"),
    }
    ExitCode::SUCCESS
}

fn emit_events(
    blockages: &vyges_tap::Blockages,
    rows_before: usize,
    rows_after: usize,
    applied: bool,
) {
    use vyges_events::{Event, Severity};
    // Upstream's TAP-32, in its words: a macro with no position cannot be cut around, and a
    // caller that never hears about it will not understand the rows that cross it later.
    for name in &blockages.unplaced {
        vyges_events::emit(
            &Event::new(
                "vyges-tap",
                Severity::Warn,
                format!("TAP-0032 Macro {name} is not placed."),
            )
            .with_code("TAP-MACRO-UNPLACED")
            .with_objects(vec![format!("inst:{name}")]),
        );
    }
    vyges_events::emit(
        &Event::new(
            "vyges-tap",
            Severity::Info,
            format!(
                "rows {}: {} -> {} around {} macro(s)",
                if applied { "cut" } else { "would be cut" },
                rows_before,
                rows_after,
                blockages.cut_around.len()
            ),
        )
        .with_code("TAP-CUT-DONE"),
    );
}

/// Report the row region's boundary, classified. Endcap placement is driven entirely by this
/// classification, so being able to see it on its own — before any cell is placed — is what makes
/// the corner rules checkable against the goldens, which carry the type in each instance name.
fn boundary_report(args: &[String]) -> ExitCode {
    let Some(path) = args.iter().find(|a| !a.starts_with('-')) else {
        eprintln!("vyges-tap: `boundary` needs a path to a .odb\n\n{USAGE}");
        return ExitCode::from(2);
    };
    let db = match Db::open(path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("vyges-tap: cannot read {path}: {e}");
            return ExitCode::from(2);
        }
    };

    let (core, row_list) = read_rows(&db);
    let rows: Vec<boundary::Rect> = row_list.iter().map(|r| r.bbox).collect();
    let region = boundary::row_region(core, &rows);
    let classified = boundary::classify(&region);

    let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
    for p in &classified {
        for c in p.corners() {
            *counts.entry(format!("{:?}", c.kind)).or_default() += 1;
        }
    }
    let corner_json = counts
        .iter()
        .map(|(k, v)| format!("    \"{k}\": {v}"))
        .collect::<Vec<_>>()
        .join(",\n");
    println!(
        "{{\n  \"tool\": \"vyges-tap\",\n  \"rows\": {},\n  \"polygons\": {},\n  \"holes\": {},\n  \"corners\": {{\n{}\n  }}\n}}",
        rows.len(),
        classified.len(),
        classified.iter().map(|p| p.holes.len()).sum::<usize>(),
        corner_json
    );
    ExitCode::SUCCESS
}

/// **`Tapcell::maxCoreCellHeight`** — the tallest master a row actually has to hold.
///
/// ⚠️ **Core, auto-placeable, and none of the special kinds.** Blocks, pads, covers, endcaps and
/// fillers are all skipped, as is anything the library marks not auto-placeable — a macro's height
/// would put the row-height floor above every real row and cut the design to pieces.
fn max_core_cell_height(db: &Db) -> i32 {
    let mut tallest = 0;
    for i in 0..db.num_masters().unwrap_or(0) {
        let Ok(m) = db.nth_master_name(i) else { continue };
        if !db.master_is_core_auto_placeable(&m) {
            continue;
        }
        let kind = db.master_get_type(&m).unwrap_or_default();
        // ⚠️ odb spells these with a SPACE ("CORE SPACER", "ENDCAP TOPLEFT"), not the enum
        // spelling — the same trap that made every endcap invisible once before.
        if kind.starts_with("BLOCK")
            || kind.starts_with("PAD")
            || kind.starts_with("COVER")
            || kind.starts_with("ENDCAP")
            || kind.contains("SPACER")
        {
            continue;
        }
        tallest = tallest.max(db.master_get_height(&m) as i32);
    }
    tallest
}

/// Read a master's properties, or report why it cannot be used.
/// **`Tapcell::checkPlaceable`** — a master that cannot sit in a core row is REFUSED, not placed.
///
/// Upstream rule: `TAP-36` is a **fatal error** whenever a master given for one of the cell
/// options is not `isCoreAutoPlaceable()` — *"Master {} with class {} given for {} cannot be
/// placed in core rows and would be ignored by detailed placement."*
///
/// ⛔ **Measured before this existed:** `place-endcaps --corner CORNER_TOPLEFT_LEGACY` (class
/// `ENDCAP TOPLEFT`) reported `status: applied` with **4 endcaps** and exit 0, and
/// `place-tapcells --master` the same with **10 tapcells** — cells detailed placement would then
/// ignore, reported as a completed transformation. Upstream refuses all three commands.
///
/// 🔑 **The option name in the message is the CANONICAL one, not the user's spelling.** Upstream
/// reports `-left_top_corner` for a master supplied via `-corner`, because the check runs on the
/// resolved option struct. Invisible to every harness: `invalid_master_class` has an `.ok` and no
/// `.defok`, so no golden diff reaches it.
fn check_placeable(db: &Db, master: Option<&Master>, option: &str) -> Result<(), String> {
    let Some(m) = master else { return Ok(()) };
    if db.master_is_core_auto_placeable(&m.name) {
        return Ok(());
    }
    let class = db.master_get_type(&m.name).unwrap_or_default();
    Err(format!(
        "master {} with class {} given for {} cannot be placed in core rows and would be \
         ignored by detailed placement",
        m.name, class, option
    ))
}

fn read_master(db: &Db, name: &str) -> Result<Master, String> {
    let (w, h) = (
        db.master_get_width(name) as i32,
        db.master_get_height(name) as i32,
    );
    if w <= 0 || h <= 0 {
        return Err(format!(
            "master `{name}` is unknown or has no extent ({w} x {h})"
        ));
    }
    Ok(Master {
        name: name.to_string(),
        site: db.master_get_site(name),
        width: w,
        height: h,
        symmetry_x: db.master_get_symmetry_x(name),
        symmetry_y: db.master_get_symmetry_y(name),
    })
}

/// The rows this engine considers, and the core they sit in.
///
/// PAD-class rows are excluded: they are outside the core and are not part of the region standard
/// cells occupy.
fn read_rows(db: &Db) -> (boundary::Rect, Vec<tapcells::Row>) {
    let core = boundary::Rect::new(
        db.block_get_core_area_x_min(),
        db.block_get_core_area_y_min(),
        db.block_get_core_area_x_max(),
        db.block_get_core_area_y_max(),
    );
    // ⚠️ BY INDEX, never by name. Row names are NOT unique — `cut_rows` splits a row into pieces
    // whose names can collide with another family's — and the by-name accessors return the first
    // match, so a walk by name reads one row's geometry for another and silently loses the rest.
    // That produced phantom gaps in the region and thousands of spurious cells.
    let names = db.row_names().unwrap_or_default();
    let rows = (0..db.num_rows().unwrap_or(0))
        .filter_map(|i| db.nth_row(i).ok().flatten().map(|r| (i, r)))
        .filter(|(_, (_, site, _))| db.site_get_class(site).unwrap_or_default() != "PAD")
        .map(|(i, (bbox, site, orient))| tapcells::Row {
            name: names.get(i).cloned().unwrap_or_default(),
            site_width: db.site_get_width(&site),
            bbox: boundary::Rect::new(bbox[0], bbox[1], bbox[2], bbox[3]),
            site,
            orient,
        })
        .collect();
    (core, rows)
}

/// Create every planned cell. Physical-only, locked and marked as tool-placed, exactly as
/// upstream does — a tap that a later placer is free to move is not a tap.
fn apply(db: &mut Db, placements: &[Placement]) -> Result<usize, String> {
    for p in placements {
        db.create_physical_inst(&p.master, &p.name)
            .map_err(|e| format!("cannot create {}: {e}", p.name))?;
        // ORIENTATION FIRST, then location. odb pivots a mirrored cell about its origin, so
        // orienting after placing shifts it by its own width or height -- which looks like a
        // planner error and is not one. Upstream sets them in this order for the same reason.
        db.set_inst_orient(&p.name, &p.orient)
            .map_err(|e| format!("cannot orient {}: {e}", p.name))?;
        db.set_inst_location(&p.name, p.x, p.y)
            .map_err(|e| format!("cannot place {}: {e}", p.name))?;
        db.inst_set_placement_status(&p.name, "LOCKED")
            .map_err(|e| format!("cannot lock {}: {e}", p.name))?;
        db.inst_set_source_type(&p.name, "DIST")
            .map_err(|e| format!("cannot mark {}: {e}", p.name))?;
    }
    Ok(placements.len())
}

/// Open the database and check it has a usable scale — the preamble every verb shares.
fn open_scaled(path: &str) -> Result<(Db, f64), ExitCode> {
    let db = Db::open(path).map_err(|e| {
        eprintln!("vyges-tap: cannot read {path}: {e}");
        ExitCode::from(2)
    })?;
    let dbu = db.dbu_per_micron();
    if dbu <= 0 {
        eprintln!("vyges-tap: no DBU scale");
        return Err(ExitCode::from(2));
    }
    Ok((db, dbu as f64))
}

/// Write the database (and optionally a DEF), honouring --dry-run and --out-odb.
fn finish(db: &Db, path: &str, opts: &Opts, planned: usize, what: &str) -> ExitCode {
    let mut written = None;
    if !opts.dry_run {
        let out = opts.out_odb.clone().unwrap_or_else(|| path.to_string());
        if let Err(e) = db.write(&out) {
            eprintln!("vyges-tap: cannot write {out}: {e}");
            return ExitCode::from(2);
        }
        written = Some(out);
    }
    if let Some(def) = opts.out_def.as_deref() {
        if let Err(e) = db.write_def(def) {
            eprintln!("vyges-tap: cannot write {def}: {e}");
            return ExitCode::from(2);
        }
    }
    vyges_events::emit(
        &vyges_events::Event::new(
            "vyges-tap",
            vyges_events::Severity::Info,
            format!(
                "{} {} {}",
                if written.is_some() {
                    "inserted"
                } else {
                    "planned"
                },
                planned,
                what
            ),
        )
        .with_code("TAP-PLACED"),
    );
    println!(
        "{{\n  \"tool\": \"vyges-tap\",\n  \"status\": \"{}\",\n  \"{}\": {},\n  \"odb_written\": {}\n}}",
        vyges_tap::settle_status(opts.dry_run, planned),
        what,
        planned,
        match written.as_deref() {
            Some(p) => format!("\"{p}\""),
            None => "null".to_string(),
        }
    );
    ExitCode::SUCCESS
}

/// Options shared by the placement verbs.
#[derive(Debug, Default)]
struct Opts {
    odb: String,
    out_odb: Option<String>,
    out_def: Option<String>,
    dry_run: bool,
    /// Every `--flag value` pair seen, in order, so each verb can read its own.
    keys: Vec<(String, String)>,
}

fn parse_opts(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts::default();
    let mut odb = None;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "--dry-run" | "--json" => {}
            "--out-odb" | "--out-def" => {
                i += 1;
                let v = args
                    .get(i)
                    .cloned()
                    .ok_or_else(|| format!("{a} needs a value"))?;
                if a == "--out-odb" {
                    o.out_odb = Some(v);
                } else {
                    o.out_def = Some(v);
                }
            }
            a if a.starts_with("--") => {
                i += 1;
                let v = args
                    .get(i)
                    .cloned()
                    .ok_or_else(|| format!("{a} needs a value"))?;
                o.keys.push((a.trim_start_matches("--").to_string(), v));
            }
            a if a.starts_with('-') => return Err(format!("unknown option `{a}`")),
            a => odb = Some(a.to_string()),
        }
        i += 1;
    }
    o.dry_run = args.iter().any(|a| a == "--dry-run");
    o.odb = odb.ok_or("a path to a .odb is required")?;
    Ok(o)
}

impl Opts {
    fn get(&self, key: &str) -> Option<&str> {
        self.keys
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

fn place_tapcells(args: &[String]) -> ExitCode {
    let opts = match parse_opts(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("vyges-tap: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let (mut db, dbu) = match open_scaled(&opts.odb) {
        Ok(v) => v,
        Err(c) => return c,
    };
    let Some(master_name) = opts.get("master") else {
        eprintln!("vyges-tap: `place-tapcells` needs --master");
        return ExitCode::from(2);
    };
    let master = match read_master(&db, master_name) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("vyges-tap: {e}");
            return ExitCode::from(2);
        }
    };
    // Upstream `placeTapcells(Options)` checks the master straight after the null test and before
    // the distance default, and names the option `-master`.
    if let Err(e) = check_placeable(&db, Some(&master), "-master") {
        eprintln!("vyges-tap: {e}");
        return ExitCode::from(2);
    }
    let dist = match opts.get("distance") {
        Some(v) => match v.parse::<f64>() {
            Ok(um) => (um * dbu).round() as i32,
            Err(_) => {
                eprintln!("vyges-tap: --distance wants a number, got `{v}`");
                return ExitCode::from(2);
            }
        },
        None => 2 * dbu as i32,
    };

    let (core, rows) = read_rows(&db);
    let region = boundary::row_region(core, &rows.iter().map(|r| r.bbox).collect::<Vec<_>>());
    let edges: Vec<boundary::Edge> = boundary::classify(&region)
        .into_iter()
        .flat_map(|p| p.edges().copied().collect::<Vec<_>>())
        .collect();
    // Positional flags, not a set of names: row names are not unique, and a name-keyed set makes
    // one boundary row mark every other row that happens to share its name.
    let on_edge = tapcells::edge_rows(&edges, &rows, &master.site);

    // Fixed cells already in the design are obstacles; covers are not, since they are markers
    // rather than occupied area.
    let fixed: Vec<(boundary::Rect, ())> = db
        .inst_names()
        .into_iter()
        .filter(|n| db.inst_is_fixed(n) && !db.master_is_cover(&db.inst_master(n)))
        .map(|n| {
            (
                {
                    let b = db.inst_bbox(&n).unwrap_or_default();
                    if b.len() == 4 {
                        boundary::Rect::new(b[0], b[1], b[2], b[3])
                    } else {
                        boundary::Rect::new(0, 0, 0, 0)
                    }
                },
                (),
            )
        })
        .collect();

    let disallow_gaps = !db.has_one_site_master();
    let mut idx = 0usize;
    let mut planned = Vec::new();
    for (ri, row) in rows.iter().enumerate() {
        let in_row: Vec<boundary::Rect> = fixed
            .iter()
            .map(|(r, _)| *r)
            .filter(|r| r.y1 > row.bbox.y0 && r.y0 < row.bbox.y1)
            .collect();
        planned.extend(tapcells::place_in_row(
            row,
            &master,
            dist,
            on_edge[ri],
            disallow_gaps,
            &in_row,
            // Taps and endcaps have DIFFERENT default prefixes upstream: TAP_ and PHY_. They
            // are separate namespaces so `tapcell_ripup` can remove one without the other.
            opts.get("tap-prefix").unwrap_or("TAP_"),
            &mut idx,
        ));
    }

    if !opts.dry_run {
        if let Err(e) = apply(&mut db, &planned) {
            eprintln!("vyges-tap: {e}");
            return ExitCode::from(2);
        }
    }
    finish(&db, &opts.odb, &opts, planned.len(), "tapcells")
}

fn place_endcaps(args: &[String]) -> ExitCode {
    let opts = match parse_opts(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("vyges-tap: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let (mut db, _) = match open_scaled(&opts.odb) {
        Ok(v) => v,
        Err(c) => return c,
    };

    // Upstream resolves each position through a three-level fallback: the specific option, then
    // a grouped one, then the most general. `--endcap` therefore feeds the vertical caps as well
    // as the horizontal fill, and the inner-corner masters fall back to `--corner` rather than to
    // `--edge-corner`. Getting this chain wrong silently omits whole families of cells.
    let pick = |specific: &str, mid: &str, general: &str| -> Result<Option<Master>, String> {
        for k in [specific, mid, general] {
            if let Some(n) = opts.get(k) {
                return read_master(&db, n).map(Some);
            }
        }
        Ok(None)
    };
    let pick_many = |specific: &str, mid: &str, general: &str| -> Result<Vec<Master>, String> {
        for k in [specific, mid, general] {
            if let Some(v) = opts.get(k) {
                return v
                    .split([',', ' '])
                    .filter(|t| !t.is_empty())
                    .map(|n| read_master(&db, n))
                    .collect();
            }
        }
        Ok(Vec::new())
    };

    let build = || -> Result<EndcapMasters, String> {
        Ok(EndcapMasters {
            left_top_corner: pick("left-top-corner", "corner", "corner")?,
            right_top_corner: pick("right-top-corner", "corner", "corner")?,
            left_bottom_corner: pick("left-bottom-corner", "corner", "corner")?,
            right_bottom_corner: pick("right-bottom-corner", "corner", "corner")?,

            left_top_edge: pick("left-top-edge", "edge-corner", "corner")?,
            right_top_edge: pick("right-top-edge", "edge-corner", "corner")?,
            left_bottom_edge: pick("left-bottom-edge", "edge-corner", "corner")?,
            right_bottom_edge: pick("right-bottom-edge", "edge-corner", "corner")?,

            left_edge: pick("left-edge", "endcap-vertical", "endcap")?,
            right_edge: pick("right-edge", "endcap-vertical", "endcap")?,

            top_edge: pick_many("top-edge", "endcap-horizontal", "endcap")?,
            bottom_edge: pick_many("bottom-edge", "endcap-horizontal", "endcap")?,

            prefix: opts.get("prefix").unwrap_or("PHY_").to_string(),
        })
    };

    let mut masters = match build() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("vyges-tap: {e}");
            return ExitCode::from(2);
        }
    };

    // 🔑 CALL ORDER: upstream's `placeEndcaps` runs `checkPlaceable(options)` BEFORE
    // `correctEndcapOptions`, so only what the CALLER named is checked — a master the library
    // auto-selection supplies is deliberately not re-checked. Placing this after `autoselect`
    // would check a different set.
    let named: [(Option<&Master>, &str); 10] = [
        (masters.left_top_corner.as_ref(), "-left_top_corner"),
        (masters.right_top_corner.as_ref(), "-right_top_corner"),
        (masters.left_bottom_corner.as_ref(), "-left_bottom_corner"),
        (masters.right_bottom_corner.as_ref(), "-right_bottom_corner"),
        (masters.left_top_edge.as_ref(), "-left_top_edge"),
        (masters.right_top_edge.as_ref(), "-right_top_edge"),
        (masters.left_bottom_edge.as_ref(), "-left_bottom_edge"),
        (masters.right_bottom_edge.as_ref(), "-right_bottom_edge"),
        (masters.left_edge.as_ref(), "-left_edge"),
        (masters.right_edge.as_ref(), "-right_edge"),
    ];
    for (m, option) in named {
        if let Err(e) = check_placeable(&db, m, option) {
            eprintln!("vyges-tap: {e}");
            return ExitCode::from(2);
        }
    }
    for (list, option) in [
        (&masters.top_edge, "-top_edge"),
        (&masters.bottom_edge, "-bottom_edge"),
    ] {
        for m in list {
            if let Err(e) = check_placeable(&db, Some(m), option) {
                eprintln!("vyges-tap: {e}");
                return ExitCode::from(2);
            }
        }
    }

    // Anything the caller did not name is filled from the library's own master types. A library
    // that states nothing useful leaves those positions empty, and placement puts nothing there.
    let library = match db.masters_with_types() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("vyges-tap: cannot read the master library: {e}");
            return ExitCode::from(2);
        }
    };
    if let Err(e) = endcaps::autoselect(&mut masters, &library, |n| read_master(&db, n).ok()) {
        eprintln!("vyges-tap: {e}");
        return ExitCode::from(2);
    }

    let (core, rows) = read_rows(&db);
    let region = boundary::row_region(core, &rows.iter().map(|r| r.bbox).collect::<Vec<_>>());
    let classified = boundary::classify(&region);
    let mut idx = 0usize;
    let planned = endcaps::place_all(&classified, &rows, &masters, &mut idx);

    if !opts.dry_run {
        if let Err(e) = apply(&mut db, &planned) {
            eprintln!("vyges-tap: {e}");
            return ExitCode::from(2);
        }
    }
    finish(&db, &opts.odb, &opts, planned.len(), "endcaps")
}

/// Cut the rows, cap the boundary, then place the taps — upstream's `tapcell`, in one pass over
/// one database.
///
/// Not a shell script over the three verbs: the phases share a database in memory, the rows must
/// be re-read *after* cutting (that is the whole point of cutting them), and the physical-instance
/// counter runs across endcaps and taps together.
fn tapcell(args: &[String]) -> ExitCode {
    let opts = match parse_opts(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("vyges-tap: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    for dead in [
        "endcap-cpp",
        "tbtie-cpp",
        "no-cell-at-top-bottom",
        "disallow-one-site-gaps",
    ] {
        if opts.get(dead).is_some() || args.iter().any(|a| a == &format!("--{dead}")) {
            // Upstream warns and ignores these; refusing would block a ported script for no gain.
            vyges_events::emit(
                &vyges_events::Event::new(
                    "vyges-tap",
                    vyges_events::Severity::Warn,
                    format!("--{dead} is deprecated and ignored"),
                )
                .with_code("TAP-DEPRECATED-OPTION"),
            );
        }
    }
    let (mut db, dbu) = match open_scaled(&opts.odb) {
        Ok(v) => v,
        Err(c) => return c,
    };

    let master = |k: &str| -> Result<Option<Master>, String> {
        match opts.get(k) {
            Some(n) => read_master(&db, n).map(Some),
            None => Ok(None),
        }
    };
    let um = |k: &str| -> Result<Option<i32>, String> {
        match opts.get(k) {
            Some(v) => v
                .parse::<f64>()
                .map(|x| Some((x * dbu).round() as i32))
                .map_err(|_| format!("--{k} wants a number, got `{v}`")),
            None => Ok(None),
        }
    };

    /// Everything `tapcell` needs, gathered in one fallible pass so a bad master name is one
    /// message rather than a partly-built configuration.
    struct Gathered {
        flat: TapcellMasters,
        tap_master: Option<Master>,
        distance: Option<i32>,
        halo_x: Option<i32>,
        halo_y: Option<i32>,
        row_min: Option<i32>,
    }

    let gathered = (|| -> Result<Gathered, String> {
        Ok(Gathered {
            flat: TapcellMasters {
                endcap_master: master("endcap-master")?,
                tap_nwin2_master: master("tap-nwin2-master")?,
                tap_nwin3_master: master("tap-nwin3-master")?,
                tap_nwintie_master: master("tap-nwintie-master")?,
                tap_nwout2_master: master("tap-nwout2-master")?,
                tap_nwout3_master: master("tap-nwout3-master")?,
                tap_nwouttie_master: master("tap-nwouttie-master")?,
                cnrcap_nwin_master: master("cnrcap-nwin-master")?,
                cnrcap_nwout_master: master("cnrcap-nwout-master")?,
                incnrcap_nwin_master: master("incnrcap-nwin-master")?,
                incnrcap_nwout_master: master("incnrcap-nwout-master")?,
                endcap_prefix: opts.get("endcap-prefix").unwrap_or("PHY_").to_string(),
            },
            tap_master: master("tapcell-master")?,
            distance: um("distance")?,
            halo_x: um("halo-width-x")?,
            halo_y: um("halo-width-y")?,
            row_min: um("row-min-width")?,
        })
    })();
    let Gathered {
        flat,
        tap_master,
        distance,
        halo_x,
        halo_y,
        row_min,
    } = match gathered {
        Ok(v) => v,
        Err(e) => {
            eprintln!("vyges-tap: {e}");
            return ExitCode::from(2);
        }
    };

    // 🔑 CALL ORDER: upstream's `run()` opens with `checkPlaceable(options)` over all twelve
    // masters, BEFORE `cutRows`. Refusing here means a bad master never gets as far as cutting
    // rows, so the database is not half-modified by a run that is going to fail anyway. The option
    // names are upstream's Tcl spellings, which is what its message prints.
    let all_twelve: [(Option<&Master>, &str); 12] = [
        (flat.endcap_master.as_ref(), "-endcap_master"),
        (tap_master.as_ref(), "-tapcell_master"),
        (flat.cnrcap_nwin_master.as_ref(), "-cnrcap_nwin_master"),
        (flat.cnrcap_nwout_master.as_ref(), "-cnrcap_nwout_master"),
        (flat.tap_nwintie_master.as_ref(), "-tap_nwintie_master"),
        (flat.tap_nwin2_master.as_ref(), "-tap_nwin2_master"),
        (flat.tap_nwin3_master.as_ref(), "-tap_nwin3_master"),
        (flat.tap_nwouttie_master.as_ref(), "-tap_nwouttie_master"),
        (flat.tap_nwout2_master.as_ref(), "-tap_nwout2_master"),
        (flat.tap_nwout3_master.as_ref(), "-tap_nwout3_master"),
        (flat.incnrcap_nwin_master.as_ref(), "-incnrcap_nwin_master"),
        (flat.incnrcap_nwout_master.as_ref(), "-incnrcap_nwout_master"),
    ];
    for (m, option) in all_twelve {
        if let Err(e) = check_placeable(&db, m, option) {
            eprintln!("vyges-tap: {e}");
            return ExitCode::from(2);
        }
    }

    // ---- phase 1: cut the rows around placed macros ----
    let instances: Vec<Instance> = db
        .inst_names()
        .into_iter()
        .map(|name| Instance {
            is_block: db.inst_is_block(&name),
            is_placed: db.inst_is_placed(&name),
            name,
        })
        .collect();
    let blockages = select_blockages(&instances);
    let default = default_distance(dbu as i32);
    // 🔑 **The narrow-region floor, and it must not be zero here.** odb gates the check on
    // `min_row_height > 0`; a tapcell run passing 0 leaves regions between stacked blockages that
    // no endcap can ever fill. `ifp` passes 0 because it places no endcaps — see `min_row_height`.
    let min_height = min_row_height(
        flat.endcap_master.as_ref().map(|m| m.height),
        max_core_cell_height(&db),
        None,
    );
    if let Err(e) = db.cut_rows(
        min_row_width(flat.endcap_master.as_ref().map(|m| m.width), row_min),
        min_height,
        &blockages.cut_around,
        or_default(halo_x, default),
        or_default(halo_y, default),
    ) {
        eprintln!("vyges-tap: cannot cut rows: {e}");
        return ExitCode::from(2);
    }

    // ---- phase 2: cap the boundary. Rows are re-read HERE, after cutting. ----
    let mut masters = flat.to_positions();
    let library = match db.masters_with_types() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("vyges-tap: cannot read the master library: {e}");
            return ExitCode::from(2);
        }
    };
    if let Err(e) = endcaps::autoselect(&mut masters, &library, |n| read_master(&db, n).ok()) {
        eprintln!("vyges-tap: {e}");
        return ExitCode::from(2);
    }

    let (core, rows) = read_rows(&db);
    let region = boundary::row_region(core, &rows.iter().map(|r| r.bbox).collect::<Vec<_>>());
    let classified = boundary::classify(&region);
    // One counter across both phases: upstream numbers every physical instance in one sequence.
    let mut idx = 0usize;
    let endcap_cells = endcaps::place_all(&classified, &rows, &masters, &mut idx);

    // ---- phase 3: the taps ----
    // The endcaps are applied FIRST and then read back as obstacles, which is what upstream does
    // and the only way to get their extents right: a mirrored cell extends the other way from its
    // placement point, so computing `x .. x + width` puts the obstacle on the wrong side and the
    // taps walk straight through it. Applying mutates only the in-memory database; nothing is
    // written unless the caller asked for it.
    if let Err(e) = apply(&mut db, &endcap_cells) {
        eprintln!("vyges-tap: {e}");
        return ExitCode::from(2);
    }
    let obstacles: Vec<boundary::Rect> = db
        .inst_names()
        .into_iter()
        .filter(|n| db.inst_is_fixed(n) && !db.master_is_cover(&db.inst_master(n)))
        .filter_map(|n| {
            let b = db.inst_bbox(&n).unwrap_or_default();
            (b.len() == 4).then(|| boundary::Rect::new(b[0], b[1], b[2], b[3]))
        })
        .collect();

    let mut tap_cells = Vec::new();
    if let Some(tm) = tap_master {
        let dist = distance.unwrap_or(default);
        let edges: Vec<boundary::Edge> = classified
            .iter()
            .flat_map(|p| p.edges().copied().collect::<Vec<_>>())
            .collect();
        // Positional flags, not names — see the note in `edge_rows`.
        let on_edge = tapcells::edge_rows(&edges, &rows, &tm.site);
        let disallow_gaps = !db.has_one_site_master();
        let prefix = opts.get("tap-prefix").unwrap_or("TAP_");
        for (ri, row) in rows.iter().enumerate() {
            let in_row: Vec<boundary::Rect> = obstacles
                .iter()
                .copied()
                .filter(|r| r.y1 > row.bbox.y0 && r.y0 < row.bbox.y1)
                .collect();
            tap_cells.extend(tapcells::place_in_row(
                row,
                &tm,
                dist,
                on_edge[ri],
                disallow_gaps,
                &in_row,
                prefix,
                &mut idx,
            ));
        }
    }

    if !opts.dry_run {
        if let Err(e) = apply(&mut db, &tap_cells) {
            eprintln!("vyges-tap: {e}");
            return ExitCode::from(2);
        }
    }

    finish(
        &db,
        &opts.odb,
        &opts,
        endcap_cells.len() + tap_cells.len(),
        "cells",
    )
}

/// Remove what a previous run inserted, so `tapcell` can be re-run with different parameters.
///
/// Taps and endcaps are removed separately because they carry different prefixes and a caller may
/// want only one gone. Nothing else identifies these cells — they are physical-only instances
/// with no nets — so the prefix is the whole contract, and an empty one removes nothing.
fn ripup(args: &[String]) -> ExitCode {
    let opts = match parse_opts(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("vyges-tap: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let mut db = match Db::open(&opts.odb) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("vyges-tap: cannot read {}: {e}", opts.odb);
            return ExitCode::from(2);
        }
    };

    let names = db.inst_names();
    let taps = cells_with_prefix(&names, opts.get("tap-prefix").unwrap_or("TAP_"));
    let endcaps = cells_with_prefix(&names, opts.get("endcap-prefix").unwrap_or("PHY_"));

    if !opts.dry_run {
        for n in taps.iter().chain(endcaps.iter()) {
            if let Err(e) = db.destroy_inst(n) {
                eprintln!("vyges-tap: cannot remove {n}: {e}");
                return ExitCode::from(2);
            }
        }
    }

    for (n, what, code) in [
        (taps.len(), "tapcells", "TAP-RIPUP-TAPS"),
        (endcaps.len(), "endcaps", "TAP-RIPUP-ENDCAPS"),
    ] {
        vyges_events::emit(
            &vyges_events::Event::new(
                "vyges-tap",
                vyges_events::Severity::Info,
                format!("Removed {n} {what}."),
            )
            .with_code(code),
        );
    }

    finish(&db, &opts.odb, &opts, taps.len() + endcaps.len(), "removed")
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // 🔑 **The commit, not just the version.** Two binaries can share a version and differ by a
    // fix, so a bug report needs the build. build.rs prefers GITHUB_SHA on CI, which is what stops
    // a release being stamped -dirty by the untracked files a release run leaves behind.
    //
    // ⚠️ Answered before --describe, --help and any argument parsing: asking a binary what it is
    // must not depend on the rest of the command line being valid.
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("vyges-tap {} ({})", vyges_tap::VERSION, env!("VYGES_GIT_SHA"));
        println!("{}", vyges_tap::COPYRIGHT);
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "--describe") {
        print!("{}", describe());
        return ExitCode::SUCCESS;
    }
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return if args.is_empty() {
            ExitCode::from(2)
        } else {
            ExitCode::SUCCESS
        };
    }
    match args[0].as_str() {
        "cut-rows" => cut_rows(&args[1..]),
        "boundary" => boundary_report(&args[1..]),
        "place-tapcells" => place_tapcells(&args[1..]),
        "place-endcaps" => place_endcaps(&args[1..]),
        "ripup" => ripup(&args[1..]),
        "tapcell" => tapcell(&args[1..]),
        other => {
            eprintln!("vyges-tap: unknown command `{other}`\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_descriptor_is_valid_json_and_declares_what_is_missing() {
        let d: serde_json::Value = serde_json::from_str(DESCRIBE).expect("valid JSON");
        assert_eq!(d["name"], "tap");
        assert_eq!(
            d["maturity"], "structured",
            "every command is implemented and measured against the goldens"
        );
        let limits = d["provenance_limitations"].as_array().expect("an array");
        // What is still missing has to be stated, or "tap ran" reads as "tap is done".
        // The one thing a caller must not get wrong about rip-up.
        assert!(
            limits.iter().any(|l| l
                .as_str()
                .unwrap_or("")
                .contains("EMPTY prefix removes nothing")),
            "the descriptor must state the empty-prefix guard"
        );
        // ...and the measured result must be stated, not implied.
        assert!(
            limits
                .iter()
                .any(|l| l.as_str().unwrap_or("").contains("MEASURED")),
            "the descriptor must state what was measured"
        );
    }

    #[test]
    fn microns_convert_by_rounding() {
        assert_eq!(to_dbu(2.0, 1000.0), 2000);
        assert_eq!(to_dbu(0.0155, 1000.0), 16);
    }

    #[test]
    fn the_odb_argument_is_required_and_options_are_checked() {
        assert!(parse_args(&[]).is_err());
        assert!(parse_args(&["d.odb".to_string()]).is_ok());
        let bad = ["d.odb", "--halo-x", "wide"].map(String::from);
        assert!(parse_args(&bad).expect_err("refuses").contains("--halo-x"));
        let missing = ["d.odb", "--halo-y"].map(String::from);
        assert!(parse_args(&missing)
            .expect_err("refuses")
            .contains("--halo-y"));
    }

    #[test]
    fn the_unimplemented_commands_are_named_rather_than_reported_as_unknown() {
        // A caller who asks for `tapcell` should be told it is missing, not that it does not
        // exist — the difference decides whether they go looking for a typo.
        for cmd in ["tapcell", "place-endcaps", "place-tapcells"] {
            assert!(USAGE.contains("cut-rows"));
            assert_ne!(cmd, "cut-rows");
        }
    }
}

#[cfg(test)]
mod pin_tests {
    use super::{describe, PIN_TOKEN};

    #[test]
    fn the_descriptor_reports_the_pin_this_binary_was_built_against() {
        let d = describe();
        assert!(
            !d.contains(PIN_TOKEN),
            "the pin placeholder survived into the output -- the substitution did not run"
        );
        let v: serde_json::Value =
            serde_json::from_str(&d).expect("the descriptor is still valid JSON once filled in");
        assert_eq!(
            v["openroad_pin"], super::CRATE_PIN,
            "the descriptor must report the pin this binary was actually built against"
        );
        assert_eq!(super::CRATE_PIN.len(), 40, "a full commit SHA, not an abbreviation");
    }

    /// ⛔ The whole point of inheriting the pin is that no engine carries one of its own.
    #[test]
    fn no_sha_is_hardcoded_anywhere_in_the_descriptor() {
        let raw = super::DESCRIBE;
        for tok in raw.split(|c: char| !c.is_ascii_hexdigit()) {
            assert!(
                tok.len() < 40,
                "{tok} looks like a hardcoded commit -- use the {PIN_TOKEN} placeholder"
            );
        }
    }
}
