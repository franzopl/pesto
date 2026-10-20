//! Issue #199: installer platform selection must never fetch the wrong CPU build.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn executable(path: &Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn install(
    arch: &str,
    libc: &str,
    force_musl: bool,
    arm_available: bool,
    expected: Result<&str, &str>,
) {
    let root = tempfile::tempdir().unwrap();
    let stubs = root.path().join("stubs");
    std::fs::create_dir(&stubs).unwrap();
    executable(
        &stubs.join("uname"),
        "#!/bin/bash\ncase \"$1\" in -s) echo Linux;; -m) echo \"$TEST_ARCH\";; *) exit 99;; esac\n",
    );
    executable(&stubs.join("ldd"), "#!/bin/bash\necho \"$TEST_LIBC\"\n");
    executable(
        &stubs.join("curl"),
        r##"#!/bin/bash
set -eu
url=""
destination=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        -o) destination="$2"; shift 2;;
        -H) shift 2;;
        -*) shift;;
        *) url="$1"; shift;;
    esac
done
printf '%s\n' "$url" >> "$TEST_CURL_LOG"
if [ "$url" = https://api.github.com/repos/franzopl/pesto/releases ]; then
    printf '%s\n' "$TEST_RELEASE_JSON"
else
    case "$url" in
        https://example.invalid/pesto-linux-x86_64|https://example.invalid/pesto-linux-x86_64-musl|https://example.invalid/pesto-linux-aarch64) ;;
        *) exit 99;;
    esac
    # The installed stub only allows the installer's version smoke check.
    # It cannot upload, run hooks, or launch the configuration wizard.
    printf '#!/bin/bash\n[ "$1" = --version ] || exit 99\n' > "$destination"
fi
"##,
    );
    let curl_log = root.path().join("curl.log");
    std::fs::write(&curl_log, "").unwrap();
    let install_dir = root.path().join("bin");
    let mut assets = vec![
        serde_json::json!({"browser_download_url": "https://example.invalid/pesto-linux-x86_64"}),
        serde_json::json!({"browser_download_url": "https://example.invalid/pesto-linux-x86_64-musl"}),
    ];
    if arm_available {
        assets.push(serde_json::json!({"browser_download_url": "https://example.invalid/pesto-linux-aarch64"}));
    }
    let release = serde_json::json!([{"tag_name": "pesto-v0.10.5", "assets": assets}]);
    let mut command = Command::new("bash");
    command
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/install.sh"
        ))
        .args([
            "--no-path-update",
            "--no-config-wizard",
            "--no-api-key-prompt",
            "--no-mediainfo-check",
            "--install-dir",
        ])
        .arg(&install_dir)
        .env("TEST_ARCH", arch)
        .env("TEST_LIBC", libc)
        .env("TEST_CURL_LOG", &curl_log)
        .env("TEST_RELEASE_JSON", release.to_string())
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env(
            "PATH",
            std::env::join_paths(
                std::iter::once(stubs)
                    .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
            )
            .unwrap(),
        );
    if force_musl {
        command.arg("--musl");
    }
    let output = command.output().unwrap();
    let downloads = std::fs::read_to_string(curl_log).unwrap();
    match expected {
        Ok(asset) => {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                downloads.lines().collect::<Vec<_>>(),
                [
                    "https://api.github.com/repos/franzopl/pesto/releases",
                    &format!("https://example.invalid/{asset}")
                ]
            );
            assert!(install_dir.join("pesto").is_file());
        }
        Err(message) => {
            assert!(!output.status.success());
            assert!(String::from_utf8_lossy(&output.stderr).contains(message));
            if arm_available {
                assert!(
                    downloads.is_empty(),
                    "unsupported platforms must fail before any download"
                );
            } else {
                assert_eq!(
                    downloads.trim(),
                    "https://api.github.com/repos/franzopl/pesto/releases"
                );
            }
            assert!(!install_dir.exists());
        }
    }
}

#[test]
fn installer_selects_linux_architecture_aliases() {
    for (arch, asset) in [
        ("x86_64", "pesto-linux-x86_64"),
        ("amd64", "pesto-linux-x86_64"),
        ("aarch64", "pesto-linux-aarch64"),
        ("arm64", "pesto-linux-aarch64"),
    ] {
        install(arch, "GNU libc 2.36", false, true, Ok(asset));
    }
}

#[test]
fn installer_selects_x86_64_musl_automatically_or_explicitly() {
    install(
        "x86_64",
        "musl libc",
        false,
        true,
        Ok("pesto-linux-x86_64-musl"),
    );
    install(
        "amd64",
        "GNU libc 2.36",
        true,
        true,
        Ok("pesto-linux-x86_64-musl"),
    );
}

#[test]
fn installer_rejects_unpublished_platforms_before_downloading() {
    install(
        "aarch64",
        "musl libc",
        false,
        true,
        Err("No Linux musl build"),
    );
    install(
        "arm64",
        "GNU libc 2.36",
        true,
        true,
        Err("No Linux musl build"),
    );
    install(
        "armv7l",
        "GNU libc 2.36",
        false,
        true,
        Err("No Linux build"),
    );
    install(
        "riscv64",
        "GNU libc 2.36",
        false,
        true,
        Err("No Linux build"),
    );
}

#[test]
fn installer_reports_a_release_without_arm64_assets() {
    install(
        "aarch64",
        "GNU libc 2.36",
        false,
        false,
        Err("Could not find a pesto-v* release with asset pesto-linux-aarch64"),
    );
}
