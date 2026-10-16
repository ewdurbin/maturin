use crate::common::integration::{self, IntegrationCase};
use crate::common::other;
use crate::common::{
    CFFI_MIXED_IMPLICIT_COPY, CFFI_MIXED_INCLUDE_EXCLUDE_COPY, CFFI_MIXED_PY_SUBDIR_COPY,
    CFFI_MIXED_SRC_COPY, CFFI_MIXED_SUBMODULE_COPY, CFFI_MIXED_WITH_PATH_DEP_COPY, handle_result,
    has_conda, has_uniffi_bindgen, has_uv, is_ci, test_python_implementation,
    test_python_supports_abi3t,
};
use std::path::Path;

#[cfg(any(target_os = "macos", target_env = "msvc"))]
#[test]
fn compiler_associates_companions_with_each_binary() -> anyhow::Result<()> {
    use anyhow::Context;
    use cargo_metadata::CrateType;
    use maturin::{BuildOptions, CargoOptions, OutputOptions};
    use std::process::Command;

    // CI can disable Rust debug info globally to speed up maturin's own build.
    // Run this test in isolation with the fixture's flags, without changing
    // the environment of the other tests running in parallel.
    if [
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_BUILD_RUSTFLAGS",
    ]
    .iter()
    .any(|key| std::env::var_os(key).is_some())
    {
        let output = Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "integration::compiler_associates_companions_with_each_binary",
                "--nocapture",
            ])
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("CARGO_BUILD_RUSTFLAGS")
            .output()?;
        assert!(
            output.status.success(),
            "{}\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return Ok(());
    }

    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    fs_err::create_dir_all(root.join("src/bin"))?;
    fs_err::write(
        root.join("Cargo.toml"),
        "[package]\nname = 'companions-test'\nversion = '1.0.0'\nedition = '2021'\n\n\
         [profile.companions]\ninherits = 'dev'\ndebug = 2\nstrip = false\n",
    )?;
    fs_err::write(
        root.join("pyproject.toml"),
        "[build-system]\nrequires = ['maturin']\nbuild-backend = 'maturin'\n\n\
         [tool.maturin]\nbindings = 'bin'\n",
    )?;
    fs_err::write(
        root.join("src/lib.rs"),
        "pub fn message() -> &'static str { \"test\" }\n",
    )?;
    for name in ["one", "two"] {
        fs_err::write(
            root.join(format!("src/bin/{name}.rs")),
            "fn main() { println!(\"{}\", companions_test::message()); }\n",
        )?;
    }
    let context = BuildOptions {
        cargo: CargoOptions {
            manifest_path: Some(root.join("Cargo.toml")),
            profile: Some("companions".into()),
            ..Default::default()
        },
        output: OutputOptions {
            include_debuginfo: true,
            ..Default::default()
        },
        ..Default::default()
    }
    .into_build_context()
    .strip(Some(false))
    .build()?;

    // Fresh and cached Cargo messages must both attach the extra files to
    // the binary, rather than to an arbitrary entry for the same package.
    for _ in 0..8 {
        let result = maturin::compile(&context, None, &context.project.compile_targets)?;
        assert_eq!(result.artifacts.len(), 2);
        for artifacts in result.artifacts {
            let binary = artifacts.get(&CrateType::Bin).context("missing binary")?;
            #[cfg(target_os = "macos")]
            let expected = binary.path.with_extension("dSYM");
            #[cfg(target_env = "msvc")]
            let expected = binary.path.with_extension("pdb");
            assert!(
                expected.exists(),
                "missing Cargo companion: {}",
                expected.display()
            );
            assert_eq!(binary.debuginfo_path.as_deref(), Some(expected.as_path()));
        }
    }
    Ok(())
}

#[test]
fn integration_pyo3_bin() {
    let python_implementation = test_python_implementation().unwrap();
    if python_implementation == "pypy" || python_implementation == "graalpy" {
        // PyPy & GraalPy do not support the auto-initialize feature of pyo3.
        return;
    }

    handle_result(integration::test_integration(&IntegrationCase::new(
        "integration-pyo3-bin",
        "test-crates/pyo3-bin",
    )));
}

#[rstest::rstest]
#[case::pyo3_pure(IntegrationCase::new("integration-pyo3-pure", "test-crates/pyo3-pure"))]
#[case::pyo3_mixed(IntegrationCase::new("integration-pyo3-mixed", "test-crates/pyo3-mixed"))]
// Keep the old mixed-layout regression coverage after moving these integration cases from pyo3 to
// cffi. The fixtures generate package files in-tree, so each case runs from a copied workspace.
#[case::cffi_mixed_include_exclude(IntegrationCase::new(
    "integration-cffi-mixed-include-exclude",
    "test-crates/cffi-mixed-include-exclude",
).copied(CFFI_MIXED_INCLUDE_EXCLUDE_COPY))]
#[case::cffi_mixed_submodule(IntegrationCase::new(
    "integration-cffi-mixed-submodule",
    "test-crates/cffi-mixed-submodule",
).copied(CFFI_MIXED_SUBMODULE_COPY))]
#[case::cffi_mixed_with_path_dep(IntegrationCase::new(
    "integration-cffi-mixed-with-path-dep",
    "test-crates/cffi-mixed-with-path-dep",
).copied(CFFI_MIXED_WITH_PATH_DEP_COPY))]
#[case::cffi_mixed_implicit(IntegrationCase::new(
    "integration-cffi-mixed-implicit",
    "test-crates/cffi-mixed-implicit",
).copied(CFFI_MIXED_IMPLICIT_COPY))]
#[case::cffi_mixed_py_subdir({
    let case = IntegrationCase::new(
        "integration-cffi-mixed-py-subdir",
        "test-crates/cffi-mixed-py-subdir",
    ).copied(CFFI_MIXED_PY_SUBDIR_COPY);
    if cfg!(unix) { case.zig() } else { case }
})]
#[case::cffi_mixed_src_layout(IntegrationCase::new(
    "integration-cffi-mixed-src",
    "test-crates/cffi-mixed-src/rust",
).copied(CFFI_MIXED_SRC_COPY))]
#[case::uniffi_pure_proc_macro(IntegrationCase::new(
    "integration-uniffi-pure-proc-macro",
    "test-crates/uniffi-pure-proc-macro",
))]
#[case::hello_world(IntegrationCase::new("integration-hello-world", "test-crates/hello-world"))]
#[case::pyo3_ffi_pure(IntegrationCase::new(
    "integration-pyo3-ffi-pure",
    "test-crates/pyo3-ffi-pure"
))]
#[case::with_data(IntegrationCase::new("integration-with-data", "test-crates/with-data"))]
#[case::readme_duplication(IntegrationCase::new(
    "integration-readme-duplication",
    "test-crates/readme-duplication/readme-py",
))]
#[case::workspace_inverted_order(IntegrationCase::new(
    "integration-workspace-inverted-order",
    "test-crates/workspace-inverted-order/path-dep-with-root",
))]
#[case::pyo3_stub_generation_pure(IntegrationCase::new(
    "integration-pyo3-stub-generation-pure",
    "test-crates/pyo3-stub-generation-pure",
).generate_stubs())]
#[cfg_attr(unix, case::pyo3_stub_generation_pure_zig(IntegrationCase::new(
    "integration-pyo3-stub-generation-pure-zig",
    "test-crates/pyo3-stub-generation-pure",
).generate_stubs().zig()))]
#[case::pyo3_stub_generation_mixed(IntegrationCase::new(
    "integration-pyo3-stub-generation-mixed",
    "test-crates/pyo3-stub-generation-mixed",
).generate_stubs())]
#[case::pyo3_stub_generation_mixed_py_subdir(IntegrationCase::new(
    "integration-pyo3-stub-generation-mixed-py-subdir",
    "test-crates/pyo3-stub-generation-mixed-py-subdir",
).generate_stubs())]
#[test]
fn integration_cases(#[case] case: IntegrationCase<'_>) {
    handle_result(integration::test_integration(&case));
}

#[test]
fn integration_pyo3_bin_uv_multi_python() {
    if has_uv() {
        handle_result(integration::test_integration_uv_multi_python(
            &IntegrationCase::new(
                "integration-pyo3-bin-uv-multi-python",
                "test-crates/pyo3-bin",
            ),
        ));
    }
}

#[test]
#[cfg_attr(target_os = "macos", ignore)]
fn integration_pyo3_mixed_conda() {
    // Don't run it on macOS, too slow.
    if has_conda() {
        handle_result(integration::test_integration_conda(
            "test-crates/pyo3-mixed",
            None,
            "integration-pyo3-mixed-conda",
        ));
    }
}

#[rstest::rstest]
#[case::cffi_pure(IntegrationCase::new("integration-cffi-pure", "test-crates/cffi-pure"))]
#[case::cffi_mixed(IntegrationCase::new("integration-cffi-mixed", "test-crates/cffi-mixed"))]
#[test]
fn integration_cffi_cases(#[case] case: IntegrationCase<'_>) {
    if is_ci() && test_python_implementation().unwrap() == "pypy" {
        return;
    }
    handle_result(integration::test_integration(&case));
}

#[rstest::rstest]
#[case::uniffi_pure(IntegrationCase::new("integration-uniffi-pure", "test-crates/uniffi-pure"))]
#[case::uniffi_mixed(IntegrationCase::new("integration-uniffi-mixed", "test-crates/uniffi-mixed"))]
#[test]
fn integration_uniffi_cases(#[case] case: IntegrationCase<'_>) {
    if is_ci() || has_uniffi_bindgen() {
        handle_result(integration::test_integration(&case));
    }
}

#[test]
#[cfg(any(
    all(target_os = "windows", target_arch = "x86_64"),
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64"),
        target_env = "gnu",
    ),
    all(
        target_os = "macos",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
))]
fn integration_wasm_hello_world() {
    handle_result(integration::test_integration(
        &IntegrationCase::new("integration-wasm-hello-world", "test-crates/hello-world")
            .target("wasm32-wasip1"),
    ));

    let python_implementation = test_python_implementation().unwrap();
    let venv_name =
        format!("integration-wasm-hello-world-py3-wasm32-wasip1-{python_implementation}");

    assert!(
        Path::new("test-crates")
            .join("venvs")
            .join(venv_name)
            .join(if cfg!(target_os = "windows") {
                "Scripts"
            } else {
                "bin"
            })
            .join("hello-world.wasm")
            .is_file()
    )
}

#[test]
fn abi3_without_version() {
    handle_result(other::abi3_without_version())
}

#[test]
fn pyo3_cffi_build_script() {
    handle_result(other::pyo3_cffi_build_script())
}

#[test]
fn abi3t_without_version() {
    // abi3t requires CPython >= 3.15 (PEP 803). On older runners the build
    // would reject the only available interpreter, so skip cleanly.
    if !test_python_supports_abi3t() {
        return;
    }
    handle_result(other::abi3t_without_version())
}

#[test]
fn abi3_and_abi3t_wheel_selection() {
    handle_result(other::combined_stable_abi_wheel_selection(
        "abi3-and-abi3t-wheel-selection",
        &[],
    ));
}

#[test]
fn abi3_and_current_abi3t_wheel_selection() {
    handle_result(other::combined_stable_abi_wheel_selection(
        "abi3-and-current-abi3t-wheel-selection",
        &["abi3-and-current-abi3t"],
    ));
}

#[test]
fn integration_pyo3_abi3t() {
    // abi3t requires CPython >= 3.15 (PEP 803).
    if !test_python_supports_abi3t() {
        return;
    }
    handle_result(integration::test_integration(&IntegrationCase::new(
        "integration-pyo3-abi3t",
        "test-crates/pyo3-abi3t",
    )));
}

#[test]
fn abi3_python_interpreter_args() {
    handle_result(other::abi3_python_interpreter_args());
}

#[test]
fn abi3_generate_stubs() {
    handle_result(other::generate_stubs(
        "test-crates/pyo3-stub-generation-pure",
        &[
            "pyo3_stub_generation_pure/__init__.pyi",
            "pyo3_stub_generation_pure/submodule.pyi",
        ],
    ));
}

/// `module-name` places the extension inside a python package, and the stubs have to follow it
/// there — writing `_pyo3_mixed.pyi` to the root of `--out` puts it where nothing will look for it.
#[test]
fn abi3_generate_stubs_mixed_py_subdir() {
    handle_result(other::generate_stubs(
        "test-crates/pyo3-stub-generation-mixed-py-subdir",
        &["pyo3_stub_generation_mixed_py_subdir/_pyo3_mixed.pyi"],
    ));
}
