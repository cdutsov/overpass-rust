use anyhow::Result;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use clap::{Parser, Subcommand};
use overpass::import::{ImportOptions, ImportReport, import_file_with, set_base_timestamp};
use overpass::exec::{RunError, RunOptions};
use overpass::store::Db;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Parser)]
#[command(name = "overpass", about = "Overpass API server and tools")]
struct Cli {
    /// Database directory.
    #[arg(long, global = true, default_value = "data/db")]
    db: PathBuf,
    /// Worker threads for parallel decoding and formatting (default: half the cores, at most 8; 1 = serial).
    #[arg(long, global = true, env = "OVERPASS_THREADS")]
    threads: Option<usize>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Load .osm.pbf, .osm or .osc files (.osm/.osc also as .gz or .bz2; `-` for
    /// OSM XML on stdin). Into an empty database, a sorted OSM file (nodes,
    /// ways, relations) is bulk loaded; this scales to the whole planet.
    Import {
        files: Vec<PathBuf>,
        /// Data timestamp for output headers (default: replication timestamp
        /// of a .pbf header, else the newest element timestamp).
        #[arg(long)]
        timestamp: Option<String>,
        /// Store meta data (version, timestamp, changeset, user): yes or no.
        #[arg(long, default_value = "yes", value_parser = ["yes", "no"])]
        meta: String,
        /// Memory for sorting during a bulk import, in MiB.
        #[arg(long, default_value_t = 1024, env = "OVERPASS_IMPORT_MEMORY")]
        memory: usize,
        /// Directory for temporary files (default: inside the database directory).
        #[arg(long)]
        tmp: Option<PathBuf>,
    },
    /// Apply OsmChange files in one transaction.
    Update {
        files: Vec<PathBuf>,
        #[arg(long)]
        timestamp: Option<String>,
    },
    /// Keep the database current with replication diffs (minutely, hourly or
    /// daily), e.g. https://planet.openstreetmap.org/replication/minute.
    Replicate {
        /// Replication base URL (default: from the header of the imported .pbf).
        #[arg(long)]
        url: Option<String>,
        /// Continue after this sequence number (default: the last applied one,
        /// else derived from the data timestamp).
        #[arg(long)]
        sequence: Option<u64>,
        /// Apply the available diffs and exit.
        #[arg(long)]
        once: bool,
        /// Seconds between checks for new diffs.
        #[arg(long, default_value_t = 60)]
        interval: u64,
        /// Diffs applied per transaction.
        #[arg(long, default_value_t = 60)]
        batch: usize,
    },
    /// Build areas from multipolygon/boundary relations (C++ areas.osm3s rules),
    /// replacing the stored ones.
    MakeAreas {
        /// Memory for sorting, in MiB.
        #[arg(long, default_value_t = 256)]
        memory: usize,
        /// Print only the summary, not the remarks about broken relations.
        #[arg(long)]
        quiet: bool,
    },
    /// Show the database state (timestamps, replication, size).
    Info,
    /// Rewrite the database file without unused pages.
    Compact,
    /// Serve the Overpass API over HTTP.
    Serve {
        #[arg(long, default_value = "127.0.0.1:8080")]
        listen: String,
        /// Maximum number of queries running at the same time.
        #[arg(long)]
        max_concurrent: Option<usize>,
        /// Upper bound for the [timeout:] a query may request, in seconds.
        #[arg(long, default_value_t = 1800)]
        max_timeout: u32,
        /// Upper bound for the [maxsize:] a query may request, in bytes.
        #[arg(long, default_value_t = 4 << 30)]
        max_maxsize: u64,
        /// Upper bound for the sum of [maxsize:] over the running queries,
        /// in bytes, like the --space of the C++ dispatcher. A query waits
        /// for room as for a slot. Default: no bound.
        #[arg(long)]
        max_space: Option<u64>,
        /// Largest accepted query text, in bytes.
        #[arg(long, default_value_t = 1 << 20)]
        max_query_size: usize,
    },
    /// Run an Overpass QL query (from a file, or stdin when omitted).
    Query {
        file: Option<PathBuf>,
        /// Query text given directly.
        #[arg(long, short = 'q')]
        request: Option<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(n) = cli.threads {
        rayon::ThreadPoolBuilder::new().num_threads(n.max(1)).build_global()?;
        overpass::exec::set_threads(n);
    }
    match cli.cmd {
        Cmd::Import { files, timestamp, meta, memory, tmp } => {
            let db = Db::open(&cli.db)?;
            db.check_complete(&cli.db)?;
            let start = Instant::now();
            let mut report = ImportReport::default();
            let opts = ImportOptions { tmp, memory: memory << 20, meta: meta == "yes" };
            for f in &files {
                import_file_with(&db, f, &opts, &mut report)?;
            }
            set_base_timestamp(&db, timestamp.as_deref(), &report)?;
            eprintln!("applied {} elements in {:.2?}", report.elements, start.elapsed());
            // Chunks rewritten by later batches leave free pages behind.
            let (used, size) = db.usage()?;
            if size - used > (used / 8).max(16 << 20) {
                db.compact(&cli.db)?;
                eprintln!("compacted {} MB to {} MB", size >> 20, used >> 20);
            }
        }
        Cmd::Compact => {
            let db = Db::open(&cli.db)?;
            let (used, size) = db.usage()?;
            db.compact(&cli.db)?;
            eprintln!("compacted {} MB to {} MB", size >> 20, used >> 20);
        }
        Cmd::Update { files, timestamp } => {
            let db = Db::open(&cli.db)?;
            db.check_complete(&cli.db)?;
            let start = Instant::now();
            let mut report = ImportReport::default();
            overpass::import::update_files(&db, &files, timestamp.as_deref(), &mut report)?;
            eprintln!("applied {} elements in {:.2?}", report.elements, start.elapsed());
        }
        Cmd::Replicate { url, sequence, once, interval, batch } => {
            let db = Db::open(&cli.db)?;
            db.check_complete(&cli.db)?;
            let opts = overpass::replicate::ReplicateOptions { url, sequence, once, interval: std::time::Duration::from_secs(interval), batch };
            overpass::replicate::run(&db, &opts)?;
        }
        Cmd::Info => {
            let db = Db::open_read(&cli.db, 8)?;
            let snap = db.snapshot()?;
            for k in ["format", "osm_base", "area_base", "meta", "replication_url", "replication_sequence", "replication_timestamp"] {
                if let Some(v) = snap.info(k)? {
                    println!("{k}: {v}");
                }
            }
            let (used, size) = db.usage()?;
            println!("size: {} MiB ({} MiB in use)", size >> 20, used >> 20);
        }
        Cmd::MakeAreas { memory, quiet } => {
            let db = Db::open(&cli.db)?;
            db.check_complete(&cli.db)?;
            let start = Instant::now();
            let tmp = cli.db.join("areas-tmp");
            let r = overpass::update::areas::make_areas_with(&db, &tmp, memory << 20)?;
            for m in r.remarks.iter().filter(|_| !quiet) {
                eprintln!("runtime remark: {m}");
            }
            eprintln!("built {} of {} candidate areas in {:.2?}", r.created, r.candidates, start.elapsed());
        }
        Cmd::Serve { listen, max_concurrent, max_timeout, max_maxsize, max_space, max_query_size } => {
            let mut cfg = overpass::server::ServerConfig { max_timeout: Some(max_timeout), max_maxsize: Some(max_maxsize), max_space, max_query_len: max_query_size, ..Default::default() };
            if let Some(m) = max_concurrent {
                cfg.max_concurrent = m;
            }
            let db = std::sync::Arc::new(Db::open_read(&cli.db, cfg.max_concurrent as u32 + 16)?);
            // Large stacks: query evaluation recurses over the statement tree.
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(64 << 20).build()?;
            rt.block_on(overpass::server::serve(db, &listen, cfg))?;
        }
        Cmd::Query { file, request } => {
            let src = match (request, file) {
                (Some(r), _) => r,
                (None, Some(f)) => std::fs::read_to_string(f)?,
                (None, None) => {
                    let mut s = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
                    s
                }
            };
            let db = Db::open_read(&cli.db, 8)?;
            let stdout = std::io::stdout();
            let out = std::io::BufWriter::new(stdout.lock());
            let opts = RunOptions { cli: true, ..Default::default() };
            match overpass::exec::run(&db, &src, out, &opts) {
                Ok(_) => {}
                Err((_, RunError::Rejected(m))) => {
                    eprintln!("{m}");
                }
                Err((_, RunError::Runtime(m))) => {
                    eprintln!("{m}");
                    std::process::exit(2);
                }
                Err((_, RunError::Io(e))) => return Err(e.into()),
            }
        }
    }
    Ok(())
}
