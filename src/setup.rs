//! `devbox-mcp setup` and `devbox-mcp set-key`: configure this box to serve devbox-mcp through the OpenAI tunnel, as a
//! systemd user service (SPEC_impl.md, "Deployment").
//!
//! Everything specific to one deployment (the tunnel ID and the runtime key) is given here, on the box, so none of it is
//! in the repository. The key has a command of its own, `set-key`, so that it is pasted once per box (and again only to
//! replace it), while `setup` can be rerun freely, for example after changing the tunnel ID or the PATH. `set-key`
//! writes an environment file holding the key and nothing else. `setup` writes tunnel-client's profile (by running
//! `tunnel-client init`, so devbox-mcp never writes tunnel-client's YAML) and the unit, and never touches the key.
//! Neither runs anything else: enabling and starting the service is left to the user, who gets the commands printed,
//! because those change what runs on the box from then on.
//!
//! Paths go into a unit file and a command line unescaped, so any path that would need escaping is refused instead.
//! Escaping correctly for both systemd and tunnel-client's command splitting is easy to get subtly wrong, and refusing
//! costs a deployment nothing it needs.

use std::fs::{self, OpenOptions, Permissions};
use std::io::{IsTerminal as _, Read as _, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, bail};

/// The tunnel-client profile name setup creates and the unit runs.
const PROFILE: &str = "devbox";

/// The unit's name, under `~/.config/systemd/user/`.
const UNIT_NAME: &str = "devbox-mcp.service";

/// Flags of `devbox-mcp setup`.
#[derive(Debug, clap::Args)]
pub struct SetupArgs {
    /// The tunnel's ID, as shown on the tunnel management page (`tunnel_` followed by 32 lowercase letters or digits).
    /// The tunnel's runtime key is given separately, with `devbox-mcp set-key`.
    #[arg(long)]
    tunnel_id: String,

    /// The Codex executable the service runs. Defaults to `codex` as found on PATH now, because the service's own PATH
    /// is minimal and usually lacks the directory it is in.
    #[arg(long)]
    codex_bin: Option<PathBuf>,

    /// The Claude Code executable the service runs. Defaults to `claude` as found on PATH now, for the same reason; if
    /// it is not there, the service is set up without it and Claude sessions use whatever `claude` the service's PATH
    /// holds, so a box without Claude Code can still serve Codex sessions.
    #[arg(long)]
    claude_bin: Option<PathBuf>,

    /// The tunnel-client executable the service runs. Defaults to ~/.local/bin/tunnel-client.
    #[arg(long)]
    tunnel_client: Option<PathBuf>,

    /// Set up in this home directory instead of the current user's. For tests.
    #[arg(long, hide = true)]
    home: Option<PathBuf>,
}

/// Flags of `devbox-mcp set-key`.
#[derive(Debug, clap::Args)]
pub struct SetKeyArgs {
    /// Write the key in this home directory instead of the current user's. For tests.
    #[arg(long, hide = true)]
    home: Option<PathBuf>,
}

/// The file holding the runtime key, which the unit loads. Only `set_key` writes it.
fn key_file(home: &Path) -> PathBuf {
    home.join(".config/devbox-mcp/tunnel.env")
}

/// The home directory to work in: the hidden `--home` flag's, or the current user's.
fn home_dir(home: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    match home {
        Some(home) => Ok(home),
        None => std::env::home_dir().context("cannot determine home directory"),
    }
}

/// Runs set-key: reads the runtime key from stdin, never from the command line, where other processes could see it,
/// and writes it to the key file, replacing any earlier key.
pub fn set_key(args: SetKeyArgs) -> anyhow::Result<()> {
    let home = home_dir(args.home)?;
    let key = read_key()?;
    let key_file = key_file(&home);
    let dir = key_file
        .parent()
        .expect("the key file is inside a directory");
    create_private_dir(dir)?;
    write_private(&key_file, &format!("CONTROL_PLANE_API_KEY={key}\n"))?;
    println!(
        "Wrote the runtime key to {}. If the service is already running, restart it to use the new key:\n\
         \n\
         \x20   systemctl --user restart {UNIT_NAME}",
        key_file.display()
    );
    Ok(())
}

/// Runs setup: validates everything first, so a refused setup has changed nothing, then writes the profile and the
/// unit, in that order. The key file is left alone; the unit only refers to it.
pub fn run(args: SetupArgs) -> anyhow::Result<()> {
    let home = home_dir(args.home)?;
    // This binary is what tunnel-client will start, so it must be the installed copy, not a build directory's.
    let devbox_mcp = std::env::current_exe().context("cannot determine devbox-mcp's own path")?;
    let codex = match args.codex_bin {
        Some(path) => path,
        None => find_on_path("codex").context("codex is not on PATH; pass --codex-bin")?,
    };
    let claude = args.claude_bin.or_else(|| find_on_path("claude"));
    let tunnel_client = args
        .tunnel_client
        .unwrap_or_else(|| home.join(".local/bin/tunnel-client"));

    let config = home.join(".config");
    let profile_dir = config.join("tunnel-client");
    let env_file = key_file(&home);
    let unit_dir = config.join("systemd/user");
    let mut executables = vec![
        ("devbox-mcp", &devbox_mcp),
        ("codex", &codex),
        ("tunnel-client", &tunnel_client),
    ];
    if let Some(claude) = &claude {
        executables.push(("claude", claude));
    }
    for (what, path) in executables.iter().copied().chain([("home", &home)]) {
        plain_path(what, path)?;
    }
    for (what, path) in executables {
        if !is_executable(path) {
            bail!("{what} at {} is missing or not executable", path.display());
        }
    }

    let (path, skipped) = service_path(std::env::var_os("PATH").as_deref());
    for entry in &skipped {
        eprintln!(
            "devbox-mcp setup: leaving {entry:?} out of the service's PATH (relative, or it would need escaping in the \
             unit)"
        );
    }

    let mut mcp_command = format!("{} --codex-bin {}", devbox_mcp.display(), codex.display());
    if let Some(claude) = &claude {
        mcp_command.push_str(&format!(" --claude-bin {}", claude.display()));
    }
    // init's own stdout is kept from the user: it advises running `tunnel-client run` in the foreground, which would
    // serve the admin API on loopback TCP (the profile's default) instead of the unit's private socket (`SEC-3`). It is
    // shown only if init fails, since its error messages may be there.
    let output = Command::new(&tunnel_client)
        .args(["init", "--force", "--profile", PROFILE, "--profile-dir"])
        .arg(&profile_dir)
        .args([
            "--tunnel-id",
            &args.tunnel_id,
            "--mcp-command",
            &mcp_command,
        ])
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("cannot run {}", tunnel_client.display()))?;
    if !output.status.success() {
        bail!(
            "tunnel-client init failed ({}); nothing else was changed. Its output:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout).trim_end()
        );
    }

    fs::create_dir_all(&unit_dir)
        .with_context(|| format!("cannot create {}", unit_dir.display()))?;
    let unit = unit_dir.join(UNIT_NAME);
    fs::write(
        &unit,
        unit_text(&tunnel_client, &profile_dir, &env_file, path.as_deref()),
    )
    .with_context(|| format!("cannot write {}", unit.display()))?;

    // Without a key the service would fail to start, so the first step depends on whether one is there yet.
    let first_step = if env_file.exists() {
        String::new()
    } else {
        "There is no runtime key yet. Give it first, on stdin (see docs/deployment.md):\n\
         \n\
         \x20   devbox-mcp set-key\n\
         \n"
        .to_owned()
    };
    println!(
        "Wrote the tunnel-client profile {PROFILE:?} in {} and {}.\n\
         \n\
         The agents' commands get this shell's PATH. To change it, run setup again from a shell with the PATH you \
         want.\n\
         \n\
         {first_step}\
         To start the service, now and at every boot:\n\
         \n\
         \x20   loginctl enable-linger\n\
         \x20   systemctl --user daemon-reload\n\
         \x20   systemctl --user enable --now {UNIT_NAME}\n\
         \n\
         If it is already running, restart it instead to pick up the change:\n\
         \n\
         \x20   systemctl --user daemon-reload\n\
         \x20   systemctl --user restart {UNIT_NAME}",
        profile_dir.display(),
        unit.display()
    );
    Ok(())
}

/// The unit, from one fixed text with the paths and the PATH filled in.
///
/// The health and admin listener goes on a Unix socket in a runtime directory only this user can enter: tunnel-client's
/// admin API can start Codex with full access, so on its default loopback TCP port any local user could use it
/// (SPEC.md `SEC-3`). `Restart=always` because tunnel-client exits with status 0 when devbox-mcp crashes. The rest is
/// explained in SPEC_impl.md, "Deployment".
fn unit_text(
    tunnel_client: &Path,
    profile_dir: &Path,
    env_file: &Path,
    path: Option<&str>,
) -> String {
    let environment = path.map_or(String::new(), |path| format!("Environment=PATH={path}\n"));
    format!(
        "\
# Written by `devbox-mcp setup`; running it again replaces this file.
[Unit]
Description=devbox-mcp, reached through the OpenAI tunnel

[Service]
{environment}EnvironmentFile={env_file}
ExecStart={tunnel_client} run --profile {PROFILE} --profile-dir {profile_dir} --health.unix-socket %t/devbox-mcp/tunnel-client.sock
RuntimeDirectory=devbox-mcp
RuntimeDirectoryMode=0700
Restart=always
RestartSec=5
KillMode=control-group
TimeoutStopSec=15

[Install]
WantedBy=default.target
",
        env_file = env_file.display(),
        tunnel_client = tunnel_client.display(),
        profile_dir = profile_dir.display(),
    )
}

/// Refuses a path that is relative or would need escaping in a unit file or in tunnel-client's MCP command line:
/// whitespace, quotes, backslashes, `%` (a systemd specifier), and `$` (systemd variable expansion).
fn plain_path(what: &str, path: &Path) -> anyhow::Result<()> {
    let Some(text) = path.to_str() else {
        bail!("the {what} path {} is not valid UTF-8", path.display());
    };
    if !path.is_absolute() {
        bail!("the {what} path {text:?} must be absolute");
    }
    if needs_escaping(text) {
        bail!(
            "the {what} path {text:?} contains a character that would need escaping in a unit file (whitespace, a \
             quote, a backslash, '%', or '$'); use a path without one"
        );
    }
    Ok(())
}

/// Whether text would need escaping in a unit file or in tunnel-client's MCP command line: whitespace, quotes,
/// backslashes, `%` (a systemd specifier), and `$` (systemd variable expansion).
fn needs_escaping(text: &str) -> bool {
    text.chars()
        .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '\'' | '\\' | '%' | '$'))
}

/// The PATH the service runs with, taken from the PATH setup runs with, so that the agents' commands find the same
/// tools as the user's own shell. Without it the service gets the systemd user manager's minimal PATH, which usually
/// lacks directories such as `~/.local/bin` or `~/.cargo/bin`.
///
/// Entries that are relative or empty (both mean "the current directory", which a service must not depend on), not
/// UTF-8, or that would need escaping in the unit are left out and returned separately, so setup can say so. Returns
/// no PATH when nothing usable is left, in which case the unit sets none and the manager's default applies.
fn service_path(path: Option<&std::ffi::OsStr>) -> (Option<String>, Vec<String>) {
    let mut kept = Vec::new();
    let mut skipped = Vec::new();
    for entry in path.map(std::env::split_paths).into_iter().flatten() {
        if entry.as_os_str().is_empty() {
            // An empty entry, as in `a::b`, means the current directory too, but is too common to warn about.
            continue;
        }
        match entry.to_str() {
            Some(text) if entry.is_absolute() && !needs_escaping(text) => {
                kept.push(text.to_owned())
            }
            _ => skipped.push(entry.to_string_lossy().into_owned()),
        }
    }
    let path = (!kept.is_empty()).then(|| kept.join(":"));
    (path, skipped)
}

/// Reads the runtime key from stdin: one line, of printable ASCII without spaces, quotes, or backslashes, so it can go
/// into an environment file unquoted.
fn read_key() -> anyhow::Result<String> {
    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        eprintln!("Paste the tunnel's runtime API key, then press Enter and Ctrl-D:");
    }
    let mut input = String::new();
    stdin
        .read_to_string(&mut input)
        .context("cannot read the runtime key from stdin")?;
    let key = input.trim();
    if key.is_empty() {
        bail!("no runtime key on stdin; pipe it in, for example from a file");
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_graphic() && !matches!(c, '"' | '\'' | '\\'))
    {
        bail!(
            "the runtime key on stdin is not one word of printable ASCII without quotes or backslashes"
        );
    }
    Ok(key.to_owned())
}

/// Creates a directory readable only by the user, or tightens an existing one.
fn create_private_dir(dir: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    fs::set_permissions(dir, Permissions::from_mode(0o700))
        .with_context(|| format!("cannot restrict {}", dir.display()))
}

/// Writes a new file readable only by the user, replacing whatever was at `path`.
///
/// The old entry is removed rather than opened, so the contents never go through a symlink or hardlink someone left
/// there into another file, and a file whose mode was loosened is not reused. The new file is created exclusively with
/// mode 0600, so it is never readable by others, not even briefly.
fn write_private(path: &Path, contents: &str) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("cannot replace {}", path.display())),
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("cannot write {}", path.display()))
}

/// Whether `path` is a regular file (after following symlinks) that someone may execute.
fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Finds an executable on PATH, as the shell would, returning its absolute path.
fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|path| is_executable(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The service's PATH keeps the user's absolute entries in order and drops what a unit cannot carry unescaped or a
    /// service must not depend on, reporting all but empty entries.
    #[test]
    fn service_path_keeps_only_plain_absolute_entries() {
        let (path, skipped) = service_path(Some("/a/bin::bin:/b c/bin:/d%/bin:/usr/bin".as_ref()));
        assert_eq!(path.as_deref(), Some("/a/bin:/usr/bin"));
        assert_eq!(skipped, ["bin", "/b c/bin", "/d%/bin"]);
    }

    /// With nothing usable, the unit sets no PATH at all rather than an empty one, which would leave the service unable
    /// to find anything.
    #[test]
    fn service_path_is_none_without_usable_entries() {
        assert_eq!(service_path(None), (None, vec![]));
        assert_eq!(service_path(Some("rel:".as_ref())).0, None);
    }
}
