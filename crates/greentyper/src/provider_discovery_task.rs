//! One-shot Provider discovery work launched only by an explicit terminal action.

use std::fmt;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};

use greentyper_core::provider::ProviderProfileSnapshot;

use crate::credential_vault::PlatformCredentialVault;
use crate::provider_connection::{
    ModelsHttpConnectionTester, ProviderConnectionTestStatus, ProviderConnectionTester,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProviderDiscoveryIdentity {
    profile: String,
    template: String,
    fingerprint: u64,
}

impl ProviderDiscoveryIdentity {
    fn from_profile(profile: &ProviderProfileSnapshot) -> Self {
        Self {
            profile: profile.profile().to_owned(),
            template: profile.template().to_owned(),
            fingerprint: profile.fingerprint(),
        }
    }

    pub(crate) fn profile(&self) -> &str {
        &self.profile
    }

    pub(crate) fn template(&self) -> &str {
        &self.template
    }

    pub(crate) const fn fingerprint(&self) -> u64 {
        self.fingerprint
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProviderDiscoveryTrigger {
    OnOpen,
    Manual,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProviderDiscoveryJob {
    identity: ProviderDiscoveryIdentity,
    trigger: ProviderDiscoveryTrigger,
}

impl ProviderDiscoveryJob {
    pub(crate) fn identity(&self) -> &ProviderDiscoveryIdentity {
        &self.identity
    }

    pub(crate) const fn trigger(&self) -> ProviderDiscoveryTrigger {
        self.trigger
    }
}

#[derive(Debug)]
pub(crate) enum ProviderDiscoveryTaskEvent {
    Started(ProviderDiscoveryJob),
    Completed {
        job: ProviderDiscoveryJob,
        status: ProviderConnectionTestStatus,
    },
    AlreadyRunning,
    WorkerUnavailable,
}

pub(crate) trait ProviderDiscoveryTask {
    fn request(
        &mut self,
        profile: ProviderProfileSnapshot,
        trigger: ProviderDiscoveryTrigger,
    ) -> ProviderDiscoveryTaskEvent;

    fn wait(&mut self) -> Option<ProviderDiscoveryTaskEvent>;

    fn cancel(&mut self);
}

type Probe = dyn Fn(ProviderProfileSnapshot) -> ProviderConnectionTestStatus + Send + Sync;

struct ActiveProviderDiscoveryTask {
    job: ProviderDiscoveryJob,
    receiver: Receiver<ProviderConnectionTestStatus>,
    worker: JoinHandle<()>,
}

pub(crate) struct OnDemandProviderDiscoveryTask {
    probe: Arc<Probe>,
    active: Option<ActiveProviderDiscoveryTask>,
}

impl OnDemandProviderDiscoveryTask {
    pub(crate) fn platform() -> Self {
        Self::with_probe(|profile| {
            let vault = PlatformCredentialVault;
            let mut tester = ModelsHttpConnectionTester::new(&vault);
            tester.test(&profile)
        })
    }

    fn with_probe<F>(probe: F) -> Self
    where
        F: Fn(ProviderProfileSnapshot) -> ProviderConnectionTestStatus + Send + Sync + 'static,
    {
        Self {
            probe: Arc::new(probe),
            active: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn testing<F>(probe: F) -> Self
    where
        F: Fn(ProviderProfileSnapshot) -> ProviderConnectionTestStatus + Send + Sync + 'static,
    {
        Self::with_probe(probe)
    }
}

impl ProviderDiscoveryTask for OnDemandProviderDiscoveryTask {
    fn request(
        &mut self,
        profile: ProviderProfileSnapshot,
        trigger: ProviderDiscoveryTrigger,
    ) -> ProviderDiscoveryTaskEvent {
        if self.active.is_some() {
            return ProviderDiscoveryTaskEvent::AlreadyRunning;
        }
        let job = ProviderDiscoveryJob {
            identity: ProviderDiscoveryIdentity::from_profile(&profile),
            trigger,
        };
        let (sender, receiver) = mpsc::sync_channel(1);
        let probe = Arc::clone(&self.probe);
        let spawn = thread::Builder::new()
            .name("greentyper-provider-discovery-once".to_owned())
            .spawn(move || {
                let status = probe(profile);
                let _ = sender.send(status);
            });
        let Ok(worker) = spawn else {
            return ProviderDiscoveryTaskEvent::WorkerUnavailable;
        };
        self.active = Some(ActiveProviderDiscoveryTask {
            job: job.clone(),
            receiver,
            worker,
        });
        ProviderDiscoveryTaskEvent::Started(job)
    }

    fn wait(&mut self) -> Option<ProviderDiscoveryTaskEvent> {
        let active = self.active.take()?;
        let status = active.receiver.recv();
        let joined = active.worker.join();
        match (status, joined) {
            (Ok(status), Ok(())) => Some(ProviderDiscoveryTaskEvent::Completed {
                job: active.job,
                status,
            }),
            _ => Some(ProviderDiscoveryTaskEvent::WorkerUnavailable),
        }
    }

    fn cancel(&mut self) {
        if let Some(active) = self.active.take() {
            drop(active.receiver);
            let _ = active.worker.join();
        }
    }
}

impl Drop for OnDemandProviderDiscoveryTask {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl fmt::Debug for OnDemandProviderDiscoveryTask {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OnDemandProviderDiscoveryTask")
            .field("active", &self.active.is_some())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    use greentyper_core::config::{ConfigDocument, ConfigPaths, ConfigRuntime};

    use crate::provider_connection::{ObservedProviderModel, ProviderConnectionTestStatus};

    use super::{
        OnDemandProviderDiscoveryTask, ProviderDiscoveryTask, ProviderDiscoveryTaskEvent,
        ProviderDiscoveryTrigger,
    };

    fn profile() -> greentyper_core::provider::ProviderProfileSnapshot {
        static NEXT_PROFILE: AtomicU64 = AtomicU64::new(1);
        let root = std::env::temp_dir().join(format!(
            "greentyper-on-demand-discovery-profile-{}-{}",
            std::process::id(),
            NEXT_PROFILE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("create profile fixture root");
        let user = root.join("user.toml");
        let project = root.join("project.toml");
        std::fs::write(
            &user,
            r#"schema_version = 1

[providers.primary]
template = "openai"
credential = "credential"
base_url = "https://example.invalid/v1"
dialects = ["responses"]

[providers.primary.routes]
responses = "/responses"
models = "/models"

[providers.primary.pricing]
source = "unknown"
"#,
        )
        .expect("write profile fixture");
        let runtime = ConfigRuntime::open(ConfigPaths::new(user, project), ConfigDocument::empty())
            .expect("open profile fixture");
        let profile = runtime
            .provider_profile("primary")
            .expect("resolve profile")
            .expect("profile snapshot");
        std::fs::remove_dir_all(root).expect("remove profile fixture");
        profile
    }

    #[test]
    fn one_shot_task_is_lazy_bounded_and_leaves_no_worker() {
        let (started_sender, started_receiver) = mpsc::sync_channel(1);
        let (release_sender, release_receiver) = mpsc::sync_channel(1);
        let release_receiver = std::sync::Mutex::new(release_receiver);
        let mut task = OnDemandProviderDiscoveryTask::testing(move |profile| {
            started_sender.send(()).expect("started receiver");
            release_receiver
                .lock()
                .expect("release lock")
                .recv()
                .expect("release signal");
            ProviderConnectionTestStatus::Succeeded {
                profile: profile.profile().to_owned(),
                fingerprint: profile.fingerprint(),
                models: vec![ObservedProviderModel {
                    id: "gpt-test".to_owned(),
                    release_catalog_key: None,
                }],
            }
        });

        let requested_profile = profile();
        let started = task.request(requested_profile.clone(), ProviderDiscoveryTrigger::OnOpen);
        assert!(matches!(
            started,
            ProviderDiscoveryTaskEvent::Started(job)
                if job.trigger() == ProviderDiscoveryTrigger::OnOpen
                    && job.identity().profile() == requested_profile.profile()
                    && job.identity().template() == requested_profile.template()
                    && job.identity().fingerprint() == requested_profile.fingerprint()
        ));
        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("worker started without blocking caller");
        assert!(matches!(
            task.request(profile(), ProviderDiscoveryTrigger::Manual),
            ProviderDiscoveryTaskEvent::AlreadyRunning
        ));
        release_sender.send(()).expect("release worker");

        let completed = task.wait().expect("worker completion");
        assert!(matches!(
            completed,
            ProviderDiscoveryTaskEvent::Completed {
                status: ProviderConnectionTestStatus::Succeeded { models, .. },
                ..
            } if models.len() == 1 && models[0].id == "gpt-test"
        ));
        assert!(
            task.wait().is_none(),
            "completed task has no resident worker"
        );
    }

    #[test]
    fn dropping_idle_task_never_invokes_probe() {
        let calls = std::sync::Arc::new(AtomicU64::new(0));
        let observed = std::sync::Arc::clone(&calls);
        let task = OnDemandProviderDiscoveryTask::testing(move |_| {
            observed.fetch_add(1, Ordering::Relaxed);
            ProviderConnectionTestStatus::Untested
        });
        drop(task);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn canceling_active_task_joins_worker_and_clears_state() {
        let (started_sender, started_receiver) = mpsc::sync_channel(1);
        let (release_sender, release_receiver) = mpsc::sync_channel(1);
        let release_receiver = std::sync::Mutex::new(release_receiver);
        let finished = std::sync::Arc::new(AtomicBool::new(false));
        let observed_finished = std::sync::Arc::clone(&finished);
        let mut task = OnDemandProviderDiscoveryTask::testing(move |_| {
            started_sender.send(()).expect("started receiver");
            release_receiver
                .lock()
                .expect("release lock")
                .recv()
                .expect("release signal");
            observed_finished.store(true, Ordering::Release);
            ProviderConnectionTestStatus::Untested
        });

        assert!(matches!(
            task.request(profile(), ProviderDiscoveryTrigger::Manual),
            ProviderDiscoveryTaskEvent::Started(_)
        ));
        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("worker started");
        release_sender.send(()).expect("release worker");
        task.cancel();

        assert!(finished.load(Ordering::Acquire));
        assert!(
            task.wait().is_none(),
            "cancelled task has no resident worker"
        );
    }
}
