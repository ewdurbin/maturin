//! Host-platform symbol export tests for Linux (GNU or musl), macOS and Windows MSVC.
//! Binary fixtures are dependency-free; extension tests copy a PyO3 fixture.
//! Both build in private directories to preserve the checked-in fixtures.
#![cfg(any(target_os = "linux", target_os = "macos", target_env = "msvc"))]

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use flate2::read::GzDecoder;
use fs_err as fs;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;
use zip::ZipArchive;

fn checked(cmd: &mut Command) -> Output {
    let output = cmd.output().unwrap_or_else(|err| panic!("{cmd:?}: {err}"));
    assert!(
        output.status.success(),
        "{cmd:?}: {}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn project() -> Result<TempDir> {
    let temp = tempfile::tempdir()?;
    fs::create_dir_all(temp.path().join("src/bin"))?;
    fs::write(
        temp.path().join("Cargo.toml"),
        r#"[package]
name = "symbols-test"
version = "1.0.0"
edition = "2021"

[profile.ship]
inherits = "release"
debug = 2
strip = true
"#,
    )?;
    fs::write(
        temp.path().join("pyproject.toml"),
        r#"[build-system]
requires = ["maturin"]
build-backend = "maturin"

[tool.maturin]
bindings = "bin"
strip = true
"#,
    )?;
    for name in ["one", "two"] {
        fs::write(
            temp.path().join(format!("src/bin/{name}.rs")),
            format!(
                "#[inline(never)]\nfn message() -> &'static str {{ \"{name} symbols\" }}\nfn main() {{ println!(\"{{}}\", message()); }}\n"
            ),
        )?;
    }
    Ok(temp)
}

fn pyo3_project() -> Result<TempDir> {
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("test-crates/pyo3-stub-generation-pure");
    fs::create_dir(root.join("src"))?;
    for path in ["Cargo.toml", "Cargo.lock", "pyproject.toml", "src/lib.rs"] {
        fs::copy(fixture.join(path), root.join(path))?;
    }
    let manifest = root.join("Cargo.toml");
    let mut toml = fs::read_to_string(&manifest)?;
    toml.push_str("\n[profile.ship]\ninherits = \"release\"\ndebug = 2\nstrip = true\n");
    fs::write(&manifest, toml)?;
    Ok(temp)
}

fn maturin(root: &Path) -> Command {
    maturin_with_symbols(root, Path::new("symbols"))
}

fn maturin_with_symbols(root: &Path, symbols: &Path) -> Command {
    let mut cmd = maturin_with_default_symbols(root);
    cmd.args(["--out", "wheels", "--debug-symbols-out"])
        .arg(symbols);
    cmd
}

fn maturin_with_default_symbols(root: &Path) -> Command {
    let mut cmd = build_command(root);
    cmd.args(["--auditwheel", "skip", "--debug-symbols"]);
    cmd
}

fn build_command(root: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_maturin"));
    // CI disables debuginfo to speed up building maturin itself. These
    // fixtures control their own flags, including tests of flag precedence.
    cmd.env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CARGO_BUILD_RUSTFLAGS");
    cmd.current_dir(root).args(["build", "--profile", "ship"]);
    cmd
}

fn mixed_project() -> Result<TempDir> {
    let temp = project()?;
    let root = temp.path();
    let pyproject = root.join("pyproject.toml");
    let mut content = fs::read_to_string(&pyproject)?;
    content.push_str("module-name = \"symbols_test\"\npython-source = \"python\"\n");
    fs::write(&pyproject, content)?;
    let package = root.join("python/symbols_test");
    fs::create_dir_all(&package)?;
    fs::write(package.join("__init__.py"), "# This must be in the wheel\n")?;
    Ok(temp)
}

fn entries(path: &Path) -> Result<Vec<PathBuf>> {
    let mut entries = fs::read_dir(path)?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort();
    Ok(entries)
}

fn read_wheel(wheel: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut zip = ZipArchive::new(fs::File::open(wheel)?)?;
    let mut files = BTreeMap::new();
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        if entry.is_dir() {
            continue;
        }
        assert!(!entry.name().ends_with(".pdb"));
        assert!(!entry.name().ends_with(".debug"));
        assert!(!entry.name().contains(".dSYM/"));
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        files.insert(entry.name().to_owned(), bytes);
    }
    let (record_path, record) = files
        .iter()
        .find(|(p, _)| p.ends_with(".dist-info/RECORD"))
        .context("missing RECORD")?;
    let record = std::str::from_utf8(record)?;
    for (path, bytes) in &files {
        if path == record_path {
            assert!(record.lines().any(|line| line == format!("{path},,")));
        } else {
            let hash = URL_SAFE_NO_PAD.encode(Sha256::digest(bytes));
            let expected = format!("{path},sha256={hash},{}", bytes.len());
            assert!(record.lines().any(|line| line == expected), "{expected}");
        }
    }
    Ok(files)
}

#[cfg(target_os = "macos")]
fn verify_symbols(exe: &Path, debug: &Path, cargo: &Path) -> Result<()> {
    checked(
        Command::new("codesign")
            .args(["--verify", "--strict"])
            .arg(exe),
    );
    let uuid = |path: &Path| {
        let output = checked(Command::new("dwarfdump").arg("--uuid").arg(path));
        String::from_utf8(output.stdout)
            .unwrap()
            .split_whitespace()
            .nth(1)
            .expect("Mach-O UUID")
            .to_owned()
    };
    assert_eq!(uuid(exe), uuid(debug));
    assert_eq!(uuid(exe), uuid(cargo));
    let dwarf = checked(Command::new("dwarfdump").arg("--debug-info").arg(debug));
    assert!(String::from_utf8_lossy(&dwarf.stdout).contains("DW_TAG_compile_unit"));
    assert!(fs::metadata(exe)?.len() < fs::metadata(cargo)?.len());
    Ok(())
}

#[cfg(target_os = "linux")]
fn verify_symbols(exe: &Path, debug: &Path, cargo: &Path) -> Result<()> {
    fn section(file: &Path, name: &str) -> Result<Option<Vec<u8>>> {
        let bytes = fs::read(file)?;
        let elf = goblin::elf::Elf::parse(&bytes)?;
        Ok(elf.section_headers.iter().find_map(|header| {
            if elf.shdr_strtab.get_at(header.sh_name) == Some(name)
                && header.sh_type != goblin::elf::section_header::SHT_NOBITS
            {
                let start = header.sh_offset as usize;
                Some(bytes[start..start + header.sh_size as usize].to_vec())
            } else {
                None
            }
        }))
    }
    assert!(section(exe, ".debug_info")?.is_none());
    assert!(section(cargo, ".debug_info")?.is_some());
    assert_eq!(
        section(debug, ".debug_info")?,
        section(cargo, ".debug_info")?
    );
    assert_eq!(
        section(debug, ".note.gnu.build-id")?,
        section(exe, ".note.gnu.build-id")?
    );
    let link = section(exe, ".gnu_debuglink")?.context("missing gnu_debuglink")?;
    let filename = debug.file_name().unwrap().to_string_lossy();
    assert!(link.starts_with(filename.as_bytes()));
    assert!(fs::metadata(exe)?.len() < fs::metadata(cargo)?.len());
    Ok(())
}

#[cfg(target_env = "msvc")]
fn verify_symbols(exe: &Path, debug: &Path, cargo: &Path) -> Result<()> {
    let bytes = fs::read(exe)?;
    // No PE strip: CodeView identity must be byte-for-byte identical to Cargo.
    assert_eq!(bytes, fs::read(cargo)?);
    assert!(goblin::pe::PE::parse(&bytes)?.debug_data.is_some());
    assert_eq!(fs::read(debug)?, fs::read(cargo.with_extension("pdb"))?);
    Ok(())
}

#[test]
fn exports_matching_symbols_and_preserves_cached_binaries() -> Result<()> {
    let temp = project()?;
    let root = temp.path();
    let mut original = BTreeMap::new();
    for pass in 0..2 {
        checked(&mut maturin(root));
        let wheels = entries(&root.join("wheels"))?;
        assert_eq!(wheels.len(), 1);
        let exported = entries(&root.join("symbols"))?;
        assert_eq!(exported.len(), 1, "no intermediate export directories");
        assert_eq!(
            exported[0].file_name().unwrap().to_string_lossy(),
            format!(
                "{}.symbols",
                wheels[0].file_name().unwrap().to_string_lossy()
            )
        );
        assert_eq!(entries(&exported[0])?.len(), 2, "no stale companions");
        let files = read_wheel(&wheels[0])?;
        for name in ["one", "two"] {
            let exe_name = format!("{name}{}", std::env::consts::EXE_SUFFIX);
            let (_, bytes) = files
                .iter()
                .find(|(path, _)| path.ends_with(&format!(".data/scripts/{exe_name}")))
                .context("missing wheel executable")?;
            let exe = root.join(format!("unpacked-{exe_name}"));
            fs::write(&exe, bytes)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755))?;
            }
            let output = checked(&mut Command::new(&exe));
            assert_eq!(
                String::from_utf8(output.stdout)?.trim(),
                format!("{name} symbols")
            );
            let cargo = root.join("target/ship").join(&exe_name);
            let cache = fs::read(&cargo)?;
            if pass == 0 {
                original.insert(name, cache);
            } else {
                assert_eq!(
                    original[name], cache,
                    "Cargo artifact changed on cached build"
                );
            }
            #[cfg(target_os = "macos")]
            let debug = exported[0].join(format!("{exe_name}.dSYM"));
            #[cfg(target_os = "linux")]
            let debug = exported[0].join(format!("{exe_name}.debug"));
            #[cfg(target_env = "msvc")]
            let debug = exported[0].join(format!("{name}.pdb"));
            verify_symbols(&exe, &debug, &cargo)?;
        }
        // Must disappear only once a new wheel has successfully been written.
        fs::write(exported[0].join("stale-companion"), "stale")?;
    }
    Ok(())
}

#[test]
fn export_failure_restores_cargo_and_keeps_previous_symbols() -> Result<()> {
    let temp = project()?;
    let root = temp.path();
    checked(&mut maturin(root));
    let symbols = entries(&root.join("symbols"))?;
    let marker = symbols[0].join("keep-until-success");
    fs::write(&marker, "previous wheel")?;
    // Make the export location unwritable as a directory after a good build.
    // The build can still repair/stage all binaries, but cannot stage exports.
    let mut cmd = maturin(root);
    // A filesystem failure after copy-back (not a clap argument failure).
    fs::rename(root.join("symbols"), root.join("old-symbols"))?;
    fs::write(root.join("symbols"), "not a directory")?;
    let output = cmd.output()?;
    assert!(!output.status.success());
    assert!(
        fs::read_to_string(
            root.join("old-symbols")
                .join(symbols[0].file_name().unwrap())
                .join("keep-until-success")
        )?
        .contains("previous wheel")
    );
    for name in ["one", "two"] {
        let cargo = root
            .join("target/ship")
            .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        checked(&mut Command::new(cargo));
    }
    Ok(())
}

#[test]
fn previous_exports_stay_excluded_when_changing_or_disabling_export() -> Result<()> {
    let temp = mixed_project()?;
    let root = temp.path();
    let package = root.join("python/symbols_test");
    for name in ["exports-a", "exports-b"] {
        let symbols = package.join(name);
        fs::create_dir(&symbols)?;
        fs::write(symbols.join("public.txt"), "ordinary package data")?;
        checked(&mut maturin_with_symbols(root, &symbols));
        let wheels = entries(&root.join("wheels"))?;
        let files = read_wheel(&wheels[0])?;
        assert!(files.contains_key(&format!("symbols_test/{name}/public.txt")));
        assert!(files.keys().all(|path| !path.contains(".whl.symbols/")));
    }

    // Neither a normal wheel nor a standalone sdist has an active export dir.
    checked(build_command(root).args(["--auditwheel", "skip", "--out", "wheels"]));
    let wheels = entries(&root.join("wheels"))?;
    let files = read_wheel(&wheels[0])?;
    for name in ["exports-a", "exports-b"] {
        assert!(files.contains_key(&format!("symbols_test/{name}/public.txt")));
    }
    assert!(files.keys().all(|path| !path.contains(".whl.symbols/")));
    checked(
        Command::new(env!("CARGO_BIN_EXE_maturin"))
            .current_dir(root)
            .args(["sdist", "--out", "sdists"]),
    );
    let sdists = entries(&root.join("sdists"))?;
    let decoder = GzDecoder::new(fs::File::open(&sdists[0])?);
    for entry in tar::Archive::new(decoder).entries()? {
        let entry = entry?;
        let path = entry.path()?.to_string_lossy().into_owned();
        assert!(!path.contains(".whl.symbols/"), "leaked into sdist: {path}");
    }
    Ok(())
}

#[test]
#[cfg_attr(
    all(windows, target_arch = "aarch64"),
    ignore = "possible windows aarch64 pgo issue, see https://github.com/rust-lang/rust/issues/156675"
)]
fn pgo_only_replaces_exports_after_the_optimized_build() -> Result<()> {
    if std::env::var_os("MATURIN_TEST_SKIP_PGO").is_some() || !crate::common::has_uv() {
        return Ok(());
    }
    let temp = project()?;
    let root = temp.path();
    let pyproject = root.join("pyproject.toml");
    let mut config = fs::read_to_string(&pyproject)?;
    config.push_str("pgo-command = 'python train.py'\n");
    fs::write(pyproject, config)?;
    fs::write(
        root.join("train.py"),
        r#"from pathlib import Path
import subprocess

exports = list(Path("symbols").iterdir())
assert len(exports) == 1, exports
assert (exports[0] / "keep-until-success").read_text() == "previous wheel"
for name in ("one", "two"):
    subprocess.run([name], check=True)
"#,
    )?;
    checked(&mut maturin(root));
    let exports = entries(&root.join("symbols"))?;
    let marker = exports[0].join("keep-until-success");
    fs::write(&marker, "previous wheel")?;
    let python = std::env::var_os("MATURIN_TEST_PYTHON")
        .unwrap_or_else(|| if cfg!(windows) { "python" } else { "python3" }.into());
    checked(maturin(root).args(["--pgo", "--interpreter"]).arg(python));
    assert!(!marker.exists(), "optimized build must replace old exports");
    assert_eq!(entries(&root.join("symbols"))?, exports);
    assert_eq!(entries(&exports[0])?.len(), 2);
    let wheels = entries(&root.join("wheels"))?;
    let files = read_wheel(&wheels[0])?;
    for name in ["one", "two"] {
        let exe_name = format!("{name}{}", std::env::consts::EXE_SUFFIX);
        let (_, bytes) = files
            .iter()
            .find(|(path, _)| path.ends_with(&format!(".data/scripts/{exe_name}")))
            .context("missing optimized executable")?;
        let exe = root.join(format!("unpacked-{exe_name}"));
        fs::write(&exe, bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755))?;
        }
        checked(&mut Command::new(&exe));
        #[cfg(target_os = "macos")]
        let debug = exports[0].join(format!("{exe_name}.dSYM"));
        #[cfg(target_os = "linux")]
        let debug = exports[0].join(format!("{exe_name}.debug"));
        #[cfg(target_env = "msvc")]
        let debug = exports[0].join(format!("{name}.pdb"));
        verify_symbols(&exe, &debug, &root.join("target/ship").join(&exe_name))?;
    }
    Ok(())
}

#[test]
#[cfg(all(
    feature = "auditwheel",
    any(target_os = "macos", all(target_os = "linux", target_env = "gnu"))
))]
fn repairs_shared_libraries_before_exporting_symbols() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let temp = project()?;
    let root = temp.path();
    let native = root.join("native");
    fs::create_dir(&native)?;
    fs::write(
        native.join("symbols.c"),
        "int symbols_number(void) { return 42; }\n",
    )?;
    #[cfg(target_os = "macos")]
    let lib = native.join("libsymbols_test.dylib");
    #[cfg(target_os = "linux")]
    let lib = native.join("libsymbols_test.so");
    let mut cc = Command::new("cc");
    cc.args(["-g", "-fPIC"]);
    #[cfg(target_os = "macos")]
    cc.arg("-dynamiclib")
        .arg(format!("-Wl,-install_name,{}", lib.display()));
    #[cfg(target_os = "linux")]
    cc.args(["-shared", "-Wl,-soname,libsymbols_test.so"]);
    checked(cc.arg(native.join("symbols.c")).arg("-o").arg(&lib));
    fs::write(
        root.join("build.rs"),
        r#"fn main() {
    let native = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("native");
    println!("cargo:rustc-link-search=native={}", native.display());
    println!("cargo:rustc-link-lib=dylib=symbols_test");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", native.display());
}
"#,
    )?;
    for name in ["one", "two"] {
        fs::write(
            root.join(format!("src/bin/{name}.rs")),
            "extern \"C\" { fn symbols_number() -> i32; }\nfn main() { println!(\"{}\", unsafe { symbols_number() }); }\n",
        )?;
    }
    checked(build_command(root).args([
        "--auditwheel",
        "repair",
        "--debug-symbols",
        "--out",
        "wheels",
        "--debug-symbols-out",
        "symbols",
    ]));
    let wheels = entries(&root.join("wheels"))?;
    let files = read_wheel(&wheels[0])?;
    #[cfg(target_os = "macos")]
    let lib_dir = "symbols_test.dylibs/";
    #[cfg(target_os = "linux")]
    let lib_dir = "symbols_test.libs/";
    assert!(
        files.keys().any(|p| p.starts_with(lib_dir)),
        "repair must bundle the external library"
    );
    let unpacked = root.join("unpacked");
    let mut zip = ZipArchive::new(fs::File::open(&wheels[0])?)?;
    zip.extract(&unpacked)?;
    let exports = entries(&root.join("symbols"))?;
    assert_eq!(exports.len(), 1, "only the repaired wheel's symbols");
    assert_eq!(entries(&exports[0])?.len(), 2);
    // Prove the packaged executable uses the repaired dependency path.
    fs::remove_file(&lib)?;
    for name in ["one", "two"] {
        assert!(
            files.contains_key(&format!("symbols_test-1.0.0.data/scripts/{name}")),
            "missing shim"
        );
        let exe = unpacked.join("symbols_test.scripts").join(name);
        fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755))?;
        let output = checked(&mut Command::new(&exe));
        assert_eq!(String::from_utf8(output.stdout)?.trim(), "42");
        #[cfg(target_os = "macos")]
        let debug = exports[0].join(format!("{name}.dSYM"));
        #[cfg(target_os = "linux")]
        let debug = exports[0].join(format!("{name}.debug"));
        verify_symbols(&exe, &debug, &root.join("target/ship").join(name))?;
    }
    Ok(())
}

#[test]
fn conflicting_symbol_outputs_are_rejected() -> Result<()> {
    let temp = project()?;
    let output = maturin(temp.path()).arg("--include-debuginfo").output()?;
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("--include-debuginfo") && error.contains("--debug-symbols"),
        "{error}"
    );
    Ok(())
}

#[test]
fn symbol_output_requires_explicit_export() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let output = Command::new(env!("CARGO_BIN_EXE_maturin"))
        .current_dir(temp.path())
        .args(["build", "--debug-symbols-out", "symbols"])
        .output()?;
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("required arguments were not provided") && error.contains("--debug-symbols"),
        "{error}"
    );
    assert!(!temp.path().join("symbols").exists());
    Ok(())
}

#[test]
fn symbol_exports_default_to_the_final_wheel_directory() -> Result<()> {
    for (out, from_sdist) in [(Some("dist"), false), (None, false), (None, true)] {
        let temp = project()?;
        let root = temp.path();
        let mut cmd = maturin_with_default_symbols(root);
        if let Some(dir) = out {
            cmd.args(["--out", dir]);
        }
        if from_sdist {
            cmd.arg("--sdist");
        }
        checked(&mut cmd);
        let dir = root.join(out.unwrap_or("target/wheels"));
        let artifacts = entries(&dir)?;
        let wheel = artifacts
            .iter()
            .find(|p| p.extension().is_some_and(|e| e == "whl"))
            .context("wheel not produced in the final output directory")?;
        // Also verifies RECORD and that the companions are not in the wheel.
        read_wheel(wheel)?;
        let symbols = dir.join(format!(
            "{}.symbols",
            wheel.file_name().unwrap().to_string_lossy()
        ));
        assert_eq!(entries(&symbols)?.len(), 2);
        assert_eq!(
            artifacts.len(),
            if from_sdist { 3 } else { 2 },
            "expected only a wheel, its symbols directory, and (if requested) the sdist"
        );
    }
    Ok(())
}

#[test]
fn absent_debug_information_fails_without_publishing() -> Result<()> {
    let temp = project()?;
    let output = maturin(temp.path())
        .env("CARGO_PROFILE_SHIP_DEBUG", "0")
        .output()?;
    assert!(
        !output.status.success(),
        "export with no debug information succeeded"
    );
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("CARGO_PROFILE_RELEASE_DEBUG=2"),
        "missing actionable debug configuration error: {error}"
    );
    let symbols = temp.path().join("symbols");
    assert!(!symbols.exists() || entries(&symbols)?.is_empty());
    Ok(())
}

#[test]
fn excludes_exports_in_external_data_even_on_the_first_build() -> Result<()> {
    let temp = project()?;
    let root = temp.path();
    let shared = tempfile::tempdir()?;
    let data = shared.path().join("data");
    let symbols = data.join("artifacts[debug]/exports");
    // data can be outside the Cargo/pyproject root; paths relative to the
    // project may contain "..", and output paths may be absolute.
    assert_eq!(shared.path().parent(), root.parent());
    let relative = Path::new("..").join(shared.path().file_name().unwrap());
    let pyproject = root.join("pyproject.toml");
    let mut config = fs::read_to_string(&pyproject)?;
    config.push_str(&format!(
        "data = '{}'\n",
        relative.to_string_lossy().replace('\\', "/")
    ));
    fs::write(pyproject, config)?;
    fs::create_dir_all(&data)?;
    fs::write(data.join("public.txt"), "public data")?;
    for pass in 0..2 {
        checked(&mut maturin_with_symbols(root, &symbols));
        let wheels = entries(&root.join("wheels"))?;
        let files = read_wheel(&wheels[0])?;
        assert!(files.contains_key("symbols_test-1.0.0.data/data/public.txt"));
        for path in files.keys() {
            assert!(!path.contains("exports/.maturin-symbols-"), "{path}");
            assert!(!path.contains("exports/previous.whl.symbols/"), "{path}");
            assert!(!path.ends_with("private-alias.txt"), "{path}");
        }
        if pass == 0 {
            // Ordinary files alongside generated exports must still ship.
            let previous = symbols.join("previous.whl.symbols");
            fs::create_dir(&previous)?;
            fs::write(previous.join("private.txt"), "do not ship")?;
            fs::write(symbols.join("public.txt"), "public next to exports")?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(
                previous.join("private.txt"),
                data.join("private-alias.txt"),
            )?;
        } else {
            assert!(
                files.contains_key(
                    "symbols_test-1.0.0.data/data/artifacts[debug]/exports/public.txt"
                )
            );
        }
    }
    Ok(())
}

#[test]
fn excludes_exports_from_python_source_outside_the_project() -> Result<()> {
    let temp = project()?;
    let root = temp.path();
    let external = tempfile::tempdir()?;
    let package = external.path().join("symbols_test");
    let symbols = package.join("symbols");
    let previous = symbols.join("previous.whl.symbols");
    fs::create_dir_all(&previous)?;
    fs::write(
        package.join("__init__.py"),
        "# included from external python-source\n",
    )?;
    fs::write(symbols.join("public.txt"), "ordinary python package data")?;
    fs::write(previous.join("private.txt"), "private symbol data")?;
    let pyproject = root.join("pyproject.toml");
    let mut config = fs::read_to_string(&pyproject)?;
    config.push_str(&format!(
        "module-name = 'symbols_test'\npython-source = '{}'\ninclude = [{{ path = 'symbols_test/**/*', format = 'wheel' }}]\n",
        external.path().to_string_lossy().replace('\\', "/")
    ));
    fs::write(pyproject, config)?;
    for _ in 0..2 {
        checked(&mut maturin_with_symbols(root, &symbols));
        let wheels = entries(&root.join("wheels"))?;
        let files = read_wheel(&wheels[0])?;
        assert!(files.contains_key("symbols_test/__init__.py"));
        assert!(files.contains_key("symbols_test/symbols/public.txt"));
        for path in files.keys() {
            assert!(!path.contains(".whl.symbols/"), "leaked {path}");
            assert!(!path.contains(".maturin-symbols-"), "leaked {path}");
        }
    }
    Ok(())
}

#[test]
fn bin_and_lib_companions_always_belong_to_their_cargo_target() -> Result<()> {
    let temp = project()?;
    let root = temp.path();
    fs::write(
        root.join("src/lib.rs"),
        "#[inline(never)]\npub fn message() -> &'static str { \"from library\" }\n",
    )?;
    for name in ["one", "two"] {
        fs::write(
            root.join(format!("src/bin/{name}.rs")),
            "fn main() { println!(\"{}\", symbols_test::message()); }\n",
        )?;
    }
    // Cargo emits both the package library and the binary. Check repeated
    // cached builds, whose randomized artifact maps previously lost companions.
    for _ in 0..8 {
        checked(&mut maturin(root));
        let wheels = entries(&root.join("wheels"))?;
        let exported = entries(&root.join("symbols"))?;
        assert_eq!(exported.len(), 1);
        assert_eq!(entries(&exported[0])?.len(), 2);
        let files = read_wheel(&wheels[0])?;
        for name in ["one", "two"] {
            let exe_name = format!("{name}{}", std::env::consts::EXE_SUFFIX);
            let (_, bytes) = files
                .iter()
                .find(|(path, _)| path.ends_with(&format!(".data/scripts/{exe_name}")))
                .context("missing wheel binary")?;
            let exe = root.join(format!("unpacked-{exe_name}"));
            fs::write(&exe, bytes)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755))?;
            }
            #[cfg(target_os = "macos")]
            let debug_name = format!("{exe_name}.dSYM");
            #[cfg(target_os = "linux")]
            let debug_name = format!("{exe_name}.debug");
            #[cfg(target_env = "msvc")]
            let debug_name = format!("{name}.pdb");
            verify_symbols(
                &exe,
                &exported[0].join(debug_name),
                &root.join("target/ship").join(&exe_name),
            )?;
        }
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_env = "msvc"))]
#[test]
fn debuginfo_overrides_follow_actual_cargo_rustc_precedence() -> Result<()> {
    let temp = project()?;
    let root = temp.path();
    // Rust flags win over profile debuginfo (also reported on cached builds).
    for _ in 0..2 {
        checked(
            maturin(root)
                .env("CARGO_PROFILE_SHIP_DEBUG", "0")
                .env("RUSTFLAGS", "-g"),
        );
    }
    // RUSTFLAGS come after cargo rustc's trailing flags. A plain profile check
    // or the inverse ordering would accept this without any project debug info.
    let output = maturin(root)
        .env("RUSTFLAGS", "-Cdebuginfo=0")
        .args(["--", "-Cdebuginfo=2"])
        .output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("CARGO_PROFILE_RELEASE_DEBUG=2"));
    // Direct flags work in the absence of a competing RUSTFLAGS value.
    checked(
        maturin(root)
            .env("CARGO_PROFILE_SHIP_DEBUG", "0")
            .args(["--", "-C", "debuginfo=2"]),
    );
    Ok(())
}

#[test]
fn excludes_debug_symbols_inside_python_package_for_absolute_and_relative_paths() -> Result<()> {
    let temp = mixed_project()?;
    let root = temp.path();
    // Also exercise literal glob metacharacters in an output path.
    let symbols = root.join("python/symbols_test/artifacts[debug]/nested");
    let marker = symbols.join("previous.whl.symbols");
    let caller = root.join("caller");
    fs::create_dir(&caller)?;
    for pass in 0..2 {
        if pass == 0 {
            checked(&mut maturin_with_symbols(root, &symbols));
        } else {
            // Relative output directories are relative to cwd, not to the
            // manifest or matcher's root. Reuse the first build's exports.
            let relative = Path::new("../python/symbols_test/artifacts[debug]/nested");
            let mut cmd = maturin_with_symbols(&caller, relative);
            cmd.args(["--manifest-path", "../Cargo.toml"]);
            checked(&mut cmd);
        }
        let wheels = if pass == 0 {
            entries(&root.join("wheels"))?
        } else {
            entries(&caller.join("wheels"))?
        };
        assert_eq!(wheels.len(), 1);
        let files = read_wheel(&wheels[0])?;
        assert!(files.contains_key("symbols_test/__init__.py"));
        for path in files.keys() {
            assert!(
                !path.contains(".whl.symbols/"),
                "leaked prior symbols: {path}"
            );
            assert!(
                !path.contains(".maturin-symbols-"),
                "leaked in-progress symbols: {path}"
            );
        }
        if pass == 0 {
            assert_eq!(entries(&symbols)?.len(), 1, "new symbols output");
            // A rebuild must exclude both real and stale companions while
            // preserving normal data stored in the same output directory.
            fs::create_dir(&marker)?;
            fs::write(marker.join("must-not-ship.debug"), "private debug data")?;
            fs::write(symbols.join("public.txt"), "ordinary package data")?;
        } else {
            assert!(files.contains_key("symbols_test/artifacts[debug]/nested/public.txt"));
            assert_eq!(
                entries(&symbols)?.len(),
                3,
                "marker, public.txt, and the completed wheel's symbols"
            );
        }
    }
    Ok(())
}

#[test]
fn builds_wheel_through_sdist_without_exporting_symbols_into_either_archive() -> Result<()> {
    let temp = mixed_project()?;
    let root = temp.path();
    let symbols = root.join("python/symbols_test/symbols");
    let marker = symbols.join("previous.whl.symbols");
    fs::create_dir_all(&marker)?;
    fs::write(marker.join("must-not-ship.debug"), "private debug data")?;
    checked(maturin_with_symbols(root, &symbols).arg("--sdist"));
    let artifacts = entries(&root.join("wheels"))?;
    assert_eq!(artifacts.len(), 2, "one wheel and one source distribution");
    let wheel = artifacts
        .iter()
        .find(|p| p.extension().is_some_and(|e| e == "whl"))
        .context("wheel not produced")?;
    let sdist = artifacts
        .iter()
        .find(|p| p.extension().is_some_and(|e| e == "gz"))
        .context("sdist not produced")?;
    let files = read_wheel(wheel)?;
    assert!(files.contains_key("symbols_test/__init__.py"));
    let decoder = GzDecoder::new(fs::File::open(sdist)?);
    for entry in tar::Archive::new(decoder).entries()? {
        let entry = entry?;
        let path = entry.path()?.to_string_lossy().into_owned();
        assert!(
            !path.contains(".whl.symbols/"),
            "leaked symbols into sdist: {path}"
        );
        assert!(
            !path.contains(".maturin-symbols-"),
            "leaked in-progress symbols: {path}"
        );
    }
    let dir = symbols.join(format!(
        "{}.symbols",
        wheel.file_name().unwrap().to_string_lossy()
    ));
    assert_eq!(
        entries(&dir)?.len(),
        2,
        "wheel symbols must still be exported"
    );
    Ok(())
}

#[test]
fn generates_pyo3_stubs_before_stripping_exported_symbols() -> Result<()> {
    let temp = pyo3_project()?;
    let root = temp.path();

    let python = std::env::var_os("MATURIN_TEST_PYTHON")
        .unwrap_or_else(|| if cfg!(windows) { "python" } else { "python3" }.into());
    let mut cmd = maturin(root);
    cmd.arg("--generate-stubs")
        .arg("--interpreter")
        .arg(&python);
    checked(&mut cmd);
    let wheels = entries(&root.join("wheels"))?;
    assert_eq!(wheels.len(), 1);
    // This also checks that no companions leaked into the wheel and that
    // RECORD covers the final (stripped) library and the generated stubs.
    let files = read_wheel(&wheels[0])?;
    let package = "pyo3_stub_generation_pure";
    let init_stub = &files[&format!("{package}/__init__.pyi")];
    assert!(std::str::from_utf8(init_stub)?.contains("def func("));
    let submodule_stub = &files[&format!("{package}/submodule.pyi")];
    assert!(std::str::from_utf8(submodule_stub)?.contains("class Class2"));
    assert!(files.contains_key(&format!("{package}/py.typed")));

    let unpacked = root.join("unpacked");
    for (path, bytes) in &files {
        let dest = unpacked.join(path);
        fs::create_dir_all(dest.parent().unwrap())?;
        fs::write(dest, bytes)?;
    }
    let (library_path, _) = files
        .iter()
        .find(|(path, _)| {
            path.starts_with(&format!("{package}/"))
                && (path.ends_with(".so") || path.ends_with(".pyd"))
        })
        .context("wheel has no extension module")?;
    let cargo_name = format!(
        "{}{package}{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    );
    let cargo = root.join("target/ship").join(cargo_name);
    let exports = entries(&root.join("symbols"))?;
    assert_eq!(exports.len(), 1);
    let companions = entries(&exports[0])?;
    assert_eq!(companions.len(), 1);
    verify_symbols(&unpacked.join(library_path), &companions[0], &cargo)?;

    // The final, stripped extension must still be importable and usable.
    checked(Command::new(python).current_dir(&unpacked).args([
        "-c",
        "import pyo3_stub_generation_pure as m; assert m.func(42) == 42",
    ]));
    Ok(())
}

#[cfg(target_os = "macos")]
#[test]
fn refuses_stale_dsym_without_replacing_previous_exports() -> Result<()> {
    let temp = pyo3_project()?;
    let root = temp.path();
    checked(&mut maturin(root));
    let exported = entries(&root.join("symbols"))?;
    let wheel_exports = &exported[0];
    let marker = wheel_exports.join("previous-release.txt");
    fs::write(&marker, "keep previous matching symbols")?;
    let original_dsym = entries(wheel_exports)?
        .into_iter()
        .find(|p| p.extension().is_some_and(|ext| ext == "dSYM"))
        .context("no first dSYM")?;
    let original_uuid = checked(Command::new("dwarfdump").arg("--uuid").arg(&original_dsym));
    // Cargo can report the dSYM path but leave the old bundle untouched when
    // a cdylib is rebuilt with debug information disabled via a rustc flag.
    let output = maturin(root).args(["--", "-Cdebuginfo=0"]).output()?;
    assert!(!output.status.success(), "export accepted a stale dSYM");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("dSYM"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&marker)?,
        "keep previous matching symbols"
    );
    let after = checked(Command::new("dwarfdump").arg("--uuid").arg(&original_dsym));
    assert_eq!(original_uuid.stdout, after.stdout);
    assert_eq!(entries(&root.join("symbols"))?, exported);
    Ok(())
}

#[test]
fn export_keeps_cargo_cli_rustflags() -> Result<()> {
    let temp = project()?;
    let root = temp.path();
    for name in ["one", "two"] {
        fs::write(
            root.join(format!("src/bin/{name}.rs")),
            "#[cfg(not(maturin_cli_flag))]\ncompile_error!(\"lost Cargo CLI flag\");\nfn main() {}\n",
        )?;
    }
    for _ in 0..2 {
        checked(maturin(root).env("CARGO_PROFILE_SHIP_DEBUG", "0").args([
            "--config",
            "build.rustflags=['--cfg','maturin_cli_flag','-C','debuginfo=2']",
        ]));
    }
    let wheel = entries(&root.join("wheels"))?;
    let files = read_wheel(&wheel[0])?;
    let name = format!(".data/scripts/one{}", std::env::consts::EXE_SUFFIX);
    assert!(files.keys().any(|p| p.ends_with(&name)));
    Ok(())
}

#[test]
fn export_uses_cargo_working_directory_for_external_manifests() -> Result<()> {
    let project = project()?;
    let root = project.path();
    let caller = tempfile::tempdir()?;
    let cwd = caller.path();
    fs::create_dir(root.join(".cargo"))?;
    fs::create_dir(cwd.join(".cargo"))?;
    fs::write(
        root.join(".cargo/config.toml"),
        "[build]\nrustflags=['--cfg','wrong_manifest_config']\n",
    )?;
    fs::write(
        cwd.join(".cargo/config.toml"),
        "[build]\nrustflags=['--cfg','from_caller','-C','debuginfo=2']\n",
    )?;
    fs::write(
        cwd.join(".cargo/release.toml"),
        "[build]\nrustflags=['--cfg','from_release_file']\n",
    )?;
    for name in ["one", "two"] {
        fs::write(
            root.join(format!("src/bin/{name}.rs")),
            r#"#[cfg(not(from_caller))]
compile_error!("lost Cargo's caller config");
#[cfg(wrong_manifest_config)]
compile_error!("loaded config that Cargo ignores for --manifest-path");
fn main() {}
"#,
        )?;
    }

    // Ordinary builds establish Cargo's behavior before opting into exports.
    let manifest = root.join("Cargo.toml");
    checked(
        Command::new(env!("CARGO_BIN_EXE_maturin"))
            .current_dir(cwd)
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("CARGO_BUILD_RUSTFLAGS")
            .env("CARGO_PROFILE_SHIP_DEBUG", "0")
            .args([
                "build",
                "--profile",
                "ship",
                "--auditwheel",
                "skip",
                "--out",
                "wheels",
            ])
            .arg("--manifest-path")
            .arg(&manifest),
    );
    for args in [
        &[][..],
        &["--config", ".cargo/release.toml"][..],
        &["--sdist", "--config", ".cargo/release.toml"][..],
    ] {
        checked(
            maturin(cwd)
                .env("CARGO_PROFILE_SHIP_DEBUG", "0")
                .arg("--manifest-path")
                .arg(&manifest)
                .args(args),
        );
        let wheel = entries(&cwd.join("wheels"))?
            .into_iter()
            .find(|path| path.extension().is_some_and(|ext| ext == "whl"))
            .context("no wheel")?;
        read_wheel(&wheel)?;
        let exports = entries(&cwd.join("symbols"))?;
        assert_eq!(exports.len(), 1);
        assert_eq!(entries(&exports[0])?.len(), 2);
    }
    Ok(())
}

#[test]
fn export_resolves_build_file_and_environment_flags_without_cli_config() -> Result<()> {
    let temp = project()?;
    let root = temp.path();
    fs::create_dir(root.join(".cargo"))?;
    fs::write(
        root.join(".cargo/config.toml"),
        "[build]\nrustflags=['--cfg','from_file']\n",
    )?;
    for name in ["one", "two"] {
        fs::write(
            root.join(format!("src/bin/{name}.rs")),
            r#"#[cfg(not(all(from_file, from_env)))]
compile_error!("lost Cargo build flags from file or environment");
fn main() {}
"#,
        )?;
    }
    checked(
        build_command(root)
            .args(["--auditwheel", "skip", "--out", "wheels"])
            .env("CARGO_BUILD_RUSTFLAGS", "--cfg from_env"),
    );
    checked(maturin(root).env("CARGO_BUILD_RUSTFLAGS", "--cfg from_env"));
    let wheels = entries(&root.join("wheels"))?;
    read_wheel(&wheels[0])?;
    assert_eq!(entries(&entries(&root.join("symbols"))?[0])?.len(), 2);
    Ok(())
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[test]
fn export_resolves_target_feature_cfg_from_selected_target_flags() -> Result<()> {
    let temp = project()?;
    let root = temp.path();
    let rustc = checked(Command::new("rustc").arg("-vV"));
    let target = std::str::from_utf8(&rustc.stdout)?
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .context("no host triple")?;
    let feature = if cfg!(target_arch = "aarch64") {
        "sve"
    } else {
        "avx"
    };
    fs::create_dir(root.join(".cargo"))?;
    fs::write(
        root.join(".cargo/config.toml"),
        format!(
            "[target.{target}]\nrustflags=['-C','target-feature=+{feature}']\n\
             [target.'cfg(target_feature = \"{feature}\")']\nrustflags=['--cfg','from_feature_cfg']\n"
        ),
    )?;
    for name in ["one", "two"] {
        fs::write(
            root.join(format!("src/bin/{name}.rs")),
            r#"#[cfg(not(from_feature_cfg))]
compile_error!("lost Cargo flags selected by target feature");
fn main() {}
"#,
        )?;
    }
    checked(build_command(root).args(["--auditwheel", "skip", "--out", "wheels"]));
    // Unrelated CLI overrides must not change which cfg tables Cargo selects.
    for config in [&[][..], &["--config", "net.offline=true"][..]] {
        checked(maturin(root).args(config));
        let wheels = entries(&root.join("wheels"))?;
        read_wheel(&wheels[0])?;
        assert_eq!(entries(&entries(&root.join("symbols"))?[0])?.len(), 2);
    }
    Ok(())
}

#[test]
fn export_resolves_cli_files_target_cfg_and_environment_flags() -> Result<()> {
    let temp = project()?;
    let root = temp.path();
    let rustc = checked(Command::new("rustc").arg("-vV"));
    let version = std::str::from_utf8(&rustc.stdout)?;
    let target = version
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .context("no host triple")?;
    let target_env = format!(
        "CARGO_TARGET_{}_RUSTFLAGS",
        target.to_uppercase().replace(['-', '.'], "_")
    );
    fs::create_dir(root.join(".cargo"))?;
    fs::write(
        root.join(".cargo/config.toml"),
        format!("[target.{target}]\nrustflags=['--cfg','from_project']\n"),
    )?;
    // --config file is resolved against the command's cwd, not .cargo.
    fs::write(
        root.join("release-cargo.toml"),
        format!(
            "[build]\nrustflags=['--cfg','wrong_build_fallback']\n\
             [target.{target}]\nrustflags=['--cfg','from_cli_file']\n\
             [target.'cfg(any(unix, windows))']\nrustflags=['--cfg','matching_cfg']\n\
             [target.'cfg(target_os = \"none\")']\nrustflags=['--cfg','wrong_target']\n"
        ),
    )?;
    for name in ["one", "two"] {
        fs::write(
            root.join(format!("src/bin/{name}.rs")),
            r#"#[cfg(not(all(from_project, from_env, from_cli_file, from_cli, matching_cfg)))]
compile_error!("lost file, environment or CLI target flags");
#[cfg(any(wrong_build_fallback, wrong_target))]
compile_error!("wrong Cargo target flags");
fn main() {}
"#,
        )?;
    }
    checked(
        maturin(root)
            .env("CARGO_PROFILE_SHIP_DEBUG", "0")
            .env(&target_env, "--cfg from_env")
            .args(["--config", "release-cargo.toml", "--config"])
            .arg(format!(
                "target.{target}.rustflags=['--cfg','from_cli','-C','debuginfo=2']"
            )),
    );
    let wheel = entries(&root.join("wheels"))?;
    read_wheel(&wheel[0])?;
    assert_eq!(entries(&entries(&root.join("symbols"))?[0])?.len(), 2);
    Ok(())
}

#[test]
fn export_keeps_cargo_rustflags_precedence_over_cli_config() -> Result<()> {
    let temp = project()?;
    let root = temp.path();
    for name in ["one", "two"] {
        fs::write(
            root.join(format!("src/bin/{name}.rs")),
            r#"#[cfg(not(from_env))]
compile_error!("lost higher priority Rust flags");
#[cfg(from_cli)]
compile_error!("CLI flag took precedence over Rust flags");
fn main() {}
"#,
        )?;
    }
    let opt = "build.rustflags=['--cfg','from_cli','-C','debuginfo=0']";
    for (env, flags) in [
        ("RUSTFLAGS", "--cfg from_env -Cdebuginfo=2"),
        (
            "CARGO_ENCODED_RUSTFLAGS",
            "--cfg\x1ffrom_env\x1f-Cdebuginfo=2",
        ),
    ] {
        checked(
            maturin(root)
                .env("CARGO_PROFILE_SHIP_DEBUG", "0")
                .env(env, flags)
                .args(["--config", opt]),
        );
    }
    let wheel = entries(&root.join("wheels"))?;
    read_wheel(&wheel[0])?;
    assert_eq!(entries(&entries(&root.join("symbols"))?[0])?.len(), 2);
    Ok(())
}

#[test]
fn export_replaces_string_cli_flags_and_retains_build_environment() -> Result<()> {
    let temp = project()?;
    let root = temp.path();
    fs::create_dir(root.join(".cargo"))?;
    fs::write(
        root.join(".cargo/config.toml"),
        "[build]\nrustflags='--cfg from_project'\n",
    )?;
    for name in ["one", "two"] {
        fs::write(
            root.join(format!("src/bin/{name}.rs")),
            r#"#[cfg(not(all(from_env, from_cli)))]
compile_error!("lost Cargo CLI or environment Rust flags");
#[cfg(any(from_project, from_first_cli))]
compile_error!("scalar flag should have been replaced");
fn main() {}
"#,
        )?;
    }
    checked(
        maturin(root)
            .env("CARGO_PROFILE_SHIP_DEBUG", "0")
            .env("CARGO_BUILD_RUSTFLAGS", "--cfg from_env")
            .args([
                "--config",
                "build.rustflags='--cfg from_first_cli'",
                "--config",
                "build.rustflags='--cfg from_cli -Cdebuginfo=2'",
            ]),
    );
    let wheel = entries(&root.join("wheels"))?;
    read_wheel(&wheel[0])?;
    Ok(())
}

#[cfg(target_os = "macos")]
#[test]
fn cli_rustflags_do_not_reapply_config_when_tempdir_is_in_project() -> Result<()> {
    let temp = project()?;
    let root = temp.path();
    fs::create_dir(root.join(".cargo"))?;
    fs::create_dir(root.join("scratch"))?;
    fs::write(
        root.join(".cargo/config.toml"),
        "[build]\nrustflags = ['-C','link-arg=-Wl,-rpath,@executable_path/probe', \
         '-C','link-arg=-Wl,-fatal_warnings']\n",
    )?;
    let cli = "build.rustflags=['-C','debuginfo=2']";
    // An existing wheel feature also adds rustflags. No symbol export.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_maturin"));
    checked(
        cmd.current_dir(root)
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env("TMPDIR", root.join("scratch"))
            .args([
                "build",
                "--profile",
                "ship",
                "--auditwheel",
                "skip",
                "--strip=false",
                "--out",
                "wheels",
                "--include-debuginfo",
                "--config",
                cli,
            ]),
    );
    assert_eq!(entries(&root.join("wheels"))?.len(), 1);
    // And test export itself with the same config hierarchy.
    checked(
        maturin(root)
            .env("TMPDIR", root.join("scratch"))
            .args(["--config", cli]),
    );
    let wheels = entries(&root.join("wheels"))?;
    read_wheel(&wheels[0])?;
    Ok(())
}
