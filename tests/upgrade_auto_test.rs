//! Auto-upgrade end-to-end integration tests (plan-20260714 §A.11).
//!
//! Behind the `test-upgrade` feature (excluded from a bare `cargo test --all`
//! via `required-features`), these exercise the auto-upgrade subsystem across
//! process boundaries using the real built binary for candidate self-checks
//! and the public `internal::upgrade` API for the signature chain,
//! anti-rollback state, and the crash-recovery matrix.
//!
//! Endpoint/key injection is compile-time only (the `test-upgrade` feature),
//! so a release build cannot alter the trust root even with `LIBRA_TEST=1`.

#![cfg(all(feature = "test-upgrade", unix))]

use std::{os::unix::fs::PermissionsExt, path::Path, process::Command};

use base64::Engine as _;
use libra::internal::upgrade::{
    flow::{DecisionContext, FlowError, SkipReason, UpgradeDecision, decide_from_envelope},
    lock::InstallDir,
    manifest::{ReleaseVersion, SIGNATURE_DOMAIN_PREFIX, verify_envelope_bytes},
    marker::{
        InstallMarker, OFFICIAL_INSTALL_SOURCE, TARGET_BINARY_NAME, official_marker_for_target,
    },
    platform::Platform,
    state::{StateRejection, UpgradeState, evaluate_manifest},
    trusted_keys::{TrustedKey, test_injection},
    txn::{self, CANDIDATE_NAME, OldTarget, TxnError, TxnOutcome},
};
use sha2::Digest as _;

const SEED: [u8; 32] = [7u8; 32];
const NEXT_GENERATION_SEED: [u8; 32] = [8u8; 32];
/// Inside the payload lifetime `[2026-07-01, 2026-09-29)` (published_at is
/// 1_782_864_000; this is a few minutes later).
const GOOD_DATE: i64 = 1_782_864_100;

fn keypair() -> ring::signature::Ed25519KeyPair {
    ring::signature::Ed25519KeyPair::from_seed_unchecked(&SEED).unwrap()
}

fn pubkey() -> [u8; 32] {
    use ring::signature::KeyPair;
    keypair().public_key().as_ref().try_into().unwrap()
}

fn pubkey_for(seed: &[u8; 32]) -> [u8; 32] {
    use ring::signature::KeyPair;

    ring::signature::Ed25519KeyPair::from_seed_unchecked(seed)
        .unwrap()
        .public_key()
        .as_ref()
        .try_into()
        .unwrap()
}

/// Install the test trust key once (idempotent; first call wins).
fn install_test_trust() -> Vec<TrustedKey> {
    let keys: &'static [TrustedKey] = Box::leak(Box::new([TrustedKey {
        key_id: "test-key-1",
        ed25519_pubkey: pubkey(),
        not_before: 0,
        not_after: 4_102_444_800,
        generation: 1,
    }]));
    test_injection::inject_keys(keys);
    keys.to_vec()
}

fn artifact(platform: &str, version: &str) -> serde_json::Value {
    serde_json::json!({
        "platform": platform,
        "url": format!("https://download.libra.tools/libra/releases/v{version}/libra-{platform}"),
        "sha256": "a".repeat(64),
        "size": 4096,
    })
}

fn payload(version: &str, control: u64) -> serde_json::Value {
    payload_with_generation(version, control, 1)
}

fn payload_with_generation(
    version: &str,
    control: u64,
    min_key_generation: u32,
) -> serde_json::Value {
    serde_json::json!({
        "channel": "stable",
        "version": version,
        "control_revision": control,
        "published_at": "2026-07-01T00:00:00Z",
        "expires_at": "2026-09-29T00:00:00Z",
        "min_key_generation": min_key_generation,
        "paused": false,
        "revoked_versions": [],
        "artifacts": [
            artifact("linux-amd64", version),
            artifact("linux-arm64", version),
            artifact("darwin-arm64", version),
            artifact("windows-amd64", version),
        ],
    })
}

fn envelope(payload: &serde_json::Value) -> Vec<u8> {
    envelope_with_signers(payload, &[("test-key-1", SEED)])
}

fn envelope_with_signers(payload: &serde_json::Value, signers: &[(&str, [u8; 32])]) -> Vec<u8> {
    let payload_bytes = serde_json::to_vec(payload).unwrap();
    let mut message = SIGNATURE_DOMAIN_PREFIX.to_vec();
    message.extend_from_slice(&payload_bytes);
    let signatures: Vec<_> = signers
        .iter()
        .map(|(key_id, seed)| {
            let keypair = ring::signature::Ed25519KeyPair::from_seed_unchecked(seed).unwrap();
            let signature = keypair.sign(&message);
            serde_json::json!({
                "key_id": key_id,
                "signature": base64::engine::general_purpose::STANDARD.encode(signature.as_ref()),
            })
        })
        .collect();
    serde_json::to_vec(&serde_json::json!({
        "schema_version": 1,
        "payload": base64::engine::general_purpose::STANDARD.encode(&payload_bytes),
        "signatures": signatures,
    }))
    .unwrap()
}

fn decision_context<'a>(state: &'a UpgradeState, trust: &'a [TrustedKey]) -> DecisionContext<'a> {
    DecisionContext {
        state,
        https_date: Some(GOOD_DATE),
        local_now: GOOD_DATE,
        trust,
        platform: Some(Platform::DarwinArm64),
        installed_version: ReleaseVersion::parse("1.0.0").unwrap(),
        installed_at_rfc3339: "2026-07-17T00:00:00Z",
    }
}

fn owned_dir() -> (tempfile::TempDir, InstallDir) {
    let guard = tempfile::tempdir().unwrap();
    let path = guard.path().canonicalize().unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let dir = InstallDir::open_validated(&path).unwrap();
    (guard, dir)
}

// ── §A.11 mandated: full signature + decision chain ──────────────────────────

#[test]
fn upgrade_full_verify_and_decide_installs_newer() {
    let trust = install_test_trust();
    let env = envelope(&payload("2.0.0", 5));
    let ctx = DecisionContext {
        state: &UpgradeState::default(),
        https_date: Some(GOOD_DATE),
        local_now: GOOD_DATE,
        trust: &trust,
        platform: Some(Platform::DarwinArm64),
        installed_version: ReleaseVersion::parse("1.0.0").unwrap(),
        installed_at_rfc3339: "2026-07-17T00:00:00Z",
    };
    match decide_from_envelope(&ctx, &env).unwrap() {
        UpgradeDecision::Install(plan) => {
            assert_eq!(plan.version, ReleaseVersion(2, 0, 0));
            assert_eq!(plan.marker.install_source, OFFICIAL_INSTALL_SOURCE);
        }
        other => panic!("expected install, got {other:?}"),
    }
}

#[test]
fn upgrade_release_binary_has_no_test_trust_root() {
    // The production table may contain ceremony keys, but test-only signing
    // material is never a production trust root.
    let env = envelope(&payload("2.0.0", 5));
    assert!(!libra::internal::upgrade::trusted_keys::PRODUCTION_TRUSTED_KEYS.is_empty());
    assert!(
        libra::internal::upgrade::trusted_keys::PRODUCTION_TRUSTED_KEYS
            .iter()
            .all(|key| key.key_id != "test-key-1")
    );
    assert!(
        verify_envelope_bytes(
            &env,
            libra::internal::upgrade::trusted_keys::PRODUCTION_TRUSTED_KEYS
        )
        .is_err()
    );
}

#[test]
fn upgrade_persisted_generation_floor_selects_new_signer_and_rejects_lower_policy() {
    let trust = vec![
        TrustedKey {
            key_id: "old-key",
            ed25519_pubkey: pubkey_for(&SEED),
            not_before: 0,
            not_after: 4_102_444_800,
            generation: 1,
        },
        TrustedKey {
            key_id: "new-key",
            ed25519_pubkey: pubkey_for(&NEXT_GENERATION_SEED),
            not_before: 0,
            not_after: 4_102_444_800,
            generation: 2,
        },
    ];
    let state = UpgradeState {
        generation_floor: 2,
        ..Default::default()
    };
    let old_only = envelope_with_signers(
        &payload_with_generation("2.0.0", 5, 1),
        &[("old-key", SEED)],
    );
    assert!(matches!(
        decide_from_envelope(&decision_context(&state, &trust), &old_only),
        Err(FlowError::State(
            StateRejection::SignerGenerationBelowFloor {
                offered: 1,
                floor: 2,
                ..
            }
        ))
    ));

    // The old signature deliberately appears first. Filtering by the durable
    // floor must select the generation-2 signature rather than reject a
    // valid dual-signed rotation envelope.
    let dual_signed = envelope_with_signers(
        &payload_with_generation("2.0.0", 5, 2),
        &[("old-key", SEED), ("new-key", NEXT_GENERATION_SEED)],
    );
    let UpgradeDecision::Install(plan) =
        decide_from_envelope(&decision_context(&state, &trust), &dual_signed).unwrap()
    else {
        panic!("expected a generation-2 dual-signed manifest to install");
    };
    assert_eq!(plan.marker.manifest_key_id, "new-key");

    let lower_policy = envelope_with_signers(
        &payload_with_generation("2.0.0", 5, 1),
        &[("new-key", NEXT_GENERATION_SEED)],
    );
    assert!(matches!(
        decide_from_envelope(&decision_context(&state, &trust), &lower_policy),
        Err(FlowError::State(StateRejection::GenerationFloorRollback {
            offered: 1,
            floor: 2,
        }))
    ));
}

#[test]
fn upgrade_persisted_floor_reports_effective_floor_when_no_key_qualifies() {
    // Persisted floor 3, only a generation-1 key in trust, manifest
    // min_key_generation 2: the eligible-set pass finds no key and the
    // full-table retry rejects on the weaker signed/compile-time floor (2).
    // The surfaced error must carry the EFFECTIVE three-source floor (3).
    let trust = vec![TrustedKey {
        key_id: "old-key",
        ed25519_pubkey: pubkey_for(&SEED),
        not_before: 0,
        not_after: 4_102_444_800,
        generation: 1,
    }];
    let state = UpgradeState {
        generation_floor: 3,
        ..Default::default()
    };
    let env = envelope_with_signers(
        &payload_with_generation("2.0.0", 5, 2),
        &[("old-key", SEED)],
    );
    assert!(matches!(
        decide_from_envelope(&decision_context(&state, &trust), &env),
        Err(FlowError::Manifest(
            libra::internal::upgrade::manifest::ManifestError::KeyGenerationBelowFloor {
                floor: 3,
                manifest_min: 2,
            }
        ))
    ));
}

#[test]
fn upgrade_windows_is_explicitly_unsupported() {
    let trust = install_test_trust();
    let env = envelope(&payload("2.0.0", 5));
    let ctx = DecisionContext {
        state: &UpgradeState::default(),
        https_date: Some(GOOD_DATE),
        local_now: GOOD_DATE,
        trust: &trust,
        platform: Some(Platform::WindowsAmd64),
        installed_version: ReleaseVersion::parse("1.0.0").unwrap(),
        installed_at_rfc3339: "2026-07-17T00:00:00Z",
    };
    assert!(matches!(
        decide_from_envelope(&ctx, &env).unwrap(),
        UpgradeDecision::Skip {
            reason: SkipReason::UnsupportedPlatform(Platform::WindowsAmd64),
            ..
        }
    ));
}

#[test]
fn upgrade_revocation_replay_rejected_by_control_revision() {
    let trust = install_test_trust();
    // Accept control revision 6 (a revocation bump), then a replayed older
    // revision 5 envelope must be rejected even though it is still valid.
    let accepted = evaluate_manifest(
        &UpgradeState::default(),
        &verify_envelope_bytes(&envelope(&payload("2.0.0", 6)), &trust).unwrap(),
        Some(GOOD_DATE),
        GOOD_DATE,
    )
    .unwrap()
    .new_state;
    let replay = verify_envelope_bytes(&envelope(&payload("2.0.0", 5)), &trust).unwrap();
    assert!(evaluate_manifest(&accepted, &replay, Some(GOOD_DATE), GOOD_DATE).is_err());
}

#[test]
fn upgrade_same_version_artifact_identity_immutable() {
    let trust = install_test_trust();
    let state = evaluate_manifest(
        &UpgradeState::default(),
        &verify_envelope_bytes(&envelope(&payload("2.0.0", 5)), &trust).unwrap(),
        Some(GOOD_DATE),
        GOOD_DATE,
    )
    .unwrap()
    .new_state;
    // Same version, mutated artifact identity → rejected.
    let mut forged = payload("2.0.0", 6);
    forged["artifacts"][0]["sha256"] = serde_json::json!("b".repeat(64));
    let forged = verify_envelope_bytes(&envelope(&forged), &trust).unwrap();
    assert!(evaluate_manifest(&state, &forged, Some(GOOD_DATE), GOOD_DATE).is_err());
}

// ── §A.11 mandated: candidate self-check across a real process boundary ───────

#[test]
fn upgrade_probe_entry_self_checks_the_running_binary() {
    let exe = env!("CARGO_BIN_EXE_libra");
    let version = installed_version_string(exe);
    // Correct version → exit 0.
    let ok = Command::new(exe)
        .args([
            "__upgrade-probe",
            "--kind",
            "post-install",
            "--expected-version",
            &version,
        ])
        .output()
        .unwrap();
    assert!(
        ok.status.success(),
        "probe should pass for the running version"
    );
    assert!(
        ok.stdout.is_empty() && ok.stderr.is_empty(),
        "probe is silent"
    );
    // Wrong version → nonzero, still silent, and does NOT run a user command.
    let bad = Command::new(exe)
        .args([
            "__upgrade-probe",
            "--kind",
            "version",
            "--expected-version",
            "0.0.0-nope",
        ])
        .output()
        .unwrap();
    assert_eq!(bad.status.code(), Some(1));
    // Malformed probe fails closed.
    let malformed = Command::new(exe)
        .args([
            "__upgrade-probe",
            "--kind",
            "bogus",
            "--expected-version",
            &version,
        ])
        .output()
        .unwrap();
    assert_eq!(malformed.status.code(), Some(1));
}

fn installed_version_string(exe: &str) -> String {
    let out = Command::new(exe).arg("--version").output().unwrap();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .rsplit(' ')
        .next()
        .unwrap()
        .to_string()
}

// ── §A.11 mandated: transaction install + recovery across a fresh dir ─────────

fn marker_for(version: &str, bytes: &[u8]) -> InstallMarker {
    InstallMarker {
        schema_version: 1,
        installed_at: "2026-07-17T00:00:00Z".into(),
        install_source: OFFICIAL_INSTALL_SOURCE.into(),
        platform: "darwin-arm64".into(),
        version: version.into(),
        sha256: hex::encode(sha2::Sha256::digest(bytes)),
        size: bytes.len() as u64,
        manifest_key_id: "test-key-1".into(),
    }
}

fn hash(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
}

#[test]
fn upgrade_present_txn_commit_then_marker_is_official() {
    let (_g, dir) = owned_dir();
    dir.write_file_atomic(TARGET_BINARY_NAME, b"OLD", 0o755)
        .unwrap();
    dir.write_file_atomic(CANDIDATE_NAME, b"NEW", 0o755)
        .unwrap();
    let old_marker = marker_for("1.0.0", b"OLD");
    libra::internal::upgrade::marker::write_marker(&dir, &old_marker).unwrap();

    let pass = |_: &InstallDir| Ok::<bool, TxnError>(true);
    let outcome = txn::run_install(
        &dir,
        OldTarget::Present {
            hash: hash(b"OLD"),
            marker_snapshot: Some(old_marker),
        },
        "2.0.0",
        &hash(b"NEW"),
        marker_for("2.0.0", b"NEW"),
        UpgradeState::default(),
        &pass,
    )
    .unwrap();
    assert_eq!(outcome, TxnOutcome::Installed);
    assert_eq!(
        dir.read_file(TARGET_BINARY_NAME).unwrap().as_deref(),
        Some(&b"NEW"[..])
    );
    let m = official_marker_for_target(&dir, "darwin-arm64")
        .unwrap()
        .unwrap();
    assert_eq!(m.version, "2.0.0");
}

#[test]
fn upgrade_present_probe_failure_rolls_back_and_restores_marker() {
    // A failing post-install probe on an upgrade must restore the previous
    // target byte-for-byte and its marker, then leave no transaction behind.
    let (_g, dir) = owned_dir();
    dir.write_file_atomic(TARGET_BINARY_NAME, b"OLD", 0o755)
        .unwrap();
    dir.write_file_atomic(CANDIDATE_NAME, b"NEW", 0o755)
        .unwrap();
    let old_marker = marker_for("1.0.0", b"OLD");
    libra::internal::upgrade::marker::write_marker(&dir, &old_marker).unwrap();

    let fail = |_: &InstallDir| Ok::<bool, TxnError>(false);
    let outcome = txn::run_install(
        &dir,
        OldTarget::Present {
            hash: hash(b"OLD"),
            marker_snapshot: Some(old_marker),
        },
        "2.0.0",
        &hash(b"NEW"),
        marker_for("2.0.0", b"NEW"),
        UpgradeState::default(),
        &fail,
    )
    .unwrap();
    assert_eq!(outcome, TxnOutcome::RolledBack);
    assert_eq!(
        dir.read_file(TARGET_BINARY_NAME).unwrap().as_deref(),
        Some(&b"OLD"[..])
    );
    let m = official_marker_for_target(&dir, "darwin-arm64")
        .unwrap()
        .unwrap();
    assert_eq!(m.version, "1.0.0");
    // A subsequent recovery finds nothing to do.
    let pass = |_: &InstallDir| Ok::<bool, TxnError>(true);
    assert_eq!(txn::recover(&dir, &pass).unwrap(), TxnOutcome::NoOp);
}

// Guard: this whole target requires the feature; without it, `cargo test --all`
// skips it (required-features). Provide a trivial reachable test so the target
// is not "empty" on platforms where the cfg holds.
#[test]
fn upgrade_platform_matrix_is_the_release_matrix() {
    assert_eq!(Platform::RELEASE_MATRIX.len(), 4);
    assert!(Path::new(env!("CARGO_BIN_EXE_libra")).exists());
}
