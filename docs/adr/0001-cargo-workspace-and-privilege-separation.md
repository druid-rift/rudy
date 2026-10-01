# ADR 0001: Cargo Workspace Structure and Privilege Separation

## Status
**Superseded by [ADR 0003](0003-flatpak-packaging-and-udisks2-privilege.md)** (2026-08-22).

Retained for history. The privilege-separation *reasoning* below still holds and is carried
forward by 0003 — a desktop GUI must not run with ambient root, and application code must
never handle passwords. What changed is the *mechanism*: the project moved to Flatpak
distribution with no system install, which makes a pkexec-launched helper binary at
`/usr/lib/rudy/` impossible. udisks2 over the system D-Bus replaces it.

The Windows half of this ADR is withdrawn entirely: Rudy is Linux-only as of the same
re-scope.

Original text follows.

---

## Status (original)
Accepted

## Context
Rudy requires low-level block storage access to format, partition, lock, and write bootloader artifacts directly to raw physical disk devices (`/dev/sdX` on Linux, `\\\\.\\PhysicalDriveN` on Windows).

However, running a full desktop GUI application (Slint) or complex terminal client with ambient root/administrator privileges introduces severe security vulnerabilities and desktop integration issues:
- Graphical environments (Wayland/X11) should never run as root.
- User interface crashes or memory bugs in UI libraries should not compromise the root security boundary.
- Credentials / passwords must never be captured or held in application memory.

## Decision

1. **Multi-Crate Cargo Workspace**:
   - `rudy-core`: Pure, unprivileged domain models, partition math, MBR/GPT table encoders, Rudy disk signatures, and streaming Zstd decoders.
   - `rudy-platform`: Unified `StoragePlatform` facade for drive discovery, safety blacklists, unmounting, and raw I/O across Linux and Windows.
   - `rudy-worker`: Minimal, self-contained elevated helper binary responsible exclusively for acquiring locks, formatting partitions, and streaming writes.
   - `rudy-cli`: Headless terminal interface.
   - `rudy-gui`: Desktop graphical interface built with Slint.

2. **Privilege Boundary & OS Authentication**:
   - **Linux**: The unprivileged client spawns `rudy-worker` via Polkit (`pkexec`). The system's standard authentication agent prompts the user natively on the desktop. The application itself never requests, handles, or stores passwords.
   - **Windows**: The unprivileged client spawns `rudy-worker` via UAC (`ShellExecuteExW` with verb `runas`).

3. **IPC Protocol**:
   - The worker streams one newline-delimited JSON (`ProgressEvent`) protocol.
     Linux carries it over captured `stdout`; Windows carries it over a local,
     authenticated named pipe because `ShellExecuteExW("runas")` cannot provide
     reliable redirected standard handles across UAC elevation.
   - The Windows launcher creates a restricted, non-remote pipe with an
     unpredictable name and token. A PID-bound hello/acknowledgement handshake
     must complete before the elevated worker accesses the target. Physical-disk
     operations require this transport; stdout is permitted on Windows only for
     explicit disk-image workflows.
   - The client parses events asynchronously on a background thread and dispatches updates to the UI/CLI.

## Consequences
- Clean separation of concerns with 0 privilege leakage.
- Industry-standard security compliance.
- Fast, deterministic mock block testing in user-space without root requirements.
