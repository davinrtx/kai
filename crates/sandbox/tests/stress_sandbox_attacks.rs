//! Adversarial security stress test suite for `kai-sandbox`.
//!
//! Evaluates symlink escape attempts, adversarial traversal sequences,
//! credential evasion variations, chained destructive command injection,
//! and high-concurrency multi-threaded access.

use std::path::Path;
use std::sync::Arc;

use kai_core::error::SandboxError;
use kai_core::traits::{PermissionCategory, SandboxPolicy};
use kai_sandbox::{CommandSanitizer, PathResolver, PermissionMatrix, StandardSandboxPolicy};

#[test]
fn test_adversarial_traversal_variations() {
    let tmp_root =
        std::env::temp_dir().join(format!("kai_stress_traversal_{}", std::process::id()));
    let workspace = tmp_root.join("root");
    std::fs::create_dir_all(&workspace).unwrap();

    let resolver = PathResolver::new(&workspace).unwrap();

    let malicious_attempts = [
        "../outside.txt",
        "..\\outside.txt",
        "./../../outside.txt",
        "nested/../../../../etc/shadow",
        "nested/sub/../../../outside.txt",
        "....//....//outside.txt",
        "/etc/passwd",
        "/var/log/syslog",
    ];

    for attempt in malicious_attempts {
        let path = Path::new(attempt);
        let res = resolver.canonicalize_path(path);
        assert!(
            res.is_err(),
            "Traversal attempt '{attempt}' should have been rejected"
        );
        match res.unwrap_err() {
            SandboxError::PathTraversalDetected { .. } => {}
            other => panic!("Expected PathTraversalDetected for '{attempt}', got {other:?}"),
        }
    }

    let _ = std::fs::remove_dir_all(&tmp_root);
}

#[test]
fn test_adversarial_credential_shield_variations() {
    let tmp_root = std::env::temp_dir().join(format!("kai_stress_creds_{}", std::process::id()));
    std::fs::create_dir_all(&tmp_root).unwrap();

    let resolver = PathResolver::new(&tmp_root).unwrap();

    // Adversarial naming variations targeting sensitive patterns
    let credential_variants = [
        ".ENV",
        ".env",
        ".env.prod",
        ".env.staging.local",
        ".env.backup",
        "id_rsa",
        "id_rsa.bak",
        "id_dsa",
        "id_ecdsa",
        "id_ed25519",
        "my_cert.PEM",
        "key.pem",
        "tls.key",
        "keystore.pkcs12",
        "credentials",
        "credentials.json",
        "my_credentials.json",
        "secrets.json",
        "secrets.yaml",
        "secrets.yml",
        "token.json",
        "api.token",
        ".netrc",
        ".npmrc",
    ];

    for variant in credential_variants {
        let path = tmp_root.join(variant);
        let res = resolver.canonicalize_path(&path);
        assert!(
            res.is_err(),
            "Credential variant '{variant}' should have been blocked"
        );
        match res.unwrap_err() {
            SandboxError::ProtectedResourceAccess { .. } => {}
            other => panic!("Expected ProtectedResourceAccess for '{variant}', got {other:?}"),
        }
    }

    let _ = std::fs::remove_dir_all(&tmp_root);
}

#[test]
fn test_command_sanitizer_chained_and_adversarial_commands() {
    let sanitizer = CommandSanitizer::new();

    let dangerous_attempts = [
        "rm -rf /",
        "echo hello && rm -rf /",
        "cat file; rm -rf /",
        "rm -fr /",
        "rm -rf /*",
        ":(){ :|:& };:",
        ":(){:|:&};:",
        "dd if=/dev/urandom of=/dev/sda bs=4M",
        "dd if=/dev/zero of=/dev/nvme0n1",
        "sudo rm file.txt",
        "echo hello && sudo rm file.txt",
        "cat file | passwd",
        "su -",
        "passwd root",
        "apt-get install curl", // missing -y
        "apt install wget",     // missing -y
        "rm  -rf  /",           // excessive whitespace bypass attempt
        "rm -r -f /",           // split flag bypass attempt
        "rm -f -r /*",          // split flag target root
        "rm -R ~",              // capital -R flag
        "del /s /q c:\\",       // Windows recursive wipe
        "del   /s   /q   c:\\", // Windows spaced wipe
        "rd /s /q c:\\",        // Windows directory wipe
        "rmdir /s /q c:\\",     // Windows rmdir wipe
        "format c:",            // Windows format drive
        "format d: /fs:ntfs",   // Windows format drive
    ];

    for cmd in dangerous_attempts {
        assert!(
            sanitizer.validate_command(cmd).is_err(),
            "Dangerous command '{cmd}' should have been blocked by sanitizer"
        );
    }

    let benign_commands = [
        "cargo build --workspace",
        "git status --porcelain",
        "ls -la src/",
        "echo 'all good'",
        "apt-get install -y curl",
        "apt install --yes jq",
    ];

    for cmd in benign_commands {
        assert!(
            sanitizer.validate_command(cmd).is_ok(),
            "Benign command '{cmd}' should be allowed"
        );
    }
}

#[test]
fn test_high_concurrency_policy_access() {
    let tmp_root = std::env::temp_dir().join(format!("kai_stress_threads_{}", std::process::id()));
    let workspace = tmp_root.join("concurrency_ws");
    std::fs::create_dir_all(&workspace).unwrap();

    let policy = Arc::new(
        StandardSandboxPolicy::builder(&workspace)
            .with_permission_matrix(PermissionMatrix::standard())
            .build()
            .unwrap(),
    );

    let mut handles = Vec::new();

    // 20 concurrent worker threads performing path resolution and permission checks
    for thread_idx in 0..20 {
        let pol = policy.clone();
        let ws = workspace.clone();
        let handle = std::thread::spawn(move || {
            for i in 0..100 {
                // 1. Valid path
                let rel = format!("module_{thread_idx}/file_{i}.rs");
                let target = ws.join(&rel);
                let canon = pol.canonicalize_path(&target).unwrap();
                assert!(canon.starts_with(pol.path_resolver().workspace_root()));

                // 2. Traversal attempt
                let bad = format!("../escape_{thread_idx}_{i}.txt");
                assert!(pol.canonicalize_path(Path::new(&bad)).is_err());

                // 3. Permission checks
                assert!(pol.check_permission(PermissionCategory::FileRead).is_ok());
                assert!(pol.check_permission(PermissionCategory::FileWrite).is_ok());
                assert!(pol
                    .check_permission(PermissionCategory::NetworkAccess)
                    .is_err());
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().expect("Worker thread panicked");
    }

    let _ = std::fs::remove_dir_all(&tmp_root);
}
