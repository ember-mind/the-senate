//! An inspectable, local progress brief over committed mission evidence.
//!
//! The model contains facts from `MissionDetails`. A lead's words are kept in
//! separately labelled narrative fields; they never determine package state.
//! Identical input produces identical HTML, JSON and a content-addressed path.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use thiserror::Error;

use crate::domain::{MissionStatus, WorkPackageResult, WorkPackageStatus};
use crate::providers::section;
use crate::store::sha256_hex;

use super::mission_lead::LeadAnswer;
use super::mission_query::{DecisionSummary, MissionDetails, WorkPackageSummary};

const MAX_VISUALS: usize = 8;
const MAX_VISUAL_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum BriefError {
    #[error("mission recap needs a file-backed Senate data directory")]
    NoDataDirectory,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("a recap accepts at most {MAX_VISUALS} visual evidence files")]
    TooManyVisuals,
    #[error("visual evidence {0} must be a PNG, JPEG, GIF or WebP image")]
    UnsupportedVisual(PathBuf),
    #[error("visual evidence {0} exceeds the 8 MiB limit")]
    VisualTooLarge(PathBuf),
    #[error("existing recap at {0} differs from its content-addressed evidence")]
    ExistingRecapDiffers(PathBuf),
}

#[derive(Clone, Debug, Serialize)]
struct BriefPackage {
    id: String,
    title: String,
    goal: String,
    status: WorkPackageStatus,
    reason: Option<String>,
    result: Option<WorkPackageResult>,
}

impl From<&WorkPackageSummary> for BriefPackage {
    fn from(package: &WorkPackageSummary) -> Self {
        Self {
            id: package.id.to_string(),
            title: package.title.clone(),
            goal: package.goal.clone(),
            status: package.status,
            reason: package.reason.clone(),
            result: package.result.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct Narrative {
    source: String,
    overview: Option<String>,
    progress: Option<String>,
    quality: Option<String>,
    decisions: Option<String>,
    next_steps: Option<String>,
}

impl Narrative {
    fn from_answer(answer: &LeadAnswer) -> Self {
        let prose = answer.prose();
        let progress = section::extract_verbatim(&prose, "progress");
        let quality = section::extract_verbatim(&prose, "quality");
        let decisions = section::extract_verbatim(&prose, "decisions");
        let next_steps = section::extract_verbatim(&prose, "nextsteps");
        let overview = section::extract_verbatim(&prose, "summary")
            .or_else(|| section::extract_verbatim(&prose, "bottomline"))
            .or_else(|| {
                (progress.is_none()
                    && quality.is_none()
                    && decisions.is_none()
                    && next_steps.is_none()
                    && !prose.trim().is_empty())
                .then_some(prose)
            });
        Self {
            source: format!("Consul · run {} · turn {}", answer.run_id, answer.turn),
            overview,
            progress,
            quality,
            decisions,
            next_steps,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct VisualEvidence {
    caption: String,
    file: String,
    sha256: String,
}

struct VisualFile {
    evidence: VisualEvidence,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug, Serialize)]
struct ProgressBrief {
    schema_version: u8,
    mission_id: String,
    title: String,
    goal: String,
    status: MissionStatus,
    revision: u64,
    updated_at: String,
    integrated: Vec<BriefPackage>,
    unfinished: Vec<BriefPackage>,
    cancelled: Vec<BriefPackage>,
    decisions: Vec<DecisionSummary>,
    narrative: Option<Narrative>,
    visuals: Vec<VisualEvidence>,
}

impl ProgressBrief {
    fn new(details: &MissionDetails, answer: Option<&LeadAnswer>, visuals: &[VisualFile]) -> Self {
        Self {
            schema_version: 1,
            mission_id: details.id.to_string(),
            title: details.title.clone(),
            goal: details.goal.clone(),
            status: details.status,
            revision: details.revision.value(),
            updated_at: details.updated_at.to_rfc3339(),
            integrated: details
                .packages
                .iter()
                .filter(|package| package.status == WorkPackageStatus::Integrated)
                .map(BriefPackage::from)
                .collect(),
            unfinished: details
                .packages
                .iter()
                .filter(|package| {
                    !matches!(
                        package.status,
                        WorkPackageStatus::Integrated | WorkPackageStatus::Cancelled
                    )
                })
                .map(BriefPackage::from)
                .collect(),
            cancelled: details
                .packages
                .iter()
                .filter(|package| package.status == WorkPackageStatus::Cancelled)
                .map(BriefPackage::from)
                .collect(),
            decisions: details.decisions.clone(),
            narrative: answer.map(Narrative::from_answer),
            visuals: visuals.iter().map(|file| file.evidence.clone()).collect(),
        }
    }
}

/// Writes one immutable snapshot under `<data-dir>/missions/<id>/briefs/`.
/// A repeated request over identical evidence returns the same path.
pub(crate) fn write(
    data_dir: &Path,
    details: &MissionDetails,
    answer: Option<&LeadAnswer>,
    visual_paths: &[PathBuf],
) -> Result<PathBuf, BriefError> {
    let visuals = read_visuals(visual_paths)?;
    let brief = ProgressBrief::new(details, answer, &visuals);
    let json = serde_json::to_vec_pretty(&brief)?;
    let html = render(&brief);
    let brief_id = sha256_hex(&json);
    let briefs_dir = data_dir
        .join("missions")
        .join(details.id.to_string())
        .join("briefs");
    let destination = briefs_dir.join(&brief_id);
    let index = destination.join("index.html");
    if destination.exists() {
        verify_existing(&destination, &json, html.as_bytes(), &visuals)?;
        return Ok(index);
    }
    fs::create_dir_all(&briefs_dir)?;
    let staging = tempfile::Builder::new()
        .prefix(".brief-")
        .tempdir_in(&briefs_dir)?;
    fs::write(staging.path().join("brief.json"), &json)?;
    fs::write(staging.path().join("index.html"), html.as_bytes())?;
    if !visuals.is_empty() {
        fs::create_dir(staging.path().join("assets"))?;
        for visual in &visuals {
            fs::write(staging.path().join(&visual.evidence.file), &visual.bytes)?;
        }
    }
    match fs::rename(staging.path(), &destination) {
        Ok(()) => {
            // `TempDir` sees its old path missing after the rename. Its drop
            // leaves the committed, content-addressed directory untouched.
            Ok(index)
        }
        Err(_) if destination.exists() => {
            verify_existing(&destination, &json, html.as_bytes(), &visuals)?;
            Ok(index)
        }
        Err(error) => Err(BriefError::Io(error)),
    }
}

fn verify_existing(
    directory: &Path,
    json: &[u8],
    html: &[u8],
    visuals: &[VisualFile],
) -> Result<(), BriefError> {
    if fs::read(directory.join("brief.json"))? != json
        || fs::read(directory.join("index.html"))? != html
        || visuals.iter().any(|visual| {
            fs::read(directory.join(&visual.evidence.file))
                .ok()
                .as_deref()
                != Some(visual.bytes.as_slice())
        })
    {
        return Err(BriefError::ExistingRecapDiffers(directory.to_path_buf()));
    }
    Ok(())
}

fn read_visuals(paths: &[PathBuf]) -> Result<Vec<VisualFile>, BriefError> {
    if paths.len() > MAX_VISUALS {
        return Err(BriefError::TooManyVisuals);
    }
    paths
        .iter()
        .map(|path| {
            let size = fs::metadata(path)?.len();
            if size > MAX_VISUAL_BYTES {
                return Err(BriefError::VisualTooLarge(path.clone()));
            }
            let bytes = fs::read(path)?;
            if bytes.len() as u64 > MAX_VISUAL_BYTES {
                return Err(BriefError::VisualTooLarge(path.clone()));
            }
            let extension = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
                "png"
            } else if bytes.starts_with(b"\xff\xd8\xff") {
                "jpg"
            } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
                "gif"
            } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
                "webp"
            } else {
                return Err(BriefError::UnsupportedVisual(path.clone()));
            };
            let digest = sha256_hex(&bytes);
            let caption = path.file_stem().map_or_else(
                || "Visual evidence".to_owned(),
                |stem| stem.to_string_lossy().into_owned(),
            );
            Ok(VisualFile {
                evidence: VisualEvidence {
                    caption,
                    file: format!("assets/{digest}.{extension}"),
                    sha256: digest,
                },
                bytes,
            })
        })
        .collect()
}

#[allow(
    clippy::too_many_lines,
    reason = "one deterministic HTML template over the brief model"
)]
fn render(brief: &ProgressBrief) -> String {
    let mut html = String::from(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><meta name=\"color-scheme\" content=\"light\"><title>",
    );
    html.push_str(&escape(&brief.title));
    html.push_str(" · The Senate recap</title><style>");
    html.push_str("body{margin:0;background:#f4f0e7;color:#24221f;font:16px/1.55 system-ui,sans-serif}main{max-width:960px;margin:auto;padding:48px 24px 100px}header{border-bottom:3px solid #9d3c31;padding-bottom:24px}h1{font:700 clamp(2rem,5vw,3.5rem)/1.1 Georgia,serif;margin:.3em 0}h2{font:700 1.65rem Georgia,serif;margin:2em 0 .7em}h3{margin:.2em 0}p{margin:.5em 0 1em}.eyebrow{color:#9d3c31;font-weight:700;letter-spacing:.12em;text-transform:uppercase}.meta,.muted{color:#675e54}.stats{display:flex;gap:12px;flex-wrap:wrap;margin:24px 0}.stat,.card,details,.narrative{background:#fff;border:1px solid #d8d0c4;border-radius:10px;padding:16px 20px}.stat strong{display:block;font:700 1.5rem Georgia,serif}.grid{display:grid;gap:14px}.card{border-left:4px solid #9d3c31}.card small{color:#675e54}.narrative{border-left:4px solid #7d8181;background:#f9f9f7;white-space:pre-wrap}.narrative b{display:block;font-size:.8rem;text-transform:uppercase;letter-spacing:.08em}.list{padding-left:1.3em}details{margin-top:14px}summary{cursor:pointer;font-weight:700}code{font-size:.85em;overflow-wrap:anywhere}figure{margin:0 0 24px}figure img{max-width:100%;height:auto;border:1px solid #d8d0c4;border-radius:8px}figcaption{color:#675e54;font-size:.9em}footer{margin-top:48px;border-top:1px solid #d8d0c4;padding-top:16px;color:#675e54;font-size:.9em}@media(max-width:600px){main{padding:28px 16px 70px}}");
    html.push_str("</style></head><body><main><header><div class=\"eyebrow\">The Senate · Mission recap</div><h1>");
    html.push_str(&escape(&brief.title));
    html.push_str("</h1><p>");
    html.push_str(&escape(&brief.goal));
    html.push_str("</p><div class=\"meta\">Mission <code>");
    html.push_str(&escape(&brief.mission_id));
    let _ = write!(
        html,
        "</code> · revision {} · {} · {}</div></header>",
        brief.revision,
        escape(&brief.updated_at),
        status(brief.status)
    );
    let total = brief.integrated.len() + brief.unfinished.len() + brief.cancelled.len();
    let _ = write!(
        html,
        "<div class=\"stats\"><div class=\"stat\"><strong>{}/{total}</strong>Orders integrated</div><div class=\"stat\"><strong>{}</strong>Unfinished</div><div class=\"stat\"><strong>{}</strong>Decisions recorded</div></div>",
        brief.integrated.len(),
        brief.unfinished.len(),
        brief.decisions.len()
    );
    narrative(&mut html, brief.narrative.as_ref(), |n| {
        n.overview.as_deref()
    });
    html.push_str("<section><h2>Integrated work</h2><div class=\"grid\">");
    if brief.integrated.is_empty() {
        html.push_str("<p class=\"muted\">No Orders integrated yet.</p>");
    }
    for package in &brief.integrated {
        html.push_str("<article class=\"card\"><small>INTEGRATED · ");
        html.push_str(&escape(&package.id));
        html.push_str("</small><h3>");
        html.push_str(&escape(&package.title));
        html.push_str("</h3><p>");
        html.push_str(&escape(&package.goal));
        html.push_str("</p>");
        if let Some(result) = &package.result {
            if let Some(bottom_line) = &result.bottom_line {
                html.push_str("<p>");
                html.push_str(&escape(bottom_line));
                html.push_str("</p>");
            }
            let _ = write!(
                html,
                "<small>{} changed file(s){} · verification: {} · reviews: {}</small>",
                result.changed_files.len(),
                if result.changes_complete {
                    ""
                } else {
                    " (partial list)"
                },
                stage_status(result.verification.as_ref()),
                if result.reviews.is_empty() {
                    "none".to_owned()
                } else {
                    result
                        .reviews
                        .iter()
                        .map(|review| {
                            format!(
                                "{} {}",
                                escape(&review.stage_id.to_string()),
                                escape(&format!("{:?}", review.status))
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            );
        } else {
            html.push_str("<small>Delivery evidence unavailable.</small>");
        }
        html.push_str("</article>");
    }
    html.push_str("</div>");
    narrative(&mut html, brief.narrative.as_ref(), |n| {
        n.progress.as_deref()
    });
    html.push_str("</section><section><h2>Quality</h2>");
    if brief.integrated.is_empty() {
        html.push_str("<p class=\"muted\">No integrated Orders to assess yet.</p>");
    } else {
        html.push_str("<ul class=\"list\">");
        for package in &brief.integrated {
            let verification = package
                .result
                .as_ref()
                .and_then(|result| result.verification.as_ref());
            let review_count = package
                .result
                .as_ref()
                .map_or(0, |result| result.reviews.len());
            let _ = write!(
                html,
                "<li><strong>{}</strong> — verification: {}; {} review(s) recorded</li>",
                escape(&package.title),
                stage_status(verification),
                review_count
            );
        }
        html.push_str("</ul>");
    }
    narrative(&mut html, brief.narrative.as_ref(), |n| {
        n.quality.as_deref()
    });
    html.push_str("</section><section><h2>Decisions</h2>");
    if brief.decisions.is_empty() {
        html.push_str("<p class=\"muted\">No decisions recorded.</p>");
    } else {
        html.push_str("<ul class=\"list\">");
        for decision in &brief.decisions {
            let _ = write!(
                html,
                "<li><strong>{}</strong> — {} <small>({:?}, {})</small></li>",
                escape(&decision.title),
                escape(&decision.rationale),
                decision.author,
                escape(&decision.recorded_at.to_rfc3339())
            );
        }
        html.push_str("</ul>");
    }
    narrative(&mut html, brief.narrative.as_ref(), |n| {
        n.decisions.as_deref()
    });
    html.push_str("</section><section><h2>Unfinished work</h2>");
    if brief.unfinished.is_empty() {
        html.push_str("<p class=\"muted\">All Orders integrated.</p>");
    } else {
        html.push_str("<div class=\"grid\">");
        for package in &brief.unfinished {
            let _ = write!(
                html,
                "<article class=\"card\"><small>{:?} · {}</small><h3>{}</h3><p>{}</p>",
                package.status,
                escape(&package.id),
                escape(&package.title),
                escape(&package.goal)
            );
            if let Some(reason) = &package.reason {
                let _ = write!(html, "<p>{}</p>", escape(reason));
            }
            html.push_str("</article>");
        }
        html.push_str("</div>");
    }
    narrative(&mut html, brief.narrative.as_ref(), |n| {
        n.next_steps.as_deref()
    });
    html.push_str("</section>");
    if !brief.cancelled.is_empty() {
        html.push_str("<section><h2>Cancelled Orders</h2><ul class=\"list\">");
        for package in &brief.cancelled {
            let _ = write!(
                html,
                "<li>{} ({})</li>",
                escape(&package.title),
                escape(&package.id)
            );
        }
        html.push_str("</ul></section>");
    }
    if !brief.visuals.is_empty() {
        html.push_str("<section><h2>Visual evidence</h2>");
        for visual in &brief.visuals {
            let _ = write!(
                html,
                "<figure><img loading=\"lazy\" src=\"{}\" alt=\"{}\"><figcaption>{}</figcaption></figure>",
                escape(&visual.file),
                escape(&visual.caption),
                escape(&visual.caption)
            );
        }
        html.push_str("</section>");
    }
    html.push_str("<details><summary>Technical evidence</summary>");
    for package in &brief.integrated {
        let _ = write!(
            html,
            "<h3>{} ({})</h3>",
            escape(&package.title),
            escape(&package.id)
        );
        if let Some(result) = &package.result {
            let _ = write!(
                html,
                "<p>Run <code>{}</code> · captured {}</p>",
                result.run_id,
                escape(&result.captured_at.to_rfc3339())
            );
            html.push_str("<ul class=\"list\">");
            for file in &result.changed_files {
                let _ = write!(
                    html,
                    "<li><code>{}</code>{}</li>",
                    escape(&file.path),
                    if file.binary { " (binary)" } else { "" }
                );
            }
            html.push_str("</ul>");
            if !result.changes_complete {
                html.push_str("<p>Changed-file list truncated.</p>");
            }
            for outcome in result
                .verification
                .iter()
                .chain(result.reviews.iter())
                .chain(result.decision.iter())
            {
                let _ = write!(
                    html,
                    "<p><code>{}</code> · {:?}",
                    escape(&outcome.stage_id.to_string()),
                    outcome.status
                );
                if let Some(line) = &outcome.bottom_line {
                    let _ = write!(html, "<br>{}", escape(line));
                }
                html.push_str("</p>");
            }
            if let Some(open) = &result.open_questions {
                let _ = write!(html, "<p>Follow-ups: {}</p>", escape(open));
            }
        }
    }
    html.push_str("</details><footer>Snapshot of committed mission evidence. Consul narrative, when present, is labelled separately and may predate this snapshot. Local file; no network resources.</footer></main></body></html>");
    html
}

fn stage_status(stage: Option<&crate::domain::StageOutcome>) -> String {
    stage.map_or_else(
        || "not recorded".to_owned(),
        |stage| format!("{:?}", stage.status),
    )
}

fn status(status: MissionStatus) -> &'static str {
    match status {
        MissionStatus::Planning => "Planning",
        MissionStatus::Active => "Active",
        MissionStatus::Completed => "Completed",
        MissionStatus::Cancelled => "Cancelled",
    }
}

fn narrative(
    html: &mut String,
    source: Option<&Narrative>,
    field: impl Fn(&Narrative) -> Option<&str>,
) {
    if let Some(narrative) = source
        && let Some(text) = field(narrative)
    {
        let _ = write!(
            html,
            "<aside class=\"narrative\"><b>Consul narrative · {}</b>{}</aside>",
            escape(&narrative.source),
            escape(text)
        );
    }
}

fn escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone as _, Utc};

    use super::*;
    use crate::domain::{
        ChangedFile, DecisionAuthor, DecisionId, MissionAttention, MissionId, Role, RunId, StageId,
        StageOutcome, StageStatus, WorkPackageId, WorkflowKind,
    };
    use crate::store::MissionRevision;

    fn fixture() -> MissionDetails {
        let at = Utc.with_ymd_and_hms(2026, 9, 29, 12, 0, 0).unwrap();
        let package = |name: &str, status, result| WorkPackageSummary {
            id: WorkPackageId::new(name).unwrap(),
            title: format!("<script>{name}</script>"),
            goal: format!("deliver {name}"),
            rationale: String::new(),
            scope: String::new(),
            acceptance_criteria: Vec::new(),
            verification: String::new(),
            workflow: WorkflowKind::Standard,
            status,
            dependencies: Vec::new(),
            runs: Vec::new(),
            current_run: None,
            run_status: None,
            reason: None,
            result,
            handoff: None,
            created_at: at,
            updated_at: at,
        };
        let result = WorkPackageResult {
            run_id: RunId::from_u128(2),
            captured_at: at,
            stage_count: 3,
            changed_files: vec![ChangedFile {
                path: "src/<unsafe>.rs".to_owned(),
                binary: false,
            }],
            changes_complete: true,
            bottom_line: Some("Done <b>now</b>".to_owned()),
            verification: Some(StageOutcome {
                stage_id: StageId::new("verify").unwrap(),
                role: Role::Verifier,
                status: StageStatus::Completed,
                bottom_line: None,
            }),
            reviews: Vec::new(),
            decision: None,
            open_questions: None,
        };
        MissionDetails {
            id: MissionId::from_u128(1),
            title: "<img src=x onerror=alert(1)>".to_owned(),
            goal: "Make something useful".to_owned(),
            repository: PathBuf::from("/repo"),
            base_commit: "abc".to_owned(),
            status: MissionStatus::Active,
            packages: vec![
                package("core", WorkPackageStatus::Integrated, Some(result)),
                package("next", WorkPackageStatus::Ready, None),
                package("old", WorkPackageStatus::Cancelled, None),
            ],
            decisions: vec![DecisionSummary {
                id: DecisionId::from_u128(3),
                title: "Ship <safe>".to_owned(),
                rationale: "Because & now".to_owned(),
                author: DecisionAuthor::User,
                recorded_at: at,
            }],
            attention: MissionAttention::default(),
            lead: None,
            revision: MissionRevision::initial(),
            created_at: at,
            updated_at: at,
        }
    }

    #[test]
    fn only_integrated_work_counts_and_untrusted_text_is_escaped() {
        let brief = ProgressBrief::new(&fixture(), None, &[]);
        assert_eq!(brief.integrated.len(), 1);
        assert_eq!(brief.unfinished.len(), 1);
        assert_eq!(brief.cancelled.len(), 1);
        let html = render(&brief);
        assert!(!html.contains("<script>"));
        assert!(!html.contains("<img src=x"));
        assert!(html.contains("&lt;script&gt;core&lt;/script&gt;"));
        assert!(html.contains("src/&lt;unsafe&gt;.rs"));
        assert!(html.contains("Because &amp; now"));
        assert!(html.contains("verification: Completed; 0 review(s) recorded"));
        assert!(html.contains("Technical evidence"));
    }

    #[test]
    fn identical_evidence_reuses_immutable_path_and_visuals_stay_local() {
        let temp = tempfile::TempDir::new().unwrap();
        let image = temp.path().join("proof.png");
        fs::write(&image, b"\x89PNG\r\n\x1a\nsmall fixture").unwrap();
        let details = fixture();
        let first = write(temp.path(), &details, None, std::slice::from_ref(&image)).unwrap();
        let second = write(temp.path(), &details, None, &[image]).unwrap();
        assert_eq!(first, second);
        assert!(first.exists());
        assert!(fs::read_to_string(&first).unwrap().contains("assets/"));
        assert!(first.parent().unwrap().join("brief.json").exists());
        let visual = temp.path().join("not-image.svg");
        fs::write(&visual, b"<svg onload=alert(1)></svg>").unwrap();
        assert!(matches!(
            write(temp.path(), &details, None, &[visual]),
            Err(BriefError::UnsupportedVisual(_))
        ));
    }

    #[test]
    fn lead_words_remain_labelled_narrative_and_never_change_facts() {
        let answer = LeadAnswer {
            run_id: RunId::from_u128(4),
            stage_id: StageId::new("lead_1").unwrap(),
            turn: 1,
            text: "## Progress\n\nWe are finished.\n\n## Next steps\n\nShip soon.\n\n## Plan changes\n\n- none\n".to_owned(),
            bottom_line: None,
            proposals: Ok(Vec::new()),
        };
        let brief = ProgressBrief::new(&fixture(), Some(&answer), &[]);
        assert_eq!(brief.integrated.len(), 1);
        assert_eq!(brief.unfinished.len(), 1);
        let html = render(&brief);
        assert!(html.contains("Consul narrative"));
        assert!(html.contains("We are finished."));
        assert!(!html.contains("Plan changes"));
    }
}
