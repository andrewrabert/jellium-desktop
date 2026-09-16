pub fn parse(s: &str) -> u32 {
    jfn_color::parse_mpv(s.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_returns_zero() {
        assert_eq!(parse(""), 0);
    }

    #[test]
    fn forwards_to_jfn_color() {
        assert_eq!(parse("#ff112233"), jfn_color::parse_mpv(b"#ff112233"));
    }
}
