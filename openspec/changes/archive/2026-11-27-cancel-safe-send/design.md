# Design

## Context

See proposal.md (Why) for the bug. The code as it is on `main` (cda817d):

- `hubbridge-link` (`crates/link/src/lib.rs`): `SecureSender<W>` holds the write half, an
  `Arc<StatelessTransportState>` and `nonce: u64`. `send_bytes` builds `u32 length +
  postcard`, splits it into ≤ 65519-byte chunks, encrypts each with `nonce` (incremented per
  record) into one `out` buffer, then `write_all(&out).await` and `flush().await`. The nonce
  advances **before** the write; a dropped future loses the rest of `out`. `SecureReceiver`
  reads `u16` length + body records and decrypts with its own counter; any gap or partial
  record is a decrypt / framing error, and the agent ends the session on it.
- Host client (`crates/host/src/client.rs`): `Client` keeps
  `writer: tokio::sync::Mutex<SecureSender<OwnedWriteHalf>>`, `pending:
  Arc<Mutex<HashMap<u32, oneshot::Sender<Message>>>>`, a reader task (spawned in `open`) that
  routes replies by `req`, sends `req == 0` frames as `ClientEvent::Message` and, on any
  receive error or `Bye`, clears `pending` and emits `ClientEvent::Disconnected(reason)`.
  `submit` allocates `req`, inserts the pending entry, then `writer.lock().await.send(..)
  .await` — the caller's future does the socket I/O. `close(self)` locks the writer, sends
  `Bye`, shuts down; no time limit. `Drop` aborts the reader. After the reader ended, a new
  `submit` still inserts a pending entry and waits for its full timeout (10 s + transfer
  timeout) — a smaller bug of the same area, fixed here too.
- Agent (`crates/agent/src/lib.rs`, `run_session`): the receiver lives in its own task
  ("recv() is not cancel-safe"); one session loop owns the `SecureSender` and writes from
  `select!` branch **bodies** (never cancelled once chosen); finished USB requests come back
  through an unbounded `mpsc<Frame>`. So the agent already has a single writer; the host is
  the side to fix.
- Protocol limits: `MAX_MESSAGE_LEN` = 16 MiB, transfers ≤ 1 MiB, the agent keeps at most
  `MAX_IN_FLIGHT` = 256 pending requests per session and stops reading beyond that, and
  closes a session idle for 20 s.

### Host send paths

Only `client.rs` can touch the sender (the field is private), so every host send goes
through `Client::submit` or `Client::close`. All of them get the fix without call-site
changes:

| Path | Entry | Cancellation points today |
|---|---|---|
| `Client::request` → `ping`, `agent_info`, `list_devices`, `request_permission` | `submit` | desktop `stop_connection` aborts `run_connection` (inside `ping`/`agent_info`/`list_devices`); desktop `request_permission` task |
| `Client::expect_done` → `open_device`, `close_device`, `claim_interface`, `release_interface`, `set_alternate`, `clear_halt`, `reset_device` | `submit` | serial `close()`: `timeout(RELEASE_TIMEOUT 500 ms, release_interface / close_device)` |
| `Client::control` | `submit` | serial `raw_reply`: `timeout(SHUTDOWN_REQUEST_TIMEOUT 300 ms)` while closing; `timeout(CHIP_SHUTDOWN_BUDGET 1 s, shutdown_chip)`; `cp210x_poller` aborted by `close()` |
| `Client::submit_transfer` / `transfer` | `submit` | serial `in_pump` and `out_writer` tasks aborted by `close()`; `finish_out` → `clear_halt` |
| `Client::close` (`Bye`) | sender directly | CLI and desktop call it on exit / disconnect |
| CLI (`crates/host/src/main.rs`): `info`, `devices`, `grant`, `ping`, `watch`, `usb descriptors\|storage`, `serial`, bench modes | the methods above | `select!` with `ctrl_c` (bodies run to completion; Ctrl-C mid-request exits the process) |
| Mass storage (`crates/host/src/msc.rs`): `control` (reset, descriptors), `transfer`, `submit_transfer` (pipelined `READ(10)` data phase) | the methods above | none of its own; runs under the CLI |
| Serial (`crates/host/src/serial/port.rs`, used by `bridge.rs`, `pty.rs`, `tcp.rs`, `rfc2217.rs`, `bench.rs`) | `control`, `submit_transfer`, `clear_halt`, `release_interface`, `close_device` | the close budgets and aborts above |
| Desktop (`crates/desktop/src/app.rs`): `ping` every second, `agent_info`, `list_devices`, `request_permission`, serial bridges, `close` | the methods above | `conn_task.abort()` in `stop_connection`; `Arc::try_unwrap` → `close` only when no bridge holds the client |

## Goals / Non-Goals

**Goals:**
- No future a host caller can drop touches the socket; a dropped `submit` queues all of its
  frame or none of it.
- Same `Client` API and signatures; callers unchanged.
- Failures end the session for every caller promptly (no 10 s waits after the session is
  gone, no hangs).
- Same bytes on the wire as before.

**Non-Goals:**
- A write timeout or a keep-alive for a peer that stops reading (the agent's 20 s idle
  timeout and the callers' request timeouts stay the only clocks; callers can now time out
  safely).
- Cancelling a request the agent already received (no "cancel request" message — would be a
  protocol change).
- Changing the agent's session loop (it already has a single writer).
- Batching several frames into one write (possible later inside the writer; not needed).
- Session resumption after a drop (ROADMAP step 4).

## Decisions

### 1. One writer task per session owns the `SecureSender`

`Client::start` (new, private, generic over the read and write halves so unit tests can run
it over a `tokio::io::duplex`) spawns the reader task (as today) and a **writer task** that
owns `SecureSender<W>`. `Client` keeps an `mpsc::Sender<Vec<u8>>` to it instead of the
`Mutex<SecureSender>`. The writer loop:

```
loop {
    select! { biased;
        _ = ended.wait()            => break,            // the session ended elsewhere
        next = rx.recv()            => match next {
            Some(bytes) => if let Err(e) = sender.send_bytes(&bytes).await {   // body: not cancelled
                               state.end(format!("send failed: {e}")); break }
            None        => break,                                              // Client closed/dropped
        }
    }
}
rx.close(); drop(rx);                 // queued frames of a dead session are discarded
timeout(1 s, sender.shutdown()).await
```

`rx.recv()` is cancel-safe; `send_bytes` runs in the branch body, so `select!` never drops
it. The only way to interrupt a write is aborting the writer task, which only `close()`
does after its time limit, when the session is being dropped anyway.

Alternative: keep the mutex and make every caller's send cancel-safe by spawning the write
per call (`tokio::spawn(async move { lock; send })` and await its handle). Rejected: a
spawned task per request, ordering would depend on lock acquisition inside spawned tasks,
and errors would still not reach other callers. Alternative: a `std::sync::Mutex` +
synchronous encryption into a shared buffer drained by a writer — same thing as the channel
with more locking.

### 2. What goes into the queue: encoded plaintext; the writer encrypts

`submit` encodes the `Frame` with postcard (`proto::encode`) and checks it against
`hubbridge_proto::MAX_MESSAGE_LEN` itself (returning `LinkError::TooLarge`), synchronously,
so a too-large or unencodable message fails for that caller only and never reaches the
queue (spec: Oversized messages are refused locally). No new `hubbridge-link` API is
needed; `send_bytes` keeps its own check as a safety net. The queue carries `Vec<u8>`, and
**only the writer task calls `send_bytes`**: nonces are taken and records encrypted only by
the task that writes them, in queue order. This is the "encrypt + nonce only inside the
writer task" option.

Alternative: queue `Frame`s and encode in the writer. Rejected: a size or encode error would
surface in the writer, far from the caller, and would need a per-frame reply channel.

### 3. Bounded queue (back-pressure), FIFO, reserve-then-send

`tokio::sync::mpsc::channel::<Vec<u8>>(SEND_QUEUE)` with `SEND_QUEUE = 64` frames.

- **Bounded, not unbounded:** before this change a caller waited until its frame was in the
  socket buffer, so a flood of requests was throttled by TCP and, behind it, by the agent's
  256-request limit. An unbounded queue would let the serial OUT path, the mass-storage
  pipeline or a USB/IP client grow host memory without limit while the agent is not reading
  (up to 1 MiB per transfer frame). 64 matches the agent's own inbound queue (`in_rx`, 64)
  and is far above what any current caller keeps in flight (serial: IN depth + 4 OUT; mass
  storage: one `READ(10)` data phase), so they never notice it in normal operation. Worst
  case held memory: the host only checks the 16 MiB message limit (`MAX_MESSAGE_LEN`), so
  64 queued frames can hold up to 64 × 16 MiB = 1 GiB if a caller sends maximum-size
  messages; today's callers send at most 1 MiB transfers (≈ 64 MiB), and only while the
  agent does not read.
- **Reserve, then send.** `submit` does, in this order:
  1. `permit = select! { p = tx.reserve() => p, _ = ended() => Err(connection lost) }` —
     the **only** await in `submit`. `reserve()` is cancel-safe (a dropped `reserve` takes
     no slot) and tokio's bounded channel serves waiting senders FIFO;
  2. allocate `req` (skipping 0);
  3. encode `Frame { req, msg }` and check the size — on error return it; the unused
     permit is released when dropped;
  4. insert the pending entry, refused with "connection lost: <reason>" if the session has
     ended;
  5. `permit.send(bytes)` (synchronous) and return `Pending`.

  Steps 2–5 never await, so a cancelled `submit` either did nothing (dropped while
  reserving) or queued its whole frame and returned `Pending`. The pending entry exists
  before the frame can reach the agent, so no reply is lost.
- **Ordering:** frames enter the queue in the order `submit` calls get their permits, the
  writer sends them in queue order, so one caller's sequential requests stay in order and
  concurrent callers' frames never interleave (each frame is one `send_bytes`). `req` ids are
  allocated after the permit but the frame is queued at `permit.send`, so on a multi-thread
  runtime ids are not guaranteed to follow wire order (nothing depends on it: replies are
  routed by `req`). The doc comment on `submit` changes from "in the order `submit` is
  called" to "in the order `submit` calls are accepted; sequential calls from one task keep
  their order; `submit` returns once the frame is queued, not when it is on the wire".

### 4. Shared session state: one end, seen by both tasks and every caller

`Arc<Session>` (private) replaces the bare `PendingMap`:

```
struct Session {
    state: Mutex<State>,                 // State { pending: HashMap<u32, oneshot::Sender<Message>>, ended: Option<String> }
    ended_tx: watch::Sender<bool>,       // wakes the writer, the reader and waiting submits
}
fn end(&self, reason) -> bool            // first caller wins: sets `ended`, drains `pending`, fires the watch
```

- **Writer I/O error** → `end("send failed: …")`. Draining `pending` drops the oneshot
  senders, so every `Pending::wait` returns at once. The watch wakes `submit` calls waiting
  in `reserve()` and the reader.
- **Reader end** (`Bye`, EOF, decrypt error) → `end(reason)`; the watch stops the writer
  (it discards the queue and shuts down), and new `submit`s fail at insert or while
  reserving.
- **The reader always emits the single `Disconnected`:** it `select!`s on `receiver.recv()`
  and the watch (dropping `recv()` is acceptable here: the session is over); on either it
  calls `end` (no-op if already ended) and sends `Disconnected(session.reason())`, so the
  reason is the first failure (e.g. the write error, not the EOF that follows it).
- **A panicking task ends the session too:** the writer and the reader each hold a guard
  whose `Drop` calls `end("writer stopped")` / `end("reader stopped")` when the task is
  unwinding, so pending requests fail at once and `Disconnected` is still emitted once.
  `Drop` aborts the reader before `end(..)`, so it emits no `Disconnected`. `close` aborts
  the reader only after the writer finished, so an agent EOF right after `Bye` can still
  make the reader emit one `Disconnected` before the abort.
- **Error texts:** `submit` after the end → `"connection lost: {reason}"`;
  `Pending::wait` on a dropped oneshot → `"connection lost: {reason}"` (reads the reason from
  the session; today it is just "connection lost"). `serial::port::fatal_reason` matches
  "connection lost", so a port now also stops with "connection to the phone lost" when a
  `submit` fails (today a send error reads "connection closed" and is not recognised as
  fatal); the existing `fatal_reason` unit test keeps passing.
- `Pending` keeps an `Arc<Session>` (today `PendingMap`) to remove its entry on timeout.

### 5. Close and drop

- `close(self)`: queue `Bye` through the same reserve path (bounded by the time limit), drop
  the `mpsc::Sender` so the writer sees `None` after the last queued frame, await the
  writer's `JoinHandle` with `CLOSE_TIMEOUT = 2 s` total; on timeout abort it. Then `end
  ("closed")` and abort the reader. Every frame accepted before `close` precedes `Bye`
  (`close` takes `self`, so with `Arc<Client>` it only runs once all other holders dropped
  theirs — as today).
- `Drop` (no `close`): abort the reader and the writer first, so the socket closes right
  away as it does today and no `Disconnected` is reported for our own end, then
  `end("client dropped")` (pending requests fail at once). Frames still queued are discarded: nobody can read their answers any
  more, and the agent closes the session's devices when the connection drops. `close` is
  the orderly path; `Drop` is the fallback (e.g. the desktop's `Arc::try_unwrap` fails while
  a bridge still holds the client, and the last holder drops it later).
  Alternative: let the writer drain the queue after `Drop`. Rejected: a detached task with a
  socket and no owner, for frames whose replies are lost anyway.
- Because `close` and `Drop` move fields out, the fields that need moving become `Option`s
  (`tx`, `writer` handle) taken in `close`; `Drop` handles the `None` case.

### 6. `SecureSender` is made cancel-safe too (defence in depth)

The writer task makes the host safe on its own, but `SecureSender` is a public type of
`hubbridge-link`, used by the agent and by any later tool, and its failure mode (silent
stream corruption that the peer only sees as a decrypt error) is the worst kind. The fix is
small and testable at the layer where the bug is:

- `SecureSender` gets `out: Vec<u8>` and `done: usize` (bytes of `out` already written).
- `send_bytes`: (1) finish any leftover `out[done..]` from a cancelled send; (2) size
  check; (3) encrypt all records of the new message into `out` (nonces advance here — the
  bytes are now committed and **will** be written before anything else); (4) write
  `out[done..]` with `write()` in a loop, advancing `done` after each successful write
  (a `poll_write` that returns `Pending` wrote nothing, so `done` is always exact); (5)
  `flush()`; clear `out`.
- `shutdown()`: finish leftover bytes (best effort), flush, then shut down.
- Contract in the doc comment: "Cancel-safe: if the future is dropped, the message is
  either not started or will be finished by the next `send`/`send_bytes`/`shutdown`; the
  record stream is never corrupted. A dropped send may therefore still be delivered."
- Size check before encryption so a `TooLarge` never touches `out` or the nonce.
- Errors: after an I/O error the sender is unusable as today (the session ends); no retry.

Alternative: leave `SecureSender` as is and only document "not cancel-safe, own it from one
task". Rejected: the agent's correctness would rest on `select!` body semantics only, and
the next user of the type would hit the same bug. Alternative: a "poisoned" flag that makes
the next send fail. Rejected: it turns corruption into a clean error but still kills the
session; the buffer keeps it alive for the same cost.

The agent needs no code change: its sends stay where they are and become safe by
construction.

### 7. No protocol change

The writer calls the same `send_bytes`, producing the same records (`u16` length + Noise
ciphertext), the same message framing (`u32` length + postcard split into records) and the
same nonce sequence (0, 1, 2… per direction). Only *who* calls it and *when* the bytes leave
the process change. `PROTO_VERSION` stays 1, no `Message` variant is added, hellos and key
derivation are untouched, `docs/PROTOCOL.md` is unchanged. An old agent works with a new
host and vice versa (spec: The wire format does not change).

### 8. Tests

- **Link** (`crates/link/src/lib.rs` tests): handshake over `duplex(64)`; agent side not
  reading; `timeout(50 ms, client.sender.send_bytes(200 KiB))` expires (partial write);
  then the server reads in a task and the client sends a second message; assert both
  decode in order. Plus: `TooLarge` leaves the sender usable; `shutdown` finishes a leftover.
  The existing `pair_then_resume_and_exchange` stays.
- **Host client unit tests** (`crates/host/src/client.rs` tests): `Client::start` over a
  `duplex`, with an in-test "agent" built from `hubbridge_link::server_read_hello` +
  `server_handshake` that decodes every frame and answers `Pong` / `Done`, and can be told
  to stop reading. A `FaultyWrite<W>` wrapper with a shared switch makes `poll_write` fail
  with an injected error while the read side keeps working, so the writer-error path is
  tested on its own. A test-only `Client::start_with(.., queue)` takes the queue size so a
  queue of 2 fills with a few frames.
- **E2E** (`crates/host/tests/`): a shared `common/proxy.rs` with a TCP proxy between the
  host and the real `hubbridge_agent::Agent` that can pause the host → agent direction
  (stop reading the host's socket), so the host's socket really fills. Requests to device
  id 999 (`Transfer` OUT with 1 MiB data, answered `NotOpen` at once) fill buffers without
  opening a device. Serial: the CP210x fake from `serial_e2e.rs`.

## Risks / Trade-offs

- [`submit` returns earlier than before (on queueing, not after the kernel took the
  bytes)] → A frame that waits in the queue now counts against its `Pending` timeout; it
  did before too while waiting on the lock. The queue is short compared with the socket
  buffer, so the change is small. Documented on `submit`.
- [A dropped `SecureSender::send` may still be delivered later] → Only for direct users of
  `hubbridge-link`; the host's writer is never cancelled mid-send. Documented in the
  contract.
- [Writer stuck on a peer that stops reading, socket open] → Same as today (callers would
  block on the lock); now callers can time out or be aborted safely, `close` is bounded by
  2 s, and the agent's 20 s idle timeout ends it from the other side.
- [64-frame bound too small for a future USB/IP client with deep queues] → A constant, easy
  to raise; USB/IP is limited by the agent's 256 in-flight anyway.
- [`end` reason races (write error vs EOF)] → First caller wins under the state mutex; the
  test "One disconnect event" pins it.
- [Error text change "connection lost" → "connection lost: <reason>"] → The prefix is kept,
  so `serial::port::fatal_reason` and the tests that look for it still match; grep
  `crates/` for "connection lost" / "connection closed" during implementation.

## Migration Plan

Library-internal change; no config, no stored state, no protocol version. Hosts and agents
of either version interoperate. Rollback = revert the commit. The APK does not need a
rebuild for this fix (the agent's behaviour is unchanged; the new `SecureSender` reaches the
phone with the next APK build).
