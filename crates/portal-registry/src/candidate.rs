use controller_wire::RUNTIME_RKYV_V1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlCandidate {
    LegacyJson,
    RuntimeRkyvV1,
}

impl ControlCandidate {
    pub const fn legacy_json() -> Self {
        Self::LegacyJson
    }

    pub const fn runtime_rkyv_v1() -> Self {
        Self::RuntimeRkyvV1
    }

    pub const fn profile_token(self) -> Option<&'static str> {
        match self {
            Self::LegacyJson => None,
            Self::RuntimeRkyvV1 => Some(RUNTIME_RKYV_V1),
        }
    }

    pub const fn is_legacy(self) -> bool {
        matches!(self, Self::LegacyJson)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectionFailureClass {
    Unavailable,
    Unsupported,
    Authentication,
    Authorization,
    MalformedProfile,
    // Reserved now so the N2 downgrade policy cannot accidentally omit the
    // fail-closed classes introduced by the N3 identity/application exchange.
    #[allow(dead_code)]
    IdentityMismatch,
    #[allow(dead_code)]
    PostRegistration,
    Internal,
}

impl ConnectionFailureClass {
    pub(crate) const fn allows_fallback(self) -> bool {
        matches!(self, Self::Unavailable | Self::Unsupported)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{class:?}: {message}")]
pub(crate) struct ConnectionFailure {
    pub class: ConnectionFailureClass,
    pub message: String,
}

impl ConnectionFailure {
    pub(crate) fn new(class: ConnectionFailureClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
        }
    }
}
