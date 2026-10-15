# Rehome — Design (working name)

Move any folder to a bigger drive and leave a directory junction behind, so every app still finds it at the original path. Built for people whose C: drive keeps filling up from `%LOCALAPPDATA%`, `%PROGRAMDATA%` and game/launcher caches.

Status: engine implemented and tested (37 tests on Linux with an emulated platform). The Windows
layer (`app/`) is written but **has not been compiled or run on Windows yet**; the first CI run on
`windows-latest` (build + end-to-end smoke test) is its first real test. License: MIT.

---

## 1. Goals and non-goals

**Goals**
- One right-click (or one pick in a small window) to relocate a folder and leave a working junction.
- Safer than doing it by hand: refuse dangerous moves, warn about risky ones, never lose data, and always be able to undo.
- Very small and fast: a single ~1–2 MB native exe, no runtime, no background service, no startup cost.
- Every refusal, warning and error says what happened, why, and what to do next.

**Non-goals (v1)**
- Moving files between drives automatically in the background / "storage sense" style cleanup.
- Network drives, cloud-synced folders, MSIX/Store app data, or anything under the Windows directory.
- Replacing a proper uninstall or "change install location" feature when an app offers one.

## 2. Design principles

1. **The original is never touched until the copy is verified.** Copy first, verify, then swap.
2. **Every step is journaled before it runs** so a crash, power cut or killed process can be recovered or rolled back on next launch.
3. **Use the OS as the lock detector.** Renaming a directory in place fails if anything holds files open inside it. That is our final "is it really not in use?" test, and it is atomic.
4. **Never recursively delete through a junction.** Removing a junction uses a link-only remove call.
5. **Refuse by default, explain always.** Unsafe = blocked with a reason. Risky = warned, with a confirm. Nothing silently skipped.
6. **Dumb and small beats clever.** No daemon, no in-process Explorer DLL in v1, minimal dependencies.

## 3. User flow

```
Right-click folder → "Move to another drive…"
  1. Checks run (≈ instant, size scan shows progress)
  2. Confirmation dialog:
       • what will move, how big, from → to
       • free space on destination after the move
       • apps using it (if detected) with "I've closed them — re-check"
       • warnings (e.g. services, hard links, exFAT)
  3. Progress: Copying → Verifying → Switching over → Cleaning up
  4. Done: "Freed 14.2 GB on C:". Link to Undo.
```

Destination default: `<Drive>:\Rehome\<original path without drive>` e.g. `D:\Rehome\Users\sam\AppData\Local\SomeGame`. Keeps names unique and recognisable. The user picks the drive (only suitable drives are offered, with free space shown).

## 4. How a move works

State machine (each transition is written to the journal first):

```
Planned
  → Copying        copy tree to <dest>.rehome-partial (never the final name)
  → Copied
  → Verified       counts, sizes, timestamps, reparse points, security checked
  → SourceRenamed  rename <src> → <src>.rehome-old   (ATOMIC IN-USE TEST — fails if anything has files open)
  → DeltaSynced    re-compare old vs copy; copy anything that changed during the copy window
  → DestPublished  rename <dest>.rehome-partial → <dest>
  → JunctionMade   create junction <src> → <dest>; read it back and confirm it resolves
  → Finalized      delete <src>.rehome-old (only now is the original data gone)
```

Why this order:
- **Copy while C: is full works** — nothing extra is written to C:. The in-place rename needs no free space.
- **The rename is the lock test.** If it fails, nothing has changed: we delete the partial copy and tell the user which apps to close. This is more reliable than guessing from process lists.
- **After the rename succeeds nobody can be writing** (no open handles), so the delta pass is race-free and the copy is exactly current.
- **Rollback at any step before Finalized** = remove junction (link only) if it exists, rename `.rehome-old` back, delete partial copy. The original is intact until Finalized.
- Deleting the old copy is the only irreversible step. Setting `keep_backup_until_reboot` (off by default) leaves it for the user to delete.

Undo ("Move back"): same machine in reverse. Copy dest → `<src>.rehome-restoring` next to the junction, verify, remove the junction (link only), rename into place, delete the relocated copy.

## 5. Safety checks

### Blocked (refuse, with reason)

| Check | Why |
|---|---|
| Not a directory / doesn't exist | Nothing to move |
| Already a junction or symlink | Offer **Move back / Repair** instead |
| Drive root, `C:\Windows`, `Program Files*`, `ProgramData` root, `Users`, the user profile root, `AppData`, `AppData\Local` itself, `AppData\Local\Microsoft\Windows` | Breaking these can make Windows or the profile unbootable/unloadable |
| `…\AppData\Local\Packages\*` (MSIX/Store apps) | Package data is protected; junctions there are unsupported and break updates |
| Destination inside source, or source inside destination | Infinite copy / data loss |
| Destination is network, optical, or removable drive | Junction would dangle when the drive disappears |
| Destination is FAT32 | No ACLs, 4 GB file limit |
| Not enough free space (needs size × 1.05 + 256 MB) | Would fail mid-copy |
| Source and destination are the same volume | Pointless (frees nothing) |
| Cloud placeholders present (OneDrive etc., `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`) | Copying would download everything, or the sync client would fight the junction |
| Folder is being synced (OneDrive/Dropbox known-folder redirection) | Sync clients and junctions don't mix |

### Warned (confirm required)

| Check | Message gist |
|---|---|
| Destination is exFAT | No permissions/streams; fine for games, risky for system data |
| Hard links inside (link count > 1) | *(not implemented in v1)* The copy duplicates them; space saved will be less than expected |
| EFS-encrypted files | Moves only if the user's key is available; otherwise refused per file |
| A Windows service has files open | We do not stop services automatically — stop it first or accept a failure |
| Folder under `ProgramData` or needs elevation | Will ask for administrator permission once |
| Destination drive may not be present at boot (e.g. unlock-later BitLocker) | Apps will see a broken folder until it's available |
| Very large (> 100 GB) | Will take a while; can be cancelled safely |

## 6. Detecting running apps

Layered, cheapest first. The first three *inform*; the rename (step 4) *decides*.

> v1 implements layer 2 (Restart Manager) and layer 4 (the rename). Layers 1 and 3 are not implemented yet.

1. **Executables running from inside the folder** — enumerate processes (`CreateToolhelp32Snapshot`), compare `QueryFullProcessImageNameW` against the folder. Catches games/launchers that live in their own data folder.
2. **Restart Manager** (`RmRegisterResources` + `RmGetList`) — Windows' own "which apps are using these files" API. We register up to a few thousand files, prioritised toward the likeliest-to-be-open types (`.db`, `.sqlite`, `.lock`, `.log`, `.dll`, `.exe`, `.pak`). Returns app names and whether each is a service. This is what the Windows installer uses.
3. **Loaded modules / current directory** under the folder — `EnumProcessModulesEx` on processes found in 1–2 plus well-known launcher names, to catch DLL locks.
4. **Authoritative: the in-place rename.** If something has handles open without delete-sharing, the rename fails with a sharing violation; we map that to "something is still using this folder" and show whatever 1–3 found.

UX: the confirm dialog lists detected apps by friendly name (icon + "Steam — running"), a **Re-check** button, and a plain statement when nothing was detected: *"No running apps found. Rehome will double-check right before switching."* We never kill processes in v1.

Limits (be honest in the UI/README): Restart Manager can miss handles held by some drivers and AV filters; we do not hunt for every handle. The rename test covers the gap for files that matter; apps that open files *only after* the switch will simply use the junction normally.

## 7. Crash safety and recovery

- Journal: `%LOCALAPPDATA%\Rehome\journal\<id>.json` (a few KB), written atomically (write temp + rename) before each state change. A copy of the origin info is also stored at `<dest>\.rehome\origin.json`, so a lost journal doesn't orphan the data.
- On every launch (cheap: list a directory) check for incomplete journals and show: *"A previous move of `…\SomeGame` didn't finish. Your original files are safe. [Resume] [Roll back]"*.
- Recovery is driven only by the recorded state and by checking what's on disk, never by assumptions.
- The registry of completed relocations (for the "Moved folders" list and Undo) is just the set of origin files: no central database to corrupt.

## 8. Error and message style

Every user-visible problem is a typed error with three fields:
- **title**: what happened, plain language
- **explanation**: why, specific (names, paths, numbers)
- **suggestion**: the next action

Example — in use:
> **Can't switch over yet — something is still using this folder.**
> Windows reported that files in `C:\Users\sam\AppData\Local\SomeGame` are open. Apps that may be using it: *SomeGame Launcher (running)*.
> **What to do:** Close those apps (check the system tray too), then click Try again. Your original files have not been changed.

Example — space:
> **Not enough room on D:.** Moving needs about 15.1 GB, but D: has 9.8 GB free. Pick another drive or free up 5.3 GB.

Rules: always say whether the user's data is safe; never show raw Win32 codes without an explanation (put the code in a "Details" expander); no jargon (say "shortcut folder", mention "junction" only in details).

## 9. Explorer integration

| Phase | Approach | Notes |
|---|---|---|
| **1 (v1)** | Installed with `rehome install` (HKCU, no admin). Registry verb: `HKCU\Software\Classes\Directory\shell\Rehome\command` → `rehome.exe move "%1"` | No DLL in Explorer (a bug can't crash Explorer), no admin, trivially removable. On Windows 11 it appears under **Show more options**. Applies to **all folders**, guarded by the blocklist. |
| **2** | `IExplorerCommand` COM handler + sparse MSIX package | Puts the entry in the Windows 11 modern menu. Requires package identity and a signing certificate — plan for signing before promising this. |

Also add a verb on junctions created by Rehome: "Move back to original drive" (detected by reading the junction target and checking `<dest>\.rehome\origin.json`).

Install/uninstall are just "write/remove these registry keys" (`rehome install` / `rehome uninstall`), no installer framework needed for v1.

## 10. Standalone window

Opens from Start or the menu. Three tabs:
1. **Find big folders** — scans `%LOCALAPPDATA%`, `%PROGRAMDATA%`, `%APPDATA%` one level deep (parallel size scan, cancellable), ranks by size, flags unsafe ones with the reason. One click → same move flow.
2. **Moved folders** — list with current location, size, drive status, **Move back**.
3. **Settings** — default destination, keep-backup option, uninstall menu entry.

Built with native Win32 (TaskDialog + ListView) to stay tiny; swap to a UI toolkit only if development speed demands it.

## 11. Architecture

```
rehome/
  Cargo.toml              workspace: rehome-core only
  crates/rehome-core/     ALL logic. No dependencies. Builds and tests on any OS.
    error.rs   typed errors (title / explanation / suggestion / data_safe)
    safety.rs  path policy: blocked / warned, pure string logic
    fsops.rs   scan, copy, diff, apply_diff (std only)
    journal.rs crash-safe key=value journal
    platform.rs the Platform trait: the only seam to the OS
    engine.rs  check / move / restore / recover with rollback
    tests/engine.rs  engine against an emulated platform with failure injection
  app/                    Windows binary; its OWN workspace (needs windows-sys)
    win.rs     Platform impl: junctions, volumes, Restart Manager
    main.rs    CLI + right-click entry point (`rehome menu "<folder>"`)
  .github/    CI, release, smoke test, dependabot
```

Why `app/` is a separate workspace: the engine must build and test on any machine (including ones
without access to Windows crates), and Windows-only dependencies would otherwise be resolved for
every build. CI builds both.

Size budget: `opt-level="z"`, LTO, `panic="abort"`, `strip`; `windows-sys` (not `windows`);
hand-rolled argument parsing; no async runtime, no serde. Target <= 1 MB.

## 12. Known risks and open questions

- **Anti-cheat / launchers** that validate install paths or file IDs might object to junctions. Mitigation: easy Undo; maintain a short "known issues" note.
- **ACL fidelity.** `CopyFileEx` copies data, attributes and streams but destination files inherit ACLs from the new parent. We copy security descriptors explicitly (owner/DACL) for directories and files; under `ProgramData`, wrong ACLs are the most likely real-world failure.
- **Drive letters can change.** Junction targets store a drive-letter path (what `mklink /J` does). Option: store the `\\?\Volume{GUID}\` path so the link survives letter changes.
- **Windows updates / feature upgrades** can recreate folders under a junction in odd cases (rare; document).
- **Sync/backup tools** may follow junctions and double-count; document.
- Do we need an elevated helper for `ProgramData`, or relaunch the CLI with `runas` on demand? (Plan: relaunch on demand.)

## 13. Roadmap

1. Core + CLI with the full state machine, tests, dry-run (`rehome check`). ✅ done (core tested)
2. Windows implementation (junctions, volumes, Restart Manager). ✍️ written, unverified: CI smoke test is the first run. Still missing: ACL/owner copy, process-image scan, hard-link warning, elevation relaunch.
3. Registry verb install/uninstall ✅ (via `reg.exe`); TaskDialog/GUI UX still to do.
4. Scanner/"Moved folders" window.
5. Windows 11 modern menu (sparse package + signing).

## 14. Licensing, repository and CI

- License: MIT (`LICENSE`). Cargo metadata says `MIT`. Adding Apache-2.0 for a dual license is just a second file plus `MIT OR Apache-2.0`.
- CI (`.github/workflows/ci.yml`), on every push and PR:
  - **core** on ubuntu + windows: rustfmt, clippy (`-D warnings`), tests.
  - **app** on windows: rustfmt, clippy, release build, then `smoke.ps1`, which drives the real exe against real junctions and the runner's D: drive: check, refusal of unsafe paths, move, junction + content readable, double-move refused, undo, and **an in-use folder (a file held open with no sharing) must be refused and left untouched, then succeed once released.** That last case validates the central assumption of §6 on real Windows.
  - uploads `rehome.exe` as a build artifact.
- Release (`release.yml`): push a `v*` tag; builds, re-runs the smoke test on the exact binary, writes a SHA-256 file, publishes a GitHub Release.
- Dependabot: cargo (both workspaces) and actions, weekly.
- Not yet: `Cargo.lock` for `app/` (generate it once on a networked machine and commit it, then add `--locked` to the CI/release builds), code signing, winget/Scoop manifests.

## 15. Supported Windows versions

- **Windows 10 and 11, 64-bit.** Rust's own floor for the MSVC targets is "Windows 10 or higher" (Windows Server 2016 on servers), so nothing in the toolchain is below it.
- Every API used exists on Windows 10 (and long before): Restart Manager (Vista), volume APIs, NTFS junction reparse points, standard file rename.
- The C runtime is linked statically (`app/.cargo/config.toml`), so a clean Windows 10 without the Visual C++ Redistributable can start the exe. CI fails if `vcruntime140`/`msvcp140` ever reappear as imports.
- On Windows 10 the classic context menu is the only menu, so the registry verb shows up directly; on Windows 11 it's under "Show more options" until the packaged (phase 2) entry exists.
- **CI cannot run real Windows 10.** GitHub retired `windows-2019` (Server 2019, the Windows 10 1809 base) in 2025. CI runs on `windows-2022` and `windows-2025`, both Windows Server. Windows 10 behaviour is covered by the manual checklist in `TESTING.md`.
- 32-bit Windows and Windows on ARM are not built.
