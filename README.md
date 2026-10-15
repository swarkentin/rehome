# Rehome

Move a big folder to another drive and leave a **directory junction** behind, so every app still finds it at the original path.

Built for people whose `C:` drive keeps filling up from `%LOCALAPPDATA%` and `%PROGRAMDATA%`: game launchers, caches, downloads, screenshots. It automates the manual trick (move the folder, `mklink /J`) and removes the ways it goes wrong.

> **Status: pre-release.** The engine is tested; the Windows layer is new and hasn't been run on real Windows outside CI yet. Try it on a folder you don't mind re-downloading first.

## Requirements

Windows 10 or 11, 64-bit, with the folder on an NTFS drive and the destination on another internal drive. Nothing else to install (no .NET, no Visual C++ runtime).

## Install

There's no installer yet; it's one file.

1. Download `rehome.exe` from the [Releases](../../releases) page (verify it with the `.sha256` file).
2. Put it somewhere permanent, e.g. `C:\Tools\Rehome\`.
3. Run `rehome install`. This adds **Move to another drive...** to the folder right-click menu (current user only, no admin needed). On Windows 10 it appears directly in the menu; on Windows 11 it's under **Show more options**.

Remove it with `rehome uninstall` and delete the exe. Folders you've already moved keep working: they're ordinary junctions.

Windows may show a SmartScreen warning because the exe isn't code-signed yet.

## Use

Right-click a folder, choose **Move to another drive...**, pick a drive, confirm. Or from a terminal:

```
rehome check  "C:\Users\me\AppData\Local\SomeGame" --to D:\    # look, change nothing
rehome move   "C:\Users\me\AppData\Local\SomeGame" --to D:\
rehome undo   "C:\Users\me\AppData\Local\SomeGame"             # bring it back
rehome recover                                                 # after a crash or power loss
```

Right-click a folder Rehome already moved and the same menu entry offers to move it back.

## How it stays safe

- **Copy, verify, then switch.** The original is deleted last, only after the junction exists and has been read back.
- **In-use detection.** Renaming a folder fails while anything has files open in it, so that rename is the final, authoritative check. Windows Restart Manager adds a friendly list of which apps to close.
- **Rollback.** Any failure before the end puts the original back. A journal makes interrupted moves recoverable.
- **Refuses dangerous moves:** Windows, Program Files, whole profiles, `AppData` itself, Store app data, network and removable drives, folders with links or cloud-only files, not enough space.
- **Plain-language errors** that say what happened, why, and what to do.

Details: [DESIGN.md](DESIGN.md).

## Build and test

```
cargo test --workspace                                     # engine, any OS
cd app && cargo build --release                            # Windows binary, on Windows
./.github/scripts/smoke.ps1 -Exe app/target/release/rehome.exe   # end-to-end, on Windows
```

CI runs all of this on every push.

## License

MIT. See [LICENSE](LICENSE).
