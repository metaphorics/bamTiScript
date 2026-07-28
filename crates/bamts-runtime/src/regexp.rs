use std::collections::BTreeMap;
use std::ops::Range;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Flags {
    pub(crate) global: bool,
    pub(crate) ignore_case: bool,
    pub(crate) multiline: bool,
    pub(crate) dot_all: bool,
    pub(crate) unicode: bool,
    pub(crate) sticky: bool,
}

impl Flags {
    fn parse(text: &str) -> Result<Self, RegexError> {
        let mut flags = Self::default();
        for flag in text.chars() {
            let slot = match flag {
                'g' => &mut flags.global,
                'i' => &mut flags.ignore_case,
                'm' => &mut flags.multiline,
                's' => &mut flags.dot_all,
                'u' => &mut flags.unicode,
                'y' => &mut flags.sticky,
                _ => {
                    return Err(RegexError::new(format!(
                        "invalid regular expression flag '{flag}'"
                    )));
                }
            };
            if *slot {
                return Err(RegexError::new(format!(
                    "duplicate regular expression flag '{flag}'"
                )));
            }
            *slot = true;
        }
        Ok(flags)
    }

    pub(crate) fn canonical(self) -> String {
        let mut result = String::new();
        for (enabled, flag) in [
            (self.global, 'g'),
            (self.ignore_case, 'i'),
            (self.multiline, 'm'),
            (self.dot_all, 's'),
            (self.unicode, 'u'),
            (self.sticky, 'y'),
        ] {
            if enabled {
                result.push(flag);
            }
        }
        result
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RegexError {
    message: String,
}

impl RegexError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Match {
    pub(crate) range: Range<usize>,
    pub(crate) captures: Vec<Option<Range<usize>>>,
    pub(crate) named: BTreeMap<String, Option<Range<usize>>>,
}

#[derive(Clone, Debug)]
pub(crate) struct Regex {
    expression: Node,
    flags: Flags,
    capture_count: usize,
    names: BTreeMap<String, usize>,
}

impl Regex {
    pub(crate) fn compile(pattern: &str, flags: &str) -> Result<Self, RegexError> {
        let flags = Flags::parse(flags)?;
        let mut parser = Parser::new(pattern);
        let expression = parser.parse_disjunction(None)?;
        if parser.peek().is_some() {
            return Err(parser.error("unexpected token"));
        }
        Ok(Self {
            expression,
            flags,
            capture_count: parser.capture_count,
            names: parser.names,
        })
    }

    pub(crate) fn flags(&self) -> Flags {
        self.flags
    }

    pub(crate) fn exec(&self, input: &str, start: usize) -> Option<Match> {
        let chars: Vec<char> = input.chars().collect();
        let start = start.min(chars.len());
        let positions: Box<dyn Iterator<Item = usize>> = if self.flags.sticky {
            Box::new(std::iter::once(start))
        } else {
            Box::new(start..=chars.len())
        };
        for position in positions {
            let state = State {
                position,
                captures: vec![None; self.capture_count + 1],
            };
            if let Some(mut matched) = self
                .match_node(&self.expression, &chars, state)
                .into_iter()
                .next()
            {
                matched.captures[0] = Some(position..matched.position);
                let named = self
                    .names
                    .iter()
                    .map(|(name, index)| (name.clone(), matched.captures[*index].clone()))
                    .collect();
                return Some(Match {
                    range: position..matched.position,
                    captures: matched.captures,
                    named,
                });
            }
        }
        None
    }

    fn match_node(&self, node: &Node, input: &[char], state: State) -> Vec<State> {
        match node {
            Node::Sequence(nodes) => self.match_sequence(nodes, input, state),
            Node::Alternation(branches) => branches
                .iter()
                .flat_map(|branch| self.match_node(branch, input, state.clone()))
                .collect(),
            Node::Literal(expected) => input
                .get(state.position)
                .filter(|actual| self.char_eq(**actual, *expected))
                .map_or_else(Vec::new, |_| vec![state.advanced(1)]),
            Node::Dot => input
                .get(state.position)
                .filter(|actual| {
                    self.flags.dot_all || !matches!(actual, '\n' | '\r' | '\u{2028}' | '\u{2029}')
                })
                .map_or_else(Vec::new, |_| vec![state.advanced(1)]),
            Node::Class(class) => input
                .get(state.position)
                .filter(|actual| class.matches(**actual, self.flags.ignore_case))
                .map_or_else(Vec::new, |_| vec![state.advanced(1)]),
            Node::Start => {
                let at_start = state.position == 0
                    || (self.flags.multiline
                        && state.position > 0
                        && is_line_terminator(input[state.position - 1]));
                at_start.then_some(state).into_iter().collect()
            }
            Node::End => {
                let at_end = state.position == input.len()
                    || (self.flags.multiline
                        && input
                            .get(state.position)
                            .is_some_and(|value| is_line_terminator(*value)));
                at_end.then_some(state).into_iter().collect()
            }
            Node::WordBoundary(positive) => {
                let left = state
                    .position
                    .checked_sub(1)
                    .and_then(|index| input.get(index))
                    .is_some_and(|c| is_word(*c));
                let right = input.get(state.position).is_some_and(|c| is_word(*c));
                ((left != right) == *positive)
                    .then_some(state)
                    .into_iter()
                    .collect()
            }
            Node::Group { index, body } => {
                let begin = state.position;
                self.match_node(body, input, state)
                    .into_iter()
                    .map(|mut matched| {
                        if let Some(index) = index {
                            matched.captures[*index] = Some(begin..matched.position);
                        }
                        matched
                    })
                    .collect()
            }
            Node::BackReference(index) => {
                let Some(range) = state.captures.get(*index).and_then(Clone::clone) else {
                    return vec![state];
                };
                let count = range.end - range.start;
                if state.position + count > input.len() {
                    return Vec::new();
                }
                let matches = input[range]
                    .iter()
                    .zip(&input[state.position..state.position + count])
                    .all(|(left, right)| self.char_eq(*left, *right));
                matches.then(|| state.advanced(count)).into_iter().collect()
            }
            Node::NamedBackReference(name) => self.names.get(name).map_or_else(Vec::new, |index| {
                self.match_node(&Node::BackReference(*index), input, state)
            }),
            Node::Look {
                body,
                behind,
                positive,
            } => {
                let candidates = if *behind {
                    (0..=state.position)
                        .flat_map(|begin| {
                            let mut initial = state.clone();
                            initial.position = begin;
                            self.match_node(body, input, initial)
                        })
                        .filter(|matched| matched.position == state.position)
                        .collect::<Vec<_>>()
                } else {
                    self.match_node(body, input, state.clone())
                };
                if *positive {
                    candidates
                        .into_iter()
                        .map(|mut matched| {
                            matched.position = state.position;
                            matched
                        })
                        .collect()
                } else if candidates.is_empty() {
                    vec![state]
                } else {
                    Vec::new()
                }
            }
            Node::Repeat { .. } => unreachable!("repeat is handled by its containing sequence"),
        }
    }

    fn match_sequence(&self, nodes: &[Node], input: &[char], state: State) -> Vec<State> {
        let Some((first, rest)) = nodes.split_first() else {
            return vec![state];
        };
        if let Node::Repeat {
            body,
            min,
            max,
            greedy,
        } = first
        {
            let mut levels = vec![vec![state]];
            let limit = max.unwrap_or(input.len().saturating_add(*min).saturating_add(1));
            for count in 0..limit {
                let mut next = Vec::new();
                for prior in &levels[count] {
                    for matched in self.match_node(body, input, prior.clone()) {
                        if matched.position == prior.position && count + 1 >= *min {
                            continue;
                        }
                        next.push(matched);
                    }
                }
                if next.is_empty() {
                    break;
                }
                levels.push(next);
            }
            let counts: Box<dyn Iterator<Item = usize>> = if *greedy {
                Box::new((*min..levels.len()).rev())
            } else {
                Box::new(*min..levels.len())
            };
            let mut result = Vec::new();
            for count in counts {
                for candidate in levels[count].clone() {
                    result.extend(self.match_sequence(rest, input, candidate));
                }
            }
            result
        } else {
            self.match_node(first, input, state)
                .into_iter()
                .flat_map(|matched| self.match_sequence(rest, input, matched))
                .collect()
        }
    }

    fn char_eq(&self, left: char, right: char) -> bool {
        left == right || (self.flags.ignore_case && fold(left) == fold(right))
    }
}

#[derive(Clone, Debug)]
struct State {
    position: usize,
    captures: Vec<Option<Range<usize>>>,
}

impl State {
    fn advanced(mut self, count: usize) -> Self {
        self.position += count;
        self
    }
}

#[derive(Clone, Debug)]
enum Node {
    Sequence(Vec<Node>),
    Alternation(Vec<Node>),
    Literal(char),
    Dot,
    Class(CharacterClass),
    Start,
    End,
    WordBoundary(bool),
    Group {
        index: Option<usize>,
        body: Box<Node>,
    },
    BackReference(usize),
    NamedBackReference(String),
    Look {
        body: Box<Node>,
        behind: bool,
        positive: bool,
    },
    Repeat {
        body: Box<Node>,
        min: usize,
        max: Option<usize>,
        greedy: bool,
    },
}

#[derive(Clone, Debug)]
struct CharacterClass {
    negated: bool,
    items: Vec<ClassItem>,
}

impl CharacterClass {
    fn matches(&self, value: char, ignore_case: bool) -> bool {
        let matched = self
            .items
            .iter()
            .any(|item| item.matches(value, ignore_case));
        matched != self.negated
    }
}

#[derive(Clone, Debug)]
enum ClassItem {
    Character(char),
    Range(char, char),
    Digit,
    NotDigit,
    Word,
    NotWord,
    Space,
    NotSpace,
}

impl ClassItem {
    fn matches(&self, value: char, ignore_case: bool) -> bool {
        match self {
            Self::Character(expected) => {
                value == *expected || (ignore_case && fold(value) == fold(*expected))
            }
            Self::Range(start, end) => {
                let candidate = if ignore_case { fold(value) } else { value };
                let start = if ignore_case { fold(*start) } else { *start };
                let end = if ignore_case { fold(*end) } else { *end };
                (start..=end).contains(&candidate)
            }
            Self::Digit => value.is_ascii_digit(),
            Self::NotDigit => !value.is_ascii_digit(),
            Self::Word => is_word(value),
            Self::NotWord => !is_word(value),
            Self::Space => value.is_whitespace() || value == '\u{feff}',
            Self::NotSpace => !(value.is_whitespace() || value == '\u{feff}'),
        }
    }
}

fn fold(value: char) -> char {
    value.to_lowercase().next().unwrap_or(value)
}

fn is_word(value: char) -> bool {
    value.is_ascii_alphanumeric() || value == '_'
}

fn is_line_terminator(value: char) -> bool {
    matches!(value, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

struct Parser {
    chars: Vec<char>,
    position: usize,
    capture_count: usize,
    names: BTreeMap<String, usize>,
}

impl Parser {
    fn new(pattern: &str) -> Self {
        Self {
            chars: pattern.chars().collect(),
            position: 0,
            capture_count: 0,
            names: BTreeMap::new(),
        }
    }

    fn parse_disjunction(&mut self, terminator: Option<char>) -> Result<Node, RegexError> {
        let mut branches = Vec::new();
        loop {
            branches.push(Node::Sequence(self.parse_sequence(terminator)?));
            if self.peek() != Some('|') {
                break;
            }
            self.position += 1;
        }
        if branches.len() == 1 {
            Ok(branches.pop().expect("one branch"))
        } else {
            Ok(Node::Alternation(branches))
        }
    }

    fn parse_sequence(&mut self, terminator: Option<char>) -> Result<Vec<Node>, RegexError> {
        let mut nodes = Vec::new();
        while let Some(token) = self.peek() {
            if Some(token) == terminator || token == '|' {
                break;
            }
            let atom = self.parse_atom()?;
            nodes.push(self.parse_quantifier(atom)?);
        }
        Ok(nodes)
    }

    fn parse_atom(&mut self) -> Result<Node, RegexError> {
        let token = self
            .next()
            .ok_or_else(|| self.error("expected regular expression atom"))?;
        match token {
            '.' => Ok(Node::Dot),
            '^' => Ok(Node::Start),
            '$' => Ok(Node::End),
            '[' => self.parse_class().map(Node::Class),
            '(' => self.parse_group(),
            '\\' => self.parse_escape(false),
            ')' => Err(self.error("unmatched ')'")),
            '*' | '+' | '?' => Err(self.error("nothing to repeat")),
            value => Ok(Node::Literal(value)),
        }
    }

    fn parse_group(&mut self) -> Result<Node, RegexError> {
        let mut index = None;
        let mut look = None;
        if self.peek() == Some('?') {
            self.position += 1;
            match self.next() {
                Some(':') => {}
                Some('=') => look = Some((false, true)),
                Some('!') => look = Some((false, false)),
                Some('<') => match self.peek() {
                    Some('=') => {
                        self.position += 1;
                        look = Some((true, true));
                    }
                    Some('!') => {
                        self.position += 1;
                        look = Some((true, false));
                    }
                    _ => {
                        let name = self.take_until('>')?;
                        if name.is_empty() || self.names.contains_key(&name) {
                            return Err(self.error("invalid duplicate capture group name"));
                        }
                        self.capture_count += 1;
                        index = Some(self.capture_count);
                        self.names.insert(name, self.capture_count);
                    }
                },
                _ => return Err(self.error("invalid group")),
            }
        } else {
            self.capture_count += 1;
            index = Some(self.capture_count);
        }
        let body = self.parse_disjunction(Some(')'))?;
        if self.next() != Some(')') {
            return Err(self.error("unterminated group"));
        }
        Ok(if let Some((behind, positive)) = look {
            Node::Look {
                body: Box::new(body),
                behind,
                positive,
            }
        } else {
            Node::Group {
                index,
                body: Box::new(body),
            }
        })
    }

    fn parse_class(&mut self) -> Result<CharacterClass, RegexError> {
        let negated = if self.peek() == Some('^') {
            self.position += 1;
            true
        } else {
            false
        };
        let mut items = Vec::new();
        let mut first = true;
        while let Some(token) = self.peek() {
            if token == ']' && !first {
                self.position += 1;
                return Ok(CharacterClass { negated, items });
            }
            first = false;
            let left = self.parse_class_item()?;
            if self.peek() == Some('-') && self.chars.get(self.position + 1) != Some(&']') {
                self.position += 1;
                let right = self.parse_class_item()?;
                match (left, right) {
                    (ClassItem::Character(start), ClassItem::Character(end)) if start <= end => {
                        items.push(ClassItem::Range(start, end))
                    }
                    _ => return Err(self.error("invalid character class range")),
                }
            } else {
                items.push(left);
            }
        }
        Err(self.error("unterminated character class"))
    }

    fn parse_class_item(&mut self) -> Result<ClassItem, RegexError> {
        let token = self
            .next()
            .ok_or_else(|| self.error("unterminated character class"))?;
        if token != '\\' {
            return Ok(ClassItem::Character(token));
        }
        let escaped = self.next().ok_or_else(|| self.error("trailing escape"))?;
        match escaped {
            'd' => Ok(ClassItem::Digit),
            'D' => Ok(ClassItem::NotDigit),
            'w' => Ok(ClassItem::Word),
            'W' => Ok(ClassItem::NotWord),
            's' => Ok(ClassItem::Space),
            'S' => Ok(ClassItem::NotSpace),
            _ => self.escape_character(escaped).map(ClassItem::Character),
        }
    }

    fn parse_escape(&mut self, _in_class: bool) -> Result<Node, RegexError> {
        let escaped = self.next().ok_or_else(|| self.error("trailing escape"))?;
        match escaped {
            'd' => Ok(Node::Class(CharacterClass {
                negated: false,
                items: vec![ClassItem::Digit],
            })),
            'D' => Ok(Node::Class(CharacterClass {
                negated: false,
                items: vec![ClassItem::NotDigit],
            })),
            'w' => Ok(Node::Class(CharacterClass {
                negated: false,
                items: vec![ClassItem::Word],
            })),
            'W' => Ok(Node::Class(CharacterClass {
                negated: false,
                items: vec![ClassItem::NotWord],
            })),
            's' => Ok(Node::Class(CharacterClass {
                negated: false,
                items: vec![ClassItem::Space],
            })),
            'S' => Ok(Node::Class(CharacterClass {
                negated: false,
                items: vec![ClassItem::NotSpace],
            })),
            'b' => Ok(Node::WordBoundary(true)),
            'B' => Ok(Node::WordBoundary(false)),
            'k' if self.next() == Some('<') => Ok(Node::NamedBackReference(self.take_until('>')?)),
            value if value.is_ascii_digit() && value != '0' => {
                let mut number = value.to_digit(10).expect("digit") as usize;
                while let Some(digit) = self.peek().and_then(|next| next.to_digit(10)) {
                    self.position += 1;
                    number = number * 10 + digit as usize;
                }
                Ok(Node::BackReference(number))
            }
            value => self.escape_character(value).map(Node::Literal),
        }
    }

    fn escape_character(&mut self, escaped: char) -> Result<char, RegexError> {
        match escaped {
            'n' => Ok('\n'),
            'r' => Ok('\r'),
            't' => Ok('\t'),
            'f' => Ok('\u{c}'),
            'v' => Ok('\u{b}'),
            '0' => Ok('\0'),
            'x' => self.hex_escape(2),
            'u' if self.peek() == Some('{') => {
                self.position += 1;
                let digits = self.take_until('}')?;
                u32::from_str_radix(&digits, 16)
                    .ok()
                    .and_then(char::from_u32)
                    .ok_or_else(|| self.error("invalid Unicode escape"))
            }
            'u' => self.hex_escape(4),
            value => Ok(value),
        }
    }

    fn hex_escape(&mut self, count: usize) -> Result<char, RegexError> {
        if self.position + count > self.chars.len() {
            return Err(self.error("invalid hexadecimal escape"));
        }
        let digits: String = self.chars[self.position..self.position + count]
            .iter()
            .collect();
        self.position += count;
        u32::from_str_radix(&digits, 16)
            .ok()
            .and_then(char::from_u32)
            .ok_or_else(|| self.error("invalid hexadecimal escape"))
    }

    fn parse_quantifier(&mut self, atom: Node) -> Result<Node, RegexError> {
        let Some(token) = self.peek() else {
            return Ok(atom);
        };
        let (min, max) = match token {
            '*' => {
                self.position += 1;
                (0, None)
            }
            '+' => {
                self.position += 1;
                (1, None)
            }
            '?' => {
                self.position += 1;
                (0, Some(1))
            }
            '{' => {
                let checkpoint = self.position;
                self.position += 1;
                let Some(minimum) = self.parse_decimal() else {
                    self.position = checkpoint;
                    return Ok(atom);
                };
                match self.next() {
                    Some('}') => (minimum, Some(minimum)),
                    Some(',') => {
                        let maximum = self.parse_decimal();
                        if self.next() != Some('}') {
                            return Err(self.error("invalid quantifier"));
                        }
                        if maximum.is_some_and(|value| value < minimum) {
                            return Err(self.error("quantifier range out of order"));
                        }
                        (minimum, maximum)
                    }
                    _ => return Err(self.error("invalid quantifier")),
                }
            }
            _ => return Ok(atom),
        };
        let greedy = if self.peek() == Some('?') {
            self.position += 1;
            false
        } else {
            true
        };
        Ok(Node::Repeat {
            body: Box::new(atom),
            min,
            max,
            greedy,
        })
    }

    fn parse_decimal(&mut self) -> Option<usize> {
        let begin = self.position;
        let mut value = 0usize;
        while let Some(digit) = self.peek().and_then(|token| token.to_digit(10)) {
            self.position += 1;
            value = value.checked_mul(10)?.checked_add(digit as usize)?;
        }
        (self.position > begin).then_some(value)
    }

    fn take_until(&mut self, terminator: char) -> Result<String, RegexError> {
        let begin = self.position;
        while self.peek().is_some_and(|token| token != terminator) {
            self.position += 1;
        }
        if self.next() != Some(terminator) {
            return Err(self.error(format!("expected '{terminator}'")));
        }
        Ok(self.chars[begin..self.position - 1].iter().collect())
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.position).copied()
    }
    fn next(&mut self) -> Option<char> {
        let value = self.peek()?;
        self.position += 1;
        Some(value)
    }
    fn error(&self, message: impl Into<String>) -> RegexError {
        RegexError::new(format!("{} at position {}", message.into(), self.position))
    }
}

#[cfg(test)]
mod tests {
    use super::Regex;

    fn ranges(pattern: &str, flags: &str, input: &str) -> Vec<Option<std::ops::Range<usize>>> {
        Regex::compile(pattern, flags)
            .unwrap()
            .exec(input, 0)
            .unwrap()
            .captures
    }

    #[test]
    fn corpus_escape_patterns_match_byte_exact_node_results() {
        let escaped =
            Regex::compile(r"\\ \^ \$ \* \+ \? \. \( \) \| \{ \} \[ \] \x2d", "").unwrap();
        assert_eq!(
            escaped
                .exec(r"\ ^ $ * + ? . ( ) | { } [ ] -", 0)
                .unwrap()
                .range,
            0..29
        );
        assert!(Regex::compile(r"\x2d", "u").unwrap().exec("-", 0).is_some());
        assert_eq!(
            Regex::compile(r"\\", "g")
                .unwrap()
                .exec(r"a\b", 0)
                .unwrap()
                .range,
            1..2
        );
    }

    #[test]
    fn supports_corpus_glob_shapes() {
        assert!(
            Regex::compile(r"^(?:[^/]*?)\.js$", "")
                .unwrap()
                .exec("a.js", 0)
                .is_some()
        );
        assert!(
            Regex::compile(r"^(?:.*?/)?.*?\.ts$", "")
                .unwrap()
                .exec("src/a/b.ts", 0)
                .is_some()
        );
        assert!(
            Regex::compile(r"^[abc]at$", "")
                .unwrap()
                .exec("cat", 0)
                .is_some()
        );
        assert!(
            Regex::compile(r"^(a|b)\.js$", "")
                .unwrap()
                .exec("b.js", 0)
                .is_some()
        );
    }

    #[test]
    fn captures_backreferences_and_lookarounds() {
        assert_eq!(
            ranges(r"(?<word>[a-z]+)-\k<word>", "i", "Ab-ab")[1],
            Some(0..2)
        );
        assert!(
            Regex::compile(r"(?<=foo)bar(?=$)", "")
                .unwrap()
                .exec("foobar", 0)
                .is_some()
        );
        assert!(
            Regex::compile(r"foo(?!bar)", "")
                .unwrap()
                .exec("foobaz", 0)
                .is_some()
        );
    }

    #[test]
    fn lazy_and_sticky_matching() {
        assert_eq!(
            Regex::compile(r"a.*?b", "s")
                .unwrap()
                .exec("a1b2b", 0)
                .unwrap()
                .range,
            0..3
        );
        let sticky = Regex::compile("b", "y").unwrap();
        assert!(sticky.exec("ab", 0).is_none());
        assert_eq!(sticky.exec("ab", 1).unwrap().range, 1..2);
    }

    #[test]
    fn flags_are_canonical_like_node_24() {
        assert_eq!(
            Regex::compile("", "yusmig").unwrap().flags().canonical(),
            "gimsuy"
        );
        assert!(Regex::compile("", "gg").is_err());
    }
}
