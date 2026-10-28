//! Explicit qualification-only immutable-filesystem admission. No fallback,
//! runtime selector, worker launch, mount operation or production default.
use super::{Image, SCRATCH};
use crate::validator_ipc::{
    Failure,
    controller::{Budget, budget::Credit},
};
use crate::{
    bundle_snapshot::Pin,
    validator_init_diagnostic::{InitDiagnostic, Stage},
};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::Read,
    os::{
        fd::AsRawFd,
        unix::fs::{FileExt, MetadataExt},
    },
    sync::{Arc, Mutex},
};

const MOUNT_BYTES: usize = 262_144;
const MOUNT_ENTRIES: usize = 1024;
const FDINFO_BYTES: usize = 4096;
const STATUS_BYTES: usize = 16_384;
const PATH_BYTES: usize = 4096;
const PATH_COMPONENTS: usize = 32;
const FIXED_BYTES: usize = 16_384;

/// Supported implementations, NOT discovery or a selected Lambda filesystem.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filesystem {
    Ext4,
    Squashfs,
    Erofs,
}
impl Filesystem {
    fn name(self) -> &'static str {
        match self {
            Self::Ext4 => "ext4",
            Self::Squashfs => "squashfs",
            Self::Erofs => "erofs",
        }
    }
    fn magic(self) -> u64 {
        match self {
            Self::Ext4 => 0xef53,
            Self::Squashfs => 0x73717368,
            Self::Erofs => 0xe0f5e1e2,
        }
    }
}

/// Trusted source construction only. Selecting a filesystem is not proof that
/// the platform supplies immutable backing. No Default/Deserialize or env input.
#[derive(Clone, Copy)]
pub struct Profile {
    filesystem: Filesystem,
    root: &'static str,
    path: &'static str,
    bytes: usize,
    sha256: [u8; 32],
}
impl Profile {
    pub const fn pinned_layer_candidate(filesystem: Filesystem) -> Self {
        Self {
            filesystem,
            root: "/opt/precision",
            path: "/opt/precision/precision-worker",
            bytes: 17_482_280,
            sha256: [
                222, 123, 78, 78, 161, 27, 129, 40, 48, 73, 172, 215, 6, 55, 22, 124, 171, 129,
                224, 45, 152, 194, 89, 206, 55, 73, 38, 253, 20, 135, 179, 234,
            ],
        }
    }
    /// Pure admission, including exact spelling; no filesystem or child work.
    pub fn validate_pin(&self, pin: &Pin) -> Result<(), Failure> {
        if pin.path.as_os_str().as_encoded_bytes() != self.path.as_bytes()
            || pin.bytes != self.bytes
            || pin.sha256 != self.sha256
            || pin.bytes < 64
            || self.path.len() > PATH_BYTES
            || self.path.split('/').count() > PATH_COMPONENTS
        {
            return Err(Failure::AdapterInvariant);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity {
    root_device: u64,
    root_inode: u64,
    root_mode: u32,
    device: u64,
    inode: u64,
    size: u64,
    mode: u32,
    mount_id: u64,
}

// Kept private: production can construct only Real. Tests inject finite fake
// observations, never a public Engine/kernel or runtime configuration hook.
trait Kernel: Send + Sync {
    fn open_root(&self, profile: Profile) -> Result<File, Failure>;
    fn open_worker(&self, root: &File) -> Result<File, Failure>;
    fn observe(
        &self,
        root: &File,
        file: &File,
        fs: Filesystem,
        scratch: &mut [u8],
    ) -> Result<Identity, Failure>;
}
struct Real;

pub(super) struct Protection {
    root: File,
    expected: Identity,
    filesystem: Filesystem,
    kernel: Arc<dyn Kernel>,
    // One precharged buffer, reused under the mutex for all command rechecks.
    scratch: Mutex<Vec<u8>>,
}
impl Protection {
    pub(super) fn recheck(&self, file: &File) -> Result<(), Failure> {
        let mut scratch = self.scratch.lock().map_err(|_| Failure::NoReady)?;
        if self
            .kernel
            .observe(&self.root, file, self.filesystem, &mut scratch)?
            != self.expected
        {
            return Err(Failure::NoReady);
        }
        Ok(())
    }
}

fn charge(pin: &Pin) -> Result<usize, Failure> {
    pin.bytes
        .checked_add(pin.bytes.min(SCRATCH))
        .and_then(|n| n.checked_add(MOUNT_BYTES + 1 + FIXED_BYTES))
        .and_then(|n| n.checked_add(pin.path.as_os_str().len().checked_add(1)?))
        .and_then(|n| {
            n.checked_add(std::mem::size_of::<Image>() + std::mem::size_of::<Protection>())
        })
        .ok_or(Failure::ParentBudget)
}

pub(in crate::validator_ipc) fn capture(
    pin: &Pin,
    profile: Profile,
    budget: &Arc<Budget>,
    diagnostic: Option<Arc<InitDiagnostic>>,
) -> Result<Arc<Image>, Failure> {
    // Charge before the kernel owner, scratch allocation or any open.
    profile.validate_pin(pin)?;
    let credit = budget.reserve(charge(pin)?)?;
    capture_owned(pin, profile, credit, diagnostic, Arc::new(Real))
}
fn capture_owned(
    pin: &Pin,
    profile: Profile,
    credit: Credit,
    diagnostic: Option<Arc<InitDiagnostic>>,
    kernel: Arc<dyn Kernel>,
) -> Result<Arc<Image>, Failure> {
    let mut stage = Stage::ImageReadonlyOpen;
    let result = (|| {
        let mut scratch = Vec::new();
        scratch
            .try_reserve_exact(MOUNT_BYTES + 1)
            .map_err(|_| Failure::ParentBudget)?;
        scratch.resize(MOUNT_BYTES + 1, 0);
        let root = kernel.open_root(profile)?;
        let file = kernel.open_worker(&root)?;
        stage = Stage::ImageReadonlyProof;
        let identity = kernel.observe(&root, &file, profile.filesystem, &mut scratch)?;
        if identity.size != pin.bytes as u64 || identity.mode != libc::S_IFREG | 0o555 {
            return Err(Failure::AdapterInvariant);
        }
        stage = Stage::ImageReadonlyHash;
        let mut digest = Sha256::new();
        let mut at = 0usize;
        while at < pin.bytes {
            let n = (pin.bytes - at).min(SCRATCH);
            file.read_exact_at(&mut scratch[..n], at as u64)
                .map_err(|_| Failure::AdapterInvariant)?;
            digest.update(&scratch[..n]);
            at += n;
        }
        let mut extra = [0];
        if file
            .read_at(&mut extra, pin.bytes as u64)
            .map_err(|_| Failure::NoReady)?
            != 0
            || <[u8; 32]>::from(digest.finalize()) != pin.sha256
        {
            return Err(Failure::AdapterInvariant);
        }
        stage = Stage::ImageElfInspect;
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        super::static_elf::validate(&file, pin.bytes as u64)?;
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
        return Err(Failure::NoReady);
        stage = Stage::ImageReadonlyProof;
        if kernel.observe(&root, &file, profile.filesystem, &mut scratch)? != identity {
            return Err(Failure::NoReady);
        }
        let image = Image {
            exec_path: format!("/proc/self/fd/{}", file.as_raw_fd()).into(),
            file,
            readonly: Some(Protection {
                root,
                expected: identity,
                filesystem: profile.filesystem,
                kernel,
                scratch: Mutex::new(scratch),
            }),
            _credit: credit,
            diagnostic: diagnostic.clone(),
        };
        Ok(Arc::new(image))
    })();
    if let Err(failure) = &result
        && let Some(d) = &diagnostic
    {
        d.note(stage, Some(failure));
    }
    result
}

fn text(bytes: &[u8]) -> Result<&str, Failure> {
    if bytes.is_empty() || !bytes.ends_with(b"\n") || bytes.contains(&0) {
        return Err(Failure::NoReady);
    }
    std::str::from_utf8(bytes).map_err(|_| Failure::NoReady)
}
fn number(value: &str) -> Result<u64, Failure> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Failure::NoReady);
    }
    value.parse().map_err(|_| Failure::NoReady)
}
fn mount_id(bytes: &[u8]) -> Result<u64, Failure> {
    if bytes.len() > FDINFO_BYTES {
        return Err(Failure::NoReady);
    }
    let mut found = None;
    for line in text(bytes)?.lines() {
        if let Some(value) = line.strip_prefix("mnt_id:") {
            if found.is_some() {
                return Err(Failure::NoReady);
            }
            let id = number(value.trim())?;
            if id == 0 {
                return Err(Failure::NoReady);
            }
            found = Some(id);
        }
    }
    found.ok_or(Failure::NoReady)
}
fn readonly_options(options: &str, superblock: bool) -> Result<(), Failure> {
    let mut ro = 0;
    for option in options.split(',') {
        match option {
            "ro" => ro += 1,
            "nosuid" | "nodev" | "noatime" | "nodiratime" | "relatime" | "strictatime"
            | "lazytime" | "sync" | "dirsync" => {}
            "errors=remount-ro" | "data=ordered" | "seclabel" if superblock => {}
            _ => return Err(Failure::NoReady), // includes rw/noexec/unknown/empty
        }
    }
    if ro != 1 {
        return Err(Failure::NoReady);
    }
    Ok(())
}
fn mount_proof(bytes: &[u8], id: u64, device: (u32, u32), fs: Filesystem) -> Result<(), Failure> {
    if bytes.len() > MOUNT_BYTES {
        return Err(Failure::NoReady);
    }
    let mut seen = [0u64; MOUNT_ENTRIES];
    let mut found = false;
    for (index, line) in text(bytes)?.lines().enumerate() {
        if index >= MOUNT_ENTRIES {
            return Err(Failure::NoReady);
        }
        let mut fields = line.split(' ');
        let current = number(fields.next().ok_or(Failure::NoReady)?)?;
        if current == 0 || seen[..index].contains(&current) {
            return Err(Failure::NoReady);
        }
        seen[index] = current;
        number(fields.next().ok_or(Failure::NoReady)?)?;
        let dev = fields.next().ok_or(Failure::NoReady)?;
        let root = fields.next().ok_or(Failure::NoReady)?;
        let point = fields.next().ok_or(Failure::NoReady)?;
        let opts = fields.next().ok_or(Failure::NoReady)?;
        if root.is_empty() || point.is_empty() || !root.starts_with('/') || !point.starts_with('/')
        {
            return Err(Failure::NoReady);
        }
        // Escaped/ambiguous selected paths fail closed rather than decoding to
        // another trusted root. Other mounts do not establish our proof.
        if current == id && (root.contains('\\') || point.contains('\\')) {
            return Err(Failure::NoReady);
        }
        loop {
            let optional = fields.next().ok_or(Failure::NoReady)?;
            if optional == "-" {
                break;
            }
            if optional == "unbindable" {
                continue;
            }
            let (key, value) = optional.split_once(':').ok_or(Failure::NoReady)?;
            if !matches!(key, "shared" | "master" | "propagate_from") {
                return Err(Failure::NoReady);
            }
            number(value)?;
        }
        let kind = fields.next().ok_or(Failure::NoReady)?;
        let source = fields.next().ok_or(Failure::NoReady)?;
        let super_opts = fields.next().ok_or(Failure::NoReady)?;
        if kind.is_empty() || source.is_empty() || fields.next().is_some() {
            return Err(Failure::NoReady);
        }
        if current == id {
            let (major, minor) = dev.split_once(':').ok_or(Failure::NoReady)?;
            if number(major)? != u64::from(device.0)
                || number(minor)? != u64::from(device.1)
                || kind != fs.name()
            {
                return Err(Failure::NoReady);
            }
            readonly_options(opts, false)?;
            readonly_options(super_opts, true)?;
            found = true;
        }
    }
    if !found {
        return Err(Failure::NoReady);
    }
    Ok(())
}
fn unprivileged(bytes: &[u8]) -> Result<(), Failure> {
    if bytes.len() > STATUS_BYTES {
        return Err(Failure::NoReady);
    }
    let mut fields = 0u8;
    for line in text(bytes)?.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let bit = match key {
            "Uid" => 1,
            "CapInh" => 2,
            "CapPrm" => 4,
            "CapEff" => 8,
            "CapAmb" => 16,
            _ => continue,
        };
        if fields & bit != 0 {
            return Err(Failure::NoReady);
        }
        fields |= bit;
        if key == "Uid" {
            let mut uid = None;
            let mut count = 0;
            for field in value.split_ascii_whitespace() {
                let n = u32::try_from(number(field)?).map_err(|_| Failure::NoReady)?;
                if n == 0 || uid.is_some_and(|first| first != n) {
                    return Err(Failure::NoReady);
                }
                uid = Some(n);
                count += 1;
            }
            if count != 4 {
                return Err(Failure::NoReady);
            }
        } else {
            let value = value.trim();
            if value.is_empty()
                || value.len() > 16
                || !value.bytes().all(|b| b.is_ascii_hexdigit())
                || u64::from_str_radix(value, 16).map_err(|_| Failure::NoReady)? != 0
            {
                return Err(Failure::NoReady);
            }
        }
    }
    if fields != 31 {
        return Err(Failure::NoReady);
    }
    Ok(())
}

fn backing_flags(fs: Filesystem, magic: u64, flags: u64) -> Result<(), Failure> {
    if magic != fs.magic() || flags & libc::ST_RDONLY == 0 || flags & libc::ST_NOEXEC != 0 {
        return Err(Failure::NoReady);
    }
    Ok(())
}
fn same_mount(
    root_id: u64,
    file_id: u64,
    root_device: u64,
    file_device: u64,
) -> Result<(), Failure> {
    if root_id == 0 || root_id != file_id || root_device != file_device {
        return Err(Failure::NoReady);
    }
    Ok(())
}
fn absent_capabilities(result: isize, errno: Option<i32>) -> Result<(), Failure> {
    if result != -1 || errno != Some(libc::ENODATA) {
        return Err(Failure::NoReady);
    }
    Ok(())
}

fn kernel_procfs(magic: u64) -> Result<(), Failure> {
    if magic != 0x9fa0 {
        return Err(Failure::NoReady);
    }
    Ok(())
}

fn read_bounded(file: &mut File, scratch: &mut [u8], limit: usize) -> Result<usize, Failure> {
    if scratch.len() <= limit {
        return Err(Failure::ParentBudget);
    }
    let mut used = 0;
    loop {
        let n = file
            .read(&mut scratch[used..=limit])
            .map_err(|_| Failure::NoReady)?;
        if n == 0 {
            return Ok(used);
        }
        used += n;
        if used > limit {
            return Err(Failure::NoReady);
        }
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod linux {
    use super::*;
    use std::{
        fs::OpenOptions,
        os::{
            fd::{FromRawFd, RawFd},
            unix::fs::OpenOptionsExt,
        },
    };
    fn owned(raw: RawFd) -> Result<File, Failure> {
        if raw < 0 {
            return Err(Failure::NoReady);
        }
        let file = unsafe { File::from_raw_fd(raw) };
        if raw >= 3 {
            return Ok(file);
        }
        let duplicate = unsafe { libc::fcntl(raw, libc::F_DUPFD_CLOEXEC, 3) };
        if duplicate < 0 {
            return Err(Failure::NoReady);
        }
        Ok(unsafe { File::from_raw_fd(duplicate) })
    }
    fn proc_read(
        path: &std::path::Path,
        buffer: &mut [u8],
        limit: usize,
    ) -> Result<usize, Failure> {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(|_| Failure::NoReady)?;
        // Evidence must come from the opened procfs object, not merely a path
        // named /proc containing ordinary files with plausible text.
        let mut fs = std::mem::MaybeUninit::<libc::statfs>::uninit();
        if unsafe { libc::fstatfs(file.as_raw_fd(), fs.as_mut_ptr()) } != 0 {
            return Err(Failure::NoReady);
        }
        kernel_procfs(unsafe { fs.assume_init() }.f_type as u64)?;
        read_bounded(&mut file, buffer, limit)
    }
    fn fd_mount(file: &File, scratch: &mut [u8]) -> Result<u64, Failure> {
        let path = format!("/proc/self/fdinfo/{}", file.as_raw_fd());
        let n = proc_read(std::path::Path::new(&path), scratch, FDINFO_BYTES)?;
        mount_id(&scratch[..n])
    }
    fn filesystem(file: &File, expected: Filesystem) -> Result<(), Failure> {
        let mut fs = std::mem::MaybeUninit::<libc::statfs>::uninit();
        let mut flags = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        let fd = file.as_raw_fd();
        if unsafe { libc::fstatfs(fd, fs.as_mut_ptr()) } != 0
            || unsafe { libc::fstatvfs(fd, flags.as_mut_ptr()) } != 0
        {
            return Err(Failure::NoReady);
        }
        let fs = unsafe { fs.assume_init() };
        let flags = unsafe { flags.assume_init() };
        backing_flags(expected, fs.f_type as u64, flags.f_flag)
    }
    fn descriptor(file: &File) -> Result<(), Failure> {
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
        let access = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        if file.as_raw_fd() < 3
            || flags < 0
            || flags & libc::FD_CLOEXEC == 0
            || access < 0
            || access & libc::O_ACCMODE != libc::O_RDONLY
        {
            return Err(Failure::NoReady);
        }
        Ok(())
    }
    impl Kernel for Real {
        fn open_root(&self, profile: Profile) -> Result<File, Failure> {
            if profile.root != "/opt/precision" {
                return Err(Failure::AdapterInvariant);
            }
            let flags = libc::O_RDONLY
                | libc::O_DIRECTORY
                | libc::O_CLOEXEC
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK;
            let root = owned(unsafe { libc::open(c"/".as_ptr(), flags) })?;
            let opt = owned(unsafe { libc::openat(root.as_raw_fd(), c"opt".as_ptr(), flags) })?;
            owned(unsafe { libc::openat(opt.as_raw_fd(), c"precision".as_ptr(), flags) })
        }
        fn open_worker(&self, root: &File) -> Result<File, Failure> {
            owned(unsafe {
                libc::openat(
                    root.as_raw_fd(),
                    c"precision-worker".as_ptr(),
                    libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                )
            })
        }
        fn observe(
            &self,
            root: &File,
            file: &File,
            fs: Filesystem,
            scratch: &mut [u8],
        ) -> Result<Identity, Failure> {
            descriptor(root)?;
            descriptor(file)?;
            let r = root.metadata().map_err(|_| Failure::NoReady)?;
            let f = file.metadata().map_err(|_| Failure::NoReady)?;
            if !r.is_dir()
                || !f.is_file()
                || f.mode() != libc::S_IFREG | 0o555
                || r.dev() != f.dev()
            {
                return Err(Failure::NoReady);
            }
            filesystem(root, fs)?;
            filesystem(file, fs)?;
            let root_mount = fd_mount(root, scratch)?;
            let file_mount = fd_mount(file, scratch)?;
            same_mount(root_mount, file_mount, r.dev(), f.dev())?;
            let n = proc_read(
                std::path::Path::new("/proc/self/mountinfo"),
                scratch,
                MOUNT_BYTES,
            )?;
            mount_proof(
                &scratch[..n],
                file_mount,
                (libc::major(f.dev()), libc::minor(f.dev())),
                fs,
            )?;
            let n = proc_read(
                std::path::Path::new("/proc/thread-self/status"),
                scratch,
                STATUS_BYTES,
            )?;
            unprivileged(&scratch[..n])?;
            // Only ENODATA establishes absence; unsupported/denied is unknown.
            let capabilities = unsafe {
                libc::fgetxattr(
                    file.as_raw_fd(),
                    c"security.capability".as_ptr(),
                    std::ptr::null_mut(),
                    0,
                )
            };
            absent_capabilities(capabilities, std::io::Error::last_os_error().raw_os_error())?;
            Ok(Identity {
                root_device: r.dev(),
                root_inode: r.ino(),
                root_mode: r.mode(),
                device: f.dev(),
                inode: f.ino(),
                size: f.len(),
                mode: f.mode(),
                mount_id: file_mount,
            })
        }
    }
}
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
impl Kernel for Real {
    fn open_root(&self, _: Profile) -> Result<File, Failure> {
        Err(Failure::NoReady)
    }
    fn open_worker(&self, _: &File) -> Result<File, Failure> {
        Err(Failure::NoReady)
    }
    fn observe(
        &self,
        _: &File,
        _: &File,
        _: Filesystem,
        _: &mut [u8],
    ) -> Result<Identity, Failure> {
        Err(Failure::NoReady)
    }
}

#[cfg(all(test, target_os = "linux", target_arch = "x86_64"))]
#[path = "readonly_executable_tests.rs"]
mod tests;
