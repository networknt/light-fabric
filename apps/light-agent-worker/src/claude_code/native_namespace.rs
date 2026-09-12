//! Native state is private to one workflow conversation. Only selected installed
//! configuration and the existing credential file are mounted into its namespace.
//! No credentials are copied, and no other native history is exposed.
use super::*;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

pub(super) fn wrap(
    command: &mut Command,
    host: &HostContext,
    spec: &CodingTurnSpec,
    repository: &Path,
    metadata: &Path,
    scratch: &Path,
    writable: &[PathBuf],
    source: PermissionSource,
) -> Result<()> {
    for settings in [
        host.native_home.join("settings.json"),
        repository.join(".claude/settings.json"),
        repository.join(".claude/settings.local.json"),
        PathBuf::from("/etc/claude-code/managed-settings.json"),
    ] {
        reject_provider_overrides(&settings)?;
    }
    let launcher = Path::new("/usr/bin/bwrap");
    let meta = std::fs::symlink_metadata(launcher).context("Claude requires bubblewrap")?;
    ensure!(
        meta.is_file()
            && !meta.file_type().is_symlink()
            && meta.uid() == 0
            && meta.permissions().mode() & 0o022 == 0,
        "bubblewrap must be a root-owned non-writable regular executable"
    );
    let control = spec
        .thread
        .as_ref()
        .context("Claude namespace requires a conversation")?;
    let key = agent_runtime_protocol::canonical_digest(
        &json!({"home":host.native_home,"scope":host.thread_scope,"session":control.session_ref,"role":spec.role}),
    )?;
    let directory = host
        .native_home
        .parent()
        .context("native home needs a parent")?
        .join(format!(
            ".light-claude-native-{}",
            key.trim_start_matches("sha256:")
        ));
    private_directory(&directory)?;
    let native_state = directory.join("config");
    private_directory(&native_state)?;
    let cli_home = directory.join("home");
    private_directory(&cli_home)?;
    let credentials = host.native_home.join(".credentials.json");
    let credential_meta = std::fs::symlink_metadata(&credentials)
        .context("Claude local worker requires the native file-backed login store")?;
    ensure!(
        credential_meta.is_file()
            && !credential_meta.file_type().is_symlink()
            && credential_meta.uid() == unsafe { libc::geteuid() },
        "unsafe native credential store"
    );

    let home_path = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| host.native_home.parent().unwrap().to_owned());
    ensure!(
        home_path.is_absolute() && home_path != Path::new("/"),
        "invalid native HOME"
    );
    let config_path = &host.native_home;
    let original = command.as_std();
    let args = original
        .get_args()
        .map(|s| s.to_owned())
        .collect::<Vec<_>>();
    let env = original
        .get_envs()
        .filter_map(|(k, v)| v.map(|v| (k.to_owned(), v.to_owned())))
        .collect::<Vec<_>>();
    let cwd = original
        .get_current_dir()
        .context("Claude cwd missing")?
        .to_owned();
    let mut isolated = Command::new(launcher);
    isolated
        .env_clear()
        .envs(env)
        .env("HOME", &home_path)
        .env("CLAUDE_CONFIG_DIR", config_path)
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("XDG_CACHE_HOME", home_path.join(".cache"))
        .env("XDG_CONFIG_HOME", home_path.join(".config"))
        .env("XDG_DATA_HOME", home_path.join(".local/share"))
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env_remove("XDG_RUNTIME_DIR")
        .args([
            "--die-with-parent",
            "--new-session",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
        ]);
    // A sparse filesystem: no host /home, /root, /var or /tmp is mounted.
    for path in ["/usr", "/etc"] {
        isolated.arg("--ro-bind").arg(path).arg(path);
    }
    for path in ["/bin", "/sbin", "/lib", "/lib64"] {
        let path = Path::new(path);
        if path.is_symlink() {
            isolated
                .arg("--symlink")
                .arg(std::fs::read_link(path)?)
                .arg(path);
        } else if path.exists() {
            isolated.arg("--ro-bind").arg(path).arg(path);
        }
    }
    // Some distributions keep resolv.conf behind a symlink into /run. Expose
    // only that resolved resolver file, not the host's runtime directory.
    let resolver = std::fs::canonicalize("/etc/resolv.conf")?;
    if !resolver.starts_with("/etc") {
        isolated.arg("--ro-bind").arg(&resolver).arg(&resolver);
    }
    isolated
        .args(["--dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp"])
        .arg("--bind")
        .arg(&cli_home)
        .arg(&home_path)
        .arg("--bind")
        .arg(&native_state)
        .arg(config_path)
        .arg("--ro-bind")
        .arg(&credentials)
        .arg(config_path.join(".credentials.json"));
    // Mount the installed policy, not other session state. Ambient paths outside
    // this declared configuration surface are unavailable to native tools.
    if source == PermissionSource::ClaudeCli {
        for name in [
            "settings.json",
            "CLAUDE.md",
            "plugins",
            "skills",
            "commands",
            "agents",
            "hooks",
        ] {
            let path = host.native_home.join(name);
            if path.exists() {
                ensure!(
                    !path.is_symlink(),
                    "native configuration mount cannot be a symlink"
                );
                isolated
                    .arg("--ro-bind")
                    .arg(&path)
                    .arg(config_path.join(name));
            }
        }
        if let Some(home) = std::env::var_os("HOME") {
            let config = Path::new(&home).join(".claude.json");
            if config.is_file() {
                isolated
                    .arg("--ro-bind")
                    .arg(config)
                    .arg(home_path.join(".claude.json"));
            }
        }
    }
    isolated
        .arg("--ro-bind")
        .arg(repository)
        .arg(repository)
        .arg("--ro-bind")
        .arg(metadata)
        .arg(metadata)
        .arg("--bind")
        .arg(scratch)
        .arg(scratch);
    for root in writable {
        isolated.arg("--bind").arg(root).arg(root);
    }
    isolated
        .arg("--ro-bind")
        .arg(&host.executable)
        .arg("/opt/light-claude/claude")
        .arg("--chdir")
        .arg(cwd)
        .arg("--")
        .arg("/opt/light-claude/claude")
        .args(args);
    *command = isolated;
    Ok(())
}
fn private_directory(path: &Path) -> Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.into()),
    }
    let m = std::fs::symlink_metadata(path)?;
    ensure!(
        m.is_dir()
            && !m.file_type().is_symlink()
            && m.permissions().mode() & 0o077 == 0
            && m.uid() == unsafe { libc::geteuid() },
        "unsafe native session directory"
    );
    Ok(())
}

fn reject_provider_overrides(path: &Path) -> Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= 1024 * 1024,
        "unsafe Claude settings file"
    );
    let value: Value =
        serde_json::from_slice(&std::fs::read(path)?).context("invalid Claude settings JSON")?;
    let forbidden = [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_BASE_URL",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
    ];
    ensure!(
        value.get("apiKeyHelper").is_none()
            && value.get("awsAuthRefresh").is_none()
            && value.get("awsCredentialExport").is_none()
            && value
                .get("env")
                .and_then(Value::as_object)
                .is_none_or(|env| forbidden.iter().all(|key| !env.contains_key(*key))),
        "Claude personal configuration contains a provider credential or route override"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn personal_native_settings_cannot_inject_an_api_provider() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        for value in [
            json!({"env":{"ANTHROPIC_API_KEY":"synthetic"}}),
            json!({"apiKeyHelper":"echo synthetic"}),
            json!({"env":{"ANTHROPIC_BASE_URL":"https://example.invalid"}}),
        ] {
            std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
            assert!(reject_provider_overrides(&path).is_err());
        }
        std::fs::write(
            &path,
            b"{\"permissions\":{\"defaultMode\":\"bypassPermissions\"}}",
        )
        .unwrap();
        reject_provider_overrides(&path).unwrap();
    }
}
