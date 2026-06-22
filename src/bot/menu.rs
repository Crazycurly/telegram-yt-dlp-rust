use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

/// What the user picked from the format menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatChoice {
    BestVideo,
    AudioMp3,
}

/// Callback-data tokens. Kept tiny so they fit Telegram's 64-byte limit; the
/// URL itself lives in [`crate::state::PendingJob`], never in callback data.
pub const CB_BEST_VIDEO: &str = "f:bv";
pub const CB_AUDIO_MP3: &str = "f:mp3";
pub const CB_CANCEL: &str = "cancel";

impl FormatChoice {
    pub fn from_callback(data: &str) -> Option<Self> {
        match data {
            CB_BEST_VIDEO => Some(Self::BestVideo),
            CB_AUDIO_MP3 => Some(Self::AudioMp3),
            _ => None,
        }
    }

    pub fn is_audio(self) -> bool {
        matches!(self, Self::AudioMp3)
    }
}

/// The format-selection keyboard shown under the video info.
pub fn format_menu() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback("🎬 Best video (MP4)", CB_BEST_VIDEO),
        InlineKeyboardButton::callback("🎵 MP3 audio", CB_AUDIO_MP3),
    ]])
}

/// A single Cancel button kept on the progress message.
pub fn cancel_menu() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
        "✖ Cancel", CB_CANCEL,
    )]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_roundtrip() {
        assert_eq!(FormatChoice::from_callback(CB_BEST_VIDEO), Some(FormatChoice::BestVideo));
        assert_eq!(FormatChoice::from_callback(CB_AUDIO_MP3), Some(FormatChoice::AudioMp3));
        assert_eq!(FormatChoice::from_callback("nonsense"), None);
        assert!(FormatChoice::AudioMp3.is_audio());
        assert!(!FormatChoice::BestVideo.is_audio());
    }
}
