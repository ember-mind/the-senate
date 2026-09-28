//! The enforcement core of the image tool, with no transport in it: one
//! call in, one PNG in the worktree plus one evidence row out, or a typed
//! error the coding agent can read and plan around.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::{Role, RunId, StageId};
use crate::store::{ImageGenerationRecord, SqliteStore};

use super::path::{OutputPathError, validate_output_path};
use super::{ImageBackendError, ImageGenerator, ImageQuality, ImageRequest, ImageSize, png};

/// Longest prompt accepted, in bytes. Well above what a hero image needs and
/// below anything that looks like a smuggled document.
pub const MAX_PROMPT_BYTES: usize = 4_000;

/// Exactly what the agent may say. Anything else in the MCP arguments is
/// rejected by the schema, and every optional field has a Senate default.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageToolCall {
    pub prompt: String,
    pub output_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transparent_background: Option<bool>,
}

/// Stable machine-readable failure classes returned to the agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageToolErrorCode {
    NotAuthorized,
    BackendNotConfigured,
    LimitReached,
    InvalidArgument,
    InvalidOutputPath,
    OutputExists,
    BackendRejected,
    BackendUnreachable,
    InvalidImage,
    WriteFailed,
    Internal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct ImageToolError {
    pub code: ImageToolErrorCode,
    pub message: String,
}

impl ImageToolError {
    pub(crate) fn new(code: ImageToolErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<OutputPathError> for ImageToolError {
    fn from(error: OutputPathError) -> Self {
        let code = match error {
            OutputPathError::AlreadyExists => ImageToolErrorCode::OutputExists,
            OutputPathError::Io(_) => ImageToolErrorCode::WriteFailed,
            _ => ImageToolErrorCode::InvalidOutputPath,
        };
        Self::new(code, error.to_string())
    }
}

impl From<ImageBackendError> for ImageToolError {
    fn from(error: ImageBackendError) -> Self {
        let code = match error {
            ImageBackendError::NotConfigured(_) => ImageToolErrorCode::BackendNotConfigured,
            ImageBackendError::Rejected(_) => ImageToolErrorCode::BackendRejected,
            ImageBackendError::Network(_) => ImageToolErrorCode::BackendUnreachable,
            ImageBackendError::InvalidResponse(_) => ImageToolErrorCode::InvalidImage,
        };
        Self::new(code, error.to_string())
    }
}

/// What the agent gets back when a generation succeeded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageToolSuccess {
    /// Worktree-relative path the PNG was written to.
    pub output_path: String,
    pub width: u32,
    pub height: u32,
    pub byte_size: u64,
    pub sha256: String,
    pub backend: String,
    pub model: String,
    /// This generation's 1-based number within the run.
    pub ordinal: u32,
    /// Generations still allowed in this run after this one.
    pub remaining: u32,
}

/// The stage on whose behalf a call is made. Set by the provider adapter
/// from the scheduler's request, never by the agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageToolScope {
    pub run_id: RunId,
    pub stage_id: StageId,
    pub attempt: u32,
    pub role: Role,
    pub worktree: PathBuf,
    /// The run's database file, where the bound is counted and evidence
    /// rows land. Taken from the store the adapter polls with.
    pub database: PathBuf,
}

/// Authorization, bound, placement, and evidence for one run.
pub struct ImageToolService {
    evidence_root: PathBuf,
    generator: Option<Arc<dyn ImageGenerator>>,
    allowed_roles: Vec<Role>,
    max_generations: u32,
    /// Serializes calls so the count-then-insert bound cannot be raced by
    /// parallel tool calls from one agent turn.
    serial: Mutex<()>,
}

impl fmt::Debug for ImageToolService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImageToolService")
            .field("evidence_root", &self.evidence_root)
            .field("backend", &self.generator.as_ref().map(|g| g.backend()))
            .field("allowed_roles", &self.allowed_roles)
            .field("max_generations", &self.max_generations)
            .finish_non_exhaustive()
    }
}

impl ImageToolService {
    /// `generator` is `None` when the run is authorized but this process has
    /// no configured backend (say, resumed without the credential): calls
    /// then fail with `BackendNotConfigured` instead of the run failing.
    #[must_use]
    pub fn new(
        evidence_root: PathBuf,
        generator: Option<Arc<dyn ImageGenerator>>,
        allowed_roles: Vec<Role>,
        max_generations: u32,
    ) -> Self {
        Self {
            evidence_root,
            generator,
            allowed_roles,
            max_generations,
            serial: Mutex::new(()),
        }
    }

    #[must_use]
    pub const fn max_generations(&self) -> u32 {
        self.max_generations
    }

    #[must_use]
    pub fn allows_role(&self, role: Role) -> bool {
        self.allowed_roles.contains(&role)
    }

    /// The backend this process would use, for prompts and evidence.
    #[must_use]
    pub fn backend(&self) -> Option<(&'static str, String)> {
        self.generator
            .as_ref()
            .map(|generator| (generator.backend(), generator.model().to_owned()))
    }

    /// Runs one call end to end. Order matters and is the point: everything
    /// that can be refused for free is refused before the vendor is called.
    ///
    /// # Errors
    /// Every failure is a typed tool error for the agent; none changes run
    /// state.
    pub fn generate(
        &self,
        scope: &ImageToolScope,
        call: &ImageToolCall,
    ) -> Result<ImageToolSuccess, ImageToolError> {
        let _serial = self.serial.lock().map_err(|_| {
            ImageToolError::new(ImageToolErrorCode::Internal, "image tool lock poisoned")
        })?;
        if !self.allows_role(scope.role) {
            return Err(ImageToolError::new(
                ImageToolErrorCode::NotAuthorized,
                format!(
                    "image generation is not authorized for the {:?} role in this run",
                    scope.role
                ),
            ));
        }
        let request = Self::request_from(call)?;
        let requested_at = now();
        let mut store = SqliteStore::open(&scope.database).map_err(|error| {
            ImageToolError::new(ImageToolErrorCode::Internal, error.to_string())
        })?;
        let used = store
            .count_image_generations(scope.run_id)
            .map_err(|error| {
                ImageToolError::new(ImageToolErrorCode::Internal, error.to_string())
            })?;
        if used >= self.max_generations {
            return Err(ImageToolError::new(
                ImageToolErrorCode::LimitReached,
                format!(
                    "this run has used all {} allowed image generations; continue without a new image",
                    self.max_generations
                ),
            ));
        }
        let output = validate_output_path(&scope.worktree, &call.output_path)?;
        let generator = self.generator.as_ref().ok_or_else(|| {
            ImageToolError::new(
                ImageToolErrorCode::BackendNotConfigured,
                "no image backend is configured in this Senate process",
            )
        })?;
        let image = generator.generate(&request)?;
        let header = png::validate(&image.png)
            .map_err(|reason| ImageToolError::new(ImageToolErrorCode::InvalidImage, reason))?;
        let written = output.write_no_overwrite(&image.png)?;
        let ordinal = used + 1;
        let record = ImageGenerationRecord {
            id: format!("imggen-{}", ulid::Ulid::new()),
            run_id: scope.run_id,
            stage_id: scope.stage_id.clone(),
            attempt: scope.attempt,
            ordinal,
            backend: image.backend.to_owned(),
            model: image.model.clone(),
            output_path: output.relative.clone(),
            output_sha256: sha256_hex(&image.png),
            output_size: image.png.len() as u64,
            prompt_sha256: sha256_hex(request.prompt.as_bytes()),
            response_id: image.response_id.clone(),
            requested_at,
            completed_at: now(),
        };
        if let Err(error) = self.write_evidence(&record, &request).and_then(|()| {
            store
                .insert_image_generation(&record)
                .map_err(|e| e.to_string())
        }) {
            // Evidence and file must agree: a PNG nobody can account for is
            // worse than no PNG, and the agent is told to try again.
            let _ = fs::remove_file(&written);
            return Err(ImageToolError::new(
                ImageToolErrorCode::Internal,
                format!("generated image could not be recorded and was removed: {error}"),
            ));
        }
        Ok(ImageToolSuccess {
            output_path: record.output_path,
            width: header.width,
            height: header.height,
            byte_size: record.output_size,
            sha256: record.output_sha256,
            backend: record.backend,
            model: record.model,
            ordinal,
            remaining: self.max_generations - ordinal,
        })
    }

    fn request_from(call: &ImageToolCall) -> Result<ImageRequest, ImageToolError> {
        let prompt = call.prompt.trim();
        if prompt.is_empty() {
            return Err(ImageToolError::new(
                ImageToolErrorCode::InvalidArgument,
                "prompt must not be empty",
            ));
        }
        if prompt.len() > MAX_PROMPT_BYTES {
            return Err(ImageToolError::new(
                ImageToolErrorCode::InvalidArgument,
                format!("prompt exceeds {MAX_PROMPT_BYTES} bytes"),
            ));
        }
        let size = match call.size.as_deref() {
            None => ImageSize::default(),
            Some(value) => ImageSize::parse(value).ok_or_else(|| {
                ImageToolError::new(
                    ImageToolErrorCode::InvalidArgument,
                    format!("size {value:?} is not one of auto, 1024x1024, 1536x1024, 1024x1536"),
                )
            })?,
        };
        let quality = match call.quality.as_deref() {
            None => ImageQuality::default(),
            Some(value) => ImageQuality::parse(value).ok_or_else(|| {
                ImageToolError::new(
                    ImageToolErrorCode::InvalidArgument,
                    format!("quality {value:?} is not one of low, medium, high"),
                )
            })?,
        };
        Ok(ImageRequest {
            prompt: prompt.to_owned(),
            size,
            quality,
            transparent_background: call.transparent_background.unwrap_or(false),
        })
    }

    /// Run-private evidence with the prompt in it, beside the process logs
    /// that already retain the agent's tool-call arguments. Not in the
    /// repository, not in the database.
    fn write_evidence(
        &self,
        record: &ImageGenerationRecord,
        request: &ImageRequest,
    ) -> Result<(), String> {
        let directory = self.evidence_directory(record.run_id);
        fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
        let evidence = serde_json::json!({
            "schema_version": 1,
            "id": record.id,
            "run_id": record.run_id.to_string(),
            "stage_id": record.stage_id.as_str(),
            "attempt": record.attempt,
            "ordinal": record.ordinal,
            "backend": record.backend,
            "model": record.model,
            "output_path": record.output_path,
            "output_sha256": record.output_sha256,
            "output_size": record.output_size,
            "prompt": request.prompt,
            "prompt_sha256": record.prompt_sha256,
            "size": request.size.as_str(),
            "quality": request.quality.as_str(),
            "transparent_background": request.transparent_background,
            "response_id": record.response_id,
            "requested_at": record.requested_at.to_rfc3339(),
            "completed_at": record.completed_at.to_rfc3339(),
        });
        let path = directory.join(format!("{:03}.json", record.ordinal));
        let temporary = directory.join(format!(".{:03}.json.tmp", record.ordinal));
        fs::write(
            &temporary,
            serde_json::to_vec_pretty(&evidence).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        fs::rename(&temporary, &path).map_err(|error| error.to_string())
    }

    /// Where a run's prompt evidence lives.
    #[must_use]
    pub fn evidence_directory(&self, run_id: RunId) -> PathBuf {
        evidence_directory(&self.evidence_root, run_id)
    }
}

/// `<evidence_root>/<run>/image-generations/`, beside `processes/`.
#[must_use]
pub fn evidence_directory(root: &Path, run_id: RunId) -> PathBuf {
    root.join(run_id.to_string()).join("image-generations")
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(hex, "{byte:02x}").expect("String writes cannot fail");
    }
    hex
}

fn now() -> DateTime<Utc> {
    std::time::SystemTime::now().into()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use chrono::{DateTime, Utc};
    use tempfile::TempDir;

    use super::*;
    use crate::domain::{
        ConfigSnapshotId, EventId, EventMetadata, Run, StageDefinition, StageKind,
        WorkflowDefinition, WorkflowKind,
    };
    use crate::image::{FakeImageGenerator, ImageBackendError};
    use crate::store::ResolvedConfigSnapshot;

    struct Fixture {
        _temp: TempDir,
        database: PathBuf,
        evidence: PathBuf,
        worktree: PathBuf,
        run_id: RunId,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = TempDir::new().unwrap();
            let database = temp.path().join("senate.db");
            let worktree = temp.path().join("worktree");
            fs::create_dir_all(worktree.join("assets")).unwrap();
            let run_id = RunId::new();
            let mut store = SqliteStore::open(&database).unwrap();
            let at: DateTime<Utc> = std::time::SystemTime::now().into();
            let config_id = ConfigSnapshotId::new("img-config").unwrap();
            let workflow = WorkflowDefinition::new(
                WorkflowKind::Fast,
                vec![StageDefinition::new(
                    StageId::new("implementation").unwrap(),
                    StageKind::Implementation,
                    Role::Implementer,
                    vec![],
                )],
            )
            .unwrap();
            let run = Run::new(run_id, workflow, config_id.clone(), at);
            let config =
                ResolvedConfigSnapshot::new(config_id, 1, serde_json::json!({"v": 1}), at).unwrap();
            let event = run.created_event(EventMetadata::new(EventId::new(), at));
            store.create_run(&run, &config, &[event]).unwrap();
            Self {
                evidence: temp.path().join("runs"),
                _temp: temp,
                database,
                worktree,
                run_id,
            }
        }

        fn scope(&self, role: Role) -> ImageToolScope {
            ImageToolScope {
                run_id: self.run_id,
                stage_id: StageId::new("implementation").unwrap(),
                attempt: 1,
                role,
                worktree: self.worktree.clone(),
                database: self.database.clone(),
            }
        }

        fn service(&self, generator: Arc<dyn ImageGenerator>, max: u32) -> ImageToolService {
            ImageToolService::new(
                self.evidence.clone(),
                Some(generator),
                vec![Role::Implementer],
                max,
            )
        }
    }

    fn call(path: &str) -> ImageToolCall {
        ImageToolCall {
            prompt: format!("hero image for {path}"),
            output_path: path.to_owned(),
            size: None,
            quality: None,
            transparent_background: None,
        }
    }

    #[test]
    fn a_generation_lands_exact_bytes_evidence_and_prompt_file() {
        let fixture = Fixture::new();
        let generator = Arc::new(FakeImageGenerator::new());
        let service = fixture.service(generator.clone(), 4);
        let success = service
            .generate(&fixture.scope(Role::Implementer), &call("assets/hero.png"))
            .unwrap();
        let expected = FakeImageGenerator::png_for("hero image for assets/hero.png");
        let on_disk = fs::read(fixture.worktree.join("assets/hero.png")).unwrap();
        assert_eq!(on_disk, expected);
        assert_eq!(success.output_path, "assets/hero.png");
        assert_eq!(success.sha256, sha256_hex(&expected));
        assert_eq!(success.byte_size, expected.len() as u64);
        assert_eq!((success.width, success.height), (8, 8));
        assert_eq!((success.ordinal, success.remaining), (1, 3));
        assert_eq!(success.backend, "fake");

        let store = SqliteStore::open(&fixture.database).unwrap();
        let records = store.list_image_generations(fixture.run_id).unwrap();
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.stage_id.as_str(), "implementation");
        assert_eq!(record.attempt, 1);
        assert_eq!(record.ordinal, 1);
        assert_eq!(record.backend, "fake");
        assert_eq!(record.model, "fake-image-v1");
        assert_eq!(record.output_path, "assets/hero.png");
        assert_eq!(record.output_sha256, sha256_hex(&on_disk));
        assert_eq!(record.output_size, on_disk.len() as u64);
        assert_eq!(
            record.prompt_sha256,
            sha256_hex(b"hero image for assets/hero.png")
        );
        assert_eq!(record.response_id.as_deref(), Some("fake-1"));
        assert!(record.completed_at >= record.requested_at);

        let evidence: serde_json::Value = serde_json::from_slice(
            &fs::read(service.evidence_directory(fixture.run_id).join("001.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(evidence["prompt"], "hero image for assets/hero.png");
        assert_eq!(evidence["output_sha256"], record.output_sha256);
        assert_eq!(evidence["quality"], "medium");
        assert_eq!(evidence["size"], "auto");
        // The worktree holds the PNG and nothing else new: no temp files, no
        // evidence, no prompt.
        let names: Vec<_> = fs::read_dir(fixture.worktree.join("assets"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["hero.png"]);
        assert_eq!(generator.requests().len(), 1);
    }

    #[test]
    fn the_bound_is_enforced_from_persisted_rows_and_never_calls_the_backend_past_it() {
        let fixture = Fixture::new();
        let generator = Arc::new(FakeImageGenerator::new());
        let service = fixture.service(generator.clone(), 2);
        let scope = fixture.scope(Role::Implementer);
        assert_eq!(
            service.generate(&scope, &call("a.png")).unwrap().remaining,
            1
        );
        assert_eq!(
            service.generate(&scope, &call("b.png")).unwrap().remaining,
            0
        );
        let error = service.generate(&scope, &call("c.png")).unwrap_err();
        assert_eq!(error.code, ImageToolErrorCode::LimitReached);
        assert!(!fixture.worktree.join("c.png").exists());
        assert_eq!(
            generator.requests().len(),
            2,
            "N+1 must not reach the backend"
        );
        // A fresh service over the same database (a restarted The Senate) sees
        // the same count: the bound lives in the store, not in memory.
        let restarted = fixture.service(Arc::new(FakeImageGenerator::new()), 2);
        assert_eq!(
            restarted.generate(&scope, &call("d.png")).unwrap_err().code,
            ImageToolErrorCode::LimitReached
        );
        assert_eq!(
            SqliteStore::open(&fixture.database)
                .unwrap()
                .count_image_generations(fixture.run_id)
                .unwrap(),
            2
        );
    }

    #[test]
    fn unauthorized_role_missing_backend_and_bad_paths_are_typed_and_free() {
        let fixture = Fixture::new();
        let generator = Arc::new(FakeImageGenerator::new());
        let service = fixture.service(generator.clone(), 4);
        for role in [
            Role::CodeQualityReviewer,
            Role::SpecReviewer,
            Role::Architect,
        ] {
            assert_eq!(
                service
                    .generate(&fixture.scope(role), &call("assets/x.png"))
                    .unwrap_err()
                    .code,
                ImageToolErrorCode::NotAuthorized,
                "{role:?}"
            );
        }
        let scope = fixture.scope(Role::Implementer);
        for (path, code) in [
            ("/etc/x.png", ImageToolErrorCode::InvalidOutputPath),
            ("../x.png", ImageToolErrorCode::InvalidOutputPath),
            ("assets/x.jpg", ImageToolErrorCode::InvalidOutputPath),
            (".git/x.png", ImageToolErrorCode::InvalidOutputPath),
        ] {
            assert_eq!(
                service.generate(&scope, &call(path)).unwrap_err().code,
                code,
                "{path}"
            );
        }
        fs::write(fixture.worktree.join("assets/taken.png"), b"project").unwrap();
        assert_eq!(
            service
                .generate(&scope, &call("assets/taken.png"))
                .unwrap_err()
                .code,
            ImageToolErrorCode::OutputExists
        );
        assert_eq!(
            fs::read(fixture.worktree.join("assets/taken.png")).unwrap(),
            b"project"
        );
        let mut bad = call("assets/ok.png");
        bad.size = Some("4096x4096".to_owned());
        assert_eq!(
            service.generate(&scope, &bad).unwrap_err().code,
            ImageToolErrorCode::InvalidArgument
        );
        let mut empty = call("assets/ok.png");
        empty.prompt = "   ".to_owned();
        assert_eq!(
            service.generate(&scope, &empty).unwrap_err().code,
            ImageToolErrorCode::InvalidArgument
        );
        assert!(
            generator.requests().is_empty(),
            "no refusal may cost a vendor call"
        );
        assert_eq!(
            SqliteStore::open(&fixture.database)
                .unwrap()
                .count_image_generations(fixture.run_id)
                .unwrap(),
            0
        );

        let unconfigured =
            ImageToolService::new(fixture.evidence.clone(), None, vec![Role::Implementer], 4);
        assert_eq!(
            unconfigured
                .generate(&scope, &call("assets/ok.png"))
                .unwrap_err()
                .code,
            ImageToolErrorCode::BackendNotConfigured
        );
        let rejected = fixture.service(
            Arc::new(FakeImageGenerator::failing(ImageBackendError::Rejected(
                "policy".to_owned(),
            ))),
            4,
        );
        assert_eq!(
            rejected
                .generate(&scope, &call("assets/ok.png"))
                .unwrap_err()
                .code,
            ImageToolErrorCode::BackendRejected
        );
        assert!(!fixture.worktree.join("assets/ok.png").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_escape_never_writes_outside_the_worktree() {
        let fixture = Fixture::new();
        let outside = TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), fixture.worktree.join("public")).unwrap();
        let generator = Arc::new(FakeImageGenerator::new());
        let service = fixture.service(generator.clone(), 4);
        let error = service
            .generate(&fixture.scope(Role::Implementer), &call("public/hero.png"))
            .unwrap_err();
        assert_eq!(error.code, ImageToolErrorCode::InvalidOutputPath);
        assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
        assert!(generator.requests().is_empty());
    }
}
