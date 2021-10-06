// Copyright 2019 Pants project contributors (see CONTRIBUTORS.md).
// Licensed under the Apache License, Version 2.0 (see LICENSE).

use std::io::{self, BufRead, Read};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_lock::{Mutex, MutexGuardArc};
use hashing::Fingerprint;
use lazy_static::lazy_static;
use log::{debug, info};
use regex::Regex;
use store::Store;
use task_executor::Executor;
use tempfile::TempDir;

use crate::local::prepare_workdir;
use crate::{Context, MultiPlatformProcess, NamedCaches, Process, ProcessMetadata};

lazy_static! {
  static ref NAILGUN_PORT_REGEX: Regex = Regex::new(r".*\s+port\s+(\d+)\.$").unwrap();
}

struct PoolEntry {
  fingerprint: NailgunProcessFingerprint,
  last_used: Instant,
  process: Arc<Mutex<NailgunProcess>>,
}

pub type Port = u16;

///
/// A NailgunPool contains a small Vec of running NailgunProcess instances, fingerprinted with the
/// request used to start them.
///
/// Mutations of the Vec are protected by a Mutex, but each NailgunProcess is also protected by its
/// own Mutex, which is used to track when the process is in use.
///
/// NB: This pool expects to be used under a semaphore with size equal to the pool size. Because of
/// this, it never actually waits for a pool entry to complete, and can instead assume that at
/// least one pool slot is always idle when `acquire` is entered.
///
#[derive(Clone)]
pub struct NailgunPool {
  workdir_base: PathBuf,
  size: usize,
  store: Store,
  executor: Executor,
  named_caches: NamedCaches,
  processes: Arc<Mutex<Vec<PoolEntry>>>,
}

impl NailgunPool {
  pub fn new(
    workdir_base: PathBuf,
    size: usize,
    store: Store,
    executor: Executor,
    named_caches: NamedCaches,
  ) -> Self {
    NailgunPool {
      workdir_base,
      size,
      store,
      executor,
      named_caches,
      processes: Arc::default(),
    }
  }

  ///
  /// Given a name and a `Process` configuration, return a port of a nailgun server running
  /// under that name and configuration.
  ///
  /// If the server is not running, or if it's running with a different configuration,
  /// this code will start a new server as a side effect.
  ///
  pub async fn acquire(
    &self,
    server_process: Process,
    context: Context,
  ) -> Result<BorrowedNailgunProcess, String> {
    let name = server_process.description.clone();
    let requested_fingerprint = NailgunProcessFingerprint::new(name.clone(), &server_process)?;
    let mut processes = self.processes.lock().await;

    // Start by seeing whether there are any idle processes with a matching fingerprint.
    if let Some((_idx, process)) = Self::find_usable(&mut *processes, &requested_fingerprint)? {
      return Ok(BorrowedNailgunProcess::new(process));
    }

    // There wasn't a matching, valid, available process. We need to start one.
    if processes.len() >= self.size {
      // Find the oldest idle non-matching process and remove it.
      let idx = Self::find_lru_idle(&mut *processes)?.ok_or_else(|| {
        // NB: See the method docs: the pool assumes that it is running under a semaphore, so this
        // should be impossible.
        "No idle slots in nailgun pool.".to_owned()
      })?;

      processes.swap_remove(idx);
    }

    // Start the new process.
    let process = Arc::new(Mutex::new(
      NailgunProcess::start_new(
        name.clone(),
        server_process,
        &self.workdir_base,
        context,
        &self.store,
        self.executor.clone(),
        &self.named_caches,
        requested_fingerprint.clone(),
      )
      .await?,
    ));
    processes.push(PoolEntry {
      fingerprint: requested_fingerprint,
      last_used: Instant::now(),
      process: process.clone(),
    });

    Ok(BorrowedNailgunProcess::new(process.lock_arc().await))
  }

  ///
  /// Find a usable process in the pool that matches the given fingerprint.
  ///
  fn find_usable(
    pool_entries: &mut Vec<PoolEntry>,
    fingerprint: &NailgunProcessFingerprint,
  ) -> Result<Option<(usize, MutexGuardArc<NailgunProcess>)>, String> {
    let mut dead_processes = Vec::new();
    for (idx, pool_entry) in pool_entries.iter_mut().enumerate() {
      if &pool_entry.fingerprint != fingerprint {
        continue;
      }

      match Self::try_use(pool_entry)? {
        TryUse::Usable(process) => return Ok(Some((idx, process))),
        TryUse::Dead => dead_processes.push(idx),
        TryUse::Busy => continue,
      }
    }
    // NB: We'll only prune dead processes if we don't find a live match, but that's fine.
    for dead_process_idx in dead_processes.into_iter().rev() {
      pool_entries.swap_remove(dead_process_idx);
    }
    Ok(None)
  }

  ///
  /// Find the least recently used idle (but not necessarily usable) process in the pool.
  ///
  fn find_lru_idle(pool_entries: &mut Vec<PoolEntry>) -> Result<Option<usize>, String> {
    // 24 hours of clock skew would be surprising?
    let mut lru_age = Instant::now() + Duration::from_secs(60 * 60 * 24);
    let mut lru = None;
    for (idx, pool_entry) in pool_entries.iter_mut().enumerate() {
      if pool_entry.process.try_lock_arc().is_some() && pool_entry.last_used < lru_age {
        lru = Some(idx);
        lru_age = pool_entry.last_used;
      }
    }
    Ok(lru)
  }

  fn try_use(pool_entry: &mut PoolEntry) -> Result<TryUse, String> {
    let mut process = if let Some(process) = pool_entry.process.try_lock_arc() {
      process
    } else {
      return Ok(TryUse::Busy);
    };

    pool_entry.last_used = Instant::now();

    debug!(
      "Checking if nailgun server {} is still alive at port {}...",
      process.name, process.port
    );

    // Check if it's alive using the handle.
    let status = process
      .handle
      .try_wait()
      .map_err(|e| format!("Error getting the process status! {}", e))?;
    match status {
      None => {
        // Process hasn't exited yet.
        debug!(
          "Found nailgun process {}, with fingerprint {:?}",
          process.name, process.fingerprint
        );
        Ok(TryUse::Usable(process))
      }
      Some(status) => {
        // The process has exited with some exit code: restart it.
        if status.signal() != Some(9) {
          // TODO: BorrowedNailgunProcess cancellation uses `kill` currently, so we avoid warning
          // for that. In future it would be nice to find a better cancellation strategy.
          log::warn!(
            "The nailgun server for {} exited with {}.",
            process.name,
            status
          );
        }
        Ok(TryUse::Dead)
      }
    }
  }
}

enum TryUse {
  Usable(MutexGuardArc<NailgunProcess>),
  Busy,
  Dead,
}

/// Representation of a running nailgun server.
pub struct NailgunProcess {
  pub name: String,
  fingerprint: NailgunProcessFingerprint,
  workdir: TempDir,
  port: Port,
  executor: task_executor::Executor,
  handle: std::process::Child,
}

fn read_port(child: &mut std::process::Child) -> Result<Port, String> {
  let stdout = child
    .stdout
    .as_mut()
    .ok_or_else(|| "No stdout found!".to_string());
  let port_line = stdout
    .and_then(|stdout| {
      let reader = io::BufReader::new(stdout);
      reader
        .lines()
        .next()
        .ok_or_else(|| "There is no line ready in the child's output".to_string())
    })
    .and_then(|res| res.map_err(|e| format!("Failed to read from stdout: {}", e)));

  // If we failed to read a port line and the child has exited, report that.
  if port_line.is_err() {
    if let Some(exit_status) = child.try_wait().map_err(|e| e.to_string())? {
      let mut stderr = String::new();
      child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .map_err(|e| e.to_string())?;
      return Err(format!(
        "Nailgun failed to start: exited with {}, stderr:\n{}",
        exit_status, stderr
      ));
    }
  }
  let port_line = port_line?;

  let port = &NAILGUN_PORT_REGEX
    .captures_iter(&port_line)
    .next()
    .ok_or_else(|| format!("Output for nailgun server was unexpected:\n{:?}", port_line))?[1];
  port
    .parse::<Port>()
    .map_err(|e| format!("Error parsing port {}! {}", &port, e))
}

impl NailgunProcess {
  async fn start_new(
    name: String,
    startup_options: Process,
    workdir_base: &Path,
    context: Context,
    store: &Store,
    executor: Executor,
    named_caches: &NamedCaches,
    nailgun_server_fingerprint: NailgunProcessFingerprint,
  ) -> Result<NailgunProcess, String> {
    let workdir = tempfile::Builder::new()
      .prefix("process-execution")
      .tempdir_in(workdir_base)
      .map_err(|err| format!("Error making tempdir for nailgun server: {:?}", err))?;

    prepare_workdir(
      workdir.path().to_owned(),
      &startup_options,
      context.clone(),
      store.clone(),
      executor.clone(),
      named_caches,
    )
    .await?;
    store
      .materialize_directory(workdir.path().to_owned(), startup_options.input_files)
      .await?;

    let cmd = startup_options.argv[0].clone();
    // TODO: This is an expensive operation, and thus we info! it.
    //       If it becomes annoying, we can downgrade the logging to just debug!
    info!(
      "Starting new nailgun server with cmd: {:?}, args {:?}, in cwd {:?}",
      cmd,
      &startup_options.argv[1..],
      workdir.path()
    );
    let mut child = std::process::Command::new(&cmd)
      .args(&startup_options.argv[1..])
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .current_dir(&workdir)
      .spawn()
      .map_err(|e| {
        format!(
          "Failed to create child handle with cmd: {} options {:#?}: {}",
          &cmd, &startup_options, e
        )
      })?;

    let port = read_port(&mut child)?;
    debug!(
      "Created nailgun server process with pid {} and port {}",
      child.id(),
      port
    );

    // Now that we've started it, clear its directory before the first client can access it.
    clear_workdir(workdir.path(), &executor).await?;

    Ok(NailgunProcess {
      port,
      fingerprint: nailgun_server_fingerprint,
      workdir,
      name,
      executor,
      handle: child,
    })
  }
}

impl Drop for NailgunProcess {
  fn drop(&mut self) {
    debug!("Exiting nailgun server process {:?}", self.name);
    if self.handle.kill().is_ok() {
      // NB: This is blocking, but should be a short wait in general.
      let _ = self.handle.wait();
    }
  }
}

/// The fingerprint of an nailgun server process.
///
/// This is calculated by hashing together:
///   - The jvm options and classpath used to create the server
///   - The path to the jdk
#[derive(Clone, Hash, PartialEq, Eq, Debug)]
struct NailgunProcessFingerprint {
  pub name: String,
  pub fingerprint: Fingerprint,
}

impl NailgunProcessFingerprint {
  pub fn new(name: String, nailgun_req: &Process) -> Result<Self, String> {
    let nailgun_req_digest = crate::digest(
      MultiPlatformProcess::from(nailgun_req.clone()),
      &ProcessMetadata::default(),
    );
    Ok(NailgunProcessFingerprint {
      name,
      fingerprint: nailgun_req_digest.hash,
    })
  }
}

///
/// A wrapper around a NailgunProcess checked out from the pool. If `release` is not called, the
/// guard assumes cancellation, and kills the underlying process.
///
pub struct BorrowedNailgunProcess(Option<MutexGuardArc<NailgunProcess>>);

impl BorrowedNailgunProcess {
  fn new(process: MutexGuardArc<NailgunProcess>) -> Self {
    Self(Some(process))
  }

  pub fn name(&self) -> &str {
    &self.0.as_ref().unwrap().name
  }

  pub fn port(&self) -> u16 {
    self.0.as_ref().unwrap().port
  }

  pub fn address(&self) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), self.port())
  }

  pub fn workdir_path(&self) -> &Path {
    self.0.as_ref().unwrap().workdir.path()
  }

  ///
  /// Return the NailgunProcess to the pool.
  ///
  /// Clears the working directory for the process before returning it.
  ///
  pub async fn release(&mut self) -> Result<(), String> {
    let process = self.0.as_ref().expect("release may only be called once.");

    clear_workdir(process.workdir.path(), &process.executor).await?;

    // Once we've successfully cleaned up, remove the process.
    let _ = self.0.take();
    Ok(())
  }
}

impl Drop for BorrowedNailgunProcess {
  fn drop(&mut self) {
    if let Some(mut process) = self.0.take() {
      // Kill the process, but rely on the pool to notice that it is dead and restart it.
      debug!(
        "Killing nailgun process {:?} due to cancellation.",
        process.name
      );
      let _ = process.handle.kill();
    }
  }
}

async fn clear_workdir(workdir: &Path, executor: &Executor) -> Result<(), String> {
  // Move all content into a temporary directory.
  let garbage_dir = tempfile::Builder::new()
    .prefix("process-execution")
    .tempdir_in(workdir.parent().unwrap())
    .map_err(|err| {
      format!(
        "Error making garbage directory for nailgun cleanup: {:?}",
        err
      )
    })?;
  let mut dir_entries = tokio::fs::read_dir(workdir)
    .await
    .map_err(|e| format!("Failed to read nailgun process directory: {}", e))?;
  while let Some(dir_entry) = dir_entries
    .next_entry()
    .await
    .map_err(|e| format!("Failed to read entry in nailgun process directory: {}", e))?
  {
    tokio::fs::rename(
      dir_entry.path(),
      garbage_dir.path().join(dir_entry.file_name()),
    )
    .await
    .map_err(|e| {
      format!(
        "Failed to move {} to garbage: {}",
        dir_entry.path().display(),
        e
      )
    })?;
  }

  // And drop it in the background.
  let _ = executor.spawn_blocking(move || std::mem::drop(garbage_dir));

  Ok(())
}
