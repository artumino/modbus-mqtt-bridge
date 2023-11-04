use core::str::Lines;

pub enum RegistryType {
    Holding,
    Input
}

pub enum RegistryValueType {
    Float
}

pub struct RegistryEntry {
    reg_type: RegistryType,
    address: u16,
    reg_value_type: RegistryValueType
}

impl RegistryEntry {
    pub fn parse_from_line(line: &str) -> RegistryEntry {
        let mut split = line.split(',');

        let reg_type = match split.next().unwrap() {
            "h" => RegistryType::Holding,
            "i" => RegistryType::Input,
            _ => panic!("Invalid registry type")
        };

        let address = split.next().unwrap().parse::<u16>().unwrap();

        let reg_value_type = match split.next().unwrap() {
            "f" => RegistryValueType::Float,
            _ => panic!("Invalid registry value type")
        };

        RegistryEntry {
            reg_type,
            address,
            reg_value_type
        }
    }
}

pub struct RegistryMap {
    lines: Lines<'static>
}

impl RegistryMap {
    pub fn new(in_memory_map: &'static str) -> RegistryMap {
        let lines = in_memory_map.lines();
        RegistryMap {
            lines
        }
    }
}

impl Iterator for RegistryMap {
    type Item = RegistryEntry;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(line) = self.lines.next() {
            return Some(RegistryEntry::parse_from_line(line))
        }
        None
    }
}