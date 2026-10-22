# RemoteCraft

RemoteCraft is a clean-room, open-source remote-desktop tool written in Rust.
It lets one person see and control another computer's desktop over the network,
in the same spirit as AnyDesk or TeamViewer, with a self-hosted relay so no
third-party service is involved.

It is built for a concrete job: letting a colleague working from home see and
control an office PC. Both machines run the same desktop app; a small **relay
server** that you host (on Linux or on a Windows PC in the office LAN) connects
them by a short numeric ID, so neither PC needs an inbound port opened.

> Status: **working end to end.** The relay, the encrypted host/viewer
> session, screen sharing, mouse/keyboard control, remote audio and file
> download all work, and a graphical launcher lets a non-technical user pick a
> role without the command line (see Roadmap).

## Components

| Component | Crate | Runs on | Purpose |
|-----------|-------|---------|---------|
| Relay server | `remotecraft-relay` | Linux, Windows, macOS | Rendezvous + byte relay between two peers, addressed by numeric ID. Stores nothing, inspects nothing. |
| Protocol | `remotecraft-core` | — | Shared wire framing and control messages. |
| Desktop app | `remotecraft` | Windows, macOS | Host (share this screen) and viewer (control a remote screen) in one app, with a graphical launcher. |

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

## Packages & releases

CI builds the packages on GitHub Actions:

- **`CI`** (`.github/workflows/ci.yml`) runs `fmt`, `clippy -D warnings` and the
  tests on every push and PR to `main`.
- **`Release packages`** (`.github/workflows/release.yml`) builds a Windows
  setup `.exe` (Inno Setup) + portable zip, and a universal macOS `.dmg`.

To cut a release, push a version tag; the installers are built and attached to
the GitHub release automatically:

```bash
git tag v0.1.0
git push origin v0.1.0
```

You can also trigger the workflow manually from the repo's **Actions** tab
("Release packages" → "Run workflow"); it then uploads the installers as
downloadable run artifacts without creating a release.

## Roadmap

- [x] M0 — Relay server: registration, ID assignment, session brokering, opaque
  byte relaying, with end-to-end tests.
- [x] M1 — Networking layer (`remotecraft-net`): a relay client (register,
  pair, send/recv) plus a password-authenticated, end-to-end encrypted channel
  (Noise `NNpsk0`). Demonstrated by `remotecraft-probe host|connect`.
- [x] M4 (channel) — Attended (random one-time) and unattended (fixed) password
  authentication and E2E encryption of the peer channel. Wrong password fails
  the handshake; the relay only ever sees ciphertext.
- [x] M2 — Screen capture + frame streaming; viewer window (GUI app). The
  host captures its primary monitor (xcap), JPEG-encodes frames and streams
  them; the viewer shows them scaled to its window.
- [x] M3 — Mouse + keyboard control: the viewer forwards pointer moves,
  clicks, scroll, typed text and shortcuts; the host injects them (enigo).
  Coordinates are normalized, so it is resolution-independent. On macOS the
  host needs the "Accessibility" permission.
- [x] M6 — File transfer: the viewer browses the host's folders and downloads
  files (e.g. a recording made on the office PC) to its own
  `~/RemoteCraft-Downloads`, streamed in chunks over the encrypted channel.
  (Host→viewer download; viewer→host upload is a later addition.)
- [x] M5 — Remote audio: the host captures its playback output (WASAPI
  loopback on Windows) and streams 16-bit PCM to the viewer, which plays it
  through its default output device (cpal). Enabled with `host --audio`.
  macOS host audio capture is not implemented yet (needs ScreenCaptureKit or a
  virtual device); the viewer plays audio on any platform.
- [x] M7 — Packaging: Windows setup `.exe` (Inno Setup) + portable zip and a
  universal macOS `.dmg`, built in CI on a version tag and attached to the
  GitHub release.
- [x] M8 — Graphical launcher: started with no arguments (e.g. from the
  desktop icon), it opens a small window to pick a role — view another
  computer or share this screen (attended one-time or unattended fixed
  password) — and fills the fields in, so no command line is needed. The host
  role gets a status window showing the ID and one-time password to read out.
  On macOS the `.dmg` ships a double-clickable `RemoteCraft.app`; on Windows the
  host status window can minimize/close to the system tray and keep sharing.
- [ ] M9 — Unattended Windows service: run as a SYSTEM service that can show the
  real login/lock screen (secure desktop) to the viewer and accept the Windows
  password, like AnyDesk/TeamViewer unattended access. Needs an admin install
  and isolated Win32 FFI. Until then, enable Windows auto-login on the host.
- [x] M10 — Multi-monitor: when the host has more than one monitor, the viewer
  shows a per-monitor picker (1, 2, …) next to the Files/Fullscreen buttons and
  can switch which monitor it sees and controls. Input is mapped into the chosen
  monitor's rectangle in the virtual desktop (Windows uses `SetCursorPos`, since
  enigo's absolute move only reaches the primary monitor).
- [x] M11 — Connection UX: a coloured relay LED in the viewer and host windows
  (amber = connecting, green = connected, red = problem) and a version-mismatch
  warning — the host advertises its version in `HostInfo`, and the viewer warns
  if it differs; a handshake that never completes now times out with a clear
  message instead of hanging.

### Security note

The password is mixed into the Noise handshake as a pre-shared key, so the two
peers authenticate each other and all traffic is end-to-end encrypted; the relay
cannot read it. A future milestone will upgrade password authentication to a
PAKE (e.g. SPAKE2/OPAQUE) to also resist offline guessing by a party that
captures a handshake. Until then, use a strong unattended password.

## 🐱 Support the Project (and the Cat Fleet!)

This project is completely free, open source, and developed with an AI-driven approach. While the code is crafted with the help of AI agents, the quality control is managed by three real-life supervisors:

| **Drugo** | **Elia** | **Nancy** |
| :---: | :---: | :---: |
| <img src="assets/team/drugo.jpg" alt="Drugo, a black-and-white cat peeking out from under a white sheet" width="200"> | <img src="assets/team/elia.jpg" alt="Elia, a black-and-white cat staring at treats on a table" width="200"> | <img src="assets/team/nancy.jpg" alt="Nancy, a tabby cat resting her paws on the arm of a sofa" width="200"> |
| *The Senior Consultant* | *The Quality Assurance Analyst* | *The Undercover Operations Manager* |
| Our wisest and oldest cat. He oversees structural decisions with calm. | Powered exclusively by *Catisfaction* treats. If the code compiles, he purrs. | She monitors the repository from her secret base under the bed. |

👉 [**Buy Elia some Catisfaction on Ko-fi**](https://ko-fi.com/minatore)

## Licence

MIT.

## Safety

Everything RemoteCraft reads from the network is untrusted input. The code
follows a few hard rules so a malformed or hostile peer can never crash a PC or
be used to attack it:

- No `unwrap`/`expect`/`panic!` in product code (only in tests); errors are
  returned as `Result` and propagated.
- No `unsafe` across the workspace (`unsafe_code = "deny"`), with a single
  audited exception: `crates/app/src/winos.rs`, a Windows-only module that calls
  `ShowWindow`/`SetForegroundWindow` on the app's own window to restore it from
  the system tray (eframe cannot repaint a hidden window, so it cannot show it
  again by itself). That one module opts in with `#![allow(unsafe_code)]`;
  everywhere else `unsafe` is a hard error.
- Every length read from the wire is bounded (`MAX_FRAME_LEN`) before any
  allocation, so a remote peer cannot force a huge buffer.
- Lock poisoning is handled; one bad connection never takes the relay down.
- Every crash fix ships with a synthetic regression test.

## ⚠️ Disclaimer

**Who builds this.** I am not a senior software developer. RemoteCraft is developed with substantial help from AI coding agents: I set the direction, review the changes and test them, but the code has not been audited by professional engineers and may contain bugs, security weaknesses or design mistakes.

**No warranty.** The software is provided "as is", without warranty of any kind, express or implied, including merchantability and fitness for a particular purpose. Use it at your own risk and keep backups of anything important.

**Limitation of liability.** To the maximum extent permitted by applicable law, the author and contributors are not liable for any damage, data loss or other harm arising from the use of, or the inability to use, this software. Nothing here limits any liability that cannot be excluded by law. The binding terms are those of the licence (`LICENSE-MIT`); this section summarises them and does not replace them.

**Your responsibility.** You alone are responsible for how you use this software and for complying with the laws that apply to you. In particular:

- **Authorised use only.** Only use RemoteCraft on computers that you own, or with the explicit, informed permission of their owner and of the person using them. Accessing a computer without authorisation is a crime in most countries.
- **At work.** If you use it in a workplace, follow your organisation's policies and the employment and privacy laws that apply (for example the GDPR and national rules on monitoring employees).
- **Security.** The encryption and authentication have not been independently audited. Use strong passwords, keep the relay server up to date, and do not rely on RemoteCraft where a breach would have serious consequences.

**Trademarks and affiliation.** "AnyDesk" and "TeamViewer" are trademarks of their respective owners and are named only to describe the kind of tool RemoteCraft is. RemoteCraft is an independent project, not affiliated with or endorsed by them.
