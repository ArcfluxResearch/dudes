//! The models Pi lists for the profile it runs under.

/// The argument that makes Pi print its model table and exit.
pub const LIST_ARGS: &[&str] = &["--list-models"];

/// The `provider/model` ids in the table `pi --list-models` prints, in its order.
///
/// The table has a header row and one row per model, columns aligned with spaces. The id is
/// qualified by its provider because the same model name can be listed under several providers,
/// and a bare name that is ambiguous makes Pi refuse it. Only the lines after the header count, so
/// a message Pi prints instead of a table lists nothing, and a line with fewer than two columns is
/// skipped.
#[must_use]
pub fn parse_list(stdout: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .skip_while(|line| {
            let mut columns = line.split_whitespace();
            !(columns.next() == Some("provider") && columns.next() == Some("model"))
        })
        .skip(1)
        .filter_map(|line| {
            let mut columns = line.split_whitespace();
            Some(format!("{}/{}", columns.next()?, columns.next()?))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_become_provider_qualified_ids_and_the_header_is_skipped() {
        let table = "\
provider      model          context  max-out  thinking  images
openai        gpt-5.1        400K     128K     yes       yes
openai-codex  gpt-5.1        400K     128K     yes       yes
openai        gpt-5.1-mini   400K     128K     no        yes

openai        o3-pro         200K     100K     yes       no
lonely
";
        assert_eq!(
            parse_list(table.as_bytes()),
            [
                "openai/gpt-5.1",
                "openai-codex/gpt-5.1",
                "openai/gpt-5.1-mini",
                "openai/o3-pro"
            ]
        );
        assert!(parse_list(b"").is_empty());
        assert!(parse_list(b"No models available. Run pi to sign in.\n").is_empty());
        assert!(parse_list(b"provider model context\n").is_empty());
    }
}
