//! The AppKit layer: the status item, the menu that mirrors the AirPods noise-control panel, and
//! the object every menu item targets.
//!
//! Everything here runs on the main thread. The BLE runtime only ever publishes into
//! [`SharedState`] and then asks for a redraw through [`Waker`].

use std::cell::RefCell;
use std::sync::Arc;

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, ProtocolObject};
use objc2::{
    ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication, NSApplicationActivationPolicy,
    NSApplicationDelegate, NSCellImagePosition, NSControlStateValueOff, NSControlStateValueOn,
    NSEventModifierFlags, NSImage, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem, NSTextField,
    NSVariableStatusItemLength,
};
use objc2_foundation::{NSNotification, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tracing::{debug, warn};

use crate::config::Config;
use crate::link::Waker;
use crate::menu::{
    AMBIENT_VOLUME_MAX, AMBIENT_VOLUME_MIN, Action, GestureBit, PROMPT_VOLUME_MAX,
    SHUTDOWN_CHOICES, SharedState, UiState, ambient_volume_label, anc_label, battery_line,
    battery_title, gesture_label, header_lines, icon_symbol, ldac_label, ldac_warning,
    prompt_volume_label, shutdown_timer, sound_effect_label, supports, timer_label,
};
use edifier_proto::{AncMode, DeviceModel, Feature, LdacMode, SoundEffect};

/// Width of the text field offered when renaming the headset.
const RENAME_FIELD_WIDTH: f64 = 240.0;
/// Height of the text field offered when renaming the headset.
const RENAME_FIELD_HEIGHT: f64 = 24.0;

/// AppKit objects owned by the process.
struct Main {
    /// Kept alive for the lifetime of the process: AppKit holds delegates weakly.
    #[allow(dead_code)]
    delegate: Option<Retained<AppDelegate>>,
    /// The status item and its menu, once the app finished launching.
    ui: Option<Ui>,
}

thread_local! {
    /// The only place AppKit objects live; touched from the main thread alone.
    static MAIN: RefCell<Main> = const {
        RefCell::new(Main {
            delegate: None,
            ui: None,
        })
    };
}

/// Instance variables of the application delegate.
struct DelegateIvars {
    state: SharedState,
    requests: mpsc::Sender<Action>,
    settings: Config,
}

define_class!(
    /// Builds the status item once AppKit has finished launching.
    ///
    /// SAFETY: `NSObject` has no subclassing requirements, and this class is only ever used from
    /// the main thread, like the protocol it implements.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "EdifierMenubarAppDelegate"]
    #[ivars = DelegateIvars]
    struct AppDelegate;

    impl AppDelegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            Ui::install(self);
        }

        #[unsafe(method(applicationWillTerminate:))]
        fn will_terminate(&self, _notification: &NSNotification) {
            debug!("edifier-menubar is terminating");
        }
    }

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {}
);

impl AppDelegate {
    /// Allocate and initialise the delegate.
    fn new(
        mtm: MainThreadMarker,
        state: SharedState,
        requests: mpsc::Sender<Action>,
        settings: Config,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DelegateIvars {
            state,
            requests,
            settings,
        });
        // SAFETY: the ivars are set, so `NSObject`'s `init` sees a fully initialised instance.
        unsafe { msg_send![super(this), init] }
    }
}

/// Instance variables of the menu action target.
struct TargetIvars {
    state: SharedState,
    requests: mpsc::Sender<Action>,
    settings: Config,
}

define_class!(
    /// Target of every menu item: one selector, dispatching on the item's tag.
    ///
    /// SAFETY: `NSObject` has no subclassing requirements; the target shows alerts, so it is
    /// main-thread-only.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "EdifierMenubarActionTarget"]
    #[ivars = TargetIvars]
    struct ActionTarget;

    impl ActionTarget {
        #[unsafe(method(dispatchAction:))]
        fn dispatch_action(&self, sender: Option<&NSMenuItem>) {
            let Some(item) = sender else {
                debug!("action dispatched without a menu item sender");
                return;
            };
            debug!(
                tag = item.tag(),
                title = %item.title(),
                class = ?item.class().name(),
                "menu item action fired",
            );
            let tag = item.tag();
            let Some(action) = Action::from_tag(tag) else {
                warn!(tag, "clicked a menu item with an unknown tag");
                return;
            };
            self.handle(action);
        }
    }

    unsafe impl NSObjectProtocol for ActionTarget {}
);

impl ActionTarget {
    /// Allocate and initialise the target.
    fn new(
        mtm: MainThreadMarker,
        state: SharedState,
        requests: mpsc::Sender<Action>,
        settings: Config,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TargetIvars {
            state,
            requests,
            settings,
        });
        // SAFETY: the ivars are set, so `NSObject`'s `init` sees a fully initialised instance.
        unsafe { msg_send![super(this), init] }
    }

    /// Act on a click, prompting where an action needs input or confirmation.
    fn handle(&self, action: Action) {
        match action {
            Action::Quit => {
                let app = NSApplication::sharedApplication(self.mtm());
                app.terminate(None);
            }
            Action::Rename => {
                if let Some(name) = self.prompt_for_name() {
                    self.dispatch(Action::SetName(name));
                }
            }
            Action::FactoryReset => {
                if self.confirm_factory_reset() {
                    self.dispatch(Action::FactoryReset);
                }
            }
            action => self.dispatch(action),
        }
    }

    /// Hand an action to the BLE runtime.
    fn dispatch(&self, action: Action) {
        let ivars = self.ivars();
        let label = action.label();
        match ivars.requests.try_send(action) {
            Ok(()) => debug!(action = %label, "menu item queued"),
            Err(TrySendError::Full(action)) => {
                warn!(action = %label, "the request queue is full");
                ivars.state.update(|state| {
                    state.error = Some(format!(
                        "the menu bar app is busy; {} was not sent",
                        action.label()
                    ));
                });
                wake();
            }
            Err(TrySendError::Closed(_)) => {
                warn!("the ble runtime is gone; is the app shutting down?");
            }
        }
    }

    /// Ask for a new headset name; `None` when the user cancels or types nothing.
    fn prompt_for_name(&self) -> Option<String> {
        let mtm = self.mtm();
        let ivars = self.ivars();
        let current = ivars
            .state
            .snapshot()
            .status
            .name
            .or_else(|| ivars.settings.device_name.clone())
            .unwrap_or_default();

        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str("Rename the headset"));
        alert.setInformativeText(&NSString::from_str(
            "The name is what the headset advertises to every host it has been paired with.",
        ));
        alert.addButtonWithTitle(&NSString::from_str("Rename"));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));

        let field = NSTextField::new(mtm);
        field.setFrame(NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(RENAME_FIELD_WIDTH, RENAME_FIELD_HEIGHT),
        ));
        field.setStringValue(&NSString::from_str(&current));
        alert.setAccessoryView(Some(&field));

        activate(mtm);
        if alert.runModal() != NSAlertFirstButtonReturn {
            return None;
        }
        let name = field.stringValue().to_string();
        let name = name.trim();
        (!name.is_empty()).then(|| name.to_string())
    }

    /// Ask before wiping the headset.
    fn confirm_factory_reset(&self) -> bool {
        let mtm = self.mtm();
        let alert = NSAlert::new(mtm);
        alert.setAlertStyle(NSAlertStyle::Critical);
        alert.setMessageText(&NSString::from_str("Factory reset the headset?"));
        alert.setInformativeText(&NSString::from_str(
            "Every setting is lost and the headset forgets every paired device.",
        ));
        alert.addButtonWithTitle(&NSString::from_str("Factory Reset"));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));

        activate(mtm);
        alert.runModal() == NSAlertFirstButtonReturn
    }
}

/// The status item, its menu and the target every item points at.
struct Ui {
    status_item: Retained<NSStatusItem>,
    menu: Retained<NSMenu>,
    target: Retained<ActionTarget>,
    state: SharedState,
}

impl Ui {
    /// Create the status item and its first menu; runs from `applicationDidFinishLaunching:`.
    fn install(delegate: &AppDelegate) {
        let mtm = delegate.mtm();
        let ivars = delegate.ivars();

        let status_item =
            NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        let menu = NSMenu::new(mtm);
        // Menu items are enabled by hand: without this AppKit disables every item whose target
        // does not implement `validateMenuItem:`, which would grey out the whole menu.
        menu.setAutoenablesItems(false);
        let target = ActionTarget::new(
            mtm,
            ivars.state.clone(),
            ivars.requests.clone(),
            ivars.settings.clone(),
        );

        status_item.setMenu(Some(&menu));
        let ui = Ui {
            status_item,
            menu,
            target,
            state: ivars.state.clone(),
        };
        ui.redraw();

        MAIN.with(|slot| slot.borrow_mut().ui = Some(ui));
        debug!("status item installed");
    }

    /// Refresh the icon, the battery title and every menu entry from the current snapshot.
    fn redraw(&self) {
        let state = self.state.snapshot();
        self.update_button(&state);
        self.rebuild_menu(&state);
    }

    /// Icon plus battery percentage in the menu bar.
    fn update_button(&self, state: &UiState) {
        let mtm = self.target.mtm();
        let description = accessibility_description(state);
        // `headphones.slash` may be missing on an older SF Symbols release, so `headphones` is the
        // fallback and the battery title alone still conveys the state.
        let image =
            symbol(icon_symbol(state), &description).or_else(|| symbol("headphones", &description));
        if let Some(image) = &image {
            image.setTemplate(true);
        }
        if let Some(button) = self.status_item.button(mtm) {
            button.setImage(image.as_deref());
            button.setImagePosition(NSCellImagePosition::ImageLeading);
            button.setTitle(&NSString::from_str(&battery_title(state)));
        }
    }

    /// Rebuild the whole menu; every entry carries the action for its tag.
    ///
    /// Every control the app knows is always listed; the ones this headset does not implement are
    /// disabled with a note, so a missing feature is visible instead of silently absent.
    fn rebuild_menu(&self, state: &UiState) {
        let mtm = self.target.mtm();
        let connected = state.connection.is_connected();
        let model = state.model;
        let status = &state.status;

        self.menu.removeAllItems();

        for line in header_lines(state) {
            self.menu.addItem(&inert(mtm, &line));
        }
        self.menu.addItem(&NSMenuItem::separatorItem(mtm));
        self.menu.addItem(&inert(mtm, &battery_line(state)));

        // The radio group that makes this app the AirPods noise-control panel.
        let (anc_enabled, anc_note) = gate(connected, model, Feature::NoiseGroup);
        if let Some(note) = anc_note {
            self.menu.addItem(&inert(mtm, note));
        }
        for mode in [AncMode::Normal, AncMode::NoiseCancelling, AncMode::Ambient] {
            let checked = status.anc == Some(mode);
            self.menu.addItem(&self.radio(
                mtm,
                anc_label(mode),
                Action::SetAnc(mode),
                checked,
                anc_enabled,
            ));
        }

        let (ambient_enabled, ambient_note) = gate(connected, model, Feature::AmbientSoundGroup);
        let (ambient_item, ambient_menu) =
            submenu_item(mtm, "Ambient Sound Volume", ambient_enabled);
        if let Some(note) = ambient_note {
            ambient_menu.addItem(&inert(mtm, note));
        }
        for volume in AMBIENT_VOLUME_MIN..=AMBIENT_VOLUME_MAX {
            ambient_menu.addItem(&self.radio(
                mtm,
                &ambient_volume_label(volume),
                Action::SetAmbientVolume(volume),
                status.ambient_volume == Some(volume),
                ambient_enabled,
            ));
        }
        self.menu.addItem(&ambient_item);

        let (effect_enabled, effect_note) = gate(connected, model, Feature::SoundEffectGroup);
        let (effect_item, effect_menu) = submenu_item(mtm, "Sound Effect", effect_enabled);
        if let Some(note) = effect_note {
            effect_menu.addItem(&inert(mtm, note));
        }
        for effect in [
            SoundEffect::Normal,
            SoundEffect::Pop,
            SoundEffect::Classical,
            SoundEffect::Rock,
        ] {
            effect_menu.addItem(&self.radio(
                mtm,
                sound_effect_label(effect),
                Action::SetSoundEffect(effect),
                status.sound_effect == Some(effect),
                effect_enabled,
            ));
        }
        self.menu.addItem(&effect_item);

        let (game_enabled, _) = gate(connected, model, Feature::GameMode);
        self.menu.addItem(&self.radio(
            mtm,
            "Game Mode (low latency)",
            Action::ToggleGameMode,
            status.game_mode == Some(true),
            game_enabled,
        ));

        let (ldac_enabled, ldac_note) = gate(connected, model, Feature::LdacGroup);
        let title = match status.ldac {
            Some(mode) => format!("LDAC: {}", ldac_label(mode)),
            None => "LDAC".to_string(),
        };
        let (ldac_item, ldac_menu) = submenu_item(mtm, &title, ldac_enabled);
        if let Some(note) = ldac_note {
            ldac_menu.addItem(&inert(mtm, note));
        }
        ldac_menu.addItem(&inert(mtm, ldac_warning()));
        for mode in [LdacMode::Off, LdacMode::Khz48, LdacMode::Khz96] {
            ldac_menu.addItem(&self.radio(
                mtm,
                ldac_label(mode),
                Action::SetLdac(mode),
                status.ldac == Some(mode),
                ldac_enabled,
            ));
        }
        self.menu.addItem(&ldac_item);

        let (prompt_enabled, prompt_note) = gate(connected, model, Feature::PromptVolume);
        let (prompt_item, prompt_menu) = submenu_item(
            mtm,
            &format!(
                "Prompt Volume: {}",
                status
                    .prompt_volume
                    .map_or_else(|| "?".to_string(), prompt_volume_label)
            ),
            prompt_enabled,
        );
        if let Some(note) = prompt_note {
            prompt_menu.addItem(&inert(mtm, note));
        }
        for volume in 0..=PROMPT_VOLUME_MAX {
            prompt_menu.addItem(&self.radio(
                mtm,
                &prompt_volume_label(volume),
                Action::SetPromptVolume(volume),
                status.prompt_volume == Some(volume),
                prompt_enabled,
            ));
        }
        self.menu.addItem(&prompt_item);

        let (timer_enabled, timer_note) = gate(connected, model, Feature::ShutdownTimer);
        let title = match status.shutdown_timer {
            Some(timer) => format!("Shutdown Timer: {}", timer_label(timer)),
            None => "Shutdown Timer".to_string(),
        };
        let (timer_item, timer_menu) = submenu_item(mtm, &title, timer_enabled);
        if let Some(note) = timer_note {
            timer_menu.addItem(&inert(mtm, note));
        }
        for (index, choice) in SHUTDOWN_CHOICES.iter().enumerate() {
            let Some(timer) = shutdown_timer(index) else {
                continue;
            };
            let label = match choice {
                None => "Off".to_string(),
                Some(minutes) => format!("{minutes} min"),
            };
            timer_menu.addItem(&self.radio(
                mtm,
                &label,
                Action::SetShutdownTimer(timer),
                status.shutdown_timer == Some(timer),
                timer_enabled,
            ));
        }
        self.menu.addItem(&timer_item);

        let gestures = status.anc_gestures.unwrap_or_default();
        let (gesture_enabled, gesture_note) = gate(connected, model, Feature::ControlSettingsGroup);
        let (gesture_item, gesture_menu) = submenu_item(
            mtm,
            &format!("ANC Gestures: {}", gesture_label(gestures)),
            gesture_enabled,
        );
        if let Some(note) = gesture_note {
            gesture_menu.addItem(&inert(mtm, note));
        }
        for bit in GestureBit::ALL {
            gesture_menu.addItem(&self.radio(
                mtm,
                bit.label(),
                Action::ToggleGesture(bit),
                bit.is_enabled(gestures),
                gesture_enabled,
            ));
        }
        self.menu.addItem(&gesture_item);

        self.menu.addItem(&NSMenuItem::separatorItem(mtm));

        let (audio_enabled, _) = gate(connected, model, Feature::HostSwitch);
        self.menu.addItem(&self.radio(
            mtm,
            "Move Audio to This Mac",
            Action::AudioToMac,
            false,
            audio_enabled,
        ));

        let (rename_enabled, _) = gate(connected, model, Feature::Rename);
        self.menu.addItem(&self.radio(
            mtm,
            "Rename Headset…",
            Action::Rename,
            false,
            rename_enabled,
        ));
        self.menu
            .addItem(&self.radio(mtm, "Disconnect", Action::Disconnect, false, connected));
        self.menu
            .addItem(&self.radio(mtm, "Re-Pair", Action::RePair, false, connected));
        self.menu.addItem(&self.radio(
            mtm,
            "Power Off Headset",
            Action::PowerOff,
            false,
            connected,
        ));
        self.menu.addItem(&self.radio(
            mtm,
            "Factory Reset…",
            Action::FactoryReset,
            false,
            connected,
        ));

        if let Some(error) = &state.error {
            self.menu.addItem(&NSMenuItem::separatorItem(mtm));
            self.menu.addItem(&inert(mtm, &format!("Failed: {error}")));
        }

        self.menu.addItem(&NSMenuItem::separatorItem(mtm));
        let refresh = self.radio(mtm, "Refresh Status", Action::Refresh, false, true);
        refresh.setKeyEquivalent(&NSString::from_str("r"));
        refresh.setKeyEquivalentModifierMask(NSEventModifierFlags::Command);
        self.menu.addItem(&refresh);

        let quit = self.radio(mtm, "Quit edifier-menubar", Action::Quit, false, true);
        quit.setKeyEquivalent(&NSString::from_str("q"));
        quit.setKeyEquivalentModifierMask(NSEventModifierFlags::Command);
        self.menu.addItem(&quit);
    }

    /// A checkmark item wired to the shared target.
    fn radio(
        &self,
        mtm: MainThreadMarker,
        title: &str,
        action: Action,
        checked: bool,
        enabled: bool,
    ) -> Retained<NSMenuItem> {
        let item = NSMenuItem::new(mtm);
        item.setTitle(&NSString::from_str(title));
        item.setState(if checked {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        });
        item.setTag(action.tag());
        item.setEnabled(enabled);
        wire(&item, &self.target);
        item
    }
}

/// Install the status bar application: accessory policy, delegate, action target wiring.
///
/// Call once from the main thread before [`run`].
pub fn install(
    mtm: MainThreadMarker,
    state: SharedState,
    requests: mpsc::Sender<Action>,
    settings: Config,
) {
    let app = NSApplication::sharedApplication(mtm);
    if !app.setActivationPolicy(NSApplicationActivationPolicy::Accessory) {
        warn!("cannot become a menu bar only application; a Dock icon may appear");
    }
    let delegate = AppDelegate::new(mtm, state, requests, settings);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    MAIN.with(|slot| slot.borrow_mut().delegate = Some(delegate));
}

/// Run the AppKit event loop until the app quits.
pub fn run(mtm: MainThreadMarker) {
    NSApplication::sharedApplication(mtm).run();
}

/// A [`Waker`] that hops onto the AppKit main queue.
pub struct MainQueueWaker;

impl Waker for MainQueueWaker {
    fn wake(&self) {
        DispatchQueue::main().exec_async(redraw);
    }
}

/// The waker the BLE runtime uses to reach AppKit.
#[must_use]
pub fn waker() -> Arc<dyn Waker> {
    Arc::new(MainQueueWaker)
}

/// Redraw the status item; runs on the main thread.
fn redraw() {
    MAIN.with(|slot| {
        let Ok(slot) = slot.try_borrow() else {
            warn!("skipping a redraw that landed inside another redraw");
            return;
        };
        if let Some(ui) = slot.ui.as_ref() {
            ui.redraw();
        }
    });
}

/// Nudge the main thread for a redraw.
fn wake() {
    DispatchQueue::main().exec_async(redraw);
}

/// Bring the app forward so a modal alert is visible.
fn activate(mtm: MainThreadMarker) {
    NSApplication::sharedApplication(mtm).activate();
}

/// Look an SF Symbol up by name, describing it for VoiceOver (and for AppleScript queries).
fn symbol(name: &str, description: &str) -> Option<Retained<NSImage>> {
    NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str(name),
        Some(&NSString::from_str(description)),
    )
}

/// What the status item says about itself to assistive technology.
#[must_use]
fn accessibility_description(state: &UiState) -> String {
    let name = state
        .status
        .name
        .clone()
        .or_else(|| state.model.map(|model| model.display_name().to_string()))
        .unwrap_or_else(|| "Edifier headset".to_string());
    match (state.connection.is_connected(), state.status.battery) {
        (true, Some(percent)) => format!("{name}, battery {percent}%"),
        (true, None) => name,
        (false, _) => format!("{name}: {}", state.connection.line()),
    }
}

/// Point a menu item at the shared action target.
fn wire(item: &NSMenuItem, target: &ActionTarget) {
    let any: &AnyObject = target.as_super().as_super();
    // SAFETY: `target` is an `ActionTarget`, which implements the `dispatchAction:` selector
    // below; AppKit stores menu targets weakly, and `Main` keeps the strong reference alive for
    // the lifetime of the process.
    unsafe { item.setTarget(Some(any)) };
    // SAFETY: `dispatchAction:` exists on `ActionTarget` and takes a single object argument.
    unsafe { item.setAction(Some(sel!(dispatchAction:))) };
}

/// Text shown for a control this headset does not implement.
const NOT_SUPPORTED: &str = "Not supported by this headset";

/// Whether the entry for `feature` can be used, plus the note to show when the headset cannot.
fn gate(
    connected: bool,
    model: Option<DeviceModel>,
    feature: Feature,
) -> (bool, Option<&'static str>) {
    if !connected {
        // Offline is not a capability problem: the header explains why nothing can be clicked.
        (false, None)
    } else if supports(model, feature) {
        (true, None)
    } else {
        (false, Some(NOT_SUPPORTED))
    }
}

/// A non-clickable label row.
fn inert(mtm: MainThreadMarker, title: &str) -> Retained<NSMenuItem> {
    let item = NSMenuItem::new(mtm);
    item.setTitle(&NSString::from_str(title));
    item.setEnabled(false);
    item
}

/// A submenu titled `title`, returning the item to add and the menu to fill in.
fn submenu_item(
    mtm: MainThreadMarker,
    title: &str,
    enabled: bool,
) -> (Retained<NSMenuItem>, Retained<NSMenu>) {
    let item = NSMenuItem::new(mtm);
    item.setTitle(&NSString::from_str(title));
    item.setEnabled(enabled);
    let menu = NSMenu::new(mtm);
    menu.setTitle(&NSString::from_str(title));
    menu.setAutoenablesItems(false);
    item.setSubmenu(Some(&menu));
    (item, menu)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menu::Connection;

    #[test]
    fn ldac_warning_names_the_consequence() {
        let warning = ldac_warning();
        assert!(warning.contains("no LDAC encoder"), "{warning}");
        assert!(warning.contains("audio stops"), "{warning}");
        assert_eq!(icon_symbol(&UiState::default()), "headphones.slash");
    }

    #[test]
    fn accessibility_description_reports_the_battery_and_the_link() {
        let mut state = UiState {
            connection: Connection::Connected {
                description: "BLE EDIFIER W820NB (EDIFIER W820NB)".to_string(),
            },
            model: Some(DeviceModel::W820Nb),
            ..UiState::default()
        };
        assert_eq!(accessibility_description(&state), "EDIFIER W820NB");
        state.status.battery = Some(64);
        assert_eq!(
            accessibility_description(&state),
            "EDIFIER W820NB, battery 64%"
        );
        state.connection = Connection::Offline {
            reason: "no Edifier endpoint found".to_string(),
        };
        assert_eq!(
            accessibility_description(&state),
            "EDIFIER W820NB: Not connected: no Edifier endpoint found"
        );
    }

    #[test]
    fn gate_explains_why_a_control_is_disabled() {
        let w820nb = Some(DeviceModel::W820Nb);
        assert_eq!(gate(true, w820nb, Feature::LdacGroup), (true, None));
        assert_eq!(
            gate(true, w820nb, Feature::SoundEffectGroup),
            (false, Some(NOT_SUPPORTED))
        );
        assert_eq!(gate(false, w820nb, Feature::LdacGroup), (false, None));
        assert_eq!(gate(true, None, Feature::SoundEffectGroup), (true, None));
    }

    #[test]
    fn shutdown_choices_are_labelled_for_the_menu() {
        let labels: Vec<String> = SHUTDOWN_CHOICES
            .iter()
            .enumerate()
            .filter_map(|(index, choice)| {
                shutdown_timer(index).map(|timer| match choice {
                    None => "Off".to_string(),
                    Some(_) => timer_label(timer),
                })
            })
            .collect();
        assert_eq!(
            labels,
            vec!["Off", "15 min", "30 min", "60 min", "120 min", "180 min"]
        );
    }
}
