pub fn crc32(data: &[u8]) -> u32 {
    crc32fast::hash(data)
}
