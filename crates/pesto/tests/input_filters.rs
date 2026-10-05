//! Input-filter regressions through the CLI, using offline dry-run uploads.

use std::process::Command;

#[test]
fn each_and_season_keep_path_globs_relative_to_the_input_root() {
    for mode in ["--each", "--season"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Release");
        std::fs::create_dir_all(root.join("extras")).unwrap();
        std::fs::write(root.join("extras/sample.mkv"), b"sample").unwrap();
        std::fs::write(root.join("extras/keep.mp4"), b"content").unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            r#"
            ext = ["mkv", "mp4"]
            exclude = ["extras/*.mkv"]
            [server]
            host = "example.invalid"
            [posting]
            groups = ["alt.test"]
            [output]
            history = false
            session_log = false
            no_hooks = true
        "#,
        )
        .unwrap();
        let out = dir.path().join("result.nzb");
        let result = Command::new(env!("CARGO_BIN_EXE_pesto"))
            .arg("--config")
            .arg(&config)
            .args(["--dry-run", "--no-check", "--no-hooks", "--par2", "0", mode])
            .arg("--out")
            .arg(&out)
            .arg(&root)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let nzb = std::fs::read_to_string(&out).unwrap();
        assert!(nzb.contains("keep.mp4"), "{mode}: {nzb}");
        assert!(!nzb.contains("sample.mkv"), "{mode}: {nzb}");
    }
}
