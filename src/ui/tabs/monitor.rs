//! Samples a tab's host for the status bar while the window's status
//! router holds the monitor: only the current, connected tab is sampled
//! (doc 03 §Status bar). An SSH tab's server and a local shell's machine
//! run the same script (`host_stats`), so both are read the same way.

use std::sync::{Arc, Weak};
use std::time::Instant;

use gtk4::glib;
use gtk4::prelude::*;
use tokio::sync::mpsc;

use super::{existing_state, state};
use crate::transport::bandwidth::RateMeter;
use crate::transport::connection::SshConnection;
use crate::transport::host_stats::{self, Sample};
use crate::transport::load::CpuMeter;
use crate::ui::status_bar::{DiskReading, Host, HostFigures, Load, Uptime};

/// Dropping it stops sampling and clears the page's figures, all but the
/// uptime, which counts on from its answer.
pub(crate) struct HostMonitor {
    page: glib::WeakRef<gtk4::Widget>,
    /// Receives the samples; aborting it drops the receiver, which ends the
    /// script.
    task: glib::JoinHandle<()>,
}

impl HostMonitor {
    /// Sample the server of `connection` (doc 04 §Server figures).
    pub(super) fn server(page: &gtk4::Widget, connection: Weak<SshConnection>) -> Self {
        let (samples, received) = mpsc::channel(8);
        tokio::spawn(host_stats::stream_server(connection, samples));
        Self::receive(page, Host::Server, received)
    }

    /// Sample this machine (doc 06 §Local shell).
    pub(super) fn local(page: &gtk4::Widget) -> Self {
        let (samples, received) = mpsc::channel(8);
        tokio::spawn(host_stats::stream_local(samples));
        Self::receive(page, Host::ThisComputer, received)
    }

    fn receive(page: &gtk4::Widget, of: Host, mut received: mpsc::Receiver<Sample>) -> Self {
        let weak = page.downgrade();
        let task = glib::MainContext::default().spawn_local(async move {
            let mut rates = RateMeter::default();
            let mut cpu = CpuMeter::default();
            while let Some(sample) = received.recv().await {
                let Some(page) = weak.upgrade() else { break };
                let state = state(&page);
                let mut host = state.host.borrow_mut();
                let figures = host.get_or_insert_with(|| HostFigures::new(of));
                match sample {
                    Sample::Uptime(secs) => figures.uptime = Some(Uptime::measured(secs)),
                    Sample::Load(sample) => {
                        figures.load = Some(Load {
                            busy: cpu.add(sample.cpu),
                            cores: sample.cores,
                            load_average: sample.load_average,
                            memory: sample.memory,
                        });
                    }
                    Sample::Net(counters) => {
                        if let Some(rate) = rates.add(counters, Instant::now()) {
                            figures.bandwidth = Some(rate);
                        }
                    }
                    Sample::Disks(disks) => figures.disks = Some(DiskReading::sampled(disks)),
                }
            }
        });
        Self {
            page: page.downgrade(),
            task,
        }
    }
}

impl Drop for HostMonitor {
    fn drop(&mut self) {
        self.task.abort();
        if let Some(state) = self.page.upgrade().as_ref().and_then(existing_state)
            && let Some(figures) = state.host.borrow_mut().as_mut()
        {
            *figures = HostFigures {
                uptime: figures.uptime.take(),
                ..HostFigures::new(figures.of)
            };
        }
    }
}

/// Keep `connection` on the page for its monitor, weakly.
pub(super) fn remember_connection(page: &gtk4::Widget, connection: &Arc<SshConnection>) {
    state(page).connection.replace(Arc::downgrade(connection));
}
