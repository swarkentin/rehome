# Manual test checklist (Windows 10)

CI covers Windows Server 2022 and 2025 but not a real Windows 10 client. Before a release, run this
once on a Windows 10 22H2 machine or VM (a clean one, to prove nothing extra needs installing).
You need a second internal NTFS drive (add a virtual disk to the VM).

1. **Starts on a clean machine.** Copy `rehome.exe` over, run `rehome --version`. It must start with
   no "VCRUNTIME140.dll was not found" error.
2. **Menu entry.** `rehome install`, then right-click any folder in Explorer. "Move to another
   drive..." should be in the menu directly (not under "Show more options").
3. **Basic move and undo.** Make `C:\Users\<you>\AppData\Local\RehomeTest` with a few files, including
   a ~1 GB one. Right-click it, move it to the second drive. Check:
   - `dir` shows it as `<JUNCTION>`; files open normally through the original path.
   - Move back via the same menu entry; the junction is gone and the files are intact.
4. **In use is refused.** Open a file from the folder in an app that keeps it open (for example a
   text editor, or `powershell: [IO.File]::Open(path,'Open','ReadWrite','None')`). Try to move: you
   should get "something is still using this folder", the folder should be unchanged, and moving
   should work after closing the app.
5. **Refusals read well.** Try `C:\Windows`, `C:\Program Files`, `%LOCALAPPDATA%` itself, a USB
   stick as destination. Each should say what's wrong and what to do.
6. **Interrupted move.** Start a move of a large folder, close the console window mid-copy, then run
   `rehome recover`. The original must be intact and no `.rehome-partial` / `.rehome-old` folders
   left behind.
7. **Real app.** Move a real game/launcher cache (Steam `shadercache`, Epic `webcache`, etc.) and
   start the app. It must behave normally. Note any app that objects to junctions.
8. **Uninstall.** `rehome uninstall` removes the menu entry; moved folders still work.

Report anything unexpected as an issue with the on-screen text and the "Details:" line.
