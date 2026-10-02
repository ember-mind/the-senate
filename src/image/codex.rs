//! Image backend on the user's own Codex CLI.
//!
//! Codex ships a built-in `image_gen` tool that runs on the CLI's native
//! (`ChatGPT`) authentication and needs no API key. The Senate drives it the way
//! it drives every other native CLI: one `codex exec --json` invocation with
//! the request on stdin, read-only sandbox, no approvals. Codex writes the
//! result under `$CODEX_HOME/generated_images/<thread-id>/`, a directory that
//! exists only for this invocation, so The Senate collects the PNG from there
//! and never trusts a path the model typed. The concrete image model is
//! Codex's own and is not reported; evidence records the backend as such.
//!
//! Verified against Codex CLI 0.149.0 on 2026-09-02: the `--json` stream
//! carries `thread.started` (thread id), agent messages, and `turn.completed`
//! / `turn.failed`; the tool call itself is not surfaced as an event.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::providers::codex::{CodexInstallation, CodexProviderError, session_meta};

use super::{GeneratedImage, ImageBackendError, ImageGenerator, ImageRequest};

/// Generation can take a few minutes at high quality.
const TIMEOUT: Duration = Duration::from_secs(300);
const MAX_STDOUT_BYTES: usize = 8 * 1024 * 1024;
/// Largest PNG collected from Codex; anything bigger is refused unread.
const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;
/// What the evidence row says produced the image: the CLI's built-in tool,
/// whose underlying model Codex does not expose.
pub const CODEX_IMAGE_MODEL: &str = "codex/image_gen";

/// Whether this process can reach the backend: Codex installed and
/// authenticated natively. No value is ever read or printed.
///
/// # Errors
/// The discovery failure, or `NotAuthenticated`.
pub fn backend_available() -> Result<CodexInstallation, CodexProviderError> {
    let installation = CodexInstallation::discover()?;
    if !installation.authenticated() {
        return Err(CodexProviderError::NotAuthenticated);
    }
    Ok(installation)
}

pub struct CodexImageGenerator {
    executable: PathBuf,
    codex_home: PathBuf,
}

impl std::fmt::Debug for CodexImageGenerator {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CodexImageGenerator")
            .field("executable", &self.executable)
            .field("codex_home", &self.codex_home)
            .finish()
    }
}

impl CodexImageGenerator {
    /// Discovers the installed, authenticated Codex CLI and its home.
    ///
    /// # Errors
    /// `NotConfigured` when Codex is missing, unauthenticated, or has no home.
    pub fn from_environment() -> Result<Self, ImageBackendError> {
        let installation = backend_available()
            .map_err(|error| ImageBackendError::NotConfigured(error.to_string()))?;
        let codex_home = session_meta::home_from_environment().ok_or_else(|| {
            ImageBackendError::NotConfigured("CODEX_HOME/HOME is not set".to_owned())
        })?;
        Ok(Self {
            executable: installation.executable().to_path_buf(),
            codex_home,
        })
    }

    #[cfg(test)]
    pub(crate) fn new(executable: PathBuf, codex_home: PathBuf) -> Self {
        Self {
            executable,
            codex_home,
        }
    }

    /// The instruction Codex receives. Size, quality, and transparency ride
    /// in prose because the built-in tool documents no argument surface.
    fn instruction(request: &ImageRequest) -> String {
        let mut text = String::new();
        text.push_str(
            "Use your built-in image_gen tool to generate exactly one image and nothing else. \
             Do not read, write, move, copy, or delete any file; do not run commands; do not \
             inspect the project. When the image is generated, reply with only the absolute \
             path of the generated file.\n\n",
        );
        text.push_str("Image request:\n");
        text.push_str(&request.prompt);
        text.push_str("\n\nOutput requirements:\n");
        text.push_str("- size: ");
        text.push_str(request.size.as_str());
        text.push_str("\n- quality: ");
        text.push_str(request.quality.as_str());
        text.push('\n');
        text.push_str(if request.transparent_background {
            "- background: transparent, preserve alpha\n"
        } else {
            "- background: opaque\n"
        });
        text.push_str("- format: PNG\n");
        text
    }

    fn run(&self, instruction: &str) -> Result<CodexOutput, ImageBackendError> {
        self.run_with_timeout(instruction, TIMEOUT)
    }

    fn run_with_timeout(
        &self,
        instruction: &str,
        timeout: Duration,
    ) -> Result<CodexOutput, ImageBackendError> {
        let scratch = tempfile::tempdir()
            .map_err(|error| ImageBackendError::Network(format!("scratch dir: {error}")))?;
        // Tests exec a freshly written stub `codex` script immediately, which
        // can race another test thread's fork still holding its write fd
        // open (`ETXTBSY`); retry_busy clears that window without hiding a
        // real failure.
        let mut command = crate::exec::without_jira_credentials(Command::new(&self.executable));
        command
            // Sandbox and approval are root options; the working directory
            // and the git check belong to `exec` (`codex exec --help`).
            .args(["--sandbox", "read-only", "--ask-for-approval", "never"])
            .args(["exec", "--skip-git-repo-check", "-C"])
            .arg(scratch.path())
            .args(["--json", "--color", "never", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
        let mut child = crate::exec::retry_busy(|| command.spawn())
            .map_err(|error| ImageBackendError::Network(format!("codex launch: {error}")))?;
        if let Some(mut stdin) = child.stdin.take()
            && let Err(error) = stdin.write_all(instruction.as_bytes())
        {
            crate::exec::kill_process_tree(&mut child);
            return Err(ImageBackendError::Network(format!("codex stdin: {error}")));
        }
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| ImageBackendError::Network("codex stdout unavailable".to_owned()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| ImageBackendError::Network("codex stderr unavailable".to_owned()))?;
        let reader = std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = (&mut stdout)
                .take(MAX_STDOUT_BYTES as u64)
                .read_to_end(&mut buffer);
            buffer
        });
        let errors = std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = (&mut stderr)
                .take(MAX_STDOUT_BYTES as u64)
                .read_to_end(&mut buffer);
            buffer
        });
        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) if reader.is_finished() && errors.is_finished() => break,
                Ok(_) if started.elapsed() >= timeout => {
                    crate::exec::kill_process_tree(&mut child);
                    let _ = reader.join();
                    let _ = errors.join();
                    return Err(ImageBackendError::Network(format!(
                        "codex image generation exceeded {} seconds",
                        timeout.as_secs()
                    )));
                }
                Ok(_) => std::thread::sleep(Duration::from_millis(200)),
                Err(error) => {
                    crate::exec::kill_process_tree(&mut child);
                    let _ = reader.join();
                    let _ = errors.join();
                    return Err(ImageBackendError::Network(format!("codex wait: {error}")));
                }
            }
        }
        let output = reader
            .join()
            .map_err(|_| ImageBackendError::Network("codex reader thread failed".to_owned()))?;
        let diagnostics = errors.join().unwrap_or_default();
        Ok(CodexOutput {
            events: String::from_utf8_lossy(&output).into_owned(),
            stderr_tail: tail(&String::from_utf8_lossy(&diagnostics)),
        })
    }
}

struct CodexOutput {
    events: String,
    stderr_tail: String,
}

/// The last few non-empty stderr lines, bounded, for an error message.
fn tail(text: &str) -> String {
    let lines = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let start = lines.len().saturating_sub(4);
    lines[start..].join(" | ").chars().take(600).collect()
}

/// What one invocation's JSON stream says: the thread that generated, or
/// why it failed. Split out so the vendor shapes are tested without Codex.
pub(crate) fn thread_from_events(events: &str) -> Result<String, ImageBackendError> {
    let mut thread_id = None;
    let mut completed = false;
    for line in events.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("thread.started") => {
                thread_id = value
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
            }
            Some("turn.completed") => completed = true,
            Some("turn.failed" | "error") => {
                let message = value
                    .pointer("/error/message")
                    .or_else(|| value.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("codex turn failed");
                return Err(ImageBackendError::Rejected(
                    message.chars().take(400).collect(),
                ));
            }
            _ => {}
        }
    }
    let thread_id = thread_id.ok_or_else(|| {
        ImageBackendError::InvalidResponse("codex never announced a thread".to_owned())
    })?;
    if !completed {
        return Err(ImageBackendError::InvalidResponse(
            "codex exited without completing its turn".to_owned(),
        ));
    }
    if !session_meta::is_plausible_thread_id(&thread_id) {
        return Err(ImageBackendError::InvalidResponse(
            "codex thread id is not a safe path component".to_owned(),
        ));
    }
    Ok(thread_id)
}

/// Collects the one PNG Codex wrote for `thread_id`. The directory is
/// created by Codex for this thread alone; more than one file means the
/// tool ran more than once, which is refused rather than guessed.
pub(crate) fn collect_output(
    codex_home: &Path,
    thread_id: &str,
) -> Result<Vec<u8>, ImageBackendError> {
    let directory = codex_home.join("generated_images").join(thread_id);
    let mut pngs = std::fs::read_dir(&directory)
        .map_err(|error| {
            ImageBackendError::InvalidResponse(format!(
                "codex produced no image directory for its thread: {error}"
            ))
        })?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "png"))
        .collect::<Vec<_>>();
    match pngs.len() {
        0 => Err(ImageBackendError::InvalidResponse(
            "codex completed without writing an image".to_owned(),
        )),
        1 => {
            let path = pngs.remove(0);
            let size = std::fs::metadata(&path)
                .map_err(|error| ImageBackendError::InvalidResponse(error.to_string()))?
                .len();
            if size > MAX_IMAGE_BYTES {
                return Err(ImageBackendError::InvalidResponse(format!(
                    "codex image is {size} bytes, above the {MAX_IMAGE_BYTES} byte ceiling"
                )));
            }
            std::fs::read(&path)
                .map_err(|error| ImageBackendError::InvalidResponse(error.to_string()))
        }
        count => Err(ImageBackendError::InvalidResponse(format!(
            "codex wrote {count} images for one request; refusing to pick"
        ))),
    }
}

impl ImageGenerator for CodexImageGenerator {
    fn backend(&self) -> &'static str {
        "codex"
    }

    fn model(&self) -> &str {
        CODEX_IMAGE_MODEL
    }

    fn generate(&self, request: &ImageRequest) -> Result<GeneratedImage, ImageBackendError> {
        let output = self.run(&Self::instruction(request))?;
        // A launch that never reached the model shows up as no thread at all;
        // Codex's own stderr is the only explanation there is.
        let thread_id = thread_from_events(&output.events).map_err(|error| match error {
            ImageBackendError::InvalidResponse(text) if !output.stderr_tail.is_empty() => {
                ImageBackendError::InvalidResponse(format!(
                    "{text}; codex stderr: {}",
                    output.stderr_tail
                ))
            }
            other => other,
        })?;
        let png = collect_output(&self.codex_home, &thread_id)?;
        Ok(GeneratedImage {
            png,
            backend: "codex",
            model: CODEX_IMAGE_MODEL.to_owned(),
            response_id: Some(thread_id),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::{ImageQuality, ImageSize};

    #[test]
    fn the_instruction_forbids_file_and_command_activity_and_states_the_output() {
        let text = CodexImageGenerator::instruction(&ImageRequest {
            prompt: "a hero".to_owned(),
            size: ImageSize::Landscape1536x1024,
            quality: ImageQuality::High,
            transparent_background: true,
        });
        assert!(text.contains("built-in image_gen tool"));
        assert!(text.contains("Do not read, write, move, copy, or delete any file"));
        assert!(text.contains("a hero"));
        assert!(text.contains("size: 1536x1024"));
        assert!(text.contains("quality: high"));
        assert!(text.contains("background: transparent"));
    }

    #[test]
    fn the_real_event_shape_yields_the_thread_and_failures_are_typed() {
        let stream = r#"{"type":"thread.started","thread_id":"01a0629c-e3fe-7310-83d7-02b067319e0c"}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"Generating one image."}}
{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"/Users/x/.codex/generated_images/01a0629c-e3fe-7310-83d7-02b067319e0c/exec-dff2.png"}}
{"type":"turn.completed","usage":{"input_tokens":83071,"output_tokens":896}}
"#;
        assert_eq!(
            thread_from_events(stream).unwrap(),
            "01a0629c-e3fe-7310-83d7-02b067319e0c"
        );
        let failed = r#"{"type":"thread.started","thread_id":"t-1"}
{"type":"turn.failed","error":{"message":"image_gen refused: policy"}}
"#;
        assert_eq!(
            thread_from_events(failed).unwrap_err(),
            ImageBackendError::Rejected("image_gen refused: policy".to_owned())
        );
        assert!(matches!(
            thread_from_events("{\"type\":\"turn.started\"}\n").unwrap_err(),
            ImageBackendError::InvalidResponse(_)
        ));
        assert!(matches!(
            thread_from_events("{\"type\":\"thread.started\",\"thread_id\":\"t-1\"}\n")
                .unwrap_err(),
            ImageBackendError::InvalidResponse(_)
        ));
        assert!(matches!(
            thread_from_events(
                "{\"type\":\"thread.started\",\"thread_id\":\"../escape\"}\n{\"type\":\"turn.completed\"}\n"
            )
            .unwrap_err(),
            ImageBackendError::InvalidResponse(_)
        ));
    }

    #[test]
    fn exactly_one_png_in_the_thread_directory_is_collected() {
        let home = tempfile::TempDir::new().unwrap();
        let directory = home.path().join("generated_images").join("t-1");
        std::fs::create_dir_all(&directory).unwrap();
        assert!(matches!(
            collect_output(home.path(), "t-1").unwrap_err(),
            ImageBackendError::InvalidResponse(_)
        ));
        std::fs::write(directory.join("exec-a.png"), b"png-a").unwrap();
        std::fs::write(directory.join("notes.txt"), b"ignored").unwrap();
        assert_eq!(collect_output(home.path(), "t-1").unwrap(), b"png-a");
        std::fs::write(directory.join("exec-b.png"), b"png-b").unwrap();
        assert!(matches!(
            collect_output(home.path(), "t-1").unwrap_err(),
            ImageBackendError::InvalidResponse(_)
        ));
        assert!(matches!(
            collect_output(home.path(), "missing").unwrap_err(),
            ImageBackendError::InvalidResponse(_)
        ));
    }

    /// A stand-in `codex` that behaves like the real one on the wire: it
    /// reads its instruction from stdin, announces a thread, writes one PNG
    /// under `$CODEX_HOME/generated_images/<thread>/`, and completes.
    #[cfg(unix)]
    #[test]
    fn a_fake_codex_on_the_wire_yields_the_png_it_wrote() {
        use std::os::unix::fs::PermissionsExt as _;

        let home = tempfile::TempDir::new().unwrap();
        let png = crate::image::png::synthesize(2, 2, 7);
        let png_path = home.path().join("fixture.png");
        std::fs::write(&png_path, &png).unwrap();
        let script = home.path().join("fake-codex");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ncat > \"{home}/instruction.txt\"\nmkdir -p \"{home}/generated_images/t-42\"\ncp \"{png}\" \"{home}/generated_images/t-42/exec-1.png\"\necho '{{\"type\":\"thread.started\",\"thread_id\":\"t-42\"}}'\necho '{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":\"done\"}}}}'\necho '{{\"type\":\"turn.completed\"}}'\n",
                home = home.path().display(),
                png = png_path.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let generator = CodexImageGenerator::new(script.clone(), home.path().to_path_buf());
        let image = generator
            .generate(&ImageRequest {
                prompt: "a probe".to_owned(),
                size: ImageSize::Square1024,
                quality: ImageQuality::Low,
                transparent_background: false,
            })
            .unwrap();
        assert_eq!(image.png, png);
        assert_eq!(image.backend, "codex");
        assert_eq!(image.model, CODEX_IMAGE_MODEL);
        assert_eq!(image.response_id.as_deref(), Some("t-42"));
        let instruction = std::fs::read_to_string(home.path().join("instruction.txt")).unwrap();
        assert!(instruction.contains("a probe"));
        assert!(instruction.contains("size: 1024x1024"));

        // A codex that fails its turn is a typed rejection, not a panic.
        //
        // The script drains stdin before saying anything. A real codex reads
        // its instruction; a fake one that exits without reading closes the
        // pipe under the write still in progress, and the rejection under test
        // arrives as `Network("codex stdin: Broken pipe")` instead. That is a
        // race with the machine, not a behaviour — it lost only under CI load.
        std::fs::write(
            &script,
            "#!/bin/sh\ncat > /dev/null\necho '{\"type\":\"thread.started\",\"thread_id\":\"t-43\"}'\necho '{\"type\":\"turn.failed\",\"error\":{\"message\":\"quota\"}}'\n",
        )
        .unwrap();
        assert_eq!(
            generator
                .generate(&ImageRequest {
                    prompt: "p".to_owned(),
                    size: ImageSize::Auto,
                    quality: ImageQuality::Medium,
                    transparent_background: false,
                })
                .unwrap_err(),
            ImageBackendError::Rejected("quota".to_owned())
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_codex_descendant_cannot_hold_output_past_the_generation_timeout() {
        use std::os::unix::fs::PermissionsExt as _;
        let home = tempfile::tempdir().unwrap();
        let script = home.path().join("background-codex");
        std::fs::write(&script, "#!/bin/sh\ncat > /dev/null\nsleep 6 &\nexit 0\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let generator = CodexImageGenerator::new(script, home.path().to_path_buf());
        let started = Instant::now();
        assert!(matches!(
            generator.run_with_timeout("generate", Duration::from_secs(1)),
            Err(ImageBackendError::Network(_))
        ));
        assert!(started.elapsed() < Duration::from_secs(4));
    }
}
