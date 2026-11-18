use openloop_openxr_abi::XrVersion;

pub const fn make(major: u64, minor: u64, patch: u64) -> XrVersion {
    (major << 48) | ((minor & 0xffff) << 32) | (patch & 0xffff_ffff)
}

pub const fn major(v: XrVersion) -> u64 {
    v >> 48
}

pub const fn minor(v: XrVersion) -> u64 {
    (v >> 32) & 0xffff
}

/// Major and minor only: a runtime implementing 1.1 accepts any 1.1.x request.
pub const fn major_minor(v: XrVersion) -> XrVersion {
    make(major(v), minor(v), 0)
}

pub fn runtime() -> XrVersion {
    let part = |s: &str| s.parse().unwrap_or(0);
    make(
        part(env!("CARGO_PKG_VERSION_MAJOR")),
        part(env!("CARGO_PKG_VERSION_MINOR")),
        part(env!("CARGO_PKG_VERSION_PATCH")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use openloop_openxr_abi::{XR_API_VERSION_1_0, XR_API_VERSION_1_1, XR_CURRENT_API_VERSION};

    #[test]
    fn matches_the_headers_encoding() {
        assert_eq!(
            major_minor(XR_CURRENT_API_VERSION),
            major_minor(XR_API_VERSION_1_1)
        );
        assert_eq!(major(XR_API_VERSION_1_0), 1);
        assert_eq!(minor(XR_API_VERSION_1_0), 0);
        assert_eq!(minor(XR_API_VERSION_1_1), 1);
    }
}
