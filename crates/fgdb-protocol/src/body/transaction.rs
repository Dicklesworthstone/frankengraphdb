//! Connection-scoped explicit native transactions. These selectors and
//! provisional rows are not durable outcome, resume, or reattachment tokens.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TransactionHandle(pub [u8; 16]);

impl TransactionHandle {
    fn check(self) -> Result<(), BodyError> {
        if self.0 == [0; 16] {
            Err(BodyError::Noncanonical)
        } else {
            Ok(())
        }
    }
}

/// A private workspace was opened at this committed basis. BEGIN publishes
/// nothing; a lost connection discards its unpublished effects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransactionBegun {
    pub handle: TransactionHandle,
    pub basis: u64,
}

/// COMMIT, ROLLBACK, and ROLLED_BACK select the connection's current owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransactionSelector {
    pub handle: TransactionHandle,
}

/// One statement in the retained workspace. Read mode cannot stage writes;
/// streaming-source, ordered-source, and subscription modes are not admitted.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecuteTransaction {
    pub handle: TransactionHandle,
    pub statement: Execute,
}

/// All rows of this statement were delivered provisionally. The positive
/// ordinal counts calls within the owner, not committed logical commands.
/// Only TRANSACTION_COMMITTED can establish durable completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransactionResultEnd {
    pub handle: TransactionHandle,
    pub statement: u64,
    pub rows: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransactionCompletion {
    pub handle: TransactionHandle,
    pub outcome: Outcome,
}

body!(
    TransactionBegun,
    |s, out| {
        s.handle.check()?;
        out.array(&s.handle.0);
        out.u64(s.basis);
    },
    |input| {
        let handle = TransactionHandle(input.array()?);
        handle.check()?;
        TransactionBegun {
            handle,
            basis: input.u64()?,
        }
    }
);

body!(
    TransactionSelector,
    |s, out| {
        s.handle.check()?;
        out.array(&s.handle.0);
    },
    |input| {
        let handle = TransactionHandle(input.array()?);
        handle.check()?;
        TransactionSelector { handle }
    }
);

body!(
    ExecuteTransaction,
    |s, out| {
        s.handle.check()?;
        check_len(s.statement.statement.len(), MAX_STATEMENT_BYTES)?;
        let mode = match s.statement.mode {
            ExecuteMode::Read => 0,
            ExecuteMode::Write => 1,
            _ => return Err(BodyError::Noncanonical),
        };
        out.array(&s.handle.0);
        out.u8(mode);
        out.text(&s.statement.statement);
        put_parameters(&mut out, &s.statement.parameters)?;
    },
    |input| {
        let handle = TransactionHandle(input.array()?);
        handle.check()?;
        let mode = match input.u8()? {
            0 => ExecuteMode::Read,
            1 => ExecuteMode::Write,
            _ => return Err(BodyError::Noncanonical),
        };
        ExecuteTransaction {
            handle,
            statement: Execute {
                mode,
                statement: input.text(MAX_STATEMENT_BYTES)?,
                parameters: get_parameters(&mut input)?,
            },
        }
    }
);

body!(
    TransactionResultEnd,
    |s, out| {
        s.handle.check()?;
        if s.statement == 0 {
            return Err(BodyError::Noncanonical);
        }
        out.array(&s.handle.0);
        out.u64(s.statement);
        out.u64(s.rows);
    },
    |input| {
        let handle = TransactionHandle(input.array()?);
        handle.check()?;
        let statement = input.u64()?;
        if statement == 0 {
            return Err(BodyError::Noncanonical);
        }
        TransactionResultEnd {
            handle,
            statement,
            rows: input.u64()?,
        }
    }
);

body!(
    TransactionCompletion,
    |s, out| {
        s.handle.check()?;
        let (tag, seq, statements) = match s.outcome {
            Outcome::WriteCommitted { seq, statements } => (1, seq, statements),
            Outcome::ReadClosed { seq, statements } => (2, seq, statements),
            Outcome::Rows { .. } => return Err(BodyError::Noncanonical),
        };
        out.array(&s.handle.0);
        out.u8(tag);
        out.u64(seq);
        out.u64(statements);
    },
    |input| {
        let handle = TransactionHandle(input.array()?);
        handle.check()?;
        let outcome = match input.u8()? {
            1 => Outcome::WriteCommitted {
                seq: input.u64()?,
                statements: input.u64()?,
            },
            2 => Outcome::ReadClosed {
                seq: input.u64()?,
                statements: input.u64()?,
            },
            _ => return Err(BodyError::UnknownTag),
        };
        TransactionCompletion { handle, outcome }
    }
);

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T: Body + fmt::Debug + PartialEq>(value: &T, expected: &[u8]) {
        assert_eq!(value.encode().unwrap(), expected);
        assert_eq!(&T::decode(expected).unwrap(), value);
        for end in 0..expected.len() {
            assert!(T::decode(&expected[..end]).is_err(), "prefix {end}");
        }
        let mut trailing = expected.to_vec();
        trailing.push(0);
        assert_eq!(T::decode(&trailing).unwrap_err(), BodyError::TrailingBytes);
    }

    #[test]
    fn explicit_transaction_bodies_have_fixed_bytes_and_no_prefix_aliases() {
        let handle = TransactionHandle([7; 16]);
        round_trip(&TransactionSelector { handle }, &[7; 16]);
        let mut begun = vec![7; 16];
        begun.extend_from_slice(&9_u64.to_be_bytes());
        round_trip(&TransactionBegun { handle, basis: 9 }, &begun);
        let mut execute = vec![7; 16];
        execute.extend_from_slice(&[1, 0, 0, 0, 8]);
        execute.extend_from_slice(b"RETURN 1");
        execute.extend_from_slice(&[0; 4]);
        round_trip(
            &ExecuteTransaction {
                handle,
                statement: Execute {
                    mode: ExecuteMode::Write,
                    statement: "RETURN 1".into(),
                    parameters: vec![],
                },
            },
            &execute,
        );
        let mut end = vec![7; 16];
        end.extend_from_slice(&2_u64.to_be_bytes());
        end.extend_from_slice(&3_u64.to_be_bytes());
        round_trip(
            &TransactionResultEnd {
                handle,
                statement: 2,
                rows: 3,
            },
            &end,
        );
        for (tag, outcome) in [
            (1, Outcome::WriteCommitted { seq: 10, statements: 2 }),
            (2, Outcome::ReadClosed { seq: 10, statements: 2 }),
        ] {
            let mut bytes = vec![7; 16];
            bytes.push(tag);
            bytes.extend_from_slice(&10_u64.to_be_bytes());
            bytes.extend_from_slice(&2_u64.to_be_bytes());
            round_trip(&TransactionCompletion { handle, outcome }, &bytes);
        }
    }

    #[test]
    fn transaction_selectors_modes_ordinals_and_completion_classes_fail_closed() {
        let handle = TransactionHandle([1; 16]);
        assert_eq!(
            TransactionSelector { handle: TransactionHandle([0; 16]) }.encode(),
            Err(BodyError::Noncanonical)
        );
        assert_eq!(
            TransactionSelector::decode(&[0; 16]),
            Err(BodyError::Noncanonical)
        );
        let mut statement = ExecuteTransaction {
            handle,
            statement: Execute {
                mode: ExecuteMode::Read,
                statement: "RETURN $x".into(),
                parameters: vec![("x".into(), WireValue::Null)],
            },
        };
        let mut bytes = statement.encode().unwrap();
        for mode in [ExecuteMode::Subscribe, ExecuteMode::ReadStream, ExecuteMode::ReadOrdered] {
            statement.statement.mode = mode;
            assert_eq!(statement.encode(), Err(BodyError::Noncanonical));
        }
        for tag in [2, 3, 4, 255] {
            bytes[16] = tag;
            assert_eq!(ExecuteTransaction::decode(&bytes), Err(BodyError::Noncanonical));
        }
        statement.statement.mode = ExecuteMode::Write;
        statement.statement.parameters.push(("x".into(), WireValue::Int(2)));
        assert_eq!(statement.encode(), Err(BodyError::Noncanonical));
        let mut end = TransactionResultEnd { handle, statement: 1, rows: 0 };
        let mut bytes = end.encode().unwrap();
        bytes[16..24].fill(0);
        assert_eq!(TransactionResultEnd::decode(&bytes), Err(BodyError::Noncanonical));
        end.statement = 0;
        assert_eq!(end.encode(), Err(BodyError::Noncanonical));
        let completion = TransactionCompletion { handle, outcome: Outcome::Rows { seq: 0 } };
        assert_eq!(completion.encode(), Err(BodyError::Noncanonical));
        let mut bytes = vec![1; 16];
        bytes.extend_from_slice(&[0; 17]);
        assert_eq!(TransactionCompletion::decode(&bytes), Err(BodyError::UnknownTag));
    }

    #[test]
    fn transaction_parameters_share_the_body_value_node_allowance() {
        let mut statement = ExecuteTransaction {
            handle: TransactionHandle([1; 16]),
            statement: Execute {
                mode: ExecuteMode::Write,
                statement: "RETURN $a, $b".into(),
                parameters: vec![
                    ("a".into(), WireValue::Vertices(vec![1; MAX_VALUE_NODES - 2])),
                    ("b".into(), WireValue::Null),
                ],
            },
        };
        let bytes = statement.encode().unwrap();
        assert_eq!(ExecuteTransaction::decode(&bytes).unwrap(), statement);
        statement.statement.parameters[1].1 = WireValue::List(vec![WireValue::Null]);
        assert_eq!(statement.encode(), Err(BodyError::TooLarge));
    }
}
