# Gamepad drift (stick axes resting off-centre)

Some pads made the player walk up and to the left on their own as soon as
they were connected.

On Windows, gilrs reads pads that Windows doesn't know as Xbox-style ones
through `RawGameController`, and maps their axes by index. On those pads a
trigger, which rests at -1 or +1, can land on `LeftStickX`/`LeftStickY` or
`DPadX`/`DPadY`. The port then read that axis as a direction held forever.

`crates/rumble-port/src/controls.rs` (`CentredAxes`) now ignores each
direction axis of each pad until it has read near zero (within the stick
deadzone, or 0.5 for the D-pad axes). A real stick rests at centre, so it
works at once; an axis stuck at an end gives no movement. A pad that
reconnects starts over.

If the stick still does nothing on such a pad, its stick is on other axes:
the D-pad and the `[controls.gamepad]` buttons still work, and
`stick = false` turns the stick off entirely.
