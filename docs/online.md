# Online multiplayer

Work in progress. The host runs the game; players on other PCs join it as players 2–4 with the client app. They see and hear the host's game, and send their keyboard and gamepad over the network. They need no game files.

## Hosting

Set **Esc → Game → Multiplayer** to **Online**. That also turns on everything local multiplayer does (see [multiplayer.md](multiplayer.md)), so this PC's gamepads can play alongside online players. Then:

- **Your name:** shown to the players who find your session. It is saved as `[online] name`.
- **Port:** 7777 by default, saved as `[online] port`.
- **Password:** players need it to join. It is asked each time and never saved.
- **Start hosting.** The panel then shows the address to give friends on your network (this PC's LAN address and the port), and the players who have joined. With nobody else in yet, it shows "Waiting for players…".

Toasts at the top of the screen say when someone joins ("Ash joined as Player 2"), leaves, loses their connection, or tries the wrong password.

Players from outside your network connect to your public IP address, so the router must forward the port (TCP) to this PC. Hosting asks the router to do that with UPnP: the panel says "Opening the port on your router…", then "Port opened on your router" with the public address to give friends. The mapping lasts an hour, is renewed every 30 minutes while hosting, and is removed when hosting stops. If no UPnP router answers within 3 seconds, or it refuses, the panel says how to forward the port by hand instead.

## Leaving and ending multiplayer

The game stops at "Connection with the Wii Remote has been interrupted" when a playing remote goes away, and waits for it to come back. So when a player leaves, online or by unplugging a local pad, the port keeps that remote connected but idle. The game carries on, and that player's Pokémon stands still. The next player to join takes over the seat.

To continue alone, open the game's pause menu (+) in the hub and choose **End Multiplayer**, then pick your Pokémon. **Esc → Game → End multiplayer**, then the pause menu's **Multiplayer**, does the same. The entry is the game's own: while players 2–4 are in, the pause menu's list widget (`0x801dd850`) gets it after Multiplayer when the pause menu calls it, with text from one of `main.bmg`'s unused messages (0x70, "????", rewritten as the file is unpacked). Chosen, it returns Multiplayer's action, so the selection screen opens, and tells the host to end multiplayer there (`hle/pause.rs`). The port recognizes the game's Pokémon selection screen: its player panels are the layout with panes `HELP0`, `PIISPEC` and `PLAYERID` (`dlgpiisel_p_root`, watched in `hle/lyt.rs`). While that screen shows, players 2–4 are disconnected; a remote going away there doesn't stop the game. Online players are told "Sarg ended multiplayer" and dropped, the session stays open, and local pads for players 2–4 stay out until the screen closes. With only player 1 picking, the game carries on as usual.

## Joining

Open the client app: `Pokemon Rumble Client.exe`, which setup creates next to the game with every build (send it to whoever wants to join; it needs no game files), or `rumble-port client`. Any exe whose name contains "client" opens it.

1. Enter **your name**, the **host** (`ip:port`), and the **picture** quality you want: 720p, 1080p, 1440p or 4K. Then click **Connect**. All three are remembered.
2. It shows "Sarg's multiplayer session found (1 player). Would you like to connect?". **Yes** asks for the password, then **Join**.
3. You then see the host's game, scaled to fit the window. F11 toggles fullscreen. You hear its sound and play with your keyboard and gamepads, using the bindings in `config.toml`. Esc opens a menu with **Leave session**; the game keeps going meanwhile.

### Name tags

While hosting, each player's name (the host's included) is over their health bar, in the space right of the P1–P4 badge, on a plate: the gauge's own frame, mirrored up over it and as wide as the name. The name is in the bar's font, in the deep colour of the player's badge (red, blue, yellow, green). The game draws it, so it moves with the bar exactly and joiners see it in the picture.

The health bar layout (`hp_player.brlyt`) has a text box the game keeps hidden, `LEVEL`. As the layout is unpacked it is given room for a name; while the game draws, the visible badge (`P0_P` to `P3_P`) says whose bar it is, and `LEVEL`'s draw is replaced by two: the gauge with its fill hidden and its matrix mirrored and narrowed, then the box with the name, its matrix worked out from its parent's (`hle/nametag.rs`). A long name is narrowed to fit the gauge's width.

### Picture and sound

The host draws its finished frame again, offscreen, at the size the joiners asked for. The height is the tallest any joiner chose, and the width follows the host's picture shape. The host raises its internal resolution to match, so a 4K joiner gets a real 4K picture even from a 1080p host. The frame is read back and encoded on a thread of its own, and the same picture goes to every joiner. A frame is skipped while the encoder is busy.

The picture is H.264 when every joiner can decode it (`rumble-port/src/video.rs`, with Windows' Media Foundation). The host uses the graphics card's encoder ("NVIDIA H.264 Encoder MFT", say), or Microsoft's software one without it, set for low latency: constant bitrate (about 12 Mbit/s at 1080p, at most 50), no B-frames, a key frame every 2 seconds. The host's terminal says which ("streaming H.264 1920x1080 with …"). Frames are converted to NV12 (BT.709) on several threads. The joiner decodes with Microsoft's H.264 decoder in low-latency mode; every frame is decoded, in order, but only the newest of those waiting becomes the picture. Otherwise, if a joiner can't decode H.264 or the host has no encoder, pictures are JPEG (quality 80), as the console test client asks for. The game's mixed sound goes to every joiner as 16-bit stereo samples, with the host's own volume mix. The joiner applies its own master volume.

Each joiner has its own sending thread and a queue of 64 messages, so the game and the other joiners never wait for a slow connection. A JPEG picture it hasn't sent yet is replaced by the next. H.264 frames each build on the one before, so they all go through the queue: when one doesn't fit, that joiner gets no more until a key frame, and the encoder is asked for one at once. A new joiner gets a key frame too. JPEG needs tens of Mbit/s at 1080p; H.264 about 12.

## Joining from the console (test client)

```bash
rumble-port join 192.168.1.20:7777
```

It asks for your name, then shows "Sarg's multiplayer session found (1 player). Would you like to connect?", then asks for the password. Once you've joined, this PC's gamepads play as your player. Press Enter to leave.

## Protocol (`rumble-port/src/net.rs`)

Everything goes over one TCP connection, as length-prefixed frames whose first byte says what follows.

1. **Hello.** The client sends `RMBL` and the protocol version. The host answers in the clear with how many players are in and its name, or refuses another version.
2. **Password.** Once the player says yes and types the password, both sides run SPAKE2 (`spake2` crate, Ed25519 group) with it. The password itself never crosses the network, and someone watching can't test guesses offline.
3. **Sealed messages.** From then on, messages are sealed with ChaCha20-Poly1305 under the SPAKE2 key. The nonce is the direction plus a counter per direction, so a message can't be replayed or reordered. With the wrong password, the client's first sealed message doesn't open. The host waits a second, then refuses in the clear with "wrong password".
4. **Join.** The client sends its name (at most 16 characters), the picture height it wants, and whether it can decode H.264. The host refuses an empty name, one in use (the host's included, ignoring case), or a full game. Otherwise it welcomes the client as the lowest player 2–4 that neither this PC's gamepads nor another online player uses.
5. **Play.** The client sends its Wii Remote buttons every frame, and the host gives them to that player's remote. The host sends pictures (H.264 frames, with their size and whether each is a key frame, or JPEGs), sound and toasts (joins and leaves) to every joiner. This doesn't depend on the host window having focus. A client silent for 5 seconds has lost its connection. Its leaving frees the player.

