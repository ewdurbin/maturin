//! Resolve Cargo rustflags and linker settings for symbol export.
//!
//! cargo-config2 loads files and environment variables, but its API does not
//! apply Cargo's CLI `--config` overrides. Symbol export must preserve those
//! flags before setting `CARGO_ENCODED_RUSTFLAGS`, which would otherwise hide
//! them from Cargo; GNU tool selection also needs the overridden linker.
//! If cargo-config2 supports CLI config precedence and paths, use its resolver
//! directly and remove this export-specific merge logic. Related upstream request:
//! <https://github.com/taiki-e/cargo-config2/issues/3>.

use crate::CargoOptions;
use anyhow::{Context, Result, bail};
use cargo_metadata::cargo_platform::{Cfg, Platform};
use fs_err as fs;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Cargo's normal flags, including CLI overrides, before symbol export adds
/// split-debuginfo. Encoded flags passed to Cargo would hide those overrides.
pub(crate) fn rustflags(
    cwd: &Path,
    cargo: &CargoOptions,
    target: &str,
) -> Result<cargo_config2::Flags> {
    let config = cargo_config2::Config::load_with_cwd(cwd)?;
    if cargo.config.is_empty()
        || std::env::var_os("CARGO_ENCODED_RUSTFLAGS").is_some()
        || std::env::var_os("RUSTFLAGS").is_some()
    {
        return Ok(config.rustflags(target)?.unwrap_or_default());
    }
    let mut merged = export_config(cwd, cargo, target)?;
    let mut selected = merged.target.remove(target).and_then(|c| c.rustflags);
    for matching in matching_cfg_targets(&merged.target, &config, target)? {
        if let Some(flags) = &matching.rustflags {
            if let Some(selected) = &mut selected {
                selected.flags.extend_from_slice(&flags.flags);
            } else {
                selected = Some(flags.clone());
            }
        }
    }
    let mut flags = cargo_config2::Flags::default();
    if let Some(selected) = selected.or(merged.build.rustflags) {
        flags
            .flags
            .extend(selected.flags.into_iter().map(|v| v.val));
    }
    Ok(flags)
}

/// Read only the CLI fields symbol extraction needs. Do not serialize Cargo
/// config or search from a temporary directory: that can reload parent configs.
fn export_config(
    cwd: &Path,
    cargo: &CargoOptions,
    target: &str,
) -> Result<cargo_config2::de::Config> {
    use cargo_config2::de::{Definition, Value};

    let mut merged = cargo_config2::de::Config::load_with_cwd(cwd)?;
    let prefix = format!(
        "CARGO_TARGET_{}",
        target.to_uppercase().replace(['-', '.'], "_")
    );
    let build_env = std::env::var("CARGO_BUILD_RUSTFLAGS").ok();
    let target_env = std::env::var(format!("{prefix}_RUSTFLAGS")).ok();
    // Cargo places these between the file flags and CLI flags.
    append_flags(&mut merged.build.rustflags, build_env.as_deref())?;
    let entry = merged.target.entry(target.to_owned()).or_default();
    append_flags(&mut entry.rustflags, target_env.as_deref())?;
    let linker_env = format!("{prefix}_LINKER");
    if let Ok(val) = std::env::var(&linker_env) {
        entry.linker = Some(Value {
            val,
            definition: Some(Definition::Environment(linker_env.into())),
        });
    }
    for opt in &cargo.config {
        let path = cwd.join(opt);
        let (content, path) = if path.is_file() {
            (fs::read_to_string(&path)?, Some(path))
        } else {
            (opt.to_owned(), None)
        };
        let cli: toml::Table = toml::from_str(&content).context("Invalid Cargo --config")?;
        if let Some(flags) = cli.get("build").and_then(|build| build.get("rustflags")) {
            merge_cli_flags(&mut merged.build.rustflags, flags, build_env.as_deref())?;
        }
        if let Some(targets) = cli.get("target").and_then(toml::Value::as_table) {
            for (key, value) in targets {
                let entry = merged.target.entry(key.clone()).or_default();
                if let Some(flags) = value.get("rustflags") {
                    let env = (key == target).then_some(target_env.as_deref()).flatten();
                    merge_cli_flags(&mut entry.rustflags, flags, env)?;
                }
                if let Some(linker) = value.get("linker") {
                    let mut linker: Value<String> = linker
                        .clone()
                        .try_into()
                        .context("Invalid linker in Cargo --config")?;
                    linker.definition = Some(Definition::Cli(path.clone()));
                    entry.linker = Some(linker);
                }
            }
        }
    }
    Ok(merged)
}

fn append_flags(dest: &mut Option<cargo_config2::de::Flags>, value: Option<&str>) -> Result<()> {
    if let Some(value) = value {
        let mut flags: cargo_config2::de::Flags =
            toml::Value::String(value.to_owned()).try_into()?;
        if let Some(dest) = dest {
            dest.flags.append(&mut flags.flags);
        } else {
            *dest = Some(flags);
        }
    }
    Ok(())
}

/// CLI arrays concatenate; strings replace earlier file/CLI strings, with
/// any Cargo environment flags still preceding the new CLI flags.
fn merge_cli_flags(
    dest: &mut Option<cargo_config2::de::Flags>,
    value: &toml::Value,
    env: Option<&str>,
) -> Result<()> {
    if value.is_str() {
        *dest = None;
        append_flags(dest, env)?;
    }
    let mut flags: cargo_config2::de::Flags = value
        .clone()
        .try_into()
        .context("Invalid rustflags in Cargo --config")?;
    if let Some(dest) = dest {
        dest.flags.append(&mut flags.flags);
    } else {
        *dest = Some(flags);
    }
    Ok(())
}

/// Cargo excludes test, debug_assertions, proc_macro, and features from
/// target cfg selection. Only ask rustc if there are cfg tables to evaluate.
fn matching_cfg_targets<'a>(
    targets: &'a BTreeMap<String, cargo_config2::de::TargetConfig>,
    config: &cargo_config2::Config,
    target: &str,
) -> Result<Vec<&'a cargo_config2::de::TargetConfig>> {
    let candidates: Vec<_> = targets
        .iter()
        .filter(|(key, v)| key.starts_with("cfg(") && (v.rustflags.is_some() || v.linker.is_some()))
        .collect();
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let output = Command::from(config.rustc())
        .args(["--print", "cfg", "--target", target])
        .output()
        .context("Failed to query rustc target cfg")?;
    if !output.status.success() {
        bail!(
            "rustc --print cfg failed for {target}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let cfgs = std::str::from_utf8(&output.stdout)?
        .lines()
        .map(str::parse::<Cfg>)
        .collect::<Result<Vec<_>, _>>()?;
    let cfgs: Vec<_> = cfgs
        .into_iter()
        .filter(|cfg| match cfg {
            Cfg::Name(name) => !matches!(name.as_str(), "test" | "debug_assertions" | "proc_macro"),
            Cfg::KeyPair(name, _) => name.as_str() != "feature",
        })
        .collect();
    candidates
        .into_iter()
        .filter_map(|(key, value)| match key.parse::<Platform>() {
            Ok(platform) if platform.matches(target, &cfgs) => Some(Ok(value)),
            Ok(_) => None,
            Err(err) => Some(Err(err.into())),
        })
        .collect()
}

pub(super) fn linker(cwd: &Path, cargo: &CargoOptions, target: &str) -> Result<Option<PathBuf>> {
    let config = cargo_config2::Config::load_with_cwd(cwd)?;
    if cargo.config.is_empty() {
        return Ok(config.linker(target)?);
    }
    let mut merged = export_config(cwd, cargo, target)?;
    // A triple (from file, env, or CLI) always precedes a matching cfg.
    let mut selected = merged.target.remove(target).and_then(|c| c.linker);
    if selected.is_none() {
        selected = matching_cfg_targets(&merged.target, &config, target)?
            .into_iter()
            .find_map(|matching| matching.linker.clone());
    }
    Ok(selected.map(|value| {
        let path = Path::new(&value.val);
        if !path.is_absolute() && value.val.contains(['/', '\\']) {
            // Cargo treats --config FILE paths like .cargo/config.toml:
            // program paths are relative to the file's grandparent.
            let root = match &value.definition {
                Some(
                    cargo_config2::de::Definition::Path(p)
                    | cargo_config2::de::Definition::Cli(Some(p)),
                ) => p.parent().and_then(Path::parent).unwrap_or(cwd),
                _ => cwd,
            };
            root.join(path)
        } else {
            path.to_path_buf()
        }
    }))
}
