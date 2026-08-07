use std::collections::BTreeMap;
use std::ops::Range;

use bamts_bytecode::EcmaString;

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
    fn parse(text: &EcmaString) -> Result<Self, RegexError> {
        let mut flags = Self::default();
        for unit in text.as_units() {
            let Some(flag) = char::from_u32(u32::from(*unit)).filter(char::is_ascii) else {
                return Err(RegexError::new("invalid regular expression flag"));
            };
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

    pub(crate) fn canonical(self) -> EcmaString {
        let mut result = bamts_bytecode::EcmaStringBuilder::new();
        for (enabled, flag) in [
            (self.global, b'g'),
            (self.ignore_case, b'i'),
            (self.multiline, b'm'),
            (self.dot_all, b's'),
            (self.unicode, b'u'),
            (self.sticky, b'y'),
        ] {
            if enabled {
                result.push_unit(u16::from(flag));
            }
        }
        result.finish()
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
    pub(crate) fn compile(pattern: &EcmaString, flags: &EcmaString) -> Result<Self, RegexError> {
        let flags = Flags::parse(flags)?;
        let mut parser = Parser::new(pattern.as_units(), flags.unicode);
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

    pub(crate) fn exec(&self, input: &EcmaString, start: usize) -> Option<Match> {
        let input = input.as_units();
        if start > input.len() {
            return None;
        }
        let mut position = start;
        loop {
            let state = State {
                position,
                captures: vec![None; self.capture_count + 1],
            };
            if let Some(mut matched) = self
                .match_node(&self.expression, input, state)
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
            if self.flags.sticky || position == input.len() {
                return None;
            }
            position += next_code_point(input, position, self.flags.unicode).1;
        }
    }

    fn match_node(&self, node: &Node, input: &[u16], state: State) -> Vec<State> {
        match node {
            Node::Sequence(nodes) => self.match_sequence(nodes, input, state),
            Node::Alternation(branches) => branches
                .iter()
                .flat_map(|branch| self.match_node(branch, input, state.clone()))
                .collect(),
            Node::Literal(expected) => {
                if state.position == input.len() {
                    return Vec::new();
                }
                let (actual, width) = next_code_point(input, state.position, self.flags.unicode);
                self.code_point_eq(actual, *expected)
                    .then(|| state.advanced(width))
                    .into_iter()
                    .collect()
            }
            Node::Dot => {
                if state.position == input.len() {
                    return Vec::new();
                }
                let (actual, width) = next_code_point(input, state.position, self.flags.unicode);
                (self.flags.dot_all || !is_line_terminator(actual))
                    .then(|| state.advanced(width))
                    .into_iter()
                    .collect()
            }
            Node::Class(class) => {
                if state.position == input.len() {
                    return Vec::new();
                }
                let (actual, width) = next_code_point(input, state.position, self.flags.unicode);
                class
                    .matches(actual, self.flags.ignore_case)
                    .then(|| state.advanced(width))
                    .into_iter()
                    .collect()
            }
            Node::Start => {
                let at_start = state.position == 0
                    || (self.flags.multiline
                        && state.position > 0
                        && is_line_terminator(u32::from(input[state.position - 1])));
                at_start.then_some(state).into_iter().collect()
            }
            Node::End => {
                let at_end = state.position == input.len()
                    || (self.flags.multiline
                        && input
                            .get(state.position)
                            .is_some_and(|value| is_line_terminator(u32::from(*value))));
                at_end.then_some(state).into_iter().collect()
            }
            Node::WordBoundary(positive) => {
                let left = state
                    .position
                    .checked_sub(1)
                    .and_then(|index| input.get(index))
                    .is_some_and(|unit| is_word(u32::from(*unit)));
                let right = input
                    .get(state.position)
                    .is_some_and(|unit| is_word(u32::from(*unit)));
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
                self.match_backreference(input, range, state)
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
                    let mut candidates = Vec::new();
                    let mut begin = 0;
                    loop {
                        let mut initial = state.clone();
                        initial.position = begin;
                        candidates.extend(
                            self.match_node(body, input, initial)
                                .into_iter()
                                .filter(|matched| matched.position == state.position),
                        );
                        if begin == state.position {
                            break;
                        }
                        begin += next_code_point(input, begin, self.flags.unicode).1;
                        if begin > state.position {
                            break;
                        }
                    }
                    candidates
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

    fn match_backreference(&self, input: &[u16], range: Range<usize>, state: State) -> Vec<State> {
        let mut captured = range.start;
        let mut candidate = state.position;
        while captured < range.end {
            if candidate >= input.len() {
                return Vec::new();
            }
            let (left, left_width) = next_code_point(input, captured, self.flags.unicode);
            let (right, right_width) = next_code_point(input, candidate, self.flags.unicode);
            if captured + left_width > range.end || !self.code_point_eq(left, right) {
                return Vec::new();
            }
            captured += left_width;
            candidate += right_width;
        }
        let width = candidate - state.position;
        vec![state.advanced(width)]
    }

    fn match_sequence(&self, nodes: &[Node], input: &[u16], state: State) -> Vec<State> {
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

    fn code_point_eq(&self, left: u32, right: u32) -> bool {
        left == right || (self.flags.ignore_case && fold(left) == fold(right))
    }
}

pub(crate) fn next_code_point(input: &[u16], position: usize, unicode: bool) -> (u32, usize) {
    let first = input[position];
    if unicode
        && (0xd800..=0xdbff).contains(&first)
        && let Some(second @ 0xdc00..=0xdfff) = input.get(position + 1).copied()
    {
        return (
            0x1_0000 + ((u32::from(first) - 0xd800) << 10) + (u32::from(second) - 0xdc00),
            2,
        );
    }
    (u32::from(first), 1)
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
    Literal(u32),
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
    fn matches(&self, value: u32, ignore_case: bool) -> bool {
        let matched = self
            .items
            .iter()
            .any(|item| item.matches(value, ignore_case));
        matched != self.negated
    }
}

#[derive(Clone, Debug)]
enum ClassItem {
    Character(u32),
    Range(u32, u32),
    Digit,
    NotDigit,
    Word,
    NotWord,
    Space,
    NotSpace,
}

impl ClassItem {
    fn matches(&self, value: u32, ignore_case: bool) -> bool {
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
            Self::Digit => value <= 0x7f && (value as u8).is_ascii_digit(),
            Self::NotDigit => !(value <= 0x7f && (value as u8).is_ascii_digit()),
            Self::Word => is_word(value),
            Self::NotWord => !is_word(value),
            Self::Space => {
                char::from_u32(value).is_some_and(char::is_whitespace) || value == 0xfeff
            }
            Self::NotSpace => {
                !(char::from_u32(value).is_some_and(char::is_whitespace) || value == 0xfeff)
            }
        }
    }
}

fn fold(value: u32) -> u32 {
    char::from_u32(value)
        .and_then(|value| value.to_lowercase().next())
        .map_or(value, u32::from)
}

fn is_word(value: u32) -> bool {
    value <= 0x7f && ((value as u8).is_ascii_alphanumeric() || value == u32::from(b'_'))
}

fn is_line_terminator(value: u32) -> bool {
    matches!(value, 0x0a | 0x0d | 0x2028 | 0x2029)
}

struct Parser<'a> {
    units: &'a [u16],
    position: usize,
    capture_count: usize,
    names: BTreeMap<String, usize>,
    unicode: bool,
}

impl<'a> Parser<'a> {
    fn new(units: &'a [u16], unicode: bool) -> Self {
        Self {
            units,
            position: 0,
            capture_count: 0,
            names: BTreeMap::new(),
            unicode,
        }
    }

    fn parse_disjunction(&mut self, terminator: Option<u16>) -> Result<Node, RegexError> {
        let mut branches = Vec::new();
        loop {
            branches.push(Node::Sequence(self.parse_sequence(terminator)?));
            if self.peek() != Some(u16::from(b'|')) {
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

    fn parse_sequence(&mut self, terminator: Option<u16>) -> Result<Vec<Node>, RegexError> {
        let mut nodes = Vec::new();
        while let Some(token) = self.peek() {
            if Some(token) == terminator || token == u16::from(b'|') {
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
            0x2e => Ok(Node::Dot),
            0x5e => Ok(Node::Start),
            0x24 => Ok(Node::End),
            0x5b => self.parse_class().map(Node::Class),
            0x28 => self.parse_group(),
            0x5c => self.parse_escape(false),
            0x29 => Err(self.error("unmatched ')'")),
            0x2a | 0x2b | 0x3f => Err(self.error("nothing to repeat")),
            _ => {
                let start = self.position - 1;
                let (value, width) = next_code_point(self.units, start, self.unicode);
                self.position = start + width;
                Ok(Node::Literal(value))
            }
        }
    }

    fn parse_group(&mut self) -> Result<Node, RegexError> {
        let mut index = None;
        let mut look = None;
        if self.peek() == Some(u16::from(b'?')) {
            self.position += 1;
            match self.next() {
                Some(0x3a) => {}
                Some(0x3d) => look = Some((false, true)),
                Some(0x21) => look = Some((false, false)),
                Some(0x3c) => match self.peek() {
                    Some(0x3d) => {
                        self.position += 1;
                        look = Some((true, true));
                    }
                    Some(0x21) => {
                        self.position += 1;
                        look = Some((true, false));
                    }
                    _ => {
                        let name = self.take_until(0x3e)?;
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
        let body = self.parse_disjunction(Some(0x29))?;
        if self.next() != Some(0x29) {
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
        let negated = if self.peek() == Some(0x5e) {
            self.position += 1;
            true
        } else {
            false
        };
        let mut items = Vec::new();
        let mut first = true;
        while let Some(token) = self.peek() {
            if token == 0x5d && !first {
                self.position += 1;
                return Ok(CharacterClass { negated, items });
            }
            first = false;
            let left = self.parse_class_item()?;
            if self.peek() == Some(0x2d) && self.units.get(self.position + 1) != Some(&0x5d) {
                self.position += 1;
                let right = self.parse_class_item()?;
                match (left, right) {
                    (ClassItem::Character(start), ClassItem::Character(end)) if start <= end => {
                        items.push(ClassItem::Range(start, end));
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
        if token != 0x5c {
            let start = self.position - 1;
            let (value, width) = next_code_point(self.units, start, self.unicode);
            self.position = start + width;
            return Ok(ClassItem::Character(value));
        }
        let escaped = self.next().ok_or_else(|| self.error("trailing escape"))?;
        match escaped {
            0x64 => Ok(ClassItem::Digit),
            0x44 => Ok(ClassItem::NotDigit),
            0x77 => Ok(ClassItem::Word),
            0x57 => Ok(ClassItem::NotWord),
            0x73 => Ok(ClassItem::Space),
            0x53 => Ok(ClassItem::NotSpace),
            _ => self.escape_character(escaped).map(ClassItem::Character),
        }
    }

    fn parse_escape(&mut self, _in_class: bool) -> Result<Node, RegexError> {
        let escaped = self.next().ok_or_else(|| self.error("trailing escape"))?;
        match escaped {
            0x64 => Ok(class_node(ClassItem::Digit)),
            0x44 => Ok(class_node(ClassItem::NotDigit)),
            0x77 => Ok(class_node(ClassItem::Word)),
            0x57 => Ok(class_node(ClassItem::NotWord)),
            0x73 => Ok(class_node(ClassItem::Space)),
            0x53 => Ok(class_node(ClassItem::NotSpace)),
            0x62 => Ok(Node::WordBoundary(true)),
            0x42 => Ok(Node::WordBoundary(false)),
            0x6b if self.next() == Some(0x3c) => {
                Ok(Node::NamedBackReference(self.take_until(0x3e)?))
            }
            value if value <= 0x7f && (value as u8).is_ascii_digit() && value != 0x30 => {
                let mut number = (value - 0x30) as usize;
                while let Some(digit) = self.peek().and_then(decimal_digit) {
                    self.position += 1;
                    number = number * 10 + digit;
                }
                Ok(Node::BackReference(number))
            }
            value => self.escape_character(value).map(Node::Literal),
        }
    }

    fn escape_character(&mut self, escaped: u16) -> Result<u32, RegexError> {
        match escaped {
            0x6e => Ok(0x0a),
            0x72 => Ok(0x0d),
            0x74 => Ok(0x09),
            0x66 => Ok(0x0c),
            0x76 => Ok(0x0b),
            0x30 => Ok(0),
            0x78 => self.hex_escape(2),
            0x75 if self.peek() == Some(0x7b) => {
                self.position += 1;
                let digits = self.take_until(0x7d)?;
                u32::from_str_radix(&digits, 16)
                    .ok()
                    .filter(|value| *value <= 0x10ffff && !(0xd800..=0xdfff).contains(value))
                    .ok_or_else(|| self.error("invalid Unicode escape"))
            }
            0x75 => {
                let first = self.hex_escape(4)?;
                if self.unicode && (0xd800..=0xdbff).contains(&first) {
                    let checkpoint = self.position;
                    if self.next() == Some(0x5c)
                        && self.next() == Some(0x75)
                        && let Ok(second) = self.hex_escape(4)
                        && (0xdc00..=0xdfff).contains(&second)
                    {
                        return Ok(combine_surrogates(first as u16, second as u16));
                    }
                    self.position = checkpoint;
                }
                Ok(first)
            }
            value => Ok(u32::from(value)),
        }
    }

    fn hex_escape(&mut self, count: usize) -> Result<u32, RegexError> {
        if self.position + count > self.units.len() {
            return Err(self.error("invalid hexadecimal escape"));
        }
        let mut value = 0u32;
        for unit in &self.units[self.position..self.position + count] {
            let Some(digit) = hex_digit(*unit) else {
                return Err(self.error("invalid hexadecimal escape"));
            };
            value = value * 16 + digit;
        }
        self.position += count;
        Ok(value)
    }

    fn parse_quantifier(&mut self, atom: Node) -> Result<Node, RegexError> {
        let Some(token) = self.peek() else {
            return Ok(atom);
        };
        let (min, max) = match token {
            0x2a => {
                self.position += 1;
                (0, None)
            }
            0x2b => {
                self.position += 1;
                (1, None)
            }
            0x3f => {
                self.position += 1;
                (0, Some(1))
            }
            0x7b => {
                let checkpoint = self.position;
                self.position += 1;
                let Some(minimum) = self.parse_decimal() else {
                    self.position = checkpoint;
                    return Ok(atom);
                };
                match self.next() {
                    Some(0x7d) => (minimum, Some(minimum)),
                    Some(0x2c) => {
                        let maximum = self.parse_decimal();
                        if self.next() != Some(0x7d) {
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
        let greedy = if self.peek() == Some(0x3f) {
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
        while let Some(digit) = self.peek().and_then(decimal_digit) {
            self.position += 1;
            value = value.checked_mul(10)?.checked_add(digit)?;
        }
        (self.position > begin).then_some(value)
    }

    fn take_until(&mut self, terminator: u16) -> Result<String, RegexError> {
        let begin = self.position;
        while self.peek().is_some_and(|token| token != terminator) {
            self.position += 1;
        }
        if self.next() != Some(terminator) {
            let printable = char::from_u32(u32::from(terminator)).unwrap_or('?');
            return Err(self.error(format!("expected '{printable}'")));
        }
        String::from_utf16(&self.units[begin..self.position - 1])
            .map_err(|_| self.error("invalid Unicode capture name"))
    }

    fn peek(&self) -> Option<u16> {
        self.units.get(self.position).copied()
    }

    fn next(&mut self) -> Option<u16> {
        let value = self.peek()?;
        self.position += 1;
        Some(value)
    }

    fn error(&self, message: impl Into<String>) -> RegexError {
        RegexError::new(format!("{} at position {}", message.into(), self.position))
    }
}

fn class_node(item: ClassItem) -> Node {
    Node::Class(CharacterClass {
        negated: false,
        items: vec![item],
    })
}

fn decimal_digit(unit: u16) -> Option<usize> {
    (unit <= 0x7f)
        .then_some(unit as u8)
        .and_then(|byte| byte.is_ascii_digit().then(|| (byte - b'0') as usize))
}

fn hex_digit(unit: u16) -> Option<u32> {
    (unit <= 0x7f)
        .then_some(unit as u8)
        .and_then(|byte| (byte as char).to_digit(16))
}

fn combine_surrogates(high: u16, low: u16) -> u32 {
    0x1_0000 + ((u32::from(high) - 0xd800) << 10) + (u32::from(low) - 0xdc00)
}

#[cfg(test)]
mod tests {
    use super::Regex;
    use bamts_bytecode::EcmaString;

    fn text(value: &str) -> EcmaString {
        EcmaString::from_utf8(value)
    }

    fn regex(pattern: &str, flags: &str) -> Regex {
        Regex::compile(&text(pattern), &text(flags)).unwrap()
    }

    fn ranges(pattern: &str, flags: &str, input: &str) -> Vec<Option<std::ops::Range<usize>>> {
        regex(pattern, flags)
            .exec(&text(input), 0)
            .unwrap()
            .captures
    }

    #[test]
    fn corpus_escape_patterns_match_byte_exact_node_results() {
        let escaped = regex(r"\\ \^ \$ \* \+ \? \. \( \) \| \{ \} \[ \] \x2d", "");
        assert_eq!(
            escaped
                .exec(&text(r"\ ^ $ * + ? . ( ) | { } [ ] -"), 0)
                .unwrap()
                .range,
            0..29
        );
        assert_eq!(regex(r"\x2d", "u").exec(&text("-"), 0).unwrap().range, 0..1);
        assert_eq!(
            regex(r"\\", "g").exec(&text(r"a\b"), 0).unwrap().range,
            1..2
        );
    }

    #[test]
    fn supports_corpus_glob_shapes() {
        assert!(
            regex(r"^(?:[^/]*?)\.js$", "")
                .exec(&text("a.js"), 0)
                .is_some()
        );
        assert!(
            regex(r"^(?:.*?)\/?\.ts$", "")
                .exec(&text("src/a/b.ts"), 0)
                .is_some()
        );
        assert!(regex(r"[abc]at", "").exec(&text("cat"), 0).is_some());
        assert!(regex(r"^(a|b)\.js$", "").exec(&text("b.js"), 0).is_some());
    }

    #[test]
    fn destr_json_signature_matches_number_and_preserves_unmatched_captures() {
        let input = text("123");
        let matched = regex(
            r#"^\s*["[{]|^\s*-?\d{1,16}(\.\d{1,17})?([Ee][+-]?\d+)?\s*$"#,
            "",
        )
        .exec(&input, 0)
        .unwrap();

        assert_eq!(matched.range, 0..input.as_units().len());
        assert_eq!(
            matched.captures,
            vec![Some(0..input.as_units().len()), None, None]
        );
    }

    #[test]
    fn captures_backreferences_and_lookarounds() {
        assert_eq!(
            ranges(r"(?<word>[A-z]+)-\k<word>", "i", "Ab-ab")[1],
            Some(0..2)
        );
        assert!(
            regex(r"(?<=foo)bar(?=$)", "")
                .exec(&text("foobar"), 0)
                .is_some()
        );
        assert!(regex(r"foo(?!bar)", "").exec(&text("foobaz"), 0).is_some());
    }

    #[test]
    fn lazy_and_sticky_matching() {
        let lazy = regex("a.*?b", "s");
        assert_eq!(lazy.exec(&text("a1b2b"), 0).unwrap().range, 0..3);
        let sticky = regex("b", "y");
        assert!(sticky.exec(&text("ab"), 0).is_none());
        assert_eq!(sticky.exec(&text("ab"), 1).unwrap().range, 1..2);
    }

    #[test]
    fn flags_are_canonical_like_node_24() {
        assert!(regex("", "yusmig").flags().canonical().eq_ascii("gimsuy"));
        assert!(Regex::compile(&text(""), &text("gg")).is_err());
    }

    #[test]
    fn dot_uses_code_units_without_u_and_code_points_with_u() {
        let input = text("😀");
        let plain = regex(".", "g");
        assert_eq!(plain.exec(&input, 0).unwrap().range, 0..1);
        assert_eq!(plain.exec(&input, 1).unwrap().range, 1..2);
        assert_eq!(regex(".", "u").exec(&input, 0).unwrap().range, 0..2);
    }

    #[test]
    fn captures_and_match_indices_are_code_unit_offsets() {
        let matched = regex("(x)", "").exec(&text("😀x"), 0).unwrap();
        assert_eq!(matched.range, 2..3);
        assert_eq!(matched.captures[1], Some(2..3));
    }

    #[test]
    fn unicode_classes_support_supplementary_ranges() {
        let matched = regex(r"[\u{1F600}-\u{1F64F}]", "u")
            .exec(&text("😀"), 0)
            .unwrap();
        assert_eq!(matched.range, 0..2);
    }

    #[test]
    fn lone_surrogates_remain_exact_units() {
        let input = EcmaString::from_units(&[0xd800]);
        let pattern = EcmaString::from_units(&[0xd800]);
        let matched = Regex::compile(&pattern, &text(""))
            .unwrap()
            .exec(&input, 0)
            .unwrap();
        assert_eq!(matched.range, 0..1);
        assert_eq!(input.as_units(), &[0xd800]);
    }

    #[test]
    fn sticky_offsets_are_code_units() {
        let sticky = regex("x", "y");
        let input = text("😀x");
        assert!(sticky.exec(&input, 1).is_none());
        assert_eq!(sticky.exec(&input, 2).unwrap().range, 2..3);
    }

    #[test]
    fn escaped_surrogate_pairs_combine_only_in_unicode_mode() {
        let input = text("😀");
        assert_eq!(
            regex(r"\uD83D\uDE00", "u").exec(&input, 0).unwrap().range,
            0..2
        );
        assert_eq!(
            regex(r"\uD83D\uDE00", "").exec(&input, 0).unwrap().range,
            0..2
        );
    }
}
