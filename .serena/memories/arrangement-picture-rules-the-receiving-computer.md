# The picture is the adjacency for the receiving computer; the input computer's OS layout is physical

`inherit_display_edges` (transport `display_arrangement.rs`) adds each computer's own desktop
seams from OS geometry, except where `placed` gives a picture position: `InspectedPeer::topology`
passes free-mode `arrangement.positions` for the displays of the computer that is NOT the source,
so in "Place each display" mode the receiving computer's displays touch exactly as drawn (its
cursor only goes where MonHop injects it). The input computer always keeps its OS seams: its real
cursor moves by its own OS, so a crossing on an edge its desktop already routes is still refused
("already leads to … on the same computer", mirrored by `validateLayout` checking only
`ownSeams(all.source)` in `sharing-model.mjs`). Grouped mode is unchanged (picture == OS).
Why: the user placed the Mac's Built-in below the Windows display while macOS stacks the the external display above
the Built-in; the old rule refused that legal picture. Tests: `placed_positions_replace_a_computers_own_geometry`
and `placed_one_by_one_the_picture_rules_the_other_computers_adjacency`.
