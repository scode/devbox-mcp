//! Tests of `devbox-mcp setup` and `devbox-mcp set-key` (SPEC_impl.md, "Deployment"), run against a scratch home
//! directory with the real, pinned tunnel-client, so the profile checked here is the one tunnel-client itself writes.
//! Nothing here enables, starts, or talks to a service: both commands only write files, and these tests only read them
//! back.

mod support;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use support::dev_proxy::default_tunnel_client;
use support::hermetic_env;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;

/// A well-formed tunnel ID (tunnel-client checks the format) that names no real tunnel.
const TUNNEL_ID: &str = "tunnel_0123456789abcdef0123456789abcdef";

/// The key file, relative to the home's `.config`.
const KEY_FILE: &str = "devbox-mcp/tunnel.env";

/// A scratch home plus a stand-in Codex executable, which setup only checks exists.
struct Home {
    dir: tempfile::TempDir,
    codex: PathBuf,
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let codex = dir.path().join("codex");
        std::fs::write(&codex, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir, codex }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// A file under the home's `.config`, as a string.
    fn config(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path().join(".config").join(rel)).unwrap()
    }

    fn mode(&self, rel: &str) -> u32 {
        std::fs::metadata(self.path().join(".config").join(rel))
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    }

    /// Runs setup in this home with the default paths for everything else.
    async fn setup(&self, tunnel_id: &str) -> Output {
        self.run_setup(tunnel_id, self.path(), None).await
    }

    /// Runs setup with `home` passed as the home directory, which may differ from this scratch home, to test path
    /// checks.
    async fn setup_with_home(&self, home: &Path) -> Output {
        self.run_setup(TUNNEL_ID, home, None).await
    }

    /// Runs setup in this home with `path` as the PATH of the shell it runs in.
    async fn setup_with_path(&self, path: &str) -> Output {
        self.run_setup(TUNNEL_ID, self.path(), Some(path)).await
    }

    /// Runs setup with stdin closed, so a setup that tried to read a key would get nothing.
    async fn run_setup(&self, tunnel_id: &str, home: &Path, path: Option<&str>) -> Output {
        let mut command = self.command("setup", home);
        command
            .arg("--tunnel-id")
            .arg(tunnel_id)
            .arg("--tunnel-client")
            .arg(default_tunnel_client())
            .arg("--codex-bin")
            .arg(&self.codex)
            .stdin(Stdio::null());
        if let Some(path) = path {
            command.env("PATH", path);
        }
        command.output().await.unwrap()
    }

    /// Runs set-key in this home with `key` on stdin.
    async fn set_key(&self, key: &str) -> Output {
        let mut child = self
            .command("set-key", self.path())
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(key.as_bytes()).await.unwrap();
        drop(stdin);
        child.wait_with_output().await.unwrap()
    }

    /// A devbox-mcp subcommand working in `home`. Its environment is hermetic, so neither it nor tunnel-client can see
    /// the developer's own configuration.
    fn command(&self, subcommand: &str, home: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_devbox-mcp"));
        command
            .arg(subcommand)
            .arg("--home")
            .arg(home)
            .env_clear()
            .envs(hermetic_env(self.path()))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Spec (SPEC_impl.md "Deployment"): set-key writes the key, read from stdin, to a file only the user can read, in a
/// directory only the user can enter, and does not echo it.
#[tokio::test]
async fn set_key_writes_a_private_key_file() {
    let home = Home::new();
    let output = home.set_key("sk-test-key\n").await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(home.config(KEY_FILE), "CONTROL_PLANE_API_KEY=sk-test-key\n");
    assert_eq!(home.mode(KEY_FILE), 0o600);
    assert_eq!(home.mode("devbox-mcp"), 0o700);
    assert!(!stdout(&output).contains("sk-test-key"));
}

/// Spec (SPEC_impl.md "Deployment", `SEC-3`): setup writes tunnel-client's profile (through tunnel-client) and a unit
/// that loads the key file and runs tunnel-client with its admin listener on a private Unix socket, restarting it
/// always. The key appears in neither, and setup only prints how to start the service.
///
/// These are the properties the security model and the service's survival depend on, so they are pinned here rather
/// than left to the deployment guide.
#[tokio::test]
async fn setup_writes_the_profile_and_unit() {
    let home = Home::new();
    assert!(home.set_key("sk-test-key").await.status.success());
    let output = home.setup(TUNNEL_ID).await;
    assert!(output.status.success(), "{}", stderr(&output));

    let profile = home.config("tunnel-client/devbox.yaml");
    assert!(profile.contains(TUNNEL_ID), "{profile}");
    let devbox_mcp = env!("CARGO_BIN_EXE_devbox-mcp");
    let command = format!("{devbox_mcp} --codex-bin {}", home.codex.display());
    assert!(profile.contains(&command), "{profile}");

    let unit = home.config("systemd/user/devbox-mcp.service");
    let tunnel_client = default_tunnel_client();
    let profile_dir = home.path().join(".config/tunnel-client");
    let key_file = home.path().join(".config").join(KEY_FILE);
    for line in [
        format!("EnvironmentFile={}", key_file.display()),
        format!(
            "ExecStart={} run --profile devbox --profile-dir {} --health.unix-socket %t/devbox-mcp/tunnel-client.sock",
            tunnel_client.display(),
            profile_dir.display()
        ),
        "RuntimeDirectory=devbox-mcp".to_owned(),
        "RuntimeDirectoryMode=0700".to_owned(),
        "Restart=always".to_owned(),
        "KillMode=control-group".to_owned(),
        "TimeoutStopSec=15".to_owned(),
        "WantedBy=default.target".to_owned(),
    ] {
        assert!(
            unit.lines().any(|l| l == line),
            "missing {line:?} in:\n{unit}"
        );
    }

    assert!(!profile.contains("sk-test-key"));
    assert!(!unit.contains("sk-test-key"));
    let stdout = stdout(&output);
    assert!(
        stdout.contains("systemctl --user enable --now devbox-mcp.service"),
        "{stdout}"
    );
    // A key is there, so there is no reason to ask for one.
    assert!(!stdout.contains("set-key"), "{stdout}");
    // tunnel-client init's own advice to run it in the foreground would bypass the unit's private socket.
    assert!(!stdout.contains("tunnel-client run"), "{stdout}");
}

/// Spec (SPEC_impl.md "Deployment"): setup works before any key is given, and then says to run set-key first, since
/// the service cannot start without one.
#[tokio::test]
async fn setup_without_a_key_says_to_set_one() {
    let home = Home::new();
    let output = home.setup(TUNNEL_ID).await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("devbox-mcp set-key"),
        "{}",
        stdout(&output)
    );
    assert!(!home.path().join(".config").join(KEY_FILE).exists());
}

/// Spec (SPEC_impl.md "Deployment"): running setup again replaces its files, for example to change the tunnel ID, and
/// leaves the key exactly as it was, so that it never has to be pasted again for that.
#[tokio::test]
async fn running_setup_again_replaces_its_files_but_not_the_key() {
    let home = Home::new();
    assert!(home.set_key("the-key").await.status.success());
    assert!(home.setup(TUNNEL_ID).await.status.success());

    let other_id = "tunnel_ffffffffffffffffffffffffffffffff";
    let output = home.setup(other_id).await;
    assert!(output.status.success(), "{}", stderr(&output));
    let profile = home.config("tunnel-client/devbox.yaml");
    assert!(
        profile.contains(other_id) && !profile.contains(TUNNEL_ID),
        "{profile}"
    );
    assert_eq!(home.config(KEY_FILE), "CONTROL_PLANE_API_KEY=the-key\n");
}

/// Spec (SPEC_impl.md "Deployment"): set-key replaces an earlier key, and re-restricts a key file whose permissions
/// were loosened in the meantime.
#[tokio::test]
async fn set_key_replaces_the_key() {
    let home = Home::new();
    assert!(home.set_key("first-key").await.status.success());
    let key_file = home.path().join(".config").join(KEY_FILE);
    std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o644)).unwrap();

    let output = home.set_key("second-key").await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(home.config(KEY_FILE), "CONTROL_PLANE_API_KEY=second-key\n");
    assert_eq!(home.mode(KEY_FILE), 0o600);
}

/// Spec (SPEC_impl.md "Deployment"): the service gets the PATH of the shell setup ran in, minus entries a unit cannot
/// carry unescaped or a service must not depend on, so the agents' commands find the user's usual tools rather than
/// only the systemd user manager's minimal PATH. Dropped entries are reported.
#[tokio::test]
async fn gives_the_service_the_shells_path() {
    let home = Home::new();
    let output = home
        .setup_with_path("/opt/tools/bin:relative/bin:/has space/bin:/usr/bin:/bin")
        .await;
    assert!(output.status.success(), "{}", stderr(&output));
    let unit = home.config("systemd/user/devbox-mcp.service");
    assert!(
        unit.lines()
            .any(|l| l == "Environment=PATH=/opt/tools/bin:/usr/bin:/bin"),
        "{unit}"
    );
    let stderr = stderr(&output);
    assert!(
        stderr.contains("relative/bin") && stderr.contains("/has space/bin"),
        "{stderr}"
    );
}

/// Spec (SPEC_impl.md "Deployment"): the key goes into a fresh file, never through a symlink or hardlink left at the
/// key file's path, because the key must appear in no file but the key file, and the link's target may hold something
/// else entirely.
#[tokio::test]
async fn never_writes_the_key_through_a_link() {
    for link in ["symlink", "hardlink"] {
        let home = Home::new();
        let key_file = home.path().join(".config").join(KEY_FILE);
        std::fs::create_dir_all(key_file.parent().unwrap()).unwrap();
        let other = home.path().join("other-file");
        std::fs::write(&other, "unrelated\n").unwrap();
        if link == "symlink" {
            std::os::unix::fs::symlink(&other, &key_file).unwrap();
        } else {
            std::fs::hard_link(&other, &key_file).unwrap();
        }

        let output = home.set_key("secret-key").await;
        assert!(output.status.success(), "{link}: {}", stderr(&output));
        assert_eq!(
            std::fs::read_to_string(&other).unwrap(),
            "unrelated\n",
            "{link}"
        );
        let meta = std::fs::symlink_metadata(&key_file).unwrap();
        assert!(meta.is_file(), "{link}: key file is not a regular file");
        assert_eq!(home.config(KEY_FILE), "CONTROL_PLANE_API_KEY=secret-key\n");
        assert_eq!(home.mode(KEY_FILE), 0o600, "{link}");
    }
}

/// Spec (SPEC_impl.md "Deployment"): a path that would need escaping in the unit, a tunnel ID tunnel-client rejects, a
/// Codex that is not executable, and a missing or malformed key all fail without writing anything.
///
/// Refusing instead of escaping is a deliberate simplification, so it must at least be reliable: a half-written setup
/// would leave a unit pointing at files that do not match.
#[tokio::test]
async fn refuses_bad_input_without_writing_anything() {
    let home = Home::new();
    let spaced = home.path().join("my home");
    std::fs::create_dir(&spaced).unwrap();

    for (what, output) in [
        ("path with a space", home.setup_with_home(&spaced).await),
        ("bad tunnel ID", home.setup("tunnel_x").await),
        ("empty key", home.set_key("\n").await),
        ("key with a quote", home.set_key("ab\"cd").await),
    ] {
        assert!(!output.status.success(), "{what}: succeeded");
        assert!(!stderr(&output).is_empty(), "{what}: no error message");
    }
    assert!(!home.path().join(".config").exists());
    assert!(!spaced.join(".config").exists());

    // A Codex that cannot be executed would only fail later, when the service starts a turn.
    let home = Home::new();
    std::fs::set_permissions(&home.codex, std::fs::Permissions::from_mode(0o644)).unwrap();
    let output = home.setup(TUNNEL_ID).await;
    assert!(
        !output.status.success(),
        "non-executable codex: setup succeeded"
    );
    assert!(!home.path().join(".config").exists());
}
