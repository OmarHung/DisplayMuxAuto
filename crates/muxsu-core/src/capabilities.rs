use crate::DisplayInput;

/// Parses the value list declared for VCP 0x60 without probing inputs by switching them.
pub(crate) fn parse_input_sources(capabilities: &[u8]) -> Vec<DisplayInput> {
    let text = String::from_utf8_lossy(capabilities);
    let Some(vcp_body) = group_body_after_token(&text, "vcp") else {
        return Vec::new();
    };
    let Some(input_body) = group_body_after_token(vcp_body, "60") else {
        return Vec::new();
    };

    let mut inputs = input_body
        .split_ascii_whitespace()
        .filter_map(|token| {
            let token = token.trim_matches(|character: char| !character.is_ascii_hexdigit());
            if token.is_empty() || token.len() > 2 {
                return None;
            }
            u32::from_str_radix(token, 16)
                .ok()
                .and_then(|value| DisplayInput::new(value).ok())
        })
        .collect::<Vec<_>>();
    inputs.sort_by_key(|input| input.value());
    inputs.dedup();
    inputs
}

fn group_body_after_token<'a>(text: &'a str, token: &str) -> Option<&'a str> {
    let bytes = text.as_bytes();
    let mut offset = 0;
    while offset + token.len() <= bytes.len() {
        let candidate = &text[offset..offset + token.len()];
        if candidate.eq_ignore_ascii_case(token)
            && is_token_boundary(bytes.get(offset.wrapping_sub(1)).copied())
            && is_token_boundary(bytes.get(offset + token.len()).copied())
        {
            let suffix = &text[offset + token.len()..];
            let whitespace = suffix.len() - suffix.trim_start_matches(char::is_whitespace).len();
            if suffix.as_bytes().get(whitespace) != Some(&b'(') {
                offset += token.len();
                continue;
            }
            let open = offset + token.len() + whitespace;
            let mut depth = 0_u32;
            for (relative, character) in text[open..].char_indices() {
                match character {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(&text[open + 1..open + relative]);
                        }
                    }
                    _ => {}
                }
            }
            return None;
        }
        offset += 1;
    }
    None
}

fn is_token_boundary(character: Option<u8>) -> bool {
    character.is_none_or(|value| !value.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_and_vendor_specific_input_values() {
        let parsed = parse_input_sources(
            b"(prot(monitor)type(LCD)model(X)vcp(10 12 60(0f 11 1b 1B) D6(01 04)))",
        );
        assert_eq!(
            parsed.iter().map(|input| input.value()).collect::<Vec<_>>(),
            vec![0x0f, 0x11, 0x1b]
        );
    }

    #[test]
    fn does_not_confuse_other_feature_values_with_inputs() {
        assert!(parse_input_sources(b"(vcp(10 12(60) D6(01 04)))").is_empty());
        assert!(parse_input_sources(b"(type(LCD))").is_empty());
    }

    #[test]
    fn accepts_uppercase_and_irregular_spacing() {
        let parsed = parse_input_sources(b"(VCP( 60 ( 0F\n10\tE0 ) ))");
        assert_eq!(
            parsed.iter().map(|input| input.value()).collect::<Vec<_>>(),
            vec![0x0f, 0x10, 0xe0]
        );
    }
}
