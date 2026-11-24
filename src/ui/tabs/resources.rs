//! Samples a tab's host CPU, memory and disks for the status bar while the
//! window's status router holds the monitor: only the current, connected
//! tab is sampled (doc 03 §Status bar).

use std::sync::Weak;

use gtk4::glib;
use gtk4::prelude::*;
use tokio::sync::mpsc;

use super::{existing_state, state};
use crate::transport::connection::SshConnection;
use crate::transport::disks::{self, Filesystem};
use crate::transport::load::{self, CpuMeter, LoadSample};
use crate::ui::status_bar::{Host, HostDisks, HostLoad};

/// Dropping it stops sampling and clears the page's figures.
pub(super) struct ResourceMonitor {
    page: glib::WeakRef<gtk4::Widget>,
    /// Receive the samples; aborting them drops the receivers, which ends
    /// the streams.
    tasks: [glib::JoinHandle<()>; 2],
}

impl ResourceMonitor {
    /// Sample the server of `connection` (doc 04 §Server load and disks).
    pub(super) fn server(page: &gtk4::Widget, connection: Weak<SshConnection>) -> Self {
        let (loads, load_rx) = mpsc::channel(4);
        let (disk_tx, disk_rx) = mpsc::channel(2);
        tokio::spawn(load::stream_server(connection.clone(), loads));
        tokio::spawn(disks::stream_server(connection, disk_tx));
        Self::receive(page, Host::Server, load_rx, disk_rx)
    }

    /// Sample this machine (doc 06 §Local shell).
    pub(super) fn local(page: &gtk4::Widget) -> Self {
        let (loads, load_rx) = mpsc::channel(4);
        let (disk_tx, disk_rx) = mpsc::channel(2);
        tokio::spawn(load::stream_local(loads));
        tokio::spawn(disks::stream_local(disk_tx));
        Self::receive(page, Host::ThisComputer, load_rx, disk_rx)
    }

    fn receive(
        page: &gtk4::Widget,
        of: Host,
        mut loads: mpsc::Receiver<LoadSample>,
        mut filesystems: mpsc::Receiver<Vec<Filesystem>>,
    ) -> Self {
        let context = glib::MainContext::default();
        let weak = page.downgrade();
        let load_task = context.spawn_local(async move {
            let mut meter = CpuMeter::default();
            while let Some(sample) = loads.recv().await {
                let Some(page) = weak.upgrade() else { break };
                let load = HostLoad {
                    of,
                    busy: meter.add(sample.cpu),
                    cores: sample.cores,
                    load_average: sample.load_average,
                    memory: sample.memory,
                };
                state(&page).load.replace(Some(load));
            }
        });
        let weak = page.downgrade();
        let disk_task = context.spawn_local(async move {
            while let Some(filesystems) = filesystems.recv().await {
                let Some(page) = weak.upgrade() else { break };
                state(&page)
                    .disks
                    .replace(Some(HostDisks { of, filesystems }));
            }
        });
        Self {
            page: page.downgrade(),
            tasks: [load_task, disk_task],
        }
    }
}

impl Drop for ResourceMonitor {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        if let Some(state) = self.page.upgrade().as_ref().and_then(existing_state) {
            state.load.replace(None);
            state.disks.replace(None);
        }
    }
}
