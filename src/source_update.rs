//! Concurrent, source-scoped acquisition updates.
//!
//! Each registered source owns its discovery and fetch implementation plus an
//! independent request policy. The coordinator only runs adapters concurrently,
//! collects deterministic outcomes, and keeps incremental updates distinct from
//! explicit full-source repair crawls.

use crate::legal_source::{source_registry, SourceId, SourceRegistry};
use crate::source::{link_download, scrape_diff, LinkDownloadArgs};
use anyhow::{anyhow, bail, Context, Result};
use fs2::FileExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};

const MAX_INVENTORY_LINE_BYTES: usize = 4 * 1024 * 1024;
const ATO_WHATS_NEW_URL: &str = "https://www.ato.gov.au/law/view/whatsnew.htm?fid=whatsnew";
const ATO_BASE_URL: &str = "https://www.ato.gov.au";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct SourceRatePolicy {
    pub(crate) minimum_request_interval_ms: u64,
    pub(crate) max_concurrency: usize,
    pub(crate) request_timeout_seconds: u64,
}

impl SourceRatePolicy {
    fn validate(self, source: &SourceId) -> Result<()> {
        if self.max_concurrency == 0 {
            bail!("source `{source}` rate policy requires positive concurrency");
        }
        if self.request_timeout_seconds == 0 {
            bail!("source `{source}` rate policy requires a positive timeout");
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SourceUpdateRequest {
    pub(crate) source: SourceId,
    pub(crate) workspace: PathBuf,
    pub(crate) run_dir: PathBuf,
}

#[derive(Debug)]
pub(crate) struct SourceUpdateContext {
    pub(crate) workspace: PathBuf,
    pub(crate) run_dir: PathBuf,
    _workspace_lock: File,
}

pub(crate) fn lock_workspace_exclusive(workspace: &Path) -> Result<File> {
    lock_workspace(workspace, true)
}

pub(crate) fn lock_workspace_shared(workspace: &Path) -> Result<File> {
    lock_workspace(workspace, false)
}

fn lock_workspace(workspace: &Path, exclusive: bool) -> Result<File> {
    let lock_path = workspace.join(".source-update.lock");
    if fs::symlink_metadata(&lock_path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!(
            "source workspace lock must not be a symlink: {}",
            lock_path.display()
        );
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening source workspace lock {}", lock_path.display()))?;
    let result = if exclusive {
        lock.try_lock_exclusive()
    } else {
        fs2::FileExt::try_lock_shared(&lock)
    };
    result.with_context(|| {
        format!(
            "source workspace is already being updated{}: {}",
            if exclusive { "" } else { " during a build" },
            workspace.display()
        )
    })?;
    Ok(lock)
}

#[derive(Clone, Debug)]
pub(crate) struct SourceDiscoveryBatch {
    pub(crate) path: PathBuf,
    pub(crate) records: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct SourceFetchReport {
    pub(crate) completed: usize,
    pub(crate) failed: usize,
    pub(crate) skipped: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct SourceInventoryFingerprint {
    pub(crate) records: usize,
    pub(crate) sha256: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SourceUpdateReport {
    pub(crate) source: SourceId,
    pub(crate) workspace: PathBuf,
    pub(crate) run_dir: PathBuf,
    pub(crate) rate_policy: SourceRatePolicy,
    pub(crate) discovered: usize,
    pub(crate) completed: usize,
    pub(crate) failed: usize,
    pub(crate) skipped: usize,
    pub(crate) changed: bool,
    pub(crate) inventory_before: SourceInventoryFingerprint,
    pub(crate) inventory_after: SourceInventoryFingerprint,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum SourceUpdateOutcome {
    Current {
        #[serde(flatten)]
        report: SourceUpdateReport,
    },
    Updated {
        #[serde(flatten)]
        report: SourceUpdateReport,
    },
    Partial {
        #[serde(flatten)]
        report: SourceUpdateReport,
    },
    Failed {
        source: SourceId,
        workspace: PathBuf,
        run_dir: PathBuf,
        error: String,
    },
}

impl SourceUpdateOutcome {
    pub(crate) fn source(&self) -> &SourceId {
        match self {
            Self::Current { report } | Self::Updated { report } | Self::Partial { report } => {
                &report.source
            }
            Self::Failed { source, .. } => source,
        }
    }

    pub(crate) fn is_success(&self) -> bool {
        matches!(self, Self::Current { .. } | Self::Updated { .. })
    }
}

pub(crate) trait SourceAcquisition: Send + Sync {
    fn rate_policy(&self) -> SourceRatePolicy;

    fn inventory(&self, context: &SourceUpdateContext) -> Result<SourceInventoryFingerprint>;

    fn discover_incremental(&self, context: &SourceUpdateContext) -> Result<SourceDiscoveryBatch>;

    fn fetch(
        &self,
        context: &SourceUpdateContext,
        discovery: &SourceDiscoveryBatch,
    ) -> Result<SourceFetchReport>;
}

#[derive(Debug)]
pub(crate) struct AtoAcquisition;

pub(crate) static ATO_ACQUISITION: AtoAcquisition = AtoAcquisition;

impl SourceAcquisition for AtoAcquisition {
    fn rate_policy(&self) -> SourceRatePolicy {
        SourceRatePolicy {
            minimum_request_interval_ms: 50,
            max_concurrency: 4,
            request_timeout_seconds: 30,
        }
    }

    fn inventory(&self, context: &SourceUpdateContext) -> Result<SourceInventoryFingerprint> {
        fingerprint_jsonl(&ato_index_path(context)?)
    }

    fn discover_incremental(&self, context: &SourceUpdateContext) -> Result<SourceDiscoveryBatch> {
        let index_path = ato_index_path(context)?;
        let path = context.run_dir.join("pending.jsonl");
        scrape_diff(&index_path, None, Some(ATO_WHATS_NEW_URL), None, &path)?;
        let records = fingerprint_jsonl(&path)?.records;
        Ok(SourceDiscoveryBatch { path, records })
    }

    fn fetch(
        &self,
        context: &SourceUpdateContext,
        discovery: &SourceDiscoveryBatch,
    ) -> Result<SourceFetchReport> {
        if discovery.records == 0 {
            return Ok(SourceFetchReport::default());
        }
        let policy = self.rate_policy();
        let report = link_download(LinkDownloadArgs {
            deduped_links: discovery.path.clone(),
            out_dir: context.workspace.clone(),
            base_url: ATO_BASE_URL.to_string(),
            request_delay_seconds: policy.minimum_request_interval_ms as f64 / 1_000.0,
            max_workers: policy.max_concurrency,
            timeout_seconds: policy.request_timeout_seconds as f64,
            force: false,
            workspace_lock_held: true,
        })?;
        Ok(SourceFetchReport {
            completed: report.completed,
            failed: report.errors,
            skipped: report.skipped,
        })
    }
}

pub(crate) fn run_source_updates(
    requests: Vec<SourceUpdateRequest>,
) -> Result<Vec<SourceUpdateOutcome>> {
    run_source_updates_with_registry(source_registry(), requests)
}

fn run_source_updates_with_registry(
    registry: &SourceRegistry,
    requests: Vec<SourceUpdateRequest>,
) -> Result<Vec<SourceUpdateOutcome>> {
    if requests.is_empty() {
        bail!("source-update requires at least one source workspace");
    }

    let mut seen = BTreeSet::new();
    let mut jobs = Vec::with_capacity(requests.len());
    for request in requests {
        if !seen.insert(request.source.clone()) {
            bail!("duplicate source workspace `{}`", request.source);
        }
        let source = registry.source(&request.source)?;
        let acquisition = source
            .acquisition()
            .ok_or_else(|| anyhow!("source `{}` has no acquisition adapter", request.source))?;
        acquisition.rate_policy().validate(&request.source)?;
        let context = prepare_context(&request)?;
        jobs.push((request.source, acquisition, context));
    }

    let mut outcomes = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(jobs.len());
        for (source, acquisition, context) in jobs {
            let workspace = context.workspace.clone();
            let run_dir = context.run_dir.clone();
            let worker_source = source.clone();
            let handle = scope.spawn(move || update_one(worker_source, acquisition, context));
            handles.push((source, workspace, run_dir, handle));
        }

        handles
            .into_iter()
            .map(|(source, workspace, run_dir, handle)| match handle.join() {
                Ok(Ok(outcome)) => outcome,
                Ok(Err(error)) => SourceUpdateOutcome::Failed {
                    source: source.clone(),
                    workspace,
                    run_dir,
                    error: format!("{error:#}"),
                },
                Err(_) => SourceUpdateOutcome::Failed {
                    source: source.clone(),
                    workspace,
                    run_dir,
                    error: "source update worker panicked".to_string(),
                },
            })
            .collect::<Vec<_>>()
    });
    outcomes.sort_by(|left, right| left.source().cmp(right.source()));
    Ok(outcomes)
}

fn update_one(
    source: SourceId,
    acquisition: &dyn SourceAcquisition,
    context: SourceUpdateContext,
) -> Result<SourceUpdateOutcome> {
    let inventory_before = acquisition.inventory(&context)?;
    let discovery = acquisition.discover_incremental(&context)?;
    let fetch = acquisition.fetch(&context, &discovery)?;
    let inventory_after = acquisition.inventory(&context)?;
    let report = SourceUpdateReport {
        source,
        workspace: context.workspace,
        run_dir: context.run_dir,
        rate_policy: acquisition.rate_policy(),
        discovered: discovery.records,
        completed: fetch.completed,
        failed: fetch.failed,
        skipped: fetch.skipped,
        changed: inventory_before != inventory_after,
        inventory_before,
        inventory_after,
    };

    if report.failed > 0 {
        Ok(SourceUpdateOutcome::Partial { report })
    } else if report.changed {
        Ok(SourceUpdateOutcome::Updated { report })
    } else {
        Ok(SourceUpdateOutcome::Current { report })
    }
}

fn prepare_context(request: &SourceUpdateRequest) -> Result<SourceUpdateContext> {
    let workspace_metadata = fs::symlink_metadata(&request.workspace).with_context(|| {
        format!(
            "reading source workspace metadata {}",
            request.workspace.display()
        )
    })?;
    if workspace_metadata.file_type().is_symlink() || !workspace_metadata.is_dir() {
        bail!(
            "source workspace must be a real directory, not a symlink: {}",
            request.workspace.display()
        );
    }
    let workspace = request.workspace.canonicalize().with_context(|| {
        format!(
            "canonicalizing source workspace {}",
            request.workspace.display()
        )
    })?;

    if request.run_dir.exists() {
        bail!(
            "source run directory already exists: {}",
            request.run_dir.display()
        );
    }
    let run_parent = request
        .run_dir
        .parent()
        .ok_or_else(|| anyhow!("source run directory has no parent"))?;
    if request
        .run_dir
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        bail!(
            "source run directory must not contain parent traversal: {}",
            request.run_dir.display()
        );
    }
    let planned_run_parent = canonicalize_planned_path(run_parent)?;
    if planned_run_parent.starts_with(&workspace) || workspace.starts_with(&planned_run_parent) {
        bail!(
            "source workspace and run directory must not overlap: {} and {}",
            workspace.display(),
            request.run_dir.display()
        );
    }

    let workspace_lock = lock_workspace_exclusive(&workspace)?;

    fs::create_dir_all(run_parent)
        .with_context(|| format!("creating source run parent {}", run_parent.display()))?;
    let run_parent = run_parent.canonicalize()?;
    if run_parent.starts_with(&workspace) || workspace.starts_with(&run_parent) {
        bail!(
            "source workspace and run directory must not overlap: {} and {}",
            workspace.display(),
            request.run_dir.display()
        );
    }
    fs::create_dir(&request.run_dir).with_context(|| {
        format!(
            "creating source run directory {}",
            request.run_dir.display()
        )
    })?;
    let run_dir = request.run_dir.canonicalize()?;

    Ok(SourceUpdateContext {
        workspace,
        run_dir,
        _workspace_lock: workspace_lock,
    })
}

fn canonicalize_planned_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut existing = absolute.as_path();
    let mut missing = Vec::new();
    while !existing.exists() {
        let name = existing
            .file_name()
            .ok_or_else(|| anyhow!("planned path has no existing ancestor: {}", path.display()))?;
        missing.push(name.to_os_string());
        existing = existing
            .parent()
            .ok_or_else(|| anyhow!("planned path has no existing ancestor: {}", path.display()))?;
    }
    let mut resolved = existing.canonicalize()?;
    for component in missing.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn ato_index_path(context: &SourceUpdateContext) -> Result<PathBuf> {
    let index_path = context.workspace.join("index.jsonl");
    let metadata = fs::symlink_metadata(&index_path)
        .with_context(|| format!("reading ATO source index {}", index_path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "ATO source index must be a regular file, not a symlink: {}",
            index_path.display()
        );
    }
    let canonical = index_path.canonicalize()?;
    if !canonical.starts_with(&context.workspace) {
        bail!("ATO source index escaped its workspace");
    }
    Ok(canonical)
}

fn fingerprint_jsonl(path: &Path) -> Result<SourceInventoryFingerprint> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reading inventory metadata {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "source inventory must be a regular file, not a symlink: {}",
            path.display()
        );
    }

    let mut reader = BufReader::new(
        File::open(path).with_context(|| format!("opening source inventory {}", path.display()))?,
    );
    let mut hasher = Sha256::new();
    let mut records = 0usize;
    let mut line = Vec::new();
    loop {
        line.clear();
        let bytes = reader.read_until(b'\n', &mut line)?;
        if bytes == 0 {
            break;
        }
        if line.len() > MAX_INVENTORY_LINE_BYTES {
            bail!(
                "source inventory line exceeded {MAX_INVENTORY_LINE_BYTES} bytes in {}",
                path.display()
            );
        }
        hasher.update(&line);
        if line.iter().any(|byte| !byte.is_ascii_whitespace()) {
            std::str::from_utf8(&line)
                .with_context(|| format!("source inventory is not UTF-8: {}", path.display()))?;
            records += 1;
        }
    }

    Ok(SourceInventoryFingerprint {
        records,
        sha256: format!("{:x}", hasher.finalize()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::legal_source::{LegalSource, SourceDescriptor};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::tempdir;

    #[derive(Debug)]
    struct FakeSource {
        descriptor: SourceDescriptor,
        acquisition: FakeAcquisition,
    }

    impl LegalSource for FakeSource {
        fn descriptor(&self) -> &SourceDescriptor {
            &self.descriptor
        }

        fn acquisition(&self) -> Option<&dyn SourceAcquisition> {
            Some(&self.acquisition)
        }
    }

    #[derive(Debug)]
    struct FakeAcquisition {
        policy: SourceRatePolicy,
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
        inventory: Arc<AtomicUsize>,
        fail_fetch: bool,
    }

    impl SourceAcquisition for FakeAcquisition {
        fn rate_policy(&self) -> SourceRatePolicy {
            self.policy
        }

        fn inventory(&self, _context: &SourceUpdateContext) -> Result<SourceInventoryFingerprint> {
            let value = self.inventory.load(Ordering::SeqCst);
            Ok(SourceInventoryFingerprint {
                records: value,
                sha256: format!("{value:064x}"),
            })
        }

        fn discover_incremental(
            &self,
            context: &SourceUpdateContext,
        ) -> Result<SourceDiscoveryBatch> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(60));
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(SourceDiscoveryBatch {
                path: context.run_dir.join("pending.jsonl"),
                records: 1,
            })
        }

        fn fetch(
            &self,
            _context: &SourceUpdateContext,
            _discovery: &SourceDiscoveryBatch,
        ) -> Result<SourceFetchReport> {
            if self.fail_fetch {
                bail!("intentional source failure");
            }
            self.inventory.fetch_add(1, Ordering::SeqCst);
            Ok(SourceFetchReport {
                completed: 1,
                failed: 0,
                skipped: 0,
            })
        }
    }

    fn fake_source(
        id: &str,
        policy: SourceRatePolicy,
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
        fail_fetch: bool,
    ) -> Box<dyn LegalSource> {
        Box::new(FakeSource {
            descriptor: SourceDescriptor::new(id.parse().expect("valid fake source id"), id)
                .expect("valid fake source descriptor"),
            acquisition: FakeAcquisition {
                policy,
                active,
                max_active,
                inventory: Arc::new(AtomicUsize::new(1)),
                fail_fetch,
            },
        })
    }

    #[test]
    fn source_updates_run_concurrently_with_independent_policies() -> Result<()> {
        let temp = tempdir()?;
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let ato_policy = SourceRatePolicy {
            minimum_request_interval_ms: 50,
            max_concurrency: 4,
            request_timeout_seconds: 30,
        };
        let wa_policy = SourceRatePolicy {
            minimum_request_interval_ms: 250,
            max_concurrency: 1,
            request_timeout_seconds: 45,
        };
        let registry = SourceRegistry::try_new(
            "ato".parse()?,
            vec![
                fake_source(
                    "ato",
                    ato_policy,
                    Arc::clone(&active),
                    Arc::clone(&max_active),
                    false,
                ),
                fake_source(
                    "wa",
                    wa_policy,
                    Arc::clone(&active),
                    Arc::clone(&max_active),
                    false,
                ),
            ],
        )?;
        let ato_workspace = temp.path().join("ato-workspace");
        let wa_workspace = temp.path().join("wa-workspace");
        fs::create_dir_all(&ato_workspace)?;
        fs::create_dir_all(&wa_workspace)?;
        let outcomes = run_source_updates_with_registry(
            &registry,
            vec![
                SourceUpdateRequest {
                    source: "ato".parse()?,
                    workspace: ato_workspace,
                    run_dir: temp.path().join("runs/ato"),
                },
                SourceUpdateRequest {
                    source: "wa".parse()?,
                    workspace: wa_workspace,
                    run_dir: temp.path().join("runs/wa"),
                },
            ],
        )?;

        assert_eq!(max_active.load(Ordering::SeqCst), 2);
        assert!(outcomes.iter().all(SourceUpdateOutcome::is_success));
        let policies: Vec<SourceRatePolicy> = outcomes
            .iter()
            .filter_map(|outcome| match outcome {
                SourceUpdateOutcome::Updated { report } => Some(report.rate_policy),
                _ => None,
            })
            .collect();
        assert_eq!(policies, vec![ato_policy, wa_policy]);
        Ok(())
    }

    #[test]
    fn source_update_failure_does_not_cancel_other_sources() -> Result<()> {
        let temp = tempdir()?;
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let policy = SourceRatePolicy {
            minimum_request_interval_ms: 1,
            max_concurrency: 1,
            request_timeout_seconds: 1,
        };
        let registry = SourceRegistry::try_new(
            "ato".parse()?,
            vec![
                fake_source(
                    "ato",
                    policy,
                    Arc::clone(&active),
                    Arc::clone(&max_active),
                    false,
                ),
                fake_source(
                    "broken",
                    policy,
                    Arc::clone(&active),
                    Arc::clone(&max_active),
                    true,
                ),
            ],
        )?;
        let ato_workspace = temp.path().join("ato-workspace");
        let broken_workspace = temp.path().join("broken-workspace");
        fs::create_dir_all(&ato_workspace)?;
        fs::create_dir_all(&broken_workspace)?;
        let outcomes = run_source_updates_with_registry(
            &registry,
            vec![
                SourceUpdateRequest {
                    source: "broken".parse()?,
                    workspace: broken_workspace,
                    run_dir: temp.path().join("runs/broken"),
                },
                SourceUpdateRequest {
                    source: "ato".parse()?,
                    workspace: ato_workspace,
                    run_dir: temp.path().join("runs/ato"),
                },
            ],
        )?;

        assert!(matches!(outcomes[0], SourceUpdateOutcome::Updated { .. }));
        assert!(matches!(outcomes[1], SourceUpdateOutcome::Failed { .. }));
        assert!(outcomes[0].is_success());
        assert!(!outcomes[1].is_success());
        Ok(())
    }

    #[test]
    fn fingerprint_jsonl_is_exact_and_bounded() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("index.jsonl");
        fs::write(&path, b"{\"id\":1}\n\n{\"id\":2}\n")?;
        let fingerprint = fingerprint_jsonl(&path)?;
        assert_eq!(fingerprint.records, 2);
        assert_eq!(
            fingerprint.sha256,
            format!("{:x}", Sha256::digest(b"{\"id\":1}\n\n{\"id\":2}\n"))
        );
        Ok(())
    }

    #[test]
    fn update_run_directory_cannot_overlap_or_mutate_the_source_workspace() -> Result<()> {
        let temp = tempdir()?;
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace)?;
        let run_dir = workspace.join("runs/ato");
        let error = prepare_context(&SourceUpdateRequest {
            source: "ato".parse()?,
            workspace: workspace.clone(),
            run_dir: run_dir.clone(),
        })
        .expect_err("overlapping run directory must fail");
        assert!(error.to_string().contains("must not overlap"));
        assert!(!workspace.join("runs").exists());
        Ok(())
    }

    #[test]
    fn workspace_lock_rejects_concurrent_source_updates() -> Result<()> {
        let temp = tempdir()?;
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace)?;
        let first = prepare_context(&SourceUpdateRequest {
            source: "ato".parse()?,
            workspace: workspace.clone(),
            run_dir: temp.path().join("runs/first"),
        })?;
        let error = prepare_context(&SourceUpdateRequest {
            source: "ato".parse()?,
            workspace,
            run_dir: temp.path().join("runs/second"),
        })
        .expect_err("concurrent update must fail while the first lock is held");
        assert!(error
            .to_string()
            .contains("source workspace is already being updated"));
        drop(first);
        Ok(())
    }

    #[test]
    fn shared_build_lock_rejects_a_source_update() -> Result<()> {
        let temp = tempdir()?;
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace)?;
        let build_lock = lock_workspace_shared(&workspace)?;
        let error = prepare_context(&SourceUpdateRequest {
            source: "ato".parse()?,
            workspace,
            run_dir: temp.path().join("runs/update"),
        })
        .expect_err("source update must wait for a build snapshot");
        assert!(error
            .to_string()
            .contains("source workspace is already being updated"));
        drop(build_lock);
        Ok(())
    }

    #[test]
    fn invalid_source_rate_policy_is_rejected_before_update_work() -> Result<()> {
        let temp = tempdir()?;
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let registry = SourceRegistry::try_new(
            "ato".parse()?,
            vec![fake_source(
                "ato",
                SourceRatePolicy {
                    minimum_request_interval_ms: 0,
                    max_concurrency: 0,
                    request_timeout_seconds: 30,
                },
                active,
                max_active,
                false,
            )],
        )?;
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace)?;
        let error = run_source_updates_with_registry(
            &registry,
            vec![SourceUpdateRequest {
                source: "ato".parse()?,
                workspace,
                run_dir: temp.path().join("runs/ato"),
            }],
        )
        .expect_err("zero concurrency must fail before the run directory is created");
        assert!(error.to_string().contains("positive concurrency"));
        assert!(!temp.path().join("runs").exists());
        Ok(())
    }
}
