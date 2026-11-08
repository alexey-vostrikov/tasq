#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use rusqlite::Connection;

use tasq::edge;
use tasq::error::{Error, Result};
use tasq::lease;
use tasq::node::{self, Filter, NewNode, Node, Patch};
use tasq::project::{self, Project};
use tasq::resource;
use tasq::status::{Kind, Status};
use tasq::{activity, db};

#[derive(Parser)]
#[command(
    name = "tasq",
    version,
    about = "Local-first task tracker for AI agents",
    long_about = None
)]
struct Cli {
    /// Path to the database; overrides the upward search for .tasq/tasq.db
    #[arg(long, global = true, value_name = "PATH")]
    db: Option<PathBuf>,

    /// Machine-readable output
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create .tasq/tasq.db in the current directory and register a project
    Init {
        /// Project key used in dot-notation IDs ([A-Z0-9]{2,8})
        #[arg(long, default_value = "TASQ")]
        key: String,
        /// Project name
        #[arg(long)]
        name: Option<String>,
    },
    /// Check database integrity, schema version and lease hygiene
    Doctor {
        /// Only check, never modify (currently the default behaviour)
        #[arg(long)]
        check: bool,
        /// Close expired attempts and release their reservations
        #[arg(long)]
        gc: bool,
    },
    /// Create a node; without --parent it becomes a root epic
    Add {
        /// Node title
        title: String,
        /// Parent node: dot-ID, bare path or #id
        #[arg(long, value_name = "REF")]
        parent: Option<String>,
        /// Node kind: epic | task | subtask | micro (default: from depth)
        #[arg(long)]
        kind: Option<String>,
        /// Description (the node's specification)
        #[arg(long)]
        desc: Option<String>,
        /// Priority: low | medium | high | critical
        #[arg(long)]
        priority: Option<String>,
        /// Due date, YYYY-MM-DD
        #[arg(long)]
        due: Option<String>,
        /// Weight for burn-up, default 1
        #[arg(long)]
        weight: Option<f64>,
        /// Estimated hours
        #[arg(long)]
        estimate: Option<f64>,
        /// Reference to the code this work touches
        #[arg(long)]
        source: Option<String>,
        /// Git branch this node belongs to
        #[arg(long)]
        branch: Option<String>,
    },
    /// Show a node, optionally with its subtree
    Show {
        /// Node reference
        reference: String,
        /// Include the whole subtree
        #[arg(long)]
        tree: bool,
    },
    /// List nodes in tree order
    Ls {
        /// Only this subtree
        #[arg(long, value_name = "REF")]
        parent: Option<String>,
        /// Filter by effective status
        #[arg(long)]
        status: Option<String>,
        /// Filter by assignee
        #[arg(long)]
        assignee: Option<String>,
        /// Only overdue nodes
        #[arg(long)]
        overdue: bool,
        /// Include archived nodes
        #[arg(long)]
        archived: bool,
    },
    /// Change node fields; only the given fields are touched
    Set {
        /// Node reference
        reference: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        desc: Option<String>,
        /// Target status
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        priority: Option<String>,
        #[arg(long)]
        due: Option<String>,
        #[arg(long)]
        assignee: Option<String>,
        #[arg(long)]
        weight: Option<f64>,
        /// Allow guarded transitions (reopen a closed node, finish with open children)
        #[arg(long)]
        force: bool,
        /// Protect the description from rewrites
        #[arg(long = "lock-desc", conflicts_with = "unlock_desc")]
        lock_desc: bool,
        /// Allow the description to change again
        #[arg(long = "unlock-desc")]
        unlock_desc: bool,
    },
    /// Move a node under another parent; descendant paths follow
    Mv {
        /// Node reference
        reference: String,
        /// New parent; omit to move to the project root
        #[arg(long, value_name = "REF")]
        parent: Option<String>,
        /// Take the next free number among the new siblings
        #[arg(long)]
        renumber: bool,
    },
    /// Soft-delete a todo node
    Rm {
        /// Node reference
        reference: String,
    },
    /// Bring back a soft-deleted node
    Restore {
        /// Node reference
        reference: String,
    },
    /// Archive (or unarchive) a subtree
    Archive {
        /// Node reference
        reference: String,
        /// Undo the archive
        #[arg(long)]
        undo: bool,
    },
    /// Show recent activity
    Log {
        /// How many records to show
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// Take a lease on a node and print its token
    Claim {
        /// Node reference
        reference: String,
        /// Who takes the lease (agent name); defaults to $TASQ_HOLDER or `cli`
        #[arg(long, env = "TASQ_HOLDER")]
        holder: Option<String>,
        /// Lease time to live in seconds
        #[arg(long, default_value_t = tasq::lease::DEFAULT_TTL)]
        ttl: i64,
        /// Take over a live lease (the previous attempt is recorded as superseded)
        #[arg(long)]
        steal: bool,
        /// Claim despite unfinished blockers
        #[arg(long)]
        force: bool,
        /// Checkpoint to store for the next holder
        #[arg(long)]
        checkpoint: Option<String>,
        /// Take a file or directory (repeatable): --reserve src/api
        #[arg(long, value_name = "PATH")]
        reserve: Vec<String>,
        /// Take a glob pattern (repeatable): --reserve-glob 'src/**/*.rs'
        #[arg(long = "reserve-glob", value_name = "GLOB")]
        reserve_glob: Vec<String>,
        /// Take a logical key (repeatable): --reserve-logical port:8080
        #[arg(long = "reserve-logical", value_name = "KEY")]
        reserve_logical: Vec<String>,
        /// Share instead of locking exclusively (applies to the reservations of this call)
        #[arg(long)]
        shared: bool,
    },
    /// Add reservations to a live lease
    Reserve {
        /// Lease token printed by `tasq claim`
        token: String,
        /// File or directory (repeatable)
        #[arg(long, value_name = "PATH")]
        path: Vec<String>,
        /// Glob pattern (repeatable)
        #[arg(long, value_name = "GLOB")]
        glob: Vec<String>,
        /// Logical key (repeatable)
        #[arg(long, value_name = "KEY")]
        logical: Vec<String>,
        /// Shared reservation instead of exclusive
        #[arg(long)]
        shared: bool,
    },
    /// Drop reservations of a live lease (all of them without arguments)
    Unreserve {
        /// Lease token printed by `tasq claim`
        token: String,
        /// File or directory (repeatable)
        #[arg(long, value_name = "PATH")]
        path: Vec<String>,
        /// Glob pattern (repeatable)
        #[arg(long, value_name = "GLOB")]
        glob: Vec<String>,
        /// Logical key (repeatable)
        #[arg(long, value_name = "KEY")]
        logical: Vec<String>,
    },
    /// Extend a lease before it expires
    Renew {
        /// Lease token printed by `tasq claim`
        token: String,
        /// New time to live in seconds
        #[arg(long, default_value_t = tasq::lease::DEFAULT_TTL)]
        ttl: i64,
    },
    /// Close a lease: released | failed | completed
    Release {
        /// Lease token printed by `tasq claim`
        token: String,
        /// What happened to the work
        #[arg(long, default_value = "released")]
        outcome: String,
        /// Checkpoint for whoever picks the node up next
        #[arg(long)]
        checkpoint: Option<String>,
        /// Finish despite unfinished children or blockers
        #[arg(long)]
        force: bool,
    },
    /// Live leases: who holds what and until when
    Holders,
    /// Dependencies between nodes: add, remove, list, graph
    Dep {
        #[command(subcommand)]
        command: DepCommand,
    },
    /// What to work on next, with the reason for the choice
    Next {
        /// How many candidates to show
        #[arg(long, default_value_t = 3)]
        limit: usize,
    },
}

#[derive(Subcommand)]
enum DepCommand {
    /// Add a link: `tasq dep add A B` means A waits for B
    Add {
        /// The node that waits
        from: String,
        /// The node it waits for (or the target of the relation)
        to: String,
        /// Relation kind: blocks | related | duplicates | discovered_from | split_from
        #[arg(long, default_value = "blocks")]
        kind: String,
    },
    /// Remove a link
    Rm {
        from: String,
        to: String,
        #[arg(long, default_value = "blocks")]
        kind: String,
    },
    /// List links of the project
    Ls {
        /// Only this kind
        #[arg(long)]
        kind: Option<String>,
    },
    /// Neighbourhood of a node: what it waits for, what it unblocks, Mermaid
    Graph {
        reference: String,
        /// How deep to walk the blockers chain
        #[arg(long, default_value_t = 3)]
        depth: i64,
        /// Print a Mermaid diagram instead of a table
        #[arg(long)]
        mermaid: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(code) => code,
        Err(err) => {
            report_error(&err, cli.json);
            ExitCode::from(1)
        }
    }
}

/// Открывает базу и проект; нужен почти каждой команде.
fn open(db_path: Option<&std::path::Path>) -> Result<(Connection, Project)> {
    let cwd = std::env::current_dir()?;
    let path = db::discover(&cwd, db_path)?;
    let conn = db::open_checked(&path)?;
    let project = project::default_project(&conn)?;
    Ok((conn, project))
}

fn parse_status(value: &str) -> Result<Status> {
    Status::parse(value).ok_or_else(|| Error::InvalidValue {
        field: "status".to_string(),
        value: value.to_string(),
    })
}

fn parse_kind(value: &str) -> Result<Kind> {
    Kind::parse(value).ok_or_else(|| Error::InvalidValue {
        field: "kind".to_string(),
        value: value.to_string(),
    })
}

/// Собирает резервы из аргументов CLI в одном порядке: пути, шаблоны, логические ключи.
fn collect_resources(
    paths: &[String],
    globs: &[String],
    logical: &[String],
    shared: bool,
) -> Vec<resource::Resource> {
    let mut items: Vec<resource::Resource> = Vec::new();
    for value in paths {
        items.push(resource::Resource::path(value));
    }
    for value in globs {
        items.push(resource::Resource::glob(value));
    }
    for value in logical {
        items.push(resource::Resource::logical(value));
    }
    if shared {
        items = items.into_iter().map(|item| item.shared()).collect();
    }
    items
}

fn print_json<T: serde::Serialize>(value: &T) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("serialisable")
    );
}

fn progress(node: &Node) -> String {
    if node.progress_total <= 0.0 {
        return "-".to_string();
    }
    let percent = (node.progress_done / node.progress_total * 100.0).round() as i64;
    format!("{percent}%")
}

fn line(node: &Node, indent: usize) -> String {
    let status = &node.effective_status;
    let due = node.due_date.as_deref().unwrap_or("-");
    let assignee = node.assigned_to.as_deref().unwrap_or("-");
    format!(
        "{:indent$}{:<18} {:<11} {:<8} {:<10} {:>5} {:<12} {}",
        "",
        node.dot_id,
        status,
        node.priority,
        due,
        progress(node),
        assignee,
        node.title,
        indent = indent
    )
}

fn run(cli: &Cli) -> Result<ExitCode> {
    match &cli.command {
        Command::Init { key, name } => {
            let cwd = std::env::current_dir()?;
            let name = name.clone().unwrap_or_else(|| key.clone());
            let outcome = db::init(&cwd, key, &name)?;
            if cli.json {
                print_json(&outcome);
            } else {
                println!("database: {}", outcome.db_path);
                println!("schema:   v{}", outcome.schema_version);
                println!(
                    "project:  {} ({})",
                    outcome.project_key,
                    if outcome.project_created {
                        "created"
                    } else {
                        "already present"
                    }
                );
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Doctor { check: _, gc } => {
            let cwd = std::env::current_dir()?;
            let path = db::discover(&cwd, cli.db.as_deref())?;
            let report = db::doctor(&path)?;
            if cli.json {
                print_json(&report);
            } else {
                println!("database:  {}", report.db_path);
                println!(
                    "schema:    v{} (supported v{})",
                    report.schema_version, report.supported_version
                );
                println!("integrity: {}", report.integrity);
                println!(
                    "foreign keys: {} violation(s)",
                    report.foreign_key_violations
                );
                println!(
                    "data:      {} project(s), {} node(s), {} live lease(s)",
                    report.projects, report.nodes, report.live_attempts
                );
                if report.expired_attempts > 0 {
                    println!(
                        "stale:     {} expired attempt(s), {} orphan reservation(s)",
                        report.expired_attempts, report.orphan_reservations
                    );
                }
                println!("status:    {}", if report.ok { "ok" } else { "FAILED" });
            }
            if *gc {
                let cwd = std::env::current_dir()?;
                let path = db::discover(&cwd, cli.db.as_deref())?;
                let mut conn = db::open_checked(&path)?;
                let project = project::default_project(&conn)?;
                let closed = lease::gc(&mut conn, &project)?;
                let unblocked = edge::clear_stale_blocked(&mut conn, &project)?;
                if cli.json {
                    print_json(&serde_json::json!({
                        "expired_attempts_closed": closed,
                        "stale_blocked_cleared": unblocked,
                    }));
                } else {
                    println!("closed {closed} expired attempt(s)");
                    if unblocked > 0 {
                        println!("returned {unblocked} node(s) from blocked to todo");
                    }
                }
            }
            Ok(if report.ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }

        Command::Add {
            title,
            parent,
            kind,
            desc,
            priority,
            due,
            weight,
            estimate,
            source,
            branch,
        } => {
            let (mut conn, project) = open(cli.db.as_deref())?;
            let kind = match kind {
                Some(value) => Some(parse_kind(value)?),
                None => None,
            };
            let created = node::create(
                &mut conn,
                &project,
                NewNode {
                    parent: parent.as_deref(),
                    title,
                    kind,
                    description: desc.as_deref(),
                    priority: priority.as_deref(),
                    due_date: due.as_deref(),
                    estimated_hours: *estimate,
                    weight: *weight,
                    source_ref: source.as_deref(),
                    branch: branch.as_deref(),
                },
            )?;
            if cli.json {
                print_json(&created);
            } else {
                println!("{}", line(&created, 0));
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Show { reference, tree } => {
            let (conn, project) = open(cli.db.as_deref())?;
            if *tree {
                let nodes = node::subtree(&conn, &project, reference)?;
                let root_depth = nodes.first().map(|n| n.depth).unwrap_or(0);
                if cli.json {
                    print_json(&nodes);
                } else {
                    for node in &nodes {
                        let indent = ((node.depth - root_depth) as usize) * 2;
                        println!("{}", line(node, indent));
                    }
                }
                return Ok(ExitCode::SUCCESS);
            }
            let node = node::resolve(&conn, &project, reference)?;
            if cli.json {
                print_json(&node);
                return Ok(ExitCode::SUCCESS);
            }
            println!("{}", node.dot_id);
            println!("title     {}", node.title);
            println!("kind      {}", node.kind);
            println!(
                "status    {} (stored {})",
                node.effective_status, node.status
            );
            println!("priority  {}", node.priority);
            if let Some(due) = &node.due_date {
                println!("due       {due}");
            }
            if let Some(assignee) = &node.assigned_to {
                println!("assignee  {assignee}");
            }
            println!(
                "progress  {}/{} ({})",
                node.progress_done,
                node.progress_total,
                progress(&node)
            );
            if let Some(hours) = node.estimated_hours {
                println!("estimate  {hours}h");
            }
            if let Some(source) = &node.source_ref {
                println!("source    {source}");
            }
            if let Some(branch) = &node.branch {
                println!("branch    {branch}");
            }
            if let Some(lease) = &node.lease_holder {
                println!(
                    "lease     {lease} until {}",
                    node.lease_expires_at.as_deref().unwrap_or("?")
                );
            }
            if node.description_locked {
                println!("desc      (locked)");
            }
            if let Some(description) = &node.description {
                println!("--- description ---");
                println!("{description}");
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Ls {
            parent,
            status,
            assignee,
            overdue,
            archived,
        } => {
            let (conn, project) = open(cli.db.as_deref())?;
            let nodes = node::list(
                &conn,
                &project,
                &Filter {
                    parent: parent.as_deref(),
                    status: status.as_deref(),
                    assignee: assignee.as_deref(),
                    overdue: *overdue,
                    include_archived: *archived,
                    include_deleted: false,
                    limit: None,
                },
            )?;
            if cli.json {
                print_json(&nodes);
                return Ok(ExitCode::SUCCESS);
            }
            if nodes.is_empty() {
                println!("no nodes; create one with `tasq add \"title\"`");
                return Ok(ExitCode::SUCCESS);
            }
            let mut current_root = String::new();
            for node in &nodes {
                let indent = ((node.depth - 1) as usize) * 2;
                if node.depth == 1 {
                    current_root = node.path.clone();
                }
                let _ = &current_root;
                println!("{}", line(node, indent));
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Set {
            reference,
            title,
            desc,
            status,
            priority,
            due,
            assignee,
            weight,
            force,
            lock_desc,
            unlock_desc,
        } => {
            let (mut conn, project) = open(cli.db.as_deref())?;
            let status = match status {
                Some(value) => Some(parse_status(value)?),
                None => None,
            };
            let description_locked = if *lock_desc {
                Some(true)
            } else if *unlock_desc {
                Some(false)
            } else {
                None
            };
            let updated = node::update(
                &mut conn,
                &project,
                reference,
                Patch {
                    title: title.as_deref(),
                    description: desc.as_deref(),
                    status,
                    priority: priority.as_deref(),
                    assigned_to: assignee.as_deref(),
                    due_date: due.as_deref(),
                    weight: *weight,
                    estimated_hours: None,
                    source_ref: None,
                    branch: None,
                    force: *force,
                    description_locked,
                    lease: false,
                    actor: None,
                },
            )?;
            if cli.json {
                print_json(&updated);
            } else {
                println!("{}", line(&updated, 0));
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Mv {
            reference,
            parent,
            renumber,
        } => {
            let (mut conn, project) = open(cli.db.as_deref())?;
            let moved =
                node::move_node(&mut conn, &project, reference, parent.as_deref(), *renumber)?;
            if cli.json {
                print_json(&moved);
            } else {
                println!("{}", line(&moved, 0));
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Rm { reference } => {
            let (mut conn, project) = open(cli.db.as_deref())?;
            node::delete(&mut conn, &project, reference)?;
            if !cli.json {
                println!("removed {reference}");
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Restore { reference } => {
            let (mut conn, project) = open(cli.db.as_deref())?;
            let restored = node::restore(&mut conn, &project, reference)?;
            if cli.json {
                print_json(&restored);
            } else {
                println!("restored {}", restored.dot_id);
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Archive { reference, undo } => {
            let (mut conn, project) = open(cli.db.as_deref())?;
            let affected = node::archive(&mut conn, &project, reference, *undo)?;
            if !cli.json {
                println!(
                    "{} {affected} node(s)",
                    if *undo { "unarchived" } else { "archived" }
                );
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Log { limit } => {
            let (conn, project) = open(cli.db.as_deref())?;
            let records = activity::recent(&conn, project.id, *limit)?;
            if cli.json {
                print_json(&records);
                return Ok(ExitCode::SUCCESS);
            }
            for record in &records {
                let target = record.dot_id.as_deref().unwrap_or("-");
                let change = match (&record.old_value, &record.new_value) {
                    (Some(old), Some(new)) => format!("{old} -> {new}"),
                    (None, Some(new)) => format!("-> {new}"),
                    (Some(old), None) => format!("{old} ->"),
                    (None, None) => String::new(),
                };
                let detail = match (record.summary.as_deref(), change.is_empty()) {
                    (Some(summary), false) => format!("{summary}  [{change}]"),
                    (Some(summary), true) => summary.to_string(),
                    (None, _) => change,
                };
                println!(
                    "{} {:<10} {:<12} {}",
                    record.created_at, record.action, target, detail
                );
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Claim {
            reference,
            holder,
            ttl,
            steal,
            force,
            checkpoint,
            reserve,
            reserve_glob,
            reserve_logical,
            shared,
        } => {
            let (mut conn, project) = open(cli.db.as_deref())?;
            let holder = holder.clone().unwrap_or_else(|| "cli".to_string());
            let resources = collect_resources(reserve, reserve_glob, reserve_logical, *shared);
            let lease = lease::claim(
                &mut conn,
                &project,
                reference,
                &lease::ClaimRequest {
                    holder: &holder,
                    ttl_seconds: *ttl,
                    steal: *steal,
                    force: *force,
                    checkpoint: checkpoint.as_deref(),
                    reservations: resources,
                },
            )?;
            if cli.json {
                print_json(&lease);
                return Ok(ExitCode::SUCCESS);
            }
            println!("{}", lease.dot_id);
            println!("token      {}", lease.token);
            println!("holder     {} (attempt {})", lease.holder, lease.attempt_no);
            println!(
                "expires    {} (ttl {}s)",
                lease.expires_at, lease.ttl_seconds
            );
            if let Some(attempt) = lease.superseded_attempt {
                println!("superseded attempt #{attempt}");
            }
            if !lease.reservations.is_empty() {
                println!("reserved   {}", lease.reservations.join(", "));
            }
            if let Some(note) = &lease.inherited_checkpoint {
                println!("--- checkpoint from the previous attempt ---");
                println!("{note}");
            }
            println!();
            println!("keep the token: renew and release need it");
            Ok(ExitCode::SUCCESS)
        }

        Command::Reserve {
            token,
            path,
            glob,
            logical,
            shared,
        } => {
            let (mut conn, project) = open(cli.db.as_deref())?;
            let resources = collect_resources(path, glob, logical, *shared);
            let held = lease::reserve(&mut conn, &project, token, &resources)?;
            if cli.json {
                print_json(&held);
            } else {
                println!("held: {}", held.join(", "));
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Unreserve {
            token,
            path,
            glob,
            logical,
        } => {
            let (mut conn, project) = open(cli.db.as_deref())?;
            let resources = collect_resources(path, glob, logical, false);
            let requested = if resources.is_empty() {
                None
            } else {
                Some(resources.as_slice())
            };
            let removed = lease::unreserve(&mut conn, &project, token, requested)?;
            if cli.json {
                print_json(&serde_json::json!({ "released": removed }));
            } else {
                println!("released {removed} reservation(s)");
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Renew { token, ttl } => {
            let (mut conn, project) = open(cli.db.as_deref())?;
            let lease = lease::renew(&mut conn, &project, token, *ttl)?;
            if cli.json {
                print_json(&lease);
            } else {
                println!("{} renewed until {}", lease.dot_id, lease.expires_at);
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Release {
            token,
            outcome,
            checkpoint,
            force,
        } => {
            let (mut conn, project) = open(cli.db.as_deref())?;
            let outcome = lease::Outcome::parse(outcome).ok_or_else(|| Error::InvalidValue {
                field: "outcome".to_string(),
                value: outcome.clone(),
            })?;
            let node = lease::release(
                &mut conn,
                &project,
                token,
                outcome,
                checkpoint.as_deref(),
                *force,
            )?;
            if cli.json {
                print_json(&node);
            } else {
                println!("{} -> {}", node.dot_id, node.status);
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Holders => {
            let (conn, project) = open(cli.db.as_deref())?;
            let live = lease::holdings(&conn, &project)?;
            if cli.json {
                print_json(&live);
                return Ok(ExitCode::SUCCESS);
            }
            if live.is_empty() {
                println!("no live leases");
                return Ok(ExitCode::SUCCESS);
            }
            for holding in &live {
                println!(
                    "{:<18} {:<14} attempt {} until {} (ttl {}s)",
                    holding.dot_id,
                    holding.holder,
                    holding.attempt_no,
                    holding.expires_at,
                    holding.ttl_seconds
                );
                for reservation in &holding.reservations {
                    println!("{:<18}   holds {reservation}", "");
                }
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Dep { command } => match command {
            DepCommand::Add { from, to, kind } => {
                let kind = parse_edge_kind(kind)?;
                let (mut conn, project) = open(cli.db.as_deref())?;
                let edge = edge::link(&mut conn, &project, from, to, kind, "-")?;
                if cli.json {
                    print_json(&edge);
                } else {
                    println!("{} {} {}", edge.from, edge.kind, edge.to);
                }
                Ok(ExitCode::SUCCESS)
            }
            DepCommand::Rm { from, to, kind } => {
                let kind = parse_edge_kind(kind)?;
                let (mut conn, project) = open(cli.db.as_deref())?;
                let removed = edge::unlink(&mut conn, &project, from, to, kind, "-")?;
                if !cli.json {
                    if removed {
                        println!("removed {} {} {}", from, kind.as_str(), to);
                    } else {
                        println!("no such link: {} {} {}", from, kind.as_str(), to);
                    }
                }
                Ok(if removed {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::from(1)
                })
            }
            DepCommand::Ls { kind } => {
                let kind = match kind {
                    Some(value) => Some(parse_edge_kind(value)?),
                    None => None,
                };
                let (conn, project) = open(cli.db.as_deref())?;
                let links = edge::list(&conn, &project, kind)?;
                if cli.json {
                    print_json(&links);
                    return Ok(ExitCode::SUCCESS);
                }
                if links.is_empty() {
                    println!("no links; add one with `tasq dep add A B`");
                    return Ok(ExitCode::SUCCESS);
                }
                for link in &links {
                    println!("{:<16} {} -> {}", link.kind, link.from, link.to);
                }
                Ok(ExitCode::SUCCESS)
            }
            DepCommand::Graph {
                reference,
                depth,
                mermaid,
            } => {
                let (conn, project) = open(cli.db.as_deref())?;
                let graph = edge::graph(&conn, &project, reference, *depth)?;
                if cli.json {
                    print_json(&graph);
                    return Ok(ExitCode::SUCCESS);
                }
                if *mermaid {
                    println!("{}", graph.to_mermaid());
                    return Ok(ExitCode::SUCCESS);
                }
                println!("{}", graph.node);
                if graph.blocked_by.is_empty() {
                    println!("waits for   nothing");
                }
                for neighbour in &graph.blocked_by {
                    println!(
                        "waits for   {} ({}) {}",
                        neighbour.dot_id, neighbour.status, neighbour.title
                    );
                }
                if graph.blocks.is_empty() {
                    println!("unblocks    nothing");
                }
                for neighbour in &graph.blocks {
                    println!(
                        "unblocks    {} ({}) {}",
                        neighbour.dot_id, neighbour.status, neighbour.title
                    );
                }
                if graph.unblocks_total > 0 {
                    println!("transitively unblocks {} node(s)", graph.unblocks_total);
                }
                if graph.critical_chain.len() > 1 {
                    println!("chain       {}", graph.critical_chain.join(" -> "));
                }
                if let Some(blocker) = graph
                    .transitive_blockers
                    .iter()
                    .find(|neighbour| !neighbour.terminal)
                {
                    println!(
                        "next blocker {} ({} away) {}",
                        blocker.dot_id, blocker.distance, blocker.title
                    );
                }
                for link in &graph.related {
                    println!("{:<16} {} -> {}", link.kind, link.from, link.to);
                }
                Ok(ExitCode::SUCCESS)
            }
        },

        Command::Next { limit } => {
            let (conn, project) = open(cli.db.as_deref())?;
            let candidates = edge::next_candidates(&conn, &project, *limit)?;
            if cli.json {
                print_json(&candidates);
                return Ok(ExitCode::SUCCESS);
            }
            if candidates.is_empty() {
                println!("nothing to pick up: every open node is blocked or leased");
                return Ok(ExitCode::SUCCESS);
            }
            for (index, candidate) in candidates.iter().enumerate() {
                println!(
                    "{}. {:<18} {:<8} {}",
                    index + 1,
                    candidate.dot_id,
                    candidate.priority,
                    candidate.title
                );
                println!("   because {}", candidate.reason);
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn parse_edge_kind(value: &str) -> Result<edge::Kind> {
    edge::Kind::parse(value).ok_or_else(|| Error::InvalidValue {
        field: "kind".to_string(),
        value: format!(
            "{value} (allowed: blocks, related, duplicates, discovered_from, split_from)"
        ),
    })
}

fn report_error(err: &Error, json: bool) {
    if json {
        print_json(&err.to_json());
    } else {
        eprintln!("error[{}]: {}", err.code(), err);
        eprintln!("hint: {}", err.hint());
    }
}
