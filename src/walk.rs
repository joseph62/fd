use std::borrow::Cow;
use std::ffi::OsStr;
use std::fs::{FileType, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time;

use anyhow::{anyhow, Result};
use ignore::overrides::OverrideBuilder;
use ignore::{self, WalkBuilder};
use once_cell::unsync::OnceCell;
use regex::bytes::Regex;

use crate::config::Config;
use crate::error::print_error;
use crate::exec;
use crate::exit_codes::{merge_exitcodes, ExitCode};
use crate::filesystem;
use crate::output;

/// The receiver thread can either be buffering results or directly streaming to the console.
enum ReceiverMode {
    /// Receiver is still buffering in order to sort the results, if the search finishes fast
    /// enough.
    Buffering,

    /// Receiver is directly printing results to the output.
    Streaming,
}

/// The Worker threads can result in a valid entry having PathBuf or an error.
pub enum WorkerResult {
    Entry(PathBuf),
    Error(ignore::Error),
}

/// Maximum size of the output buffer before flushing results to the console
pub const MAX_BUFFER_LENGTH: usize = 1000;
/// Default duration until output buffering switches to streaming.
pub const DEFAULT_MAX_BUFFER_TIME: time::Duration = time::Duration::from_millis(100);

/// Recursively scan the given search path for files / pathnames matching the pattern.
///
/// If the `--exec` argument was supplied, this will create a thread pool for executing
/// jobs in parallel from a given command line and the discovered paths. Otherwise, each
/// path will simply be written to standard output.
pub fn scan(path_vec: &[PathBuf], pattern: Arc<Regex>, config: Arc<Config>) -> Result<ExitCode> {
    let mut path_iter = path_vec.iter();
    let first_path_buf = path_iter
        .next()
        .expect("Error: Path vector can not be empty");
    let (tx, rx) = channel();

    let mut override_builder = OverrideBuilder::new(first_path_buf.as_path());

    for pattern in &config.exclude_patterns {
        override_builder
            .add(pattern)
            .map_err(|e| anyhow!("Malformed exclude pattern: {}", e))?;
    }
    let overrides = override_builder
        .build()
        .map_err(|_| anyhow!("Mismatch in exclude patterns"))?;

    let mut walker = WalkBuilder::new(first_path_buf.as_path());
    walker
        .hidden(config.ignore_hidden)
        .ignore(config.read_fdignore)
        .parents(config.read_parent_ignore)
        .git_ignore(config.read_vcsignore)
        .git_global(config.read_vcsignore)
        .git_exclude(config.read_vcsignore)
        .overrides(overrides)
        .follow_links(config.follow_links)
        // No need to check for supported platforms, option is unavailable on unsupported ones
        .same_file_system(config.one_file_system)
        .max_depth(config.max_depth);

    if config.read_fdignore {
        walker.add_custom_ignore_filename(".fdignore");
    }

    if config.read_global_ignore {
        #[cfg(target_os = "macos")]
        let config_dir_op = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| dirs_next::home_dir().map(|d| d.join(".config")));

        #[cfg(not(target_os = "macos"))]
        let config_dir_op = dirs_next::config_dir();

        if let Some(global_ignore_file) = config_dir_op
            .map(|p| p.join("fd").join("ignore"))
            .filter(|p| p.is_file())
        {
            let result = walker.add_ignore(global_ignore_file);
            match result {
                Some(ignore::Error::Partial(_)) => (),
                Some(err) => {
                    print_error(format!(
                        "Malformed pattern in global ignore file. {}.",
                        err.to_string()
                    ));
                }
                None => (),
            }
        }
    }

    for ignore_file in &config.ignore_files {
        let result = walker.add_ignore(ignore_file);
        match result {
            Some(ignore::Error::Partial(_)) => (),
            Some(err) => {
                print_error(format!(
                    "Malformed pattern in custom ignore file. {}.",
                    err.to_string()
                ));
            }
            None => (),
        }
    }

    for path_entry in path_iter {
        walker.add(path_entry.as_path());
    }

    let parallel_walker = walker.threads(config.threads).build_parallel();

    let wants_to_quit = Arc::new(AtomicBool::new(false));
    if config.ls_colors.is_some() && config.command.is_none() {
        let wq = Arc::clone(&wants_to_quit);
        ctrlc::set_handler(move || {
            if wq.load(Ordering::Relaxed) {
                // Ctrl-C has been pressed twice, exit NOW
                process::exit(ExitCode::KilledBySigint.into());
            } else {
                wq.store(true, Ordering::Relaxed);
            }
        })
        .unwrap();
    }

    // Spawn the thread that receives all results through the channel.
    let receiver_thread = spawn_receiver(&config, &wants_to_quit, rx);

    // Spawn the sender threads.
    spawn_senders(&config, &wants_to_quit, pattern, parallel_walker, tx);

    // Wait for the receiver thread to print out all results.
    let exit_code = receiver_thread.join().unwrap();

    if wants_to_quit.load(Ordering::Relaxed) {
        Ok(ExitCode::KilledBySigint)
    } else {
        Ok(exit_code)
    }
}

fn spawn_receiver(
    config: &Arc<Config>,
    wants_to_quit: &Arc<AtomicBool>,
    rx: Receiver<WorkerResult>,
) -> thread::JoinHandle<ExitCode> {
    let config = Arc::clone(config);
    let wants_to_quit = Arc::clone(wants_to_quit);

    let show_filesystem_errors = config.show_filesystem_errors;
    let threads = config.threads;
    // This will be used to check if output should be buffered when only running a single thread
    let enable_output_buffering: bool = threads > 1;
    thread::spawn(move || {
        // This will be set to `Some` if the `--exec` argument was supplied.
        if let Some(ref cmd) = config.command {
            if cmd.in_batch_mode() {
                exec::batch(rx, cmd, show_filesystem_errors, enable_output_buffering)
            } else {
                let shared_rx = Arc::new(Mutex::new(rx));

                let out_perm = Arc::new(Mutex::new(()));

                // Each spawned job will store it's thread handle in here.
                let mut handles = Vec::with_capacity(threads);
                for _ in 0..threads {
                    let rx = Arc::clone(&shared_rx);
                    let cmd = Arc::clone(cmd);
                    let out_perm = Arc::clone(&out_perm);

                    // Spawn a job thread that will listen for and execute inputs.
                    let handle = thread::spawn(move || {
                        exec::job(
                            rx,
                            cmd,
                            out_perm,
                            show_filesystem_errors,
                            enable_output_buffering,
                        )
                    });

                    // Push the handle of the spawned thread into the vector for later joining.
                    handles.push(handle);
                }

                // Wait for all threads to exit before exiting the program.
                let exit_codes = handles
                    .into_iter()
                    .map(|handle| handle.join().unwrap())
                    .collect::<Vec<_>>();
                merge_exitcodes(exit_codes)
            }
        } else {
            let start = time::Instant::now();

            let mut buffer = vec![];

            // Start in buffering mode
            let mut mode = ReceiverMode::Buffering;

            // Maximum time to wait before we start streaming to the console.
            let max_buffer_time = config.max_buffer_time.unwrap_or(DEFAULT_MAX_BUFFER_TIME);

            let stdout = io::stdout();
            let mut stdout = stdout.lock();

            let mut num_results = 0;

            for worker_result in rx {
                match worker_result {
                    WorkerResult::Entry(value) => {
                        if config.quiet {
                            return ExitCode::HasResults(true);
                        }

                        match mode {
                            ReceiverMode::Buffering => {
                                buffer.push(value);

                                // Have we reached the maximum buffer size or maximum buffering time?
                                if buffer.len() > MAX_BUFFER_LENGTH
                                    || start.elapsed() > max_buffer_time
                                {
                                    // Flush the buffer
                                    for v in &buffer {
                                        output::print_entry(
                                            &mut stdout,
                                            v,
                                            &config,
                                            &wants_to_quit,
                                        );
                                    }
                                    buffer.clear();

                                    // Start streaming
                                    mode = ReceiverMode::Streaming;
                                }
                            }
                            ReceiverMode::Streaming => {
                                output::print_entry(&mut stdout, &value, &config, &wants_to_quit);
                            }
                        }

                        num_results += 1;
                        if let Some(max_results) = config.max_results {
                            if num_results >= max_results {
                                break;
                            }
                        }
                    }
                    WorkerResult::Error(err) => {
                        if show_filesystem_errors {
                            print_error(err.to_string());
                        }
                    }
                }
            }

            // If we have finished fast enough (faster than max_buffer_time), we haven't streamed
            // anything to the console, yet. In this case, sort the results and print them:
            buffer.sort();
            for value in buffer {
                output::print_entry(&mut stdout, &value, &config, &wants_to_quit);
            }

            if config.quiet {
                ExitCode::HasResults(false)
            } else {
                ExitCode::Success
            }
        }
    })
}

enum DirEntryInner {
    Normal(ignore::DirEntry),
    BrokenSymlink(PathBuf),
}

pub struct DirEntry {
    inner: DirEntryInner,
    metadata: OnceCell<Option<Metadata>>,
}

impl DirEntry {
    fn normal(e: ignore::DirEntry) -> Self {
        Self {
            inner: DirEntryInner::Normal(e),
            metadata: OnceCell::new(),
        }
    }

    fn broken_symlink(path: PathBuf) -> Self {
        Self {
            inner: DirEntryInner::BrokenSymlink(path),
            metadata: OnceCell::new(),
        }
    }

    pub fn path(&self) -> &Path {
        match &self.inner {
            DirEntryInner::Normal(e) => e.path(),
            DirEntryInner::BrokenSymlink(pathbuf) => pathbuf.as_path(),
        }
    }

    pub fn file_type(&self) -> Option<FileType> {
        match &self.inner {
            DirEntryInner::Normal(e) => e.file_type(),
            DirEntryInner::BrokenSymlink(_) => self.metadata().map(|m| m.file_type()),
        }
    }

    pub fn metadata(&self) -> Option<&Metadata> {
        self.metadata
            .get_or_init(|| match &self.inner {
                DirEntryInner::Normal(e) => e.metadata().ok(),
                DirEntryInner::BrokenSymlink(path) => path.symlink_metadata().ok(),
            })
            .as_ref()
    }

    pub fn depth(&self) -> Option<usize> {
        match &self.inner {
            DirEntryInner::Normal(e) => Some(e.depth()),
            DirEntryInner::BrokenSymlink(_) => None,
        }
    }
}

fn spawn_senders(
    config: &Arc<Config>,
    wants_to_quit: &Arc<AtomicBool>,
    pattern: Arc<Regex>,
    parallel_walker: ignore::WalkParallel,
    tx: Sender<WorkerResult>,
) {
    parallel_walker.run(|| {
        let config = Arc::clone(config);
        let pattern = Arc::clone(&pattern);
        let tx_thread = tx.clone();
        let wants_to_quit = Arc::clone(wants_to_quit);

        Box::new(move |entry_o| {
            if wants_to_quit.load(Ordering::Relaxed) {
                return ignore::WalkState::Quit;
            }

            let entry = match entry_o {
                Ok(ref e) if e.depth() == 0 => {
                    // Skip the root directory entry.
                    return ignore::WalkState::Continue;
                }
                Ok(e) => DirEntry::normal(e),
                Err(ignore::Error::WithPath {
                    path,
                    err: inner_err,
                }) => match inner_err.as_ref() {
                    ignore::Error::Io(io_error)
                        if io_error.kind() == io::ErrorKind::NotFound
                            && path
                                .symlink_metadata()
                                .ok()
                                .map_or(false, |m| m.file_type().is_symlink()) =>
                    {
                        DirEntry::broken_symlink(path)
                    }
                    _ => {
                        return match tx_thread.send(WorkerResult::Error(ignore::Error::WithPath {
                            path,
                            err: inner_err,
                        })) {
                            Ok(_) => ignore::WalkState::Continue,
                            Err(_) => ignore::WalkState::Quit,
                        }
                    }
                },
                Err(err) => {
                    return match tx_thread.send(WorkerResult::Error(err)) {
                        Ok(_) => ignore::WalkState::Continue,
                        Err(_) => ignore::WalkState::Quit,
                    }
                }
            };

            if let Some(min_depth) = config.min_depth {
                if entry.depth().map_or(true, |d| d < min_depth) {
                    return ignore::WalkState::Continue;
                }
            }

            // Check the name first, since it doesn't require metadata
            let entry_path = entry.path();

            let search_str: Cow<OsStr> = if config.search_full_path {
                let path_abs_buf = filesystem::path_absolute_form(entry_path)
                    .expect("Retrieving absolute path succeeds");
                Cow::Owned(path_abs_buf.as_os_str().to_os_string())
            } else {
                match entry_path.file_name() {
                    Some(filename) => Cow::Borrowed(filename),
                    None => unreachable!(
                        "Encountered file system entry without a file name. This should only \
                         happen for paths like 'foo/bar/..' or '/' which are not supposed to \
                         appear in a file system traversal."
                    ),
                }
            };

            if !pattern.is_match(&filesystem::osstr_to_bytes(search_str.as_ref())) {
                return ignore::WalkState::Continue;
            }

            // Filter out unwanted extensions.
            if let Some(ref exts_regex) = config.extensions {
                if let Some(path_str) = entry_path.file_name() {
                    if !exts_regex.is_match(&filesystem::osstr_to_bytes(path_str)) {
                        return ignore::WalkState::Continue;
                    }
                } else {
                    return ignore::WalkState::Continue;
                }
            }

            // Filter out unwanted file types.
            if let Some(ref file_types) = config.file_types {
                if file_types.should_ignore(&entry) {
                    return ignore::WalkState::Continue;
                }
            }

            #[cfg(unix)]
            {
                if let Some(ref owner_constraint) = config.owner_constraint {
                    if let Some(metadata) = entry.metadata() {
                        if !owner_constraint.matches(metadata) {
                            return ignore::WalkState::Continue;
                        }
                    } else {
                        return ignore::WalkState::Continue;
                    }
                }
            }

            // Filter out unwanted sizes if it is a file and we have been given size constraints.
            if !config.size_constraints.is_empty() {
                if entry_path.is_file() {
                    if let Some(metadata) = entry.metadata() {
                        let file_size = metadata.len();
                        if config
                            .size_constraints
                            .iter()
                            .any(|sc| !sc.is_within(file_size))
                        {
                            return ignore::WalkState::Continue;
                        }
                    } else {
                        return ignore::WalkState::Continue;
                    }
                } else {
                    return ignore::WalkState::Continue;
                }
            }

            // Filter out unwanted modification times
            if !config.time_constraints.is_empty() {
                let mut matched = false;
                if let Some(metadata) = entry.metadata() {
                    if let Ok(modified) = metadata.modified() {
                        matched = config
                            .time_constraints
                            .iter()
                            .all(|tf| tf.applies_to(&modified));
                    }
                }
                if !matched {
                    return ignore::WalkState::Continue;
                }
            }

            let send_result = tx_thread.send(WorkerResult::Entry(entry_path.to_owned()));

            if send_result.is_err() {
                return ignore::WalkState::Quit;
            }

            // Apply pruning.
            if config.prune {
                return ignore::WalkState::Skip;
            }

            ignore::WalkState::Continue
        })
    });
}
