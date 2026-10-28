//! Proposed bounded Init breadcrumbs. No arbitrary strings or error payloads.
//! Tags are crate-private; no configuration/environment/event can select one.
use crate::validator_ipc::Failure;
use std::sync::atomic::{AtomicU32, Ordering};
use tracing_core::{Metadata, callsite::DefaultCallsite};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub(crate) enum Stage {
    FactoryCreate,
    DeadlineCreation,
    PrecisionSettingsLoad,
    PrecisionSettingsParse,
    EngineTimeout,
    EngineInitialize,
    MetadataScope,
    MetadataMissing,
    MetadataReservation,
    MetadataTaskReservation,
    MetadataTaskTimeout,
    MetadataTaskJoin,
    MetadataDeadline,
    MetadataPublish,
    ResourcesMissing,
    SessionSlot,
    ResourceSettingsLoad,
    ResourceSettingsParse,
    ResourceCaptureMissing,
    ControllerDrain,
    ControllerOwnerReserve,
    ControllerCapture,
    ImageOpen,
    ImageStat,
    ImageMemfdCreate,
    ImageFdDuplicate,
    ImageSourceRead,
    ImageWrite,
    ImageSourceEnd,
    ImageChmod,
    ImageSealAdd,
    ImageSealRead,
    ImageSealRequired,
    ImageFdFlagsRead,
    ImageCloexecRequired,
    ImageElfInspect,
    ControllerPhase,
    ControllerOwnerMissing,
    ControllerOwnerStart,
    ControllerOwnerReady,
    ControllerActorMissing,
    ControllerPublish,
    ImageReadonlyOpen,
    ImageReadonlyProof,
    ImageReadonlyHash,
    ImageReadonlyRecheck,
}
impl Stage {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::FactoryCreate => "factory_create",
            Self::DeadlineCreation => "deadline_creation",
            Self::PrecisionSettingsLoad => "precision_settings_load",
            Self::PrecisionSettingsParse => "precision_settings_parse",
            Self::EngineTimeout => "engine_timeout",
            Self::EngineInitialize => "engine_initialize",
            Self::MetadataScope => "metadata_scope",
            Self::MetadataMissing => "metadata_missing",
            Self::MetadataReservation => "metadata_reservation",
            Self::MetadataTaskReservation => "metadata_task_reservation",
            Self::MetadataTaskTimeout => "metadata_task_timeout",
            Self::MetadataTaskJoin => "metadata_task_join",
            Self::MetadataDeadline => "metadata_deadline",
            Self::MetadataPublish => "metadata_publish",
            Self::ResourcesMissing => "resources_missing",
            Self::SessionSlot => "session_slot",
            Self::ResourceSettingsLoad => "resource_settings_load",
            Self::ResourceSettingsParse => "resource_settings_parse",
            Self::ResourceCaptureMissing => "resource_capture_missing",
            Self::ControllerDrain => "controller_drain",
            Self::ControllerOwnerReserve => "controller_owner_reserve",
            Self::ControllerCapture => "controller_capture",
            Self::ImageOpen => "image_open",
            Self::ImageStat => "image_stat",
            Self::ImageMemfdCreate => "image_memfd_create",
            Self::ImageFdDuplicate => "image_fd_duplicate",
            Self::ImageSourceRead => "image_source_read",
            Self::ImageWrite => "image_write",
            Self::ImageSourceEnd => "image_source_end",
            Self::ImageChmod => "image_chmod",
            Self::ImageSealAdd => "image_seal_add",
            Self::ImageSealRead => "image_seal_read",
            Self::ImageSealRequired => "image_seal_required",
            Self::ImageFdFlagsRead => "image_fd_flags_read",
            Self::ImageCloexecRequired => "image_cloexec_required",
            Self::ImageElfInspect => "image_elf_inspect",
            Self::ControllerPhase => "controller_phase",
            Self::ControllerOwnerMissing => "controller_owner_missing",
            Self::ControllerOwnerStart => "controller_owner_start",
            Self::ControllerOwnerReady => "controller_owner_ready",
            Self::ControllerActorMissing => "controller_actor_missing",
            Self::ControllerPublish => "controller_publish",
            Self::ImageReadonlyOpen => "image_readonly_open",
            Self::ImageReadonlyProof => "image_readonly_proof",
            Self::ImageReadonlyHash => "image_readonly_hash",
            Self::ImageReadonlyRecheck => "image_readonly_recheck",
        }
    }
}
#[cfg(test)]
fn failure_label(failure: Option<&Failure>) -> &'static str {
    match failure {
        None => "not_available",
        Some(Failure::Frame) => "frame",
        Some(Failure::ParentBudget) => "parent_budget",
        Some(Failure::RequestedQuota) => "requested_quota",
        Some(Failure::AllocatorRefused) => "allocator_refused",
        Some(Failure::EngineResource) => "engine_resource",
        Some(Failure::EngineInternal) => "engine_internal",
        Some(Failure::AdapterInvariant) => "adapter_invariant",
        Some(Failure::Protocol) => "protocol",
        Some(Failure::Deadline) => "deadline",
        Some(Failure::Cancelled) => "cancelled",
        Some(Failure::Death) => "death",
        Some(Failure::NoReady) => "no_ready",
        Some(Failure::Rate) => "rate",
        Some(Failure::Stale) => "stale",
    }
}
static ROOT_CALLSITE: DefaultCallsite = DefaultCallsite::new(&ROOT_METADATA);
static ROOT_METADATA: Metadata<'static> = tracing_core::metadata! {
    name: "safe_validator_init",
    target: "light_lambda::validator_init",
    level: tracing_core::Level::ERROR,
    fields: &[],
    callsite: &ROOT_CALLSITE,
    kind: tracing_core::metadata::Kind::SPAN,
};

fn emit(stage: &'static str, failure: &'static str) {
    if !tracing::enabled!(target: "light_lambda::validator_init", tracing::Level::ERROR) {
        return;
    }
    // Explicit parent=None is insufficient for formatters that consult the
    // current registry span. Enter an empty root synchronously, then restore
    // the caller's context by RAII. No await, thread or subscriber replacement.
    let neutral = tracing::dispatcher::get_default(|dispatch| {
        let values = ROOT_METADATA.fields().value_set_all(&[]);
        tracing::Span::new_root_with(&ROOT_METADATA, &values, dispatch)
    });
    let _entered = neutral.enter();
    tracing::error!(target: "light_lambda::validator_init", parent: None,
        stage = stage, failure = failure);
}

/// Trusted per-Init bounded diagnostic. No I/O, payload, global state or reset.
/// Export only after the owning lifecycle runner has returned; this is not a
/// cleanup receipt and cannot prove that a worker started or was reaped.
#[derive(Default)]
pub struct InitDiagnostic {
    first: AtomicU32,
}
impl InitDiagnostic {
    pub(crate) fn note(&self, stage: Stage, failure: Option<&Failure>) {
        let code = match failure {
            None => 0,
            Some(Failure::Frame) => 1,
            Some(Failure::ParentBudget) => 2,
            Some(Failure::RequestedQuota) => 3,
            Some(Failure::AllocatorRefused) => 4,
            Some(Failure::EngineResource) => 5,
            Some(Failure::EngineInternal) => 6,
            Some(Failure::AdapterInvariant) => 7,
            Some(Failure::Protocol) => 8,
            Some(Failure::Deadline) => 9,
            Some(Failure::Cancelled) => 10,
            Some(Failure::Death) => 11,
            Some(Failure::NoReady) => 12,
            Some(Failure::Rate) => 13,
            Some(Failure::Stale) => 14,
        };
        self.note_encoded(stage, code, 0);
    }
    fn note_encoded(&self, stage: Stage, failure: u32, class: u32) {
        let encoded = ((stage as u32 + 1) << 8) | (class << 4) | failure;
        let _ = self
            .first
            .compare_exchange(0, encoded, Ordering::Relaxed, Ordering::Relaxed);
    }
    /// Finite IO classes only; never retain or format an OS error/message/path.
    pub(crate) fn note_io(&self, stage: Stage, error: &std::io::Error) {
        self.note_encoded(stage, 12, io_class(error));
    }
    pub(crate) fn note_refusal(
        &self,
        stage: Stage,
        refusal: crate::validator_ipc::supervision::Refusal,
    ) {
        use crate::validator_ipc::supervision::Refusal;
        self.note_encoded(
            stage,
            12,
            match refusal {
                Refusal::Busy => 9,
                Refusal::Closed => 10,
                Refusal::InvalidPolicy => 11,
            },
        );
    }
    /// Explicit trusted post-run export. Respects the subscriber filter; does
    /// not retry, flush, claim delivery or add work before owned cleanup.
    pub fn emit_after_run(&self) {
        let encoded = self.first.load(Ordering::Relaxed);
        if encoded == 0 {
            return;
        }
        let stages = [
            Stage::FactoryCreate,
            Stage::DeadlineCreation,
            Stage::PrecisionSettingsLoad,
            Stage::PrecisionSettingsParse,
            Stage::EngineTimeout,
            Stage::EngineInitialize,
            Stage::MetadataScope,
            Stage::MetadataMissing,
            Stage::MetadataReservation,
            Stage::MetadataTaskReservation,
            Stage::MetadataTaskTimeout,
            Stage::MetadataTaskJoin,
            Stage::MetadataDeadline,
            Stage::MetadataPublish,
            Stage::ResourcesMissing,
            Stage::SessionSlot,
            Stage::ResourceSettingsLoad,
            Stage::ResourceSettingsParse,
            Stage::ResourceCaptureMissing,
            Stage::ControllerDrain,
            Stage::ControllerOwnerReserve,
            Stage::ControllerCapture,
            Stage::ImageOpen,
            Stage::ImageStat,
            Stage::ImageMemfdCreate,
            Stage::ImageFdDuplicate,
            Stage::ImageSourceRead,
            Stage::ImageWrite,
            Stage::ImageSourceEnd,
            Stage::ImageChmod,
            Stage::ImageSealAdd,
            Stage::ImageSealRead,
            Stage::ImageSealRequired,
            Stage::ImageFdFlagsRead,
            Stage::ImageCloexecRequired,
            Stage::ImageElfInspect,
            Stage::ControllerPhase,
            Stage::ControllerOwnerMissing,
            Stage::ControllerOwnerStart,
            Stage::ControllerOwnerReady,
            Stage::ControllerActorMissing,
            Stage::ControllerPublish,
            Stage::ImageReadonlyOpen,
            Stage::ImageReadonlyProof,
            Stage::ImageReadonlyHash,
            Stage::ImageReadonlyRecheck,
        ];
        let failures = [
            "not_available",
            "frame",
            "parent_budget",
            "requested_quota",
            "allocator_refused",
            "engine_resource",
            "engine_internal",
            "adapter_invariant",
            "protocol",
            "deadline",
            "cancelled",
            "death",
            "no_ready",
            "rate",
            "stale",
        ];
        let classes = [
            "no_ready",
            "no_ready_not_found",
            "no_ready_permission_denied",
            "no_ready_unsupported",
            "no_ready_invalid_argument",
            "no_ready_descriptor_limit",
            "no_ready_resource",
            "no_ready_interrupted",
            "no_ready_other_io",
            "no_ready_busy",
            "no_ready_closed",
            "no_ready_invalid_policy",
        ];
        let class = (encoded >> 4) & 15;
        let failure = if class == 0 {
            failures[(encoded & 15) as usize]
        } else {
            classes[class as usize]
        };
        emit(stages[((encoded >> 8) - 1) as usize].label(), failure);
    }
}

fn io_class(error: &std::io::Error) -> u32 {
    match error.raw_os_error() {
        Some(libc::ENOENT | libc::ENOTDIR) => 1,
        Some(libc::EACCES | libc::EPERM) => 2,
        Some(libc::ENOSYS | libc::EOPNOTSUPP) => 3,
        Some(libc::EINVAL) => 4,
        Some(libc::EMFILE | libc::ENFILE) => 5,
        Some(libc::ENOMEM | libc::ENOSPC) => 6,
        Some(libc::EINTR) => 7,
        _ => 8,
    }
}
#[cfg(test)]
#[path = "validator_init_diagnostic_tests.rs"]
mod tests;
