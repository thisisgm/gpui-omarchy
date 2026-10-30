use std::{
    cell::RefCell,
    env,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};
#[cfg(any(feature = "wayland", feature = "x11"))]
use std::{ffi::OsString, fs::File, os::fd::AsFd, time::Duration};

#[cfg(any(feature = "wayland", feature = "x11"))]
use anyhow::ensure;
use anyhow::{Context as _, anyhow};
#[cfg(any(feature = "wayland", feature = "x11"))]
use ashpd::{
    desktop::{
        Request,
        inhibit::{InhibitFlags, InhibitOptions, InhibitProxy},
    },
    enumflags2::BitFlags,
};
use calloop::{EventLoop, LoopHandle, LoopSignal, channel::Sender};
use futures::channel::oneshot;
use gpui_util::{ResultExt as _, new_std_command};
#[cfg(any(feature = "wayland", feature = "x11"))]
use xkbcommon::xkb::{self, Keycode, Keysym, State};

use crate::linux::{
    Backend, DisplayConnection, HeadlessConnection, LinuxDispatcher, PriorityQueueCalloopReceiver,
    select_backend,
};
use gpui::{
    Action, ActivityGuard, AnyWindowHandle, BackgroundExecutor, ClipboardItem, CursorStyle,
    ForegroundExecutor, GraphicalEnvironment, Keymap, Menu, MenuItem, OwnedMenu, PathPromptOptions,
    Platform, PlatformDisplay, PlatformKeyboardLayout, PlatformKeyboardMapper, PlatformTextSystem,
    PlatformWindow, Result, RunnableVariant, Task, ThermalState, WindowAppearance,
    WindowButtonLayout, WindowParams, WindowingModes, WindowingRequest,
};
#[cfg(any(feature = "wayland", feature = "x11"))]
use gpui::{Pixels, Point, px};

#[cfg(any(feature = "wayland", feature = "x11"))]
pub(crate) const SCROLL_LINES: f32 = 3.0;

// Values match the defaults on GTK.
// Taken from https://github.com/GNOME/gtk/blob/main/gtk/gtksettings.c#L320
#[cfg(any(feature = "wayland", feature = "x11"))]
pub(crate) const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(400);
#[cfg(any(feature = "wayland", feature = "x11"))]
pub(crate) const DOUBLE_CLICK_DISTANCE: Pixels = px(5.0);
pub(crate) const KEYRING_LABEL: &str = "zed-github-account";

#[cfg(any(feature = "wayland", feature = "x11"))]
const FILE_PICKER_PORTAL_MISSING: &str =
    "Couldn't open file picker due to missing xdg-desktop-portal implementation.";

#[derive(Default)]
pub(crate) struct PlatformHandlers {
    pub(crate) clipboard_change: Option<Box<dyn FnMut(gpui::ClipboardChange)>>,
    pub(crate) native_offer:
        Option<Box<dyn FnMut(gpui::NativeDragOffer) -> gpui::NativeDragActions>>,
    pub(crate) native_drop: Option<Box<dyn FnMut(gpui::NativeDropEvent) -> bool>>,
    pub(crate) native_drag: Option<Box<dyn FnMut(gpui::NativeDragEvent)>>,
    pub(crate) open_urls: Option<Box<dyn FnMut(Vec<String>)>>,
    pub(crate) quit: Option<Box<dyn FnMut() -> bool>>,
    pub(crate) reopen: Option<Box<dyn FnMut()>>,
    pub(crate) app_menu_action: Option<Box<dyn FnMut(&dyn Action)>>,
    pub(crate) will_open_app_menu: Option<Box<dyn FnMut()>>,
    pub(crate) validate_app_menu_command: Option<Box<dyn FnMut(&dyn Action) -> bool>>,
    pub(crate) keyboard_layout_change: Option<Box<dyn FnMut()>>,
    pub(crate) system_sleep: Option<Box<dyn FnMut()>>,
    pub(crate) system_wake: Option<Box<dyn FnMut()>>,
    pub(crate) displays_changed: Option<Box<dyn FnMut()>>,
}

/// A logind `PrepareForSleep` signal, forwarded from the D-Bus listener to
/// the client's event loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(all(target_os = "linux", any(feature = "wayland", feature = "x11"))),
    allow(dead_code)
)]
pub(crate) enum SystemPowerEvent {
    Sleep,
    Wake,
}

pub(crate) struct LinuxCommon {
    pub(crate) background_executor: BackgroundExecutor,
    pub(crate) foreground_executor: ForegroundExecutor,
    pub(crate) text_system: Arc<dyn PlatformTextSystem>,
    pub(crate) appearance: WindowAppearance,
    pub(crate) auto_hide_scrollbars: bool,
    pub(crate) button_layout: WindowButtonLayout,
    pub(crate) callbacks: PlatformHandlers,
    pub(crate) signal: LoopSignal,
    pub(crate) menus: Vec<OwnedMenu>,
    app_name: Option<String>,
    system_notifications: crate::linux::system_notifications::SystemNotificationState,
    #[cfg_attr(
        not(all(target_os = "linux", any(feature = "wayland", feature = "x11"))),
        allow(dead_code)
    )]
    power_sender: Sender<SystemPowerEvent>,
    power_listener_started: bool,
    quitting: bool,
    /// Runs after each foreground runnable registered by [`LinuxCommon::register_sources`].
    ///
    /// X11 uses this to handle events that a runnable's requests read into xcb's queue, where
    /// they would not wake the event loop.
    pub(crate) after_runnable: Option<Rc<dyn Fn()>>,
    /// The mapper of the active keyboard layout generation; the dummy mapper while no keymap is known.
    pub(crate) keyboard_mapper: Option<Rc<dyn PlatformKeyboardMapper>>,
}

impl LinuxCommon {
    pub fn new(
        signal: LoopSignal,
    ) -> (
        Self,
        PriorityQueueCalloopReceiver<RunnableVariant>,
        calloop::channel::Channel<SystemPowerEvent>,
    ) {
        let (main_sender, main_receiver) = PriorityQueueCalloopReceiver::new();
        let (power_sender, power_receiver) = calloop::channel::channel();

        #[cfg(any(feature = "wayland", feature = "x11"))]
        // Omarchy: fonts are read through fontconfig when first used, not all parsed at startup.
        let text_system = Arc::new(crate::linux::CosmicTextSystem::new_on_demand(
            "IBM Plex Sans",
        ));
        #[cfg(not(any(feature = "wayland", feature = "x11")))]
        let text_system = Arc::new(gpui::NoopTextSystem::new());

        let callbacks = PlatformHandlers::default();

        let dispatcher = Arc::new(LinuxDispatcher::new(main_sender));

        let background_executor = BackgroundExecutor::new(dispatcher.clone());

        let common = LinuxCommon {
            background_executor,
            foreground_executor: ForegroundExecutor::new(dispatcher),
            text_system,
            appearance: WindowAppearance::Light,
            auto_hide_scrollbars: false,
            button_layout: WindowButtonLayout::linux_default(),
            callbacks,
            signal,
            menus: Vec::new(),
            app_name: None,
            system_notifications: crate::linux::system_notifications::SystemNotificationState::new(
            ),
            power_sender,
            power_listener_started: false,
            quitting: false,
            after_runnable: None,
            keyboard_mapper: None,
        };

        (common, main_receiver, power_receiver)
    }

    pub(crate) fn start_power_listener(&mut self) {
        if !self.power_listener_started {
            #[cfg(all(target_os = "linux", any(feature = "wayland", feature = "x11")))]
            smol::spawn({
                let power_sender = self.power_sender.clone();
                async move {
                    if let Err(error) = listen_for_system_power_events(power_sender).await {
                        log::debug!("failed to listen for system sleep/wake events: {error:?}");
                    }
                }
            })
            .detach();

            self.power_listener_started = true;
        }
    }

    /// Registers the foreground executor and power listener on a loop shared with display clients.
    ///
    /// Foreground runnables run as idle callbacks, after the loop has dispatched all pending events.
    pub(crate) fn register_sources(
        common: &Rc<RefCell<Self>>,
        loop_handle: &calloop::LoopHandle<'static, ()>,
        main_receiver: PriorityQueueCalloopReceiver<RunnableVariant>,
        power_receiver: calloop::channel::Channel<SystemPowerEvent>,
    ) -> anyhow::Result<()> {
        loop_handle
            .insert_source(main_receiver, {
                let loop_handle = loop_handle.downgrade();
                let common = common.clone();
                move |event, _, _| {
                    if let calloop::channel::Event::Msg(runnable) = event
                        && let Some(loop_handle) = loop_handle.upgrade()
                    {
                        let common = common.clone();
                        loop_handle.insert_idle(move |_| {
                            let location = runnable.metadata().location;
                            let spawned = runnable.metadata().spawned;
                            gpui::profiler::update_running_task(spawned, location);
                            runnable.run();
                            gpui::profiler::save_task_timing();

                            let after_runnable = common.borrow().after_runnable.clone();
                            if let Some(after_runnable) = after_runnable {
                                after_runnable();
                            }
                        });
                    }
                }
            })
            .map_err(|error| anyhow!("failed to register foreground executor: {error}"))?;
        loop_handle
            .insert_source(power_receiver, {
                let common = common.clone();
                move |event, _, _| {
                    if let calloop::channel::Event::Msg(event) = event {
                        LinuxCommon::handle_system_power_event(&common, event);
                    }
                }
            })
            .map_err(|error| anyhow!("failed to register power listener: {error}"))?;
        Ok(())
    }

    /// Runs the app's sleep or wake callback, without `common` borrowed, since the callback
    /// can call back into the platform.
    fn handle_system_power_event(common: &RefCell<Self>, event: SystemPowerEvent) {
        fn slot(
            callbacks: &mut PlatformHandlers,
            event: SystemPowerEvent,
        ) -> &mut Option<Box<dyn FnMut()>> {
            match event {
                SystemPowerEvent::Sleep => &mut callbacks.system_sleep,
                SystemPowerEvent::Wake => &mut callbacks.system_wake,
            }
        }
        let callback = slot(&mut common.borrow_mut().callbacks, event).take();
        if let Some(mut callback) = callback {
            callback();
            let mut common = common.borrow_mut();
            let slot = slot(&mut common.callbacks, event);
            // The callback may have replaced itself.
            if slot.is_none() {
                *slot = Some(callback);
            }
        }
    }
}

#[cfg(all(target_os = "linux", any(feature = "wayland", feature = "x11")))]
async fn listen_for_system_power_events(
    power_sender: Sender<SystemPowerEvent>,
) -> anyhow::Result<()> {
    use futures::StreamExt as _;

    let connection = ashpd::zbus::Connection::system().await?;
    let proxy = ashpd::zbus::Proxy::new(
        &connection,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await?;
    let mut sleep_events = proxy.receive_signal("PrepareForSleep").await?;

    while let Some(message) = sleep_events.next().await {
        let event = if message.body().deserialize::<bool>()? {
            SystemPowerEvent::Sleep
        } else {
            SystemPowerEvent::Wake
        };
        power_sender.send(event).ok();
    }

    Ok(())
}

/// GPUI's platform for Linux and FreeBSD.
///
/// One event loop serves the whole process. The foreground executor and power listener are
/// registered on it once, and the current [`DisplayConnection`] adds and removes its own sources
/// as the app switches between the display modes it allows.
pub(crate) struct LinuxPlatform {
    common: Rc<RefCell<LinuxCommon>>,
    /// The event loop, until `run` takes it.
    event_loop: RefCell<Option<EventLoop<'static, ()>>>,
    loop_handle: LoopHandle<'static, ()>,
    connection: RefCell<DisplayConnection>,
    allowed_modes: WindowingModes,
    /// The mode to start in, applied when `run` starts. Windowed in the process's own
    /// environment when unset.
    initial_windowing: RefCell<Option<WindowingRequest>>,
    /// Taken by the connection made when `run` starts.
    startup_environment: RefCell<Option<StartupEnvironment>>,
    /// A requested switch waiting for the event loop to apply it.
    pending_mode: RefCell<Option<WindowingRequest>>,
    transition_waiter: RefCell<Option<oneshot::Sender<anyhow::Result<()>>>>,
}

/// What a Wayland connection made at startup may use from this process's environment.
///
/// Both are taken from the environment when the platform is created, before it starts any
/// threads, and are only meaningful for the connection made then.
#[derive(Default)]
struct StartupEnvironment {
    #[cfg(feature = "wayland")]
    activation_token: Option<String>,
    /// The socket `WAYLAND_SOCKET` names: a connection to the compositor that the process which
    /// started us already opened.
    #[cfg(feature = "wayland")]
    wayland_socket: Option<std::os::unix::net::UnixStream>,
}

impl LinuxPlatform {
    /// Creates the platform, headless until `run` connects it in its initial mode.
    pub(crate) fn new(allowed_modes: WindowingModes) -> Self {
        let startup = StartupEnvironment {
            #[cfg(feature = "wayland")]
            activation_token: crate::linux::take_startup_activation_token_from_environment(),
            #[cfg(feature = "wayland")]
            wayland_socket: crate::linux::take_wayland_socket_from_environment(),
        };
        let event_loop = EventLoop::try_new().expect("failed to create Linux event loop");
        let (common, main_receiver, power_receiver) = LinuxCommon::new(event_loop.get_signal());
        let common = Rc::new(RefCell::new(common));
        let loop_handle = event_loop.handle();
        LinuxCommon::register_sources(&common, &loop_handle, main_receiver, power_receiver)
            .expect("failed to register Linux event sources");
        Self {
            common,
            event_loop: RefCell::new(Some(event_loop)),
            loop_handle,
            connection: RefCell::new(DisplayConnection::Headless(HeadlessConnection::new())),
            allowed_modes,
            initial_windowing: RefCell::new(None),
            startup_environment: RefCell::new(Some(startup)),
            pending_mode: RefCell::new(None),
            transition_waiter: RefCell::new(None),
        }
    }

    /// Connects in the initial mode, before the app finishes launching.
    ///
    /// # Panics
    ///
    /// Panics if the allowed modes don't allow the initial mode, or if its display server can't
    /// be reached, since the app can't start. A windowed initial mode whose environment names no
    /// allowed display server starts headless when that's allowed.
    fn connect_initially(&self) {
        let request = self
            .initial_windowing
            .borrow_mut()
            .take()
            .unwrap_or_else(|| WindowingRequest::Windowed(GraphicalEnvironment::detect()));
        // Dropped unused when starting headless, closing any inherited `WAYLAND_SOCKET`.
        let startup = self
            .startup_environment
            .borrow_mut()
            .take()
            .unwrap_or_default();
        let backend = match &request {
            WindowingRequest::Headless => None,
            WindowingRequest::Windowed(environment) => {
                select_backend(self.allowed_modes, environment)
                    .map(|backend| (backend, environment))
            }
        };
        match backend {
            Some((backend, environment)) => {
                let connection = self
                    .connect(backend, environment, startup)
                    .unwrap_or_else(|error| panic!("{error:#}"));
                self.replace_connection(connection);
            }
            None => assert!(
                self.allowed_modes.contains(WindowingModes::HEADLESS),
                "{:?} does not allow starting headless, and no allowed display server was given",
                self.allowed_modes
            ),
        }
    }

    fn with_common<R>(&self, function: impl FnOnce(&mut LinuxCommon) -> R) -> R {
        function(&mut self.common.borrow_mut())
    }

    /// Connects to the display server `backend` in `environment`.
    ///
    /// `startup` is only given for the connection made when the platform is created.
    #[cfg_attr(
        not(feature = "wayland"),
        allow(
            unused_variables,
            reason = "only Wayland uses the startup environment, and without a display backend \
                      `Backend` has no variants"
        )
    )]
    fn connect(
        &self,
        backend: Backend,
        environment: &GraphicalEnvironment,
        startup: StartupEnvironment,
    ) -> anyhow::Result<DisplayConnection> {
        let loop_handle = self.loop_handle.clone();
        let common = self.common.clone();
        match backend {
            #[cfg(feature = "wayland")]
            Backend::Wayland => {
                let activation_token = environment
                    .activation_token
                    .clone()
                    .or(startup.activation_token);
                Ok(DisplayConnection::Wayland(
                    crate::linux::WaylandConnection::attach(
                        loop_handle,
                        common,
                        environment,
                        startup.wayland_socket,
                        activation_token,
                    )?,
                ))
            }
            #[cfg(feature = "x11")]
            Backend::X11 => Ok(DisplayConnection::X11(crate::linux::X11Connection::attach(
                loop_handle,
                common,
                environment,
            )?)),
        }
    }

    /// Installs `connection`, then drops the previous one outside the borrow, since dropping a
    /// connection can call back into the platform.
    fn replace_connection(&self, connection: DisplayConnection) {
        let previous = std::mem::replace(&mut *self.connection.borrow_mut(), connection);
        drop(previous);
        // The app caches the keyboard layout and the displays, which belong to
        // the connection.
        let callback = self
            .common
            .borrow_mut()
            .callbacks
            .keyboard_layout_change
            .take();
        if let Some(mut callback) = callback {
            callback();
            let mut common = self.common.borrow_mut();
            // The callback may have replaced itself.
            if common.callbacks.keyboard_layout_change.is_none() {
                common.callbacks.keyboard_layout_change = Some(callback);
            }
        }
        let callback = self.common.borrow_mut().callbacks.displays_changed.take();
        if let Some(mut callback) = callback {
            callback();
            self.common
                .borrow_mut()
                .callbacks
                .displays_changed
                .get_or_insert(callback);
        }
    }

    /// Applies a pending switch.
    ///
    /// Runs between event-loop iterations, so no event source is mid-dispatch when the
    /// connection's sources are added or removed.
    fn apply_pending_mode(&self) {
        let Some(mode) = self.pending_mode.borrow().clone() else {
            return;
        };
        if self.connection.borrow().clipboard_handover_pending() {
            return;
        }
        // `request_mode` rejects open windows, but one can be opened before this runs.
        let result = if self.connection.borrow().has_windows() {
            Err(anyhow!("a window was opened while switching display modes"))
        } else {
            match mode {
                WindowingRequest::Headless => {
                    self.replace_connection(DisplayConnection::Headless(HeadlessConnection::new()));
                    Ok(())
                }
                WindowingRequest::Windowed(environment) => {
                    select_backend(self.allowed_modes, &environment)
                        .context("the environment names no allowed Wayland or X11 display server")
                        .and_then(|backend| {
                            self.connect(backend, &environment, StartupEnvironment::default())
                        })
                        .map(|connection| self.replace_connection(connection))
                }
            }
        };
        self.pending_mode.borrow_mut().take();
        if let Some(waiter) = self.transition_waiter.borrow_mut().take() {
            waiter.send(result).ok();
        }
    }

    /// Validates a requested switch, then leaves it for the event loop to apply.
    fn request_mode(&self, mode: WindowingRequest) -> Task<anyhow::Result<()>> {
        if self.pending_mode.borrow().is_some() {
            return Task::ready(Err(anyhow!(
                "a display mode transition is already in progress"
            )));
        }
        let is_headless = self.connection.borrow().is_headless();
        match &mode {
            WindowingRequest::Headless
                if !self.allowed_modes.contains(WindowingModes::HEADLESS) =>
            {
                return Task::ready(Err(anyhow!(
                    "{:?} does not allow headless mode",
                    self.allowed_modes
                )));
            }
            WindowingRequest::Headless if is_headless => {
                return Task::ready(Err(anyhow!("already headless")));
            }
            WindowingRequest::Headless => {}
            WindowingRequest::Windowed(_) if !is_headless => {
                return Task::ready(Err(anyhow!(
                    "already windowed ({}); switch to headless mode first",
                    self.connection.borrow().compositor_name()
                )));
            }
            WindowingRequest::Windowed(environment) => {
                if select_backend(self.allowed_modes, environment).is_none() {
                    return Task::ready(Err(anyhow!(
                        "the environment names no display server that {:?} allows",
                        self.allowed_modes
                    )));
                }
            }
        }
        let (sender, receiver) = oneshot::channel();
        self.transition_waiter.borrow_mut().replace(sender);
        self.connection.borrow().begin_clipboard_handover();
        *self.pending_mode.borrow_mut() = Some(mode);
        // The wakeup persists until the loop next polls, so a request made before `run` starts is
        // applied after its first iteration.
        self.common.borrow().signal.wakeup();
        self.common.borrow().foreground_executor.spawn(async move {
            receiver
                .await
                .map_err(|_| anyhow!("display mode transition was canceled"))
                .and_then(|result| result)
        })
    }
}

impl Drop for LinuxPlatform {
    fn drop(&mut self) {
        self.replace_connection(DisplayConnection::Headless(HeadlessConnection::new()));
    }
}

impl Platform for LinuxPlatform {
    fn background_executor(&self) -> BackgroundExecutor {
        self.with_common(|common| common.background_executor.clone())
    }

    fn foreground_executor(&self) -> ForegroundExecutor {
        self.with_common(|common| common.foreground_executor.clone())
    }

    fn text_system(&self) -> Arc<dyn PlatformTextSystem> {
        self.with_common(|common| common.text_system.clone())
    }

    fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
        self.connection.borrow().keyboard_layout()
    }

    fn keyboard_mapper(&self) -> Rc<dyn PlatformKeyboardMapper> {
        self.with_common(|common| common.keyboard_mapper.clone())
            .unwrap_or_else(|| Rc::new(gpui::DummyKeyboardMapper))
    }

    fn on_keyboard_layout_change(&self, callback: Box<dyn FnMut()>) {
        self.with_common(|common| common.callbacks.keyboard_layout_change = Some(callback));
    }

    fn on_thermal_state_change(&self, _callback: Box<dyn FnMut()>) {}

    fn thermal_state(&self) -> ThermalState {
        ThermalState::Nominal
    }

    #[cfg(not(any(feature = "wayland", feature = "x11")))]
    fn prevent_idle_sleep(&self, reason: &str) -> Task<Result<ActivityGuard>> {
        Task::ready(Err(anyhow!(
            "Idle sleep prevention for {reason:?} requires a Linux windowing backend"
        )))
    }

    #[cfg(any(feature = "wayland", feature = "x11"))]
    fn prevent_idle_sleep(&self, reason: &str) -> Task<Result<ActivityGuard>> {
        let executor = self.background_executor();
        let (guard_tx, guard_rx) = oneshot::channel();
        executor
            .spawn({
                let executor = executor.clone();
                let reason = reason.to_owned();
                async move {
                    guard_tx
                        .send(inhibit_idle_sleep(reason, executor).await)
                        .ok();
                }
            })
            .detach();
        executor
            .clone()
            .spawn(async move { await_idle_sleep_prevention(guard_rx, &executor).await })
    }

    fn run(&self, on_finish_launching: Box<dyn FnOnce()>) {
        self.connect_initially();
        on_finish_launching();

        let mut event_loop = self
            .event_loop
            .borrow_mut()
            .take()
            .expect("App is already running");
        event_loop
            .run(None, &mut (), |_| self.apply_pending_mode())
            .log_err();

        let quit = self.with_common(|common| common.callbacks.quit.take());
        if let Some(mut fun) = quit {
            fun();
        }
    }

    fn quit(&self) {
        if self.with_common(|common| {
            if common.quitting {
                true
            } else {
                common.quitting = true;
                false
            }
        }) {
            return;
        }
        let handover = self.connection.borrow().begin_clipboard_handover();
        if let Some(handover) = handover {
            let signal = self.common.borrow().signal.clone();
            self.common
                .borrow()
                .foreground_executor
                .spawn(async move {
                    let _ = handover.await;
                    signal.stop();
                })
                .detach();
        } else {
            self.with_common(|common| common.signal.stop());
        }
    }

    fn set_initial_windowing(&self, request: WindowingRequest) {
        *self.initial_windowing.borrow_mut() = Some(request);
    }

    fn request_windowing(&self, request: WindowingRequest) -> Task<anyhow::Result<()>> {
        self.request_mode(request)
    }

    fn compositor_name(&self) -> &'static str {
        self.connection.borrow().compositor_name()
    }

    fn graphical_environment(&self) -> Option<GraphicalEnvironment> {
        self.connection.borrow().graphical_environment()
    }

    fn restart(&self, binary_path: Option<PathBuf>, arguments: Vec<std::ffi::OsString>) {
        use std::os::unix::process::CommandExt as _;

        // get the process id of the current process
        let app_pid = std::process::id().to_string();
        // get the path to the executable
        let app_path = if let Some(path) = binary_path {
            path
        } else {
            match self.app_path() {
                Ok(path) => path,
                Err(err) => {
                    log::error!("Failed to get app path: {:?}", err);
                    return;
                }
            }
        };

        log::info!("Restarting process, using app path: {:?}", app_path);

        // Script to wait for the current process to exit and then restart the app.
        // Pass dynamic values as positional parameters to avoid shell interpolation issues.
        let script = r#"
            while kill -0 "$0" 2>/dev/null; do
                sleep 0.1
            done

            app_path="$1"
            shift
            "$app_path" "$@"
            "#;

        #[allow(
            clippy::disallowed_methods,
            reason = "We are restarting ourselves, using std command thus is fine"
        )]
        let restart_process = new_std_command("/usr/bin/env")
            .arg("bash")
            .arg("-c")
            .arg(script)
            .arg(&app_pid)
            .arg(&app_path)
            .args(arguments)
            .process_group(0)
            .spawn();

        match restart_process {
            Ok(_) => self.quit(),
            Err(e) => log::error!("failed to spawn restart script: {:?}", e),
        }
    }

    fn activate(&self, _ignoring_other_apps: bool) {
        log::info!("activate is not implemented on Linux, ignoring the call")
    }

    fn hide(&self) {
        log::info!("hide is not implemented on Linux, ignoring the call")
    }

    fn hide_other_apps(&self) {
        log::info!("hide_other_apps is not implemented on Linux, ignoring the call")
    }

    fn unhide_other_apps(&self) {
        log::info!("unhide_other_apps is not implemented on Linux, ignoring the call")
    }

    fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        self.connection.borrow().primary_display()
    }

    fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
        self.connection.borrow().displays()
    }

    fn on_displays_changed(&self, callback: Box<dyn FnMut()>) {
        self.with_common(|common| common.callbacks.displays_changed = Some(callback));
    }

    #[cfg(feature = "screen-capture")]
    fn is_screen_capture_supported(&self) -> bool {
        self.connection.borrow().is_screen_capture_supported()
    }

    #[cfg(feature = "screen-capture")]
    fn screen_capture_sources(
        &self,
    ) -> oneshot::Receiver<Result<Vec<Rc<dyn gpui::ScreenCaptureSource>>>> {
        self.connection.borrow().screen_capture_sources()
    }

    fn active_window(&self) -> Option<AnyWindowHandle> {
        self.connection.borrow().active_window()
    }

    fn window_stack(&self) -> Option<Vec<AnyWindowHandle>> {
        self.connection.borrow().window_stack()
    }

    fn open_window(
        &self,
        handle: AnyWindowHandle,
        options: WindowParams,
    ) -> anyhow::Result<Box<dyn PlatformWindow>> {
        self.connection.borrow().open_window(handle, options)
    }

    fn open_url(&self, url: &str) {
        self.connection.borrow().open_uri(url);
    }

    fn on_open_urls(&self, callback: Box<dyn FnMut(Vec<String>)>) {
        self.with_common(|common| common.callbacks.open_urls = Some(callback));
    }

    fn prompt_for_paths(
        &self,
        options: PathPromptOptions,
    ) -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>> {
        let (done_tx, done_rx) = oneshot::channel();

        #[cfg(not(any(feature = "wayland", feature = "x11")))]
        let _ = (done_tx.send(Ok(None)), options);

        #[cfg(any(feature = "wayland", feature = "x11"))]
        let identifier = self.connection.borrow().window_identifier();

        #[cfg(any(feature = "wayland", feature = "x11"))]
        self.foreground_executor()
            .spawn(async move {
                let title = if options.directories {
                    "Open Folder"
                } else {
                    "Open File"
                };

                let request = match ashpd::desktop::file_chooser::OpenFileRequest::default()
                    .identifier(identifier.await)
                    .modal(true)
                    .title(title)
                    .accept_label(options.prompt.as_ref().map(gpui::SharedString::as_str))
                    .multiple(options.multiple)
                    .directory(options.directories)
                    .send()
                    .await
                {
                    Ok(request) => request,
                    Err(err) => {
                        let result = match err {
                            ashpd::Error::PortalNotFound(_) => anyhow!(FILE_PICKER_PORTAL_MISSING),
                            err => err.into(),
                        };
                        let _ = done_tx.send(Err(result));
                        return;
                    }
                };

                let result = match request.response() {
                    Ok(response) => Ok(Some(
                        response
                            .uris()
                            .iter()
                            .filter_map(|uri: &ashpd::Uri| url::Url::parse(uri.as_str()).ok())
                            .filter_map(|uri: url::Url| uri.to_file_path().ok())
                            .collect::<Vec<_>>(),
                    )),
                    Err(ashpd::Error::Response(_)) => Ok(None),
                    Err(e) => Err(e.into()),
                };
                let _ = done_tx.send(result);
            })
            .detach();
        done_rx
    }

    fn prompt_for_new_path(
        &self,
        directory: &Path,
        suggested_name: Option<&str>,
    ) -> oneshot::Receiver<Result<Option<PathBuf>>> {
        let (done_tx, done_rx) = oneshot::channel();

        #[cfg(not(any(feature = "wayland", feature = "x11")))]
        let _ = (done_tx.send(Ok(None)), directory, suggested_name);

        #[cfg(any(feature = "wayland", feature = "x11"))]
        let identifier = self.connection.borrow().window_identifier();

        #[cfg(any(feature = "wayland", feature = "x11"))]
        self.foreground_executor()
            .spawn({
                let directory = directory.to_owned();
                let suggested_name = suggested_name.map(|s| s.to_owned());

                async move {
                    let mut request_builder =
                        ashpd::desktop::file_chooser::SaveFileRequest::default()
                            .identifier(identifier.await)
                            .modal(true)
                            .title("Save File")
                            .current_folder(directory)
                            .expect("pathbuf should not be nul terminated");

                    if let Some(suggested_name) = suggested_name {
                        request_builder = request_builder.current_name(suggested_name.as_str());
                    }

                    let request = match request_builder.send().await {
                        Ok(request) => request,
                        Err(err) => {
                            let result = match err {
                                ashpd::Error::PortalNotFound(_) => {
                                    anyhow!(FILE_PICKER_PORTAL_MISSING)
                                }
                                err => err.into(),
                            };
                            let _ = done_tx.send(Err(result));
                            return;
                        }
                    };

                    let result = match request.response() {
                        Ok(response) => Ok(response
                            .uris()
                            .first()
                            .and_then(|uri: &ashpd::Uri| url::Url::parse(uri.as_str()).ok())
                            .and_then(|uri: url::Url| uri.to_file_path().ok())),
                        Err(ashpd::Error::Response(_)) => Ok(None),
                        Err(e) => Err(e.into()),
                    };
                    let _ = done_tx.send(result);
                }
            })
            .detach();

        done_rx
    }

    fn can_select_mixed_files_and_dirs(&self) -> bool {
        // org.freedesktop.portal.FileChooser only supports "pick files" and "pick directories".
        false
    }

    fn reveal_path(&self, path: &Path) {
        self.connection.borrow().reveal_path(path.to_owned());
    }

    fn open_with_system(&self, path: &Path) {
        let path = path.to_owned();
        let environment = self.connection.borrow().graphical_environment();
        self.background_executor()
            .spawn(async move {
                let mut command = new_std_command("xdg-open");
                command.arg(path);
                if let Some(environment) = environment {
                    environment.apply_to(&mut command);
                }
                #[allow(
                    clippy::disallowed_methods,
                    reason = "running on a background thread, so blocking is fine"
                )]
                command.status().context("invoking xdg-open").log_err();
            })
            .detach();
    }

    fn on_quit(&self, callback: Box<dyn FnMut() -> bool>) {
        self.with_common(|common| {
            common.callbacks.quit = Some(callback);
        });
    }

    fn on_reopen(&self, callback: Box<dyn FnMut()>) {
        self.with_common(|common| {
            common.callbacks.reopen = Some(callback);
        });
    }

    fn on_system_sleep(&self, callback: Box<dyn FnMut()>) {
        self.with_common(|common| {
            common.callbacks.system_sleep = Some(callback);
            common.start_power_listener();
        });
    }

    fn on_system_wake(&self, callback: Box<dyn FnMut()>) {
        self.with_common(|common| {
            common.callbacks.system_wake = Some(callback);
            common.start_power_listener();
        });
    }

    fn set_app_identity(&self, _identifier: &str, name: &str) {
        self.with_common(|common| common.app_name = Some(name.to_string()));
    }

    fn show_system_notification(&self, notification: gpui::SystemNotification) {
        self.with_common(|common| {
            common
                .system_notifications
                .show(common.app_name.as_deref(), notification)
        });
    }

    fn dismiss_system_notification(&self, tag: &str) {
        self.with_common(|common| common.system_notifications.dismiss(tag));
    }

    fn on_system_notification_response(
        &self,
        callback: Box<dyn FnMut(gpui::SystemNotificationResponse)>,
    ) {
        self.with_common(|common| {
            let executor = common.foreground_executor.clone();
            common.system_notifications.on_response(&executor, callback)
        });
    }

    fn on_app_menu_action(&self, callback: Box<dyn FnMut(&dyn Action)>) {
        self.with_common(|common| {
            common.callbacks.app_menu_action = Some(callback);
        });
    }

    fn on_will_open_app_menu(&self, callback: Box<dyn FnMut()>) {
        self.with_common(|common| {
            common.callbacks.will_open_app_menu = Some(callback);
        });
    }

    fn on_validate_app_menu_command(&self, callback: Box<dyn FnMut(&dyn Action) -> bool>) {
        self.with_common(|common| {
            common.callbacks.validate_app_menu_command = Some(callback);
        });
    }

    fn app_path(&self) -> Result<PathBuf> {
        // get the path of the executable of the current process
        let app_path = env::current_exe()?;
        Ok(app_path)
    }

    fn set_menus(&self, menus: Vec<Menu>, _keymap: &Keymap) {
        self.with_common(|common| {
            common.menus = menus.into_iter().map(|menu| menu.owned()).collect();
        })
    }

    fn get_menus(&self) -> Option<Vec<OwnedMenu>> {
        self.with_common(|common| Some(common.menus.clone()))
    }

    fn set_dock_menu(&self, _menu: Vec<MenuItem>, _keymap: &Keymap) {
        // todo(linux)
    }

    fn path_for_auxiliary_executable(&self, _name: &str) -> Result<PathBuf> {
        Err(anyhow::Error::msg(
            "Platform<LinuxPlatform>::path_for_auxiliary_executable is not implemented yet",
        ))
    }

    fn set_cursor_style(&self, style: CursorStyle) {
        self.connection.borrow().set_cursor_style(style)
    }

    fn hide_cursor_until_mouse_moves(&self) {
        self.connection.borrow().hide_cursor_until_mouse_moves()
    }

    fn is_cursor_visible(&self) -> bool {
        self.connection.borrow().is_cursor_visible()
    }

    fn should_auto_hide_scrollbars(&self) -> bool {
        self.with_common(|common| common.auto_hide_scrollbars)
    }

    fn write_credentials(&self, url: &str, username: &str, password: &[u8]) -> Task<Result<()>> {
        let url = url.to_string();
        let username = username.to_string();
        let password = password.to_vec();
        self.background_executor().spawn(async move {
            let keyring = oo7::Keyring::new().await?;
            keyring.unlock().await?;
            keyring
                .create_item(
                    KEYRING_LABEL,
                    &vec![("url", &url), ("username", &username)],
                    password,
                    true,
                )
                .await?;
            Ok(())
        })
    }

    fn read_credentials(&self, url: &str) -> Task<Result<Option<(String, Vec<u8>)>>> {
        let url = url.to_string();
        self.background_executor().spawn(async move {
            let keyring = oo7::Keyring::new().await?;
            keyring.unlock().await?;

            let items = keyring.search_items(&vec![("url", &url)]).await?;

            for item in items.into_iter() {
                if item.label().await.is_ok_and(|label| label == KEYRING_LABEL) {
                    let attributes = item.attributes().await?;
                    let username = attributes
                        .get("username")
                        .context("Cannot find username in stored credentials")?;
                    item.unlock().await?;
                    let secret = item.secret().await?;

                    // we lose the zeroizing capabilities at this boundary,
                    // a current limitation GPUI's credentials api
                    return Ok(Some((username.to_string(), secret.to_vec())));
                } else {
                    continue;
                }
            }
            Ok(None)
        })
    }

    fn delete_credentials(&self, url: &str) -> Task<Result<()>> {
        let url = url.to_string();
        self.background_executor().spawn(async move {
            let keyring = oo7::Keyring::new().await?;
            keyring.unlock().await?;

            let items = keyring.search_items(&vec![("url", &url)]).await?;

            for item in items.into_iter() {
                if item.label().await.is_ok_and(|label| label == KEYRING_LABEL) {
                    item.delete().await?;
                    return Ok(());
                }
            }

            Ok(())
        })
    }

    fn window_appearance(&self) -> WindowAppearance {
        self.with_common(|common| common.appearance)
    }

    fn button_layout(&self) -> Option<WindowButtonLayout> {
        Some(self.with_common(|common| common.button_layout))
    }

    fn register_url_scheme(&self, _: &str) -> Task<anyhow::Result<()>> {
        Task::ready(Err(anyhow!("register_url_scheme unimplemented")))
    }

    fn write_to_primary(&self, item: ClipboardItem) {
        self.connection.borrow().write_to_primary(item)
    }

    fn clear_clipboard_if_owned(&self) -> bool {
        self.connection.borrow().clear_clipboard_if_owned()
    }

    fn on_clipboard_change(&self, callback: Box<dyn FnMut(gpui::ClipboardChange)>) {
        self.common.borrow_mut().callbacks.clipboard_change = Some(callback);
    }

    fn on_native_drag_offer(
        &self,
        callback: Box<dyn FnMut(gpui::NativeDragOffer) -> gpui::NativeDragActions>,
    ) {
        self.common.borrow_mut().callbacks.native_offer = Some(callback);
    }

    fn on_native_drop(&self, callback: Box<dyn FnMut(gpui::NativeDropEvent) -> bool>) {
        self.common.borrow_mut().callbacks.native_drop = Some(callback);
    }

    fn on_native_drag(&self, callback: Box<dyn FnMut(gpui::NativeDragEvent)>) {
        self.common.borrow_mut().callbacks.native_drag = Some(callback);
    }

    fn read_from_clipboard_async(
        &self,
    ) -> Task<Result<Option<ClipboardItem>, gpui::ClipboardReadError>> {
        self.connection.borrow().read_from_clipboard_async()
    }

    fn write_to_clipboard(&self, item: ClipboardItem) {
        self.connection.borrow().write_to_clipboard(item)
    }

    fn read_from_primary_async(
        &self,
    ) -> gpui::Task<Result<Option<ClipboardItem>, gpui::ClipboardReadError>> {
        self.connection.borrow().read_from_primary_async()
    }

    fn read_from_primary(&self) -> Option<ClipboardItem> {
        self.connection.borrow().read_from_primary()
    }

    fn read_from_clipboard(&self) -> Option<ClipboardItem> {
        self.connection.borrow().read_from_clipboard()
    }

    fn add_recent_document(&self, _path: &Path) {}
}

/// Opens `target` with the first of the desktop's launchers that succeeds. Returns whether one
/// did.
#[cfg(any(feature = "wayland", feature = "x11"))]
async fn run_open_commands(
    target: &std::ffi::OsStr,
    activation_token: Option<&str>,
    environment: &GraphicalEnvironment,
) -> bool {
    for mut command in open::commands(target) {
        environment.apply_to(&mut command);
        if let Some(token) = activation_token {
            command.env("XDG_ACTIVATION_TOKEN", token);
        }
        let program = format!("{:?}", command.get_program());
        match smol::process::Command::from(command).spawn() {
            Ok(mut child) => match child.status().await {
                Ok(status) if status.success() => return true,
                Ok(status) => log::error!("Command {} exited with status: {}", program, status),
                Err(error) => log::error!("Failed to get status from {}: {}", program, error),
            },
            Err(error) => log::error!("Failed to open with {}: {}", program, error),
        }
    }
    false
}

#[cfg(any(feature = "wayland", feature = "x11"))]
pub(super) fn open_uri_internal(
    executor: BackgroundExecutor,
    uri: &str,
    activation_token: Option<String>,
    environment: GraphicalEnvironment,
) {
    if let Some(uri) = ashpd::Uri::parse(uri).log_err() {
        executor
            .spawn(async move {
                let opened = run_open_commands(
                    uri.as_str().as_ref(),
                    activation_token.as_deref(),
                    &environment,
                )
                .await;
                if !opened {
                    match ashpd::desktop::open_uri::OpenFileRequest::default()
                        .activation_token(activation_token.map(ashpd::ActivationToken::from))
                        .send_uri(&uri)
                        .await
                        .and_then(|e| e.response())
                    {
                        Ok(()) => {}
                        Err(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)) => {}
                        Err(e) => {
                            log::error!("Failed to open with dbus: {}", e);
                        }
                    }
                }
            })
            .detach();
    }
}

#[cfg(any(feature = "x11", feature = "wayland"))]
pub(super) fn reveal_path_internal(
    executor: BackgroundExecutor,
    path: PathBuf,
    activation_token: Option<String>,
    environment: GraphicalEnvironment,
) {
    executor
        .spawn(async move {
            if let Some(dir) = File::open(path.clone()).log_err() {
                match ashpd::desktop::open_uri::OpenDirectoryRequest::default()
                    .activation_token(activation_token.clone().map(ashpd::ActivationToken::from))
                    .send(&dir.as_fd())
                    .await
                {
                    Ok(_) => return,
                    Err(e) => log::error!("Failed to open with dbus: {}", e),
                }
                let directory = if path.is_dir() {
                    path.as_path()
                } else {
                    path.parent().unwrap_or(Path::new(""))
                };
                run_open_commands(
                    directory.as_os_str(),
                    activation_token.as_deref(),
                    &environment,
                )
                .await;
            }
        })
        .detach();
}

#[cfg(any(feature = "wayland", feature = "x11"))]
pub(super) fn is_within_click_distance(a: Point<Pixels>, b: Point<Pixels>) -> bool {
    let diff = a - b;
    diff.x.abs() <= DOUBLE_CLICK_DISTANCE && diff.y.abs() <= DOUBLE_CLICK_DISTANCE
}

/// Creates an XKB context for keymaps supplied by Wayland or X11.
///
/// Server keymaps are already resolved and need no local keyboard definitions.
/// Loading default include paths can fail on systems without those files.
#[cfg(any(feature = "wayland", feature = "x11"))]
pub(super) fn new_xkb_context() -> anyhow::Result<xkb::Context> {
    validate_xkb_context(xkb::Context::new(xkb::CONTEXT_NO_DEFAULT_INCLUDES))
}

#[cfg(any(feature = "wayland", feature = "x11"))]
fn validate_xkb_context(context: xkb::Context) -> anyhow::Result<xkb::Context> {
    ensure!(
        !context.get_raw_ptr().is_null(),
        "libxkbcommon failed to create an XKB context"
    );
    Ok(context)
}

#[cfg(any(feature = "wayland", feature = "x11"))]
pub(super) fn get_xkb_compose_state(cx: &xkb::Context) -> Option<xkb::compose::State> {
    let mut locales = Vec::default();
    if let Some(locale) = env::var_os("LC_CTYPE") {
        locales.push(locale);
    }
    locales.push(OsString::from("C"));
    let mut state: Option<xkb::compose::State> = None;
    for locale in locales {
        if let Ok(table) =
            xkb::compose::Table::new_from_locale(cx, &locale, xkb::compose::COMPILE_NO_FLAGS)
        {
            state = Some(xkb::compose::State::new(
                &table,
                xkb::compose::STATE_NO_FLAGS,
            ));
            break;
        }
    }
    state
}

#[cfg(all(test, any(feature = "wayland", feature = "x11")))]
pub(super) const PIPE_READ_TIMEOUT: Duration = super::transfer::TRANSFER_TIMEOUT;

#[cfg(all(test, any(feature = "wayland", feature = "x11")))]
pub(super) fn read_fd_with_timeout(
    fd: filedescriptor::FileDescriptor,
    timeout: Duration,
) -> Result<Vec<u8>> {
    use std::os::fd::{FromRawFd, IntoRawFd};
    let file = unsafe { File::from_raw_fd(fd.into_raw_fd()) };
    super::transfer::read(
        file,
        &super::transfer::cancelled(),
        std::time::Instant::now() + timeout,
        super::transfer::BYTE_LIMIT,
    )
    .map_err(Into::into)
}

#[cfg(any(feature = "wayland", feature = "x11"))]
pub(super) const DEFAULT_CURSOR_ICON_NAME: &str = "left_ptr";

#[cfg(any(feature = "wayland", feature = "x11"))]
pub(super) fn cursor_style_to_icon_names(style: CursorStyle) -> &'static [&'static str] {
    // Based on cursor names from chromium:
    // https://github.com/chromium/chromium/blob/d3069cf9c973dc3627fa75f64085c6a86c8f41bf/ui/base/cursor/cursor_factory.cc#L113
    match style {
        CursorStyle::Arrow => &[DEFAULT_CURSOR_ICON_NAME],
        CursorStyle::IBeam => &["text", "xterm"],
        CursorStyle::Crosshair => &["crosshair", "cross"],
        CursorStyle::ClosedHand => &["closedhand", "grabbing", "hand2"],
        CursorStyle::OpenHand => &["openhand", "grab", "hand1"],
        CursorStyle::PointingHand => &["pointer", "hand", "hand2"],
        CursorStyle::ResizeLeft => &["w-resize", "left_side"],
        CursorStyle::ResizeRight => &["e-resize", "right_side"],
        CursorStyle::ResizeLeftRight => &["ew-resize", "sb_h_double_arrow"],
        CursorStyle::ResizeUp => &["n-resize", "top_side"],
        CursorStyle::ResizeDown => &["s-resize", "bottom_side"],
        CursorStyle::ResizeUpDown => &["sb_v_double_arrow", "ns-resize"],
        CursorStyle::ResizeUpLeftDownRight => &["size_fdiag", "bd_double_arrow", "nwse-resize"],
        CursorStyle::ResizeUpRightDownLeft => &["size_bdiag", "nesw-resize", "fd_double_arrow"],
        CursorStyle::ResizeColumn => &["col-resize", "sb_h_double_arrow"],
        CursorStyle::ResizeRow => &["row-resize", "sb_v_double_arrow"],
        CursorStyle::IBeamCursorForVerticalLayout => &["vertical-text"],
        CursorStyle::OperationNotAllowed => &["not-allowed", "crossed_circle"],
        CursorStyle::DragLink => &["alias"],
        CursorStyle::DragCopy => &["copy"],
        CursorStyle::ContextualMenu => &["context-menu"],
    }
}

#[cfg(any(feature = "wayland", feature = "x11"))]
pub(super) fn log_cursor_icon_warning(message: impl std::fmt::Display) {
    if let Ok(xcursor_path) = env::var("XCURSOR_PATH") {
        log::warn!(
            "{:#}\ncursor icon loading may be failing if XCURSOR_PATH environment variable is invalid. \
                    XCURSOR_PATH overrides the default icon search. Its current value is '{}'",
            message,
            xcursor_path
        );
    } else {
        log::warn!("{:#}", message);
    }
}

/// The US key at `keycode`'s position: letters lowercase on both levels, other keys their Shift character.
#[cfg(any(feature = "wayland", feature = "x11"))]
fn guess_ascii(keycode: Keycode, shift: bool) -> Option<char> {
    let &(_, lower, upper) = US_POSITIONS
        .iter()
        .find(|(raw, _, _)| *raw == keycode.raw())?;
    Some(if shift && !lower.is_ascii_alphabetic() {
        upper
    } else {
        lower
    })
}

/// Control codes Ctrl makes from the letters `a` through `z`.
#[cfg(any(feature = "wayland", feature = "x11"))]
const CONTROL_LETTERS: std::ops::RangeInclusive<u32> = 0x01..=0x1a;

/// The distance from a letter's control code to its lowercase character.
#[cfg(any(feature = "wayland", feature = "x11"))]
const CONTROL_TO_LOWERCASE: u32 = 0x60;

/// The ASCII key xkb's text names for a keysym that is not ASCII: Ctrl's Latin fallback, with `ctrl-a` mapped to `a`.
#[cfg(any(feature = "wayland", feature = "x11"))]
fn ascii_fallback(key_utf8: &str, key_utf32: u32) -> Option<char> {
    let mut chars = key_utf8.chars();
    let key = chars.next().filter(|_| chars.next().is_none())?;
    if key.is_ascii_graphic() {
        Some(key.to_ascii_lowercase())
    } else if CONTROL_LETTERS.contains(&key_utf32) {
        char::from_u32(key_utf32 + CONTROL_TO_LOWERCASE)
    } else {
        None
    }
}

/// The US key position of a one-character key name: its keycode, its US character and whether Shift types it.
#[cfg(any(feature = "wayland", feature = "x11"))]
fn us_position(key: &str) -> Option<(Keycode, char, bool)> {
    let mut chars = key.chars();
    let target = chars.next().filter(|_| chars.next().is_none())?;
    US_POSITIONS.iter().find_map(|&(raw, lower, upper)| {
        if target == lower {
            Some((Keycode::new(raw), lower, false))
        } else if target == upper {
            let us = if lower.is_ascii_alphabetic() {
                lower
            } else {
                upper
            };
            Some((Keycode::new(raw), us, true))
        } else {
            None
        }
    })
}

#[cfg(any(feature = "wayland", feature = "x11"))]
pub(super) fn keystroke_from_xkb(
    state: &State,
    mut modifiers: gpui::Modifiers,
    keycode: Keycode,
) -> gpui::Keystroke {
    let key_utf32 = state.key_get_utf32(keycode);
    let key_utf8 = state.key_get_utf8(keycode);
    let key_sym = state.key_get_one_sym(keycode);
    let latin = group_is_latin(&state.get_keymap(), state.key_get_layout(keycode));

    let key = match key_sym {
        Keysym::Return => "enter".to_owned(),
        Keysym::Prior => "pageup".to_owned(),
        Keysym::Next => "pagedown".to_owned(),
        Keysym::ISO_Left_Tab => "tab".to_owned(),
        Keysym::KP_Prior => "pageup".to_owned(),
        Keysym::KP_Next => "pagedown".to_owned(),
        Keysym::XF86_Back => "back".to_owned(),
        Keysym::XF86_Forward => "forward".to_owned(),
        Keysym::XF86_Cut => "cut".to_owned(),
        Keysym::XF86_Copy => "copy".to_owned(),
        Keysym::XF86_Paste => "paste".to_owned(),
        Keysym::XF86_New => "new".to_owned(),
        Keysym::XF86_Open => "open".to_owned(),
        Keysym::XF86_Save => "save".to_owned(),

        Keysym::comma => ",".to_owned(),
        Keysym::period => ".".to_owned(),
        Keysym::less => "<".to_owned(),
        Keysym::greater => ">".to_owned(),
        Keysym::slash => "/".to_owned(),
        Keysym::question => "?".to_owned(),

        Keysym::semicolon => ";".to_owned(),
        Keysym::colon => ":".to_owned(),
        Keysym::apostrophe => "'".to_owned(),
        Keysym::quotedbl => "\"".to_owned(),

        Keysym::bracketleft => "[".to_owned(),
        Keysym::braceleft => "{".to_owned(),
        Keysym::bracketright => "]".to_owned(),
        Keysym::braceright => "}".to_owned(),
        Keysym::backslash => "\\".to_owned(),
        Keysym::bar => "|".to_owned(),

        Keysym::grave => "`".to_owned(),
        Keysym::asciitilde => "~".to_owned(),
        Keysym::exclam => "!".to_owned(),
        Keysym::at => "@".to_owned(),
        Keysym::numbersign => "#".to_owned(),
        Keysym::dollar => "$".to_owned(),
        Keysym::percent => "%".to_owned(),
        Keysym::asciicircum => "^".to_owned(),
        Keysym::ampersand => "&".to_owned(),
        Keysym::asterisk => "*".to_owned(),
        Keysym::parenleft => "(".to_owned(),
        Keysym::parenright => ")".to_owned(),
        Keysym::minus => "-".to_owned(),
        Keysym::underscore => "_".to_owned(),
        Keysym::equal => "=".to_owned(),
        Keysym::plus => "+".to_owned(),
        Keysym::space => "space".to_owned(),
        Keysym::BackSpace => "backspace".to_owned(),
        Keysym::Tab => "tab".to_owned(),
        Keysym::Delete => "delete".to_owned(),
        Keysym::Escape => "escape".to_owned(),

        Keysym::Left => "left".to_owned(),
        Keysym::Right => "right".to_owned(),
        Keysym::Up => "up".to_owned(),
        Keysym::Down => "down".to_owned(),
        Keysym::Home => "home".to_owned(),
        Keysym::End => "end".to_owned(),
        Keysym::Insert => "insert".to_owned(),

        _ => {
            let name = xkb::keysym_get_name(key_sym).to_lowercase();
            if key_sym.is_keypad_key() {
                name.replace("kp_", "")
            } else if let Some(key) =
                char::from_u32(xkb::keysym_to_utf32(key_sym)).filter(char::is_ascii_graphic)
            {
                // Ctrl turns some digits into control codes; the keysym still names the key.
                key.to_ascii_lowercase().to_string()
            } else if let Some(key) = ascii_fallback(&key_utf8, key_utf32)
                .or_else(|| guess_ascii(keycode, modifiers.shift))
                .filter(|key| !(latin && group_prints(state, keycode, *key)))
            {
                // A Latin group keeps the keysym name when another key prints this character.
                key.to_string()
            } else {
                name
            }
        }
    };

    // A non-Latin group reports the US key at each main-block position, so shortcuts keep their physical keys.
    let key = match guess_ascii(keycode, modifiers.shift) {
        Some(us) if !latin => us.to_string(),
        _ => key,
    };

    if modifiers.shift {
        // we only include the shift for upper-case letters by convention,
        // so don't include for numbers and symbols, but do include for
        // tab/enter, etc.
        if key.chars().count() == 1 && key.to_lowercase() == key.to_uppercase() {
            modifiers.shift = false;
        }
    }

    // Ignore control characters (and DEL) for the purposes of key_char
    let key_char =
        (key_utf32 >= 32 && key_utf32 != 127 && !key_utf8.is_empty()).then_some(key_utf8);

    gpui::Keystroke {
        modifiers,
        key,
        key_char,
        is_keypad: key_sym.is_keypad_key(),
    }
}

/**
 * Returns which symbol the dead key represents
 * <https://developer.mozilla.org/en-US/docs/Web/API/UI_Events/Keyboard_event_key_values#dead_keycodes_for_linux>
 */
#[cfg(any(feature = "wayland", feature = "x11"))]
pub fn keystroke_underlying_dead_key(keysym: Keysym) -> Option<String> {
    match keysym {
        Keysym::dead_grave => Some("`".to_owned()),
        Keysym::dead_acute => Some("´".to_owned()),
        Keysym::dead_circumflex => Some("^".to_owned()),
        Keysym::dead_tilde => Some("~".to_owned()),
        Keysym::dead_macron => Some("¯".to_owned()),
        Keysym::dead_breve => Some("˘".to_owned()),
        Keysym::dead_abovedot => Some("˙".to_owned()),
        Keysym::dead_diaeresis => Some("¨".to_owned()),
        Keysym::dead_abovering => Some("˚".to_owned()),
        Keysym::dead_doubleacute => Some("˝".to_owned()),
        Keysym::dead_caron => Some("ˇ".to_owned()),
        Keysym::dead_cedilla => Some("¸".to_owned()),
        Keysym::dead_ogonek => Some("˛".to_owned()),
        Keysym::dead_iota => Some("ͅ".to_owned()),
        Keysym::dead_voiced_sound => Some("゙".to_owned()),
        Keysym::dead_semivoiced_sound => Some("゚".to_owned()),
        Keysym::dead_belowdot => Some("̣̣".to_owned()),
        Keysym::dead_hook => Some("̡".to_owned()),
        Keysym::dead_horn => Some("̛".to_owned()),
        Keysym::dead_stroke => Some("̶̶".to_owned()),
        Keysym::dead_abovecomma => Some("̓̓".to_owned()),
        Keysym::dead_abovereversedcomma => Some("ʽ".to_owned()),
        Keysym::dead_doublegrave => Some("̏".to_owned()),
        Keysym::dead_belowring => Some("˳".to_owned()),
        Keysym::dead_belowmacron => Some("̱".to_owned()),
        Keysym::dead_belowcircumflex => Some("ꞈ".to_owned()),
        Keysym::dead_belowtilde => Some("̰".to_owned()),
        Keysym::dead_belowbreve => Some("̮".to_owned()),
        Keysym::dead_belowdiaeresis => Some("̤".to_owned()),
        Keysym::dead_invertedbreve => Some("̯".to_owned()),
        Keysym::dead_belowcomma => Some("̦".to_owned()),
        Keysym::dead_currency => None,
        Keysym::dead_lowline => None,
        Keysym::dead_aboveverticalline => None,
        Keysym::dead_belowverticalline => None,
        Keysym::dead_longsolidusoverlay => None,
        Keysym::dead_a => None,
        Keysym::dead_A => None,
        Keysym::dead_e => None,
        Keysym::dead_E => None,
        Keysym::dead_i => None,
        Keysym::dead_I => None,
        Keysym::dead_o => None,
        Keysym::dead_O => None,
        Keysym::dead_u => None,
        Keysym::dead_U => None,
        Keysym::dead_small_schwa => Some("ə".to_owned()),
        Keysym::dead_capital_schwa => Some("Ə".to_owned()),
        Keysym::dead_greek => None,
        _ => None,
    }
}
#[cfg(any(feature = "wayland", feature = "x11"))]
pub(super) fn modifiers_from_xkb(keymap_state: &State) -> gpui::Modifiers {
    let shift = keymap_state.mod_name_is_active(xkb::MOD_NAME_SHIFT, xkb::STATE_MODS_EFFECTIVE);
    let alt = keymap_state.mod_name_is_active(xkb::MOD_NAME_ALT, xkb::STATE_MODS_EFFECTIVE);
    let control = keymap_state.mod_name_is_active(xkb::MOD_NAME_CTRL, xkb::STATE_MODS_EFFECTIVE);
    let platform = keymap_state.mod_name_is_active(xkb::MOD_NAME_LOGO, xkb::STATE_MODS_EFFECTIVE);
    gpui::Modifiers {
        shift,
        alt,
        control,
        platform,
        function: false,
    }
}

#[cfg(any(feature = "wayland", feature = "x11"))]
pub(super) fn capslock_from_xkb(keymap_state: &State) -> gpui::Capslock {
    let on = keymap_state.mod_name_is_active(xkb::MOD_NAME_CAPS, xkb::STATE_MODS_EFFECTIVE);
    gpui::Capslock { on }
}

/// US characters of the main key block by evdev keycode: unshifted, then shifted.
#[cfg(any(feature = "wayland", feature = "x11"))]
const US_POSITIONS: [(u32, char, char); 47] = [
    (10, '1', '!'),
    (11, '2', '@'),
    (12, '3', '#'),
    (13, '4', '$'),
    (14, '5', '%'),
    (15, '6', '^'),
    (16, '7', '&'),
    (17, '8', '*'),
    (18, '9', '('),
    (19, '0', ')'),
    (20, '-', '_'),
    (21, '=', '+'),
    (24, 'q', 'Q'),
    (25, 'w', 'W'),
    (26, 'e', 'E'),
    (27, 'r', 'R'),
    (28, 't', 'T'),
    (29, 'y', 'Y'),
    (30, 'u', 'U'),
    (31, 'i', 'I'),
    (32, 'o', 'O'),
    (33, 'p', 'P'),
    (34, '[', '{'),
    (35, ']', '}'),
    (38, 'a', 'A'),
    (39, 's', 'S'),
    (40, 'd', 'D'),
    (41, 'f', 'F'),
    (42, 'g', 'G'),
    (43, 'h', 'H'),
    (44, 'j', 'J'),
    (45, 'k', 'K'),
    (46, 'l', 'L'),
    (47, ';', ':'),
    (48, '\'', '"'),
    (49, '`', '~'),
    (51, '\\', '|'),
    (52, 'z', 'Z'),
    (53, 'x', 'X'),
    (54, 'c', 'C'),
    (55, 'v', 'V'),
    (56, 'b', 'B'),
    (57, 'n', 'N'),
    (58, 'm', 'M'),
    (59, ',', '<'),
    (60, '.', '>'),
    (61, '/', '?'),
];

/// Evdev keycodes of the letter rows AD01 to AD12, AC01 to AC11 and AB01 to AB10.
#[cfg(any(feature = "wayland", feature = "x11"))]
const LETTER_ROWS: [std::ops::RangeInclusive<u32>; 3] = [24..=35, 38..=48, 52..=61];

/// One bit for each of the 26 ASCII letters.
#[cfg(any(feature = "wayland", feature = "x11"))]
const ALL_LETTERS: u32 = (1 << 26) - 1;

/// Whether `layout` types all 26 ASCII letters on the unmodified letter keys (rows AD, AC and AB).
#[cfg(any(feature = "wayland", feature = "x11"))]
pub(super) fn group_is_latin(keymap: &xkb::Keymap, layout: xkb::LayoutIndex) -> bool {
    let mut letters = 0u32;
    for raw in LETTER_ROWS.into_iter().flatten() {
        for sym in keymap.key_get_syms_by_level(Keycode::new(raw), layout, 0) {
            if let Some(letter) =
                char::from_u32(xkb::keysym_to_utf32(*sym)).filter(char::is_ascii_lowercase)
            {
                letters |= 1 << (letter as u32 - 'a' as u32);
            }
        }
    }
    letters == ALL_LETTERS
}

/// Whether `state`'s group types `character` on any key's base or Shift level, the levels reached without AltGr.
#[cfg(any(feature = "wayland", feature = "x11"))]
fn group_prints(state: &State, keycode: Keycode, character: char) -> bool {
    let keymap = state.get_keymap();
    let layout = state.key_get_layout(keycode);
    let printed = |raw| {
        (0..2).any(|level| {
            keymap
                .key_get_syms_by_level(Keycode::new(raw), layout, level)
                .iter()
                .any(|sym| xkb::keysym_to_utf32(*sym) == character as u32 && !sym.is_keypad_key())
        })
    };
    (keymap.min_keycode().raw()..=keymap.max_keycode().raw()).any(printed)
}

/// Whether AltGr (the level-three modifier, Mod5) is held; such a key press is text input and never a command.
#[cfg(any(feature = "wayland", feature = "x11"))]
pub(super) fn prefers_character_input(state: &State) -> bool {
    state.mod_name_is_active("Mod5", xkb::STATE_MODS_EFFECTIVE)
}

/// Whether a key press may feed a dead-key composition: never while Ctrl, Alt or Super make it a shortcut.
#[cfg(any(feature = "wayland", feature = "x11"))]
pub(super) fn composes(modifiers: gpui::Modifiers) -> bool {
    !(modifiers.control || modifiers.alt || modifiers.platform)
}

#[cfg(any(feature = "wayland", feature = "x11"))]
static NEXT_KEYBOARD_LAYOUT_GENERATION: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

#[cfg(any(feature = "wayland", feature = "x11"))]
const UNKNOWN_KEYBOARD_LAYOUT: &str = "unknown";

/// The active XKB group of one keymap at one layout generation.
#[cfg(any(feature = "wayland", feature = "x11"))]
#[derive(Clone)]
pub(crate) struct XkbKeyboardLayout {
    id: gpui::SharedString,
    identity: gpui::KeyboardLayoutIdentity,
    keymap: Option<xkb::Keymap>,
}

#[cfg(any(feature = "wayland", feature = "x11"))]
impl XkbKeyboardLayout {
    pub(crate) fn unknown() -> Self {
        Self::new(
            gpui::KeyboardLayoutIdentity {
                name: UNKNOWN_KEYBOARD_LAYOUT.into(),
                group: 0,
                generation: 0,
            },
            None,
        )
    }

    fn new(identity: gpui::KeyboardLayoutIdentity, keymap: Option<xkb::Keymap>) -> Self {
        Self {
            id: identity.to_id().into(),
            identity,
            keymap,
        }
    }

    /// The layout of `state`'s effective group, or `None` when neither the keymap nor its group changed.
    pub(crate) fn follow(&self, state: Option<&State>) -> Option<Self> {
        let generation =
            || NEXT_KEYBOARD_LAYOUT_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let Some(state) = state else {
            return self.keymap.is_some().then(|| {
                Self::new(
                    gpui::KeyboardLayoutIdentity {
                        name: UNKNOWN_KEYBOARD_LAYOUT.into(),
                        group: 0,
                        generation: generation(),
                    },
                    None,
                )
            });
        };
        let keymap = state.get_keymap();
        let group = state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE);
        let same_keymap = self
            .keymap
            .as_ref()
            .is_some_and(|known| known.get_raw_ptr() == keymap.get_raw_ptr());
        if same_keymap && group == self.identity.group {
            return None;
        }
        let name = match keymap.layout_get_name(group) {
            "" => UNKNOWN_KEYBOARD_LAYOUT,
            name => name,
        }
        .to_owned();
        Some(Self::new(
            gpui::KeyboardLayoutIdentity {
                name,
                group,
                generation: generation(),
            },
            Some(keymap),
        ))
    }

    /// The keyboard mapper of this generation.
    pub(crate) fn mapper(&self) -> Rc<dyn PlatformKeyboardMapper> {
        match &self.keymap {
            Some(keymap) => Rc::new(LinuxKeyboardMapper::new(
                keymap.clone(),
                self.identity.group,
            )),
            None => Rc::new(gpui::DummyKeyboardMapper),
        }
    }
}

#[cfg(any(feature = "wayland", feature = "x11"))]
impl PlatformKeyboardLayout for XkbKeyboardLayout {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.identity.name
    }
}

/// Resolves bindings on one XKB group: as printed, or with key equivalents at the US key position and labelled with that key's keycap.
#[cfg(any(feature = "wayland", feature = "x11"))]
pub(crate) struct LinuxKeyboardMapper {
    keymap: xkb::Keymap,
    group: xkb::LayoutIndex,
    latin: bool,
    key_equivalents: collections::HashMap<char, char>,
}

#[cfg(any(feature = "wayland", feature = "x11"))]
impl LinuxKeyboardMapper {
    fn new(keymap: xkb::Keymap, group: xkb::LayoutIndex) -> Self {
        let latin = group_is_latin(&keymap, group);
        let mut mapper = Self {
            keymap,
            group,
            latin,
            key_equivalents: collections::HashMap::default(),
        };
        let mut key_equivalents = collections::HashMap::default();
        for &(raw, lower, upper) in &US_POSITIONS {
            for (us, shift) in [(lower, false), (upper, true)] {
                if lower.is_ascii_alphabetic() && (shift || latin) {
                    continue;
                }
                let modifiers = gpui::Modifiers {
                    shift,
                    ..Default::default()
                };
                let event = mapper.event(Keycode::new(raw), modifiers);
                let mut chars = event.key.chars();
                if let Some(key) = chars.next().filter(|_| chars.next().is_none())
                    && key != us
                {
                    key_equivalents.insert(us, key);
                }
            }
        }
        mapper.key_equivalents = key_equivalents;
        mapper
    }

    /// The state of this group with `modifiers` held.
    fn state(&self, modifiers: gpui::Modifiers) -> State {
        let mut mask = 0;
        for (held, name) in [
            (modifiers.shift, xkb::MOD_NAME_SHIFT),
            (modifiers.control, xkb::MOD_NAME_CTRL),
            (modifiers.alt, xkb::MOD_NAME_ALT),
            (modifiers.platform, xkb::MOD_NAME_LOGO),
        ] {
            let index = self.keymap.mod_get_index(name);
            if held && index != xkb::MOD_INVALID {
                mask |= 1 << index;
            }
        }
        let mut state = State::new(&self.keymap);
        state.update_mask(mask, 0, 0, 0, 0, self.group);
        state
    }

    /// The keystroke `keycode` reports on this group with `modifiers` held.
    fn event(&self, keycode: Keycode, modifiers: gpui::Modifiers) -> gpui::Keystroke {
        let state = self.state(modifiers);
        keystroke_from_xkb(&state, modifiers_from_xkb(&state), keycode)
    }

    /// The label of the key at a US position: the US character where the key types it, else the key's own text.
    fn keycap(&self, keycode: Keycode, us: char, shift: bool, event: &gpui::Keystroke) -> String {
        let us = us.to_string();
        if !self.latin {
            return us;
        }
        let types_us = [false, true].into_iter().any(|shift| {
            let modifiers = gpui::Modifiers {
                shift,
                ..Default::default()
            };
            self.state(modifiers).key_get_utf8(keycode) == us
        });
        if types_us {
            return us;
        }
        let state = self.state(gpui::Modifiers {
            shift,
            ..Default::default()
        });
        let typed = state.key_get_utf8(keycode);
        if typed
            .chars()
            .next()
            .is_some_and(|first| !first.is_control())
        {
            return typed;
        }
        keystroke_underlying_dead_key(state.key_get_one_sym(keycode))
            .unwrap_or_else(|| event.key.clone())
    }
}

#[cfg(any(feature = "wayland", feature = "x11"))]
impl PlatformKeyboardMapper for LinuxKeyboardMapper {
    fn map_key_equivalent(
        &self,
        keystroke: gpui::Keystroke,
        use_key_equivalents: bool,
    ) -> gpui::KeybindingKeystroke {
        // Letters on a Latin group always match where they are printed.
        let position = us_position(&keystroke.key)
            .filter(|(_, us, _)| !(self.latin && us.is_ascii_alphabetic()));
        let (true, Some((keycode, us, level_shift))) = (use_key_equivalents, position) else {
            return gpui::KeybindingKeystroke::from_keystroke(keystroke);
        };
        let held = gpui::Modifiers {
            shift: keystroke.modifiers.shift || level_shift,
            ..keystroke.modifiers
        };
        let event = self.event(keycode, held);
        let keycap = self.keycap(keycode, us, held.shift, &event);
        let inner = gpui::Keystroke {
            key_char: None,
            ..event
        };
        // A keycap that is not the US character needs the Shift its level takes shown.
        let display_modifiers = gpui::Modifiers {
            shift: keystroke.modifiers.shift || (level_shift && keycap != us.to_string()),
            ..keystroke.modifiers
        };
        gpui::KeybindingKeystroke::new_mapped(inner, display_modifiers, keycap, keystroke)
    }

    fn get_key_equivalents(&self) -> Option<&collections::HashMap<char, char>> {
        Some(&self.key_equivalents)
    }
}

/// Resolve a Linux `dev_t` to PCI vendor/device IDs via sysfs, returning a
/// [`CompositorGpuHint`] that the GPU adapter selection code can use to
/// prioritize the compositor's rendering device.
#[cfg(any(feature = "wayland", feature = "x11"))]
pub(super) fn compositor_gpu_hint_from_dev_t(dev: u64) -> Option<gpui_wgpu::CompositorGpuHint> {
    fn dev_major(dev: u64) -> u32 {
        ((dev >> 8) & 0xfff) as u32 | (((dev >> 32) & !0xfff) as u32)
    }

    fn dev_minor(dev: u64) -> u32 {
        (dev & 0xff) as u32 | (((dev >> 12) & !0xff) as u32)
    }

    fn read_sysfs_hex_id(path: &str) -> Option<u32> {
        let content = std::fs::read_to_string(path).ok()?;
        let trimmed = content.trim().strip_prefix("0x").unwrap_or(content.trim());
        u32::from_str_radix(trimmed, 16).ok()
    }

    let major = dev_major(dev);
    let minor = dev_minor(dev);

    let vendor_path = format!("/sys/dev/char/{major}:{minor}/device/vendor");
    let device_path = format!("/sys/dev/char/{major}:{minor}/device/device");

    let vendor_id = read_sysfs_hex_id(&vendor_path)?;
    let device_id = read_sysfs_hex_id(&device_path)?;

    log::info!(
        "Compositor GPU hint: vendor={:#06x}, device={:#06x} (from dev {major}:{minor})",
        vendor_id,
        device_id,
    );

    Some(gpui_wgpu::CompositorGpuHint {
        vendor_id,
        device_id,
    })
}

#[cfg(any(feature = "wayland", feature = "x11"))]
async fn inhibit_idle_sleep(reason: String, executor: BackgroundExecutor) -> Result<ActivityGuard> {
    let proxy = InhibitProxy::new()
        .await
        .context("Idle sleep prevention portal is unavailable")?;
    let request = proxy
        .inhibit(
            None,
            BitFlags::from(InhibitFlags::Suspend),
            InhibitOptions::default().set_reason(reason.as_str()),
        )
        .await
        .context("Failed to request idle sleep prevention")?;
    request
        .response()
        .context("Idle sleep prevention request was rejected")?;
    Ok(release_on_drop(request, executor))
}

#[cfg(any(feature = "wayland", feature = "x11"))]
fn release_on_drop(request: Request<()>, executor: BackgroundExecutor) -> ActivityGuard {
    ActivityGuard::new(move || {
        executor
            .spawn(async move {
                request
                    .close()
                    .await
                    .context("Failed to release idle sleep prevention")
                    .log_err();
            })
            .detach();
    })
}

#[cfg(any(feature = "wayland", feature = "x11"))]
async fn await_idle_sleep_prevention(
    guard_rx: oneshot::Receiver<Result<ActivityGuard>>,
    executor: &BackgroundExecutor,
) -> Result<ActivityGuard> {
    match futures::future::select(guard_rx, executor.timer(Duration::from_secs(10))).await {
        futures::future::Either::Left((Ok(result), _)) => result,
        futures::future::Either::Left((Err(_), _)) => {
            Err(anyhow!("Idle sleep prevention request was abandoned"))
        }
        futures::future::Either::Right(_) => Err(anyhow!(
            "Idle sleep prevention acquisition timed out after 10 seconds"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Point, px};

    #[cfg(any(feature = "wayland", feature = "x11"))]
    #[test]
    fn rejects_null_xkb_context() {
        let context = unsafe {
            // libxkbcommon permits unref on null, matching the value returned by Context::new on failure.
            xkb::Context::from_raw_ptr(std::ptr::null_mut())
        };
        let error = validate_xkb_context(context)
            .err()
            .expect("null XKB context should be rejected");

        assert_eq!(
            error.to_string(),
            "libxkbcommon failed to create an XKB context"
        );
    }

    #[test]
    fn test_is_within_click_distance() {
        let zero = Point::new(px(0.0), px(0.0));
        assert!(is_within_click_distance(zero, Point::new(px(5.0), px(5.0))));
        assert!(is_within_click_distance(
            zero,
            Point::new(px(-4.9), px(5.0))
        ));
        assert!(is_within_click_distance(
            Point::new(px(3.0), px(2.0)),
            Point::new(px(-2.0), px(-2.0))
        ));
        assert!(!is_within_click_distance(
            zero,
            Point::new(px(5.0), px(5.1))
        ),);
    }

    #[cfg(any(feature = "wayland", feature = "x11"))]
    mod idle_sleep_prevention {
        use super::super::await_idle_sleep_prevention;
        use anyhow::{Result, anyhow};
        use futures::channel::oneshot;
        use gpui::{ActivityGuard, TestAppContext};
        use std::{
            sync::{
                Arc,
                atomic::{AtomicUsize, Ordering::SeqCst},
            },
            time::Duration,
        };

        #[gpui::test]
        async fn guard_passes_through_and_releases_on_drop(cx: &mut TestAppContext) {
            let released = Arc::new(AtomicUsize::new(0));
            let (guard_tx, guard_rx) = oneshot::channel();
            assert!(guard_tx.send(Ok(release_guard(released.clone()))).is_ok());
            let guard = await_idle_sleep_prevention(guard_rx, &cx.background_executor)
                .await
                .expect("acquisition should succeed");

            assert_eq!(released.load(SeqCst), 0);
            drop(guard);
            assert_eq!(released.load(SeqCst), 1);
        }

        #[gpui::test]
        async fn acquisition_error_passes_through(cx: &mut TestAppContext) {
            let (guard_tx, guard_rx) = oneshot::channel::<Result<ActivityGuard>>();
            assert!(guard_tx.send(Err(anyhow!("inhibition rejected"))).is_ok());
            assert_eq!(
                await_idle_sleep_prevention(guard_rx, &cx.background_executor)
                    .await
                    .err()
                    .expect("acquisition should fail")
                    .to_string(),
                "inhibition rejected"
            );
        }

        #[gpui::test]
        async fn abandoned_acquisition_is_an_error(cx: &mut TestAppContext) {
            let (guard_tx, guard_rx) = oneshot::channel::<Result<ActivityGuard>>();
            drop(guard_tx);
            assert_eq!(
                await_idle_sleep_prevention(guard_rx, &cx.background_executor)
                    .await
                    .err()
                    .expect("acquisition should fail")
                    .to_string(),
                "Idle sleep prevention request was abandoned"
            );
        }

        #[gpui::test]
        async fn late_guard_after_timeout_is_released(cx: &mut TestAppContext) {
            let released = Arc::new(AtomicUsize::new(0));
            let (guard_tx, guard_rx) = oneshot::channel();
            let task = cx.background_executor.spawn({
                let executor = cx.background_executor.clone();
                async move { await_idle_sleep_prevention(guard_rx, &executor).await }
            });
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_secs(9));
            cx.run_until_parked();
            assert!(!guard_tx.is_canceled());
            cx.executor().advance_clock(Duration::from_secs(1));
            assert_eq!(
                task.await
                    .err()
                    .expect("acquisition should time out")
                    .to_string(),
                "Idle sleep prevention acquisition timed out after 10 seconds"
            );

            assert!(guard_tx.is_canceled());
            if let Err(Ok(guard)) = guard_tx.send(Ok(release_guard(released.clone()))) {
                assert_eq!(released.load(SeqCst), 0);
                drop(guard);
            }
            assert_eq!(released.load(SeqCst), 1);
        }

        fn release_guard(released: Arc<AtomicUsize>) -> ActivityGuard {
            ActivityGuard::new(move || {
                released.fetch_add(1, SeqCst);
            })
        }
    }

    #[cfg(any(feature = "wayland", feature = "x11"))]
    mod read_fd_with_timeout {
        use super::super::{PIPE_READ_TIMEOUT, read_fd_with_timeout};
        use std::io::Write as _;
        use std::time::{Duration, Instant};

        #[test]
        fn reads_data_written_before_close() {
            let mut pipe = filedescriptor::Pipe::new().unwrap();
            pipe.write.write_all(b"hello clipboard").unwrap();
            drop(pipe.write);

            let bytes = read_fd_with_timeout(pipe.read, PIPE_READ_TIMEOUT).unwrap();
            assert_eq!(bytes, b"hello clipboard");
        }

        #[test]
        fn returns_empty_when_writer_closes_without_writing() {
            let pipe = filedescriptor::Pipe::new().unwrap();
            drop(pipe.write);

            let bytes = read_fd_with_timeout(pipe.read, PIPE_READ_TIMEOUT).unwrap();
            assert!(bytes.is_empty());
        }

        #[test]
        fn times_out_when_writer_never_writes() {
            let pipe = filedescriptor::Pipe::new().unwrap();
            let _open_writer = pipe.write;

            let timeout = Duration::from_millis(50);
            let started = Instant::now();
            let result = read_fd_with_timeout(pipe.read, timeout);
            let elapsed = started.elapsed();

            let err = result.unwrap_err();
            assert!(
                err.to_string().contains("timed out"),
                "unexpected error: {err}"
            );
            assert!(elapsed >= timeout, "returned before the timeout elapsed");
        }

        #[test]
        fn times_out_when_writer_stalls_after_partial_write() {
            let mut pipe = filedescriptor::Pipe::new().unwrap();
            pipe.write.write_all(b"partial").unwrap();
            let _open_writer = pipe.write;

            let err = read_fd_with_timeout(pipe.read, Duration::from_millis(50)).unwrap_err();
            assert!(
                err.to_string().contains("timed out"),
                "unexpected error: {err}"
            );
        }

        #[test]
        fn slow_writer_cannot_reset_absolute_deadline() {
            let pipe = filedescriptor::Pipe::new().unwrap();
            let timeout = Duration::from_millis(80);
            let writer = std::thread::spawn({
                let mut write = pipe.write;
                move || {
                    for _ in 0..50 {
                        std::thread::sleep(Duration::from_millis(5));
                        if write.write_all(&[b'x'; 1000]).is_err() {
                            break;
                        }
                    }
                }
            });
            let started = Instant::now();
            let error = read_fd_with_timeout(pipe.read, timeout).unwrap_err();
            writer.join().unwrap();
            assert!(error.to_string().contains("timed out"));
            assert!(started.elapsed() < Duration::from_secs(1));
        }

        #[test]
        fn reads_payload_larger_than_pipe_capacity() {
            let pipe = filedescriptor::Pipe::new().unwrap();
            // Exceeds the 64 KiB pipe capacity, forcing the writer to block.
            let payload = vec![b'z'; 1024 * 1024];

            let writer = std::thread::spawn({
                let mut write = pipe.write;
                let payload = payload.clone();
                move || write.write_all(&payload).unwrap()
            });
            let bytes = read_fd_with_timeout(pipe.read, PIPE_READ_TIMEOUT).unwrap();
            writer.join().unwrap();
            assert_eq!(bytes, payload);
        }
    }
}

#[cfg(test)]
mod display_mode_tests {
    use std::{cell::Cell, process::Command};

    #[cfg(feature = "wayland")]
    use gpui::{AppContext as _, Context, IntoElement, Render, Window, WindowOptions};
    use gpui::{Application, AsyncApp, QuitMode};

    use super::*;

    /// Set in the child process that runs a scenario, naming the scenario to run.
    const SCENARIO_ENV: &str = "GPUI_LINUX_DISPLAY_MODE_SCENARIO";
    const MISSING_WAYLAND_DISPLAY: &str = "/gpui-test/wayland-display-does-not-exist";

    #[cfg(feature = "wayland")]
    #[test]
    fn failed_wayland_attach_preserves_headless_app_and_allows_retry() {
        run_scenario(
            "failed_wayland_attach_preserves_headless_app_and_allows_retry",
            WindowingModes::all(),
            |cx| {
                Box::pin(async move {
                    let entity = cx.new(|_| 1usize);
                    let (release_task, wait_for_release) = oneshot::channel();
                    let queued_task_ran = Rc::new(Cell::new(false));
                    let queued_task = cx.spawn({
                        let queued_task_ran = queued_task_ran.clone();
                        async move |_| {
                            wait_for_release
                                .await
                                .expect("queued task release sender remains alive");
                            queued_task_ran.set(true);
                        }
                    });

                    let first_attach = cx.update(|cx| {
                        cx.request_windowing(WindowingRequest::Windowed(
                            GraphicalEnvironment::detect(),
                        ))
                    });
                    assert_error_contains(
                        first_attach.await,
                        "failed to connect to Wayland compositor",
                    );
                    entity.update(cx, |value, _| *value += 1);
                    release_task
                        .send(())
                        .expect("queued task remains attached to the executor");
                    queued_task.await;
                    assert!(queued_task_ran.get());

                    let second_attach = cx.update(|cx| {
                        cx.request_windowing(WindowingRequest::Windowed(
                            GraphicalEnvironment::detect(),
                        ))
                    });
                    assert_error_contains(
                        second_attach.await,
                        "failed to connect to Wayland compositor",
                    );
                    entity.update(cx, |value, _| *value += 1);
                    assert_eq!(entity.read_with(cx, |value, _| *value), 3);
                })
            },
        );
    }

    #[cfg(feature = "wayland")]
    #[test]
    fn headless_windows_block_switching_to_a_display_server() {
        run_scenario(
            "headless_windows_block_switching_to_a_display_server",
            WindowingModes::all(),
            |cx| {
                Box::pin(async move {
                    let window = cx
                        .update(|cx| {
                            cx.open_window(WindowOptions::default(), |_, cx| cx.new(|_| Blank))
                        })
                        .expect("headless mode opens headless windows");
                    let attach = cx.update(|cx| {
                        cx.request_windowing(WindowingRequest::Windowed(
                            GraphicalEnvironment::detect(),
                        ))
                    });
                    assert_error_contains(attach.await, "while windows are open");

                    cx.update(|cx| window.update(cx, |_, window, _| window.remove_window()))
                        .expect("close the headless window");
                    let attach = cx.update(|cx| {
                        cx.request_windowing(WindowingRequest::Windowed(
                            GraphicalEnvironment::detect(),
                        ))
                    });
                    assert_error_contains(attach.await, "failed to connect to Wayland compositor");
                })
            },
        );
    }

    #[test]
    fn display_modes_limit_switching() {
        run_scenario(
            "display_modes_limit_switching",
            WindowingModes::HEADLESS,
            |cx| {
                Box::pin(async move {
                    let attach = cx.update(|cx| {
                        cx.request_windowing(WindowingRequest::Windowed(
                            GraphicalEnvironment::detect(),
                        ))
                    });
                    assert_error_contains(attach.await, "names no display server that");
                    assert_eq!(cx.update(|cx| cx.compositor_name()), "headless");
                    assert!(cx.update(|cx| cx.graphical_environment()).is_none());

                    let detach = cx.update(|cx| cx.request_windowing(WindowingRequest::Headless));
                    assert_error_contains(detach.await, "already headless");
                })
            },
        );
    }

    #[cfg(feature = "wayland")]
    struct Blank;

    #[cfg(feature = "wayland")]
    impl Render for Blank {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            gpui::Empty
        }
    }

    type Scenario = fn(&mut AsyncApp) -> futures::future::LocalBoxFuture<'_, ()>;

    /// Runs `scenario` on a headless platform in a child process, where `WAYLAND_DISPLAY` names a
    /// socket that doesn't exist.
    fn run_scenario(name: &str, modes: WindowingModes, scenario: Scenario) {
        if std::env::var(SCENARIO_ENV).as_deref() == Ok(name) {
            let platform = Rc::new(LinuxPlatform::new(modes));
            platform.set_initial_windowing(WindowingRequest::Headless);
            let completed = Rc::new(Cell::new(false));
            Application::with_platform(platform)
                .with_quit_mode(QuitMode::Explicit)
                .run({
                    let completed = completed.clone();
                    move |cx| {
                        cx.spawn(async move |cx| {
                            scenario(cx).await;
                            completed.set(true);
                            cx.update(|cx| cx.quit());
                        })
                        .detach();
                    }
                });
            assert!(completed.get(), "scenario did not complete");
            return;
        }

        // Test names don't include the crate's name.
        let module = module_path!()
            .split_once("::")
            .map_or(module_path!(), |(_, module)| module);
        let test_name = format!("{module}::{name}");
        #[allow(
            clippy::disallowed_methods,
            reason = "the test thread has nothing else to do while the child runs"
        )]
        let output = Command::new(std::env::current_exe().expect("current test executable"))
            .args(["--exact", &test_name, "--nocapture"])
            .env(SCENARIO_ENV, name)
            .env("WAYLAND_DISPLAY", MISSING_WAYLAND_DISPLAY)
            .env_remove("DISPLAY")
            .output()
            .expect("run the scenario in a child process");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "scenario failed\nstdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr),
        );
    }

    fn assert_error_contains(result: anyhow::Result<()>, expected: &str) {
        let error = result.expect_err("the transition fails");
        assert!(
            format!("{error:#}").contains(expected),
            "expected an error containing {expected:?}, got: {error:#}"
        );
    }
}

#[cfg(test)]
#[path = "platform/fd_lifetime_tests.rs"]
mod fd_lifetime_tests;

#[cfg(all(test, any(feature = "wayland", feature = "x11")))]
mod i50_xkb_tests {
    use super::*;
    use gpui::{KeyboardLayoutIdentity, Keystroke, Modifiers};

    const SHIFT: &str = xkb::MOD_NAME_SHIFT;
    const CTRL: &str = xkb::MOD_NAME_CTRL;
    const ALT: &str = xkb::MOD_NAME_ALT;
    const SUPER: &str = xkb::MOD_NAME_LOGO;
    const ALTGR: &str = "Mod5";

    // Modifier sets of the shortcut table, Shift alone included.
    const SHORTCUT_MODS: [&[&str]; 7] = [
        &[],
        &[SHIFT],
        &[CTRL],
        &[CTRL, SHIFT],
        &[ALT],
        &[SUPER],
        &[SUPER, SHIFT],
    ];

    // Omarchy 03ff1eff installer rows per the I50-06 matrix (xkeyboard-config 2.42): id, model, layout, variant, options, groups as (XKB name, Latin).
    #[allow(clippy::type_complexity)]
    #[rustfmt::skip]
    const INSTALLER: [(&str, &str, &str, &str, &str, &[(&str, bool)]); 52] = [
        ("01-us", "pc105", "us", "", "terminate:ctrl_alt_bksp", &[("English (US)", true)]),
        ("02-uk", "pc105", "gb", "", "terminate:ctrl_alt_bksp", &[("English (UK)", true)]),
        ("03-dvorak", "pc105", "us", "dvorak", "terminate:ctrl_alt_bksp", &[("English (Dvorak)", true)]),
        ("04-colemak", "pc105", "us", "colemak", "terminate:ctrl_alt_bksp", &[("English (Colemak)", true)]),
        ("05-by", "pc105", "by,us", "", "terminate:ctrl_alt_bksp,grp:shifts_toggle,grp_led:scroll", &[("Belarusian", false), ("English (US)", true)]),
        ("06-be-latin1", "pc105", "be", "", "terminate:ctrl_alt_bksp", &[("Belgian", true)]),
        ("07-bg-cp1251", "pc105", "bg,us", "phonetic,", "terminate:ctrl_alt_bksp,grp:shifts_toggle,grp_led:scroll", &[("Bulgarian (traditional phonetic)", false), ("English (US)", true)]),
        ("08-us", "pc105", "us", "", "terminate:ctrl_alt_bksp", &[("English (US)", true)]),
        ("09-croat", "pc105", "hr", "", "terminate:ctrl_alt_bksp", &[("Croatian", true)]),
        ("10-cz", "pc105", "cz,us", "qwerty,", "terminate:ctrl_alt_bksp,grp:shifts_toggle,grp_led:scroll", &[("Czech (QWERTY)", true), ("English (US)", true)]),
        ("11-dk-latin1", "pc105", "dk", "", "terminate:ctrl_alt_bksp", &[("Danish", true)]),
        ("12-nl", "pc105", "nl", "", "terminate:ctrl_alt_bksp", &[("Dutch", true)]),
        ("13-et", "pc105", "ee", "", "terminate:ctrl_alt_bksp", &[("Estonian", true)]),
        ("14-fi", "pc105", "fi", "", "terminate:ctrl_alt_bksp", &[("Finnish", true)]),
        ("15-fr", "pc105", "fr", "", "terminate:ctrl_alt_bksp", &[("French", true)]),
        ("16-cf", "pc105", "ca", "", "terminate:ctrl_alt_bksp", &[("French (Canada)", true)]),
        ("17-fr_CH", "pc105", "ch", "fr", "terminate:ctrl_alt_bksp", &[("French (Switzerland)", true)]),
        ("18-ge", "pc105", "ge,us", "", "terminate:ctrl_alt_bksp", &[("Georgian", false), ("English (US)", true)]),
        ("19-de", "pc105", "de", "", "terminate:ctrl_alt_bksp", &[("German", true)]),
        ("20-de_CH-latin1", "pc105", "ch", "", "terminate:ctrl_alt_bksp", &[("German (Switzerland)", true)]),
        ("21-gr", "pc105", "gr,us", "", "terminate:ctrl_alt_bksp,grp:shifts_toggle,grp_led:scroll", &[("Greek", false), ("English (US)", true)]),
        ("22-il", "pc105", "il", "", "terminate:ctrl_alt_bksp", &[("Hebrew", false)]),
        ("23-us", "pc105", "us", "", "terminate:ctrl_alt_bksp", &[("English (US)", true)]),
        ("24-hu", "pc105", "hu", "", "terminate:ctrl_alt_bksp", &[("Hungarian", true)]),
        ("25-is-latin1", "pc105", "is", "", "terminate:ctrl_alt_bksp", &[("Icelandic", true)]),
        ("26-ie", "pc105", "ie", "", "terminate:ctrl_alt_bksp", &[("Irish", true)]),
        ("27-it", "pc105", "it", "", "terminate:ctrl_alt_bksp", &[("Italian", true)]),
        ("28-jp106", "jp106", "jp", "", "terminate:ctrl_alt_bksp", &[("Japanese", true)]),
        ("29-us", "pc105", "us", "", "terminate:ctrl_alt_bksp", &[("English (US)", true)]),
        ("30-kazakh", "pc105", "kz,us", "", "terminate:ctrl_alt_bksp,grp:shifts_toggle,grp_led:scroll", &[("Kazakh", false), ("English (US)", true)]),
        ("31-us", "pc105", "kr", "kr104", "", &[("Korean (101/104-key compatible)", true)]),
        ("32-kyrgyz", "pc105", "kg,us", "", "terminate:ctrl_alt_bksp,grp:shifts_toggle,grp_led:scroll", &[("Kyrgyz", false), ("English (US)", true)]),
        ("33-us", "pc105", "la", "", "", &[("Lao", false)]),
        ("34-lv", "pc105", "lv", "apostrophe", "terminate:ctrl_alt_bksp", &[("Latvian (apostrophe)", true)]),
        ("35-lt", "pc105", "lt", "", "terminate:ctrl_alt_bksp", &[("Lithuanian", true)]),
        ("36-mk-utf", "pc105", "mk,us", "", "terminate:ctrl_alt_bksp,grp:shifts_toggle,grp_led:scroll", &[("Macedonian", false), ("English (US)", true)]),
        ("37-no-latin1", "pc105", "no", "", "terminate:ctrl_alt_bksp", &[("Norwegian", true)]),
        ("38-pl", "pc105", "pl", "", "terminate:ctrl_alt_bksp", &[("Polish", true)]),
        ("39-pt-latin1", "pc105", "pt", "", "terminate:ctrl_alt_bksp", &[("Portuguese", true)]),
        ("40-br-abnt2", "abnt2", "br", "", "terminate:ctrl_alt_bksp", &[("Portuguese (Brazil)", true)]),
        ("41-ro", "pc105", "ro", "", "terminate:ctrl_alt_bksp", &[("Romanian", true)]),
        ("42-ru", "pc105", "ru,us", "", "terminate:ctrl_alt_bksp,grp:shifts_toggle,grp_led:scroll", &[("Russian", false), ("English (US)", true)]),
        ("43-sr-latin", "pc105", "rs", "latin", "terminate:ctrl_alt_bksp", &[("Serbian (Latin)", true)]),
        ("44-sk-qwertz", "pc105", "sk", "", "terminate:ctrl_alt_bksp", &[("Slovak", true)]),
        ("45-slovene", "pc105", "si", "", "terminate:ctrl_alt_bksp", &[("Slovenian", true)]),
        ("46-es", "pc105", "es", "", "terminate:ctrl_alt_bksp", &[("Spanish", true)]),
        ("47-la-latin1", "pc105", "latam", "", "terminate:ctrl_alt_bksp", &[("Spanish (Latin American)", true)]),
        ("48-sv-latin1", "pc105", "se", "", "terminate:ctrl_alt_bksp", &[("Swedish", true)]),
        ("49-us", "pc105", "us", "", "terminate:ctrl_alt_bksp", &[("English (US)", true)]),
        ("50-tj_alt-UTF8", "pc105", "tj", "", "terminate:ctrl_alt_bksp", &[("Tajik", false)]),
        ("51-trq", "pc105", "tr", "", "terminate:ctrl_alt_bksp", &[("Turkish", true)]),
        ("52-ua", "pc105", "ua,us", "", "terminate:ctrl_alt_bksp,grp:shifts_toggle,grp_led:scroll", &[("Ukrainian", false), ("English (US)", true)]),
    ];

    fn keymap(model: &str, layout: &str, variant: &str, options: &str) -> xkb::Keymap {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        assert!(!context.get_raw_ptr().is_null());
        xkb::Keymap::new_from_names(
            &context,
            "evdev",
            model,
            layout,
            variant,
            Some(options.to_owned()),
            xkb::COMPILE_NO_FLAGS,
        )
        .unwrap_or_else(|| panic!("XKB cannot compile {layout}({variant}); no US fallback"))
    }

    fn pc105(layout: &str, variant: &str) -> xkb::Keymap {
        keymap("pc105", layout, variant, "")
    }

    fn state(keymap: &xkb::Keymap, group: u32, mods: &[&str]) -> State {
        let mut state = State::new(keymap);
        let mut mask = 0;
        for name in mods {
            let index = keymap.mod_get_index(*name);
            assert_ne!(index, xkb::MOD_INVALID, "{name} is not in the keymap");
            mask |= 1 << index;
        }
        state.update_mask(mask, 0, 0, 0, 0, group);
        assert_eq!(state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE), group);
        state
    }

    fn code(keymap: &xkb::Keymap, name: &str) -> Keycode {
        keymap
            .key_by_name(name)
            .unwrap_or_else(|| panic!("{name} is not in the keymap"))
    }

    fn press_code(keymap: &xkb::Keymap, group: u32, mods: &[&str], code: Keycode) -> Keystroke {
        let state = state(keymap, group, mods);
        keystroke_from_xkb(&state, modifiers_from_xkb(&state), code)
    }

    fn press(keymap: &xkb::Keymap, group: u32, mods: &[&str], key: &str) -> Keystroke {
        press_code(keymap, group, mods, code(keymap, key))
    }

    fn sym_name(keymap: &xkb::Keymap, group: u32, mods: &[&str], key: &str) -> String {
        xkb::keysym_get_name(state(keymap, group, mods).key_get_one_sym(code(keymap, key)))
    }

    // Whether any key types `character` unmodified or with Shift, read from real key states.
    fn prints_anywhere(keymap: &xkb::Keymap, group: u32, character: char) -> bool {
        let text = character.to_string();
        [&[][..], &[SHIFT]].into_iter().any(|mods| {
            let state = state(keymap, group, mods);
            (keymap.min_keycode().raw()..=keymap.max_keycode().raw()).any(|raw| {
                let code = Keycode::new(raw);
                state.key_get_utf8(code) == text && !state.key_get_one_sym(code).is_keypad_key()
            })
        })
    }

    fn layout_at(keymap: &xkb::Keymap, group: u32) -> XkbKeyboardLayout {
        XkbKeyboardLayout::unknown()
            .follow(Some(&state(keymap, group, &[])))
            .expect("a keymap replaces the unknown layout")
    }

    trait IdentityOf {
        fn identity_of(&self) -> KeyboardLayoutIdentity;
    }

    impl IdentityOf for XkbKeyboardLayout {
        fn identity_of(&self) -> KeyboardLayoutIdentity {
            KeyboardLayoutIdentity::parse(self.id()).expect("a Linux layout id is an identity")
        }
    }

    fn binding(text: &str) -> Keystroke {
        Keystroke::parse(text).expect("valid binding")
    }

    fn mapped(
        keymap: &xkb::Keymap,
        group: u32,
        text: &str,
        physical: bool,
    ) -> gpui::KeybindingKeystroke {
        layout_at(keymap, group)
            .mapper()
            .map_key_equivalent(binding(text), physical)
    }

    fn identity(keystroke: &Keystroke) -> (String, Modifiers) {
        (keystroke.key.clone(), keystroke.modifiers)
    }

    fn modifiers(mods: &[&str]) -> Modifiers {
        Modifiers {
            shift: mods.contains(&SHIFT),
            control: mods.contains(&CTRL),
            alt: mods.contains(&ALT),
            platform: mods.contains(&SUPER),
            function: false,
        }
    }

    #[test]
    fn i50_xkb_nonlatin_physical() {
        for (layout, variant) in [
            ("ru,us", ""),
            ("il", ""),
            ("gr,us", ""),
            ("ua,us", ""),
            ("by,us", ""),
            ("bg,us", "phonetic,"),
            ("ge,us", ""),
            ("kz,us", ""),
            ("kg,us", ""),
            ("mk,us", ""),
            ("la", ""),
            ("tj", ""),
        ] {
            let keymap = pc105(layout, variant);
            assert!(!group_is_latin(&keymap, 0), "{layout} group 0 is not Latin");
            for &(raw, lower, upper) in &US_POSITIONS {
                for mods in SHORTCUT_MODS {
                    let event = press_code(&keymap, 0, mods, Keycode::new(raw));
                    let shifted = mods.contains(&SHIFT) && !lower.is_ascii_alphabetic();
                    let expected = if shifted { upper } else { lower };
                    assert_eq!(
                        event.key,
                        expected.to_string(),
                        "{layout} keycode {raw} {mods:?} reports the US key at its position"
                    );
                }
            }
        }
        let russian = pc105("ru,us", "");
        // Real XKB: the US slash position types a period on Russian, and the US z position types ya.
        assert_eq!(sym_name(&russian, 0, &[], "AB10"), "period");
        assert_eq!(sym_name(&russian, 0, &[CTRL], "AB01"), "Cyrillic_ya");
        let typed = press(&russian, 0, &[], "AC07");
        assert_eq!(typed.key, "j");
        assert_eq!(typed.key_char.as_deref(), Some("о"), "text stays Cyrillic");
        let hebrew = pc105("il", "");
        assert_eq!(sym_name(&hebrew, 0, &[], "AD01"), "slash");
        let quit = press(&hebrew, 0, &[CTRL], "AD01");
        assert_eq!(identity(&quit), ("q".into(), Modifiers::control()));
        assert_eq!(
            press(&hebrew, 0, &[], "AD01").key_char.as_deref(),
            Some("/")
        );
    }

    #[test]
    fn i50_xkb_latin_printed() {
        for (layout, variant, key, letter) in [
            ("de", "", "AB01", "y"),
            ("de", "", "AD06", "z"),
            ("fr", "", "AD01", "a"),
            ("fr", "", "AC01", "q"),
            ("fr", "", "AB01", "w"),
            ("fr", "", "AC10", "m"),
            ("us", "dvorak", "AD05", "y"),
            ("us", "dvorak", "AC01", "a"),
            ("us", "colemak", "AD03", "f"),
            ("us", "colemak", "AC02", "r"),
        ] {
            let keymap = pc105(layout, variant);
            assert!(group_is_latin(&keymap, 0), "{layout}({variant}) is Latin");
            for mods in SHORTCUT_MODS {
                let event = press(&keymap, 0, mods, key);
                assert_eq!(
                    identity(&event),
                    (letter.to_owned(), modifiers(mods)),
                    "{layout}({variant}) {key} {mods:?} types its printed letter"
                );
            }
        }
        let german = pc105("de", "");
        let french = pc105("fr", "");
        let turkish = pc105("tr", "");
        // Printed punctuation is matched where the layout prints it.
        assert_eq!(press(&german, 0, &[SHIFT], "AE07").key, "/");
        assert_eq!(press(&german, 0, &[CTRL, SHIFT], "AE07").key, "/");
        assert_eq!(press(&french, 0, &[], "AB08").key, ";");
        assert_eq!(press(&french, 0, &[], "AB10").key, "!");
        // A non-ASCII key answers to its US position, or keeps its keysym name where another key prints that character.
        let swiss = pc105("ch", "");
        let icelandic = pc105("is", "");
        for (keymap, mods, key, us) in [
            (&german, &[][..], "AD11", "["),
            (&german, &[SHIFT][..], "AD11", "{"),
            (&german, &[][..], "AC11", "adiaeresis"),
            (&turkish, &[][..], "AB08", "odiaeresis"),
            (&turkish, &[SHIFT][..], "AB09", "ccedilla"),
            (&icelandic, &[SHIFT][..], "AB10", "thorn"),
            (&swiss, &[CTRL, SHIFT][..], "AE12", "dead_grave"),
        ] {
            assert_eq!(press(keymap, 0, mods, key).key, us, "{key} {mods:?}");
        }
        assert_eq!(press(&german, 0, &[SHIFT], "BKSL").key, "'");
        assert_eq!(press(&turkish, 0, &[], "BKSL").key, ",");
        assert_eq!(press(&icelandic, 0, &[SHIFT], "AD12").key, "?");
        assert_eq!(press(&swiss, 0, &[CTRL, SHIFT], "AE01").key, "+");
        assert_eq!(
            press(&turkish, 0, &[SHIFT], "AB08").key_char.as_deref(),
            Some("Ö")
        );
        // A printed binding stays as written and is labelled with itself.
        let printed = mapped(&german, 0, "ctrl-y", false);
        assert_eq!(
            identity(printed.inner()),
            ("y".into(), Modifiers::control())
        );
        assert_eq!(printed.key(), "y");
        // A binding matched at its US position resolves to that key's event and is labelled with its keycap.
        let position = mapped(&german, 0, "ctrl-[", true);
        assert_eq!(
            identity(position.inner()),
            identity(&press(&german, 0, &[CTRL], "AD11"))
        );
        assert_eq!(position.key(), "ü");
        // A dead key at the US position is labelled with the character it composes.
        let previous = mapped(&french, 0, "ctrl-[", true);
        assert_eq!(
            identity(previous.inner()),
            identity(&press(&french, 0, &[CTRL], "AD11"))
        );
        assert_eq!(previous.key(), "^");
        // Letters on a Latin group are never moved to their US positions.
        let undo = mapped(&german, 0, "ctrl-z", true);
        assert_eq!(identity(undo.inner()), ("z".into(), Modifiers::control()));
        assert_eq!(undo.key(), "z");
        assert_eq!(position.modifiers(), &Modifiers::control());
    }

    #[test]
    fn i50_xkb_azerty_digits() {
        let digits = [
            "AE01", "AE02", "AE03", "AE04", "AE05", "AE06", "AE07", "AE08", "AE09", "AE10",
        ];
        for layout in ["fr", "be"] {
            let keymap = pc105(layout, "");
            for (index, key) in digits.iter().enumerate() {
                let digit = ((index + 1) % 10).to_string();
                assert_eq!(
                    press(&keymap, 0, &[SHIFT], key).key,
                    digit,
                    "{layout} prints {digit} on Shift"
                );
                for chord in ["", "ctrl-", "ctrl-shift-", "alt-", "super-"] {
                    let text = format!("{chord}{digit}");
                    let target = mapped(&keymap, 0, &text, true);
                    let mods: Vec<&str> = [
                        (chord.contains("ctrl"), CTRL),
                        (chord.contains("shift"), SHIFT),
                        (chord.contains("alt"), ALT),
                        (chord.contains("super"), SUPER),
                    ]
                    .into_iter()
                    .filter_map(|(on, name)| on.then_some(name))
                    .collect();
                    assert_eq!(
                        identity(target.inner()),
                        identity(&press(&keymap, 0, &mods, key)),
                        "{layout} {text} resolves to the digit key's own event"
                    );
                    assert_eq!(
                        target.key(),
                        digit,
                        "{layout} {text} is labelled with its digit"
                    );
                }
            }
            let bare = press(&keymap, 0, &[], "AE01");
            assert_eq!(
                bare.key_char.as_deref(),
                Some("&"),
                "{layout} still types its printed character"
            );
            let mapper = layout_at(&keymap, 0).mapper();
            let equivalents = mapper
                .get_key_equivalents()
                .expect("Linux reports key equivalents");
            assert_eq!(equivalents.get(&'1'), Some(&'&'));
            assert_eq!(
                equivalents.get(&'a'),
                None,
                "{layout} letters match where printed"
            );
            let quit = mapped(&keymap, 0, "ctrl-q", true);
            assert_eq!(identity(quit.inner()), ("q".into(), Modifiers::control()));
            assert_eq!(
                identity(&press(&keymap, 0, &[CTRL], "AC01")),
                identity(quit.inner())
            );
        }
        let us = pc105("us", "");
        let reset = mapped(&us, 0, "ctrl-shift-0", true);
        assert_eq!(
            identity(reset.inner()),
            identity(&press(&us, 0, &[CTRL, SHIFT], "AE10"))
        );
        assert_eq!(reset.key(), "0");
        assert_eq!(reset.modifiers(), &Modifiers::control_shift());
        let french = pc105("fr", "");
        let bang = mapped(&french, 0, "!", true);
        assert_eq!(
            identity(bang.inner()),
            identity(&press(&french, 0, &[SHIFT], "AE01"))
        );
        assert_eq!((bang.key(), bang.modifiers()), ("1", &Modifiers::shift()));
        let one = mapped(&french, 0, "1", true);
        assert_eq!((one.key(), one.modifiers()), ("1", &Modifiers::none()));
        // Serialising gives back the written binding, so reparsing with key equivalents maps it the same way.
        for written in ["!", "1", "ctrl-shift-0", "ctrl-["] {
            let first = mapped(&french, 0, written, true);
            assert_eq!(first.unparse(), written);
            let again = mapped(&french, 0, &first.unparse(), true);
            assert_eq!(again, first, "{written} round-trips");
        }
        // Editing a mapped binding leaves the unmapped binding as written, which its caller maps again.
        let mut edited = mapped(&french, 0, "1", true);
        edited.set_modifiers(Modifiers::none());
        assert_eq!(
            edited,
            gpui::KeybindingKeystroke::from_keystroke(binding("1"))
        );
        edited.set_modifiers(Modifiers::control());
        edited.set_key("2".into());
        assert_eq!(
            edited,
            gpui::KeybindingKeystroke::from_keystroke(binding("ctrl-2"))
        );
        assert_eq!(edited.unparse(), "ctrl-2");
        let remapped = mapped(&french, 0, &edited.unparse(), true);
        assert_eq!(
            identity(remapped.inner()),
            identity(&press(&french, 0, &[CTRL], "AE02"))
        );
    }

    #[test]
    fn i50_xkb_altgr_editor() {
        for (layout, key, text) in [
            ("de", "AD01", "@"),
            ("pl", "AC01", "ą"),
            ("fr", "AE10", "@"),
        ] {
            let keymap = pc105(layout, "");
            let altgr = state(&keymap, 0, &[ALTGR]);
            let event = keystroke_from_xkb(&altgr, modifiers_from_xkb(&altgr), code(&keymap, key));
            assert_eq!(
                event.key_char.as_deref(),
                Some(text),
                "{layout} AltGr {key} types {text}"
            );
            assert!(
                prefers_character_input(&altgr),
                "{layout} AltGr {key} is text input, never a command"
            );
            let command = state(&keymap, 0, &[ALTGR, CTRL]);
            assert!(
                prefers_character_input(&command),
                "{layout} Ctrl+AltGr {key} holds AltGr"
            );
            for mods in SHORTCUT_MODS {
                assert!(
                    !prefers_character_input(&state(&keymap, 0, mods)),
                    "{layout} {mods:?} without AltGr"
                );
            }
            let ctrl_alt = state(&keymap, 0, &[CTRL, ALT]);
            assert!(
                !prefers_character_input(&ctrl_alt),
                "{layout} Ctrl+Alt stays a command chord"
            );
        }
    }

    #[test]
    fn i50_xkb_dead_key() {
        let french = pc105("fr", "");
        let german = pc105("de", "");
        for (keymap, mods, key, sym, us) in [
            (&french, &[][..], "AD11", "dead_circumflex", "["),
            (&french, &[SHIFT][..], "AD11", "dead_diaeresis", "{"),
            (&german, &[][..], "TLDE", "dead_circumflex", "`"),
        ] {
            assert_eq!(sym_name(keymap, 0, mods, key), sym);
            let event = press(keymap, 0, mods, key);
            assert!(
                composes(event.modifiers),
                "{sym} without a command modifier starts a composition"
            );
            assert_eq!(event.key_char, None, "{sym} types nothing by itself");
            assert_eq!(event.key, us, "{sym} never reports its keysym name");
            for command in [&[CTRL][..], &[SUPER], &[ALT], &[CTRL, ALT]] {
                let held: Vec<&str> = mods.iter().chain(command).copied().collect();
                assert_eq!(sym_name(keymap, 0, &held, key), sym);
                let chord = press(keymap, 0, &held, key);
                assert!(
                    !composes(chord.modifiers),
                    "{sym} {held:?} is a shortcut, not a composition"
                );
                assert_eq!(chord.key, us, "{sym} {held:?} matches at its US position");
            }
        }
        for (layout, variant) in [
            ("fr", ""),
            ("de", ""),
            ("gr,us", ""),
            ("ca", ""),
            ("us", "intl"),
        ] {
            let keymap = pc105(layout, variant);
            for &(raw, _, _) in &US_POSITIONS {
                for mods in SHORTCUT_MODS {
                    let state = state(&keymap, 0, mods);
                    let sym = xkb::keysym_get_name(state.key_get_one_sym(Keycode::new(raw)));
                    if sym.starts_with("dead_") {
                        let event = keystroke_from_xkb(
                            &state,
                            modifiers_from_xkb(&state),
                            Keycode::new(raw),
                        );
                        let us = guess_ascii(Keycode::new(raw), mods.contains(&SHIFT))
                            .expect("a main-block key");
                        let expected =
                            if group_is_latin(&keymap, 0) && prints_anywhere(&keymap, 0, us) {
                                sym.to_lowercase()
                            } else {
                                us.to_string()
                            };
                        assert_eq!(
                            event.key, expected,
                            "{layout}({variant}) {raw} {mods:?} {sym}"
                        );
                        assert_eq!(
                            composes(event.modifiers),
                            !mods.iter().any(|m| [CTRL, ALT, SUPER].contains(m))
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn i50_xkb_group_switch_generation() {
        let keymap = keymap("pc105", "us,ru", "", "grp:shifts_toggle");
        let us = layout_at(&keymap, 0);
        assert_eq!(
            (us.identity_of().name.as_str(), us.identity_of().group),
            ("English (US)", 0)
        );
        assert!(
            us.identity_of().generation > 0,
            "a real keymap starts a generation"
        );
        assert!(
            us.follow(Some(&state(&keymap, 0, &[]))).is_none(),
            "an unchanged group keeps its generation"
        );
        assert!(
            us.follow(Some(&state(&keymap, 0, &[CTRL]))).is_none(),
            "modifiers never change the layout"
        );
        let russian = us
            .follow(Some(&state(&keymap, 1, &[])))
            .expect("a group switch is a change");
        assert_eq!(
            (
                russian.identity_of().name.as_str(),
                russian.identity_of().group
            ),
            ("Russian", 1)
        );
        assert!(russian.identity_of().generation > us.identity_of().generation);
        assert_eq!(russian.id(), russian.identity_of().to_id());
        assert_eq!(russian.name(), "Russian");
        let back = russian
            .follow(Some(&state(&keymap, 0, &[])))
            .expect("switching back is a change");
        assert!(
            back.identity_of().generation > russian.identity_of().generation,
            "generations are never reused"
        );
        let recompiled = keymap_with_same_names();
        let fresh = back
            .follow(Some(&state(&recompiled, 0, &[])))
            .expect("a new keymap is a change");
        assert!(fresh.identity_of().generation > back.identity_of().generation);
        let lost = fresh.follow(None).expect("losing the keymap is a change");
        assert_eq!(lost.identity_of().name, "unknown");
        assert!(lost.identity_of().generation > fresh.identity_of().generation);
        assert!(lost.follow(None).is_none());
        // Both groups match the same shortcut letters; only the typed text and the keycap source differ.
        for group in [0, 1] {
            let undo = press(&keymap, group, &[CTRL], "AB01");
            assert_eq!(identity(&undo), ("z".into(), Modifiers::control()));
            let target = mapped(&keymap, group, "ctrl-z", true);
            assert_eq!(identity(target.inner()), identity(&undo));
            assert_eq!(
                target.key(),
                "z",
                "group {group} labels the physical z key with its shortcut letter"
            );
        }
        assert_eq!(
            press(&keymap, 1, &[], "AB01").key_char.as_deref(),
            Some("я")
        );
        let filter = mapped(&keymap, 1, "/", true);
        assert_eq!(
            filter.inner().key,
            "/",
            "Russian matches slash at its US position"
        );
        assert_eq!(press(&keymap, 1, &[], "AB10").key, "/");
    }

    fn keymap_with_same_names() -> xkb::Keymap {
        keymap("pc105", "us,ru", "", "grp:shifts_toggle")
    }

    #[test]
    fn i50_xkb_punctuation_matrix() {
        assert_eq!(
            INSTALLER.len(),
            52,
            "literal installer entries at the pinned source"
        );
        let groups: usize = INSTALLER.iter().map(|row| row.5.len()).sum();
        assert_eq!(groups, 62, "configured groups at the pinned source");
        let mut checked = 0;
        for (id, model, layout, variant, options, groups) in INSTALLER {
            let keymap = keymap(model, layout, variant, options);
            assert_eq!(keymap.num_layouts() as usize, groups.len(), "{id}");
            let has_altgr = keymap.mod_get_index(ALTGR) != xkb::MOD_INVALID;
            for (index, &(name, latin)) in groups.iter().enumerate() {
                let group = index as u32;
                assert_eq!(keymap.layout_get_name(group), name, "{id} group {group}");
                assert_eq!(group_is_latin(&keymap, group), latin, "{id} {name} script");
                let mapper = layout_at(&keymap, group).mapper();
                for &(raw, lower, upper) in &US_POSITIONS {
                    let code = Keycode::new(raw);
                    for mods in SHORTCUT_MODS {
                        let event = press_code(&keymap, group, mods, code);
                        let shifted = mods.contains(&SHIFT);
                        let target = if shifted && !lower.is_ascii_alphabetic() {
                            upper
                        } else {
                            lower
                        };
                        let canonical = Keystroke {
                            modifiers: modifiers(mods),
                            key: target.to_string(),
                            key_char: None,
                            is_keypad: false,
                        };
                        let resolved = mapper.map_key_equivalent(canonical.clone(), true);
                        let expected = if latin && lower.is_ascii_alphabetic() {
                            identity(&canonical)
                        } else {
                            identity(&event)
                        };
                        assert_eq!(
                            identity(resolved.inner()),
                            expected,
                            "{id} {name}: {canonical} resolves at its US position, or as printed for a Latin letter"
                        );
                        assert_eq!(
                            resolved.modifiers(),
                            &canonical.modifiers,
                            "{id} {name} {canonical}"
                        );
                        if !latin {
                            assert_eq!(
                                event.key,
                                target.to_string(),
                                "{id} {name}: {canonical} rule c"
                            );
                            assert_eq!(
                                resolved.key(),
                                target.to_string(),
                                "{id} {name}: label of {canonical}"
                            );
                        }
                        let printed = mapper.map_key_equivalent(canonical.clone(), false);
                        assert_eq!(
                            printed.inner(),
                            &canonical,
                            "{id} {name}: printed bindings stay as written"
                        );
                        assert_eq!(printed.key(), canonical.key, "{id} {name}: printed label");
                        checked += 1;
                    }
                }
                if has_altgr {
                    assert!(
                        prefers_character_input(&state(&keymap, group, &[ALTGR])),
                        "{id} {name} AltGr"
                    );
                }
                if latin {
                    assert_printed_paths(id, &keymap, group);
                    assert_fallbacks_are_unique(id, &keymap, group);
                }
            }
        }
        assert_eq!(checked, 62 * US_POSITIONS.len() * SHORTCUT_MODS.len());
    }

    // Rule a: every ASCII character a Latin group types without AltGr is matched on the key that prints it.
    fn assert_printed_paths(id: &str, keymap: &xkb::Keymap, group: u32) {
        let mut reached = 0;
        for raw in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
            let code = Keycode::new(raw);
            for level in [&[][..], &[SHIFT]] {
                let level_state = state(keymap, group, level);
                let sym = level_state.key_get_one_sym(code);
                let Some(printed) = char::from_u32(xkb::keysym_to_utf32(sym))
                    .filter(|c| c.is_ascii_graphic() && !sym.is_keypad_key())
                else {
                    continue;
                };
                for command in [&[][..], &[CTRL], &[SUPER], &[ALT]] {
                    let held: Vec<&str> = level.iter().chain(command).copied().collect();
                    if state(keymap, group, &held).key_get_one_sym(code) != sym {
                        continue;
                    }
                    let event = press_code(keymap, group, &held, code);
                    assert_eq!(
                        event.key,
                        printed.to_ascii_lowercase().to_string(),
                        "{id} group {group} keycode {raw} {held:?} prints {printed}"
                    );
                    reached += 1;
                }
            }
        }
        assert!(reached > 0, "{id} group {group} prints ASCII");
    }

    // A key reporting a US character it does not print never shares that character with a key that prints it.
    fn assert_fallbacks_are_unique(id: &str, keymap: &xkb::Keymap, group: u32) {
        for mods in SHORTCUT_MODS {
            let state = state(keymap, group, mods);
            for &(raw, _, _) in &US_POSITIONS {
                let code = Keycode::new(raw);
                let event = keystroke_from_xkb(&state, modifiers_from_xkb(&state), code);
                let mut chars = event.key.chars();
                let (Some(reported), None) = (chars.next(), chars.next()) else {
                    continue;
                };
                let sym = state.key_get_one_sym(code);
                let printed =
                    char::from_u32(xkb::keysym_to_utf32(sym)).map(|c| c.to_ascii_lowercase());
                if printed != Some(reported) {
                    assert!(
                        !prints_anywhere(keymap, group, reported),
                        "{id} group {group} keycode {raw} {mods:?} reports {reported}, which another key prints"
                    );
                }
            }
        }
    }

    #[test]
    fn i50_xkb_us_positions_match_us() {
        let us = pc105("us", "");
        for &(raw, lower, upper) in &US_POSITIONS {
            let code = Keycode::new(raw);
            assert_eq!(state(&us, 0, &[]).key_get_utf8(code), lower.to_string());
            assert_eq!(
                state(&us, 0, &[SHIFT]).key_get_utf8(code),
                upper.to_string()
            );
            assert_eq!(guess_ascii(code, false), Some(lower));
            let shifted = if lower.is_ascii_alphabetic() {
                lower
            } else {
                upper
            };
            assert_eq!(guess_ascii(code, true), Some(shifted));
        }
    }

    // Writes every event of every installer group, for the matrix comparison kept beside the unit's evidence.
    #[test]
    #[ignore = "evidence dump, run on demand"]
    fn i50_dump_xkb_events() {
        for (id, model, layout, variant, options, groups) in INSTALLER {
            let keymap = keymap(model, layout, variant, options);
            for group in 0..groups.len() as u32 {
                for mask in 0..32u32 {
                    let mods: Vec<&str> = [SHIFT, CTRL, ALT, SUPER, ALTGR]
                        .into_iter()
                        .enumerate()
                        .filter_map(|(bit, name)| (mask & (1 << bit) != 0).then_some(name))
                        .collect();
                    let state = state(&keymap, group, &mods);
                    for raw in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
                        let code = Keycode::new(raw);
                        let Some(name) = keymap.key_get_name(code) else {
                            continue;
                        };
                        if state.key_get_one_sym(code).raw() == 0 {
                            continue;
                        }
                        let event = keystroke_from_xkb(&state, modifiers_from_xkb(&state), code);
                        let dead =
                            xkb::keysym_get_name(state.key_get_one_sym(code)).starts_with("dead_");
                        println!(
                            "I50XKB\t{id}\t{group}\t{name}:{mask}\t{}\t{}{}{}{}\t{}\t{}\t{}\t{}",
                            event.key.escape_default(),
                            u8::from(event.modifiers.shift),
                            u8::from(event.modifiers.control),
                            u8::from(event.modifiers.alt),
                            u8::from(event.modifiers.platform),
                            event.key_char.clone().unwrap_or_default().escape_default(),
                            u8::from(event.is_keypad),
                            u8::from(prefers_character_input(&state)),
                            u8::from(dead && composes(event.modifiers)),
                        );
                    }
                }
            }
        }
    }
}
