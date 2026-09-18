# Skill: Release Management & Versioning

Defines the exact protocol for version bumps, changelog maintenance, Git tagging, and GitHub Releases in KAI.

---

## 1. SemVer Rules for KAI (0.y.z Series)

KAI follows Rust Cargo semantic versioning conventions for pre-1.0 development:

* **Minor Bump (`0.y.0`):** Breaking changes, crate trait re-architectures, or completion of a roadmap phase (`0.1.0` -> `0.2.0`).
* **Patch Bump (`0.y.z`):** Bug fixes, internal refactors, and backward-compatible additions within the current milestone (`0.1.0` -> `0.1.1`).
* **Pre-release Suffixes:**
  * Alpha: `0.y.0-alpha.N` (early milestone scaffold).
  * Beta: `0.y.0-beta.N` (feature-complete phase).
  * Release Candidate: `0.y.0-rc.N` (stabilization freeze before tagging `0.y.0`).

---

## 2. Release Preparation Checklist

Before initiating any release, execute the following validation sequence in order. If any step fails, abort the release immediately.

1. **Verify Clean Tree:**
   ```bash
   git status --porcelain

Must return zero uncommitted changes.

    Verify Full Quality Gate:
    code Bash

    cargo fmt --all -- --check
    cargo check --workspace
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace

    Check Dependency Duplication:
    code Bash

    cargo tree --duplicates

    Must return zero unwanted duplicate packages.

3. Release Execution Steps

When authorized to cut a release:

    Update Workspace Version:
    Modify [workspace.package].version in root Cargo.toml to the target version (e.g., 0.2.0).

    Update CHANGELOG.md:

        Move entries from ## [Unreleased] into a new section: ## [0.y.z] - YYYY-MM-DD.

        Keep subsection headers: ### Added, ### Changed, ### Fixed, ### Removed.

        Recreate an empty ## [Unreleased] section at the top.

    Update ROADMAP.md Status:
    Mark the completed phase milestone as [x] Completed.

    Commit the Release:
    Stage only the modified versioning files:
    code Bash

    git add Cargo.toml Cargo.lock CHANGELOG.md ROADMAP.md
    git commit -m "docs(release): cut version v0.y.z"

    Tag the Release:
    Create an annotated, signed (if available) Git tag:
    code Bash

    git tag -a v0.y.z -m "Release v0.y.z"

    Push to Remote:
    code Bash

    git push origin main
    git push origin v0.y.z

    Publish GitHub Release via gh:
    Generate release notes from the changelog section and dispatch:
    code Bash

    gh release create v0.y.z --title "v0.y.z" --notes-file <(sed -n '/## \[0.y.z\]/,/## \[/p' CHANGELOG.md | sed '$d')

4. PR & Versioning Guardrails

    No Version Bumps in Feature PRs: Feature or bugfix PRs must never touch version in Cargo.toml. Version increments occur exclusively on dedicated release commits.

    Changelog Requirement: Non-documentation PRs must add a line to ## [Unreleased] in CHANGELOG.md describing the behavioral change.

    Immutable Releases: Released tags and changelog headers are immutable. Never re-tag an existing version.
    EOF

