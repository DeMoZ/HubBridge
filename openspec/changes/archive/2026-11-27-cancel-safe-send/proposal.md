# Proposal

## Why

The host sends every request on the session from the caller's own future:
`Client::submit` locks the shared `SecureSender` and awaits `SecureSender::send_bytes`,
which takes the Noise nonces for the message, encrypts it and then awaits `write_all`
(`crates/link/src/lib.rs`, `send_bytes`). If that future is dropped while `write_all` waits
on a full socket — a `tokio::time::timeout` around a request, an aborted task, a `select!`
that drops the request — the agent gets a half-written record, or the next message is
encrypted with nonces the agent never saw. The agent fails to decrypt and ends the session,
and every open device and serial bridge on it goes down (GitHub issue #7, found in the
review of PR #6).

The serial bridge added such cancellation points (`crates/host/src/serial/port.rs`: the
300 ms / 500 ms / 1 s close budgets around control, release and close requests, and
`close()` aborting the IN pumps, the OUT writer and the CP210x poller), and the desktop
aborts its connection task, which may be inside `ping()`. The chance is low today (at most
64 KiB of serial OUT in flight, below the socket send buffer), but any consumer that wraps a
request in a timeout can hit it, and USB/IP (ROADMAP step 2, item 4) will add many more.

## What Changes

- **One writer task per host session** owns the `SecureSender` (the socket's write half).
  `Client::submit` and `Client::close` only put an encoded frame into a bounded queue for
  it; no caller awaits socket I/O any more, so dropping a caller's future can never leave a
  partial record or a skipped nonce on the wire. The agent already works this way (one
  session loop owns its sender).
- **The queue is bounded** (back-pressure, as before when callers waited on the socket):
  `submit` waits only for queue space, and that wait is cancel-safe — a dropped `submit`
  either queued its whole frame or nothing.
- **Session end is shared by both tasks:** a write error ends the session like a read
  error does: pending requests fail with "connection lost", later and waiting `submit`
  calls fail at once with the reason, and the event stream gets one `Disconnected`.
- **`close()` flushes:** `Bye` is queued after every frame already submitted, the writer
  writes them all and shuts the write half down, within a time limit.
- **`SecureSender` becomes cancel-safe by itself** (defence in depth, also protects the
  agent and any later user of `hubbridge-link`): a message is committed to an internal
  output buffer when it is encrypted, and a send dropped in the middle is completed by the
  next `send` or `shutdown` instead of corrupting the stream.
- **No protocol change:** the same records, framing and nonce sequence go on the wire;
  `PROTO_VERSION` stays 1 and `docs/PROTOCOL.md` is unchanged. The agent and the APK are
  not changed (only `hubbridge-link`, which the agent links, gets the buffered sender).
- Tests: link unit test (send dropped mid-write on a tiny duplex), host client unit tests
  over a duplex with an in-test agent (dropped submits, injected write error, concurrent
  callers), e2e tests against the real agent through a pausable TCP proxy (full socket,
  serial port close budgets).
- Docs: `docs/ARCHITECTURE.md` (host send path), `docs/ROADMAP.md` (the #7 follow-up line
  resolved).

## Capabilities

### New Capabilities
- `host-session`: how the desktop host sends on an established session with an agent —
  queueing requests for a single writer, cancel safety, ordering, back-pressure, how write
  and read failures end the session for every caller, and an orderly close.

### Modified Capabilities
<!-- none: usb-device-access, usb-mass-storage, usb-serial and serial-bridge keep their
     requirements. Requests still reach the agent in submit order, so "Bulk and interrupt
     transfers complete in request order" holds unchanged; the serial close budgets keep
     their values and simply become safe. -->

## Impact

- Code: `crates/link/src/lib.rs` (`SecureSender` output buffer, cancel-safe `send_bytes`
  and `shutdown`, docs), `crates/host/src/client.rs` (writer task, bounded queue, shared
  session state, `submit`, `close`, `Drop`, `Pending::wait` error text). No changes needed
  in the callers (`crates/host/src/main.rs`, `msc.rs`, `serial/*`, `crates/desktop/src/app.rs`):
  the `Client` API keeps its signatures.
- New tests: unit tests in `crates/link` and `crates/host/src/client.rs`; e2e tests in
  `crates/host/tests/` with a shared pausable TCP proxy helper.
- Dependencies: none new (tokio `mpsc` and `watch`).
- Protocol: unchanged (v1, same wire bytes). Agent behaviour: unchanged.
- Docs: `docs/ARCHITECTURE.md`, `docs/ROADMAP.md`.
