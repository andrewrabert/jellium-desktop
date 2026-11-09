//! Hwdec mode policy: which mpv hardware-decode backends each OS offers.

pub const HWDEC_DEFAULT: &str = "no";

#[expect(
    dead_code,
    reason = "every OS row stays compiled; only CURRENT_OS's variant is constructed"
)]
enum TargetOs {
    Linux,
    Windows,
    Macos,
}

#[cfg(target_os = "linux")]
const CURRENT_OS: TargetOs = TargetOs::Linux;
#[cfg(target_os = "windows")]
const CURRENT_OS: TargetOs = TargetOs::Windows;
#[cfg(target_os = "macos")]
const CURRENT_OS: TargetOs = TargetOs::Macos;

pub fn hwdec_options() -> &'static [&'static str] {
    match CURRENT_OS {
        TargetOs::Linux => &["auto", "no", "vaapi", "nvdec", "vulkan"],
        TargetOs::Windows => &["auto", "no", "d3d11va", "nvdec", "vulkan"],
        TargetOs::Macos => &["auto", "no", "videotoolbox", "vulkan"],
    }
}

/// A hardware-decode mode this OS offers, parsed once from user input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hwdec(&'static str);

impl Hwdec {
    /// The mpv `hwdec` option value.
    pub fn as_str(self) -> &'static str {
        self.0
    }
}

impl Default for Hwdec {
    fn default() -> Self {
        Hwdec(HWDEC_DEFAULT)
    }
}

impl serde::Serialize for Hwdec {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0)
    }
}

impl<'de> serde::Deserialize<'de> for Hwdec {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <&str>::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("'{value}' is not a hardware-decode mode this OS offers")]
pub struct UnknownHwdec {
    value: String,
}

impl std::str::FromStr for Hwdec {
    type Err = UnknownHwdec;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        hwdec_options()
            .iter()
            .find(|option| **option == value)
            .map(|option| Hwdec(option))
            .ok_or_else(|| UnknownHwdec {
                value: value.to_owned(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_always_valid() {
        assert_eq!(
            "auto".parse::<Hwdec>().ok().map(Hwdec::as_str),
            Some("auto")
        );
        assert_eq!("no".parse::<Hwdec>().ok().map(Hwdec::as_str), Some("no"));
        assert_eq!(HWDEC_DEFAULT.parse::<Hwdec>().ok(), Some(Hwdec::default()));
    }

    /// mpv's documented `--hwdec` default: "no: always use software decoding
    /// (default)". The settings view shows this value for an unset setting.
    #[test]
    fn default_is_mpv_software_decoding() {
        assert_eq!(Hwdec::default().as_str(), "no");
    }

    #[test]
    fn rejects_garbage() {
        let error = "".parse::<Hwdec>().unwrap_err();
        assert_eq!(
            error.to_string(),
            "'' is not a hardware-decode mode this OS offers"
        );
        let error = "garbage".parse::<Hwdec>().unwrap_err();
        assert_eq!(
            error.to_string(),
            "'garbage' is not a hardware-decode mode this OS offers"
        );
    }
}
