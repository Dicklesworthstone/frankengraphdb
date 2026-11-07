//! Live admission for row binding and its shared value conversions.
//!
//! Counters cover explicit conversion/copy/ordering loops. Allocation, native
//! numeric parsing and canonical constructors remain indivisible calls; this
//! is a work boundary, not a wall-clock or allocator bound.

pub(crate) const BINDING_RUN_BYTES: usize = 64 * 1024;
const BINDING_RUN_ITEMS: usize = 64;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ControlledError<E, C> {
    Invalid(E),
    Interrupted(C),
}

impl<E, C> From<E> for ControlledError<E, C> {
    fn from(error: E) -> Self {
        Self::Invalid(error)
    }
}

impl<E, C> ControlledError<E, C> {
    pub(crate) fn map_invalid<F>(self, map: impl FnOnce(E) -> F) -> ControlledError<F, C> {
        match self {
            Self::Invalid(error) => ControlledError::Invalid(map(error)),
            Self::Interrupted(error) => ControlledError::Interrupted(error),
        }
    }
}

pub(crate) struct BindingControl<'a, C> {
    poll: &'a mut dyn FnMut() -> Result<(), C>,
    bytes: usize,
    items: usize,
}

impl<'a, C> BindingControl<'a, C> {
    pub(crate) fn new(poll: &'a mut dyn FnMut() -> Result<(), C>) -> Self {
        Self {
            poll,
            bytes: BINDING_RUN_BYTES,
            items: BINDING_RUN_ITEMS,
        }
    }

    pub(crate) fn checkpoint(&mut self) -> Result<(), C> {
        (self.poll)()?;
        self.bytes = BINDING_RUN_BYTES;
        self.items = BINDING_RUN_ITEMS;
        Ok(())
    }

    pub(crate) fn item(&mut self) -> Result<(), C> {
        if self.items == 0 || self.bytes == 0 {
            self.checkpoint()?;
        }
        self.items -= 1;
        Ok(())
    }

    /// Admit one actual byte run before its scan/copy. Callers split runs at
    /// BINDING_RUN_BYTES; one exhausted budget refreshes both counters.
    pub(crate) fn bytes(&mut self, bytes: usize) -> Result<(), C> {
        assert!(bytes <= BINDING_RUN_BYTES);
        if bytes > self.bytes || self.items == 0 {
            self.checkpoint()?;
        }
        self.bytes -= bytes;
        Ok(())
    }

    /// Native number parsing has no fallible callback. Large source tokens
    /// acquire byte-run grants before that indivisible call; this does not
    /// claim to interrupt the standard-library conversion internally.
    pub(crate) fn numeric_token(&mut self, text: &str) -> Result<(), C> {
        for bytes in text.as_bytes().chunks(BINDING_RUN_BYTES) {
            self.bytes(bytes.len())?;
        }
        Ok(())
    }

    pub(crate) fn native_complete(&mut self, text: &str) -> Result<(), C> {
        if text.len() > BINDING_RUN_BYTES {
            self.checkpoint()?;
        }
        Ok(())
    }

    pub(crate) fn trim<'s>(&mut self, text: &'s str) -> Result<&'s str, C> {
        let mut low = 0;
        for ch in text.chars() {
            self.bytes(ch.len_utf8())?;
            if !ch.is_whitespace() {
                break;
            }
            low += ch.len_utf8();
        }
        let mut high = text.len();
        for ch in text[low..].chars().rev() {
            self.bytes(ch.len_utf8())?;
            if !ch.is_whitespace() {
                break;
            }
            high -= ch.len_utf8();
        }
        Ok(&text[low..high])
    }

    pub(crate) fn comma_fields<'s>(
        &mut self,
        text: &'s str,
        limit: usize,
    ) -> Result<Vec<&'s str>, C> {
        let mut fields = Vec::with_capacity(limit.min(text.len().saturating_add(1)));
        if limit == 0 {
            return Ok(fields);
        }
        if limit == 1 {
            fields.push(text);
            return Ok(fields);
        }
        let mut start = 0;
        let mut base = 0;
        for chunk in text.as_bytes().chunks(BINDING_RUN_BYTES) {
            self.bytes(chunk.len())?;
            for (offset, byte) in chunk.iter().enumerate() {
                if *byte == b',' {
                    let end = base + offset;
                    fields.push(&text[start..end]);
                    start = end + 1;
                    if fields.len() + 1 == limit {
                        fields.push(&text[start..]);
                        return Ok(fields);
                    }
                }
            }
            base += chunk.len();
        }
        fields.push(&text[start..]);
        Ok(fields)
    }

    pub(crate) fn text(&mut self, text: &str) -> Result<String, C> {
        let mut output = String::with_capacity(text.len());
        let mut low = 0;
        while low < text.len() {
            let mut high = low.saturating_add(BINDING_RUN_BYTES).min(text.len());
            while !text.is_char_boundary(high) {
                high -= 1;
            }
            self.bytes(high - low)?;
            output.push_str(&text[low..high]);
            low = high;
        }
        Ok(output)
    }

    /// In-place fallible heapsort. Keys must be cheap fixed-size values. No
    /// payload cloning or auxiliary sort allocation; refusal leaves private
    /// state that the owning row discards. Equal keys may move.
    pub(crate) fn sort_by_key<T, K: Ord>(
        &mut self,
        values: &mut [T],
        key: impl Fn(&T) -> K,
    ) -> Result<(), C> {
        fn sift<T, K: Ord, C>(
            values: &mut [T],
            mut root: usize,
            end: usize,
            key: &impl Fn(&T) -> K,
            control: &mut BindingControl<'_, C>,
        ) -> Result<(), C> {
            while root < end / 2 {
                control.item()?;
                let mut child = root * 2 + 1;
                if child + 1 < end && key(&values[child]) < key(&values[child + 1]) {
                    child += 1;
                }
                if key(&values[root]) >= key(&values[child]) {
                    break;
                }
                values.swap(root, child);
                root = child;
            }
            Ok(())
        }

        // Canonical input is common. Its linear check also remains governed.
        let mut ordered = true;
        for pair in values.windows(2) {
            self.item()?;
            if key(&pair[0]) > key(&pair[1]) {
                ordered = false;
                break;
            }
        }
        if ordered {
            return Ok(());
        }
        let len = values.len();
        for root in (0..len / 2).rev() {
            sift(values, root, len, &key, self)?;
        }
        for end in (1..len).rev() {
            self.item()?;
            values.swap(0, end);
            sift(values, 0, end, &key, self)?;
        }
        Ok(())
    }
}

/// The shared vector decoder scans delimiters in bounded byte runs and checks
/// each coordinate before conversion. Numeric conversion itself is native.
pub(crate) fn packed_vector<C>(
    text: &str,
    control: &mut BindingControl<'_, C>,
) -> Result<Vec<u8>, ControlledError<&'static str, C>> {
    fn coordinate<C>(
        raw: &str,
        bytes: &mut Vec<u8>,
        control: &mut BindingControl<'_, C>,
    ) -> Result<(), ControlledError<&'static str, C>> {
        control.item().map_err(ControlledError::Interrupted)?;
        let raw = control.trim(raw).map_err(ControlledError::Interrupted)?;
        control
            .numeric_token(raw)
            .map_err(ControlledError::Interrupted)?;
        let value: f32 = raw
            .parse()
            .map_err(|_| "vector: is comma-separated numbers")?;
        control
            .native_complete(raw)
            .map_err(ControlledError::Interrupted)?;
        if !value.is_finite() {
            return Err("vector: coordinates must be finite f32".into());
        }
        bytes.extend_from_slice(&value.to_le_bytes());
        Ok(())
    }
    let mut bytes = Vec::new();
    let mut start = 0;
    let mut base = 0;
    for chunk in text.as_bytes().chunks(BINDING_RUN_BYTES) {
        control
            .bytes(chunk.len())
            .map_err(ControlledError::Interrupted)?;
        for (offset, byte) in chunk.iter().enumerate() {
            if *byte == b',' {
                let end = base + offset;
                coordinate(&text[start..end], &mut bytes, control)?;
                start = end + 1;
            }
        }
        base += chunk.len();
    }
    coordinate(&text[start..], &mut bytes, control)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::convert::Infallible;
    use std::rc::Rc;

    #[test]
    fn short_binding_runs_reuse_the_outer_admission() {
        let mut poll = || -> Result<(), Infallible> { panic!("unnecessary grant") };
        let mut control = BindingControl::new(&mut poll);
        assert_eq!(control.text("Ada").unwrap(), "Ada");
        let mut labels = [9, 1, 4];
        control.sort_by_key(&mut labels, |v| *v).unwrap();
        assert_eq!(labels, [1, 4, 9]);
    }

    #[test]
    fn diagnostic_mapping_never_replaces_typed_cancellation() {
        let invalid: ControlledError<&str, Rc<u8>> = ControlledError::Invalid("old diagnostic");
        assert_eq!(
            invalid.map_invalid(str::to_owned),
            ControlledError::Invalid("old diagnostic".to_owned())
        );
        let token = Rc::new(7);
        let cancelled: ControlledError<&str, _> = ControlledError::Interrupted(token.clone());
        let ControlledError::Interrupted(error) =
            cancelled.map_invalid(|_| -> String { panic!("mapped control error") })
        else {
            panic!("lost cancellation")
        };
        assert!(Rc::ptr_eq(&error, &token));
    }

    #[test]
    fn chunked_utf8_copy_preserves_bytes_and_the_original_non_send_refusal() {
        let text = format!(
            "{}🦀{}",
            "x".repeat(BINDING_RUN_BYTES - 1),
            "é".repeat(BINDING_RUN_BYTES)
        );
        let count = Cell::new(0);
        let mut success = || {
            count.set(count.get() + 1);
            Ok::<(), Infallible>(())
        };
        assert_eq!(BindingControl::new(&mut success).text(&text).unwrap(), text);
        assert!(count.get() >= 2);
        for cancel in 1..=count.get() {
            let token = Rc::new(cancel);
            let mut seen = 0;
            let mut poll = || {
                seen += 1;
                if seen == cancel {
                    Err(token.clone())
                } else {
                    Ok(())
                }
            };
            let error = BindingControl::new(&mut poll).text(&text).unwrap_err();
            assert!(Rc::ptr_eq(&error, &token));
        }
    }

    #[test]
    fn ordering_matches_standard_sort_and_every_refusal_is_terminal() {
        for len in 0..=193 {
            let input: Vec<_> = (0..len).map(|n| ((n * 71 + 23) % 29, n)).collect();
            let mut expected = input.clone();
            expected.sort_unstable();
            let mut actual = input.clone();
            let mut polls = 0;
            let mut poll = || {
                polls += 1;
                Ok::<(), Infallible>(())
            };
            BindingControl::new(&mut poll)
                .sort_by_key(&mut actual, |v| *v)
                .unwrap();
            assert_eq!(actual, expected);
            for cancel in 1..=polls {
                let mut actual = input.clone();
                let token = Rc::new(cancel);
                let mut seen = 0;
                let mut poll = || {
                    seen += 1;
                    if seen == cancel {
                        Err(token.clone())
                    } else {
                        Ok(())
                    }
                };
                let error = BindingControl::new(&mut poll)
                    .sort_by_key(&mut actual, |v| *v)
                    .unwrap_err();
                assert!(Rc::ptr_eq(&error, &token));
                assert_eq!(seen, cancel);
            }
        }
    }

    #[test]
    fn vector_conversion_keeps_native_values_and_first_errors() {
        fn original(text: &str) -> Result<Vec<u8>, &'static str> {
            let mut bytes = Vec::new();
            for raw in text.split(',') {
                let value: f32 = raw
                    .trim()
                    .parse()
                    .map_err(|_| "vector: is comma-separated numbers")?;
                if !value.is_finite() {
                    return Err("vector: coordinates must be finite f32");
                }
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            Ok(bytes)
        }
        let cases = [
            "",
            "0",
            "-0,+0,1e-45,3.4028235e38",
            " 1,\u{2003}-2\u{2003},3 ",
            "1,",
            ",1",
            "1,,2",
            "garbage,NaN",
            "NaN,garbage",
            "1e999,garbage",
            "0x1",
            "infinity",
            "1\0",
            "+",
            "  ",
        ];
        for text in cases {
            let mut poll = || Ok::<(), Infallible>(());
            assert_eq!(
                packed_vector(text, &mut BindingControl::new(&mut poll)),
                original(text).map_err(ControlledError::Invalid),
                "{text:?}",
            );
        }
        let text = format!("{}1,2", "0".repeat(BINDING_RUN_BYTES + 19));
        let mut poll = || Ok::<(), Infallible>(());
        assert_eq!(
            packed_vector(&text, &mut BindingControl::new(&mut poll)),
            original(&text).map_err(ControlledError::Invalid)
        );
    }

    #[test]
    fn every_vector_run_can_refuse_with_the_same_non_send_token() {
        let text = "1.5,-2,3,".repeat(BINDING_RUN_ITEMS * 4) + "4";
        let mut polls = 0;
        let mut poll = || {
            polls += 1;
            Ok::<(), Infallible>(())
        };
        packed_vector(&text, &mut BindingControl::new(&mut poll)).unwrap();
        assert!(polls > 1);
        for cancel in 1..=polls {
            let token = Rc::new(cancel);
            let mut seen = 0;
            let mut poll = || {
                seen += 1;
                if seen == cancel {
                    Err(token.clone())
                } else {
                    Ok(())
                }
            };
            let error = packed_vector(&text, &mut BindingControl::new(&mut poll)).unwrap_err();
            let ControlledError::Interrupted(error) = error else {
                panic!("not cancellation")
            };
            assert!(Rc::ptr_eq(&error, &token));
            assert_eq!(seen, cancel);
        }
    }

    #[test]
    fn unicode_trim_uses_real_byte_runs_before_native_coordinate_parsing() {
        for text in [
            "",
            "abc",
            " \tabc\n",
            "\u{2003}abc\u{3000}",
            "\u{200b}",
            " \n\t\u{2003}",
        ] {
            let mut poll = || Ok::<(), Infallible>(());
            assert_eq!(
                BindingControl::new(&mut poll).trim(text).unwrap(),
                text.trim()
            );
        }
        let text = "\u{2003}".repeat(BINDING_RUN_BYTES) + "1" + &" ".repeat(BINDING_RUN_BYTES);
        let mut polls = 0;
        let mut poll = || {
            polls += 1;
            Ok::<(), Infallible>(())
        };
        assert_eq!(BindingControl::new(&mut poll).trim(&text).unwrap(), "1");
        assert!(polls >= 3);
        for cancel in 1..=polls {
            let token = Rc::new(cancel);
            let mut seen = 0;
            let mut poll = || {
                seen += 1;
                if seen == cancel {
                    Err(token.clone())
                } else {
                    Ok(())
                }
            };
            let error = BindingControl::new(&mut poll).trim(&text).unwrap_err();
            assert!(Rc::ptr_eq(&error, &token));
            assert_eq!(seen, cancel);
        }
    }

    #[test]
    fn bounded_comma_fields_keep_splitn_order_and_poll_inside_long_fields() {
        for text in ["", ",", ",,,,", "a,b,c,d,e,f", "α,β,,δ,🦀", "plain"] {
            for limit in 0..=7 {
                let mut poll = || Ok::<(), Infallible>(());
                assert_eq!(
                    BindingControl::new(&mut poll)
                        .comma_fields(text, limit)
                        .unwrap(),
                    text.splitn(limit, ',').collect::<Vec<_>>()
                );
            }
        }
        let text = "🦀".repeat(BINDING_RUN_BYTES) + ",1,2,3,4,5";
        let mut polls = 0;
        let mut poll = || {
            polls += 1;
            Ok::<(), Infallible>(())
        };
        assert_eq!(
            BindingControl::new(&mut poll)
                .comma_fields(&text, 5)
                .unwrap(),
            text.splitn(5, ',').collect::<Vec<_>>()
        );
        assert!(polls >= 3);
        for cancel in 1..=polls {
            let token = Rc::new(cancel);
            let mut seen = 0;
            let mut poll = || {
                seen += 1;
                if seen == cancel {
                    Err(token.clone())
                } else {
                    Ok(())
                }
            };
            let error = BindingControl::new(&mut poll)
                .comma_fields(&text, 5)
                .unwrap_err();
            assert!(Rc::ptr_eq(&error, &token));
            assert_eq!(seen, cancel);
        }
    }
}
