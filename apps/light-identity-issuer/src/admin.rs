//! Operator commands on the durable spent-token record.
//!
//! The issuer accepts a bootstrap token once. Re-arming one (for a developer who
//! deleted their CLI's state, or after a deliberate re-bootstrap) is an explicit
//! operator action on the journal, not an HTTP endpoint anyone could reach:
//!
//! ```text
//! light-identity-issuer-service list-spent
//! light-identity-issuer-service reset-token <jti>
//! ```
//!
//! They open the journal, which takes its lock, so the service must be stopped
//! first. The token's `jti` is a claim inside the token itself.

use std::path::Path;

use light_identity_issuer::{FileSpentTokens, SpentTokenStore};

#[derive(Debug, PartialEq, Eq)]
pub enum Admin {
    ResetToken(String),
    ListSpent,
}

pub const USAGE: &str = "usage:\n  light-identity-issuer-service                 run the service\n  \
     light-identity-issuer-service list-spent      list spent bootstrap tokens\n  \
     light-identity-issuer-service reset-token <jti>  allow a spent token to enroll once more\n\
     (the two commands need the service stopped: they take the journal's lock)";

/// `Ok(None)` means no command was given: run the service.
pub fn parse(args: &[String]) -> Result<Option<Admin>, String> {
    match args {
        [] => Ok(None),
        [cmd] if cmd == "list-spent" => Ok(Some(Admin::ListSpent)),
        [cmd, jti] if cmd == "reset-token" && !jti.trim().is_empty() => {
            Ok(Some(Admin::ResetToken(jti.trim().to_string())))
        }
        _ => Err(USAGE.to_string()),
    }
}

pub fn run(admin: Admin, journal: &Path) -> Result<String, String> {
    let store = FileSpentTokens::open(journal).map_err(|e| e.to_string())?;
    match admin {
        Admin::ListSpent => {
            let spent = store.spent();
            if spent.is_empty() {
                return Ok("no spent tokens".to_string());
            }
            Ok(spent
                .iter()
                .map(|(key, at)| format!("{at}\t{key}"))
                .collect::<Vec<_>>()
                .join("\n"))
        }
        Admin::ResetToken(key) => {
            if !store.spent().iter().any(|(k, _)| k == &key) {
                return Ok(format!(
                    "token {key} is not recorded as spent; nothing to do"
                ));
            }
            store.reset(&key).map_err(|e| e.to_string())?;
            Ok(format!("token {key} re-armed: it can enroll once more"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn commands_parse_and_anything_else_is_usage() {
        assert_eq!(parse(&args(&[])).unwrap(), None);
        assert_eq!(
            parse(&args(&["list-spent"])).unwrap(),
            Some(Admin::ListSpent)
        );
        assert_eq!(
            parse(&args(&["reset-token", " abc "])).unwrap(),
            Some(Admin::ResetToken("abc".into()))
        );
        for bad in [
            &["reset-token"][..],
            &["reset-token", ""],
            &["nonsense"],
            &["list-spent", "x"],
        ] {
            assert!(parse(&args(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_spent_token_is_listed_and_can_be_re_armed() {
        let dir = TempDir::new().unwrap();
        let journal = dir.path().join("spent-tokens.jsonl");
        {
            let store = FileSpentTokens::open(&journal).unwrap();
            assert!(store.try_consume("jti-1").unwrap());
        }
        assert!(run(Admin::ListSpent, &journal).unwrap().contains("jti-1"));
        assert!(
            run(Admin::ResetToken("jti-1".into()), &journal)
                .unwrap()
                .contains("re-armed")
        );
        assert_eq!(run(Admin::ListSpent, &journal).unwrap(), "no spent tokens");
        // ...and the re-arm is durable: the token really can be consumed again.
        assert!(
            FileSpentTokens::open(&journal)
                .unwrap()
                .try_consume("jti-1")
                .unwrap()
        );
    }

    #[test]
    fn resetting_a_token_that_is_not_spent_says_so() {
        let dir = TempDir::new().unwrap();
        let journal = dir.path().join("spent-tokens.jsonl");
        assert!(
            run(Admin::ResetToken("nope".into()), &journal)
                .unwrap()
                .contains("nothing to do")
        );
    }

    #[test]
    fn the_commands_refuse_while_the_service_holds_the_journal() {
        let dir = TempDir::new().unwrap();
        let journal = dir.path().join("spent-tokens.jsonl");
        let _service = FileSpentTokens::open(&journal).unwrap();
        let error = run(Admin::ListSpent, &journal).unwrap_err();
        assert!(
            error.contains("in use by another issuer process"),
            "{error}"
        );
    }
}
