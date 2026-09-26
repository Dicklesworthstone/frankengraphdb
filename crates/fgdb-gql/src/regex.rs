//! A bounded, deterministic regular-expression matcher: fgdb regex profile 1,
//! the dialect of openCypher `=~` (fgdb-20foe).
//!
//! Thompson construction and a Pike VM. Matching costs O(text x program) time
//! and O(program) memory, with no backtracking, so no pattern has a
//! catastrophic case. The program size is capped when the pattern is compiled,
//! which happens once, at query preparation. A match is a WHOLE-string match,
//! as openCypher defines `=~`.
//!
//! Profile 1 syntax:
//! - literals and `.`, which is any character except a line terminator;
//! - escapes: `\d \D \w \W \s \S \t \n \r` and any escaped punctuation;
//! - classes `[...]` / `[^...]` with ranges and the escapes above;
//! - groups `(...)` and `(?:...)`;
//! - alternation `|`;
//! - quantifiers `* + ?`, `{m}`, `{m,}` and `{m,n}`, where a trailing lazy
//!   `?` is accepted and has no effect on a whole-string match;
//! - `^` only at the very start and `$` only at the very end, both redundant
//!   under whole-string matching;
//! - a leading `(?i)` selects simple one-to-one Unicode case folding.
//!
//! Anything else (backreferences, lookaround, word boundaries, possessive
//! quantifiers, other flags) is a typed refusal, never a guess.

/// The largest compiled program, in instructions.
pub const MAX_REGEX_PROGRAM: usize = 4_096;
/// The largest bound a `{m,n}` quantifier may name.
pub const MAX_REGEX_REPEAT: u32 = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegexErrorKind {
    /// The pattern is malformed or uses syntax outside profile 1.
    Syntax,
    /// A `{m,n}` bound is out of order or above MAX_REGEX_REPEAT.
    Repeat,
    /// The compiled program would exceed MAX_REGEX_PROGRAM instructions.
    TooLarge,
}

/// A refusal at a character offset into the pattern.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegexError {
    pub at: usize,
    pub kind: RegexErrorKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Inst {
    Char(char),
    Any,
    Class {
        ranges: Vec<(char, char)>,
        negated: bool,
    },
    Split(usize, usize),
    Jump(usize),
    Match,
}

/// A compiled profile-1 pattern. It compares by its source and flags, since
/// the program is a pure function of them.
#[derive(Clone, PartialEq, Eq)]
pub struct CompiledRegex {
    source: String,
    insensitive: bool,
    program: Vec<Inst>,
}

impl core::fmt::Debug for CompiledRegex {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("CompiledRegex([REDACTED])")
    }
}

#[derive(Clone, Debug)]
enum Node {
    Empty,
    Char(char),
    Any,
    Class(Vec<(char, char)>, bool),
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat {
        node: Box<Node>,
        min: u32,
        max: Option<u32>,
    },
}

const DIGIT: &[(char, char)] = &[('0', '9')];
const WORD: &[(char, char)] = &[('0', '9'), ('A', 'Z'), ('_', '_'), ('a', 'z')];
const SPACE: &[(char, char)] = &[('\t', '\r'), (' ', ' ')];

fn is_line_terminator(ch: char) -> bool {
    matches!(ch, '\n' | '\r' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// Simple one-to-one case folding; a character whose lowercase is not a
/// single character folds to itself.
fn fold(ch: char) -> char {
    let mut lower = ch.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(single), None) => single,
        _ => ch,
    }
}

struct Parser {
    chars: Vec<char>,
    at: usize,
}

impl Parser {
    fn error(&self, kind: RegexErrorKind) -> RegexError {
        RegexError { at: self.at, kind }
    }
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }
    fn eat(&mut self, ch: char) -> bool {
        if self.peek() == Some(ch) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn alternation(&mut self, depth: usize) -> Result<Node, RegexError> {
        if depth > 64 {
            return Err(self.error(RegexErrorKind::TooLarge));
        }
        let mut arms = vec![self.concatenation(depth)?];
        while self.eat('|') {
            arms.push(self.concatenation(depth)?);
        }
        Ok(if arms.len() == 1 {
            arms.pop().unwrap_or(Node::Empty)
        } else {
            Node::Alt(arms)
        })
    }

    fn concatenation(&mut self, depth: usize) -> Result<Node, RegexError> {
        let mut items = Vec::new();
        while let Some(ch) = self.peek() {
            if ch == '|' || ch == ')' {
                break;
            }
            items.push(self.repetition(depth)?);
        }
        Ok(match items.len() {
            0 => Node::Empty,
            1 => items.pop().unwrap_or(Node::Empty),
            _ => Node::Concat(items),
        })
    }

    fn repetition(&mut self, depth: usize) -> Result<Node, RegexError> {
        let node = self.atom(depth)?;
        let (min, max) = match self.peek() {
            Some('*') => {
                self.at += 1;
                (0, None)
            }
            Some('+') => {
                self.at += 1;
                (1, None)
            }
            Some('?') => {
                self.at += 1;
                (0, Some(1))
            }
            Some('{') => self.bounds()?,
            _ => return Ok(node),
        };
        // A lazy suffix cannot change a whole-string match. A possessive one
        // could, and a second quantifier is a dangling one: both refuse.
        self.eat('?');
        if matches!(self.peek(), Some('*' | '+' | '?' | '{')) {
            return Err(self.error(RegexErrorKind::Syntax));
        }
        Ok(Node::Repeat {
            node: Box::new(node),
            min,
            max,
        })
    }

    fn number(&mut self) -> Result<u32, RegexError> {
        let start = self.at;
        let mut value: u32 = 0;
        while let Some(digit) = self.peek().and_then(|ch| ch.to_digit(10)) {
            value = value
                .checked_mul(10)
                .and_then(|value| value.checked_add(digit))
                .ok_or_else(|| self.error(RegexErrorKind::Repeat))?;
            self.at += 1;
        }
        if self.at == start {
            return Err(self.error(RegexErrorKind::Syntax));
        }
        Ok(value)
    }

    fn bounds(&mut self) -> Result<(u32, Option<u32>), RegexError> {
        let open = self.at;
        self.at += 1;
        let min = self.number()?;
        let max = if self.eat(',') {
            if self.peek() == Some('}') {
                None
            } else {
                Some(self.number()?)
            }
        } else {
            Some(min)
        };
        if !self.eat('}') {
            return Err(self.error(RegexErrorKind::Syntax));
        }
        if min > MAX_REGEX_REPEAT || max.is_some_and(|max| max > MAX_REGEX_REPEAT || max < min) {
            return Err(RegexError {
                at: open,
                kind: RegexErrorKind::Repeat,
            });
        }
        Ok((min, max))
    }

    fn atom(&mut self, depth: usize) -> Result<Node, RegexError> {
        let Some(ch) = self.peek() else {
            return Err(self.error(RegexErrorKind::Syntax));
        };
        match ch {
            '(' => {
                self.at += 1;
                if self.eat('?') && !self.eat(':') {
                    // Only (?i), and only as the leading flag, is profile 1.
                    return Err(self.error(RegexErrorKind::Syntax));
                }
                let inner = self.alternation(depth + 1)?;
                if !self.eat(')') {
                    return Err(self.error(RegexErrorKind::Syntax));
                }
                Ok(inner)
            }
            '[' => self.class(),
            '.' => {
                self.at += 1;
                Ok(Node::Any)
            }
            '\\' => {
                self.at += 1;
                self.escape().map(|(ranges, negated)| {
                    if !negated && ranges.len() == 1 && ranges[0].0 == ranges[0].1 {
                        Node::Char(ranges[0].0)
                    } else {
                        Node::Class(ranges, negated)
                    }
                })
            }
            '^' if self.at == 0 => {
                self.at += 1;
                Ok(Node::Empty)
            }
            '$' if self.at + 1 == self.chars.len() => {
                self.at += 1;
                Ok(Node::Empty)
            }
            '*' | '+' | '?' | '{' | '}' | ')' | ']' | '^' | '$' => {
                Err(self.error(RegexErrorKind::Syntax))
            }
            literal => {
                self.at += 1;
                Ok(Node::Char(literal))
            }
        }
    }

    /// After a backslash: a class escape, or a literal.
    fn escape(&mut self) -> Result<(Vec<(char, char)>, bool), RegexError> {
        let Some(ch) = self.peek() else {
            return Err(self.error(RegexErrorKind::Syntax));
        };
        self.at += 1;
        Ok(match ch {
            'd' => (DIGIT.to_vec(), false),
            'D' => (DIGIT.to_vec(), true),
            'w' => (WORD.to_vec(), false),
            'W' => (WORD.to_vec(), true),
            's' => (SPACE.to_vec(), false),
            'S' => (SPACE.to_vec(), true),
            't' => (vec![('\t', '\t')], false),
            'n' => (vec![('\n', '\n')], false),
            'r' => (vec![('\r', '\r')], false),
            punctuation if punctuation.is_ascii_punctuation() => {
                (vec![(punctuation, punctuation)], false)
            }
            _ => {
                self.at -= 1;
                return Err(self.error(RegexErrorKind::Syntax));
            }
        })
    }

    fn class(&mut self) -> Result<Node, RegexError> {
        self.at += 1;
        let negated = self.eat('^');
        let mut ranges = Vec::new();
        let mut first = true;
        loop {
            let Some(ch) = self.peek() else {
                return Err(self.error(RegexErrorKind::Syntax));
            };
            if ch == ']' && !first {
                self.at += 1;
                break;
            }
            first = false;
            let low = if ch == '\\' {
                self.at += 1;
                let (escaped, escaped_negated) = self.escape()?;
                if escaped_negated || escaped.len() != 1 || escaped[0].0 != escaped[0].1 {
                    if escaped_negated {
                        // A negated shorthand inside a class needs set
                        // difference, which profile 1 does not have.
                        return Err(self.error(RegexErrorKind::Syntax));
                    }
                    ranges.extend(escaped);
                    continue;
                }
                escaped[0].0
            } else {
                self.at += 1;
                ch
            };
            if self.peek() == Some('-') && self.chars.get(self.at + 1).is_some_and(|&c| c != ']') {
                self.at += 1;
                let high = match self.peek() {
                    Some('\\') => {
                        self.at += 1;
                        let (escaped, escaped_negated) = self.escape()?;
                        if escaped_negated || escaped.len() != 1 || escaped[0].0 != escaped[0].1 {
                            return Err(self.error(RegexErrorKind::Syntax));
                        }
                        escaped[0].0
                    }
                    Some(high) => {
                        self.at += 1;
                        high
                    }
                    None => return Err(self.error(RegexErrorKind::Syntax)),
                };
                if high < low {
                    return Err(self.error(RegexErrorKind::Syntax));
                }
                ranges.push((low, high));
            } else {
                ranges.push((low, low));
            }
        }
        Ok(Node::Class(ranges, negated))
    }
}

struct Compiler {
    program: Vec<Inst>,
}

impl Compiler {
    fn push(&mut self, inst: Inst) -> Result<usize, RegexErrorKind> {
        if self.program.len() >= MAX_REGEX_PROGRAM {
            return Err(RegexErrorKind::TooLarge);
        }
        self.program.push(inst);
        Ok(self.program.len() - 1)
    }

    fn node(&mut self, node: &Node) -> Result<(), RegexErrorKind> {
        match node {
            Node::Empty => {}
            Node::Char(ch) => {
                self.push(Inst::Char(*ch))?;
            }
            Node::Any => {
                self.push(Inst::Any)?;
            }
            Node::Class(ranges, negated) => {
                self.push(Inst::Class {
                    ranges: ranges.clone(),
                    negated: *negated,
                })?;
            }
            Node::Concat(items) => {
                for item in items {
                    self.node(item)?;
                }
            }
            Node::Alt(arms) => {
                // Split(arm, next-split) chains; every arm jumps to the end.
                let mut exits = Vec::new();
                for (index, arm) in arms.iter().enumerate() {
                    if index + 1 < arms.len() {
                        let split = self.push(Inst::Split(0, 0))?;
                        self.node(arm)?;
                        exits.push(self.push(Inst::Jump(0))?);
                        let next = self.program.len();
                        self.program[split] = Inst::Split(split + 1, next);
                    } else {
                        self.node(arm)?;
                    }
                }
                let end = self.program.len();
                for exit in exits {
                    self.program[exit] = Inst::Jump(end);
                }
            }
            Node::Repeat { node, min, max } => {
                for _ in 0..*min {
                    self.node(node)?;
                }
                match max {
                    None => {
                        // L1: Split(L2, L3); L2: node; Jump(L1); L3:
                        let split = self.push(Inst::Split(0, 0))?;
                        self.node(node)?;
                        self.push(Inst::Jump(split))?;
                        let end = self.program.len();
                        self.program[split] = Inst::Split(split + 1, end);
                    }
                    Some(max) => {
                        // Each optional copy may skip straight to the end.
                        let mut skips = Vec::new();
                        for _ in *min..*max {
                            skips.push(self.push(Inst::Split(0, 0))?);
                            self.node(node)?;
                        }
                        let end = self.program.len();
                        for skip in skips {
                            self.program[skip] = Inst::Split(skip + 1, end);
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// A set of program counters with O(1) insert, membership and clear, in
/// insertion order, so every step visits threads deterministically.
struct Threads {
    dense: Vec<usize>,
    member: Vec<bool>,
}

impl Threads {
    fn new(size: usize) -> Self {
        Self {
            dense: Vec::with_capacity(size),
            member: vec![false; size],
        }
    }
    fn clear(&mut self) {
        for &pc in &self.dense {
            self.member[pc] = false;
        }
        self.dense.clear();
    }
}

impl CompiledRegex {
    /// Compile a profile-1 pattern, refusing, typed, anything outside it.
    pub fn compile(source: &str) -> Result<Self, RegexError> {
        let (insensitive, body) = match source.strip_prefix("(?i)") {
            Some(body) => (true, body),
            None => (false, source),
        };
        let offset = source.chars().count() - body.chars().count();
        let mut parser = Parser {
            chars: body.chars().collect(),
            at: 0,
        };
        let shift = |mut error: RegexError| {
            error.at += offset;
            error
        };
        let node = parser.alternation(0).map_err(shift)?;
        if parser.at != parser.chars.len() {
            return Err(shift(parser.error(RegexErrorKind::Syntax)));
        }
        let mut compiler = Compiler {
            program: Vec::new(),
        };
        compiler
            .node(&node)
            .and_then(|()| compiler.push(Inst::Match).map(|_| ()))
            .map_err(|kind| RegexError { at: 0, kind })?;
        Ok(Self {
            source: source.to_owned(),
            insensitive,
            program: compiler.program,
        })
    }

    /// The pattern text, exactly as written.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Compiled instructions; the matcher's work is bounded by text length
    /// times this.
    #[must_use]
    pub fn program_len(&self) -> usize {
        self.program.len()
    }

    fn accepts(&self, inst: &Inst, ch: char) -> bool {
        match inst {
            Inst::Char(expected) => {
                *expected == ch || self.insensitive && fold(*expected) == fold(ch)
            }
            Inst::Any => !is_line_terminator(ch),
            Inst::Class { ranges, negated } => {
                let within = |c: char| ranges.iter().any(|&(low, high)| low <= c && c <= high);
                let hit = within(ch)
                    || self.insensitive && (within(fold(ch)) || ch.to_uppercase().any(within));
                hit != *negated
            }
            Inst::Split(..) | Inst::Jump(_) | Inst::Match => false,
        }
    }

    /// Add `pc` and its epsilon closure (Split/Jump) to `threads`, iteratively.
    fn add(&self, threads: &mut Threads, pc: usize, stack: &mut Vec<usize>) {
        stack.push(pc);
        while let Some(pc) = stack.pop() {
            if threads.member[pc] {
                continue;
            }
            threads.member[pc] = true;
            threads.dense.push(pc);
            match self.program[pc] {
                Inst::Jump(target) => stack.push(target),
                // Push the second branch first so the first is explored first.
                Inst::Split(first, second) => {
                    stack.push(second);
                    stack.push(first);
                }
                _ => {}
            }
        }
    }

    /// Whether the WHOLE of `text` matches.
    #[must_use]
    pub fn is_match(&self, text: &str) -> bool {
        let size = self.program.len();
        let mut current = Threads::new(size);
        let mut next = Threads::new(size);
        let mut stack = Vec::new();
        self.add(&mut current, 0, &mut stack);
        for ch in text.chars() {
            next.clear();
            for index in 0..current.dense.len() {
                let pc = current.dense[index];
                if self.accepts(&self.program[pc], ch) {
                    self.add(&mut next, pc + 1, &mut stack);
                }
            }
            core::mem::swap(&mut current, &mut next);
            if current.dense.is_empty() {
                return false;
            }
        }
        current
            .dense
            .iter()
            .any(|&pc| matches!(self.program[pc], Inst::Match))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(pattern: &str, text: &str) -> bool {
        CompiledRegex::compile(pattern)
            .expect(pattern)
            .is_match(text)
    }

    #[test]
    fn whole_string_matching_over_the_profile_syntax() {
        for (pattern, yes, no) in [
            ("abc", &["abc"][..], &["ab", "abcd", "xabc", ""][..]),
            ("a.c", &["abc", "a-c", "aéc"], &["ac", "a\nc"]),
            ("A.*", &["A", "Ada", "Anything goes"], &["ada", "BA"]),
            ("a|b|cd", &["a", "b", "cd"], &["c", "ab", ""]),
            ("(ab)+", &["ab", "abab"], &["", "aba"]),
            ("(?:ab)?c", &["c", "abc"], &["ac", "ababc"]),
            ("a{2,3}", &["aa", "aaa"], &["a", "aaaa"]),
            ("a{2}", &["aa"], &["a", "aaa"]),
            ("a{2,}", &["aa", "aaaaa"], &["a"]),
            ("[a-c]x", &["ax", "cx"], &["dx", "x"]),
            ("[^0-9]+", &["abc", "é"], &["a1", ""]),
            ("\\d{3}-\\d{4}", &["555-1234"], &["55-1234", "555-12345"]),
            (
                "\\w+@\\w+\\.com",
                &["ada@lab.com"],
                &["ada@lab.org", "@lab.com"],
            ),
            ("\\s*x\\s*", &[" x ", "x", "\tx"], &["y"]),
            ("a\\.b", &["a.b"], &["axb"]),
            ("[\\d_]+", &["1_2"], &["1-2"]),
            ("^ab$", &["ab"], &["abc"]),
            ("", &[""], &["a"]),
            ("a*?b", &["b", "aab"], &["a"]),
            ("[]a]", &["]", "a"], &["b"]),
            ("[a-]", &["a", "-"], &["b"]),
        ] {
            for text in yes {
                assert!(matches(pattern, text), "{pattern} should match {text:?}");
            }
            for text in no {
                assert!(
                    !matches(pattern, text),
                    "{pattern} should not match {text:?}"
                );
            }
        }
    }

    #[test]
    fn case_insensitive_flag_folds_simple_unicode_case() {
        assert!(matches("(?i)ada.*", "ADA Lovelace"));
        assert!(matches("(?i)[a-c]+", "ABCabc"));
        assert!(matches("(?i)éa", "ÉA"));
        assert!(!matches("ada", "ADA"));
        // Only a LEADING (?i) is the flag; elsewhere it refuses.
        assert!(CompiledRegex::compile("a(?i)b").is_err());
    }

    #[test]
    fn unsupported_or_malformed_patterns_refuse_typed() {
        for (pattern, kind) in [
            ("(ab", RegexErrorKind::Syntax),
            ("ab)", RegexErrorKind::Syntax),
            ("*a", RegexErrorKind::Syntax),
            ("a**+", RegexErrorKind::Syntax),
            ("a++", RegexErrorKind::Syntax),
            ("[abc", RegexErrorKind::Syntax),
            ("[z-a]", RegexErrorKind::Syntax),
            ("\\bword", RegexErrorKind::Syntax),
            ("(?=a)", RegexErrorKind::Syntax),
            ("(a)\\1", RegexErrorKind::Syntax),
            ("a^b", RegexErrorKind::Syntax),
            ("a$b", RegexErrorKind::Syntax),
            ("[\\D]", RegexErrorKind::Syntax),
            ("a{3,2}", RegexErrorKind::Repeat),
            ("a{257}", RegexErrorKind::Repeat),
            ("(a{200}){200}", RegexErrorKind::TooLarge),
        ] {
            assert_eq!(
                CompiledRegex::compile(pattern)
                    .map(|_| ())
                    .unwrap_err()
                    .kind,
                kind,
                "{pattern}"
            );
        }
    }

    #[test]
    fn nested_quantifiers_stay_linear() {
        // A backtracking engine explores exponentially many splits here.
        let regex = CompiledRegex::compile("(a*)*b").unwrap();
        let text = "a".repeat(20_000) + "c";
        assert!(!regex.is_match(&text));
        assert!(regex.is_match(&("a".repeat(20_000) + "b")));
        let alternation = CompiledRegex::compile("(a|a)*").unwrap();
        assert!(alternation.is_match(&"a".repeat(20_000)));
    }

    /// An independent, deliberately naive oracle: it tries every split of the
    /// text against the syntax tree. It is exponential, so it runs only on
    /// tiny inputs.
    fn oracle(node: &Node, text: &[char], insensitive: bool) -> bool {
        match node {
            Node::Empty => text.is_empty(),
            Node::Char(expected) => {
                text.len() == 1
                    && (text[0] == *expected || insensitive && fold(text[0]) == fold(*expected))
            }
            Node::Any => text.len() == 1 && !is_line_terminator(text[0]),
            Node::Class(ranges, negated) => {
                let within = |c: char| ranges.iter().any(|&(l, h)| l <= c && c <= h);
                text.len() == 1
                    && (within(text[0])
                        || insensitive
                            && (within(fold(text[0])) || text[0].to_uppercase().any(within)))
                        != *negated
            }
            Node::Concat(items) => match items.split_first() {
                None => text.is_empty(),
                Some((head, rest)) => (0..=text.len()).any(|cut| {
                    oracle(head, &text[..cut], insensitive)
                        && oracle(&Node::Concat(rest.to_vec()), &text[cut..], insensitive)
                }),
            },
            Node::Alt(arms) => arms.iter().any(|arm| oracle(arm, text, insensitive)),
            Node::Repeat { node, min, max } => {
                fn count(node: &Node, text: &[char], times: u32, insensitive: bool) -> bool {
                    if times == 0 {
                        return text.is_empty();
                    }
                    (0..=text.len()).any(|cut| {
                        oracle(node, &text[..cut], insensitive)
                            && count(node, &text[cut..], times - 1, insensitive)
                    })
                }
                let upper = max.unwrap_or(text.len() as u32 + *min);
                (*min..=upper).any(|times| count(node, text, times, insensitive))
            }
        }
    }

    #[test]
    fn the_pike_vm_agrees_with_a_naive_oracle_on_every_small_input() {
        let patterns = [
            "a*b*",
            "(a|b)*a",
            "(ab|a)(b|)",
            "a?b+a?",
            "(a*)*b",
            "[ab]{1,2}b",
            "(a|ab)(c|bcd)?",
            "((a|b)(a|b))*",
            "a{0,2}b{1,}",
            "[^a]*a",
            "(?i)A(b|B)*",
            ".a.",
            "(a?){2}a{2}",
        ];
        let alphabet = ['a', 'b', 'c', 'A'];
        for pattern in patterns {
            let compiled = CompiledRegex::compile(pattern).unwrap();
            let (insensitive, body) = pattern
                .strip_prefix("(?i)")
                .map_or((false, pattern), |body| (true, body));
            let mut parser = Parser {
                chars: body.chars().collect(),
                at: 0,
            };
            let tree = parser.alternation(0).unwrap();
            // Every string over the alphabet up to length 5.
            let mut texts: Vec<Vec<char>> = vec![Vec::new()];
            let mut frontier = vec![Vec::new()];
            for _ in 0..5 {
                let mut grown = Vec::new();
                for text in &frontier {
                    for &ch in &alphabet {
                        let mut longer: Vec<char> = text.clone();
                        longer.push(ch);
                        grown.push(longer);
                    }
                }
                texts.extend(grown.iter().cloned());
                frontier = grown;
            }
            for text in texts {
                let string: String = text.iter().collect();
                assert_eq!(
                    compiled.is_match(&string),
                    oracle(&tree, &text, insensitive),
                    "{pattern} on {string:?}"
                );
            }
        }
    }
}
