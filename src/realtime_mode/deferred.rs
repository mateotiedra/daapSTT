//! Modifier-safe deferred delivery for Discord realtime dictation.

use log::warn;
use tokio::sync::mpsc;

use crate::live_text::LiveText;
use crate::{deliver, hotkey, placeholder};

const DISCORD_KEY_DELAY_MS: u64 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RealtimeDelivery {
    Live,
    Deferred,
}

impl RealtimeDelivery {
    pub(super) fn is_deferred(self) -> bool {
        self == Self::Deferred
    }
}

pub(super) async fn wait_for_delivery_safety(
    delivery: RealtimeDelivery,
    release_started: bool,
    hotkey_rx: &mut mpsc::Receiver<hotkey::HotkeyEvent>,
) -> bool {
    if release_started {
        return crate::wait_for_release_completion(hotkey_rx).await;
    }
    if !delivery.is_deferred() {
        return true;
    }
    match crate::wait_for_release_started(hotkey_rx).await {
        crate::ReleaseStart::Started => crate::wait_for_release_completion(hotkey_rx).await,
        crate::ReleaseStart::ChannelClosed => false,
    }
}

pub(super) fn accumulate_committed_text(live_text: &mut LiveText, text: &str) {
    *live_text = live_text.commit(text).next;
}

pub(super) async fn deliver_transcript(live_text: &LiveText) {
    let text = live_text.committed_text();
    if text.is_empty() {
        return;
    }
    let chunks = placeholder::parse_banana_chunks(text);
    let result = if chunks
        .iter()
        .any(|chunk| matches!(chunk, placeholder::TranscriptChunk::ClipboardPlaceholder))
    {
        deliver::deliver_chunks_paced(&chunks, DISCORD_KEY_DELAY_MS).await
    } else {
        deliver::type_text_paced(text, DISCORD_KEY_DELAY_MS).await
    };
    if let Err(e) = result {
        warn!("failed to deliver deferred Discord transcript: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_segments_accumulate_without_a_mutable_tail() {
        let mut text = LiveText::new();
        accumulate_committed_text(&mut text, "first");
        accumulate_committed_text(&mut text, "second segment");

        assert_eq!(text.committed_text(), "first second segment");
        assert_eq!(text.tail(), "");
    }

    #[tokio::test]
    async fn deferred_delivery_waits_for_both_release_phases() {
        let (tx, mut rx) = mpsc::channel(2);
        tx.send(hotkey::HotkeyEvent::ReleaseStarted).await.unwrap();
        tx.send(hotkey::HotkeyEvent::ReleaseCompleted)
            .await
            .unwrap();

        assert!(wait_for_delivery_safety(RealtimeDelivery::Deferred, false, &mut rx).await);
    }

    #[tokio::test]
    async fn live_delivery_without_a_release_start_does_not_wait() {
        let (_tx, mut rx) = mpsc::channel(1);

        assert!(wait_for_delivery_safety(RealtimeDelivery::Live, false, &mut rx).await);
    }
}
