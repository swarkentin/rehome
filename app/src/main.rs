//! Rehome command line. Also the target of the Explorer right-click entry (`rehome menu "<folder>"`).

#[cfg(not(windows))]
fn main() {
    eprintln!("Rehome only runs on Windows. The engine and its tests live in crates/rehome-core.");
    std::process::exit(2);
}

#[cfg(windows)]
mod win;

#[cfg(windows)]
fn main() {
    cli::run();
}

#[cfg(windows)]
mod cli {
    use crate::win::{self, Windows};
    use rehome_core::engine::human;
    use rehome_core::safety::{default_destination, Known};
    use rehome_core::*;
    use std::cell::Cell;
    use std::io::{self, BufRead, Write};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant};

    const HELP: &str = "\
Rehome: move a folder to another drive and leave a shortcut (junction) behind.

USAGE
  rehome check   <folder> [--to <drive or folder>]    Check a move without changing anything
  rehome move    <folder> [--to <drive or folder>] [--yes] [--accept-warnings] [--keep-backup]
  rehome undo    <folder>                             Move a relocated folder back
  rehome recover                                      Finish or roll back an interrupted move
  rehome install | uninstall                          Add/remove the right-click menu entry
  rehome --help | --version

--to D:\\          puts the folder at D:\\Rehome\\<original path>
--to D:\\Games\\X  uses that exact folder (it must not exist yet)
";

    struct Console {
        last: Cell<Option<Instant>>,
    }

    impl Observer for Console {
        fn step(&self, s: Step) {
            println!("\n==> {s}...");
        }
        fn progress(&self, done: u64, total: u64) {
            let now = Instant::now();
            if self
                .last
                .get()
                .is_some_and(|t| now.duration_since(t) < Duration::from_millis(250))
            {
                return;
            }
            self.last.set(Some(now));
            print!("\r    {} of {}   ", human(done), human(total));
            let _ = io::stdout().flush();
        }
    }

    pub fn run() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let menu = args.first().is_some_and(|a| a == "menu");
        let result = dispatch(&args);
        let code = match result {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("\n{e}");
                if let Some(d) = &e.details {
                    eprintln!("\nDetails: {d}");
                }
                1
            }
        };
        if menu {
            print!("\nPress Enter to close this window.");
            let _ = io::stdout().flush();
            let _ = io::stdin().lock().read_line(&mut String::new());
        }
        std::process::exit(code);
    }

    fn usage(msg: &str) -> RehomeError {
        RehomeError::new(
            ErrorKind::Blocked,
            "That command wasn't understood.",
            msg,
            "Run `rehome --help` to see what's available.",
        )
    }

    fn dispatch(args: &[String]) -> Result<(), RehomeError> {
        let cmd = args.first().map(String::as_str).unwrap_or("--help");
        let flag = |f: &str| args.iter().any(|a| a == f);
        let value = |f: &str| {
            args.iter()
                .position(|a| a == f)
                .and_then(|i| args.get(i + 1))
                .cloned()
        };
        let folder = || -> Result<PathBuf, RehomeError> {
            let raw = args
                .get(1)
                .filter(|a| !a.starts_with("--"))
                .ok_or_else(|| usage("Which folder? Give its full path."))?;
            Ok(absolute(raw))
        };
        match cmd {
            "--help" | "-h" | "help" => {
                print!("{HELP}");
                Ok(())
            }
            "--version" | "-V" => {
                println!("rehome {}", env!("CARGO_PKG_VERSION"));
                Ok(())
            }
            "install" => install(),
            "uninstall" => uninstall(),
            "recover" => recover_all(true),
            "check" => {
                let src = folder()?;
                let dest = pick_dest(&src, value("--to"))?;
                let (eng, _) = engine();
                let plan = eng
                    .check(
                        &src,
                        &dest,
                        &Console {
                            last: Cell::new(None),
                        },
                    )
                    .map_err(refused)?;
                show_plan(&plan);
                println!("\nNothing was changed. Run the same command with `move` to go ahead.");
                Ok(())
            }
            "move" | "menu" => {
                recover_all(false)?;
                let src = folder()?;
                let (eng, win) = engine();
                // A folder Rehome already moved: offer to bring it back.
                if cmd == "menu" && matches!(win.read_junction(&src), Ok(Some(_))) {
                    return undo(&eng, &src, false);
                }
                let dest = pick_dest(&src, value("--to"))?;
                do_move(
                    &eng,
                    &src,
                    &dest,
                    flag("--yes"),
                    flag("--accept-warnings"),
                    flag("--keep-backup"),
                )
            }
            "undo" => {
                recover_all(false)?;
                let (eng, _) = engine();
                undo(&eng, &folder()?, flag("--yes"))
            }
            other => Err(usage(&format!("\"{other}\" isn't a command."))),
        }
    }

    fn engine() -> (Engine<'static, Windows>, &'static Windows) {
        static WIN: Windows = Windows;
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        (
            Engine::new(&WIN, Known::from_env(), base.join("Rehome").join("journal")),
            &WIN,
        )
    }

    fn refused(findings: Vec<safety::Finding>) -> RehomeError {
        let mut it = findings.into_iter();
        let mut e: RehomeError = it.next().expect("at least one finding").into();
        for f in it {
            e.explanation.push_str(&format!(" Also: {}", f.title));
        }
        e
    }

    /// Absolute, without trailing separators or `.` / `..` surprises.
    fn absolute(raw: &str) -> PathBuf {
        let p = std::path::absolute(raw.trim_matches('"')).unwrap_or_else(|_| PathBuf::from(raw));
        p.components().collect()
    }

    fn ask(prompt: &str) -> String {
        print!("{prompt}");
        let _ = io::stdout().flush();
        let mut s = String::new();
        let _ = io::stdin().lock().read_line(&mut s);
        s.trim().to_string()
    }

    fn confirm(prompt: &str) -> bool {
        matches!(
            ask(&format!("{prompt} [y/N] ")).to_lowercase().as_str(),
            "y" | "yes"
        )
    }

    fn pick_dest(src: &Path, to: Option<String>) -> Result<PathBuf, RehomeError> {
        let src_s = src.to_string_lossy();
        if let Some(t) = to {
            let t = t.trim_matches('"');
            let drive_only = matches!(t.as_bytes(), [d, b':'] | [d, b':', b'\\' | b'/'] if d.is_ascii_alphabetic());
            return Ok(if drive_only {
                PathBuf::from(default_destination(&src_s, t))
            } else {
                absolute(t)
            });
        }
        let src_vol = Windows.volume_info(src).ok().map(|v| v.volume_id);
        let drives: Vec<_> = win::fixed_drives()
            .into_iter()
            .filter(|(_, v)| Some(&v.volume_id) != src_vol.as_ref())
            .collect();
        if drives.is_empty() {
            return Err(RehomeError::new(
                ErrorKind::Blocked,
                "There's no other drive to move this to.",
                "Rehome only moves folders to a different internal drive, and none was found.",
                "Add or connect an internal drive, or give a destination with --to.",
            ));
        }
        println!("\nMove \"{}\" to which drive?\n", src.display());
        for (i, (root, v)) in drives.iter().enumerate() {
            println!(
                "  {}) {root}   {} free   ({})",
                i + 1,
                human(v.free_bytes),
                v.fs
            );
        }
        let n: usize = ask("\nNumber (or press Enter to cancel): ")
            .parse()
            .unwrap_or(0);
        let (root, _) = drives
            .get(n.wrapping_sub(1))
            .ok_or_else(RehomeError::cancelled)?;
        Ok(PathBuf::from(default_destination(&src_s, root)))
    }

    fn show_plan(p: &Plan) {
        println!("\nFrom:   {}", p.source.display());
        println!("To:     {}", p.dest.display());
        println!("Size:   {} in {} files", human(p.scan.bytes), p.scan.files);
        println!(
            "Space:  {} free on the destination; {} will be left after the move",
            human(p.dest_free_bytes),
            human(p.dest_free_bytes.saturating_sub(p.scan.bytes))
        );
        if !p.users.is_empty() {
            println!("\nThese apps appear to be using the folder (close them first):");
            for u in &p.users {
                println!(
                    "  - {}{}",
                    u.name,
                    if u.is_service { " (service)" } else { "" }
                );
            }
        } else {
            println!("\nNo running apps were found using it. Rehome will double-check right before switching.");
        }
        for w in &p.warnings {
            println!(
                "\nWARNING: {}\n  {}\n  {}",
                w.title, w.explanation, w.suggestion
            );
        }
    }

    fn do_move(
        eng: &Engine<'static, Windows>,
        src: &Path,
        dest: &Path,
        yes: bool,
        accept: bool,
        keep: bool,
    ) -> Result<(), RehomeError> {
        let obs = Console {
            last: Cell::new(None),
        };
        println!("Checking {}...", src.display());
        let mut plan = eng.check(src, dest, &obs).map_err(refused)?;
        show_plan(&plan);
        loop {
            if !yes && !confirm("\nMove this folder now?") {
                return Err(RehomeError::cancelled());
            }
            let opts = Options {
                accept_warnings: accept
                    || (!plan.warnings.is_empty()
                        && !yes
                        && confirm("Continue despite the warnings above?")),
                keep_backup: keep,
            };
            match eng.move_folder(&plan, &opts, &obs) {
                Ok(out) => {
                    println!(
                        "\n\nDone. {} moved to {}.",
                        human(out.bytes_moved),
                        dest.display()
                    );
                    println!("Apps will still find the folder at {}.", src.display());
                    if let Some(l) = out.leftover {
                        println!("\nNote: the original files are still at \"{}\". Delete that folder to free the space on this drive.", l.display());
                    }
                    println!("To undo: rehome undo \"{}\"", src.display());
                    return Ok(());
                }
                Err(e) if e.kind == ErrorKind::InUse && !yes => {
                    eprintln!("\n{e}");
                    if !confirm("\nClose those apps now, then try again?") {
                        return Err(RehomeError::cancelled());
                    }
                    plan = eng.check(src, dest, &obs).map_err(refused)?;
                    show_plan(&plan);
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn undo(eng: &Engine<'static, Windows>, src: &Path, yes: bool) -> Result<(), RehomeError> {
        println!("\n\"{}\" was moved by Rehome. Its files will be copied back to this drive and the shortcut removed.", src.display());
        if !yes && !confirm("Move it back now?") {
            return Err(RehomeError::cancelled());
        }
        let out = eng.restore(
            src,
            &Console {
                last: Cell::new(None),
            },
        )?;
        println!(
            "\n\nDone. {} is back at {}.",
            human(out.bytes_moved),
            src.display()
        );
        if let Some(l) = out.leftover {
            println!(
                "Note: couldn't delete \"{}\". You can delete it yourself.",
                l.display()
            );
        }
        Ok(())
    }

    fn recover_all(verbose: bool) -> Result<(), RehomeError> {
        let (eng, _) = engine();
        let pending = eng.pending();
        if pending.is_empty() && verbose {
            println!("Nothing to recover. No interrupted moves were found.");
        }
        let mut first_err = None;
        for mut j in pending {
            println!(
                "An earlier move of \"{}\" didn't finish. Making it safe...",
                j.source.display()
            );
            match eng.recover(&mut j) {
                Ok(RecoverOutcome::RolledBack) => {
                    println!("  Rolled back. \"{}\" is as it was.", j.source.display())
                }
                Ok(RecoverOutcome::Completed) => println!(
                    "  Finished. \"{}\" now points to {}.",
                    j.source.display(),
                    j.dest.display()
                ),
                Err(e) => first_err = first_err.or(Some(e)),
            }
        }
        first_err.map_or(Ok(()), Err)
    }

    fn reg(args: &[&str]) -> Result<(), RehomeError> {
        let out = Command::new("reg")
            .args(args)
            .output()
            .map_err(|e| RehomeError::io("run the registry tool", Path::new("reg.exe"), &e))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(RehomeError::new(
                ErrorKind::Io,
                "Couldn't update the right-click menu.",
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
                "Try again from a normal (non-restricted) terminal.",
            ))
        }
    }

    const KEY: &str = r"HKCU\Software\Classes\Directory\shell\Rehome";

    fn install() -> Result<(), RehomeError> {
        let exe = std::env::current_exe().map_err(|e| {
            RehomeError::io("find Rehome's own location", Path::new("rehome.exe"), &e)
        })?;
        let exe = exe.to_string_lossy();
        reg(&["add", KEY, "/ve", "/d", "Move to another drive...", "/f"])?;
        reg(&["add", KEY, "/v", "Icon", "/d", &exe, "/f"])?;
        reg(&[
            "add",
            &format!("{KEY}\\command"),
            "/ve",
            "/d",
            &format!("\"{exe}\" menu \"%1\""),
            "/f",
        ])?;
        println!("Added \"Move to another drive...\" to the folder right-click menu (Windows 11: under \"Show more options\").");
        println!("Keep rehome.exe where it is, or run `rehome install` again after moving it.");
        Ok(())
    }

    fn uninstall() -> Result<(), RehomeError> {
        reg(&["delete", KEY, "/f"])?;
        println!("Removed the right-click menu entry. Folders you already moved keep working.");
        Ok(())
    }
}
