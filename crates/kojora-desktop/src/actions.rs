use eframe::egui;
use std::io;
use std::sync::mpsc;
use std::thread;

#[derive(Debug)]
pub(super) enum ActionOutcome {
    BotCreated(u64),
    TaskStarted(u64),
    Notice(String),
}

struct PendingAction {
    failure: &'static str,
    result: mpsc::Receiver<io::Result<ActionOutcome>>,
}

#[derive(Default)]
pub(super) struct Actions {
    pending: Option<PendingAction>,
}

impl Actions {
    pub fn is_busy(&self) -> bool {
        self.pending.is_some()
    }

    pub fn start(
        &mut self,
        ctx: &egui::Context,
        failure: &'static str,
        run: impl FnOnce() -> io::Result<ActionOutcome> + Send + 'static,
    ) -> io::Result<()> {
        if self.is_busy() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Another Studio action is pending",
            ));
        }
        let (sender, result) = mpsc::channel();
        let repaint = ctx.clone();
        thread::Builder::new()
            .name("kojora-action".into())
            .spawn(move || {
                let _ = sender.send(run());
                repaint.request_repaint();
            })?;
        self.pending = Some(PendingAction { failure, result });
        Ok(())
    }

    pub fn poll(&mut self) -> Option<Result<ActionOutcome, String>> {
        let pending = self.pending.as_ref()?;
        let result = match pending.result.try_recv() {
            Ok(result) => result.map_err(|error| format!("{}: {error}", pending.failure)),
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => {
                Err(format!("{}: action thread stopped", pending.failure))
            }
        };
        self.pending = None;
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn pending_action_does_not_block_frames_or_allow_duplicate_mutations() -> io::Result<()> {
        let ctx = egui::Context::default();
        let mut actions = Actions::default();
        let (release, gate) = mpsc::channel();
        actions.start(&ctx, "Could not submit", move || {
            gate.recv_timeout(Duration::from_secs(5))
                .map_err(io::Error::other)?;
            Ok(ActionOutcome::TaskStarted(42))
        })?;
        for _ in 0..10 {
            assert!(actions.poll().is_none());
            assert!(actions.is_busy());
        }
        let second = actions.start(&ctx, "Could not submit", || {
            Ok(ActionOutcome::TaskStarted(99))
        });
        assert_eq!(
            second.err().map(|error| error.kind()),
            Some(io::ErrorKind::WouldBlock)
        );
        release.send(()).map_err(io::Error::other)?;
        let result = (0..200).find_map(|_| {
            let result = actions.poll();
            if result.is_none() {
                thread::sleep(Duration::from_millis(5));
            }
            result
        });
        assert!(matches!(result, Some(Ok(ActionOutcome::TaskStarted(42)))));
        assert!(!actions.is_busy());
        assert!(actions.poll().is_none());
        Ok(())
    }
}
