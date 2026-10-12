#![expect(
    clippy::disallowed_methods,
    reason = "synchronous migration tests use disposable files and a child process to isolate umask"
)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use super::files;

const ENVIRONMENTS: &str =
    "[environments.qa]\nprovider = \"local\"\nenv = { CANARY = \"harmless\" }\n";
const SLATEDB: &str = "[server.slatedb]\nprovider = \"local\"\nprefix = \"harmless\"\n";
const LEGACY_SANDBOX: &str = "[run.sandbox]\nprovider = \"docker\"\n";
const ENVIRONMENT_BACKUP: &str = "settings.toml.settings-environments-migration";
const SLATEDB_BACKUP: &str = "settings.toml.server-slatedb-migration";

#[test]
fn migrations_preserve_permissions_under_controlled_umasks() {
    const CHILD_UMASK: &str = "FABRO_CONFIG_TEST_MIGRATION_UMASK";
    if let Some(umask) = std::env::var_os(CHILD_UMASK) {
        let umask = u32::from_str_radix(umask.to_str().expect("umask is ASCII"), 8).unwrap();
        for source_mode in [0o600, 0o640] {
            for (source, backup_stems, environment_name) in [
                (SLATEDB.to_string(), vec![SLATEDB_BACKUP], None),
                (
                    ENVIRONMENTS.to_string(),
                    vec![ENVIRONMENT_BACKUP],
                    Some("qa"),
                ),
                (
                    format!("{ENVIRONMENTS}\n{SLATEDB}"),
                    vec![ENVIRONMENT_BACKUP, SLATEDB_BACKUP],
                    Some("qa"),
                ),
                (
                    LEGACY_SANDBOX.to_string(),
                    vec![ENVIRONMENT_BACKUP],
                    Some("default"),
                ),
            ] {
                let dir = tempfile::tempdir().unwrap();
                let settings = dir.path().join("settings.toml");
                fs::write(&settings, &source).unwrap();
                fs::set_permissions(&settings, fs::Permissions::from_mode(source_mode)).unwrap();

                // Force the suffix retry; existing backups must remain untouched.
                for stem in &backup_stems {
                    fs::write(dir.path().join(format!("{stem}.bak")), "older backup").unwrap();
                }
                let report = super::run_migrations(&settings, &source).unwrap().unwrap();
                assert_mode(&settings, source_mode);
                assert_eq!(fs::read_to_string(&settings).unwrap(), report.contents);
                assert!(!report.contents.contains("[server.slatedb]"));
                assert!(!report.contents.contains("[environments."));

                for stem in &backup_stems {
                    assert_eq!(
                        fs::read_to_string(dir.path().join(format!("{stem}.bak"))).unwrap(),
                        "older backup"
                    );
                    let backup = dir.path().join(format!("{stem}.1.bak"));
                    assert_mode(&backup, source_mode & !umask);
                    let contents = fs::read_to_string(&backup).unwrap();
                    if *stem == ENVIRONMENT_BACKUP || backup_stems.len() == 1 {
                        assert_eq!(contents, source);
                    } else {
                        assert!(contents.contains("[server.slatedb]"));
                        assert!(!contents.contains("[environments."));
                    }
                }
                if let Some(name) = environment_name {
                    let environment = dir.path().join(format!("environments/{name}.toml"));
                    assert_mode(&environment, source_mode & !umask);
                    if name == "qa" {
                        assert!(
                            fs::read_to_string(environment)
                                .unwrap()
                                .contains("harmless")
                        );
                    }
                }
                assert!(
                    super::run_migrations(&settings, &report.contents)
                        .unwrap()
                        .is_none()
                );
                for stem in &backup_stems {
                    assert!(!dir.path().join(format!("{stem}.2.bak")).exists());
                }
            }
        }
        return;
    }

    // Changing umask in a threaded test process would race other file creation.
    for umask in ["000", "022", "077"] {
        let output = Command::new("sh")
            .args([
                "-c",
                "umask \"$1\"; exec \"$2\" --exact migrations::tests::migrations_preserve_permissions_under_controlled_umasks --nocapture",
                "migration-permissions",
                umask,
            ])
            .arg(std::env::current_exe().unwrap())
            .env(CHILD_UMASK, umask)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "umask {umask}:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn new_files_are_private_before_contents_are_written() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("backup");
    let file = files::create_new_file(&path, &fs::Permissions::from_mode(0o600)).unwrap();
    assert_mode(&path, 0o600);
    assert_eq!(file.metadata().unwrap().len(), 0);
    assert_eq!(
        files::create_new_file(&path, &fs::Permissions::from_mode(0o644))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::AlreadyExists
    );
    assert_mode(&path, 0o600);
}

#[test]
fn existing_environment_keeps_contents_and_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let settings = dir.path().join("settings.toml");
    fs::write(&settings, ENVIRONMENTS).unwrap();
    fs::set_permissions(&settings, fs::Permissions::from_mode(0o600)).unwrap();
    fs::create_dir(dir.path().join("environments")).unwrap();
    let environment = dir.path().join("environments/qa.toml");
    let existing = "provider = \"local\"\nenv = { CANARY = \"existing\" }\n";
    fs::write(&environment, existing).unwrap();
    fs::set_permissions(&environment, fs::Permissions::from_mode(0o640)).unwrap();

    super::run_migrations(&settings, ENVIRONMENTS)
        .unwrap()
        .unwrap();

    assert_eq!(fs::read_to_string(&environment).unwrap(), existing);
    assert_mode(&environment, 0o640);
    assert_mode(&settings, 0o600);
}

#[test]
fn missing_source_permissions_abort_before_creating_destinations() {
    use std::error::Error as _;

    for source in [ENVIRONMENTS, SLATEDB] {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.toml");
        let error = super::run_migrations(&settings, source).unwrap_err();
        let cause = error
            .source()
            .unwrap()
            .downcast_ref::<std::io::Error>()
            .unwrap();
        assert_eq!(cause.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}

fn assert_mode(path: &Path, expected: u32) {
    assert_eq!(
        fs::metadata(path)
            .expect("migration file metadata")
            .permissions()
            .mode()
            & 0o777,
        expected,
        "permissions on {}",
        path.display()
    );
}
