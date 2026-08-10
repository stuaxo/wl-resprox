//! Resolves a Wayland client's real PID to the `.desktop` file id that
//! actually identifies it, for clients whose own `xdg_toplevel.app_id`
//! doesn't match any installed `.desktop` file (see
//! docs/KNOWN_BUGS.md's PID-collision entry). gnome-shell's own
//! `app_id` -> `.desktop` file lookup runs first and would resolve
//! these correctly on its own if given the right string -- it only
//! falls through to a PID-based match (and misfires, since every
//! client through this proxy shares the proxy's own PID) because the
//! string it's given doesn't match anything. Rewriting the string
//! before it reaches the compositor means gnome-shell's own normal,
//! first-choice lookup succeeds and the broken PID step is never
//! reached -- see `relay_ready_messages`'s `xdg_toplevel.set_app_id`
//! handling in lib.rs for where this gets used.
//!
//! Deliberately simpler than `Gio.DesktopAppInfo`'s own resolution: no
//! `TryExec=`, no argument-placeholder handling beyond splitting on
//! whitespace, no disambiguation between multiple `.desktop` files
//! sharing one binary (first one found under `build()`'s scan order
//! wins). Good enough to fix the common case (a plain, unwrapped
//! `Exec=<binary> ...`); a client already sending a matching `app_id`
//! is never second-guessed.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub struct DesktopAppIndex {
    /// Every installed `.desktop` file's own id (filename minus the
    /// `.desktop` extension). An `app_id` already in this set is left
    /// alone -- exactly what a real, unproxied session would see.
    ids: HashSet<String>,
    /// `Exec=`'s first token's basename -> the `.desktop` file's id
    /// that declared it, from each file's `[Desktop Entry]` group only
    /// (a `[Desktop Action ...]` group's own `Exec=` describes a
    /// secondary action, not the app itself).
    by_binary: HashMap<String, String>,
}

impl DesktopAppIndex {
    /// Scans the standard system + per-user application directories.
    pub fn build() -> Self {
        let mut dirs = vec![PathBuf::from("/usr/share/applications")];
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(home).join(".local/share/applications"));
        }
        Self::build_from_dirs(&dirs)
    }

    /// As `build()`, but scanning exactly the given directories --
    /// `build()`'s own real-filesystem call, testable without needing
    /// to touch `/usr/share/applications`.
    pub fn build_from_dirs(dirs: &[PathBuf]) -> Self {
        let mut ids = HashSet::new();
        let mut by_binary = HashMap::new();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                    continue;
                }
                let Some(id) = path.file_stem().and_then(|s| s.to_str()) else { continue };
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Some(binary) = main_entry_exec(&content).as_deref().and_then(exec_binary_name) {
                        by_binary.entry(binary).or_insert_with(|| id.to_string());
                    }
                }
                ids.insert(id.to_string());
            }
        }
        Self { ids, by_binary }
    }

    /// Whether `app_id` is already a real, installed `.desktop` file's
    /// own id -- if so, it should be left untouched.
    pub fn has_id(&self, app_id: &str) -> bool {
        self.ids.contains(app_id)
    }

    /// The `.desktop` file id whose `Exec=` runs the given binary name,
    /// if any.
    pub fn resolve_by_binary(&self, binary_name: &str) -> Option<&str> {
        self.by_binary.get(binary_name).map(String::as_str)
    }
}

/// The `[Desktop Entry]` group's own `Exec=` value.
fn main_entry_exec(content: &str) -> Option<String> {
    let mut in_main_entry = false;
    for line in content.lines() {
        let line = line.trim();
        if let Some(group) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            in_main_entry = group == "Desktop Entry";
            continue;
        }
        if in_main_entry {
            if let Some(value) = line.strip_prefix("Exec=") {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// The binary an `Exec=` value actually runs: its first
/// whitespace-separated token (field codes like `%f`/`%U` are always
/// separate tokens, never glued to it), with any path stripped.
fn exec_binary_name(exec: &str) -> Option<String> {
    let first = exec.split_whitespace().next()?;
    Some(Path::new(first).file_name()?.to_str()?.to_string())
}

/// The real binary a running process is, via `/proc/<pid>/exe` -- the
/// bridge from a Wayland client's actual (unspoofable) PID, which this
/// proxy has from `peer_cred()` on its own accept()ed socket, to
/// something `resolve_by_binary` can look up. `None` covers every
/// failure mode (pid gone, `/proc` unavailable, non-UTF8 path) alike --
/// best-effort, never fatal to the caller.
pub fn binary_name_for_pid(pid: i32) -> Option<String> {
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    exe.file_name()?.to_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, uniquely-named scratch directory under the OS temp dir,
    /// cleaned up on drop -- same `temp_dir().join(unique-name)` pattern
    /// `tests/integration.rs` already uses throughout, rather than
    /// pulling in a `tempfile` dependency this project doesn't have.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(unique: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("wayland-proxy-desktop-apps-test-{unique}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("create scratch dir");
            Self(dir)
        }

        fn write(&self, filename: &str, content: &str) -> &Self {
            std::fs::write(self.0.join(filename), content).expect("write desktop file");
            self
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn resolves_a_plain_exec_by_binary_name() {
        let dir = ScratchDir::new("plain-exec");
        dir.write("com.gexperts.Tilix.desktop", "[Desktop Entry]\nName=Tilix\nExec=tilix\nType=Application\n");
        let index = DesktopAppIndex::build_from_dirs(std::slice::from_ref(&dir.0));
        assert_eq!(index.resolve_by_binary("tilix"), Some("com.gexperts.Tilix"));
        assert!(index.has_id("com.gexperts.Tilix"));
        assert!(!index.has_id("tilix"), "the app_id string itself was never installed as a desktop file id");
    }

    #[test]
    fn ignores_exec_under_a_desktop_action_group() {
        let dir = ScratchDir::new("action-group");
        dir.write(
            "org.gnome.Nautilus.desktop",
            "[Desktop Entry]\nName=Files\nExec=nautilus --new-window %U\nType=Application\n\n\
             [Desktop Action new-window]\nName=New Window\nExec=nautilus --new-window\n",
        );
        let index = DesktopAppIndex::build_from_dirs(std::slice::from_ref(&dir.0));
        assert_eq!(index.resolve_by_binary("nautilus"), Some("org.gnome.Nautilus"));
    }

    #[test]
    fn strips_path_and_arguments_from_exec() {
        let dir = ScratchDir::new("path-and-args");
        dir.write("org.example.Foo.desktop", "[Desktop Entry]\nExec=/usr/bin/foo --flag arg\nType=Application\n");
        let index = DesktopAppIndex::build_from_dirs(std::slice::from_ref(&dir.0));
        assert_eq!(index.resolve_by_binary("foo"), Some("org.example.Foo"));
    }

    #[test]
    fn unknown_binary_and_unknown_id_both_resolve_to_nothing() {
        let dir = ScratchDir::new("unknown");
        dir.write("org.example.Foo.desktop", "[Desktop Entry]\nExec=foo\n");
        let index = DesktopAppIndex::build_from_dirs(std::slice::from_ref(&dir.0));
        assert_eq!(index.resolve_by_binary("not-installed"), None);
        assert!(!index.has_id("not.installed"));
    }

    #[test]
    fn first_directory_wins_on_a_binary_name_collision() {
        let first = ScratchDir::new("collision-first");
        let second = ScratchDir::new("collision-second");
        first.write("first.desktop", "[Desktop Entry]\nExec=shared-binary\n");
        second.write("second.desktop", "[Desktop Entry]\nExec=shared-binary\n");
        let index = DesktopAppIndex::build_from_dirs(&[first.0.clone(), second.0.clone()]);
        assert_eq!(index.resolve_by_binary("shared-binary"), Some("first"));
    }

    #[test]
    fn non_desktop_files_are_ignored() {
        let dir = ScratchDir::new("non-desktop");
        dir.write("not-a-desktop-file.txt", "[Desktop Entry]\nExec=should-not-appear\n");
        let index = DesktopAppIndex::build_from_dirs(std::slice::from_ref(&dir.0));
        assert_eq!(index.resolve_by_binary("should-not-appear"), None);
    }

    #[test]
    fn binary_name_for_pid_resolves_this_own_test_process() {
        let expected = std::env::current_exe().expect("current_exe");
        let expected_name = expected.file_name().and_then(|n| n.to_str()).expect("exe file name");
        let resolved = binary_name_for_pid(std::process::id() as i32);
        assert_eq!(resolved.as_deref(), Some(expected_name));
    }

    #[test]
    fn binary_name_for_pid_returns_none_for_an_impossible_pid() {
        assert_eq!(binary_name_for_pid(-1), None);
    }
}
