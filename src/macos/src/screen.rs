use jfn_platform_abi::ScreenName;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplaySerialNumber(pub u32);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenIdentity {
    name: String,
    serial_number: DisplaySerialNumber,
}

impl ScreenIdentity {
    pub fn parse(localized_name: &str, serial_number: DisplaySerialNumber) -> ScreenIdentity {
        ScreenIdentity {
            name: without_ordinal(localized_name)
                .unwrap_or(localized_name)
                .to_owned(),
            serial_number,
        }
    }

    pub fn unique_name(&self) -> Option<ScreenName> {
        ScreenName::new(format!("{} ({})", self.name, self.serial_number.0))
    }
}

fn without_ordinal(localized_name: &str) -> Option<&str> {
    let (name, suffix) = localized_name.rsplit_once(" (")?;
    let digits = suffix.strip_suffix(')')?;
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique(localized_name: &str, serial_number: u32) -> Option<String> {
        ScreenIdentity::parse(localized_name, DisplaySerialNumber(serial_number))
            .unique_name()
            .map(|n| n.as_str().to_owned())
    }

    #[test]
    fn the_unique_name_appends_the_serial_number() {
        assert_eq!(
            unique("Built-in Retina Display", 0),
            Some("Built-in Retina Display (0)".to_owned())
        );
        assert_eq!(
            unique("DELL U2720Q", 12345),
            Some("DELL U2720Q (12345)".to_owned())
        );
    }

    #[test]
    fn a_trailing_numbered_suffix_is_stripped_first() {
        assert_eq!(
            unique("DELL U2720Q (2)", 777),
            Some("DELL U2720Q (777)".to_owned())
        );
        assert_eq!(unique("LG (HDR) (10)", 5), Some("LG (HDR) (5)".to_owned()));
    }

    #[test]
    fn a_suffix_that_is_not_a_number_is_kept() {
        assert_eq!(unique("LG (HDR)", 5), Some("LG (HDR) (5)".to_owned()));
        assert_eq!(unique("LG ()", 5), Some("LG () (5)".to_owned()));
        assert_eq!(unique("LG (+2)", 5), Some("LG (+2) (5)".to_owned()));
        assert_eq!(
            unique("LG (2) extra", 5),
            Some("LG (2) extra (5)".to_owned())
        );
    }
}
