# Remote control from iOS

Design notes for driving qmux terminals from a phone. Nothing here is
implemented yet; this is the options review and the handshake design.

## Short answer

The control semantics already exist. `src-tauri/src/control.rs` is a
32-operation public API with a principal model, workspace scoping, and a
wire format (`qmux-proto`) whose own doc comment reserves the right to
travel over something other than a Unix socket:

> Transport-agnostic on purpose — today the frames travel over a local
> Unix socket, but nothing here may assume that: a forwarded socket or a
> network transport must be able to reuse these types unchanged.

So an iOS controller is a *second client of a protocol that already
exists*, not a new subsystem. Four things are genuinely missing:

1. A network transport (the socket is `AF_UNIX`, 0600, local-only).
2. An event/output subscription (the socket is request/response only).
3. A principal that is not bound to a pane (see below).
4. Terminal *content* for a remote screen — the webview never sees
   terminal bytes today, so there is no existing stream to forward.

Recommended shape: **one iroh endpoint behind one master toggle, with
"reachable from anywhere" as a second switch rather than a second
implementation.** See the iroh section below — it collapses options A,
B, and C into a single dependency. Hand-building it (LAN-direct with
Bonjour first, an opt-in relay later) is the fallback if that
dependency is unacceptable.

## Four facts that constrain the design

### The webview never sees terminal bytes

`pty::start_reader_thread` hands each 64 KB chunk to the native Ghostty
surface over FFI and appends it to the durable per-pane journal. On
non-macOS there is no renderer at all — from the comment in `pty.rs`:

> Without a native surface (non-macOS) there is no renderer — the webview
> dropped the old per-chunk `pty.data` events unread — so output is only
> recorded.

`TerminalPane.tsx` is a layout placeholder for a native surface; there is
no xterm.js anywhere. Consequence: "see my terminals" is a new data path,
and you get to pick its fidelity. Three levels, cheapest first:

| Fidelity | Source | Client needs | Notes |
| --- | --- | --- | --- |
| Plain text tail | `pane.read` (exists) | nothing | `mcp::terminal_text_tail` strips escapes; already used by the CLI |
| Plain text screen | `native_terminal_read_viewport_text` (exists) | nothing | Main-thread FFI hop per call; polls, no colors, no cursor |
| Full terminal | new tap in `start_reader_thread` | a terminal emulator (SwiftTerm) | Prime with `scrollback::sanitize_scrollback_replay`, then stream deltas |

The first two are free — they already ship. Only the third needs new
plumbing, and it is the only one that gives a real remote terminal.

### `control.rs` is pane-scoped

`ControlContext` carries `pane_id` + `workspace_id` + optional agent, and
the principal is derived from which token matched:

```rust
pub enum ControlPrincipal { User, Agent }
```

`User` means "a `QMUX_USER_TOKEN` matched a pane", and its scope is *that
pane's workspace only* — `allowed_pane_ids` filters on
`pane.group_id == context.workspace_id`, and `workspace.rename` refuses
anything but the current workspace. A phone has no pane, and wants to see
every workspace. That needs a third principal:

```rust
pub enum ControlPrincipal { User, Agent, Remote }
```

`Remote`'s read scope is the workspaces the user allowed (default: all);
its write scope is the same, gated by a per-device read-only flag. The
cheapest way to keep the ~15 `context.pane_id` call sites working is to
give a remote session a *focus pane* — defaulting to the app's active
pane, settable with a new `session.focus` op — so `pane.current`,
`pane.create` placement, and `workspace.create` positioning all behave
unchanged.

### Right-pane semantics live in TypeScript

The composer's capability gating is `deriveComposerGating` in
`src/lib/composerActions.ts`, driven by per-adapter `ComposerPolicy`
tables in `src/adapters/*.tsx`. Permission approval is *keystrokes*:

```ts
permissionActions: [
  { id: "approve", label: "Approve", input: "y" },
  { id: "deny",    label: "Deny",    input: "n" },
]
```

The Rust `AgentAdapter` trait knows nothing about any of this. So
"equivalent of right pane actions" means either moving these tables into
Rust (so the backend can serve them to any client) or duplicating them in
Swift and accepting drift the first time an adapter changes. Move them.

### No event subscription exists

`control_socket::handle_line` is one request, one response, 5 s read
timeout, connection closed. Live state reaches the frontend through
Tauri's event channel from `AppState::emit` — a single choke point, which
is convenient: one fan-out there feeds every remote session.

## Transport options

| Option | Reach | Infra you run | New code | Trust surface | Fails when |
| --- | --- | --- | --- | --- | --- |
| **A. LAN direct + Bonjour** | Same network | none | TLS listener, mDNS advertise, iOS browser | you only | Off-network; client-isolated Wi-Fi |
| **B. Relay via qmux.app** | Anywhere | Fly app (exists) | A + relay client + relay service | you + relay host (ciphertext only) | Relay down |
| **C. Overlay (Tailscale)** | Anywhere | none | ~none — bind to the tailnet address | you + Tailscale | User won't install it |
| **D. SSH** | Anywhere reachable | none | iOS SSH client; reuse NDJSON over a forwarded socket | you + sshd | Still needs inbound reachability; streaming is awkward |
| **E. Third-party tunnel (`cloudflared`)** | Anywhere | none | spawn/supervise a child process | you + Cloudflare | Public hostname exists whenever it runs |

Client shape is a separate axis:

- **Native iOS app.** Only path to a real terminal (SwiftTerm), Bonjour
  browsing, Keychain/Secure Enclave keys, push notifications. Costs a
  reimplementation of the transcript renderer.
- **Web client.** `src/lib/api.ts` is the *only* module that touches
  `invoke`/`listen`. Reimplement that one file against the remote
  transport and the entire React app runs elsewhere — transcript,
  composer, queue, everything. You'd add xterm.js for terminals and
  fight a desktop-shaped layout on a phone, but it is dramatically less
  code and needs no App Store.

A good split: web client to prove the protocol, native app for the
terminal-grade experience.

### Recommendation

Ship **A**, design for **B**, document **C**.

- **A** is self-contained: no third party, no account, lowest latency,
  and the toggle provably controls the whole thing (nothing is bound
  until you flip it).
- **B** is the same session tunnelled: both sides dial *out*, so no port
  forwarding and no inbound firewall hole. Keep it a second, separately
  confirmed switch — "reachable from anywhere" is a different consent
  than "reachable from this room".
- **C** costs you nothing to support. If the listener binds to all
  interfaces rather than just loopback, a tailnet address already works.

Make the transport a config choice next to the existing `remotes` block
in `qmux.config.json`:

```json
{
  "remoteControl": {
    "transport": "lan",
    "relay": "wss://relay.qmux.app",
    "enabledAtLaunch": false
  }
}
```

On the client, try LAN first and fall back to relay — store the last
known LAN address per paired Mac, race both, take whichever answers.

## Do you need discovery?

**On the LAN, yes.** Not for correctness — you could type an IP — but
DHCP addresses move and typing `192.168.x.x:port` on a phone is the kind
of friction that makes a feature go unused. Advertise `_qmux._tcp` with
`NWListener`'s Bonjour integration from the existing Swift package
(`src-tauri/swift-terminal/`), publishing only the service name, port,
and a short device label. No secrets in TXT records — the record is
visible to everyone on the network.

Advertise *only while remote control is on*, and withdraw the record when
it goes off. On iOS you need `NSLocalNetworkUsageDescription` and
`NSBonjourServices` (`_qmux._tcp`) in `Info.plist`, plus the user's
Local Network permission prompt.

**Off the LAN, no.** There is no discovery protocol to write: the relay's
registry *is* discovery. The Mac registers under a stable device id when
it connects; the phone asks for that id. The pairing record on the phone
holds the id, so there is nothing to look up.

## Do you need a tunnel server?

Only for off-LAN reach, and only if you reject C and E. If you do run
one, the honest job description is: *a router that matches two
connections by session id and copies encrypted bytes between them.* It
should not be able to read frames, and with a mutually-pinned TLS session
running inside the relay connection, it can't.

The existing Fly deployment (`fly.toml`, `web/server.tsx`) is the natural
home, with three changes:

- `auto_stop_machines = "stop"` and `min_machines_running = 1` are tuned
  for stateless page serving. A relay holding long-lived sockets wants
  the machine pinned up.
- Fly's proxy will idle-close a quiet WebSocket; both sides need
  application-level pings well inside that window.
- Add a per-account connection cap and a byte budget. A relay is a free
  bandwidth amplifier if you don't.

Cheapest credible alternative: don't. Recommend Tailscale in the docs and
spend the effort on the LAN path and the client instead.

## Would iroh replace most of this?

Largely, yes — and it collapses the transport question from "pick one of
five, stage the second one later" to "one dependency, two modes".

[iroh](https://github.com/n0-computer/iroh) is a QUIC-based peer-to-peer
library. An `Endpoint` is identified by a `NodeId` — an ed25519 public
key — and dialing a `NodeId` is what proves the peer's identity, because
the QUIC/TLS handshake is bound to that key. It attempts direct UDP
paths, hole-punches through NATs, falls back to relaying when it can't,
and *upgrades relay to direct mid-connection* when a path opens.

### What it replaces

| Hand-built above | With iroh |
| --- | --- |
| `rustls` listener + `rcgen` cert generation | An ed25519 keypair. No certificate plumbing. |
| SPKI pinning in both directions | Dialing a `NodeId` *is* the pinning |
| Bonjour advertise + `NWBrowser` on iOS | Local mDNS discovery, built in |
| A relay on Fly: registry, keepalives, byte budgets | n0's public relays, or self-hosted `iroh-relay` |
| LAN-first / relay-fallback racing on the client | Built in, including the mid-connection upgrade |
| `tungstenite` framing over one socket | QUIC streams |

Options A, B, and C from the table above stop being separate options.
That also deletes most of stage 2, stage 5, and all of stage 7.

### The stream multiplexing is a real win, not a detail

The WebSocket design has one socket carrying control RPC, the event
stream, and every subscribed pane's output. That is head-of-line
blocking by construction: a pane dumping a build log delays the response
to a permission approval. QUIC gives each its own stream, independently
flow-controlled. Control on one stream, events on another, one stream per
subscribed pane — which is exactly the shape this workload wants, and is
the difference between remote control that feels immediate and remote
control that feels laggy whenever anything is compiling.

### What it does not give you

**Authorization.** iroh tells you *which key* connected. It does not
decide whether that key may drive your terminals. Everything on the
accept side is still yours: check `remote_node_id()` against the paired
list, close otherwise; the approve-on-first-pair dialog; revocation; the
per-device read-only flag. "End-to-end encrypted and authenticated" is
not "authorized", and that gap is where this kind of feature usually goes
wrong.

Also untouched: the `Remote` principal, the fan-out from
`AppState::emit`, the PTY tap, moving the composer policy into Rust, the
toggle and its UI, and the client itself. iroh is transport. Every
qmux-shaped problem in this document survives it.

### It maps onto the off-switch requirement better than the hand-built plan

iroh's defaults run *against* "disconnected unless I turn it on": the
default relay mode holds an open connection to n0's servers so the node
stays reachable, and the default discovery can publish the node's record
to public infrastructure (DNS, or the Mainline DHT via pkarr). Neither is
acceptable as a default here.

Configured deliberately, though, the three states fall out cleanly:

| Toggle | Configuration | Reach |
| --- | --- | --- |
| **Off** | no `Endpoint` bound at all | nothing; no socket, no record, no relay |
| **On · local** | relays disabled, mDNS discovery only, no publishing | this network only |
| **On · anywhere** | relay enabled, discovery publishing on | anywhere |

One code path, one dependency, and "reachable from anywhere" is a second
switch rather than a second implementation. That is better than the
LAN-now/relay-later staging above, where the second half was always the
part most likely never to get built.

### The costs, honestly

1. **It is a posture change.** `connection_limit.rs` says it is
   hand-rolled "to keep the backend's no-new-dependencies posture (cf.
   the hand-rolled HTTP in file_server.rs)". iroh brings quinn and a
   substantial tree. That deserves a deliberate decision rather than a
   shrug — though note the distinction: hand-rolling a GET/Range server
   is a weekend and then it's finished; hand-rolling NAT traversal is
   never finished.

2. **Async, but not from zero.** iroh is tokio-based. The backend is
   synchronous threads and mutexes — but tokio is *already* in the tree
   via reqwest/hyper, Tauri commands are already `async fn`, and
   `tauri::async_runtime::spawn_blocking` appears in about thirty places.
   So the runtime exists; what's new is the first long-lived async task.
   The rule that matters: never hold a `std::sync::Mutex` guard across an
   `.await`. Bridge with channels — async task on one side, the existing
   synchronous fan-out registry on the other.

3. **Version churn.** iroh's 0.x line has broken APIs regularly. Pin it
   exactly and budget for upgrades. There's precedent: `tauri` is already
   pinned to `~2.11.3` with a comment explaining why.

4. **Binary size and build time.** quinn, tokio's net stack, DNS. The
   release profile is tuned with thin LTO and stripped symbols
   specifically for launch time, and every in-shell `qmux` CLI fork pages
   this binary in.

5. **The iOS side needs verifying.** `iroh-ffi` publishes bindings
   including Swift via UniFFI, but check its current state before
   committing — that's the load-bearing assumption. Then the ordinary
   mobile problems: iOS suspends the app and kills the QUIC connection,
   so reconnect-on-foreground is required, and mDNS still needs the Local
   Network permission.

6. **Networks that block UDP** break the direct path. The relay transport
   is HTTPS-based and survives; LAN-only mode on such a network would
   not. Rare on a home network, common on corporate guest Wi-Fi.

### Verdict

Take it, if the dependency posture change is acceptable — and the
question to settle first is whether off-LAN access is ever wanted.
Wanting to check on agents from a phone rather strongly implies "from
anywhere", and off-LAN is precisely the part iroh gives you for free and
the hand-built plan pays for forever. If the honest answer is "LAN only,
always", then mDNS plus pinned TLS is a few hundred lines and iroh is a
large dependency earning little.

Note that these are API and ecosystem claims about a fast-moving
library, made from memory. Check the current docs before designing
against specific type names.

## The handshake

Two distinct ceremonies. Conflating them is the usual mistake.

### Pairing (once per device)

State on the Mac: a self-signed TLS identity generated on first enable,
private key in the macOS Keychain — the same pattern
`publishing.rs` already uses for the GitHub token
(`keyring::Entry::new("app.qmux.github-oauth", "github")`), under a new
service name like `app.qmux.remote-control`.

1. User flips remote control on. The Mac generates its identity if
   absent, binds, and advertises.
2. The panel shows a QR code and a typeable short code. The QR carries:

   ```
   qmux-pair:v1?spki=<base64 SHA-256 of the Mac's cert SPKI>
              &psk=<128-bit one-time secret>
              &host=<lan addr>&port=<port>
              &device=<stable device id>&name=<Mac name>
   ```

   The PSK is single-use and expires in ~3 minutes.
3. The phone generates its own keypair (Secure Enclave where possible),
   self-signs a client cert, and connects. It verifies the Mac by
   comparing the presented cert's SPKI hash to `spki` — pinning, not a
   CA. A self-signed cert is fine precisely because nothing trusts a CA
   here.
4. TLS 1.3 completes with the phone's client cert presented. The phone's
   first application frame is `pair` carrying the PSK.
5. The Mac checks the PSK, burns it, and shows a confirmation dialog:
   device name plus a short authentication string — four words derived
   from a hash of both certs' SPKIs — displayed on *both* screens. The
   user compares and approves.
6. Both sides persist the peer's SPKI hash. Pairing is over; the PSK is
   dead.

Why the PSK: without it, first contact is trust-on-first-use and anyone
on the LAN can race the pairing. Why the SAS on top: it makes a leaked or
shoulder-surfed QR insufficient on its own.

### Session (every connect)

Mutual TLS 1.3, both ends pinned to stored SPKI hashes. No tokens, no
passwords, no bearer credentials to steal — the credential is a key that
never leaves the device. Then, inside the session:

```
→ hello   { apiVersion: 1, client: "qmux-ios/0.1", device: <id> }
← ready   { app: "qmux/0.3.1", apiVersion: 1, scopes: [...], readOnly: false }
→ call    { seq, operation: "pane.list", arguments: {} }
← result  { seq, ok: true, result: {...} }
→ subscribe { events: ["agent.*", "turn.*"], panes: ["pane-…"] }
← event   { type: "turn.updated", paneId, payload }
← output  { paneId, seq, bytes: <base64 pty chunk> }
```

`call`/`result` are `PublicControlRequest`/`PublicControlResponse`
verbatim — the same types the CLI uses — with a `seq` for pipelining.
`subscribe`/`event`/`output` are the new part.

Notes that matter:

- **Revocation is a list, not a protocol.** Deleting a device's stored
  SPKI hash is complete revocation; the next handshake fails at TLS.
- **Replay** is handled by TLS for the channel. Give writes a monotonic
  `seq` per session anyway so a reconnect can't double-send a queued
  turn.
- **The relay learns nothing** but "device X and device Y exchanged N
  bytes". Session keys are negotiated inside.
- **Version skew** is why `hello`/`ready` exchange `apiVersion`;
  `PUBLIC_API_VERSION` is already 1 in `qmux-proto`.

## The off switch

"Disconnected from the tunnel unless I turn it on" has to mean something
stronger than "listening but refusing". Off should mean:

- No listening socket bound. `lsof` shows nothing; there is no port to
  scan, no TLS stack to have a bug in.
- No Bonjour record published.
- No outbound relay connection.

And flipping off must actively tear down: close live sessions with a
`going_away` frame, withdraw the mDNS record, drop the relay socket,
and stop the event fan-out.

The toggle belongs in the backend preferences file
(`persistence::AppPreferences`, owner-only 0600), *not* localStorage —
the listener is backend state, and the "on at launch" choice has to be
readable before the webview exists. That file is already where
`open_router_key` and `use_login_shell` live for exactly this reason.

Two more behaviors worth deciding now: re-verify the bound interface on
network change (a Mac that moves networks should not keep advertising a
stale address), and show an unmistakable on-state — the header button
pressed, and probably a menu-bar dot. Remote control that is silently on
is a footgun.

## The button

It goes in the grouped header controls in `App.tsx`, beside the terminal
map and collapse buttons:

```tsx
<div className="sidebar-header-controls is-grouped">
  <TerminalMapButton … />
  <RemoteControlButton pressed={remoteControlOn} onClick={…} />
  <button … >Collapse left sidebar</button>
</div>
```

"Hidden if collapsed" is free: when `leftSidebarCollapsed` is true the
whole `<aside className="sidebar">` isn't rendered — only a
`sidebar-collapsed-placeholder` div is. Nothing to add.

One thing to *not* do: `leftSidebarRestorePlacement` in
`src/lib/sidebarControls.ts` relocates the sidebar *restore* control into
the turn-pane header when collapsed, and `TurnPaneHeader` conditionally
renders a terminal-map button there too. Do not follow that pattern for
this button — the requirement is that it disappears with the pane.

The button should open a popover, not just toggle. It needs to hold: the
master switch, the LAN/relay choice, the pairing QR, the paired-device
list with revoke, live sessions ("iPhone · connected 2m"), and the
per-device read-only flag. A bare toggle with no visible device list
means no way to notice a device you didn't intend to keep.

## What the controller can do

Free, from the existing 32 operations: `workspace.list`, `pane.list`,
`pane.read`, `agent.list`, `agent.read` (transcript turns),
`artifact.list`, `split.list`, `pane.send`, `pane.run`, `pane.focus`,
`pane.rename`, `pane.close`, `pane.create`, `agent.start`, `agent.fork`,
`agent.prompt` (which routes to `turn_queue::submit_agent_turn` in Auto
mode — the composer's own send path), `agent.wait`, `agent.release`.

New ops needed for right-pane parity:

- `subscribe` / `event` — fan out from `AppState::emit`.
- `pane.stream` / `output` — tap `pty::start_reader_thread`, primed by
  `scrollback::sanitize_scrollback_replay`.
- `agent.queue.list|remove|reorder|sendNext|pause|unpause` — wrap the
  existing `turn_queue` functions that the right pane already calls.
- `agent.submit` with an explicit mode (`send`/`queue`/`steer`) —
  `agent.prompt` hardcodes Auto.
- `agent.permission` — the adapter's `permissionActions`, once those
  tables live in Rust.
- `adapter.policy` — so the client can gate its buttons the way
  `deriveComposerGating` does instead of guessing.

**One real decision: don't let the phone resize panes.** `pane.resize`
exists as a Tauri command, and calling it from the phone would reflow the
Mac's visible terminal to fit a phone screen. Send the pane's current
dimensions and letterbox on the client instead.

## Implementation map

Staged so each step is useful alone.

**Stage 1 — remote principal, no network.**
`control.rs`: add `ControlPrincipal::Remote`, `session.focus`, and scope
`Remote` to allowed workspaces. Testable entirely through the existing
Unix socket.

**Stage 2 — the streaming channel.**
New `src-tauri/src/remote_control.rs`: TLS listener over `TcpListener` +
`rustls` (already a dependency), WebSocket framing via `tungstenite`
(already a dependency — but `default-features = false, features =
["handshake"]`, so TLS is ours to wrap, which is what we want anyway).
Fan-out registry subscribed from `AppState::emit` and
`pty::start_reader_thread`. Reuse `ConnectionLimiter` for the accept
loop; follow `file_server.rs` and `control_socket.rs` for the
supervisor/teardown pattern, which already handle exactly the "must be
gone when told to be gone" problem.

**Stage 3 — pairing and identity.**
Cert generation (this needs `rcgen` or hand-rolled DER), Keychain
storage, paired-device list in `AppPreferences`, QR payload, SAS
derivation.

**Stage 4 — UI.**
`RemoteControlButton` + popover, `remote_control_*` Tauri commands, the
`AppPreferences` toggle, menu-bar indicator.

**Stage 5 — Bonjour.**
`NWListener` advertisement in the Swift package, published and withdrawn
with the toggle.

**Stage 6 — client.** Web client against a reimplemented `api.ts`, or
native iOS with SwiftTerm.

**Stage 7 — relay,** if you still want it after living on the LAN.

## Decisions, settled

| Question | Decision |
| --- | --- |
| Terminal fidelity | Stream real PTY bytes; the phone brings SwiftTerm |
| Client shape | Native iOS |
| Off-LAN | iroh, n0's relays to start, behind a second switch |
| Composer policy in Rust? | Yes, in stage 6, before the iOS client |
| What a paired device may do | Every workspace, read-write |
| Push notifications | Out of scope for v1 |

Still genuinely open: whether a dependency the size of iroh is acceptable
in a backend that hand-rolls HTTP and base64 on purpose. That is a values
question about the codebase, and the answer shapes nothing else in the
plan — the stages are the same either way, only the contents of stage 2
change.

The build plan that follows from these is in `remote-control-plan.md`.
