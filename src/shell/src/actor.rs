//! The render actor.
//!
//! Its thread is the sole writer of the swapchain and of the `CAMetalLayer` /
//! `IDCompositionVisual` / `wl_surface` behind it. Everything else in the
//! process talks to the overlay by posting [`Work`].

use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use iced_core::mouse::{self, Cursor};
use iced_core::widget::Id;
use iced_core::{Element, Event, Point, Size, clipboard, renderer, shell, window};
use iced_runtime::user_interface::{self, UserInterface};
use jfn_gpu_paint::{Acquired, Presented};
use jfn_platform_abi::{
    FrameSource, LogicalPoint, LogicalSize, SurfaceHandle, Visibility, WindowExtent,
};

use crate::field::Act;
use crate::fields::{Apply, Fields};

use crate::chrome::Titlebar;
use crate::modal::{Stack, Transition};
use crate::paint::Painter;
use crate::state::{self, ChromeInputs};
use crate::theme::Theme;

/// How long the actor waits for a surface target before giving up: the
/// backend creates the window on its own thread, moments after `alloc_surface`.
const TARGET_WAIT: Duration = Duration::from_secs(5);
const TARGET_POLL: Duration = Duration::from_millis(10);

pub enum Work {
    Event(Event),
    Resize {
        extent: WindowExtent,
    },
    Redraw,
    OpenAbout,
    Chrome(ChromeInputs),
    /// The buffered theme colour changed; the titlebar and backdrop repaint.
    ChromeBackground(iced_core::Color),
    /// A selection read's text; `None` for a read that fetched nothing.
    SelectionText {
        reader: Reader,
        text: Option<String>,
    },
    /// A right press the shell overlay owns, in window coordinates.
    ContextMenu(LogicalPoint),
    /// The Menu key or Shift+F10: the edit menu at the focused field's caret.
    /// With no field focused it raises nothing.
    EditMenuAtCaret,
    /// A middle press the shell overlay owns, in window coordinates.
    PrimaryPaste(LogicalPoint),
    /// An edit menu's selection, for the field it named.
    EditAt {
        field: Target,
        command: jfn_input::EditCommand,
    },
    /// Bring-up advanced; the pass re-reads its screen.
    BringUpChanged,
    Shutdown,
}

/// Which field an edit acts on.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Target {
    /// The field holding keyboard focus, whichever it is.
    Focused,
    /// The field a menu was raised over, focused or not.
    Named(Id),
}

/// Where a selection read's text goes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Reader {
    /// iced asked for it; it reaches the focused field as
    /// [`iced_core::clipboard::Event::Read`].
    Iced,
    /// A menu paste or a middle press asked for it; it is applied to the field
    /// it names.
    Field(Id),
}

/// Bound on the render thread's shutdown drain; a wedged present must not hold
/// the process open.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// When the next redraw is due, folded from every source: iced's
/// `RedrawRequest`, the deadline bring-up names, the spinner's own animation
/// while the connect screen is working, and a model that changed during the
/// pass. The caret's blink is not among
/// them — a focused editor asks for its own next frame through the
/// `RedrawRequest` this already folds, and an unfocused one asks for nothing.
///
/// A deadline already in the past yields one immediate pass and then `None`,
/// never a zero-length wait that spins.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Deadline(Option<Instant>);

impl Deadline {
    pub fn none() -> Deadline {
        Deadline(None)
    }

    pub fn at(when: Instant) -> Deadline {
        Deadline(Some(when))
    }

    pub fn merge(self, other: Deadline) -> Deadline {
        match (self.0, other.0) {
            (Some(a), Some(b)) => Deadline(Some(a.min(b))),
            (a, b) => Deadline(a.or(b)),
        }
    }

    pub fn elapsed(self, now: Instant) -> bool {
        self.0.is_some_and(|at| now >= at)
    }

    /// `None` blocks until the next posted work; `Some` is the bounded wait.
    pub fn wait_for(self, now: Instant) -> Option<Duration> {
        self.0.map(|at| at.saturating_duration_since(now))
    }
}

/// The actor's channel, built before its thread exists. Work posted into it
/// waits in the queue the thread takes at spawn rather than being dropped.
pub struct Channel {
    tx: Sender<Work>,
    rx: parking_lot::Mutex<Option<Receiver<Work>>>,
}

impl Default for Channel {
    fn default() -> Self {
        Self::new()
    }
}

impl Channel {
    pub fn new() -> Channel {
        let (tx, rx) = channel();
        Channel {
            tx,
            rx: parking_lot::Mutex::new(Some(rx)),
        }
    }

    pub fn post(&self, work: Work) {
        drop(self.tx.send(work));
    }

    fn sender(&self) -> Sender<Work> {
        self.tx.clone()
    }

    fn take_receiver(&self) -> Option<Receiver<Work>> {
        self.rx.lock().take()
    }
}

pub struct Actor {
    tx: Sender<Work>,
    thread: JoinHandle<()>,
}

impl Actor {
    /// Spawns the render thread. It is the sole writer of the swapchain and of
    /// the `CAMetalLayer` / `IDCompositionVisual` / `wl_surface` behind it.
    ///
    /// One pass, in order:
    ///   1. drain every queued [`Work`], blocking only when the queue is empty
    ///      and no deadline is due, so a pointer stream faster than the refresh
    ///      rate collapses into one pass instead of backing up;
    ///   2. build the `UserInterface` from the model;
    ///   3. `update` it with the drained events followed by
    ///      `Event::Window(window::Event::RedrawRequested(Instant::now()))` —
    ///      the event widgets commit hover, press, focus and caret state on;
    ///   4. apply every produced message, and every event `update` reported
    ///      ignored, to the model;
    ///   5. if that changed the model, or `update` returned `State::Outdated`,
    ///      begin the next pass immediately and draw nothing this one;
    ///   6. otherwise acquire a frame, draw into it and present it. Hidden, and
    ///      on a swapchain that had no frame to give, the pass draws nothing.
    ///
    /// A pass that changed the model re-applies focus to
    /// [`Connect::focus_target`] before drawing, so the URL field keeps its
    /// caret across a window resize and across an Escape the editor consumed as
    /// an unfocus.
    ///
    /// The thread calls [`crate::wait_fonts_ready`] before its first draw and
    /// never again.
    pub fn spawn(surface: SurfaceHandle, channel: &Channel) -> Option<Actor> {
        let rx = channel.take_receiver()?;
        let tx = channel.sender();
        let wake_tx = channel.sender();
        let thread = std::thread::Builder::new()
            .name("jfn-shell".to_owned())
            .spawn(move || run(surface, &rx, &wake_tx))
            .ok()?;
        Some(Actor { tx, thread })
    }

    pub fn post(&self, work: Work) {
        drop(self.tx.send(work));
    }

    /// Drains `Work::Shutdown` and joins the thread, bounded by
    /// [`SHUTDOWN_TIMEOUT`]. `true` once the thread is gone and the swapchain
    /// with it — the only condition under which the surface may be freed.
    /// `false` leaves a wedged render thread owning both for the rest of the
    /// process.
    #[must_use]
    pub fn join(self) -> bool {
        drop(self.tx.send(Work::Shutdown));
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        while !self.thread.is_finished() {
            if Instant::now() >= deadline {
                tracing::warn!("shell: render thread did not stop within the shutdown bound");
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        self.thread.join().is_ok()
    }
}

/// Whether an event iced reported as ignored would change the model, asked
/// before the pass has let go of the widget tree that borrows it.
/// [`iced_core::keyboard::key::Named::Escape`] reaches the occupant as
/// [`Transition::Escape`]; everything else is dropped.
fn handles(model: &Model, event: &Event) -> bool {
    use iced_core::keyboard::{self, Key, key::Named};
    matches!(
        event,
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: Key::Named(Named::Escape),
            ..
        })
    ) && model.stack.occupied()
}

#[derive(Clone, Debug)]
enum Message {
    Modal(crate::modal::Message),
    Chrome(crate::chrome::Message),
}

pub struct Model {
    stack: Stack,
    screen: jfn_bringup::Screen,
    titlebar: Titlebar,
    inputs: ChromeInputs,
    theme: Theme,
}

impl Model {
    fn titlebar_shown(&self) -> bool {
        state::titlebar_shown(self.inputs)
    }

    /// The modal stack's own view, then the titlebar, then nothing.
    fn view(&self) -> Element<'_, Message, Theme, iced_wgpu::Renderer> {
        if self.stack.occupied() {
            return self.stack.view(&self.screen).map(Message::Modal);
        }
        if self.titlebar_shown() {
            return self.titlebar.view().map(Message::Chrome);
        }
        iced_widget::space::horizontal().into()
    }

    fn advance(&mut self, transition: Transition) {
        self.stack.advance(transition);
    }

    fn update(&mut self, message: Message) {
        match message {
            Message::Modal(m) => self.advance(Transition::Message(m)),
            Message::Chrome(m) => self.titlebar.update(m),
        }
    }

    /// The open modal's backdrop; fully transparent when none is open, so
    /// jellyfin-web shows through everywhere no widget draws.
    fn backdrop(&self) -> iced_core::Color {
        self.stack
            .backdrop(self.theme.chrome_background, &self.screen)
    }

    /// When the model next needs a frame on its own.
    fn deadline(&self) -> Deadline {
        self.stack.deadline(&self.screen)
    }

    /// What this model asks the overlay surface to be.
    fn visibility(&self) -> Visibility {
        Visibility::shown(state::overlay_visible(
            self.stack.occupied(),
            self.titlebar_shown(),
        ))
    }
}

fn run(surface: SurfaceHandle, rx: &Receiver<Work>, wake_tx: &Sender<Work>) {
    let Some(gpu) = jfn_gpu_paint::surfaces() else {
        tracing::info!("shell: no GPU device; overlay stays hidden");
        crate::publish_no_overlay();
        return;
    };
    let Some(target) = wait_for_target(surface) else {
        tracing::error!("shell: no window target for the overlay surface");
        crate::publish_no_overlay();
        return;
    };

    let Some(extent) = initial_extent() else {
        tracing::error!("shell: no extent to start the overlay at");
        crate::publish_no_overlay();
        return;
    };
    let wake = {
        let tx = wake_tx.clone();
        Arc::new(move || {
            drop(tx.send(Work::Redraw));
        }) as Arc<dyn Fn() + Send + Sync>
    };
    let mut painter = match Painter::new(gpu, target, extent, Arc::clone(&wake)) {
        Ok(painter) => painter,
        Err(e) => {
            tracing::error!("shell: swapchain creation failed: {e}");
            crate::publish_no_overlay();
            return;
        }
    };

    let mut model = Model {
        stack: Stack::empty(),
        screen: jfn_bringup::screen(),
        titlebar: Titlebar::new(),
        inputs: crate::chrome::inputs(),
        theme: Theme {
            chrome_background: crate::theme::chrome_background(),
            ..Theme::default()
        },
    };
    let mut cache = user_interface::Cache::new();
    let mut events: Vec<Event> = Vec::new();
    let mut cursor = Cursor::Unavailable;
    let waker = shell::Waker::new({
        let wake = Arc::clone(&wake);
        move || wake()
    });
    let redraw = Redraw(wake_tx.clone());
    let mut current = extent;
    // Every pass that discarded the widget cache re-applies focus, so the URL
    // field keeps the caret across a window resize.
    let mut refocus = true;
    publish(&model, current);
    apply_visibility(surface, &model);

    let mut pending = Deadline::none();
    // Set by a pass that left the model unsettled: the next one starts without
    // waiting and draws nothing until it does settle.
    let mut immediate = false;
    let mut batch: Vec<Work> = Vec::new();
    // Edits waiting for a widget tree to apply them to, and the requests that
    // need one to resolve against.
    let mut queued: Vec<Apply> = Vec::new();
    let mut deferred: Vec<Deferred> = Vec::new();
    // The primary selection this process last published, so an unchanged
    // selection does not re-take the selection every pass.
    let mut last_primary: Option<(Id, u64)> = None;
    // The first draw waits for the bundled font; every later one does not.
    let mut drew_nothing_yet = true;

    'pass: loop {
        if !immediate {
            // A deadline already in the past yields one immediate pass rather
            // than a zero-length wait the loop would spin on.
            let deadline = pending.merge(model.deadline());
            let now = Instant::now();
            if !deadline.elapsed(now) {
                let blocked = match deadline.wait_for(now) {
                    Some(timeout) => match rx.recv_timeout(timeout) {
                        Ok(work) => Some(work),
                        Err(RecvTimeoutError::Timeout) => None,
                        Err(RecvTimeoutError::Disconnected) => break,
                    },
                    None => match rx.recv() {
                        Ok(work) => Some(work),
                        Err(_) => break,
                    },
                };
                batch.extend(blocked);
            }
        }
        pending = Deadline::none();
        immediate = false;
        // The whole queue, so a pointer stream faster than the refresh rate
        // collapses into one pass instead of backing up.
        loop {
            match rx.try_recv() {
                Ok(work) => batch.push(work),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break 'pass,
            }
        }

        for work in batch.drain(..) {
            match work {
                Work::Event(event) => {
                    if let Event::Mouse(mouse::Event::CursorMoved { position }) = event {
                        cursor = Cursor::Available(position);
                    }
                    if matches!(event, Event::Mouse(mouse::Event::CursorLeft)) {
                        cursor = Cursor::Unavailable;
                    }
                    events.push(event);
                }
                Work::Resize { extent } => {
                    painter.resize(extent);
                    current = extent;
                    cache = user_interface::Cache::new();
                    refocus = true;
                }
                Work::Redraw => {}
                Work::OpenAbout => model.advance(Transition::OpenAbout),
                Work::Chrome(inputs) => model.inputs = inputs,
                Work::ChromeBackground(color) => model.theme.chrome_background = color,
                Work::SelectionText { reader, text } => match (reader, text) {
                    (Reader::Iced, Some(text)) => {
                        events.push(Event::Clipboard(clipboard::Event::Read(Ok(Arc::new(
                            clipboard::Content::Text(text),
                        )))));
                    }
                    (Reader::Field(id), Some(text)) => {
                        queued.push(Apply::act(id, Act::Paste(text)))
                    }
                    (_, None) => {}
                },
                Work::ContextMenu(p) => deferred.push(Deferred::ContextMenu(p)),
                Work::EditMenuAtCaret => deferred.push(Deferred::EditMenuAtCaret),
                Work::PrimaryPaste(p) => deferred.push(Deferred::PrimaryPaste(p)),
                Work::EditAt { field, command } => deferred.push(Deferred::Edit(field, command)),
                Work::BringUpChanged => {}
                Work::Shutdown => break 'pass,
            }
        }

        model.advance(Transition::Tick(Instant::now()));
        // Every pass re-reads bring-up: it is the authority for what the shell
        // overlay shows, and the stack holds none of it.
        model.screen = jfn_bringup::screen();
        model.stack.reconcile(&model.screen);
        pending = pending.merge(jfn_bringup::deadline().map_or_else(Deadline::none, Deadline::at));

        model.theme.backdrop = model.backdrop();
        let focus_target = model.stack.focus_target(&model.screen);
        let mut ui = UserInterface::build(
            model.view(),
            Size::new(current.logical().w as f32, current.logical().h as f32),
            std::mem::replace(&mut cache, user_interface::Cache::new()),
            painter.renderer(),
        );
        if refocus {
            refocus = false;
            if let Some(id) = focus_target {
                ui.operate(
                    painter.renderer(),
                    &mut iced_core::widget::operation::focusable::focus::<()>(id),
                );
            }
        }
        let fields = Fields::collect(&mut ui, painter.renderer());
        let mut menu_anchor = None;
        for request in deferred.drain(..) {
            match request {
                Deferred::ContextMenu(p) => {
                    menu_anchor = menu_anchor.or(raise_menu(&fields, p, &mut queued));
                }
                Deferred::EditMenuAtCaret => {
                    menu_anchor = menu_anchor.or_else(|| fields.focused().map(caret_anchor));
                }
                Deferred::PrimaryPaste(p) => {
                    if let Some(field) = fields.at(point(p.x, p.y)) {
                        read_primary(wake_tx.clone(), Reader::Field(field.id.clone()));
                    }
                }
                Deferred::Edit(target, command) => {
                    queue_edit(&fields, &target, command, &mut queued, wake_tx);
                }
            }
        }
        for text in apply_queued(&mut ui, painter.renderer(), &mut queued) {
            jfn_platform_abi::get().clipboard_write_text(&text);
        }
        if let Some(anchor) = menu_anchor {
            let raised = Fields::collect(&mut ui, painter.renderer());
            if let Some(field) = raised.at(point(anchor.x, anchor.y)) {
                crate::menu::open_edit(field, anchor, crate::lang::strings());
            }
        }

        // The event widgets commit hover, press, focus and caret state on.
        events.push(Event::Window(
            window::Event::RedrawRequested(Instant::now()),
        ));
        let mut bus = shell::Bus::new();
        let (state, statuses) = ui.update(
            &window::Headless,
            &waker,
            &events,
            cursor,
            painter.renderer(),
            &mut bus,
        );
        // The focused widget sees every event first; only what iced ignored is
        // offered to the open modal.
        let ignored: Vec<Event> = events
            .iter()
            .zip(statuses)
            .filter(|(_, status)| *status == iced_core::event::Status::Ignored)
            .map(|(event, _)| event.clone())
            .collect();
        events.clear();

        let messages: Vec<Message> = bus.drain().collect();
        let interaction = match &state {
            user_interface::State::Updated {
                mouse_interaction,
                redraw_request,
                ..
            } => {
                pending = pending.merge(match redraw_request {
                    window::RedrawRequest::NextFrame => Deadline::at(Instant::now()),
                    window::RedrawRequest::At(at) => Deadline::at(*at),
                    window::RedrawRequest::Wait => Deadline::none(),
                });
                *mouse_interaction
            }
            user_interface::State::Outdated => mouse::Interaction::None,
        };
        if let user_interface::State::Updated { clipboard, .. } = &state {
            write_clipboard(clipboard);
            if clipboard.reads.contains(&clipboard::Kind::Text) {
                read_clipboard(wake_tx.clone(), Reader::Iced);
            }
        }
        let settled_fields = Fields::collect(&mut ui, painter.renderer());
        publish_primary(&settled_fields, &mut last_primary);
        jfn_input::publish_field_edit(
            settled_fields
                .focused()
                .map(crate::fields::Snapshot::edit_state),
        );
        crate::router_sink::set_interaction(interaction);

        // Asked while the widget tree still borrows the model, because applying
        // any of it has to wait until the tree is gone.
        let settled = messages.is_empty()
            && !ignored.iter().any(|event| handles(&model, event))
            && !matches!(state, user_interface::State::Outdated);

        if settled {
            if drew_nothing_yet {
                drew_nothing_yet = false;
                // The fontdb scan is paid on the warm-up thread, not here, and
                // no paragraph caches against a fallback family.
                crate::wait_fonts_ready();
            }
            match paint(&mut painter, surface, &mut ui, &model, cursor, &redraw) {
                Painted::Shown(_) | Painted::Hidden | Painted::Requested => {}
                Painted::Deferred(retry_at) => pending = pending.merge(Deadline::at(retry_at)),
            }
            cache = ui.into_cache();
            publish(&model, current);
            continue;
        }

        cache = ui.into_cache();
        for message in messages {
            model.update(message);
        }
        for event in &ignored {
            if handles(&model, event) {
                model.advance(Transition::Escape);
            }
        }
        // The tree the next pass builds is a new one; focus is applied to it
        // rather than inherited.
        refocus = true;
        immediate = true;
        publish(&model, current);
        apply_visibility(surface, &model);
    }
}

/// What one settled pass did.
enum Painted {
    /// The frame it drew reached the surface's commit stream.
    Shown(Presented),
    /// It drew nothing, and the commit that hid the surface landed.
    Hidden,
    /// The swapchain had no frame; the next pass is due at this instant.
    Deferred(Instant),
    /// The swapchain had no frame and the display reports no refresh interval;
    /// the wake was requested from the overlay's own frame source.
    Requested,
}

/// The shell overlay's own producer: a deferred acquire with no retry deadline
/// asks it for the wake that re-runs the pass.
struct Redraw(Sender<Work>);

impl FrameSource for Redraw {
    fn request_frame(&self) {
        drop(self.0.send(Work::Redraw));
    }
}

/// Draws and commits one settled pass. A shown overlay acquires the frame it
/// draws into and presents it; a hidden or deferred one acquires nothing and
/// draws nothing.
///
/// Hidden, the widget tree still updates and nothing is presented: a present
/// against a surface that is not on screen blocks the thread inside the
/// compositor's FIFO queue.
fn paint(
    painter: &mut Painter,
    surface: SurfaceHandle,
    ui: &mut UserInterface<'_, Message, Theme, iced_wgpu::Renderer>,
    model: &Model,
    cursor: Cursor,
    redraw: &Redraw,
) -> Painted {
    match apply_visibility(surface, model) {
        Visibility::Hidden => Painted::Hidden,
        Visibility::Shown => match painter.acquire() {
            Acquired::Deferred(deferred) => match deferred.retry_at() {
                Some(at) => Painted::Deferred(at),
                None => {
                    redraw.request_frame();
                    Painted::Requested
                }
            },
            Acquired::Frame(frame) => {
                ui.draw(
                    painter.renderer(),
                    &model.theme,
                    &renderer::Style::default(),
                    cursor,
                );
                Painted::Shown(painter.present(frame))
            }
        },
    }
}

/// A request that needs a widget tree to resolve against, held until the pass
/// has built one.
enum Deferred {
    ContextMenu(LogicalPoint),
    EditMenuAtCaret,
    PrimaryPaste(LogicalPoint),
    Edit(Target, jfn_input::EditCommand),
}

/// Applies every queued [`Apply`] and returns what `Cut` and `Copy` produced,
/// in order.
fn apply_queued<Message>(
    ui: &mut UserInterface<'_, Message, Theme, iced_wgpu::Renderer>,
    renderer: &iced_wgpu::Renderer,
    queued: &mut Vec<Apply>,
) -> Vec<String> {
    let mut produced = Vec::new();
    for mut apply in queued.drain(..) {
        ui.operate(renderer, &mut apply);
        if let Some(text) = apply.produced() {
            produced.push(text.to_owned());
        }
    }
    produced
}

/// Queues the acts `command` becomes for `target`, and requests the clipboard
/// read that `Paste` needs. An edit chosen from a menu leaves keyboard focus
/// where it is, so the edit menu acts on an unfocused field on Wayland and X11.
fn queue_edit(
    fields: &Fields,
    target: &Target,
    command: jfn_input::EditCommand,
    queued: &mut Vec<Apply>,
    tx: &Sender<Work>,
) {
    use jfn_input::EditCommand as E;
    let field = match target {
        Target::Focused => fields.focused(),
        Target::Named(id) => fields.named(id),
    };
    let Some(field) = field else {
        return;
    };
    let id = field.id.clone();
    let act = match command {
        E::Undo => Act::Undo,
        E::Redo => Act::Redo,
        E::Cut => Act::Cut,
        E::Copy => Act::Copy,
        E::SelectAll => Act::SelectAll,
        E::Paste => {
            read_clipboard(tx.clone(), Reader::Field(id));
            return;
        }
    };
    queued.push(Apply::act(id, act));
}

/// Requests the OS clipboard's text; the reply arrives as
/// [`Work::SelectionText`].
fn read_clipboard(tx: Sender<Work>, reader: Reader) {
    jfn_platform_abi::get().clipboard_read_text_async(Box::new(move |text| {
        drop(tx.send(Work::SelectionText {
            reader,
            text: text.map(str::to_owned),
        }));
    }));
}

/// Requests the primary selection's text, replying `None` on a backend that
/// serves none.
fn read_primary(tx: Sender<Work>, reader: Reader) {
    let plat = jfn_platform_abi::get();
    let Some(primary) = plat.primary_selection() else {
        drop(tx.send(Work::SelectionText { reader, text: None }));
        return;
    };
    primary.read_text_async(Box::new(move |text| {
        drop(tx.send(Work::SelectionText {
            reader,
            text: text.map(str::to_owned),
        }));
    }));
}

/// Writes iced's pending clipboard content, text alone.
fn write_clipboard(clipboard: &clipboard::Clipboard) {
    if let Some(clipboard::Content::Text(text)) = &clipboard.write {
        jfn_platform_abi::get().clipboard_write_text(text);
    }
}

/// Writes the focused field's selection to the primary selection whenever the
/// selection changed and is not empty; a selection replaced by an identical
/// one is a change, and a backend that serves none writes nothing.
fn publish_primary(fields: &Fields, last: &mut Option<(Id, u64)>) {
    let plat = jfn_platform_abi::get();
    let Some(primary) = plat.primary_selection() else {
        return;
    };
    let Some(field) = fields.focused() else {
        return;
    };
    let mark = (field.id.clone(), field.selection_generation);
    if last.as_ref() == Some(&mark) {
        return;
    }
    *last = Some(mark);
    let Some(text) = &field.selection else {
        return;
    };
    primary.write_text(text);
}

/// The window point an edit menu raised from the keyboard anchors at: the
/// focused field's caret.
fn caret_anchor(field: &crate::fields::Snapshot) -> LogicalPoint {
    LogicalPoint {
        x: field.caret.x as i32,
        y: field.caret.y as i32,
    }
}

/// The menu a right press raises: the edit menu over a shell field, with the
/// focus and the caret act ADR 0012 gives the backend queued first, and the app
/// menu everywhere else the shell overlay owns.
///
/// The field takes focus on Windows and macOS whether or not the press landed
/// inside its selection, so the keys typed after the menu closes reach it and
/// macOS's own Edit menu resolves [`Target::Focused`] to it.
fn raise_menu(fields: &Fields, p: LogicalPoint, queued: &mut Vec<Apply>) -> Option<LogicalPoint> {
    let at = point(p.x, p.y);
    let Some(field) = fields.at(at) else {
        jfn_cef::app_menu::open_at(p.x, p.y);
        return None;
    };
    let backend = jfn_platform_abi::get().display();
    let caret = crate::fields::press_caret(backend, field, at);
    if crate::fields::press_focuses(backend) {
        queued.push(Apply::focus(field.id.clone(), caret));
    } else if let Some(act) = caret {
        queued.push(Apply::act(field.id.clone(), act));
    }
    Some(p)
}

/// Publishes the routing state at the extent's exact logical size.
fn publish(model: &Model, extent: WindowExtent) {
    jfn_input::publish_shell_state(crate::state::shell_state(
        Some(extent),
        model.inputs,
        model.stack.occupied(),
    ));
}

/// Writes the overlay surface's visibility and returns once the commit
/// carrying it has landed. The surface's own backend holds the value; the model
/// holds only what it asked for.
fn apply_visibility(surface: SurfaceHandle, model: &Model) -> Visibility {
    jfn_platform_abi::get()
        .set_surface_visibility(surface, model.visibility())
        .acknowledged()
}

fn wait_for_target(surface: SurfaceHandle) -> Option<jfn_gpu_paint::WindowTarget> {
    let plat = jfn_platform_abi::get();
    let deadline = Instant::now() + TARGET_WAIT;
    loop {
        if let Some(target) = plat.surface_window_target(surface) {
            return Some(target);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(TARGET_POLL);
    }
}

/// The extent the overlay starts at: the window source's own when it has one,
/// else 1280x720 logical at the platform's reported scale.
fn initial_extent() -> Option<WindowExtent> {
    let plat = jfn_platform_abi::get();
    if let Some(extent) = plat.window_owner().source().snapshot().extent {
        return Some(extent);
    }
    let scale = plat.scale();
    let logical = LogicalSize { w: 1280, h: 720 };
    WindowExtent::new(logical.to_physical(scale)?, scale, logical)
}

/// The pointer position an iced event carries, for the sink's convenience.
pub(crate) fn point(x: i32, y: i32) -> Point {
    Point::new(x as f32, y as f32)
}
