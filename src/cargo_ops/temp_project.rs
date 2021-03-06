use std::path::{Path, PathBuf};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::collections::HashSet;
use std::env;
use std::rc::Rc;
use std::cell::RefCell;

use tempdir::TempDir;
use toml::Value;
use toml::value::Table;
use cargo::util::errors::CargoResultExt;
use cargo::core::{PackageId, Workspace};
use cargo::util::{CargoError, CargoErrorKind, CargoResult, Config};
use cargo::ops::{update_lockfile, UpdateOptions};

use Options;
use super::{ElaborateWorkspace, Manifest};

/// A temporary project
pub struct TempProject<'tmp> {
    pub workspace: Rc<RefCell<Option<Workspace<'tmp>>>>,
    pub temp_dir: TempDir,
    manifest_paths: Vec<PathBuf>,
    config: Config,
    relative_manifest: String,
}

impl<'tmp> TempProject<'tmp> {
    /// Copy needed manifest and lock files from an existing workspace
    pub fn from_workspace(
        orig_workspace: &ElaborateWorkspace,
        orig_manifest: &str,
        options: &Options,
    ) -> CargoResult<TempProject<'tmp>> {
        // e.g. /path/to/project
        let workspace_root = orig_workspace.workspace.root().to_str().ok_or_else(|| {
            CargoError::from_kind(CargoErrorKind::Msg(format!(
                "Invalid character found in path {}",
                orig_workspace.workspace.root().to_string_lossy()
            )))
        })?;

        let temp_dir = TempDir::new("cargo-outdated")?;
        let manifest_paths = manifest_paths(orig_workspace)?;
        let mut tmp_manifest_paths = vec![];
        for from in &manifest_paths {
            // e.g. /path/to/project/src/sub
            let mut from_dir = from.clone();
            from_dir.pop();
            let from_dir = from_dir.to_string_lossy();
            // e.g. /tmp/cargo.xxx/src/sub
            let mut dest = PathBuf::from(format!(
                "{}/{}",
                temp_dir.path().to_string_lossy(),
                &from_dir[workspace_root.len()..]
            ));
            fs::create_dir_all(&dest)?;
            // e.g. /tmp/cargo.xxx/src/sub/Cargo.toml
            dest.push("Cargo.toml");
            tmp_manifest_paths.push(dest.clone());
            fs::copy(from, &dest)?;
            let lockfile = PathBuf::from(format!("{}/Cargo.lock", from_dir));
            if lockfile.is_file() {
                dest.pop();
                dest.push("Cargo.lock");
                fs::copy(lockfile, dest)?;
            }
        }
        Self::write_manifest_semver_with_paths(
            &tmp_manifest_paths,
            workspace_root,
            &temp_dir.path().to_string_lossy(),
        )?;

        // virtual root
        let mut virtual_root = PathBuf::from(format!("{}/Cargo.toml", workspace_root));
        if !manifest_paths.contains(&virtual_root) && virtual_root.is_file() {
            fs::copy(
                &virtual_root,
                format!("{}/Cargo.toml", temp_dir.path().to_string_lossy()),
            )?;
            virtual_root.pop();
            virtual_root.push("Cargo.lock");
            if virtual_root.is_file() {
                fs::copy(
                    &virtual_root,
                    format!("{}/Cargo.lock", temp_dir.path().to_string_lossy()),
                )?;
            }
        }

        let relative_manifest =
            String::from(&orig_manifest[orig_workspace.workspace.root().to_string_lossy().len()..]);
        let config = Self::generate_config(
            &temp_dir.path().to_string_lossy(),
            &relative_manifest,
            options,
        )?;
        Ok(TempProject {
            // workspace: Workspace::new(Path::new(&root_manifest), config)?,
            workspace: Rc::new(RefCell::new(None)),
            temp_dir: temp_dir,
            manifest_paths: tmp_manifest_paths,
            config: config,
            relative_manifest: relative_manifest,
        })
    }

    fn generate_config(
        root: &str,
        relative_manifest: &str,
        options: &Options,
    ) -> CargoResult<Config> {
        let shell = ::cargo::core::Shell::new();
        let cwd = env::current_dir()
            .chain_err(|| "Cargo couldn't get the current directory of the process")?;

        let homedir = ::cargo::util::homedir(&cwd).ok_or_else(|| {
            "Cargo couldn't find your home directory. \
             This probably means that $HOME was not set."
        })?;
        let mut cwd = PathBuf::from(format!("{}/{}", root, relative_manifest));
        cwd.pop();
        let config = Config::new(shell, cwd, homedir);
        config.configure(
            0,
            if options.flag_verbose > 0 {
                None
            } else {
                Some(true)
            },
            &options.flag_color,
            options.flag_frozen,
            options.flag_locked,
            &[],
        )?;
        Ok(config)
    }

    /// Run `cargo update` against the temporary project
    pub fn cargo_update(&self) -> CargoResult<()> {
        let update_opts = UpdateOptions {
            aggressive: false,
            precise: None,
            to_update: &[],
            config: &self.config,
        };
        update_lockfile(self.workspace.borrow().as_ref().unwrap(), &update_opts)?;
        Ok(())
    }

    fn write_manifest<P: AsRef<Path>>(manifest: &Manifest, path: P) -> CargoResult<()> {
        let mut file = try!(File::create(path));
        let serialized = ::toml::to_string(manifest).expect("Failed to serialized Cargo.toml");
        try!(write!(file, "{}", serialized));
        Ok(())
    }

    fn manipulate_dependencies(manifest: &mut Manifest, f: &Fn(&mut Table)) {
        manifest.dependencies.as_mut().map(f);
        manifest.dev_dependencies.as_mut().map(f);
        manifest.build_dependencies.as_mut().map(f);
        manifest
            .target
            .as_mut()
            .map(|ref mut t| for target in t.values_mut() {
                if let Value::Table(ref mut target) = *target {
                    for dependency_tables in
                        &["dependencies", "dev-dependencies", "build-dependencies"]
                    {
                        target.get_mut(*dependency_tables).map(|dep_table| {
                            if let Value::Table(ref mut dep_table) = *dep_table {
                                f(dep_table);
                            }
                        });
                    }
                }
            });
    }

    /// Write manifests with SemVer requirements
    pub fn write_manifest_semver(&'tmp self) -> CargoResult<()> {
        let root_manifest = format!(
            "{}/{}",
            self.temp_dir.path().to_string_lossy(),
            self.relative_manifest
        );
        *self.workspace.borrow_mut() =
            Some(Workspace::new(Path::new(&root_manifest), &self.config)?);
        Ok(())
    }

    fn write_manifest_semver_with_paths<P: AsRef<Path>>(
        manifest_paths: &[PathBuf],
        orig_root: P,
        tmp_root: P,
    ) -> CargoResult<()> {
        let bin = {
            let mut bin = Table::new();
            bin.insert("name".to_owned(), Value::String("test".to_owned()));
            bin.insert("path".to_owned(), Value::String("test.rs".to_owned()));
            bin
        };
        for manifest_path in manifest_paths {
            let mut manifest: Manifest = {
                let mut buf = String::new();
                let mut file = File::open(manifest_path)?;
                file.read_to_string(&mut buf)?;
                ::toml::from_str(&buf)?
            };
            manifest.bin = Some(vec![bin.clone()]);
            // provide lib.path
            manifest.lib.as_mut().map(|lib| {
                lib.insert("path".to_owned(), Value::String("test_lib.rs".to_owned()));
            });
            Self::manipulate_dependencies(&mut manifest, &|deps| {
                Self::replace_path_with_absolute(
                    deps,
                    orig_root.as_ref(),
                    tmp_root.as_ref(),
                    manifest_path,
                )
            });
            Self::write_manifest(&manifest, manifest_path)?;
        }

        Ok(())
    }

    /// Write manifests with wildcard requirements
    pub fn write_manifest_latest(&'tmp self) -> CargoResult<()> {
        let bin = {
            let mut bin = Table::new();
            bin.insert("name".to_owned(), Value::String("test".to_owned()));
            bin.insert("path".to_owned(), Value::String("test.rs".to_owned()));
            bin
        };
        for manifest_path in &self.manifest_paths {
            let mut manifest: Manifest = {
                let mut buf = String::new();
                let mut file = File::open(manifest_path)?;
                file.read_to_string(&mut buf)?;
                ::toml::from_str(&buf)?
            };
            manifest.bin = Some(vec![bin.clone()]);
            // provide lib.path
            manifest.lib.as_mut().map(|lib| {
                lib.insert("path".to_owned(), Value::String("test_lib.rs".to_owned()));
            });
            Self::manipulate_dependencies(&mut manifest, &Self::replace_version_with_wildcard);
            Self::write_manifest(&manifest, manifest_path)?;
        }

        let root_manifest = format!(
            "{}/{}",
            self.temp_dir.path().to_string_lossy(),
            self.relative_manifest
        );
        *self.workspace.borrow_mut() =
            Some(Workspace::new(Path::new(&root_manifest), &self.config)?);
        Ok(())
    }

    fn replace_version_with_wildcard(dependencies: &mut Table) {
        let dep_names: Vec<_> = dependencies.keys().cloned().collect();
        for name in dep_names {
            let original = dependencies.get(&name).cloned().unwrap();
            match original {
                Value::String(_) => {
                    dependencies.insert(name, Value::String("*".to_owned()));
                }
                Value::Table(ref t) => {
                    if t.contains_key("path") {
                        continue;
                    }
                    let mut replaced = t.clone();
                    if replaced.contains_key("version") {
                        replaced.insert("version".to_owned(), Value::String("*".to_owned()));
                    }
                    dependencies.insert(name, Value::Table(replaced));
                }
                _ => panic!("Dependency spec is neither a string nor a table {}", name),
            }
        }
    }

    fn replace_path_with_absolute(
        dependencies: &mut Table,
        orig_root: &Path,
        tmp_root: &Path,
        tmp_manifest: &Path,
    ) {
        let dep_names: Vec<_> = dependencies.keys().cloned().collect();
        for name in dep_names {
            let original = dependencies.get(&name).cloned().unwrap();
            match original {
                Value::Table(ref t) if t.contains_key("path") => {
                    if let Value::String(ref orig_path) = t["path"] {
                        let orig_path = Path::new(orig_path);
                        if orig_path.is_relative() {
                            let relative = {
                                let delimiter: &[_] = &['/', '\\'];
                                let relative = &tmp_manifest.to_string_lossy()
                                    [tmp_root.to_string_lossy().len()..];
                                let mut relative =
                                    PathBuf::from(relative.trim_left_matches(delimiter));
                                relative.pop();
                                relative.join(orig_path)
                            };
                            if !tmp_root.join(&relative).join("Cargo.toml").exists() {
                                let mut replaced = t.clone();
                                replaced.insert(
                                    "path".to_owned(),
                                    Value::String(
                                        fs::canonicalize(orig_root.join(relative))
                                            .unwrap()
                                            .to_string_lossy()
                                            .to_string(),
                                    ),
                                );
                                dependencies.insert(name, Value::Table(replaced));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// Paths of all manifest files in current workspace
fn manifest_paths(elab: &ElaborateWorkspace) -> CargoResult<Vec<PathBuf>> {
    let mut visited: HashSet<PackageId> = HashSet::new();
    let mut manifest_paths = vec![];

    fn manifest_paths_recursive(
        pkg_id: &PackageId,
        elab: &ElaborateWorkspace,
        workspace_path: &str,
        visited: &mut HashSet<PackageId>,
        manifest_paths: &mut Vec<PathBuf>,
    ) -> CargoResult<()> {
        if visited.contains(pkg_id) {
            return Ok(());
        }
        visited.insert(pkg_id.clone());
        let pkg = &elab.pkgs[pkg_id];
        let pkg_path = pkg.root().to_string_lossy();
        if pkg_path.starts_with(workspace_path) {
            manifest_paths.push(pkg.manifest_path().to_owned());
        }

        for dep in elab.pkg_deps[pkg_id].keys() {
            manifest_paths_recursive(dep, elab, workspace_path, visited, manifest_paths)?;
        }

        Ok(())
    };

    // executed against a virtual manifest
    let workspace_path = elab.workspace.root().to_string_lossy();
    // if cargo workspace is not explicitly used, the pacakge itself would be a member
    for member in elab.workspace.members() {
        let root_pkg_id = member.package_id();
        manifest_paths_recursive(
            root_pkg_id,
            elab,
            &workspace_path,
            &mut visited,
            &mut manifest_paths,
        )?;
    }

    Ok(manifest_paths)
}
