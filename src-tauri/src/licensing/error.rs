use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LicenseError {
    InvalidEngine(String),
    Locked(String),
    DeviceIdentityLost,
    Storage(String),
    Network(String),
    Unauthorized,
    DeviceConflict,
    SubscriptionExpired,
    Revoked,
    InvalidLease(String),
    InvalidTrialToken(String),
    ClockRollback,
    InvalidLicenseKey,
    Configuration(String),
}

impl fmt::Display for LicenseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEngine(id) => write!(f, "engine lisensi tidak dikenal: {id}"),
            Self::Locked(message) => f.write_str(message),
            Self::DeviceIdentityLost => f.write_str("identitas perangkat tidak dapat dipulihkan"),
            Self::Storage(message) => write!(f, "penyimpanan lisensi gagal: {message}"),
            Self::Network(message) => write!(f, "layanan lisensi tidak tersedia: {message}"),
            Self::Unauthorized => f.write_str("license key tidak valid"),
            Self::DeviceConflict => f.write_str("lisensi sudah terikat ke perangkat lain"),
            Self::SubscriptionExpired => f.write_str("langganan lisensi sudah berakhir"),
            Self::Revoked => f.write_str("lisensi telah dicabut"),
            Self::InvalidLease(message) => write!(f, "lease lisensi tidak valid: {message}"),
            Self::InvalidTrialToken(message) => write!(f, "token trial tidak valid: {message}"),
            Self::ClockRollback => f.write_str("jam perangkat mundur dari waktu server tepercaya"),
            Self::InvalidLicenseKey => f.write_str("masukkan license key yang valid"),
            Self::Configuration(message) => write!(f, "konfigurasi lisensi tidak valid: {message}"),
        }
    }
}

impl std::error::Error for LicenseError {}
