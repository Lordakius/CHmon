use crate::config::{Flavor};
#[cfg(target_os = "macos")]
use crate::error::FilesystemError;

use regex::Regex;
use retry::delay::Fibonacci;
use retry::{retry, Error as RetryError, OperationResult};
use serde::Deserialize;

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Takes a `&str` and formats it into a proper
/// World of Warcraft release version.
///
/// Eg. 90001 would be 9.0.1.
pub fn format_interface_into_game_version(interface: &str) -> String {
    if interface.len() == 5 {
        let major = interface[..1].parse::<u8>();
        let minor = interface[1..3].parse::<u8>();
        let patch = interface[3..5].parse::<u8>();
        if let (Ok(major), Ok(minor), Ok(patch)) = (major, minor, patch) {
            return format!("{}.{}.{}", major, minor, patch);
        }
    }

    interface.to_owned()
}

/// Takes a `&str` and strips any non-digit.
/// This is used to unify and compare addon versions:
///
/// A string looking like 213r323 would return 213323.
/// A string looking like Rematch_4_10_15.zip would return 41015.
pub(crate) fn strip_non_digits(string: &str) -> String {
    let re = Regex::new(r"[\D]").unwrap();
    let stripped = re.replace_all(string, "").to_string();
    stripped
}

#[derive(Debug, Deserialize, Clone)]
pub struct Release {
    pub tag_name: String,
    pub prerelease: bool,
    pub assets: Vec<ReleaseAsset>,
    pub body: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ReleaseAsset {
    pub name: String,
    #[serde(rename = "browser_download_url")]
    pub download_url: String,
}

/// Logic to help pick the right World of Warcraft folder.
pub fn wow_path_resolution(path: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = path {
        // Known folders in World of Warcraft dir
        let known_folders = Flavor::ALL
            .iter()
            .map(|f| f.folder_name())
            .collect::<Vec<String>>();

        // If chosen path has any of the known Wow folders, we have the right one.
        for folder in known_folders.iter() {
            if path.join(folder).exists() {
                return Some(path);
            }
        }

        // Iterate ancestors. If we find any of the known folders we can guess the root.
        for ancestor in path.as_path().ancestors() {
            if let Some(file_name) = ancestor.file_name() {
                for folder in known_folders.iter() {
                    if file_name == OsStr::new(folder) {
                        return ancestor.parent().map(|p| p.to_path_buf());
                    }
                }
            }
        }
    }

    None
}

/// Rename a file or directory to a new name, retrying if the operation fails because of permissions
///
/// Will retry for ~30 seconds with longer and longer delays between each, to allow for virus scan
/// and other automated operations to complete.
pub fn rename<F, T>(from: F, to: T) -> io::Result<()>
where
    F: AsRef<Path>,
    T: AsRef<Path>,
{
    // 21 Fibonacci steps starting at 1 ms is ~28 seconds total
    // See https://github.com/rust-lang/rustup/pull/1873 where this was used by Rustup to work around
    // virus scanning file locks
    let from = from.as_ref();
    let to = to.as_ref();

    retry(Fibonacci::from_millis(1).take(21), || {
        match fs::rename(from, to) {
            Ok(_) => OperationResult::Ok(()),
            Err(e) => match e.kind() {
                io::ErrorKind::PermissionDenied => OperationResult::Retry(e),
                _ => OperationResult::Err(e),
            },
        }
    })
    .map_err(|e| match e {
        RetryError::Operation { error, .. } => error,
        RetryError::Internal(message) => io::Error::new(io::ErrorKind::Other, message),
    })
}

/// Remove a file, retrying if the operation fails because of permissions
///
/// Will retry for ~30 seconds with longer and longer delays between each, to allow for virus scan
/// and other automated operations to complete.
pub fn remove_file<P>(path: P) -> io::Result<()>
where
    P: AsRef<Path>,
{
    // 21 Fibonacci steps starting at 1 ms is ~28 seconds total
    // See https://github.com/rust-lang/rustup/pull/1873 where this was used by Rustup to work around
    // virus scanning file locks
    let path = path.as_ref();

    retry(
        Fibonacci::from_millis(1).take(21),
        || match fs::remove_file(path) {
            Ok(_) => OperationResult::Ok(()),
            Err(e) => match e.kind() {
                io::ErrorKind::PermissionDenied => OperationResult::Retry(e),
                _ => OperationResult::Err(e),
            },
        },
    )
    .map_err(|e| match e {
        RetryError::Operation { error, .. } => error,
        RetryError::Internal(message) => io::Error::new(io::ErrorKind::Other, message),
    })
}

pub(crate) fn truncate(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        None => s,
        Some((idx, _)) => &s[..idx],
    }
}

pub(crate) fn regex_html_tags_to_newline() -> Regex {
    regex::Regex::new(r"<br ?/?>|#.\s").unwrap()
}

pub(crate) fn regex_html_tags_to_space() -> Regex {
    regex::Regex::new(r"<[^>]*>|&#?\w+;|[gl]t;").unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wow_path_resolution() {
        let classic_addon_path =
            PathBuf::from(r"/Applications/World of Warcraft/_classic_/Interface/Addons");
        let retail_addon_path =
            PathBuf::from(r"/Applications/World of Warcraft/_retail_/Interface/Addons");
        let retail_interface_path =
            PathBuf::from(r"/Applications/World of Warcraft/_retail_/Interface");
        let classic_interface_path =
            PathBuf::from(r"/Applications/World of Warcraft/_classic_/Interface");
        let classic_alternate_path = PathBuf::from(r"/Applications/Wow/_classic_");

        let root_alternate_path = PathBuf::from(r"/Applications/Wow");
        let root_path = PathBuf::from(r"/Applications/World of Warcraft");

        assert!(root_path.eq(&wow_path_resolution(Some(classic_addon_path)).unwrap()),);
        assert!(root_path.eq(&wow_path_resolution(Some(retail_addon_path)).unwrap()),);
        assert!(root_path.eq(&wow_path_resolution(Some(retail_interface_path)).unwrap()),);
        assert!(root_path.eq(&wow_path_resolution(Some(classic_interface_path)).unwrap()),);
        assert!(
            root_alternate_path.eq(&wow_path_resolution(Some(classic_alternate_path)).unwrap()),
        );
    }

    #[test]
    fn test_interface() {
        let interface = "90001";
        assert_eq!("9.0.1", format_interface_into_game_version(interface));

        let interface = "11305";
        assert_eq!("1.13.5", format_interface_into_game_version(interface));

        let interface = "100000";
        assert_eq!("100000", format_interface_into_game_version(interface));

        let interface = "9.0.1";
        assert_eq!("9.0.1", format_interface_into_game_version(interface));
    }
}
