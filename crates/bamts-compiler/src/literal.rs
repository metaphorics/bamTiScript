use bamts_bytecode::{EcmaString, EcmaStringBuilder};

/// Cooks one scanned numeric lexeme into its ECMAScript number value.
pub(crate) fn number_value(lexeme: &str) -> Option<f64> {
    if let Some(rest) = lexeme
        .strip_prefix("0x")
        .or_else(|| lexeme.strip_prefix("0X"))
    {
        return radix_value(rest, 16);
    }
    if let Some(rest) = lexeme
        .strip_prefix("0o")
        .or_else(|| lexeme.strip_prefix("0O"))
    {
        return radix_value(rest, 8);
    }
    if let Some(rest) = lexeme
        .strip_prefix("0b")
        .or_else(|| lexeme.strip_prefix("0B"))
    {
        return radix_value(rest, 2);
    }
    if !lexeme.contains('_') {
        return lexeme.parse().ok();
    }
    lexeme
        .chars()
        .filter(|character| *character != '_')
        .collect::<String>()
        .parse()
        .ok()
}
pub(crate) const MAX_BIGINT_CONVERSION_LIMB_OPS: usize = 1 << 24;
/// Failure while canonicalizing BigInt source text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BigIntTextError {
    Invalid,
    Bytes,
    Work,
}
/// Canonicalizes a BigInt lexeme to bounded decimal text.
pub(crate) fn canonical_bigint_text(
    lexeme: &str,
    max_bytes: usize,
    max_limb_ops: usize,
) -> Result<String, BigIntTextError> {
    const LIMB_BASE: u64 = 1_000_000_000;
    const DECIMAL_LIMB_DIGITS: usize = 9;
    const LOG10_2_UPPER_NUMERATOR: usize = 30_103;
    const LOG10_2_UPPER_DENOMINATOR: usize = 100_000;
    let literal = lexeme.strip_suffix('n').ok_or(BigIntTextError::Invalid)?;
    let (digits, radix) = literal
        .strip_prefix("0x")
        .or_else(|| literal.strip_prefix("0X"))
        .map(|digits| (digits, 16))
        .or_else(|| {
            literal
                .strip_prefix("0o")
                .or_else(|| literal.strip_prefix("0O"))
                .map(|digits| (digits, 8))
        })
        .or_else(|| {
            literal
                .strip_prefix("0b")
                .or_else(|| literal.strip_prefix("0B"))
                .map(|digits| (digits, 2))
        })
        .unwrap_or((literal, 10));
    if digits.is_empty() {
        return Err(BigIntTextError::Invalid);
    }
    let mut previous_was_digit = false;
    let mut significant_digits = 0_usize;
    let mut first_significant_bits = 0_usize;
    for character in digits.chars() {
        if character == '_' {
            if !previous_was_digit {
                return Err(BigIntTextError::Invalid);
            }
            previous_was_digit = false;
            continue;
        }
        let digit = character.to_digit(radix).ok_or(BigIntTextError::Invalid)?;
        previous_was_digit = true;
        if first_significant_bits == 0 {
            if digit == 0 {
                continue;
            }
            first_significant_bits = (u32::BITS - digit.leading_zeros()) as usize;
        }
        significant_digits = significant_digits
            .checked_add(1)
            .ok_or(BigIntTextError::Work)?;
    }
    if !previous_was_digit {
        return Err(BigIntTextError::Invalid);
    }
    if radix == 10 {
        let output_bytes = significant_digits.max(1);
        if output_bytes > max_bytes {
            return Err(BigIntTextError::Bytes);
        }
        let mut output = String::with_capacity(output_bytes);
        let mut significant = false;
        for character in digits.chars().filter(|character| *character != '_') {
            if character != '0' || significant {
                significant = true;
                output.push(character);
            }
        }
        if output.is_empty() {
            output.push('0');
        }
        return Ok(output);
    }
    let bit_length = significant_digits
        .saturating_sub(1)
        .checked_mul(radix.trailing_zeros() as usize)
        .and_then(|remaining| remaining.checked_add(first_significant_bits))
        .ok_or(BigIntTextError::Work)?;
    let max_decimal_bytes = if bit_length == 0 {
        1
    } else {
        bit_length
            .checked_mul(LOG10_2_UPPER_NUMERATOR)
            .and_then(|value| value.checked_add(LOG10_2_UPPER_DENOMINATOR - 1))
            .map(|value| value / LOG10_2_UPPER_DENOMINATOR)
            .ok_or(BigIntTextError::Work)?
    };
    if max_decimal_bytes > max_bytes {
        return Err(BigIntTextError::Bytes);
    }
    let max_limb_count = max_decimal_bytes.div_ceil(DECIMAL_LIMB_DIGITS);
    let worst_case_limb_ops = significant_digits
        .checked_mul(max_limb_count)
        .ok_or(BigIntTextError::Work)?;
    if worst_case_limb_ops > max_limb_ops {
        return Err(BigIntTextError::Work);
    }
    let mut limbs = vec![0_u32];
    let mut significant = false;
    for character in digits.chars().filter(|character| *character != '_') {
        let digit = u64::from(
            character
                .to_digit(radix)
                .expect("validated BigInt digit remains valid"),
        );
        if digit == 0 && !significant {
            continue;
        }
        significant = true;
        let mut carry = digit;
        for limb in &mut limbs {
            let value = u64::from(*limb) * u64::from(radix) + carry;
            *limb = (value % LIMB_BASE) as u32;
            carry = value / LIMB_BASE;
        }
        if carry != 0 {
            limbs.push(carry as u32);
        }
    }
    let mut output = limbs.pop().ok_or(BigIntTextError::Invalid)?.to_string();
    for limb in limbs.iter().rev() {
        use std::fmt::Write as _;
        write!(output, "{limb:09}").expect("writing to String cannot fail");
    }
    if output.len() > max_bytes {
        return Err(BigIntTextError::Bytes);
    }
    Ok(output)
}

/// Cooks one quoted JavaScript string literal.
pub(crate) fn string_value(lexeme: &str) -> Option<EcmaString> {
    let quote = lexeme.as_bytes().first().copied()?;
    if !matches!(quote, b'\'' | b'"') || lexeme.as_bytes().last().copied() != Some(quote) {
        return None;
    }
    let interior = lexeme.get(1..lexeme.len().checked_sub(1)?)?;
    Some(cook_escapes(interior))
}

/// Cooks JavaScript escape sequences in a string or template interior.
pub(crate) fn cook_escapes(input: &str) -> EcmaString {
    if !input.contains('\\') {
        return EcmaString::encode(input);
    }
    let mut output = EcmaStringBuilder::with_capacity(input.encode_utf16().count());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            output
                .push_code_point(u32::from(ch))
                .expect("a Rust char is a Unicode scalar");
            continue;
        }
        let Some(escape) = chars.next() else {
            output.push_unit(b'\\'.into());
            break;
        };
        match escape {
            'n' => output.push_unit(b'\n'.into()),
            't' => output.push_unit(b'\t'.into()),
            'r' => output.push_unit(b'\r'.into()),
            'b' => output.push_unit(0x0008),
            'f' => output.push_unit(0x000C),
            'v' => output.push_unit(0x000B),
            '0' if !chars.peek().is_some_and(|c| c.is_ascii_digit()) => output.push_unit(0),
            '\n' => {}
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
            }
            // U+2028 LINE SEPARATOR and U+2029 PARAGRAPH SEPARATOR are LineTerminator
            // characters per spec; after a backslash they act as line continuations
            // (the character is consumed and emits nothing), like \n and \r above.
            '\u{2028}' => {}
            '\u{2029}' => {}
            'x' => {
                let hi = chars.next();
                let lo = chars.next();
                if let (Some(hi), Some(lo)) = (hi, lo)
                    && let (Some(h), Some(l)) = (hi.to_digit(16), lo.to_digit(16))
                {
                    output.push_unit((h * 16 + l) as u16);
                } else {
                    output.push_unit(b'x'.into());
                }
            }
            'u' => cook_unicode_escape(&mut chars, &mut output),
            other => output
                .push_code_point(u32::from(other))
                .expect("a Rust char is a Unicode scalar"),
        }
    }
    output.finish()
}

fn radix_value(digits: &str, radix: u32) -> Option<f64> {
    let mut value = 0.0_f64;
    let mut any = false;
    for character in digits.chars().filter(|character| *character != '_') {
        let digit = character.to_digit(radix)?;
        value = value * f64::from(radix) + f64::from(digit);
        any = true;
    }
    any.then_some(value)
}

fn cook_unicode_escape(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    output: &mut EcmaStringBuilder,
) {
    if chars.peek() == Some(&'{') {
        chars.next();
        let mut value = 0_u32;
        let mut any = false;
        while let Some(&character) = chars.peek() {
            if character == '}' {
                chars.next();
                break;
            }
            let Some(digit) = character.to_digit(16) else {
                break;
            };
            value = value.saturating_mul(16).saturating_add(digit);
            any = true;
            chars.next();
        }
        if any && value <= 0x10_FFFF {
            output
                .push_code_point(value)
                .expect("a bounded code point is representable");
        }
        return;
    }

    let mut value = 0_u16;
    let mut count = 0;
    while count < 4 {
        let Some(&character) = chars.peek() else {
            break;
        };
        let Some(digit) = character.to_digit(16) else {
            break;
        };
        value = value * 16 + digit as u16;
        chars.next();
        count += 1;
    }
    if count == 4 {
        output.push_unit(value);
    } else {
        output.push_unit(b'u'.into());
    }
}

#[cfg(test)]
mod tests {
    use super::{cook_escapes, number_value, string_value};

    #[test]
    fn cooks_numbers_and_utf16_strings() {
        assert_eq!(number_value("0x10"), Some(16.0));
        assert_eq!(number_value("1_024"), Some(1024.0));
        assert_eq!(
            string_value("'a\\n'").unwrap().as_units(),
            [b'a'.into(), b'\n'.into()]
        );
        assert_eq!(cook_escapes("\\uD800").as_units(), [0xD800]);
        assert_eq!(cook_escapes("\\u{1F603}").as_units(), [0xD83D, 0xDE03]);
    }
    #[test]
    fn cooks_line_continuations() {
        assert_eq!(
            cook_escapes("a\\\u{2028}b").as_units(),
            [b'a'.into(), b'b'.into()]
        );
        assert_eq!(
            cook_escapes("a\\\u{2029}b").as_units(),
            [b'a'.into(), b'b'.into()]
        );
    }
}
