use std::path::{Component, Path, PathBuf};

/// Computes workspace spellings once at startup, using trusted inputs only.
///
/// `canonical_root` must be the canonical runtime cwd and `current_dir` the
/// absolute process cwd used to resolve a relative user-supplied `cd`. With no
/// `cd`, an absolute shell `pwd` is considered instead. The `cd` spelling is made
/// absolute and lexically normalized. Candidates are retained only when they
/// canonicalize to `canonical_root`. Each retained `/private/...` spelling contributes its
/// shorter spelling if that resolves to the same root. Results are deduplicated,
/// with the canonical root first. Never pass a model-supplied path here.
pub fn workspace_root_aliases(
    canonical_root: &Path,
    cd: Option<&Path>,
    current_dir: &Path,
    pwd: Option<&Path>,
) -> Vec<PathBuf> {
    let mut aliases = vec![canonical_root.to_path_buf()];
    // A relative `cd` is resolved against the physical cwd and, when the shell's
    // logical cwd names the same directory, against that spelling too.
    let logical_cwd = pwd.filter(|p| {
        p.is_absolute()
            && matches!(
                (std::fs::canonicalize(p), std::fs::canonicalize(current_dir)),
                (Ok(a), Ok(b)) if a == b
            )
    });
    let candidates: Vec<PathBuf> = match cd {
        Some(cd) => std::iter::once(current_dir)
            .chain(logical_cwd.filter(|_| cd.is_relative()))
            .map(|base| normalize(&base.join(cd)))
            .collect(),
        None => pwd
            .filter(|p| p.is_absolute())
            .map(Path::to_path_buf)
            .into_iter()
            .collect(),
    };
    for candidate in candidates {
        if candidate.is_absolute()
            && std::fs::canonicalize(&candidate).is_ok_and(|p| p == canonical_root)
            && !aliases.contains(&candidate)
        {
            aliases.push(candidate);
        }
    }
    // Consider the canonical root and the validated user spelling, never paths
    // discovered from file tool inputs. Probe only these startup candidates.
    for alias in aliases.clone() {
        if let Ok(suffix) = alias.strip_prefix("/private") {
            let short = Path::new("/").join(suffix);
            if std::fs::canonicalize(&short).is_ok_and(|p| p == canonical_root)
                && !aliases.contains(&short)
            {
                aliases.push(short);
            }
        }
    }
    aliases
}

/// Removes `.` and resolves `..` lexically, without touching the filesystem.
fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn layout() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("private/tmp/workspace");
        std::fs::create_dir_all(&root).unwrap();
        symlink(dir.path().join("private/tmp"), dir.path().join("tmp")).unwrap();
        let canonical = std::fs::canonicalize(root).unwrap();
        let logical = dir.path().join("tmp/workspace");
        (dir, canonical, logical)
    }

    #[test]
    fn cd_alias_is_absolute_normalized_and_deduplicated() {
        let (dir, root, logical) = layout();
        for cd in [
            logical.join("./child/.."),
            PathBuf::from("tmp/./workspace/"),
        ] {
            let aliases = workspace_root_aliases(&root, Some(&cd), dir.path(), None);
            assert_eq!(aliases[0], root);
            assert!(aliases.contains(&logical));
            assert_eq!(aliases.iter().filter(|p| *p == &logical).count(), 1);
        }
        let aliases = workspace_root_aliases(&root, Some(&root), dir.path(), None);
        assert_eq!(aliases.iter().filter(|p| *p == &root).count(), 1);
    }

    #[test]
    fn absolute_pwd_is_used_only_without_cd_and_when_it_matches() {
        let (dir, root, logical) = layout();
        assert!(workspace_root_aliases(&root, None, dir.path(), Some(&logical)).contains(&logical));
        assert!(
            !workspace_root_aliases(&root, Some(&root), dir.path(), Some(&logical))
                .contains(&logical)
        );
        for pwd in [
            dir.path().to_path_buf(),
            PathBuf::from("tmp/workspace"),
            dir.path().join("missing"),
        ] {
            assert!(!workspace_root_aliases(&root, None, dir.path(), Some(&pwd)).contains(&pwd));
        }
    }

    #[test]
    fn mismatched_cd_does_not_fall_back_to_pwd() {
        let (dir, root, logical) = layout();
        for cd in [dir.path().to_path_buf(), dir.path().join("missing")] {
            let aliases = workspace_root_aliases(&root, Some(&cd), dir.path(), Some(&logical));
            assert!(!aliases.contains(&cd));
            assert!(!aliases.contains(&logical));
        }
    }

    #[test]
    fn normalization_across_a_symlink_cannot_trust_a_different_directory() {
        let (dir, root, logical) = layout();
        symlink(&root, dir.path().join("link")).unwrap();
        std::fs::create_dir(dir.path().join("workspace")).unwrap();
        let cd = dir.path().join("link/../workspace");
        assert_eq!(std::fs::canonicalize(&cd).unwrap(), root);
        let aliases = workspace_root_aliases(&root, Some(&cd), dir.path(), Some(&logical));
        assert!(!aliases.contains(&dir.path().join("workspace")));
        assert!(!aliases.contains(&logical));
        // PWD retains its supplied spelling and is checked directly, since
        // lexical normalization could change which directory it designates.
        assert!(workspace_root_aliases(&root, None, dir.path(), Some(&cd)).contains(&cd));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn real_private_prefix_is_shortened_when_it_resolves_to_the_root() {
        let dir = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let Ok(suffix) = root.strip_prefix("/private") else {
            return;
        };
        let short = Path::new("/").join(suffix);
        let aliases = workspace_root_aliases(&root, None, &root, None);
        assert!(aliases.contains(&root));
        assert!(aliases.contains(&short));
        let aliases = workspace_root_aliases(&root, Some(&short), &root, None);
        assert_eq!(aliases.iter().filter(|p| *p == &short).count(), 1);
    }

    #[test]
    fn relative_cd_is_also_resolved_against_a_matching_logical_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let physical = std::fs::canonicalize(dir.path()).unwrap().join("project");
        std::fs::create_dir_all(physical.join("sub")).unwrap();
        let logical = std::fs::canonicalize(dir.path()).unwrap().join("link");
        symlink(&physical, &logical).unwrap();
        let root = std::fs::canonicalize(physical.join("sub")).unwrap();
        let aliases =
            workspace_root_aliases(&root, Some(Path::new("sub")), &physical, Some(&logical));
        assert!(aliases.contains(&logical.join("sub")), "{aliases:?}");
        // A PWD that names another directory is ignored even when PWD/sub
        // exists and resolves to the root.
        let other = std::fs::canonicalize(dir.path()).unwrap().join("other");
        std::fs::create_dir(&other).unwrap();
        symlink(physical.join("sub"), other.join("sub")).unwrap();
        let aliases =
            workspace_root_aliases(&root, Some(Path::new("sub")), &physical, Some(&other));
        assert!(!aliases.contains(&other.join("sub")), "{aliases:?}");
    }
}
