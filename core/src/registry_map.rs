use core::str::Lines;

pub enum RegistryType {
    Holding,
    Input,
}

#[non_exhaustive]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
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
    /// Parses a line like `1,0,i,f32,l1_voltage`, `None` if it is malformed
    pub fn try_parse(line: &'a str) -> Option<RegistryEntry<'a>> {
        let mut split = line.split(',');

        let device_id = split.next()?.trim().parse::<u8>().ok()?;
        let address = split.next()?.trim().parse::<u16>().ok()?;

        let reg_type = match split.next()?.trim() {
            "h" => RegistryType::Holding,
            "i" => RegistryType::Input,
            _ => return None,
        };

        let reg_value_type = match split.next()?.trim() {
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
            _ => return None,
        };

        let topic = split.next()?.trim();
        if topic.is_empty() {
            return None;
        }

        Some(RegistryEntry {
            reg_type,
            address,
            device_id,
            reg_value_type,
            topic,
        })
    }

    /// Like [`try_parse`](Self::try_parse) for maps that are known to be valid (compiled in or
    /// validated before being stored). Panics on a malformed line.
    pub fn parse_from_line(line: &'a str) -> RegistryEntry<'a> {
        Self::try_parse(line).expect("invalid registry map line")
    }
}

pub struct RegistryMap<'a> {
    lines: Lines<'a>,
}

impl<'a> RegistryMap<'a> {
    pub fn new(in_memory_map: &'a str) -> RegistryMap<'a> {
        let lines = in_memory_map.lines();
        RegistryMap { lines }
    }
}

impl RegistryMap<'_> {
    /// Checks every line of `map` without panicking. Returns the number of entries, or the
    /// 1-based number of the first malformed line.
    pub fn validate(map: &str) -> Result<usize, usize> {
        let mut entries = 0;
        for (index, line) in map.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            RegistryEntry::try_parse(line).ok_or(index + 1)?;
            entries += 1;
        }
        Ok(entries)
    }
}

impl<'a> Iterator for RegistryMap<'a> {
    type Item = RegistryEntry<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        // Blank lines are allowed, `validate` accepts them too
        self.lines
            .by_ref()
            .find(|line| !line.trim().is_empty())
            .map(RegistryEntry::parse_from_line)
    }
}
