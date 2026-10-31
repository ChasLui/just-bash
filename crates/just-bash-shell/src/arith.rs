//! Shell arithmetic: `$(( … ))`, `(( … ))` and `let`.
//!
//! Implements the full C-like operator set bash supports, with bash's
//! precedence and associativity. Values are `i64`; variables are read through
//! [`Vars`] so the evaluator can run against the live shell environment or a
//! frozen snapshot.

use std::collections::BTreeMap;

/// Variable access for the evaluator.
pub trait Vars {
    fn get(&self, name: &str) -> Option<String>;
    fn set(&mut self, name: &str, value: i64);
}

/// The live shell environment: assignments persist.
pub struct LiveVars<'a> {
    map: &'a mut BTreeMap<String, String>,
    positionals: &'a [String],
}

impl<'a> LiveVars<'a> {
    pub fn new(map: &'a mut BTreeMap<String, String>, positionals: &'a [String]) -> Self {
        Self { map, positionals }
    }
}

impl Vars for LiveVars<'_> {
    fn get(&self, name: &str) -> Option<String> {
        if let Some(index) = positional_index(name) {
            return self.positionals.get(index).cloned();
        }
        self.map.get(name).cloned()
    }

    fn set(&mut self, name: &str, value: i64) {
        // Positional parameters are read-only; bash reports an error, but a
        // silent no-op keeps the evaluator total.
        if positional_index(name).is_none() {
            self.map.insert(name.to_string(), value.to_string());
        }
    }
}

/// A read-only view used where the caller only has `&self` (`${a[i]}`,
/// slice bounds). Assignments evaluate correctly but are discarded.
pub struct FrozenVars<'a> {
    map: &'a BTreeMap<String, String>,
    positionals: &'a [String],
}

impl<'a> FrozenVars<'a> {
    pub fn new(map: &'a BTreeMap<String, String>, positionals: &'a [String]) -> Self {
        Self { map, positionals }
    }
}

impl Vars for FrozenVars<'_> {
    fn get(&self, name: &str) -> Option<String> {
        if let Some(index) = positional_index(name) {
            return self.positionals.get(index).cloned();
        }
        self.map.get(name).cloned()
    }

    fn set(&mut self, _name: &str, _value: i64) {}
}

/// `1`-based positional parameter name (`"1"`) → zero-based index.
fn positional_index(name: &str) -> Option<usize> {
    if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    name.parse::<usize>()
        .ok()
        .and_then(|index| index.checked_sub(1))
}

/// Evaluate one arithmetic expression.
pub fn eval(vars: &mut impl Vars, expression: &str) -> Result<i64, String> {
    let mut parser = Parser {
        tokens: tokenize(expression)?,
        pos: 0,
        vars,
        depth: 0,
        expression: expression.to_string(),
    };
    let value = parser.parse_comma()?;
    if parser.pos != parser.tokens.len() {
        return Err(parser.syntax_error());
    }
    Ok(value)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Number(i64),
    Name(String),
    Operator(&'static str),
    LParen,
    RParen,
    Question,
    Colon,
}

const OPERATORS: &[&str] = &[
    "<<=", ">>=", "**", "++", "--", "<<", ">>", "<=", ">=", "==", "!=", "&&", "||", "+=", "-=",
    "*=", "/=", "%=", "&=", "|=", "^=", "+", "-", "*", "/", "%", "!", "~", "&", "|", "^", "<", ">",
    "=", ",",
];

fn tokenize(expression: &str) -> Result<Vec<Token>, String> {
    let bytes = expression.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if byte.is_ascii_digit() {
            let (value, next) = read_number(expression, index)?;
            tokens.push(Token::Number(value));
            index = next;
            continue;
        }
        // Parameters may be written with a `$` prefix inside arithmetic:
        // `$(( $1 - 1 ))` and `$(( ${n} + 1 ))` are both valid.
        if byte == b'$' {
            let mut cursor = index + 1;
            if cursor < bytes.len() && bytes[cursor] == b'{' {
                let start = cursor + 1;
                let mut end = start;
                while end < bytes.len() && bytes[end] != b'}' {
                    end += 1;
                }
                if end >= bytes.len() {
                    return Err(format!("syntax error in expression `{expression}'"));
                }
                tokens.push(Token::Name(expression[start..end].to_string()));
                cursor = end + 1;
            } else {
                let start = cursor;
                while cursor < bytes.len()
                    && (bytes[cursor] == b'_' || bytes[cursor].is_ascii_alphanumeric())
                {
                    cursor += 1;
                }
                if start == cursor {
                    return Err(format!("syntax error in expression `{expression}'"));
                }
                tokens.push(Token::Name(expression[start..cursor].to_string()));
            }
            index = cursor;
            continue;
        }
        if byte == b'_' || byte.is_ascii_alphabetic() {
            let start = index;
            while index < bytes.len()
                && (bytes[index] == b'_' || bytes[index].is_ascii_alphanumeric())
            {
                index += 1;
            }
            tokens.push(Token::Name(expression[start..index].to_string()));
            continue;
        }
        match byte {
            b'(' => {
                tokens.push(Token::LParen);
                index += 1;
                continue;
            }
            b')' => {
                tokens.push(Token::RParen);
                index += 1;
                continue;
            }
            b'?' => {
                tokens.push(Token::Question);
                index += 1;
                continue;
            }
            b':' => {
                tokens.push(Token::Colon);
                index += 1;
                continue;
            }
            _ => {}
        }
        let rest = &expression[index..];
        let Some(operator) = OPERATORS.iter().find(|op| rest.starts_with(**op)) else {
            return Err(format!("syntax error in expression `{expression}'"));
        };
        tokens.push(Token::Operator(operator));
        index += operator.len();
    }
    Ok(tokens)
}

/// Read a decimal, `0x` hexadecimal or leading-zero octal literal.
fn read_number(expression: &str, start: usize) -> Result<(i64, usize), String> {
    let bytes = expression.as_bytes();
    let mut index = start;
    if bytes[index] == b'0' && index + 1 < bytes.len() && (bytes[index + 1] | 0x20) == b'x' {
        index += 2;
        let digits_start = index;
        while index < bytes.len() && bytes[index].is_ascii_hexdigit() {
            index += 1;
        }
        if index == digits_start {
            return Err(format!("syntax error in expression `{expression}'"));
        }
        let value = i64::from_str_radix(&expression[digits_start..index], 16)
            .map_err(|_| format!("syntax error in expression `{expression}'"))?;
        return Ok((value, index));
    }
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    // `BASE#DIGITS`, e.g. `16#ff` or `2#1010`.
    if index < bytes.len() && bytes[index] == b'#' && index > start {
        let base: u32 = expression[start..index]
            .parse()
            .map_err(|_| format!("syntax error in expression `{expression}'"))?;
        if !(2..=64).contains(&base) {
            return Err(format!("invalid arithmetic base `{base}'"));
        }
        let digits_start = index + 1;
        let mut cursor = digits_start;
        while cursor < bytes.len()
            && (bytes[cursor].is_ascii_alphanumeric()
                || bytes[cursor] == b'@'
                || bytes[cursor] == b'_')
        {
            cursor += 1;
        }
        if cursor == digits_start {
            return Err(format!("syntax error in expression `{expression}'"));
        }
        let digits = &expression[digits_start..cursor];
        let mut value: i64 = 0;
        for ch in digits.chars() {
            let digit = match ch {
                '0'..='9' => ch as u32 - '0' as u32,
                'a'..='z' => ch as u32 - 'a' as u32 + 10,
                'A'..='Z' => ch as u32 - 'A' as u32 + 10,
                '@' => 62,
                '_' => 63,
                _ => return Err(format!("invalid digit in `{digits}'")),
            };
            if digit >= base {
                return Err(format!("value too great for base `{digits}'"));
            }
            value = value.wrapping_mul(base as i64).wrapping_add(digit as i64);
        }
        return Ok((value, cursor));
    }
    let text = &expression[start..index];
    let value = if text.len() > 1 && text.starts_with('0') {
        // `010` is octal, as in bash.
        i64::from_str_radix(&text[1..], 8)
            .map_err(|_| format!("value too great for base `{text}'"))?
    } else {
        text.parse::<i64>()
            .map_err(|_| format!("value too great for base `{text}'"))?
    };
    Ok((value, index))
}

struct Parser<'a, V: Vars> {
    tokens: Vec<Token>,
    pos: usize,
    vars: &'a mut V,
    depth: usize,
    expression: String,
}

impl<V: Vars> Parser<'_, V> {
    fn syntax_error(&self) -> String {
        format!("syntax error in expression `{}'", self.expression)
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn peek_operator(&self) -> Option<&'static str> {
        match self.peek() {
            Some(Token::Operator(op)) => Some(op),
            _ => None,
        }
    }

    fn eat_operator(&mut self, wanted: &str) -> bool {
        if self.peek_operator() == Some(wanted) {
            self.pos += 1;
            return true;
        }
        false
    }

    /// Low-precedence comma operator (only valid inside `(( ))`-style lists).
    fn parse_comma(&mut self) -> Result<i64, String> {
        let mut value = self.parse_assignment()?;
        while self.eat_operator(",") {
            value = self.parse_assignment()?;
        }
        Ok(value)
    }

    fn parse_assignment(&mut self) -> Result<i64, String> {
        // Assignment is right-associative and needs an lvalue.
        if let Some(Token::Name(name)) = self.peek().cloned() {
            let saved = self.pos;
            self.pos += 1;
            if let Some(op) = self.peek_operator()
                && matches!(
                    op,
                    "=" | "+=" | "-=" | "*=" | "/=" | "%=" | "<<=" | ">>=" | "&=" | "|=" | "^="
                )
            {
                self.pos += 1;
                let right = self.parse_assignment()?;
                let current = self.read_variable(&name);
                let value = match op {
                    "=" => right,
                    "+=" => current.wrapping_add(right),
                    "-=" => current.wrapping_sub(right),
                    "*=" => current.wrapping_mul(right),
                    "/=" => {
                        if right == 0 {
                            return Err(self.division_by_zero());
                        }
                        current.wrapping_div(right)
                    }
                    "%=" => {
                        if right == 0 {
                            return Err(self.division_by_zero());
                        }
                        current.wrapping_rem(right)
                    }
                    "<<=" => current.wrapping_shl(right as u32),
                    ">>=" => current.wrapping_shr(right as u32),
                    "&=" => current & right,
                    "|=" => current | right,
                    "^=" => current ^ right,
                    _ => unreachable!(),
                };
                self.vars.set(&name, value);
                return Ok(value);
            }
            self.pos = saved;
        }
        self.parse_ternary()
    }

    fn parse_ternary(&mut self) -> Result<i64, String> {
        let condition = self.parse_binary(0)?;
        if self.peek() == Some(&Token::Question) {
            self.pos += 1;
            let when_true = self.parse_assignment()?;
            if self.peek() != Some(&Token::Colon) {
                return Err(self.syntax_error());
            }
            self.pos += 1;
            let when_false = self.parse_ternary()?;
            return Ok(if condition != 0 {
                when_true
            } else {
                when_false
            });
        }
        Ok(condition)
    }

    /// Binary operators, by precedence level (`||` loosest).
    fn parse_binary(&mut self, level: usize) -> Result<i64, String> {
        const LEVELS: &[&[&str]] = &[
            &["||"],
            &["&&"],
            &["|"],
            &["^"],
            &["&"],
            &["==", "!="],
            &["<", "<=", ">", ">="],
            &["<<", ">>"],
            &["+", "-"],
            &["*", "/", "%"],
        ];
        if level == LEVELS.len() {
            return self.parse_power();
        }
        let mut left = self.parse_binary(level + 1)?;
        while let Some(operator) = self.peek_operator() {
            if !LEVELS[level].contains(&operator) {
                break;
            }
            self.pos += 1;
            // `&&` and `||` short-circuit like C.
            if operator == "&&" && left == 0 {
                self.parse_binary(level + 1)?;
                left = 0;
                continue;
            }
            if operator == "||" && left != 0 {
                self.parse_binary(level + 1)?;
                left = 1;
                continue;
            }
            let right = self.parse_binary(level + 1)?;
            left = match operator {
                "||" => i64::from(left != 0 || right != 0),
                "&&" => i64::from(left != 0 && right != 0),
                "|" => left | right,
                "^" => left ^ right,
                "&" => left & right,
                "==" => i64::from(left == right),
                "!=" => i64::from(left != right),
                "<" => i64::from(left < right),
                "<=" => i64::from(left <= right),
                ">" => i64::from(left > right),
                ">=" => i64::from(left >= right),
                "<<" => left.wrapping_shl(right as u32),
                ">>" => left.wrapping_shr(right as u32),
                "+" => left.wrapping_add(right),
                "-" => left.wrapping_sub(right),
                "*" => left.wrapping_mul(right),
                "/" => {
                    if right == 0 {
                        return Err(self.division_by_zero());
                    }
                    left.wrapping_div(right)
                }
                "%" => {
                    if right == 0 {
                        return Err(self.division_by_zero());
                    }
                    left.wrapping_rem(right)
                }
                _ => unreachable!(),
            };
        }
        Ok(left)
    }

    /// `**` is right-associative (`2**3**2` is `2**(3**2)`) and, as in bash,
    /// binds looser than a leading sign: `-2**2` is `4`.
    fn parse_power(&mut self) -> Result<i64, String> {
        let base = self.parse_unary()?;
        if self.eat_operator("**") {
            // Right-associative: the exponent is itself a power expression.
            let exponent = self.parse_power()?;
            if exponent < 0 {
                return Err(format!(
                    "exponent less than 0 in expression `{}'",
                    self.expression
                ));
            }
            return Ok(pow(base, exponent));
        }
        Ok(base)
    }

    /// A primary expression followed by an optional postfix `++`/`--`.
    fn parse_postfix(&mut self) -> Result<i64, String> {
        let start = self.pos;
        let value = self.parse_primary()?;
        if self.peek_operator() == Some("++") || self.peek_operator() == Some("--") {
            let operator = self.peek_operator().unwrap_or("++").to_string();
            if let Some(Token::Name(name)) = self.tokens.get(start).cloned() {
                self.pos += 1;
                let updated = if operator == "++" {
                    value.wrapping_add(1)
                } else {
                    value.wrapping_sub(1)
                };
                self.vars.set(&name, updated);
                // The postfix form yields the previous value.
                return Ok(value);
            }
            return Err(self.syntax_error());
        }
        Ok(value)
    }

    fn parse_unary(&mut self) -> Result<i64, String> {
        if self.depth > 64 {
            return Err(self.syntax_error());
        }
        self.depth += 1;
        let result = self.parse_unary_inner();
        self.depth -= 1;
        result
    }

    fn parse_unary_inner(&mut self) -> Result<i64, String> {
        match self.peek().cloned() {
            Some(Token::Operator("+")) => {
                self.pos += 1;
                self.parse_unary()
            }
            Some(Token::Operator("-")) => {
                self.pos += 1;
                Ok(self.parse_unary()?.wrapping_neg())
            }
            Some(Token::Operator("!")) => {
                self.pos += 1;
                Ok(i64::from(self.parse_unary()? == 0))
            }
            Some(Token::Operator("~")) => {
                self.pos += 1;
                Ok(!self.parse_unary()?)
            }
            Some(Token::Operator("++")) => {
                self.pos += 1;
                let Some(Token::Name(name)) = self.peek().cloned() else {
                    return Err(self.syntax_error());
                };
                self.pos += 1;
                let value = self.read_variable(&name).wrapping_add(1);
                self.vars.set(&name, value);
                Ok(value)
            }
            Some(Token::Operator("--")) => {
                self.pos += 1;
                let Some(Token::Name(name)) = self.peek().cloned() else {
                    return Err(self.syntax_error());
                };
                self.pos += 1;
                let value = self.read_variable(&name).wrapping_sub(1);
                self.vars.set(&name, value);
                Ok(value)
            }
            _ => self.parse_postfix(),
        }
    }

    fn parse_primary(&mut self) -> Result<i64, String> {
        match self.peek().cloned() {
            Some(Token::Number(value)) => {
                self.pos += 1;
                Ok(value)
            }
            Some(Token::Name(name)) => {
                self.pos += 1;
                Ok(self.read_variable(&name))
            }
            Some(Token::LParen) => {
                self.pos += 1;
                let value = self.parse_comma()?;
                if self.peek() != Some(&Token::RParen) {
                    return Err(self.syntax_error());
                }
                self.pos += 1;
                Ok(value)
            }
            _ => Err(self.syntax_error()),
        }
    }

    /// Resolve a variable to a number. An unset variable is 0; a value that is
    /// itself an expression is evaluated (bounded, so `x=x` cannot loop).
    fn read_variable(&mut self, name: &str) -> i64 {
        let Some(raw) = self.vars.get(name) else {
            return 0;
        };
        let text = raw.trim();
        if text.is_empty() {
            return 0;
        }
        if let Ok(value) = text.parse::<i64>() {
            return value;
        }
        if self.depth > 16 {
            return 0;
        }
        self.depth += 2;
        let mut nested = Parser {
            tokens: tokenize(text).unwrap_or_default(),
            pos: 0,
            vars: self.vars,
            depth: self.depth,
            expression: text.to_string(),
        };
        let value = nested.parse_comma().unwrap_or(0);
        self.depth -= 2;
        value
    }

    fn division_by_zero(&self) -> String {
        format!("division by 0 in expression `{}'", self.expression)
    }
}

/// `base ** exponent`; the caller rejects negative exponents, as bash does.
fn pow(base: i64, exponent: i64) -> i64 {
    let mut result = 1i64;
    let mut factor = base;
    let mut remaining = exponent;
    while remaining > 0 {
        if remaining & 1 == 1 {
            result = result.wrapping_mul(factor);
        }
        factor = factor.wrapping_mul(factor);
        remaining >>= 1;
    }
    result
}
