# host-session Specification

## Purpose
Defines how the desktop host sends on an established session with an agent: requests from
any number of callers reach the agent whole and in order, an abandoned request never breaks
the session, and a failure or a close ends the session cleanly for every caller.

## Requirements

### Requirement: An abandoned request never breaks the session
When a caller stops waiting for a request it is sending — a timeout around it, an aborted
task, a `select!` that drops it — the session SHALL stay usable: the request is either sent
whole or not at all, nothing partial reaches the agent, and later requests on the same
session SHALL succeed.

#### Scenario: Request dropped while the socket is full
- **WHEN** the agent's side stops reading so the host's socket send buffer is full, and a caller's request future is dropped by a timeout while it waits to be sent
- **THEN** after the agent reads again, later requests on the same session get their answers, the agent decrypts every frame it receives, and the session is not closed by either side

#### Scenario: Task aborted while sending
- **WHEN** a task is aborted while one of its requests waits to be sent
- **THEN** other callers' requests on the session still get their answers and no `Disconnected` event is emitted

#### Scenario: Serial port closed with expired budgets
- **WHEN** a serial port is closed while the socket is full, so its close budgets (300 ms per control request while closing, 1 s for the chip, 500 ms per release and close) run out and its IN, OUT and poller tasks are aborted in the middle of requests
- **THEN** once the agent reads again the session is still open: a ping succeeds, the device list can be read, and a device can be opened again on the same session

### Requirement: Frames stay whole and in order
Every frame the host sends SHALL reach the agent whole, never interleaved with another
frame, including frames that span several records. Frames SHALL reach the agent in the
order their requests were accepted for sending, so requests one caller makes one after
another arrive in that order.

#### Scenario: Concurrent callers
- **WHEN** eight tasks each send 50 requests one after another on one session, with payloads from a few bytes to more than one record (over 64 KiB)
- **THEN** the agent decodes all 400 frames with their payloads intact, and each task's requests arrive in the order that task sent them

#### Scenario: Pipelined transfers on one endpoint
- **WHEN** a caller submits ten OUT transfers to one endpoint without waiting and then reads them back
- **THEN** the agent receives them in submission order and the bytes come back in that order

### Requirement: Sending is bounded
The host SHALL keep at most a fixed number of frames queued for sending per session. When
the queue is full, a new request SHALL wait for space instead of growing memory; that wait
SHALL be cancellable without effect on the session, and the queued frames keep their order.

#### Scenario: Agent stops reading
- **WHEN** the agent stops reading and a caller keeps submitting requests
- **THEN** once the queue is full the next submit waits, the host's memory for queued frames stays bounded, and when the agent reads again the waiting submit is accepted and every queued frame is delivered in order

#### Scenario: Giving up on a full queue
- **WHEN** a submit waiting for queue space is cancelled by a timeout
- **THEN** its frame is never sent, its request id gets no pending entry, and the frames already queued are still delivered

### Requirement: A failed write ends the session for every caller
When writing to the session fails, the host SHALL end the session at once: every request
waiting for an answer SHALL fail with a "connection lost" error, every request waiting to be
sent and every later request SHALL fail without waiting for a timeout, and the event stream
SHALL get exactly one `Disconnected` with the reason.

#### Scenario: Write error with requests pending
- **WHEN** a write to the socket fails while requests wait for answers and others wait in the send queue
- **THEN** within one second all of them fail with an error saying the connection was lost, without waiting for their request timeouts

#### Scenario: Request after the failure
- **WHEN** a caller submits a request after a write failed
- **THEN** the submit fails at once with an error naming the reason, and nothing is queued

#### Scenario: One disconnect event
- **WHEN** a write fails and the read side then also sees the connection close
- **THEN** the event stream gets exactly one `Disconnected`, carrying the write failure as the reason

### Requirement: A closed connection ends sending too
When the read side sees the session end (the agent says `Bye`, closes the connection, or
sends something that cannot be decrypted), the host SHALL stop accepting requests on that
session: later submits and submits waiting for queue space SHALL fail at once.

#### Scenario: Agent closes the session
- **WHEN** the agent ends the session and a caller then submits a request
- **THEN** the submit fails at once with an error saying the connection was lost, instead of timing out after 10 s

### Requirement: Oversized messages are refused locally
A request whose encoded message is larger than the protocol's maximum message size SHALL
fail for that caller before anything is queued, and the session SHALL stay usable.

#### Scenario: Message over the limit
- **WHEN** a caller submits a message that encodes to more than 16 MiB
- **THEN** the submit fails with a "message too large" error and the next request on the session gets its answer

### Requirement: Closing sends everything first
Closing a session SHALL send every frame already accepted, then `Bye`, then shut the write
side down, and SHALL finish within a bounded time even when the agent no longer reads.

#### Scenario: Close after pipelined requests
- **WHEN** a caller submits several requests without waiting and then closes the session
- **THEN** the agent receives all of those requests and then `Bye`, and ends the session as an orderly close

#### Scenario: Close with a stuck peer
- **WHEN** the session is closed while the agent has stopped reading and the socket is full
- **THEN** close returns within its time limit (2 s) and the connection is dropped

### Requirement: The secure link sender is cancel-safe
A send on the encrypted link that is dropped part way SHALL not corrupt the record stream:
the message is either not started or completed by the next send or shutdown on that link,
and the receiver SHALL decrypt every record in order.

#### Scenario: Send dropped mid-write on a tiny pipe
- **WHEN** a sender writes a 200 KiB message into a pipe with a 64-byte buffer that nobody reads, the send is dropped after a timeout, and then the receiver starts reading and the sender sends a second message
- **THEN** the receiver gets the first message and then the second, both intact

### Requirement: The wire format does not change
The records, the message framing and the Noise nonce sequence on the wire SHALL stay as
protocol version 1 defines them; the protocol version SHALL not change, and an agent and a
host of protocol version 1 SHALL keep working together whichever of them has this change.

#### Scenario: Agent without the change
- **WHEN** a host with this change pairs with and uses an agent whose session code is unchanged
- **THEN** pairing, resume, requests and events work as before, and both report protocol version 1
