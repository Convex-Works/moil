//! Running the agent in a process, as the app and `moil agent` do, so that
//! both start and stop it the same way: clean up after a crash, link what
//! hooks find on their `PATH`, start the agent, and serve the control
//! socket (spec/machine.md §4).

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use moil_runtime::Runtime;
use tokio::sync::Mutex;

use crate::control::{self, HostControls, ServerHandle};
use crate::report::{AgentInfo, Host};
use crate::{Agent, AgentHandle, AgentOptions, Error, Machine, hooks};

/// What a process needs to run the agent.
pub struct HostConfig {
    /// This computer's moil state, opened with its lock held.
    pub machine: Machine,
    /// Runs the attempts. Its uv is the one hooks find on their `PATH`.
    pub runtime: Runtime,
    /// The `moil` that belongs to this process, which hooks find on their
    /// `PATH` and which a command of another version is pointed to.
    pub moil: PathBuf,
    /// What runs the agent.
    pub host: Host,
    /// What only the process can do: quit, and the app's login item.
    pub controls: Arc<dyn HostControls>,
    pub options: AgentOptions,
}

impl fmt::Debug for HostConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostConfig")
            .field("machine", &self.machine)
            .field("moil", &self.moil)
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}

/// The agent running in this process, and the control socket through which
/// commands reach it.
pub struct Hosted {
    agent: AgentHandle,
    server: Mutex<Option<ServerHandle>>,
}

impl fmt::Debug for Hosted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Hosted").finish_non_exhaustive()
    }
}

impl Hosted {
    /// Kills what attempts of a crashed run left behind, links
    /// `.machine/bin`, starts the agent, and with it the owner's hooks, and
    /// serves the control socket. Must be called from within a Tokio
    /// runtime. Fails only if commands couldn't reach the agent, having
    /// shut it down again.
    pub async fn start(config: HostConfig) -> Result<Self, Error> {
        let HostConfig {
            machine,
            runtime,
            moil,
            host,
            controls,
            options,
        } = config;
        // Leftovers are worth a warning, not refusing to work.
        match runtime.reap_orphans() {
            Ok(0) => {}
            Ok(n) => tracing::info!("cleaned up {n} attempts left over from an earlier run"),
            Err(e) => {
                tracing::warn!("couldn't clean up attempts left over from an earlier run: {e}");
            }
        }
        if let Err(e) = hooks::link_bin(machine.home(), &moil, &runtime.config().uv) {
            tracing::warn!("hooks won't find moil and uv on their PATH: {e}");
        }
        let agent = Agent::start_with(machine, Arc::new(runtime), options);
        let info = AgentInfo {
            host,
            version: control::VERSION.to_owned(),
        };
        match control::serve(agent.clone(), info, controls) {
            Ok(server) => Ok(Self {
                agent,
                server: Mutex::new(Some(server)),
            }),
            Err(e) => {
                agent.shutdown().await;
                Err(e)
            }
        }
    }

    pub fn agent(&self) -> &AgentHandle {
        &self.agent
    }

    /// Stops the running job and reports it, closes every connection,
    /// stops the running hook, and then stops listening for commands.
    /// Commands can still reach the agent while it shuts down.
    pub async fn shutdown(&self) {
        self.agent.shutdown().await;
        if let Some(server) = self.server.lock().await.take() {
            server.shutdown().await;
        }
    }
}
