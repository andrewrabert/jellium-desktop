//! Which paint path to use, as requested on the command line.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WlPaintOverride {
    Dmabuf,
    Gpu,
    Shm,
}
