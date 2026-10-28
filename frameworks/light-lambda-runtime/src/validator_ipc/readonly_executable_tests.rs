//! NON-CHILD tests. Positive mount/privilege observations are FAKE, never host
//! immutability proof. Only header-shaped non-runnable ELF fixture data is used.
use super::*;
use std::{
    os::unix::fs::PermissionsExt,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
};

struct Fake {
    dir: tempfile::TempDir,
    opens: AtomicUsize,
    observations: AtomicUsize,
    refuse: AtomicBool,
    change_at: AtomicUsize,
}
impl Fake {
    fn fixture(bytes: &[u8]) -> Arc<Self> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data"), bytes).unwrap();
        std::fs::set_permissions(
            dir.path().join("data"),
            std::fs::Permissions::from_mode(0o555),
        )
        .unwrap();
        Arc::new(Self {
            dir,
            opens: AtomicUsize::new(0),
            observations: AtomicUsize::new(0),
            refuse: AtomicBool::new(false),
            change_at: AtomicUsize::new(usize::MAX),
        })
    }
}
impl Kernel for Fake {
    fn open_root(&self, _: Profile) -> Result<File, Failure> {
        self.opens.fetch_add(1, SeqCst);
        File::open(self.dir.path()).map_err(|_| Failure::NoReady)
    }
    fn open_worker(&self, _: &File) -> Result<File, Failure> {
        self.opens.fetch_add(1, SeqCst);
        File::open(self.dir.path().join("data")).map_err(|_| Failure::NoReady)
    }
    fn observe(
        &self,
        root: &File,
        file: &File,
        _: Filesystem,
        _: &mut [u8],
    ) -> Result<Identity, Failure> {
        let n = self.observations.fetch_add(1, SeqCst);
        if self.refuse.load(SeqCst) {
            return Err(Failure::NoReady);
        }
        let r = root.metadata().unwrap();
        let f = file.metadata().unwrap();
        Ok(Identity {
            root_device: r.dev(),
            root_inode: r.ino(),
            root_mode: r.mode(),
            device: f.dev(),
            inode: f.ino(),
            size: f.len(),
            mode: f.mode(),
            mount_id: if n >= self.change_at.load(SeqCst) {
                99
            } else {
                12
            },
        })
    }
}
fn fixture() -> Vec<u8> {
    super::super::static_elf::fixture()
}
fn profile(bytes: &[u8]) -> Profile {
    Profile {
        filesystem: Filesystem::Ext4,
        root: "/fake/readonly",
        path: "/fake/readonly/precision-worker",
        bytes: bytes.len(),
        sha256: Sha256::digest(bytes).into(),
    }
}
fn pin(profile: Profile) -> Pin {
    Pin {
        path: profile.path.into(),
        bytes: profile.bytes,
        sha256: profile.sha256,
    }
}
fn capture_fake(
    profile: Profile,
    pin: &Pin,
    budget: &Arc<Budget>,
    fake: Arc<Fake>,
) -> Result<Arc<Image>, Failure> {
    profile.validate_pin(pin)?;
    let credit = budget.reserve(charge(pin)?)?;
    capture_owned(pin, profile, credit, None, fake)
}
fn mount(options: &str, super_options: &str) -> String {
    format!("12 1 8:1 / /opt/precision {options} - ext4 /dev/synthetic {super_options}\n")
}
fn status() -> &'static [u8] {
    b"Uid:\t1000\t1000\t1000\t1000\nCapInh:\t0000000000000000\nCapPrm:\t0000000000000000\nCapEff:\t0000000000000000\nCapAmb:\t0000000000000000\n"
}

#[test]
fn r01_fake_positive_captures_static_data_and_builds_only_fd_command() {
    let bytes = fixture();
    let p = profile(&bytes);
    let fake = Fake::fixture(&bytes);
    let b = Budget::new(2 << 20);
    let image = capture_fake(p, &pin(p), &b, fake.clone()).unwrap();
    assert_eq!(fake.opens.load(SeqCst), 2);
    assert_eq!(fake.observations.load(SeqCst), 2);
    let command = image.command().unwrap();
    assert_eq!(command.as_std().get_program(), image.exec_path.as_os_str());
    assert!(image.requires_no_new_privileges());
    assert!(image.exec_path.starts_with("/proc/self/fd"));
    // Never spawn this command. Its ELF bytes are non-runnable fixture data.
    assert!(b.used() > bytes.len());
    drop(image);
    assert_eq!(b.used(), 0);
}
#[test]
fn r02_readonly_bind_over_writable_superblock_rejects() {
    assert!(mount_proof(mount("ro", "rw").as_bytes(), 12, (8, 1), Filesystem::Ext4).is_err());
}
#[test]
fn r03_both_readonly_exact_local_filesystem_and_device_required() {
    let valid = mount("ro,nosuid,nodev", "ro,errors=remount-ro");
    assert!(mount_proof(valid.as_bytes(), 12, (8, 1), Filesystem::Ext4).is_ok());
    for bad in [
        mount("rw", "ro"),
        valid.replace("ext4", "overlay"),
        valid.replace("ext4", "nfs"),
        valid.replace("8:1", "8:2"),
        valid.replace("ext4", "squashfs"),
    ] {
        assert!(mount_proof(bad.as_bytes(), 12, (8, 1), Filesystem::Ext4).is_err());
    }
    assert_ne!(Filesystem::Ext4.magic(), Filesystem::Erofs.magic());
    assert_ne!(Filesystem::Ext4.magic(), Filesystem::Squashfs.magic());
}
#[test]
fn r04_rw_noexec_and_ambiguous_option_tokens_reject() {
    assert!(backing_flags(Filesystem::Ext4, Filesystem::Ext4.magic(), libc::ST_RDONLY).is_ok());
    for flags in [0, libc::ST_NOEXEC, libc::ST_RDONLY | libc::ST_NOEXEC] {
        assert!(backing_flags(Filesystem::Ext4, Filesystem::Ext4.magic(), flags).is_err());
    }
    assert!(backing_flags(Filesystem::Ext4, Filesystem::Erofs.magic(), libc::ST_RDONLY).is_err());
    for options in [
        "rw",
        "ro,noexec",
        "ro,rw",
        "ro,ro",
        "errors=remount-ro",
        "ro,unknown",
        "ro,",
    ] {
        assert!(
            mount_proof(
                mount(options, "ro").as_bytes(),
                12,
                (8, 1),
                Filesystem::Ext4
            )
            .is_err()
        );
    }
}
#[test]
fn r05_fdinfo_requires_exact_single_numeric_mount_id() {
    assert_eq!(mount_id(b"pos:\t0\nmnt_id:\t12\n").unwrap(), 12);
    for bad in [
        b"pos: 0\n".as_slice(),
        b"mnt_id: 12",
        b"mnt_id: 12\nmnt_id: 12\n",
        b"mnt_id: +12\n",
        b"mnt_id: 0\n",
        b"mnt_id: 18446744073709551616\n",
    ] {
        assert!(mount_id(bad).is_err());
    }
    assert!(mount_proof(mount("ro", "ro").as_bytes(), 13, (8, 1), Filesystem::Ext4).is_err());
}
#[test]
fn r06_mount_inventory_is_bounded_and_duplicate_or_truncated_rejects() {
    let valid = mount("ro", "ro");
    for bad in [
        valid.repeat(2),
        valid.trim_end().to_string(),
        "x".repeat(MOUNT_BYTES + 1),
        valid.replace("12 1", "+12 1"),
    ] {
        assert!(mount_proof(bad.as_bytes(), 12, (8, 1), Filesystem::Ext4).is_err());
    }
    let mut many = String::new();
    for id in 1..=MOUNT_ENTRIES + 1 {
        many.push_str(&format!("{id} 0 8:1 / / ro - ext4 /dev/fake ro\n"));
    }
    assert!(mount_proof(many.as_bytes(), 12, (8, 1), Filesystem::Ext4).is_err());
}
#[test]
fn r07_escaped_paths_unknown_fields_and_malformed_grammar_reject() {
    let valid = mount("ro", "ro");
    for bad in [
        valid.replace("/opt/precision", r"/opt/pri\040cision"),
        valid.replace(" - ", " unknown:1 - "),
        valid.replace(" - ", "  - "),
        valid.replace(" - ", " shared:bad - "),
        valid.replace("ro -", "ro - -"),
        valid.replace("12 1", "12  1"),
    ] {
        assert!(mount_proof(bad.as_bytes(), 12, (8, 1), Filesystem::Ext4).is_err());
    }
    assert!(
        mount_proof(
            valid.replace(" - ", " shared:2 - ").as_bytes(),
            12,
            (8, 1),
            Filesystem::Ext4
        )
        .is_ok()
    );
}
#[test]
fn r08_unavailable_proof_closes_capture_without_command_or_retained_credit() {
    assert!(kernel_procfs(0x9fa0).is_ok());
    for magic in [0, Filesystem::Ext4.magic(), Filesystem::Erofs.magic()] {
        assert!(kernel_procfs(magic).is_err());
    }
    let bytes = fixture();
    let p = profile(&bytes);
    let fake = Fake::fixture(&bytes);
    fake.refuse.store(true, SeqCst);
    let b = Budget::new(2 << 20);
    assert!(matches!(
        capture_fake(p, &pin(p), &b, fake),
        Err(Failure::NoReady)
    ));
    assert_eq!(b.used(), 0);
    assert!(mount_id(b"").is_err());
    assert!(mount_proof(b"", 12, (8, 1), Filesystem::Ext4).is_err());
    assert!(unprivileged(b"").is_err());
}
#[test]
fn r09_exact_pin_spelling_rejects_relative_dot_symlink_alias_and_overrides_before_open() {
    let bytes = fixture();
    let p = profile(&bytes);
    let fake = Fake::fixture(&bytes);
    let b = Budget::new(2 << 20);
    for path in [
        "relative",
        "/fake/readonly/../readonly/precision-worker",
        "/fake/readonly/./precision-worker",
        "/fake//readonly/precision-worker",
        "/fake/alias",
        "/other/precision-worker",
    ] {
        let mut v = pin(p);
        v.path = path.into();
        assert!(matches!(
            capture_fake(p, &v, &b, fake.clone()),
            Err(Failure::AdapterInvariant)
        ));
    }
    assert_eq!(fake.opens.load(SeqCst), 0);
}
#[test]
fn r10_nonregular_and_symlink_opens_are_refused_by_production_flags() {
    // Owned temp fixtures only, never /opt or a real worker. Exercise the exact
    // nonblocking/nofollow flags separately from the fake positive mount proof.
    use std::{
        ffi::CString,
        os::unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    };
    let d = tempfile::tempdir().unwrap();
    let fifo = d.path().join("fifo");
    let c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&fifo)
        .unwrap();
    assert!(!file.metadata().unwrap().is_file());
    let link = d.path().join("link");
    std::os::unix::fs::symlink(&fifo, &link).unwrap();
    assert!(
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(link)
            .is_err()
    );
}
#[test]
fn r11_device_and_mount_relationships_are_exact() {
    assert!(same_mount(12, 12, 8, 8).is_ok());
    for (r, f, rd, fd) in [(0, 0, 8, 8), (12, 13, 8, 8), (12, 12, 8, 9)] {
        assert!(same_mount(r, f, rd, fd).is_err());
    }
    assert!(mount_proof(mount("ro", "ro").as_bytes(), 12, (9, 1), Filesystem::Ext4).is_err());
    assert!(mount_proof(mount("ro", "ro").as_bytes(), 11, (8, 1), Filesystem::Ext4).is_err());
}
#[test]
fn r12_size_hash_extra_and_short_data_refund() {
    let bytes = fixture();
    let p = profile(&bytes);
    let mut long = bytes.clone();
    long.push(0);
    let short = &bytes[..bytes.len() - 1];
    let mut changed = bytes.clone();
    changed[511] ^= 1;
    for data in [&long[..], short, &changed[..]] {
        let b = Budget::new(2 << 20);
        assert!(capture_fake(p, &pin(p), &b, Fake::fixture(data)).is_err());
        assert_eq!(b.used(), 0);
    }
}
#[test]
fn r13_privilege_status_requires_all_zero_caps_and_nonroot_matching_uids() {
    assert!(absent_capabilities(-1, Some(libc::ENODATA)).is_ok());
    for (result, errno) in [
        (0, None),
        (20, None),
        (-1, Some(libc::EACCES)),
        (-1, Some(libc::EOPNOTSUPP)),
        (-1, None),
    ] {
        assert!(absent_capabilities(result, errno).is_err());
    }
    assert!(unprivileged(status()).is_ok());
    let valid = std::str::from_utf8(status()).unwrap();
    for bad in [
        valid.replace("1000", "0"),
        valid.replace("1000", "4294967296"),
        valid.replacen("1000", "1001", 1),
        valid.replace("0000000000000000", "0000000000000001"),
        valid.replace("CapAmb:", "CapUnknown:"),
        format!("{valid}CapEff: 0\n"),
        valid.replace("0000000000000000", "-0"),
    ] {
        assert!(unprivileged(bad.as_bytes()).is_err());
    }
    let bytes = fixture();
    let p = profile(&bytes);
    let fake = Fake::fixture(&bytes);
    std::fs::set_permissions(
        fake.dir.path().join("data"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let b = Budget::new(2 << 20);
    assert!(capture_fake(p, &pin(p), &b, fake).is_err());
    assert_eq!(b.used(), 0);
}
#[test]
fn r14_existing_static_parser_refuses_interpreter_needed_and_wx() {
    let base = fixture();
    for kind in 0..3 {
        let mut bytes = base.clone();
        match kind {
            0 => bytes[120..124].copy_from_slice(&3u32.to_le_bytes()),
            1 => bytes[256..264].copy_from_slice(&1u64.to_le_bytes()),
            _ => bytes[68..72].copy_from_slice(&7u32.to_le_bytes()),
        };
        let p = profile(&bytes);
        let b = Budget::new(2 << 20);
        assert!(matches!(
            capture_fake(p, &pin(p), &b, Fake::fixture(&bytes)),
            Err(Failure::AdapterInvariant)
        ));
        assert_eq!(b.used(), 0);
    }
}
#[test]
fn r15_recheck_changed_or_unavailable_evidence_refuses_before_spawn() {
    let bytes = fixture();
    let p = profile(&bytes);
    let fake = Fake::fixture(&bytes);
    let b = Budget::new(2 << 20);
    let image = capture_fake(p, &pin(p), &b, fake.clone()).unwrap();
    fake.change_at.store(2, SeqCst);
    assert!(matches!(image.command(), Err(Failure::NoReady)));
    assert_eq!(fake.opens.load(SeqCst), 2);
    fake.refuse.store(true, SeqCst);
    assert!(image.command().is_err());
    drop(image);
    assert_eq!(b.used(), 0);
}
#[test]
fn r16_unlink_and_path_replacement_never_reopens_captured_descriptor() {
    let bytes = fixture();
    let p = profile(&bytes);
    let fake = Fake::fixture(&bytes);
    let b = Budget::new(2 << 20);
    let image = capture_fake(p, &pin(p), &b, fake.clone()).unwrap();
    std::fs::remove_file(fake.dir.path().join("data")).unwrap();
    std::fs::write(fake.dir.path().join("data"), b"replacement").unwrap();
    assert!(image.command().is_ok());
    assert_eq!(fake.opens.load(SeqCst), 2);
    let mut actual = vec![0; bytes.len()];
    image.file.read_exact_at(&mut actual, 0).unwrap();
    assert_eq!(actual, bytes);
}
#[test]
fn r17_budget_refusal_precedes_open_and_overflow_is_checked() {
    let bytes = fixture();
    let p = profile(&bytes);
    let fake = Fake::fixture(&bytes);
    let b = Budget::new(1);
    assert!(matches!(
        capture_fake(p, &pin(p), &b, fake.clone()),
        Err(Failure::ParentBudget)
    ));
    assert_eq!(fake.opens.load(SeqCst), 0);
    assert_eq!(b.used(), 0);
    let mut v = pin(p);
    v.bytes = usize::MAX;
    assert!(matches!(charge(&v), Err(Failure::ParentBudget)));
}
#[test]
fn r18_failure_after_capture_observation_does_not_refund_a_live_owner() {
    let bytes = fixture();
    let p = profile(&bytes);
    let fake = Fake::fixture(&bytes);
    fake.change_at.store(1, SeqCst);
    let b = Budget::new(2 << 20);
    assert!(matches!(
        capture_fake(p, &pin(p), &b, fake),
        Err(Failure::NoReady)
    ));
    assert_eq!(b.used(), 0);
}
#[test]
fn r19_last_arc_retains_one_descriptor_and_credit_owner() {
    let bytes = fixture();
    let p = profile(&bytes);
    let b = Budget::new(2 << 20);
    let image = capture_fake(p, &pin(p), &b, Fake::fixture(&bytes)).unwrap();
    let amount = b.used();
    let second = image.clone();
    drop(image);
    assert_eq!(b.used(), amount);
    drop(second);
    assert_eq!(b.used(), 0);
    // This proves image ownership only, NOT an actual child wait/reap.
}
#[test]
fn r20_no_new_privileges_failure_stops_only_the_readonly_branch() {
    use crate::validator_ipc::controller::readonly_pre_exec;
    let calls = AtomicUsize::new(0);
    assert!(
        readonly_pre_exec(true, || {
            calls.fetch_add(1, SeqCst);
            Err(std::io::Error::from_raw_os_error(libc::EPERM))
        })
        .is_err()
    );
    assert!(
        readonly_pre_exec(false, || {
            calls.fetch_add(1, SeqCst);
            panic!("sealed branch must not call")
        })
        .is_ok()
    );
    assert_eq!(calls.load(SeqCst), 1);
}
#[test]
fn r21_readonly_capture_is_distinct_and_does_not_claim_memfd_seals() {
    let bytes = fixture();
    let p = profile(&bytes);
    let b = Budget::new(2 << 20);
    let image = capture_fake(p, &pin(p), &b, Fake::fixture(&bytes)).unwrap();
    assert!(image.readonly.is_some());
    assert!(image.check_seals().is_err());
    assert!(image.command().is_ok());
}
#[test]
fn r22_published_profile_is_fixed_and_has_no_runtime_selector() {
    let p = Profile::pinned_layer_candidate(Filesystem::Ext4);
    assert_eq!(p.path, "/opt/precision/precision-worker");
    assert_eq!(p.bytes, 17_482_280);
    let mut v = pin(p);
    assert!(p.validate_pin(&v).is_ok());
    v.sha256[0] ^= 1;
    assert!(p.validate_pin(&v).is_err());
    v = pin(p);
    v.bytes -= 1;
    assert!(p.validate_pin(&v).is_err());
}
#[test]
fn r23_bounded_reads_refuse_oversized_or_unterminated_proc_data() {
    let mut file = tempfile::tempfile().unwrap();
    std::io::Write::write_all(&mut file, b"12345").unwrap();
    std::io::Seek::rewind(&mut file).unwrap();
    let mut scratch = [0; 5];
    assert!(read_bounded(&mut file, &mut scratch, 4).is_err());
    assert!(unprivileged(&vec![b'x'; STATUS_BYTES + 1]).is_err());
    assert!(mount_id(&vec![b'x'; FDINFO_BYTES + 1]).is_err());
    assert!(text(b"secret\0\n").is_err());
}
#[test]
fn r24_finite_diagnostic_stages_cannot_retain_proc_or_error_payloads() {
    use std::io::Write;
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Self;
        fn make_writer(&'a self) -> Self {
            self.clone()
        }
    }
    for stage in [
        Stage::ImageReadonlyOpen,
        Stage::ImageReadonlyProof,
        Stage::ImageReadonlyHash,
        Stage::ImageReadonlyRecheck,
    ] {
        for level in [
            tracing::level_filters::LevelFilter::INFO,
            tracing::level_filters::LevelFilter::DEBUG,
            tracing::level_filters::LevelFilter::TRACE,
        ] {
            let writer = Capture::default();
            let inner = tracing_subscriber::fmt()
                .json()
                .without_time()
                .with_current_span(true)
                .with_span_list(true)
                .with_max_level(level)
                .with_writer(writer.clone())
                .finish();
            tracing::subscriber::with_default(
                crate::log_privacy::Privacy(tracing_core::Dispatch::new(inner)),
                || {
                    let outer = tracing::info_span!(
                        "PRIVATE_PARENT",
                        authorization = "PRIVATE_TOKEN",
                        source = "PRIVATE_MOUNT"
                    );
                    let _entered = outer.enter();
                    let diagnostic = InitDiagnostic::default();
                    diagnostic.note_io(
                        stage,
                        &std::io::Error::other("PRIVATE_PAYLOAD /PRIVATE_PATH stack"),
                    );
                    diagnostic.note(Stage::EngineInitialize, Some(&Failure::Protocol));
                    diagnostic.emit_after_run();
                },
            );
            let raw = String::from_utf8(writer.0.lock().unwrap().clone()).unwrap();
            assert!(!raw.contains("PRIVATE_"));
            let rows: Vec<serde_json::Value> = raw
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(rows.len(), 1);
            assert_eq!(
                rows[0]["fields"],
                serde_json::json!({"stage":stage.label(),"failure":"no_ready_other_io"})
            );
        }
    }
}

#[tokio::test]
async fn readonly_preparation_deadline_and_closed_drain_refuse_before_capture() {
    use crate::validator_ipc::{
        Settings,
        controller::{Controller, SourceFormat},
        ownership::Drain,
        supervision::{Authority, Policy},
    };
    let p = Profile::pinned_layer_candidate(Filesystem::Ext4);
    let pin = pin(p);
    let policy = Policy {
        platform_bytes: 8 << 20,
        parent_requested_bytes: 2 << 20,
        worker_requested_bytes: 2 << 20,
        headroom_bytes: 4 << 20,
        worker_address_space_bytes: 4 << 20,
        lifetime_cpu_seconds: 1,
    };
    let drain = Drain::new();
    let result = Controller::prepare_supervised_async_selected(
        &pin,
        Arc::from(b"x".as_slice()),
        SourceFormat::Yaml,
        Arc::new(Settings::default()),
        policy,
        tokio::time::Instant::now(),
        (Authority::process(), drain.clone(), None, None),
        Some(p),
    )
    .await;
    assert!(matches!(result, Err(Failure::Deadline)));
    assert_eq!(drain.snapshot().captures, 0);
    drain.close();
    let result = Controller::prepare_supervised_async_selected(
        &pin,
        Arc::from(b"x".as_slice()),
        SourceFormat::Yaml,
        Arc::new(Settings::default()),
        policy,
        tokio::time::Instant::now() + std::time::Duration::from_secs(1),
        (Authority::process(), drain.clone(), None, None),
        Some(p),
    )
    .await;
    assert!(matches!(result, Err(Failure::NoReady)));
    assert_eq!(drain.snapshot().captures, 0);
    assert_eq!(drain.snapshot().owners, 0);
}
