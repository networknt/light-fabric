//! Trusted operation matching and metadata. Request-schema validation is a
//! separate policy; this handler never establishes schema-validation coverage.
use super::specification_work;
use crate::{
    compat::Compatibility,
    plan::{Binding, HandlerFactory, RequestCapabilities, TargetRoutes},
    *,
};
use async_trait::async_trait;
use gateway_policy::openapi_operation::MAX_NORMALIZATION_PATH_BYTES;
use gateway_policy::openapi_operation::{BasePathProfile, OperationCatalogue, SourceFormat};
use serde_json::Value;
use std::sync::Arc;

#[derive(Clone)]
pub struct SpecificationHandler {
    catalogue: OperationCatalogue,
    paths: TargetRoutes,
    // Only trusted raw-source callers acquire a qualified normalization profile;
    // metadata-only Value callers do not acquire normalization or source order.
    _normalization: Option<Arc<BasePathProfile>>,
}
impl SpecificationHandler {
    /// The caller must provide trusted local configuration, never ingress JSON.
    /// Path selection deliberately precedes method selection, as Java does.
    pub fn from_document(document: &Value) -> Result<Self, InitError> {
        let catalogue =
            OperationCatalogue::from_document(document).map_err(InitError::Malformed)?;
        Self::from_catalogue(catalogue)
    }
    /// Order-qualified metadata from trusted local raw configuration only.
    pub fn from_source(source: &[u8], format: SourceFormat) -> Result<Self, InitError> {
        Self::from_source_with_base_path(source, format, None)
    }
    /// Immutable handler.yml fallback; validate only the effective regex, while
    /// retaining ordinary fallback type validation even when unused.
    pub fn from_source_with_base_path(
        source: &[u8],
        format: SourceFormat,
        fallback: Option<&Value>,
    ) -> Result<Self, InitError> {
        let normalization =
            BasePathProfile::from_source(source, format, fallback).map_err(InitError::Malformed)?;
        let catalogue =
            OperationCatalogue::from_source(source, format).map_err(InitError::Malformed)?;
        let mut handler = Self::from_catalogue(catalogue)?;
        handler._normalization = Some(Arc::new(normalization));
        Ok(handler)
    }
    fn from_catalogue(catalogue: OperationCatalogue) -> Result<Self, InitError> {
        let paths = TargetRoutes::compile(catalogue.templates().map(|path| {
            (
                format!("{path}@get"),
                Target::Function("operation-metadata-only".into()),
            )
        }))?;
        Ok(Self {
            catalogue,
            paths,
            _normalization: None,
        })
    }
    /// Reuse trusted, already prepared resources without changing their provenance.
    /// In particular, a Value-derived catalogue remains order-unqualified.
    pub fn from_prepared(
        catalogue: OperationCatalogue,
        profile: Arc<BasePathProfile>,
    ) -> Result<Self, InitError> {
        let mut handler = Self::from_catalogue(catalogue)?;
        handler._normalization = Some(profile);
        Ok(handler)
    }
}
#[async_trait]
impl RequestHandler for SpecificationHandler {
    fn id(&self) -> HandlerId {
        HandlerId::Specification
    }
    fn operation_authorization_role(&self) -> OperationAuthorizationRole {
        OperationAuthorizationRole::MetadataProducer
    }
    async fn on_request(
        &self,
        inv: &mut Invocation,
        state: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        state.operation = None;
        if let Err(error) = specification_work::budget(inv.deadline, &inv.cancel) {
            return Ok(HandlerOutcome::Reject(error));
        }
        if inv.path.len() > MAX_NORMALIZATION_PATH_BYTES {
            return Ok(HandlerOutcome::Reject(
                StatusError::specification_path_too_large(),
            ));
        }
        let normalized;
        let path = if let Some(profile) = &self._normalization {
            normalized = match specification_work::normalize(
                profile.clone(),
                &inv.path,
                inv.deadline,
                &inv.cancel,
            )
            .await
            {
                Ok(path) => path,
                Err(error) => return Ok(HandlerOutcome::Reject(error)),
            };
            normalized.as_str()
        } else {
            inv.path.as_str()
        };
        if let Err(error) = specification_work::budget(inv.deadline, &inv.cancel) {
            return Ok(HandlerOutcome::Reject(error));
        }
        let Some(template) = self.paths.matching_template(&http::Method::GET, path) else {
            if let Err(error) = specification_work::budget(inv.deadline, &inv.cancel) {
                return Ok(HandlerOutcome::Reject(error));
            }
            return Ok(HandlerOutcome::Reject(StatusError::new(
                404,
                "ERR10007",
                "INVALID_REQUEST_PATH",
                &format!("Request path {} cannot be found in the spec.", inv.path),
            )));
        };
        let Some(operation) = self.catalogue.operation(template, inv.method.as_str()) else {
            if let Err(error) = specification_work::budget(inv.deadline, &inv.cancel) {
                return Ok(HandlerOutcome::Reject(error));
            }
            return Ok(HandlerOutcome::Reject(StatusError::new(
                405,
                "ERR10008",
                "METHOD_NOT_ALLOWED",
                &format!(
                    "Request method {} cannot be found in the path {}.",
                    inv.method.as_str().to_ascii_lowercase(),
                    template
                ),
            )));
        };
        // No request/header/path-parameter mutation, RouteMatch replacement,
        // auth work or dispatch. Metadata construction does not enforce scopes.
        if let Err(error) = specification_work::budget(inv.deadline, &inv.cancel) {
            return Ok(HandlerOutcome::Reject(error));
        }
        state.operation = Some(operation);
        if let Err(error) = specification_work::budget(inv.deadline, &inv.cancel) {
            state.operation = None;
            return Ok(HandlerOutcome::Reject(error));
        }
        Ok(HandlerOutcome::Continue)
    }
}
impl HandlerFactory for SpecificationHandler {
    fn validator_media_role(&self) -> crate::plan::ValidatorMediaRole {
        crate::plan::ValidatorMediaRole::Preserves
    }
    fn build(&self, _: &Compatibility, _: &str) -> Result<Binding, InitError> {
        Ok(Binding::Request(
            Arc::new(self.clone()),
            RequestCapabilities::default(),
        ))
    }
}
