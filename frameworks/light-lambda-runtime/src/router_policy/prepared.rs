//! Cold-start program/factory composition without worker admission or execution.
use super::*;
use crate::router_program::{LoadingLimits, Program};

pub struct PreparedRewriteProgram {
    pub factory: Arc<dyn HandlerFactory>,
    pub resources: Arc<RouterSettings>,
    pub program: Arc<Program>,
    source_identity: String,
}
impl PreparedRewriteProgram {
    pub fn source_identity(&self) -> &str {
        &self.source_identity
    }
}
impl RouterPolicyProvider {
    /// Captured sources are the only authority. No file is reopened and no
    /// worker/controller is started. Loading limits remain explicit inputs.
    pub fn prepare_rewrite_program(
        &self,
        sources: &CapturedRouterSources,
        limits: LoadingLimits,
    ) -> Result<PreparedRewriteProgram, InitError> {
        let (value, destinations) = sources.resolve()?;
        serverless_core::unsupported::remaining::router(&value).map_err(|_| invalid())?;
        let program = Arc::new(Program::capture(&value, limits).map_err(|_| invalid())?);
        let https = value
            .get("httpsEnabled")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let catalogue = destinations
            .into_iter()
            .map(|(id, raw)| {
                let urls = raw
                    .split(',')
                    .map(|raw| normalize_destination(raw.trim(), https))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((id, urls))
            })
            .collect::<Result<BTreeMap<_, _>, InitError>>()?;
        let timeout = Duration::from_millis(
            value
                .get("maxRequestTime")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(1000),
        );
        let factory = Arc::new(RouterHandler::new(catalogue.keys().cloned()));
        Ok(PreparedRewriteProgram {
            factory,
            resources: Arc::new(RouterSettings((catalogue, timeout, Vec::new()))),
            program,
            source_identity: sources.identity().to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serverless_core::{ExchangeState, HandlerId, HandlerOutcome, plan::Binding};
    #[tokio::test]
    async fn captured_program_builds_real_factory_without_reopening_or_spawning() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("absent");
        std::fs::write(
            dir.path().join("router.yml"),
            "enabled: true\nhttpsEnabled: false\nurlRewriteRules: ['^/x$ /rewritten']\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("direct-registry.yml"),
            "directUrls:\n  owned: http://127.0.0.1:19091/base\n",
        )
        .unwrap();
        let sources = CapturedRouterSources::capture(
            &[],
            None,
            &absent,
            dir.path(),
            CaptureLimits {
                per_source: 1024,
                aggregate: 4096,
            },
        )
        .unwrap();
        std::fs::remove_file(dir.path().join("router.yml")).unwrap();
        std::fs::remove_file(dir.path().join("direct-registry.yml")).unwrap();
        let prepared = RouterPolicyProvider
            .prepare_rewrite_program(
                &sources,
                LoadingLimits {
                    rules: 1,
                    per_rule_bytes: 64,
                    aggregate_bytes: 64,
                    token_utf16: 32,
                },
            )
            .unwrap();
        assert_eq!(prepared.source_identity(), sources.identity());
        assert_eq!(prepared.program.rules()[0].pattern, "^/x$");
        assert_eq!(prepared.program.rules()[0].replacement, "/rewritten");
        assert_eq!(
            prepared.resources.0.0["owned"],
            ["http://127.0.0.1:19091/base"]
        );
        assert!(
            prepared.resources.0.2.is_empty(),
            "Java program must not become literal rewrites"
        );
        let compat = serverless_core::compat::COMPATIBILITY
            .iter()
            .find(|c| c.id == HandlerId::Router)
            .unwrap();
        let Binding::Request(handler, _) = prepared.factory.build(compat, "router").unwrap() else {
            panic!("Router request factory");
        };
        let now = tokio::time::Instant::now();
        let mut inv = serverless_aws::alb::decode(serde_json::json!({"httpMethod":"GET","path":"/x","requestContext":{"elb":{"targetGroupArn":"owned"}},"multiValueHeaders":{"service_id":["owned"]}}), serverless_aws::alb::DecodeContext {request_id:"factory-only".into(), deadline:now+Duration::from_secs(1),response_deadline:now+Duration::from_secs(2),cancel:tokio_util::sync::CancellationToken::new()}, &Default::default()).unwrap();
        let mut state = ExchangeState::default();
        assert!(matches!(
            handler.on_request(&mut inv, &mut state).await.unwrap(),
            HandlerOutcome::Continue
        ));
        assert!(!inv.headers.contains_key("service_id"));
        assert_eq!(
            inv.path, "/x",
            "rewrite belongs to terminal, not request policy"
        );
        inv.headers
            .insert("service_id", "not-owned".parse().unwrap());
        let mut fresh = ExchangeState::default();
        assert!(matches!(
            handler.on_request(&mut inv, &mut fresh).await.unwrap(),
            HandlerOutcome::Reject(error) if error.code == "ERR10089"
        ));
    }
}
