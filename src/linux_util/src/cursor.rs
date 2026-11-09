//! CEF cursor shape → freedesktop cursor icon, shared by the Wayland and X11
//! pointer paths.

use cursor_icon::CursorIcon;
use jfn_platform_abi::cursor::CursorShape;

/// The named cursor a fixed CEF shape maps to; unmapped shapes fall back to
/// the default arrow.
pub fn icon_for(shape: CursorShape) -> CursorIcon {
    use CursorShape::*;
    match shape {
        Cross => CursorIcon::Crosshair,
        Hand => CursorIcon::Pointer,
        IBeam => CursorIcon::Text,
        Wait => CursorIcon::Wait,
        Help => CursorIcon::Help,
        EastResize => CursorIcon::EResize,
        NorthResize => CursorIcon::NResize,
        NorthEastResize => CursorIcon::NeResize,
        NorthWestResize => CursorIcon::NwResize,
        SouthResize => CursorIcon::SResize,
        SouthEastResize => CursorIcon::SeResize,
        SouthWestResize => CursorIcon::SwResize,
        WestResize => CursorIcon::WResize,
        NorthSouthResize => CursorIcon::NsResize,
        EastWestResize => CursorIcon::EwResize,
        NorthEastSouthWestResize => CursorIcon::NeswResize,
        NorthWestSouthEastResize => CursorIcon::NwseResize,
        ColumnResize => CursorIcon::ColResize,
        RowResize => CursorIcon::RowResize,
        Move => CursorIcon::Move,
        VerticalText => CursorIcon::VerticalText,
        Cell => CursorIcon::Cell,
        ContextMenu => CursorIcon::ContextMenu,
        Alias => CursorIcon::Alias,
        Progress => CursorIcon::Progress,
        NoDrop => CursorIcon::NoDrop,
        Copy => CursorIcon::Copy,
        NotAllowed => CursorIcon::NotAllowed,
        ZoomIn => CursorIcon::ZoomIn,
        ZoomOut => CursorIcon::ZoomOut,
        Grab => CursorIcon::Grab,
        Grabbing => CursorIcon::Grabbing,
        MiddlePanning | MiddlePanningVertical | MiddlePanningHorizontal => CursorIcon::AllScroll,
        _ => CursorIcon::Default,
    }
}
