//! Path policy: which folders may be moved, which need a warning, which are refused.
//! Pure string logic, so it is identical (and testable) on every OS.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub code: &'static str,
    pub title: String,
    pub explanation: String,
    pub suggestion: String,
}

impl Finding {
    pub fn new(code: &'static str, title: &str, explanation: String, suggestion: &str) -> Self {
        Self {
            code,
            title: title.into(),
            explanation,
            suggestion: suggestion.into(),
        }
    }
}

#[derive(Debug, Default)]
pub struct Verdict {
    pub blocked: Vec<Finding>,
    pub warnings: Vec<Finding>,
}

/// Well-known locations, already normalized with [`normalize`].
#[derive(Debug, Clone)]
pub struct Known {
    pub windir: String,
    pub program_files: Vec<String>,
    pub program_data: String,
    pub user_profile: String,
    pub users_dir: String,
    pub local_app_data: String,
    pub app_data: String,
}

impl Known {
    pub fn from_env() -> Known {
        let get = |k: &str| {
            std::env::var(k)
                .ok()
                .map(|v| normalize(&v))
                .filter(|v| !v.is_empty())
        };
        let user_profile = get("USERPROFILE").unwrap_or_else(|| "c:\\users\\default".into());
        let users_dir = parent(&user_profile).unwrap_or_else(|| "c:\\users".into());
        let mut program_files: Vec<String> = ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"]
            .iter()
            .filter_map(|k| get(k))
            .collect();
        program_files.sort();
        program_files.dedup();
        Known {
            windir: get("SystemRoot")
                .or_else(|| get("windir"))
                .unwrap_or_else(|| "c:\\windows".into()),
            program_files,
            program_data: get("ProgramData").unwrap_or_else(|| "c:\\programdata".into()),
            local_app_data: get("LOCALAPPDATA")
                .unwrap_or_else(|| format!("{user_profile}\\appdata\\local")),
            app_data: get("APPDATA").unwrap_or_else(|| format!("{user_profile}\\appdata\\roaming")),
            user_profile,
            users_dir,
        }
    }

    /// Typical layout, for tests and docs.
    pub fn typical(user: &str) -> Known {
        let up = format!("c:\\users\\{}", user.to_lowercase());
        Known {
            windir: "c:\\windows".into(),
            program_files: vec!["c:\\program files".into(), "c:\\program files (x86)".into()],
            program_data: "c:\\programdata".into(),
            users_dir: "c:\\users".into(),
            local_app_data: format!("{up}\\appdata\\local"),
            app_data: format!("{up}\\appdata\\roaming"),
            user_profile: up,
        }
    }
}

/// Lowercase, backslashes, no `\\?\` prefix, `.`/`..` resolved, no trailing slash.
/// A drive root becomes `c:`.
pub fn normalize(p: &str) -> String {
    let mut s = p.replace('/', "\\");
    if let Some(rest) = s
        .strip_prefix("\\\\?\\UNC\\")
        .or_else(|| s.strip_prefix("\\\\?\\unc\\"))
    {
        s = format!("\\\\{rest}");
    } else if let Some(rest) = s.strip_prefix("\\\\?\\") {
        s = rest.to_string();
    }
    let unc = s.starts_with("\\\\");
    let mut parts: Vec<&str> = Vec::new();
    for c in s.split('\\') {
        match c {
            "" | "." => {}
            ".." => {
                if parts.len() > 1 || (parts.len() == 1 && !parts[0].ends_with(':')) {
                    parts.pop();
                }
            }
            c => parts.push(c),
        }
    }
    let joined = parts.join("\\").to_lowercase();
    if unc {
        format!("\\\\{joined}")
    } else {
        joined
    }
}

pub fn parent(norm: &str) -> Option<String> {
    norm.rfind('\\')
        .filter(|&i| i > 0)
        .map(|i| norm[..i].to_string())
}

pub fn is_same(a: &str, b: &str) -> bool {
    normalize(a) == normalize(b)
}

/// `child` is strictly inside `parent` (and not equal to it).
pub fn is_inside(child: &str, parent: &str) -> bool {
    let (c, p) = (normalize(child), normalize(parent));
    c.len() > p.len() && c.starts_with(&p) && c.as_bytes()[p.len()] == b'\\'
}

fn is_drive_root(n: &str) -> bool {
    let b = n.as_bytes();
    b.len() == 2 && b[1] == b':' && b[0].is_ascii_alphabetic()
}

fn at_or_in(n: &str, root: &str) -> bool {
    n == root || is_inside(n, root)
}

/// Where a folder goes by default: `<dest root>\Rehome\<original path without drive>`.
pub fn default_destination(source: &str, dest_root: &str) -> String {
    let src = source.replace('/', "\\");
    let rest = match src.as_bytes() {
        [d, b':', ..] if d.is_ascii_alphabetic() => &src[2..],
        _ => &src[..],
    };
    let root = dest_root.trim_end_matches(['\\', '/']);
    format!("{root}\\Rehome\\{}", rest.trim_start_matches('\\'))
}

/// Source-path policy. `source` may be in any spelling; it is normalized here.
pub fn check_source(source: &str, k: &Known) -> Verdict {
    let mut v = Verdict::default();
    let n = normalize(source);
    let shown = source.to_string();

    if n.contains('\n') || n.contains('\r') {
        v.blocked.push(Finding::new(
            "bad_path",
            "This folder name can't be used.",
            "The path contains a line break.".into(),
            "Rename the folder and try again.",
        ));
        return v;
    }
    if n.starts_with("\\\\") {
        v.blocked.push(Finding::new(
            "network_source",
            "Network folders can't be moved.",
            format!("\"{shown}\" is on a network share."),
            "Choose a folder on a drive that is physically in this PC.",
        ));
        return v;
    }
    if is_drive_root(&n) {
        v.blocked.push(Finding::new(
            "drive_root",
            "An entire drive can't be moved.",
            format!("\"{shown}\" is the root of a drive."),
            "Choose a folder inside the drive instead.",
        ));
        return v;
    }
    if at_or_in(&n, &k.windir) {
        v.blocked.push(Finding::new(
            "windows_folder",
            "Windows system files can't be moved.",
            format!("\"{shown}\" is part of the Windows folder. Moving it could stop Windows from starting."),
            "Pick a folder under your user profile (AppData) or an app's own data folder.",
        ));
    }
    if k.program_files.iter().any(|p| at_or_in(&n, p)) {
        v.blocked.push(Finding::new(
            "program_files",
            "Installed program folders can't be moved here.",
            format!("\"{shown}\" is under Program Files. Installers and updaters expect these folders to be real."),
            "Many apps can change their install location in their own settings or installer. Use that instead.",
        ));
    }

    let protected = [
        (&k.program_data, "ProgramData"),
        (&k.user_profile, "your user profile"),
        (&k.users_dir, "the Users folder"),
        (&k.app_data, "AppData\\Roaming"),
        (&k.local_app_data, "AppData\\Local"),
    ];
    for (p, name) in protected {
        if &n == p {
            v.blocked.push(Finding::new(
                "protected_root",
                "This folder is too important to move as a whole.",
                format!("\"{shown}\" is {name}. Windows and many apps depend on it being a normal folder."),
                "Move a specific folder inside it instead, such as a game's cache or download folder.",
            ));
        }
    }
    if parent(&n).as_deref() == Some(k.users_dir.as_str()) {
        v.blocked.push(Finding::new(
            "profile_folder",
            "A whole user profile can't be moved.",
            format!("\"{shown}\" is a user's profile folder."),
            "Move a specific folder inside the profile instead.",
        ));
    }

    let ld = &k.local_app_data;
    let pd = &k.program_data;
    let sensitive: [(String, &str, &str); 6] = [
        (format!("{ld}\\microsoft\\windows"), "windows_user_data", "Windows keeps its per-user system data here."),
        (format!("{ld}\\microsoft\\windowsapps"), "windows_apps", "Windows uses this for app launch shortcuts."),
        (format!("{ld}\\packages"), "store_apps", "This holds Microsoft Store app data, which is protected and can't be redirected safely."),
        (format!("{pd}\\microsoft\\windows"), "windows_system_data", "Windows keeps system data here."),
        (format!("{pd}\\microsoft\\windows defender"), "defender", "Windows Security (Defender) manages this and may block changes."),
        (format!("{pd}\\microsoft\\crypto"), "crypto", "Windows stores encryption keys here."),
    ];
    for (root, code, why) in sensitive {
        if at_or_in(&n, &root) {
            v.blocked.push(Finding::new(
                code,
                "Windows needs this folder where it is.",
                format!("\"{shown}\" is protected. {why}"),
                "Pick a folder belonging to a game or app instead.",
            ));
        }
    }

    if v.blocked.is_empty() && is_inside(&n, &k.program_data) {
        v.warnings.push(Finding::new(
            "program_data",
            "This folder is shared by all users and may be used by background services.",
            format!("\"{shown}\" is under ProgramData."),
            "Make sure the app and any helper services it installs are stopped. You may be asked for administrator permission.",
        ));
    }
    v
}

/// The two paths must not contain one another or be identical.
pub fn check_relation(source: &str, dest: &str) -> Verdict {
    let mut v = Verdict::default();
    if is_same(source, dest) || is_inside(dest, source) {
        v.blocked.push(Finding::new(
            "dest_inside_source",
            "The destination can't be inside the folder being moved.",
            format!("\"{dest}\" is inside \"{source}\"."),
            "Choose a destination on a different drive.",
        ));
    } else if is_inside(source, dest) {
        v.blocked.push(Finding::new(
            "source_inside_dest",
            "The destination already contains the folder being moved.",
            format!("\"{source}\" is inside \"{dest}\"."),
            "Choose a different destination folder.",
        ));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k() -> Known {
        Known::typical("Sam")
    }
    fn blocked(p: &str) -> Vec<&'static str> {
        check_source(p, &k())
            .blocked
            .iter()
            .map(|f| f.code)
            .collect()
    }

    #[test]
    fn normalizes() {
        assert_eq!(normalize("C:/Users/Sam/"), "c:\\users\\sam");
        assert_eq!(normalize("\\\\?\\C:\\A\\..\\B"), "c:\\b");
        assert_eq!(normalize("C:\\"), "c:");
        assert_eq!(normalize("C:\\..\\.."), "c:");
        assert_eq!(normalize("\\\\?\\UNC\\srv\\share\\x"), "\\\\srv\\share\\x");
    }

    #[test]
    fn inside_respects_boundaries() {
        assert!(is_inside("C:\\a\\b", "C:\\A"));
        assert!(!is_inside("C:\\ab", "C:\\a"));
        assert!(!is_inside("C:\\a", "C:\\a"));
    }

    #[test]
    fn allows_a_game_cache() {
        let v = check_source("C:\\Users\\Sam\\AppData\\Local\\SomeGame\\Cache", &k());
        assert!(v.blocked.is_empty() && v.warnings.is_empty());
    }

    #[test]
    fn blocks_system_places() {
        assert_eq!(blocked("C:\\"), ["drive_root"]);
        assert_eq!(blocked("c:\\windows\\system32"), ["windows_folder"]);
        assert_eq!(blocked("C:\\Program Files\\Steam"), ["program_files"]);
        assert_eq!(blocked("C:\\Program Files (x86)"), ["program_files"]);
        assert_eq!(blocked("C:\\Users"), ["protected_root"]);
        assert_eq!(
            blocked("C:\\Users\\Sam"),
            ["protected_root", "profile_folder"]
        );
        assert_eq!(blocked("C:\\Users\\Public"), ["profile_folder"]);
        assert_eq!(
            blocked("C:\\Users\\Sam\\AppData\\Local"),
            ["protected_root"]
        );
        assert_eq!(blocked("C:\\ProgramData"), ["protected_root"]);
        assert_eq!(blocked("\\\\nas\\share\\games"), ["network_source"]);
    }

    #[test]
    fn blocks_sensitive_subtrees_but_not_lookalikes() {
        assert_eq!(
            blocked("C:\\Users\\Sam\\AppData\\Local\\Microsoft\\Windows\\INetCache"),
            ["windows_user_data"]
        );
        assert_eq!(
            blocked("C:\\Users\\Sam\\AppData\\Local\\Packages\\Foo"),
            ["store_apps"]
        );
        assert_eq!(
            blocked("C:\\ProgramData\\Microsoft\\Windows Defender\\Scans"),
            ["defender"]
        );
        assert!(blocked("C:\\Users\\Sam\\AppData\\Local\\Microsoft\\Teams").is_empty());
        assert!(blocked("C:\\Users\\Sam\\AppData\\Local\\Microsoft\\WindowsFoo").is_empty());
    }

    #[test]
    fn dotdot_cannot_sneak_past() {
        assert_eq!(
            blocked("C:\\Users\\Sam\\AppData\\Local\\Game\\..\\..\\Local"),
            ["protected_root"]
        );
    }

    #[test]
    fn program_data_subfolder_warns() {
        let v = check_source("C:\\ProgramData\\Epic\\Launcher", &k());
        assert!(v.blocked.is_empty());
        assert_eq!(v.warnings[0].code, "program_data");
    }

    #[test]
    fn destination_naming() {
        assert_eq!(
            default_destination("C:\\Users\\Sam\\AppData\\Local\\G", "D:\\"),
            "D:\\Rehome\\Users\\Sam\\AppData\\Local\\G"
        );
        assert_eq!(default_destination("C:\\x", "D:"), "D:\\Rehome\\x");
    }

    #[test]
    fn relation() {
        assert!(check_relation("C:\\a", "D:\\a").blocked.is_empty());
        assert!(!check_relation("C:\\a", "C:\\a\\b").blocked.is_empty());
        assert!(!check_relation("C:\\a\\b", "C:\\a").blocked.is_empty());
        assert!(!check_relation("C:\\a", "C:\\A\\").blocked.is_empty());
    }
}
