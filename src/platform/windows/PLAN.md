# Windows Platform Layer Rewrite Plan

## Context

The current Windows platform layer in `src/platform/windows/mod.rs` (~2166 lines) is difficult to read and maintain. It uses overlapped (async) I/O with IO Completion Ports, an `AliasedCell` abstraction for kernel-aliased memory, raw pointer casting for message headers, and pervasive `unsafe` blocks. The goal is a fresh implementation that minimises unsafe code and prioritises readability and correctness over raw performance, while preserving the crate's public API and passing all cross-platform tests.

## Architecture Overview

### I/O Strategy: Overlapped I/O, Two Modes

All pipes are created with `FILE_FLAG_OVERLAPPED`. Two distinct usage patterns:

**1. Single-receiver reads (`recv`, `try_recv`, `try_recv_timeout`):**
Use overlapped I/O with a **manual-reset event object**. The pattern is:
- Allocate a `Box<OVERLAPPED>` with an event, issue `ReadFile`
- Call `WaitForSingleObject(event, timeout)` — `INFINITE` for `recv`, `0` for `try_recv`, milliseconds for `try_recv_timeout`
- On completion: read data from buffer, done
- On timeout: `CancelIoEx`, wait for cancellation, return `NoData`

This is conceptually synchronous from the caller's perspective — we always wait for completion before returning, so the `OVERLAPPED` and buffer are never accessed while aliased by the kernel. No `AliasedCell` needed.

**2. Receiver set reads (`select`, `try_select`, `try_select_timeout`):**
Use **IO Completion Ports (IOCP)**. IOCP has no handle count limit, so it scales to any number of receivers (important for the router). The pattern is:
- When a receiver is added to the set, associate its handle with the set's IOCP and issue an overlapped read
- The pending read state (OVERLAPPED + buffer) is heap-allocated and stored in a map, keyed by handle value
- `select()` calls `GetQueuedCompletionStatus` to dequeue completions
- On completion: find the matching entry, take ownership of the pending state, extract data, re-issue a new read

The pending read data is stored in `Option<Box<PendingRead>>` per reader. Between issue and completion, the `Option` is `Some` but its contents are never accessed — only taken out after the kernel signals completion. This provides the same safety guarantee as `AliasedCell` through code structure, without the `ManuallyDrop`/`DropBomb` machinery.

A handle can only be associated with one IOCP, and once associated, events don't work on it. This is fine: receivers use event-based reads until added to a set, then switch to IOCP. The receiver is consumed when added to a set, so there's no dual-use.

### Message Framing: Explicit Byte Encoding

Replace the current `MessageHeader` raw pointer cast with explicit `u32` little-endian encoding:

```rust
const HEADER_SIZE: usize = 8;
fn encode_header(data_len: u32, oob_len: u32) -> [u8; HEADER_SIZE] { ... }
fn decode_header(bytes: &[u8; HEADER_SIZE]) -> (u32, u32) { ... }
```

Zero `unsafe` for message framing.

### Module Structure

```
src/platform/windows/
  mod.rs          -- public types, channel(), error types, constants, re-exports
  handle.rs       -- WinHandle RAII wrapper, dup/move helpers
  sender.rs       -- OsIpcSender
  receiver.rs     -- OsIpcReceiver (event-based overlapped reads)
  receiver_set.rs -- OsIpcReceiverSet (IOCP-based)
  shared_memory.rs -- OsIpcSharedMemory
  oob.rs          -- OutOfBandMessage, serialization
  tests.rs        -- fresh Windows-specific tests
```

`aliased_cell.rs` is deleted entirely.

## Files Changed

| File | Action |
|------|--------|
| `src/platform/windows/mod.rs` | Rewrite (slim coordinator, re-exports) |
| `src/platform/windows/handle.rs` | New (WinHandle + dup helpers) |
| `src/platform/windows/sender.rs` | New (OsIpcSender) |
| `src/platform/windows/receiver.rs` | New (OsIpcReceiver) |
| `src/platform/windows/receiver_set.rs` | New (OsIpcReceiverSet) |
| `src/platform/windows/shared_memory.rs` | New (OsIpcSharedMemory) |
| `src/platform/windows/oob.rs` | New (OutOfBandMessage) |
| `src/platform/windows/tests.rs` | Rewrite from scratch |
| `src/platform/windows/aliased_cell.rs` | Delete |

No changes to `src/platform/mod.rs`, `src/ipc.rs`, unix, macos, or inprocess code.

## Detailed Design

### 1. `handle.rs` — WinHandle and Handle Utilities

```rust
pub(crate) struct WinHandle { handle: HANDLE }
```

- RAII `Drop` calls `CloseHandle` (one `unsafe` block)
- `new()`, `invalid()`, `is_valid()`, `as_raw()`, `take()` — safe method bodies except `Drop`
- `unsafe impl Send` / `unsafe impl Sync` (required, documented)
- Free functions with safe signatures:
  - `dup_handle(h: &WinHandle) -> Result<WinHandle, WinError>`
  - `dup_handle_to_process(h: &WinHandle, process: &WinHandle) -> Result<WinHandle, WinError>`
  - `move_handle_to_process(h: WinHandle, process: &WinHandle) -> Result<WinHandle, WinError>` — uses `DUPLICATE_CLOSE_SOURCE`, forgets source
  - `open_process_for_dup(pid: u32) -> Result<WinHandle, WinError>`

Static helpers (same as current):
- `CURRENT_PROCESS_ID: LazyLock<u32>`
- `CURRENT_PROCESS_HANDLE: LazyLock<WinHandle>`

### 2. `oob.rs` — Out-of-Band Message

Same structure as current code, cleaned up:

```rust
pub(crate) struct OutOfBandMessage {
    pub target_process_id: u32,
    pub channel_handles: Vec<isize>,
    pub shmem_handles: Vec<(isize, u64)>,
    pub big_data_receiver_handle: Option<(isize, u64)>,
}
```

- `Serialize`/`Deserialize` via tuple (zero unsafe)
- `needs_to_be_sent(&self) -> bool`
- `recover_handles(&mut self) -> Result<(), WinError>` — delegates to `handle.rs` helpers
- `new(target_id: u32) -> Self`

### 3. `shared_memory.rs` — OsIpcSharedMemory

```rust
pub struct OsIpcSharedMemory {
    handle: WinHandle,
    ptr: *mut u8,
    length: usize,
}
```

- `new(length)`: `CreateFileMappingA` + `MapViewOfFile`
- `from_handle(handle, length)`: `MapViewOfFile`
- `from_bytes(bytes)`: `new()` + `ptr::copy_nonoverlapping`
- `from_byte(byte, length)`: `new()` + fill via `slice::from_raw_parts_mut`
- `Deref<[u8]>`: `slice::from_raw_parts`
- `deref_mut()`: `slice::from_raw_parts_mut` (already `unsafe` in API)
- `take()`: copies to `Vec<u8>` via `Deref` (safe)
- `Clone`: `dup_handle` + `from_handle`
- `Drop`: `UnmapViewOfFile`
- `unsafe impl Send/Sync`

Unsafe here is irreducible — memory-mapped I/O inherently requires it.

### 4. `receiver.rs` — OsIpcReceiver

```rust
pub struct OsIpcReceiver {
    handle: RefCell<WinHandle>,
    read_buf: RefCell<Vec<u8>>,
}
```

Interior mutability via `RefCell` (required because `recv()` takes `&self`).

**Core read helper — event-based overlapped I/O:**

```rust
fn read_once(handle: HANDLE, buf: &mut Vec<u8>, timeout: u32) -> Result<usize, WinIpcError> {
    let event = /* CreateEventA */;
    let mut ov = Box::new(OVERLAPPED { hEvent: event, ..zeroed() });
    // Extend buf capacity, issue ReadFile into spare capacity
    // WaitForSingleObject(event, timeout)
    // If WAIT_TIMEOUT: CancelIoEx + GetOverlappedResult(wait=true) -> return NoData
    // If WAIT_OBJECT_0: GetOverlappedResult -> update buf.len(), return bytes read
}
```

The `OVERLAPPED` lives in a `Box` (stable address). We always wait for completion or cancel before returning, so the kernel alias is resolved before we touch the data. No `AliasedCell` needed.

**`recv()` / `try_recv()` / `try_recv_timeout()` flow:**

1. Check `read_buf` for a complete message (safe byte-slice parsing with `decode_header`).
2. If no complete message, call `read_once(handle, &mut read_buf, timeout)`.
3. For `recv()`: loop with `INFINITE` timeout until a complete message is buffered.
4. For `try_recv()`: single call with timeout `0`. If `NoData` and no complete message, return `NoData`.
5. For `try_recv_timeout(d)`: first call with the full duration. If partial message received, loop with `INFINITE` (commit to reading the rest).
6. Parse the message: extract data bytes, OOB bytes, decode handles/shmem, handle big-data side-channel.
7. Drain consumed bytes from `read_buf`.

**Other methods:**
- `consume(&self)`: Takes handle and read_buf out of RefCells.
- `new_named(pipe_name)`: `CreateNamedPipeA` with `FILE_FLAG_OVERLAPPED`.
- `accept()`: `ConnectNamedPipe` with overlapped + wait.
- `prepare_for_transfer()`: Checks read_buf is empty (no pending partial reads).

### 5. `sender.rs` — OsIpcSender

```rust
pub struct OsIpcSender { handle: WinHandle }
```

**`send()` data flow:**
1. If ports or shmem regions present, get server process handle via `GetNamedPipeServerProcessId`.
2. Build `OutOfBandMessage`, duplicating/moving handles to target process.
3. Check if data exceeds `MAX_FRAGMENT_SIZE`. If so, create a side-channel, put its receiver handle in the OOB.
4. Encode message header (safe: `u32::to_le_bytes`).
5. Write header + data + OOB bytes atomically via `write_all()`.
6. If big data, write full payload to side-channel sender.

**`write_all()` helper:** wraps `WriteFile` in a loop. Single `unsafe` block per call.

**`connect(name)` / `connect_named(pipe_name)`:** wraps `CreateFileA`. Single `unsafe` block.

**`clone()`:** uses `dup_handle()`.

### 6. `receiver_set.rs` — OsIpcReceiverSet (IOCP)

```rust
pub struct OsIpcReceiverSet {
    incrementor: RangeFrom<u64>,
    iocp: WinHandle,
    readers: Vec<SetReader>,
    closed_ids: Vec<u64>,
}

struct SetReader {
    entry_id: u64,
    handle: HANDLE,           // raw handle (not WinHandle — ownership is with PendingRead)
    read_buf: Vec<u8>,        // accumulated data not yet forming a complete message
    pending: Option<PendingRead>,  // in-flight async read state
}

struct PendingRead {
    handle: WinHandle,        // pipe handle, moved here during async read
    overlapped: Box<OVERLAPPED>,
    buffer: Vec<u8>,          // buffer with spare capacity for the read
}
```

**State management:** When an async read is in flight, `pending` is `Some`. The handle, overlapped, and buffer are inside `PendingRead` and must not be accessed. When the IOCP signals completion, we `take()` the `PendingRead`, extract the bytes, move the handle back, and append data to `read_buf`.

**`add(receiver)`:**
1. Consume the receiver, extract handle and read_buf.
2. Associate handle with IOCP via `CreateIoCompletionPort`.
3. Issue an overlapped read, storing state in `pending`.
4. Push a `SetReader` onto `readers`.

**`select()`:**
1. Report any `closed_ids` first.
2. Loop: call `GetQueuedCompletionStatus(INFINITE)`.
3. Find the `SetReader` by completion key (handle value).
4. Take `pending`, call `GetOverlappedResult` to get byte count, append to `read_buf`.
5. Try to extract complete messages from `read_buf`.
6. Re-issue an overlapped read (or mark as closed if `ERROR_BROKEN_PIPE`).
7. Return when at least one result is available.

**`try_select()` / `try_select_timeout()`:** Same but with timeout `0` / duration. `WAIT_TIMEOUT` from `GetQueuedCompletionStatus` returns `OsTrySelectError::Empty`.

**Drop:** Cancel all in-flight reads via `CancelIoEx`, wait for completions to drain, then drop handles.

**Why this is simpler than the current code:**
- No `AliasedCell` — we use `Option<PendingRead>` and only access after `take()`
- No `NoDebug` wrapper — `OVERLAPPED` is in a `Box`, `Debug` derived on enclosing structs with `#[derive(Debug)]` where possible, manual impl where not
- No `Overlapped` newtype with `Drop` for event cleanup — the event is managed directly
- The `SetReader` state machine has only two states: reading (pending is Some) and idle (pending is None), which is clear from the `Option`

### 7. `mod.rs` — Coordinator

Slim file containing:
- Module declarations and re-exports
- `channel()` function (creates named pipe pair)
- `OsIpcChannel` enum
- `OsOpaqueIpcChannel` struct
- `OsIpcOneShotServer` struct
- `WinIpcError` enum and conversions to `crate::IpcError`, `crate::TryRecvError`, `io::Error`
- `OsTrySelectError` enum
- Constants (`MAX_FRAGMENT_SIZE`, `PIPE_BUFFER_SIZE`)
- `make_pipe_id()`, `make_pipe_name()` helpers
- `win32_trace!` macro (keep for debugging)

### 8. `tests.rs` — Fresh Tests

**WinHandle tests:**
- Create and drop
- `take()` invalidates the original
- `dup_handle` produces a distinct but equivalent handle
- `move_handle_to_process` invalidates the source

**Channel basic tests:**
- Send/receive small data
- Send/receive empty data
- Send/receive at MAX_FRAGMENT_SIZE boundary (just under, exactly, just over)
- Multiple sends then multiple receives (ordering preserved)
- Sender clone: both clones can send, receiver gets all messages
- Drop all senders, receiver gets ChannelClosed

**Non-blocking tests:**
- `try_recv` returns NoData when empty
- `try_recv` returns data when available
- `try_recv_timeout` returns NoData on expiry
- `try_recv_timeout` returns data before timeout

**Handle transfer tests:**
- Send a sender through a channel, use it to send
- Send a receiver through a channel, use it to receive
- Send multiple channels in a single message

**Shared memory tests:**
- `from_bytes` / `Deref` round-trip
- `from_byte` fills correctly
- `clone` produces independent view of same data
- `take` returns the data as a Vec

**ReceiverSet tests:**
- Add one receiver, select gets its message
- Add multiple receivers, messages route correctly
- Sender close reports ChannelClosed
- `try_select` returns Empty when no data
- `try_select_timeout` returns data within timeout
- Add already-closed channel, get ChannelClosed on next select

**OneShotServer tests:**
- Create, connect, accept + receive first message
- Server name is a valid UUID string

**Big data tests:**
- Data larger than MAX_FRAGMENT_SIZE round-trips correctly
- Big data with channel handles in OOB

## Unsafe Code Summary

| Location | Unsafe Usage | Reason |
|----------|-------------|--------|
| `handle.rs` Drop | `CloseHandle` | FFI, irreducible |
| `handle.rs` dup/move | `DuplicateHandle`, `OpenProcess` | FFI, irreducible |
| `handle.rs` | `unsafe impl Send/Sync` | HANDLE is a transferable integer |
| `sender.rs` write | `WriteFile` | FFI |
| `sender.rs` connect | `CreateFileA` | FFI |
| `sender.rs` pid query | `GetNamedPipeServerProcessId` | FFI |
| `receiver.rs` read | `ReadFile`, `GetOverlappedResult` | FFI |
| `receiver.rs` wait | `WaitForSingleObject` | FFI |
| `receiver.rs` cancel | `CancelIoEx` | FFI |
| `receiver.rs` event | `CreateEventA` | FFI |
| `receiver.rs` pipe creation | `CreateNamedPipeA` | FFI |
| `receiver.rs` accept | `ConnectNamedPipe` | FFI |
| `receiver.rs` | `mem::zeroed::<OVERLAPPED>()` | FFI struct init |
| `receiver_set.rs` IOCP | `CreateIoCompletionPort`, `GetQueuedCompletionStatus` | FFI |
| `receiver_set.rs` read | `ReadFile`, `CancelIoEx` | FFI |
| `shared_memory.rs` | `CreateFileMappingA`, `MapViewOfFile`, `UnmapViewOfFile` | FFI |
| `shared_memory.rs` | `slice::from_raw_parts[_mut]`, `ptr::copy_nonoverlapping` | Memory-mapped access |
| `shared_memory.rs` | `unsafe impl Send/Sync` | Mapped memory valid for lifetime |

**Eliminated unsafe compared to current:**
- No `AliasedCell` (`ManuallyDrop`, `DropBomb`, unsafe alias methods)
- No raw pointer casting for `MessageHeader` (use `u32::from_le_bytes` instead)
- No `set_len()` tricks on vectors to expose uninitialised capacity to the kernel (use spare capacity directly)
- No complex state machine with unsafe transitions between async states

**All remaining unsafe is irreducible FFI calls**, each wrapped in a small function with a safe signature.

## Risks and Mitigations

1. **IOCP complexity in receiver_set:** Mitigated by using a simple `Option<PendingRead>` state rather than `AliasedCell`. The two-state model (idle/reading) is enforced by the `Option`.

2. **Cancellation on Drop:** `CancelIoEx` + drain completions. Same approach as current code but simpler since we don't need to distinguish between set and non-set readers.

3. **Big data side-channel with overlapped I/O:** The side-channel is a fresh pipe used exclusively for one transfer. The receiver does overlapped reads with `INFINITE` wait in a loop until all data is received. Straightforward.

4. **`WaitForSingleObject` on pipe handles:** Pipe handles are waitable objects on Windows. When data arrives, the handle is signalled. This is documented behaviour. If a specific Windows version doesn't support this for overlapped pipes, the fallback is to wait on the OVERLAPPED's event object instead (which is always valid for overlapped I/O). **Plan: always wait on the event object, not the pipe handle** — this is guaranteed to work.

5. **Cross-process handle transfer:** Unchanged in principle. `DuplicateHandle` is the only Windows mechanism. The unsafe FFI calls are wrapped in safe functions in `handle.rs`.

## Implementation Steps (Ordered)

1. **Create `handle.rs`**: WinHandle struct, Drop, dup/move helpers, static process handle/id.
2. **Create `oob.rs`**: OutOfBandMessage, Serialize/Deserialize, recover_handles.
3. **Create `shared_memory.rs`**: OsIpcSharedMemory with all methods.
4. **Create `receiver.rs`**: OsIpcReceiver with event-based overlapped reads, message framing.
5. **Create `sender.rs`**: OsIpcSender with write logic, handle transfer, big-data side-channel.
6. **Rewrite `mod.rs`**: Coordinator with channel(), enums, error types, re-exports.
7. **Create `receiver_set.rs`**: OsIpcReceiverSet with IOCP.
8. **Delete `aliased_cell.rs`**.
9. **Rewrite `tests.rs`**: Fresh tests covering all components.
10. **Run cross-platform test suite** (`src/platform/test.rs`) to verify compatibility.

## Verification

1. `cargo test --target x86_64-pc-windows-msvc` — all tests pass
2. `cargo test --target x86_64-pc-windows-msvc -- platform::test` — cross-platform tests pass
3. `cargo test --target x86_64-pc-windows-msvc -- platform::windows::tests` — new Windows tests pass
4. `cargo clippy --target x86_64-pc-windows-msvc` — no warnings
5. Manual review: confirm every `unsafe` block is a minimal FFI wrapper
