//! Gather once, reference N times.
//!
//! A fleet that needs the same fresh fact pays for it once per worker: every
//! lane searches, fetches and re-reads the same page into its own context. This
//! module does the lookup a single time, on the controller side, and leaves a
//! private artifact on disk. Workers then get a path, not a payload — the raw
//! bytes never enter an agent transcript, and no model token is spent gathering.
//!
//! Sources are detected, never installed, and each one is bounded by a wall
//! deadline with its stdout streamed straight to its artifact file so nothing
//! large is ever held in memory here.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Hard ceiling for any single artifact. A source that produces more is
/// truncated on a UTF-8 boundary and says so in the manifest.
const MAX_ARTIFACT_BYTES: u64 = 512 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(50);

pub struct ResearchRequest<'a> {
    pub name: &'a str,
    pub query: &'a str,
    pub mode: &'a str,
    pub bytes: u32,
    pub code: bool,
    pub trends: bool,
    /// Ask the provider for official primary sources before the general web.
    pub primary: bool,
    pub deadline_secs: u64,
    /// How long a completed gather stays reusable. The whole point of research
    /// is to pay once; re-fetching the same query an hour later throws that
    /// away.
    pub ttl_secs: u64,
    pub refresh: bool,
    pub dry_run: bool,
}

#[derive(Serialize, Deserialize)]
struct SourceRecord {
    source: String,
    program: String,
    status: String,
    exit_code: Option<i32>,
    elapsed_ms: u64,
    artifact: Option<String>,
    artifact_bytes: u64,
    artifact_sha256: Option<String>,
    truncated: bool,
    model_tokens_spent: u64,
    detail: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct ResearchManifest {
    schema: String,
    schema_version: u8,
    name: String,
    query_sha256: String,
    query_bytes: usize,
    mode: String,
    requested_bytes: u32,
    deadline_secs: u64,
    /// Identifies the inputs that decide what the artifacts contain. Two runs
    /// with the same key would fetch the same thing, so the second one does
    /// not run at all.
    cache_key: String,
    created_ms: u128,
    sources: Vec<SourceRecord>,
    model_tokens_spent: u64,
    capsule_line: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct CachePointer {
    schema: String,
    schema_version: u8,
    cache_key: String,
    run_dir: String,
    created_ms: u128,
}

pub fn run(request: ResearchRequest<'_>, data_dir: &Path) -> Result<()> {
    validate(&request)?;
    let query_hash = sha256_hex(request.query.as_bytes());

    let cache_key = cache_key(&request);
    // Cache first: a completed gather is the artifact, so an identical request
    // inside the TTL must not touch the network at all.
    let cached = if request.refresh {
        None
    } else {
        lookup_cached(data_dir, &cache_key, request.ttl_secs)
    };

    let mut planned: Vec<(&'static str, String)> = Vec::new();
    let adapter = find_on_path("llmadapter");
    planned.push((
        "web",
        adapter
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "llmadapter (missing)".to_string()),
    ));
    if request.primary {
        planned.push((
            "primary",
            adapter
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "llmadapter (missing)".to_string()),
        ));
    }
    if request.code {
        planned.push((
            "code",
            find_on_path("ghmax")
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "ghmax (missing)".to_string()),
        ));
    }
    if request.trends {
        planned.push((
            "trends",
            find_on_path("superweb")
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "superweb (missing)".to_string()),
        ));
    }

    if request.dry_run {
        println!("research dry-run");
        println!("  name        : {}", request.name);
        println!("  query sha256: {query_hash}");
        println!("  query bytes : {}", request.query.len());
        println!("  mode        : {}", request.mode);
        println!("  bytes       : {}", request.bytes);
        println!("  deadline    : {}s per source", request.deadline_secs);
        println!("  cache key   : {cache_key}");
        println!(
            "  cache       : {}",
            match cached.as_ref() {
                _ if request.refresh => "bypassed (--refresh)".to_string(),
                Some(hit) => format!("hit, {}s old — no source would run", hit.age_secs),
                None => "miss — every source would run".to_string(),
            }
        );
        for (source, program) in &planned {
            println!("  source      : {source} -> {program}");
        }
        println!("  model tokens: 0 (gathering never calls a model)");
        return Ok(());
    }

    if let Some(hit) = cached {
        report(&hit.manifest, &hit.run_dir, Some(hit.age_secs));
        return Ok(());
    }

    let adapter =
        adapter.context("llmadapter not found on PATH; research needs it for the web source")?;
    let run_dir = create_run_dir(data_dir, &slug(request.name))?;
    let mut sources = Vec::new();

    if request.primary {
        sources.push(run_primary(&adapter, &request, &run_dir)?);
    }
    sources.push(run_web(&adapter, &request, &run_dir)?);
    if request.code {
        sources.push(run_code(&request, &run_dir)?);
    }
    if request.trends {
        sources.push(run_trends(&request, &run_dir)?);
    }

    let capsule_line = sources
        .iter()
        .filter(|record| record.status == "ok")
        .filter_map(|record| record.artifact.clone())
        .reduce(|a, b| format!("{a}`, `{b}"))
        .map(|paths| {
            format!(
                "Evidence: read `{paths}` before any fresh-fact claim. Do not re-fetch; cite the file.",
            )
        });

    let manifest = ResearchManifest {
        schema: "agentmaster.research".to_string(),
        schema_version: 1,
        name: request.name.to_string(),
        query_sha256: query_hash.clone(),
        query_bytes: request.query.len(),
        mode: request.mode.to_string(),
        requested_bytes: request.bytes,
        deadline_secs: request.deadline_secs,
        cache_key: cache_key.clone(),
        created_ms: now_ms(),
        model_tokens_spent: sources.iter().map(|record| record.model_tokens_spent).sum(),
        capsule_line: capsule_line.clone(),
        sources,
    };
    let manifest_path = run_dir.join("index.json");
    write_private_json(&manifest_path, &manifest)?;
    // Only a run with at least one usable artifact is worth pointing at.
    if manifest.sources.iter().any(|record| record.status == "ok") {
        store_pointer(data_dir, &cache_key, &run_dir, manifest.created_ms)?;
    }
    report(&manifest, &run_dir, None);
    Ok(())
}

/// Newest usable gather for a research name, as (run dir, capsule line).
/// Used by the swarm so N lanes reference one artifact instead of each looking
/// the same fact up.
pub fn latest_run(data_dir: &Path, name: &str) -> Option<(PathBuf, String)> {
    let dir = data_dir.join("research").join(slug(name));
    let mut runs: Vec<PathBuf> = fs::read_dir(&dir)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_dir())
        .collect();
    // Run directories are named "<millis>-<pid>", so lexical order is age order
    // for any run this millennium.
    runs.sort();
    for run in runs.into_iter().rev() {
        let Ok(bytes) = fs::read(run.join("index.json")) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_slice::<ResearchManifest>(&bytes) else {
            continue;
        };
        let Some(line) = manifest.capsule_line.clone() else {
            continue;
        };
        let artifacts_present = manifest
            .sources
            .iter()
            .filter(|record| record.status == "ok")
            .all(|record| {
                record
                    .artifact
                    .as_ref()
                    .is_some_and(|path| Path::new(path).is_file())
            });
        if artifacts_present {
            return Some((run, line));
        }
    }
    None
}

fn report(manifest: &ResearchManifest, run_dir: &Path, cache_age_secs: Option<u64>) {
    println!("research {}", manifest.name);
    println!("  run dir : {}", run_dir.display());
    match cache_age_secs {
        Some(age) => println!("  cache   : HIT ({age}s old) — no source ran, nothing fetched"),
        None => println!("  cache   : miss — sources ran once, reusable from now on"),
    }
    for record in &manifest.sources {
        match record.status.as_str() {
            "ok" => println!(
                "  {:<7}: ok {} bytes{} -> {}",
                record.source,
                record.artifact_bytes,
                if record.truncated { " (truncated)" } else { "" },
                record.artifact.as_deref().unwrap_or("-")
            ),
            other => println!(
                "  {:<7}: {other}{}",
                record.source,
                record
                    .detail
                    .as_deref()
                    .map(|detail| format!(" ({detail})"))
                    .unwrap_or_default()
            ),
        }
    }
    println!(
        "  tokens  : {} model tokens spent",
        manifest.model_tokens_spent
    );
    println!("  manifest: {}", run_dir.join("index.json").display());
    if let Some(line) = &manifest.capsule_line {
        println!("  capsule : {line}");
    }
}

/// Everything that decides what the artifacts contain. The name is deliberately
/// absent: the same query gathered under two names is the same fetch.
fn cache_key(request: &ResearchRequest<'_>) -> String {
    sha256_hex(
        format!(
            "v1|{}|{}|{}|{}|{}|{}",
            request.query.trim(),
            request.mode,
            request.bytes,
            request.primary,
            request.code,
            request.trends,
        )
        .as_bytes(),
    )
}

struct CacheHit {
    manifest: ResearchManifest,
    run_dir: PathBuf,
    age_secs: u64,
}

/// A pointer is a hint, never an authority: the run directory has to sit under
/// this data dir, still exist, and carry a manifest whose key matches.
fn lookup_cached(data_dir: &Path, key: &str, ttl_secs: u64) -> Option<CacheHit> {
    let pointer_path = pointer_path(data_dir, key);
    let pointer: CachePointer = serde_json::from_slice(&fs::read(&pointer_path).ok()?).ok()?;
    if pointer.cache_key != key {
        return None;
    }
    let run_dir = PathBuf::from(&pointer.run_dir);
    let root = data_dir.join("research");
    if !run_dir.starts_with(&root) || !run_dir.is_dir() {
        return None;
    }
    let manifest: ResearchManifest =
        serde_json::from_slice(&fs::read(run_dir.join("index.json")).ok()?).ok()?;
    if manifest.cache_key != key {
        return None;
    }
    let age_ms = now_ms().saturating_sub(manifest.created_ms);
    let age_secs = (age_ms / 1000) as u64;
    if age_secs > ttl_secs {
        return None;
    }
    // A manifest that points at artifacts somebody deleted is not a hit.
    if !manifest
        .sources
        .iter()
        .filter(|record| record.status == "ok")
        .all(|record| {
            record
                .artifact
                .as_ref()
                .is_some_and(|path| Path::new(path).is_file())
        })
    {
        return None;
    }
    Some(CacheHit {
        manifest,
        run_dir,
        age_secs,
    })
}

fn store_pointer(data_dir: &Path, key: &str, run_dir: &Path, created_ms: u128) -> Result<()> {
    write_private_json(
        &pointer_path(data_dir, key),
        &CachePointer {
            schema: "agentmaster.research.pointer".to_string(),
            schema_version: 1,
            cache_key: key.to_string(),
            run_dir: run_dir.display().to_string(),
            created_ms,
        },
    )
}

fn pointer_path(data_dir: &Path, key: &str) -> PathBuf {
    data_dir
        .join("research")
        .join(".index")
        .join(format!("{key}.json"))
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or_default()
}

fn validate(request: &ResearchRequest<'_>) -> Result<()> {
    if request.name.trim().is_empty() || request.query.trim().is_empty() {
        bail!("research name and query must not be empty");
    }
    if !matches!(request.mode, "research" | "mega" | "fetch" | "primary") {
        bail!("--mode must be research, mega, fetch or primary");
    }
    if request.ttl_secs == 0 && !request.refresh {
        bail!("--ttl-secs must be greater than zero; pass --refresh to bypass the cache");
    }
    if !(1..=4000).contains(&request.bytes) {
        bail!("--bytes must be between 1 and 4000");
    }
    if request.deadline_secs == 0 {
        bail!("--deadline-secs must be greater than zero");
    }
    // Sources take the query as an argument, so a leading `--` would turn it
    // into a flag for a program we do not control.
    if request.query.trim_start().starts_with("--") {
        bail!("query must not start with `--`");
    }
    Ok(())
}

/// Official sources before general web results. A version, price or policy
/// claim should cite whoever owns the fact, not the best-ranked blog about it.
fn run_primary(
    adapter: &Path,
    request: &ResearchRequest<'_>,
    run_dir: &Path,
) -> Result<SourceRecord> {
    run_adapter_evidence(adapter, request, run_dir, "primary")
}

/// The web source is `llmadapter evidence`: it caches, projects and reports,
/// and it runs no lane, so it costs zero model tokens.
fn run_web(adapter: &Path, request: &ResearchRequest<'_>, run_dir: &Path) -> Result<SourceRecord> {
    run_adapter_evidence(adapter, request, run_dir, request.mode)
}

fn run_adapter_evidence(
    adapter: &Path,
    request: &ResearchRequest<'_>,
    run_dir: &Path,
    mode: &str,
) -> Result<SourceRecord> {
    let label = if mode == "primary" { "primary" } else { "web" };
    let artifact = run_dir.join(format!("{label}.txt"));
    let report_path = run_dir.join(format!("{label}-report.json"));
    let started = Instant::now();
    let mut command = Command::new(adapter);
    command
        .arg("evidence")
        .arg("--mode")
        .arg(mode)
        .arg("--target")
        .arg(request.query)
        .arg("--bytes")
        .arg(request.bytes.to_string())
        .arg("--out")
        .arg(&artifact);
    if request.refresh {
        // A forced refresh must reach the provider, not the adapter's cache.
        command.arg("--no-cache");
    }
    let outcome = run_bounded(
        command,
        &report_path,
        Duration::from_secs(request.deadline_secs),
    )?;
    let (bytes, sha, truncated) = artifact_facts(&artifact)?;
    let ok = outcome.exit_code == Some(0) && bytes > 0;
    Ok(SourceRecord {
        source: label.to_string(),
        program: adapter.display().to_string(),
        status: if ok {
            "ok"
        } else if outcome.timed_out {
            "timeout"
        } else {
            "failed"
        }
        .to_string(),
        exit_code: outcome.exit_code,
        elapsed_ms: started.elapsed().as_millis() as u64,
        artifact: ok.then(|| artifact.display().to_string()),
        artifact_bytes: bytes,
        artifact_sha256: sha,
        truncated,
        model_tokens_spent: 0,
        detail: (!ok).then(|| format!("report at {}", report_path.display())),
    })
}

fn run_code(request: &ResearchRequest<'_>, run_dir: &Path) -> Result<SourceRecord> {
    let Some(program) = find_on_path("ghmax") else {
        return Ok(missing_source("code", "ghmax"));
    };
    let artifact = run_dir.join("code.md");
    let started = Instant::now();
    let mut command = Command::new(&program);
    command
        .arg(request.query)
        .arg("--format")
        .arg("compact")
        .arg("-n")
        .arg("10");
    let outcome = run_bounded(
        command,
        &artifact,
        Duration::from_secs(request.deadline_secs),
    )?;
    let (bytes, sha, truncated) = artifact_facts(&artifact)?;
    let ok = outcome.exit_code == Some(0) && bytes > 0;
    Ok(SourceRecord {
        source: "code".to_string(),
        program: program.display().to_string(),
        status: if ok {
            "ok"
        } else if outcome.timed_out {
            "timeout"
        } else {
            "failed"
        }
        .to_string(),
        exit_code: outcome.exit_code,
        elapsed_ms: started.elapsed().as_millis() as u64,
        artifact: ok.then(|| artifact.display().to_string()),
        artifact_bytes: bytes,
        artifact_sha256: sha,
        truncated,
        model_tokens_spent: 0,
        detail: None,
    })
}

fn run_trends(request: &ResearchRequest<'_>, run_dir: &Path) -> Result<SourceRecord> {
    let Some(program) = find_on_path("superweb") else {
        return Ok(missing_source("trends", "superweb"));
    };
    let artifact = run_dir.join("trends.md");
    let started = Instant::now();
    let mut command = Command::new(&program);
    command
        .arg("gitstars")
        .arg("--window")
        .arg("7d")
        .arg("--sort")
        .arg("trending")
        .arg("--limit")
        .arg("20")
        .arg("--gems");
    let outcome = run_bounded(
        command,
        &artifact,
        Duration::from_secs(request.deadline_secs),
    )?;
    let (bytes, sha, truncated) = artifact_facts(&artifact)?;
    let ok = outcome.exit_code == Some(0) && bytes > 0;
    Ok(SourceRecord {
        source: "trends".to_string(),
        program: program.display().to_string(),
        status: if ok {
            "ok"
        } else if outcome.timed_out {
            "timeout"
        } else {
            "failed"
        }
        .to_string(),
        exit_code: outcome.exit_code,
        elapsed_ms: started.elapsed().as_millis() as u64,
        artifact: ok.then(|| artifact.display().to_string()),
        artifact_bytes: bytes,
        artifact_sha256: sha,
        truncated,
        model_tokens_spent: 0,
        detail: None,
    })
}

fn missing_source(source: &str, program: &str) -> SourceRecord {
    SourceRecord {
        source: source.to_string(),
        program: program.to_string(),
        status: "skipped".to_string(),
        exit_code: None,
        elapsed_ms: 0,
        artifact: None,
        artifact_bytes: 0,
        artifact_sha256: None,
        truncated: false,
        model_tokens_spent: 0,
        detail: Some(format!("{program} not on PATH")),
    }
}

struct Outcome {
    exit_code: Option<i32>,
    timed_out: bool,
}

/// stdout goes straight to the artifact file, so a large page never passes
/// through this process. stderr is dropped: a source explains itself by its
/// exit code and its artifact, not by noise.
fn run_bounded(mut command: Command, artifact: &Path, deadline: Duration) -> Result<Outcome> {
    let file = create_private_file(artifact)?;
    command.stdout(Stdio::from(file));
    command.stderr(Stdio::null());
    command.stdin(Stdio::null());
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn {:?}", command.get_program()))?;
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Outcome {
                exit_code: status.code(),
                timed_out: false,
            });
        }
        if started.elapsed() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(Outcome {
                exit_code: None,
                timed_out: true,
            });
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn artifact_facts(path: &Path) -> Result<(u64, Option<String>, bool)> {
    let Ok(metadata) = fs::metadata(path) else {
        return Ok((0, None, false));
    };
    let mut truncated = false;
    if metadata.len() > MAX_ARTIFACT_BYTES {
        let bytes = fs::read(path)?;
        let mut end = MAX_ARTIFACT_BYTES as usize;
        while end > 0 && (bytes[end] & 0xC0) == 0x80 {
            end -= 1;
        }
        fs::write(path, &bytes[..end])?;
        truncated = true;
    }
    let bytes = fs::read(path)?;
    if bytes.is_empty() {
        return Ok((0, None, truncated));
    }
    Ok((bytes.len() as u64, Some(sha256_hex(&bytes)), truncated))
}

fn create_run_dir(data_dir: &Path, name: &str) -> Result<PathBuf> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or_default();
    let dir = data_dir
        .join("research")
        .join(name)
        .join(format!("{stamp}-{}", std::process::id()));
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create research dir {}", dir.display()))?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

fn create_private_file(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn write_private_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut file = create_private_file(path)?;
    file.write_all(serde_json::to_string_pretty(value)?.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

fn find_on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

fn slug(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "research".to_string()
    } else {
        trimmed
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request<'a>(name: &'a str, query: &'a str) -> ResearchRequest<'a> {
        ResearchRequest {
            name,
            query,
            mode: "research",
            bytes: 1200,
            code: false,
            trends: false,
            primary: false,
            deadline_secs: 30,
            ttl_secs: 86_400,
            refresh: false,
            dry_run: false,
        }
    }

    #[test]
    fn empty_name_or_query_is_refused() {
        assert!(validate(&request("", "something")).is_err());
        assert!(validate(&request("name", "  ")).is_err());
    }

    #[test]
    fn a_query_cannot_impersonate_a_flag() {
        assert!(validate(&request("name", "--force")).is_err());
    }

    #[test]
    fn mode_and_bytes_are_bounded() {
        let mut bad = request("name", "query");
        bad.mode = "browse";
        assert!(validate(&bad).is_err());
        let mut oversized = request("name", "query");
        oversized.bytes = 9000;
        assert!(validate(&oversized).is_err());
    }

    fn write_run(data_dir: &Path, name: &str, key: &str, created_ms: u128) -> PathBuf {
        let run_dir = data_dir.join("research").join(name).join("run");
        fs::create_dir_all(&run_dir).unwrap();
        let artifact = run_dir.join("web.txt");
        fs::write(&artifact, "evidence body").unwrap();
        let manifest = ResearchManifest {
            schema: "agentmaster.research".to_string(),
            schema_version: 1,
            name: name.to_string(),
            query_sha256: sha256_hex(b"query"),
            query_bytes: 5,
            mode: "research".to_string(),
            requested_bytes: 1200,
            deadline_secs: 300,
            cache_key: key.to_string(),
            created_ms,
            sources: vec![SourceRecord {
                source: "web".to_string(),
                program: "llmadapter".to_string(),
                status: "ok".to_string(),
                exit_code: Some(0),
                elapsed_ms: 1,
                artifact: Some(artifact.display().to_string()),
                artifact_bytes: 13,
                artifact_sha256: Some(sha256_hex(b"evidence body")),
                truncated: false,
                model_tokens_spent: 0,
                detail: None,
            }],
            model_tokens_spent: 0,
            capsule_line: Some("Read `web.txt`".to_string()),
        };
        write_private_json(&run_dir.join("index.json"), &manifest).unwrap();
        store_pointer(data_dir, key, &run_dir, created_ms).unwrap();
        run_dir
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("am-research-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_cache_key_ignores_the_name_but_not_the_sources() {
        let mut a = request("alpha", "same query");
        let mut b = request("beta", "same query");
        assert_eq!(cache_key(&a), cache_key(&b), "a name is not a fetch input");
        b.code = true;
        assert_ne!(cache_key(&a), cache_key(&b));
        a.code = true;
        a.bytes = 900;
        assert_ne!(cache_key(&a), cache_key(&b));
    }

    #[test]
    fn a_fresh_run_is_reused_and_an_expired_one_is_not() {
        let data_dir = scratch("ttl");
        let key = "k1";
        write_run(&data_dir, "alpha", key, now_ms());
        assert!(lookup_cached(&data_dir, key, 86_400).is_some());

        // The same run, stamped an hour ago, is past a ten-minute TTL.
        let aged = scratch("ttl-aged");
        write_run(&aged, "alpha", key, now_ms().saturating_sub(3_600_000));
        assert!(lookup_cached(&aged, key, 600).is_none());
        assert!(lookup_cached(&aged, key, 7_200).is_some());
        let _ = fs::remove_dir_all(&data_dir);
        let _ = fs::remove_dir_all(&aged);
    }

    #[test]
    fn a_hit_needs_its_artifacts_to_still_exist() {
        let data_dir = scratch("artifacts");
        let key = "k2";
        let run_dir = write_run(&data_dir, "alpha", key, now_ms());
        fs::remove_file(run_dir.join("web.txt")).unwrap();
        assert!(lookup_cached(&data_dir, key, 86_400).is_none());
        let _ = fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn a_pointer_aimed_outside_the_data_dir_is_refused() {
        let data_dir = scratch("escape");
        let key = "k3";
        write_run(&data_dir, "alpha", key, now_ms());
        write_private_json(
            &pointer_path(&data_dir, key),
            &CachePointer {
                schema: "agentmaster.research.pointer".to_string(),
                schema_version: 1,
                cache_key: key.to_string(),
                run_dir: "/etc".to_string(),
                created_ms: now_ms(),
            },
        )
        .unwrap();
        assert!(lookup_cached(&data_dir, key, 86_400).is_none());
        let _ = fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn latest_run_returns_the_newest_usable_capsule_line() {
        let data_dir = scratch("latest");
        write_run(&data_dir, "alpha", "k4", now_ms());
        let (_, line) = latest_run(&data_dir, "alpha").expect("a usable run");
        assert!(line.contains("web.txt"));
        assert!(latest_run(&data_dir, "missing").is_none());
        let _ = fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn a_zero_ttl_without_refresh_is_refused() {
        let mut req = request("alpha", "query");
        req.ttl_secs = 0;
        assert!(validate(&req).is_err());
        req.refresh = true;
        assert!(validate(&req).is_ok());
    }

    #[test]
    fn slug_never_escapes_a_directory() {
        assert_eq!(slug("../../etc"), "etc");
        assert_eq!(slug("plain name"), "plain-name");
        assert_eq!(slug("///"), "research");
    }

    #[test]
    fn a_bounded_source_writes_its_stdout_to_a_private_artifact() {
        let dir = std::env::temp_dir().join(format!("am-research-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let artifact = dir.join("out.txt");
        let mut command = Command::new("printf");
        command.arg("hello evidence");
        let outcome = run_bounded(command, &artifact, Duration::from_secs(5)).unwrap();
        assert_eq!(outcome.exit_code, Some(0));
        assert!(!outcome.timed_out);
        let (bytes, sha, truncated) = artifact_facts(&artifact).unwrap();
        assert_eq!(bytes, "hello evidence".len() as u64);
        assert!(sha.is_some());
        assert!(!truncated);
        let mode = fs::metadata(&artifact).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_source_that_never_exits_is_killed_at_the_deadline() {
        let dir = std::env::temp_dir().join(format!("am-research-slow-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut command = Command::new("sleep");
        command.arg("5");
        // Sub-second so this test does not hold a slot next to the timing-sensitive
        // swarm deadline tests.
        let outcome =
            run_bounded(command, &dir.join("slow.txt"), Duration::from_millis(200)).unwrap();
        assert!(outcome.timed_out);
        assert_eq!(outcome.exit_code, None);
        let _ = fs::remove_dir_all(&dir);
    }
}
