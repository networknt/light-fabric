//! Reviewed opt-in worker-image capture. No worker is spawned by this module.
//! Trusted code supplies the pin; no request/config-derived executable identity.
use super::controller::{Budget, Failure, budget::Credit};
use crate::bundle_snapshot::Pin;
use crate::validator_init_diagnostic::{InitDiagnostic, Stage};
use sha2::{Digest, Sha256};
use std::{fs::File, path::PathBuf, sync::Arc};

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[path = "static_elf.rs"]
mod static_elf;

#[path = "readonly_executable.rs"]
pub(super) mod readonly;

const SCRATCH: usize = 64 * 1024;
const FIXED_REQUESTED_BYTES: usize = 4096;

/// Private file ownership is inseparable from its requested-byte credit. Every
/// controller/process generation retains the same Arc; recovery never reopens
/// the original pathname. Main-image sealing is not dynamic-library pinning.
pub(super) struct Image {
    file: File,
    exec_path: PathBuf,
    readonly: Option<readonly::Protection>,
    _credit: Credit,
    diagnostic: Option<Arc<InitDiagnostic>>,
}

fn native_header(bytes: &[u8]) -> bool {
    bytes.len() >= 64
        && bytes[..7] == [0x7f, b'E', b'L', b'F', 2, 1, 1]
        && matches!(u16::from_le_bytes([bytes[16], bytes[17]]), 2 | 3)
        && u16::from_le_bytes([bytes[18], bytes[19]]) == 62
        && bytes[20..24] == [1, 0, 0, 0]
}

fn no_ready(
    diagnostic: Option<&InitDiagnostic>,
    stage: Stage,
    error: Option<&std::io::Error>,
) -> Failure {
    if let Some(d) = diagnostic {
        match error {
            Some(error) => d.note_io(stage, error),
            None => d.note(stage, Some(&Failure::NoReady)),
        }
    }
    Failure::NoReady
}
impl Image {
    pub(super) fn capture(pin: &Pin, budget: &Arc<Budget>) -> Result<Arc<Self>, Failure> {
        Self::capture_observed(pin, budget, None)
    }
    fn capture_observed(
        pin: &Pin,
        budget: &Arc<Budget>,
        diagnostic: Option<Arc<InitDiagnostic>>,
    ) -> Result<Arc<Self>, Failure> {
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
        {
            let _ = (pin, budget);
            return Err(Failure::NoReady);
        }
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            use std::{
                fs::OpenOptions,
                io::{Read, Write},
                os::{
                    fd::{AsRawFd, FromRawFd},
                    unix::fs::OpenOptionsExt,
                },
            };
            if pin.bytes < 64
                || !pin.path.is_absolute()
                || pin
                    .path
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err(Failure::AdapterInvariant);
            }
            let scratch = pin.bytes.min(SCRATCH);
            // Covers requested memfd payload, bounded userspace copy buffer,
            // Image/Arc/path and disclosed bookkeeping/page-rounding allowance.
            // It is not an allocator/RSS/kernel metadata or production bound.
            let requested = pin
                .bytes
                .checked_add(scratch)
                .and_then(|n| n.checked_add(FIXED_REQUESTED_BYTES))
                .and_then(|n| n.checked_add(pin.path.as_os_str().len().checked_add(1)?))
                .and_then(|n| n.checked_add(std::mem::size_of::<Self>()))
                .ok_or(Failure::ParentBudget)?;
            let credit = budget.reserve(requested)?;
            // Open once, nofollow/nonblocking, then inspect that same descriptor.
            // A pathname replacement cannot turn a regular-file check into a
            // blocking FIFO/device open; intermediate directories are trusted.
            let mut source = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&pin.path)
                .map_err(|error| no_ready(diagnostic.as_deref(), Stage::ImageOpen, Some(&error)))?;
            let metadata = source
                .metadata()
                .map_err(|error| no_ready(diagnostic.as_deref(), Stage::ImageStat, Some(&error)))?;
            if !metadata.is_file() || metadata.len() != pin.bytes as u64 {
                return Err(Failure::AdapterInvariant);
            }
            let mut buffer = Vec::new();
            buffer
                .try_reserve_exact(scratch)
                .map_err(|_| Failure::ParentBudget)?;
            buffer.resize(scratch, 0);
            // Explicit executable-memfd support is required. Unsupported/denied
            // creation fails closed; no old-kernel, disk, shell or path fallback.
            let raw = unsafe {
                libc::memfd_create(
                    c"light-lambda-verified-worker".as_ptr(),
                    libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING | libc::MFD_EXEC,
                )
            };
            if raw < 0 {
                return Err(no_ready(
                    diagnostic.as_deref(),
                    Stage::ImageMemfdCreate,
                    Some(&std::io::Error::last_os_error()),
                ));
            }
            // SAFETY: a successful memfd_create returns a new owned descriptor.
            let mut file = unsafe { File::from_raw_fd(raw) };
            if raw < 3 {
                let duplicate = unsafe { libc::fcntl(raw, libc::F_DUPFD_CLOEXEC, 3) };
                if duplicate < 0 {
                    return Err(no_ready(
                        diagnostic.as_deref(),
                        Stage::ImageFdDuplicate,
                        Some(&std::io::Error::last_os_error()),
                    ));
                }
                // Keep the image away from child stdio remapping targets.
                file = unsafe { File::from_raw_fd(duplicate) };
            }
            let mut remaining = pin.bytes;
            let mut digest = Sha256::new();
            let mut first = true;
            while remaining != 0 {
                let n = remaining.min(buffer.len());
                source.read_exact(&mut buffer[..n]).map_err(|e| {
                    if e.kind() == std::io::ErrorKind::UnexpectedEof {
                        Failure::AdapterInvariant
                    } else {
                        no_ready(diagnostic.as_deref(), Stage::ImageSourceRead, Some(&e))
                    }
                })?;
                if first && !native_header(&buffer[..n]) {
                    return Err(Failure::AdapterInvariant);
                }
                first = false;
                digest.update(&buffer[..n]);
                file.write_all(&buffer[..n]).map_err(|error| {
                    no_ready(diagnostic.as_deref(), Stage::ImageWrite, Some(&error))
                })?;
                remaining -= n;
            }
            let mut extra = [0];
            if source.read(&mut extra).map_err(|error| {
                no_ready(diagnostic.as_deref(), Stage::ImageSourceEnd, Some(&error))
            })? != 0
                || <[u8; 32]>::from(digest.finalize()) != pin.sha256
            {
                return Err(Failure::AdapterInvariant);
            }
            if unsafe { libc::fchmod(file.as_raw_fd(), 0o500) } != 0 {
                return Err(no_ready(
                    diagnostic.as_deref(),
                    Stage::ImageChmod,
                    Some(&std::io::Error::last_os_error()),
                ));
            }
            let seals = libc::F_SEAL_WRITE
                | libc::F_SEAL_GROW
                | libc::F_SEAL_SHRINK
                | libc::F_SEAL_EXEC
                | libc::F_SEAL_SEAL;
            if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) } < 0 {
                return Err(no_ready(
                    diagnostic.as_deref(),
                    Stage::ImageSealAdd,
                    Some(&std::io::Error::last_os_error()),
                ));
            }
            let image = Arc::new(Self {
                exec_path: PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd())),
                file,
                readonly: None,
                _credit: credit,
                diagnostic,
            });
            image.check_seals()?;
            Ok(image)
        }
    }

    /// Additional opt-in static envelope check over the already sealed image.
    /// This constructor is not production profile/catalogue admission.
    pub(super) fn capture_static(pin: &Pin, budget: &Arc<Budget>) -> Result<Arc<Self>, Failure> {
        Self::capture_static_observed(pin, budget, None)
    }
    pub(super) fn capture_static_observed(
        pin: &Pin,
        budget: &Arc<Budget>,
        diagnostic: Option<Arc<InitDiagnostic>>,
    ) -> Result<Arc<Self>, Failure> {
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
        {
            let _ = (pin, budget);
            Err(Failure::NoReady)
        }
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            let image = Self::capture_observed(pin, budget, diagnostic)?;
            static_elf::validate(&image.file, pin.bytes as u64).map_err(|failure| {
                if matches!(failure, Failure::NoReady) {
                    image.note_no_ready(Stage::ImageElfInspect);
                }
                failure
            })?;
            Ok(image)
        }
    }
    fn check_seals(&self) -> Result<(), Failure> {
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
        {
            return Err(Failure::NoReady);
        }
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            use std::os::fd::AsRawFd;
            let seals = unsafe { libc::fcntl(self.file.as_raw_fd(), libc::F_GET_SEALS) };
            if seals < 0 {
                no_ready(
                    self.diagnostic.as_deref(),
                    Stage::ImageSealRead,
                    Some(&std::io::Error::last_os_error()),
                );
            }
            let flags = unsafe { libc::fcntl(self.file.as_raw_fd(), libc::F_GETFD) };
            if flags < 0 {
                no_ready(
                    self.diagnostic.as_deref(),
                    Stage::ImageFdFlagsRead,
                    Some(&std::io::Error::last_os_error()),
                );
            }
            let required = libc::F_SEAL_WRITE
                | libc::F_SEAL_GROW
                | libc::F_SEAL_SHRINK
                | libc::F_SEAL_EXEC
                | libc::F_SEAL_SEAL;
            if seals >= 0 && seals & required != required {
                self.note_no_ready(Stage::ImageSealRequired);
            }
            if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
                self.note_no_ready(Stage::ImageCloexecRequired);
            }
            if seals < 0
                || seals & required != required
                || flags < 0
                || flags & libc::FD_CLOEXEC == 0
            {
                return Err(Failure::NoReady);
            }
            Ok(())
        }
    }

    pub(super) fn note_no_ready(&self, stage: Stage) {
        if let Some(d) = &self.diagnostic {
            d.note(stage, Some(&Failure::NoReady));
        }
    }
    pub(super) fn note_refusal(&self, stage: Stage, refusal: super::supervision::Refusal) {
        if let Some(d) = &self.diagnostic {
            d.note_refusal(stage, refusal);
        }
    }
    pub(super) fn command(&self) -> Result<tokio::process::Command, Failure> {
        if let Some(protection) = &self.readonly {
            protection.recheck(&self.file).inspect_err(|_| {
                self.note_no_ready(Stage::ImageReadonlyRecheck);
            })?;
        } else {
            self.check_seals()?;
        }
        // The owning Memory clone holds this fd across pre_exec/spawn and
        // through actual child wait. CLOEXEC closes the child's extra fd only
        // after the kernel has opened this native ELF. No descriptor clearing.
        Ok(tokio::process::Command::new(&self.exec_path))
    }
    pub(super) fn requires_no_new_privileges(&self) -> bool {
        self.readonly.is_some()
    }
}

#[cfg(all(test, target_os = "linux", target_arch = "x86_64"))]
mod tests {
    use super::*;
    use std::{
        os::{
            fd::AsRawFd,
            unix::fs::{FileExt, symlink},
        },
        path::Path,
    };
    fn elf() -> Vec<u8> {
        // Header-shaped non-runnable test data, never a spawned helper/worker.
        let mut b = vec![0; 64];
        b[..7].copy_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1]);
        b[16] = 3;
        b[18] = 62;
        b[20] = 1;
        b
    }
    fn pin(path: &Path, b: &[u8]) -> Pin {
        Pin {
            path: path.to_owned(),
            bytes: b.len(),
            sha256: Sha256::digest(b).into(),
        }
    }
    #[test]
    fn budget_rejects_before_open() {
        let b = Budget::new(1);
        let p = pin(Path::new("/not-opened-by-image-budget"), &elf());
        assert!(matches!(Image::capture(&p, &b), Err(Failure::ParentBudget)));
        assert_eq!(b.used(), 0);
    }
    #[test]
    fn invalid_header_and_wrong_hash_fail_without_retained_credit() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("input");
        let b = Budget::new(1 << 20);
        let mut bytes = elf();
        bytes[0] = 0;
        std::fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            Image::capture(&pin(&path, &bytes), &b),
            Err(Failure::AdapterInvariant)
        ));
        bytes = elf();
        std::fs::write(&path, &bytes).unwrap();
        let mut p = pin(&path, &bytes);
        p.sha256 = [0; 32];
        assert!(matches!(
            Image::capture(&p, &b),
            Err(Failure::AdapterInvariant)
        ));
        assert_eq!(b.used(), 0);
    }
    #[test]
    fn last_component_symlink_is_not_followed() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("input");
        let link = d.path().join("link");
        let bytes = elf();
        std::fs::write(&path, &bytes).unwrap();
        symlink(&path, &link).unwrap();
        let b = Budget::new(1 << 20);
        assert!(matches!(
            Image::capture(&pin(&link, &bytes), &b),
            Err(Failure::NoReady)
        ));
        assert_eq!(b.used(), 0);
    }
    #[test]
    fn nonregular_source_is_rejected_without_a_blocking_read() {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("fifo");
        let name = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let b = Budget::new(1 << 20);
        assert!(matches!(
            Image::capture(&pin(&path, &elf()), &b),
            Err(Failure::AdapterInvariant)
        ));
        assert_eq!(b.used(), 0);
    }
    #[test]
    fn replace_and_remove_source_cannot_change_captured_image() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("input");
        let bytes = elf();
        std::fs::write(&path, &bytes).unwrap();
        let b = Budget::new(1 << 20);
        let image = Image::capture(&pin(&path, &bytes), &b).unwrap();
        std::fs::write(&path, b"changed").unwrap();
        std::fs::remove_file(&path).unwrap();
        let mut actual = vec![0; 64];
        image.file.read_exact_at(&mut actual, 0).unwrap();
        assert_eq!(actual, bytes);
        let command = image.command().unwrap();
        assert_eq!(command.as_std().get_program(), image.exec_path.as_os_str());
        assert!(image.exec_path.starts_with("/proc/self/fd"));
        assert!(
            unsafe { libc::fcntl(image.file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC != 0
        );
    }
    #[test]
    fn kernel_seals_reject_write_resize_and_execution_mode_changes() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("input");
        let bytes = elf();
        std::fs::write(&path, &bytes).unwrap();
        let image = Image::capture(&pin(&path, &bytes), &Budget::new(1 << 20)).unwrap();
        let fd = image.file.as_raw_fd();
        assert!(image.file.write_at(b"x", 0).is_err());
        assert_eq!(unsafe { libc::ftruncate(fd, 63) }, -1);
        assert_eq!(unsafe { libc::ftruncate(fd, 65) }, -1);
        assert_eq!(unsafe { libc::fchmod(fd, 0o600) }, -1);
        let mapping = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                64,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        assert_eq!(mapping, libc::MAP_FAILED);
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, libc::F_SEAL_FUTURE_WRITE) },
            -1
        );
        image.check_seals().unwrap();
    }
    #[test]
    fn shared_image_retains_one_charge_until_last_owner() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("input");
        let bytes = elf();
        std::fs::write(&path, &bytes).unwrap();
        let b = Budget::new(1 << 20);
        let image = Image::capture(&pin(&path, &bytes), &b).unwrap();
        let used = b.used();
        assert!(used >= bytes.len() + 4096);
        let other = image.clone();
        drop(image);
        assert_eq!(b.used(), used);
        other.check_seals().unwrap();
        drop(other);
        assert_eq!(b.used(), 0);
    }

    #[test]
    fn static_capture_rejects_legacy_header_and_refunds_before_command() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("image");
        let bytes = elf();
        std::fs::write(&path, &bytes).unwrap();
        let budget = Budget::new(1 << 20);
        assert!(matches!(
            Image::capture_static(&pin(&path, &bytes), &budget),
            Err(Failure::AdapterInvariant)
        ));
        assert_eq!(budget.used(), 0);
        // Existing generic capture continues to support its original profile.
        assert!(Image::capture(&pin(&path, &bytes), &budget).is_ok());
    }
    #[test]
    fn static_capture_owns_same_sealed_descriptor_and_lifetime_credit() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("image");
        let bytes = static_elf::fixture();
        std::fs::write(&path, &bytes).unwrap();
        let budget = Budget::new(1 << 20);
        let image = Image::capture_static(&pin(&path, &bytes), &budget).unwrap();
        let charge = budget.used();
        let other = image.clone();
        std::fs::remove_file(&path).unwrap();
        image.check_seals().unwrap();
        assert_eq!(
            image.command().unwrap().as_std().get_program(),
            image.exec_path.as_os_str()
        );
        drop(image);
        assert_eq!(budget.used(), charge);
        drop(other);
        assert_eq!(budget.used(), 0);
    }
}
