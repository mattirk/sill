//! Two presses for what cannot be taken back: the first press of a destructive action arms it,
//! the second press of the same thing confirms it. What "the same thing" is belongs to the key:
//! where the person confirms what they were shown, the key holds that too, so a press after it
//! changed arms again.

/// A destructive action's first press: the second press of the same thing confirms it.
#[derive(Debug, Clone, PartialEq)]
pub struct Armed<K>(Option<K>);

impl<K> Default for Armed<K> {
    fn default() -> Self {
        Self(None)
    }
}

impl<K: PartialEq> Armed<K> {
    /// A press of `key`: true when this is the confirming press (and the state clears);
    /// otherwise `key` becomes what is armed.
    pub fn press(&mut self, key: K) -> bool {
        if self.0.as_ref() == Some(&key) {
            self.0 = None;
            return true;
        }
        self.0 = Some(key);
        false
    }

    /// Whether `key` is what is armed: its button asks for the second press.
    pub fn is(&self, key: &K) -> bool {
        self.0.as_ref() == Some(key)
    }

    pub fn clear(&mut self) {
        self.0 = None;
    }

    /// What is armed, for a caller that compares a part of the key.
    pub fn get(&self) -> Option<&K> {
        self.0.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::Armed;

    #[test]
    fn the_second_press_of_the_same_key_confirms_and_clears() {
        let mut armed = Armed::default();
        assert!(!armed.press("a"));
        assert!(armed.is(&"a") && !armed.is(&"b"));
        assert_eq!(armed.get(), Some(&"a"));
        assert!(armed.press("a"));
        assert_eq!(armed.get(), None);
        // confirmed is not armed: the next press starts over
        assert!(!armed.press("a"));
    }

    #[test]
    fn a_press_of_another_key_arms_that_one_instead() {
        let mut armed = Armed::default();
        assert!(!armed.press(("ws", 1)));
        // what was shown changed between the presses: armed again, for what is shown now
        assert!(!armed.press(("ws", 2)));
        assert!(armed.is(&("ws", 2)));
        assert!(armed.press(("ws", 2)));
    }

    #[test]
    fn clearing_takes_the_first_press_back() {
        let mut armed = Armed::default();
        assert!(!armed.press(()));
        armed.clear();
        assert!(!armed.is(&()));
        assert!(!armed.press(()));
        assert!(armed.press(()));
    }
}
