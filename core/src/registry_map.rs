use core::str::Lines;

pub enum RegistryType {
    Holding,
    Input,
}

#[non_exhaustive]
pub enum RegistryValueType {
    Float32,
    Unsigned8,
    Int8,
    Unsigned16,
    Int16,
    Unsigned32,
    Int32,
    Unsigned64,
    Int64,
    Float64,
}

pub struct RegistryEntry<'a> {
    pub reg_type: RegistryType,
    pub address: u16,
    pub device_id: u8,
    pub reg_value_type: RegistryValueType,
    pub topic: &'a str,
}

impl<'a> RegistryEntry<'a> {
    pub fn parse_from_line(line: &'a str) -> RegistryEntry {
        //1,0,i,f,l1_voltage
        let mut split = line.split(',');

        let device_id = split.next().unwrap().parse::<u8>().unwrap();
        let address = split.next().unwrap().parse::<u16>().unwrap();

        let reg_type = match split.next().unwrap() {
            "h" => RegistryType::Holding,
            "i" => RegistryType::Input,
            _ => panic!("Invalid registry type"),
        };

        let reg_value_type = match split.next().unwrap() {
            "f32" => RegistryValueType::Float32,
            "u8" => RegistryValueType::Unsigned8,
            "i8" => RegistryValueType::Int8,
            "u16" => RegistryValueType::Unsigned16,
            "i16" => RegistryValueType::Int16,
            "u32" => RegistryValueType::Unsigned32,
            "i32" => RegistryValueType::Int32,
            "u64" => RegistryValueType::Unsigned64,
            "i64" => RegistryValueType::Int64,
            "f64" => RegistryValueType::Float64,
            _ => panic!("Invalid registry value type"),
        };

        let topic = split.next().unwrap();

        RegistryEntry {
            reg_type,
            address,
            device_id,
            reg_value_type,
            topic,
        }
    }
}

pub struct RegistryMap<'a> {
    lines: Lines<'a>,
}

impl<'a> RegistryMap<'a> {
    pub fn new(in_memory_map: &'a str) -> RegistryMap {
        let lines = in_memory_map.lines();
        RegistryMap { lines }
    }
}

impl<'a> Iterator for RegistryMap<'a> {
    type Item = RegistryEntry<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(line) = self.lines.next() {
            return Some(RegistryEntry::parse_from_line(line));
        }
        None
    }
}