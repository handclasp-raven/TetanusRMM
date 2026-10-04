//! The technician's Bitwarden vault, through the official `bw` command
//! line client: unlock it, search it, and fetch one username, password or
//! one-time code to have typed on the remote machine.
//!
//! `bw` must be installed and signed in (`bw login`, once, outside the
//! viewer). The master password is never kept: it goes to one `bw unlock`
//! in that process's environment (not on its command line, which other
//! local users can read) and is wiped. What is kept, in memory only, is
//! the session key `bw unlock` returns, which opens the vault until it is
//! locked again; it too only ever travels in a child's environment.
//!
//! A search keeps no passwords: of each item only its name, username and
//! address, and whether it has a password and a one-time code. The value
//! to type is fetched when it is typed, and wiped once sent.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use protocol::ipc::Secret;
use protocol::vault::TextKind;
use serde::de::IgnoredAny;
use serde::Deserialize;
use zeroize::Zeroizing;

/// Opens the vault until it is locked. Wiped when dropped.
pub type SessionKey = Zeroizing<String>;

/// Most items a search shows.
pub const MAX_RESULTS: usize = 50;

/// The child's environment variable the master password is passed in.
const PASSWORD_ENV: &str = "RMM_BW_PASSWORD";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VaultError {
    #[error("Bitwarden's command line client was not found ({0}). Install `bw`, or give its path with --bw.")]
    NotInstalled(String),
    #[error("Not signed in to Bitwarden: run `bw login` once, then try again.")]
    NotLoggedIn,
    #[error("The vault is locked.")]
    Locked,
    #[error("Wrong master password.")]
    WrongPassword,
    #[error("Bitwarden: {0}")]
    Other(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// `bw login` has not been done.
    Unauthenticated,
    Locked,
    /// The session key in use opens the vault.
    Unlocked,
}

/// A login in the vault, as a search lists it. No secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub id: String,
    pub name: String,
    pub username: Option<String>,
    /// Its first address, if it has one.
    pub uri: Option<String>,
    pub has_password: bool,
    pub has_totp: bool,
}

impl Item {
    pub fn has(&self, kind: TextKind) -> bool {
        match kind {
            TextKind::Username => self.username.is_some(),
            TextKind::Password => self.has_password,
            TextKind::Totp => self.has_totp,
        }
    }
}

// What `bw list items` prints, as far as it is wanted. The password and
// the one-time code's seed are skipped, never copied out of the output.
#[derive(Deserialize)]
struct RawItem {
    id: String,
    name: String,
    login: Option<RawLogin>,
}

#[derive(Deserialize)]
struct RawLogin {
    username: Option<String>,
    password: Option<IgnoredAny>,
    totp: Option<IgnoredAny>,
    uris: Option<Vec<RawUri>>,
}

#[derive(Deserialize)]
struct RawUri {
    uri: Option<String>,
}

#[derive(Deserialize)]
struct RawStatus {
    status: String,
}

/// The `bw` client to run.
#[derive(Debug, Clone)]
pub struct Bw {
    program: PathBuf,
}

impl Bw {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
        }
    }

    /// Run `bw args...` and return what it printed, which is wiped when
    /// dropped. `env` is added to the child's environment only.
    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Result<Zeroizing<Vec<u8>>, VaultError> {
        let mut command = Command::new(&self.program);
        command
            .args(args)
            // Never wait at a prompt nobody can see.
            .env("BW_NOINTERACTION", "true")
            .env_remove("BW_SESSION")
            .envs(env.iter().copied())
            .stdin(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW: no console flashing up over the viewer.
            command.creation_flags(0x0800_0000);
        }
        let output = command.output().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                VaultError::NotInstalled(self.program.display().to_string())
            }
            _ => VaultError::Other(format!("cannot run {}: {e}", self.program.display())),
        })?;
        let stdout = Zeroizing::new(output.stdout);
        if output.status.success() {
            return Ok(stdout);
        }
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(classify(&message))
    }

    fn session(session: Option<&SessionKey>) -> Vec<(&'static str, &str)> {
        session
            .map(|key| ("BW_SESSION", key.as_str()))
            .into_iter()
            .collect()
    }

    /// Whether the vault is signed in, and whether `session` opens it.
    pub fn status(&self, session: Option<&SessionKey>) -> Result<Status, VaultError> {
        let out = self.run(&["status"], &Self::session(session))?;
        let raw: RawStatus = serde_json::from_slice(&out)
            .map_err(|e| VaultError::Other(format!("unexpected `bw status` output: {e}")))?;
        Ok(match raw.status.as_str() {
            "unlocked" => Status::Unlocked,
            "locked" => Status::Locked,
            _ => Status::Unauthenticated,
        })
    }

    /// Unlock with the master password, which is the caller's to wipe.
    pub fn unlock(&self, master_password: &str) -> Result<SessionKey, VaultError> {
        let out = self.run(
            &["unlock", "--raw", "--passwordenv", PASSWORD_ENV],
            &[(PASSWORD_ENV, master_password)],
        )?;
        let key = std::str::from_utf8(&out).unwrap_or_default().trim();
        if key.is_empty() {
            return Err(VaultError::Other("`bw unlock` gave no session key".into()));
        }
        Ok(Zeroizing::new(key.to_owned()))
    }

    /// The logins matching `query`, by name; at most [`MAX_RESULTS`].
    pub fn search(&self, session: &SessionKey, query: &str) -> Result<Vec<Item>, VaultError> {
        // One argument, so a query starting with a dash is not an option.
        let search = format!("--search={query}");
        let out = self.run(&["list", "items", &search], &Self::session(Some(session)))?;
        parse_items(&out)
    }

    /// The value of `kind` for item `id`, to type.
    pub fn fetch(
        &self,
        session: &SessionKey,
        id: &str,
        kind: TextKind,
    ) -> Result<Secret, VaultError> {
        let out = self.run(&["get", kind.as_str(), id], &Self::session(Some(session)))?;
        let text = std::str::from_utf8(&out)
            .map_err(|_| VaultError::Other("the value is not text".into()))?
            .trim_end_matches(['\r', '\n']);
        if text.is_empty() {
            return Err(VaultError::Other(format!("this item has no {kind}")));
        }
        Ok(Secret::new(text.encode_utf16().collect()))
    }

    /// Fetch the latest vault from the server.
    pub fn sync(&self, session: &SessionKey) -> Result<(), VaultError> {
        self.run(&["sync"], &Self::session(Some(session))).map(drop)
    }

    /// Lock the vault: every session key stops working, this one and any
    /// another program holds.
    pub fn lock(&self) -> Result<(), VaultError> {
        self.run(&["lock"], &[]).map(drop)
    }
}

/// What `bw` meant by `message` on its standard error.
fn classify(message: &str) -> VaultError {
    let lower = message.to_lowercase();
    if lower.contains("not logged in") {
        VaultError::NotLoggedIn
    } else if lower.contains("invalid master password") {
        VaultError::WrongPassword
    } else if lower.contains("vault is locked") || lower.contains("master password") {
        // Asking for the master password (which it may not, here) means
        // the session key did not open the vault.
        VaultError::Locked
    } else if message.is_empty() {
        VaultError::Other("the command failed".into())
    } else {
        // The last line: earlier ones are progress and warnings.
        let line = message.lines().last().unwrap_or(message).trim();
        VaultError::Other(line.chars().take(200).collect())
    }
}

fn parse_items(json: &[u8]) -> Result<Vec<Item>, VaultError> {
    let raw: Vec<RawItem> = serde_json::from_slice(json)
        .map_err(|e| VaultError::Other(format!("unexpected `bw list` output: {e}")))?;
    let mut items: Vec<Item> = raw
        .into_iter()
        .filter_map(|item| {
            let login = item.login?;
            Some(Item {
                id: item.id,
                name: item.name,
                username: login.username.filter(|u| !u.is_empty()),
                uri: login
                    .uris
                    .and_then(|uris| uris.into_iter().find_map(|u| u.uri)),
                has_password: login.password.is_some(),
                has_totp: login.totp.is_some(),
            })
        })
        .collect();
    items.sort_by_key(|item| item.name.to_lowercase());
    items.truncate(MAX_RESULTS);
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = r#"[
      {"id":"b","name":"zeta wifi","type":2,"login":null,"notes":"n0te-s3cret"},
      {"id":"a2","name":"Contoso VPN","type":1,
       "login":{"username":"","password":null,"totp":"otpauth://totp/x?secret=JBSWY3DP","uris":[]}},
      {"id":"a1","name":"contoso admin","type":1,
       "login":{"username":"CORP\\admin","password":"hunter2-s3cret","totp":null,
                "uris":[{"match":null,"uri":"https://dc.contoso.example"}]}}
    ]"#;

    #[test]
    fn a_search_keeps_logins_by_name_and_no_secrets() {
        let items = parse_items(LIST.as_bytes()).unwrap();
        assert_eq!(
            items,
            [
                Item {
                    id: "a1".into(),
                    name: "contoso admin".into(),
                    username: Some("CORP\\admin".into()),
                    uri: Some("https://dc.contoso.example".into()),
                    has_password: true,
                    has_totp: false,
                },
                Item {
                    id: "a2".into(),
                    name: "Contoso VPN".into(),
                    username: None,
                    uri: None,
                    has_password: false,
                    has_totp: true,
                },
            ]
        );
        let shown = format!("{items:?}");
        assert!(!shown.contains("s3cret") && !shown.contains("JBSWY3DP"));
        assert!(items[0].has(TextKind::Username) && items[0].has(TextKind::Password));
        assert!(!items[0].has(TextKind::Totp) && !items[1].has(TextKind::Username));
    }

    #[test]
    fn long_result_lists_are_cut() {
        let many: Vec<String> = (0..MAX_RESULTS + 20)
            .map(|i| format!(r#"{{"id":"{i}","name":"n{i:03}","login":{{}}}}"#))
            .collect();
        let items = parse_items(format!("[{}]", many.join(",")).as_bytes()).unwrap();
        assert_eq!(items.len(), MAX_RESULTS);
        assert_eq!(items[0].name, "n000");
    }

    #[test]
    fn bw_errors_are_told_apart() {
        assert_eq!(classify("You are not logged in."), VaultError::NotLoggedIn);
        assert_eq!(
            classify("Invalid master password."),
            VaultError::WrongPassword
        );
        assert_eq!(classify("Vault is locked."), VaultError::Locked);
        assert_eq!(
            classify("? Master password: [input is hidden]"),
            VaultError::Locked
        );
        assert_eq!(
            classify("warning\nNot found."),
            VaultError::Other("Not found.".into())
        );
    }

    #[test]
    fn a_missing_client_says_so() {
        let bw = Bw::new("/nonexistent/rmm-test-bw");
        assert!(matches!(bw.status(None), Err(VaultError::NotInstalled(_))));
    }

    /// A stand-in `bw`: a shell script that records how it was called and
    /// answers like the real one.
    #[cfg(unix)]
    mod fake {
        use super::super::*;
        use std::os::unix::fs::PermissionsExt;
        use std::path::Path;

        const SCRIPT: &str = r#"#!/bin/sh
dir=$(dirname "$0")
echo "$*" >> "$dir/argv"
[ "$BW_NOINTERACTION" = true ] || { echo "would prompt" >&2; exit 1; }
case "$1" in
  status)
    if [ "$BW_SESSION" = KEY ]; then echo '{"status":"unlocked","userEmail":"t@example.com"}'
    else echo '{"status":"locked"}'; fi ;;
  unlock)
    [ "$2 $3 $4" = "--raw --passwordenv RMM_BW_PASSWORD" ] || exit 2
    if [ "$RMM_BW_PASSWORD" = "correct horse" ]; then printf KEY
    else echo "Invalid master password." >&2; exit 1; fi ;;
  lock) echo "Your vault is locked." ;;
  *)
    [ "$BW_SESSION" = KEY ] || { echo "Vault is locked." >&2; exit 1; }
    case "$1 $2" in
      "list items") echo "[{\"id\":\"a1\",\"name\":\"$3\",\"login\":{\"username\":\"u\",\"password\":\"p\"}}]" ;;
      "get password") echo "pässword $3" ;;
      "get totp") echo 123456 ;;
      sync*) echo "Syncing complete." ;;
      *) echo "Not found." >&2; exit 1 ;;
    esac ;;
esac
"#;

        fn install() -> (tempdir::Dir, Bw) {
            let dir = tempdir::Dir::new();
            let path = dir.path().join("bw");
            std::fs::write(&path, SCRIPT).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            (dir, Bw::new(path))
        }

        fn argv(dir: &Path) -> String {
            std::fs::read_to_string(dir.join("argv")).unwrap_or_default()
        }

        /// A directory of its own, removed afterwards.
        mod tempdir {
            use std::path::{Path, PathBuf};
            use std::sync::atomic::{AtomicU32, Ordering};

            static NEXT: AtomicU32 = AtomicU32::new(0);

            pub struct Dir(PathBuf);

            impl Dir {
                pub fn new() -> Self {
                    let path = std::env::temp_dir().join(format!(
                        "rmm-bw-{}-{}",
                        std::process::id(),
                        NEXT.fetch_add(1, Ordering::Relaxed)
                    ));
                    std::fs::create_dir_all(&path).unwrap();
                    Self(path)
                }

                pub fn path(&self) -> &Path {
                    &self.0
                }
            }

            impl Drop for Dir {
                fn drop(&mut self) {
                    let _ = std::fs::remove_dir_all(&self.0);
                }
            }
        }

        #[test]
        fn unlocking_searching_and_fetching() {
            let (dir, bw) = install();
            assert_eq!(bw.status(None).unwrap(), Status::Locked);
            assert_eq!(bw.unlock("wrong"), Err(VaultError::WrongPassword));
            let key = bw.unlock("correct horse").unwrap();
            assert_eq!(key.as_str(), "KEY");
            assert_eq!(bw.status(Some(&key)).unwrap(), Status::Unlocked);

            let items = bw.search(&key, "-contoso admin").unwrap();
            assert_eq!(items[0].name, "--search=-contoso admin");
            assert!(items[0].has_password);

            let password = bw.fetch(&key, "a1", TextKind::Password).unwrap();
            let expected: Vec<u16> = "pässword a1".encode_utf16().collect();
            assert_eq!(password.units(), expected);
            let code = bw.fetch(&key, "a1", TextKind::Totp).unwrap();
            assert_eq!(code.units(), "123456".encode_utf16().collect::<Vec<_>>());
            bw.sync(&key).unwrap();
            bw.lock().unwrap();

            // Neither the master password nor the session key was ever on
            // a command line.
            let argv = argv(dir.path());
            assert!(argv.contains("unlock --raw --passwordenv RMM_BW_PASSWORD"));
            assert!(
                !argv.contains("correct horse") && !argv.contains("KEY"),
                "{argv}"
            );
        }

        #[test]
        fn a_stale_session_key_reads_as_locked() {
            let (_dir, bw) = install();
            let stale = Zeroizing::new("OLD".to_owned());
            assert_eq!(bw.status(Some(&stale)).unwrap(), Status::Locked);
            assert_eq!(bw.search(&stale, "x"), Err(VaultError::Locked));
            assert_eq!(
                bw.fetch(&stale, "a1", TextKind::Password).map(drop),
                Err(VaultError::Locked)
            );
        }
    }
}
