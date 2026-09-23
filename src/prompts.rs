//! Local, hash-verified prompt artifacts.
//!
//! Prompt text is data, never an instruction to AgentMaster.  The catalog
//! deliberately has no executable format: it can list, search, show, render
//! `{{variables}}`, and explicitly send text through the already checked fleet
//! transport, but it never evaluates a shell command or follows embedded text.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

const SCHEMA_VERSION: u32 = 1;
const LINK_REGISTRY_SCHEMA_VERSION: u32 = 1;
const CATALOG_FILE: &str = "prompt-catalog.json";
const LINK_REGISTRY: &str = include_str!("../docs/prompt-source-registry.v1.json");
const MAX_DISCOVERED_ENTRIES: usize = 512;
const MAX_ARTIFACT_BYTES: u64 = 128 * 1024;
const MAX_VARIABLE_BYTES: usize = 8 * 1024;
const SKIPPED_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    "venv",
    ".venv",
    "site-packages",
    "__pycache__",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactRole {
    Catalog,
    Corpus,
    Template,
    Document,
    Duplicate,
}

impl ArtifactRole {
    fn is_duplicate(&self) -> bool {
        matches!(self, Self::Duplicate)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptArtifact {
    pub id: String,
    pub role: ArtifactRole,
    pub title: String,
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicate_of: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptCatalog {
    pub schema_version: u32,
    pub catalog_sha256: String,
    pub entries: Vec<PromptArtifact>,
}

#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub healthy: bool,
    pub catalog_path: String,
    pub schema_version: Option<u32>,
    pub catalog_sha256: Option<String>,
    pub entries: usize,
    pub active_entries: usize,
    pub duplicates: usize,
    pub links: LinkDoctorReport,
    pub errors: Vec<String>,
}

#[derive(Debug, Serialize)]
struct Rendered<'a> {
    id: &'a str,
    sha256: &'a str,
    text: &'a str,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PromptLinkKind {
    PromptLibrary,
    SkillLibrary,
    SystemPromptReference,
    PromptTool,
    SecurityResearch,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptLink {
    id: String,
    kind: PromptLinkKind,
    title: String,
    url: String,
    summary: String,
}

#[derive(Debug, Clone, Deserialize)]
struct LinkedArtifact {
    id: String,
    sha256: String,
}

#[derive(Debug, Clone, Deserialize)]
struct PromptLinkRegistry {
    schema_version: u32,
    source_artifact: LinkedArtifact,
    links: Vec<PromptLink>,
}

#[derive(Debug, Serialize)]
pub struct LinkDoctorReport {
    pub healthy: bool,
    pub linked: bool,
    pub source_artifact_id: String,
    pub source_count: usize,
    pub errors: Vec<String>,
}

pub fn catalog_path(dir: &Path) -> PathBuf {
    dir.join(CATALOG_FILE)
}

fn hash_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn catalog_hash(entries: &[PromptArtifact]) -> String {
    let value = json!({"schema_version": SCHEMA_VERSION, "entries": entries});
    hash_bytes(&serde_json::to_vec(&value).expect("catalog value serializes"))
}

impl PromptCatalog {
    fn new(mut entries: Vec<PromptArtifact>) -> Self {
        entries.sort_by(|a, b| a.id.cmp(&b.id).then(a.path.cmp(&b.path)));
        let catalog_sha256 = catalog_hash(&entries);
        Self {
            schema_version: SCHEMA_VERSION,
            catalog_sha256,
            entries,
        }
    }
}

fn allowed_extension(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|value| value.to_str()),
        Some("md" | "markdown" | "txt" | "yaml" | "yml" | "json")
    )
}

fn prompt_named(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    name.contains("prompt") || name.contains("template") || name.contains("instruction")
}

fn slug(value: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            dash = false;
        } else if !out.is_empty() && !dash {
            out.push('-');
            dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

fn title(text: &str, fallback: &str) -> String {
    let value = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.trim_start_matches('#').trim())
        .filter(|line| !line.is_empty())
        .unwrap_or(fallback);
    let mut out = value.chars().take(160).collect::<String>();
    if value.chars().count() > out.chars().count() {
        out.push('…');
    }
    out
}

fn role_for(path: &Path) -> ArtifactRole {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if name.contains("template") {
        ArtifactRole::Template
    } else if name.contains("corpus") {
        ArtifactRole::Corpus
    } else if name.contains("librar") || name.contains("catalog") {
        ArtifactRole::Catalog
    } else {
        ArtifactRole::Document
    }
}

fn base_id(id: &str) -> String {
    ["-plain", "-copy", "-duplicate"]
        .iter()
        .find_map(|suffix| id.strip_suffix(suffix))
        .unwrap_or(id)
        .to_string()
}

fn is_plain_variant(id: &str) -> bool {
    id.ends_with("-plain") || id.ends_with("-copy") || id.ends_with("-duplicate")
}

fn parse_day(raw: Option<&str>) -> Result<Option<NaiveDate>> {
    raw.map(|value| {
        NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .with_context(|| format!("invalid --modified-on date '{value}', use YYYY-MM-DD"))
    })
    .transpose()
}

fn modified_on(path: &Path, day: NaiveDate) -> Result<bool> {
    let modified = fs::metadata(path)
        .with_context(|| format!("metadata for {}", path.display()))?
        .modified()
        .with_context(|| format!("mtime for {}", path.display()))?;
    let local: DateTime<Local> = modified.into();
    Ok(local.date_naive() == day)
}

fn walk(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut children = fs::read_dir(root)
        .with_context(|| format!("read prompt root {}", root.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    children.sort_by_key(|entry| entry.file_name());
    for entry in children {
        if out.len() >= MAX_DISCOVERED_ENTRIES {
            bail!(
                "prompt discovery stopped at {MAX_DISCOVERED_ENTRIES} files; use a narrower --root"
            );
        }
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            let name = entry.file_name();
            if SKIPPED_DIRS.iter().any(|skip| name == *skip) {
                continue;
            }
            walk(&path, out)?;
        } else if file_type.is_file() && allowed_extension(&path) && prompt_named(&path) {
            out.push(path);
        }
    }
    Ok(())
}

fn artifact_from(path: &Path, day: Option<NaiveDate>) -> Result<Option<PromptArtifact>> {
    if let Some(day) = day
        && !modified_on(path, day)?
    {
        return Ok(None);
    }
    let metadata = fs::metadata(path)?;
    if metadata.len() > MAX_ARTIFACT_BYTES {
        return Ok(None);
    }
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(_) => return Ok(None),
    };
    let fallback = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("prompt-artifact");
    let id = slug(fallback);
    if id.is_empty() {
        return Ok(None);
    }
    Ok(Some(PromptArtifact {
        id,
        role: role_for(path),
        title: title(text, fallback),
        path: path.canonicalize()?.to_string_lossy().into_owned(),
        sha256: hash_bytes(&bytes),
        bytes: bytes.len() as u64,
        duplicate_of: None,
    }))
}

fn make_unique_ids(entries: &mut [PromptArtifact]) {
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let mut seen = BTreeMap::<String, usize>::new();
    for entry in entries {
        let count = seen.entry(entry.id.clone()).or_default();
        *count += 1;
        if *count > 1 {
            entry.id = format!("{}-{}", entry.id, count);
        }
    }
}

fn mark_duplicates(entries: &mut [PromptArtifact]) {
    entries.sort_by(|a, b| {
        base_id(&a.id)
            .cmp(&base_id(&b.id))
            .then(is_plain_variant(&a.id).cmp(&is_plain_variant(&b.id)))
            .then(a.path.cmp(&b.path))
    });
    let mut by_hash = BTreeMap::<String, String>::new();
    let mut by_base = BTreeMap::<String, String>::new();
    for entry in entries {
        let base = base_id(&entry.id);
        let duplicate_of = by_hash.get(&entry.sha256).cloned().or_else(|| {
            is_plain_variant(&entry.id)
                .then(|| by_base.get(&base).cloned())
                .flatten()
        });
        if let Some(id) = duplicate_of {
            entry.role = ArtifactRole::Duplicate;
            entry.duplicate_of = Some(id);
        } else {
            by_hash.insert(entry.sha256.clone(), entry.id.clone());
            by_base.insert(base, entry.id.clone());
        }
    }
}

fn discover_catalog(roots: &[PathBuf], modified_on: Option<&str>) -> Result<PromptCatalog> {
    if roots.is_empty() {
        bail!("at least one --root is required");
    }
    let day = parse_day(modified_on)?;
    let mut paths = Vec::new();
    for root in roots {
        let root = root
            .canonicalize()
            .with_context(|| format!("prompt root {}", root.display()))?;
        if !root.is_dir() {
            bail!("prompt root {} is not a directory", root.display());
        }
        walk(&root, &mut paths)?;
    }
    paths.sort();
    paths.dedup();
    let mut entries = paths
        .iter()
        .map(|path| artifact_from(path, day))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    make_unique_ids(&mut entries);
    mark_duplicates(&mut entries);
    Ok(PromptCatalog::new(entries))
}

fn write_catalog(catalog: &PromptCatalog, dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = catalog_path(dir);
    let temporary = dir.join(format!(".prompt-catalog-{}.tmp", Uuid::new_v4()));
    let bytes = serde_json::to_vec_pretty(catalog)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, &path)?;
    Ok(path)
}

fn load_catalog(dir: &Path) -> Result<PromptCatalog> {
    let path = catalog_path(dir);
    let bytes =
        fs::read(&path).with_context(|| format!("read prompt catalog {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parse prompt catalog {}", path.display()))
}

fn load_link_registry() -> Result<PromptLinkRegistry> {
    serde_json::from_str(LINK_REGISTRY).context("parse bundled prompt source registry")
}

fn link_report_unavailable(error: impl ToString) -> LinkDoctorReport {
    match load_link_registry() {
        Ok(registry) => LinkDoctorReport {
            healthy: false,
            linked: false,
            source_artifact_id: registry.source_artifact.id,
            source_count: registry.links.len(),
            errors: vec![error.to_string()],
        },
        Err(registry_error) => LinkDoctorReport {
            healthy: false,
            linked: false,
            source_artifact_id: "unknown".to_string(),
            source_count: 0,
            errors: vec![registry_error.to_string(), error.to_string()],
        },
    }
}

fn link_report_for_catalog(catalog: &PromptCatalog) -> LinkDoctorReport {
    let registry = match load_link_registry() {
        Ok(registry) => registry,
        Err(error) => return link_report_unavailable(error),
    };
    let mut errors = Vec::new();
    if registry.schema_version != LINK_REGISTRY_SCHEMA_VERSION {
        errors.push(format!(
            "unsupported link registry schema_version {}",
            registry.schema_version
        ));
    }
    if registry.links.is_empty() {
        errors.push("link registry has no curated prompt sources".to_string());
    }
    let mut ids = BTreeSet::new();
    let mut urls = BTreeSet::new();
    for link in &registry.links {
        if slug(&link.id) != link.id || link.id.is_empty() {
            errors.push(format!("invalid prompt source id '{}'", link.id));
        }
        if !ids.insert(link.id.as_str()) {
            errors.push(format!("duplicate prompt source id '{}'", link.id));
        }
        if !link.url.starts_with("https://") || link.url.contains(char::is_whitespace) {
            errors.push(format!("unsafe prompt source URL '{}'", link.url));
        }
        if !urls.insert(link.url.as_str()) {
            errors.push(format!("duplicate prompt source URL '{}'", link.url));
        }
        if link.title.trim().is_empty() || link.summary.trim().is_empty() {
            errors.push(format!("incomplete prompt source '{}'", link.id));
        }
    }

    let source = match catalog
        .entries
        .iter()
        .find(|entry| entry.id == registry.source_artifact.id)
    {
        Some(source) => source,
        None => {
            return LinkDoctorReport {
                healthy: errors.is_empty(),
                linked: false,
                source_artifact_id: registry.source_artifact.id,
                source_count: registry.links.len(),
                errors,
            };
        }
    };
    if source.sha256 != registry.source_artifact.sha256 {
        errors.push(format!(
            "linked source artifact {} hash differs from bundled registry",
            source.id
        ));
    }
    match read_verified(source) {
        Ok(text) => {
            for link in &registry.links {
                if !text.contains(&link.url) {
                    errors.push(format!(
                        "linked source '{}' is absent from {}",
                        link.id, source.id
                    ));
                }
            }
        }
        Err(error) => errors.push(error.to_string()),
    }
    LinkDoctorReport {
        healthy: errors.is_empty(),
        linked: true,
        source_artifact_id: registry.source_artifact.id,
        source_count: registry.links.len(),
        errors,
    }
}

pub fn links_doctor_report(dir: &Path) -> LinkDoctorReport {
    match load_catalog(dir) {
        Ok(catalog) => link_report_for_catalog(&catalog),
        Err(error) => link_report_unavailable(error),
    }
}

fn ready_link_registry(dir: &Path) -> Result<PromptLinkRegistry> {
    let report = links_doctor_report(dir);
    if !report.healthy {
        bail!("prompt link registry failed: {}", report.errors.join("; "));
    }
    if !report.linked {
        bail!(
            "prompt link registry is not linked; register {} first",
            report.source_artifact_id
        );
    }
    load_link_registry()
}

pub fn links_list(json_output: bool, dir: &Path) -> Result<()> {
    let registry = ready_link_registry(dir)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&registry.links)?);
    } else {
        for link in &registry.links {
            println!("{:<36} {:<24?} {}", link.id, link.kind, link.url);
        }
    }
    Ok(())
}

pub fn links_search(query: &str, json_output: bool, dir: &Path) -> Result<()> {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        bail!("empty prompt link search query");
    }
    let registry = ready_link_registry(dir)?;
    let matches = registry
        .links
        .iter()
        .filter(|link| {
            format!(
                "{} {:?} {} {} {}",
                link.id, link.kind, link.title, link.url, link.summary
            )
            .to_ascii_lowercase()
            .contains(&query)
        })
        .collect::<Vec<_>>();
    if json_output {
        println!("{}", serde_json::to_string_pretty(&matches)?);
    } else if matches.is_empty() {
        println!("no curated prompt links match '{query}'");
    } else {
        for link in matches {
            println!("{:<36} {:<24?} {}", link.id, link.kind, link.url);
        }
    }
    Ok(())
}

pub fn links_show(id: &str, json_output: bool, dir: &Path) -> Result<()> {
    let registry = ready_link_registry(dir)?;
    let link = registry
        .links
        .iter()
        .find(|link| link.id == id)
        .with_context(|| format!("unknown curated prompt link '{id}'"))?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(link)?);
    } else {
        println!(
            "# {} ({:?})\n{}\n\n{}",
            link.title, link.kind, link.url, link.summary
        );
    }
    Ok(())
}

pub fn links_doctor(json_output: bool, dir: &Path) -> Result<()> {
    let report = links_doctor_report(dir);
    if json_output {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if report.healthy && report.linked {
        println!(
            "prompt links: healthy ({} curated sources linked to {})",
            report.source_count, report.source_artifact_id
        );
    } else if report.healthy {
        println!(
            "prompt links: not linked (register {} first)",
            report.source_artifact_id
        );
    } else {
        println!("prompt links: unhealthy ({})", report.source_artifact_id);
        for error in &report.errors {
            println!("  - {error}");
        }
    }
    if report.healthy && report.linked {
        Ok(())
    } else {
        bail!("prompt links doctor failed")
    }
}

fn active(catalog: &PromptCatalog, include_duplicates: bool) -> Vec<&PromptArtifact> {
    catalog
        .entries
        .iter()
        .filter(|entry| include_duplicates || !entry.role.is_duplicate())
        .collect()
}

fn read_verified(entry: &PromptArtifact) -> Result<String> {
    let path = Path::new(&entry.path);
    let link_metadata = fs::symlink_metadata(path)
        .with_context(|| format!("registered artifact {} missing", entry.id))?;
    if link_metadata.file_type().is_symlink() {
        bail!("registered artifact {} is a symlink", entry.id);
    }
    let metadata =
        fs::metadata(path).with_context(|| format!("registered artifact {} missing", entry.id))?;
    if !metadata.is_file() {
        bail!("registered artifact {} is not a regular file", entry.id);
    }
    if metadata.len() > MAX_ARTIFACT_BYTES {
        bail!(
            "registered artifact {} exceeds {MAX_ARTIFACT_BYTES} bytes",
            entry.id
        );
    }
    let bytes = fs::read(path)?;
    let actual = hash_bytes(&bytes);
    if actual != entry.sha256 {
        bail!(
            "registered artifact {} changed (catalog {}, current {})",
            entry.id,
            entry.sha256,
            actual
        );
    }
    String::from_utf8(bytes)
        .with_context(|| format!("registered artifact {} is not UTF-8", entry.id))
}

fn entry<'a>(catalog: &'a PromptCatalog, id: &str) -> Result<&'a PromptArtifact> {
    let entry = catalog
        .entries
        .iter()
        .find(|entry| entry.id == id)
        .with_context(|| format!("unknown prompt artifact '{id}'"))?;
    if entry.role.is_duplicate() {
        bail!(
            "prompt artifact '{id}' is a duplicate of {}; select the canonical artifact",
            entry.duplicate_of.as_deref().unwrap_or("an unknown entry")
        );
    }
    Ok(entry)
}

fn parse_variables(raw: &[String]) -> Result<BTreeMap<String, String>> {
    let mut variables = BTreeMap::new();
    for item in raw {
        let (key, value) = item
            .split_once('=')
            .with_context(|| format!("invalid --var '{item}', use key=value"))?;
        if key.is_empty()
            || !key
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
            || value.contains('\0')
            || value.len() > MAX_VARIABLE_BYTES
        {
            bail!("unsafe or oversized --var '{key}'");
        }
        if variables
            .insert(key.to_string(), value.to_string())
            .is_some()
        {
            bail!("duplicate --var key '{key}'");
        }
    }
    Ok(variables)
}

fn unresolved_variable(text: &str) -> Option<String> {
    let start = text.find("{{")?;
    let rest = &text[start + 2..];
    let end = rest.find("}}")?;
    let key = rest[..end].trim();
    (!key.is_empty()).then(|| key.to_string())
}

fn render(entry: &PromptArtifact, variables: &BTreeMap<String, String>) -> Result<String> {
    let mut text = read_verified(entry)?;
    for (key, value) in variables {
        text = text.replace(&format!("{{{{{key}}}}}"), value);
    }
    if let Some(key) = unresolved_variable(&text) {
        bail!("missing --var for '{{{{{key}}}}}'");
    }
    Ok(text)
}

pub fn discover(
    roots: &[PathBuf],
    modified_on: Option<&str>,
    json_output: bool,
    dir: &Path,
) -> Result<()> {
    let catalog = discover_catalog(roots, modified_on)?;
    let path = write_catalog(&catalog, dir)?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"catalog_path": path, "catalog": catalog}))?
        );
    } else {
        println!(
            "registered {} prompt artifacts ({} active, hash {}) at {}",
            catalog.entries.len(),
            active(&catalog, false).len(),
            catalog.catalog_sha256,
            path.display()
        );
        for entry in &catalog.entries {
            let duplicate = entry
                .duplicate_of
                .as_deref()
                .map(|id| format!(" -> duplicate of {id}"))
                .unwrap_or_default();
            println!("  {:<34} {:?}{duplicate}", entry.id, entry.role);
        }
    }
    Ok(())
}

pub fn list(include_duplicates: bool, json_output: bool, dir: &Path) -> Result<()> {
    let catalog = load_catalog(dir)?;
    let entries = active(&catalog, include_duplicates);
    if json_output {
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else {
        for entry in entries {
            println!("{:<34} {:<10?} {}", entry.id, entry.role, entry.title);
        }
    }
    Ok(())
}

pub fn search(query: &str, include_duplicates: bool, json_output: bool, dir: &Path) -> Result<()> {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        bail!("empty prompt search query");
    }
    let catalog = load_catalog(dir)?;
    let mut matches = Vec::new();
    for entry in active(&catalog, include_duplicates) {
        let metadata =
            format!("{} {:?} {}", entry.id, entry.role, entry.title).to_ascii_lowercase();
        let text = read_verified(entry)?;
        if metadata.contains(&query) || text.to_ascii_lowercase().contains(&query) {
            matches.push(entry);
        }
    }
    if json_output {
        println!("{}", serde_json::to_string_pretty(&matches)?);
    } else if matches.is_empty() {
        println!("no registered prompt artifacts match '{query}'");
    } else {
        for entry in matches {
            println!("{:<34} {:<10?} {}", entry.id, entry.role, entry.title);
        }
    }
    Ok(())
}

pub fn show(id: &str, json_output: bool, dir: &Path) -> Result<()> {
    let catalog = load_catalog(dir)?;
    let entry = entry(&catalog, id)?;
    let text = read_verified(entry)?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"artifact": entry, "text": text}))?
        );
    } else {
        println!(
            "# {} ({:?})\n# sha256: {}\n",
            entry.id, entry.role, entry.sha256
        );
        print!("{text}");
    }
    Ok(())
}

pub fn render_command(
    id: &str,
    raw_variables: &[String],
    json_output: bool,
    dir: &Path,
) -> Result<()> {
    let catalog = load_catalog(dir)?;
    let entry = entry(&catalog, id)?;
    let variables = parse_variables(raw_variables)?;
    let text = render(entry, &variables)?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&Rendered {
                id: &entry.id,
                sha256: &entry.sha256,
                text: &text
            })?
        );
    } else {
        print!("{text}");
    }
    Ok(())
}

pub fn send(
    id: &str,
    target: &str,
    raw_variables: &[String],
    dry_run: bool,
    dir: &Path,
) -> Result<()> {
    if target.trim().is_empty() {
        bail!("an explicit live-agent target is required");
    }
    let catalog = load_catalog(dir)?;
    let entry = entry(&catalog, id)?;
    let variables = parse_variables(raw_variables)?;
    let text = render(entry, &variables)?;
    if dry_run {
        println!(
            "would send {} ({} bytes, sha256 {}) to {target}",
            entry.id,
            text.len(),
            entry.sha256
        );
        return Ok(());
    }
    crate::cli::send(target, &text, dir)?;
    println!(
        "sent {} ({} bytes, sha256 {}) to {target}",
        entry.id,
        text.len(),
        entry.sha256
    );
    Ok(())
}

pub fn doctor_report(dir: &Path) -> DoctorReport {
    let path = catalog_path(dir);
    let mut errors = Vec::new();
    let catalog = match load_catalog(dir) {
        Ok(catalog) => catalog,
        Err(error) => {
            let message = error.to_string();
            return DoctorReport {
                healthy: false,
                catalog_path: path.to_string_lossy().into_owned(),
                schema_version: None,
                catalog_sha256: None,
                entries: 0,
                active_entries: 0,
                duplicates: 0,
                links: link_report_unavailable(&message),
                errors: vec![message],
            };
        }
    };
    if catalog.schema_version != SCHEMA_VERSION {
        errors.push(format!(
            "unsupported schema_version {}",
            catalog.schema_version
        ));
    }
    if catalog.entries.is_empty() {
        errors.push("catalog has no entries".to_string());
    }
    let expected_hash = catalog_hash(&catalog.entries);
    if catalog.catalog_sha256 != expected_hash {
        errors.push("catalog_sha256 mismatch".to_string());
    }
    let ids = catalog
        .entries
        .iter()
        .map(|entry| entry.id.as_str())
        .collect::<BTreeSet<_>>();
    if ids.len() != catalog.entries.len() {
        errors.push("duplicate artifact ids".to_string());
    }
    for entry in &catalog.entries {
        if entry.path.is_empty() || !Path::new(&entry.path).is_absolute() {
            errors.push(format!("{} has a non-absolute path", entry.id));
        }
        if entry.role.is_duplicate() {
            match entry.duplicate_of.as_deref() {
                Some(id) if ids.contains(id) => {}
                _ => errors.push(format!("{} has no valid duplicate_of", entry.id)),
            }
        } else if entry.duplicate_of.is_some() {
            errors.push(format!("{} is canonical but has duplicate_of", entry.id));
        }
        if let Err(error) = read_verified(entry) {
            errors.push(error.to_string());
        }
    }
    let duplicates = catalog
        .entries
        .iter()
        .filter(|entry| entry.role.is_duplicate())
        .count();
    let active_entries = catalog.entries.len().saturating_sub(duplicates);
    let links = link_report_for_catalog(&catalog);
    if !links.healthy {
        errors.push("prompt link registry failed".to_string());
    }
    DoctorReport {
        healthy: errors.is_empty(),
        catalog_path: path.to_string_lossy().into_owned(),
        schema_version: Some(catalog.schema_version),
        catalog_sha256: Some(catalog.catalog_sha256),
        entries: catalog.entries.len(),
        active_entries,
        duplicates,
        links,
        errors,
    }
}

pub fn doctor(json_output: bool, dir: &Path) -> Result<()> {
    let report = doctor_report(dir);
    if json_output {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if report.healthy {
        println!(
            "prompt catalog: healthy ({} active, {} duplicates, {})",
            report.active_entries, report.duplicates, report.catalog_path
        );
        if report.links.linked {
            println!(
                "prompt links: healthy ({} curated sources)",
                report.links.source_count
            );
        }
    } else {
        println!("prompt catalog: unhealthy ({})", report.catalog_path);
        for error in &report.errors {
            println!("  - {error}");
        }
    }
    if report.healthy {
        Ok(())
    } else {
        bail!("prompt catalog doctor failed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("agentmaster-prompts-{label}-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn discovers_hashes_and_marks_plain_variant_as_duplicate() {
        let root = temp_dir("discover");
        write(&root, "prompt-libraries-top50.md", "# Catalog\nalpha");
        write(&root, "prompt-library-corpus-en.txt", "# Corpus\nbeta");
        write(
            &root,
            "prompt-library-corpus-en-plain.txt",
            "# Corpus plain\nbeta",
        );
        let catalog = discover_catalog(std::slice::from_ref(&root), None).unwrap();
        assert_eq!(catalog.entries.len(), 3);
        assert_eq!(catalog.entries[0].role, ArtifactRole::Catalog);
        let corpus = catalog
            .entries
            .iter()
            .find(|entry| entry.id == "prompt-library-corpus-en")
            .unwrap();
        let plain = catalog
            .entries
            .iter()
            .find(|entry| entry.id == "prompt-library-corpus-en-plain")
            .unwrap();
        assert_eq!(corpus.role, ArtifactRole::Corpus);
        assert_eq!(plain.role, ArtifactRole::Duplicate);
        assert_eq!(
            plain.duplicate_of.as_deref(),
            Some("prompt-library-corpus-en")
        );
        assert_eq!(catalog.catalog_sha256, catalog_hash(&catalog.entries));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn render_requires_all_variables_and_never_executes_text() {
        let root = temp_dir("render");
        write(
            &root,
            "safe-template.md",
            "# Safe\nhello {{name}}; $(never-run)",
        );
        let catalog = discover_catalog(std::slice::from_ref(&root), None).unwrap();
        let template = entry(&catalog, "safe-template").unwrap();
        let variables = parse_variables(&["name=world".to_string()]).unwrap();
        assert_eq!(
            render(template, &variables).unwrap(),
            "# Safe\nhello world; $(never-run)"
        );
        assert!(
            render(template, &BTreeMap::new())
                .unwrap_err()
                .to_string()
                .contains("missing --var")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn doctor_detects_tampered_registered_artifact() {
        let root = temp_dir("doctor-root");
        let data = temp_dir("doctor-data");
        let file = write(&root, "safe-template.md", "# Safe\nhello");
        let catalog = discover_catalog(std::slice::from_ref(&root), None).unwrap();
        write_catalog(&catalog, &data).unwrap();
        assert!(doctor_report(&data).healthy);
        fs::write(file, "# Safe\nchanged").unwrap();
        let report = doctor_report(&data);
        assert!(!report.healthy);
        assert!(report.errors.iter().any(|error| error.contains("changed")));
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(data);
    }

    #[cfg(unix)]
    #[test]
    fn doctor_rejects_symlink_replacement_after_registration() {
        let root = temp_dir("symlink-root");
        let data = temp_dir("symlink-data");
        let file = write(&root, "safe-template.md", "# Safe\nhello");
        let replacement = write(&root, "replacement.md", "# Safe\nhello");
        let catalog = discover_catalog(std::slice::from_ref(&root), None).unwrap();
        write_catalog(&catalog, &data).unwrap();
        fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink(&replacement, &file).unwrap();
        let report = doctor_report(&data);
        assert!(!report.healthy);
        assert!(report.errors.iter().any(|error| error.contains("symlink")));
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(data);
    }

    #[test]
    fn explicit_target_is_required_before_dispatch() {
        let error = send("does-not-matter", "   ", &[], true, Path::new("/tmp")).unwrap_err();
        assert!(error.to_string().contains("explicit live-agent target"));
    }

    #[test]
    fn curated_link_registry_has_unique_https_sources() {
        let registry = load_link_registry().unwrap();
        assert_eq!(registry.schema_version, LINK_REGISTRY_SCHEMA_VERSION);
        assert_eq!(
            registry.source_artifact.id,
            "prompt-libraries-top50-2026-07-31"
        );
        assert_eq!(registry.links.len(), 45);
        let ids = registry
            .links
            .iter()
            .map(|link| link.id.as_str())
            .collect::<BTreeSet<_>>();
        let urls = registry
            .links
            .iter()
            .map(|link| link.url.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(ids.len(), registry.links.len());
        assert_eq!(urls.len(), registry.links.len());
        assert!(
            registry
                .links
                .iter()
                .all(|link| link.url.starts_with("https://"))
        );
    }

    #[test]
    fn curated_links_fail_closed_when_the_source_report_hash_changes() {
        let root = temp_dir("links-root");
        write(
            &root,
            "prompt-libraries-top50-2026-07-31.md",
            "# Changed\nhttps://github.com/promptfoo/promptfoo",
        );
        let catalog = discover_catalog(std::slice::from_ref(&root), None).unwrap();
        let report = link_report_for_catalog(&catalog);
        assert!(report.linked);
        assert!(!report.healthy);
        assert!(
            report
                .errors
                .iter()
                .any(|error| error.contains("hash differs"))
        );
        let _ = fs::remove_dir_all(root);
    }
}
