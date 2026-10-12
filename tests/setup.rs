//! Tests of `devbox-mcp setup` (SPEC_impl.md, "Deployment"), run against a scratch home directory with the real,
//! pinned tunnel-client, so the profile checked here is the one tunnel-client itself writes. Nothing here enables,
//! starts, or talks to a service: setup only writes files, and these tests only read them back.

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

    /// Runs setup in this home with `key` on stdin and the default paths for everything else.
    async fn setup(&self, tunnel_id: &str, key: &str) -> Output {
        self.setup_with(tunnel_id, key, self.path()).await
    }

    /// Runs setup with `home` passed as the home directory (which may differ from this scratch home, to test path
    /// checks). Its environment is hermetic, so neither setup nor tunnel-client can see the developer's own
    /// configuration.
    async fn setup_with(&self, tunnel_id: &str, key: &str, home: &Path) -> Output {
        self.run_setup(tunnel_id, key, home, None).await
    }

    /// Runs setup in this home with `path` as the PATH of the shell it runs in.
    async fn setup_with_path(&self, path: &str) -> Output {
        self.run_setup(TUNNEL_ID, "key", self.path(), Some(path))
            .await
    }

    async fn run_setup(
        &self,
        tunnel_id: &str,
        key: &str,
        home: &Path,
        path: Option<&str>,
    ) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_devbox-mcp"));
        command
            .arg("setup")
            .arg("--tunnel-id")
            .arg(tunnel_id)
            .arg("--home")
            .arg(home)
            .arg("--tunnel-client")
            .arg(default_tunnel_client())
            .arg("--codex-bin")
            .arg(&self.codex)
            .env_clear()
            .envs(hermetic_env(self.path()))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(path) = path {
            command.env("PATH", path);
        }
        let mut child = command.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(key.as_bytes()).await.unwrap();
        drop(stdin);
        child.wait_with_output().await.unwrap()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Spec (SPEC_impl.md "Deployment", `SEC-3`): setup writes tunnel-client's profile (through tunnel-client), the key to a
/// file only the user can read, and a unit that runs tunnel-client with its admin listener on a private Unix socket and
/// restarts it always. The key appears nowhere but the key file, and setup only prints how to start the service.
///
/// These are the properties the security model and the service's survival depend on, so they are pinned here rather
/// than left to the deployment guide.
#[tokio::test]
async fn writes_the_profile_key_and_unit() {
    let home = Home::new();
    let output = home.setup(TUNNEL_ID, "sk-test-key\n").await;
    assert!(output.status.success(), "{}", stderr(&output));

    let profile = home.config("tunnel-client/devbox.yaml");
    assert!(profile.contains(TUNNEL_ID), "{profile}");
    let devbox_mcp = env!("CARGO_BIN_EXE_devbox-mcp");
    let command = format!("{devbox_mcp} --codex-bin {}", home.codex.display());
    assert!(profile.contains(&command), "{profile}");

    assert_eq!(
        home.config("devbox-mcp/tunnel.env"),
        "CONTROL_PLANE_API_KEY=sk-test-key\n"
    );
    assert_eq!(home.mode("devbox-mcp/tunnel.env"), 0o600);
    assert_eq!(home.mode("devbox-mcp"), 0o700);

    let unit = home.config("systemd/user/devbox-mcp.service");
    let tunnel_client = default_tunnel_client();
    let profile_dir = home.path().join(".config/tunnel-client");
    let env_file = home.path().join(".config/devbox-mcp/tunnel.env");
    for line in [
        format!("EnvironmentFile={}", env_file.display()),
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
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("sk-test-key"));
    assert!(
        stdout.contains("systemctl --user enable --now devbox-mcp.service"),
        "{stdout}"
    );
    // tunnel-client init's own advice to run it in the foreground would bypass the unit's private socket.
    assert!(!stdout.contains("tunnel-client run"), "{stdout}");
}

/// Spec (SPEC_impl.md "Deployment"): running setup again replaces its files, for example to change the tunnel ID, and
/// re-restricts a key file whose permissions were loosened in the meantime.
#[tokio::test]
async fn running_again_replaces_the_files() {
    let home = Home::new();
    assert!(home.setup(TUNNEL_ID, "first-key").await.status.success());
    let env_file = home.path().join(".config/devbox-mcp/tunnel.env");
    std::fs::set_permissions(&env_file, std::fs::Permissions::from_mode(0o644)).unwrap();

    let other_id = "tunnel_ffffffffffffffffffffffffffffffff";
    let output = home.setup(other_id, "second-key").await;
    assert!(output.status.success(), "{}", stderr(&output));
    let profile = home.config("tunnel-client/devbox.yaml");
    assert!(
        profile.contains(other_id) && !profile.contains(TUNNEL_ID),
        "{profile}"
    );
    assert_eq!(
        home.config("devbox-mcp/tunnel.env"),
        "CONTROL_PLANE_API_KEY=second-key\n"
    );
    assert_eq!(home.mode("devbox-mcp/tunnel.env"), 0o600);
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
        let env_dir = home.path().join(".config/devbox-mcp");
        std::fs::create_dir_all(&env_dir).unwrap();
        let other = home.path().join("other-file");
        std::fs::write(&other, "unrelated\n").unwrap();
        let env_file = env_dir.join("tunnel.env");
        if link == "symlink" {
            std::os::unix::fs::symlink(&other, &env_file).unwrap();
        } else {
            std::fs::hard_link(&other, &env_file).unwrap();
        }

        let output = home.setup(TUNNEL_ID, "secret-key").await;
        assert!(output.status.success(), "{link}: {}", stderr(&output));
        assert_eq!(
            std::fs::read_to_string(&other).unwrap(),
            "unrelated\n",
            "{link}"
        );
        let meta = std::fs::symlink_metadata(&env_file).unwrap();
        assert!(meta.is_file(), "{link}: key file is not a regular file");
        assert_eq!(
            home.config("devbox-mcp/tunnel.env"),
            "CONTROL_PLANE_API_KEY=secret-key\n"
        );
        assert_eq!(home.mode("devbox-mcp/tunnel.env"), 0o600, "{link}");
    }
}

/// Spec (SPEC_impl.md "Deployment"): a path that would need escaping in the unit, a missing or malformed key, a tunnel
/// ID tunnel-client rejects, and a Codex that is not executable all fail without writing anything.
///
/// Refusing instead of escaping is a deliberate simplification, so it must at least be reliable: a half-written setup
/// would leave a unit pointing at files that do not match.
#[tokio::test]
async fn refuses_bad_input_without_writing_anything() {
    let home = Home::new();
    let spaced = home.path().join("my home");
    std::fs::create_dir(&spaced).unwrap();

    for (what, output) in [
        (
            "path with a space",
            home.setup_with(TUNNEL_ID, "key", &spaced).await,
        ),
        ("empty key", home.setup(TUNNEL_ID, "\n").await),
        ("key with a quote", home.setup(TUNNEL_ID, "ab\"cd").await),
        ("bad tunnel ID", home.setup("tunnel_x", "key").await),
    ] {
        assert!(!output.status.success(), "{what}: setup succeeded");
        assert!(!stderr(&output).is_empty(), "{what}: no error message");
    }
    assert!(!home.path().join(".config").exists());
    assert!(!spaced.join(".config").exists());

    // A Codex that cannot be executed would only fail later, when the service starts a turn.
    let home = Home::new();
    std::fs::set_permissions(&home.codex, std::fs::Permissions::from_mode(0o644)).unwrap();
    let output = home.setup(TUNNEL_ID, "key").await;
    assert!(
        !output.status.success(),
        "non-executable codex: setup succeeded"
    );
    assert!(!home.path().join(".config").exists());
}
