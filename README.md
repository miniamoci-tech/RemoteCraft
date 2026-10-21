# RemoteCraft

RemoteCraft is a clean-room, open-source remote-desktop tool written in Rust.
It lets one person see and control another computer's desktop over the network,
in the same spirit as AnyDesk or TeamViewer, with a self-hosted relay so no
third-party service is involved.

It is built for a concrete job: letting a colleague working from home see and
control an office PC. Both machines run the same desktop app; a small **relay
server** that you host (on Linux or on a Windows PC in the office LAN) connects
them by a short numeric ID, so neither PC needs an inbound port opened.

> Status: **early development.** The relay server works and is tested. The
> desktop host/client app is next (see Roadmap).

## Components

| Component | Crate | Runs on | Purpose |
|-----------|-------|---------|---------|
| Relay server | `remotecraft-relay` | Linux, Windows, macOS | Rendezvous + byte relay between two peers, addressed by numeric ID. Stores nothing, inspects nothing. |
| Protocol | `remotecraft-core` | — | Shared wire framing and control messages. |
| Desktop app | `remotecraft` *(planned)* | Windows, macOS | Host (share this screen) and viewer (control a remote screen) in one app. |

## How it will work (target design)

- **Two roles, one app.** The PC being controlled runs as **host**; the person
  connecting runs as **viewer**. Each PC can do either.
- **Attended access** ("accesso vigilato"): the host shows an ID and a one-time
  password. The person at the host reads them out; the viewer types them in.
- **Unattended access** ("accesso non vigilato"): the host owner sets a fixed
  password in advance; the viewer connects any time with the ID and that
  password. No one needs to be at the host.
- **Screen + input**: the viewer sees the host's screen and controls its mouse
  and keyboard.
- **Remote audio**: sound playing on the host (e.g. a YouTube video, a recorded
  council meeting) is captured and streamed to the viewer, so the person at home
  hears what the office PC plays.
- **Encryption**: the two peers establish an end-to-end encrypted channel; the
  relay only ever forwards opaque bytes and cannot read the session.

## The relay server

The relay is the only component that needs a reachable address. Both desktop
peers dial *out* to it, so it works across different home/office routers without
port-forwarding on either desktop.

```bash
# Build
cargo build -p remotecraft-relay --release

# Run (defaults to 0.0.0.0:21118)
./target/release/remotecraft-relay
REMOTECRAFT_RELAY_BIND=0.0.0.0:21118 ./target/release/remotecraft-relay
./target/release/remotecraft-relay --bind 0.0.0.0:21118
```

You can run it two ways for this use case:

1. **On a Linux box** with a public address (a small VPS, or an office machine
   with a forwarded port). See [`deploy/linux/`](deploy/linux/).
2. **On a Windows PC in the office LAN**, if the viewer reaches the office
   network over a VPN. See [`deploy/windows/`](deploy/windows/). The relay is a
   single `.exe` with no dependencies.

The relay keeps no logs of session contents, stores nothing on disk, and never
inspects the bytes it forwards.

## Building

Requires a recent stable Rust toolchain.

```bash
cargo test --workspace        # run the tests
cargo build --release         # build everything
```

On Windows, if you build from a redirected/sandboxed folder and hit linker
error LNK1104, point the build output at a plain path:
`CARGO_TARGET_DIR=C:/Users/you/rc-target cargo build --release`.

## Roadmap

- [x] M0 — Relay server: registration, ID assignment, session brokering, opaque
  byte relaying, with end-to-end tests.
- [x] M1 — Networking layer (`remotecraft-net`): a relay client (register,
  pair, send/recv) plus a password-authenticated, end-to-end encrypted channel
  (Noise `NNpsk0`). Demonstrated by `remotecraft-probe host|connect`.
- [x] M4 (channel) — Attended (random one-time) and unattended (fixed) password
  authentication and E2E encryption of the peer channel. Wrong password fails
  the handshake; the relay only ever sees ciphertext.
- [ ] M2 — Screen capture + frame streaming; viewer window (GUI app).
- [x] M3 — Mouse + keyboard control: the viewer forwards pointer moves,
  clicks, scroll, typed text and shortcuts; the host injects them (enigo).
  Coordinates are normalized, so it is resolution-independent. On macOS the
  host needs the "Accessibility" permission.
- [x] M6 — File transfer: the viewer browses the host's folders and downloads
  files (e.g. a recording made on the office PC) to its own
  `~/RemoteCraft-Downloads`, streamed in chunks over the encrypted channel.
  (Host→viewer download; viewer→host upload is a later addition.)
- [ ] M5 — Remote audio: capture host system playback, stream to viewer.
- [ ] M7 — Packaging: signed-where-possible Windows setup `.exe` and macOS
  `.dmg`, built in CI.

### Security note

The password is mixed into the Noise handshake as a pre-shared key, so the two
peers authenticate each other and all traffic is end-to-end encrypted; the relay
cannot read it. A future milestone will upgrade password authentication to a
PAKE (e.g. SPAKE2/OPAQUE) to also resist offline guessing by a party that
captures a handshake. Until then, use a strong unattended password.

## Licence

MIT.

## Safety

Everything RemoteCraft reads from the network is untrusted input. The code
follows a few hard rules so a malformed or hostile peer can never crash a PC or
be used to attack it:

- No `unwrap`/`expect`/`panic!` in product code (only in tests); errors are
  returned as `Result` and propagated.
- `unsafe_code = "forbid"` across the workspace.
- Every length read from the wire is bounded (`MAX_FRAME_LEN`) before any
  allocation, so a remote peer cannot force a huge buffer.
- Lock poisoning is handled; one bad connection never takes the relay down.
- Every crash fix ships with a synthetic regression test.
