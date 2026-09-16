use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use iced_core::mouse::{self, Cursor};
use iced_core::widget::{Id, Operation as _};
use iced_core::{Element, Event, Point, Size, clipboard, renderer, shell, window};
use iced_runtime::user_interface::{self, UserInterface};
use jfn_gpu_paint::{Acquired, Presented};
use jfn_platform_abi::{
    FrameSource, LogicalPoint, LogicalSize, SurfaceHandle, Visibility, WindowExtent,
};

use crate::shell::controls::{self, Direction};
use crate::shell::field::Act;
use crate::shell::fields::{Apply, Fields};

use crate::shell::chrome::Titlebar;
use crate::shell::modal::{Identity, Stack, Transition};
use crate::shell::paint::Painter;
use crate::shell::state::{self, ChromeInputs};
use crate::shell::theme::Theme;

const TARGET_WAIT: Duration = Duration::from_secs(5);

pub enum Work {
    Event(Event),
    Resize {
        extent: WindowExtent,
    },
    Redraw,
    OpenAbout,
    OpenClientSettings,
    Chrome(ChromeInputs),
    ChromeBackground(iced_core::Color),
    SelectionText {
        reader: Reader,
        text: Option<String>,
    },
    ContextMenu(LogicalPoint),
    EditMenuAtCaret,
    PrimaryPaste(LogicalPoint),
    EditAt {
        field: Target,
        command: jfn_input::EditCommand,
    },
    WebAttached(jfn_cef::WebOverlay),
    WebEvent(jfn_cef::WebEvent),
    Shutdown,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Target {
    Focused,
    Named(Id),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Reader {
    Iced,
    Field(Id),
}

pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

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

    pub fn wait_for(self, now: Instant) -> Option<Duration> {
        self.0.map(|at| at.saturating_duration_since(now))
    }
}

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
    pub fn spawn(
        surface: SurfaceHandle,
        channel: &Channel,
        metadata: crate::shell::metadata::ApplicationMetadata,
        actions: crate::shell::ApplicationActions,
        platform: jfn_platform_abi::PlatformLease,
        on_ready: std::sync::mpsc::SyncSender<Result<(), LoopStartError>>,
    ) -> Result<Actor, crate::shell::StartError> {
        let rx = channel
            .take_receiver()
            .ok_or(crate::shell::StartError::ReceiverUnavailable)?;
        let tx = channel.sender();
        let wake_tx = channel.sender();
        let thread = std::thread::Builder::new()
            .name("jfn-shell".to_owned())
            .spawn(move || {
                let _platform = platform;
                run(surface, &rx, &wake_tx, metadata, actions, on_ready);
            })?;
        Ok(Actor { tx, thread })
    }

    pub fn post(&self, work: Work) {
        drop(self.tx.send(work));
    }

    pub fn join(self) -> JoinOutcome {
        drop(self.tx.send(Work::Shutdown));
        join_bounded(self.thread, SHUTDOWN_TIMEOUT)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use = "unconfirmed termination must retain native dependencies"]
pub enum JoinOutcome {
    Terminated,
    Panicked,
    TimedOut,
    JoinerUnavailable,
}
impl JoinOutcome {
    pub fn terminated(self) -> bool {
        matches!(self, Self::Terminated | Self::Panicked)
    }
}

fn join_bounded(thread: JoinHandle<()>, timeout: Duration) -> JoinOutcome {
    let (tx, rx) = channel();
    if std::thread::Builder::new()
        .name("jfn-shell-join".to_owned())
        .spawn(move || {
            let result = if thread.join().is_ok() {
                JoinOutcome::Terminated
            } else {
                JoinOutcome::Panicked
            };
            tx.send(result).unwrap_or_default();
        })
        .is_err()
    {
        return JoinOutcome::JoinerUnavailable;
    }
    rx.recv_timeout(timeout).unwrap_or(JoinOutcome::TimedOut)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum IgnoredKey {
    Escape,
    Focus(Direction),
}

fn ignored_key(model: &Model, event: &Event) -> Option<IgnoredKey> {
    use iced_core::keyboard::{self, Key, key::Named};
    let Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) = event else {
        return None;
    };
    match key.as_ref() {
        Key::Named(Named::Escape) if model.stack.occupied() => Some(IgnoredKey::Escape),
        Key::Named(Named::Tab) if model.stack.identity() == Some(Identity::SettingsOverlay) => {
            Some(IgnoredKey::Focus(if modifiers.shift() {
                Direction::Backward
            } else {
                Direction::Forward
            }))
        }
        _ => None,
    }
}

fn focus_after_rebuild(
    previous_identity: Option<Identity>,
    identity: Option<Identity>,
    previous_tab: Option<crate::shell::settings_overlay::Tab>,
    tab: Option<crate::shell::settings_overlay::Tab>,
    initial: Option<Id>,
    prior: Option<Id>,
    cache_lost: bool,
) -> Option<Id> {
    if previous_identity != identity || previous_tab != tab {
        initial
    } else if cache_lost {
        prior
    } else {
        None
    }
}

#[derive(Clone, Debug)]
enum Message {
    Modal(crate::shell::modal::Message),
    Chrome(crate::shell::chrome::Message),
}

pub struct Model {
    connection: crate::connection::Connection,
    stack: Stack,
    screen: crate::connection::Screen,
    titlebar: Titlebar,
    inputs: ChromeInputs,
    theme: Theme,
}

impl Model {
    fn titlebar_shown(&self) -> bool {
        state::titlebar_shown(self.inputs)
    }

    fn view(&self) -> Element<'_, Message, Theme, iced_wgpu::Renderer> {
        if self.stack.occupied() {
            return self.stack.view(&self.screen).map(Message::Modal);
        }
        if self.titlebar_shown() {
            return self.titlebar.view().map(Message::Chrome);
        }
        iced_widget::space::horizontal().into()
    }

    fn transition(&mut self, transition: Transition) {
        self.stack.update(transition, &mut self.connection);
        self.connection.flush_requests();
    }

    fn update(&mut self, message: Message) {
        match message {
            Message::Modal(m) => self.transition(Transition::Message(m)),
            Message::Chrome(m) => self.titlebar.update(m),
        }
    }

    fn apply_message_batch(&mut self, messages: Vec<Message>) {
        for message in settings_overlay_dismiss_last(messages) {
            self.update(message);
        }
    }

    fn backdrop(&self) -> iced_core::Color {
        self.stack
            .backdrop(self.theme.chrome_background, &self.screen)
    }

    fn deadline(&self) -> Deadline {
        self.stack.deadline(&self.screen)
    }

    fn visibility(&self) -> Visibility {
        Visibility::shown(state::overlay_visible(
            self.stack.occupied(),
            self.titlebar_shown(),
        ))
    }
}

fn settings_overlay_dismiss_last(messages: Vec<Message>) -> Vec<Message> {
    let (mut ordinary, dismissals): (Vec<_>, Vec<_>) = messages.into_iter().partition(|message| {
        !matches!(
            message,
            Message::Modal(crate::shell::modal::Message::SettingsOverlay(
                crate::shell::settings_overlay::Message::Dismiss
                    | crate::shell::settings_overlay::Message::Settings(
                        crate::shell::settings::Message::ResetSavedServer
                    )
            ))
        )
    });
    ordinary.extend(dismissals);
    ordinary
}

type Ui<'a> = UserInterface<'a, Message, Theme, iced_wgpu::Renderer>;

#[derive(PartialEq, Eq)]
enum Flow {
    Continue,
    Stop,
}

#[derive(Default)]
struct FocusMemory {
    modal_identity: Option<Identity>,
    modal_tab: Option<crate::shell::settings_overlay::Tab>,
    prior: Option<Id>,
    cache_lost: bool,
    pending_move: Option<Direction>,
    settings_chain: Option<Box<dyn iced_core::widget::Operation>>,
}

struct FocusPlan {
    target: Option<Id>,
    restoration: Option<crate::shell::settings_overlay::Restoration>,
}

impl FocusMemory {
    fn plan(&mut self, model: &mut Model) -> FocusPlan {
        let identity = model.stack.identity();
        let cache_was_lost = self.cache_lost;
        let restoration = model.stack.settings_overlay_mut().and_then(|overlay| {
            if overlay.active() != crate::shell::settings_overlay::Tab::Settings {
                None
            } else {
                overlay
                    .take_restoration()
                    .or_else(|| cache_was_lost.then(|| overlay.restoration()))
            }
        });
        let tab = model.stack.active_settings_tab();
        let target = focus_after_rebuild(
            self.modal_identity,
            identity,
            self.modal_tab,
            tab,
            model.stack.initial_focus(&model.screen),
            self.prior.clone(),
            self.cache_lost,
        );
        self.modal_identity = identity;
        self.modal_tab = tab;
        self.cache_lost = false;
        FocusPlan {
            target,
            restoration,
        }
    }

    fn apply(&mut self, ui: &mut Ui<'_>, renderer: &iced_wgpu::Renderer, plan: FocusPlan) -> bool {
        if let Some(id) = plan.target {
            ui.operate(
                renderer,
                &mut iced_core::widget::operation::focusable::focus::<()>(id),
            );
        }
        if self.modal_identity == Some(Identity::SettingsOverlay)
            && let Some(mut operation) = self.settings_chain.take()
        {
            operate_all(ui, renderer, &mut *operation);
        } else if self.modal_identity != Some(Identity::SettingsOverlay) {
            self.settings_chain = None;
        }
        if let Some(restoration) = plan.restoration {
            if let Some(focus) = restoration.focus {
                ui.operate(
                    renderer,
                    &mut iced_core::widget::operation::focusable::focus::<()>(focus),
                );
            }
            ui.operate(
                renderer,
                &mut controls::restore_scroll(
                    crate::shell::settings::SETTINGS_SCROLL,
                    restoration.scroll,
                ),
            );
        }
        let mut chained = false;
        if let Some(direction) = self.pending_move.take() {
            let mut movement =
                controls::move_focus(crate::shell::settings::SETTINGS_SCROLL, direction);
            ui.operate(renderer, &mut movement);
            if let iced_core::widget::operation::Outcome::Chain(operation) = movement.finish() {
                self.settings_chain = Some(operation);
                chained = true;
            }
        }
        chained
    }
}

struct Updated {
    messages: Vec<Message>,
    ignored: Vec<Event>,
    outdated: bool,
    interaction: mouse::Interaction,
    redraw: Deadline,
}

struct Loop {
    actions: crate::shell::ApplicationActions,
    surface: SurfaceHandle,
    wake_tx: Sender<Work>,
    painter: Painter,
    model: Model,
    cache: user_interface::Cache,
    events: Vec<Event>,
    cursor: Cursor,
    waker: shell::Waker,
    redraw: Redraw,
    current: WindowExtent,
    focus: FocusMemory,
    pending: Deadline,
    immediate: bool,
    batch: Vec<Work>,
    queued: Vec<Apply>,
    deferred: Vec<Deferred>,
    last_primary: Option<(Id, u64)>,
    drew_nothing_yet: bool,
}

fn run(
    surface: SurfaceHandle,
    rx: &Receiver<Work>,
    wake_tx: &Sender<Work>,
    metadata: crate::shell::metadata::ApplicationMetadata,
    actions: crate::shell::ApplicationActions,
    on_ready: std::sync::mpsc::SyncSender<Result<(), LoopStartError>>,
) {
    let mut pending = Vec::new();
    let Some(target) = wait_for_target(surface, rx, wake_tx, &mut pending) else {
        crate::shell::publish_no_overlay();
        drop(on_ready.send(Err(LoopStartError::NoTarget)));
        return;
    };
    let mut state = match Loop::start(surface, wake_tx, target, metadata, actions) {
        Ok(state) => state,
        Err(error) => {
            drop(on_ready.send(Err(error)));
            crate::shell::publish_no_overlay();
            return;
        }
    };
    drop(on_ready.send(Ok(())));
    state.batch = pending;
    state.immediate = true;
    state.run(rx);
}

#[derive(Debug, thiserror::Error)]
pub enum LoopStartError {
    #[error("window target unavailable or startup canceled")]
    NoTarget,
    #[error("GPU device unavailable")]
    NoGpu,
    #[error("window extent unavailable")]
    NoExtent,
    #[error("swapchain creation failed: {0}")]
    Painter(#[source] jfn_gpu_paint::SurfaceLost),
}

impl Loop {
    fn start(
        surface: SurfaceHandle,
        wake_tx: &Sender<Work>,
        target: jfn_gpu_paint::WindowTarget,
        metadata: crate::shell::metadata::ApplicationMetadata,
        actions: crate::shell::ApplicationActions,
    ) -> Result<Loop, LoopStartError> {
        let gpu = jfn_gpu_paint::surfaces().ok_or(LoopStartError::NoGpu)?;
        let extent = initial_extent().ok_or(LoopStartError::NoExtent)?;
        let wake = {
            let tx = wake_tx.clone();
            Arc::new(move || {
                drop(tx.send(Work::Redraw));
            }) as Arc<dyn Fn() + Send + Sync>
        };
        let painter = Painter::new(gpu, target, extent, Arc::clone(&wake))
            .map_err(LoopStartError::Painter)?;
        let connection = crate::connection::Connection::new(jfn_config::server_url());
        let model = Model {
            stack: Stack::empty(metadata, crate::shell::settings::Settings::new),
            screen: connection.screen(),
            connection,
            titlebar: Titlebar::new(),
            inputs: crate::shell::chrome::inputs(),
            theme: Theme {
                chrome_background: crate::shell::theme::chrome_background(),
                ..Theme::default()
            },
        };
        let waker = shell::Waker::new({
            let wake = Arc::clone(&wake);
            move || wake()
        });
        publish(&model, extent);
        apply_visibility(surface, &model);
        Ok(Loop {
            actions,
            surface,
            wake_tx: wake_tx.clone(),
            painter,
            model,
            cache: user_interface::Cache::new(),
            events: Vec::new(),
            cursor: Cursor::Unavailable,
            waker,
            redraw: Redraw(wake_tx.clone()),
            current: extent,
            focus: FocusMemory {
                cache_lost: true,
                ..FocusMemory::default()
            },
            pending: Deadline::none(),
            immediate: false,
            batch: Vec::new(),
            queued: Vec::new(),
            deferred: Vec::new(),
            last_primary: None,
            drew_nothing_yet: true,
        })
    }

    fn run(mut self, rx: &Receiver<Work>) {
        loop {
            if !self.immediate && self.wait(rx) == Flow::Stop {
                break;
            }
            self.pending = Deadline::none();
            self.immediate = false;
            if self.drain(rx) == Flow::Stop {
                break;
            }
            if self.absorb_batch() == Flow::Stop {
                break;
            }
            self.pass();
        }
    }

    fn wait(&mut self, rx: &Receiver<Work>) -> Flow {
        let deadline = self.pending.merge(self.model.deadline());
        let now = Instant::now();
        if deadline.elapsed(now) {
            return Flow::Continue;
        }
        let blocked = match deadline.wait_for(now) {
            Some(timeout) => match rx.recv_timeout(timeout) {
                Ok(work) => Some(work),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return Flow::Stop,
            },
            None => match rx.recv() {
                Ok(work) => Some(work),
                Err(_) => return Flow::Stop,
            },
        };
        self.batch.extend(blocked);
        Flow::Continue
    }

    fn drain(&mut self, rx: &Receiver<Work>) -> Flow {
        loop {
            match rx.try_recv() {
                Ok(work) => self.batch.push(work),
                Err(TryRecvError::Empty) => return Flow::Continue,
                Err(TryRecvError::Disconnected) => return Flow::Stop,
            }
        }
    }

    fn absorb_batch(&mut self) -> Flow {
        let batch = std::mem::take(&mut self.batch);
        for work in batch {
            if self.absorb(work) == Flow::Stop {
                return Flow::Stop;
            }
        }
        Flow::Continue
    }

    fn absorb(&mut self, work: Work) -> Flow {
        match work {
            Work::Event(event) => {
                if let Event::Mouse(mouse::Event::CursorMoved { position }) = event {
                    self.cursor = Cursor::Available(position);
                }
                if matches!(event, Event::Mouse(mouse::Event::CursorLeft)) {
                    self.cursor = Cursor::Unavailable;
                }
                self.events.push(event);
            }
            Work::Resize { extent } => {
                self.painter.resize(extent);
                self.current = extent;
                self.discard_cache();
            }
            Work::Redraw => {}
            Work::OpenAbout => {
                self.model.transition(Transition::OpenAbout);
                self.discard_cache();
            }
            Work::OpenClientSettings => {
                self.model.transition(Transition::OpenClientSettings);
                self.discard_cache();
            }
            Work::Chrome(inputs) => self.model.inputs = inputs,
            Work::ChromeBackground(color) => self.model.theme.chrome_background = color,
            Work::SelectionText { reader, text } => match (reader, text) {
                (Reader::Iced, Some(text)) => {
                    self.events
                        .push(Event::Clipboard(clipboard::Event::Read(Ok(Arc::new(
                            clipboard::Content::Text(text),
                        )))));
                }
                (Reader::Field(id), Some(text)) => {
                    self.queued.push(Apply::act(id, Act::Paste(text)));
                }
                (_, None) => {}
            },
            Work::ContextMenu(p) => self.deferred.push(Deferred::ContextMenu(p)),
            Work::EditMenuAtCaret => self.deferred.push(Deferred::EditMenuAtCaret),
            Work::PrimaryPaste(p) => self.deferred.push(Deferred::PrimaryPaste(p)),
            Work::EditAt { field, command } => {
                self.deferred.push(Deferred::Edit(field, command));
            }
            Work::WebAttached(overlay) => self.model.connection.attach(overlay),
            Work::WebEvent(event) => {
                let connection = &mut self.model.connection;
                match event {
                    jfn_cef::WebEvent::ProbeFinished { cycle, base } => match base {
                        Some(base) => connection.probe_resolved(cycle, base),
                        None => connection.probe_failed(cycle),
                    },
                    jfn_cef::WebEvent::NavigationFailed(navigation) => {
                        connection.navigation_failed(navigation)
                    }
                    jfn_cef::WebEvent::FramePresented(presented) => {
                        connection.frame_presented(presented)
                    }
                }
                connection.flush_requests();
            }
            Work::Shutdown => return Flow::Stop,
        }
        Flow::Continue
    }

    fn discard_cache(&mut self) {
        self.cache = user_interface::Cache::new();
        self.focus.cache_lost = true;
    }

    fn pass(&mut self) {
        self.model.transition(Transition::Tick(Instant::now()));
        self.model.screen = self.model.connection.screen();
        self.model.stack.reconcile(&self.model.screen);
        self.pending = self.pending.merge(
            self.model
                .connection
                .deadline()
                .map_or_else(Deadline::none, Deadline::at),
        );
        self.model.theme.backdrop = self.model.backdrop();
        let plan = self.focus.plan(&mut self.model);

        let this = &mut *self;
        let model = &this.model;
        let painter = &mut this.painter;
        let mut ui = UserInterface::build(
            model.view(),
            Size::new(
                this.current.logical().w as f32,
                this.current.logical().h as f32,
            ),
            std::mem::replace(&mut this.cache, user_interface::Cache::new()),
            painter.renderer(),
        );
        if this.focus.apply(&mut ui, painter.renderer(), plan) {
            this.immediate = true;
        }
        resolve_deferred(
            &mut ui,
            painter,
            model,
            &mut this.deferred,
            &mut this.queued,
            &this.wake_tx,
            this.actions,
        );
        let retained_settings = retained_settings(&mut ui, painter, model);

        let updated = update_ui(
            &mut ui,
            painter,
            &mut this.events,
            this.cursor,
            &this.waker,
            &this.wake_tx,
        );
        this.pending = this.pending.merge(updated.redraw);
        let settled_fields = Fields::collect(&mut ui, painter.renderer());
        let mut focused = controls::focused_id();
        ui.operate(painter.renderer(), &mut focused);
        this.focus.prior = focused.get();
        publish_primary(&settled_fields, &mut this.last_primary);
        jfn_input::publish_field_edit(
            settled_fields
                .focused()
                .map(crate::shell::fields::Snapshot::edit_state),
        );
        crate::shell::router_sink::set_interaction(updated.interaction);

        let settled = updated.messages.is_empty()
            && !updated
                .ignored
                .iter()
                .any(|event| ignored_key(model, event).is_some())
            && !updated.outdated;

        if settled {
            if this.drew_nothing_yet {
                this.drew_nothing_yet = false;
                if let Err(error) = crate::shell::wait_fonts_ready() {
                    tracing::error!("shell cannot paint: {error}");
                    return;
                }
            }
            match paint(
                painter,
                this.surface,
                &mut ui,
                model,
                this.cursor,
                &this.redraw,
            ) {
                Painted::Shown(_) | Painted::Hidden | Painted::Requested => {}
                Painted::Deferred(retry_at) => {
                    this.pending = this.pending.merge(Deadline::at(retry_at));
                }
            }
        }
        this.cache = ui.into_cache();
        if let Some((focus, offset)) = retained_settings
            && let Some(overlay) = this.model.stack.settings_overlay_mut()
        {
            overlay.retain_settings_state(focus, offset);
        }
        if settled {
            publish(&this.model, this.current);
            return;
        }

        this.model.apply_message_batch(updated.messages);
        for event in &updated.ignored {
            match ignored_key(&this.model, event) {
                Some(IgnoredKey::Escape) => this.model.transition(Transition::Escape),
                Some(IgnoredKey::Focus(direction)) => this.focus.pending_move = Some(direction),
                None => {}
            }
        }
        this.immediate = true;
        publish(&this.model, this.current);
        apply_visibility(this.surface, &this.model);
    }
}

fn resolve_deferred(
    ui: &mut Ui<'_>,
    painter: &mut Painter,
    model: &Model,
    deferred: &mut Vec<Deferred>,
    queued: &mut Vec<Apply>,
    wake_tx: &Sender<Work>,
    actions: crate::shell::ApplicationActions,
) {
    let fields = Fields::collect(ui, painter.renderer());
    let mut menu_anchor = None;
    for request in deferred.drain(..) {
        match request {
            Deferred::ContextMenu(p) => {
                menu_anchor = menu_anchor.or(raise_menu(
                    &fields,
                    p,
                    queued,
                    model.stack.identity() == Some(Identity::SettingsOverlay),
                    actions,
                ));
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
                queue_edit(&fields, &target, command, queued, wake_tx);
            }
        }
    }
    for text in apply_queued(ui, painter.renderer(), queued) {
        if let Some(lease) = jfn_platform_abi::try_lease() {
            lease.platform().clipboard_write_text(&text);
        }
    }
    if let Some(anchor) = menu_anchor {
        let raised = Fields::collect(ui, painter.renderer());
        if let Some(field) = raised.at(point(anchor.x, anchor.y)) {
            crate::shell::menu::open_edit(field, anchor, crate::shell::lang::strings());
        }
    }
}

fn retained_settings(
    ui: &mut Ui<'_>,
    painter: &mut Painter,
    model: &Model,
) -> Option<(
    Option<Id>,
    iced_core::widget::operation::scrollable::AbsoluteOffset,
)> {
    if model.stack.active_settings_tab() != Some(crate::shell::settings_overlay::Tab::Settings) {
        return None;
    }
    let mut focused = controls::focused_id();
    ui.operate(painter.renderer(), &mut focused);
    let mut offset = controls::scroll_offset(crate::shell::settings::SETTINGS_SCROLL);
    ui.operate(painter.renderer(), &mut offset);
    offset.get().map(|offset| (focused.get(), offset))
}

fn update_ui(
    ui: &mut Ui<'_>,
    painter: &mut Painter,
    events: &mut Vec<Event>,
    cursor: Cursor,
    waker: &shell::Waker,
    wake_tx: &Sender<Work>,
) -> Updated {
    events.push(Event::Window(
        window::Event::RedrawRequested(Instant::now()),
    ));
    let mut bus = shell::Bus::new();
    let (state, statuses) = ui.update(
        &window::Headless,
        waker,
        events,
        cursor,
        painter.renderer(),
        &mut bus,
    );
    let ignored: Vec<Event> = events
        .iter()
        .zip(statuses)
        .filter(|(_, status)| *status == iced_core::event::Status::Ignored)
        .map(|(event, _)| event.clone())
        .collect();
    events.clear();

    let messages: Vec<Message> = bus.drain().collect();
    let (interaction, redraw) = match &state {
        user_interface::State::Updated {
            mouse_interaction,
            redraw_request,
            ..
        } => (
            *mouse_interaction,
            match redraw_request {
                window::RedrawRequest::NextFrame => Deadline::at(Instant::now()),
                window::RedrawRequest::At(at) => Deadline::at(*at),
                window::RedrawRequest::Wait => Deadline::none(),
            },
        ),
        user_interface::State::Outdated => (mouse::Interaction::None, Deadline::none()),
    };
    if let user_interface::State::Updated { clipboard, .. } = &state {
        write_clipboard(clipboard);
        if clipboard.reads.contains(&clipboard::Kind::Text) {
            read_clipboard(wake_tx.clone(), Reader::Iced);
        }
    }
    Updated {
        messages,
        ignored,
        outdated: matches!(state, user_interface::State::Outdated),
        interaction,
        redraw,
    }
}

enum Painted {
    Shown(Presented),
    Hidden,
    Deferred(Instant),
    Requested,
}

struct Redraw(Sender<Work>);

impl FrameSource for Redraw {
    fn request_frame(&self) {
        drop(self.0.send(Work::Redraw));
    }
}

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

enum Deferred {
    ContextMenu(LogicalPoint),
    EditMenuAtCaret,
    PrimaryPaste(LogicalPoint),
    Edit(Target, jfn_input::EditCommand),
}

fn operate_all<Message>(
    ui: &mut UserInterface<'_, Message, Theme, iced_wgpu::Renderer>,
    renderer: &iced_wgpu::Renderer,
    operation: &mut dyn iced_core::widget::Operation,
) {
    ui.operate(renderer, operation);
    let mut outcome = operation.finish();
    while let iced_core::widget::operation::Outcome::Chain(mut next) = outcome {
        ui.operate(renderer, &mut *next);
        outcome = next.finish();
    }
}

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

fn read_clipboard(tx: Sender<Work>, reader: Reader) {
    let Some(lease) = jfn_platform_abi::try_lease() else {
        return;
    };
    lease
        .platform()
        .clipboard_read_text_async(Box::new(move |text| {
            drop(tx.send(Work::SelectionText {
                reader,
                text: text.map(str::to_owned),
            }));
        }));
}

fn read_primary(tx: Sender<Work>, reader: Reader) {
    let Some(lease) = jfn_platform_abi::try_lease() else {
        return;
    };
    let plat = lease.platform();
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

fn write_clipboard(clipboard: &clipboard::Clipboard) {
    if let Some(clipboard::Content::Text(text)) = &clipboard.write
        && let Some(lease) = jfn_platform_abi::try_lease()
    {
        lease.platform().clipboard_write_text(text);
    }
}

fn publish_primary(fields: &Fields, last: &mut Option<(Id, u64)>) {
    let Some(lease) = jfn_platform_abi::try_lease() else {
        return;
    };
    let plat = lease.platform();
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

fn caret_anchor(field: &crate::shell::fields::Snapshot) -> LogicalPoint {
    LogicalPoint {
        x: field.caret.x as i32,
        y: field.caret.y as i32,
    }
}

fn raise_menu(
    fields: &Fields,
    p: LogicalPoint,
    queued: &mut Vec<Apply>,
    restricted: bool,
    actions: crate::shell::ApplicationActions,
) -> Option<LogicalPoint> {
    let at = point(p.x, p.y);
    let Some(field) = fields.at(at) else {
        (actions.open_menu)(p, restricted);
        return None;
    };
    let lease = jfn_platform_abi::try_lease()?;
    let backend = lease.platform().display();
    let caret = crate::shell::fields::press_caret(backend, field, at);
    if crate::shell::fields::press_focuses(backend) {
        queued.push(Apply::focus(field.id.clone(), caret));
    } else if let Some(act) = caret {
        queued.push(Apply::act(field.id.clone(), act));
    }
    Some(p)
}

fn publish(model: &Model, extent: WindowExtent) {
    jfn_input::publish_shell_state(crate::shell::state::shell_state(
        Some(extent),
        model.inputs,
        model.stack.occupied(),
    ));
}

fn apply_visibility(surface: SurfaceHandle, model: &Model) -> Visibility {
    let Some(lease) = jfn_platform_abi::try_lease() else {
        return Visibility::Hidden;
    };
    lease
        .platform()
        .set_surface_visibility(surface, model.visibility())
        .acknowledged()
}

fn wait_for_target(
    surface: SurfaceHandle,
    rx: &Receiver<Work>,
    wake_tx: &Sender<Work>,
    pending: &mut Vec<Work>,
) -> Option<jfn_gpu_paint::WindowTarget> {
    let lease = jfn_platform_abi::try_lease()?;
    let plat = lease.platform();
    let tx = wake_tx.clone();
    plat.on_surface_target_ready(
        surface,
        Box::new(move || {
            drop(tx.send(Work::Redraw));
        }),
    );
    await_target(rx, pending, Instant::now() + TARGET_WAIT, || {
        plat.surface_window_target(surface)
    })
}

fn await_target<T>(
    rx: &Receiver<Work>,
    pending: &mut Vec<Work>,
    deadline: Instant,
    mut query: impl FnMut() -> Option<T>,
) -> Option<T> {
    loop {
        if let Some(target) = query() {
            return Some(target);
        }
        if Instant::now() >= deadline {
            tracing::error!("shell: no window target for the overlay surface");
            return None;
        }
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Work::Shutdown) | Err(RecvTimeoutError::Disconnected) => return None,
            Ok(work) => pending.push(work),
            Err(RecvTimeoutError::Timeout) => {
                tracing::error!("shell: no window target for the overlay surface");
                return None;
            }
        }
    }
}

fn initial_extent() -> Option<WindowExtent> {
    let lease = jfn_platform_abi::try_lease()?;
    let plat = lease.platform();
    if let Some(extent) = plat.window_owner().source().snapshot().extent {
        return Some(extent);
    }
    let scale = plat.scale();
    let logical = LogicalSize { w: 1280, h: 720 };
    WindowExtent::new(logical.to_physical(scale)?, scale, logical)
}

pub(crate) fn point(x: i32, y: i32) -> Point {
    Point::new(x as f32, y as f32)
}

#[cfg(test)]
mod tests {
    #[test]
    fn bounded_join_observes_exit_and_panic() {
        assert_eq!(
            super::join_bounded(std::thread::spawn(|| {}), super::SHUTDOWN_TIMEOUT),
            super::JoinOutcome::Terminated
        );
        assert_eq!(
            super::join_bounded(
                std::thread::spawn(|| std::panic::resume_unwind(Box::new("test panic"))),
                super::SHUTDOWN_TIMEOUT
            ),
            super::JoinOutcome::Panicked
        );
    }

    #[test]
    fn bounded_join_does_not_wait_for_a_blocked_worker() {
        let (tx, rx) = super::channel::<()>();
        let thread = std::thread::spawn(move || {
            rx.recv().unwrap_or_default();
        });
        assert_eq!(
            super::join_bounded(thread, super::Duration::ZERO),
            super::JoinOutcome::TimedOut
        );
        drop(tx);
    }

    #[test]
    fn target_wait_preserves_work_and_observes_readiness() {
        let (tx, rx) = super::channel();
        tx.send(super::Work::OpenAbout).unwrap();
        let mut pending = Vec::new();
        let mut queries = 0;
        let target = super::await_target(
            &rx,
            &mut pending,
            super::Instant::now() + super::TARGET_WAIT,
            || {
                queries += 1;
                (queries == 2).then_some(42)
            },
        );
        assert_eq!(target, Some(42));
        assert!(matches!(pending.as_slice(), [super::Work::OpenAbout]));
    }

    #[test]
    fn target_wait_stops_on_shutdown_or_deadline() {
        let (tx, rx) = super::channel();
        tx.send(super::Work::Shutdown).unwrap();
        let mut pending = Vec::new();
        assert_eq!(
            super::await_target(
                &rx,
                &mut pending,
                super::Instant::now() + super::TARGET_WAIT,
                || None::<()>
            ),
            None
        );
        tx.send(super::Work::Redraw).unwrap();
        assert_eq!(
            super::await_target(&rx, &mut pending, super::Instant::now(), || None::<()>),
            None
        );
        assert!(pending.is_empty());
    }

    use super::*;
    use iced_core::keyboard::key::{NativeCode, Physical};
    use iced_core::keyboard::{Location, Modifiers};

    fn key(name: iced_core::keyboard::key::Named, modifiers: Modifiers) -> Event {
        let key = iced_core::keyboard::Key::Named(name);
        Event::Keyboard(iced_core::keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: Physical::Unidentified(NativeCode::Unidentified),
            location: Location::Standard,
            modifiers,
            text: None,
            repeat: false,
        })
    }

    fn settings_message(message: crate::shell::settings::Message) -> Message {
        Message::Modal(crate::shell::modal::Message::SettingsOverlay(
            crate::shell::settings_overlay::Message::Settings(message),
        ))
    }

    #[test]
    fn settings_dismiss_is_applied_after_final_edits_without_reordering_ordinary_messages() {
        let messages = vec![
            Message::Modal(crate::shell::modal::Message::SettingsOverlay(
                crate::shell::settings_overlay::Message::Dismiss,
            )),
            settings_message(crate::shell::settings::Message::DeviceNameEdited(
                "final device".to_owned(),
            )),
            settings_message(crate::shell::settings::Message::CommitDeviceName),
            settings_message(crate::shell::settings::Message::AudioPassthroughEdited(
                "first audio".to_owned(),
            )),
            settings_message(crate::shell::settings::Message::AudioPassthroughEdited(
                "final audio".to_owned(),
            )),
        ];

        let ordered = settings_overlay_dismiss_last(messages.clone());
        assert!(matches!(
            &ordered[0],
            Message::Modal(crate::shell::modal::Message::SettingsOverlay(
                crate::shell::settings_overlay::Message::Settings(
                    crate::shell::settings::Message::DeviceNameEdited(value)
                )
            )) if value == "final device"
        ));
        assert!(matches!(
            &ordered[1],
            Message::Modal(crate::shell::modal::Message::SettingsOverlay(
                crate::shell::settings_overlay::Message::Settings(
                    crate::shell::settings::Message::CommitDeviceName
                )
            ))
        ));
        assert!(matches!(
            &ordered[2],
            Message::Modal(crate::shell::modal::Message::SettingsOverlay(
                crate::shell::settings_overlay::Message::Settings(
                    crate::shell::settings::Message::AudioPassthroughEdited(value)
                )
            )) if value == "first audio"
        ));
        assert!(matches!(
            &ordered[3],
            Message::Modal(crate::shell::modal::Message::SettingsOverlay(
                crate::shell::settings_overlay::Message::Settings(
                    crate::shell::settings::Message::AudioPassthroughEdited(value)
                )
            )) if value == "final audio"
        ));
        assert!(matches!(
            &ordered[4],
            Message::Modal(crate::shell::modal::Message::SettingsOverlay(
                crate::shell::settings_overlay::Message::Dismiss
            ))
        ));

        let mut model = Model {
            stack: Stack::testing_settings(),
            screen: crate::connection::Screen::Gone,
            connection: crate::connection::Connection::new(String::new()),
            titlebar: Titlebar::new(),
            inputs: ChromeInputs::default(),
            theme: Theme::default(),
        };
        model.apply_message_batch(messages);

        assert!(!model.stack.occupied());
    }

    #[test]
    fn settings_reset_is_applied_after_final_edits_without_reordering_ordinary_messages() {
        let messages = vec![
            settings_message(crate::shell::settings::Message::ResetSavedServer),
            settings_message(crate::shell::settings::Message::DeviceNameEdited(
                "first device".to_owned(),
            )),
            settings_message(crate::shell::settings::Message::AudioPassthroughEdited(
                "first audio".to_owned(),
            )),
            settings_message(crate::shell::settings::Message::DeviceNameEdited(
                "final device".to_owned(),
            )),
            settings_message(crate::shell::settings::Message::AudioPassthroughEdited(
                "final audio".to_owned(),
            )),
        ];

        let ordered = settings_overlay_dismiss_last(messages);
        assert!(matches!(
            &ordered[0],
            Message::Modal(crate::shell::modal::Message::SettingsOverlay(
                crate::shell::settings_overlay::Message::Settings(
                    crate::shell::settings::Message::DeviceNameEdited(value)
                )
            )) if value == "first device"
        ));
        assert!(matches!(
            &ordered[1],
            Message::Modal(crate::shell::modal::Message::SettingsOverlay(
                crate::shell::settings_overlay::Message::Settings(
                    crate::shell::settings::Message::AudioPassthroughEdited(value)
                )
            )) if value == "first audio"
        ));
        assert!(matches!(
            &ordered[2],
            Message::Modal(crate::shell::modal::Message::SettingsOverlay(
                crate::shell::settings_overlay::Message::Settings(
                    crate::shell::settings::Message::DeviceNameEdited(value)
                )
            )) if value == "final device"
        ));
        assert!(matches!(
            &ordered[3],
            Message::Modal(crate::shell::modal::Message::SettingsOverlay(
                crate::shell::settings_overlay::Message::Settings(
                    crate::shell::settings::Message::AudioPassthroughEdited(value)
                )
            )) if value == "final audio"
        ));
        assert!(matches!(
            &ordered[4],
            Message::Modal(crate::shell::modal::Message::SettingsOverlay(
                crate::shell::settings_overlay::Message::Settings(
                    crate::shell::settings::Message::ResetSavedServer
                )
            ))
        ));

        let mut model = Model {
            stack: Stack::testing_settings(),
            screen: crate::connection::Screen::Gone,
            connection: crate::connection::Connection::new(String::new()),
            titlebar: Titlebar::new(),
            inputs: ChromeInputs::default(),
            theme: Theme::default(),
        };
        for message in ordered[..4].iter().cloned() {
            model.update(message);
        }

        let settings = model
            .stack
            .settings_overlay_mut()
            .expect("reset must remain pending")
            .settings();
        assert_eq!(settings.device_name, "final device");
        assert_eq!(settings.audio_passthrough, "final audio");

        model.update(ordered[4].clone());
        assert!(!model.stack.occupied());
    }

    #[test]
    fn about_selection_retains_pre_event_settings_focus_and_scroll() {
        let mut model = Model {
            stack: Stack::testing_settings(),
            screen: crate::connection::Screen::Gone,
            connection: crate::connection::Connection::new(String::new()),
            titlebar: Titlebar::new(),
            inputs: ChromeInputs::default(),
            theme: Theme::default(),
        };
        let pre_event_focus = Some(crate::shell::settings::DEVICE_NAME_FIELD);
        let pre_event_scroll =
            iced_core::widget::operation::scrollable::AbsoluteOffset { x: 3.0, y: 142.0 };
        let settled_focus: Option<Id> = None;

        assert!(
            model
                .stack
                .settings_overlay_mut()
                .map(|overlay| {
                    overlay.retain_settings_state(pre_event_focus.clone(), pre_event_scroll);
                })
                .is_some()
        );
        model.apply_message_batch(vec![Message::Modal(
            crate::shell::modal::Message::SettingsOverlay(
                crate::shell::settings_overlay::Message::Select(
                    crate::shell::settings_overlay::Tab::About,
                ),
            ),
        )]);
        model.transition(Transition::OpenClientSettings);

        assert_eq!(settled_focus, None);
        assert_eq!(
            model
                .stack
                .settings_overlay_mut()
                .and_then(crate::shell::settings_overlay::SettingsOverlay::take_restoration),
            Some(crate::shell::settings_overlay::Restoration {
                focus: pre_event_focus,
                scroll: pre_event_scroll,
            })
        );
    }

    #[test]
    fn settings_messages_preserve_the_current_focus_target() {
        let focused = Id::new("focused-setting");
        assert_eq!(
            focus_after_rebuild(
                Some(Identity::SettingsOverlay),
                Some(Identity::SettingsOverlay),
                Some(crate::shell::settings_overlay::Tab::Settings),
                Some(crate::shell::settings_overlay::Tab::Settings),
                Some(Id::new("initial")),
                Some(focused),
                false,
            ),
            None
        );
    }

    #[test]
    fn tab_changes_choose_the_active_tabs_initial_target() {
        let initial = Id::new("new-initial");
        assert_eq!(
            focus_after_rebuild(
                Some(Identity::SettingsOverlay),
                Some(Identity::SettingsOverlay),
                Some(crate::shell::settings_overlay::Tab::About),
                Some(crate::shell::settings_overlay::Tab::Settings),
                Some(initial.clone()),
                Some(Id::new("old-focus")),
                true,
            ),
            Some(initial)
        );
    }

    #[test]
    fn resize_or_cache_loss_restores_the_prior_target() {
        let prior = Id::new("prior-focus");
        assert_eq!(
            focus_after_rebuild(
                Some(Identity::SettingsOverlay),
                Some(Identity::SettingsOverlay),
                Some(crate::shell::settings_overlay::Tab::Settings),
                Some(crate::shell::settings_overlay::Tab::Settings),
                Some(Id::new("initial")),
                Some(prior.clone()),
                true,
            ),
            Some(prior)
        );
    }

    #[test]
    fn fresh_overlay_uses_the_active_tabs_initial_target() {
        let initial = Id::new("fresh-initial");
        assert_eq!(
            focus_after_rebuild(
                None,
                Some(Identity::SettingsOverlay),
                None,
                Some(crate::shell::settings_overlay::Tab::Settings),
                Some(initial.clone()),
                None,
                true,
            ),
            Some(initial)
        );
    }

    #[test]
    fn tab_directions_are_forward_and_backward_in_settings() {
        let model = Model {
            stack: Stack::testing_settings(),
            screen: crate::connection::Screen::Gone,
            connection: crate::connection::Connection::new(String::new()),
            titlebar: Titlebar::new(),
            inputs: ChromeInputs::default(),
            theme: Theme::default(),
        };

        assert_eq!(
            ignored_key(
                &model,
                &key(iced_core::keyboard::key::Named::Tab, Modifiers::empty())
            ),
            Some(IgnoredKey::Focus(Direction::Forward))
        );
        assert_eq!(
            ignored_key(
                &model,
                &key(iced_core::keyboard::key::Named::Tab, Modifiers::SHIFT)
            ),
            Some(IgnoredKey::Focus(Direction::Backward))
        );
    }
}
