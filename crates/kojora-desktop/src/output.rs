use eframe::egui;
use kojora_studio::{worker::StudioConnection, AgentOutput, AgentProvider};
use std::io;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const REFRESH_INTERVAL: Duration = Duration::from_millis(350);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum OutputSource {
    Agent(u64, AgentProvider),
    TaskLog(u64, bool),
    CheckLog(u64, bool),
}

impl OutputSource {
    fn id(self) -> u64 {
        match self {
            Self::Agent(id, _) | Self::TaskLog(id, _) | Self::CheckLog(id, _) => id,
        }
    }

    pub fn read(self, studio: &StudioConnection) -> io::Result<AgentOutput> {
        match self {
            Self::Agent(task_id, provider) => studio.read_agent_output(task_id, provider),
            Self::TaskLog(task_id, stderr) => {
                studio.read_log_tail(task_id, stderr).map(AgentOutput::Raw)
            }
            Self::CheckLog(check_id, stderr) => {
                studio.check_log(check_id, stderr).map(AgentOutput::Raw)
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum OutputArea {
    Conversation,
    Inspector,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ReadPriority {
    area: OutputArea,
    missing: bool,
    live: bool,
    id: u64,
}

#[derive(Clone, Copy)]
struct OutputRequest {
    source: OutputSource,
    complete: bool,
}

struct CachedOutput {
    request: OutputRequest,
    result: io::Result<AgentOutput>,
    refresh_at: Option<Instant>,
}

#[derive(Default)]
pub(super) struct OutputReads {
    entries: Vec<CachedOutput>,
    visible: Vec<OutputSource>,
    next: Option<(ReadPriority, OutputRequest)>,
    pending: Option<(OutputRequest, mpsc::Receiver<io::Result<AgentOutput>>)>,
}

impl OutputReads {
    pub fn start_frame(&mut self, now: Instant) {
        self.visible.clear();
        self.next = None;
        if let Some((request, receiver)) = &self.pending {
            let result = match receiver.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Err(io::Error::other("Output reader stopped"))
                }
            };
            let request = *request;
            self.pending = None;
            self.record(request, result, now);
        }
    }

    pub fn get(
        &mut self,
        source: OutputSource,
        complete: bool,
        now: Instant,
        area: OutputArea,
    ) -> Option<&io::Result<AgentOutput>> {
        self.visible.push(source);
        let cached = self
            .entries
            .iter()
            .find(|cached| cached.request.source == source);
        let needs_read = cached.is_none_or(|cached| {
            cached.request.complete != complete || cached.refresh_at.is_some_and(|when| now >= when)
        });
        let priority = ReadPriority {
            area,
            missing: cached.is_none(),
            live: !complete,
            id: source.id(),
        };
        if needs_read
            && self.next.is_none_or(|(queued, _)| priority > queued)
            && self
                .pending
                .as_ref()
                .is_none_or(|(request, _)| request.source != source)
        {
            self.next = Some((priority, OutputRequest { source, complete }));
        }
        cached.map(|cached| &cached.result)
    }

    pub fn finish_frame(
        &mut self,
        ctx: &egui::Context,
        now: Instant,
        read: impl FnOnce(OutputSource) -> io::Result<AgentOutput> + Send + 'static,
    ) {
        self.entries
            .retain(|cached| self.visible.contains(&cached.request.source));
        if self.pending.is_some() {
            return;
        }
        if let Some((_, request)) = self.next.take() {
            let context = ctx.clone();
            let (sender, receiver) = mpsc::channel();
            match thread::Builder::new()
                .name("kojora-output".into())
                .spawn(move || {
                    let _ = sender.send(read(request.source));
                    context.request_repaint();
                }) {
                Ok(_) => self.pending = Some((request, receiver)),
                Err(error) => self.record(request, Err(error), now),
            }
        }
        if let Some(next) = self
            .entries
            .iter()
            .filter_map(|cached| cached.refresh_at)
            .min()
        {
            ctx.request_repaint_after(next.saturating_duration_since(now));
        }
    }

    fn record(&mut self, request: OutputRequest, result: io::Result<AgentOutput>, now: Instant) {
        let refresh_at = (!request.complete || result.is_err()).then_some(now + REFRESH_INTERVAL);
        let entry = CachedOutput {
            request,
            result,
            refresh_at,
        };
        if let Some(cached) = self
            .entries
            .iter_mut()
            .find(|cached| cached.request.source == request.source)
        {
            *cached = entry;
        } else {
            self.entries.push(entry);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_visible_conversation_is_loaded_before_older_history() {
        let mut output = OutputReads::default();
        let older = OutputSource::Agent(1, AgentProvider::Codex);
        let newer = OutputSource::Agent(2, AgentProvider::Codex);
        let selected = OutputSource::TaskLog(1, true);
        let now = Instant::now();
        assert!(output
            .get(older, true, now, OutputArea::Conversation)
            .is_none());
        assert!(output
            .get(newer, true, now, OutputArea::Conversation)
            .is_none());
        assert!(output
            .next
            .is_some_and(|(_, request)| request.source == newer));
        assert!(output
            .get(selected, false, now, OutputArea::Inspector)
            .is_none());
        assert!(output
            .next
            .is_some_and(|(_, request)| request.source == selected));

        let later = now + REFRESH_INTERVAL;
        output.start_frame(later);
        output.record(
            OutputRequest {
                source: newer,
                complete: false,
            },
            Ok(AgentOutput::Message("live output".into())),
            now,
        );
        assert!(output
            .get(newer, false, later, OutputArea::Conversation)
            .is_some());
        assert!(output
            .get(older, true, later, OutputArea::Conversation)
            .is_none());
        assert!(output
            .next
            .is_some_and(|(_, request)| request.source == older));
    }

    #[test]
    fn stalled_read_leaves_frames_free_and_never_populates_another_log() -> io::Result<()> {
        let ctx = egui::Context::default();
        let mut output = OutputReads::default();
        let old = OutputSource::TaskLog(1, false);
        let selected = OutputSource::TaskLog(1, true);
        let now = Instant::now();
        let (started_tx, started) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();

        output.start_frame(now);
        assert!(output
            .get(old, false, now, OutputArea::Conversation)
            .is_none());
        output.finish_frame(&ctx, now, move |_| {
            started_tx.send(()).map_err(io::Error::other)?;
            release_rx
                .recv_timeout(Duration::from_secs(5))
                .map_err(io::Error::other)?;
            Ok(AgentOutput::Raw("old stdout".into()))
        });
        started
            .recv_timeout(Duration::from_secs(2))
            .map_err(io::Error::other)?;

        for _ in 0..10 {
            output.start_frame(now);
            assert!(output
                .get(selected, false, now, OutputArea::Conversation)
                .is_none());
            output.finish_frame(&ctx, now, |_| {
                Err(io::Error::other("unexpected second read"))
            });
        }
        release.send(()).map_err(io::Error::other)?;
        for _ in 0..200 {
            output.start_frame(now);
            if output.pending.is_none() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(output.pending.is_none());
        assert!(output
            .get(selected, false, now, OutputArea::Conversation)
            .is_none());
        output.finish_frame(&ctx, now, move |source| {
            assert!(source == selected);
            Ok(AgentOutput::Raw("selected stderr".into()))
        });
        for _ in 0..200 {
            output.start_frame(now);
            if output.pending.is_none() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            matches!(output.get(selected, false, now, OutputArea::Conversation), Some(Ok(AgentOutput::Raw(text))) if text == "selected stderr")
        );
        assert!(output
            .get(old, false, now, OutputArea::Conversation)
            .is_none());
        Ok(())
    }

    #[test]
    fn live_output_is_shared_until_refresh_and_completed_output_is_kept() {
        let ctx = egui::Context::default();
        let mut output = OutputReads::default();
        let source = OutputSource::Agent(3, AgentProvider::Codex);
        let now = Instant::now();
        output.record(
            OutputRequest {
                source,
                complete: false,
            },
            Ok(AgentOutput::Message("partial".into())),
            now,
        );
        for elapsed in [0, 10, 100] {
            let time = now + Duration::from_millis(elapsed);
            output.start_frame(time);
            for _ in 0..2 {
                assert!(
                    matches!(output.get(source, false, time, OutputArea::Conversation), Some(Ok(AgentOutput::Message(text))) if text == "partial")
                );
            }
            assert!(output.next.is_none());
        }

        output.start_frame(now + REFRESH_INTERVAL);
        assert!(output
            .get(
                source,
                false,
                now + REFRESH_INTERVAL,
                OutputArea::Conversation
            )
            .is_some());
        assert!(output.next.is_some());

        output.start_frame(now);
        assert!(output
            .get(source, true, now, OutputArea::Conversation)
            .is_some());
        assert!(output.next.is_some_and(|(_, request)| request.complete));
        output.record(
            OutputRequest {
                source,
                complete: true,
            },
            Ok(AgentOutput::Message("final".into())),
            now,
        );
        output.start_frame(now + Duration::from_secs(10));
        assert!(
            matches!(output.get(source, true, now + Duration::from_secs(10), OutputArea::Conversation), Some(Ok(AgentOutput::Message(text))) if text == "final")
        );
        assert!(output.next.is_none());

        output.start_frame(now);
        output.finish_frame(&ctx, now, |_| {
            Err(io::Error::other("no visible output requested"))
        });
        assert!(output
            .get(source, true, now, OutputArea::Conversation)
            .is_none());
    }
}
