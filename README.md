# vyges-tap

Row cutting and physical-cell insertion over the OpenDB design database.

All five commands are implemented and checked against the upstream goldens: `cut-rows`,
`place-endcaps`, `place-tapcells`, the combined `tapcell`, and `ripup`.

```text
vyges loom tap cut-rows design.odb --endcap-master TAPCELL_X1 --halo-x 2 --halo-y 2
```

## What `cut-rows` does

Placed macros block standard-cell rows. This cuts every row that crosses one, leaving a keep-out
halo around it, and discards the fragments left too narrow to be useful.

- **Blockages are placed macros.** A macro that has not been placed yet is skipped and *named*
  (`TAP-0032`), never silently ignored — otherwise rows would be left crossing wherever it
  eventually lands and nothing would say why.
- **The halo** defaults to 2 µm in each direction, converted through the database's own scale.
- **The minimum row width** is the larger of two endcap widths and any `--row-min-width` given, so
  a caller's floor cannot quietly produce rows too narrow to cap at both ends.

## What is delegated, and why that is the point

The cutting itself is **`odb::cutRows`**, from `odb/util.h`. It is not reimplemented here.

That is a deliberate line, not a shortcut. OpenDB is the substrate this whole engine suite is built
on; `cutRows` is odb's own algorithm operating on odb's own rows, so reimplementing it would mean
reimplementing part of the thing we chose to build on. What belongs to the engine is the *policy* —
which instances count as blockages, what the halo and minimum width are, and what to say about a
macro it had to skip. That policy lives in this crate and is unit-tested without a database; the
cutting goes straight to odb.

The same reasoning covers `odb::hasOneSiteMaster` and `odb::makeSiteLoc`.

## Correctness — measured, not claimed

Checked against the upstream `tap` regression goldens at pin
`945a9f48dc6e5cc91d865daa92c45a1094cb682c` (`vyges-openroad 2026.08.0`), re-measured 2026-08-23.

⚠️ **A correlation result is a statement about one upstream commit.** This one was exact at the
previous pin and is not at this one — see the table.

Unlike a floorplan, this engine prints almost nothing — four message codes in the whole upstream
module — so its goldens are **DEF, not logs**. Conformance therefore diffs the DEF `ROW` section
against each case's `.defok`: every row's name, site, origin, orientation and step.

| | |
| --- | --- |
| **10 pass** | every golden `ROW` line identical — every case whose row cutting we can drive |
| **0 fail** | |
| **28 not comparable** | 26 need `tapcell`/`place_endcaps`/`place_tapcells`; 2 have no `.defok` |

**Ten of ten is not ten of thirty-eight.** The implemented subset is exact, and the subset is
small. The suite does not really open up until endcap placement exists.

## Tapcells and endcaps

`place-tapcells` places well taps along each row at a fixed pitch — twice as densely on rows that
touch the top or bottom boundary, staggered between neighbouring rows, sliding past anything fixed
that is already there.

`place-endcaps` caps the boundary: a cell at every corner, and edge cells filling the stretches
between them. Which master goes where follows from the corner or edge type and the orientation of
the row it lands in.

| | |
| --- | --- |
| `cut_rows` | **10 of 10** comparable cases exact |
| endcaps | **9 of 9** exact |
| combined `tapcell` | ⚠️ **19 of 20** — was 16 of 16 at pin `b5624809` |

Every physical cell is compared — name, master, position, orientation — plus the DEF `ROW` lines.
The largest case places 22,844 cells.

**Where it stands.** Three fixes have tracked upstream's rework: the odb narrow-region cut height,
row occupancy persisting across polygons and holes, and corner displacement. One case remains —
`abutting_macros_step_no_corners`, where four cells sit at the right coordinates under the wrong
edge kind: a die-corner position upstream attributes to the horizontal edge and this engine to the
vertical one. Cell count, positions and rows all match.

**Why combined `tapcell` regressed.** Between the two pins upstream changed endcap and tapcell
placement across five commits — `src/tap` moved by 32 files, +8,323/−243 — and this engine has not
tracked those changes. Six cases differ; **four of them are new upstream cases**, and all six have
changed goldens, so this is upstream behaviour not yet implemented rather than a defect introduced
here. Both the instance naming and the orientation moved:

```text
ours      PHY_EDGE_ROW_0_Left        TAPCELL_X1  0 0  N
upstream  PHY_EDGE_CORE_ROW_0_1_Left TAPCELL_X1  0 0  FS
```

⟹ Treat combined `tapcell` output as **not correlated at this pin**. `cut_rows` and endcap
placement are unaffected and remain exact.

Instance *names* match except for the trailing counter: upstream numbers physical cells in the
order Boost hands back polygon vertices, an arbitrary starting vertex rather than a stated rule.

## Rip-up

`ripup` removes what a previous run inserted, so `tapcell` can be re-run with different parameters.
It matches by **name prefix** — the only mark these cells carry, since they are physical-only
instances with no nets. Taps and endcaps have separate prefixes (`TAP_`, `PHY_`) so one can be
removed without the other.

**An empty prefix removes nothing.** Every name starts with the empty string, so the literal
reading would delete every instance in the design. That guard is the difference between undoing a
tap step and destroying the netlist.

Positions you do not name are filled from the library's own LEF58 master types. Two masters
claiming one position is an **error naming both**, not a coin flip: a wrong endcap is a well-tie
fault nobody sees until silicon. A position nothing fills stays empty and places nothing.

Taps and endcaps use different default prefixes — `TAP_` and `PHY_` — because they are separate
namespaces that can be ripped up independently.

Instance *names* match except for the trailing counter: upstream numbers physical cells in the
order Boost hands back polygon vertices, which is an arbitrary starting vertex rather than a stated
rule, so reproducing it would mean reverse-engineering a library's internals.

## The boundary classification

`vyges loom tap boundary design.odb` reports the row region — how many connected pieces, how many
holes, and the census of corner types. Endcap placement is driven entirely by that classification,
so being able to see it before any cell is placed is what makes it checkable.

The goldens name every corner cell for the corner type that produced it
(`PHY_CORNER_ROW_0_OuterBottomLeft_0`), so they state upstream's classification outright. Against
those: **3 cases match exactly** — including one exercising all seven corner types on a polygon
boundary with notches, and one where five macros produce exactly five of each inner corner type.

Five more are **unresolved, not failing**: the golden counts corner cells *placed*, and upstream
places one only where a row actually reaches the corner. Having more corners than the golden has
cells is expected there. No case is missing a corner the golden placed — which is the direction
that would mean the classification is wrong.

## Exit status

| | |
| --- | --- |
| `0` | applied — rows were cut and the database written |
| `1` | refused — the design cannot be processed as asked |
| `2` | error — usage, unreadable database, no DBU scale, or a failed write |

Asking for `tapcell`, `place-endcaps` or `place-tapcells` exits `2` with a message saying the
command is *not implemented* rather than *unknown* — the difference decides whether a caller goes
looking for a typo.

## Building

```text
cargo build --release
cargo test
```

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
