//! The `CLIPBOARD` and `PRIMARY` selections this client owns and serves.
//!
//! Both run on the input thread's connection, from a dedicated `InputOnly`
//! window: the selection events an owner receives are delivered without an
//! event mask, and the input thread is the only one polling that connection.

use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use xcb::{Xid, XidNew, x};

use jfn_platform_abi::OnText;

use crate::x11_state::Atoms;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Clipboard,
    Primary,
}

impl Kind {
    fn atom(self, atoms: &Atoms) -> x::Atom {
        let raw = match self {
            Kind::Clipboard => atoms.clipboard,
            Kind::Primary => atoms.primary,
        };
        x::Atom::new(raw)
    }

    fn slot(self) -> usize {
        match self {
            Kind::Clipboard => 0,
            Kind::Primary => 1,
        }
    }
}

struct Pending {
    on_done: OnText,
}

/// The `CLIPBOARD` and `PRIMARY` selections this client owns and serves.
pub(crate) struct Selections {
    conn: Arc<xcb::Connection>,
    owner: x::Window,
    atoms: Atoms,
    /// The text this client offers for each selection, indexed by
    /// [`Kind::slot`]; `None` once another client took the selection.
    stored: Mutex<[Option<String>; 2]>,
    pending: Mutex<Option<Pending>>,
}

static SELECTIONS: OnceLock<Selections> = OnceLock::new();

/// The served selections, `None` until the input thread created them.
pub(crate) fn selections() -> Option<&'static Selections> {
    SELECTIONS.get()
}

/// Creates the selections once, on the input thread's connection.
pub(crate) fn install(conn: &Arc<xcb::Connection>, screen_num: i32) {
    let Some(selections) = Selections::new(conn, screen_num) else {
        return;
    };
    drop(SELECTIONS.set(selections));
}

impl Selections {
    /// Creates the selection-owner window on `conn`.
    pub(crate) fn new(conn: &Arc<xcb::Connection>, screen_num: i32) -> Option<Selections> {
        let atoms = crate::x11_state::host()?.atoms;
        let setup = conn.get_setup();
        let screen = setup.roots().nth(screen_num.max(0) as usize)?;
        let owner: x::Window = conn.generate_id();
        let cookie = conn.send_request_checked(&x::CreateWindow {
            depth: x::COPY_FROM_PARENT as u8,
            wid: owner,
            parent: screen.root(),
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            border_width: 0,
            class: x::WindowClass::InputOnly,
            visual: x::COPY_FROM_PARENT,
            value_list: &[],
        });
        conn.check_request(cookie).ok()?;
        Some(Selections {
            conn: Arc::clone(conn),
            owner,
            atoms,
            stored: Mutex::new([None, None]),
            pending: Mutex::new(None),
        })
    }

    /// Stores `text` and takes the selection.
    ///
    /// A `SetSelectionOwner` the server did not honour leaves the previous
    /// owner and the previous contents.
    pub(crate) fn write_text(&self, kind: Kind, text: &str) {
        let selection = kind.atom(&self.atoms);
        self.conn.send_request(&x::SetSelectionOwner {
            owner: self.owner,
            selection,
            time: x::CURRENT_TIME,
        });
        let cookie = self.conn.send_request(&x::GetSelectionOwner { selection });
        drop(self.conn.flush());
        let Ok(reply) = self.conn.wait_for_reply(cookie) else {
            return;
        };
        if reply.owner() != self.owner {
            return;
        }
        self.stored.lock()[kind.slot()] = Some(text.to_owned());
    }

    /// Converts the selection to `UTF8_STRING`.
    ///
    /// `on_done` fires with `None` for an unowned selection, a refused
    /// conversion, and an `INCR` reply.
    /// A second read supersedes the first, resolving it with `None`.
    pub(crate) fn read_text_async(&self, kind: Kind, on_done: OnText) {
        let selection = kind.atom(&self.atoms);
        let cookie = self.conn.send_request(&x::GetSelectionOwner { selection });
        drop(self.conn.flush());
        let owned = self
            .conn
            .wait_for_reply(cookie)
            .is_ok_and(|reply| reply.owner() != x::Window::none());
        if !owned {
            on_done(None);
            return;
        }
        let superseded = self.pending.lock().replace(Pending { on_done });
        if let Some(superseded) = superseded {
            (superseded.on_done)(None);
        }
        self.conn.send_request(&x::ConvertSelection {
            requestor: self.owner,
            selection,
            target: x::Atom::new(self.atoms.utf8_string),
            property: x::Atom::new(self.atoms.jfn_selection),
            time: x::CURRENT_TIME,
        });
        drop(self.conn.flush());
    }

    /// Answers `TARGETS`, `TIMESTAMP`, `UTF8_STRING`, `STRING`, `TEXT` and
    /// `text/plain;charset=utf-8`.
    ///
    /// A request naming any other target, and one whose value exceeds the
    /// connection's maximum request length, is refused with a
    /// `SelectionNotify` naming no property.
    pub(crate) fn on_selection_request(&self, ev: &x::SelectionRequestEvent) {
        let property = self.answer(ev).unwrap_or(x::ATOM_NONE);
        self.conn.send_request(&x::SendEvent {
            propagate: false,
            destination: x::SendEventDest::Window(ev.requestor()),
            event_mask: x::EventMask::empty(),
            event: &x::SelectionNotifyEvent::new(
                ev.time(),
                ev.requestor(),
                ev.selection(),
                ev.target(),
                property,
            ),
        });
        drop(self.conn.flush());
    }

    /// The property the answer was written into, or `None` for a refusal.
    fn answer(&self, ev: &x::SelectionRequestEvent) -> Option<x::Atom> {
        let property = if ev.property() == x::ATOM_NONE {
            ev.target()
        } else {
            ev.property()
        };
        let kind = self.kind_of(ev.selection())?;
        let text = self.stored.lock()[kind.slot()].clone()?;
        let target = ev.target();
        if target == x::Atom::new(self.atoms.targets) {
            let targets: [u32; 6] = [
                self.atoms.targets,
                self.atoms.timestamp,
                self.atoms.utf8_string,
                x::ATOM_STRING.resource_id(),
                self.atoms.text,
                self.atoms.text_plain_utf8,
            ];
            self.put(ev.requestor(), property, x::ATOM_ATOM, 32, &targets)?;
            return Some(property);
        }
        if target == x::Atom::new(self.atoms.timestamp) {
            self.put(
                ev.requestor(),
                property,
                x::ATOM_INTEGER,
                32,
                &[x::CURRENT_TIME],
            )?;
            return Some(property);
        }
        let text_targets = [
            self.atoms.utf8_string,
            x::ATOM_STRING.resource_id(),
            self.atoms.text,
            self.atoms.text_plain_utf8,
        ];
        if !text_targets.contains(&target.resource_id()) {
            return None;
        }
        self.put(ev.requestor(), property, target, 8, text.as_bytes())?;
        Some(property)
    }

    /// Writes one property, refusing a value the connection cannot carry in a
    /// single request rather than starting an `INCR` transfer.
    fn put<T: x::PropEl>(
        &self,
        window: x::Window,
        property: x::Atom,
        type_: x::Atom,
        format: u8,
        data: &[T],
    ) -> Option<()> {
        let units = data.len() * (format as usize / 8);
        // The request header costs a handful of words; the whole value must
        // still fit inside one request.
        if units / 4 + 8 > self.conn.get_maximum_request_length() as usize {
            return None;
        }
        let cookie = self.conn.send_request_checked(&x::ChangeProperty {
            mode: x::PropMode::Replace,
            window,
            property,
            r#type: type_,
            data,
        });
        self.conn.check_request(cookie).ok()
    }

    pub(crate) fn on_selection_notify(&self, ev: &x::SelectionNotifyEvent) {
        let Some(pending) = self.pending.lock().take() else {
            return;
        };
        if ev.property() == x::ATOM_NONE {
            (pending.on_done)(None);
            return;
        }
        (pending.on_done)(self.take_property(ev.property()).as_deref());
    }

    /// Reads and deletes the property a conversion was delivered into.
    fn take_property(&self, property: x::Atom) -> Option<String> {
        let cookie = self.conn.send_request(&x::GetProperty {
            delete: true,
            window: self.owner,
            property,
            r#type: x::ATOM_ANY,
            long_offset: 0,
            long_length: u32::MAX / 4,
        });
        let reply = self.conn.wait_for_reply(cookie).ok()?;
        if reply.r#type() == x::Atom::new(self.atoms.incr) {
            return None;
        }
        let text = String::from_utf8(reply.value::<u8>().to_vec()).ok()?;
        (!text.is_empty()).then_some(text)
    }

    /// Drops the stored text for the selection another client took.
    pub(crate) fn on_selection_clear(&self, ev: &x::SelectionClearEvent) {
        let Some(kind) = self.kind_of(ev.selection()) else {
            return;
        };
        self.stored.lock()[kind.slot()] = None;
    }

    /// Resolves every pending read with no text.
    pub(crate) fn cleanup(&self) {
        if let Some(pending) = self.pending.lock().take() {
            (pending.on_done)(None);
        }
    }

    fn kind_of(&self, selection: x::Atom) -> Option<Kind> {
        let raw = selection.resource_id();
        if raw == self.atoms.clipboard {
            Some(Kind::Clipboard)
        } else if raw == self.atoms.primary {
            Some(Kind::Primary)
        } else {
            None
        }
    }
}

pub(crate) struct X11Primary;

impl jfn_platform_abi::PrimarySelection for X11Primary {
    fn read_text_async(&self, on_done: OnText) {
        match selections() {
            Some(selections) => selections.read_text_async(Kind::Primary, on_done),
            None => on_done(None),
        }
    }

    fn write_text(&self, text: &str) {
        if let Some(selections) = selections() {
            selections.write_text(Kind::Primary, text);
        }
    }
}
