//! The workflow selects only a native model; permission policy belongs to the Agent.
use super::*;
pub(super) fn admit_native_selection(
    runtime: &CodingAdapterRuntime,
    request: &CodingDispatchRequest,
) -> Result<()> {
    if runtime.contract.adapter_id == coding_agent_runtime::claude::ADAPTER_ID {
        let policy = runtime
            .claude_policy
            .as_ref()
            .context("Claude runtime policy missing")?;
        policy.resolve(request.native_model.as_deref())?;
        if request.thread.is_none() || runtime.enterprise_gateway.is_some() {
            bail!("Claude requires explicit workflow-owned personal thread control");
        }
    } else if runtime.contract.adapter_id == coding_agent_runtime::CODEX_APP_SERVER_ADAPTER_ID
        && let Some(policy) = &runtime.codex_policy
    {
        policy.validate(request.native_model.as_deref())?;
        if runtime.enterprise_gateway.is_some() || runtime.claude_policy.is_some() {
            bail!("Codex personal policy requires the personal route");
        }
    } else if request.native_model.is_some()
        || runtime.claude_policy.is_some()
        || runtime.codex_policy.is_some()
    {
        bail!("nativeModel is not supported by the selected coding adapter");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use coding_agent_runtime::claude as c;
    use serde_json::json;
    fn policy() -> CodingProfilePolicy {
        let d = agent_core::sha256_digest(b"local-claude-test");
        let contract:CodingAdapterContract=serde_json::from_value(json!({
            "schemaVersion":1,"adapterId":c::ADAPTER_ID,"adapterVersion":c::VERSION,"adapterProtocolVersion":c::PROTOCOL,
            "actionKind":c::ACTION,"compatibilityDigest":d,"imageDigest":d,"capabilityDigest":agent_runtime_protocol::canonical_digest(&c::capabilities()).unwrap(),
            "templateId":c::TEMPLATE,"templateVersion":1,"templateDigest":d,"executable":"/usr/local/bin/claude",
            "binaryDigest":c::BINARY_DIGEST,"schemaDigest":c::schema_digest(),
            "requiredFeatures":[c::ADAPTER_ID,"canonical-patch-output","workflow-coding-threads-v1","claude-review-namespace-v1"]
        })).unwrap();
        let mut value = serde_json::to_value(&contract).unwrap();
        let extra = json!({"productProfileDigest":d,"repositoryUriPrefix":"file:///spool/",
            "model":"coding-implementer","reviewModel":"coding-reviewer","authenticationProfile":"personal-subscription",
            "claudePolicy":{"permissionSource":"claude-cli","permissionMode":"inherit","defaultModel":"sonnet",
                "models":{"sonnet":"claude-sonnet-5","opus":"claude-opus-test"},"tools":[],"allowedTools":[]},
            "qualification":{"schemaVersion":1,"adapterId":c::ADAPTER_ID,"adapterVersion":c::VERSION,"status":"local-qualified",
                "evaluatedDimensions":c::local_dimensions(),"contractDigest":contract.digest().unwrap(),"evidenceDigest":c::evidence_digest()}});
        value
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(value).unwrap()
    }
    fn request() -> serde_json::Value {
        json!({"text":"implement", "profile":"coding", "coding":{
            "nativeModel":"opus", "repository":{"artifactUri":"file:///spool/repo.bundle","digest":agent_core::sha256_digest(b"bundle"),"size":10,"mediaType":"application/x-git-bundle"},
            "baseRevision":"a".repeat(40),"workspaceRoot":"/workspace/repository","allowedTools":["fs.read","fs.write","process.exec"],
            "maximumPatchBytes":4096,"maximumChangedFiles":1,
            "thread":{"runnerId":"claude-runner","sessionRef":Uuid::now_v7(),"stageId":"stage","mode":"new","closeAfterTurn":false}}})
    }
    #[test]
    fn local_claude_profile_and_workflow_model_are_admitted_without_policy_override() {
        let config = coding_profile_from_policy(Some(&policy()))
            .unwrap()
            .unwrap();
        let input: ClientMessage = serde_json::from_value(request()).unwrap();
        let input = input.coding.unwrap();
        let (_, spec, runtime) = crate::coding_jobs::prepare(&config, &input, "implement").unwrap();
        assert_eq!(runtime.native_model.as_deref(), Some("opus"));
        assert_eq!(spec.model_alias, "coding-implementer");
        assert_eq!(runtime.contract.action_kind, c::ACTION);
        assert_eq!(
            runtime.claude_policy.unwrap().permission_source,
            c::PermissionSource::ClaudeCli
        );
        let mut untrusted = request();
        untrusted["coding"]["claudePolicy"] = json!({"permissionMode":"bypassPermissions"});
        assert!(serde_json::from_value::<ClientMessage>(untrusted).is_err());
        let mut bad = request();
        bad["coding"]["nativeModel"] = json!("not-allowed");
        let bad: ClientMessage = serde_json::from_value(bad).unwrap();
        assert!(crate::coding_jobs::prepare(&config, &bad.coding.unwrap(), "implement").is_err());
        let mut omitted = request();
        omitted["coding"]
            .as_object_mut()
            .unwrap()
            .remove("nativeModel");
        let omitted: ClientMessage = serde_json::from_value(omitted).unwrap();
        assert!(
            crate::coding_jobs::prepare(&config, &omitted.coding.unwrap(), "implement")
                .unwrap()
                .2
                .native_model
                .is_none()
        );
    }
    #[test]
    fn local_claude_profile_accepts_validated_workspace_bindings() {
        let mut p = policy();
        p.workspace_bindings = serde_json::from_value(json!([{
            "schemaVersion": 1, "workspaceId": "personal", "hostId": "host",
            "environment": "dev", "runnerId": "claude-runner",
            "membershipRevision": agent_core::sha256_digest(b"membership"),
            "authorizationRevision": 1, "subjects": ["user"],
            "agents": ["claude-agent"], "intents": ["inspect", "implement", "review"]
        }]))
        .unwrap();
        assert!(coding_profile_from_policy(Some(&p)).is_ok());
        p.workspace_bindings[0].membership_revision = "invalid".into();
        assert!(coding_profile_from_policy(Some(&p)).is_err());
    }
    #[test]
    fn local_qualification_cannot_be_promoted_or_confused_with_other_profiles() {
        let mut p = policy();
        p.qualification.status = coding_agent_runtime::CodingAdapterQualificationStatus::Qualified;
        assert!(coding_profile_from_policy(Some(&p)).is_err());
        let mut p = policy();
        p.authentication_profile = CodingAuthenticationProfile::EnterpriseApi;
        assert!(coding_profile_from_policy(Some(&p)).is_err());
        let mut p = policy();
        p.claude_policy = None;
        assert!(coding_profile_from_policy(Some(&p)).is_err());
        let mut p = policy();
        p.binary_digest = agent_core::sha256_digest(b"different binary");
        assert!(coding_profile_from_policy(Some(&p)).is_err());
        let mut p = policy();
        p.qualification.evidence_digest = agent_core::sha256_digest(b"different evidence");
        assert!(coding_profile_from_policy(Some(&p)).is_err());
    }
}
