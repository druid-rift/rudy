# 0002. Asynchronous Progress Event Pipeline and Disk Operation Traits

- **Status:** Accepted; amended 2026-08-22, 2026-08-30 and 2026-09-13 (see the Amendments below)
- **Date:** 2026-08-18
- **Context:** Rudy Project — originally an early design ticket, an effort deleted 2026-08-30 and recoverable from git history.

## Context and Problem Statement

Disk partitioning, image flashing (32 MiB RUDYEFI), and filesystem formatting take varying amounts of time depending on USB bus speeds (USB 2.0 vs 3.2 Gen 2). The CLI and Slint GUI require real-time byte-level progress updates, stage transitions, cancellation support, and error propagation without blocking UI rendering or main threads.

## Decision Outcome

1. **Async Runtime:** Standardize on **Tokio** (`tokio = { version = "1.40", features = ["full"] }`) with `tokio-util` cancellation tokens.
2. **Event Pipeline:** Use structured event channels (`tokio::sync::mpsc`) streaming `ProgressEvent` instances.
3. **Core Disk Traits:** Define decoupled traits in `rudy-core`:
   - `DiskEnumerator`: Asynchronously scans and filters host block devices.
   - `DiskLocker`: Safely unmounts and claims exclusive locks on target drives.
   - `RawDiskWriter`: Sector-aligned direct I/O writer.
   - `AssetProvider`: Provides zstd-compressed bootloader payloads and manifests.
4. **FAT Filesystem Generation:** Use pure Rust `fatfs` crate to construct and populate Partition 2 (`RUDYEFI`) FAT16/32 filesystem in-memory or streamed directly, avoiding external `mkfs.vfat` dependencies.

### Event Schema

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProgressEvent {
    PhaseChanged { phase: InstallPhase, description: String },
    ByteProgress { stage_bytes_written: u64, stage_total_bytes: u64, total_percent: f32 },
    Warning { message: String },
    Log { message: String },
    Completed { partition1_label: String, partition2_label: String },
    Failed { error: String },
}
```


---

## Amendment (2026-08-22)

The re-scope to Flatpak + udisks2 ([ADR 0003](0003-flatpak-packaging-and-udisks2-privilege.md))
changes what carries these events, not the event model itself.

- **`ProgressEvent` survives unchanged.** It remains the structured progress vocabulary for
  both the GUI and CLI.
- **The newline-delimited JSON transport is retired** along with the elevated worker
  process. With no separate privileged binary there is nothing to serialise across; events
  are dispatched in-process over `tokio::sync::mpsc` as originally designed.
  `WorkerProtocolTracker` and the worker protocol module retire with it.
- **`DiskLocker` and `RawDiskWriter` are re-homed.** udisks2 owns the device claim and the
  authenticated descriptor, so these traits are implemented against udisks2 objects rather
  than a retained `O_EXCL` file descriptor. The obligation they encode — re-verify identity
  after the claim and before mutation — is unchanged and must survive the move.
- **`AssetProvider` survives unchanged**, but now provides a payload this project builds
  from pinned GRUB2 source ([ADR 0004](0004-uefi-only-grub2-payload.md)) rather than a
  third-party bundle.
- **Cancellation** (`tokio-util` cancellation tokens) was never implemented and remains
  open. It is simpler under the in-process model than it was across a process boundary.

---

## Amendment (2026-08-30): the async runtime is gone

**Tokio and `tokio-util` were removed from the workspace.** They were declared — `tokio`
with `features = ["full"]` in `rudy-worker` and `rudy-gui` — and used by nothing: no `src/`,
`tests/` or `build.rs` file in any crate referenced either. Removing them dropped 148
transitive crates, 610 → 462.

Decision Outcome point 1 is therefore **superseded**: there is no async runtime, and adding
one back needs its own justification rather than inheriting this ADR's.

What was actually built is synchronous throughout. The privileged path is a blocking
descriptor and blocking D-Bus calls (`zbus`'s `blocking` API, chosen for the same reason —
see the comment on the dependency in `Cargo.toml`), and progress is reported by callback
rather than over a channel. Point 2's `tokio::sync::mpsc` was never wired up either; the
2026-08-22 amendment above repeated the intention without checking, which is how a dependency
survived four months of not being used.

`ProgressEvent`, the disk traits and `AssetProvider` are unaffected — they are data and
interfaces, not runtime. **Cancellation is still open**, and is now a question about how to
interrupt a blocking write rather than about cancellation tokens.

---

## Amendment (2026-09-13): the event stream is not a result, and it was never a disk seam

Made under AR-12, and deliberately whole: AR-18 was carrying a correction to this ADR too, and
two tickets each writing half of one amendment is how the document came to disagree with
itself.

**`ProgressEvent` has no terminal variants.** The Event Schema above lists `Completed` and
`Failed`. Nothing ever constructed either: completion and failure were `run_install`'s return
value, respec 07 had already found `Failed` dead in the GUI, and the rule was held by a test
watching the stream for the two variants' absence. Both are deleted. So are the labels
`InstallCompletion` carried, which no client read — the entry points return
`Result<(), InstallError>`. (`Warning`, also in the schema, does not exist in the code.)

**The serde derives are gone from `ProgressEvent` and `InstallPhase`.** They were the wire
format of the newline-delimited worker protocol the 2026-08-22 amendment retired, and nothing
has serialized an event since.

**Progress has two measures, and neither is completion.** Stage progress is the bytes of the
stage in hand. Overall progress is a fixed mapping from the stage, decided in
`rudy_platform::install` rather than handed in by each caller, and it stops short of 100 by the
work still owed after the last byte. A client shows 100 from `Ok` and from nothing else.
`CONTEXT.md` §2 states the contract.

**The disk traits were never re-homed.** The 2026-08-22 amendment says `DiskLocker` and
`RawDiskWriter` "are implemented against udisks2 objects". They are implemented against
nothing: `rudy_core::traits` has no implementor anywhere, and the udisks2 claim lives in
`rudy_platform::authorized_target`, which never names them. The 2026-08-30 amendment then
called them "unaffected" — four paragraphs after diagnosing this exact failure about Tokio, an
intention repeated without checking. The seam that was actually built is the Authorized Target
Session, with a private `SessionEffects` for its tests; the traits are AR-18's to delete.

*AR-18 deleted `rudy_core::traits` on 2026-09-13, with no adapter left in its place.*
