pub const PAGE_SIZE: usize = 0x1000;

pub fn pages(size: usize) -> usize {
    size.div_ceil(PAGE_SIZE)
}
