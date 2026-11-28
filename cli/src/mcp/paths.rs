//! Where the MCP server may read and write.
//!
//! An agent names files; this decides whether it may. Reading is allowed
//! under the `--allow` roots (and the output folder, so a saved template can
//! be loaded again), writing only directly inside `--out-dir`. Both are
//! decided on **canonical** paths — `..`, symlinks and relative tricks are
//! resolved by the filesystem before the prefix check, so they cannot walk
//! out. Recordings are never written: `headless::check_output` refuses a
//! target that canonicalises to the source even if the folders overlap.
//!
//! This replaces MCP "roots", which the 2026-07-28 specification deprecates
//! (SEP-2577): the user decides the folders once, on the command line.

use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
pub struct PathGuard {
    read_roots: Vec<PathBuf>,
    out_dir: PathBuf,
}

impl PathGuard {
    /// Canonicalises the roots; each must exist and be a directory.
    pub fn new(allow: &[PathBuf], out_dir: &Path) -> Result<PathGuard, String> {
        let canon = |p: &Path| -> Result<PathBuf, String> {
            let c = std::fs::canonicalize(p).map_err(|e| format!("{}: {e}", p.display()))?;
            if !c.is_dir() {
                return Err(format!("{} is not a directory", p.display()));
            }
            Ok(c)
        };
        if allow.is_empty() {
            return Err("at least one --allow folder is required".into());
        }
        Ok(PathGuard {
            read_roots: allow.iter().map(|p| canon(p)).collect::<Result<_, _>>()?,
            out_dir: canon(out_dir)?,
        })
    }

    pub fn out_dir(&self) -> &Path {
        &self.out_dir
    }

    /// A file the server may read: absolute, existing, and — after
    /// resolving every `..` and symlink — inside an allowed folder or the
    /// output folder.
    pub fn readable(&self, path: &str) -> Result<PathBuf, String> {
        let p = Path::new(path);
        if !p.is_absolute() {
            return Err(format!(
                "{path:?} is not an absolute path; allowed folders are {}",
                self.describe()
            ));
        }
        let c = std::fs::canonicalize(p).map_err(|e| format!("{path:?}: {e}"))?;
        let inside = self
            .read_roots
            .iter()
            .chain(std::iter::once(&self.out_dir))
            .any(|root| c.starts_with(root));
        if !inside {
            return Err(format!(
                "{path:?} is outside the allowed folders ({})",
                self.describe()
            ));
        }
        if !c.is_file() {
            return Err(format!("{path:?} is not a file"));
        }
        Ok(c)
    }

    /// A file the server may write: a plain file name, placed directly in
    /// the output folder. No separators, no `..`, no leading dot (the
    /// render's own `.partial` files live there), and no existing symlink
    /// under that name — a link would let the write land elsewhere.
    pub fn writable(&self, file_name: &str) -> Result<PathBuf, String> {
        let p = Path::new(file_name);
        let mut parts = p.components();
        let single = matches!(
            (parts.next(), parts.next()),
            (Some(Component::Normal(_)), None)
        );
        if !single || file_name.contains(['/', '\\']) || file_name.starts_with('.') {
            return Err(format!(
                "{file_name:?} must be a plain file name; files are written to {}",
                self.out_dir.display()
            ));
        }
        let target = self.out_dir.join(p);
        if let Ok(meta) = std::fs::symlink_metadata(&target) {
            if meta.file_type().is_symlink() {
                return Err(format!(
                    "{file_name:?} exists as a symbolic link; refusing to write through it"
                ));
            }
        }
        Ok(target)
    }

    fn describe(&self) -> String {
        self.read_roots
            .iter()
            .chain(std::iter::once(&self.out_dir))
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dirs {
        _tmp: tempfile::TempDir,
        allow: PathBuf,
        out: PathBuf,
        outside: PathBuf,
    }

    fn dirs() -> Dirs {
        let tmp = tempfile::tempdir().unwrap();
        let allow = tmp.path().join("takes");
        let out = tmp.path().join("out");
        let outside = tmp.path().join("private");
        for d in [&allow, &out, &outside] {
            std::fs::create_dir(d).unwrap();
        }
        std::fs::write(allow.join("take.wav"), b"x").unwrap();
        std::fs::write(outside.join("secret.wav"), b"x").unwrap();
        Dirs {
            _tmp: tmp,
            allow,
            out,
            outside,
        }
    }

    fn guard(d: &Dirs) -> PathGuard {
        PathGuard::new(std::slice::from_ref(&d.allow), &d.out).unwrap()
    }

    fn s(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn reads_inside_the_allowed_folder() {
        let d = dirs();
        let g = guard(&d);
        assert!(g.readable(&s(&d.allow.join("take.wav"))).is_ok());
    }

    #[test]
    fn dot_dot_cannot_climb_out() {
        let d = dirs();
        let g = guard(&d);
        let sneaky = d.allow.join("..").join("private").join("secret.wav");
        assert!(g.readable(&s(&sneaky)).is_err());
    }

    #[test]
    fn an_absolute_path_outside_is_refused() {
        let d = dirs();
        let g = guard(&d);
        assert!(g.readable(&s(&d.outside.join("secret.wav"))).is_err());
        assert!(g.readable("/etc/passwd").is_err());
    }

    #[test]
    fn relative_paths_are_refused() {
        let d = dirs();
        assert!(guard(&d).readable("take.wav").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_inside_pointing_outside_is_refused() {
        let d = dirs();
        let link = d.allow.join("innocent.wav");
        std::os::unix::fs::symlink(d.outside.join("secret.wav"), &link).unwrap();
        assert!(guard(&d).readable(&s(&link)).is_err());
        // And a linked folder is no way out either.
        let dir_link = d.allow.join("elsewhere");
        std::os::unix::fs::symlink(&d.outside, &dir_link).unwrap();
        assert!(guard(&d)
            .readable(&s(&dir_link.join("secret.wav")))
            .is_err());
    }

    #[test]
    fn writes_only_plain_names_into_the_output_folder() {
        let d = dirs();
        let g = guard(&d);
        assert_eq!(
            g.writable("mix.mp3").unwrap(),
            std::fs::canonicalize(&d.out).unwrap().join("mix.mp3")
        );
        for bad in [
            "../mix.mp3",
            "sub/mix.mp3",
            "/tmp/mix.mp3",
            "..",
            ".",
            ".hidden",
            "a\\b.wav",
            "",
        ] {
            assert!(g.writable(bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_symlink_is_not_written_through() {
        let d = dirs();
        let g = guard(&d);
        std::os::unix::fs::symlink(d.outside.join("secret.wav"), d.out.join("mix.wav")).unwrap();
        assert!(g.writable("mix.wav").is_err());
    }

    #[test]
    fn the_output_folder_is_readable_for_saved_templates() {
        let d = dirs();
        std::fs::write(d.out.join("band.mixstack.json"), b"{}").unwrap();
        assert!(guard(&d)
            .readable(&s(&d.out.join("band.mixstack.json")))
            .is_ok());
    }

    #[test]
    fn roots_must_exist() {
        let d = dirs();
        assert!(PathGuard::new(&[d.allow.join("nope")], &d.out).is_err());
        assert!(PathGuard::new(&[], &d.out).is_err());
        assert!(PathGuard::new(std::slice::from_ref(&d.allow), &d.allow.join("take.wav")).is_err());
    }
}
