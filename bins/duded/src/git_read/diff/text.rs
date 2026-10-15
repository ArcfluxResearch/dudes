pub(super) const POLICY_VERSION: u32 = 1;

const SIDE_BYTES: usize = 16 * 1024;
const LINE_BYTES: usize = 4 * 1024;
const CREDENTIAL_KEYS: &[&[u8]] = &[
    b"api_key",
    b"api-key",
    b"apikey",
    b"access_token",
    b"access-token",
    b"accesstoken",
    b"client_secret",
    b"client-secret",
    b"clientsecret",
    b"password",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TextRefusal {
    OversizeFile,
    OversizeLine,
    Binary,
    UnsupportedText,
    SensitiveContent,
}

pub(super) fn policy(bytes: &[u8]) -> Result<&str, TextRefusal> {
    if bytes.len() > SIDE_BYTES {
        return Err(TextRefusal::OversizeFile);
    }
    if bytes.contains(&0) {
        return Err(TextRefusal::Binary);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| TextRefusal::UnsupportedText)?;
    if text.chars().any(|c| {
        !matches!(c, '\t' | '\n' | '\r') && (c.is_control() || super::super::text::is_format(c))
    }) {
        return Err(TextRefusal::UnsupportedText);
    }
    if bytes
        .split(|&byte| byte == b'\n')
        .any(|line| line.len() > LINE_BYTES)
    {
        return Err(TextRefusal::OversizeLine);
    }
    if contains_secret(bytes) {
        return Err(TextRefusal::SensitiveContent);
    }
    Ok(text)
}

pub(super) fn sensitive_path(path: &str) -> bool {
    let mut parts = path.split('/').peekable();
    while let Some(part) = parts.next() {
        if part.eq_ignore_ascii_case(".env")
            || part
                .get(..5)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(".env."))
            || parts.peek().is_some()
                && (part.eq_ignore_ascii_case(".ssh") || part.eq_ignore_ascii_case(".aws"))
        {
            return true;
        }
        if parts.peek().is_none()
            && part.rsplit_once('.').is_some_and(|(_, suffix)| {
                ["pem", "key", "p12", "pfx"]
                    .iter()
                    .any(|ext| suffix.eq_ignore_ascii_case(ext))
            })
        {
            return true;
        }
    }
    false
}

fn contains_secret(bytes: &[u8]) -> bool {
    (0..bytes.len()).any(|start| {
        private_key(&bytes[start..]) || token(bytes, start) || credential_value(bytes, start)
    })
}

fn private_key(bytes: &[u8]) -> bool {
    let Some(rest) = bytes.strip_prefix(b"-----BEGIN ") else {
        return false;
    };
    let end = rest
        .iter()
        .position(|byte| !byte.is_ascii_uppercase() && !byte.is_ascii_digit() && *byte != b' ')
        .unwrap_or(rest.len());
    rest[..end].ends_with(b"PRIVATE KEY") && rest[end..].starts_with(b"-----")
}

fn word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn token(bytes: &[u8], start: usize) -> bool {
    if start > 0 && word(bytes[start - 1]) {
        return false;
    }
    let rest = &bytes[start..];
    if rest.starts_with(b"AKIA") || rest.starts_with(b"ASIA") {
        return rest.get(4..20).is_some_and(|suffix| {
            suffix
                .iter()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        }) && rest.get(20).is_none_or(|&byte| !word(byte));
    }
    let (prefix_len, allow_hyphen) = if rest.starts_with(b"sk-") {
        (3, true)
    } else if rest.starts_with(b"gh")
        && rest.get(2).is_some_and(|kind| b"pousr".contains(kind))
        && rest.get(3) == Some(&b'_')
    {
        (4, false)
    } else {
        return false;
    };
    let suffix = &rest[prefix_len..];
    let length = suffix
        .iter()
        .take(256)
        .take_while(|&&byte| {
            byte.is_ascii_alphanumeric() || allow_hyphen && matches!(byte, b'_' | b'-')
        })
        .count();
    (20..=255).contains(&length) && suffix.get(length).is_none_or(|&byte| !word(byte))
}

fn credential_value(bytes: &[u8], start: usize) -> bool {
    if start > 0 && (word(bytes[start - 1]) || matches!(bytes[start - 1], b'\'' | b'"')) {
        return false;
    }
    let mut rest = &bytes[start..];
    let quote = rest
        .first()
        .copied()
        .filter(|byte| matches!(byte, b'\'' | b'"'));
    if quote.is_some() {
        rest = &rest[1..];
    }
    let Some(key) = CREDENTIAL_KEYS.iter().find(|key| {
        rest.get(..key.len())
            .is_some_and(|value| value.eq_ignore_ascii_case(key))
    }) else {
        return false;
    };
    rest = &rest[key.len()..];
    if let Some(quote) = quote {
        if rest.first() != Some(&quote) {
            return false;
        }
        rest = &rest[1..];
    }
    rest = horizontal_space(rest);
    if !matches!(rest.first(), Some(b':' | b'=')) {
        return false;
    }
    rest = horizontal_space(&rest[1..]);
    let Some(&quote @ (b'\'' | b'"')) = rest.first() else {
        return false;
    };
    rest = &rest[1..];
    let Some(end) = rest
        .iter()
        .position(|&byte| byte == quote || matches!(byte, b'\r' | b'\n'))
    else {
        return false;
    };
    rest[end] == quote
        && std::str::from_utf8(&rest[..end]).is_ok_and(|value| value.chars().take(8).count() == 8)
}

fn horizontal_space(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t'))
        .unwrap_or(bytes.len());
    &bytes[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    const _: () = assert!(POLICY_VERSION == 1);

    fn eligible(text: &str) {
        assert!(
            policy(text.as_bytes()) == Ok(text),
            "eligible source changed or refused"
        );
    }

    fn refused(bytes: &[u8], reason: TextRefusal) {
        assert!(policy(bytes) == Err(reason), "wrong whole-side refusal");
    }

    #[test]
    fn v1_preserves_source_unicode_and_literal_markup() {
        eligible("let 日本語 = \"é💫\";\n\tvalue\r\nline\u{2028}separator\u{2029}end");
        eligible("<script src=\"https://example.test/a\">/tmp/source</script>");
        eligible("");
    }

    #[test]
    fn v1_refuses_the_whole_oversized_side() {
        let exact = format!("{}\n", "x".repeat(4095)).repeat(4);
        let oversize = format!("{exact}x");
        refused(oversize.as_bytes(), TextRefusal::OversizeFile);
        eligible(&exact);
    }

    #[test]
    fn v1_refuses_the_whole_oversized_line_by_utf8_bytes() {
        refused("x".repeat(4097).as_bytes(), TextRefusal::OversizeLine);
        refused("é".repeat(2049).as_bytes(), TextRefusal::OversizeLine);
        eligible(&"x".repeat(4096));
        eligible(&"é".repeat(2048));
        eligible(&format!("{}\n{}", "x".repeat(4096), "y".repeat(4096)));
    }

    #[test]
    fn v1_counts_carriage_return_but_not_lf_in_line_bytes() {
        refused(
            format!("{}\r\n", "x".repeat(4096)).as_bytes(),
            TextRefusal::OversizeLine,
        );
        eligible(&format!("{}\r\n", "x".repeat(4095)));
    }

    #[test]
    fn v1_refuses_invalid_utf8_without_replacement() {
        for bytes in [&[0xff][..], &[0xc0, 0xaf], &[0xf0, 0x9f, 0x92]] {
            refused(bytes, TextRefusal::UnsupportedText);
        }
        eligible("valid � replacement character written by the source");
    }

    #[test]
    fn v1_refuses_nul_as_binary() {
        refused(b"before\0after", TextRefusal::Binary);
        eligible("before\\0after");
    }

    #[test]
    fn v1_refuses_cc_except_tab_lf_and_cr() {
        for code in (1..=31)
            .chain(127..=159)
            .filter(|code| ![9, 10, 13].contains(code))
        {
            let text = format!("before{}after", char::from_u32(code).unwrap());
            refused(text.as_bytes(), TextRefusal::UnsupportedText);
        }
        eligible("tab\there\nline\rreturn");
    }

    #[test]
    fn v1_refuses_unicode_format_categories() {
        for code in [
            0x00ad, 0x0600, 0x0605, 0x061c, 0x06dd, 0x070f, 0x0890, 0x0891, 0x08e2, 0x180e, 0x200b,
            0x200f, 0x202a, 0x202e, 0x2060, 0x2064, 0x2066, 0x206f, 0xfeff, 0xfff9, 0xfffb,
            0x110bd, 0x110cd, 0x13430, 0x1343f, 0x1bca0, 0x1bca3, 0x1d173, 0x1d17a, 0xe0001,
            0xe0020, 0xe007f,
        ] {
            let text = format!("before{}after", char::from_u32(code).unwrap());
            refused(text.as_bytes(), TextRefusal::UnsupportedText);
        }
        eligible("ordinary 日本語 and combining e\u{301}");
    }

    #[test]
    fn v1_refuses_sensitive_path_names_case_insensitively() {
        for path in [
            ".env",
            ".ENV.local",
            "src/.env.",
            "src/.EnV.example",
            ".env/source.rs",
            "src/.env.local/source.rs",
            ".ssh/config",
            "src/.SSH/id",
            ".aws/credentials",
            "src/.AwS/config",
            "cert.pem",
            "src/cert.KEY",
            "cert.P12",
            "cert.pFx",
        ] {
            assert!(sensitive_path(path), "sensitive path admitted");
        }
        for path in [
            ".env-example",
            ".environment",
            "env",
            "src/public.key.rs",
            "src/pem",
            "src/.ssh-config",
            "src/.aws-config",
            "src/normal.rs",
        ] {
            assert!(!sensitive_path(path), "ordinary path refused");
        }
    }

    #[test]
    fn v1_refuses_literal_private_key_delimiters() {
        for label in ["", "RSA ", "EC ", "OPENSSH ", "123 ", "  "] {
            let text = format!("source\n-----BEGIN {label}PRIVATE KEY-----\nremaining");
            refused(text.as_bytes(), TextRefusal::SensitiveContent);
        }
        for text in [
            "-----BEGIN PUBLIC KEY-----",
            "-----BEGIN private key-----",
            "-----BEGIN RSA_PRIVATE KEY-----",
            "-----BEGIN RSA PRIVATE KEY----",
            "-----BEGIN RSA\nPRIVATE KEY-----",
        ] {
            eligible(text);
        }
    }

    #[test]
    fn v1_refuses_bounded_access_key_words() {
        for prefix in ["AKIA", "ASIA"] {
            let token = format!("{prefix}{}", "A1".repeat(8));
            refused(
                format!("token ({token})").as_bytes(),
                TextRefusal::SensitiveContent,
            );
            for text in [
                format!("x{token}"),
                format!("{token}x"),
                format!("{token}_"),
                format!("{prefix}{}", "A".repeat(15)),
                format!("{prefix}{}", "A".repeat(17)),
                format!("{prefix}{}", "a".repeat(16)),
            ] {
                eligible(&text);
            }
        }
    }

    #[test]
    fn v1_refuses_each_bounded_host_token_shape() {
        for kind in ['p', 'o', 'u', 's', 'r'] {
            for length in [20, 255] {
                let token = format!("gh{kind}_{}", "a".repeat(length));
                refused(
                    format!("[{token}]").as_bytes(),
                    TextRefusal::SensitiveContent,
                );
            }
        }
        for text in [
            format!("ghp_{}", "a".repeat(19)),
            format!("ghp_{}", "a".repeat(256)),
            format!("ghn_{}", "a".repeat(20)),
            format!("GHP_{}", "a".repeat(20)),
            format!("xghp_{}", "a".repeat(20)),
            format!("ghp_{}_", "a".repeat(20)),
        ] {
            eligible(&text);
        }
    }

    #[test]
    fn v1_refuses_bounded_prefixed_tokens() {
        for suffix in ["a".repeat(20), "a".repeat(255), "A1_-".repeat(5)] {
            refused(
                format!("(sk-{suffix})").as_bytes(),
                TextRefusal::SensitiveContent,
            );
        }
        for text in [
            format!("sk-{}", "a".repeat(19)),
            format!("sk-{}", "a".repeat(256)),
            format!("xsk-{}", "a".repeat(20)),
            format!("SK-{}", "a".repeat(20)),
            format!("sk-{}.{}", "a".repeat(10), "a".repeat(10)),
        ] {
            eligible(&text);
        }
    }

    #[test]
    fn v1_refuses_quoted_credential_values() {
        for key in [
            "api_key",
            "API-KEY",
            "ApiKey",
            "access_token",
            "ACCESS-TOKEN",
            "AccessToken",
            "client_secret",
            "CLIENT-SECRET",
            "ClientSecret",
            "PASSWORD",
        ] {
            for separator in [':', '='] {
                for quote in ['\'', '"'] {
                    let text =
                        format!("{quote}{key}{quote} \t{separator} \t{quote}abcdefgh{quote}");
                    refused(text.as_bytes(), TextRefusal::SensitiveContent);
                }
            }
        }
        refused(
            "api_key = '日本語四五六七八'".as_bytes(),
            TextRefusal::SensitiveContent,
        );
    }

    #[test]
    fn v1_credential_shapes_keep_literal_boundaries() {
        for text in [
            "api_key = 'abcdefg'",
            "api_key = '日本語四五六七'",
            "api_key = abcdefgh",
            "api_key = 'abcd\nefgh'",
            "api_key = 'abcd\refgh'",
            "api_key\n= 'abcdefgh'",
            "my_api_key = 'abcdefgh'",
            "password_hint = 'abcdefgh'",
            "passwordish = 'abcdefgh'",
            "password = 'abcdefgh\"",
            "password => 'abcdefgh'",
            "'password\" = 'abcdefgh'",
        ] {
            eligible(text);
        }
        refused(b"password = 'abcdefgh'", TextRefusal::SensitiveContent);
    }

    #[test]
    fn v1_false_positive_fixtures_are_refused_whole() {
        let token = format!("ghp_{}", "X".repeat(20));
        refused(
            format!("test token = \"{token}\"").as_bytes(),
            TextRefusal::SensitiveContent,
        );
        refused(b"password = 'example-only'", TextRefusal::SensitiveContent);
    }

    #[test]
    fn v1_intentionally_misses_a_novel_secret_shape() {
        eligible("nebula_credential = 'novel-example-value'\ncustom-token ZX.1234567890.abcdef");
    }
}
