# Gamepads

Gamepads are read with gilrs. On Windows, Xbox-style pads come through Windows.Gaming.Input with their sticks and buttons already laid out; other pads are mapped with SDL's controller database. Buttons are bound per action in **Esc → Controls** (`[controls.gamepad]`), the D-pad and left stick move, and with local multiplayer each pad plays as the player set under **Controls → Players**.

## The left stick

The game is played with the Wii Remote held sideways, so its movement is the remote's D-pad: eight directions. By default the stick gives those (`crates/rumble-port/src/controls.rs`, `stick_direction`):

- It counts once pushed 30% of its reach, measured as a circle, and stops once back within 22%, so it doesn't flicker at the edge.
- Its angle picks one of eight equal 45-degree directions. Within 7 degrees past a direction's edge it keeps that direction, so it doesn't flicker between neighbours.

## True stick controls

With **Controls → Left stick → True stick controls** (`[controls] true_stick`), the stick steers in any direction, and how far it is pushed sets the speed.

The game has this itself, for the Classic Controller: its player controller (`0x8006f630` in the Weekend DOL) reads, when a remote has one attached, the Classic Controller's left stick and turns it into an angle and a strength (`0x800526d0`). So with the setting on, the Wii Remote replacements (`hle/wpad.rs`) report a Classic Controller on every remote, with the pad's stick (a 15% dead zone at the centre, the rest stretched to the full range). The buttons bound to the remote are passed as their Classic Controller equivalents, which the game reads the same way: 2 as a, 1 as b, A as X, the D-pad as it lies (the remote's is turned), and +, − and Home as they are. The remote's B, which the game doesn't use, becomes ZL. Online joiners send their stick with their buttons.

The game also reads a Nunchuk's stick (`0x8006f360`) and a GameCube controller's (`0x8006f0d0`) the same way.

## Drift

Some pads made the player walk up and to the left on their own as soon as they were connected. Pads Windows doesn't know as Xbox-style ones have their axes mapped by index, and on those a trigger, which rests at -1 or +1, can land on `LeftStickX`/`LeftStickY` or `DPadX`/`DPadY`, and read as a direction held forever.

`CentredAxes` ignores each direction axis of each pad until it has read near zero. A real stick rests at centre, so it works at once; an axis stuck at an end gives no movement. A pad that reconnects starts over.
