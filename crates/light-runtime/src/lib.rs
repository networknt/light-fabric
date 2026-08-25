pub mod cache;
pub mod config;
pub mod lifecycle;
pub mod logging;
pub mod module_registry;
pub mod runtime;
pub mod signal;
pub mod transport;

pub use cache::{CacheRegistry, ClearCacheOutcome, MokaRuntimeCache, RuntimeCache};
pub use config::{
    BootstrapConfig, ConfigProvenance, ConfigSource, DirectRegistryConfig, PortalRegistryConfig,
    PreparedConfig, RemoteBootstrapResult, RuntimeConfig, ServerConfig, ServiceIdentity,
};
pub use config_loader::EmbeddedConfigFile;
pub use lifecycle::{
    AdmissionClosed, AdmissionGate, AdmissionKind, AdmissionPermit, LifecycleParticipant,
    LifecycleRegistrar, LifecycleRegistry, MANDATORY_CLEANUP_FLOOR, ShutdownContext, ShutdownMode,
    ShutdownReason,
};
pub use logging::{
    LOGGING_FILTER_KEY, LOGGING_MODULE_ID, LogFileAccess, LogStreamBroadcaster, LoggingControl,
    LoggingFilterState, TracingGuard, TracingInitError, TracingOptions, init_tracing,
};
pub use module_registry::{
    CLIENT_CONFIG_NAME, CLIENT_MODULE_ID, ConfigManager, MaskSpec, ModuleEntry, ModuleKind,
    ModuleRegistry, ModuleSummary, ReloadContext, ReloadFailed, ReloadModulesResult, ReloadOutcome,
    ReloadSkipped, ReloadStatus, ReloadableModule, RuntimeMcpHandler, client_config_masks,
};
pub use portal_registry::{
    DiscoveryNode, DiscoverySnapshot, DiscoverySubscription, PortalRegistryClient,
    RegistrationState, ServiceMetadataUpdate,
};
pub use runtime::{
    LifecycleState, LightRuntime, LightRuntimeBuilder, Module, RegistrationPolicy, RunningRuntime,
    RuntimeError,
};
pub use signal::ShutdownWatcher;
pub use transport::{BoundTransport, ResolvedServerMetadata, TransportRuntime};
