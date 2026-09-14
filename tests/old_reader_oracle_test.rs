//! Opt-in, hash-pinned Linux old-reader oracle. No downloads or ambient DBs.

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires explicit opt-in and the attested v0.22.16 Linux binary"]
fn barrier_config_blocks_02216_without_write() {
    use std::{
        fs,
        io::Read,
        path::Path,
        process::{Command, Output},
    };

    use sha2::{Digest, Sha256};

    const EXPECTED_SHA: &str = "d04f6221e4a8aa77f118e287fa2eba7e45a0b3520e2e9f10f2a1a81b7c0a6f55";
    assert_eq!(
        std::env::var("LIBRA_ENABLE_OLD_READER_ORACLE").as_deref(),
        Ok("1")
    );
    assert_eq!(
        std::env::var("LIBRA_TEST_OLD_BINARY_SHA256").as_deref(),
        Ok(EXPECTED_SHA)
    );
    let old = fs::canonicalize(
        std::env::var_os("LIBRA_TEST_OLD_BINARY").expect("provide pinned old binary"),
    )
    .unwrap();
    let mut file = fs::File::open(&old).unwrap();
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let length = file.read(&mut buffer).unwrap();
        if length == 0 {
            break;
        }
        hasher.update(&buffer[..length]);
    }
    assert_eq!(
        hex::encode(hasher.finalize()),
        EXPECTED_SHA,
        "old binary provenance mismatch"
    );
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let run = |binary: &Path, args: &[&str]| -> Output {
        Command::new("/usr/bin/timeout")
            .args(["--signal=INT", "--kill-after=5s", "30s"])
            .arg(binary)
            .args(args)
            .current_dir(root)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", root)
            .env("USERPROFILE", root)
            .env("XDG_CONFIG_HOME", root.join("xdg"))
            .env("LIBRA_CONFIG_GLOBAL_DB", root.join("global.db"))
            .env("LIBRA_CONFIG_SYSTEM_DB", root.join("system.db"))
            .env("LIBRA_TEST", "1")
            .output()
            .unwrap()
    };
    let version = run(&old, &["--version"]);
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        "libra 0.22.16"
    );
    let current = Path::new(env!("CARGO_BIN_EXE_libra"));
    for (flag, filename) in [("--global", "global.db"), ("--system", "system.db")] {
        let created = run(
            current,
            &["config", "set", flag, "test.oracle", "preserved"],
        );
        assert!(
            created.status.success(),
            "{}",
            String::from_utf8_lossy(&created.stderr)
        );
        let path = root.join(filename);
        let before = fs::read(&path).unwrap();
        for args in [
            vec!["config", "set", flag, "test.oracle", "must-not-write"],
            vec!["config", "unset", flag, "test.oracle"],
        ] {
            let rejected = run(&old, &args);
            assert!(!rejected.status.success());
            assert_ne!(
                rejected.status.code(),
                Some(124),
                "old reader timed out instead of refusing"
            );
            let stderr = String::from_utf8_lossy(&rejected.stderr);
            assert!(
                stderr.contains("newer") && stderr.contains("9223372036854775807"),
                "{stderr}"
            );
            assert_eq!(
                fs::read(&path).unwrap(),
                before,
                "old reader changed {flag}"
            );
        }
        let read = run(current, &["config", "get", flag, "test.oracle"]);
        assert!(read.status.success());
        assert!(String::from_utf8_lossy(&read.stdout).contains("preserved"));
        assert_eq!(fs::read(&path).unwrap(), before);
        let updated = run(
            current,
            &["config", "set", flag, "test.oracle", "new-build"],
        );
        assert!(
            updated.status.success(),
            "{}",
            String::from_utf8_lossy(&updated.stderr)
        );
    }
}
