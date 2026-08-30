# Remote control: the plan

Locked assumptions: **iroh** for transport, **QR pairing**, **n0's
relays** to start, QUIC/TLS 1.3 between endpoints. The options review
that led here is in `remote-control.md`.

## What is being built

A paired iOS device can see every qmux terminal and drive it with the
same actions the right pane offers: send, queue, steer, reorder, approve
or deny a permission prompt. The Mac is unreachable until a button at
the top of the left sidebar turns it on.

Ten stages, four milestones. Each stage lands something you can run.

---

## Architecture

### Trust and reach

```
off              no endpoint bound. no socket, no record, no relay.
on · local       RelayMode::Disabled + mDNS discovery, no publishing.
on · anywhere    n0 relays + discovery publishing.
```

One code path. "Anywhere" is a second switch, not a second
implementation. `off → local` is the toggle; `local → anywhere` is a
separately confirmed choice, because it is a different consent.

### Two ALPNs

Pairing gets its own protocol identifier so it is a separate, tiny
surface that cannot reach a control operation even by mistake:

| ALPN | Who may connect | What it can do |
| --- | --- | --- |
| `qmux/pair/1` | any node, only while a pairing window is open | present a one-time secret, nothing else |
| `qmux/remote/1` | only a paired `NodeId` | the full session |

The accept loop checks `connection.remote_node_id()` against the paired
list before the first frame is read. An unpaired node on
`qmux/remote/1` is closed, not answered.

### Streams

QUIC gives independent, separately flow-controlled streams. Use them —
this is the whole reason the transport is worth changing.

| Stream | Direction | Carries |
| --- | --- | --- |
| `control` | bidirectional, client opens first | `hello`/`ready`, then `call`/`result` |
| `events` | Mac → phone, one | `QmuxEvent` frames after `subscribe` |
| `pane:<id>` | Mac → phone, one per subscribed pane | raw PTY bytes |

Framing is a `u32` big-endian length prefix. Control and event payloads
are JSON; pane payloads are **raw bytes** — QUIC is binary-safe, so no
base64 tax on the highest-volume path.

Writes go over `control` as ordinary calls. They are low-volume and want
ordering against reads.

### Backpressure — the thing that will actually break

A slow phone must never stall the PTY reader thread or `AppState::emit`.
Both are on the critical path for the person sitting at the Mac.

- **Pane bytes.** Per-pane bounded ring (256 KB). The reader thread does
  a non-blocking push; on overflow the ring drops its oldest bytes and
  sets a `gap` flag. When the writer drains it emits a `reset` marker
  and the client re-primes from `pane.snapshot`. Terminal output
  tolerates this because a fresh screen is always a valid resync point.
- **Events.** Bounded queue (1024). On overflow the session is marked
  desynced, queueing stops, and a single `resync` frame goes out; the
  client refetches `pane.list`, `agent.list`, and the queues. State
  events are **never** silently dropped — a lost `turn.updated` would
  leave the phone quietly wrong, which is worse than a visible resync.

The rule underneath both: the producer side is always non-blocking, and
recovery is always "re-read the truth", never "replay the difference".

### Async boundary

iroh is tokio-based; the backend is synchronous threads and mutexes.
Tokio is already in the tree (reqwest/hyper) and `tauri::async_runtime`
is already used in ~30 places, so the runtime exists — what is new is the
first long-lived async task.

One rule, and it is not optional: **never hold a `std::sync::Mutex` guard
across an `.await`.** The async side owns the connections; the sync side
owns `AppState`. They meet at channels and at the bounded rings above.

---

## Stages

### S0 · Groundwork — no UI

- Pin `iroh` exactly in `src-tauri/Cargo.toml`, with a comment naming the
  reason, in the style of the existing `tauri = "~2.11.3"` pin.
- Wire types into `qmux-proto` (they belong beside `PublicControlRequest`,
  and a future Rust client should share them).
- New module directory `src-tauri/src/remote/`:
  `mod.rs`, `frames.rs`, `endpoint.rs`, `session.rs`, `fanout.rs`,
  `pairing.rs`, `devices.rs`.
- `npm run test:remote` wired into the existing per-feature script list.

**Done when:** the crate builds with an endpoint that binds and
immediately shuts down, under test.

### S1 · The `Remote` principal — no UI

- `ControlPrincipal::Remote` in `control.rs`.
- A remote session carries a *focus pane* (defaulting to the app's active
  pane) so the ~15 existing `context.pane_id` readers keep working
  untouched; `session.focus` sets it.
- `allowed_pane_ids` and `workspace_list` learn the remote scope: the
  workspaces the device is allowed, default all.
- `require_user` becomes `require_write`, honouring the per-device
  read-only flag.

**Done when:** every existing operation behaves correctly for a Remote
principal, tested end-to-end over the existing Unix socket. No network
yet — this is deliberately the cheapest stage to get wrong and the
easiest to test.

### S2 · Endpoint and the control stream — dev CLI only

- `endpoint.rs`: build the iroh endpoint on `tauri::async_runtime`,
  relay mode and discovery from config, ALPN dispatch, accept loop.
- `frames.rs`: the length-prefixed codec and the frame enum.
- `session.rs`: `hello`/`ready`, then `call`/`result` bridged into
  `control::handle_call` with the Remote principal from S1.
- `qmux remote-probe <ticket>` as a hidden dev subcommand in `qmux-cli` —
  the first client, and the thing the integration tests drive.

**Done when:** `qmux remote-probe` runs `pane.list` against a second
qmux process on the same machine with relays disabled.

### S3 · Events and terminal bytes

- `fanout.rs`: the session registry, the bounded rings, the gap/resync
  logic above.
- A tap in `AppState::emit` — one function, so one insertion point.
- A tap in `pty::start_reader_thread`, beside the existing native-surface
  handoff and scrollback append.
- `subscribe`, `pane.stream`, `pane.snapshot` (primed by
  `scrollback::sanitize_scrollback_replay`).

**Done when:** the probe renders live terminal output and receives
`turn.updated` while an agent works, and a deliberately stalled reader
produces a gap and a clean resync rather than backing up the PTY.

### S4 · Identity, pairing, authorization

- Node secret key in the macOS Keychain under `app.qmux.remote-control`,
  mirroring the `app.qmux.github-oauth` pattern in `publishing.rs`.
- `PairedDevice { node_id, name, paired_at, last_seen, read_only,
  workspaces }` persisted in `persistence::AppPreferences`.
- The pairing window: a 128-bit one-time secret, three-minute TTL,
  single use, burned on presentation.
- QR payload:
  `qmux-pair:v1?node=<NodeId>&psk=<secret>&name=<Mac name>`
- Accept-side gate, the approval prompt, and revocation.

**Done when:** an unpaired node is refused before its first frame, a
burned secret cannot be reused, and revoking a device breaks its next
connection at the handshake.

### S5 · The macOS UI

- `RemoteControlButton` in `sidebar-header-controls is-grouped`, beside
  `TerminalMapButton` and the collapse button — automatically absent when
  the sidebar is collapsed, since the whole `<aside>` is unrendered.
- The popover: master switch, local/anywhere, pairing QR, paired devices
  with revoke, live sessions, per-device read-only.
- `remote_*` Tauri commands and the `AppPreferences` toggle, including
  on-at-launch.
- Menu-bar indicator while a session is live — remote control that is
  silently on is a footgun.

**Done when:** the feature is reachable, visible, and revocable without
a terminal.

### S6 · Right-pane parity

- Move `ComposerPolicy` and `permissionActions` out of
  `src/adapters/*.tsx` and into the Rust `AgentAdapter` trait; serve them
  as `adapter.policy`. The frontend then reads the same source, so the
  two clients cannot drift.
- `agent.submit` with an explicit mode (`send`/`queue`/`steer`) —
  `agent.prompt` hardcodes Auto.
- `agent.queue.list|remove|reorder|sendNext|pause|unpause`, wrapping the
  `turn_queue` functions the right pane already calls.
- `agent.permission`, sending the adapter's approve/deny keystrokes.

**Done when:** `deriveComposerGating` in the web frontend is driven by
backend-supplied policy, with no per-adapter table left in TypeScript.

### S7 · The iOS app

SwiftUI, `iroh-ffi` for the endpoint, SwiftTerm for terminal rendering.
Screens are mocked separately.

- Pair by camera; node key in the Secure Enclave where available.
- Terminal list grouped by workspace, with agent status.
- Terminal view (SwiftTerm) with an input bar; **no pane resize** — the
  Mac's layout must not reflow to a phone, so send dimensions and
  letterbox.
- Transcript and composer; queue view; permission prompts.
- Reconnect on foreground: iOS suspension kills the connection, so this
  is required, not polish.

**Done when:** an agent can be driven from the phone from start to
finish without touching the Mac.

### S8 · Notifications — the honest gap

iOS suspends the app and the connection dies with it, so "your agent
needs permission" cannot arrive over iroh while backgrounded. Real push
needs APNs, and APNs needs a server holding a token. That is the one
place where "no infrastructure" genuinely breaks down.

Two options, both deferrable:

- **Live-only.** Local notifications while the app is foregrounded or in
  its brief background window. Costs nothing, helps less.
- **A push sender.** A small service the Mac tells "device D wants to
  know", which forwards through APNs. Reintroduces a server — but a far
  smaller one than a relay, holding only device tokens and never
  terminal content.

Decide this after living with S7. It is genuinely optional, and shipping
without it is honest as long as the limitation is documented.

### S9 · Hardening and release

- Per-device session cap, connection rate limit, byte budget.
- Network-change handling: rebind and re-advertise when the Mac moves
  networks; never keep advertising a stale address.
- Run the repo's `/security-review` over the diff.
- README section, and the `remoteControl` block in `qmux.config.json`.
- Optional: self-hosted `iroh-relay` for anyone who won't use n0's.

---

## Milestones

| # | Stages | The sentence that becomes true |
| --- | --- | --- |
| **M1** | S0–S3 | Two processes on my Mac talk over iroh, with live output |
| **M2** | S4–S5 | My phone drives it from the couch, and I can turn it off |
| **M3** | S6–S7 | It's the right pane, on a phone, from anywhere |
| **M4** | S8–S9 | I'd let someone else use it |

M1 is entirely headless and entirely testable — no UI, no phone, no
network. That is deliberate: it is the part where the design can still
be wrong cheaply.

---

## Security model, stated plainly

**What the encryption gives you.** iroh connections are QUIC, so TLS 1.3
between the two endpoints, keyed to the node identities. n0's relays
forward encrypted packets; they can see that two NodeIds are exchanging
traffic, and how much and when, but not what.

**What it does not give you.** iroh authenticates *which key* connected.
It does not decide whether that key may drive your terminals. Every
authorization decision is local: the paired list, the approval prompt,
revocation, the read-only flag.

**Metadata.** In "anywhere" mode the node record is published to
discovery infrastructure. That is a real disclosure — your NodeId
becomes linkable to your addresses — and it is why publishing is tied to
that mode rather than to the master switch.

**Threats and answers.**

| Threat | Answer |
| --- | --- |
| Someone on the LAN races the pairing | The QR is out-of-band; the one-time secret authenticates the first frame |
| The QR is photographed over your shoulder | Single use, three-minute TTL, plus a Mac-side approval prompt naming the device |
| A paired phone is stolen | Revocation in the popover; the next handshake fails |
| The relay operator is hostile | Sees ciphertext, sizes, timing. Not content |
| An agent inside a pane tries to reach the remote API | It has a pane token, which is a different principal; `qmux/remote/1` requires a paired NodeId |
| The Mac is compromised | Out of scope. Nothing here helps, and pretending otherwise would be dishonest |

---

## Test plan

Hermetic, no network, following the repo's existing conventions.

- **Scope and principal** (`control.rs` tests): a Remote principal sees
  the workspaces it is allowed and no others; read-only refuses writes.
- **Frame codec**: round-trip, truncation, oversize rejection.
- **Backpressure**: fill a pane ring, assert the gap flag and the reset
  marker; overflow the event queue, assert exactly one `resync`.
- **Pairing**: expiry, replay of a burned secret, wrong secret, unpaired
  node on `qmux/remote/1`, revocation mid-session.
- **Integration**: two iroh endpoints in one test process with relays
  disabled — real transport, no external dependency.
- `npm run test:remote`, added to the `test` chain.

---

## Open questions inside the plan

1. **Does the phone get its own workspace scope, or all of them?** The
   `PairedDevice` record has the field either way; the question is what
   the pairing flow defaults to. Suggest: all workspaces, read-write,
   because a scoped default that nobody understands gets widened blindly.
2. **Does S6 land before or after S7?** Before means the iOS composer is
   correct from its first build. After means you see the phone sooner.
   Suggest: before, because the drift it prevents is expensive later.
3. **Is S8 in scope at all for v1?** It is the only stage that
   reintroduces a server.
