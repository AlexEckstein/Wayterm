//! Samples a tab's host bandwidth for the status bar while the window's
//! status router holds the monitor: only the current, connected tab is
//! sampled (doc 03 §Status bar).

use std::cell::RefCell;
use std::sync::{Arc, Weak};
use std::time::Instant;

use gtk4::glib;
use gtk4::prelude::*;
use tokio::sync::mpsc;

use super::{existing_state, state};
use crate::transport::bandwidth::{self, INTERVAL, RateMeter};
use crate::transport::connection::SshConnection;
use crate::ui::status_bar::{Host, HostRate};

/// Dropping it stops sampling and clears the page's figure.
pub(crate) struct BandwidthMonitor {
    page: glib::WeakRef<gtk4::Widget>,
    sampler: Sampler,
}

enum Sampler {
    /// Receives the server script's samples; aborting it ends the script.
    Server(glib::JoinHandle<()>),
    /// Reads this machine's counters every interval.
    Local(Option<glib::SourceId>),
}

impl BandwidthMonitor {
    /// Sample the server of `connection` (doc 04 §Server bandwidth).
    pub(super) fn server(page: &gtk4::Widget, connection: Weak<SshConnection>) -> Self {
        let (samples, mut received) = mpsc::channel(4);
        tokio::spawn(bandwidth::stream_server(connection, samples));
        let weak = page.downgrade();
        let task = glib::MainContext::default().spawn_local(async move {
            let mut meter = RateMeter::default();
            while let Some(counters) = received.recv().await {
                let Some(page) = weak.upgrade() else { break };
                if let Some(rate) = meter.add(counters, Instant::now()) {
                    record(&page, Host::Server, rate);
                }
            }
        });
        Self {
            page: page.downgrade(),
            sampler: Sampler::Server(task),
        }
    }

    /// Sample this machine (doc 06 §Local shell).
    pub(super) fn local(page: &gtk4::Widget) -> Self {
        let weak = page.downgrade();
        let meter = RefCell::new(RateMeter::default());
        let sample = move || {
            let Some(page) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let rate = bandwidth::local_counters()
                .and_then(|counters| meter.borrow_mut().add(counters, Instant::now()));
            if let Some(rate) = rate {
                record(&page, Host::ThisComputer, rate);
            }
            glib::ControlFlow::Continue
        };
        sample();
        let tick = glib::timeout_add_local(INTERVAL, sample);
        Self {
            page: page.downgrade(),
            sampler: Sampler::Local(Some(tick)),
        }
    }
}

fn record(page: &gtk4::Widget, of: Host, rate: bandwidth::Rate) {
    state(page).bandwidth.replace(Some(HostRate { of, rate }));
}

impl Drop for BandwidthMonitor {
    fn drop(&mut self) {
        match &mut self.sampler {
            Sampler::Server(task) => task.abort(),
            Sampler::Local(tick) => {
                if let Some(tick) = tick.take() {
                    tick.remove();
                }
            }
        }
        if let Some(state) = self.page.upgrade().as_ref().and_then(existing_state) {
            state.bandwidth.replace(None);
        }
    }
}

/// Keep `connection` on the page for its monitor, weakly.
pub(super) fn remember_connection(page: &gtk4::Widget, connection: &Arc<SshConnection>) {
    state(page).connection.replace(Arc::downgrade(connection));
}
