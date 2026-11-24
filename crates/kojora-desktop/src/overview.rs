use eframe::egui;
use kojora_studio::{checks::CheckRecord, worker::WorkerStatus, StudioState};
use std::io;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

pub(super) struct StudioOverview {
    pub status: WorkerStatus,
    pub state: StudioState,
    pub checks: Option<Vec<CheckRecord>>,
}

type RefreshResult = Result<Arc<StudioOverview>, String>;

#[derive(Default)]
pub(super) struct OverviewRefresh {
    current: Option<RefreshResult>,
    pending: Option<mpsc::Receiver<RefreshResult>>,
    next_update: Option<Instant>,
    discard_pending: bool,
}

impl OverviewRefresh {
    pub fn invalidate(&mut self) {
        self.current = None;
        self.next_update = None;
        self.discard_pending = self.pending.is_some();
    }

    pub fn needs_refresh(&self) -> bool {
        self.next_update.is_none() && self.pending.is_none()
    }

    pub fn poll(
        &mut self,
        ctx: &egui::Context,
        now: Instant,
        read: impl FnOnce() -> io::Result<StudioOverview> + Send + 'static,
    ) -> Option<RefreshResult> {
        if let Some(receiver) = &self.pending {
            let result = match receiver.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Err("Studio refresh thread stopped".into())
                }
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(super::WORKER_RETRY_DELAY);
                    return self.current.clone();
                }
            };
            self.pending = None;
            if !std::mem::take(&mut self.discard_pending) {
                self.accept(result, now);
            }
        }

        if self.next_update.is_none_or(|next| now >= next) {
            let repaint = ctx.clone();
            let (sender, receiver) = mpsc::channel();
            match thread::Builder::new()
                .name("kojora-overview".into())
                .spawn(move || {
                    let result = read().map(Arc::new).map_err(|error| error.to_string());
                    let _ = sender.send(result);
                    repaint.request_repaint();
                }) {
                Ok(_) => self.pending = Some(receiver),
                Err(error) => self.accept(Err(error.to_string()), now),
            }
        }

        let delay = if self.pending.is_some() {
            super::WORKER_RETRY_DELAY
        } else {
            self.next_update.map_or(super::WORKER_RETRY_DELAY, |next| {
                next.saturating_duration_since(now)
            })
        };
        ctx.request_repaint_after(delay);
        self.current.clone()
    }

    fn accept(&mut self, result: RefreshResult, now: Instant) {
        let active = result
            .as_ref()
            .is_ok_and(|view| view.status.active_count != 0 || view.status.checks_pending != 0);
        let delay = if active {
            Duration::from_millis(350)
        } else {
            super::WORKER_RETRY_DELAY
        };
        self.next_update = Some(now + delay);
        self.current = Some(result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_keeps_one_background_request_and_discards_an_invalidated_result() -> io::Result<()> {
        let context = egui::Context::default();
        let start = Instant::now();
        let mut overview = OverviewRefresh::default();
        let (started, starting) = mpsc::channel();
        let (release, waiting) = mpsc::channel();
        assert!(overview
            .poll(&context, start, move || {
                started.send(()).map_err(io::Error::other)?;
                waiting
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(io::Error::other)?;
                Err(io::Error::other("stale result"))
            })
            .is_none());
        starting
            .recv_timeout(Duration::from_secs(2))
            .map_err(io::Error::other)?;

        overview.invalidate();
        for _ in 0..10 {
            assert!(overview
                .poll(&context, start, || Err(io::Error::other(
                    "unexpected second request"
                )))
                .is_none());
        }
        release.send(()).map_err(io::Error::other)?;
        let fresh = (0..200).find_map(|_| {
            let result = overview.poll(&context, Instant::now(), || {
                Err(io::Error::other("fresh result"))
            });
            if result.is_none() {
                thread::sleep(Duration::from_millis(5));
            }
            result
        });
        assert!(matches!(fresh, Some(Err(error)) if error == "fresh result"));
        let latest = overview.poll(&context, start, || Err(io::Error::other("too early")));
        assert!(matches!(latest, Some(Err(error)) if error == "fresh result"));
        assert!(overview.pending.is_none());
        Ok(())
    }
}
