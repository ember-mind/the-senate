use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::domain::{
    ConfigSnapshotId, EffortSetting, ProviderId, Role, RunId, StageRouteOverride,
    WorkflowDefinition,
};
use crate::engine::{
    FakeProvider, FakeScenario, Provider, ProviderAttentionContext, ProviderError, ProviderPoll,
    ProviderRequest,
};
use crate::image::{
    CodexImageGenerator, FakeImageGenerator, ImageGenerator, ImageToolHost, ImageToolService,
};
use crate::providers::claude::{ClaudeInstallation, ClaudeProvider, ClaudeProviderError};
use crate::providers::codex::{CodexInstallation, CodexProvider, CodexProviderError};
use crate::providers::verify::VerifyProvider;
use crate::store::{ResolvedConfigSnapshot, SequencedEvent, SqliteStore};

use super::AppError;
use super::routing::{
    EffortRequest, ExecutionSelection, ExecutionTarget, ImageGenerationPlan,
    RecommendedAvailability, ResourcePlan, RoutingPlan, UniformProvider, VERIFY_PROVIDER_ID,
    resolve_config, resolve_config_with_image,
};

pub trait ProviderFactory {
    type Provider: Provider;

    /// Resolves immutable execution configuration for a new run.
    ///
    /// # Errors
    /// Rejects unavailable providers, failed probes, or invalid routing.
    fn config_for_new_run(
        &self,
        selection: ExecutionSelection,
        effort: EffortRequest,
        workflow: &WorkflowDefinition,
        id: ConfigSnapshotId,
        created_at: DateTime<Utc>,
    ) -> Result<ResolvedConfigSnapshot, AppError>;

    /// [`Self::config_for_new_run`] with an image-generation grant. A factory
    /// that does not host the image tool refuses an enabled grant here, before
    /// anything is persisted, instead of sealing an authorization it cannot
    /// honor.
    ///
    /// # Errors
    /// Rejects an enabled grant unless the factory overrides this.
    fn config_for_new_run_with_image(
        &self,
        selection: ExecutionSelection,
        effort: EffortRequest,
        image: &ImageGenerationPlan,
        workflow: &WorkflowDefinition,
        id: ConfigSnapshotId,
        created_at: DateTime<Utc>,
    ) -> Result<ResolvedConfigSnapshot, AppError> {
        if image.is_enabled() {
            return Err(AppError::ImageGenerationUnsupported(
                "this provider factory does not host the image tool".to_owned(),
            ));
        }
        self.config_for_new_run(selection, effort, workflow, id, created_at)
    }

    /// Reconstructs provider behavior from durable configuration and events.
    ///
    /// # Errors
    /// Rejects unsupported or structurally invalid configuration.
    fn for_run(
        &self,
        run_id: RunId,
        config: &ResolvedConfigSnapshot,
        workflow: &WorkflowDefinition,
        events: &[SequencedEvent],
    ) -> Result<Self::Provider, AppError>;

    /// Confirms one provider can take a stage right now, before a retry is
    /// committed to it. Same bar as `--provider` at creation: installed and
    /// authenticated, or refused before any state changes.
    ///
    /// # Errors
    /// Rejects providers this factory cannot run or that are not ready.
    fn require_provider(&self, provider: UniformProvider) -> Result<(), AppError>;
}

pub trait ProviderResolver {
    type Provider: Provider;

    /// See [`ProviderFactory::require_provider`].
    ///
    /// # Errors
    /// Rejects providers this resolver cannot run or that are not ready.
    fn require_provider(&self, provider: UniformProvider) -> Result<(), AppError>;

    /// Reconstructs provider behavior from one validated immutable configuration.
    ///
    /// # Errors
    /// Rejects invalid configuration or unavailable routed provider infrastructure.
    fn resolve_for_run(
        &self,
        run_id: RunId,
        config: &ResolvedConfigSnapshot,
        workflow: &WorkflowDefinition,
        events: &[SequencedEvent],
    ) -> Result<Self::Provider, AppError>;
}

impl<T: ProviderFactory> ProviderResolver for T {
    type Provider = T::Provider;

    fn resolve_for_run(
        &self,
        run_id: RunId,
        config: &ResolvedConfigSnapshot,
        workflow: &WorkflowDefinition,
        events: &[SequencedEvent],
    ) -> Result<Self::Provider, AppError> {
        ProviderFactory::for_run(self, run_id, config, workflow, events)
    }

    fn require_provider(&self, provider: UniformProvider) -> Result<(), AppError> {
        ProviderFactory::require_provider(self, provider)
    }
}

/// Fakes every agent role and runs verification for real, writing its
/// artifacts under an explicit root rather than the developer's data
/// directory — every in-process test would otherwise leave a `verify.md`
/// in `~/.senate/runs`.
#[derive(Clone)]
pub struct DevelopmentFakeProviderFactory {
    artifact_root: PathBuf,
    /// Backend the image tool uses when a run is authorized; the
    /// deterministic fake unless a test injects its own.
    image_generator: Arc<dyn ImageGenerator>,
}

impl std::fmt::Debug for DevelopmentFakeProviderFactory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DevelopmentFakeProviderFactory")
            .field("artifact_root", &self.artifact_root)
            .field("image_backend", &self.image_generator.backend())
            .finish()
    }
}

impl DevelopmentFakeProviderFactory {
    #[must_use]
    pub fn new(artifact_root: PathBuf) -> Self {
        Self {
            artifact_root,
            image_generator: Arc::new(FakeImageGenerator::new()),
        }
    }

    /// The same factory with an explicit image backend, so a test can count
    /// or fail vendor calls.
    #[must_use]
    pub fn with_image_generator(mut self, generator: Arc<dyn ImageGenerator>) -> Self {
        self.image_generator = generator;
        self
    }
}

impl ProviderFactory for DevelopmentFakeProviderFactory {
    /// Routed rather than a bare `FakeProvider`: every agent role is faked,
    /// but the verifier is never an agent, so the run's verify stages go
    /// to the real `verify` provider here exactly as they do in production.
    type Provider = RoutedProvider;

    fn config_for_new_run(
        &self,
        selection: ExecutionSelection,
        effort: EffortRequest,
        workflow: &WorkflowDefinition,
        id: ConfigSnapshotId,
        created_at: DateTime<Utc>,
    ) -> Result<ResolvedConfigSnapshot, AppError> {
        if selection != ExecutionSelection::Uniform(UniformProvider::Fake) {
            return Err(AppError::UnsupportedProvider(format!("{selection:?}")));
        }
        Ok(resolve_config(
            selection,
            effort,
            workflow,
            RecommendedAvailability::default(),
            id,
            created_at,
        )?)
    }

    fn config_for_new_run_with_image(
        &self,
        selection: ExecutionSelection,
        effort: EffortRequest,
        image: &ImageGenerationPlan,
        workflow: &WorkflowDefinition,
        id: ConfigSnapshotId,
        created_at: DateTime<Utc>,
    ) -> Result<ResolvedConfigSnapshot, AppError> {
        if selection != ExecutionSelection::Uniform(UniformProvider::Fake) {
            return Err(AppError::UnsupportedProvider(format!("{selection:?}")));
        }
        Ok(resolve_config_with_image(
            selection,
            effort,
            image,
            workflow,
            RecommendedAvailability::default(),
            id,
            created_at,
        )?)
    }

    fn for_run(
        &self,
        run_id: RunId,
        config: &ResolvedConfigSnapshot,
        workflow: &WorkflowDefinition,
        _events: &[SequencedEvent],
    ) -> Result<Self::Provider, AppError> {
        let plan = RoutingPlan::from_snapshot(config, workflow).map_err(|error| {
            if config.schema_version() == 1 {
                AppError::LegacyExecutionConfig(run_id)
            } else {
                error.into()
            }
        })?;
        if plan
            .routes()
            .any(|(_, route)| route.target().provider_id().as_str() != "fake")
        {
            return Err(AppError::UnsupportedProvider(
                "development factory only supports fake routes".to_owned(),
            ));
        }
        let resource_plan = ResourcePlan::from_snapshot(config, workflow)?;
        let image_plan = ImageGenerationPlan::from_snapshot(config, workflow)?;
        let provider = RoutedProvider::new(plan, resource_plan, workflow.clone())
            .with_artifact_root(self.artifact_root.clone());
        if !image_plan.is_enabled() {
            return Ok(provider);
        }
        let host = start_image_host(
            run_id,
            &image_plan,
            Some(Arc::clone(&self.image_generator)),
            self.artifact_root.clone(),
        )?;
        Ok(provider.with_image_tool(image_plan, host))
    }

    fn require_provider(&self, provider: UniformProvider) -> Result<(), AppError> {
        if provider == UniformProvider::Fake {
            Ok(())
        } else {
            Err(AppError::UnsupportedProvider(provider.as_str().to_owned()))
        }
    }
}

pub enum RuntimeProvider {
    Fake(FakeProvider),
    Claude(ClaudeProvider),
    Codex(CodexProvider),
    Verify(VerifyProvider),
}

impl Provider for RuntimeProvider {
    fn provider_id_for(&self, request: &ProviderRequest) -> Result<ProviderId, ProviderError> {
        match self {
            Self::Fake(provider) => provider.provider_id_for(request),
            Self::Claude(provider) => provider.provider_id_for(request),
            Self::Codex(provider) => provider.provider_id_for(request),
            Self::Verify(provider) => provider.provider_id_for(request),
        }
    }

    fn supports_role(&self, role: crate::domain::Role) -> bool {
        match self {
            Self::Fake(provider) => provider.supports_role(role),
            Self::Claude(provider) => provider.supports_role(role),
            Self::Codex(provider) => provider.supports_role(role),
            Self::Verify(provider) => provider.supports_role(role),
        }
    }

    fn keep_attached_for(&self, request: &ProviderRequest) -> Result<bool, ProviderError> {
        match self {
            Self::Fake(provider) => provider.keep_attached_for(request),
            Self::Claude(provider) => provider.keep_attached_for(request),
            Self::Codex(provider) => provider.keep_attached_for(request),
            Self::Verify(provider) => provider.keep_attached_for(request),
        }
    }

    fn stage_attention_response(
        &mut self,
        store: &mut SqliteStore,
        context: &ProviderAttentionContext,
        response: Option<&str>,
    ) -> Result<(), ProviderError> {
        match self {
            Self::Fake(provider) => provider.stage_attention_response(store, context, response),
            Self::Claude(provider) => provider.stage_attention_response(store, context, response),
            Self::Codex(provider) => provider.stage_attention_response(store, context, response),
            Self::Verify(provider) => provider.stage_attention_response(store, context, response),
        }
    }

    fn can_auto_resolve_attention(
        &mut self,
        store: &mut SqliteStore,
        context: &ProviderAttentionContext,
    ) -> Result<bool, ProviderError> {
        match self {
            Self::Fake(provider) => provider.can_auto_resolve_attention(store, context),
            Self::Claude(provider) => provider.can_auto_resolve_attention(store, context),
            Self::Codex(provider) => provider.can_auto_resolve_attention(store, context),
            Self::Verify(provider) => provider.can_auto_resolve_attention(store, context),
        }
    }

    fn stage_continue_instruction(
        &mut self,
        store: &mut SqliteStore,
        run_id: crate::domain::RunId,
        stage_id: &crate::domain::StageId,
        role: crate::domain::Role,
        instruction: &str,
    ) -> Result<(), ProviderError> {
        match self {
            Self::Fake(provider) => {
                provider.stage_continue_instruction(store, run_id, stage_id, role, instruction)
            }
            Self::Claude(provider) => {
                provider.stage_continue_instruction(store, run_id, stage_id, role, instruction)
            }
            Self::Codex(provider) => {
                provider.stage_continue_instruction(store, run_id, stage_id, role, instruction)
            }
            Self::Verify(provider) => {
                provider.stage_continue_instruction(store, run_id, stage_id, role, instruction)
            }
        }
    }

    fn discard_continue_instruction(
        &mut self,
        store: &mut SqliteStore,
        run_id: crate::domain::RunId,
        stage_id: &crate::domain::StageId,
    ) -> Result<(), ProviderError> {
        match self {
            Self::Fake(provider) => provider.discard_continue_instruction(store, run_id, stage_id),
            Self::Claude(provider) => {
                provider.discard_continue_instruction(store, run_id, stage_id)
            }
            Self::Codex(provider) => provider.discard_continue_instruction(store, run_id, stage_id),
            Self::Verify(provider) => {
                provider.discard_continue_instruction(store, run_id, stage_id)
            }
        }
    }

    fn poll(
        &mut self,
        store: &mut SqliteStore,
        request: &ProviderRequest,
    ) -> Result<ProviderPoll, ProviderError> {
        match self {
            Self::Fake(provider) => provider.poll(store, request),
            Self::Claude(provider) => provider.poll(store, request),
            Self::Codex(provider) => provider.poll(store, request),
            Self::Verify(provider) => provider.poll(store, request),
        }
    }
}

/// Request-aware provider composition with lazy native adapter construction.
pub struct RoutedProvider {
    plan: RoutingPlan,
    resource_plan: ResourcePlan,
    /// Which roles may call the image tool. Disabled unless the snapshot
    /// says otherwise; never consulted for routing or effort.
    image_plan: ImageGenerationPlan,
    /// The live tool host for this process, present exactly when the plan
    /// is enabled and the factory could bind the run's socket.
    image_host: Option<Arc<ImageToolHost>>,
    workflow: WorkflowDefinition,
    /// Keyed by target, effort, and whether the image tool is granted, so a
    /// reviewer routed to the same target as the Implementer never inherits
    /// the Implementer's tool.
    runtimes: HashMap<(ExecutionTarget, EffortSetting, bool), RuntimeProvider>,
    isolated_runtime: Option<(PathBuf, PathBuf)>,
    /// Where the verify provider writes its artifacts when not the
    /// configured data directory: the eval runtime root, or a test fixture.
    artifact_root: Option<PathBuf>,
    eval_auto_approve: bool,
}

impl RoutedProvider {
    #[must_use]
    pub fn new(
        plan: RoutingPlan,
        resource_plan: ResourcePlan,
        workflow: WorkflowDefinition,
    ) -> Self {
        Self {
            plan,
            resource_plan,
            image_plan: ImageGenerationPlan::disabled(),
            image_host: None,
            workflow,
            runtimes: HashMap::new(),
            isolated_runtime: None,
            artifact_root: None,
            eval_auto_approve: false,
        }
    }

    /// The same provider with the image tool granted to `plan`'s roles and
    /// served by `host`.
    #[must_use]
    pub fn with_image_tool(mut self, plan: ImageGenerationPlan, host: Arc<ImageToolHost>) -> Self {
        self.image_plan = plan;
        self.image_host = Some(host);
        self
    }

    /// The immutable image-generation grant this provider runs under.
    #[must_use]
    pub const fn image_plan(&self) -> &ImageGenerationPlan {
        &self.image_plan
    }

    /// The tool host a role's adapter should carry: only when the plan
    /// grants the role and this process hosts the tool.
    fn image_tool_for_role(&self, role: Role) -> Option<Arc<ImageToolHost>> {
        if self.image_plan.allows(role) {
            self.image_host.clone()
        } else {
            None
        }
    }

    /// The same provider with verification artifacts redirected under
    /// `root`, the tree the native adapters would use under the data
    /// directory.
    #[must_use]
    pub fn with_artifact_root(mut self, root: PathBuf) -> Self {
        self.artifact_root = Some(root);
        self
    }

    /// A provider whose *runtime* is redirected: process data root and runner
    /// executable both point somewhere the evaluation harness owns, so an
    /// eval never touches the user's real process data or their installed
    /// CLI. It also auto-approves eval permission prompts, which no caller
    /// should be surprised by after reading the name.
    ///
    /// What it does not isolate, and what the name must not be read as
    /// claiming: the git checkout, the store, or anything else a run touches.
    /// Those are shared with whatever else is running.
    #[must_use]
    pub(crate) fn isolated(
        plan: RoutingPlan,
        resource_plan: ResourcePlan,
        workflow: WorkflowDefinition,
        process_root: PathBuf,
        runner_executable: PathBuf,
    ) -> Self {
        Self {
            plan,
            resource_plan,
            image_plan: ImageGenerationPlan::disabled(),
            image_host: None,
            workflow,
            runtimes: HashMap::new(),
            artifact_root: Some(process_root.clone()),
            isolated_runtime: Some((process_root, runner_executable)),
            eval_auto_approve: true,
        }
    }

    #[must_use]
    pub const fn plan(&self) -> &RoutingPlan {
        &self.plan
    }

    fn target_for_role(&self, role: Role) -> Result<ExecutionTarget, ProviderError> {
        self.plan
            .route(role)
            .map(|route| route.target().clone())
            .ok_or_else(|| ProviderError::new(format!("configured route missing for {role:?}")))
    }

    /// The stage's operator-chosen destination when it has one, else the
    /// role's configured route. The override wins outright, model included:
    /// an operator who sends a stage to Claude without naming a model gets
    /// Claude's native default, not the model the snapshot pinned for Codex.
    fn target_for(
        &self,
        route_override: Option<&StageRouteOverride>,
        role: Role,
    ) -> Result<ExecutionTarget, ProviderError> {
        route_override.map_or_else(
            || self.target_for_role(role),
            |route| {
                Ok(ExecutionTarget::new(
                    route.provider_id().clone(),
                    route.model_id().cloned(),
                ))
            },
        )
    }

    /// Requested effort resolved once from the immutable resource plan.
    fn effort_for_role(&self, role: Role) -> Result<EffortSetting, ProviderError> {
        self.resource_plan
            .effort(role)
            .ok_or_else(|| ProviderError::new(format!("configured effort missing for {role:?}")))
    }

    fn runtime_for(
        &mut self,
        target: &ExecutionTarget,
        effort: EffortSetting,
        role: Role,
    ) -> Result<&mut RuntimeProvider, ProviderError> {
        let image_tool = self.image_tool_for_role(role);
        // A granted role whose tool this process could not host is a
        // configuration the run was sealed with but cannot be honored here;
        // that is a typed refusal, never a silent run without the tool.
        if self.image_plan.allows(role) && image_tool.is_none() {
            return Err(ProviderError::new(format!(
                "image generation is granted to {role:?} but no image tool host is available in this process"
            )));
        }
        let key = (target.clone(), effort, image_tool.is_some());
        if !self.runtimes.contains_key(&key) {
            let runtime = match target.provider_id().as_str() {
                "fake" => RuntimeProvider::Fake(
                    FakeProvider::new(FakeScenario::successful(&self.workflow))
                        .map_err(|error| ProviderError::new(error.to_string()))?,
                ),
                "claude" => RuntimeProvider::Claude(
                    self.claude_provider(target.model_id().cloned())
                        .map(|provider| {
                            provider
                                .with_effort(effort)
                                .with_image_tool(image_tool.clone())
                        })
                        .map_err(|error| {
                            ProviderError::new(format!(
                                "configured provider unavailable for claude target: {error}"
                            ))
                        })?,
                ),
                "codex" => RuntimeProvider::Codex(
                    self.codex_provider(target.model_id().cloned())
                        .map(|provider| {
                            provider
                                .with_effort(effort)
                                .with_image_tool(image_tool.clone())
                        })
                        .map_err(|error| {
                            ProviderError::new(format!(
                                "configured provider unavailable for codex target: {error}"
                            ))
                        })?,
                ),
                VERIFY_PROVIDER_ID => RuntimeProvider::Verify(self.verify_provider()?),
                other => {
                    return Err(ProviderError::new(format!(
                        "unsupported configured provider {other:?}"
                    )));
                }
            };
            self.runtimes.insert(key.clone(), runtime);
        }
        self.runtimes
            .get_mut(&key)
            .ok_or_else(|| ProviderError::new("lazy provider cache insertion failed"))
    }

    fn claude_provider(
        &self,
        model: Option<crate::domain::ModelId>,
    ) -> Result<ClaudeProvider, ClaudeProviderError> {
        match &self.isolated_runtime {
            Some((root, runner)) if self.eval_auto_approve => {
                ClaudeProvider::from_runtime_with_eval_policy(
                    model,
                    root.clone(),
                    runner.clone(),
                    true,
                )
            }
            Some((root, runner)) => {
                ClaudeProvider::from_runtime(model, root.clone(), runner.clone())
            }
            None => ClaudeProvider::from_environment(model),
        }
    }

    /// The verifier writes artifacts where the native adapters do: under
    /// the explicit root when one was given (eval runtime, test fixture),
    /// else the configured data directory.
    fn verify_provider(&self) -> Result<VerifyProvider, ProviderError> {
        match &self.artifact_root {
            Some(root) => Ok(VerifyProvider::new(root.clone())),
            None => VerifyProvider::from_environment().map_err(|error| {
                ProviderError::new(format!(
                    "configured provider unavailable for verify target: {error}"
                ))
            }),
        }
    }

    fn codex_provider(
        &self,
        model: Option<crate::domain::ModelId>,
    ) -> Result<CodexProvider, CodexProviderError> {
        match &self.isolated_runtime {
            Some((root, runner)) => {
                CodexProvider::from_runtime(model, root.clone(), runner.clone())
            }
            None => CodexProvider::from_environment(model),
        }
    }
}

impl Provider for RoutedProvider {
    fn provider_id_for(&self, request: &ProviderRequest) -> Result<ProviderId, ProviderError> {
        Ok(self
            .target_for(request.route_override(), request.role())?
            .provider_id()
            .clone())
    }

    fn supports_role(&self, role: crate::domain::Role) -> bool {
        self.plan.route(role).is_some()
    }

    fn keep_attached_for(&self, request: &ProviderRequest) -> Result<bool, ProviderError> {
        let target = self.target_for(request.route_override(), request.role())?;
        let effort = self.effort_for_role(request.role())?;
        let granted = self.image_tool_for_role(request.role()).is_some();
        self.runtimes
            .get(&(target, effort, granted))
            .ok_or_else(|| ProviderError::new("waiting provider was not instantiated"))?
            .keep_attached_for(request)
    }

    fn stage_attention_response(
        &mut self,
        store: &mut SqliteStore,
        context: &ProviderAttentionContext,
        response: Option<&str>,
    ) -> Result<(), ProviderError> {
        let target = self.target_for(context.route_override(), context.role())?;
        let session = store
            .list_provider_sessions(context.run_id())
            .map_err(|error| ProviderError::new(error.to_string()))?
            .into_iter()
            .find(|session| {
                session.stage_id() == context.stage_id()
                    && session
                        .pending_attention()
                        .is_some_and(|pending| pending.attention_id() == context.request_id())
            })
            .ok_or_else(|| ProviderError::new("attention has no matching provider session"))?;
        if session.provider_id() != target.provider_id() {
            return Err(ProviderError::new(format!(
                "attention route provider mismatch: route={}, session={}",
                target.provider_id(),
                session.provider_id()
            )));
        }
        let effort = self.effort_for_role(context.role())?;
        self.runtime_for(&target, effort, context.role())?
            .stage_attention_response(store, context, response)
    }

    fn can_auto_resolve_attention(
        &mut self,
        store: &mut SqliteStore,
        context: &ProviderAttentionContext,
    ) -> Result<bool, ProviderError> {
        let target = self.target_for(context.route_override(), context.role())?;
        let session = store
            .list_provider_sessions(context.run_id())
            .map_err(|error| ProviderError::new(error.to_string()))?
            .into_iter()
            .find(|session| {
                session.stage_id() == context.stage_id()
                    && session
                        .pending_attention()
                        .is_some_and(|pending| pending.attention_id() == context.request_id())
            })
            .ok_or_else(|| ProviderError::new("attention has no matching provider session"))?;
        if session.provider_id() != target.provider_id() {
            return Ok(false);
        }
        let effort = self.effort_for_role(context.role())?;
        self.runtime_for(&target, effort, context.role())?
            .can_auto_resolve_attention(store, context)
    }

    /// No provider session exists yet for the stage this instruction is
    /// staged for, so — unlike attention, which resolves its route from an
    /// existing session — this resolves the route from `role` directly. Every
    /// continue cycle's follow-up stage routes through `Role::Implementer`,
    /// so the caller supplies exactly the role that stage will use.
    fn stage_continue_instruction(
        &mut self,
        store: &mut SqliteStore,
        run_id: crate::domain::RunId,
        stage_id: &crate::domain::StageId,
        role: crate::domain::Role,
        instruction: &str,
    ) -> Result<(), ProviderError> {
        let target = self.target_for_role(role)?;
        let effort = self.effort_for_role(role)?;
        self.runtime_for(&target, effort, role)?
            .stage_continue_instruction(store, run_id, stage_id, role, instruction)
    }

    /// Same route resolution as [`Self::stage_continue_instruction`]: the
    /// stage still has no provider session, so the leaf runtime is resolved
    /// from `Role::Implementer` directly, not from session lookup.
    fn discard_continue_instruction(
        &mut self,
        store: &mut SqliteStore,
        run_id: crate::domain::RunId,
        stage_id: &crate::domain::StageId,
    ) -> Result<(), ProviderError> {
        let target = self.target_for_role(Role::Implementer)?;
        let effort = self.effort_for_role(Role::Implementer)?;
        self.runtime_for(&target, effort, Role::Implementer)?
            .discard_continue_instruction(store, run_id, stage_id)
    }

    fn poll(
        &mut self,
        store: &mut SqliteStore,
        request: &ProviderRequest,
    ) -> Result<ProviderPoll, ProviderError> {
        let target = self.target_for(request.route_override(), request.role())?;
        let effort = self.effort_for_role(request.role())?;
        let runtime = self.runtime_for(&target, effort, request.role())?;
        if runtime.provider_id_for(request)? != *target.provider_id() {
            return Err(ProviderError::new(
                "resolved leaf provider identity mismatch",
            ));
        }
        if !runtime.supports_request(request)? {
            return Err(ProviderError::new(format!(
                "provider {} does not support role {:?}",
                target.provider_id(),
                request.role()
            )));
        }
        runtime.poll(store, request)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RuntimeProviderFactory;

impl ProviderFactory for RuntimeProviderFactory {
    type Provider = RoutedProvider;

    fn config_for_new_run(
        &self,
        selection: ExecutionSelection,
        effort: EffortRequest,
        workflow: &WorkflowDefinition,
        id: ConfigSnapshotId,
        created_at: DateTime<Utc>,
    ) -> Result<ResolvedConfigSnapshot, AppError> {
        let availability = match selection {
            ExecutionSelection::Uniform(provider) => {
                require_explicit_provider(provider)?;
                RecommendedAvailability::default()
            }
            ExecutionSelection::Recommended => probe_recommended_availability()?,
        };
        Ok(resolve_config(
            selection,
            effort,
            workflow,
            availability,
            id,
            created_at,
        )?)
    }

    fn config_for_new_run_with_image(
        &self,
        selection: ExecutionSelection,
        effort: EffortRequest,
        image: &ImageGenerationPlan,
        workflow: &WorkflowDefinition,
        id: ConfigSnapshotId,
        created_at: DateTime<Utc>,
    ) -> Result<ResolvedConfigSnapshot, AppError> {
        // Same bar as a provider: the backend must be usable now, or the run
        // is refused before any state exists. The backend is the user's own
        // Codex CLI, installed and natively authenticated.
        if image.is_enabled() {
            CodexImageGenerator::from_environment()
                .map_err(|error| AppError::ImageGenerationUnavailable(error.to_string()))?;
        }
        let availability = match selection {
            ExecutionSelection::Uniform(provider) => {
                require_explicit_provider(provider)?;
                RecommendedAvailability::default()
            }
            ExecutionSelection::Recommended => probe_recommended_availability()?,
        };
        Ok(resolve_config_with_image(
            selection,
            effort,
            image,
            workflow,
            availability,
            id,
            created_at,
        )?)
    }

    fn for_run(
        &self,
        run_id: RunId,
        config: &ResolvedConfigSnapshot,
        workflow: &WorkflowDefinition,
        _events: &[SequencedEvent],
    ) -> Result<Self::Provider, AppError> {
        let plan = RoutingPlan::from_snapshot(config, workflow).map_err(|error| {
            if config.schema_version() == 1 {
                AppError::LegacyExecutionConfig(run_id)
            } else {
                error.into()
            }
        })?;
        let resource_plan = ResourcePlan::from_snapshot(config, workflow)?;
        let image_plan = ImageGenerationPlan::from_snapshot(config, workflow)?;
        let provider = RoutedProvider::new(plan, resource_plan, workflow.clone());
        if !image_plan.is_enabled() {
            return Ok(provider);
        }
        // A resumed run in a process where Codex is no longer usable still
        // hosts the tool: calls then fail typed (`backend_not_configured`)
        // instead of the run failing or the grant silently vanishing.
        let generator: Option<Arc<dyn ImageGenerator>> = CodexImageGenerator::from_environment()
            .ok()
            .map(|generator| Arc::new(generator) as Arc<dyn ImageGenerator>);
        let host = start_image_host(
            run_id,
            &image_plan,
            generator,
            crate::store::process_root()?,
        )?;
        Ok(provider.with_image_tool(image_plan, host))
    }

    fn require_provider(&self, provider: UniformProvider) -> Result<(), AppError> {
        require_explicit_provider(provider)
    }
}

/// Binds the run's tool socket in this process. Evidence (prompt files)
/// goes under `evidence_root/<run>/image-generations/`; the database is
/// learned from the store at activation time.
fn start_image_host(
    run_id: RunId,
    plan: &ImageGenerationPlan,
    generator: Option<Arc<dyn ImageGenerator>>,
    evidence_root: PathBuf,
) -> Result<Arc<ImageToolHost>, AppError> {
    let service = ImageToolService::new(
        evidence_root,
        generator,
        plan.roles().collect(),
        plan.max_generations(),
    );
    ImageToolHost::start(service, run_id).map_err(|error| {
        AppError::ImageGenerationUnavailable(format!(
            "image tool socket could not be bound: {error}"
        ))
    })
}

fn require_explicit_provider(provider: UniformProvider) -> Result<(), AppError> {
    match provider {
        UniformProvider::Fake => Ok(()),
        UniformProvider::Claude => {
            let installation = ClaudeInstallation::discover()?;
            installation
                .authenticated()
                .then_some(())
                .ok_or(ClaudeProviderError::NotAuthenticated.into())
        }
        UniformProvider::Codex => {
            let installation = CodexInstallation::discover()?;
            installation
                .authenticated()
                .then_some(())
                .ok_or(CodexProviderError::NotAuthenticated.into())
        }
    }
}

fn probe_recommended_availability() -> Result<RecommendedAvailability, AppError> {
    let claude = match ClaudeInstallation::discover() {
        Ok(installation) => installation.authenticated(),
        Err(ClaudeProviderError::NotFound | ClaudeProviderError::NotAuthenticated) => false,
        Err(error) => return Err(error.into()),
    };
    let codex = match CodexInstallation::discover() {
        Ok(installation) => installation.authenticated(),
        Err(CodexProviderError::NotFound | CodexProviderError::NotAuthenticated) => false,
        Err(error) => return Err(error.into()),
    };
    Ok(RecommendedAvailability { claude, codex })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{StageId, StageKind, StageStatus, WorkflowKind};

    fn native_default_resource_plan(workflow: &WorkflowDefinition) -> ResourcePlan {
        let snapshot = resolve_config(
            ExecutionSelection::Uniform(UniformProvider::Fake),
            EffortSetting::NativeDefault,
            workflow,
            RecommendedAvailability::default(),
            ConfigSnapshotId::new("effort-test").unwrap(),
            std::time::SystemTime::now().into(),
        )
        .unwrap();
        ResourcePlan::from_snapshot(&snapshot, workflow).unwrap()
    }

    #[test]
    fn reconstructing_routed_provider_does_not_instantiate_native_adapters() {
        let workflow = WorkflowDefinition::built_in(WorkflowKind::Standard);
        let snapshot = resolve_config(
            ExecutionSelection::Recommended,
            EffortSetting::NativeDefault,
            &workflow,
            RecommendedAvailability {
                claude: true,
                codex: true,
            },
            ConfigSnapshotId::new("lazy-test").unwrap(),
            std::time::SystemTime::now().into(),
        )
        .unwrap();
        let provider = RuntimeProviderFactory
            .for_run(RunId::from_u128(1), &snapshot, &workflow, &[])
            .unwrap();
        assert!(provider.runtimes.is_empty());
    }

    #[test]
    fn attachment_policy_comes_from_current_stage_target_not_any_configured_provider() {
        let workflow = WorkflowDefinition::built_in(WorkflowKind::Standard);
        let fake = ExecutionTarget::new(ProviderId::new("fake").unwrap(), None);
        let codex = ExecutionTarget::new(ProviderId::new("codex").unwrap(), None);
        let routes = workflow
            .stages()
            .iter()
            .map(|stage| {
                (
                    stage.role(),
                    if stage.role() == Role::Implementer {
                        codex.clone()
                    } else {
                        fake.clone()
                    },
                )
            })
            .collect();
        let mut provider = RoutedProvider::new(
            RoutingPlan::test_plan(routes),
            native_default_resource_plan(&workflow),
            workflow.clone(),
        );
        assert!(
            provider
                .plan
                .routes()
                .any(|(_, route)| { route.target().provider_id().as_str() == "codex" })
        );
        let stage = workflow.stages().first().unwrap();
        let request = ProviderRequest::new(
            RunId::from_u128(1),
            StageId::new(stage.id().as_str()).unwrap(),
            StageKind::Architecture,
            StageStatus::Running,
            stage.role(),
            "task".to_owned(),
            PathBuf::from("/tmp/senate-routing-test"),
            1,
            0,
            None,
            Vec::new(),
        );
        let mut store = SqliteStore::open_in_memory().unwrap();
        let _ = provider.poll(&mut store, &request).unwrap();
        assert!(!provider.keep_attached_for(&request).unwrap());
    }
}
