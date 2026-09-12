//! Workspace data identity is independent of each adapter's conversation identity.
use crate::coding_session::CodingSession;
use anyhow::{Context, Result, ensure};
use coding_agent_runtime::{CodingThreadControl, CodingThreadMode};
use serde_json::{Value, json};
use std::{
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
};
use workspace_execution_protocol::WorkspaceExecutionSpec;

pub(crate) struct Conversation {
    pub session: CodingSession,
    pub control: CodingThreadControl,
    pub home: PathBuf,
    ephemeral: bool,
}
impl Conversation {
    pub fn open(
        store: &Path,
        spec: &WorkspaceExecutionSpec,
        task: &str,
        contract: &Value,
    ) -> Result<Self> {
        let control = spec.request.thread.clone().unwrap_or(CodingThreadControl {
            runner_id: spec.binding.runner_id.clone(),
            session_ref: uuid::Uuid::new_v4(),
            stage_id: "standalone".into(),
            mode: CodingThreadMode::New,
            expected_checkpoint: None,
            close_after_turn: true,
        });
        ensure!(
            control.runner_id == spec.binding.runner_id,
            "conversation belongs to a different runner"
        );
        let scope = agent_runtime_protocol::canonical_digest(
            &json!({"workspace":spec.binding.workspace_id,
            "host":spec.binding.host_id,"environment":spec.binding.environment,"subject":spec.subject,"agent":spec.agent_id}),
        )?;
        let root = store.join("native-conversations");
        private_dir(&root)?;
        let session = CodingSession::open_bound(
            &root,
            &scope,
            &control,
            json!({
            "scope":scope,"task":task,"intent":spec.request.intent,"binding":spec.binding,
            "stage":control.stage_id,"runner":control.runner_id,"contract":contract}),
        )?;
        let key = agent_runtime_protocol::canonical_digest(&json!([scope, control.session_ref]))?;
        let home = root.join(key.trim_start_matches("sha256:"));
        private_dir(&home)?;
        Ok(Self {
            session,
            control,
            home,
            ephemeral: spec.request.thread.is_none(),
        })
    }
    /// Acquire task authority before advancing private native history.
    pub fn begin_workspace(
        &mut self,
        store: &task_workspace::WorkspaceStore,
        spec: &WorkspaceExecutionSpec,
        task: &str,
    ) -> Result<task_workspace::WorkspaceToolSession> {
        let tools = store.begin_tool_session(
            &spec.request.workspace_id,
            task,
            &spec.agent_id,
            spec.request.intent == workspace_execution_protocol::WorkspaceIntent::Implement,
            spec.request.expected_checkpoint_digest.as_deref(),
        )?;
        if let Err(error) = self.session.begin() {
            tools.finish()?;
            return Err(error);
        }
        Ok(tools)
    }

    pub fn finish(&mut self, native_id: &str) -> Result<Value> {
        let receipt = self
            .session
            .finish(native_id, "", self.control.close_after_turn)?;
        // Unaddressable standalone histories cannot be resumed. Remove only after
        // confirmed completion while retaining the locked CLOSED receipt.
        if self.ephemeral {
            if let Err(_error) = std::fs::remove_dir_all(&self.home) {
                eprintln!(
                    "completed standalone native home cleanup failed; inspect workspace storage"
                );
            }
        }
        Ok(receipt)
    }
}
/// Presentation limits must not turn a confirmed native completion into uncertainty.
pub(crate) fn public_answer(answer: Option<&str>) -> String {
    let answer = answer
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("Turn completed without a text response.");
    let mut end = answer.len().min(64 * 1024);
    while !answer.is_char_boundary(end) {
        end -= 1;
    }
    answer[..end].to_owned()
}

fn private_dir(path: &Path) -> Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.into()),
    }
    let meta = std::fs::symlink_metadata(path).context("conversation directory")?;
    ensure!(
        meta.is_dir() && meta.permissions().mode() & 0o077 == 0,
        "conversation directory is not private"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use workspace_execution_protocol::*;
    fn spec() -> WorkspaceExecutionSpec {
        serde_json::from_value(json!({"request":{"schemaVersion":1,"requestId":"job","workspaceId":"shared",
            "expectedMembershipRevision":sha256(b"members"),"task":{"kind":"existing","taskId":"task"},
            "intent":"implement","instruction":"edit","expectedCheckpointDigest":null,
            "thread":{"runnerId":"runner","sessionRef":uuid::Uuid::new_v4(),"stageId":"implement","mode":"new","closeAfterTurn":false}},
            "binding":{"schemaVersion":1,"workspaceId":"shared","hostId":"host","environment":"dev","runnerId":"runner",
            "membershipRevision":sha256(b"members"),"authorizationRevision":1,"subjects":["owner"],"agents":["agent"],"intents":["implement","review"]},
            "subject":"owner","agentId":"agent"})).unwrap()
    }
    #[test]
    fn conversation_resume_close_and_task_role_binding() {
        let root = tempfile::tempdir().unwrap();
        let mut spec = spec();
        let contract = json!({"adapter":"test"});
        let mut first = Conversation::open(root.path(), &spec, "task", &contract).unwrap();
        first.session.begin().unwrap();
        let receipt = first.finish("native").unwrap();
        drop(first);
        let control = spec.request.thread.as_mut().unwrap();
        control.mode = CodingThreadMode::Resume;
        control.expected_checkpoint =
            Some(serde_json::from_value(receipt["checkpoint"].clone()).unwrap());
        assert!(Conversation::open(root.path(), &spec, "other-task", &contract).is_err());
        let mut review = spec.clone();
        review.request.intent = WorkspaceIntent::Review;
        assert!(Conversation::open(root.path(), &review, "task", &contract).is_err());
        let resumed = Conversation::open(root.path(), &spec, "task", &contract).unwrap();
        assert_eq!(resumed.session.thread_id(), Some("native"));
        drop(resumed);
        spec.request.thread.as_mut().unwrap().mode = CodingThreadMode::Close;
        let mut closing = Conversation::open(root.path(), &spec, "task", &contract).unwrap();
        closing.session.mark_closed().unwrap();
        drop(closing);
        assert!(Conversation::open(root.path(), &spec, "task", &contract).is_err());
    }
    #[test]
    fn conversational_completion_and_presentation_limits_preserve_resume() {
        let root = tempfile::tempdir().unwrap();
        let mut spec = spec();
        for answer in [
            None,
            Some(""),
            Some("Thanks"),
            Some("é".repeat(40000).as_str()),
        ] {
            let text = public_answer(answer);
            assert!(!text.is_empty() && text.len() <= 64 * 1024);
        }
        let mut first = Conversation::open(root.path(), &spec, "task", &json!({})).unwrap();
        first.session.begin().unwrap();
        let receipt = first.finish("native").unwrap();
        drop(first);
        let control = spec.request.thread.as_mut().unwrap();
        control.mode = CodingThreadMode::Resume;
        control.expected_checkpoint =
            Some(serde_json::from_value(receipt["checkpoint"].clone()).unwrap());
        assert!(Conversation::open(root.path(), &spec, "task", &json!({})).is_ok());
    }
    #[test]
    fn successful_standalone_home_is_removed_but_uncertain_home_is_retained() {
        let root = tempfile::tempdir().unwrap();
        let mut spec = spec();
        spec.request.thread = None;
        let mut first = Conversation::open(root.path(), &spec, "task", &json!({})).unwrap();
        let home = first.home.clone();
        first.session.begin().unwrap();
        first.finish("native").unwrap();
        assert!(!home.exists());
        drop(first);
        let mut uncertain = Conversation::open(root.path(), &spec, "task", &json!({})).unwrap();
        let home = uncertain.home.clone();
        uncertain.session.begin().unwrap();
        drop(uncertain);
        assert!(home.exists());
    }
    #[test]
    fn competing_task_writer_does_not_consume_conversation_checkpoint() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        for args in [
            vec!["init", "-b", "develop"],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "base",
            ],
        ] {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(&repo)
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        }
        let store = task_workspace::WorkspaceStore::open(root.path().join("store")).unwrap();
        let registration: task_workspace::Workspace = serde_json::from_value(json!({
            "schemaVersion":1,"id":"shared","hostId":"host","agents":["agent","reviewer"],
            "repositories":[{"name":"repo","source":repo,"integrationBranch":"develop","releaseBranch":"master"}],
            "operations":["edit","review"],"indexers":{}
        })).unwrap();
        store.register(&registration).unwrap();
        store.create_task("shared", "task", "agent").unwrap();
        let mut spec = spec();
        let mut first = Conversation::open(root.path(), &spec, "task", &json!({})).unwrap();
        first.session.begin().unwrap();
        let receipt = first.finish("native").unwrap();
        drop(first);
        let control = spec.request.thread.as_mut().unwrap();
        control.mode = CodingThreadMode::Resume;
        control.expected_checkpoint =
            Some(serde_json::from_value(receipt["checkpoint"].clone()).unwrap());
        let writer = store
            .begin_tool_session("shared", "task", "reviewer", true, None)
            .unwrap();
        let mut blocked = Conversation::open(root.path(), &spec, "task", &json!({})).unwrap();
        assert!(blocked.begin_workspace(&store, &spec, "task").is_err());
        drop(blocked);
        writer.finish().unwrap();
        // The original expected checkpoint still opens after the competing turn ends.
        let mut retry = Conversation::open(root.path(), &spec, "task", &json!({})).unwrap();
        retry
            .begin_workspace(&store, &spec, "task")
            .unwrap()
            .finish()
            .unwrap();
        assert_eq!(retry.finish("native").unwrap()["state"], "READY");
    }

    #[test]
    fn uncertain_conversation_is_never_automatically_replayed() {
        let root = tempfile::tempdir().unwrap();
        let mut spec = spec();
        let mut first = Conversation::open(root.path(), &spec, "task", &json!({})).unwrap();
        first.session.begin().unwrap();
        drop(first);
        let control = spec.request.thread.as_mut().unwrap();
        control.mode = CodingThreadMode::Resume;
        control.expected_checkpoint = Some(uuid::Uuid::new_v4());
        assert!(Conversation::open(root.path(), &spec, "task", &json!({})).is_err());
    }
}
