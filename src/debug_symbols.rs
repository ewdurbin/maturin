//! Export symbols from the exact artifacts about to be packaged. Never rewrite
//! Cargo's cached outputs and never publish companions for a failed wheel.

use crate::auditwheel::AuditedArtifact;
use crate::binding_generator::copy_dir_all;
use crate::build_context::copy_back_cargo_outputs;
use crate::pgo::PgoPhase;
use crate::target::Arch;
use crate::{BuildArtifact, BuildContext, CargoOptions, Target, ui};
use anyhow::{Context, Result, bail};
use fs_err as fs;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

mod cargo_config;
pub(crate) use cargo_config::rustflags;

pub(crate) struct DebugSymbols {
    staging: TempDir,
    output: PathBuf,
}

impl DebugSymbols {
    /// Repair and binding generation have finished. Preserve Cargo outputs
    /// before touching the staged files, even if auditwheel had nothing to repair.
    pub(crate) fn prepare(
        context: &BuildContext,
        audited: &mut [AuditedArtifact],
    ) -> Result<Option<Self>> {
        let Some(output) = &context.artifact.debug_symbols_dir else {
            return Ok(None);
        };
        if matches!(context.artifact.pgo_phase, Some(PgoPhase::Generate(_))) {
            return Ok(None);
        }

        // Restore *all* originals before any extraction can fail, even on
        // MSVC where we only copy the PDB and never strip the PE debug directory.
        copy_back_cargo_outputs(audited, &vec![true; audited.len()])?;
        fs::create_dir_all(output)?;
        let staging = tempfile::Builder::new()
            .prefix(".maturin-symbols-")
            .tempdir_in(output)
            .context("Failed to stage debug symbols")?;
        let symbols = Self {
            staging,
            output: output.clone(),
        };
        for aa in audited {
            symbols
                .extract(
                    &context.project.target,
                    &context.project.cargo_options,
                    &aa.artifact,
                )
                .with_context(|| {
                    format!(
                        "Failed to export symbols for {}",
                        aa.artifact.path.display()
                    )
                })?;
        }
        Ok(Some(symbols))
    }

    fn extract(
        &self,
        target: &Target,
        cargo: &CargoOptions,
        artifact: &BuildArtifact,
    ) -> Result<()> {
        if target.is_linux() {
            self.extract_elf(target, cargo, artifact)?;
        } else if target.is_macos() {
            if !cfg!(any(feature = "auditwheel", target_os = "macos")) {
                bail!("Cross-compiling macOS symbol exports requires maturin's auditwheel feature");
            }
            let source = companion(artifact, "dSYM")?;

            // Retain global symbols required for dyld/Python imports; discard
            // debugging and local symbols. Stripping invalidates a Mach-O
            // signature, including the ad-hoc signature made by wheel repair.
            let strip = if cfg!(target_os = "macos") {
                PathBuf::from("/usr/bin/strip")
            } else {
                llvm_tool(target, "llvm-strip")?
            };
            run(Command::new(strip).args(["-S", "-x"]).arg(&artifact.path))?;
            #[cfg(any(feature = "auditwheel", target_os = "macos"))]
            crate::auditwheel::macos_sign::ad_hoc_sign(&artifact.path)?;

            // A reported path can still contain a bundle from an older build
            // (notably when raw rustc flags disable debug info). Verify against
            // the final repaired, stripped, signed file before publishing.
            let dwarf_dir = source.join("Contents/Resources/DWARF");
            let mut dwarf_files = fs::read_dir(&dwarf_dir).with_context(|| {
                format!("Failed to read dSYM DWARF files in {}", source.display())
            })?;
            let dwarf = dwarf_files
                .next()
                .transpose()?
                .context("dSYM has no DWARF file")?;
            if dwarf_files.next().transpose()?.is_some() {
                bail!(
                    "Expected one DWARF file in single-architecture dSYM {}",
                    source.display()
                );
            }
            if macho_uuid(&artifact.path)? != macho_uuid(&dwarf.path())? {
                bail!(
                    "dSYM UUID does not match {}. Cargo may have left a stale dSYM; \
                     enable debug information (e.g. CARGO_PROFILE_RELEASE_DEBUG=2) \
                     and remove conflicting rustc flags.",
                    artifact.path.display()
                );
            }
            copy_dir_all(source, &self.destination(source)?)?;
        } else {
            // MSVC already places DWARF-equivalent debug information in a PDB.
            // Stripping PE would remove its CodeView GUID/age, breaking lookup.
            let source = companion(artifact, "pdb")?;
            fs::copy(source, self.destination(source)?)?;
        }
        Ok(())
    }

    fn extract_elf(
        &self,
        target: &Target,
        cargo: &CargoOptions,
        artifact: &BuildArtifact,
    ) -> Result<()> {
        if artifact.debuginfo_path.is_some() {
            bail!(
                "ELF symbol export requires embedded DWARF; remove explicit split-debuginfo rustc arguments"
            );
        }
        // Do not silently produce an empty or unusable companion for a release
        // profile that omitted debug info, or a previously stripped executable.
        let bytes = fs::read(&artifact.path)?;
        let elf = goblin::elf::Elf::parse(&bytes)?;
        if !elf.section_headers.iter().any(|section| {
            matches!(
                elf.shdr_strtab.get_at(section.sh_name),
                Some(".debug_info" | ".zdebug_info")
            ) && section.sh_size > 0
        }) {
            bail!(
                "No DWARF debug information in {}. Set debug = true in the selected Cargo profile \
                 (for example CARGO_PROFILE_RELEASE_DEBUG=2) and enable native debug info if needed.",
                artifact.path.display()
            );
        }
        drop(bytes);
        let name = artifact
            .path
            .file_name()
            .context("Artifact has no file name")?;
        let mut debug_name = OsString::from(name);
        debug_name.push(".debug");
        let debug_path = self.destination(Path::new(&debug_name))?;
        let objcopy = elf_objcopy(target, cargo)?;
        run(Command::new(&objcopy)
            .arg("--only-keep-debug")
            .arg(&artifact.path)
            .arg(&debug_path))?;
        // A renamed staging file can still share its inode with Cargo's
        // target/.../deps artifact. GNU objcopy in-place preserves hard links
        // and would strip Cargo's cache too. Write a separate inode, then
        // replace only the wheel's staged path.
        let stripped = tempfile::NamedTempFile::new_in(
            artifact
                .path
                .parent()
                .context("Artifact path has no parent")?,
        )?;
        run(Command::new(&objcopy)
            .arg("--strip-all")
            // LLVM's --strip-all also removes non-allocated cargo-auditable metadata.
            .arg("--keep-section=.dep-v0")
            .arg(&artifact.path)
            .arg(stripped.path()))?;
        let mut debuglink = OsString::from("--add-gnu-debuglink=");
        debuglink.push(&debug_path);
        run(Command::new(&objcopy).arg(debuglink).arg(stripped.path()))?;
        stripped
            .persist(&artifact.path)
            .context("Failed to replace staged ELF with stripped output")?;
        Ok(())
    }

    fn destination(&self, source: &Path) -> Result<PathBuf> {
        let filename = source
            .file_name()
            .context("Debug companion has no file name")?;
        let dest = self.staging.path().join(filename);
        if dest.exists() {
            bail!(
                "Multiple build artifacts produced the same debug companion: {}",
                filename.to_string_lossy()
            );
        }
        Ok(dest)
    }

    /// Scope symbols by the complete wheel filename: platform, ABI and Python
    /// tags, as well as version. Replace that wheel's old exports on success so
    /// removed binaries don't leave stale symbols behind.
    pub(crate) fn publish(self, wheel: &Path) -> Result<()> {
        let mut name = OsString::from(wheel.file_name().context("Wheel has no file name")?);
        name.push(".symbols");
        let dest = self.output.join(name);
        if dest.exists() {
            fs::remove_dir_all(&dest)?;
        }
        fs::rename(self.staging.path(), &dest).context("Failed to publish wheel debug symbols")?;
        ui::status!("🔎 Exported debug symbols to {}", dest.display());
        Ok(())
    }
}

/// Symbol export currently supports single-architecture Mach-O files only.
fn macho_uuid(path: &Path) -> Result<[u8; 16]> {
    let bytes = fs::read(path)?;
    let macho = goblin::mach::MachO::parse(&bytes, 0)
        .with_context(|| format!("Failed to parse Mach-O in {}", path.display()))?;
    macho
        .load_commands
        .iter()
        .find_map(|load| match load.command {
            goblin::mach::load_command::CommandVariant::Uuid(uuid) => Some(uuid.uuid),
            _ => None,
        })
        .with_context(|| format!("No Mach-O UUID found in {}", path.display()))
}

fn companion<'a>(artifact: &'a BuildArtifact, extension: &str) -> Result<&'a Path> {
    let path = artifact
        .debuginfo_path
        .as_deref()
        .filter(|path| path.extension() == Some(OsStr::new(extension)) && path.exists())
        .with_context(|| {
            format!(
                "Cargo did not produce a .{extension} companion for {}. Set debug = true in the \
                 selected Cargo profile (e.g. CARGO_PROFILE_RELEASE_DEBUG=2); on macOS use \
                 split-debuginfo=packed. Explicit stripping linker flags must also be removed.",
                artifact.path.display()
            )
        })?;
    Ok(path)
}

fn run(command: &mut Command) -> Result<()> {
    let output = command
        .output()
        .with_context(|| format!("Failed to run {command:?}"))?;
    if !output.status.success() {
        bail!(
            "{command:?} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

/// LLVM tools support all Rust targets, unlike a native GNU objcopy. Prefer
/// the active Rust toolchain, then PATH; never use a host-only tool for cross
/// compilation.
fn llvm_tool(target: &Target, name: &str) -> Result<PathBuf> {
    if let Ok(output) = Command::new("rustc").args(["--print", "sysroot"]).output()
        && output.status.success()
    {
        let tool = PathBuf::from(String::from_utf8(output.stdout)?.trim())
            .join("lib/rustlib")
            .join(target.host_triple())
            .join("bin")
            .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        if tool.is_file() {
            return Ok(tool);
        }
    }
    which::which(name).with_context(|| {
        format!(
            "Could not find {name}. Install Rust's llvm-tools component or add LLVM tools to PATH"
        )
    })
}

fn elf_objcopy(target: &Target, cargo: &CargoOptions) -> Result<PathBuf> {
    if let Ok(path) = llvm_tool(target, "llvm-objcopy") {
        return Ok(path);
    }
    let cwd = std::env::current_dir()?;
    let linker = cargo_config::linker(&cwd, cargo, target.target_triple())?;
    gnu_objcopy(
        target,
        linker.as_deref(),
        std::env::var_os("PATH").as_deref(),
        &cwd,
    )
}

fn gnu_objcopy(
    target: &Target,
    linker: Option<&Path>,
    paths: Option<&OsStr>,
    cwd: &Path,
) -> Result<PathBuf> {
    let find = |name: &OsStr| which::which_in(name, paths, cwd).ok();
    // A configured, target-prefixed GNU linker may live outside PATH or in
    // a different toolchain from the first objcopy on PATH. Look beside the
    // linker before trying conventional names. Do not infer a native objcopy
    // from an unprefixed cc/gcc or an arbitrary wrapper during cross builds.
    if let Some(linker) = linker.and_then(|linker| find(linker.as_os_str()))
        && let Some(stem) = linker.file_stem().and_then(OsStr::to_str)
        && let Some(prefix) = ["-gcc", "-cc", "-ld"]
            .iter()
            .find_map(|suffix| stem.strip_suffix(suffix))
        && prefix.contains("-linux-")
        && let Some(objcopy) = find(
            linker
                .with_file_name(format!("{prefix}-objcopy"))
                .as_os_str(),
        )
    {
        return Ok(objcopy);
    }
    if let Some(path) = find(OsStr::new(&format!("{}-objcopy", target.target_triple()))) {
        return Ok(path);
    }
    // GNU Linux toolchains omit Rust's vendor; ARM and RISC-V also omit the
    // ISA revision/extensions. Preserve endianness and ABI (e.g. eabi vs eabihf).
    let arch = match target.target_arch() {
        Arch::Armv5teL | Arch::Armv6L | Arch::Armv7L => "arm",
        Arch::Riscv32 => "riscv32",
        Arch::Riscv64 => "riscv64",
        _ => target.target_triple().split('-').next().unwrap(),
    };
    let gnu_name = format!("{arch}-linux-{}-objcopy", target.target_env());
    if let Some(path) = find(OsStr::new(&gnu_name)) {
        return Ok(path);
    }
    if !target.cross_compiling()
        && let Some(path) = find(OsStr::new("objcopy"))
    {
        return Ok(path);
    }
    bail!(
        "Could not find an objcopy for {}. Install Rust's llvm-tools component, \
         or put llvm-objcopy or a target-prefixed GNU objcopy (e.g. {gnu_name}) on PATH",
        target.target_triple()
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[cfg(target_os = "macos")]
    use crate::compile::CargoOutputState;

    fn executable(dir: &Path, name: &str) -> Result<PathBuf> {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        fs::create_dir_all(dir)?;
        fs::write(&path, "#!/bin/sh\nexit 0\n")?;
        fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        Ok(path)
    }

    #[test]
    fn gnu_tools_use_linux_names_and_keep_the_target_abi() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let bin = temp.path().join("bin");
        // Multiple installed toolchains: neither host nor a different ABI may
        // be chosen just because it comes first on PATH.
        executable(temp.path(), "objcopy")?;
        executable(temp.path(), "arm-linux-gnueabi-objcopy")?;
        executable(temp.path(), "mips-linux-gnu-objcopy")?;
        let paths = std::env::join_paths([temp.path(), &bin])?;
        for (triple, name) in [
            ("aarch64-unknown-linux-gnu", "aarch64-linux-gnu-objcopy"),
            (
                "armv7-unknown-linux-gnueabihf",
                "arm-linux-gnueabihf-objcopy",
            ),
            (
                "armv7-unknown-linux-musleabihf",
                "arm-linux-musleabihf-objcopy",
            ),
            ("riscv64gc-unknown-linux-gnu", "riscv64-linux-gnu-objcopy"),
            ("mipsel-unknown-linux-gnu", "mipsel-linux-gnu-objcopy"),
        ] {
            let expected = executable(&bin, name)?;
            let target = Target::from_resolved_target_triple(triple)?;
            assert_eq!(
                gnu_objcopy(&target, None, Some(&paths), temp.path())?,
                expected,
                "{triple}"
            );
        }
        Ok(())
    }

    #[test]
    fn gnu_tools_are_taken_from_the_configured_linkers_directory() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let bin = temp.path().join("cross sdk/bin");
        let on_path = temp.path().join("path");
        let config_dir = temp.path().join(".cargo");
        fs::create_dir_all(&config_dir)?;
        fs::write(
            config_dir.join("config.toml"),
            r#"[target.aarch64-unknown-linux-gnu]
linker = "cross sdk/bin/aarch64-vendor-linux-gnu-gcc"
"#,
        )?;
        executable(&bin, "aarch64-vendor-linux-gnu-gcc")?;
        let expected = executable(&bin, "aarch64-vendor-linux-gnu-objcopy")?;
        // Neither the standard name nor a same-named objcopy elsewhere on
        // PATH should override the one paired with the actual linker.
        executable(&on_path, "aarch64-vendor-linux-gnu-objcopy")?;
        executable(&on_path, "aarch64-linux-gnu-objcopy")?;
        let target = Target::from_resolved_target_triple("aarch64-unknown-linux-gnu")?;
        let config = cargo_config2::Config::load_with_cwd(temp.path())?;
        let linker = config.linker(target.target_triple())?;
        assert!(linker.is_some());
        assert_eq!(
            gnu_objcopy(
                &target,
                linker.as_deref(),
                Some(on_path.as_os_str()),
                temp.path()
            )?,
            expected
        );
        // A linker configured by filename alone is found on PATH first;
        // still find its sibling rather than an earlier, same-named tool.
        let paths = std::env::join_paths([&on_path, &bin])?;
        assert_eq!(
            gnu_objcopy(
                &target,
                Some(Path::new("aarch64-vendor-linux-gnu-gcc")),
                Some(&paths),
                temp.path()
            )?,
            expected
        );
        Ok(())
    }

    #[test]
    fn cli_linker_overrides_select_the_matching_gnu_toolchain() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        let triple = "aarch64-unknown-linux-gnu";
        let target = Target::from_resolved_target_triple(triple)?;
        let on_path = root.join("path");
        let from_config = root.join("file-bin");
        let from_cli_file = root.join("release-bin");
        let from_cli = root.join("cli-bin");
        let from_cfg = root.join("cfg-bin");
        fs::create_dir(root.join(".cargo"))?;
        let mut tools = Vec::new();
        for dir in [&on_path, &from_config, &from_cli_file, &from_cli, &from_cfg] {
            executable(dir, "aarch64-linux-gnu-gcc")?;
            tools.push(executable(dir, "aarch64-linux-gnu-objcopy")?);
        }
        fs::write(
            root.join(".cargo/config.toml"),
            format!("[target.{triple}]\nlinker = 'file-bin/aarch64-linux-gnu-gcc'\n"),
        )?;
        let release_config = root.join(".cargo/release.toml");
        fs::write(
            &release_config,
            format!("[target.{triple}]\nlinker = 'release-bin/aarch64-linux-gnu-gcc'\n"),
        )?;
        let mut options = CargoOptions::default();
        let find = |options: &CargoOptions| -> Result<PathBuf> {
            let linker = cargo_config::linker(root, options, triple)?;
            gnu_objcopy(&target, linker.as_deref(), Some(on_path.as_os_str()), root)
        };
        // All four installed versions have the same GNU name. Selected
        // linkers outside PATH must take precedence over the PATH version.
        assert_eq!(find(&options)?, tools[1]);
        options.config.push(".cargo/release.toml".to_owned());
        assert_eq!(find(&options)?, tools[2]);
        options.config.push(format!(
            "target.{triple}.linker='cli-bin/aarch64-linux-gnu-gcc'"
        ));
        assert_eq!(find(&options)?, tools[3]);
        // A triple setting wins even when a later --config matches by cfg.
        options.config.push(format!(
            "target.'cfg(target_os = \"linux\")'.linker='{}'",
            from_cfg.join("aarch64-linux-gnu-gcc").display()
        ));
        assert_eq!(find(&options)?, tools[3]);
        // With no triple setting, an applicable CLI cfg linker is used.
        fs::remove_file(root.join(".cargo/config.toml"))?;
        options.config.drain(..2);
        assert_eq!(find(&options)?, tools[4]);
        // A non-matching cfg is ignored; PATH still provides a conventional
        // triple-prefixed tool for this cross build.
        options.config[0] = format!(
            "target.'cfg(target_os = \"macos\")'.linker='{}'",
            from_cfg.join("aarch64-linux-gnu-gcc").display()
        );
        assert_eq!(find(&options)?, tools[0]);
        Ok(())
    }

    #[test]
    fn cross_export_rejects_host_and_unrelated_tools() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let target = Target::from_resolved_target_triple("armv7-unknown-linux-gnueabihf")?;
        if !target.cross_compiling() {
            return Ok(());
        }
        executable(temp.path(), "objcopy")?;
        executable(temp.path(), "arm-linux-gnueabi-objcopy")?;
        executable(temp.path(), "cc")?;
        executable(temp.path(), "custom-cc")?;
        executable(temp.path(), "custom-objcopy")?;
        for linker in [None, Some(Path::new("cc")), Some(Path::new("custom-cc"))] {
            let error = gnu_objcopy(&target, linker, Some(temp.path().as_os_str()), temp.path())
                .expect_err("Host or soft-float objcopy is not valid for the cross target");
            assert!(
                error.to_string().contains("arm-linux-gnueabihf-objcopy"),
                "{error}"
            );
        }
        // Continue to support toolchains which use the Rust triple verbatim.
        let expected = executable(temp.path(), "armv7-unknown-linux-gnueabihf-objcopy")?;
        assert_eq!(
            gnu_objcopy(&target, None, Some(temp.path().as_os_str()), temp.path())?,
            expected
        );
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_export_can_still_use_unprefixed_objcopy() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let target = Target::from_target_triple(None)?;
        assert!(!target.cross_compiling());
        let expected = executable(temp.path(), "objcopy")?;
        assert_eq!(
            gnu_objcopy(&target, None, Some(temp.path().as_os_str()), temp.path())?,
            expected
        );
        Ok(())
    }

    // Exercise ELF export on macOS too, without a Linux sysroot or libc.
    // Rust's llvm-tools component is optional for maturin's normal test suite.
    #[cfg(target_os = "macos")]
    #[test]
    fn export_cross_compiled_elf_debug_symbols() -> Result<()> {
        for triple in ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-musl"] {
            let target = Target::from_resolved_target_triple(triple)?;
            if llvm_tool(&target, "llvm-objcopy").is_err() {
                eprintln!("Skipping {triple}: llvm-tools not installed");
                continue;
            }
            let temp = tempfile::tempdir()?;
            let source = temp.path().join("probe.c");
            let object = temp.path().join("probe.o");
            let executable = temp.path().join("probe");
            fs::write(&source, "int _start(void) { return 42; }\n")?;
            run(Command::new("clang")
                .args(["-target", triple, "-g", "-fno-stack-protector", "-c"])
                .arg(&source)
                .arg("-o")
                .arg(&object))?;
            run(Command::new(llvm_tool(&target, "rust-lld")?)
                .args(["-flavor", "gnu", "--build-id=sha1", "-e", "_start"])
                .arg("-o")
                .arg(&executable)
                .arg(&object))?;
            let sbom = temp.path().join("sbom");
            fs::write(&sbom, b"cargo-auditable metadata")?;
            run(Command::new(llvm_tool(&target, "llvm-objcopy")?)
                .arg(format!("--add-section=.dep-v0={}", sbom.display()))
                .arg(&executable))?;
            let original = fs::read(&executable)?;
            let symbols = DebugSymbols {
                staging: tempfile::tempdir_in(temp.path())?,
                output: temp.path().to_path_buf(),
            };
            symbols.extract(
                &target,
                &CargoOptions::default(),
                &BuildArtifact {
                    path: executable.clone(),
                    import_lib_path: None,
                    debuginfo_path: None,
                    linked_paths: Vec::new(),
                    thin_artifacts: Vec::new(),
                    staging: CargoOutputState::Patched,
                },
            )?;
            let stripped = fs::read(&executable)?;
            let companion = fs::read(symbols.staging.path().join("probe.debug"))?;
            let before = goblin::elf::Elf::parse(&original)?;
            let after = goblin::elf::Elf::parse(&stripped)?;
            let debug = goblin::elf::Elf::parse(&companion)?;
            let section = |elf: &goblin::elf::Elf<'_>, bytes: &[u8], name: &str| {
                elf.section_headers.iter().find_map(|header| {
                    if elf.shdr_strtab.get_at(header.sh_name) == Some(name)
                        && header.sh_type != goblin::elf::section_header::SHT_NOBITS
                    {
                        let start = header.sh_offset as usize;
                        Some(bytes[start..start + header.sh_size as usize].to_vec())
                    } else {
                        None
                    }
                })
            };
            assert!(section(&after, &stripped, ".debug_info").is_none());
            assert_eq!(
                section(&after, &stripped, ".dep-v0").context("Missing embedded SBOM")?,
                fs::read(&sbom)?
            );
            let dwarf = section(&debug, &companion, ".debug_info").context("No exported DWARF")?;
            assert_eq!(Some(dwarf), section(&before, &original, ".debug_info"));
            assert_eq!(after.entry, before.entry);
            let id = section(&after, &stripped, ".note.gnu.build-id").context("No ELF build ID")?;
            assert_eq!(Some(id), section(&debug, &companion, ".note.gnu.build-id"));
            assert!(
                section(&after, &stripped, ".gnu_debuglink")
                    .context("No debuglink")?
                    .starts_with(b"probe.debug\0")
            );
        }
        Ok(())
    }
}
