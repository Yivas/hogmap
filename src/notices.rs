//! Session-only, bounded notices. Paths are never written to a log.
use std::collections::VecDeque;
use std::time::SystemTime;

pub const MAX_NOTICES: usize = 256;
pub const MAX_DETAIL_CHARS: usize = 2048;

#[derive(Debug)]
pub struct Notice {
    pub time: SystemTime,
    pub text: String,
    pub detail: String,
}
#[derive(Debug, Default)]
pub struct Notices {
    pub silent: bool,
    entries: VecDeque<Notice>,
}
impl Notices {
    pub fn push(&mut self, text: &str, detail: &str) {
        if self.silent {
            return;
        }
        if self.entries.len() == MAX_NOTICES {
            self.entries.pop_front();
        }
        self.entries.push_back(Notice {
            time: SystemTime::now(),
            text: text.chars().take(160).collect(),
            detail: detail.chars().take(MAX_DETAIL_CHARS).collect(),
        });
    }
    pub fn directory_failure(&mut self, code: u32) {
        self.push(&format!("Directory incomplete · code {code}"), &format!("Directory enumeration failed or was skipped (code {code}). Unknown metadata is not zero."));
    }
    pub fn entries(&self) -> impl DoubleEndedIterator<Item = &Notice> {
        self.entries.iter()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn notices_are_bounded_silence_preserves_history_and_failure_retains_code() {
        let mut notices = Notices::default();
        for _ in 0..300 {
            notices.push("notice", "detail");
        }
        assert_eq!(notices.entries().count(), 256);
        notices.directory_failure(5);
        assert!(notices.entries().last().unwrap().text.contains("code 5"));
        notices.silent = true;
        notices.directory_failure(123);
        assert_eq!(notices.entries().count(), 256);
        assert!(notices.entries().last().unwrap().detail.contains("code 5"));
    }
}
