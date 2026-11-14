//! The login terminal's keyboard around a pasted code. An agent's login prompt (Claude Code's)
//! swallows a bracketed paste without a trace, so a pasted code is typed in one character at a
//! time; these are the rules around that, kept apart from GTK so they are tested:
//!
//! - Enter pressed while the code is still landing goes after its last character. Passed through,
//!   it sent a truncated code ("invalid code") and the rest landed on the next prompt.
//! - Another paste while one is landing is ignored, and other keys wait.
//! - The Enter that sent a pasted code holds every other Enter for [`HOLD`]: a second one reached
//!   the prompt while the agent was still exchanging the code. Enters with no pasted code (a login
//!   menu, "press Enter to continue") are never held.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How long other Enters are held after the one that sent a pasted code.
pub const HOLD: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Key { Enter, Other }

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum KeyAction {
    /// The terminal gets the key.
    Pass,
    /// The key is held: an Enter goes after the code being typed, anything else is dropped.
    Hold,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Step {
    /// Feed this character.
    Type(char),
    /// The code is typed in; `sent`: an Enter was pressed meanwhile and goes now (the caller feeds it).
    Done { typed: usize, sent: bool },
    /// Nothing to type.
    Idle,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Idle,
    /// From the paste key until the last character is fed (the clipboard read included).
    Typing,
    /// Typed in full, waiting for the person's Enter.
    Typed,
    /// Sent by an Enter at this time.
    Sent(Instant),
}

#[derive(Debug)]
pub struct CodeEntry {
    phase: Phase,
    pending: VecDeque<char>,
    total: usize,
    enter_after: bool,
}

impl Default for CodeEntry {
    fn default() -> Self { Self { phase: Phase::Idle, pending: VecDeque::new(), total: 0, enter_after: false } }
}

impl CodeEntry {
    /// The paste key during a login, before the clipboard is read; false while a code is still
    /// landing (this paste is ignored).
    pub fn begin(&mut self) -> bool {
        if self.phase == Phase::Typing { return false; }
        self.phase = Phase::Typing;
        self.pending.clear();
        self.total = 0;
        self.enter_after = false;
        true
    }

    /// The clipboard's text, after [`begin`](Self::begin): the code to type in (surrounding
    /// whitespace and control characters are not part of it). Its length; 0 ends the paste.
    pub fn paste(&mut self, text: &str) -> usize {
        self.pending = text.trim().chars().filter(|c| !c.is_control()).collect();
        self.total = self.pending.len();
        if self.total == 0 {
            self.phase = Phase::Idle;
            self.enter_after = false;
        }
        self.total
    }

    /// The next thing to feed, on each tick while typing.
    pub fn step(&mut self, now: Instant) -> Step {
        if self.phase != Phase::Typing { return Step::Idle; }
        if let Some(c) = self.pending.pop_front() { return Step::Type(c); }
        let sent = std::mem::take(&mut self.enter_after);
        self.phase = if sent { Phase::Sent(now) } else { Phase::Typed };
        Step::Done { typed: self.total, sent }
    }

    /// A key during a login.
    pub fn key(&mut self, key: Key, now: Instant) -> KeyAction {
        match (self.phase, key) {
            (Phase::Typing, Key::Enter) => {
                self.enter_after = true;
                KeyAction::Hold
            }
            (Phase::Typing, Key::Other) => KeyAction::Hold,
            (Phase::Typed, Key::Enter) => {
                self.phase = Phase::Sent(now);
                KeyAction::Pass
            }
            (Phase::Sent(at), Key::Enter) if now.duration_since(at) < HOLD => KeyAction::Hold,
            (Phase::Sent(_), Key::Enter) => {
                self.phase = Phase::Idle;
                KeyAction::Pass
            }
            _ => KeyAction::Pass,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn type_all(entry: &mut CodeEntry, now: Instant) -> (String, Step) {
        let mut typed = String::new();
        loop {
            match entry.step(now) {
                Step::Type(c) => typed.push(c),
                done => return (typed, done),
            }
        }
    }

    #[test]
    fn an_enter_pressed_while_the_code_lands_goes_after_its_last_character() {
        let (mut entry, now) = (CodeEntry::default(), Instant::now());
        assert!(entry.begin());
        // the person presses Enter before the clipboard is even read: held, not passed
        assert_eq!(entry.key(Key::Enter, now), KeyAction::Hold);
        assert_eq!(entry.paste("  code#state-1234\n"), 15);
        assert_eq!(entry.key(Key::Enter, now), KeyAction::Hold);
        assert_eq!(entry.key(Key::Other, now), KeyAction::Hold, "nothing else lands in the middle of the code");
        let (typed, done) = type_all(&mut entry, now);
        assert_eq!(typed, "code#state-1234");
        assert_eq!(done, Step::Done { typed: 15, sent: true });
    }

    #[test]
    fn a_second_enter_right_after_the_code_was_sent_is_held() {
        let (mut entry, now) = (CodeEntry::default(), Instant::now());
        entry.begin();
        entry.paste("abc");
        assert_eq!(type_all(&mut entry, now).1, Step::Done { typed: 3, sent: false });
        assert_eq!(entry.key(Key::Enter, now), KeyAction::Pass, "the Enter that sends the code");
        assert_eq!(entry.key(Key::Enter, now + Duration::from_millis(300)), KeyAction::Hold);
        assert_eq!(entry.key(Key::Other, now + Duration::from_millis(400)), KeyAction::Pass);
        assert_eq!(entry.key(Key::Enter, now + HOLD), KeyAction::Pass, "a later Enter is the person's again");
        assert_eq!(entry.key(Key::Enter, now + HOLD + Duration::from_millis(100)), KeyAction::Pass, "the hold follows a code only");
    }

    #[test]
    fn enters_with_no_pasted_code_are_never_held() {
        // a login menu, then "press Enter to continue": quick Enters all reach the prompt
        let (mut entry, now) = (CodeEntry::default(), Instant::now());
        for ms in [0, 100, 200] {
            assert_eq!(entry.key(Key::Enter, now + Duration::from_millis(ms)), KeyAction::Pass);
        }
    }

    #[test]
    fn a_paste_while_one_lands_is_ignored_and_an_empty_one_ends_quietly() {
        let (mut entry, now) = (CodeEntry::default(), Instant::now());
        assert!(entry.begin());
        entry.paste("first");
        assert_eq!(entry.step(now), Step::Type('f'));
        assert!(!entry.begin(), "a second paste mid-code");
        assert_eq!(type_all(&mut entry, now).0, "irst");
        assert!(entry.begin());
        assert_eq!(entry.paste(" \n\t"), 0);
        assert_eq!(entry.step(now), Step::Idle);
        assert_eq!(entry.key(Key::Enter, now), KeyAction::Pass);
    }

    #[test]
    fn control_characters_are_not_part_of_the_code() {
        let (mut entry, now) = (CodeEntry::default(), Instant::now());
        entry.begin();
        assert_eq!(entry.paste("ab\u{7}c\u{1b}d\r\n"), 4);
        assert_eq!(type_all(&mut entry, now).0, "abcd");
    }
}
