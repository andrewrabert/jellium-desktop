#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WlPaintOverride {
    Dmabuf,
    Gpu,
    Shm,
}
