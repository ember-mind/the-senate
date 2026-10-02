# Jira

Import engineering intent from Jira Cloud or Jira Data Center without granting a coding provider Jira credentials.

## Sub-features
- run-input: `--jira` replaces positional task text on Fast, Standard, Deep and Review. The issue is fetched before run creation; normalized title, description, acceptance criteria, source URL and Jira update timestamp become the existing immutable Run input. Resume and recovery never fetch Jira again.
- show: print the same normalized input without creating a run.
- search: JQL search follows Cloud continuation tokens or Data Center offsets. Empty queries, duplicate issues, stalled pagination, changing or contradictory Data Center totals, malformed responses and results exceeding the explicit limit fail instead of returning an incomplete selection.
- mission-import: create a Mission and one work package per search result in a single atomic commit, after fetching all results. Package IDs are Jira keys; contracts contain the captured issue input. Empty searches create nothing. Imported packages have no dependencies and no runs; inspect the plan and set dependencies explicitly before starting work.
- editions: Cloud uses REST v3 issue and enhanced `search/jql` APIs with email/API-token Basic authentication. Data Center uses REST v2 issue/search APIs with a personal access token as Bearer authentication. Data Center instances must support those APIs and PAT authentication; version-specific behavior has not been verified against a live server.
- content: Data Center text remains verbatim. Cloud ADF includes readable text plus original JSON, retaining links, marks and unsupported nodes. Acceptance criteria in the description remain there; a separate custom field is optional.
- read-only: there are no Jira comments, transitions, attachment downloads or live synchronization in this version. Re-importing creates a new Run or Mission; it does not update an existing one.

## How to get to it (user POV)
Set instance and credentials in the parent environment. Cloud: `SENATE_JIRA_KIND=cloud`, `SENATE_JIRA_URL=https://your-site.atlassian.net`, `SENATE_JIRA_EMAIL=you@example.com`, and `SENATE_JIRA_TOKEN` containing an API token usable against the site's REST API. Data Center: `SENATE_JIRA_KIND=data-center`, `SENATE_JIRA_URL=https://jira.example.com/jira`, and `SENATE_JIRA_TOKEN` containing a PAT. Base URLs may include a context path. Scoped Cloud tokens requiring an Atlassian API gateway URL are not supported in this version.

Optionally set `SENATE_JIRA_ACCEPTANCE_FIELD=customfield_10001` to import a separate acceptance-criteria field; field IDs vary by instance. Credentials are read from environment only, never from repository configuration or command arguments. Jira configuration does not cross the managed-provider environment handoff; native CLI probes, image generation, Git/GitHub, setup, verification, Evaluation validation, browser/clipboard launches and installer version probes also omit importer credentials.

## Driving it
```bash
senate jira show PROJ-123
senate jira show https://jira.example.com/jira/browse/PROJ-123
senate jira search "project = PROJ ORDER BY key ASC" --limit 100
senate standard --jira PROJ-123 --repo <path> --provider codex
senate fast --jira PROJ-123 --repo <path> --provider claude
senate deep --jira PROJ-123 --repo <path> --profile recommended
senate review --jira PROJ-123 --repo <path> --provider codex
senate jira mission "Release fixes" --goal "Deliver selected fixes" --jql "project = PROJ AND fixVersion = '1.0' ORDER BY key ASC" --repo <path> --limit 100 --workflow standard
```

## Where it lives
- `src/jira.rs` — edition/authentication selection, instance and issue validation, bounded HTTP reads, pagination, content normalization, CLI dispatch and protocol fixture tests.
- `src/exec.rs`, `src/process/environment.rs` — exclude importer credentials from child commands and managed environment handoff.
- `src/cli/mod.rs` — `JiraCommand`, mutually exclusive `RunArgs.task` and `RunArgs.jira`.
- `src/cli/commands.rs` — resolve Jira issue input before calling the existing Run service.
- `src/app/mission_service.rs` — `create_mission_with_packages`, validates packages before atomically storing Mission, input and events.
- `src/store/run_input.rs` — existing immutable Run input storage.

## Gotchas
- HTTPS is required; TLS verification stays enabled. Redirects are refused. Configure the final instance URL, including its context path.
- A browse URL must match the configured instance origin and context path, without credentials, query or fragment. Jira keys must use uppercase project identifiers and a numeric issue number. An issue renamed by Jira must be imported using its current key.
- Each request has a 30-second timeout and an 8 MiB response ceiling. Search accepts limits from 1 through 1000, defaults to 100, and uses pages of 50; exceeding the limit fails rather than silently truncating. HTTP 429 is reported without automatic retries.
- Authentication/network errors omit server response bodies and transport details; malformed issue diagnostics also omit field values to avoid leaking credentials into diagnostics.
- Mission import captures only issues visible to the configured account. Jira links do not automatically become work-package dependencies, because link names/directions are instance-specific.
- A Run still requires a clean source checkout and its normal provider/setup preconditions. A successful Jira fetch does not bypass them.

## API references
- [Cloud enhanced search](https://developer.atlassian.com/cloud/jira/platform/rest/v3/api-group-issue-search/)
- [Data Center search](https://developer.atlassian.com/server/jira/platform/rest/v10004/api-group-search/)
- [Data Center PAT authentication](https://developer.atlassian.com/server/jira/platform/personal-access-token/)
