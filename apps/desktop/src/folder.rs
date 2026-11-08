//! `/open <path>`: a typed folder path for a new workspace. Parsing is pure string work (the three
//! OS styles are told apart on any host); only [`validate`] touches the disk.

use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
enum Style {
    /// `~`, `~/x`, `~\x`
    Home,
    /// `C:\x`, `C:/x`
    Drive,
    /// `\\server\share\x`
    Unc,
    /// `/x`
    Posix,
    Relative,
}

fn style(s: &str) -> Style {
    let b = s.as_bytes();
    if s == "~" || s.starts_with("~/") || s.starts_with("~\\") {
        Style::Home
    } else if s.starts_with("\\\\") {
        Style::Unc
    } else if b.len() >= 3
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && matches!(b[2], b'\\' | b'/')
    {
        Style::Drive
    } else if s.starts_with('/') {
        Style::Posix
    } else {
        Style::Relative
    }
}

/// The path a typed line names: quotes and surrounding blanks dropped, `~` from `home`, a relative
/// path under `cwd`. A Windows drive or UNC path on another OS is an error, not a guess.
pub fn parse(input: &str, home: Option<&Path>, cwd: &Path) -> Result<PathBuf, String> {
    let t = input.trim();
    let t = t
        .strip_prefix('"')
        .and_then(|r| r.strip_suffix('"'))
        .or_else(|| t.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')))
        .unwrap_or(t)
        .trim();
    if t.is_empty() {
        return Err("/open: type a folder path".into());
    }
    if t.contains(['\0', '\n', '\r']) {
        return Err("/open: the path has a control character".into());
    }
    match style(t) {
        Style::Home => {
            let home =
                home.ok_or("/open: ~ needs a home directory (HOME or USERPROFILE is not set)")?;
            let rest = t[1..].trim_start_matches(['/', '\\']);
            Ok(if rest.is_empty() {
                home.to_path_buf()
            } else {
                home.join(rest)
            })
        }
        Style::Drive | Style::Unc if !cfg!(windows) => Err(format!(
            "/open: {t} is a Windows path and this is not Windows"
        )),
        Style::Posix if cfg!(windows) => Err(format!(
            "/open: {t} has no drive; type C:\\folder or \\\\server\\share\\folder"
        )),
        Style::Relative => Ok(cwd.join(t)),
        _ => Ok(PathBuf::from(t)),
    }
}

/// Exists, is a directory, can be read, and is not one of the `open` workspaces. Returns the real path.
pub fn validate(p: &Path, open: &[PathBuf]) -> Result<PathBuf, String> {
    let shown = p.display();
    let c = std::fs::canonicalize(p).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!("/open: {shown} does not exist"),
        std::io::ErrorKind::PermissionDenied => format!("/open: {shown} is not readable"),
        _ => format!("/open: {shown}: {e}"),
    })?;
    if !c.is_dir() {
        return Err(format!("/open: {shown} is not a folder"));
    }
    std::fs::read_dir(&c).map_err(|_| format!("/open: {shown} is not readable"))?;
    let same = |a: &Path, b: &Path| {
        if cfg!(any(windows, target_os = "macos")) {
            a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
        } else {
            a == b
        }
    };
    if open
        .iter()
        .any(|o| std::fs::canonicalize(o).is_ok_and(|o| same(&o, &c)))
    {
        return Err(format!("/open: {shown} is already open as a workspace"));
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styles_are_told_apart_on_any_host() {
        assert_eq!(style("~"), Style::Home);
        assert_eq!(style("~\\work"), Style::Home);
        assert_eq!(style("C:\\Users\\a"), Style::Drive);
        assert_eq!(style("d:/x"), Style::Drive);
        assert_eq!(style("\\\\srv\\share\\x"), Style::Unc);
        assert_eq!(style("/usr/x"), Style::Posix);
        assert_eq!(style("src/x"), Style::Relative);
        assert_eq!(
            style("C:x"),
            Style::Relative,
            "drive-relative is not a drive path"
        );
        assert_eq!(style("~user/x"), Style::Relative);
    }

    #[test]
    fn home_and_relative_use_the_given_dirs_not_the_environment() {
        let (home, cwd) = (Path::new("/h"), Path::new("/w"));
        assert_eq!(parse("~", Some(home), cwd).unwrap(), PathBuf::from("/h"));
        assert_eq!(
            parse(" ~/a/b ", Some(home), cwd).unwrap(),
            PathBuf::from("/h/a/b")
        );
        assert_eq!(
            parse("\"~/a\"", Some(home), cwd).unwrap(),
            PathBuf::from("/h/a")
        );
        assert_eq!(
            parse("proj/x", Some(home), cwd).unwrap(),
            PathBuf::from("/w/proj/x")
        );
        assert!(parse("~/a", None, cwd).unwrap_err().contains("home"));
        assert!(parse("  ", None, cwd).is_err());
        assert!(parse("a\nb", None, cwd).is_err());
    }

    #[test]
    fn a_path_of_another_os_style_is_refused_or_kept_by_host() {
        let (home, cwd) = (None, Path::new("/w"));
        let win = cfg!(windows);
        assert_eq!(parse("C:\\a", home, cwd).is_ok(), win);
        assert_eq!(parse("\\\\srv\\sh\\a", home, cwd).is_ok(), win);
        assert_eq!(parse("/a/b", home, cwd).is_ok(), !win);
    }

    #[test]
    fn validate_names_what_is_wrong() {
        let base = std::env::temp_dir().join(format!("synt-open-{}", std::process::id()));
        let dir = base.join("d");
        std::fs::create_dir_all(&dir).unwrap();
        let file = base.join("f.txt");
        std::fs::write(&file, "x").unwrap();
        assert!(
            validate(&base.join("nope"), &[])
                .unwrap_err()
                .contains("does not exist")
        );
        assert!(validate(&file, &[]).unwrap_err().contains("not a folder"));
        assert!(
            validate(&dir, std::slice::from_ref(&dir))
                .unwrap_err()
                .contains("already open")
        );
        assert_eq!(
            validate(&dir, std::slice::from_ref(&base)).unwrap(),
            std::fs::canonicalize(&dir).unwrap()
        );
        std::fs::remove_dir_all(&base).ok();
    }
}
