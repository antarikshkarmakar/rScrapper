use crate::{Error, Result};

pub(super) const MAX_DOM_DEPTH: usize = 256;

pub(super) struct FinalWriter {
    output: String,
    used_chars: usize,
    max_chars: usize,
    reserved_chars: usize,
    pending_space: bool,
    line_start: bool,
}

impl FinalWriter {
    pub(super) fn new(max_chars: usize) -> Self {
        Self {
            output: String::new(),
            used_chars: 0,
            max_chars,
            reserved_chars: 0,
            pending_space: false,
            line_start: true,
        }
    }

    pub(super) fn remaining(&self) -> usize {
        self.max_chars
            .saturating_sub(self.used_chars + self.reserved_chars)
    }

    pub(super) fn is_line_start(&self) -> bool {
        self.line_start
    }

    pub(super) fn request_space(&mut self) {
        if !self.line_start {
            self.pending_space = true;
        }
    }

    pub(super) fn discard_pending_space(&mut self) {
        self.pending_space = false;
    }

    pub(super) fn rewrite_last_scalar_as_numeric_entity(&mut self, ch: char) -> Result<()> {
        let mut value = ch as u32;
        let mut digits = [0u8; 10];
        let mut first_digit = digits.len();
        loop {
            first_digit -= 1;
            digits[first_digit] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }

        let digit_count = digits.len() - first_digit;
        let additional_chars = digit_count + 2;
        if additional_chars > self.remaining() {
            return Err(Error::BodyLimit {
                limit: self.max_chars,
            });
        }

        debug_assert!(!self.pending_space);
        let removed = self.output.pop();
        debug_assert_eq!(removed, Some(ch));
        self.used_chars -= 1;
        self.write_finalized_char('&')?;
        self.write_finalized_char('#')?;
        for digit in &digits[first_digit..] {
            self.write_finalized_char(char::from(*digit))?;
        }
        self.write_finalized_char(';')
    }

    pub(super) fn write_literal(&mut self, text: &str) -> Result<()> {
        for ch in text.chars() {
            self.write_char(ch)?;
        }
        Ok(())
    }

    pub(super) fn write_char(&mut self, ch: char) -> Result<()> {
        if self.pending_space {
            if ch == '\n' || ch == '\r' {
                self.pending_space = false;
            } else {
                self.pending_space = false;
                self.write_finalized_char(' ')?;
            }
        }
        self.write_finalized_char(ch)
    }

    pub(super) fn write_normalized_text(&mut self, text: &str) -> Result<()> {
        for ch in text.chars() {
            if ch.is_whitespace() {
                self.request_space();
            } else {
                self.write_char(ch)?;
            }
        }
        Ok(())
    }

    pub(super) fn newline(&mut self) -> Result<()> {
        self.discard_pending_space();
        if !self.line_start {
            self.write_finalized_char('\n')?;
        }
        Ok(())
    }

    pub(super) fn blank_line(&mut self) -> Result<()> {
        self.discard_pending_space();
        if self.output.is_empty() {
            return Ok(());
        }
        if !self.line_start {
            self.write_finalized_char('\n')?;
        }
        if !self.output.ends_with("\n\n") {
            self.write_finalized_char('\n')?;
        }
        Ok(())
    }

    pub(super) fn reserve(&mut self, chars: usize) -> Result<()> {
        if chars > self.remaining() {
            return Err(Error::BodyLimit {
                limit: self.max_chars,
            });
        }
        self.reserved_chars += chars;
        Ok(())
    }

    pub(super) fn release(&mut self, chars: usize) {
        debug_assert!(chars <= self.reserved_chars);
        self.reserved_chars = self.reserved_chars.saturating_sub(chars);
    }

    pub(super) fn finish(mut self) -> Result<String> {
        self.discard_pending_space();
        Ok(self.output)
    }

    fn write_finalized_char(&mut self, ch: char) -> Result<()> {
        if self.remaining() == 0 {
            return Err(Error::BodyLimit {
                limit: self.max_chars,
            });
        }
        self.output.push(ch);
        self.used_chars += 1;
        self.line_start = ch == '\n' || ch == '\r';
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::FinalWriter;
    use crate::Error;

    #[test]
    fn finalized_character_count_is_tracked_as_text_is_written() {
        let mut writer = FinalWriter::new(4);

        writer.write_literal("a🦀b").unwrap();

        assert_eq!(writer.used_chars, 3);
        assert_eq!(writer.remaining(), 1);
    }

    #[test]
    fn reservations_leave_room_for_closing_syntax() {
        let mut writer = FinalWriter::new(3);

        writer.reserve(2).unwrap();
        let error = writer.write_literal("**").unwrap_err();

        assert!(matches!(error, Error::BodyLimit { limit: 3 }));
        assert_eq!(writer.used_chars, 1);
        assert_eq!(writer.reserved_chars, 2);
    }

    #[test]
    fn releasing_a_reservation_makes_its_budget_writable() {
        let mut writer = FinalWriter::new(4);

        writer.reserve(2).unwrap();
        writer.write_literal("**").unwrap();
        writer.release(2);
        writer.write_literal("**").unwrap();

        assert_eq!(writer.finish().unwrap(), "****");
    }

    #[test]
    fn last_text_scalar_can_be_entity_encoded_without_spending_reserved_closers() {
        let mut exact = FinalWriter::new(7);
        exact.reserve(2).unwrap();
        exact.write_char('A').unwrap();

        exact.rewrite_last_scalar_as_numeric_entity('A').unwrap();
        assert_eq!(exact.used_chars, 5);
        assert_eq!(exact.reserved_chars, 2);
        assert_eq!(exact.remaining(), 0);
        exact.release(2);
        exact.write_literal("**").unwrap();
        assert_eq!(exact.finish().unwrap(), "&#65;**");

        let mut one_short = FinalWriter::new(6);
        one_short.reserve(2).unwrap();
        one_short.write_char('A').unwrap();
        assert!(matches!(
            one_short.rewrite_last_scalar_as_numeric_entity('A'),
            Err(Error::BodyLimit { limit: 6 })
        ));
        assert_eq!(one_short.used_chars, 1);
        assert_eq!(one_short.reserved_chars, 2);
    }

    #[test]
    fn pending_trailing_space_is_free_and_discarded_at_finish() {
        let mut writer = FinalWriter::new(1);

        writer.write_normalized_text("x ").unwrap();

        assert!(writer.pending_space);
        assert_eq!(writer.used_chars, 1);
        assert_eq!(writer.remaining(), 0);
        assert_eq!(writer.finish().unwrap(), "x");
    }

    #[test]
    fn leading_and_internal_whitespace_are_normalized_while_streaming() {
        let mut writer = FinalWriter::new(3);

        writer.write_normalized_text("   x     y   ").unwrap();

        assert_eq!(writer.finish().unwrap(), "x y");
    }

    #[test]
    fn a_unicode_crab_uses_one_character_of_budget() {
        let mut writer = FinalWriter::new(1);

        writer.write_char('🦀').unwrap();

        assert_eq!(writer.finish().unwrap(), "🦀");
    }

    #[test]
    fn newlines_update_line_state_without_emitting_pending_spaces() {
        let mut writer = FinalWriter::new(4);

        assert!(writer.is_line_start());
        writer.write_normalized_text("x ").unwrap();
        writer.newline().unwrap();
        assert!(writer.is_line_start());
        writer.write_literal("y").unwrap();

        assert_eq!(writer.finish().unwrap(), "x\ny");
    }

    #[test]
    fn blank_line_is_idempotent_and_finish_never_exceeds_the_limit() {
        for max_chars in 0..=6 {
            let mut writer = FinalWriter::new(max_chars);
            let result = (|| {
                writer.write_normalized_text("a ")?;
                writer.blank_line()?;
                writer.blank_line()?;
                writer.write_normalized_text("🦀")?;
                writer.request_space();
                writer.discard_pending_space();
                writer.finish()
            })();

            if let Ok(output) = result {
                assert!(output.chars().count() <= max_chars);
            }
        }
    }
}
