#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use rusqlite::Connection;

use clap::CommandFactory;
use tasq::connect;
use tasq::docs;
use tasq::edge;
use tasq::error::{Error, Result};
use tasq::lease;
use tasq::mcp;
use tasq::node::{self, Filter, NewNode, Node, Patch};
use tasq::project::{self, Project};
use tasq::resource;
use tasq::status::{Kind, Status};
use tasq::{activity, budget, burnup, comment, db, note, overview, template, transfer};

#[derive(Parser)]
#[command(
    name = "tasq",
    version,
    about = "Local-first task tracker for AI agents",
    long_about = None
)]
struct Cli {
    /// Path to the database; overrides the upward search for .tasq/tasq.db
    #[arg(long, global = true, value_name = "PATH", env = "TASQ_DB")]
    db: Option<PathBuf>,

    /// Machine-readable output
    #[arg(long, global = true)]
    json: bool,

    /// Project key to work in (default: the first project by creation order)
    #[arg(long, global = true, value_name = "KEY", env = "TASQ_PROJECT")]
    project: Option<String>,

    /// Max seconds data may stay unwritten on the mcp server (0 disables the flush)
    #[arg(
        long,
        global = true,
        value_name = "SECS",
        env = "TASQ_FLUSH_SECONDS",
        default_value_t = 3
    )]
    flush: u64,

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
    /// Notes: decisions, context, meetings, blockers, releases
    Note {
        #[command(subcommand)]
        command: NoteCommand,
    },
    /// Comments on a node: a chronological thread
    Comment {
        #[command(subcommand)]
        command: CommentCommand,
    },
    /// Task templates with {variable} placeholders
    Template {
        #[command(subcommand)]
        command: TemplateCommand,
    },
    /// Full-text search across nodes and notes
    Find {
        /// Query; supports FTS syntax (prefix `migr*`, phrases, OR)
        query: String,
    },
    /// Context chain from the root epic down to the node
    Ctx { reference: String },
    /// Board: nodes grouped by status, with overdue and live leases
    Board,
    /// Hours: estimate against the time actually spent in attempts
    Time {
        /// Only this subtree
        #[arg(long, value_name = "REF")]
        parent: Option<String>,
    },
    /// Batch operations
    Bulk {
        #[command(subcommand)]
        command: BulkCommand,
    },
    /// Burn-up: scope against completed work, with a forecast
    Burnup {
        /// Metric: count | points | hours
        #[arg(long, default_value = "count")]
        metric: String,
        /// Start day, YYYY-MM-DD
        #[arg(long)]
        since: Option<String>,
        /// End day, YYYY-MM-DD
        #[arg(long)]
        until: Option<String>,
        /// Draw an ASCII chart
        #[arg(long)]
        ascii: bool,
        /// Write snapshots for the whole series
        #[arg(long)]
        snapshot: bool,
        /// Chart width in columns
        #[arg(long, default_value_t = 70)]
        width: usize,
    },
    /// Nodes that fall behind their own due date
    Risk {
        /// Allowed lag between elapsed time and completed work, 0..1
        #[arg(long, default_value_t = 0.2)]
        threshold: f64,
    },
    /// Export the project as JSON with a checksum
    Export {
        /// Write to a file instead of stdout
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
        /// Include soft-deleted nodes
        #[arg(long)]
        include_deleted: bool,
    },
    /// Import a project from JSON
    Import {
        /// Path to the export file
        file: PathBuf,
        /// merge (add missing nodes) or replace (wipe and load)
        #[arg(long, default_value = "merge")]
        mode: String,
        /// Read a saga-mcp tracker_export file
        #[arg(long)]
        from_saga: bool,
        /// Validate and report without writing
        #[arg(long)]
        dry_run: bool,
    },
    /// Run the MCP server over stdio
    Mcp {
        /// Tool profile: full | agent | read-only | migration
        #[arg(long, default_value = "full")]
        profile: String,
    },
    /// Create something: a project
    Create {
        #[command(subcommand)]
        what: CreateWhat,
    },
    /// List things: projects
    List {
        #[command(subcommand)]
        what: ListWhat,
    },
    /// Change something: a project
    Update {
        #[command(subcommand)]
        what: UpdateWhat,
    },
    /// Delete something: a project, with all of its content
    Delete {
        #[command(subcommand)]
        what: DeleteWhat,
    },
    /// Generate the reference documentation from the binary itself
    Docs {
        /// Where to write (default: docs)
        #[arg(long, value_name = "DIR", default_value = "docs")]
        out: PathBuf,
        /// Only compare with what is on disk; exit 3 on drift
        #[arg(long)]
        check: bool,
    },
    /// Measure the MCP tool schemas in tokens against the budget
    Tokens {
        /// Only this profile
        #[arg(long)]
        profile: Option<String>,
        /// Fail (exit 1) if a profile exceeds this many tokens
        #[arg(long, value_name = "N")]
        max: Option<usize>,
        /// Show the cost of each tool
        #[arg(long)]
        per_tool: bool,
    },
    /// Register the MCP server in a client's config
    Connect {
        /// Client id; without it the command only lists candidates
        client: Option<String>,
        /// Where to write: user | project
        #[arg(long)]
        scope: Option<String>,
        /// HTTP endpoint instead of stdio (loopback only)
        #[arg(long, value_name = "URL")]
        url: Option<String>,
        /// Only show what would change
        #[arg(long)]
        dry_run: bool,
        /// Remove the entry instead of adding it
        #[arg(long)]
        remove: bool,
        /// Check the recorded config and exit 1 if it is wrong
        #[arg(long)]
        check: bool,
    },
}

/// Что создаётся командой `tasq create`.
#[derive(Subcommand)]
enum CreateWhat {
    /// Create a project in the current database
    Project {
        /// Project key used in dot-notation IDs ([A-Z0-9]{2,8}); case is normalised
        key: String,
        /// Human-readable name (default: the key)
        #[arg(long)]
        name: Option<String>,
    },
}

/// Что перечисляет команда `tasq list`.
#[derive(Subcommand)]
enum ListWhat {
    /// Projects of the current database, with live node counts
    Projects,
}

/// Что меняет команда `tasq update`.
#[derive(Subcommand)]
enum UpdateWhat {
    /// Rename or re-status a project
    Project {
        /// Project to change (default: the --project value, else the first one)
        key: Option<String>,
        /// New human-readable name
        #[arg(long)]
        name: Option<String>,
        /// New description
        #[arg(long)]
        description: Option<String>,
        /// New status: active, on_hold, completed, archived
        #[arg(long)]
        status: Option<String>,
        /// New key; every dot-ID of the project changes with it
        #[arg(long, value_name = "KEY")]
        new_key: Option<String>,
    },
}

/// Что удаляет команда `tasq delete`.
#[derive(Subcommand)]
enum DeleteWhat {
    /// Delete a project with all of its nodes, notes and history
    Project {
        /// Project to delete (default: the --project value, else the first one)
        key: Option<String>,
        /// Delete even when the project still has live nodes or is the last one
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum NoteCommand {
    /// Save a note: without --id it creates, with --id it updates
    Save {
        /// Note title
        #[arg(long)]
        title: Option<String>,
        /// Note body
        #[arg(long)]
        content: Option<String>,
        /// Type: general | decision | context | meeting | technical | blocker | progress | release
        #[arg(long = "type")]
        note_type: Option<String>,
        /// Attach to a node
        #[arg(long, value_name = "REF")]
        node: Option<String>,
        /// Update this note instead of creating one
        #[arg(long)]
        id: Option<i64>,
    },
    /// List notes, newest first
    Ls {
        #[arg(long = "type")]
        note_type: Option<String>,
        #[arg(long, value_name = "REF")]
        node: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    /// Search notes by keyword
    Search { query: String },
    /// Delete a note
    Rm { id: i64 },
}

#[derive(Subcommand)]
enum CommentCommand {
    /// Add a comment to a node
    Add {
        reference: String,
        content: String,
        /// Author name; defaults to $TASQ_HOLDER or `cli`
        #[arg(long, env = "TASQ_HOLDER")]
        author: Option<String>,
    },
    /// List comments of a node
    Ls {
        reference: String,
        /// Show soft-deleted comments with their reasons
        #[arg(long)]
        include_deleted: bool,
    },
    /// Soft-delete a comment; the row stays for the audit trail
    Rm {
        id: i64,
        /// Why it is removed
        #[arg(long)]
        reason: Option<String>,
    },
    /// Bring a soft-deleted comment back
    Restore { id: i64 },
}

#[derive(Subcommand)]
enum TemplateCommand {
    /// Save a template from a JSON file or inline JSON
    Save {
        name: String,
        /// Path to a JSON file with the template tree
        #[arg(long, value_name = "PATH")]
        file: Option<String>,
        /// Inline JSON (array of nodes or {"nodes": [...]})
        #[arg(long = "tree", value_name = "JSON")]
        tree: Option<String>,
        #[arg(long)]
        description: Option<String>,
    },
    /// List templates with their variables
    Ls,
    /// Show one template with its tree
    Show { name: String },
    /// Create nodes from a template
    Apply {
        name: String,
        /// Parent node; without it the roots land at the project top level
        #[arg(long, value_name = "REF")]
        parent: Option<String>,
        /// Variable value as name=value (repeatable)
        #[arg(long = "var", value_name = "NAME=VALUE")]
        vars: Vec<String>,
    },
    /// Delete a template
    Rm { name: String },
}

#[derive(Subcommand)]
enum BulkCommand {
    /// Move several nodes to one status; failures are reported per node
    Set {
        /// Target status
        #[arg(long)]
        status: String,
        /// Node references
        #[arg(required = true, value_name = "REF")]
        refs: Vec<String>,
        #[arg(long)]
        force: bool,
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
///
/// Проект выбирается по `--project`/`TASQ_PROJECT`, иначе берётся первый по порядку
/// создания — так работают существующие базы и скрипты.
fn open(
    db_path: Option<&std::path::Path>,
    project_key: Option<&str>,
) -> Result<(Connection, Project)> {
    let cwd = std::env::current_dir()?;
    let path = db::discover(&cwd, db_path)?;
    let conn = db::open_checked(&path)?;
    let project = project::select(&conn, project_key)?;
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

/// `tasq connect` — прописка сервера в конфиг клиента.
#[allow(clippy::too_many_arguments)]
fn run_connect(
    cli: &Cli,
    client: Option<&str>,
    scope: Option<&str>,
    url: Option<&str>,
    dry_run: bool,
    remove: bool,
    check: bool,
) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;

    // Без идентификатора клиента команда ничего не пишет: сначала показываем,
    // у кого вообще есть конфиг, иначе `connect` норовил бы создать файлы всем подряд.
    let Some(client_id) = client else {
        let states = connect::states(&cwd);
        if cli.json {
            print_json(&states);
        } else {
            println!(
                "{:<16} {:<8} {:<5} {:<5} path",
                "client", "scope", "file", "tasq"
            );
            for state in &states {
                println!(
                    "{:<16} {:<8} {:<5} {:<5} {}",
                    state.client,
                    state.scope,
                    if state.file_exists { "yes" } else { "no" },
                    if state.configured { "yes" } else { "no" },
                    state.path
                );
            }
            println!();
            println!("ничего не записано: укажите клиента, например `tasq connect claude-code`");
        }
        return Ok(ExitCode::SUCCESS);
    };

    let client = connect::client(client_id).ok_or_else(|| Error::InvalidValue {
        field: "client".to_string(),
        value: format!(
            "{client_id} (known: {})",
            connect::CLIENTS
                .iter()
                .map(|client| client.id)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    })?;

    let scope = match scope {
        Some(value) => connect::Scope::parse(value).ok_or_else(|| Error::InvalidValue {
            field: "scope".to_string(),
            value: format!("{value} (allowed: user, project)"),
        })?,
        None => client.default_scope,
    };

    if check {
        let ok = connect::verify(client, scope, &cwd)?;
        let path = connect::path_for(client, scope, &cwd)?;
        if cli.json {
            print_json(&serde_json::json!({
                "client": client.id,
                "path": path.display().to_string(),
                "configured": ok,
            }));
        } else if ok {
            println!("{}: tasq is configured in {}", client.id, path.display());
        } else {
            println!("{}: tasq is missing from {}", client.id, path.display());
        }
        return Ok(if ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        });
    }

    let transport = match url {
        Some(url) => {
            // HTTP-транспорт разрешён только на петле: наружу MCP-сервер не отдаётся молча.
            let loopback = url.starts_with("http://127.0.0.1")
                || url.starts_with("http://localhost")
                || url.starts_with("http://[::1]")
                || url.starts_with("https://127.0.0.1");
            if !loopback {
                return Err(Error::InvalidValue {
                    field: "url".to_string(),
                    value: format!("{url} (loopback address required)"),
                });
            }
            connect::Transport::Http {
                url: url.to_string(),
            }
        }
        None => connect::Transport::Stdio {
            command: std::env::current_exe()?.display().to_string(),
            args: vec!["mcp".to_string()],
        },
    };

    // Удаление и правка требуют существующего файла: создавать его ради удаления бессмысленно.
    let path = connect::path_for(client, scope, &cwd)?;
    if (remove || !dry_run) && !path.is_file() && client.created_by_client {
        return Err(connect::missing_file(client, &path));
    }

    let outcome = connect::apply(client, scope, &cwd, &transport, dry_run, remove)?;

    let code = match outcome.action.as_str() {
        "manual" | "unparsable" => ExitCode::from(3),
        _ => ExitCode::SUCCESS,
    };

    if cli.json {
        print_json(&outcome);
        return Ok(code);
    }

    match outcome.action.as_str() {
        "written" => println!("{}: written to {}", outcome.client, outcome.path),
        "removed" => println!("{}: removed from {}", outcome.client, outcome.path),
        "would-write" => println!("{}: would write to {}", outcome.client, outcome.path),
        "would-remove" => println!("{}: would remove from {}", outcome.client, outcome.path),
        "unchanged" => println!("{}: already configured in {}", outcome.client, outcome.path),
        "manual" | "unparsable" => {
            println!("{}: {} not touched", outcome.client, outcome.path);
            println!("причина: формат не позволяет безопасную правку");
            if let Some(snippet) = &outcome.snippet {
                println!("{snippet}");
            }
        }
        other => println!("{}: {other} ({})", outcome.client, outcome.path),
    }
    if let Some(backup) = &outcome.backup {
        println!("backup: {backup}");
    }
    Ok(code)
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
    // `connect` работает с конфигами клиентов и не требует базы: его можно запускать
    // в проекте, где `tasq init` ещё не делали.
    // `docs` тоже не нуждается в базе проекта: схема снимается с пустой временной базы.
    if let Command::Docs { out, check } = &cli.command {
        let files = docs::generate(&Cli::command())?;
        if *check {
            let drift = docs::check(out, &files)?;
            if !drift.is_empty() {
                if cli.json {
                    print_json(&serde_json::json!({"ok": false, "drift": drift}));
                } else {
                    for item in &drift {
                        eprintln!("{item}");
                    }
                    eprintln!("run `tasq docs` to regenerate");
                }
                return Ok(ExitCode::from(3));
            }
            if !cli.json {
                println!("docs: up to date ({})", out.display());
            }
            return Ok(ExitCode::SUCCESS);
        }
        let written = docs::write(out, &files)?;
        if cli.json {
            let names: Vec<String> = written
                .iter()
                .map(|path| path.display().to_string())
                .collect();
            print_json(&serde_json::json!({"ok": true, "written": names}));
            return Ok(ExitCode::SUCCESS);
        }
        for path in &written {
            println!("{}", path.display());
        }
        return Ok(ExitCode::SUCCESS);
    }

    if let Command::Connect {
        client,
        scope,
        url,
        dry_run,
        remove,
        check,
    } = &cli.command
    {
        return run_connect(
            cli,
            client.as_deref(),
            scope.as_deref(),
            url.as_deref(),
            *dry_run,
            *remove,
            *check,
        );
    }

    match &cli.command {
        Command::Init { key, name } => {
            let cwd = std::env::current_dir()?;
            let name = name.clone().unwrap_or_else(|| key.clone());
            // Явный путь (`--db` или `TASQ_DB`) уважается и здесь: иначе флаг работал бы
            // для всех команд, кроме той, которая создаёт базу.
            let outcome = match cli.db.as_deref() {
                Some(path) => {
                    let path = if path.is_absolute() {
                        path.to_path_buf()
                    } else {
                        cwd.join(path)
                    };
                    db::init_at(&path, key, &name)?
                }
                None => db::init(&cwd, key, &name)?,
            };
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

        Command::Create { what } => match what {
            CreateWhat::Project { key, name } => {
                let cwd = std::env::current_dir()?;
                let path = db::discover(&cwd, cli.db.as_deref())?;
                let mut conn = db::open_checked(&path)?;
                let project = project::create(&mut conn, key, name.as_deref())?;
                if cli.json {
                    print_json(&project);
                } else {
                    println!("created project {} ({})", project.key, project.name);
                    println!();
                    println!("to work in it, pass the key explicitly:");
                    println!("  tasq --project {} ls", project.key);
                    println!("  export TASQ_PROJECT={}", project.key);
                }
                Ok(ExitCode::SUCCESS)
            }
        },

        Command::List { what } => match what {
            ListWhat::Projects => {
                let cwd = std::env::current_dir()?;
                let path = db::discover(&cwd, cli.db.as_deref())?;
                let conn = db::open_checked(&path)?;
                let projects = project::list(&conn)?;
                if cli.json {
                    print_json(&projects);
                    return Ok(ExitCode::SUCCESS);
                }
                if projects.is_empty() {
                    println!("no projects; create one with `tasq create project KEY`");
                    return Ok(ExitCode::SUCCESS);
                }
                // Пометка активного проекта: иначе список не отвечает на вопрос,
                // в каком проекте пройдёт следующая команда.
                let active = project::select(&conn, cli.project.as_deref()).ok();
                println!(
                    "{:<4} {:<10} {:<10} {:>5}  name",
                    "", "key", "status", "nodes"
                );
                for item in &projects {
                    let mark = match &active {
                        Some(current) if current.id == item.id => "*",
                        _ => " ",
                    };
                    println!(
                        "{mark:<4} {:<10} {:<10} {:>5}  {}",
                        item.key, item.status, item.nodes, item.name
                    );
                }
                Ok(ExitCode::SUCCESS)
            }
        },

        Command::Update { what } => match what {
            UpdateWhat::Project {
                key,
                name,
                description,
                status,
                new_key,
            } => {
                let cwd = std::env::current_dir()?;
                let path = db::discover(&cwd, cli.db.as_deref())?;
                let mut conn = db::open_checked(&path)?;
                // Порядок: явный ключ, затем глобальный --project, затем проект по умолчанию.
                let target = key.as_deref().or(cli.project.as_deref());
                let patch = project::Patch {
                    name: name.as_deref(),
                    description: description.as_deref(),
                    status: status.as_deref(),
                    key: new_key.as_deref(),
                };
                let (updated, previous_key) = project::update(&mut conn, target, &patch)?;
                if cli.json {
                    print_json(&serde_json::json!({
                        "project": updated,
                        "previous_key": previous_key,
                        "nodes": project::live_nodes(&conn, &updated)?,
                    }));
                    return Ok(ExitCode::SUCCESS);
                }
                if updated.key == previous_key {
                    println!("updated project {} ({})", updated.key, updated.name);
                } else {
                    let nodes = project::live_nodes(&conn, &updated)?;
                    println!(
                        "project {} renamed to {} ({}, {} node(s))",
                        previous_key, updated.key, updated.name, nodes
                    );
                    if nodes > 0 {
                        // Ключ входит в dot-ID, поэтому переименование ключа меняет все адреса.
                        println!(
                            "all dot-IDs of this project now start with {}-",
                            updated.key
                        );
                    }
                }
                Ok(ExitCode::SUCCESS)
            }
        },

        Command::Delete { what } => match what {
            DeleteWhat::Project { key, force } => {
                let cwd = std::env::current_dir()?;
                let path = db::discover(&cwd, cli.db.as_deref())?;
                let mut conn = db::open_checked(&path)?;
                let target = key.as_deref().or(cli.project.as_deref());
                let nodes = {
                    let project = project::select(&conn, target)?;
                    project::live_nodes(&conn, &project)?
                };
                let removed = project::delete(&mut conn, target, *force)?;
                if cli.json {
                    print_json(&serde_json::json!({
                        "deleted": removed.key,
                        "name": removed.name,
                        "nodes": nodes,
                    }));
                    return Ok(ExitCode::SUCCESS);
                }
                println!(
                    "deleted project {} ({}) with {} live node(s)",
                    removed.key, removed.name, nodes
                );
                println!("remaining: {}", project::known_keys(&conn)?);
                Ok(ExitCode::SUCCESS)
            }
        },

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
                let project = project::select(&conn, cli.project.as_deref())?;
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
            let (mut conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
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
            let (conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
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
            let (conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
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
            let (mut conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
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
            let (mut conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
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
            let (mut conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
            node::delete(&mut conn, &project, reference)?;
            if !cli.json {
                println!("removed {reference}");
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Restore { reference } => {
            let (mut conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
            let restored = node::restore(&mut conn, &project, reference)?;
            if cli.json {
                print_json(&restored);
            } else {
                println!("restored {}", restored.dot_id);
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Archive { reference, undo } => {
            let (mut conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
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
            let (conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
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
            let (mut conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
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
            let (mut conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
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
            let (mut conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
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
            let (mut conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
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
            let (mut conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
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
            let (conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
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
                let (mut conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
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
                let (mut conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
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
                let (conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
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
                let (conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
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
            let (conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
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

        Command::Note { command } => {
            let (mut conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
            match command {
                NoteCommand::Save {
                    title,
                    content,
                    note_type,
                    node,
                    id,
                } => {
                    let saved = note::save(
                        &mut conn,
                        &project,
                        note::SaveNote {
                            id: *id,
                            title: title.as_deref(),
                            content: content.as_deref(),
                            note_type: note_type.as_deref(),
                            node: node.as_deref(),
                            actor: None,
                        },
                    )?;
                    if cli.json {
                        print_json(&saved);
                    } else {
                        println!("#{} {:<10} {}", saved.id, saved.note_type, saved.title);
                        if let Some(dot) = &saved.dot_id {
                            println!("   about {dot}");
                        }
                    }
                    Ok(ExitCode::SUCCESS)
                }
                NoteCommand::Ls {
                    note_type,
                    node,
                    limit,
                } => {
                    let notes = note::list(
                        &conn,
                        &project,
                        &note::Filter {
                            note_type: note_type.as_deref(),
                            node: node.as_deref(),
                            limit: Some(*limit),
                        },
                    )?;
                    if cli.json {
                        print_json(&notes);
                        return Ok(ExitCode::SUCCESS);
                    }
                    if notes.is_empty() {
                        println!("no notes");
                        return Ok(ExitCode::SUCCESS);
                    }
                    for note in &notes {
                        let about = note.dot_id.as_deref().unwrap_or("-");
                        println!(
                            "#{:<4} {:<10} {:<12} {}",
                            note.id, note.note_type, about, note.title
                        );
                        let preview: String = note
                            .content
                            .lines()
                            .next()
                            .unwrap_or("")
                            .chars()
                            .take(70)
                            .collect();
                        if !preview.is_empty() {
                            println!("      {preview}");
                        }
                    }
                    Ok(ExitCode::SUCCESS)
                }
                NoteCommand::Search { query } => {
                    let notes = note::search(&conn, &project, query)?;
                    if cli.json {
                        print_json(&notes);
                        return Ok(ExitCode::SUCCESS);
                    }
                    if notes.is_empty() {
                        println!("nothing found for {query:?}");
                        return Ok(ExitCode::SUCCESS);
                    }
                    for note in &notes {
                        println!("#{:<4} {:<10} {}", note.id, note.note_type, note.title);
                    }
                    Ok(ExitCode::SUCCESS)
                }
                NoteCommand::Rm { id } => {
                    let removed = note::delete(&mut conn, &project, *id, "-")?;
                    if !cli.json {
                        if removed {
                            println!("removed note #{id}");
                        } else {
                            println!("no note #{id}");
                        }
                    }
                    Ok(if removed {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::from(1)
                    })
                }
            }
        }

        Command::Comment { command } => {
            let (mut conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
            match command {
                CommentCommand::Add {
                    reference,
                    content,
                    author,
                } => {
                    let author = author.clone().unwrap_or_else(|| "cli".to_string());
                    let comment = comment::add(&mut conn, &project, reference, content, &author)?;
                    if cli.json {
                        print_json(&comment);
                    } else {
                        println!("#{} {}: {}", comment.id, comment.dot_id, comment.content);
                    }
                    Ok(ExitCode::SUCCESS)
                }
                CommentCommand::Ls {
                    reference,
                    include_deleted,
                } => {
                    let comments = comment::list(&conn, &project, reference, *include_deleted)?;
                    if cli.json {
                        print_json(&comments);
                        return Ok(ExitCode::SUCCESS);
                    }
                    if comments.is_empty() {
                        println!("no comments");
                        return Ok(ExitCode::SUCCESS);
                    }
                    for comment in &comments {
                        let author = comment.author.as_deref().unwrap_or("-");
                        let mark = if comment.is_deleted { " (removed)" } else { "" };
                        println!(
                            "#{} {} {}:{mark} {}",
                            comment.id, comment.created_at, author, comment.content
                        );
                        if let Some(reason) = &comment.delete_reason {
                            println!("      reason: {reason}");
                        }
                    }
                    Ok(ExitCode::SUCCESS)
                }
                CommentCommand::Rm { id, reason } => {
                    let comment =
                        comment::delete(&mut conn, &project, *id, "-", reason.as_deref())?;
                    if cli.json {
                        print_json(&comment);
                    } else {
                        println!("removed comment #{} (kept for the audit trail)", comment.id);
                    }
                    Ok(ExitCode::SUCCESS)
                }
                CommentCommand::Restore { id } => {
                    let comment = comment::restore(&mut conn, &project, *id, "-")?;
                    if cli.json {
                        print_json(&comment);
                    } else {
                        println!("restored comment #{}", comment.id);
                    }
                    Ok(ExitCode::SUCCESS)
                }
            }
        }

        Command::Find { query } => {
            let (conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
            let hits = overview::search(&conn, &project, query)?;
            if cli.json {
                print_json(&hits);
                return Ok(ExitCode::SUCCESS);
            }
            if hits.is_empty() {
                println!("nothing found for {query:?}");
                return Ok(ExitCode::SUCCESS);
            }
            for hit in &hits {
                let target = hit.dot_id.as_deref().unwrap_or("-");
                println!("{:<6} {:<14} {}", hit.entity, target, hit.title);
                if !hit.snippet.is_empty() {
                    println!("       {}", hit.snippet);
                }
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Ctx { reference } => {
            let (conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
            let chain = overview::context(&conn, &project, reference)?;
            if cli.json {
                print_json(&chain);
                return Ok(ExitCode::SUCCESS);
            }
            let last = chain.len().saturating_sub(1);
            for (index, entry) in chain.iter().enumerate() {
                let marker = if index == last { "->" } else { "  " };
                println!(
                    "{marker} {:<14} {:<9} {:<11} {}",
                    entry.dot_id, entry.kind, entry.status, entry.title
                );
                if let Some(description) = &entry.description {
                    for line in description.lines().take(6) {
                        println!("       {line}");
                    }
                }
                if let Some(due) = &entry.due_date {
                    println!("       due {due}");
                }
                if let Some(holder) = &entry.lease_holder {
                    println!("       held by {holder}");
                }
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Board => {
            let (conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
            let board = overview::board(&conn, &project)?;
            if cli.json {
                print_json(&board);
                return Ok(ExitCode::SUCCESS);
            }
            println!(
                "{}  open {}  done {}  overdue {}  leased {}",
                board.project, board.open, board.done, board.overdue, board.leased
            );
            for column in &board.columns {
                println!("\n{} ({})", column.status, column.count);
                for row in &column.rows {
                    let mut flags = String::new();
                    if row.days_overdue > 0 {
                        flags.push_str(&format!(" OVERDUE {}d", row.days_overdue));
                    }
                    if let Some(holder) = &row.lease_holder {
                        flags.push_str(&format!(" [{holder}]"));
                    }
                    let due = row.due_date.as_deref().unwrap_or("");
                    println!(
                        "  {:<14} {:<8} {:>5} {:<10} {}{}",
                        row.dot_id, row.priority, row.progress, due, row.title, flags
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Time { parent } => {
            let (conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
            let rows = overview::time_report(&conn, &project, parent.as_deref())?;
            if cli.json {
                print_json(&rows);
                return Ok(ExitCode::SUCCESS);
            }
            let mut total_estimate = 0.0;
            let mut total_actual = 0.0;
            let mut any = false;
            for row in &rows {
                if row.attempts == 0 && row.estimated_hours.is_none() {
                    continue;
                }
                any = true;
                let estimate = row
                    .estimated_hours
                    .map(|value| format!("{value:.1}h"))
                    .unwrap_or_else(|| "-".to_string());
                println!(
                    "{:<14} {:>7} est  {:>7} fact  {} attempt(s)  {}",
                    row.dot_id,
                    estimate,
                    format!("{:.2}h", row.actual_hours),
                    row.attempts,
                    row.title
                );
                total_estimate += row.estimated_hours.unwrap_or(0.0);
                total_actual += row.actual_hours;
            }
            if !any {
                println!("no tracked work yet: claim a node to start the clock");
            } else {
                println!(
                    "\ntotal: {:.1}h estimated, {:.2}h actual",
                    total_estimate, total_actual
                );
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Bulk { command } => match command {
            BulkCommand::Set {
                status,
                refs,
                force,
            } => {
                let target = parse_status(status)?;
                let (mut conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
                let result = overview::bulk_status(&mut conn, &project, refs, target, *force, "-")?;
                overview::log_bulk(
                    &conn,
                    &project,
                    "bulk",
                    &format!(
                        "{} node(s) -> {}; {} failed",
                        result.updated.len(),
                        target.as_str(),
                        result.failed.len()
                    ),
                    "-",
                )?;
                if cli.json {
                    print_json(&result);
                } else {
                    for dot in &result.updated {
                        println!("{dot} -> {}", target.as_str());
                    }
                    for failure in &result.failed {
                        println!("{}: {} ({})", failure.dot_id, failure.message, failure.code);
                    }
                    println!(
                        "updated {}, failed {}",
                        result.updated.len(),
                        result.failed.len()
                    );
                }
                Ok(if result.failed.is_empty() {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::from(1)
                })
            }
        },

        Command::Burnup {
            metric,
            since,
            until,
            ascii,
            snapshot,
            width,
        } => {
            let metric = burnup::Metric::parse(metric).ok_or_else(|| Error::InvalidValue {
                field: "metric".to_string(),
                value: format!("{metric} (allowed: count, points, hours)"),
            })?;
            let (mut conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
            let series =
                burnup::series(&conn, &project, metric, since.as_deref(), until.as_deref())?;

            if *snapshot {
                let written = burnup::snapshot_all(&mut conn, &project, metric)?;
                if !cli.json {
                    println!("snapshots written: {written}\n");
                }
            }

            if cli.json {
                print_json(&series);
                return Ok(ExitCode::SUCCESS);
            }

            println!(
                "{}  metric {}  scope {:.1}  done {:.1}  added {:.1}",
                series.project,
                series.metric,
                series.scope_total,
                series.scope_done,
                series.scope_added
            );
            if let Some(days) = series.days_left {
                if days < 0 {
                    println!("deadline passed {} day(s) ago", -days);
                } else {
                    println!("deadline in {days} day(s)");
                }
            }
            if let Some(forecast) = &series.forecast {
                let tail = match forecast.days_late {
                    Some(late) if late > 0 => format!(", {late} day(s) late"),
                    Some(late) if late < 0 => format!(", {} day(s) early", -late),
                    Some(_) => ", on time".to_string(),
                    None => String::new(),
                };
                println!(
                    "forecast {} at {:.2}/day over {} day(s){tail}",
                    forecast.day, forecast.rate_per_day, forecast.window_days
                );
            } else if series.scope_done < series.scope_total {
                println!("forecast: no completed work in the window, cannot extrapolate");
            }

            if *ascii {
                println!();
                print!("{}", burnup::ascii(&series, *width));
            } else {
                // Без графика — компактная таблица последних точек.
                let tail = series.points.iter().rev().take(7).collect::<Vec<_>>();
                for point in tail.iter().rev() {
                    println!(
                        "  {}  total {:>6.1}  done {:>6.1}  added {:>5.1}",
                        point.day, point.total, point.done, point.added
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Risk { threshold } => {
            if !(0.0..=1.0).contains(threshold) {
                return Err(Error::InvalidValue {
                    field: "threshold".to_string(),
                    value: format!("{threshold} (expected 0..1)"),
                });
            }
            let (conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
            let risky = burnup::at_risk(&conn, &project, *threshold)?;
            if cli.json {
                print_json(&risky);
                return Ok(ExitCode::SUCCESS);
            }
            if risky.is_empty() {
                println!("no node is behind its own deadline");
                return Ok(ExitCode::SUCCESS);
            }
            for node in &risky {
                let left = if node.days_left < 0 {
                    format!("{}d overdue", -node.days_left)
                } else {
                    format!("{}d left", node.days_left)
                };
                println!(
                    "{:<14} {:<10} done {:>3.0}% elapsed {:>3.0}% gap {:>3.0}%  {}",
                    node.dot_id,
                    left,
                    node.done * 100.0,
                    node.elapsed * 100.0,
                    node.gap * 100.0,
                    node.title
                );
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Export {
            out,
            include_deleted,
        } => {
            let (conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
            let document = transfer::export(&conn, &project, *include_deleted)?;
            let text = serde_json::to_string_pretty(&document).expect("serialisable");
            match out {
                Some(path) => {
                    std::fs::write(path, &text)?;
                    if !cli.json {
                        let summary = transfer::describe(&document);
                        println!(
                            "exported to {}: {} node(s), {} link(s), {} comment(s), {} note(s), {} template(s)",
                            path.display(),
                            summary["nodes"],
                            summary["edges"],
                            summary["comments"],
                            summary["notes"],
                            summary["templates"]
                        );
                        println!(
                            "sha256 {}",
                            document
                                .get("sha256")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("-")
                        );
                    }
                }
                None => println!("{text}"),
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Import {
            file,
            mode,
            from_saga,
            dry_run,
        } => {
            let mode_value = transfer::Mode::parse(mode).ok_or_else(|| Error::InvalidValue {
                field: "mode".to_string(),
                value: format!("{mode} (allowed: merge, replace)"),
            })?;
            let raw = std::fs::read_to_string(file)?;
            let document: serde_json::Value =
                serde_json::from_str(&raw).map_err(|err| Error::InvalidValue {
                    field: "file".to_string(),
                    value: format!("not valid JSON: {err}"),
                })?;

            let (mut conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
            let result = if *from_saga {
                if *dry_run {
                    let nodes = document
                        .get("project")
                        .and_then(|value| value.get("epics"))
                        .and_then(serde_json::Value::as_array)
                        .map(Vec::len)
                        .unwrap_or(0);
                    transfer::Imported {
                        mode: "saga dry-run".to_string(),
                        nodes_created: nodes,
                        nodes_skipped: 0,
                        edges: 0,
                        comments: 0,
                        notes: 0,
                        templates: 0,
                        checksum_verified: false,
                    }
                } else {
                    transfer::import_saga(&mut conn, &project, &document, "-")?
                }
            } else if *dry_run {
                transfer::dry_run(&conn, &project, &document)?
            } else {
                transfer::import(&mut conn, &project, &document, mode_value, "-")?
            };

            if cli.json {
                print_json(&result);
                return Ok(ExitCode::SUCCESS);
            }
            println!(
                "{}: {} node(s) created, {} skipped, {} link(s), {} comment(s), {} note(s), {} template(s)",
                result.mode,
                result.nodes_created,
                result.nodes_skipped,
                result.edges,
                result.comments,
                result.notes,
                result.templates
            );
            if result.checksum_verified {
                println!("checksum verified");
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Tokens {
            profile,
            max,
            per_tool,
        } => {
            let reports = match profile {
                Some(value) => {
                    let profile =
                        mcp::profile::Profile::parse(value).ok_or_else(|| Error::InvalidValue {
                            field: "profile".to_string(),
                            value: format!("{value} (allowed: full, agent, read-only, migration)"),
                        })?;
                    vec![budget::measure(profile)]
                }
                None => budget::measure_all(),
            };

            if cli.json {
                print_json(&reports);
            } else {
                for report in &reports {
                    let mark = if report.within_budget { "ok" } else { "OVER" };
                    println!(
                        "{:<10} {:>3} tool(s)  {:>5} tokens  budget {:>5}  headroom {:>5}  {mark}",
                        report.profile, report.tools, report.tokens, report.budget, report.headroom
                    );
                    if *per_tool {
                        for cost in report.per_tool.iter().take(10) {
                            println!("    {:>5}  {}", cost.tokens, cost.name);
                        }
                    }
                }
            }

            // Гейт: `--max` проверяет каждый показанный профиль.
            let over: Vec<&budget::Report> = match max {
                Some(limit) => reports
                    .iter()
                    .filter(|report| report.tokens > *limit)
                    .collect(),
                None => reports.iter().filter(|r| !r.within_budget).collect(),
            };
            if !over.is_empty() {
                if !cli.json {
                    for report in &over {
                        eprintln!(
                            "budget exceeded: {} has {} tokens (limit {})",
                            report.profile,
                            report.tokens,
                            max.map(|value| value.to_string())
                                .unwrap_or_else(|| report.budget.to_string())
                        );
                    }
                }
                return Ok(ExitCode::from(1));
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Connect { .. } => unreachable!("обработан до открытия базы"),
        Command::Docs { .. } => unreachable!("обработан до открытия базы"),

        Command::Mcp { profile } => {
            let profile =
                mcp::profile::Profile::parse(profile).ok_or_else(|| Error::InvalidValue {
                    field: "profile".to_string(),
                    value: format!("{profile} (allowed: full, agent, read-only, migration)"),
                })?;
            let cwd = std::env::current_dir()?;
            let flush = if cli.flush == 0 {
                None
            } else {
                Some(std::time::Duration::from_secs(cli.flush))
            };
            mcp::serve(
                &cwd,
                cli.db.as_deref(),
                profile,
                cli.project.as_deref(),
                flush,
            )?;
            Ok(ExitCode::SUCCESS)
        }

        Command::Template { command } => {
            let (mut conn, project) = open(cli.db.as_deref(), cli.project.as_deref())?;
            match command {
                TemplateCommand::Save {
                    name,
                    file,
                    tree,
                    description,
                } => {
                    let raw = match (file, tree) {
                        (Some(path), _) => std::fs::read_to_string(path)?,
                        (None, Some(inline)) => inline.clone(),
                        (None, None) => {
                            return Err(Error::InvalidValue {
                                field: "template".to_string(),
                                value: "pass --file or --tree".to_string(),
                            });
                        }
                    };
                    let saved = template::save(
                        &mut conn,
                        &project,
                        name,
                        description.as_deref(),
                        &raw,
                        "-",
                    )?;
                    if cli.json {
                        print_json(&saved);
                    } else {
                        println!(
                            "template {}: {} root node(s), variables: {}",
                            saved.name,
                            saved.tree.nodes.len(),
                            if saved.variables.is_empty() {
                                "-".to_string()
                            } else {
                                saved
                                    .variables
                                    .iter()
                                    .map(|v| format!("{{{v}}}"))
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            }
                        );
                    }
                    Ok(ExitCode::SUCCESS)
                }
                TemplateCommand::Ls => {
                    let templates = template::list(&conn, &project)?;
                    if cli.json {
                        print_json(&templates);
                        return Ok(ExitCode::SUCCESS);
                    }
                    if templates.is_empty() {
                        println!("no templates");
                        return Ok(ExitCode::SUCCESS);
                    }
                    for template in &templates {
                        println!(
                            "{:<24} {:>2} root(s)  {}",
                            template.name,
                            template.tree.nodes.len(),
                            template.variables.join(" ")
                        );
                    }
                    Ok(ExitCode::SUCCESS)
                }
                TemplateCommand::Show { name } => {
                    let template = template::by_name(&conn, &project, name)?;
                    if cli.json {
                        print_json(&template);
                        return Ok(ExitCode::SUCCESS);
                    }
                    println!("{}", template.name);
                    if let Some(description) = &template.description {
                        println!("description {description}");
                    }
                    if !template.variables.is_empty() {
                        println!(
                            "variables   {}",
                            template
                                .variables
                                .iter()
                                .map(|v| format!("{{{v}}}"))
                                .collect::<Vec<_>>()
                                .join(" ")
                        );
                    }
                    print_template_nodes(&template.tree.nodes, 0);
                    Ok(ExitCode::SUCCESS)
                }
                TemplateCommand::Apply { name, parent, vars } => {
                    let mut values = Vec::new();
                    for pair in vars {
                        let (key, value) =
                            pair.split_once('=').ok_or_else(|| Error::InvalidValue {
                                field: "--var".to_string(),
                                value: format!("{pair}: expected name=value"),
                            })?;
                        values.push((key.trim().to_string(), value.to_string()));
                    }
                    let applied =
                        template::apply(&mut conn, &project, name, parent.as_deref(), &values)?;
                    if cli.json {
                        print_json(&applied);
                    } else {
                        println!(
                            "applied {}: {} node(s)",
                            applied.template,
                            applied.created.len()
                        );
                        for dot in &applied.created {
                            println!("  {dot}");
                        }
                    }
                    Ok(ExitCode::SUCCESS)
                }
                TemplateCommand::Rm { name } => {
                    let removed = template::delete(&mut conn, &project, name, "-")?;
                    if !cli.json {
                        println!(
                            "{}",
                            if removed {
                                format!("removed template {name}")
                            } else {
                                format!("no template {name}")
                            }
                        );
                    }
                    Ok(if removed {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::from(1)
                    })
                }
            }
        }
    }
}

fn print_template_nodes(nodes: &[template::TemplateNode], indent: usize) {
    for node in nodes {
        let kind = node.kind.as_deref().unwrap_or("auto");
        let priority = node.priority.as_deref().unwrap_or("-");
        println!(
            "{:indent$}{:<8} {:<8} {}",
            "",
            kind,
            priority,
            node.title,
            indent = indent
        );
        print_template_nodes(&node.children, indent + 2);
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
