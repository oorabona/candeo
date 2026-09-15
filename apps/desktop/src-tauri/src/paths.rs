//! Paths as Candeo writes them into text: logs, the copied diagnostic, error
//! messages.
//!
//! That text ends up pasted into public bug reports, and the home directory
//! usually carries the user's name. It is written `~`; what follows still says
//! where the file is. Code that opens a folder keeps the real path, and so does
//! Settings, which shows the log folder on the machine itself.

use std::path::{Path, PathBuf};

/// `path`, with the home directory written `~`.
pub fn shown(path: &Path) -> String {
    without_home(path, home().as_deref())
}

/// The home directory, from the variable the system sets for it.
fn home() -> Option<PathBuf> {
    let variable = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(variable)
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
}

fn without_home(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) => Path::new("~").join(rest).display().to_string(),
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_home_directory_is_not_shown() {
        let home = Path::new("home").join("someone");
        let logs = home.join("logs");
        assert_eq!(
            without_home(&logs, Some(&home)),
            Path::new("~").join("logs").display().to_string()
        );
        // A name that only starts like the home directory is another directory.
        let other = Path::new("home").join("someone-else").join("logs");
        assert_eq!(
            without_home(&other, Some(&home)),
            other.display().to_string()
        );
        assert_eq!(without_home(&logs, None), logs.display().to_string());
    }

    #[test]
    fn a_path_under_the_real_home_directory_is_shown_with_a_tilde() {
        let Some(home) = home() else { return };
        let shown = shown(&home.join("candeo").join("settings.json"));
        assert!(shown.starts_with('~'), "{shown}");
        assert!(!shown.contains(&*home.to_string_lossy()), "{shown}");
    }
}
