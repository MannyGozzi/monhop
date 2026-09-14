# A display leaves the picture when the user marks it not in use; identity only picks the default

`hidden` in the arrangement (UI `layout.hidden`, native `ArrangementRequest.hidden`) is the exact
list of displays marked not in use, on either computer, whatever monitor they are on. The Arrange
step shows one switch per display ("Displays in use", `arrangement-view.mjs` `renderUse`,
model `setDisplayInUse` / `displayUseChoices` in `sharing-model.mjs`). The only rules: a computer
keeps at least one display in use, and a hidden display is never the starting display or a link
end (`validate_arrangement` in `apps/monhop-desktop/src/sharing.rs`, mirrored in
`validateLayout`). Since this change the validator no longer needs a shown twin on the other side.

Monitor identity (`monitor` = vendor-product-serial from EDID; Mac `platform-macos/native.rs`,
Windows since 84c54ab registry `Device Parameters\EDID` in `platform-windows/displays.rs`) only
chooses the default: `hiddenDisplays(source, destination, null)` hides the other computer's copy
of each monitor cabled to both, and the "shows on" segmented control flips that pair in one
gesture. Once placed, the list is pinned in `layout.hidden` and travels with the layout.

Wire and disk: `layoutArrangement` and serde always write `hidden`, even empty, so a layout that
shows every display restores that way. A layout without the key predates the switch; the UI infers
which copy it used from its links (`hiddenFromLayout`) and hides the twin.

Why hiding matters: `inherit_display_edges` adds each computer's own desktop seams for every
visible display, so a visible copy of a monitor that is really showing the other computer keeps
an OS seam that collides with a crossing on that edge (`Topology::new` → `OverlappingSourceLinks`,
"The display layout is invalid."). `ownSeams` in the UI names that conflict before Apply.
