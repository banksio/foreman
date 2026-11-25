use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use chrono::{DateTime, TimeDelta, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    DockerConfig, Enrollment, SCHEMA_VERSION, eligibility,
    engine::{self, Container, Engine},
    input,
};
use crate::{AppResult, Context, Error, Outcome, Report, process, store};

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub(super) struct StackResult {
    pub id: String,
    pub outcome: String,
    pub reason: String,
    pub changes: Vec<ImageChange>,
    pub warnings: Vec<String>,
    pub at: DateTime<Utc>,
}

impl StackResult {
    fn new(id: &str, outcome: &str, reason: impl Into<String>) -> Self {
        Self {
            id: id.to_owned(),
            outcome: outcome.to_owned(),
            reason: reason.into(),
            changes: vec![],
            warnings: vec![],
            at: Utc::now(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub(super) struct ImageChange {
    service: String,
    reference: String,
    old_id: String,
    new_id: String,
    manifest: String,
    platform: String,
    old_version: Option<String>,
    new_version: Option<String>,
    replicas: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Recovery {
    workdir: PathBuf,
    files: Vec<PathBuf>,
    changes: Vec<ImageChange>,
    at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Hold {
    image: String,
    tag: String,
    expires_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct State {
    schema_version: u32,
    pub last_result: Option<StackResult>,
    pub recovery: Option<Recovery>,
    pub holds: Vec<Hold>,
    failed_candidate: BTreeMap<String, String>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            last_result: None,
            recovery: None,
            holds: vec![],
            failed_candidate: BTreeMap::new(),
        }
    }
}

fn path(ctx: &Context, id: &str) -> PathBuf {
    ctx.paths
        .state_dir
        .join("docker")
        .join(format!("{id}.json"))
}

pub(super) fn load(ctx: &Context, id: &str) -> AppResult<State> {
    let path = path(ctx, id);
    if input::exists(&path)? {
        input::trusted(&path, false)?;
    }
    let state = store::read_json::<State>(&path)?.unwrap_or_default();
    if state.schema_version != SCHEMA_VERSION {
        return Err(Error::invalid(format!(
            "Unsupported Docker observations schema for {id}"
        )));
    }
    if state
        .holds
        .iter()
        .any(|hold| !engine::image_id(&hold.image) || hold.tag != hold_tag(&hold.image))
        || state.recovery.as_ref().is_some_and(|recovery| {
            recovery.changes.is_empty()
                || recovery.changes.iter().any(|change| {
                    !input::service_name(&change.service)
                        || !engine::image_ref(&change.reference)
                        || !engine::image_id(&change.old_id)
                        || !engine::image_id(&change.new_id)
                        || change.replicas == 0
                })
        })
    {
        return Err(Error::invalid(format!(
            "Invalid Docker recovery observations for {id}"
        )));
    }
    Ok(state)
}

fn save(ctx: &Context, id: &str, state: &State) -> AppResult<()> {
    store::write_json(&path(ctx, id), state)
}

pub(super) fn check_drift(
    engine: &Engine,
    enrollment: &Enrollment,
    containers: &[Container],
) -> AppResult<Option<String>> {
    drift(enrollment, containers, &input::hashes(engine, enrollment))
}

fn drift(
    enrollment: &Enrollment,
    containers: &[Container],
    hashes: &AppResult<BTreeMap<String, String>>,
) -> AppResult<Option<String>> {
    let matches = hashes.as_ref().is_ok_and(|hashes| {
        containers
            .iter()
            .filter(|container| container.running())
            .all(|container| {
                container
                    .service()
                    .and_then(|service| hashes.get(service))
                    .is_some_and(|hash| container.hash() == Some(hash))
            })
    });
    if matches {
        return Ok(None);
    }
    let reason = "Compose config hashes differ from running services or cannot be compared";
    if enrollment.warn_on_config_drift {
        Ok(Some(format!(
            "{reason}. The enrolled warning override permits maintenance."
        )))
    } else {
        Err(Error::blocked(format!(
            "{reason}. Review the enrolled inputs and redeploy before trying again."
        )))
    }
}

fn stable(container: &Container, settle: u64) -> bool {
    container.alive() && (Utc::now() - container.started()).num_seconds() >= settle as i64
}

fn running_services(
    containers: &[Container],
    settle: u64,
) -> AppResult<BTreeMap<&str, Vec<&Container>>> {
    let running: BTreeSet<_> = containers
        .iter()
        .filter(|container| container.running())
        .filter_map(Container::service)
        .collect();
    let mut services = BTreeMap::<&str, Vec<&Container>>::new();
    for container in containers {
        let service = container
            .service()
            .filter(|service| input::service_name(service))
            .ok_or_else(|| Error::blocked("Docker returned an invalid Compose service identity"))?;
        if running.contains(service) {
            if !stable(container, settle) {
                return Err(Error::blocked(format!(
                    "Service {service} has stopped, unhealthy, or recently started replicas"
                )));
            }
            services.entry(service).or_default().push(container);
        }
    }
    Ok(services)
}

fn repository(reference: &str) -> &str {
    let named = reference.split('@').next().unwrap_or(reference);
    match named.rfind(':') {
        Some(tag) if named.rfind('/').is_none_or(|slash| slash < tag) => &named[..tag],
        _ => named,
    }
}

fn canonical_ref(reference: &str) -> String {
    let (name, tag) = match reference.rsplit_once(':') {
        Some((name, tag)) if !tag.contains('/') => (name, tag),
        _ => (reference, "latest"),
    };
    let first = name.split_once('/').map(|(first, _)| first);
    let normalized = match first {
        None => format!("docker.io/library/{name}"),
        Some(first) if first == "docker.io" || first == "index.docker.io" => {
            let remainder = &name[first.len() + 1..];
            if remainder.contains('/') {
                format!("docker.io/{remainder}")
            } else {
                format!("docker.io/library/{remainder}")
            }
        }
        Some(first) if !first.contains(['.', ':']) && first != "localhost" => {
            format!("docker.io/{name}")
        }
        Some(_) => name.to_owned(),
    };
    format!("{normalized}:{tag}")
}

fn plan(
    engine: &Engine,
    model: &input::Compose,
    containers: &[Container],
    config: &DockerConfig,
) -> AppResult<Vec<ImageChange>> {
    let mut changes = Vec::new();
    let mut old_images = BTreeMap::new();
    let mut remote_images = BTreeMap::new();
    for (service, replicas) in running_services(containers, config.settle)? {
        let definition = model.services.get(service).ok_or_else(|| {
            Error::blocked(format!(
                "Running service {service} is missing from the enrolled Compose model"
            ))
        })?;
        if definition
            .labels
            .get("io.banksio.foreman.exclude")
            .is_some_and(|value| value.eq_ignore_ascii_case("true"))
        {
            return Err(Error::blocked(format!(
                "Service {service} explicitly excludes the stack"
            )));
        }
        let Some(reference) = definition
            .image
            .as_deref()
            .filter(|_| definition.build.is_none())
        else {
            continue;
        };
        if reference.contains('@') {
            continue;
        }
        if !engine::image_ref(reference) || repository(reference).is_empty() {
            return Err(Error::blocked(format!(
                "Service {service} has an unsupported image reference"
            )));
        }
        let canonical = canonical_ref(reference);
        let old_id = &replicas[0].image_id;
        if replicas.iter().any(|container| {
            container.image_id != *old_id
                || !engine::image_ref(container.reference())
                || canonical_ref(container.reference()) != canonical
        }) {
            return Err(Error::blocked(format!(
                "Service {service} has mixed replica images or its effective image reference changed"
            )));
        }
        match old_images.insert(canonical.clone(), old_id) {
            Some(previous) if previous != old_id => {
                return Err(Error::blocked(
                    "Services share a tag with different rollback image IDs",
                ));
            }
            _ => {}
        }
        let (local, remote, platform) = match remote_images.entry(canonical) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                let local = engine.image(old_id)?;
                let remote = engine.manifest(reference, &local).map_err(|error| {
                    Error::blocked(format!("Service {service}: {}", error.message))
                })?;
                let mut platform = format!("{}/{}", local.os, local.architecture);
                if !local.variant.is_empty() {
                    platform.push('/');
                    platform.push_str(&local.variant);
                }
                entry.insert((local, remote, platform))
            }
        };
        if remote.config != *old_id {
            changes.push(ImageChange {
                service: service.to_owned(),
                reference: reference.to_owned(),
                old_id: old_id.clone(),
                new_id: remote.config.clone(),
                manifest: remote.manifest.clone(),
                platform: platform.clone(),
                old_version: local.version().map(str::to_owned),
                new_version: None,
                replicas: replicas.len(),
            });
        }
    }
    Ok(changes)
}

type Replica = (DateTime<Utc>, u64);

fn selected(
    enrollment: &Enrollment,
    containers: &[Container],
    changes: &[ImageChange],
    old: bool,
) -> Option<BTreeMap<String, Replica>> {
    if !enrollment.matches(containers) {
        return None;
    }
    let mut replicas = BTreeMap::new();
    for change in changes {
        let current: Vec<_> = containers
            .iter()
            .filter(|container| container.service() == Some(change.service.as_str()))
            .collect();
        if current.len() != change.replicas {
            return None;
        }
        let expected = if old { &change.old_id } else { &change.new_id };
        for container in current {
            if !container.alive()
                || container.image_id != *expected
                || canonical_ref(container.reference()) != canonical_ref(&change.reference)
                || (!old && container.restart_count != 0)
            {
                return None;
            }
            replicas.insert(
                container.id.clone(),
                (container.started(), container.restart_count),
            );
        }
    }
    Some(replicas)
}

fn wait_for_health(
    engine: &Engine,
    enrollment: &Enrollment,
    changes: &[ImageChange],
    config: &DockerConfig,
    old: bool,
    events_since: DateTime<Utc>,
) -> AppResult<()> {
    let deadline = Instant::now() + Duration::from_secs(config.health_timeout);
    let settle = Duration::from_secs(config.settle);
    let mut first_seen = Instant::now();
    let mut previous = None;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        if process::interrupted() {
            return Err(Error::failed("Docker health verification was interrupted"));
        }
        let containers = engine.containers(
            Some(&enrollment.project),
            true,
            remaining.as_secs().clamp(1, 10),
        );
        let current = containers
            .ok()
            .and_then(|containers| selected(enrollment, &containers, changes, old));
        if current != previous {
            first_seen = Instant::now();
        }
        if first_seen.elapsed() >= settle
            && current.as_ref().is_some_and(|replicas| {
                replicas
                    .values()
                    .all(|(start, _)| (Utc::now() - *start).num_seconds() >= config.settle as i64)
            })
        {
            if engine.oom_events(events_since, Utc::now(), &enrollment.project)? {
                return Err(Error::failed(
                    "Docker reported an OOM event during the verification window",
                ));
            }
            return Ok(());
        }
        previous = current;
        thread::sleep(remaining.min(Duration::from_secs(2)));
    }
    Err(Error::failed(
        "Compose replicas did not reach sustained health on the expected images before the deadline",
    ))
}

fn fingerprint(changes: &[ImageChange]) -> BTreeMap<String, String> {
    changes
        .iter()
        .map(|change| (change.service.clone(), change.new_id.clone()))
        .collect()
}

fn hold_tag(image: &str) -> String {
    format!("foreman-hold/local:{}", image.trim_start_matches("sha256:"))
}

fn hold_images(
    ctx: &Context,
    engine: &Engine,
    enrollment: &Enrollment,
    state: &mut State,
    changes: &[ImageChange],
) -> AppResult<()> {
    let expires_at = Utc::now() + TimeDelta::days(7);
    for image in changes
        .iter()
        .map(|change| &change.old_id)
        .collect::<BTreeSet<_>>()
    {
        let tag = hold_tag(image);
        if let Some(hold) = state.holds.iter_mut().find(|hold| hold.image == *image) {
            hold.expires_at = expires_at;
        } else {
            state.holds.push(Hold {
                image: image.clone(),
                tag,
                expires_at,
            });
        }
    }
    // Save intent before adding tags so interrupted work keeps protecting all old images.
    save(ctx, &enrollment.project, state)?;
    for image in changes
        .iter()
        .map(|change| &change.old_id)
        .collect::<BTreeSet<_>>()
    {
        let tag = hold_tag(image);
        engine.run(
            &["image", "tag", "--", image, &tag],
            30,
            "Cannot protect the Docker rollback image",
        )?;
        if engine.image(&tag)?.id != *image {
            return Err(Error::failed(
                "Cannot verify the protected Docker rollback image",
            ));
        }
    }
    Ok(())
}

fn tagged_with(engine: &Engine, changes: &[ImageChange], old: bool) -> AppResult<()> {
    let mut tagged = BTreeSet::new();
    for change in changes {
        let image = if old { &change.old_id } else { &change.new_id };
        if tagged.insert(canonical_ref(&change.reference)) {
            engine.run(
                &["image", "tag", "--", image, &change.reference],
                30,
                "Cannot set the Compose image tag",
            )?;
        }
    }
    for change in changes {
        let expected = if old { &change.old_id } else { &change.new_id };
        if engine.image(&change.reference)?.id.as_str() != expected.as_str() {
            return Err(Error::failed("Compose image tag changed before recreation"));
        }
    }
    Ok(())
}

fn recreate(engine: &Engine, enrollment: &Enrollment, changes: &[ImageChange]) -> AppResult<()> {
    input::audit(enrollment)?;
    let scales: Vec<_> = changes
        .iter()
        .map(|change| format!("{}={}", change.service, change.replicas))
        .collect();
    let mut args = enrollment.compose_args([
        "up",
        "--detach",
        "--no-deps",
        "--no-build",
        "--pull",
        "never",
        "--force-recreate",
    ]);
    for scale in &scales {
        args.extend(["--scale", scale.as_str()]);
    }
    args.push("--");
    args.extend(changes.iter().map(|change| change.service.as_str()));
    engine.run(
        &args,
        900,
        "Compose could not recreate the selected services",
    )?;
    Ok(())
}

fn pull(engine: &Engine, changes: &mut [ImageChange]) -> AppResult<()> {
    let mut images = BTreeMap::new();
    for change in changes {
        let immutable = format!("{}@{}", repository(&change.reference), change.manifest);
        let version = match images.entry(immutable) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                engine.run(
                    &[
                        "image",
                        "pull",
                        "--quiet",
                        "--platform",
                        &change.platform,
                        "--",
                        entry.key(),
                    ],
                    1200,
                    "Docker could not pull the verified manifest",
                )?;
                let image = engine.image(entry.key())?;
                if image.id != change.new_id {
                    return Err(Error::failed(
                        "Pulled Docker image did not match the verified platform image ID",
                    ));
                }
                entry.insert(image.version().map(str::to_owned))
            }
        };
        change.new_version = version.clone();
    }
    Ok(())
}

fn recheck(
    engine: &Engine,
    enrollment: &Enrollment,
    original_hashes: &AppResult<BTreeMap<String, String>>,
    original_model: &input::Compose,
) -> AppResult<()> {
    let current_model = input::render(engine, enrollment)?;
    if original_model != &current_model {
        return Err(Error::blocked(
            "Compose input model changed while Foreman prepared the images",
        ));
    }
    let current_hashes = input::hashes(engine, enrollment);
    if original_hashes.as_ref().ok() != current_hashes.as_ref().ok() {
        return Err(Error::blocked(
            "Compose service hashes changed while Foreman prepared the images",
        ));
    }
    Ok(())
}

fn attempt(
    ctx: &Context,
    engine: &Engine,
    enrollment: &Enrollment,
    containers: &[Container],
    state: &mut State,
) -> AppResult<StackResult> {
    let id = &enrollment.project;
    let model = input::render(engine, enrollment)?;
    let hashes = input::hashes(engine, enrollment);
    let mut warnings = drift(enrollment, containers, &hashes)?
        .into_iter()
        .collect::<Vec<_>>();
    let mut changes = plan(engine, &model, containers, &ctx.config.compose)?;
    let candidate = fingerprint(&changes);
    let (outcome, reason) = if changes.is_empty() {
        (
            "current",
            "Eligible service images already match or are pinned or build-managed",
        )
    } else if candidate == state.failed_candidate {
        (
            "suppressed",
            "This image combination already failed. Clear the failure with docker stack retry after review.",
        )
    } else if ctx.dry_run {
        (
            "would_change",
            "Registry manifests identify new platform images. Tags can move after this preview.",
        )
    } else {
        pull(engine, &mut changes)?;
        wait_for_health(
            engine,
            enrollment,
            &changes,
            &ctx.config.compose,
            true,
            Utc::now(),
        )?;
        recheck(engine, enrollment, &hashes, &model)?;
        let original = selected(enrollment, containers, &changes, true);
        let current = engine.containers(Some(id), true, 30)?;
        if original.is_none() || selected(enrollment, &current, &changes, true) != original {
            return Err(Error::blocked("Running replicas changed before recreation"));
        }
        drift(enrollment, &current, &hashes)?;
        hold_images(ctx, engine, enrollment, state, &changes)?;
        state.recovery = Some(Recovery {
            workdir: enrollment.workdir.clone(),
            files: enrollment.files.clone(),
            changes: changes.clone(),
            at: Utc::now(),
        });
        save(ctx, id, state)?;
        let started_at = Utc::now();
        let result = tagged_with(engine, &changes, false)
            .and_then(|()| recreate(engine, enrollment, &changes))
            .and_then(|()| {
                wait_for_health(
                    engine,
                    enrollment,
                    &changes,
                    &ctx.config.compose,
                    false,
                    started_at,
                )
            });
        match result {
            Ok(()) => {
                state.recovery = None;
                state.failed_candidate.clear();
                (
                    "updated",
                    "Verified the new images and sustained replica health",
                )
            }
            Err(error) => {
                warnings.push(error.message);
                let rollback = Utc::now();
                let result = recheck(engine, enrollment, &hashes, &model)
                    .and_then(|()| engine.containers(Some(id), true, 30))
                    .and_then(|containers| {
                        enrollment
                            .matches(&containers)
                            .then_some(())
                            .ok_or_else(|| {
                                Error::blocked("Compose identity changed before rollback")
                            })
                    })
                    .and_then(|()| tagged_with(engine, &changes, true))
                    .and_then(|()| {
                        let current = engine.containers(Some(id), true, 30)?;
                        if selected(enrollment, &current, &changes, true).is_none() {
                            recreate(engine, enrollment, &changes)?;
                        }
                        Ok(())
                    })
                    .and_then(|()| {
                        wait_for_health(
                            engine,
                            enrollment,
                            &changes,
                            &ctx.config.compose,
                            true,
                            rollback,
                        )
                    });
                state.failed_candidate = candidate;
                match result {
                    Ok(()) => {
                        state.recovery = None;
                        (
                            "rolled_back",
                            "The update failed and the previous images and replica health were verified",
                        )
                    }
                    Err(error) => {
                        warnings.push(error.message);
                        (
                            "rollback_failed",
                            "The update and rollback could not be verified. The previous images are retained for recovery.",
                        )
                    }
                }
            }
        }
    };
    let mut result = StackResult::new(id, outcome, reason);
    result.changes = changes;
    result.warnings = warnings;
    Ok(result)
}

pub(super) fn retry(ctx: &Context, enrollment: &Enrollment) -> AppResult<Report> {
    let id = &enrollment.project;
    let mut state = load(ctx, id)?;
    if let Some(recovery) = &state.recovery {
        if recovery.workdir != enrollment.workdir || recovery.files != enrollment.files {
            return Err(Error::blocked(
                "Current enrollment differs from the recorded recovery identity",
            ));
        }
        let engine = Engine::new(enrollment.registry(ctx))?
            .with_budget(ctx.config.compose.health_timeout + 60);
        let current = engine.containers(Some(id), true, 25)?;
        if selected(enrollment, &current, &recovery.changes, true).is_none() {
            return Err(Error::blocked(
                "Restore the recorded previous images and healthy replicas before retrying",
            ));
        }
        wait_for_health(
            &engine,
            enrollment,
            &recovery.changes,
            &ctx.config.compose,
            true,
            Utc::now(),
        )?;
        state.recovery = None;
    } else if state.failed_candidate.is_empty() {
        return Ok(Report::new(
            Outcome::Unchanged,
            format!("Docker stack {id} has no recorded image failure"),
        ));
    }
    state.failed_candidate.clear();
    save(ctx, id, &state)?;
    Ok(Report::new(
        Outcome::Completed,
        format!("Cleared the image failure for Docker stack {id}"),
    ))
}

pub(super) fn maintain(
    ctx: &Context,
    enrollments: BTreeMap<String, Enrollment>,
    target: Option<&str>,
) -> AppResult<Report> {
    let project_engine = Engine::new(None)?.with_budget(10800);
    let mut results = Vec::new();
    let mut warnings = Vec::new();
    let mut attempted = false;
    let mut failed = false;
    let mut blocked = false;
    for (id, enrollment) in enrollments {
        if process::interrupted() {
            return Err(Error::failed("Docker workload maintenance was interrupted"));
        }
        if target.is_some_and(|target| target != id) {
            continue;
        }
        let mut state = load(ctx, &id)?;
        let containers = project_engine.containers(Some(&id), true, 25);
        let (containers, discovery_error) = match containers {
            Ok(containers) => (containers, None),
            Err(error) => (vec![], Some(error.message)),
        };
        let policy = eligibility(
            ctx,
            &id,
            Some(&enrollment),
            &containers,
            state.recovery.is_some(),
            discovery_error.as_deref(),
        );
        let mut result = if let Some(reason) = policy.reason {
            blocked |= target.is_some();
            StackResult::new(&id, policy.code, reason)
        } else {
            let budget = if ctx.dry_run { 300 } else { 3600 };
            match Engine::new(enrollment.registry(ctx)).and_then(|engine| {
                attempt(
                    ctx,
                    &engine.with_budget(budget),
                    &enrollment,
                    &containers,
                    &mut state,
                )
            }) {
                Ok(result) => {
                    attempted |= matches!(
                        result.outcome.as_str(),
                        "current" | "updated" | "rolled_back" | "would_change"
                    );
                    result
                }
                Err(error) => {
                    let outcome = match error.outcome {
                        Outcome::Blocked if ctx.dry_run => "unknown",
                        Outcome::Blocked => "blocked",
                        _ => "failed",
                    };
                    blocked |= target.is_some() && error.outcome == Outcome::Blocked;
                    StackResult::new(&id, outcome, error.message)
                }
            }
        };
        if matches!(result.outcome.as_str(), "failed" | "rollback_failed")
            || policy.code == "docker_unavailable"
        {
            failed = true;
        }
        if matches!(
            result.outcome.as_str(),
            "failed"
                | "rolled_back"
                | "rollback_failed"
                | "suppressed"
                | "blocked"
                | "recovery_pending"
                | "unknown"
                | "untrusted_inputs"
                | "identity_mismatch"
        ) {
            result.warnings.push(result.reason.clone());
        }
        result.warnings.extend(enrollment.input_warning());
        warnings.extend(
            result
                .warnings
                .iter()
                .map(|warning| format!("{id}: {warning}")),
        );
        if !ctx.dry_run {
            state.last_result = Some(result.clone());
            save(ctx, &id, &state)?;
        }
        results.push(result);
    }
    let prune = if target.is_none() && attempted && !failed {
        match cleanup(ctx, &project_engine.with_budget(300)) {
            Ok(prune) => prune,
            Err(error) => {
                warnings.push(error.message);
                vec![]
            }
        }
    } else {
        vec![]
    };
    let changed = results.iter().any(|result| {
        matches!(
            result.outcome.as_str(),
            "updated" | "rolled_back" | "would_change"
        )
    });
    let outcome = if failed {
        Outcome::Failed
    } else if blocked {
        Outcome::Blocked
    } else if changed {
        Outcome::Completed
    } else {
        Outcome::Unchanged
    };
    let summary = if let [stack] = results.as_slice() {
        format!("{} · {} · {}", stack.id, stack.outcome, stack.reason)
    } else {
        format!("Reviewed {} enrolled Docker stacks", results.len())
    };
    let mut report = Report::new(outcome, summary)
        .detail(serde_json::json!({"stacks": results, "cleanup": prune}));
    report.warnings = warnings;
    Ok(report)
}

fn saved_states(ctx: &Context) -> AppResult<BTreeMap<String, State>> {
    let directory = ctx.paths.state_dir.join("docker");
    if !input::exists(&directory)? {
        return Ok(BTreeMap::new());
    }
    let mut records = BTreeMap::new();
    for file in fs::read_dir(directory)? {
        let file = file?.path();
        if file
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            let id = file
                .file_stem()
                .and_then(|id| id.to_str())
                .filter(|id| input::project_name(id))
                .ok_or_else(|| {
                    Error::invalid("Docker recovery record has an invalid stack identifier")
                })?;
            records.insert(id.to_owned(), load(ctx, id)?);
        }
    }
    Ok(records)
}

fn cleanup(ctx: &Context, engine: &Engine) -> AppResult<Vec<String>> {
    let config = &ctx.config.compose;
    if ctx.dry_run {
        return Ok(if config.prune_dangling || config.prune_tagged_days > 0 {
            vec!["Would review unused daemon images and exempt every rollback hold".to_owned()]
        } else {
            vec![]
        });
    }
    let mut states = saved_states(ctx)?;
    let now = Utc::now();
    let mut protected = BTreeSet::new();
    for state in states.values() {
        protected.extend(
            state
                .holds
                .iter()
                .filter(|hold| hold.expires_at > now || state.recovery.is_some())
                .map(|hold| hold.image.clone()),
        );
        if let Some(recovery) = &state.recovery {
            protected.extend(recovery.changes.iter().map(|change| change.old_id.clone()));
        }
    }
    let mut released = BTreeSet::new();
    for (id, state) in &mut states {
        let before = state.holds.len();
        state.holds.retain(|hold| {
            if hold.expires_at > now || protected.contains(&hold.image) {
                return true;
            }
            if released.contains(&hold.image) {
                return false;
            }
            let Ok(image) = engine.image(&hold.image) else {
                return true;
            };
            if image
                .repo_tags
                .as_ref()
                .is_some_and(|tags| tags.contains(&hold.tag))
                && engine
                    .run(
                        &["image", "rm", "--no-prune", "--", &hold.tag],
                        60,
                        "Cannot release the expired Docker rollback tag",
                    )
                    .is_err()
            {
                return true;
            }
            released.insert(hold.image.clone());
            false
        });
        if state.holds.len() != before {
            save(ctx, id, state)?;
        }
        protected.extend(state.holds.iter().map(|hold| hold.image.clone()));
    }
    if !config.prune_dangling && config.prune_tagged_days == 0 {
        return Ok(vec![]);
    }
    let mut removed = 0;
    let mut retained = 0;
    let oldest = now - TimeDelta::days(config.prune_tagged_days as i64);
    for id in engine.image_ids()? {
        if protected.contains(&id) {
            continue;
        }
        let Ok(image) = engine.image(&id) else {
            continue;
        };
        let tags = image.repo_tags.unwrap_or_default();
        let tags: Vec<_> = tags
            .iter()
            .filter(|tag| tag.as_str() != "<none>:<none>")
            .collect();
        if tags.iter().any(|tag| tag.starts_with("foreman-hold/")) {
            continue;
        }
        let dangling = tags.is_empty() && image.repo_digests.as_ref().is_none_or(Vec::is_empty);
        let aged = tags.len() == 1
            && config.prune_tagged_days > 0
            && image.created.is_some_and(|created| created < oldest);
        if ((dangling && config.prune_dangling) || aged) && !protected.contains(&image.id) {
            // Non-forced removal lets Docker reject images referenced by even a stopped container.
            if engine
                .run(
                    &["image", "rm", "--no-prune", "--", &id],
                    60,
                    "Docker retained the candidate image",
                )
                .is_ok()
            {
                removed += 1;
            } else {
                retained += 1;
            }
        }
    }
    Ok(vec![format!(
        "Daemon image cleanup removed {removed} images and retained {retained} in-use or unavailable candidates"
    )])
}

pub(super) fn ensure_no_recovery(ctx: &Context<'_>) -> AppResult<()> {
    if let Some((stack, _)) = saved_states(ctx)?
        .into_iter()
        .find(|(_, state)| state.recovery.is_some())
    {
        return Err(Error::blocked(format!(
            "Resolve Docker rollback recovery for {stack} before replacing Foreman"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_preserve_registry_ports_and_normalize_default_tags() {
        assert_eq!(
            repository("local:5000/team/server:edge"),
            "local:5000/team/server"
        );
        assert_eq!(
            repository("local:5000/team/server"),
            "local:5000/team/server"
        );
        assert_eq!(
            canonical_ref("local:5000/team/server"),
            "local:5000/team/server:latest"
        );
        assert_eq!(
            canonical_ref("nginx"),
            canonical_ref("index.docker.io/library/nginx:latest")
        );
        assert_eq!(canonical_ref("team/server"), "docker.io/team/server:latest");
    }
}
