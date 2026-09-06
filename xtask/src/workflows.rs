//! Parsed repository workflow policy checks.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use yaml_rust2::parser::{Event, Parser};
use yaml_rust2::{Yaml, YamlLoader};

const MAX_BYTES: usize = 256 * 1024;
const MAX_DEPTH: usize = 64;
const MAX_NODES: usize = 20_000;
const FILES: &[&str] = &[
    "ci.yml",
    "codeql.yml",
    "links.yml",
    "mutation.yml",
    "platform.yml",
    "release.yml",
    "publish-crates-io.yml",
    "config.yml",
];

/// Check local policy without executing workflow commands or changing repository state.
pub(crate) fn check(root: &Path) -> Result<(), String> {
    let mut sources = Vec::with_capacity(FILES.len());
    for name in FILES {
        let directory = if *name == "config.yml" {
            ".github/ISSUE_TEMPLATE"
        } else {
            ".github/workflows"
        };
        let path = root.join(directory).join(name);
        if !std::fs::symlink_metadata(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?
            .is_file()
        {
            return Err(format!(
                "{}: expected a regular non-symlink file",
                path.display()
            ));
        }
        let file = File::open(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        if !file
            .metadata()
            .map_err(|error| error.to_string())?
            .is_file()
        {
            return Err(format!("{}: expected a regular file", path.display()));
        }
        let mut bytes = Vec::new();
        file.take(u64::try_from(MAX_BYTES + 1).map_err(|error| error.to_string())?)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let source =
            String::from_utf8(bytes).map_err(|error| format!("{}: {error}", path.display()))?;
        sources.push((*name, source));
    }
    let borrowed: Vec<_> = sources
        .iter()
        .map(|(name, source)| (*name, source.as_str()))
        .collect();
    check_sources(&borrowed)
}

/// Bound expanded alias/anchor allocations before the high-level loader clones nodes.
fn validate_budget(source: &str) -> Result<(), String> {
    if source.len() > MAX_BYTES {
        return Err("YAML input exceeds 256 KiB".to_owned());
    }
    let mut parser = Parser::new_from_str(source);
    let mut stack: Vec<(usize, usize, usize)> = Vec::new();
    let mut anchors = BTreeMap::new();
    let mut allocated = 0_usize;
    let mut documents = 0_usize;
    loop {
        let (event, _) = parser.next_token().map_err(|error| error.to_string())?;
        let (anchor, weight, depth, allocation) = match event {
            Event::StreamEnd => break,
            Event::DocumentStart => {
                documents += 1;
                if documents > 1 {
                    return Err("expected one YAML document".into());
                }
                continue;
            }
            Event::SequenceStart(anchor, _) | Event::MappingStart(anchor, _) => {
                if stack.len() >= MAX_DEPTH {
                    return Err("YAML nesting exceeds 64 levels".into());
                }
                stack.push((anchor, 1, 1));
                allocated = allocated
                    .checked_add(1)
                    .filter(|value| *value <= MAX_NODES)
                    .ok_or("expanded YAML exceeds 20000 nodes")?;
                continue;
            }
            Event::SequenceEnd | Event::MappingEnd => {
                let (anchor, weight, depth) = stack.pop().ok_or("unbalanced YAML container")?;
                (anchor, weight, depth, if anchor == 0 { 0 } else { weight })
            }
            Event::Scalar(_, _, anchor, _) => (anchor, 1, 0, if anchor == 0 { 1 } else { 2 }),
            Event::Alias(anchor) => {
                let (weight, depth) = *anchors
                    .get(&anchor)
                    .ok_or("undefined or recursive YAML alias")?;
                (0, weight, depth, weight)
            }
            Event::Nothing | Event::StreamStart | Event::DocumentEnd => continue,
        };
        allocated = allocated
            .checked_add(allocation)
            .filter(|value| *value <= MAX_NODES)
            .ok_or("expanded YAML exceeds 20000 nodes")?;
        if anchor != 0 {
            anchors.insert(anchor, (weight, depth));
        }
        if let Some((_, total, parent_depth)) = stack.last_mut() {
            *total = total
                .checked_add(weight)
                .ok_or("YAML node count overflow")?;
            *parent_depth = (*parent_depth).max(depth + 1);
            if *parent_depth > MAX_DEPTH {
                return Err("expanded YAML nesting exceeds 64 levels".into());
            }
        }
    }
    if documents != 1 {
        return Err("expected one YAML document".into());
    }
    Ok(())
}

fn parse(file: &str, source: &str) -> Result<Yaml, String> {
    validate_budget(source).map_err(|error| format!("{file}: {error}"))?;
    let mut documents =
        YamlLoader::load_from_str(source).map_err(|error| format!("{file}: {error}"))?;
    let document = documents
        .pop()
        .ok_or_else(|| format!("{file}: missing document"))?;
    if document.as_hash().is_none() {
        return Err(format!("{file}: expected a mapping"));
    }
    Ok(document)
}

struct Policy<'a> {
    file: &'a str,
    document: &'a Yaml,
}

impl<'a> Policy<'a> {
    fn require(&self, path: &str, valid: bool, expected: &str) -> Result<(), String> {
        if valid {
            Ok(())
        } else {
            Err(format!("{}: {path}: expected {expected}", self.file))
        }
    }

    fn job(&self, name: &str) -> Result<&'a Yaml, String> {
        let job = &self.document["jobs"][name];
        self.require(
            &format!("jobs.{name}"),
            job.as_hash().is_some(),
            "a job mapping",
        )?;
        Ok(job)
    }

    fn steps(&self, name: &str) -> Result<&'a [Yaml], String> {
        self.job(name)?["steps"]
            .as_vec()
            .map(Vec::as_slice)
            .ok_or_else(|| format!("{}: jobs.{name}.steps: expected a step sequence", self.file))
    }

    fn run(&self, job: &str, command: &str) -> Result<(), String> {
        self.require(
            &format!("jobs.{job}.steps.run"),
            self.steps(job)?.iter().any(|step| {
                step["run"]
                    .as_str()
                    .is_some_and(|run| code_lines(run).any(|line| line == command))
            }),
            command,
        )
    }

    fn run_contains(&self, job: &str, command: &str) -> Result<(), String> {
        self.require(
            &format!("jobs.{job}.steps.run"),
            self.steps(job)?.iter().any(|step| {
                step["run"]
                    .as_str()
                    .is_some_and(|run| code_lines(run).any(|line| line.contains(command)))
            }),
            command,
        )
    }

    fn step_named(&self, job: &str, name: &str) -> Result<&'a Yaml, String> {
        self.steps(job)?
            .iter()
            .find(|step| step["name"].as_str() == Some(name))
            .ok_or_else(|| format!("{}: jobs.{job}.steps: missing {name}", self.file))
    }

    fn matrix_os(&self, job: &str) -> Result<(), String> {
        self.require(
            &format!("jobs.{job}.runs-on"),
            self.job(job)?["runs-on"].as_str() == Some("${{ matrix.os }}"),
            "${{ matrix.os }}",
        )?;
        self.require(
            &format!("jobs.{job}.strategy.matrix.os"),
            strings(&self.job(job)?["strategy"]["matrix"]["os"]).is_some_and(|values| {
                values.len() == 2
                    && values.contains(&"ubuntu-24.04")
                    && values.contains(&"macos-15")
            }),
            "[ubuntu-24.04, macos-15]",
        )
    }

    fn toolchain_position(&self, job: &str, version: &str) -> Result<usize, String> {
        self.steps(job)?
            .iter()
            .position(|step| {
                step["uses"]
                    .as_str()
                    .is_some_and(|action| action.starts_with("dtolnay/rust-toolchain@"))
                    && step["with"]["toolchain"].as_str() == Some(version)
            })
            .ok_or_else(|| {
                format!(
                    "{}: jobs.{job}.steps.toolchain: expected Rust setup for {version}",
                    self.file
                )
            })
    }
}

fn code_lines(run: &str) -> impl Iterator<Item = &str> {
    run.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
}

fn strings(value: &Yaml) -> Option<Vec<&str>> {
    value.as_vec()?.iter().map(Yaml::as_str).collect()
}

fn check_sources(sources: &[(&str, &str)]) -> Result<(), String> {
    let mut documents = BTreeMap::new();
    for (name, source) in sources {
        if documents.insert(*name, parse(name, source)?).is_some() {
            return Err(format!("{name}: duplicate input file"));
        }
    }
    let policy = |file| {
        documents
            .get(file)
            .map(|document| Policy { file, document })
            .ok_or_else(|| format!("{file}: missing policy input"))
    };
    check_ci(&policy("ci.yml")?)?;
    check_codeql(&policy("codeql.yml")?)?;
    check_links(&policy("links.yml")?)?;
    check_platform(&policy("platform.yml")?)?;
    check_mutation(&policy("mutation.yml")?)?;
    check_release(&policy("release.yml")?)?;
    check_publish(&policy("publish-crates-io.yml")?)?;
    let config = policy("config.yml")?;
    config.require(
        "blank_issues_enabled",
        config.document["blank_issues_enabled"].as_bool() == Some(true),
        "true",
    )?;
    config.require(
        "contact_links.security.url",
        config.document["contact_links"]
            .as_vec()
            .is_some_and(|links| {
                links.iter().any(|link| {
                    link["url"].as_str()
                        == Some("https://github.com/jsugg/atx/security/advisories/new")
                })
            }),
        "the private security reporting URL",
    )
}

fn check_ci(policy: &Policy<'_>) -> Result<(), String> {
    policy.run("hygiene", "scripts/check.sh markdown")?;
    policy.run("hygiene", "scripts/test-markdown-harness.sh --real")?;
    policy.run("hygiene", "scripts/check.sh hygiene")?;
    let rust_setup = policy.toolchain_position("hygiene", "1.97.0")?;
    policy.require(
        "jobs.hygiene.steps.toolchain",
        policy.steps("hygiene")?[..rust_setup].iter().all(|step| {
            step["run"]
                .as_str()
                .is_none_or(|run| !code_lines(run).any(|line| line == "scripts/check.sh hygiene"))
        }),
        "Rust setup before compiling the workflow guard",
    )?;
    let install = policy.step_named("hygiene", "Install actionlint")?;
    policy.require(
        "jobs.hygiene.steps.actionlint.env.ACTIONLINT_VERSION",
        install["env"]["ACTIONLINT_VERSION"].as_str() == Some("1.7.12"),
        "1.7.12",
    )?;
    policy.require(
        "jobs.hygiene.steps.actionlint.run",
        install["run"].as_str().is_some_and(|run| {
            run.contains("rhysd/actionlint/releases/download/") && run.contains("sha256sum --check")
        }),
        "versioned, checksum-verified actionlint",
    )?;
    for job in ["clippy", "msrv", "coverage", "doc"] {
        policy.matrix_os(job)?;
    }
    let coverage = policy.job("coverage")?;
    policy.require(
        "jobs.coverage.env.COVERAGE_LINE_FLOOR",
        coverage["env"]["COVERAGE_LINE_FLOOR"].as_i64() == Some(95)
            || coverage["env"]["COVERAGE_LINE_FLOOR"].as_str() == Some("95"),
        "95",
    )?;
    policy.require(
        "jobs.coverage.env.COVERAGE_REGION_FLOOR",
        coverage["env"]["COVERAGE_REGION_FLOOR"].as_i64() == Some(85)
            || coverage["env"]["COVERAGE_REGION_FLOOR"].as_str() == Some("85"),
        "85",
    )?;
    policy.toolchain_position("coverage", "nightly-2026-08-01")?;
    for job in ["hygiene", "fmt", "test-linux", "package"] {
        policy.require(
            &format!("jobs.{job}.runs-on"),
            policy.job(job)?["runs-on"].as_str() == Some("ubuntu-24.04"),
            "ubuntu-24.04",
        )?;
    }
    Ok(())
}

fn check_codeql(policy: &Policy<'_>) -> Result<(), String> {
    policy.matrix_os("analyze")?;
    policy.require(
        "on.pull_request.paths",
        strings(&policy.document["on"]["pull_request"]["paths"])
            .is_some_and(|paths| paths.contains(&"xtask/**")),
        "xtask/** coverage",
    )
}

fn check_links(policy: &Policy<'_>) -> Result<(), String> {
    policy.run("links", "scripts/check.sh links-online")?;
    policy.require(
        "on.schedule",
        policy.document["on"]["schedule"]
            .as_vec()
            .is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item["cron"].as_str() == Some("17 5 * * 1"))
            }),
        "weekly 17 5 * * 1",
    )
}

fn check_platform(policy: &Policy<'_>) -> Result<(), String> {
    policy.require(
        "name",
        policy.document["name"].as_str() == Some("platform"),
        "platform",
    )?;
    for trigger in ["pull_request", "push"] {
        for field in ["paths", "paths-ignore"] {
            policy.require(
                &format!("on.{trigger}.{field}"),
                policy.document["on"][trigger][field].is_badvalue(),
                "no path filter on required platform checks",
            )?;
        }
    }
    let linux = policy.job("linux-root")?;
    policy.require(
        "jobs.linux-root.container",
        linux["container"].as_str() == Some("rust:1.97-bookworm"),
        "rust:1.97-bookworm",
    )?;
    policy.run(
        "linux-root",
        "cargo test --all-targets --all-features --locked",
    )?;
    let matrix = policy.job("native")?["strategy"]["matrix"]["include"]
        .as_vec()
        .ok_or("platform.yml: jobs.native.strategy.matrix.include: expected native targets")?;
    policy.require(
        "jobs.native.runs-on",
        policy.job("native")?["runs-on"].as_str() == Some("${{ matrix.os }}"),
        "${{ matrix.os }}",
    )?;
    let expected = [
        ("macos-aarch64", "macos-15", "aarch64-apple-darwin"),
        ("macos-x86_64", "macos-15-intel", "x86_64-apple-darwin"),
        (
            "linux-x86_64-glibc",
            "ubuntu-24.04",
            "x86_64-unknown-linux-gnu",
        ),
        (
            "linux-aarch64-glibc",
            "ubuntu-24.04-arm",
            "aarch64-unknown-linux-gnu",
        ),
        (
            "linux-aarch64-musl",
            "ubuntu-24.04-arm",
            "aarch64-unknown-linux-musl",
        ),
        (
            "linux-x86_64-musl",
            "ubuntu-24.04",
            "x86_64-unknown-linux-musl",
        ),
    ];
    policy.require(
        "jobs.native.strategy.matrix.include",
        matrix.len() == expected.len()
            && expected.iter().all(|(name, os, target)| {
                matrix
                    .iter()
                    .filter(|entry| {
                        entry["name"].as_str() == Some(name)
                            && entry["os"].as_str() == Some(os)
                            && entry["target"].as_str() == Some(target)
                            && entry["tests"].as_str() == Some("full")
                    })
                    .count()
                    == 1
            }),
        "the six matching native runner/target/full-test entries",
    )
}

fn check_mutation(policy: &Policy<'_>) -> Result<(), String> {
    let job = policy.job("mutants")?;
    policy.require(
        "jobs.mutants.timeout-minutes",
        job["timeout-minutes"].as_i64() == Some(55),
        "55",
    )?;
    policy.run_contains("mutants", "timeout 45m cargo mutants")?;
    policy.run_contains("mutants", "mutants.out")?;
    policy.require(
        "jobs.mutants.steps.upload",
        policy.steps("mutants")?.iter().any(|step| {
            step["uses"].as_str().is_some_and(|value| {
                value
                    .strip_prefix("actions/upload-artifact@")
                    .is_some_and(|sha| {
                        sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
                    })
            })
        }),
        "a pinned upload-artifact action",
    )
}

fn check_release(policy: &Policy<'_>) -> Result<(), String> {
    policy.run_contains(
        "archives",
        "cargo install cargo-auditable --version 0.7.7 --locked",
    )?;
    policy.run_contains(
        "archives",
        "cargo auditable build --release --locked --target",
    )?;
    policy.run_contains("archives", "cargo xtask check-sbom")?;
    let scanner = policy
        .steps("archives")?
        .iter()
        .find(|step| step["with"]["syft-version"].as_str().is_some())
        .ok_or("release.yml: jobs.archives.steps.syft-version: missing scanner")?;
    policy.require(
        "jobs.archives.steps.syft-version",
        scanner["with"]["syft-version"].as_str() == Some("v1.42.3"),
        "v1.42.3",
    )?;
    policy.require(
        "jobs.archives.steps.upload-release-assets",
        scanner["with"]["upload-release-assets"].as_bool() == Some(false),
        "false",
    )?;
    policy.run("finalize", "sha256sum -- *.tar.gz *.cdx.json > SHA256SUMS")?;
    let attest = policy.job("attest")?;
    policy.require(
        "jobs.attest.if",
        attest["if"].as_str() == Some("github.event_name == 'push'"),
        "tag push only",
    )?;
    policy.require(
        "jobs.attest.env.DRY_RUN",
        attest["env"]["DRY_RUN"].as_str() == Some("false"),
        "quoted false",
    )?;
    for name in ["Attest archives and SBOMs", "Verify attestations"] {
        policy.require(
            &format!("jobs.attest.steps.{name}.if"),
            policy.step_named("attest", name)?["if"].as_str() == Some("env.DRY_RUN == 'false'"),
            "DRY_RUN == false",
        )?;
    }
    policy.run_contains("attest", "gh attestation verify")?;
    policy.require(
        "jobs.finalize.steps.manifest",
        policy
            .steps("finalize")?
            .iter()
            .any(|step| step["with"]["path"].as_str() == Some("dist/SHA256SUMS")),
        "dist/SHA256SUMS evidence",
    )?;
    let draft = policy.step_named("draft-release", "Publish draft release")?;
    policy.require(
        "jobs.draft-release.if",
        policy.job("draft-release")?["if"].as_str() == Some("github.event_name == 'push'"),
        "tag push only",
    )?;
    policy.require(
        "jobs.draft-release.steps.fail_on_unmatched_files",
        draft["with"]["fail_on_unmatched_files"].as_bool() == Some(true),
        "true",
    )?;
    policy.require(
        "jobs.archives.steps.if-no-files-found",
        policy
            .steps("archives")?
            .iter()
            .any(|step| step["with"]["if-no-files-found"].as_str() == Some("error")),
        "error on missing artifacts",
    )
}

fn check_publish(policy: &Policy<'_>) -> Result<(), String> {
    policy.run_contains("crates-io", "git rev-parse HEAD")?;
    policy.run_contains("crates-io", "git rev-parse \"$TAG_REF^{commit}\"")?;
    policy.require(
        "jobs.steps.run",
        policy.document["jobs"].as_hash().is_some_and(|jobs| {
            jobs.values().all(|job| {
                job["steps"].as_vec().is_some_and(|steps| {
                    steps.iter().all(|step| {
                        step["run"].as_str().is_none_or(|run| {
                            !code_lines(run).any(|line| line.contains("--no-verify"))
                        })
                    })
                })
            })
        }),
        "no skipped package verification",
    )
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    const SOURCES: &[(&str, &str)] = &[
        ("ci.yml", include_str!("../../.github/workflows/ci.yml")),
        (
            "codeql.yml",
            include_str!("../../.github/workflows/codeql.yml"),
        ),
        (
            "links.yml",
            include_str!("../../.github/workflows/links.yml"),
        ),
        (
            "mutation.yml",
            include_str!("../../.github/workflows/mutation.yml"),
        ),
        (
            "platform.yml",
            include_str!("../../.github/workflows/platform.yml"),
        ),
        (
            "release.yml",
            include_str!("../../.github/workflows/release.yml"),
        ),
        (
            "publish-crates-io.yml",
            include_str!("../../.github/workflows/publish-crates-io.yml"),
        ),
        (
            "config.yml",
            include_str!("../../.github/ISSUE_TEMPLATE/config.yml"),
        ),
    ];

    #[test]
    fn current_repository_policies_are_valid() {
        super::check_sources(SOURCES).expect("current policy");
    }

    #[test]
    fn repository_files_are_required_regular_utf8_and_bounded() {
        struct OwnedRoot(std::path::PathBuf);
        impl Drop for OwnedRoot {
            fn drop(&mut self) {
                if let Err(error) = std::fs::remove_dir_all(&self.0) {
                    if error.kind() != std::io::ErrorKind::NotFound {
                        eprintln!(
                            "workflow fixture cleanup failed: {}: {error}",
                            self.0.display()
                        );
                    }
                }
            }
        }

        let root = std::env::temp_dir().join(format!("atx-workflow-policy-{}", std::process::id()));
        std::fs::create_dir(&root).expect("exclusive workflow fixture root");
        let owned = OwnedRoot(root);
        for (name, source) in SOURCES {
            let directory = if *name == "config.yml" {
                ".github/ISSUE_TEMPLATE"
            } else {
                ".github/workflows"
            };
            let parent = owned.0.join(directory);
            std::fs::create_dir_all(&parent).expect("fixture parent");
            std::fs::write(parent.join(name), source).expect("fixture workflow");
        }
        super::check(&owned.0).expect("valid repository files");
        let ci = owned.0.join(".github/workflows/ci.yml");
        std::fs::write(&ci, [0xff]).expect("invalid UTF-8");
        assert!(
            super::check(&owned.0)
                .expect_err("UTF-8")
                .contains("ci.yml")
        );
        std::fs::write(&ci, "x".repeat(super::MAX_BYTES + 1)).expect("oversized YAML");
        assert!(
            super::check(&owned.0)
                .expect_err("size")
                .contains("256 KiB")
        );
        std::fs::remove_file(&ci).expect("remove fixture file");
        assert!(
            super::check(&owned.0)
                .expect_err("missing input")
                .contains("ci.yml")
        );
        std::fs::create_dir(&ci).expect("directory instead of file");
        assert!(
            super::check(&owned.0)
                .expect_err("directory")
                .contains("regular")
        );
        std::fs::remove_dir(&ci).expect("remove fixture directory");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("codeql.yml", &ci).expect("symlink fixture");
            assert!(
                super::check(&owned.0)
                    .expect_err("symlink")
                    .contains("non-symlink")
            );
        }
        std::fs::remove_dir_all(&owned.0).expect("remove owned workflow fixture");
    }

    fn reject_change(file: &str, before: &str, after: &str, field: &str) {
        let changed: Vec<_> = SOURCES
            .iter()
            .map(|(name, source)| {
                let source = if *name == file {
                    assert!(source.contains(before), "fixture must contain {before}");
                    source.replacen(before, after, 1)
                } else {
                    (*source).to_owned()
                };
                (*name, source)
            })
            .collect();
        let borrowed: Vec<_> = changed
            .iter()
            .map(|(name, source)| (*name, source.as_str()))
            .collect();
        let error = super::check_sources(&borrowed).expect_err("changed policy");
        assert!(error.contains(file) && error.contains(field), "{error}");
    }

    #[test]
    fn matrix_values_do_not_hide_a_wrong_runner_expression() {
        for (file, field) in [
            ("ci.yml", "jobs.clippy.runs-on"),
            ("codeql.yml", "jobs.analyze.runs-on"),
            ("platform.yml", "jobs.native.runs-on"),
        ] {
            reject_change(
                file,
                "runs-on: ${{ matrix.os }}",
                "runs-on: ubuntu-22.04",
                field,
            );
        }
    }

    #[test]
    fn checksums_must_cover_archives_and_sboms_in_one_command() {
        reject_change(
            "release.yml",
            "sha256sum -- *.tar.gz *.cdx.json > SHA256SUMS",
            "sha256sum -- *.tar.gz > SHA256SUMS\n          printf '%s\\n' *.cdx.json",
            "jobs.finalize.steps.run",
        );
    }

    #[test]
    fn toolchain_input_on_an_unrelated_action_does_not_satisfy_coverage() {
        reject_change(
            "ci.yml",
            "uses: dtolnay/rust-toolchain@4360b52568e2003a75bf9bc1d59f33a8e3fc893c\n        with:\n          toolchain: nightly-2026-08-01",
            "uses: actions/checkout@4360b52568e2003a75bf9bc1d59f33a8e3fc893c\n        with:\n          toolchain: nightly-2026-08-01",
            "jobs.coverage.steps.toolchain",
        );
    }

    #[test]
    fn hygiene_requires_rust_setup_before_compiling_the_guard() {
        reject_change(
            "ci.yml",
            "      - uses: dtolnay/rust-toolchain@4360b52568e2003a75bf9bc1d59f33a8e3fc893c # stable branch\n        with:\n          toolchain: 1.97.0\n      - name: Workflow guards\n        run: scripts/check.sh hygiene",
            "      - name: Workflow guards\n        run: scripts/check.sh hygiene\n      - uses: dtolnay/rust-toolchain@4360b52568e2003a75bf9bc1d59f33a8e3fc893c # stable branch\n        with:\n          toolchain: 1.97.0",
            "jobs.hygiene.steps.toolchain",
        );
    }

    #[test]
    fn required_command_in_an_unrelated_job_does_not_satisfy_hygiene() {
        reject_change(
            "ci.yml",
            "run: scripts/check.sh hygiene",
            "run: echo skipped\n\n  unrelated:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: scripts/check.sh hygiene",
            "jobs.hygiene.steps.run",
        );
    }

    #[test]
    fn equivalent_quoting_and_sequence_layout_do_not_change_policies() {
        let changed: Vec<_> = SOURCES
            .iter()
            .map(|(name, source)| {
                (
                    *name,
                    source
                        .replace("runs-on: ubuntu-24.04", "runs-on: 'ubuntu-24.04'")
                        .replace("COVERAGE_LINE_FLOOR: 95", "COVERAGE_LINE_FLOOR: '95'")
                        .replace("COVERAGE_REGION_FLOOR: 85", "COVERAGE_REGION_FLOOR: \"85\"")
                        .replace(
                            "os: [ubuntu-24.04, macos-15]",
                            "os:\n          - 'ubuntu-24.04'\n          - \"macos-15\"",
                        )
                        .replace("DRY_RUN: 'false'", "DRY_RUN: \"false\""),
                )
            })
            .collect();
        let borrowed: Vec<_> = changed
            .iter()
            .map(|(name, source)| (*name, source.as_str()))
            .collect();
        super::check_sources(&borrowed).expect("equivalent YAML values");
    }

    #[test]
    fn runner_matrix_order_does_not_change_the_required_os_set() {
        let changed: Vec<_> = SOURCES
            .iter()
            .map(|(name, source)| {
                (
                    *name,
                    source.replace("[ubuntu-24.04, macos-15]", "[macos-15, ubuntu-24.04]"),
                )
            })
            .collect();
        let borrowed: Vec<_> = changed
            .iter()
            .map(|(name, source)| (*name, source.as_str()))
            .collect();
        super::check_sources(&borrowed).expect("same runner set");
    }

    #[test]
    fn reindentation_and_crlf_do_not_change_policies() {
        let changed: Vec<_> = SOURCES
            .iter()
            .map(|(name, source)| {
                (
                    *name,
                    source.lines().fold(String::new(), |mut output, line| {
                        output.push_str("    ");
                        output.push_str(line);
                        output.push_str("\r\n");
                        output
                    }),
                )
            })
            .collect();
        let borrowed: Vec<_> = changed
            .iter()
            .map(|(name, source)| (*name, source.as_str()))
            .collect();
        super::check_sources(&borrowed).expect("equivalent indentation");
    }

    #[test]
    fn each_required_value_is_checked_in_its_own_workflow() {
        let cases = [
            (
                "ci.yml",
                "COVERAGE_LINE_FLOOR: 95",
                "COVERAGE_LINE_FLOOR: 94",
                "coverage",
            ),
            (
                "ci.yml",
                "COVERAGE_REGION_FLOOR: 85",
                "COVERAGE_REGION_FLOOR: 84",
                "coverage",
            ),
            (
                "ci.yml",
                "os: [ubuntu-24.04, macos-15]",
                "os: [ubuntu-22.04, macos-15]",
                "clippy",
            ),
            (
                "ci.yml",
                "run: scripts/check.sh hygiene",
                "run: true",
                "hygiene",
            ),
            (
                "codeql.yml",
                "      - \"xtask/**\"",
                "      - \"unused/**\"",
                "pull_request",
            ),
            (
                "links.yml",
                "run: scripts/check.sh links-online",
                "run: scripts/check.sh links-offline",
                "links",
            ),
            (
                "platform.yml",
                "container: rust:1.97-bookworm",
                "container: rust:1.85-bookworm",
                "linux-root",
            ),
            (
                "release.yml",
                "DRY_RUN: 'false'",
                "DRY_RUN: 'true'",
                "attest",
            ),
            (
                "release.yml",
                "upload-release-assets: false",
                "upload-release-assets: true",
                "archives",
            ),
            (
                "release.yml",
                "fail_on_unmatched_files: true",
                "fail_on_unmatched_files: false",
                "draft-release",
            ),
            (
                "config.yml",
                "blank_issues_enabled: true",
                "blank_issues_enabled: false",
                "blank_issues_enabled",
            ),
        ];
        for (file, before, after, diagnostic) in cases {
            let changed: Vec<_> = SOURCES
                .iter()
                .map(|(name, source)| {
                    (
                        *name,
                        if *name == file {
                            source.replacen(before, after, 1)
                        } else {
                            (*source).to_owned()
                        },
                    )
                })
                .collect();
            assert!(
                changed
                    .iter()
                    .any(|(name, source)| *name == file && source.contains(after)),
                "mutation must apply: {file}"
            );
            let borrowed: Vec<_> = changed
                .iter()
                .map(|(name, source)| (*name, source.as_str()))
                .collect();
            let error = super::check_sources(&borrowed).expect_err("changed policy");
            assert!(
                error.contains(file) && error.contains(diagnostic),
                "{error}"
            );
        }
    }

    #[test]
    fn parsing_rejects_malformed_duplicate_and_multiple_documents() {
        for source in ["", "[]", "a: [", "a: 1\na: 2\n", "a: 1\n---\nb: 2\n"] {
            assert!(super::parse("fixture.yml", source).is_err(), "{source:?}");
        }
    }

    #[test]
    fn parsing_bounds_input_nesting_and_alias_expansion() {
        assert!(super::parse("fixture.yml", &"x".repeat(super::MAX_BYTES + 1)).is_err());
        let deep = format!(
            "{}{}",
            "[".repeat(super::MAX_DEPTH + 1),
            "]".repeat(super::MAX_DEPTH + 1)
        );
        assert!(
            super::parse("fixture.yml", &deep)
                .expect_err("depth")
                .contains("nesting")
        );
        let mut aliases = "a0: &a0 [leaf]\n".to_owned();
        for index in 1..20 {
            use std::fmt::Write;
            writeln!(
                &mut aliases,
                "a{index}: &a{index} [*a{}, *a{}]",
                index - 1,
                index - 1
            )
            .expect("append alias");
        }
        assert!(
            super::parse("fixture.yml", &aliases)
                .expect_err("alias budget")
                .contains("expanded")
        );
        super::parse(
            "fixture.yml",
            "first: &runner {os: ubuntu-24.04}\nsecond: *runner\n",
        )
        .expect("bounded valid aliases");
    }

    #[test]
    fn alias_chains_cannot_bypass_the_nesting_limit() {
        use std::fmt::Write;

        let mut source = "a0: &a0 [leaf]\n".to_owned();
        for index in 1..super::MAX_DEPTH {
            writeln!(&mut source, "a{index}: &a{index} [*a{}]", index - 1)
                .expect("append nested alias");
        }
        assert!(
            super::parse("fixture.yml", &source)
                .expect_err("expanded depth")
                .contains("nesting")
        );
    }

    #[test]
    fn recursive_and_undefined_aliases_are_rejected() {
        for source in ["a: &a [*a]\n", "a: *missing\n"] {
            assert!(super::parse("fixture.yml", source).is_err(), "{source:?}");
        }
    }

    #[test]
    fn unrelated_runner_does_not_hide_a_changed_required_runner() {
        let changed: Vec<_> = SOURCES
            .iter()
            .map(|(name, source)| {
                (
                    *name,
                    if *name == "ci.yml" {
                        format!(
                            "{}\n# ubuntu-24.04 still occurs elsewhere\n",
                            source.replacen(
                                "os: [ubuntu-24.04, macos-15]",
                                "os: [ubuntu-22.04, macos-15]",
                                1
                            )
                        )
                    } else {
                        (*source).to_owned()
                    },
                )
            })
            .collect();
        let borrowed: Vec<_> = changed
            .iter()
            .map(|(name, source)| (*name, source.as_str()))
            .collect();
        assert!(
            super::check_sources(&borrowed)
                .expect_err("runner policy")
                .contains("jobs.clippy.strategy.matrix.os")
        );
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(96))]
        #[test]
        fn arbitrary_yaml_never_panics(source in proptest::collection::vec(proptest::char::any(), 0..4096)) {
            let source: String = source.into_iter().collect();
            let _ = super::parse("property.yml", &source);
        }
    }
}
