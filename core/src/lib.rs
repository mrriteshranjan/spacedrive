use crate::file::cas::identifier::FileIdentifierJob;
use job::jobs::{Job, JobReport, Jobs};
use log::{error, info};
use prisma::PrismaClient;
use serde::{Deserialize, Serialize};
use state::client::ClientState;
use std::{fs, path::Path, sync::Arc};
use thiserror::Error;
use tokio::sync::{
  mpsc::{self, unbounded_channel, UnboundedReceiver, UnboundedSender},
  oneshot,
};
use ts_rs::TS;

use crate::encode::thumb::ThumbnailJob;

// init modules
pub mod client;
pub mod crypto;
pub mod db;
pub mod encode;
pub mod file;
pub mod job;
pub mod library;
pub mod p2p;
pub mod prisma;
pub mod state;
pub mod sync;
pub mod sys;
pub mod util;
// pub mod native;

// a wrapper around external input with a returning sender channel for core to respond
#[derive(Debug)]
pub struct ReturnableMessage<D, R = Result<CoreResponse, CoreError>> {
  data: D,
  return_sender: oneshot::Sender<R>,
}

// core controller is passed to the client to communicate with the core which runs in a dedicated thread
pub struct CoreController {
  query_sender: UnboundedSender<ReturnableMessage<ClientQuery>>,
  command_sender: UnboundedSender<ReturnableMessage<ClientCommand>>,
}

impl CoreController {
  pub async fn query(&self, query: ClientQuery) -> Result<CoreResponse, CoreError> {
    // a one time use channel to send and await a response
    let (sender, recv) = oneshot::channel();
    self
      .query_sender
      .send(ReturnableMessage {
        data: query,
        return_sender: sender,
      })
      .unwrap_or(());
    // wait for response and return
    recv.await.unwrap_or(Err(CoreError::QueryError))
  }

  pub async fn command(&self, command: ClientCommand) -> Result<CoreResponse, CoreError> {
    let (sender, recv) = oneshot::channel();
    self
      .command_sender
      .send(ReturnableMessage {
        data: command,
        return_sender: sender,
      })
      .unwrap_or(());

    recv.await.unwrap()
  }
}

#[derive(Debug)]
pub enum InternalEvent {
  JobIngest(Box<dyn Job>),
  JobComplete(String),
}

#[derive(Clone)]
pub struct CoreContext {
  pub database: Arc<PrismaClient>,
  pub event_sender: mpsc::Sender<CoreEvent>,
  pub internal_sender: UnboundedSender<InternalEvent>,
}

impl CoreContext {
  pub fn spawn_job(&self, job: Box<dyn Job>) {
    self
      .internal_sender
      .send(InternalEvent::JobIngest(job))
      .unwrap_or_else(|e| {
        error!("Failed to spawn job. {:?}", e);
      });
  }
  pub async fn emit(&self, event: CoreEvent) {
    self.event_sender.send(event).await.unwrap_or_else(|e| {
      error!("Failed to emit event. {:?}", e);
    });
  }
}

pub struct Core {
  state: ClientState,
  jobs: job::jobs::Jobs,
  database: Arc<PrismaClient>,
  // filetype_registry: library::TypeRegistry,
  // extension_registry: library::ExtensionRegistry,

  // global messaging channels
  query_channel: (
    UnboundedSender<ReturnableMessage<ClientQuery>>,
    UnboundedReceiver<ReturnableMessage<ClientQuery>>,
  ),
  command_channel: (
    UnboundedSender<ReturnableMessage<ClientCommand>>,
    UnboundedReceiver<ReturnableMessage<ClientCommand>>,
  ),
  event_sender: mpsc::Sender<CoreEvent>,

  // a channel for child threads to send events back to the core
  internal_channel: (
    UnboundedSender<InternalEvent>,
    UnboundedReceiver<InternalEvent>,
  ),
}

impl Core {
  // create new instance of core, run startup tasks
  pub async fn new(mut data_dir: std::path::PathBuf) -> (Core, mpsc::Receiver<CoreEvent>) {
    let (event_sender, event_recv) = mpsc::channel(100);

    data_dir = data_dir.join("spacedrive");
    let data_dir = data_dir.to_str().unwrap();
    // create data directory if it doesn't exist
    fs::create_dir_all(&data_dir).unwrap();
    // prepare basic client state
    let mut state = ClientState::new(data_dir, "diamond-mastering-space-dragon").unwrap();
    // load from disk
    state
      .read_disk()
      .unwrap_or(error!("No client state found, creating new one..."));

    state.save();

    println!("Client State: {:?}", state);

    let database = Arc::new(db::create_connection().await.unwrap());

    let internal_channel = unbounded_channel::<InternalEvent>();

    let core = Core {
      state,
      query_channel: unbounded_channel(),
      command_channel: unbounded_channel(),
      jobs: Jobs::new(),
      event_sender,
      database,
      internal_channel,
    };

    tokio::spawn(async move {
      p2p::listener::listen(None).await.unwrap_or(());
    });

    (core, event_recv)
  }

  pub fn get_context(&self) -> CoreContext {
    CoreContext {
      database: self.database.clone(),
      event_sender: self.event_sender.clone(),
      internal_sender: self.internal_channel.0.clone(),
    }
  }

  pub fn get_controller(&self) -> CoreController {
    CoreController {
      query_sender: self.query_channel.0.clone(),
      command_sender: self.command_channel.0.clone(),
    }
  }

  pub async fn start(&mut self) {
    let ctx = self.get_context();
    loop {
      // listen on global messaging channels for incoming messages
      tokio::select! {
          Some(msg) = self.query_channel.1.recv() => {
              let res = self.exec_query(msg.data).await;
              msg.return_sender.send(res).unwrap_or(());
          }
          Some(msg) = self.command_channel.1.recv() => {
              let res = self.exec_command(msg.data).await;
              msg.return_sender.send(res).unwrap_or(());
          }
          Some(event) = self.internal_channel.1.recv() => {
              match event {
                  InternalEvent::JobIngest(job) => {
                      self.jobs.ingest(&ctx, job).await;
                  },
                  InternalEvent::JobComplete(id) => {
                      self.jobs.complete(id);
                  },
              }
          }
      }
    }
  }
  // load library database + initialize client with db
  pub async fn initializer(&self) {
    println!("Initializing...");
    if self.state.libraries.len() == 0 {
      match library::loader::create(&self, None).await {
        Ok(library) => info!("Created new library: {:?}", library),
        Err(e) => info!("Error creating library: {:?}", e),
      }
    } else {
      for library in self.state.libraries.iter() {
        // init database for library
        match library::loader::load(&library.library_path, &library.library_uuid).await {
          Ok(library) => info!("Loaded library: {:?}", library),
          Err(e) => info!("Error loading library: {:?}", e),
        }
      }
    }
    // init client
    match client::create(&self).await {
      Ok(_) => info!("Spacedrive online"),
      Err(e) => info!("Error initializing client: {:?}", e),
    };
  }

  async fn exec_command(&mut self, cmd: ClientCommand) -> Result<CoreResponse, CoreError> {
    println!("Core command: {:?}", cmd);
    let ctx = self.get_context();
    Ok(match cmd {
      // CRUD for locations
      ClientCommand::LocCreate { path } => {
        CoreResponse::LocCreate(sys::locations::new_location_and_scan(&ctx, &path).await?)
      }
      ClientCommand::LocUpdate { id: _, name: _ } => todo!(),
      ClientCommand::LocDelete { id: _ } => todo!(),
      // CRUD for files
      ClientCommand::FileRead { id: _ } => todo!(),
      // ClientCommand::FileEncrypt { id: _, algorithm: _ } => todo!(),
      ClientCommand::FileDelete { id: _ } => todo!(),
      // CRUD for tags
      ClientCommand::TagCreate { name: _, color: _ } => todo!(),
      ClientCommand::TagAssign {
        file_id: _,
        tag_id: _,
      } => todo!(),
      ClientCommand::TagDelete { id: _ } => todo!(),
      // CRUD for libraries
      ClientCommand::SysVolumeUnmount { id: _ } => todo!(),
      ClientCommand::LibDelete { id: _ } => todo!(),
      ClientCommand::TagUpdate { name: _, color: _ } => todo!(),
      ClientCommand::GenerateThumbsForLocation { id, path } => {
        ctx.spawn_job(Box::new(ThumbnailJob {
          location_id: id,
          path,
          background: false, // fix
        }));
        CoreResponse::Success(())
      }
      // ClientCommand::PurgeDatabase => {
      //   println!("Purging database...");
      //   fs::remove_file(Path::new(&self.state.data_path).join("library.db")).unwrap();
      //   CoreResponse::Success(())
      // }
      ClientCommand::IdentifyUniqueFiles => {
        ctx.spawn_job(Box::new(FileIdentifierJob));
        CoreResponse::Success(())
      }
    })
  }

  // query sources of data
  async fn exec_query(&self, query: ClientQuery) -> Result<CoreResponse, CoreError> {
    println!("Core query: {:?}", query);
    let ctx = self.get_context();
    Ok(match query {
      // return the client state from memory
      ClientQuery::ClientGetState => CoreResponse::ClientGetState(self.state.clone()),
      // get system volumes without saving to library
      ClientQuery::SysGetVolumes => CoreResponse::SysGetVolumes(sys::volumes::get_volumes()?),
      ClientQuery::SysGetLocations => {
        CoreResponse::SysGetLocations(sys::locations::get_locations(&ctx).await?)
      }
      // get location from library
      ClientQuery::SysGetLocation { id } => {
        CoreResponse::SysGetLocation(sys::locations::get_location(&ctx, id).await?)
      }
      // return contents of a directory for the explorer
      ClientQuery::LibGetExplorerDir {
        path,
        location_id,
        limit: _,
      } => CoreResponse::LibGetExplorerDir(
        file::explorer::open::open_dir(&ctx, &location_id, &path).await?,
      ),
      ClientQuery::LibGetTags => todo!(),
      ClientQuery::JobGetRunning => CoreResponse::JobGetRunning(self.jobs.get_running().await),
      // TODO: FIX THIS
      ClientQuery::JobGetHistory => CoreResponse::JobGetHistory(Jobs::get_history(&ctx).await?),
    })
  }
}

// represents an event this library can emit
#[derive(Serialize, Deserialize, Debug, TS)]
#[serde(tag = "key", content = "params")]
#[ts(export)]
pub enum ClientCommand {
  // Files
  FileRead { id: i32 },
  // FileEncrypt { id: i32, algorithm: EncryptionAlgorithm },
  FileDelete { id: i32 },
  // Library
  LibDelete { id: i32 },
  // Tags
  TagCreate { name: String, color: String },
  TagUpdate { name: String, color: String },
  TagAssign { file_id: i32, tag_id: i32 },
  TagDelete { id: i32 },
  // Locations
  LocCreate { path: String },
  LocUpdate { id: i32, name: Option<String> },
  LocDelete { id: i32 },
  // System
  SysVolumeUnmount { id: i32 },
  GenerateThumbsForLocation { id: i32, path: String },
  // PurgeDatabase,
  IdentifyUniqueFiles,
}

// represents an event this library can emit
#[derive(Serialize, Deserialize, Debug, TS)]
#[serde(tag = "key", content = "params")]
#[ts(export)]
pub enum ClientQuery {
  ClientGetState,
  SysGetVolumes,
  LibGetTags,
  JobGetRunning,
  JobGetHistory,
  SysGetLocations,
  SysGetLocation {
    id: i32,
  },
  LibGetExplorerDir {
    location_id: i32,
    path: String,
    limit: i32,
  },
}

// represents an event this library can emit
#[derive(Serialize, Deserialize, Debug, TS)]
#[serde(tag = "key", content = "data")]
#[ts(export)]
pub enum CoreEvent {
  // most all events should be once of these two
  InvalidateQuery(ClientQuery),
  InvalidateQueryDebounced(ClientQuery),
  InvalidateResource(CoreResource),
  NewThumbnail { cas_id: String },
  Log { message: String },
  DatabaseDisconnected { reason: Option<String> },
}

#[derive(Serialize, Deserialize, Debug, TS)]
#[serde(tag = "key", content = "data")]
#[ts(export)]
pub enum CoreResponse {
  Success(()),
  SysGetVolumes(Vec<sys::volumes::Volume>),
  SysGetLocation(sys::locations::LocationResource),
  SysGetLocations(Vec<sys::locations::LocationResource>),
  LibGetExplorerDir(file::DirectoryWithContents),
  ClientGetState(ClientState),
  LocCreate(sys::locations::LocationResource),
  JobGetRunning(Vec<JobReport>),
  JobGetHistory(Vec<JobReport>),
}

#[derive(Error, Debug)]
pub enum CoreError {
  #[error("Query error")]
  QueryError,
  #[error("System error")]
  SysError(#[from] sys::SysError),
  #[error("File error")]
  FileError(#[from] file::FileError),
  #[error("Job error")]
  JobError(#[from] job::JobError),
  #[error("Database error")]
  DatabaseError(#[from] prisma::QueryError),
}

#[derive(Serialize, Deserialize, Debug, TS)]
#[ts(export)]
pub enum CoreResource {
  Client,
  Library,
  Location(sys::locations::LocationResource),
  File(file::File),
  Job(JobReport),
  Tag,
}
