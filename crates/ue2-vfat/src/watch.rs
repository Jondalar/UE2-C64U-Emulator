//! Host file watcher (FSEvents on macOS through `notify`). Events inside `.ue2-trash`, on ue2's temporary files and
//! on Finder metadata are dropped, so a sync's own writes do not look like host changes; the consumer still
//! compares the host with the manifest before acting on an event.

use std::path::{Component, Path, PathBuf};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::scan;

/// Watch `root` (canonical) recursively and call `on_change` for every relevant event (and for watcher errors,
/// which may hide one).
pub fn watch(root: &Path, on_change: impl Fn() + Send + 'static) -> notify::Result<RecommendedWatcher> {
    let base = root.to_path_buf();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
        Ok(event) if matches!(event.kind, EventKind::Access(_)) => {}
        Ok(event) if !event.paths.is_empty() && event.paths.iter().all(|p| !relevant(&base, p)) => {}
        _ => on_change(),
    })?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    Ok(watcher)
}

/// False for paths ue2 owns or never imports.
pub fn relevant(root: &Path, path: &Path) -> bool {
    let rel: PathBuf = match path.strip_prefix(root) {
        Ok(rel) => rel.to_path_buf(),
        Err(_) => return true,
    };
    !rel.components().any(|c| matches!(c, Component::Normal(name) if name.to_str().is_some_and(scan::ignored)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_paths_are_not_relevant() {
        let root = Path::new("/x/share");
        assert!(relevant(root, Path::new("/x/share/games/a.prg")));
        assert!(relevant(root, Path::new("/x/share")));
        assert!(!relevant(root, Path::new("/x/share/.ue2-trash/20260913-101500/a.prg")));
        assert!(!relevant(root, Path::new("/x/share/games/.ue2-tmp-12-3")));
        assert!(!relevant(root, Path::new("/x/share/.DS_Store")));
    }
}
