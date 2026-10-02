//! Read-only Jira input boundary. Credentials never enter run input or storage.
use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;

use crate::app::{MissionService, NewWorkPackage};
use crate::cli::JiraCommand;
use crate::domain::{WorkPackageContract, WorkPackageId, WorkflowKind};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Edition {
    Cloud,
    DataCenter,
}

pub(crate) struct Client {
    base: Url,
    edition: Edition,
    authorization: String,
    acceptance_field: Option<String>,
    agent: ureq::Agent,
}

impl Client {
    pub(crate) fn from_environment() -> Result<Self> {
        let edition = match required_env("SENATE_JIRA_KIND")?.as_str() {
            "cloud" => Edition::Cloud,
            "data-center" => Edition::DataCenter,
            _ => bail!("SENATE_JIRA_KIND must be cloud or data-center"),
        };
        let token = required_env("SENATE_JIRA_TOKEN")?;
        let authorization = if edition == Edition::Cloud {
            let email = required_env("SENATE_JIRA_EMAIL")?;
            if email.contains(':') {
                bail!("SENATE_JIRA_EMAIL must not contain a colon");
            }
            format!("Basic {}", STANDARD.encode(format!("{email}:{token}")))
        } else {
            format!("Bearer {token}")
        };
        let base = parse_base(&required_env("SENATE_JIRA_URL")?)?;
        let acceptance_field = std::env::var("SENATE_JIRA_ACCEPTANCE_FIELD").ok();
        if acceptance_field.as_ref().is_some_and(|field| {
            !field.starts_with("customfield_")
                || !field[12..].bytes().all(|byte| byte.is_ascii_digit())
                || field.len() == 12
        }) {
            bail!("SENATE_JIRA_ACCEPTANCE_FIELD must be customfield_<number>");
        }
        Ok(Self {
            base,
            edition,
            authorization,
            acceptance_field,
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(30)))
                .max_redirects(0)
                .build()
                .into(),
        })
    }

    fn version(&self) -> &'static str {
        match self.edition {
            Edition::Cloud => "3",
            Edition::DataCenter => "2",
        }
    }

    fn fields(&self) -> Vec<&str> {
        let mut fields = vec!["summary", "description", "updated"];
        if let Some(field) = &self.acceptance_field {
            fields.push(field);
        }
        fields
    }

    fn read(&self, endpoint: &str, body: Option<&Value>) -> Result<Value> {
        let url = format!("{}rest/api/{}/{endpoint}", self.base, self.version());
        let response = if let Some(body) = body {
            self.agent
                .post(&url)
                .header("Authorization", &self.authorization)
                .header("Accept", "application/json")
                .send_json(body)
        } else {
            self.agent
                .get(&url)
                .header("Authorization", &self.authorization)
                .header("Accept", "application/json")
                .call()
        };
        // Server bodies and transport errors can contain secrets. Only report
        // status/classification; never copy their contents into diagnostics.
        let mut response = response.map_err(|error| match error {
            ureq::Error::StatusCode(code) => anyhow::anyhow!(
                "Jira returned HTTP {code}; check authentication, permissions, query and instance URL"
            ),
            _ => anyhow::anyhow!("Jira request failed; check network, TLS and instance URL"),
        })?;
        if response.status().is_redirection() {
            bail!("Jira redirected the request; configure its final instance URL");
        }
        let bytes = response
            .body_mut()
            .with_config()
            .limit(8 * 1024 * 1024)
            .read_to_vec()
            .map_err(|_| anyhow::anyhow!("Jira response unreadable or exceeds 8 MiB"))?;
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("Jira returned invalid JSON"))
    }

    pub(crate) fn issue(&self, reference: &str) -> Result<Issue> {
        let key = issue_key(&self.base, reference)?;
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query.append_pair("fields", &self.fields().join(","));
        let value = self.read(&format!("issue/{key}?{}", query.finish()), None)?;
        let issue = self.normalize(value)?;
        if issue.key != key {
            bail!("Jira returned a different issue key; use its current key explicitly");
        }
        Ok(issue)
    }

    fn normalize(&self, value: Value) -> Result<Issue> {
        let raw: RawIssue = serde_json::from_value(value)
            .map_err(|_| anyhow::anyhow!("Jira issue lacks required fields"))?;
        validate_key(&raw.key)?;
        let summary = raw
            .fields
            .get("summary")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .context("Jira issue has no summary")?
            .to_owned();
        let description = document_text(raw.fields.get("description").unwrap_or(&Value::Null));
        let acceptance = self
            .acceptance_field
            .as_ref()
            .map_or_else(String::new, |field| {
                document_text(raw.fields.get(field).unwrap_or(&Value::Null))
            });
        let updated = raw
            .fields
            .get("updated")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        Ok(Issue {
            url: format!("{}browse/{}", self.base, raw.key),
            key: raw.key,
            summary,
            description,
            acceptance,
            updated,
        })
    }

    fn search(&self, jql: &str, limit: usize) -> Result<Vec<Issue>> {
        if jql.trim().is_empty() || !(1..=1000).contains(&limit) {
            bail!("JQL must be nonempty and --limit must be between 1 and 1000");
        }
        let mut issues = Vec::new();
        let mut keys = HashSet::new();
        let mut tokens = HashSet::new();
        let mut token: Option<String> = None;
        let mut expected_total = None;
        loop {
            let mut body = json!({"jql": jql, "fields": self.fields(), "maxResults": 50});
            let endpoint = if self.edition == Edition::Cloud {
                if let Some(token) = &token {
                    body["nextPageToken"] = json!(token);
                }
                "search/jql"
            } else {
                body["startAt"] = json!(issues.len());
                "search"
            };
            let page = self.read(endpoint, Some(&body))?;
            let rows = page
                .get("issues")
                .and_then(Value::as_array)
                .context("Jira search lacks issues array")?;
            for row in rows {
                let issue = self.normalize(row.clone())?;
                if !keys.insert(issue.key.clone()) {
                    bail!("Jira pagination repeated an issue; retry search");
                }
                issues.push(issue);
                if issues.len() > limit {
                    bail!("Jira search exceeds --limit {limit}; narrow JQL or raise limit");
                }
            }
            if self.edition == Edition::Cloud {
                if page.get("isLast").and_then(Value::as_bool) == Some(true) {
                    break;
                }
                token = page
                    .get("nextPageToken")
                    .and_then(Value::as_str)
                    .filter(|token| !token.is_empty())
                    .map(str::to_owned);
                let Some(next) = &token else {
                    bail!("Jira Cloud search lacks final-page or continuation evidence");
                };
                if rows.is_empty() || !tokens.insert(next.clone()) {
                    bail!("Jira pagination made no progress");
                }
            } else {
                let total = page
                    .get("total")
                    .and_then(Value::as_u64)
                    .context("Jira search lacks total")?;
                let start = page
                    .get("startAt")
                    .and_then(Value::as_u64)
                    .context("Jira search lacks startAt")?;
                if total > limit as u64 {
                    bail!("Jira search exceeds --limit {limit}; narrow JQL or raise limit");
                }
                if expected_total.is_some_and(|expected| expected != total)
                    || (issues.len() as u64) > total
                {
                    bail!("Jira search total changed or contradicts returned issues; retry search");
                }
                expected_total = Some(total);
                if start != (issues.len() - rows.len()) as u64 {
                    bail!("Jira search returned an unexpected offset");
                }
                if issues.len() as u64 >= total {
                    break;
                }
                if rows.is_empty() {
                    bail!("Jira pagination made no progress");
                }
            }
        }
        Ok(issues)
    }
}

fn required_env(name: &str) -> Result<String> {
    let value =
        std::env::var(name).map_err(|_| anyhow::anyhow!("set {name} for Jira integration"))?;
    if value.trim().is_empty() || value.chars().any(char::is_control) {
        bail!("{name} must be nonempty and contain no control characters");
    }
    Ok(value)
}

fn parse_base(value: &str) -> Result<Url> {
    let mut base = Url::parse(value)
        .map_err(|_| anyhow::anyhow!("SENATE_JIRA_URL must be an absolute HTTPS URL"))?;
    if base.scheme() != "https"
        || base.host_str().is_none()
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
    {
        bail!("SENATE_JIRA_URL requires HTTPS without credentials, query or fragment");
    }
    base.set_path(&format!("{}/", base.path().trim_end_matches('/')));
    Ok(base)
}

fn validate_key(key: &str) -> Result<()> {
    let (project, number) = key
        .rsplit_once('-')
        .context("Jira issue must be a key like PROJ-123")?;
    if project.is_empty()
        || !project
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        || number.is_empty()
        || !number.bytes().all(|byte| byte.is_ascii_digit())
    {
        bail!("Jira issue must be a key like PROJ-123");
    }
    Ok(())
}

fn issue_key(base: &Url, reference: &str) -> Result<String> {
    let key = if reference.contains("://") {
        let url = Url::parse(reference).context("invalid Jira issue URL")?;
        let prefix = format!("{}browse/", base.path());
        if url.origin() != base.origin()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            bail!("Jira issue URL must belong to the configured instance");
        }
        url.path()
            .strip_prefix(&prefix)
            .context("expected Jira browse URL")?
            .to_owned()
    } else {
        reference.to_owned()
    };
    validate_key(&key)?;
    Ok(key)
}

#[derive(Deserialize)]
struct RawIssue {
    key: String,
    fields: Value,
}

pub(crate) struct Issue {
    key: String,
    url: String,
    summary: String,
    description: String,
    acceptance: String,
    updated: String,
}

impl Issue {
    pub(crate) fn input(&self) -> String {
        format!(
            "Jira issue: {}\nSource: {}\nUpdated: {}\n\n# {}\n\n{}\n\n## Acceptance criteria\n{}",
            self.key,
            self.url,
            self.updated,
            self.summary,
            self.description,
            if self.acceptance.is_empty() {
                "See issue description; no separate acceptance field supplied."
            } else {
                &self.acceptance
            }
        )
    }

    fn package(&self, workflow: WorkflowKind) -> Result<NewWorkPackage> {
        Ok(NewWorkPackage {
            id: WorkPackageId::new(&self.key)?,
            contract: WorkPackageContract {
                title: format!("{}: {}", self.key, self.summary),
                goal: self.input(),
                rationale: format!("Imported from {}", self.url),
                scope: String::new(),
                acceptance_criteria: if self.acceptance.is_empty() {
                    vec![]
                } else {
                    vec![self.acceptance.clone()]
                },
                verification: String::new(),
                workflow,
            },
            dependencies: vec![],
        })
    }
}

// Keep rich Jira Cloud content as its original ADF JSON alongside readable
// text. Unsupported nodes, marks, links and media references cannot disappear.
fn document_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        _ => {
            let mut text = String::new();
            adf_text(value, &mut text);
            format!(
                "{}\n\nOriginal Jira content (JSON):\n{}",
                text.trim(),
                value
            )
        }
    }
}

fn adf_text(value: &Value, text: &mut String) {
    if let Some(word) = value.get("text").and_then(Value::as_str) {
        text.push_str(word);
    }
    if value.get("type").and_then(Value::as_str) == Some("hardBreak") {
        text.push('\n');
    }
    if let Some(children) = value.get("content").and_then(Value::as_array) {
        for child in children {
            adf_text(child, text);
        }
    }
    if matches!(
        value.get("type").and_then(Value::as_str),
        Some("paragraph" | "heading" | "listItem" | "codeBlock" | "tableRow")
    ) {
        text.push('\n');
    }
}

pub(crate) fn execute(command: &JiraCommand) -> Result<()> {
    let client = Client::from_environment()?;
    match command {
        JiraCommand::Show { issue } => println!("{}", client.issue(issue)?.input()),
        JiraCommand::Search { jql, limit } => {
            for issue in client.search(jql, *limit)? {
                println!("{}\t{}\t{}", issue.key, issue.summary, issue.url);
            }
        }
        JiraCommand::Mission {
            title,
            goal,
            jql,
            repo,
            limit,
            workflow,
        } => {
            let workflow = match workflow.as_str() {
                "fast" => WorkflowKind::Fast,
                "deep" => WorkflowKind::Deep,
                "review" => WorkflowKind::Review,
                _ => WorkflowKind::Standard,
            };
            let issues = client.search(jql, *limit)?;
            if issues.is_empty() {
                bail!("Jira search returned no issues; no mission created");
            }
            let packages = issues
                .iter()
                .map(|issue| issue.package(workflow))
                .collect::<Result<Vec<_>>>()?;
            let details = MissionService::from_environment()?
                .create_mission_with_packages(title, goal, repo, packages)?;
            println!(
                "Mission {} created with {} Jira work packages. Inspect with `senate mission show {}`; set dependencies before starting packages.",
                details.id,
                issues.len(),
                details.id
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::thread;

    fn fixture(
        edition: Edition,
        responses: Vec<Value>,
    ) -> (Client, thread::JoinHandle<Vec<String>>) {
        fixture_status(edition, responses, "200 OK", "")
    }

    fn fixture_status(
        edition: Edition,
        responses: Vec<Value>,
        status: &'static str,
        extra_headers: &'static str,
    ) -> (Client, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || {
            let mut requests = Vec::new();
            for body in responses {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                loop {
                    let count = socket.read(&mut buffer).unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&buffer[..count]);
                    if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]).to_lowercase();
                        let length = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length: "))
                            .map_or(0, |length| length.parse::<usize>().unwrap());
                        if headers.contains("transfer-encoding: chunked") {
                            if request.ends_with(b"\r\n0\r\n\r\n") {
                                let mut offset = end + 4;
                                let mut body = Vec::new();
                                loop {
                                    let size_end = request[offset..]
                                        .windows(2)
                                        .position(|bytes| bytes == b"\r\n")
                                        .unwrap()
                                        + offset;
                                    let size = usize::from_str_radix(
                                        std::str::from_utf8(&request[offset..size_end]).unwrap(),
                                        16,
                                    )
                                    .unwrap();
                                    if size == 0 {
                                        break;
                                    }
                                    offset = size_end + 2;
                                    body.extend_from_slice(&request[offset..offset + size]);
                                    offset += size + 2;
                                }
                                request.truncate(end + 4);
                                request.extend(body);
                                break;
                            }
                        } else if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                requests.push(String::from_utf8(request).unwrap());
                let body = body.to_string();
                write!(socket, "HTTP/1.1 {status}\r\n{extra_headers}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
            requests
        });
        (
            Client {
                base: Url::parse(&format!("http://{address}/jira/")).unwrap(),
                edition,
                authorization: if edition == Edition::Cloud {
                    "Basic TEST"
                } else {
                    "Bearer TEST"
                }
                .to_owned(),
                acceptance_field: Some("customfield_10001".to_owned()),
                agent: ureq::Agent::config_builder()
                    .timeout_global(Some(Duration::from_secs(5)))
                    .max_redirects(0)
                    .build()
                    .into(),
            },
            worker,
        )
    }

    fn raw(key: &str) -> Value {
        json!({"key": key, "fields": {"summary": "Fix café", "description": "Keep Unicode\nand lines", "updated": "2026-09-29", "customfield_10001": "No regression"}})
    }

    #[test]
    fn cloud_pages_and_credentials_use_v3() {
        let (client, worker) = fixture(
            Edition::Cloud,
            vec![
                json!({"issues": [raw("APP-1")], "isLast": false, "nextPageToken": "next"}),
                json!({"issues": [raw("APP-2")], "isLast": true}),
            ],
        );
        let issues = client.search("project = APP", 2).unwrap();
        assert_eq!(issues.len(), 2);
        let snapshot = issues[0].input();
        assert!(snapshot.contains("No regression"));
        assert!(snapshot.contains("Keep Unicode\nand lines"));
        assert!(!snapshot.contains("TEST"));
        let requests = worker.join().unwrap();
        assert!(requests[0].starts_with("POST /jira/rest/api/3/search/jql "));
        assert!(
            requests[0]
                .to_lowercase()
                .contains("authorization: basic test")
        );
        let body: Value =
            serde_json::from_str(requests[1].split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["nextPageToken"], "next");
    }

    #[test]
    fn data_center_pages_use_offsets_and_pat() {
        let (client, worker) = fixture(
            Edition::DataCenter,
            vec![
                json!({"issues": [raw("APP-1")], "startAt": 0, "total": 2}),
                json!({"issues": [raw("APP-2")], "startAt": 1, "total": 2}),
            ],
        );
        assert_eq!(client.search("project = APP", 2).unwrap().len(), 2);
        let requests = worker.join().unwrap();
        assert!(requests[0].starts_with("POST /jira/rest/api/2/search "));
        assert!(
            requests[0]
                .to_lowercase()
                .contains("authorization: bearer test")
        );
        let body: Value =
            serde_json::from_str(requests[1].split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["startAt"], 1);
    }

    #[test]
    fn issue_import_uses_selected_fields_and_preserves_source() {
        let (client, worker) = fixture(Edition::DataCenter, vec![raw("APP-1")]);
        let reference = format!("{}browse/APP-1", client.base);
        let issue = client.issue(&reference).unwrap();
        let package = issue.package(WorkflowKind::Standard).unwrap();
        assert_eq!(package.id.as_str(), "APP-1");
        assert!(package.dependencies.is_empty());
        assert_eq!(package.contract.acceptance_criteria, ["No regression"]);
        assert!(package.contract.goal.contains(&reference));
        let requests = worker.join().unwrap();
        assert!(requests[0].starts_with("GET /jira/rest/api/2/issue/APP-1?fields="));
        assert!(requests[0].contains("customfield_10001"));
    }

    #[test]
    fn refuses_partial_and_nonprogressing_searches() {
        for page in [
            json!({"issues": [raw("APP-1"), raw("APP-2")], "isLast": true}),
            json!({"issues": [], "isLast": false, "nextPageToken": "next"}),
            json!({"issues": [raw("APP-1")]}),
            json!({"issues": [raw("APP-1"), raw("APP-1")], "isLast": true}),
        ] {
            let (client, worker) = fixture(Edition::Cloud, vec![page]);
            assert!(client.search("project = APP", 1).is_err());
            worker.join().unwrap();
        }
    }

    #[test]
    fn refuses_wrong_offsets_and_issue_identity() {
        let (client, worker) = fixture(
            Edition::DataCenter,
            vec![
                json!({"issues": [raw("APP-1")], "startAt": 1, "total": 1}),
                raw("APP-2"),
            ],
        );
        assert!(client.search("project = APP", 2).is_err());
        assert!(client.issue("APP-1").is_err());
        worker.join().unwrap();
    }

    #[test]
    fn data_center_search_refuses_changing_or_contradictory_totals() {
        for total in [1, 3] {
            let (client, worker) = fixture(
                Edition::DataCenter,
                vec![
                    json!({"issues": [raw("APP-1")], "startAt": 0, "total": 2}),
                    json!({"issues": [raw("APP-2")], "startAt": 1, "total": total}),
                ],
            );
            assert!(client.search("project = APP", 10).is_err());
            worker.join().unwrap();
        }
        let (client, worker) = fixture(
            Edition::DataCenter,
            vec![json!({"issues": [raw("APP-1")], "startAt": 0, "total": 0})],
        );
        assert!(client.search("project = APP", 10).is_err());
        worker.join().unwrap();
    }

    #[test]
    fn malformed_issue_errors_do_not_echo_server_values() {
        let (client, worker) = fixture(
            Edition::Cloud,
            vec![json!({"key": {"secret-token": true}, "fields": {}})],
        );
        let error = client.issue("APP-1").err().unwrap();
        assert!(!format!("{error:#}").contains("secret-token"));
        worker.join().unwrap();
    }

    #[test]
    fn http_errors_hide_response_bodies_and_redirects_are_refused() {
        for (status, headers) in [
            ("401 Unauthorized", ""),
            ("403 Forbidden", ""),
            ("429 Too Many Requests", ""),
            ("302 Found", "Location: http://127.0.0.1:1/stolen\r\n"),
        ] {
            let (client, worker) = fixture_status(
                Edition::Cloud,
                vec![json!({"error": "secret-token"})],
                status,
                headers,
            );
            let error = client.issue("APP-1").err().unwrap().to_string();
            assert!(!error.contains("secret-token"));
            worker.join().unwrap();
        }
    }

    #[test]
    fn enforces_https_instance_and_browse_boundaries() {
        let base = parse_base("https://jira.example/jira").unwrap();
        assert_eq!(base.as_str(), "https://jira.example/jira/");
        assert_eq!(
            issue_key(&base, "https://jira.example/jira/browse/APP-1").unwrap(),
            "APP-1"
        );
        for url in [
            "http://jira.example",
            "https://secret@jira.example",
            "https://jira.example?token=secret",
        ] {
            assert!(parse_base(url).is_err());
        }
        for reference in [
            "https://evil.example/jira/browse/APP-1",
            "https://jira.example/browse/APP-1",
            "APP-1/../secret",
            "APP-1?token=secret",
        ] {
            assert!(issue_key(&base, reference).is_err());
        }
    }

    #[test]
    fn adf_retains_links_and_unknown_nodes() {
        let document = json!({"type":"doc","version":1,"content":[
            {"type":"paragraph","content":[{"type":"text","text":"Hello café","marks":[{"type":"link","attrs":{"href":"https://example.com"}}]}]},
            {"type":"media","attrs":{"id":"attachment-1"}}
        ]});
        let rendered = document_text(&document);
        assert!(rendered.starts_with("Hello café"));
        assert!(rendered.contains("https://example.com"));
        assert!(rendered.contains("attachment-1"));
        assert_eq!(document_text(&Value::Null), "");
        assert_eq!(document_text(&json!("wiki *markup*")), "wiki *markup*");
    }

    #[test]
    fn jira_run_cli_requires_exactly_one_input() {
        use crate::cli::Cli;
        use clap::Parser as _;
        assert!(Cli::try_parse_from(["senate", "standard", "--jira", "APP-1"]).is_ok());
        assert!(Cli::try_parse_from(["senate", "standard", "task"]).is_ok());
        assert!(Cli::try_parse_from(["senate", "standard"]).is_err());
        assert!(Cli::try_parse_from(["senate", "standard", "task", "--jira", "APP-1"]).is_err());
    }
}
