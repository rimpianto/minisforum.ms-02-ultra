// CIM enums and structs
#[derive(Debug, Clone, Copy)]
pub enum PowerState {
    Unknown = 0,
    FullPower = 1,
    On = 2,
    SleepLight = 3,
    SleepDeep = 4,
    PowerCycle = 5,
    OffHard = 6,
    Hibernate = 7,
    // ... other states as needed
}

impl PowerState {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "on" => Some(PowerState::On),
            "off" => Some(PowerState::OffHard),
            "reset" => Some(PowerState::PowerCycle),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub enum BootSource {
    HardDisk,
    Network,
    CdDvd,
    // ...
}
