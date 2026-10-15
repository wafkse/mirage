//! Process-level regression coverage for initial rendering, failures, and filesystem reloads.

use std::{
    fs,
    path::PathBuf,
    process::{Child, Command, ExitStatus, Output},
    thread,
    time::{Duration, Instant},
};

use tempfile::TempDir;

/// An isolated manifest, configure file, and template tree for a CLI invocation.
// NOTE(invariant): None.
struct Fixture {
    /// Directory retaining all inputs, outputs, and daemon logs for the test.
    root: TempDir,
}

impl Fixture {
    /// Create valid inputs with the template tree beside the configure directory.
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();

        let fixture = Self { root };

        fs::create_dir(fixture.path("configure")).unwrap();
        fs::create_dir(fixture.path("templates")).unwrap();

        let templates = fixture.path("templates");
        let socket = fixture.path("control.sock");

        let manifest = format!(
            "[configure]\ntemplate = '{}'\nlisten-sock = '{}'\nnotificate-period = 50\n[[candidate]]\npath = 'base.toml'\npolicy = 'override'\n",
            templates.display(),
            socket.display(),
        );

        fs::write(fixture.path("configure/.mirage.toml"), manifest).unwrap();
        fs::write(
            fixture.path("configure/base.toml"),
            "[default]\nvalue = 'ready'\n",
        )
        .unwrap();
        fs::write(fixture.path("templates/result.jinja"), "{{ value }}").unwrap();

        fixture
    }

    /// Resolve a test path inside the retained directory.
    fn path(&self, name: &str) -> PathBuf {
        let Self { root } = self;

        root.path().join(name)
    }

    /// Construct a CLI command with configure overrides removed from its environment.
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mirage"));

        command.arg("-C").arg(self.path("configure"));
        command.env("RUST_LOG", "info");

        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("MIRAGE_") {
                command.env_remove(key);
            }
        }

        command
    }

    /// Start a daemon whose lifetime and logs are owned by the test.
    fn daemon(&self) -> Daemon {
        let log = self.path("daemon.log");

        Daemon::new(self.command(), log)
    }
}

/// A child daemon that is terminated and reaped when its test scope ends.
// NOTE(invariant): None.
struct Daemon {
    /// Process running against the fixture inputs.
    child: Child,

    /// Captured standard output and standard error for failure diagnostics.
    log: PathBuf,
}

impl Daemon {
    /// Spawn a daemon with output captured in the fixture directory.
    fn new(mut command: Command, log: PathBuf) -> Self {
        let output = fs::File::create(&log).unwrap();

        let error = output.try_clone().unwrap();

        let child = command.stdout(output).stderr(error).spawn().unwrap();

        Self { child, log }
    }

    /// Wait for a condition while requiring the daemon to remain alive.
    fn until(&mut self, condition: impl Fn() -> bool) {
        let Self { child, log } = self;

        let deadline = Instant::now() + Duration::from_secs(5);

        loop {
            assert!(
                child.try_wait().unwrap().is_none(),
                "{}",
                fs::read_to_string(&*log).unwrap()
            );

            if condition() {
                break;
            }

            assert!(
                Instant::now() < deadline,
                "{}",
                fs::read_to_string(&*log).unwrap()
            );

            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Collect the process status within the test's bounded shutdown interval.
    fn wait(&mut self) -> ExitStatus {
        let Self { child, log } = self;

        let deadline = Instant::now() + Duration::from_secs(5);

        loop {
            match child.try_wait().unwrap() {
                Some(status) => break status,
                None => {
                    assert!(
                        Instant::now() < deadline,
                        "{}",
                        fs::read_to_string(&*log).unwrap()
                    );

                    thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let Self { child, .. } = self;

        let _ = child.kill();
        let _ = child.wait();
    }
}

#[test]
fn oneshot() {
    let fixture = Fixture::new();

    let Output {
        status,
        stdout,
        stderr,
    } = fixture.command().arg("--oneshot").output().unwrap();

    assert!(
        status.success(),
        "{}{}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );
    assert_eq!(
        fs::read_to_string(fixture.path("templates/result")).unwrap(),
        "ready"
    );
    assert!(!fixture.path("control.sock").exists());
}

#[test]
fn failures() {
    for failure in ["manifest", "context", "module", "template", "commit"] {
        let fixture = Fixture::new();

        match failure {
            "manifest" => fs::write(fixture.path("configure/.mirage.toml"), "[").unwrap(),
            "context" => fs::write(fixture.path("configure/base.toml"), "[").unwrap(),
            "module" => {
                let path = fixture.path("configure/.mirage.toml");

                let manifest = fs::read_to_string(&path)
                    .unwrap()
                    .replace("[configure]\n", "[configure]\nmodule = 'missing.luau'\n");

                fs::write(path, manifest).unwrap();
            }
            "template" => {
                fs::write(
                    fixture.path("templates/result.jinja"),
                    "{{ missing.value }}",
                )
                .unwrap();
                fs::write(fixture.path("templates/result"), "previous").unwrap();
            }
            "commit" => fs::create_dir(fixture.path("templates/result")).unwrap(),
            _ => unreachable!(),
        }

        let Output {
            status,
            stdout,
            stderr,
        } = fixture.command().arg("--oneshot").output().unwrap();

        assert_eq!(
            status.code(),
            Some(1),
            "{failure}: {}{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );

        if failure == "template" {
            assert_eq!(
                fs::read_to_string(fixture.path("templates/result")).unwrap(),
                "previous"
            );
        }
    }
}

#[test]
fn startup() {
    let fixture = Fixture::new();

    let mut daemon = fixture.daemon();

    daemon.until(|| fixture.path("templates/result").exists());

    assert_eq!(
        fs::read_to_string(fixture.path("templates/result")).unwrap(),
        "ready"
    );
}

#[test]
fn reload() {
    let fixture = Fixture::new();

    let mut daemon = fixture.daemon();

    daemon.until(|| fixture.path("templates/result").exists());

    fs::write(
        fixture.path("configure/base.toml"),
        "[default]\nvalue = 'changed'\n",
    )
    .unwrap();

    daemon.until(|| {
        fs::read_to_string(fixture.path("templates/result")).is_ok_and(|value| value == "changed")
    });
}

#[test]
fn recovery() {
    let fixture = Fixture::new();

    fs::write(
        fixture.path("templates/result.jinja"),
        "{{ missing.value }}",
    )
    .unwrap();

    let mut daemon = fixture.daemon();

    daemon.until(|| {
        fs::read_to_string(fixture.path("daemon.log"))
            .is_ok_and(|log| log.contains("hydration pass failed"))
    });

    fs::write(fixture.path("templates/result.jinja"), "{{ value }}").unwrap();

    daemon.until(|| fixture.path("templates/result").exists());

    assert_eq!(
        fs::read_to_string(fixture.path("templates/result")).unwrap(),
        "ready"
    );
}

#[test]
fn fatal() {
    let fixture = Fixture::new();

    fs::create_dir(fixture.path("templates/result")).unwrap();

    let mut daemon = fixture.daemon();

    assert_eq!(daemon.wait().code(), Some(1));
    assert!(fixture.path("templates/result").is_dir());
    let log = fs::read_to_string(fixture.path("daemon.log")).unwrap();

    assert!(log.contains("render pass staged"));
    assert!(log.contains("mirage exited with an error"));
}
