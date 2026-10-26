use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;
use serde_json::Value;

use crate::async_runtime::run_blocking;
use crate::config;
use crate::engine;
use crate::error::{DbmError, Result};
use crate::legacy::{self, LegacyTool};
use crate::runner::{self, RunnerOperation, RunnerOutput};
use crate::source::{self, NewFormat};

const JSON_SCHEMA_VERSION: &str = "1.0";

#[derive(Debug, Parser)]
#[command(
    name = "dbm",
    version,
    about = "SQLite migration CLI with read-only PostgreSQL planning"
)]
pub struct Cli {
    #[arg(short, long, global = true, default_value = "dbm.toml")]
    config: PathBuf,
    #[arg(long, global = true)]
    namespace: Option<String>,
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Text)]
    format: OutputFormat,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
}

#[derive(Debug, Subcommand)]
enum Command {
    Init {
        #[arg(long, default_value = "sqlite")]
        driver: String,
    },
    New {
        description: String,
        #[arg(long = "migration-format", value_enum, default_value = "sql")]
        migration_format: MigrationFormatArg,
    },
    Validate,
    Plan,
    Up,
    Down {
        #[arg(long, default_value_t = 1)]
        steps: usize,
    },
    Status,
    History,
    Doctor {
        #[command(subcommand)]
        action: Option<DoctorCommand>,
    },
    Metadata {
        #[command(subcommand)]
        action: MetadataCommand,
    },
    Import {
        #[command(subcommand)]
        action: ImportCommand,
    },
    #[cfg(feature = "ai")]
    Ai {
        #[command(subcommand)]
        action: AiCommand,
    },
}

#[cfg(feature = "ai")]
#[derive(Debug, Subcommand)]
enum AiCommand {
    Generate {
        request: String,
        #[arg(long = "context-file")]
        context_files: Vec<PathBuf>,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        preview_context: bool,
    },
    Review {
        migration: PathBuf,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        preview_context: bool,
    },
    Explain {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        preview_context: bool,
    },
    SuggestDown {
        migration: PathBuf,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        preview_context: bool,
    },
}

#[derive(Debug, Subcommand)]
enum DoctorCommand {
    Reconcile {
        #[arg(long)]
        run_id: String,
        #[arg(long, value_enum)]
        result: ReconcileResultArg,
        #[arg(long)]
        evidence: PathBuf,
        #[arg(long)]
        repair_plan: PathBuf,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        actor: String,
        #[arg(long)]
        approve: bool,
    },
}

#[derive(Debug, Subcommand)]
enum MetadataCommand {
    Upgrade {
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ImportCommand {
    Inspect {
        #[arg(long, value_enum)]
        from: LegacyTool,
    },
    Plan {
        #[arg(long, value_enum)]
        from: LegacyTool,
    },
    Apply {
        #[arg(long, value_enum)]
        from: LegacyTool,
        #[arg(long)]
        plan_digest: String,
        #[arg(long)]
        yes: bool,
    },
    Verify {
        #[arg(long, value_enum)]
        from: LegacyTool,
        #[arg(long)]
        import_id: String,
    },
    Adopt {
        #[arg(long, value_enum)]
        from: LegacyTool,
        #[arg(long)]
        import_id: String,
        #[arg(long)]
        legacy_identity: String,
        #[arg(long)]
        version: String,
        #[arg(long)]
        evidence: PathBuf,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        actor: String,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ReconcileResultArg {
    OperationApplied,
    OperationNotApplied,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum MigrationFormatArg {
    Sql,
    SqlPair,
}

#[derive(Serialize)]
struct InitReport {
    schema_version: &'static str,
    command: &'static str,
    status: &'static str,
    config: String,
}

#[derive(Serialize)]
struct NewReport {
    schema_version: &'static str,
    command: &'static str,
    status: &'static str,
    files: Vec<String>,
}

#[derive(Serialize)]
struct ErrorReport {
    schema_version: &'static str,
    command: String,
    status: &'static str,
    errors: Vec<ErrorItem>,
}

#[derive(Serialize)]
struct ErrorItem {
    code: &'static str,
    message: String,
}

impl Cli {
    pub fn try_parse_cli() -> std::result::Result<Self, clap::Error> {
        <Self as Parser>::try_parse()
    }

    pub fn is_json_output(&self) -> bool {
        self.format.is_json()
    }

    pub fn command_name(&self) -> &'static str {
        match &self.command {
            Command::Init { .. } => "init",
            Command::New { .. } => "new",
            Command::Validate => "validate",
            Command::Plan => "plan",
            Command::Up => "up",
            Command::Down { .. } => "down",
            Command::Status => "status",
            Command::History => "history",
            Command::Doctor { action } => match action {
                Some(DoctorCommand::Reconcile { .. }) => "doctor reconcile",
                None => "doctor",
            },
            Command::Metadata { .. } => "metadata upgrade",
            Command::Import { action } => match action {
                ImportCommand::Inspect { .. } => "import inspect",
                ImportCommand::Plan { .. } => "import plan",
                ImportCommand::Apply { .. } => "import apply",
                ImportCommand::Verify { .. } => "import verify",
                ImportCommand::Adopt { .. } => "import adopt",
            },
            #[cfg(feature = "ai")]
            Command::Ai { action } => match action {
                AiCommand::Generate { .. } => "ai generate",
                AiCommand::Review { .. } => "ai review",
                AiCommand::Explain { .. } => "ai explain",
                AiCommand::SuggestDown { .. } => "ai suggest-down",
            },
        }
    }

    pub async fn run(self) -> Result<()> {
        match self.command {
            Command::Init { driver } => {
                if driver != "sqlite" {
                    return Err(DbmError::Unsupported(format!(
                        "driver '{driver}' is not implemented in this increment; use 'sqlite'"
                    )));
                }
                config::initialize_with_namespace(
                    &self.config,
                    self.namespace.as_deref().unwrap_or("default"),
                )?;
                let report = InitReport {
                    schema_version: JSON_SCHEMA_VERSION,
                    command: "init",
                    status: "initialized",
                    config: self.config.display().to_string(),
                };
                if self.format == OutputFormat::Json {
                    print_json(&report)?;
                } else {
                    println!(
                        "Initialized project configuration at {}",
                        self.config.display()
                    );
                }
            }
            Command::New {
                description,
                migration_format,
            } => {
                let loaded = config::load_with_namespace(&self.config, self.namespace.as_deref())?;
                let format = match migration_format {
                    MigrationFormatArg::Sql => NewFormat::Single,
                    MigrationFormatArg::SqlPair => NewFormat::Pair,
                };
                let files = source::create_new(&loaded, &description, format)?;
                let files: Vec<String> = files
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect();
                let report = NewReport {
                    schema_version: JSON_SCHEMA_VERSION,
                    command: "new",
                    status: "created",
                    files: files.clone(),
                };
                if self.format == OutputFormat::Json {
                    print_json(&report)?;
                } else {
                    for path in files {
                        println!("Created {path}");
                    }
                }
            }
            Command::Validate => {
                let config_path = self.config.clone();
                let load_path = config_path.clone();
                let namespace = self.namespace.clone();
                let loaded = run_blocking(move || {
                    config::load_with_namespace(&load_path, namespace.as_deref())
                })
                .await?;
                if loaded.config.execution.runner.is_some() {
                    match runner::invoke(&config_path, &loaded, RunnerOperation::Validate).await? {
                        RunnerOutput::ReadOnly(report) => {
                            emit_runner_validate(&report, self.format)?;
                        }
                        _ => {
                            return Err(protocol_output_error(
                                "validate returned wrong report type",
                            ));
                        }
                    }
                } else {
                    let report = engine::validate_async(&loaded).await?;
                    if self.format == OutputFormat::Json {
                        print_json(&report)?;
                    } else {
                        println!(
                            "Valid: {} migration(s), {} irreversible",
                            report.migration_count, report.irreversible_count
                        );
                        for warning in report.warnings {
                            println!("warning: {warning}");
                        }
                    }
                }
            }
            Command::Plan => {
                let config_path = self.config.clone();
                let load_path = config_path.clone();
                let namespace = self.namespace.clone();
                let loaded = run_blocking(move || {
                    config::load_with_namespace(&load_path, namespace.as_deref())
                })
                .await?;
                if loaded.config.execution.runner.is_some() {
                    let output =
                        runner::invoke(&config_path, &loaded, RunnerOperation::Plan).await?;
                    match output {
                        RunnerOutput::Plan(report) => emit_runner_plan(&report, self.format)?,
                        RunnerOutput::ReadOnly(_) => {
                            return Err(protocol_output_error("plan returned a read-only report"));
                        }
                        RunnerOutput::Execution(_) => {
                            return Err(protocol_output_error("plan returned an execution report"));
                        }
                    }
                } else {
                    let report = engine::plan_async(&loaded).await?;
                    if self.format == OutputFormat::Json {
                        print_json(&report)?;
                    } else {
                        println!("Plan {} ({})", report.plan_hash, report.status);
                        if report.operations.is_empty() {
                            println!("No pending migrations.");
                        }
                        for operation in report.operations {
                            println!(
                                "{} {}  {}",
                                operation.direction.to_ascii_uppercase(),
                                operation.migration_id,
                                operation.filename
                            );
                        }
                    }
                }
            }
            Command::Up => {
                let config_path = self.config.clone();
                let load_path = config_path.clone();
                let namespace = self.namespace.clone();
                let loaded = run_blocking(move || {
                    config::load_with_namespace(&load_path, namespace.as_deref())
                })
                .await?;
                if loaded.config.execution.runner.is_some() {
                    let output = runner::invoke(&config_path, &loaded, RunnerOperation::Up).await?;
                    match output {
                        RunnerOutput::Execution(report) => {
                            emit_runner_execution(&report, self.format)?;
                        }
                        RunnerOutput::ReadOnly(_) => {
                            return Err(protocol_output_error("up returned a read-only report"));
                        }
                        RunnerOutput::Plan(_) => {
                            return Err(protocol_output_error("up returned a plan report"));
                        }
                    }
                } else {
                    let report = engine::up_async(&loaded).await?;
                    emit_execution(&report, self.format)?;
                }
            }
            Command::Down { steps } => {
                let config_path = self.config.clone();
                let load_path = config_path.clone();
                let namespace = self.namespace.clone();
                let loaded = run_blocking(move || {
                    config::load_with_namespace(&load_path, namespace.as_deref())
                })
                .await?;
                if loaded.config.execution.runner.is_some() {
                    let output =
                        runner::invoke(&config_path, &loaded, RunnerOperation::Down { steps })
                            .await?;
                    match output {
                        RunnerOutput::Execution(report) => {
                            emit_runner_execution(&report, self.format)?;
                        }
                        RunnerOutput::ReadOnly(_) => {
                            return Err(protocol_output_error("down returned a read-only report"));
                        }
                        RunnerOutput::Plan(_) => {
                            return Err(protocol_output_error("down returned a plan report"));
                        }
                    }
                } else {
                    let report = engine::down_async(&loaded, steps).await?;
                    emit_execution(&report, self.format)?;
                }
            }
            Command::Status => {
                let config_path = self.config.clone();
                let load_path = config_path.clone();
                let namespace = self.namespace.clone();
                let loaded = run_blocking(move || {
                    config::load_with_namespace(&load_path, namespace.as_deref())
                })
                .await?;
                if loaded.config.execution.runner.is_some() {
                    match runner::invoke(&config_path, &loaded, RunnerOperation::Status).await? {
                        RunnerOutput::ReadOnly(report) => {
                            emit_runner_status(&report, self.format)?;
                        }
                        _ => {
                            return Err(protocol_output_error("status returned wrong report type"));
                        }
                    }
                } else {
                    let report = engine::status_async(&loaded).await?;
                    if self.format == OutputFormat::Json {
                        print_json(&report)?;
                    } else {
                        println!("Status: {}", report.status);
                        for migration in report.migrations {
                            match migration.reason {
                                Some(reason) => println!(
                                    "{} {}  {}  ({reason})",
                                    migration.state, migration.kind, migration.migration_id
                                ),
                                None => println!(
                                    "{} {}  {}",
                                    migration.state, migration.kind, migration.migration_id
                                ),
                            }
                        }
                    }
                }
            }
            Command::History => {
                let config_path = self.config.clone();
                let load_path = config_path.clone();
                let namespace = self.namespace.clone();
                let loaded = run_blocking(move || {
                    config::load_with_namespace(&load_path, namespace.as_deref())
                })
                .await?;
                if loaded.config.execution.runner.is_some() {
                    match runner::invoke(&config_path, &loaded, RunnerOperation::History).await? {
                        RunnerOutput::ReadOnly(report) => {
                            emit_runner_history(&report, self.format)?;
                        }
                        _ => {
                            return Err(protocol_output_error(
                                "history returned wrong report type",
                            ));
                        }
                    }
                } else {
                    let report = engine::history_async(&loaded).await?;
                    if self.format == OutputFormat::Json {
                        print_json(&report)?;
                    } else if report.runs.is_empty() {
                        println!("No completed migration runs.");
                    } else {
                        for run in report.runs {
                            let timestamp =
                                run.started_at.as_deref().unwrap_or(run.created_at.as_str());
                            println!(
                                "{timestamp} {} {} {} ({})",
                                run.direction, run.migration_id, run.state, run.run_id
                            );
                        }
                    }
                }
            }
            Command::Doctor { action } => match action {
                None => {
                    let config_path = self.config.clone();
                    let load_path = config_path.clone();
                    let namespace = self.namespace.clone();
                    let loaded = run_blocking(move || {
                        config::load_with_namespace(&load_path, namespace.as_deref())
                    })
                    .await?;
                    if loaded.config.execution.runner.is_some() {
                        match runner::invoke(&config_path, &loaded, RunnerOperation::Doctor).await?
                        {
                            RunnerOutput::ReadOnly(report) => {
                                emit_runner_doctor(&report, self.format)?;
                            }
                            _ => {
                                return Err(protocol_output_error(
                                    "doctor returned wrong report type",
                                ));
                            }
                        }
                    } else {
                        let report = engine::doctor_async(&loaded).await?;
                        if self.format == OutputFormat::Json {
                            print_json(&report)?;
                        } else {
                            println!("Doctor: {}", report.status);
                            for finding in report.findings {
                                println!(
                                    "{} [{}] {}",
                                    finding.severity, finding.code, finding.message
                                );
                            }
                        }
                    }
                }
                Some(DoctorCommand::Reconcile {
                    run_id,
                    result,
                    evidence,
                    repair_plan,
                    reason,
                    actor,
                    approve,
                }) => {
                    let config_path = self.config.clone();
                    let namespace = self.namespace.clone();
                    let operation_result = match result {
                        ReconcileResultArg::OperationApplied => "operation_applied",
                        ReconcileResultArg::OperationNotApplied => "operation_not_applied",
                    };
                    let (loaded_config, evidence_ref, repair_plan) = run_blocking(move || {
                        let loaded_config =
                            config::load_with_namespace(&config_path, namespace.as_deref())?;
                        if loaded_config.config.execution.runner.is_some() {
                            return Err(DbmError::Unsupported(
                                "doctor reconcile is not yet available through the compiled runner"
                                    .to_owned(),
                            ));
                        }
                        let evidence_bytes = std::fs::read(&evidence)
                            .map_err(|error| DbmError::io(evidence.display(), error))?;
                        if evidence_bytes.is_empty() || evidence_bytes.len() > 65_536 {
                            return Err(DbmError::Configuration(
                                "reconciliation evidence must be non-empty and no larger than 64 KiB"
                                    .to_owned(),
                            ));
                        }
                        let evidence_ref = crate::identity::raw_checksum(&evidence_bytes);
                        let repair_plan_bytes = std::fs::read(&repair_plan)
                            .map_err(|error| DbmError::io(repair_plan.display(), error))?;
                        if repair_plan_bytes.is_empty() || repair_plan_bytes.len() > 65_536 {
                            return Err(DbmError::Configuration(
                                "repair plan must be non-empty and no larger than 64 KiB"
                                    .to_owned(),
                            ));
                        }
                        let repair_plan = String::from_utf8(repair_plan_bytes).map_err(|error| {
                            DbmError::Configuration(format!(
                                "repair plan must be UTF-8 encoded JSON: {error}"
                            ))
                        })?;
                        Ok((loaded_config, evidence_ref, repair_plan))
                    })
                    .await?;
                    let report = engine::reconcile_async(
                        &loaded_config,
                        engine::ReconcileRequest {
                            run_id: &run_id,
                            operation_result,
                            evidence_ref: &evidence_ref,
                            repair_plan_json: &repair_plan,
                            reason: &reason,
                            actor: &actor,
                            approved: approve,
                        },
                    )
                    .await?;
                    if self.format == OutputFormat::Json {
                        print_json(&report)?;
                    } else {
                        println!(
                            "Reconciled run {} as {}",
                            report.run_id, report.operation_result
                        );
                    }
                }
            },
            Command::Metadata { action } => match action {
                MetadataCommand::Upgrade { yes } => {
                    let config_path = self.config.clone();
                    let namespace = self.namespace.clone();
                    let loaded_config = run_blocking(move || {
                        config::load_with_namespace(&config_path, namespace.as_deref())
                    })
                    .await?;
                    let report = engine::upgrade_metadata_async(&loaded_config, yes).await?;
                    if self.format == OutputFormat::Json {
                        print_json(&report)?;
                    } else {
                        println!("Metadata upgrade: {}", report.status);
                        if report.status == "requires_approval" {
                            println!(
                                "Review the target and repeat with --yes to apply the transactional upgrade."
                            );
                        }
                    }
                }
            },
            Command::Import { action } => match action {
                ImportCommand::Inspect { from } => {
                    let config_path = self.config.clone();
                    let namespace = self.namespace.clone();
                    let config = run_blocking(move || {
                        config::load_with_namespace(&config_path, namespace.as_deref())
                    })
                    .await?;
                    let report = legacy::inspect_async(&config, from).await?;
                    if self.format == OutputFormat::Json {
                        print_json(&serde_json::json!({
                            "schema_version": JSON_SCHEMA_VERSION,
                            "command": "import inspect",
                            "status": "inspected",
                            "source": report
                        }))?;
                    } else {
                        println!("Legacy source: {:?}", report.tool);
                        println!(
                            "{} event(s), {} currently applied; snapshot {}",
                            report.events.len(),
                            report.current_migrations.len(),
                            report.digest
                        );
                        for blocker in report.blockers {
                            println!("blocked [{}] {}", blocker.code, blocker.message);
                        }
                    }
                }
                ImportCommand::Plan { from } => {
                    let config_path = self.config.clone();
                    let namespace = self.namespace.clone();
                    let config = run_blocking(move || {
                        config::load_with_namespace(&config_path, namespace.as_deref())
                    })
                    .await?;
                    let report = legacy::plan_async(&config, from).await?;
                    if self.format == OutputFormat::Json {
                        print_json(&report)?;
                    } else {
                        println!("Legacy import plan: {}", report.status);
                        println!("Approval digest: {}", report.digest);
                        println!(
                            "{} source migration(s) are currently applied",
                            report.source.current_migrations.len()
                        );
                        println!(
                            "{} local migration(s) are included in the digest",
                            report.local_migrations.len()
                        );
                        for blocker in report.blockers {
                            println!("blocked [{}] {}", blocker.code, blocker.message);
                        }
                    }
                }
                ImportCommand::Apply {
                    from,
                    plan_digest,
                    yes,
                } => {
                    if !yes {
                        return Err(DbmError::Safety(
                            "import apply requires explicit --yes approval and the exact --plan-digest"
                                .to_owned(),
                        ));
                    }
                    let config_path = self.config.clone();
                    let namespace = self.namespace.clone();
                    let config = run_blocking(move || {
                        config::load_with_namespace(&config_path, namespace.as_deref())
                    })
                    .await?;
                    let report = legacy::apply_async(&config, from, &plan_digest).await?;
                    if self.format == OutputFormat::Json {
                        print_json(&report)?;
                    } else {
                        println!("Legacy import {}: {}", report.import_id, report.status);
                        println!("Approved plan digest: {}", report.digest);
                        println!(
                            "{} event(s), {} current migration(s)",
                            report.event_count, report.current_migration_count
                        );
                    }
                }
                ImportCommand::Verify { from, import_id } => {
                    let config_path = self.config.clone();
                    let namespace = self.namespace.clone();
                    let config = run_blocking(move || {
                        config::load_with_namespace(&config_path, namespace.as_deref())
                    })
                    .await?;
                    let report = legacy::verify_async(&config, from, &import_id).await?;
                    if self.format == OutputFormat::Json {
                        print_json(&report)?;
                    } else {
                        println!("Legacy import {}: {}", report.import_id, report.status);
                        println!("Snapshot digest: {}", report.snapshot_digest);
                        println!("Plan digest: {}", report.plan_digest);
                        println!(
                            "{} event(s), {} current migration(s) verified",
                            report.event_count, report.current_migration_count
                        );
                    }
                }
                ImportCommand::Adopt {
                    from,
                    import_id,
                    legacy_identity,
                    version,
                    evidence,
                    reason,
                    actor,
                    yes,
                } => {
                    let config_path = self.config.clone();
                    let namespace = self.namespace.clone();
                    let config = run_blocking(move || {
                        config::load_with_namespace(&config_path, namespace.as_deref())
                    })
                    .await?;
                    let evidence_bytes = run_blocking(move || {
                        std::fs::read(&evidence)
                            .map_err(|error| DbmError::io(evidence.display(), error))
                    })
                    .await?;
                    if evidence_bytes.is_empty() || evidence_bytes.len() > 65_536 {
                        return Err(DbmError::Configuration(
                            "adoption evidence must be non-empty and no larger than 64 KiB"
                                .to_owned(),
                        ));
                    }
                    let evidence_ref = crate::identity::raw_checksum(&evidence_bytes);
                    let report = legacy::adopt_async(
                        &config,
                        legacy::LegacyAdoptRequest {
                            tool: from,
                            import_id: &import_id,
                            legacy_identity: &legacy_identity,
                            native_version: &version,
                            evidence_ref: &evidence_ref,
                            reason: &reason,
                            actor: &actor,
                            approved: yes,
                        },
                    )
                    .await?;
                    if self.format == OutputFormat::Json {
                        print_json(&report)?;
                    } else {
                        println!(
                            "Adopted legacy {} as native migration {} ({})",
                            report.legacy_identity, report.native_version, report.status
                        );
                        println!("Adoption record: {}", report.adoption_id);
                    }
                }
            },
            #[cfg(feature = "ai")]
            Command::Ai { action } => {
                let config_path = self.config.clone();
                let config_load_path = config_path.clone();
                let namespace = self.namespace.clone();
                let loaded = run_blocking(move || {
                    config::load_with_namespace(&config_load_path, namespace.as_deref())
                })
                .await?;
                let (action, profile, preview_context) = match action {
                    AiCommand::Generate {
                        request,
                        context_files,
                        profile,
                        preview_context,
                    } => (
                        crate::ai::AiAction::Generate {
                            request,
                            context_files,
                        },
                        profile,
                        preview_context,
                    ),
                    AiCommand::Review {
                        migration,
                        profile,
                        preview_context,
                    } => (
                        crate::ai::AiAction::Review { migration },
                        profile,
                        preview_context,
                    ),
                    AiCommand::Explain {
                        plan,
                        profile,
                        preview_context,
                    } => (
                        crate::ai::AiAction::Explain { plan },
                        profile,
                        preview_context,
                    ),
                    AiCommand::SuggestDown {
                        migration,
                        profile,
                        preview_context,
                    } => (
                        crate::ai::AiAction::SuggestDown { migration },
                        profile,
                        preview_context,
                    ),
                };
                let report = crate::ai::execute(
                    &config_path,
                    &loaded,
                    action,
                    profile.as_deref(),
                    preview_context,
                )
                .await?;
                emit_ai_report(&report, self.format)?;
            }
        }
        Ok(())
    }
}

impl OutputFormat {
    pub fn is_json(self) -> bool {
        self == Self::Json
    }
}

pub fn emit_error(command: &str, error: &DbmError) -> Result<()> {
    let report = ErrorReport {
        schema_version: JSON_SCHEMA_VERSION,
        command: command.to_owned(),
        status: "error",
        errors: vec![ErrorItem {
            code: error_code(error),
            message: error.to_string(),
        }],
    };
    print_json(&report)
}

fn error_code(error: &DbmError) -> &'static str {
    match error {
        DbmError::Configuration(_) => "CONFIG_INVALID",
        DbmError::InvalidMigration { .. } => "MIGRATION_INVALID",
        DbmError::Diverged(_) => "STATE_DIVERGED",
        DbmError::Safety(_) => "SAFETY_BLOCKED",
        DbmError::Unsupported(_) => "CAPABILITY_UNSUPPORTED",
        DbmError::Execution(_) => "EXECUTION_FAILED",
        DbmError::ReconciliationRequired(_) => "RECONCILIATION_REQUIRED",
        DbmError::Lock(_) => "LOCK_UNAVAILABLE",
        DbmError::Io { .. } => "IO_ERROR",
        DbmError::Database(_) => "DATABASE_ERROR",
        DbmError::Driver { .. } => "DATABASE_ERROR",
        DbmError::Json(_) => "OUTPUT_ERROR",
        DbmError::Timestamp(_) => "TIME_ERROR",
        DbmError::Ai { code, .. } => code,
    }
}

fn emit_execution(report: &crate::engine::ExecutionReport, format: OutputFormat) -> Result<()> {
    if format == OutputFormat::Json {
        print_json(report)?;
    } else if report.migrations.is_empty() {
        println!("No migrations changed ({})", report.command);
    } else {
        let verb = if report.command == "up" {
            "Applied"
        } else {
            "Reverted"
        };
        for migration in &report.migrations {
            println!("{verb} {migration}");
        }
    }
    Ok(())
}

fn emit_runner_plan(report: &Value, format: OutputFormat) -> Result<()> {
    if format == OutputFormat::Json {
        return print_json(report);
    }
    let plan_hash = report
        .get("plan_hash")
        .and_then(Value::as_str)
        .ok_or_else(|| protocol_output_error("runner plan has no plan_hash"))?;
    let status = report
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(|| protocol_output_error("runner plan has no status"))?;
    println!("Plan {plan_hash} ({status})");
    let operations = report
        .get("operations")
        .and_then(Value::as_array)
        .ok_or_else(|| protocol_output_error("runner plan has no operations array"))?;
    if operations.is_empty() {
        println!("No pending migrations.");
    }
    for operation in operations {
        let direction = operation
            .get("direction")
            .and_then(Value::as_str)
            .unwrap_or("up")
            .to_ascii_uppercase();
        let migration_id = operation
            .get("migration_id")
            .and_then(Value::as_str)
            .ok_or_else(|| protocol_output_error("runner plan operation has no migration_id"))?;
        let filename = operation
            .get("filename")
            .and_then(Value::as_str)
            .ok_or_else(|| protocol_output_error("runner plan operation has no filename"))?;
        println!("{direction} {migration_id}  {filename}");
    }
    Ok(())
}

fn emit_runner_validate(report: &Value, format: OutputFormat) -> Result<()> {
    if format == OutputFormat::Json {
        return print_json(report);
    }
    let migration_count = report
        .get("migration_count")
        .and_then(Value::as_u64)
        .ok_or_else(|| protocol_output_error("runner validate report has no migration_count"))?;
    let irreversible_count = report
        .get("irreversible_count")
        .and_then(Value::as_u64)
        .ok_or_else(|| protocol_output_error("runner validate report has no irreversible_count"))?;
    println!("Valid: {migration_count} migration(s), {irreversible_count} irreversible");
    print_string_array(report.get("warnings"), "warning")
}

fn emit_runner_status(report: &Value, format: OutputFormat) -> Result<()> {
    if format == OutputFormat::Json {
        return print_json(report);
    }
    let status = report
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(|| protocol_output_error("runner status report has no status"))?;
    println!("Status: {status}");
    let migrations = report
        .get("migrations")
        .and_then(Value::as_array)
        .ok_or_else(|| protocol_output_error("runner status report has no migrations array"))?;
    for migration in migrations {
        let state = migration
            .get("state")
            .and_then(Value::as_str)
            .ok_or_else(|| protocol_output_error("runner status entry has no state"))?;
        let migration_id = migration
            .get("migration_id")
            .and_then(Value::as_str)
            .ok_or_else(|| protocol_output_error("runner status entry has no migration_id"))?;
        match migration.get("reason").and_then(Value::as_str) {
            Some(reason) => println!("{state}  {migration_id}  ({reason})"),
            None => println!("{state}  {migration_id}"),
        }
    }
    Ok(())
}

fn emit_runner_history(report: &Value, format: OutputFormat) -> Result<()> {
    if format == OutputFormat::Json {
        return print_json(report);
    }
    let runs = report
        .get("runs")
        .and_then(Value::as_array)
        .ok_or_else(|| protocol_output_error("runner history report has no runs array"))?;
    if runs.is_empty() {
        println!("No completed migration runs.");
        return Ok(());
    }
    for run in runs {
        let timestamp = run
            .get("started_at")
            .and_then(Value::as_str)
            .or_else(|| run.get("created_at").and_then(Value::as_str))
            .ok_or_else(|| protocol_output_error("runner history row has no timestamp"))?;
        let direction = required_string(run, "direction", "runner history row")?;
        let migration_id = required_string(run, "migration_id", "runner history row")?;
        let state = required_string(run, "state", "runner history row")?;
        let run_id = required_string(run, "run_id", "runner history row")?;
        println!("{timestamp} {direction} {migration_id} {state} ({run_id})");
    }
    Ok(())
}

fn emit_runner_doctor(report: &Value, format: OutputFormat) -> Result<()> {
    if format == OutputFormat::Json {
        return print_json(report);
    }
    let status = report
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(|| protocol_output_error("runner doctor report has no status"))?;
    println!("Doctor: {status}");
    let findings = report
        .get("findings")
        .and_then(Value::as_array)
        .ok_or_else(|| protocol_output_error("runner doctor report has no findings array"))?;
    for finding in findings {
        let severity = required_string(finding, "severity", "runner doctor finding")?;
        let code = required_string(finding, "code", "runner doctor finding")?;
        let message = required_string(finding, "message", "runner doctor finding")?;
        println!("{severity} [{code}] {message}");
    }
    Ok(())
}

fn required_string<'a>(value: &'a Value, field: &str, label: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| protocol_output_error(&format!("{label} has no {field}")))
}

fn print_string_array(value: Option<&Value>, prefix: &str) -> Result<()> {
    let values = value
        .and_then(Value::as_array)
        .ok_or_else(|| protocol_output_error("runner report has no string array"))?;
    for value in values {
        let value = value
            .as_str()
            .ok_or_else(|| protocol_output_error("runner report contains a non-string warning"))?;
        println!("{prefix}: {value}");
    }
    Ok(())
}

fn emit_runner_execution(report: &Value, format: OutputFormat) -> Result<()> {
    if format == OutputFormat::Json {
        return print_json(report);
    }
    let command = report
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| protocol_output_error("runner result has no command"))?;
    let migrations = report
        .get("migrations")
        .and_then(Value::as_array)
        .ok_or_else(|| protocol_output_error("runner result has no migrations array"))?;
    if migrations.is_empty() {
        println!("No migrations changed ({command})");
        return Ok(());
    }
    let verb = if command == "up" {
        "Applied"
    } else {
        "Reverted"
    };
    for migration in migrations {
        let migration = migration.as_str().ok_or_else(|| {
            protocol_output_error("runner result contains an invalid migration id")
        })?;
        println!("{verb} {migration}");
    }
    Ok(())
}

fn protocol_output_error(message: &str) -> DbmError {
    DbmError::Driver {
        driver: "migration runner protocol",
        message: message.to_owned(),
    }
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    let json = serde_json::to_string_pretty(value)?;
    println!("{json}");
    Ok(())
}

#[cfg(feature = "ai")]
fn emit_ai_report(report: &Value, format: OutputFormat) -> Result<()> {
    if format == OutputFormat::Json {
        return print_json(report);
    }
    let command = required_string(report, "command", "AI report")?;
    let status = required_string(report, "status", "AI report")?;
    println!("{command}: {status}");
    if status == "context_preview" {
        println!("No provider request was sent. Review these messages before using the network:");
        let messages = report
            .get("messages")
            .and_then(Value::as_array)
            .ok_or_else(|| protocol_output_error("AI context preview has no messages"))?;
        for message in messages {
            let role = required_string(message, "role", "AI context message")?;
            let content = required_string(message, "content", "AI context message")?;
            println!("--- {role} ---\n{content}");
        }
        return Ok(());
    }
    if let Some(model) = report.get("model").and_then(Value::as_str) {
        println!("Provider model: {model}");
    }
    if let Some(result) = report.get("result") {
        println!("{}", serde_json::to_string_pretty(result)?);
    }
    Ok(())
}
